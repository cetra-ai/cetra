//! Rule-based optimizer for Shape B SQL [`LogicalPlan`]s.
//!
//! Executes six deterministic passes in fixed order:
//! 1. Constant folding and boolean simplification (preserving division-by-zero/overflow errors).
//! 2. Filter pushdown through `Project` and safe `Join` sides (before hex-ID rewrite).
//! 3. Hex-literal ID rewrite (`encode(id, 'hex') =/<>/IN` -> `FixedBytes8`/`FixedBytes16`, D89).
//! 4. Storage-safe `Scan` filter pushdown (`Traces`: `start_time`/`trace_id`, `Scores`: `created_at`)
//!    and pruning hint extraction (`time_range`, `id_lookups`, CNF `substrings`), retaining full
//!    predicates in `Scan.filters`.
//! 5. Top-down projection pruning and `MapProjection` pushdown into `Scan.map_projections`.
//! 6. Bounded `ORDER BY ... LIMIT k [OFFSET n]` -> `TopK` rewrite when `offset + limit <= 65_536`.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use cetra_column::{
    Batch, CancelToken, DataType, GlobalPool, Reservation, ScalarValue, Schema, ScopedPool,
};

use crate::error::QueryError;
use crate::eval::eval_expr;
use crate::exec::hash_agg::{AggExpr, AggFunc};
use crate::exec::sort_key::SortKey;
use crate::exec::sort_window::{WindowFunc, WindowSpec};
use crate::exec::source::{
    IdLookupHint, MapProjection, SubstringHint, TimeRangeHint, collect_expr_map_projections,
};
use crate::ir::{BinaryOp, CastMode, EvalContext, Expr, ScalarFn, UnaryOp};

use super::catalog::SqlViewKind;
use super::logical::{LogicalJoinKind, LogicalPlan};

/// Maximum `offset + limit` rewritten to unspillable in-memory `TopK` (above this, keep spillable `ExternalSort`).
pub const TOPK_MAX_ROWS: usize = 65_536;

/// Memory budget enforced on each `ConstFolder` `eval_expr` evaluation (64 KiB).
const FOLD_MEM_CAP: usize = 64 * 1024;

/// Maximum AST nodes allowed when substituting a conjunct through `Project`.
const MAX_SUBSTITUTED_EXPR_NODES: usize = 2_048;

/// Optimizes a typed [`LogicalPlan`] using the fixed 6-pass rule pipeline.
pub fn optimize(plan: LogicalPlan) -> Result<LogicalPlan, QueryError> {
    let p1 = pass1_constant_fold(plan)?;
    let p2 = pass2_filter_pushdown(p1)?;
    let p3 = pass3_hex_id_rewrite(p2)?;
    let p4 = pass4_scan_pushdown(p3)?;
    let p5 = pass5_projection_and_map_keys(p4)?;
    Ok(pass6_topk_rewrite(p5))
}

// ---------------------------------------------------------------------------
// Pass 1: Constant folding & boolean simplification
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Default)]
struct FoldFlags {
    has_col: bool,
    has_now: bool,
    has_redacted: bool,
    has_unfoldable: bool,
}

impl FoldFlags {
    fn merge(self, other: Self) -> Self {
        Self {
            has_col: self.has_col || other.has_col,
            has_now: self.has_now || other.has_now,
            has_redacted: self.has_redacted || other.has_redacted,
            has_unfoldable: self.has_unfoldable || other.has_unfoldable,
        }
    }

    fn is_foldable(self) -> bool {
        !self.has_col && !self.has_now && !self.has_redacted && !self.has_unfoldable
    }
}

struct ConstFolder {
    batch: Batch,
    eval_ctx: EvalContext,
    _pool: Arc<ScopedPool>,
    res: Reservation,
}

impl ConstFolder {
    fn new() -> Result<Self, QueryError> {
        let empty = Schema::try_new_arc(Vec::new())?;
        let batch = Batch::try_new_all(empty, Vec::new(), 1)?;
        let eval_ctx = EvalContext::new(0, CancelToken::new());
        let pool = GlobalPool::standalone(FOLD_MEM_CAP);
        let res = pool.register("const_fold", false);
        Ok(Self {
            batch,
            eval_ctx,
            _pool: pool,
            res,
        })
    }

    fn fold_plan(&mut self, plan: LogicalPlan) -> Result<LogicalPlan, QueryError> {
        Ok(match plan {
            LogicalPlan::Scan {
                table,
                kind,
                full_schema,
                projection,
                filters,
                time_range,
                id_lookups,
                substrings,
                map_projections,
                limit,
                schema,
            } => LogicalPlan::Scan {
                table,
                kind,
                full_schema,
                projection,
                filters: filters
                    .into_iter()
                    .map(|f| self.fold_expr(f))
                    .filter(|f| !is_bool_lit(f, true))
                    .map(|f| {
                        if is_null_lit(&f) {
                            Expr::Literal(ScalarValue::Bool(false))
                        } else {
                            f
                        }
                    })
                    .collect(),
                time_range,
                id_lookups,
                substrings,
                map_projections,
                limit,
                schema,
            },
            LogicalPlan::Values { schema, rows } => LogicalPlan::Values {
                schema,
                rows: rows
                    .into_iter()
                    .map(|row| row.into_iter().map(|e| self.fold_expr(e)).collect())
                    .collect(),
            },
            LogicalPlan::EmptyRow { schema } => LogicalPlan::EmptyRow { schema },
            LogicalPlan::Filter { input, predicate } => {
                let input = self.fold_plan(*input)?;
                let pred = self.fold_expr(predicate);
                if is_bool_lit(&pred, true) {
                    return Ok(input);
                }
                let pred = if is_null_lit(&pred) {
                    Expr::Literal(ScalarValue::Bool(false))
                } else {
                    pred
                };
                LogicalPlan::Filter {
                    input: Box::new(input),
                    predicate: pred,
                }
            }
            LogicalPlan::Project {
                input,
                exprs,
                schema,
            } => LogicalPlan::Project {
                input: Box::new(self.fold_plan(*input)?),
                exprs: exprs.into_iter().map(|e| self.fold_expr(e)).collect(),
                schema,
            },
            LogicalPlan::Aggregate {
                input,
                group_by,
                aggs,
                schema,
            } => LogicalPlan::Aggregate {
                input: Box::new(self.fold_plan(*input)?),
                group_by: group_by
                    .into_iter()
                    .map(|(n, e)| (n, self.fold_expr(e)))
                    .collect(),
                aggs: aggs.into_iter().map(|agg| self.fold_agg(agg)).collect(),
                schema,
            },
            LogicalPlan::Window {
                input,
                partition_by,
                order_by,
                specs,
                schema,
            } => LogicalPlan::Window {
                input: Box::new(self.fold_plan(*input)?),
                partition_by: partition_by
                    .into_iter()
                    .map(|e| self.fold_expr(e))
                    .collect(),
                order_by: order_by
                    .into_iter()
                    .map(|k| self.fold_sort_key(k))
                    .collect(),
                specs: specs.into_iter().map(|s| self.fold_window(s)).collect(),
                schema,
            },
            LogicalPlan::Sort { input, keys } => LogicalPlan::Sort {
                input: Box::new(self.fold_plan(*input)?),
                keys: keys.into_iter().map(|k| self.fold_sort_key(k)).collect(),
            },
            LogicalPlan::Limit { input, skip, fetch } => LogicalPlan::Limit {
                input: Box::new(self.fold_plan(*input)?),
                skip,
                fetch,
            },
            LogicalPlan::TopK {
                input,
                keys,
                limit,
                offset,
            } => LogicalPlan::TopK {
                input: Box::new(self.fold_plan(*input)?),
                keys: keys.into_iter().map(|k| self.fold_sort_key(k)).collect(),
                limit,
                offset,
            },
            LogicalPlan::Distinct { input } => LogicalPlan::Distinct {
                input: Box::new(self.fold_plan(*input)?),
            },
            LogicalPlan::Join {
                left,
                right,
                kind,
                left_keys,
                right_keys,
                null_equals_null,
                residual,
                schema,
            } => LogicalPlan::Join {
                left: Box::new(self.fold_plan(*left)?),
                right: Box::new(self.fold_plan(*right)?),
                kind,
                left_keys: left_keys.into_iter().map(|e| self.fold_expr(e)).collect(),
                right_keys: right_keys.into_iter().map(|e| self.fold_expr(e)).collect(),
                null_equals_null,
                residual: residual.map(|r| self.fold_expr(r)),
                schema,
            },
            LogicalPlan::Union {
                inputs,
                all,
                schema,
            } => {
                let mut out = Vec::with_capacity(inputs.len());
                for branch in inputs {
                    out.push(self.fold_plan(branch)?);
                }
                LogicalPlan::Union {
                    inputs: out,
                    all,
                    schema,
                }
            }
            LogicalPlan::Unnest {
                input,
                columns,
                preserve_nulls,
                schema,
            } => LogicalPlan::Unnest {
                input: Box::new(self.fold_plan(*input)?),
                columns,
                preserve_nulls,
                schema,
            },
            LogicalPlan::Explain { inner, schema } => LogicalPlan::Explain {
                inner: Box::new(self.fold_plan(*inner)?),
                schema,
            },
        })
    }

    fn fold_sort_key(&mut self, mut key: SortKey) -> SortKey {
        key.expr = self.fold_expr(key.expr);
        key
    }

    fn fold_agg(&mut self, agg: AggExpr) -> AggExpr {
        AggExpr {
            func: map_agg_func_exprs(agg.func, &mut |e| self.fold_expr(e)),
            filter: agg.filter.map(|f| self.fold_expr(f)),
            output_name: agg.output_name,
        }
    }

    fn fold_window(&mut self, spec: WindowSpec) -> WindowSpec {
        WindowSpec {
            partition_by: spec
                .partition_by
                .into_iter()
                .map(|e| self.fold_expr(e))
                .collect(),
            order_by: spec
                .order_by
                .into_iter()
                .map(|k| self.fold_sort_key(k))
                .collect(),
            frame: spec.frame,
            func: map_window_func_exprs(spec.func, &mut |e| self.fold_expr(e)),
            output_name: spec.output_name,
        }
    }

    fn try_eval_const(&mut self, expr: Expr, mut flags: FoldFlags) -> (Expr, FoldFlags) {
        if !flags.is_foldable() {
            return (expr, flags);
        }
        let orig_dt = expr.data_type();
        let folded_sv = eval_expr(&expr, &self.batch, &self.eval_ctx, Some(&mut self.res))
            .ok()
            .and_then(|col| col.get_scalar(0).ok());
        self.res.free();
        if let Some(sv) = folded_sv
            && sv.heap_bytes() <= FOLD_MEM_CAP
        {
            let sv = if sv.is_null() && sv.data_type() != orig_dt {
                ScalarValue::Null(orig_dt)
            } else {
                sv
            };
            return (Expr::Literal(sv), FoldFlags::default());
        }
        flags.has_unfoldable = true;
        (expr, flags)
    }

    fn fold_expr(&mut self, expr: Expr) -> Expr {
        enum FoldTask {
            Eval(Expr),
            BuildUnary {
                op: UnaryOp,
                data_type: DataType,
            },
            BuildBinary {
                op: BinaryOp,
                data_type: DataType,
            },
            BuildCast {
                target: DataType,
                mode: CastMode,
            },
            BuildBetween {
                negated: bool,
            },
            BuildInList {
                n: usize,
                negated: bool,
            },
            BuildInListRedacted {
                list: Vec<Expr>,
                negated: bool,
            },
            BuildLike {
                escape: Option<char>,
                case_insensitive: bool,
                negated: bool,
            },
            BuildCase {
                has_op: bool,
                n_wt: usize,
                has_else: bool,
                data_type: DataType,
            },
            BuildMapIndex,
            BuildMakeArray {
                n: usize,
                item_type: DataType,
            },
            BuildCall {
                func: ScalarFn,
                n: usize,
                data_type: DataType,
            },
        }

        let mut tasks = vec![FoldTask::Eval(expr)];
        let mut vals: Vec<(Expr, FoldFlags)> = Vec::new();

        while let Some(task) = tasks.pop() {
            match task {
                FoldTask::Eval(node) => match node {
                    Expr::Column { .. } => {
                        vals.push((
                            node,
                            FoldFlags {
                                has_col: true,
                                ..Default::default()
                            },
                        ));
                    }
                    Expr::Literal(_) => {
                        vals.push((node, FoldFlags::default()));
                    }
                    Expr::Unary {
                        op,
                        expr: inner,
                        data_type,
                    } => {
                        tasks.push(FoldTask::BuildUnary { op, data_type });
                        tasks.push(FoldTask::Eval(*inner));
                    }
                    Expr::Binary {
                        left,
                        op,
                        right,
                        data_type,
                    } => {
                        tasks.push(FoldTask::BuildBinary { op, data_type });
                        tasks.push(FoldTask::Eval(*right));
                        tasks.push(FoldTask::Eval(*left));
                    }
                    Expr::Cast {
                        expr: inner,
                        target,
                        mode,
                    } => {
                        tasks.push(FoldTask::BuildCast { target, mode });
                        tasks.push(FoldTask::Eval(*inner));
                    }
                    Expr::Between {
                        expr: inner,
                        low,
                        high,
                        negated,
                    } => {
                        tasks.push(FoldTask::BuildBetween { negated });
                        tasks.push(FoldTask::Eval(*high));
                        tasks.push(FoldTask::Eval(*low));
                        tasks.push(FoldTask::Eval(*inner));
                    }
                    Expr::InList {
                        expr: inner,
                        list,
                        negated,
                        redacted,
                    } => {
                        if redacted {
                            tasks.push(FoldTask::BuildInListRedacted { list, negated });
                            tasks.push(FoldTask::Eval(*inner));
                        } else {
                            tasks.push(FoldTask::BuildInList {
                                n: list.len(),
                                negated,
                            });
                            for item in list.into_iter().rev() {
                                tasks.push(FoldTask::Eval(item));
                            }
                            tasks.push(FoldTask::Eval(*inner));
                        }
                    }
                    Expr::Like {
                        expr: inner,
                        pattern,
                        escape,
                        case_insensitive,
                        negated,
                    } => {
                        tasks.push(FoldTask::BuildLike {
                            escape,
                            case_insensitive,
                            negated,
                        });
                        tasks.push(FoldTask::Eval(*pattern));
                        tasks.push(FoldTask::Eval(*inner));
                    }
                    Expr::Case {
                        operand,
                        when_then,
                        else_expr,
                        data_type,
                    } => {
                        tasks.push(FoldTask::BuildCase {
                            has_op: operand.is_some(),
                            n_wt: when_then.len(),
                            has_else: else_expr.is_some(),
                            data_type,
                        });
                        if let Some(el) = else_expr {
                            tasks.push(FoldTask::Eval(*el));
                        }
                        for (w, t) in when_then.into_iter().rev() {
                            tasks.push(FoldTask::Eval(t));
                            tasks.push(FoldTask::Eval(w));
                        }
                        if let Some(op) = operand {
                            tasks.push(FoldTask::Eval(*op));
                        }
                    }
                    Expr::MapIndex { map, key } => {
                        tasks.push(FoldTask::BuildMapIndex);
                        tasks.push(FoldTask::Eval(*key));
                        tasks.push(FoldTask::Eval(*map));
                    }
                    Expr::MakeArray { elems, item_type } => {
                        tasks.push(FoldTask::BuildMakeArray {
                            n: elems.len(),
                            item_type,
                        });
                        for e in elems.into_iter().rev() {
                            tasks.push(FoldTask::Eval(e));
                        }
                    }
                    Expr::Call {
                        func,
                        args,
                        data_type,
                    } => {
                        tasks.push(FoldTask::BuildCall {
                            func,
                            n: args.len(),
                            data_type,
                        });
                        for a in args.into_iter().rev() {
                            tasks.push(FoldTask::Eval(a));
                        }
                    }
                },
                FoldTask::BuildUnary { op, data_type } => {
                    let Some((inner, flags)) = vals.pop() else {
                        break;
                    };
                    if op == UnaryOp::Not
                        && let Expr::Unary {
                            op: UnaryOp::Not,
                            expr: grand,
                            ..
                        } = inner
                    {
                        vals.push((coerce_null_type(*grand, data_type), flags));
                        continue;
                    }
                    let node = Expr::Unary {
                        op,
                        expr: Box::new(inner),
                        data_type,
                    };
                    vals.push(self.try_eval_const(node, flags));
                }
                FoldTask::BuildBinary { op, data_type } => {
                    let (Some((right, r_flags)), Some((left, l_flags))) = (vals.pop(), vals.pop())
                    else {
                        break;
                    };
                    match op {
                        BinaryOp::And => {
                            if is_bool_lit(&left, true) {
                                vals.push((coerce_null_type(right, data_type), r_flags));
                                continue;
                            }
                            if is_bool_lit(&right, true) {
                                vals.push((coerce_null_type(left, data_type), l_flags));
                                continue;
                            }
                            if is_bool_lit(&left, false) {
                                vals.push((
                                    Expr::Literal(ScalarValue::Bool(false)),
                                    FoldFlags::default(),
                                ));
                                continue;
                            }
                            if is_bool_lit(&right, false) && is_infallible(&left) {
                                vals.push((
                                    Expr::Literal(ScalarValue::Bool(false)),
                                    FoldFlags::default(),
                                ));
                                continue;
                            }
                        }
                        BinaryOp::Or => {
                            if is_bool_lit(&left, false) {
                                vals.push((coerce_null_type(right, data_type), r_flags));
                                continue;
                            }
                            if is_bool_lit(&right, false) {
                                vals.push((coerce_null_type(left, data_type), l_flags));
                                continue;
                            }
                            if is_bool_lit(&left, true) {
                                vals.push((
                                    Expr::Literal(ScalarValue::Bool(true)),
                                    FoldFlags::default(),
                                ));
                                continue;
                            }
                            if is_bool_lit(&right, true) && is_infallible(&left) {
                                vals.push((
                                    Expr::Literal(ScalarValue::Bool(true)),
                                    FoldFlags::default(),
                                ));
                                continue;
                            }
                        }
                        _ => {}
                    }
                    let node = Expr::Binary {
                        left: Box::new(left),
                        op,
                        right: Box::new(right),
                        data_type,
                    };
                    vals.push(self.try_eval_const(node, l_flags.merge(r_flags)));
                }
                FoldTask::BuildCast { target, mode } => {
                    let Some((inner, flags)) = vals.pop() else {
                        break;
                    };
                    let node = Expr::Cast {
                        expr: Box::new(inner),
                        target,
                        mode,
                    };
                    vals.push(self.try_eval_const(node, flags));
                }
                FoldTask::BuildBetween { negated } => {
                    let (Some((high, h_f)), Some((low, l_f)), Some((inner, i_f))) =
                        (vals.pop(), vals.pop(), vals.pop())
                    else {
                        break;
                    };
                    let node = Expr::Between {
                        expr: Box::new(inner),
                        low: Box::new(low),
                        high: Box::new(high),
                        negated,
                    };
                    vals.push(self.try_eval_const(node, i_f.merge(l_f).merge(h_f)));
                }
                FoldTask::BuildInList { n, negated } => {
                    let start = vals.len().saturating_sub(n);
                    let mut list = Vec::with_capacity(n);
                    let mut flags = FoldFlags::default();
                    for (item, f) in vals.drain(start..) {
                        flags = flags.merge(f);
                        list.push(item);
                    }
                    let Some((inner, i_f)) = vals.pop() else {
                        break;
                    };
                    let node = Expr::InList {
                        expr: Box::new(inner),
                        list,
                        negated,
                        redacted: false,
                    };
                    vals.push(self.try_eval_const(node, i_f.merge(flags)));
                }
                FoldTask::BuildInListRedacted { list, negated } => {
                    let Some((inner, mut flags)) = vals.pop() else {
                        break;
                    };
                    flags.has_redacted = true;
                    vals.push((
                        Expr::InList {
                            expr: Box::new(inner),
                            list,
                            negated,
                            redacted: true,
                        },
                        flags,
                    ));
                }
                FoldTask::BuildLike {
                    escape,
                    case_insensitive,
                    negated,
                } => {
                    let (Some((pattern, p_f)), Some((inner, i_f))) = (vals.pop(), vals.pop())
                    else {
                        break;
                    };
                    let node = Expr::Like {
                        expr: Box::new(inner),
                        pattern: Box::new(pattern),
                        escape,
                        case_insensitive,
                        negated,
                    };
                    vals.push(self.try_eval_const(node, i_f.merge(p_f)));
                }
                FoldTask::BuildCase {
                    has_op,
                    n_wt,
                    has_else,
                    data_type,
                } => {
                    let mut flags = FoldFlags::default();
                    let else_expr = if has_else {
                        let Some((el, f)) = vals.pop() else {
                            break;
                        };
                        flags = flags.merge(f);
                        Some(Box::new(el))
                    } else {
                        None
                    };
                    let wt_start = vals.len().saturating_sub(n_wt * 2);
                    let mut when_then = Vec::with_capacity(n_wt);
                    let mut drained = vals.drain(wt_start..);
                    while let (Some((w, wf)), Some((t, tf))) = (drained.next(), drained.next()) {
                        flags = flags.merge(wf).merge(tf);
                        when_then.push((w, t));
                    }
                    drop(drained);
                    let operand = if has_op {
                        let Some((op, f)) = vals.pop() else {
                            break;
                        };
                        flags = flags.merge(f);
                        Some(Box::new(op))
                    } else {
                        None
                    };
                    let node = Expr::Case {
                        operand,
                        when_then,
                        else_expr,
                        data_type,
                    };
                    vals.push(self.try_eval_const(node, flags));
                }
                FoldTask::BuildMapIndex => {
                    let (Some((key, k_f)), Some((map, m_f))) = (vals.pop(), vals.pop()) else {
                        break;
                    };
                    let node = Expr::MapIndex {
                        map: Box::new(map),
                        key: Box::new(key),
                    };
                    vals.push(self.try_eval_const(node, m_f.merge(k_f)));
                }
                FoldTask::BuildMakeArray { n, item_type } => {
                    let start = vals.len().saturating_sub(n);
                    let mut elems = Vec::with_capacity(n);
                    let mut flags = FoldFlags::default();
                    for (e, f) in vals.drain(start..) {
                        flags = flags.merge(f);
                        elems.push(e);
                    }
                    let node = Expr::MakeArray { elems, item_type };
                    vals.push(self.try_eval_const(node, flags));
                }
                FoldTask::BuildCall { func, n, data_type } => {
                    let start = vals.len().saturating_sub(n);
                    let mut args = Vec::with_capacity(n);
                    let mut flags = FoldFlags {
                        has_now: func == ScalarFn::Now,
                        ..Default::default()
                    };
                    for (a, f) in vals.drain(start..) {
                        flags = flags.merge(f);
                        args.push(a);
                    }
                    let node = Expr::Call {
                        func,
                        args,
                        data_type,
                    };
                    vals.push(self.try_eval_const(node, flags));
                }
            }
        }

        vals.pop()
            .map(|(e, _)| e)
            .unwrap_or(Expr::Literal(ScalarValue::Null(DataType::Null)))
    }
}

fn pass1_constant_fold(plan: LogicalPlan) -> Result<LogicalPlan, QueryError> {
    let mut folder = ConstFolder::new()?;
    folder.fold_plan(plan)
}

fn is_bool_lit(expr: &Expr, expected: bool) -> bool {
    matches!(expr, Expr::Literal(ScalarValue::Bool(b)) if *b == expected)
}

fn is_null_lit(expr: &Expr) -> bool {
    matches!(expr, Expr::Literal(ScalarValue::Null(_)))
}

fn coerce_null_type(expr: Expr, expected: DataType) -> Expr {
    match expr {
        Expr::Literal(ScalarValue::Null(dt)) if dt != expected => {
            Expr::Literal(ScalarValue::Null(expected))
        }
        other => other,
    }
}

fn contains_decimal(dt: &DataType) -> bool {
    match dt {
        DataType::Decimal128 { .. } => true,
        DataType::List(inner) => contains_decimal(inner),
        _ => false,
    }
}

/// Conservative infallibility check so boolean identities never hide division-by-zero,
/// integer overflow, strict cast errors, decimal comparison errors, or regex/JSONPath errors.
fn is_infallible(expr: &Expr) -> bool {
    let mut stack = vec![expr];
    while let Some(node) = stack.pop() {
        match node {
            Expr::Column { .. } | Expr::Literal(_) => {}
            Expr::Unary { op, expr, .. } => {
                if matches!(op, UnaryOp::Neg) {
                    return false;
                }
                stack.push(expr);
            }
            Expr::Binary {
                left,
                op,
                right,
                data_type,
            } => {
                let is_cmp = matches!(
                    op,
                    BinaryOp::Eq
                        | BinaryOp::NotEq
                        | BinaryOp::Lt
                        | BinaryOp::LtEq
                        | BinaryOp::Gt
                        | BinaryOp::GtEq
                        | BinaryOp::IsDistinctFrom
                        | BinaryOp::IsNotDistinctFrom
                        | BinaryOp::ArrayContains
                );
                if is_cmp {
                    if contains_decimal(&left.data_type()) || contains_decimal(&right.data_type()) {
                        return false;
                    }
                    stack.push(right);
                    stack.push(left);
                } else if matches!(op, BinaryOp::And | BinaryOp::Or) {
                    stack.push(right);
                    stack.push(left);
                } else if *op == BinaryOp::Div && *data_type == DataType::F64 {
                    let valid_divisor = matches!(left.data_type(), DataType::F64 | DataType::Null)
                        && matches!(
                            right.as_ref(),
                            Expr::Literal(ScalarValue::F64(v)) if v.is_finite() && *v != 0.0
                        );
                    if !valid_divisor {
                        return false;
                    }
                    stack.push(left);
                } else {
                    return false;
                }
            }
            Expr::Cast {
                expr: inner,
                target,
                mode,
            } => {
                let lossless_total = matches!(
                    (inner.data_type(), target),
                    (DataType::I64, DataType::F64)
                        | (DataType::TimestampNs { .. }, DataType::I64)
                        | (DataType::I64, DataType::TimestampNs { .. })
                        | (DataType::TimestampNs { .. }, DataType::TimestampNs { .. })
                        | (
                            DataType::FixedBytes8 | DataType::FixedBytes16,
                            DataType::Binary
                        )
                        | (DataType::Decimal128 { .. }, DataType::F64)
                );
                if *mode != CastMode::Try && !lossless_total {
                    return false;
                }
                stack.push(inner);
            }
            Expr::Between {
                expr, low, high, ..
            } => {
                if contains_decimal(&expr.data_type())
                    || contains_decimal(&low.data_type())
                    || contains_decimal(&high.data_type())
                {
                    return false;
                }
                stack.push(high);
                stack.push(low);
                stack.push(expr);
            }
            Expr::InList { expr, list, .. } => {
                if contains_decimal(&expr.data_type())
                    || list.iter().any(|e| contains_decimal(&e.data_type()))
                {
                    return false;
                }
                for item in list {
                    stack.push(item);
                }
                stack.push(expr);
            }
            Expr::Like { .. } => return false,
            Expr::Case {
                operand,
                when_then,
                else_expr,
                ..
            } => {
                if operand.as_deref().is_some_and(|op| {
                    contains_decimal(&op.data_type())
                        || when_then
                            .iter()
                            .any(|(w, _)| contains_decimal(&w.data_type()))
                }) {
                    return false;
                }
                if let Some(el) = else_expr {
                    stack.push(el);
                }
                for (w, t) in when_then {
                    stack.push(t);
                    stack.push(w);
                }
                if let Some(op) = operand {
                    stack.push(op);
                }
            }
            Expr::MapIndex { map, key } => {
                stack.push(key);
                stack.push(map);
            }
            Expr::MakeArray { elems, .. } => {
                for e in elems {
                    stack.push(e);
                }
            }
            Expr::Call { func, args, .. } => {
                if matches!(func, ScalarFn::Encode) {
                    if args.len() != 2 {
                        return false;
                    }
                    let ok_arg = matches!(
                        args[0].data_type(),
                        DataType::FixedBytes8 | DataType::FixedBytes16 | DataType::Null
                    );
                    let ok_fmt = matches!(
                        &args[1],
                        Expr::Literal(ScalarValue::Utf8(s)) if s.eq_ignore_ascii_case("hex")
                    );
                    if !ok_arg || !ok_fmt {
                        return false;
                    }
                    stack.push(&args[0]);
                    continue;
                }
                if matches!(
                    func,
                    ScalarFn::Greatest
                        | ScalarFn::Least
                        | ScalarFn::Nullif
                        | ScalarFn::Floor
                        | ScalarFn::Ceil
                ) && args.iter().any(|a| contains_decimal(&a.data_type()))
                {
                    return false;
                }
                let safe_fn = matches!(
                    func,
                    ScalarFn::Lower
                        | ScalarFn::Upper
                        | ScalarFn::Length
                        | ScalarFn::Trim
                        | ScalarFn::Ltrim
                        | ScalarFn::Rtrim
                        | ScalarFn::StartsWith
                        | ScalarFn::EndsWith
                        | ScalarFn::Contains
                        | ScalarFn::Strpos
                        | ScalarFn::Now
                        | ScalarFn::Floor
                        | ScalarFn::Ceil
                        | ScalarFn::Coalesce
                        | ScalarFn::Nullif
                        | ScalarFn::Greatest
                        | ScalarFn::Least
                        | ScalarFn::IsNan
                        | ScalarFn::IsFinite
                        | ScalarFn::ArrayHas
                        | ScalarFn::ArrayHasAll
                        | ScalarFn::ArrayLength
                );
                if !safe_fn {
                    return false;
                }
                for a in args {
                    stack.push(a);
                }
            }
        }
    }
    true
}

// ---------------------------------------------------------------------------
// Pass 2: Filter pushdown through Project and safe Join sides
// ---------------------------------------------------------------------------

fn pass2_filter_pushdown(plan: LogicalPlan) -> Result<LogicalPlan, QueryError> {
    match plan {
        LogicalPlan::Filter { input, predicate } => {
            let conjuncts = split_conjuncts(predicate);
            push_filter(conjuncts, *input)
        }
        LogicalPlan::Project {
            input,
            exprs,
            schema,
        } => Ok(LogicalPlan::Project {
            input: Box::new(pass2_filter_pushdown(*input)?),
            exprs,
            schema,
        }),
        LogicalPlan::Aggregate {
            input,
            group_by,
            aggs,
            schema,
        } => Ok(LogicalPlan::Aggregate {
            input: Box::new(pass2_filter_pushdown(*input)?),
            group_by,
            aggs,
            schema,
        }),
        LogicalPlan::Window {
            input,
            partition_by,
            order_by,
            specs,
            schema,
        } => Ok(LogicalPlan::Window {
            input: Box::new(pass2_filter_pushdown(*input)?),
            partition_by,
            order_by,
            specs,
            schema,
        }),
        LogicalPlan::Sort { input, keys } => Ok(LogicalPlan::Sort {
            input: Box::new(pass2_filter_pushdown(*input)?),
            keys,
        }),
        LogicalPlan::Limit { input, skip, fetch } => Ok(LogicalPlan::Limit {
            input: Box::new(pass2_filter_pushdown(*input)?),
            skip,
            fetch,
        }),
        LogicalPlan::TopK {
            input,
            keys,
            limit,
            offset,
        } => Ok(LogicalPlan::TopK {
            input: Box::new(pass2_filter_pushdown(*input)?),
            keys,
            limit,
            offset,
        }),
        LogicalPlan::Distinct { input } => Ok(LogicalPlan::Distinct {
            input: Box::new(pass2_filter_pushdown(*input)?),
        }),
        LogicalPlan::Join {
            left,
            right,
            kind,
            left_keys,
            right_keys,
            null_equals_null,
            residual,
            schema,
        } => Ok(LogicalPlan::Join {
            left: Box::new(pass2_filter_pushdown(*left)?),
            right: Box::new(pass2_filter_pushdown(*right)?),
            kind,
            left_keys,
            right_keys,
            null_equals_null,
            residual,
            schema,
        }),
        LogicalPlan::Union {
            inputs,
            all,
            schema,
        } => {
            let mut out = Vec::with_capacity(inputs.len());
            for branch in inputs {
                out.push(pass2_filter_pushdown(branch)?);
            }
            Ok(LogicalPlan::Union {
                inputs: out,
                all,
                schema,
            })
        }
        LogicalPlan::Unnest {
            input,
            columns,
            preserve_nulls,
            schema,
        } => Ok(LogicalPlan::Unnest {
            input: Box::new(pass2_filter_pushdown(*input)?),
            columns,
            preserve_nulls,
            schema,
        }),
        LogicalPlan::Explain { inner, schema } => Ok(LogicalPlan::Explain {
            inner: Box::new(pass2_filter_pushdown(*inner)?),
            schema,
        }),
        leaf @ (LogicalPlan::Scan { .. }
        | LogicalPlan::Values { .. }
        | LogicalPlan::EmptyRow { .. }) => Ok(leaf),
    }
}

fn push_filter(conjuncts: Vec<Expr>, plan: LogicalPlan) -> Result<LogicalPlan, QueryError> {
    if conjuncts.is_empty() {
        return pass2_filter_pushdown(plan);
    }

    match plan {
        LogicalPlan::Filter { input, predicate } => {
            let mut combined = split_conjuncts(predicate);
            combined.extend(conjuncts);
            push_filter(combined, *input)
        }
        LogicalPlan::Project {
            input,
            exprs,
            schema,
        } => {
            let mut push_into = Vec::new();
            let mut keep_above = Vec::new();
            for conj in conjuncts {
                let ref_counts = count_expr_column_refs(&conj, exprs.len())?;
                let can_push_refs = ref_counts.iter().enumerate().all(|(idx, &cnt)| {
                    if cnt == 0 {
                        return true;
                    }
                    let proj = &exprs[idx];
                    if !is_infallible(proj) {
                        return false;
                    }
                    if cnt > 1
                        && !matches!(proj, Expr::Column { .. } | Expr::Literal(_))
                        && !is_hex_encoded_column(proj)
                    {
                        return false;
                    }
                    true
                });
                if can_push_refs
                    && (keep_above.is_empty() || is_infallible(&conj))
                    && let Some(sub) = substitute_expr(&conj, &exprs)?
                {
                    push_into.push(sub);
                } else {
                    keep_above.push(conj);
                }
            }
            let pushed_input = if push_into.is_empty() {
                pass2_filter_pushdown(*input)?
            } else {
                push_filter(push_into, *input)?
            };
            let proj = LogicalPlan::Project {
                input: Box::new(pushed_input),
                exprs,
                schema,
            };
            wrap_filter(proj, keep_above)
        }
        LogicalPlan::Sort { input, keys } => {
            if keys.iter().all(|k| is_infallible(&k.expr)) {
                let pushed_input = push_filter(conjuncts, *input)?;
                Ok(LogicalPlan::Sort {
                    input: Box::new(pushed_input),
                    keys,
                })
            } else {
                let inner = pass2_filter_pushdown(*input)?;
                wrap_filter(
                    LogicalPlan::Sort {
                        input: Box::new(inner),
                        keys,
                    },
                    conjuncts,
                )
            }
        }
        LogicalPlan::Distinct { input } => {
            let pushed_input = push_filter(conjuncts, *input)?;
            Ok(LogicalPlan::Distinct {
                input: Box::new(pushed_input),
            })
        }
        LogicalPlan::Union {
            inputs,
            all,
            schema,
        } => {
            let mut refs = BTreeSet::new();
            for c in &conjuncts {
                c.referenced_columns(&mut refs);
            }
            let can_push_union = inputs.iter().all(|branch| {
                let bs = branch.schema();
                refs.iter().all(|&idx| {
                    idx < bs.len()
                        && idx < schema.len()
                        && bs.fields()[idx].data_type == schema.fields()[idx].data_type
                })
            });
            if !can_push_union {
                let opt = pass2_filter_pushdown(LogicalPlan::Union {
                    inputs,
                    all,
                    schema,
                })?;
                return wrap_filter(opt, conjuncts);
            }
            let mut out = Vec::with_capacity(inputs.len());
            for branch in inputs {
                let branch_schema = Arc::clone(branch.schema());
                let retagged: Vec<Expr> = conjuncts
                    .iter()
                    .map(|c| retag_expr_columns(c, &branch_schema))
                    .collect();
                out.push(push_filter(retagged, branch)?);
            }
            Ok(LogicalPlan::Union {
                inputs: out,
                all,
                schema,
            })
        }
        LogicalPlan::Join {
            left,
            right,
            kind,
            left_keys,
            right_keys,
            null_equals_null,
            residual,
            schema,
        } => {
            let left_len = left.schema().len();
            let mut left_push = Vec::new();
            let mut right_push = Vec::new();
            let mut keep_above = Vec::new();

            for conj in conjuncts {
                if !is_infallible(&conj) {
                    keep_above.push(conj);
                    continue;
                }
                let mut cols = BTreeSet::new();
                conj.referenced_columns(&mut cols);
                if cols.is_empty() {
                    match kind {
                        LogicalJoinKind::Inner
                        | LogicalJoinKind::Cross
                        | LogicalJoinKind::Left
                        | LogicalJoinKind::Semi
                        | LogicalJoinKind::Anti
                        | LogicalJoinKind::AntiNullAware => left_push.push(conj),
                        LogicalJoinKind::Right => right_push.push(conj),
                        LogicalJoinKind::Full => keep_above.push(conj),
                    }
                } else if cols.iter().all(|&i| i < left_len) {
                    match kind {
                        LogicalJoinKind::Inner
                        | LogicalJoinKind::Cross
                        | LogicalJoinKind::Left
                        | LogicalJoinKind::Semi
                        | LogicalJoinKind::Anti
                        | LogicalJoinKind::AntiNullAware => left_push.push(conj),
                        LogicalJoinKind::Right | LogicalJoinKind::Full => keep_above.push(conj),
                    }
                } else if cols.iter().all(|&i| i >= left_len)
                    && matches!(
                        kind,
                        LogicalJoinKind::Inner | LogicalJoinKind::Cross | LogicalJoinKind::Right
                    )
                {
                    let right_len = right.schema().len();
                    let mut map = vec![None; left_len + right_len];
                    for r_idx in 0..right_len {
                        map[left_len + r_idx] = Some(r_idx);
                    }
                    right_push.push(conj.remap_columns(&map)?);
                } else {
                    keep_above.push(conj);
                }
            }

            let new_left = if left_push.is_empty() {
                pass2_filter_pushdown(*left)?
            } else {
                push_filter(left_push, *left)?
            };
            let new_right = if right_push.is_empty() {
                pass2_filter_pushdown(*right)?
            } else {
                push_filter(right_push, *right)?
            };

            let join = LogicalPlan::Join {
                left: Box::new(new_left),
                right: Box::new(new_right),
                kind,
                left_keys,
                right_keys,
                null_equals_null,
                residual,
                schema,
            };
            wrap_filter(join, keep_above)
        }
        // NEVER push below Window, Aggregate, Limit, TopK, Unnest; leave above Scan/Values/EmptyRow for Pass 4.
        other => {
            let opt = pass2_filter_pushdown(other)?;
            wrap_filter(opt, conjuncts)
        }
    }
}

fn wrap_filter(input: LogicalPlan, conjuncts: Vec<Expr>) -> Result<LogicalPlan, QueryError> {
    match combine_and(conjuncts) {
        Some(predicate) => Ok(LogicalPlan::Filter {
            input: Box::new(input),
            predicate,
        }),
        None => Ok(input),
    }
}

fn count_expr_column_refs(expr: &Expr, num_cols: usize) -> Result<Vec<usize>, QueryError> {
    let mut counts = vec![0usize; num_cols];
    let mut stack = vec![expr];
    while let Some(node) = stack.pop() {
        match node {
            Expr::Column { index, .. } => {
                let slot = counts.get_mut(*index).ok_or_else(|| {
                    QueryError::internal(format!(
                        "filter column index {index} out of bounds for projection ({num_cols} cols)"
                    ))
                })?;
                *slot = slot.saturating_add(1);
            }
            Expr::Literal(_) => {}
            Expr::Unary { expr, .. } | Expr::Cast { expr, .. } => stack.push(expr),
            Expr::Binary { left, right, .. } => {
                stack.push(right);
                stack.push(left);
            }
            Expr::Between {
                expr, low, high, ..
            } => {
                stack.push(high);
                stack.push(low);
                stack.push(expr);
            }
            Expr::InList {
                expr,
                list,
                redacted,
                ..
            } => {
                if !redacted {
                    for item in list {
                        stack.push(item);
                    }
                }
                stack.push(expr);
            }
            Expr::Like { expr, pattern, .. } => {
                stack.push(pattern);
                stack.push(expr);
            }
            Expr::Case {
                operand,
                when_then,
                else_expr,
                ..
            } => {
                if let Some(el) = else_expr {
                    stack.push(el);
                }
                for (w, t) in when_then {
                    stack.push(t);
                    stack.push(w);
                }
                if let Some(op) = operand {
                    stack.push(op);
                }
            }
            Expr::MapIndex { map, key } => {
                stack.push(key);
                stack.push(map);
            }
            Expr::MakeArray { elems, .. } => {
                for e in elems {
                    stack.push(e);
                }
            }
            Expr::Call { args, .. } => {
                for a in args {
                    stack.push(a);
                }
            }
        }
    }
    Ok(counts)
}

fn expr_node_count(expr: &Expr) -> usize {
    let mut count = 0usize;
    let mut stack = vec![expr];
    while let Some(node) = stack.pop() {
        count = count.saturating_add(1);
        match node {
            Expr::Column { .. } | Expr::Literal(_) => {}
            Expr::Unary { expr, .. } | Expr::Cast { expr, .. } => stack.push(expr),
            Expr::Binary { left, right, .. } => {
                stack.push(right);
                stack.push(left);
            }
            Expr::Between {
                expr, low, high, ..
            } => {
                stack.push(high);
                stack.push(low);
                stack.push(expr);
            }
            Expr::InList {
                expr,
                list,
                redacted,
                ..
            } => {
                if !redacted {
                    for item in list {
                        stack.push(item);
                    }
                }
                stack.push(expr);
            }
            Expr::Like { expr, pattern, .. } => {
                stack.push(pattern);
                stack.push(expr);
            }
            Expr::Case {
                operand,
                when_then,
                else_expr,
                ..
            } => {
                if let Some(el) = else_expr {
                    stack.push(el);
                }
                for (w, t) in when_then {
                    stack.push(t);
                    stack.push(w);
                }
                if let Some(op) = operand {
                    stack.push(op);
                }
            }
            Expr::MapIndex { map, key } => {
                stack.push(key);
                stack.push(map);
            }
            Expr::MakeArray { elems, .. } => {
                for e in elems {
                    stack.push(e);
                }
            }
            Expr::Call { args, .. } => {
                for a in args {
                    stack.push(a);
                }
            }
        }
    }
    count
}

fn substitute_expr(expr: &Expr, project_exprs: &[Expr]) -> Result<Option<Expr>, QueryError> {
    enum SubTask<'a> {
        Visit(&'a Expr),
        BuildUnary {
            op: UnaryOp,
            data_type: DataType,
        },
        BuildBinary {
            op: BinaryOp,
            data_type: DataType,
        },
        BuildCast {
            target: DataType,
            mode: CastMode,
        },
        BuildBetween {
            negated: bool,
        },
        BuildInList {
            n: usize,
            negated: bool,
        },
        BuildInListRedacted {
            list: &'a [Expr],
            negated: bool,
        },
        BuildLike {
            escape: Option<char>,
            case_insensitive: bool,
            negated: bool,
        },
        BuildCase {
            has_op: bool,
            n_wt: usize,
            has_else: bool,
            data_type: DataType,
        },
        BuildMapIndex,
        BuildMakeArray {
            n: usize,
            item_type: DataType,
        },
        BuildCall {
            func: ScalarFn,
            n: usize,
            data_type: DataType,
        },
    }

    let mut nodes_used = 0usize;
    let mut tasks = vec![SubTask::Visit(expr)];
    let mut vals: Vec<Expr> = Vec::new();

    while let Some(task) = tasks.pop() {
        match task {
            SubTask::Visit(node) => match node {
                Expr::Column { index, .. } => {
                    let repl = project_exprs.get(*index).ok_or_else(|| {
                        QueryError::internal(format!(
                            "filter column index {index} out of bounds for projection ({} cols)",
                            project_exprs.len()
                        ))
                    })?;
                    nodes_used = nodes_used.saturating_add(expr_node_count(repl));
                    if nodes_used > MAX_SUBSTITUTED_EXPR_NODES {
                        return Ok(None);
                    }
                    vals.push(repl.clone());
                }
                Expr::Literal(v) => {
                    nodes_used = nodes_used.saturating_add(1);
                    if nodes_used > MAX_SUBSTITUTED_EXPR_NODES {
                        return Ok(None);
                    }
                    vals.push(Expr::Literal(v.clone()));
                }
                Expr::Unary {
                    op,
                    expr: inner,
                    data_type,
                } => {
                    nodes_used = nodes_used.saturating_add(1);
                    if nodes_used > MAX_SUBSTITUTED_EXPR_NODES {
                        return Ok(None);
                    }
                    tasks.push(SubTask::BuildUnary {
                        op: *op,
                        data_type: data_type.clone(),
                    });
                    tasks.push(SubTask::Visit(inner));
                }
                Expr::Binary {
                    left,
                    op,
                    right,
                    data_type,
                } => {
                    nodes_used = nodes_used.saturating_add(1);
                    if nodes_used > MAX_SUBSTITUTED_EXPR_NODES {
                        return Ok(None);
                    }
                    tasks.push(SubTask::BuildBinary {
                        op: *op,
                        data_type: data_type.clone(),
                    });
                    tasks.push(SubTask::Visit(right));
                    tasks.push(SubTask::Visit(left));
                }
                Expr::Cast {
                    expr: inner,
                    target,
                    mode,
                } => {
                    nodes_used = nodes_used.saturating_add(1);
                    if nodes_used > MAX_SUBSTITUTED_EXPR_NODES {
                        return Ok(None);
                    }
                    tasks.push(SubTask::BuildCast {
                        target: target.clone(),
                        mode: *mode,
                    });
                    tasks.push(SubTask::Visit(inner));
                }
                Expr::Between {
                    expr: inner,
                    low,
                    high,
                    negated,
                } => {
                    nodes_used = nodes_used.saturating_add(1);
                    if nodes_used > MAX_SUBSTITUTED_EXPR_NODES {
                        return Ok(None);
                    }
                    tasks.push(SubTask::BuildBetween { negated: *negated });
                    tasks.push(SubTask::Visit(high));
                    tasks.push(SubTask::Visit(low));
                    tasks.push(SubTask::Visit(inner));
                }
                Expr::InList {
                    expr: inner,
                    list,
                    negated,
                    redacted,
                } => {
                    nodes_used = nodes_used.saturating_add(1);
                    if nodes_used > MAX_SUBSTITUTED_EXPR_NODES {
                        return Ok(None);
                    }
                    if *redacted {
                        tasks.push(SubTask::BuildInListRedacted {
                            list,
                            negated: *negated,
                        });
                        tasks.push(SubTask::Visit(inner));
                    } else {
                        tasks.push(SubTask::BuildInList {
                            n: list.len(),
                            negated: *negated,
                        });
                        for item in list.iter().rev() {
                            tasks.push(SubTask::Visit(item));
                        }
                        tasks.push(SubTask::Visit(inner));
                    }
                }
                Expr::Like {
                    expr: inner,
                    pattern,
                    escape,
                    case_insensitive,
                    negated,
                } => {
                    nodes_used = nodes_used.saturating_add(1);
                    if nodes_used > MAX_SUBSTITUTED_EXPR_NODES {
                        return Ok(None);
                    }
                    tasks.push(SubTask::BuildLike {
                        escape: *escape,
                        case_insensitive: *case_insensitive,
                        negated: *negated,
                    });
                    tasks.push(SubTask::Visit(pattern));
                    tasks.push(SubTask::Visit(inner));
                }
                Expr::Case {
                    operand,
                    when_then,
                    else_expr,
                    data_type,
                } => {
                    nodes_used = nodes_used.saturating_add(1);
                    if nodes_used > MAX_SUBSTITUTED_EXPR_NODES {
                        return Ok(None);
                    }
                    tasks.push(SubTask::BuildCase {
                        has_op: operand.is_some(),
                        n_wt: when_then.len(),
                        has_else: else_expr.is_some(),
                        data_type: data_type.clone(),
                    });
                    if let Some(el) = else_expr {
                        tasks.push(SubTask::Visit(el));
                    }
                    for (w, t) in when_then.iter().rev() {
                        tasks.push(SubTask::Visit(t));
                        tasks.push(SubTask::Visit(w));
                    }
                    if let Some(op) = operand {
                        tasks.push(SubTask::Visit(op));
                    }
                }
                Expr::MapIndex { map, key } => {
                    nodes_used = nodes_used.saturating_add(1);
                    if nodes_used > MAX_SUBSTITUTED_EXPR_NODES {
                        return Ok(None);
                    }
                    tasks.push(SubTask::BuildMapIndex);
                    tasks.push(SubTask::Visit(key));
                    tasks.push(SubTask::Visit(map));
                }
                Expr::MakeArray { elems, item_type } => {
                    nodes_used = nodes_used.saturating_add(1);
                    if nodes_used > MAX_SUBSTITUTED_EXPR_NODES {
                        return Ok(None);
                    }
                    tasks.push(SubTask::BuildMakeArray {
                        n: elems.len(),
                        item_type: item_type.clone(),
                    });
                    for e in elems.iter().rev() {
                        tasks.push(SubTask::Visit(e));
                    }
                }
                Expr::Call {
                    func,
                    args,
                    data_type,
                } => {
                    nodes_used = nodes_used.saturating_add(1);
                    if nodes_used > MAX_SUBSTITUTED_EXPR_NODES {
                        return Ok(None);
                    }
                    tasks.push(SubTask::BuildCall {
                        func: *func,
                        n: args.len(),
                        data_type: data_type.clone(),
                    });
                    for a in args.iter().rev() {
                        tasks.push(SubTask::Visit(a));
                    }
                }
            },
            SubTask::BuildUnary { op, data_type } => {
                let inner = vals.pop().ok_or_else(|| {
                    QueryError::internal("substitute_expr stack underflow on Unary")
                })?;
                vals.push(Expr::Unary {
                    op,
                    expr: Box::new(inner),
                    data_type,
                });
            }
            SubTask::BuildBinary { op, data_type } => {
                let right = vals.pop().ok_or_else(|| {
                    QueryError::internal("substitute_expr stack underflow on Binary right")
                })?;
                let left = vals.pop().ok_or_else(|| {
                    QueryError::internal("substitute_expr stack underflow on Binary left")
                })?;
                vals.push(Expr::Binary {
                    left: Box::new(left),
                    op,
                    right: Box::new(right),
                    data_type,
                });
            }
            SubTask::BuildCast { target, mode } => {
                let inner = vals.pop().ok_or_else(|| {
                    QueryError::internal("substitute_expr stack underflow on Cast")
                })?;
                vals.push(Expr::Cast {
                    expr: Box::new(inner),
                    target,
                    mode,
                });
            }
            SubTask::BuildBetween { negated } => {
                let high = vals.pop().ok_or_else(|| {
                    QueryError::internal("substitute_expr stack underflow on Between high")
                })?;
                let low = vals.pop().ok_or_else(|| {
                    QueryError::internal("substitute_expr stack underflow on Between low")
                })?;
                let inner = vals.pop().ok_or_else(|| {
                    QueryError::internal("substitute_expr stack underflow on Between inner")
                })?;
                vals.push(Expr::Between {
                    expr: Box::new(inner),
                    low: Box::new(low),
                    high: Box::new(high),
                    negated,
                });
            }
            SubTask::BuildInList { n, negated } => {
                let start = vals.len().saturating_sub(n);
                let list: Vec<Expr> = vals.drain(start..).collect();
                let inner = vals.pop().ok_or_else(|| {
                    QueryError::internal("substitute_expr stack underflow on InList")
                })?;
                vals.push(Expr::InList {
                    expr: Box::new(inner),
                    list,
                    negated,
                    redacted: false,
                });
            }
            SubTask::BuildInListRedacted { list, negated } => {
                let inner = vals.pop().ok_or_else(|| {
                    QueryError::internal("substitute_expr stack underflow on InListRedacted")
                })?;
                vals.push(Expr::InList {
                    expr: Box::new(inner),
                    list: list.to_vec(),
                    negated,
                    redacted: true,
                });
            }
            SubTask::BuildLike {
                escape,
                case_insensitive,
                negated,
            } => {
                let pattern = vals.pop().ok_or_else(|| {
                    QueryError::internal("substitute_expr stack underflow on Like pattern")
                })?;
                let inner = vals.pop().ok_or_else(|| {
                    QueryError::internal("substitute_expr stack underflow on Like inner")
                })?;
                vals.push(Expr::Like {
                    expr: Box::new(inner),
                    pattern: Box::new(pattern),
                    escape,
                    case_insensitive,
                    negated,
                });
            }
            SubTask::BuildCase {
                has_op,
                n_wt,
                has_else,
                data_type,
            } => {
                let else_expr = if has_else {
                    Some(Box::new(vals.pop().ok_or_else(|| {
                        QueryError::internal("substitute_expr stack underflow on Case else")
                    })?))
                } else {
                    None
                };
                let wt_start = vals.len().saturating_sub(n_wt * 2);
                let mut when_then = Vec::with_capacity(n_wt);
                let mut drained = vals.drain(wt_start..);
                while let (Some(w), Some(t)) = (drained.next(), drained.next()) {
                    when_then.push((w, t));
                }
                drop(drained);
                let operand = if has_op {
                    Some(Box::new(vals.pop().ok_or_else(|| {
                        QueryError::internal("substitute_expr stack underflow on Case operand")
                    })?))
                } else {
                    None
                };
                vals.push(Expr::Case {
                    operand,
                    when_then,
                    else_expr,
                    data_type,
                });
            }
            SubTask::BuildMapIndex => {
                let key = vals.pop().ok_or_else(|| {
                    QueryError::internal("substitute_expr stack underflow on MapIndex key")
                })?;
                let map = vals.pop().ok_or_else(|| {
                    QueryError::internal("substitute_expr stack underflow on MapIndex map")
                })?;
                vals.push(Expr::MapIndex {
                    map: Box::new(map),
                    key: Box::new(key),
                });
            }
            SubTask::BuildMakeArray { n, item_type } => {
                let start = vals.len().saturating_sub(n);
                let elems: Vec<Expr> = vals.drain(start..).collect();
                vals.push(Expr::MakeArray { elems, item_type });
            }
            SubTask::BuildCall { func, n, data_type } => {
                let start = vals.len().saturating_sub(n);
                let args: Vec<Expr> = vals.drain(start..).collect();
                vals.push(Expr::Call {
                    func,
                    args,
                    data_type,
                });
            }
        }
    }

    let out = vals
        .pop()
        .ok_or_else(|| QueryError::internal("substitute_expr missing result"))?;
    Ok(Some(out))
}

fn retag_expr_columns(expr: &Expr, schema: &Schema) -> Expr {
    map_expr_bottom_up(expr.clone(), &mut |e| match e {
        Expr::Column {
            index,
            name,
            data_type,
        } => match schema.fields().get(index) {
            Some(f) => Expr::Column {
                index,
                name: f.name.clone(),
                data_type: f.data_type.clone(),
            },
            None => Expr::Column {
                index,
                name,
                data_type,
            },
        },
        other => other,
    })
}

fn split_conjuncts(expr: Expr) -> Vec<Expr> {
    let mut out = Vec::new();
    split_conjuncts_into(expr, &mut out);
    out
}

fn split_conjuncts_into(expr: Expr, out: &mut Vec<Expr>) {
    let mut stack = vec![expr];
    while let Some(node) = stack.pop() {
        match node {
            Expr::Binary {
                left,
                op: BinaryOp::And,
                right,
                ..
            } => {
                stack.push(*right);
                stack.push(*left);
            }
            other => out.push(other),
        }
    }
}

fn combine_and(conjuncts: Vec<Expr>) -> Option<Expr> {
    let mut iter = conjuncts.into_iter();
    let first = iter.next()?;
    Some(iter.fold(first, |acc, rhs| Expr::Binary {
        left: Box::new(acc),
        op: BinaryOp::And,
        right: Box::new(rhs),
        data_type: DataType::Bool,
    }))
}

// ---------------------------------------------------------------------------
// Pass 3: Hex-literal ID rewrite (D89, matching `views/hex.rs`)
// ---------------------------------------------------------------------------

fn pass3_hex_id_rewrite(plan: LogicalPlan) -> Result<LogicalPlan, QueryError> {
    map_plan_exprs(plan, &mut rewrite_hex_expr)
}

fn rewrite_hex_expr(expr: Expr) -> Expr {
    map_expr_bottom_up(expr, &mut |node| match node {
        Expr::Binary {
            left,
            op: op @ (BinaryOp::Eq | BinaryOp::NotEq),
            right,
            data_type,
        } => match (encoded_hex_id(&left), encoded_hex_id(&right)) {
            (Some((col, width)), None) => {
                if let Some(lit) = hex_literal(&right, width) {
                    Expr::Binary {
                        left: Box::new(col),
                        op,
                        right: Box::new(Expr::Literal(lit)),
                        data_type: DataType::Bool,
                    }
                } else {
                    Expr::Binary {
                        left,
                        op,
                        right,
                        data_type,
                    }
                }
            }
            (None, Some((col, width))) => {
                if let Some(lit) = hex_literal(&left, width) {
                    Expr::Binary {
                        left: Box::new(col),
                        op,
                        right: Box::new(Expr::Literal(lit)),
                        data_type: DataType::Bool,
                    }
                } else {
                    Expr::Binary {
                        left,
                        op,
                        right,
                        data_type,
                    }
                }
            }
            (Some((l_col, lw)), Some((r_col, rw))) if lw == rw => Expr::Binary {
                left: Box::new(l_col),
                op,
                right: Box::new(r_col),
                data_type: DataType::Bool,
            },
            _ => Expr::Binary {
                left,
                op,
                right,
                data_type,
            },
        },
        Expr::InList {
            expr: inner,
            list,
            negated,
            redacted: false,
        } => {
            if let Some((col, width)) = encoded_hex_id(&inner)
                && !list.is_empty()
            {
                let mut fixed = Vec::with_capacity(list.len());
                let mut all_valid = true;
                for item in &list {
                    if let Some(lit) = hex_literal(item, width) {
                        fixed.push(Expr::Literal(lit));
                    } else {
                        all_valid = false;
                        break;
                    }
                }
                if all_valid {
                    return Expr::InList {
                        expr: Box::new(col),
                        list: fixed,
                        negated,
                        redacted: false,
                    };
                }
            }
            Expr::InList {
                expr: inner,
                list,
                negated,
                redacted: false,
            }
        }
        other => other,
    })
}

fn is_hex_encoded_column(expr: &Expr) -> bool {
    matches!(
        encoded_hex_id(expr),
        Some((
            Expr::Column {
                data_type: DataType::FixedBytes8 | DataType::FixedBytes16,
                ..
            },
            _,
        ))
    )
}

fn encoded_hex_id(expr: &Expr) -> Option<(Expr, usize)> {
    let Expr::Call {
        func: ScalarFn::Encode,
        args,
        ..
    } = expr
    else {
        return None;
    };
    let [inner, format] = args.as_slice() else {
        return None;
    };
    let Expr::Literal(ScalarValue::Utf8(fmt)) = format else {
        return None;
    };
    if !fmt.eq_ignore_ascii_case("hex") {
        return None;
    }
    let width = match inner.data_type() {
        DataType::FixedBytes8 => 8,
        DataType::FixedBytes16 => 16,
        _ => return None,
    };
    Some((inner.clone(), width))
}

fn hex_literal(expr: &Expr, width: usize) -> Option<ScalarValue> {
    let Expr::Literal(ScalarValue::Utf8(text)) = expr else {
        return None;
    };
    let expected = width.checked_mul(2)?;
    // `encode(id, 'hex')` always emits lowercase hex; uppercase literals never match it,
    // so rewriting them to raw byte equality would turn `false` into `true`.
    if text.len() != expected || !text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        return None;
    }
    let bytes = hex::decode(text).ok()?;
    match width {
        8 => Some(ScalarValue::FixedBytes8(bytes.try_into().ok()?)),
        16 => Some(ScalarValue::FixedBytes16(bytes.try_into().ok()?)),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Pass 4: Storage-safe Scan filter pushdown & hint extraction
// ---------------------------------------------------------------------------

fn pass4_scan_pushdown(plan: LogicalPlan) -> Result<LogicalPlan, QueryError> {
    match plan {
        LogicalPlan::Filter { input, predicate } => match *input {
            LogicalPlan::Scan {
                table,
                kind,
                full_schema,
                projection,
                mut filters,
                mut time_range,
                mut id_lookups,
                mut substrings,
                map_projections,
                mut limit,
                schema,
            } => {
                let proj_to_full = projection.as_ref().map(|proj| {
                    proj.iter()
                        .copied()
                        .map(Some)
                        .collect::<Vec<Option<usize>>>()
                });

                let mut keep_above = Vec::new();
                for conj in split_conjuncts(predicate) {
                    let full_conj = match &proj_to_full {
                        Some(map) => conj.remap_columns(map)?,
                        None => conj.clone(),
                    };
                    if is_scan_filter_safe(kind, &full_conj, &full_schema)
                        && (keep_above.is_empty() || is_infallible(&full_conj))
                    {
                        filters.push(full_conj);
                    } else {
                        keep_above.push(conj);
                    }
                }
                if filters.iter().any(|f| is_bool_lit(f, false))
                    && filters.iter().all(is_infallible)
                    && keep_above.iter().all(is_infallible)
                {
                    limit = Some(0);
                }

                extract_scan_hints(
                    kind,
                    &full_schema,
                    &filters,
                    &mut time_range,
                    &mut id_lookups,
                    &mut substrings,
                );

                let scan = LogicalPlan::Scan {
                    table,
                    kind,
                    full_schema,
                    projection,
                    filters,
                    time_range,
                    id_lookups,
                    substrings,
                    map_projections,
                    limit,
                    schema,
                };
                wrap_filter(scan, keep_above)
            }
            other => {
                let opt = pass4_scan_pushdown(other)?;
                Ok(LogicalPlan::Filter {
                    input: Box::new(opt),
                    predicate,
                })
            }
        },
        LogicalPlan::Scan {
            table,
            kind,
            full_schema,
            projection,
            filters,
            mut time_range,
            mut id_lookups,
            mut substrings,
            map_projections,
            mut limit,
            schema,
        } => {
            if filters.iter().any(|f| is_bool_lit(f, false)) && filters.iter().all(is_infallible) {
                limit = Some(0);
            }
            extract_scan_hints(
                kind,
                &full_schema,
                &filters,
                &mut time_range,
                &mut id_lookups,
                &mut substrings,
            );
            Ok(LogicalPlan::Scan {
                table,
                kind,
                full_schema,
                projection,
                filters,
                time_range,
                id_lookups,
                substrings,
                map_projections,
                limit,
                schema,
            })
        }
        LogicalPlan::Project {
            input,
            exprs,
            schema,
        } => Ok(LogicalPlan::Project {
            input: Box::new(pass4_scan_pushdown(*input)?),
            exprs,
            schema,
        }),
        LogicalPlan::Aggregate {
            input,
            group_by,
            aggs,
            schema,
        } => Ok(LogicalPlan::Aggregate {
            input: Box::new(pass4_scan_pushdown(*input)?),
            group_by,
            aggs,
            schema,
        }),
        LogicalPlan::Window {
            input,
            partition_by,
            order_by,
            specs,
            schema,
        } => Ok(LogicalPlan::Window {
            input: Box::new(pass4_scan_pushdown(*input)?),
            partition_by,
            order_by,
            specs,
            schema,
        }),
        LogicalPlan::Sort { input, keys } => Ok(LogicalPlan::Sort {
            input: Box::new(pass4_scan_pushdown(*input)?),
            keys,
        }),
        LogicalPlan::Limit { input, skip, fetch } => Ok(LogicalPlan::Limit {
            input: Box::new(pass4_scan_pushdown(*input)?),
            skip,
            fetch,
        }),
        LogicalPlan::TopK {
            input,
            keys,
            limit,
            offset,
        } => Ok(LogicalPlan::TopK {
            input: Box::new(pass4_scan_pushdown(*input)?),
            keys,
            limit,
            offset,
        }),
        LogicalPlan::Distinct { input } => Ok(LogicalPlan::Distinct {
            input: Box::new(pass4_scan_pushdown(*input)?),
        }),
        LogicalPlan::Join {
            left,
            right,
            kind,
            left_keys,
            right_keys,
            null_equals_null,
            residual,
            schema,
        } => Ok(LogicalPlan::Join {
            left: Box::new(pass4_scan_pushdown(*left)?),
            right: Box::new(pass4_scan_pushdown(*right)?),
            kind,
            left_keys,
            right_keys,
            null_equals_null,
            residual,
            schema,
        }),
        LogicalPlan::Union {
            inputs,
            all,
            schema,
        } => {
            let mut out = Vec::with_capacity(inputs.len());
            for branch in inputs {
                out.push(pass4_scan_pushdown(branch)?);
            }
            Ok(LogicalPlan::Union {
                inputs: out,
                all,
                schema,
            })
        }
        LogicalPlan::Unnest {
            input,
            columns,
            preserve_nulls,
            schema,
        } => Ok(LogicalPlan::Unnest {
            input: Box::new(pass4_scan_pushdown(*input)?),
            columns,
            preserve_nulls,
            schema,
        }),
        LogicalPlan::Explain { inner, schema } => Ok(LogicalPlan::Explain {
            inner: Box::new(pass4_scan_pushdown(*inner)?),
            schema,
        }),
        leaf @ (LogicalPlan::Values { .. } | LogicalPlan::EmptyRow { .. }) => Ok(leaf),
    }
}

fn is_scan_filter_safe(kind: SqlViewKind, expr: &Expr, full_schema: &Schema) -> bool {
    match kind {
        SqlViewKind::Spans | SqlViewKind::Batch => true,
        SqlViewKind::Traces => {
            let mut cols = BTreeSet::new();
            expr.referenced_columns(&mut cols);
            !cols.is_empty()
                && is_infallible(expr)
                && cols.iter().all(|&idx| {
                    full_schema
                        .fields()
                        .get(idx)
                        .is_some_and(|f| f.name == "start_time" || f.name == "trace_id")
                })
        }
        SqlViewKind::Scores => {
            let mut cols = BTreeSet::new();
            expr.referenced_columns(&mut cols);
            !cols.is_empty()
                && is_infallible(expr)
                && cols.iter().all(|&idx| {
                    full_schema
                        .fields()
                        .get(idx)
                        .is_some_and(|f| matches!(f.name.as_str(), "created_at" | "score_id"))
                })
        }
        SqlViewKind::Sessions => false,
    }
}

fn extract_scan_hints(
    kind: SqlViewKind,
    full_schema: &Schema,
    filters: &[Expr],
    time_range: &mut TimeRangeHint,
    id_lookups: &mut Vec<IdLookupHint>,
    substrings: &mut Vec<Vec<SubstringHint>>,
) {
    let time_col = match kind {
        SqlViewKind::Spans | SqlViewKind::Traces => Some("start_time"),
        SqlViewKind::Scores => Some("created_at"),
        SqlViewKind::Batch | SqlViewKind::Sessions => None,
    };
    if let Some(col) = time_col {
        let extracted = extract_time_range(filters, full_schema, col);
        *time_range = intersect_time_range(*time_range, extracted);
    }

    for filter in filters {
        extract_id_lookups_from_expr(filter, full_schema, id_lookups);
        extract_substrings_from_expr(filter, full_schema, substrings);
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct TimeBounds {
    min: Option<i64>,
    max: Option<i64>,
}

impl TimeBounds {
    fn raise_min(&mut self, value: i64) {
        self.min = Some(self.min.map_or(value, |cur| cur.max(value)));
    }

    fn lower_max(&mut self, value: i64) {
        self.max = Some(self.max.map_or(value, |cur| cur.min(value)));
    }

    fn force_empty(&mut self) {
        self.min = Some(0);
        self.max = Some(-1);
    }

    fn intersect_with(self, other: Self) -> Self {
        Self {
            min: match (self.min, other.min) {
                (Some(a), Some(b)) => Some(a.max(b)),
                (Some(a), None) | (None, Some(a)) => Some(a),
                (None, None) => None,
            },
            max: match (self.max, other.max) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (Some(a), None) | (None, Some(a)) => Some(a),
                (None, None) => None,
            },
        }
    }

    fn union_with(self, other: Self) -> Self {
        Self {
            min: self.min.zip(other.min).map(|(a, b)| a.min(b)),
            max: self.max.zip(other.max).map(|(a, b)| a.max(b)),
        }
    }
}

fn intersect_time_range(a: TimeRangeHint, b: TimeRangeHint) -> TimeRangeHint {
    let tb = TimeBounds {
        min: a.min_ns,
        max: a.max_ns,
    }
    .intersect_with(TimeBounds {
        min: b.min_ns,
        max: b.max_ns,
    });
    TimeRangeHint {
        min_ns: tb.min,
        max_ns: tb.max,
        around_ns: a.around_ns.or(b.around_ns),
    }
}

fn is_schema_column(expr: &Expr, full_schema: &Schema, expected_name: &str) -> bool {
    let Expr::Column { index, .. } = expr else {
        return false;
    };
    full_schema
        .fields()
        .get(*index)
        .is_some_and(|f| f.name == expected_name)
}

fn extract_time_range(filters: &[Expr], full_schema: &Schema, column: &str) -> TimeRangeHint {
    let mut bounds = TimeBounds::default();
    for filter in filters {
        walk_time_bounds(&mut bounds, filter, full_schema, column);
    }
    TimeRangeHint {
        min_ns: bounds.min,
        max_ns: bounds.max,
        around_ns: None,
    }
}

fn walk_time_bounds(bounds: &mut TimeBounds, expr: &Expr, full_schema: &Schema, column: &str) {
    enum TimeFrame<'a> {
        And {
            bounds: TimeBounds,
            work: Vec<&'a Expr>,
        },
        Or {
            union: Option<TimeBounds>,
            disjuncts: Vec<&'a Expr>,
        },
    }

    let mut frames = vec![TimeFrame::And {
        bounds: *bounds,
        work: vec![expr],
    }];

    while let Some(top) = frames.last_mut() {
        match top {
            TimeFrame::And { bounds: cur, work } => {
                let Some(node) = work.pop() else {
                    let Some(TimeFrame::And {
                        bounds: finished, ..
                    }) = frames.pop()
                    else {
                        break;
                    };
                    match frames.last_mut() {
                        Some(TimeFrame::Or { union, .. }) => {
                            *union = Some(match union.take() {
                                None => finished,
                                Some(acc) => acc.union_with(finished),
                            });
                        }
                        None => {
                            *bounds = finished;
                            break;
                        }
                        Some(TimeFrame::And { .. }) => {}
                    }
                    continue;
                };
                match node {
                    Expr::Binary {
                        left,
                        op: BinaryOp::And,
                        right,
                        ..
                    } => {
                        work.push(right);
                        work.push(left);
                    }
                    Expr::Binary {
                        op: BinaryOp::Or, ..
                    } => {
                        let mut disjuncts = Vec::new();
                        collect_or_disjuncts(node, &mut disjuncts);
                        disjuncts.reverse();
                        frames.push(TimeFrame::Or {
                            union: None,
                            disjuncts,
                        });
                    }
                    Expr::Binary {
                        left, op, right, ..
                    } => {
                        if is_schema_column(left.as_ref(), full_schema, column) {
                            if let Some(ns) = expr_timestamp_ns(right) {
                                apply_time_cmp(cur, *op, ns, true);
                            }
                        } else if is_schema_column(right.as_ref(), full_schema, column)
                            && let Some(ns) = expr_timestamp_ns(left)
                        {
                            apply_time_cmp(cur, *op, ns, false);
                        }
                    }
                    Expr::Between {
                        expr: inner,
                        low,
                        high,
                        negated: false,
                    } if is_schema_column(inner.as_ref(), full_schema, column) => {
                        if let Some(lo) = expr_timestamp_ns(low) {
                            cur.raise_min(lo);
                        }
                        if let Some(hi) = expr_timestamp_ns(high) {
                            cur.lower_max(hi);
                        }
                    }
                    _ => {}
                }
            }
            TimeFrame::Or {
                union: _,
                disjuncts,
            } => {
                let Some(disjunct) = disjuncts.pop() else {
                    let Some(TimeFrame::Or { union, .. }) = frames.pop() else {
                        break;
                    };
                    if let Some(u) = union
                        && let Some(TimeFrame::And {
                            bounds: parent_bounds,
                            ..
                        }) = frames.last_mut()
                    {
                        *parent_bounds = parent_bounds.intersect_with(u);
                    }
                    continue;
                };
                if let Expr::Unary {
                    op: UnaryOp::IsNull,
                    expr: inner,
                    ..
                } = disjunct
                    && is_schema_column(inner.as_ref(), full_schema, column)
                {
                    continue;
                }
                frames.push(TimeFrame::And {
                    bounds: TimeBounds::default(),
                    work: vec![disjunct],
                });
            }
        }
    }
}

fn collect_or_disjuncts<'a>(expr: &'a Expr, out: &mut Vec<&'a Expr>) {
    let mut stack = vec![expr];
    while let Some(node) = stack.pop() {
        match node {
            Expr::Binary {
                left,
                op: BinaryOp::Or,
                right,
                ..
            } => {
                stack.push(right);
                stack.push(left);
            }
            other => out.push(other),
        }
    }
}

fn apply_time_cmp(bounds: &mut TimeBounds, op: BinaryOp, lit: i64, col_left: bool) {
    let op = if col_left {
        op
    } else {
        match op {
            BinaryOp::Eq => BinaryOp::Eq,
            BinaryOp::Gt => BinaryOp::Lt,
            BinaryOp::GtEq => BinaryOp::LtEq,
            BinaryOp::Lt => BinaryOp::Gt,
            BinaryOp::LtEq => BinaryOp::GtEq,
            _ => return,
        }
    };
    match op {
        BinaryOp::Eq => {
            bounds.raise_min(lit);
            bounds.lower_max(lit);
        }
        BinaryOp::Gt => match lit.checked_add(1) {
            Some(next) => bounds.raise_min(next),
            None => bounds.force_empty(),
        },
        BinaryOp::GtEq => bounds.raise_min(lit),
        BinaryOp::Lt => match lit.checked_sub(1) {
            Some(prev) => bounds.lower_max(prev),
            None => bounds.force_empty(),
        },
        BinaryOp::LtEq => bounds.lower_max(lit),
        _ => {}
    }
}

fn expr_timestamp_ns(expr: &Expr) -> Option<i64> {
    match expr {
        Expr::Literal(ScalarValue::TimestampNs { value, .. }) => Some(*value),
        _ => None,
    }
}

fn extract_single_id_lookup(node: &Expr, full_schema: &Schema) -> Option<IdLookupHint> {
    match node {
        Expr::Binary {
            left,
            op: BinaryOp::Eq,
            right,
            ..
        } => {
            let (col_idx, lit) = match (left.as_ref(), right.as_ref()) {
                (Expr::Column { index, .. }, Expr::Literal(sv)) => (*index, sv),
                (Expr::Literal(sv), Expr::Column { index, .. }) => (*index, sv),
                _ => return None,
            };
            let field = full_schema.fields().get(col_idx)?;
            match lit {
                ScalarValue::FixedBytes16(b) if field.data_type == DataType::FixedBytes16 => {
                    Some(IdLookupHint::Fixed16 {
                        column: field.name.clone(),
                        values: vec![*b],
                    })
                }
                ScalarValue::FixedBytes8(b) if field.data_type == DataType::FixedBytes8 => {
                    Some(IdLookupHint::Fixed8 {
                        column: field.name.clone(),
                        values: vec![*b],
                    })
                }
                ScalarValue::Utf8(s)
                    if field.data_type == DataType::Utf8 && is_utf8_id_column(&field.name) =>
                {
                    Some(IdLookupHint::Utf8 {
                        column: field.name.clone(),
                        values: vec![s.clone()],
                    })
                }
                _ => None,
            }
        }
        Expr::InList {
            expr: inner,
            list,
            negated: false,
            redacted: false,
        } => {
            let Expr::Column { index, .. } = inner.as_ref() else {
                return None;
            };
            let field = full_schema.fields().get(*index)?;
            match &field.data_type {
                DataType::FixedBytes16 => {
                    let mut vals = Vec::new();
                    for item in list {
                        let Expr::Literal(sv) = item else {
                            return None;
                        };
                        match sv {
                            ScalarValue::Null(_) => {}
                            ScalarValue::FixedBytes16(b) => vals.push(*b),
                            _ => return None,
                        }
                    }
                    Some(IdLookupHint::Fixed16 {
                        column: field.name.clone(),
                        values: vals,
                    })
                }
                DataType::FixedBytes8 => {
                    let mut vals = Vec::new();
                    for item in list {
                        let Expr::Literal(sv) = item else {
                            return None;
                        };
                        match sv {
                            ScalarValue::Null(_) => {}
                            ScalarValue::FixedBytes8(b) => vals.push(*b),
                            _ => return None,
                        }
                    }
                    Some(IdLookupHint::Fixed8 {
                        column: field.name.clone(),
                        values: vals,
                    })
                }
                DataType::Utf8 if is_utf8_id_column(&field.name) => {
                    let mut vals = Vec::new();
                    for item in list {
                        let Expr::Literal(sv) = item else {
                            return None;
                        };
                        match sv {
                            ScalarValue::Null(_) => {}
                            ScalarValue::Utf8(s) => vals.push(s.clone()),
                            _ => return None,
                        }
                    }
                    Some(IdLookupHint::Utf8 {
                        column: field.name.clone(),
                        values: vals,
                    })
                }
                _ => None,
            }
        }
        _ => None,
    }
}

fn extend_unique<T: PartialEq>(dst: &mut Vec<T>, src: Vec<T>) {
    for item in src {
        if !dst.contains(&item) {
            dst.push(item);
        }
    }
}

fn extract_id_lookups_from_expr(expr: &Expr, full_schema: &Schema, out: &mut Vec<IdLookupHint>) {
    let mut stack = vec![expr];
    while let Some(node) = stack.pop() {
        match node {
            Expr::Binary {
                left,
                op: BinaryOp::And,
                right,
                ..
            } => {
                stack.push(right);
                stack.push(left);
            }
            Expr::Binary {
                left,
                op: BinaryOp::Or,
                right,
                ..
            } => {
                let mut or_stack = vec![right.as_ref(), left.as_ref()];
                let mut merged: Option<IdLookupHint> = None;
                let mut valid = true;
                while let Some(disjunct) = or_stack.pop() {
                    if let Expr::Binary {
                        left: dl,
                        op: BinaryOp::Or,
                        right: dr,
                        ..
                    } = disjunct
                    {
                        or_stack.push(dr);
                        or_stack.push(dl);
                        continue;
                    }
                    let Some(hint) = extract_single_id_lookup(disjunct, full_schema) else {
                        valid = false;
                        break;
                    };
                    match (&mut merged, hint) {
                        (None, IdLookupHint::Fixed16 { column, values }) => {
                            let mut dedup = Vec::with_capacity(values.len());
                            extend_unique(&mut dedup, values);
                            merged = Some(IdLookupHint::Fixed16 {
                                column,
                                values: dedup,
                            });
                        }
                        (None, IdLookupHint::Fixed8 { column, values }) => {
                            let mut dedup = Vec::with_capacity(values.len());
                            extend_unique(&mut dedup, values);
                            merged = Some(IdLookupHint::Fixed8 {
                                column,
                                values: dedup,
                            });
                        }
                        (None, IdLookupHint::Utf8 { column, values }) => {
                            let mut dedup = Vec::with_capacity(values.len());
                            extend_unique(&mut dedup, values);
                            merged = Some(IdLookupHint::Utf8 {
                                column,
                                values: dedup,
                            });
                        }
                        (
                            Some(IdLookupHint::Fixed16 {
                                column: c1,
                                values: v1,
                            }),
                            IdLookupHint::Fixed16 {
                                column: c2,
                                values: v2,
                            },
                        ) if *c1 == c2 => extend_unique(v1, v2),
                        (
                            Some(IdLookupHint::Fixed8 {
                                column: c1,
                                values: v1,
                            }),
                            IdLookupHint::Fixed8 {
                                column: c2,
                                values: v2,
                            },
                        ) if *c1 == c2 => extend_unique(v1, v2),
                        (
                            Some(IdLookupHint::Utf8 {
                                column: c1,
                                values: v1,
                            }),
                            IdLookupHint::Utf8 {
                                column: c2,
                                values: v2,
                            },
                        ) if *c1 == c2 => extend_unique(v1, v2),
                        _ => {
                            valid = false;
                            break;
                        }
                    }
                }
                if valid && let Some(hint) = merged {
                    push_unique_id_lookup(out, hint);
                }
            }
            _ => {
                if let Some(hint) = extract_single_id_lookup(node, full_schema) {
                    push_unique_id_lookup(out, hint);
                }
            }
        }
    }
}

fn is_utf8_id_column(name: &str) -> bool {
    matches!(name, "session_id" | "user_id" | "score_id")
}

fn push_unique_id_lookup(out: &mut Vec<IdLookupHint>, hint: IdLookupHint) {
    if !out.contains(&hint) {
        out.push(hint);
    }
}

fn extract_single_substring_hint(expr: &Expr, full_schema: &Schema) -> Option<SubstringHint> {
    match expr {
        Expr::Like {
            expr: inner,
            pattern,
            escape,
            case_insensitive,
            negated: false,
        } => {
            let Expr::Column { index, .. } = inner.as_ref() else {
                return None;
            };
            let field = full_schema.fields().get(*index)?;
            let Expr::Literal(ScalarValue::Utf8(pat)) = pattern.as_ref() else {
                return None;
            };
            let needle = longest_like_literal(pat, *escape)?;
            Some(SubstringHint {
                column: field.name.clone(),
                needle,
                case_insensitive: *case_insensitive,
            })
        }
        Expr::Call {
            func: ScalarFn::Contains | ScalarFn::StartsWith | ScalarFn::EndsWith,
            args,
            ..
        } => {
            let [
                Expr::Column { index, .. },
                Expr::Literal(ScalarValue::Utf8(needle)),
            ] = args.as_slice()
            else {
                return None;
            };
            if needle.is_empty() {
                return None;
            }
            let field = full_schema.fields().get(*index)?;
            Some(SubstringHint {
                column: field.name.clone(),
                needle: needle.clone(),
                case_insensitive: false,
            })
        }
        _ => None,
    }
}

fn extract_substrings_from_expr(
    expr: &Expr,
    full_schema: &Schema,
    out: &mut Vec<Vec<SubstringHint>>,
) {
    let mut stack = vec![expr];
    while let Some(node) = stack.pop() {
        match node {
            Expr::Binary {
                left,
                op: BinaryOp::And,
                right,
                ..
            } => {
                stack.push(right);
                stack.push(left);
            }
            Expr::Binary {
                op: BinaryOp::Or, ..
            } => {
                let mut disjuncts = Vec::new();
                collect_or_disjuncts(node, &mut disjuncts);
                let mut clause: Vec<SubstringHint> = Vec::with_capacity(disjuncts.len());
                let mut all_matched = !disjuncts.is_empty();
                for disjunct in disjuncts {
                    if let Some(hint) = extract_single_substring_hint(disjunct, full_schema) {
                        if !clause.contains(&hint) {
                            clause.push(hint);
                        }
                    } else {
                        all_matched = false;
                        break;
                    }
                }
                if all_matched && !clause.is_empty() && !out.contains(&clause) {
                    out.push(clause);
                }
            }
            other => {
                if let Some(hint) = extract_single_substring_hint(other, full_schema) {
                    let clause = vec![hint];
                    if !out.contains(&clause) {
                        out.push(clause);
                    }
                }
            }
        }
    }
}

/// Extracts the longest guaranteed literal substring from a SQL `LIKE` pattern.
fn longest_like_literal(pattern: &str, escape: Option<char>) -> Option<String> {
    let mut best = String::new();
    let mut cur = String::new();
    let mut chars = pattern.chars();
    while let Some(ch) = chars.next() {
        if escape == Some(ch) {
            if let Some(escaped) = chars.next() {
                cur.push(escaped);
            } else {
                // Trailing lone escape matches itself literally (fn_string.rs parity).
                cur.push(ch);
            }
        } else if ch == '%' || ch == '_' {
            if cur.len() > best.len() {
                best = std::mem::take(&mut cur);
            } else {
                cur.clear();
            }
        } else {
            cur.push(ch);
        }
    }
    if cur.len() > best.len() {
        best = cur;
    }
    (!best.is_empty()).then_some(best)
}

// ---------------------------------------------------------------------------
// Pass 5: Top-down projection & MapProjection pushdown
// ---------------------------------------------------------------------------

fn full_map_reqs(schema: &Schema) -> BTreeMap<usize, MapProjection> {
    let mut out = BTreeMap::new();
    for (idx, field) in schema.fields().iter().enumerate() {
        if field.data_type == DataType::MapUtf8 {
            out.insert(idx, MapProjection::Full);
        }
    }
    out
}

fn merge_map_proj_idx(dst: &mut BTreeMap<usize, MapProjection>, idx: usize, proj: MapProjection) {
    match dst.get_mut(&idx) {
        Some(existing) => existing.merge(proj),
        None => {
            dst.insert(idx, proj);
        }
    }
}

fn merge_map_proj_name(dst: &mut BTreeMap<String, MapProjection>, col: &str, proj: MapProjection) {
    match dst.get_mut(col) {
        Some(existing) => existing.merge(proj),
        None => {
            dst.insert(col.to_owned(), proj);
        }
    }
}

fn merge_map_proj_maps(
    dst: &mut BTreeMap<usize, MapProjection>,
    src: &BTreeMap<usize, MapProjection>,
    limit: usize,
) {
    for (&idx, proj) in src {
        if idx < limit {
            merge_map_proj_idx(dst, idx, proj.clone());
        }
    }
}

fn collect_expr_map_proj_indices(
    expr: &Expr,
    schema: &Schema,
    out: &mut BTreeMap<usize, MapProjection>,
) {
    let mut stack = vec![expr];
    while let Some(node) = stack.pop() {
        match node {
            Expr::Column { index, .. } => {
                if let Some(field) = schema.fields().get(*index)
                    && field.data_type == DataType::MapUtf8
                {
                    merge_map_proj_idx(out, *index, MapProjection::Full);
                }
            }
            Expr::Literal(_) => {}
            Expr::MapIndex { map, key } => {
                if let (Expr::Column { index, .. }, Expr::Literal(ScalarValue::Utf8(k))) =
                    (map.as_ref(), key.as_ref())
                    && let Some(field) = schema.fields().get(*index)
                    && field.data_type == DataType::MapUtf8
                {
                    let mut keys = BTreeSet::new();
                    keys.insert(k.clone());
                    merge_map_proj_idx(out, *index, MapProjection::Keys(keys));
                } else {
                    stack.push(map);
                    stack.push(key);
                }
            }
            Expr::Call {
                func: ScalarFn::MapKeys,
                args,
                ..
            } if args.len() == 1 => {
                if let Expr::Column { index, .. } = &args[0]
                    && let Some(field) = schema.fields().get(*index)
                    && field.data_type == DataType::MapUtf8
                {
                    merge_map_proj_idx(out, *index, MapProjection::KeysOnly);
                } else {
                    stack.push(&args[0]);
                }
            }
            Expr::Unary { expr, .. } | Expr::Cast { expr, .. } => stack.push(expr),
            Expr::Binary { left, right, .. } => {
                stack.push(right);
                stack.push(left);
            }
            Expr::Between {
                expr, low, high, ..
            } => {
                stack.push(high);
                stack.push(low);
                stack.push(expr);
            }
            Expr::InList { expr, list, .. } => {
                for item in list {
                    stack.push(item);
                }
                stack.push(expr);
            }
            Expr::Like { expr, pattern, .. } => {
                stack.push(pattern);
                stack.push(expr);
            }
            Expr::Case {
                operand,
                when_then,
                else_expr,
                ..
            } => {
                if let Some(el) = else_expr {
                    stack.push(el);
                }
                for (w, t) in when_then {
                    stack.push(t);
                    stack.push(w);
                }
                if let Some(op) = operand {
                    stack.push(op);
                }
            }
            Expr::MakeArray { elems, .. } => {
                for e in elems {
                    stack.push(e);
                }
            }
            Expr::Call { args, .. } => {
                for a in args {
                    stack.push(a);
                }
            }
        }
    }
}

fn pass5_projection_and_map_keys(plan: LogicalPlan) -> Result<LogicalPlan, QueryError> {
    let n = plan.schema().len();
    let required: BTreeSet<usize> = (0..n).collect();
    let required_maps = full_map_reqs(plan.schema());
    let (pruned, _) = prune_node(plan, &required, &required_maps)?;
    Ok(pruned)
}

fn prune_node(
    plan: LogicalPlan,
    required: &BTreeSet<usize>,
    required_maps: &BTreeMap<usize, MapProjection>,
) -> Result<(LogicalPlan, Vec<Option<usize>>), QueryError> {
    match plan {
        LogicalPlan::Scan {
            table,
            kind,
            full_schema,
            projection,
            filters,
            time_range,
            id_lookups,
            substrings,
            mut map_projections,
            limit,
            schema,
        } => {
            let out_len = schema.len();
            let kept_out: Vec<usize> = required.iter().copied().filter(|&i| i < out_len).collect();

            for f in &filters {
                collect_expr_map_projections(f, &full_schema, &mut map_projections);
            }

            for &out_idx in &kept_out {
                let full_idx = projection.as_ref().map_or(out_idx, |p| p[out_idx]);
                if let Some(field) = full_schema.fields().get(full_idx)
                    && field.data_type == DataType::MapUtf8
                {
                    let proj = required_maps
                        .get(&out_idx)
                        .cloned()
                        .unwrap_or(MapProjection::Full);
                    merge_map_proj_name(&mut map_projections, &field.name, proj);
                }
            }

            let new_proj: Vec<usize> = kept_out
                .iter()
                .map(|&out_idx| projection.as_ref().map_or(out_idx, |p| p[out_idx]))
                .collect();
            let new_schema = Arc::new(full_schema.project(&new_proj)?);
            let is_identity_full = new_proj.len() == full_schema.len()
                && new_proj.iter().enumerate().all(|(i, &p)| i == p);
            let final_projection = if is_identity_full {
                None
            } else {
                Some(new_proj)
            };

            let mut old_to_new = vec![None; out_len];
            for (new_idx, &old_idx) in kept_out.iter().enumerate() {
                old_to_new[old_idx] = Some(new_idx);
            }

            Ok((
                LogicalPlan::Scan {
                    table,
                    kind,
                    full_schema,
                    projection: final_projection,
                    filters,
                    time_range,
                    id_lookups,
                    substrings,
                    map_projections,
                    limit,
                    schema: new_schema,
                },
                old_to_new,
            ))
        }
        LogicalPlan::Values { schema, rows } => {
            let out_len = schema.len();
            let kept_out: Vec<usize> = required.iter().copied().filter(|&i| i < out_len).collect();
            let new_schema = Arc::new(schema.project(&kept_out)?);
            let new_rows = rows
                .into_iter()
                .map(|row| {
                    kept_out
                        .iter()
                        .filter_map(|&idx| row.get(idx).cloned())
                        .collect()
                })
                .collect();
            let mut old_to_new = vec![None; out_len];
            for (new_idx, &old_idx) in kept_out.iter().enumerate() {
                old_to_new[old_idx] = Some(new_idx);
            }
            Ok((
                LogicalPlan::Values {
                    schema: new_schema,
                    rows: new_rows,
                },
                old_to_new,
            ))
        }
        LogicalPlan::EmptyRow { schema } => Ok((LogicalPlan::EmptyRow { schema }, Vec::new())),
        LogicalPlan::Filter { input, predicate } => {
            let in_schema = Arc::clone(input.schema());
            let in_len = in_schema.len();
            let mut child_needed = required.clone();
            predicate.referenced_columns(&mut child_needed);

            let mut child_maps = BTreeMap::new();
            merge_map_proj_maps(&mut child_maps, required_maps, in_len);
            collect_expr_map_proj_indices(&predicate, &in_schema, &mut child_maps);

            let (new_input, child_map) = prune_node(*input, &child_needed, &child_maps)?;
            let new_pred = predicate.remap_columns(&child_map)?;
            Ok((
                LogicalPlan::Filter {
                    input: Box::new(new_input),
                    predicate: new_pred,
                },
                child_map,
            ))
        }
        LogicalPlan::Project {
            input,
            exprs,
            schema,
        } => {
            let out_len = schema.len();
            let in_schema = Arc::clone(input.schema());
            let kept_out: Vec<usize> = required.iter().copied().filter(|&i| i < out_len).collect();

            let mut child_needed = BTreeSet::new();
            let mut child_maps = BTreeMap::new();
            for &out_idx in &kept_out {
                let expr = &exprs[out_idx];
                expr.referenced_columns(&mut child_needed);
                if let Expr::Column { index, .. } = expr {
                    if let Some(proj) = required_maps.get(&out_idx) {
                        merge_map_proj_idx(&mut child_maps, *index, proj.clone());
                    } else if expr.data_type() == DataType::MapUtf8 {
                        merge_map_proj_idx(&mut child_maps, *index, MapProjection::Full);
                    }
                } else {
                    collect_expr_map_proj_indices(expr, &in_schema, &mut child_maps);
                }
            }

            let (new_input, child_map) = prune_node(*input, &child_needed, &child_maps)?;
            let mut new_exprs = Vec::with_capacity(kept_out.len());
            for &out_idx in &kept_out {
                new_exprs.push(exprs[out_idx].remap_columns(&child_map)?);
            }
            let new_schema = Arc::new(schema.project(&kept_out)?);

            let mut old_to_new = vec![None; out_len];
            for (new_idx, &old_idx) in kept_out.iter().enumerate() {
                old_to_new[old_idx] = Some(new_idx);
            }
            Ok((
                LogicalPlan::Project {
                    input: Box::new(new_input),
                    exprs: new_exprs,
                    schema: new_schema,
                },
                old_to_new,
            ))
        }
        LogicalPlan::Aggregate {
            input,
            group_by,
            aggs,
            schema,
        } => {
            let group_len = group_by.len();
            let out_len = schema.len();
            let in_schema = Arc::clone(input.schema());

            // Always keep all GROUP BY keys so grouping semantics never change; prune unused aggs.
            let mut kept_out: Vec<usize> = (0..group_len).collect();
            let mut kept_agg_indices = Vec::new();
            for agg_i in 0..aggs.len() {
                let out_idx = group_len + agg_i;
                if required.contains(&out_idx) {
                    kept_out.push(out_idx);
                    kept_agg_indices.push(agg_i);
                }
            }

            let mut child_needed = BTreeSet::new();
            let mut child_maps = BTreeMap::new();
            for (_, g_expr) in &group_by {
                g_expr.referenced_columns(&mut child_needed);
                // Grouping hashes/compares the whole key expr (whole MapUtf8 -> Full, MapIndex -> Keys).
                collect_expr_map_proj_indices(g_expr, &in_schema, &mut child_maps);
            }
            for &agg_i in &kept_agg_indices {
                collect_agg_refs(&aggs[agg_i], &in_schema, &mut child_needed, &mut child_maps);
            }

            let (new_input, child_map) = prune_node(*input, &child_needed, &child_maps)?;
            let mut new_group_by = Vec::with_capacity(group_by.len());
            for (name, g_expr) in group_by {
                new_group_by.push((name, g_expr.remap_columns(&child_map)?));
            }
            let mut new_aggs = Vec::with_capacity(kept_agg_indices.len());
            for agg_i in kept_agg_indices {
                new_aggs.push(remap_agg(aggs[agg_i].clone(), &child_map)?);
            }
            let new_schema = Arc::new(schema.project(&kept_out)?);

            let mut old_to_new = vec![None; out_len];
            for (new_idx, &old_idx) in kept_out.iter().enumerate() {
                old_to_new[old_idx] = Some(new_idx);
            }

            Ok((
                LogicalPlan::Aggregate {
                    input: Box::new(new_input),
                    group_by: new_group_by,
                    aggs: new_aggs,
                    schema: new_schema,
                },
                old_to_new,
            ))
        }
        LogicalPlan::Window {
            input,
            partition_by,
            order_by,
            specs,
            schema,
        } => {
            let in_schema = Arc::clone(input.schema());
            let in_len = in_schema.len();
            let out_len = schema.len();

            let mut kept_specs_idx = Vec::new();
            for s_idx in 0..specs.len() {
                if required.contains(&(in_len + s_idx)) {
                    kept_specs_idx.push(s_idx);
                }
            }

            // If no window outputs are needed, eliminate the Window node entirely.
            if kept_specs_idx.is_empty() {
                let in_required: BTreeSet<usize> =
                    required.iter().copied().filter(|&i| i < in_len).collect();
                let mut in_maps = BTreeMap::new();
                merge_map_proj_maps(&mut in_maps, required_maps, in_len);
                let (new_input, child_map) = prune_node(*input, &in_required, &in_maps)?;
                let mut old_to_new = vec![None; out_len];
                for (i, mapped) in child_map.into_iter().enumerate() {
                    old_to_new[i] = mapped;
                }
                return Ok((new_input, old_to_new));
            }

            let mut child_needed: BTreeSet<usize> =
                required.iter().copied().filter(|&i| i < in_len).collect();
            let mut child_maps = BTreeMap::new();
            merge_map_proj_maps(&mut child_maps, required_maps, in_len);

            for p in &partition_by {
                p.referenced_columns(&mut child_needed);
                collect_expr_map_proj_indices(p, &in_schema, &mut child_maps);
            }
            for k in &order_by {
                k.expr.referenced_columns(&mut child_needed);
                collect_expr_map_proj_indices(&k.expr, &in_schema, &mut child_maps);
            }
            for &s_idx in &kept_specs_idx {
                collect_window_refs(
                    &specs[s_idx],
                    &in_schema,
                    &mut child_needed,
                    &mut child_maps,
                );
            }

            let (new_input, child_map) = prune_node(*input, &child_needed, &child_maps)?;
            let new_in_len = new_input.schema().len();

            let mut new_partition = Vec::with_capacity(partition_by.len());
            for p in partition_by {
                new_partition.push(p.remap_columns(&child_map)?);
            }
            let mut new_order = Vec::with_capacity(order_by.len());
            for k in order_by {
                new_order.push(remap_sort_key(k, &child_map)?);
            }
            let mut new_specs = Vec::with_capacity(kept_specs_idx.len());
            for &s_idx in &kept_specs_idx {
                new_specs.push(remap_window_spec(specs[s_idx].clone(), &child_map)?);
            }

            let mut kept_out: Vec<usize> =
                (0..in_len).filter(|&i| child_map[i].is_some()).collect();
            for &s_idx in &kept_specs_idx {
                kept_out.push(in_len + s_idx);
            }
            let new_schema = Arc::new(schema.project(&kept_out)?);

            let mut old_to_new = vec![None; out_len];
            for (i, mapped) in child_map.into_iter().enumerate() {
                old_to_new[i] = mapped;
            }
            for (new_offset, &s_idx) in kept_specs_idx.iter().enumerate() {
                old_to_new[in_len + s_idx] = Some(new_in_len + new_offset);
            }

            Ok((
                LogicalPlan::Window {
                    input: Box::new(new_input),
                    partition_by: new_partition,
                    order_by: new_order,
                    specs: new_specs,
                    schema: new_schema,
                },
                old_to_new,
            ))
        }
        LogicalPlan::Sort { input, keys } => {
            let in_schema = Arc::clone(input.schema());
            let in_len = in_schema.len();
            let mut child_needed = required.clone();
            let mut child_maps = BTreeMap::new();
            merge_map_proj_maps(&mut child_maps, required_maps, in_len);
            for k in &keys {
                k.expr.referenced_columns(&mut child_needed);
                collect_expr_map_proj_indices(&k.expr, &in_schema, &mut child_maps);
            }
            let (new_input, child_map) = prune_node(*input, &child_needed, &child_maps)?;
            let mut new_keys = Vec::with_capacity(keys.len());
            for k in keys {
                new_keys.push(remap_sort_key(k, &child_map)?);
            }
            Ok((
                LogicalPlan::Sort {
                    input: Box::new(new_input),
                    keys: new_keys,
                },
                child_map,
            ))
        }
        LogicalPlan::TopK {
            input,
            keys,
            limit,
            offset,
        } => {
            let in_schema = Arc::clone(input.schema());
            let in_len = in_schema.len();
            let mut child_needed = required.clone();
            let mut child_maps = BTreeMap::new();
            merge_map_proj_maps(&mut child_maps, required_maps, in_len);
            for k in &keys {
                k.expr.referenced_columns(&mut child_needed);
                collect_expr_map_proj_indices(&k.expr, &in_schema, &mut child_maps);
            }
            let (new_input, child_map) = prune_node(*input, &child_needed, &child_maps)?;
            let mut new_keys = Vec::with_capacity(keys.len());
            for k in keys {
                new_keys.push(remap_sort_key(k, &child_map)?);
            }
            Ok((
                LogicalPlan::TopK {
                    input: Box::new(new_input),
                    keys: new_keys,
                    limit,
                    offset,
                },
                child_map,
            ))
        }
        LogicalPlan::Limit { input, skip, fetch } => {
            let (new_input, child_map) = prune_node(*input, required, required_maps)?;
            Ok((
                LogicalPlan::Limit {
                    input: Box::new(new_input),
                    skip,
                    fetch,
                },
                child_map,
            ))
        }
        LogicalPlan::Distinct { input } => {
            // Distinct compares every column in its input schema.
            let in_schema = Arc::clone(input.schema());
            let in_len = in_schema.len();
            let all_needed: BTreeSet<usize> = (0..in_len).collect();
            let child_maps = full_map_reqs(&in_schema);
            let (new_input, child_map) = prune_node(*input, &all_needed, &child_maps)?;
            Ok((
                LogicalPlan::Distinct {
                    input: Box::new(new_input),
                },
                child_map,
            ))
        }
        LogicalPlan::Join {
            left,
            right,
            kind,
            left_keys,
            right_keys,
            null_equals_null,
            residual,
            schema,
        } => {
            let left_schema = Arc::clone(left.schema());
            let right_schema = Arc::clone(right.schema());
            let left_len = left_schema.len();
            let right_len = right_schema.len();
            let out_len = schema.len();

            let is_semi_anti = matches!(
                kind,
                LogicalJoinKind::Semi | LogicalJoinKind::Anti | LogicalJoinKind::AntiNullAware
            );

            let mut left_needed = BTreeSet::new();
            let mut right_needed = BTreeSet::new();
            let mut left_maps = BTreeMap::new();
            let mut right_maps = BTreeMap::new();

            for &idx in required {
                if idx < left_len {
                    left_needed.insert(idx);
                    if let Some(proj) = required_maps.get(&idx) {
                        merge_map_proj_idx(&mut left_maps, idx, proj.clone());
                    }
                } else if !is_semi_anti && idx < left_len + right_len {
                    let r_idx = idx - left_len;
                    right_needed.insert(r_idx);
                    if let Some(proj) = required_maps.get(&idx) {
                        merge_map_proj_idx(&mut right_maps, r_idx, proj.clone());
                    }
                }
            }

            for lk in &left_keys {
                lk.referenced_columns(&mut left_needed);
                collect_expr_map_proj_indices(lk, &left_schema, &mut left_maps);
            }
            for rk in &right_keys {
                rk.referenced_columns(&mut right_needed);
                collect_expr_map_proj_indices(rk, &right_schema, &mut right_maps);
            }
            if let Some(res) = &residual {
                let mut res_cols = BTreeSet::new();
                res.referenced_columns(&mut res_cols);
                for idx in res_cols {
                    if idx < left_len {
                        left_needed.insert(idx);
                    } else if idx < left_len + right_len {
                        right_needed.insert(idx - left_len);
                    }
                }
                let combined_schema = Schema::try_new(
                    left_schema
                        .fields()
                        .iter()
                        .chain(right_schema.fields().iter())
                        .cloned()
                        .collect(),
                )?;
                let mut combined_maps = BTreeMap::new();
                collect_expr_map_proj_indices(res, &combined_schema, &mut combined_maps);
                for (idx, proj) in combined_maps {
                    if idx < left_len {
                        merge_map_proj_idx(&mut left_maps, idx, proj);
                    } else if idx < left_len + right_len {
                        merge_map_proj_idx(&mut right_maps, idx - left_len, proj);
                    }
                }
            }

            let (new_left, left_map) = prune_node(*left, &left_needed, &left_maps)?;
            let (new_right, right_map) = prune_node(*right, &right_needed, &right_maps)?;
            let new_left_len = new_left.schema().len();

            let mut new_left_keys = Vec::with_capacity(left_keys.len());
            for lk in left_keys {
                new_left_keys.push(lk.remap_columns(&left_map)?);
            }
            let mut new_right_keys = Vec::with_capacity(right_keys.len());
            for rk in right_keys {
                new_right_keys.push(rk.remap_columns(&right_map)?);
            }

            let mut combined_map = vec![None; left_len + right_len];
            for (i, m) in left_map.iter().enumerate() {
                combined_map[i] = *m;
            }
            for (i, m) in right_map.iter().enumerate() {
                combined_map[left_len + i] = m.map(|r| new_left_len + r);
            }

            let new_residual = match residual {
                Some(res) => Some(res.remap_columns(&combined_map)?),
                None => None,
            };

            let (new_schema, old_to_new) = if is_semi_anti {
                let kept: Vec<usize> = (0..left_len).filter(|&i| left_map[i].is_some()).collect();
                (Arc::new(schema.project(&kept)?), left_map)
            } else {
                let kept: Vec<usize> = (0..out_len)
                    .filter(|&i| combined_map.get(i).copied().flatten().is_some())
                    .collect();
                (Arc::new(schema.project(&kept)?), combined_map)
            };

            Ok((
                LogicalPlan::Join {
                    left: Box::new(new_left),
                    right: Box::new(new_right),
                    kind,
                    left_keys: new_left_keys,
                    right_keys: new_right_keys,
                    null_equals_null,
                    residual: new_residual,
                    schema: new_schema,
                },
                old_to_new,
            ))
        }
        LogicalPlan::Union {
            inputs,
            all,
            schema,
        } => {
            let out_len = schema.len();
            let (effective_required, effective_maps): (
                BTreeSet<usize>,
                BTreeMap<usize, MapProjection>,
            ) = if all {
                let req = required.iter().copied().filter(|&i| i < out_len).collect();
                let mut maps = BTreeMap::new();
                merge_map_proj_maps(&mut maps, required_maps, out_len);
                (req, maps)
            } else {
                // `UNION` deduplicates across all columns.
                ((0..out_len).collect(), full_map_reqs(&schema))
            };
            let kept_out: Vec<usize> = effective_required.iter().copied().collect();
            let mut new_inputs = Vec::with_capacity(inputs.len());
            for branch in inputs {
                let (new_branch, branch_map) =
                    prune_node(branch, &effective_required, &effective_maps)?;
                let needs_project = new_branch.schema().len() != kept_out.len()
                    || kept_out.iter().enumerate().any(|(new_idx, &old_idx)| {
                        branch_map.get(old_idx).copied().flatten() != Some(new_idx)
                    });
                let aligned = if needs_project {
                    let branch_schema = new_branch.schema();
                    let mut exprs = Vec::with_capacity(kept_out.len());
                    let mut fields = Vec::with_capacity(kept_out.len());
                    for &old_idx in &kept_out {
                        let mapped =
                            branch_map.get(old_idx).copied().flatten().ok_or_else(|| {
                                QueryError::internal(format!(
                                    "union branch column {old_idx} missing after prune"
                                ))
                            })?;
                        let field = branch_schema.fields().get(mapped).ok_or_else(|| {
                            QueryError::internal(format!(
                                "union branch field {mapped} out of bounds"
                            ))
                        })?;
                        exprs.push(Expr::Column {
                            index: mapped,
                            name: field.name.clone(),
                            data_type: field.data_type.clone(),
                        });
                        fields.push(field.clone());
                    }
                    let proj_schema = Arc::new(Schema::try_new(fields)?);
                    LogicalPlan::Project {
                        input: Box::new(new_branch),
                        exprs,
                        schema: proj_schema,
                    }
                } else {
                    new_branch
                };
                new_inputs.push(aligned);
            }
            let new_schema = Arc::new(schema.project(&kept_out)?);
            let mut old_to_new = vec![None; out_len];
            for (new_idx, &old_idx) in kept_out.iter().enumerate() {
                old_to_new[old_idx] = Some(new_idx);
            }
            Ok((
                LogicalPlan::Union {
                    inputs: new_inputs,
                    all,
                    schema: new_schema,
                },
                old_to_new,
            ))
        }
        LogicalPlan::Unnest {
            input,
            columns,
            preserve_nulls,
            schema,
        } => {
            let out_len = schema.len();
            let mut child_needed: BTreeSet<usize> =
                required.iter().copied().filter(|&i| i < out_len).collect();
            for &c in &columns {
                child_needed.insert(c);
            }
            let in_len = input.schema().len();
            let mut child_maps = BTreeMap::new();
            merge_map_proj_maps(&mut child_maps, required_maps, in_len);

            let (new_input, child_map) = prune_node(*input, &child_needed, &child_maps)?;
            let mut new_cols = Vec::with_capacity(columns.len());
            for c in columns {
                let mapped = child_map.get(c).copied().flatten().ok_or_else(|| {
                    QueryError::internal(format!("unnest column index {c} missing after prune"))
                })?;
                new_cols.push(mapped);
            }
            let kept_out: Vec<usize> = (0..out_len)
                .filter(|&i| child_map.get(i).copied().flatten().is_some())
                .collect();
            let new_schema = Arc::new(schema.project(&kept_out)?);
            Ok((
                LogicalPlan::Unnest {
                    input: Box::new(new_input),
                    columns: new_cols,
                    preserve_nulls,
                    schema: new_schema,
                },
                child_map,
            ))
        }
        LogicalPlan::Explain { inner, schema } => {
            let inner_schema = Arc::clone(inner.schema());
            let inner_len = inner_schema.len();
            let inner_req: BTreeSet<usize> = (0..inner_len).collect();
            let inner_maps = full_map_reqs(&inner_schema);
            let (new_inner, _) = prune_node(*inner, &inner_req, &inner_maps)?;
            let exp_len = schema.len();
            let old_to_new = (0..exp_len).map(Some).collect();
            Ok((
                LogicalPlan::Explain {
                    inner: Box::new(new_inner),
                    schema,
                },
                old_to_new,
            ))
        }
    }
}

fn collect_agg_refs(
    agg: &AggExpr,
    schema: &Schema,
    cols: &mut BTreeSet<usize>,
    map_projs: &mut BTreeMap<usize, MapProjection>,
) {
    if let Some(f) = &agg.filter {
        f.referenced_columns(cols);
        collect_expr_map_proj_indices(f, schema, map_projs);
    }
    visit_agg_func_exprs(&agg.func, &mut |e| {
        e.referenced_columns(cols);
        collect_expr_map_proj_indices(e, schema, map_projs);
    });
}

fn remap_agg(agg: AggExpr, map: &[Option<usize>]) -> Result<AggExpr, QueryError> {
    let filter = match agg.filter {
        Some(f) => Some(f.remap_columns(map)?),
        None => None,
    };
    let func = try_map_agg_func_exprs(agg.func, &mut |e| e.remap_columns(map))?;
    Ok(AggExpr {
        func,
        filter,
        output_name: agg.output_name,
    })
}

fn collect_window_refs(
    spec: &WindowSpec,
    schema: &Schema,
    cols: &mut BTreeSet<usize>,
    map_projs: &mut BTreeMap<usize, MapProjection>,
) {
    for p in &spec.partition_by {
        p.referenced_columns(cols);
        collect_expr_map_proj_indices(p, schema, map_projs);
    }
    for k in &spec.order_by {
        k.expr.referenced_columns(cols);
        collect_expr_map_proj_indices(&k.expr, schema, map_projs);
    }
    visit_window_func_exprs(&spec.func, &mut |e| {
        e.referenced_columns(cols);
        collect_expr_map_proj_indices(e, schema, map_projs);
    });
}

fn remap_window_spec(spec: WindowSpec, map: &[Option<usize>]) -> Result<WindowSpec, QueryError> {
    let mut partition_by = Vec::with_capacity(spec.partition_by.len());
    for p in spec.partition_by {
        partition_by.push(p.remap_columns(map)?);
    }
    let mut order_by = Vec::with_capacity(spec.order_by.len());
    for k in spec.order_by {
        order_by.push(remap_sort_key(k, map)?);
    }
    let func = try_map_window_func_exprs(spec.func, &mut |e| e.remap_columns(map))?;
    Ok(WindowSpec {
        partition_by,
        order_by,
        frame: spec.frame,
        func,
        output_name: spec.output_name,
    })
}

fn remap_sort_key(mut key: SortKey, map: &[Option<usize>]) -> Result<SortKey, QueryError> {
    key.expr = key.expr.remap_columns(map)?;
    Ok(key)
}

// ---------------------------------------------------------------------------
// Pass 6: Bounded ORDER BY ... LIMIT k [OFFSET n] -> TopK rewrite
// ---------------------------------------------------------------------------

fn pass6_topk_rewrite(plan: LogicalPlan) -> LogicalPlan {
    match plan {
        LogicalPlan::Limit { input, skip, fetch } => {
            let input = pass6_topk_rewrite(*input);
            if let Some(limit) = fetch
                && skip.saturating_add(limit) <= TOPK_MAX_ROWS
            {
                let input = match input {
                    LogicalPlan::Sort { input: child, keys } => {
                        return LogicalPlan::TopK {
                            input: child,
                            keys,
                            limit,
                            offset: skip,
                        };
                    }
                    LogicalPlan::Project {
                        input: proj_in,
                        exprs,
                        schema,
                    } => {
                        if exprs.iter().all(|e| matches!(e, Expr::Column { .. }))
                            && let LogicalPlan::Sort { input: child, keys } = *proj_in
                        {
                            return LogicalPlan::Project {
                                input: Box::new(LogicalPlan::TopK {
                                    input: child,
                                    keys,
                                    limit,
                                    offset: skip,
                                }),
                                exprs,
                                schema,
                            };
                        }
                        LogicalPlan::Project {
                            input: proj_in,
                            exprs,
                            schema,
                        }
                    }
                    other => other,
                };
                return LogicalPlan::Limit {
                    input: Box::new(input),
                    skip,
                    fetch,
                };
            }
            LogicalPlan::Limit {
                input: Box::new(input),
                skip,
                fetch,
            }
        }
        LogicalPlan::Filter { input, predicate } => LogicalPlan::Filter {
            input: Box::new(pass6_topk_rewrite(*input)),
            predicate,
        },
        LogicalPlan::Project {
            input,
            exprs,
            schema,
        } => LogicalPlan::Project {
            input: Box::new(pass6_topk_rewrite(*input)),
            exprs,
            schema,
        },
        LogicalPlan::Aggregate {
            input,
            group_by,
            aggs,
            schema,
        } => LogicalPlan::Aggregate {
            input: Box::new(pass6_topk_rewrite(*input)),
            group_by,
            aggs,
            schema,
        },
        LogicalPlan::Window {
            input,
            partition_by,
            order_by,
            specs,
            schema,
        } => LogicalPlan::Window {
            input: Box::new(pass6_topk_rewrite(*input)),
            partition_by,
            order_by,
            specs,
            schema,
        },
        LogicalPlan::Sort { input, keys } => LogicalPlan::Sort {
            input: Box::new(pass6_topk_rewrite(*input)),
            keys,
        },
        LogicalPlan::TopK {
            input,
            keys,
            limit,
            offset,
        } => LogicalPlan::TopK {
            input: Box::new(pass6_topk_rewrite(*input)),
            keys,
            limit,
            offset,
        },
        LogicalPlan::Distinct { input } => LogicalPlan::Distinct {
            input: Box::new(pass6_topk_rewrite(*input)),
        },
        LogicalPlan::Join {
            left,
            right,
            kind,
            left_keys,
            right_keys,
            null_equals_null,
            residual,
            schema,
        } => LogicalPlan::Join {
            left: Box::new(pass6_topk_rewrite(*left)),
            right: Box::new(pass6_topk_rewrite(*right)),
            kind,
            left_keys,
            right_keys,
            null_equals_null,
            residual,
            schema,
        },
        LogicalPlan::Union {
            inputs,
            all,
            schema,
        } => LogicalPlan::Union {
            inputs: inputs.into_iter().map(pass6_topk_rewrite).collect(),
            all,
            schema,
        },
        LogicalPlan::Unnest {
            input,
            columns,
            preserve_nulls,
            schema,
        } => LogicalPlan::Unnest {
            input: Box::new(pass6_topk_rewrite(*input)),
            columns,
            preserve_nulls,
            schema,
        },
        LogicalPlan::Explain { inner, schema } => LogicalPlan::Explain {
            inner: Box::new(pass6_topk_rewrite(*inner)),
            schema,
        },
        leaf @ (LogicalPlan::Scan { .. }
        | LogicalPlan::Values { .. }
        | LogicalPlan::EmptyRow { .. }) => leaf,
    }
}

// ---------------------------------------------------------------------------
// Generic AST / Plan traversal helpers
// ---------------------------------------------------------------------------

fn map_expr_bottom_up(expr: Expr, f: &mut impl FnMut(Expr) -> Expr) -> Expr {
    enum MapTask {
        Visit(Expr),
        BuildUnary {
            op: UnaryOp,
            data_type: DataType,
        },
        BuildBinary {
            op: BinaryOp,
            data_type: DataType,
        },
        BuildCast {
            target: DataType,
            mode: CastMode,
        },
        BuildBetween {
            negated: bool,
        },
        BuildInList {
            n: usize,
            negated: bool,
        },
        BuildInListRedacted {
            list: Vec<Expr>,
            negated: bool,
        },
        BuildLike {
            escape: Option<char>,
            case_insensitive: bool,
            negated: bool,
        },
        BuildCase {
            has_op: bool,
            n_wt: usize,
            has_else: bool,
            data_type: DataType,
        },
        BuildMapIndex,
        BuildMakeArray {
            n: usize,
            item_type: DataType,
        },
        BuildCall {
            func: ScalarFn,
            n: usize,
            data_type: DataType,
        },
    }

    let mut tasks = vec![MapTask::Visit(expr)];
    let mut vals: Vec<Expr> = Vec::new();

    while let Some(task) = tasks.pop() {
        match task {
            MapTask::Visit(node) => match node {
                Expr::Column { .. } | Expr::Literal(_) => vals.push(f(node)),
                Expr::Unary {
                    op,
                    expr: inner,
                    data_type,
                } => {
                    tasks.push(MapTask::BuildUnary { op, data_type });
                    tasks.push(MapTask::Visit(*inner));
                }
                Expr::Binary {
                    left,
                    op,
                    right,
                    data_type,
                } => {
                    tasks.push(MapTask::BuildBinary { op, data_type });
                    tasks.push(MapTask::Visit(*right));
                    tasks.push(MapTask::Visit(*left));
                }
                Expr::Cast {
                    expr: inner,
                    target,
                    mode,
                } => {
                    tasks.push(MapTask::BuildCast { target, mode });
                    tasks.push(MapTask::Visit(*inner));
                }
                Expr::Between {
                    expr: inner,
                    low,
                    high,
                    negated,
                } => {
                    tasks.push(MapTask::BuildBetween { negated });
                    tasks.push(MapTask::Visit(*high));
                    tasks.push(MapTask::Visit(*low));
                    tasks.push(MapTask::Visit(*inner));
                }
                Expr::InList {
                    expr: inner,
                    list,
                    negated,
                    redacted,
                } => {
                    if redacted {
                        tasks.push(MapTask::BuildInListRedacted { list, negated });
                        tasks.push(MapTask::Visit(*inner));
                    } else {
                        tasks.push(MapTask::BuildInList {
                            n: list.len(),
                            negated,
                        });
                        for item in list.into_iter().rev() {
                            tasks.push(MapTask::Visit(item));
                        }
                        tasks.push(MapTask::Visit(*inner));
                    }
                }
                Expr::Like {
                    expr: inner,
                    pattern,
                    escape,
                    case_insensitive,
                    negated,
                } => {
                    tasks.push(MapTask::BuildLike {
                        escape,
                        case_insensitive,
                        negated,
                    });
                    tasks.push(MapTask::Visit(*pattern));
                    tasks.push(MapTask::Visit(*inner));
                }
                Expr::Case {
                    operand,
                    when_then,
                    else_expr,
                    data_type,
                } => {
                    tasks.push(MapTask::BuildCase {
                        has_op: operand.is_some(),
                        n_wt: when_then.len(),
                        has_else: else_expr.is_some(),
                        data_type,
                    });
                    if let Some(el) = else_expr {
                        tasks.push(MapTask::Visit(*el));
                    }
                    for (w, t) in when_then.into_iter().rev() {
                        tasks.push(MapTask::Visit(t));
                        tasks.push(MapTask::Visit(w));
                    }
                    if let Some(op) = operand {
                        tasks.push(MapTask::Visit(*op));
                    }
                }
                Expr::MapIndex { map, key } => {
                    tasks.push(MapTask::BuildMapIndex);
                    tasks.push(MapTask::Visit(*key));
                    tasks.push(MapTask::Visit(*map));
                }
                Expr::MakeArray { elems, item_type } => {
                    tasks.push(MapTask::BuildMakeArray {
                        n: elems.len(),
                        item_type,
                    });
                    for e in elems.into_iter().rev() {
                        tasks.push(MapTask::Visit(e));
                    }
                }
                Expr::Call {
                    func,
                    args,
                    data_type,
                } => {
                    tasks.push(MapTask::BuildCall {
                        func,
                        n: args.len(),
                        data_type,
                    });
                    for a in args.into_iter().rev() {
                        tasks.push(MapTask::Visit(a));
                    }
                }
            },
            MapTask::BuildUnary { op, data_type } => {
                let Some(inner) = vals.pop() else {
                    break;
                };
                vals.push(f(Expr::Unary {
                    op,
                    expr: Box::new(inner),
                    data_type,
                }));
            }
            MapTask::BuildBinary { op, data_type } => {
                let (Some(right), Some(left)) = (vals.pop(), vals.pop()) else {
                    break;
                };
                vals.push(f(Expr::Binary {
                    left: Box::new(left),
                    op,
                    right: Box::new(right),
                    data_type,
                }));
            }
            MapTask::BuildCast { target, mode } => {
                let Some(inner) = vals.pop() else {
                    break;
                };
                vals.push(f(Expr::Cast {
                    expr: Box::new(inner),
                    target,
                    mode,
                }));
            }
            MapTask::BuildBetween { negated } => {
                let (Some(high), Some(low), Some(inner)) = (vals.pop(), vals.pop(), vals.pop())
                else {
                    break;
                };
                vals.push(f(Expr::Between {
                    expr: Box::new(inner),
                    low: Box::new(low),
                    high: Box::new(high),
                    negated,
                }));
            }
            MapTask::BuildInList { n, negated } => {
                let start = vals.len().saturating_sub(n);
                let list: Vec<Expr> = vals.drain(start..).collect();
                let Some(inner) = vals.pop() else {
                    break;
                };
                vals.push(f(Expr::InList {
                    expr: Box::new(inner),
                    list,
                    negated,
                    redacted: false,
                }));
            }
            MapTask::BuildInListRedacted { list, negated } => {
                let Some(inner) = vals.pop() else {
                    break;
                };
                vals.push(f(Expr::InList {
                    expr: Box::new(inner),
                    list,
                    negated,
                    redacted: true,
                }));
            }
            MapTask::BuildLike {
                escape,
                case_insensitive,
                negated,
            } => {
                let (Some(pattern), Some(inner)) = (vals.pop(), vals.pop()) else {
                    break;
                };
                vals.push(f(Expr::Like {
                    expr: Box::new(inner),
                    pattern: Box::new(pattern),
                    escape,
                    case_insensitive,
                    negated,
                }));
            }
            MapTask::BuildCase {
                has_op,
                n_wt,
                has_else,
                data_type,
            } => {
                let else_expr = if has_else {
                    let Some(el) = vals.pop() else {
                        break;
                    };
                    Some(Box::new(el))
                } else {
                    None
                };
                let wt_start = vals.len().saturating_sub(n_wt * 2);
                let mut when_then = Vec::with_capacity(n_wt);
                let mut drained = vals.drain(wt_start..);
                while let (Some(w), Some(t)) = (drained.next(), drained.next()) {
                    when_then.push((w, t));
                }
                drop(drained);
                let operand = if has_op {
                    let Some(op) = vals.pop() else {
                        break;
                    };
                    Some(Box::new(op))
                } else {
                    None
                };
                vals.push(f(Expr::Case {
                    operand,
                    when_then,
                    else_expr,
                    data_type,
                }));
            }
            MapTask::BuildMapIndex => {
                let (Some(key), Some(map)) = (vals.pop(), vals.pop()) else {
                    break;
                };
                vals.push(f(Expr::MapIndex {
                    map: Box::new(map),
                    key: Box::new(key),
                }));
            }
            MapTask::BuildMakeArray { n, item_type } => {
                let start = vals.len().saturating_sub(n);
                let elems: Vec<Expr> = vals.drain(start..).collect();
                vals.push(f(Expr::MakeArray { elems, item_type }));
            }
            MapTask::BuildCall { func, n, data_type } => {
                let start = vals.len().saturating_sub(n);
                let args: Vec<Expr> = vals.drain(start..).collect();
                vals.push(f(Expr::Call {
                    func,
                    args,
                    data_type,
                }));
            }
        }
    }

    vals.pop()
        .unwrap_or(Expr::Literal(ScalarValue::Null(DataType::Null)))
}

fn map_plan_exprs(
    plan: LogicalPlan,
    f: &mut impl FnMut(Expr) -> Expr,
) -> Result<LogicalPlan, QueryError> {
    Ok(match plan {
        LogicalPlan::Scan {
            table,
            kind,
            full_schema,
            projection,
            filters,
            time_range,
            id_lookups,
            substrings,
            map_projections,
            limit,
            schema,
        } => LogicalPlan::Scan {
            table,
            kind,
            full_schema,
            projection,
            filters: filters.into_iter().map(&mut *f).collect(),
            time_range,
            id_lookups,
            substrings,
            map_projections,
            limit,
            schema,
        },
        LogicalPlan::Values { schema, rows } => LogicalPlan::Values {
            schema,
            rows: rows
                .into_iter()
                .map(|row| row.into_iter().map(&mut *f).collect())
                .collect(),
        },
        LogicalPlan::EmptyRow { schema } => LogicalPlan::EmptyRow { schema },
        LogicalPlan::Filter { input, predicate } => LogicalPlan::Filter {
            input: Box::new(map_plan_exprs(*input, f)?),
            predicate: f(predicate),
        },
        LogicalPlan::Project {
            input,
            exprs,
            schema,
        } => LogicalPlan::Project {
            input: Box::new(map_plan_exprs(*input, f)?),
            exprs: exprs.into_iter().map(f).collect(),
            schema,
        },
        LogicalPlan::Aggregate {
            input,
            group_by,
            aggs,
            schema,
        } => LogicalPlan::Aggregate {
            input: Box::new(map_plan_exprs(*input, f)?),
            group_by: group_by.into_iter().map(|(n, e)| (n, f(e))).collect(),
            aggs: aggs
                .into_iter()
                .map(|agg| AggExpr {
                    func: map_agg_func_exprs(agg.func, f),
                    filter: agg.filter.map(&mut *f),
                    output_name: agg.output_name,
                })
                .collect(),
            schema,
        },
        LogicalPlan::Window {
            input,
            partition_by,
            order_by,
            specs,
            schema,
        } => LogicalPlan::Window {
            input: Box::new(map_plan_exprs(*input, f)?),
            partition_by: partition_by.into_iter().map(&mut *f).collect(),
            order_by: order_by
                .into_iter()
                .map(|mut k| {
                    k.expr = f(k.expr);
                    k
                })
                .collect(),
            specs: specs
                .into_iter()
                .map(|spec| WindowSpec {
                    partition_by: spec.partition_by.into_iter().map(&mut *f).collect(),
                    order_by: spec
                        .order_by
                        .into_iter()
                        .map(|mut k| {
                            k.expr = f(k.expr);
                            k
                        })
                        .collect(),
                    frame: spec.frame,
                    func: map_window_func_exprs(spec.func, f),
                    output_name: spec.output_name,
                })
                .collect(),
            schema,
        },
        LogicalPlan::Sort { input, keys } => LogicalPlan::Sort {
            input: Box::new(map_plan_exprs(*input, f)?),
            keys: keys
                .into_iter()
                .map(|mut k| {
                    k.expr = f(k.expr);
                    k
                })
                .collect(),
        },
        LogicalPlan::Limit { input, skip, fetch } => LogicalPlan::Limit {
            input: Box::new(map_plan_exprs(*input, f)?),
            skip,
            fetch,
        },
        LogicalPlan::TopK {
            input,
            keys,
            limit,
            offset,
        } => LogicalPlan::TopK {
            input: Box::new(map_plan_exprs(*input, f)?),
            keys: keys
                .into_iter()
                .map(|mut k| {
                    k.expr = f(k.expr);
                    k
                })
                .collect(),
            limit,
            offset,
        },
        LogicalPlan::Distinct { input } => LogicalPlan::Distinct {
            input: Box::new(map_plan_exprs(*input, f)?),
        },
        LogicalPlan::Join {
            left,
            right,
            kind,
            left_keys,
            right_keys,
            null_equals_null,
            residual,
            schema,
        } => LogicalPlan::Join {
            left: Box::new(map_plan_exprs(*left, f)?),
            right: Box::new(map_plan_exprs(*right, f)?),
            kind,
            left_keys: left_keys.into_iter().map(&mut *f).collect(),
            right_keys: right_keys.into_iter().map(&mut *f).collect(),
            null_equals_null,
            residual: residual.map(f),
            schema,
        },
        LogicalPlan::Union {
            inputs,
            all,
            schema,
        } => {
            let mut out = Vec::with_capacity(inputs.len());
            for branch in inputs {
                out.push(map_plan_exprs(branch, f)?);
            }
            LogicalPlan::Union {
                inputs: out,
                all,
                schema,
            }
        }
        LogicalPlan::Unnest {
            input,
            columns,
            preserve_nulls,
            schema,
        } => LogicalPlan::Unnest {
            input: Box::new(map_plan_exprs(*input, f)?),
            columns,
            preserve_nulls,
            schema,
        },
        LogicalPlan::Explain { inner, schema } => LogicalPlan::Explain {
            inner: Box::new(map_plan_exprs(*inner, f)?),
            schema,
        },
    })
}

fn map_agg_func_exprs(func: AggFunc, f: &mut impl FnMut(Expr) -> Expr) -> AggFunc {
    match func {
        AggFunc::CountStar => AggFunc::CountStar,
        AggFunc::Count(e) => AggFunc::Count(f(e)),
        AggFunc::CountDistinct(e) => AggFunc::CountDistinct(f(e)),
        AggFunc::Sum(e) => AggFunc::Sum(f(e)),
        AggFunc::SumCostDecimal(e) => AggFunc::SumCostDecimal(f(e)),
        AggFunc::Avg(e) => AggFunc::Avg(f(e)),
        AggFunc::Min(e) => AggFunc::Min(f(e)),
        AggFunc::Max(e) => AggFunc::Max(f(e)),
        AggFunc::BoolOr(e) => AggFunc::BoolOr(f(e)),
        AggFunc::BoolAnd(e) => AggFunc::BoolAnd(f(e)),
        AggFunc::ApproxPercentileCont { expr, quantile } => AggFunc::ApproxPercentileCont {
            expr: f(expr),
            quantile,
        },
        AggFunc::PercentileCont { expr, quantile } => AggFunc::PercentileCont {
            expr: f(expr),
            quantile,
        },
        AggFunc::ArrayAgg { expr, distinct } => AggFunc::ArrayAgg {
            expr: f(expr),
            distinct,
        },
        AggFunc::FirstNonNullByOrder { value, order_by } => AggFunc::FirstNonNullByOrder {
            value: f(value),
            order_by: order_by
                .into_iter()
                .map(|mut k| {
                    k.expr = f(k.expr);
                    k
                })
                .collect(),
        },
        AggFunc::FirstByOrder { value, order_by } => AggFunc::FirstByOrder {
            value: f(value),
            order_by: order_by
                .into_iter()
                .map(|mut k| {
                    k.expr = f(k.expr);
                    k
                })
                .collect(),
        },
        AggFunc::TagUnion(e) => AggFunc::TagUnion(f(e)),
    }
}

fn try_map_agg_func_exprs(
    func: AggFunc,
    f: &mut impl FnMut(&Expr) -> Result<Expr, QueryError>,
) -> Result<AggFunc, QueryError> {
    Ok(match func {
        AggFunc::CountStar => AggFunc::CountStar,
        AggFunc::Count(e) => AggFunc::Count(f(&e)?),
        AggFunc::CountDistinct(e) => AggFunc::CountDistinct(f(&e)?),
        AggFunc::Sum(e) => AggFunc::Sum(f(&e)?),
        AggFunc::SumCostDecimal(e) => AggFunc::SumCostDecimal(f(&e)?),
        AggFunc::Avg(e) => AggFunc::Avg(f(&e)?),
        AggFunc::Min(e) => AggFunc::Min(f(&e)?),
        AggFunc::Max(e) => AggFunc::Max(f(&e)?),
        AggFunc::BoolOr(e) => AggFunc::BoolOr(f(&e)?),
        AggFunc::BoolAnd(e) => AggFunc::BoolAnd(f(&e)?),
        AggFunc::ApproxPercentileCont { expr, quantile } => AggFunc::ApproxPercentileCont {
            expr: f(&expr)?,
            quantile,
        },
        AggFunc::PercentileCont { expr, quantile } => AggFunc::PercentileCont {
            expr: f(&expr)?,
            quantile,
        },
        AggFunc::ArrayAgg { expr, distinct } => AggFunc::ArrayAgg {
            expr: f(&expr)?,
            distinct,
        },
        AggFunc::FirstNonNullByOrder { value, order_by } => {
            let mut mapped_ob = Vec::with_capacity(order_by.len());
            for mut k in order_by {
                k.expr = f(&k.expr)?;
                mapped_ob.push(k);
            }
            AggFunc::FirstNonNullByOrder {
                value: f(&value)?,
                order_by: mapped_ob,
            }
        }
        AggFunc::FirstByOrder { value, order_by } => {
            let mut mapped_ob = Vec::with_capacity(order_by.len());
            for mut k in order_by {
                k.expr = f(&k.expr)?;
                mapped_ob.push(k);
            }
            AggFunc::FirstByOrder {
                value: f(&value)?,
                order_by: mapped_ob,
            }
        }
        AggFunc::TagUnion(e) => AggFunc::TagUnion(f(&e)?),
    })
}

fn visit_agg_func_exprs(func: &AggFunc, f: &mut impl FnMut(&Expr)) {
    match func {
        AggFunc::CountStar => {}
        AggFunc::Count(e)
        | AggFunc::CountDistinct(e)
        | AggFunc::Sum(e)
        | AggFunc::SumCostDecimal(e)
        | AggFunc::Avg(e)
        | AggFunc::Min(e)
        | AggFunc::Max(e)
        | AggFunc::BoolOr(e)
        | AggFunc::BoolAnd(e)
        | AggFunc::ApproxPercentileCont { expr: e, .. }
        | AggFunc::PercentileCont { expr: e, .. }
        | AggFunc::ArrayAgg { expr: e, .. }
        | AggFunc::TagUnion(e) => f(e),
        AggFunc::FirstNonNullByOrder { value, order_by }
        | AggFunc::FirstByOrder { value, order_by } => {
            f(value);
            for k in order_by {
                f(&k.expr);
            }
        }
    }
}

fn map_window_func_exprs(func: WindowFunc, f: &mut impl FnMut(Expr) -> Expr) -> WindowFunc {
    match func {
        WindowFunc::RowNumber | WindowFunc::Rank | WindowFunc::DenseRank => func,
        WindowFunc::Lag {
            expr,
            offset,
            default,
        } => WindowFunc::Lag {
            expr: f(expr),
            offset,
            default: default.map(f),
        },
        WindowFunc::Lead {
            expr,
            offset,
            default,
        } => WindowFunc::Lead {
            expr: f(expr),
            offset,
            default: default.map(f),
        },
        WindowFunc::FirstValue(e) => WindowFunc::FirstValue(f(e)),
        WindowFunc::LastValue(e) => WindowFunc::LastValue(f(e)),
        WindowFunc::Sum(e) => WindowFunc::Sum(f(e)),
        WindowFunc::Count(e) => WindowFunc::Count(e.map(f)),
        WindowFunc::Avg(e) => WindowFunc::Avg(f(e)),
        WindowFunc::Min(e) => WindowFunc::Min(f(e)),
        WindowFunc::Max(e) => WindowFunc::Max(f(e)),
    }
}

fn try_map_window_func_exprs(
    func: WindowFunc,
    f: &mut impl FnMut(&Expr) -> Result<Expr, QueryError>,
) -> Result<WindowFunc, QueryError> {
    Ok(match func {
        WindowFunc::RowNumber | WindowFunc::Rank | WindowFunc::DenseRank => func,
        WindowFunc::Lag {
            expr,
            offset,
            default,
        } => WindowFunc::Lag {
            expr: f(&expr)?,
            offset,
            default: match default {
                Some(d) => Some(f(&d)?),
                None => None,
            },
        },
        WindowFunc::Lead {
            expr,
            offset,
            default,
        } => WindowFunc::Lead {
            expr: f(&expr)?,
            offset,
            default: match default {
                Some(d) => Some(f(&d)?),
                None => None,
            },
        },
        WindowFunc::FirstValue(e) => WindowFunc::FirstValue(f(&e)?),
        WindowFunc::LastValue(e) => WindowFunc::LastValue(f(&e)?),
        WindowFunc::Sum(e) => WindowFunc::Sum(f(&e)?),
        WindowFunc::Count(e) => WindowFunc::Count(match e {
            Some(inner) => Some(f(&inner)?),
            None => None,
        }),
        WindowFunc::Avg(e) => WindowFunc::Avg(f(&e)?),
        WindowFunc::Min(e) => WindowFunc::Min(f(&e)?),
        WindowFunc::Max(e) => WindowFunc::Max(f(&e)?),
    })
}

fn visit_window_func_exprs(func: &WindowFunc, f: &mut impl FnMut(&Expr)) {
    match func {
        WindowFunc::RowNumber | WindowFunc::Rank | WindowFunc::DenseRank => {}
        WindowFunc::Lag { expr, default, .. } | WindowFunc::Lead { expr, default, .. } => {
            f(expr);
            if let Some(d) = default {
                f(d);
            }
        }
        WindowFunc::FirstValue(e)
        | WindowFunc::LastValue(e)
        | WindowFunc::Sum(e)
        | WindowFunc::Avg(e)
        | WindowFunc::Min(e)
        | WindowFunc::Max(e) => f(e),
        WindowFunc::Count(Some(e)) => f(e),
        WindowFunc::Count(None) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::sort_window::WindowSpec;
    use crate::sql::catalog::{
        raw_scores_schema, raw_spans_schema, raw_traces_schema, scores_view_schema,
        spans_view_schema, traces_view_schema,
    };
    use cetra_column::{Field, Schema};

    fn col_expr(schema: &Schema, name: &str) -> Expr {
        let idx = schema.index_of(name).expect("column in schema");
        let dt = schema.fields()[idx].data_type.clone();
        Expr::Column {
            index: idx,
            name: name.to_owned(),
            data_type: dt,
        }
    }

    fn bin(left: Expr, op: BinaryOp, right: Expr) -> Expr {
        let dt = match op {
            BinaryOp::And
            | BinaryOp::Or
            | BinaryOp::Eq
            | BinaryOp::NotEq
            | BinaryOp::Lt
            | BinaryOp::LtEq
            | BinaryOp::Gt
            | BinaryOp::GtEq
            | BinaryOp::IsDistinctFrom
            | BinaryOp::IsNotDistinctFrom
            | BinaryOp::ArrayContains => DataType::Bool,
            BinaryOp::Add | BinaryOp::Sub | BinaryOp::Mul | BinaryOp::Div | BinaryOp::Mod => {
                left.data_type()
            }
            BinaryOp::Concat => left.data_type(),
        };
        Expr::Binary {
            left: Box::new(left),
            op,
            right: Box::new(right),
            data_type: dt,
        }
    }

    fn encode_hex(inner: Expr) -> Expr {
        Expr::Call {
            func: ScalarFn::Encode,
            args: vec![inner, Expr::Literal(ScalarValue::Utf8("hex".to_owned()))],
            data_type: DataType::Utf8,
        }
    }

    fn raw_scan(table: &str, kind: SqlViewKind, schema: Arc<Schema>) -> LogicalPlan {
        LogicalPlan::Scan {
            table: table.to_owned(),
            kind,
            full_schema: Arc::clone(&schema),
            projection: None,
            filters: Vec::new(),
            time_range: TimeRangeHint::default(),
            id_lookups: Vec::new(),
            substrings: Vec::new(),
            map_projections: BTreeMap::new(),
            limit: None,
            schema,
        }
    }

    fn spans_view_project(scan: LogicalPlan) -> LogicalPlan {
        let raw = raw_spans_schema();
        let view = spans_view_schema();
        let exprs = view
            .fields()
            .iter()
            .map(|f| match f.name.as_str() {
                "trace_id" | "span_id" | "parent_span_id" => encode_hex(col_expr(&raw, &f.name)),
                "duration_ms" => bin(
                    Expr::Cast {
                        expr: Box::new(col_expr(&raw, "duration_ns")),
                        target: DataType::F64,
                        mode: CastMode::Strict,
                    },
                    BinaryOp::Div,
                    Expr::Literal(ScalarValue::F64(1_000_000.0)),
                ),
                other => col_expr(&raw, other),
            })
            .collect();
        LogicalPlan::Project {
            input: Box::new(scan),
            exprs,
            schema: view,
        }
    }

    fn traces_view_project(scan: LogicalPlan) -> LogicalPlan {
        let raw = raw_traces_schema();
        let view = traces_view_schema();
        let exprs = view
            .fields()
            .iter()
            .map(|f| match f.name.as_str() {
                "trace_id" | "root_span_id" => encode_hex(col_expr(&raw, &f.name)),
                "duration_ns" => bin(
                    Expr::Cast {
                        expr: Box::new(col_expr(&raw, "end_time")),
                        target: DataType::I64,
                        mode: CastMode::Strict,
                    },
                    BinaryOp::Sub,
                    Expr::Cast {
                        expr: Box::new(col_expr(&raw, "start_time")),
                        target: DataType::I64,
                        mode: CastMode::Strict,
                    },
                ),
                "duration_ms" => bin(
                    Expr::Literal(ScalarValue::F64(0.0)),
                    BinaryOp::Add,
                    Expr::Literal(ScalarValue::F64(0.0)),
                ),
                other => col_expr(&raw, other),
            })
            .collect();
        LogicalPlan::Project {
            input: Box::new(scan),
            exprs,
            schema: view,
        }
    }

    fn scores_view_project(scan: LogicalPlan) -> LogicalPlan {
        let raw = raw_scores_schema();
        let view = scores_view_schema();
        let exprs = view
            .fields()
            .iter()
            .map(|f| match f.name.as_str() {
                "trace_id" | "span_id" => encode_hex(col_expr(&raw, &f.name)),
                other => col_expr(&raw, other),
            })
            .collect();
        LogicalPlan::Project {
            input: Box::new(scan),
            exprs,
            schema: view,
        }
    }

    #[test]
    fn pass1_constant_folding_preserves_div_zero_and_simplifies_booleans() {
        let raw = raw_spans_schema();
        let scan = raw_scan("spans", SqlViewKind::Spans, Arc::clone(&raw));

        // 1 + 2 = 3 -> true -> Filter elided
        let trivial_pred = bin(
            bin(
                Expr::Literal(ScalarValue::I64(1)),
                BinaryOp::Add,
                Expr::Literal(ScalarValue::I64(2)),
            ),
            BinaryOp::Eq,
            Expr::Literal(ScalarValue::I64(3)),
        );
        let plan_true = optimize(LogicalPlan::Filter {
            input: Box::new(scan.clone()),
            predicate: trivial_pred,
        })
        .unwrap();
        assert!(matches!(plan_true, LogicalPlan::Scan { .. }));

        // NOT NOT (status = 'OK') AND true -> status = 'OK' pushed into Scan.filters
        let status_eq = bin(
            col_expr(&raw, "status"),
            BinaryOp::Eq,
            Expr::Literal(ScalarValue::Utf8("OK".to_owned())),
        );
        let double_not = Expr::Unary {
            op: UnaryOp::Not,
            expr: Box::new(Expr::Unary {
                op: UnaryOp::Not,
                expr: Box::new(status_eq.clone()),
                data_type: DataType::Bool,
            }),
            data_type: DataType::Bool,
        };
        let pred = bin(
            bin(
                double_not,
                BinaryOp::And,
                Expr::Literal(ScalarValue::Bool(true)),
            ),
            BinaryOp::Or,
            Expr::Literal(ScalarValue::Bool(false)),
        );
        let plan_simp = optimize(LogicalPlan::Filter {
            input: Box::new(scan.clone()),
            predicate: pred,
        })
        .unwrap();
        let LogicalPlan::Scan { filters, .. } = plan_simp else {
            panic!("expected Scan");
        };
        assert_eq!(filters, vec![status_eq.clone()]);

        // Statically false filter: (status = 'OK') AND false -> false & limit=Some(0)
        let false_plan = optimize(LogicalPlan::Filter {
            input: Box::new(scan.clone()),
            predicate: bin(
                status_eq,
                BinaryOp::And,
                Expr::Literal(ScalarValue::Bool(false)),
            ),
        })
        .unwrap();
        let LogicalPlan::Scan { filters, limit, .. } = false_plan else {
            panic!("expected Scan");
        };
        assert_eq!(filters, vec![Expr::Literal(ScalarValue::Bool(false))]);
        assert_eq!(limit, Some(0));

        // Division by zero MUST NOT be hidden by `... AND false`
        let div_zero = bin(
            bin(
                Expr::Literal(ScalarValue::I64(1)),
                BinaryOp::Div,
                col_expr(&raw, "duration_ns"),
            ),
            BinaryOp::Eq,
            Expr::Literal(ScalarValue::I64(1)),
        );
        let keep_err = optimize(LogicalPlan::Filter {
            input: Box::new(scan.clone()),
            predicate: bin(
                div_zero.clone(),
                BinaryOp::And,
                Expr::Literal(ScalarValue::Bool(false)),
            ),
        })
        .unwrap();
        let LogicalPlan::Scan { filters, limit, .. } = keep_err else {
            panic!("expected Scan");
        };
        assert_eq!(filters.len(), 2);
        assert_eq!(filters[0], div_zero);
        assert_eq!(filters[1], Expr::Literal(ScalarValue::Bool(false)));
        assert_eq!(limit, None);

        // Constant `1 / 0` MUST NOT be folded or swallowed; `false AND (1 / 0 = 1)` short-circuits to false
        let const_div_zero = bin(
            Expr::Literal(ScalarValue::I64(1)),
            BinaryOp::Div,
            Expr::Literal(ScalarValue::I64(0)),
        );
        let const_proj = optimize(LogicalPlan::Project {
            input: Box::new(LogicalPlan::EmptyRow {
                schema: Schema::try_new_arc(Vec::new()).unwrap(),
            }),
            exprs: vec![const_div_zero.clone()],
            schema: Schema::try_new_arc(vec![Field::new("x", DataType::I64, false)]).unwrap(),
        })
        .unwrap();
        let LogicalPlan::Project { exprs, .. } = const_proj else {
            panic!("expected Project");
        };
        assert_eq!(exprs, vec![const_div_zero.clone()]);

        let short_circuit_false = optimize(LogicalPlan::Filter {
            input: Box::new(scan),
            predicate: bin(
                Expr::Literal(ScalarValue::Bool(false)),
                BinaryOp::And,
                bin(
                    const_div_zero,
                    BinaryOp::Eq,
                    Expr::Literal(ScalarValue::I64(1)),
                ),
            ),
        })
        .unwrap();
        let LogicalPlan::Scan { filters, limit, .. } = short_circuit_false else {
            panic!("expected Scan");
        };
        assert_eq!(filters, vec![Expr::Literal(ScalarValue::Bool(false))]);
        assert_eq!(limit, Some(0));

        // `now()` MUST NOT be folded at optimize time
        let now_expr = Expr::Call {
            func: ScalarFn::Now,
            args: Vec::new(),
            data_type: DataType::TimestampNs { utc: true },
        };
        let now_plan = optimize(LogicalPlan::Project {
            input: Box::new(LogicalPlan::EmptyRow {
                schema: Schema::try_new_arc(Vec::new()).unwrap(),
            }),
            exprs: vec![now_expr.clone()],
            schema: Schema::try_new_arc(vec![Field::new(
                "ts",
                DataType::TimestampNs { utc: true },
                false,
            )])
            .unwrap(),
        })
        .unwrap();
        let LogicalPlan::Project { exprs, .. } = now_plan else {
            panic!("expected Project");
        };
        assert_eq!(exprs, vec![now_expr]);
    }

    #[test]
    fn filter_through_project_rewrites_hex_id_and_populates_hints_keeping_filters() {
        let view = spans_view_schema();
        let hex_lower = "a1000000000000000000000000000001";
        let span_hex = "0123456789abcdef";

        let base_scan = raw_scan("spans", SqlViewKind::Spans, raw_spans_schema());
        let view_proj = spans_view_project(base_scan);

        // Filter on view columns: trace_id = <32-hex> AND span_id IN (<16-hex>) AND start_time >= 100 AND name LIKE '%hello%'
        let predicate = combine_and(vec![
            bin(
                col_expr(&view, "trace_id"),
                BinaryOp::Eq,
                Expr::Literal(ScalarValue::Utf8(hex_lower.to_owned())),
            ),
            Expr::InList {
                expr: Box::new(col_expr(&view, "span_id")),
                list: vec![Expr::Literal(ScalarValue::Utf8(span_hex.to_owned()))],
                negated: false,
                redacted: false,
            },
            bin(
                col_expr(&view, "start_time"),
                BinaryOp::GtEq,
                Expr::Literal(ScalarValue::TimestampNs {
                    value: 100,
                    utc: true,
                }),
            ),
            Expr::Like {
                expr: Box::new(col_expr(&view, "name")),
                pattern: Box::new(Expr::Literal(ScalarValue::Utf8("%hello%".to_owned()))),
                escape: Some('\\'),
                case_insensitive: true,
                negated: false,
            },
        ])
        .unwrap();

        let filtered = LogicalPlan::Filter {
            input: Box::new(view_proj),
            predicate,
        };
        // Outer SELECT name
        let out_schema =
            Schema::try_new_arc(vec![Field::new("name", DataType::Utf8, false)]).unwrap();
        let top = LogicalPlan::Project {
            input: Box::new(filtered),
            exprs: vec![col_expr(&view, "name")],
            schema: out_schema,
        };

        let opt = optimize(top).unwrap();
        // Top -> inner view Project -> Scan
        let LogicalPlan::Project { input, .. } = opt else {
            panic!("expected outer Project");
        };
        let LogicalPlan::Project { input, .. } = *input else {
            panic!("expected inner view Project");
        };
        let LogicalPlan::Scan {
            projection,
            filters,
            time_range,
            id_lookups,
            substrings,
            schema,
            ..
        } = *input
        else {
            panic!("expected Scan");
        };

        // Projection pruned to ONLY `name`
        assert_eq!(schema.fields().len(), 1);
        assert_eq!(schema.fields()[0].name, "name");
        assert!(projection.is_some());

        // Full pushed predicates retained in Scan.filters, with hex rewritten to FixedBytes16 / FixedBytes8
        assert_eq!(filters.len(), 4);
        let expected_trace: [u8; 16] = hex::decode(hex_lower).unwrap().try_into().unwrap();
        let expected_span: [u8; 8] = hex::decode(span_hex).unwrap().try_into().unwrap();
        assert!(matches!(
            &filters[0],
            Expr::Binary {
                left,
                op: BinaryOp::Eq,
                right,
                ..
            } if matches!(left.as_ref(), Expr::Column { name, data_type: DataType::FixedBytes16, .. } if name == "trace_id")
                && matches!(right.as_ref(), Expr::Literal(ScalarValue::FixedBytes16(b)) if *b == expected_trace)
        ));
        assert!(matches!(
            &filters[1],
            Expr::InList {
                expr,
                list,
                negated: false,
                ..
            } if matches!(expr.as_ref(), Expr::Column { name, data_type: DataType::FixedBytes8, .. } if name == "span_id")
                && list == &vec![Expr::Literal(ScalarValue::FixedBytes8(expected_span))]
        ));

        // Hints populated
        assert_eq!(time_range.min_ns, Some(100));
        assert_eq!(
            id_lookups,
            vec![
                IdLookupHint::Fixed16 {
                    column: "trace_id".to_owned(),
                    values: vec![expected_trace],
                },
                IdLookupHint::Fixed8 {
                    column: "span_id".to_owned(),
                    values: vec![expected_span],
                },
            ]
        );
        assert_eq!(
            substrings,
            vec![vec![SubstringHint {
                column: "name".to_owned(),
                needle: "hello".to_owned(),
                case_insensitive: true,
            }]]
        );
    }

    #[test]
    fn invalid_and_mixed_hex_literals_stay_as_strings() {
        let view = spans_view_schema();
        let view_proj =
            spans_view_project(raw_scan("spans", SqlViewKind::Spans, raw_spans_schema()));
        let pred = combine_and(vec![
            bin(
                col_expr(&view, "trace_id"),
                BinaryOp::Eq,
                Expr::Literal(ScalarValue::Utf8("not-hex".to_owned())),
            ),
            bin(
                col_expr(&view, "trace_id"),
                BinaryOp::Eq,
                Expr::Literal(ScalarValue::Utf8(
                    "A1000000000000000000000000000001".to_owned(),
                )),
            ),
            Expr::InList {
                expr: Box::new(col_expr(&view, "trace_id")),
                list: vec![
                    Expr::Literal(ScalarValue::Utf8(
                        "a1000000000000000000000000000001".to_owned(),
                    )),
                    Expr::Literal(ScalarValue::Utf8("nope".to_owned())),
                ],
                negated: false,
                redacted: false,
            },
        ])
        .unwrap();
        let opt = optimize(LogicalPlan::Filter {
            input: Box::new(view_proj),
            predicate: pred,
        })
        .unwrap();
        let LogicalPlan::Project { input, .. } = opt else {
            panic!("expected Project");
        };
        let LogicalPlan::Scan {
            filters,
            id_lookups,
            ..
        } = *input
        else {
            panic!("expected Scan");
        };
        assert_eq!(filters.len(), 3);
        assert!(filters[0].fmt_plan().contains("encode("));
        assert!(filters[1].fmt_plan().contains("encode("));
        assert!(filters[2].fmt_plan().contains("encode("));
        assert!(id_lookups.is_empty());
    }

    #[test]
    fn filter_is_blocked_above_fallible_sort_key() {
        let schema = Schema::try_new_arc(vec![Field::new("x", DataType::I64, false)]).unwrap();
        let scan = raw_scan("nums", SqlViewKind::Batch, Arc::clone(&schema));
        let sort = LogicalPlan::Sort {
            input: Box::new(scan),
            keys: vec![SortKey {
                expr: bin(
                    Expr::Literal(ScalarValue::I64(10)),
                    BinaryOp::Div,
                    col_expr(&schema, "x"),
                ),
                descending: false,
                nulls_first: false,
            }],
        };
        let filtered = LogicalPlan::Filter {
            input: Box::new(sort),
            predicate: bin(
                col_expr(&schema, "x"),
                BinaryOp::Gt,
                Expr::Literal(ScalarValue::I64(0)),
            ),
        };
        let opt = optimize(filtered).unwrap();
        assert!(
            matches!(opt, LogicalPlan::Filter { ref input, .. } if matches!(input.as_ref(), LogicalPlan::Sort { .. })),
            "filter must stay above Sort when a sort key is fallible: {opt:?}"
        );
    }

    #[test]
    fn filter_is_blocked_below_window_and_aggregate() {
        let raw = raw_spans_schema();
        let scan = raw_scan("spans", SqlViewKind::Spans, Arc::clone(&raw));
        let win_schema = Schema::try_new_arc({
            let mut fields = raw.fields().to_vec();
            fields.push(Field::new("rn", DataType::I64, false));
            fields
        })
        .unwrap();
        let window = LogicalPlan::Window {
            input: Box::new(scan),
            partition_by: vec![col_expr(&raw, "trace_id")],
            order_by: vec![SortKey {
                expr: col_expr(&raw, "start_time"),
                descending: false,
                nulls_first: false,
            }],
            specs: vec![WindowSpec {
                partition_by: vec![col_expr(&raw, "trace_id")],
                order_by: vec![SortKey {
                    expr: col_expr(&raw, "start_time"),
                    descending: false,
                    nulls_first: false,
                }],
                frame: None,
                func: WindowFunc::RowNumber,
                output_name: "rn".to_owned(),
            }],
            schema: Arc::clone(&win_schema),
        };
        let proj = LogicalPlan::Project {
            input: Box::new(window),
            exprs: vec![col_expr(&win_schema, "name"), col_expr(&win_schema, "rn")],
            schema: Schema::try_new_arc(vec![
                Field::new("name", DataType::Utf8, false),
                Field::new("rn", DataType::I64, false),
            ])
            .unwrap(),
        };
        let filtered = LogicalPlan::Filter {
            input: Box::new(proj),
            predicate: bin(
                Expr::Column {
                    index: 1,
                    name: "rn".to_owned(),
                    data_type: DataType::I64,
                },
                BinaryOp::Eq,
                Expr::Literal(ScalarValue::I64(1)),
            ),
        };

        let opt = optimize(filtered).unwrap();
        let LogicalPlan::Project { input, .. } = opt else {
            panic!("expected outer Project");
        };
        let LogicalPlan::Filter { input, predicate } = *input else {
            panic!("expected Filter to stop ABOVE Window");
        };
        assert!(predicate.fmt_plan().contains("rn = 1"));
        let LogicalPlan::Window { input, .. } = *input else {
            panic!("expected Window below Filter");
        };
        let LogicalPlan::Scan { filters, .. } = *input else {
            panic!("expected Scan with no pushed filters");
        };
        assert!(filters.is_empty());
    }

    #[test]
    fn traces_and_scores_pushdown_respect_storage_safety_boundaries() {
        // Traces: only start_time and trace_id push into Scan; status stays above Scan
        let t_view = traces_view_schema();
        let t_proj =
            traces_view_project(raw_scan("traces", SqlViewKind::Traces, raw_traces_schema()));
        let trace_hex = "b2000000000000000000000000000002";
        let t_pred = combine_and(vec![
            bin(
                col_expr(&t_view, "start_time"),
                BinaryOp::GtEq,
                Expr::Literal(ScalarValue::TimestampNs {
                    value: 500,
                    utc: true,
                }),
            ),
            bin(
                col_expr(&t_view, "trace_id"),
                BinaryOp::Eq,
                Expr::Literal(ScalarValue::Utf8(trace_hex.to_owned())),
            ),
            bin(
                col_expr(&t_view, "status"),
                BinaryOp::Eq,
                Expr::Literal(ScalarValue::Utf8("ERROR".to_owned())),
            ),
        ])
        .unwrap();
        let t_opt = optimize(LogicalPlan::Filter {
            input: Box::new(t_proj),
            predicate: t_pred,
        })
        .unwrap();
        let LogicalPlan::Project { input, .. } = t_opt else {
            panic!("expected Project");
        };
        let LogicalPlan::Filter { input, predicate } = *input else {
            panic!("expected Filter above Traces Scan for `status`");
        };
        assert!(predicate.fmt_plan().contains("status ="));
        let LogicalPlan::Scan {
            filters,
            time_range,
            id_lookups,
            ..
        } = *input
        else {
            panic!("expected Scan");
        };
        assert_eq!(filters.len(), 2);
        assert_eq!(time_range.min_ns, Some(500));
        assert_eq!(id_lookups.len(), 1);

        // Scores: ONLY created_at pushes into Scan; name / deleted / value_num stay above Scan
        let s_view = scores_view_schema();
        let s_proj =
            scores_view_project(raw_scan("scores", SqlViewKind::Scores, raw_scores_schema()));
        let s_pred = combine_and(vec![
            bin(
                col_expr(&s_view, "created_at"),
                BinaryOp::GtEq,
                Expr::Literal(ScalarValue::TimestampNs {
                    value: 1_000,
                    utc: true,
                }),
            ),
            bin(
                col_expr(&s_view, "name"),
                BinaryOp::Eq,
                Expr::Literal(ScalarValue::Utf8("accuracy".to_owned())),
            ),
            bin(
                col_expr(&s_view, "deleted"),
                BinaryOp::Eq,
                Expr::Literal(ScalarValue::Bool(false)),
            ),
        ])
        .unwrap();
        let s_opt = optimize(LogicalPlan::Filter {
            input: Box::new(s_proj),
            predicate: s_pred,
        })
        .unwrap();
        let LogicalPlan::Project { input, .. } = s_opt else {
            panic!("expected Project");
        };
        let LogicalPlan::Filter { input, predicate } = *input else {
            panic!("expected Filter above Scores Scan");
        };
        assert!(predicate.fmt_plan().contains("name ="));
        assert!(predicate.fmt_plan().contains("deleted ="));
        let LogicalPlan::Scan {
            filters,
            time_range,
            id_lookups,
            ..
        } = *input
        else {
            panic!("expected Scan");
        };
        assert_eq!(filters.len(), 1);
        assert_eq!(time_range.min_ns, Some(1_000));
        assert!(id_lookups.is_empty());
    }

    #[test]
    fn join_pushdown_respects_join_semantics() {
        let left_schema = Schema::try_new_arc(vec![
            Field::new("id", DataType::I64, false),
            Field::new("lval", DataType::Utf8, true),
        ])
        .unwrap();
        let right_schema = Schema::try_new_arc(vec![
            Field::new("rid", DataType::I64, false),
            Field::new("rval", DataType::Utf8, true),
        ])
        .unwrap();
        let join_schema = Schema::try_new_arc(vec![
            Field::new("id", DataType::I64, false),
            Field::new("lval", DataType::Utf8, true),
            Field::new("rid", DataType::I64, true),
            Field::new("rval", DataType::Utf8, true),
        ])
        .unwrap();

        let mk_join = |kind| LogicalPlan::Join {
            left: Box::new(raw_scan("l", SqlViewKind::Batch, Arc::clone(&left_schema))),
            right: Box::new(raw_scan("r", SqlViewKind::Batch, Arc::clone(&right_schema))),
            kind,
            left_keys: vec![col_expr(&left_schema, "id")],
            right_keys: vec![col_expr(&right_schema, "rid")],
            null_equals_null: false,
            residual: None,
            schema: Arc::clone(&join_schema),
        };

        let left_only = bin(
            col_expr(&join_schema, "lval"),
            BinaryOp::Eq,
            Expr::Literal(ScalarValue::Utf8("a".to_owned())),
        );
        let right_only = bin(
            col_expr(&join_schema, "rval"),
            BinaryOp::Eq,
            Expr::Literal(ScalarValue::Utf8("b".to_owned())),
        );

        // Left join: pushes left_only into left, keeps right_only above Join!
        let left_opt = optimize(LogicalPlan::Filter {
            input: Box::new(mk_join(LogicalJoinKind::Left)),
            predicate: bin(left_only.clone(), BinaryOp::And, right_only.clone()),
        })
        .unwrap();
        let LogicalPlan::Filter { input, predicate } = left_opt else {
            panic!("expected Filter above Left Join for right_only");
        };
        assert!(predicate.fmt_plan().contains("rval ="));
        let LogicalPlan::Join { left, right, .. } = *input else {
            panic!("expected Join");
        };
        assert!(matches!(*left, LogicalPlan::Scan { filters, .. } if filters.len() == 1));
        assert!(matches!(*right, LogicalPlan::Scan { filters, .. } if filters.is_empty()));

        // Inner join: pushes both sides into their respective Scans!
        let inner_opt = optimize(LogicalPlan::Filter {
            input: Box::new(mk_join(LogicalJoinKind::Inner)),
            predicate: bin(left_only, BinaryOp::And, right_only),
        })
        .unwrap();
        let LogicalPlan::Join { left, right, .. } = inner_opt else {
            panic!("expected Filter-free Inner Join");
        };
        assert!(matches!(*left, LogicalPlan::Scan { filters, .. } if filters.len() == 1));
        assert!(matches!(*right, LogicalPlan::Scan { filters, .. } if filters.len() == 1));
    }

    #[test]
    fn projection_and_map_key_pushdown_and_empty_count_star_projection() {
        let raw = raw_spans_schema();
        let view = spans_view_schema();
        let scan = raw_scan("spans", SqlViewKind::Spans, raw);
        let proj = spans_view_project(scan);
        // Filter on attributes['http.status_code'] = '200'
        let filter = LogicalPlan::Filter {
            input: Box::new(proj),
            predicate: bin(
                Expr::MapIndex {
                    map: Box::new(col_expr(&view, "attributes")),
                    key: Box::new(Expr::Literal(ScalarValue::Utf8(
                        "http.status_code".to_owned(),
                    ))),
                },
                BinaryOp::Eq,
                Expr::Literal(ScalarValue::Utf8("200".to_owned())),
            ),
        };
        // Outer project selects metadata['customer'] and name
        let outer = LogicalPlan::Project {
            input: Box::new(filter),
            exprs: vec![
                Expr::MapIndex {
                    map: Box::new(col_expr(&view, "metadata")),
                    key: Box::new(Expr::Literal(ScalarValue::Utf8("customer".to_owned()))),
                },
                col_expr(&view, "name"),
            ],
            schema: Schema::try_new_arc(vec![
                Field::new("cust", DataType::Utf8, true),
                Field::new("name", DataType::Utf8, false),
            ])
            .unwrap(),
        };

        let opt = optimize(outer).unwrap();
        let LogicalPlan::Project { input, .. } = opt else {
            panic!("expected outer Project");
        };
        let LogicalPlan::Project { input, .. } = *input else {
            panic!("expected inner Project");
        };
        let LogicalPlan::Scan {
            projection,
            map_projections,
            schema,
            ..
        } = *input
        else {
            panic!("expected Scan");
        };
        // Only `name` and `metadata` needed by output (`attributes` only in Scan.filters)
        let proj_names: Vec<&str> = schema.fields().iter().map(|f| f.name.as_str()).collect();
        assert_eq!(proj_names, vec!["name", "metadata"]);
        assert_eq!(projection.as_ref().map(Vec::len), Some(2));
        assert_eq!(
            map_projections,
            BTreeMap::from([
                (
                    "attributes".to_owned(),
                    MapProjection::Keys(BTreeSet::from(["http.status_code".to_owned()])),
                ),
                (
                    "metadata".to_owned(),
                    MapProjection::Keys(BTreeSet::from(["customer".to_owned()])),
                ),
            ])
        );

        // SELECT count(*) FROM spans -> Scan.projection = Some(vec![])
        let count_star = LogicalPlan::Aggregate {
            input: Box::new(spans_view_project(raw_scan(
                "spans",
                SqlViewKind::Spans,
                raw_spans_schema(),
            ))),
            group_by: Vec::new(),
            aggs: vec![AggExpr {
                func: AggFunc::CountStar,
                filter: None,
                output_name: "cnt".to_owned(),
            }],
            schema: Schema::try_new_arc(vec![Field::new("cnt", DataType::I64, false)]).unwrap(),
        };
        let opt_cnt = optimize(count_star).unwrap();
        let LogicalPlan::Aggregate { input, .. } = opt_cnt else {
            panic!("expected Aggregate");
        };
        let LogicalPlan::Project { input, .. } = *input else {
            panic!("expected Project");
        };
        let LogicalPlan::Scan {
            projection, schema, ..
        } = *input
        else {
            panic!("expected Scan");
        };
        assert_eq!(projection, Some(vec![]));
        assert_eq!(schema.len(), 0);
    }

    #[test]
    fn topk_rewrite_honors_65536_bound() {
        let schema = Schema::try_new_arc(vec![Field::new("n", DataType::I64, false)]).unwrap();
        let sort_key = SortKey {
            expr: col_expr(&schema, "n"),
            descending: true,
            nulls_first: true,
        };

        let mk_limit_sort = |skip, fetch| LogicalPlan::Limit {
            input: Box::new(LogicalPlan::Sort {
                input: Box::new(raw_scan("t", SqlViewKind::Batch, Arc::clone(&schema))),
                keys: vec![sort_key.clone()],
            }),
            skip,
            fetch,
        };

        // LIMIT 50 OFFSET 10 -> TopK(limit=50, offset=10)
        let small = optimize(mk_limit_sort(10, Some(50))).unwrap();
        assert!(matches!(
            small,
            LogicalPlan::TopK {
                limit: 50,
                offset: 10,
                ..
            }
        ));

        // Exact cap 65_536 -> TopK
        let at_cap = optimize(mk_limit_sort(0, Some(65_536))).unwrap();
        assert!(matches!(
            at_cap,
            LogicalPlan::TopK {
                limit: 65_536,
                offset: 0,
                ..
            }
        ));

        // LIMIT 100000 -> stays Limit + Sort
        let over_cap = optimize(mk_limit_sort(0, Some(100_000))).unwrap();
        assert!(matches!(
            over_cap,
            LogicalPlan::Limit {
                skip: 0,
                fetch: Some(100_000),
                input,
            } if matches!(*input, LogicalPlan::Sort { .. })
        ));

        // skip + fetch = 65_537 -> stays Limit + Sort
        let over_sum = optimize(mk_limit_sort(1, Some(65_536))).unwrap();
        assert!(matches!(over_sum, LogicalPlan::Limit { .. }));

        // Unbounded fetch -> stays Limit + Sort
        let unbounded = optimize(mk_limit_sort(10, None)).unwrap();
        assert!(matches!(unbounded, LogicalPlan::Limit { .. }));

        // Limit -> Project(cols only) -> Sort -> Project -> TopK
        let col_proj = LogicalPlan::Project {
            input: Box::new(LogicalPlan::Sort {
                input: Box::new(raw_scan("t", SqlViewKind::Batch, Arc::clone(&schema))),
                keys: vec![sort_key],
            }),
            exprs: vec![col_expr(&schema, "n")],
            schema: Arc::clone(&schema),
        };
        let proj_topk = optimize(LogicalPlan::Limit {
            input: Box::new(col_proj),
            skip: 5,
            fetch: Some(25),
        })
        .unwrap();
        assert!(matches!(
            proj_topk,
            LogicalPlan::Project { input, .. }
                if matches!(*input, LogicalPlan::TopK { limit: 25, offset: 5, .. })
        ));
    }

    #[test]
    fn hex_rewrite_collapses_same_width_encoded_columns_and_aggregate_prunes_unused_aggs() {
        let raw = raw_spans_schema();
        let view = spans_view_schema();
        let proj = spans_view_project(raw_scan("spans", SqlViewKind::Spans, raw));

        // encode(span_id, 'hex') = encode(parent_span_id, 'hex') -> span_id = parent_span_id
        let eq_encoded = bin(
            col_expr(&view, "span_id"),
            BinaryOp::Eq,
            col_expr(&view, "parent_span_id"),
        );
        let opt = optimize(LogicalPlan::Filter {
            input: Box::new(proj),
            predicate: eq_encoded,
        })
        .unwrap();
        let LogicalPlan::Project { input, .. } = opt else {
            panic!("expected Project");
        };
        let LogicalPlan::Scan { filters, .. } = *input else {
            panic!("expected Scan");
        };
        assert_eq!(filters.len(), 1);
        assert!(matches!(
            &filters[0],
            Expr::Binary {
                left,
                op: BinaryOp::Eq,
                right,
                ..
            } if matches!(left.as_ref(), Expr::Column { name, data_type: DataType::FixedBytes8, .. } if name == "span_id")
                && matches!(right.as_ref(), Expr::Column { name, data_type: DataType::FixedBytes8, .. } if name == "parent_span_id")
        ));

        // Aggregate with 2 aggs where outer Project only selects group_by + agg0 -> agg1 pruned
        let agg_in = raw_scan("spans", SqlViewKind::Spans, raw_spans_schema());
        let r = raw_spans_schema();
        let agg_schema = Schema::try_new_arc(vec![
            Field::new("status", DataType::Utf8, false),
            Field::new("cnt", DataType::I64, false),
            Field::new("sum_dur", DataType::I64, true),
        ])
        .unwrap();
        let agg = LogicalPlan::Aggregate {
            input: Box::new(agg_in),
            group_by: vec![("status".to_owned(), col_expr(&r, "status"))],
            aggs: vec![
                AggExpr {
                    func: AggFunc::CountStar,
                    filter: None,
                    output_name: "cnt".to_owned(),
                },
                AggExpr {
                    func: AggFunc::Sum(col_expr(&r, "duration_ns")),
                    filter: None,
                    output_name: "sum_dur".to_owned(),
                },
            ],
            schema: Arc::clone(&agg_schema),
        };
        let outer = LogicalPlan::Project {
            input: Box::new(agg),
            exprs: vec![
                col_expr(&agg_schema, "status"),
                col_expr(&agg_schema, "cnt"),
            ],
            schema: Schema::try_new_arc(vec![
                Field::new("status", DataType::Utf8, false),
                Field::new("cnt", DataType::I64, false),
            ])
            .unwrap(),
        };
        let opt_agg = optimize(outer).unwrap();
        let LogicalPlan::Project { input, .. } = opt_agg else {
            panic!("expected Project");
        };
        let LogicalPlan::Aggregate { aggs, input, .. } = *input else {
            panic!("expected Aggregate");
        };
        assert_eq!(aggs.len(), 1);
        assert_eq!(aggs[0].output_name, "cnt");
        let LogicalPlan::Scan { schema, .. } = *input else {
            panic!("expected Scan");
        };
        // Only `status` needed from Scan (duration_ns was pruned!)
        assert_eq!(schema.len(), 1);
        assert_eq!(schema.fields()[0].name, "status");
    }

    #[test]
    fn pass1_boolean_identities_preserve_bool_null_type_and_decimal_cmp_not_infallible() {
        let mut folder = ConstFolder::new().unwrap();
        let untyped_null = Expr::Literal(ScalarValue::Null(DataType::Null));
        let bool_null = Expr::Literal(ScalarValue::Null(DataType::Bool));

        // NULL AND true -> ScalarValue::Null(DataType::Bool)
        let null_and_true = folder.fold_expr(bin(
            untyped_null.clone(),
            BinaryOp::And,
            Expr::Literal(ScalarValue::Bool(true)),
        ));
        assert_eq!(null_and_true, bool_null);
        assert_eq!(null_and_true.data_type(), DataType::Bool);

        // false OR NULL -> ScalarValue::Null(DataType::Bool)
        let false_or_null = folder.fold_expr(bin(
            Expr::Literal(ScalarValue::Bool(false)),
            BinaryOp::Or,
            untyped_null.clone(),
        ));
        assert_eq!(false_or_null, bool_null);
        assert_eq!(false_or_null.data_type(), DataType::Bool);

        // NOT NOT NULL -> ScalarValue::Null(DataType::Bool)
        let not_not_null = folder.fold_expr(Expr::Unary {
            op: UnaryOp::Not,
            expr: Box::new(Expr::Unary {
                op: UnaryOp::Not,
                expr: Box::new(untyped_null),
                data_type: DataType::Bool,
            }),
            data_type: DataType::Bool,
        });
        assert_eq!(not_not_null, bool_null);
        assert_eq!(not_not_null.data_type(), DataType::Bool);

        // Decimal128 comparison is excluded from is_infallible, so `dec_cmp AND false` is preserved.
        let dec_col = Expr::Column {
            index: 0,
            name: "price".to_owned(),
            data_type: DataType::Decimal128 {
                precision: 10,
                scale: 2,
            },
        };
        let dec_lit = Expr::Literal(ScalarValue::Decimal128 {
            value: 100,
            precision: 10,
            scale: 4,
        });
        let dec_cmp = bin(dec_col, BinaryOp::Eq, dec_lit);
        assert!(!is_infallible(&dec_cmp));
        let folded_dec = folder.fold_expr(bin(
            dec_cmp,
            BinaryOp::And,
            Expr::Literal(ScalarValue::Bool(false)),
        ));
        assert!(matches!(
            folded_dec,
            Expr::Binary {
                op: BinaryOp::And,
                ..
            }
        ));
    }

    #[test]
    fn pass4_fallible_conjunct_keeps_limit_none_and_resolves_full_schema_column_names() {
        let raw = raw_spans_schema();
        let scan = raw_scan("spans", SqlViewKind::Spans, Arc::clone(&raw));

        // `(1 / duration_ns = 1) AND false` must NOT set `limit = Some(0)`!
        let div_zero = bin(
            bin(
                Expr::Literal(ScalarValue::I64(1)),
                BinaryOp::Div,
                col_expr(&raw, "duration_ns"),
            ),
            BinaryOp::Eq,
            Expr::Literal(ScalarValue::I64(1)),
        );
        let opt = optimize(LogicalPlan::Filter {
            input: Box::new(scan.clone()),
            predicate: bin(
                div_zero,
                BinaryOp::And,
                Expr::Literal(ScalarValue::Bool(false)),
            ),
        })
        .unwrap();
        let LogicalPlan::Scan { filters, limit, .. } = opt else {
            panic!("expected Scan");
        };
        assert_eq!(filters.len(), 2);
        assert_eq!(limit, None);

        // Aliased/stale Expr::Column.name still extracts hints using full_schema.fields()[index].
        let st_idx = raw.index_of("start_time").unwrap();
        let name_idx = raw.index_of("name").unwrap();
        let stale_time_pred = bin(
            Expr::Column {
                index: st_idx,
                name: "aliased_ts".to_owned(),
                data_type: DataType::TimestampNs { utc: true },
            },
            BinaryOp::GtEq,
            Expr::Literal(ScalarValue::TimestampNs {
                value: 1_000,
                utc: true,
            }),
        );
        let stale_like_pred = Expr::Like {
            expr: Box::new(Expr::Column {
                index: name_idx,
                name: "aliased_name".to_owned(),
                data_type: DataType::Utf8,
            }),
            pattern: Box::new(Expr::Literal(ScalarValue::Utf8("%needle%".to_owned()))),
            escape: None,
            case_insensitive: false,
            negated: false,
        };
        let opt_hints = optimize(LogicalPlan::Filter {
            input: Box::new(scan),
            predicate: bin(stale_time_pred, BinaryOp::And, stale_like_pred),
        })
        .unwrap();
        let LogicalPlan::Scan {
            time_range,
            substrings,
            ..
        } = opt_hints
        else {
            panic!("expected Scan");
        };
        assert_eq!(time_range.min_ns, Some(1_000));
        assert_eq!(
            substrings,
            vec![vec![SubstringHint {
                column: "name".to_owned(),
                needle: "needle".to_owned(),
                case_insensitive: false,
            }]]
        );
    }

    #[test]
    fn pass5_union_all_with_aggregate_branch_and_outer_project_aligns_branch_schemas() {
        let raw = raw_spans_schema();
        let agg_schema = Schema::try_new_arc(vec![
            Field::new("status", DataType::Utf8, false),
            Field::new("cnt", DataType::I64, false),
        ])
        .unwrap();
        // Branch 0: Aggregate (GROUP BY status, COUNT(*) AS cnt).
        // Pass 5 keeps `status` in Aggregate even when outer Project only reads `cnt`!
        let agg_branch = LogicalPlan::Aggregate {
            input: Box::new(raw_scan("spans", SqlViewKind::Spans, Arc::clone(&raw))),
            group_by: vec![("status".to_owned(), col_expr(&raw, "status"))],
            aggs: vec![AggExpr {
                func: AggFunc::CountStar,
                filter: None,
                output_name: "cnt".to_owned(),
            }],
            schema: Arc::clone(&agg_schema),
        };
        // Branch 1: Project selecting (status, 1).
        let proj_branch = LogicalPlan::Project {
            input: Box::new(raw_scan("spans", SqlViewKind::Spans, Arc::clone(&raw))),
            exprs: vec![col_expr(&raw, "status"), Expr::Literal(ScalarValue::I64(1))],
            schema: Arc::clone(&agg_schema),
        };

        let union_plan = LogicalPlan::Union {
            inputs: vec![agg_branch, proj_branch],
            all: true,
            schema: Arc::clone(&agg_schema),
        };
        // Outer Project keeps only column 1 (`cnt`).
        let outer = LogicalPlan::Project {
            input: Box::new(union_plan),
            exprs: vec![col_expr(&agg_schema, "cnt")],
            schema: Schema::try_new_arc(vec![Field::new("cnt", DataType::I64, false)]).unwrap(),
        };

        let opt = optimize(outer).unwrap();
        let LogicalPlan::Project { input, .. } = opt else {
            panic!("expected outer Project");
        };
        let LogicalPlan::Union { inputs, schema, .. } = *input else {
            panic!("expected Union");
        };
        assert_eq!(schema.len(), 1);
        assert_eq!(schema.fields()[0].name, "cnt");
        assert_eq!(inputs.len(), 2);
        for (i, branch) in inputs.iter().enumerate() {
            assert_eq!(
                branch.schema().len(),
                schema.len(),
                "branch {i} schema len mismatch"
            );
            assert_eq!(branch.schema().fields()[0].data_type, DataType::I64);
        }
        // Branch 0 must be wrapped in Project selecting [cnt] (index 1 from Aggregate).
        assert!(matches!(
            &inputs[0],
            LogicalPlan::Project { input, exprs, .. }
                if matches!(input.as_ref(), LogicalPlan::Aggregate { .. })
                    && matches!(exprs.as_slice(), [Expr::Column { index: 1, .. }])
        ));
    }

    fn bind_and_opt(sql: &str) -> LogicalPlan {
        let catalog = crate::sql::SqlCatalog::new(None, None);
        let (stmt, _) =
            crate::sql::validate::parse_and_validate_sql(sql, &catalog.registered_names()).unwrap();
        let plan = crate::sql::binder::bind_statement(
            &stmt,
            &catalog,
            &crate::sql::BoundParams::Values(&[]),
        )
        .unwrap();
        optimize(plan).unwrap()
    }

    fn find_first_scan(plan: &LogicalPlan) -> Option<&LogicalPlan> {
        let mut stack = vec![plan];
        while let Some(node) = stack.pop() {
            match node {
                LogicalPlan::Scan { .. } => return Some(node),
                LogicalPlan::Filter { input, .. }
                | LogicalPlan::Project { input, .. }
                | LogicalPlan::Aggregate { input, .. }
                | LogicalPlan::Window { input, .. }
                | LogicalPlan::Sort { input, .. }
                | LogicalPlan::Limit { input, .. }
                | LogicalPlan::TopK { input, .. }
                | LogicalPlan::Distinct { input }
                | LogicalPlan::Unnest { input, .. } => stack.push(input),
                LogicalPlan::Join { left, right, .. } => {
                    stack.push(right);
                    stack.push(left);
                }
                LogicalPlan::Union { inputs, .. } => {
                    for b in inputs {
                        stack.push(b);
                    }
                }
                LogicalPlan::Explain { inner, .. } => stack.push(inner),
                LogicalPlan::Values { .. } | LogicalPlan::EmptyRow { .. } => {}
            }
        }
        None
    }

    #[test]
    fn const_folder_enforces_64kib_cap_and_resets_reservation() {
        let mut folder = ConstFolder::new().unwrap();
        let big_a = Expr::Literal(ScalarValue::Utf8("a".repeat(40 * 1024)));
        let big_b = Expr::Literal(ScalarValue::Utf8("b".repeat(40 * 1024)));
        let oversized = Expr::Binary {
            left: Box::new(big_a),
            op: BinaryOp::Concat,
            right: Box::new(big_b),
            data_type: DataType::Utf8,
        };
        let out = folder.fold_expr(oversized);
        assert!(
            matches!(
                out,
                Expr::Binary {
                    op: BinaryOp::Concat,
                    ..
                }
            ),
            "expected oversized concat to stay unfolded, got {out:?}"
        );
        assert_eq!(folder.res.size(), 0);

        // Subsequent small constant fold succeeds cleanly.
        let small = bin(
            Expr::Literal(ScalarValue::I64(10)),
            BinaryOp::Add,
            Expr::Literal(ScalarValue::I64(32)),
        );
        assert_eq!(folder.fold_expr(small), Expr::Literal(ScalarValue::I64(42)));
        assert_eq!(folder.res.size(), 0);
    }

    #[test]
    fn pass2_project_substitution_bounds_nodes_and_duplicate_nontrivial_refs() {
        let schema = Schema::try_new_arc(vec![Field::new("a", DataType::Utf8, false)]).unwrap();
        let int_schema = Schema::try_new_arc(vec![Field::new("x", DataType::I64, false)]).unwrap();

        // Duplicate non-trivial projection ref (`WHERE p = p` over `SELECT coalesce(a, 'x') AS p`)
        // keeps conjunct above Project even though `coalesce` is infallible.
        let proj_coalesce = LogicalPlan::Project {
            input: Box::new(raw_scan("t", SqlViewKind::Batch, Arc::clone(&schema))),
            exprs: vec![Expr::Call {
                func: ScalarFn::Coalesce,
                args: vec![
                    col_expr(&schema, "a"),
                    Expr::Literal(ScalarValue::Utf8("x".to_owned())),
                ],
                data_type: DataType::Utf8,
            }],
            schema: Schema::try_new_arc(vec![Field::new("p", DataType::Utf8, false)]).unwrap(),
        };
        let dup_filter = LogicalPlan::Filter {
            input: Box::new(proj_coalesce),
            predicate: bin(
                Expr::Column {
                    index: 0,
                    name: "p".to_owned(),
                    data_type: DataType::Utf8,
                },
                BinaryOp::Eq,
                Expr::Column {
                    index: 0,
                    name: "p".to_owned(),
                    data_type: DataType::Utf8,
                },
            ),
        };
        let opt_dup = optimize(dup_filter).unwrap();
        assert!(matches!(opt_dup, LogicalPlan::Filter { .. }));

        // `substitute_expr` rejects > MAX_SUBSTITUTED_EXPR_NODES (2_048) cleanly.
        let big_proj_expr = (0..1_200).fold(col_expr(&int_schema, "x"), |acc, _| Expr::Call {
            func: ScalarFn::Coalesce,
            args: vec![acc, Expr::Literal(ScalarValue::I64(0))],
            data_type: DataType::I64,
        });
        let big_conj = bin(
            Expr::Column {
                index: 0,
                name: "x".to_owned(),
                data_type: DataType::I64,
            },
            BinaryOp::Eq,
            Expr::Literal(ScalarValue::I64(1)),
        );
        assert!(
            substitute_expr(&big_conj, &[big_proj_expr])
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn is_infallible_and_base_view_trace_id_pushdown() {
        let dec_ty = DataType::Decimal128 {
            precision: 10,
            scale: 2,
        };
        let str_col = Expr::Column {
            index: 0,
            name: "s".to_owned(),
            data_type: DataType::Utf8,
        };
        let i64_col = Expr::Column {
            index: 0,
            name: "i".to_owned(),
            data_type: DataType::I64,
        };
        let f64_col = Expr::Column {
            index: 0,
            name: "f".to_owned(),
            data_type: DataType::F64,
        };
        let dec_col = Expr::Column {
            index: 0,
            name: "d".to_owned(),
            data_type: dec_ty.clone(),
        };
        let map_col = Expr::Column {
            index: 0,
            name: "m".to_owned(),
            data_type: DataType::MapUtf8,
        };
        let arr_col = Expr::Column {
            index: 0,
            name: "a".to_owned(),
            data_type: DataType::List(Box::new(DataType::Utf8)),
        };

        // Newly marked fallible forms
        assert!(!is_infallible(&Expr::Binary {
            left: Box::new(str_col.clone()),
            op: BinaryOp::Concat,
            right: Box::new(str_col.clone()),
            data_type: DataType::Utf8,
        }));
        assert!(!is_infallible(&Expr::Like {
            expr: Box::new(str_col.clone()),
            pattern: Box::new(Expr::Literal(ScalarValue::Utf8("%a%".to_owned()))),
            escape: Some('\\'),
            case_insensitive: false,
            negated: false,
        }));
        for (func, arg, dt) in [
            (ScalarFn::Concat, str_col.clone(), DataType::Utf8),
            (ScalarFn::ConcatWs, str_col, DataType::Utf8),
            (
                ScalarFn::ArrayDistinct,
                arr_col,
                DataType::List(Box::new(DataType::Utf8)),
            ),
            (
                ScalarFn::MapKeys,
                map_col.clone(),
                DataType::List(Box::new(DataType::Utf8)),
            ),
            (
                ScalarFn::MapValues,
                map_col,
                DataType::List(Box::new(DataType::Utf8)),
            ),
            (ScalarFn::Floor, dec_col.clone(), dec_ty.clone()),
            (ScalarFn::Ceil, dec_col.clone(), dec_ty),
        ] {
            assert!(!is_infallible(&Expr::Call {
                func,
                args: vec![arg],
                data_type: dt,
            }));
        }

        // Explicitly infallible forms (R1-B1, H3b-B1)
        let fb8_col = Expr::Column {
            index: 0,
            name: "span_id".to_owned(),
            data_type: DataType::FixedBytes8,
        };
        let fb16_col = Expr::Column {
            index: 0,
            name: "trace_id".to_owned(),
            data_type: DataType::FixedBytes16,
        };
        let null_col = Expr::Column {
            index: 0,
            name: "n".to_owned(),
            data_type: DataType::Null,
        };
        let bin_col = Expr::Column {
            index: 0,
            name: "b".to_owned(),
            data_type: DataType::Binary,
        };
        for (col, fmt) in [
            (fb16_col.clone(), "hex"),
            (fb16_col.clone(), "HEX"),
            (fb8_col, "hex"),
            (null_col, "hex"),
        ] {
            assert!(is_infallible(&Expr::Call {
                func: ScalarFn::Encode,
                args: vec![col, Expr::Literal(ScalarValue::Utf8(fmt.to_owned()))],
                data_type: DataType::Utf8,
            }));
        }
        for (col, fmt) in [
            (fb16_col.clone(), "base64"),
            (fb16_col.clone(), "BASE64"),
            (fb16_col.clone(), "utf8"),
            (bin_col, "hex"),
        ] {
            assert!(!is_infallible(&Expr::Call {
                func: ScalarFn::Encode,
                args: vec![col, Expr::Literal(ScalarValue::Utf8(fmt.to_owned()))],
                data_type: DataType::Utf8,
            }));
        }

        // Case with operand must also reject Decimal128 in WHEN arms (H3b-m2)
        assert!(!is_infallible(&Expr::Case {
            operand: Some(Box::new(i64_col.clone())),
            when_then: vec![(dec_col.clone(), Expr::Literal(ScalarValue::I64(1)))],
            else_expr: Some(Box::new(Expr::Literal(ScalarValue::I64(0)))),
            data_type: DataType::I64,
        }));

        for (src, dst) in [
            (i64_col.clone(), DataType::F64),
            (
                Expr::Column {
                    index: 0,
                    name: "ts".to_owned(),
                    data_type: DataType::timestamp_utc(),
                },
                DataType::I64,
            ),
            (i64_col, DataType::timestamp_utc()),
            (
                Expr::Column {
                    index: 0,
                    name: "ts".to_owned(),
                    data_type: DataType::timestamp_utc(),
                },
                DataType::TimestampNs { utc: false },
            ),
            (fb16_col, DataType::Binary),
            (dec_col, DataType::F64),
        ] {
            assert!(is_infallible(&Expr::Cast {
                expr: Box::new(src),
                target: dst,
                mode: CastMode::Strict,
            }));
        }
        assert!(is_infallible(&Expr::Binary {
            left: Box::new(f64_col.clone()),
            op: BinaryOp::Div,
            right: Box::new(Expr::Literal(ScalarValue::F64(1_000_000.0))),
            data_type: DataType::F64,
        }));
        assert!(!is_infallible(&Expr::Binary {
            left: Box::new(f64_col),
            op: BinaryOp::Div,
            right: Box::new(Expr::Literal(ScalarValue::F64(0.0))),
            data_type: DataType::F64,
        }));

        // trace_id = '<32 hex>' pushes IdLookupHint::Fixed16 into Scan for spans and traces;
        // scores restricts pushdown to created_at and score_id (since upserts can change trace_id).
        let trace_hex = "0102030405060708090a0b0c0d0e0f10";
        let expected_bytes: [u8; 16] = hex::decode(trace_hex).unwrap().try_into().unwrap();
        for view in ["spans", "traces"] {
            let sql = format!("SELECT trace_id FROM {view} WHERE trace_id = '{trace_hex}'");
            let plan = bind_and_opt(&sql);
            let LogicalPlan::Scan { id_lookups, .. } = find_first_scan(&plan).expect("Scan node")
            else {
                unreachable!()
            };
            assert_eq!(
                id_lookups,
                &vec![IdLookupHint::Fixed16 {
                    column: "trace_id".to_owned(),
                    values: vec![expected_bytes],
                }],
                "missing trace_id IdLookupHint for {view}: {plan:?}"
            );
        }
        let scores_trace_plan = bind_and_opt(&format!(
            "SELECT trace_id FROM scores WHERE trace_id = '{trace_hex}'"
        ));
        let LogicalPlan::Scan {
            id_lookups: scores_trace_hints,
            ..
        } = find_first_scan(&scores_trace_plan).expect("Scores Scan node")
        else {
            unreachable!()
        };
        assert!(
            scores_trace_hints.is_empty(),
            "scores trace_id filter must not push pre-LWW: {scores_trace_hints:?}"
        );
        let scores_id_plan = bind_and_opt("SELECT score_id FROM scores WHERE score_id = 'score-1'");
        let LogicalPlan::Scan {
            id_lookups: scores_id_hints,
            ..
        } = find_first_scan(&scores_id_plan).expect("Scores Scan node")
        else {
            unreachable!()
        };
        assert_eq!(
            scores_id_hints,
            &vec![IdLookupHint::Utf8 {
                column: "score_id".to_owned(),
                values: vec!["score-1".to_owned()],
            }],
            "missing score_id IdLookupHint for scores: {scores_id_plan:?}"
        );

        // duration_ms > 5.0 on spans pushes down into Scan.filters
        let ms_plan = bind_and_opt("SELECT name FROM spans WHERE duration_ms > 5.0");
        let LogicalPlan::Scan { filters, .. } = find_first_scan(&ms_plan).unwrap() else {
            unreachable!()
        };
        assert_eq!(filters.len(), 1);
    }

    #[test]
    fn join_does_not_push_fallible_conjuncts_or_constants() {
        let ab_schema = Schema::try_new_arc(vec![
            Field::new("id", DataType::I64, false),
            Field::new("x", DataType::I64, false),
        ])
        .unwrap();
        let make_join = || LogicalPlan::Join {
            left: Box::new(raw_scan("a", SqlViewKind::Batch, Arc::clone(&ab_schema))),
            right: Box::new(raw_scan("b", SqlViewKind::Batch, Arc::clone(&ab_schema))),
            kind: LogicalJoinKind::Inner,
            left_keys: vec![col_expr(&ab_schema, "id")],
            right_keys: vec![col_expr(&ab_schema, "id")],
            null_equals_null: false,
            residual: None,
            schema: Schema::try_new_arc(vec![
                Field::new("id", DataType::I64, false),
                Field::new("x", DataType::I64, false),
                Field::new("id_r", DataType::I64, false),
                Field::new("x_r", DataType::I64, false),
            ])
            .unwrap(),
        };

        // 100 / a.x > 10 directly above Join must stay above Join.
        let div_pred = bin(
            Expr::Binary {
                left: Box::new(Expr::Literal(ScalarValue::I64(100))),
                op: BinaryOp::Div,
                right: Box::new(Expr::Column {
                    index: 1,
                    name: "x".to_owned(),
                    data_type: DataType::I64,
                }),
                data_type: DataType::I64,
            },
            BinaryOp::Gt,
            Expr::Literal(ScalarValue::I64(10)),
        );
        let opt = optimize(LogicalPlan::Filter {
            input: Box::new(make_join()),
            predicate: div_pred,
        })
        .unwrap();
        let LogicalPlan::Filter { input, .. } = opt else {
            panic!("expected fallible predicate to stay above Join, got {opt:?}");
        };
        let LogicalPlan::Join { left, right, .. } = *input else {
            panic!("expected Join below Filter");
        };
        assert!(matches!(*left, LogicalPlan::Scan { filters, .. } if filters.is_empty()));
        assert!(matches!(*right, LogicalPlan::Scan { filters, .. } if filters.is_empty()));

        // Constant fallible conjunct (`1 / 0 = 1`) must also stay above Join.
        let const_div = bin(
            Expr::Binary {
                left: Box::new(Expr::Literal(ScalarValue::I64(1))),
                op: BinaryOp::Div,
                right: Box::new(Expr::Literal(ScalarValue::I64(0))),
                data_type: DataType::I64,
            },
            BinaryOp::Eq,
            Expr::Literal(ScalarValue::I64(1)),
        );
        let opt_const = optimize(LogicalPlan::Filter {
            input: Box::new(make_join()),
            predicate: const_div,
        })
        .unwrap();
        assert!(matches!(opt_const, LogicalPlan::Filter { .. }));
    }

    #[test]
    fn pass4_cnf_substrings_and_trailing_escape_and_pass5_map_projections() {
        // 4-column OR ILIKE -> single 4-element CNF clause
        let or4 = bind_and_opt(
            "SELECT name FROM spans WHERE name ILIKE '%abc%' OR status ILIKE '%abc%' OR status_message ILIKE '%abc%' OR model ILIKE '%abc%'",
        );
        let LogicalPlan::Scan { substrings, .. } = find_first_scan(&or4).unwrap() else {
            unreachable!()
        };
        assert_eq!(substrings.len(), 1);
        assert_eq!(substrings[0].len(), 4);
        assert!(
            substrings[0]
                .iter()
                .all(|h| h.needle == "abc" && h.case_insensitive)
        );

        // Mixed OR (substring OR equality) -> no substring clause emitted
        let mixed_or =
            bind_and_opt("SELECT name FROM spans WHERE name ILIKE '%abc%' OR status = 'error'");
        let LogicalPlan::Scan { substrings, .. } = find_first_scan(&mixed_or).unwrap() else {
            unreachable!()
        };
        assert!(substrings.is_empty());

        // Trailing lone escape char in longest_like_literal matches fn_string.rs
        assert_eq!(
            longest_like_literal(r"abc\", Some('\\')),
            Some(r"abc\".to_owned())
        );

        // Pass 5 MapProjection cases
        let cases = [
            (
                "SELECT metadata, metadata['k'] FROM spans",
                Some(MapProjection::Full),
            ),
            (
                "SELECT map_keys(metadata) FROM spans",
                Some(MapProjection::KeysOnly),
            ),
            (
                "SELECT map_keys(metadata) FROM spans WHERE metadata['env'] = 'prod'",
                Some(MapProjection::Full),
            ),
            (
                "SELECT map_values(metadata) FROM spans",
                Some(MapProjection::Full),
            ),
            (
                "SELECT count(*) FROM spans GROUP BY metadata",
                Some(MapProjection::Full),
            ),
            (
                "SELECT DISTINCT metadata FROM spans",
                Some(MapProjection::Full),
            ),
        ];
        for (sql, expected) in cases {
            let plan = bind_and_opt(sql);
            let LogicalPlan::Scan {
                map_projections, ..
            } = find_first_scan(&plan).unwrap()
            else {
                unreachable!()
            };
            assert_eq!(map_projections.get("metadata"), expected.as_ref(), "{sql}");
        }
    }

    #[test]
    fn iterative_walkers_handle_512_deep_binary_on_2mib_stack() {
        let handle = std::thread::Builder::new()
            .stack_size(2 * 1024 * 1024)
            .spawn(|| {
                let mut add_expr = Expr::Literal(ScalarValue::I64(1));
                let mut and_expr = Expr::Column {
                    index: 0,
                    name: "b".to_owned(),
                    data_type: DataType::Bool,
                };
                let mut or_expr = Expr::Column {
                    index: 0,
                    name: "b".to_owned(),
                    data_type: DataType::Bool,
                };
                for _ in 0..512 {
                    add_expr = bin(add_expr, BinaryOp::Add, Expr::Literal(ScalarValue::I64(1)));
                    and_expr = bin(
                        and_expr,
                        BinaryOp::And,
                        Expr::Column {
                            index: 0,
                            name: "b".to_owned(),
                            data_type: DataType::Bool,
                        },
                    );
                    or_expr = bin(
                        or_expr,
                        BinaryOp::Or,
                        Expr::Column {
                            index: 0,
                            name: "b".to_owned(),
                            data_type: DataType::Bool,
                        },
                    );
                }

                let mut folder = ConstFolder::new().unwrap();
                assert_eq!(
                    folder.fold_expr(add_expr),
                    Expr::Literal(ScalarValue::I64(513))
                );
                assert!(is_infallible(&and_expr));
                assert!(is_infallible(&or_expr));

                let sub = substitute_expr(
                    &and_expr,
                    &[Expr::Column {
                        index: 0,
                        name: "c".to_owned(),
                        data_type: DataType::Bool,
                    }],
                )
                .unwrap()
                .unwrap();
                let conjuncts = split_conjuncts(sub);
                assert_eq!(conjuncts.len(), 513);

                let mut disjuncts = Vec::new();
                collect_or_disjuncts(&or_expr, &mut disjuncts);
                assert_eq!(disjuncts.len(), 513);

                let mapped = map_expr_bottom_up(or_expr, &mut |e| e);
                assert!(is_infallible(&mapped));

                // Alternating 512-op AND/OR spine in walk_time_bounds (H3b-m1)
                let ts_schema = Schema::try_new_arc(vec![Field::new(
                    "start_time",
                    DataType::timestamp_utc(),
                    false,
                )])
                .unwrap();
                let ts_col = Expr::Column {
                    index: 0,
                    name: "start_time".to_owned(),
                    data_type: DataType::timestamp_utc(),
                };
                let ts_lit = Expr::Literal(ScalarValue::TimestampNs {
                    value: 100,
                    utc: true,
                });
                let mut alt_expr = bin(ts_col.clone(), BinaryOp::GtEq, ts_lit.clone());
                for i in 0..512 {
                    let op = if i % 2 == 0 {
                        BinaryOp::Or
                    } else {
                        BinaryOp::And
                    };
                    let leaf = bin(ts_col.clone(), BinaryOp::GtEq, ts_lit.clone());
                    alt_expr = bin(alt_expr, op, leaf);
                }
                let hint = extract_time_range(&[alt_expr], &ts_schema, "start_time");
                assert_eq!(hint.min_ns, Some(100));
            })
            .unwrap();
        handle.join().unwrap();
    }

    #[test]
    fn fallible_conjunct_reordering_prevented_in_project_and_scan() {
        // Traces: `status = 'ok' AND 1 / (CAST(start_time AS BIGINT) - 1000) > 0`
        // keeps the fallible conjunct above Scan (both because `status` stays above Scan and
        // because `is_scan_filter_safe` requires `is_infallible` on Traces/Scores).
        let traces_plan = bind_and_opt(
            "SELECT trace_id FROM traces WHERE status = 'ok' AND 1 / (CAST(start_time AS BIGINT) - 1000) > 0",
        );
        let LogicalPlan::Scan { filters, .. } = find_first_scan(&traces_plan).unwrap() else {
            unreachable!()
        };
        assert!(
            filters.is_empty(),
            "expected fallible conjunct to stay above Traces Scan, got {filters:?}"
        );

        // Project: `SELECT * FROM (SELECT a.x, 100 / a.y AS bad FROM a) t WHERE bad > 0 AND 100 / t.x > 10`
        // keeps `100 / t.x > 10` above Project because `bad > 0` stayed above Project.
        let a_schema = Schema::try_new_arc(vec![
            Field::new("x", DataType::I64, false),
            Field::new("y", DataType::I64, false),
        ])
        .unwrap();
        let t_schema = Schema::try_new_arc(vec![
            Field::new("x", DataType::I64, false),
            Field::new("bad", DataType::I64, false),
        ])
        .unwrap();
        let bad_proj = Expr::Binary {
            left: Box::new(Expr::Literal(ScalarValue::I64(100))),
            op: BinaryOp::Div,
            right: Box::new(col_expr(&a_schema, "y")),
            data_type: DataType::I64,
        };
        let proj = LogicalPlan::Project {
            input: Box::new(raw_scan("a", SqlViewKind::Batch, Arc::clone(&a_schema))),
            exprs: vec![col_expr(&a_schema, "x"), bad_proj],
            schema: Arc::clone(&t_schema),
        };
        let pred = bin(
            bin(
                col_expr(&t_schema, "bad"),
                BinaryOp::Gt,
                Expr::Literal(ScalarValue::I64(0)),
            ),
            BinaryOp::And,
            bin(
                Expr::Binary {
                    left: Box::new(Expr::Literal(ScalarValue::I64(100))),
                    op: BinaryOp::Div,
                    right: Box::new(col_expr(&t_schema, "x")),
                    data_type: DataType::I64,
                },
                BinaryOp::Gt,
                Expr::Literal(ScalarValue::I64(10)),
            ),
        );
        let opt = optimize(LogicalPlan::Filter {
            input: Box::new(proj),
            predicate: pred,
        })
        .unwrap();
        let LogicalPlan::Filter {
            input, predicate, ..
        } = opt
        else {
            panic!("expected Filter above Project, got {opt:?}");
        };
        assert_eq!(split_conjuncts(predicate).len(), 2);
        let LogicalPlan::Project { input, .. } = *input else {
            panic!("expected Project below Filter");
        };
        assert!(matches!(*input, LogicalPlan::Scan { filters, .. } if filters.is_empty()));
    }

    #[test]
    fn const_folder_512_failing_chain_is_linear() {
        let mut folder = ConstFolder::new().unwrap();
        let big_a = Expr::Literal(ScalarValue::Utf8("a".repeat(40 * 1024)));
        let big_b = Expr::Literal(ScalarValue::Utf8("b".repeat(40 * 1024)));
        let mut chain = bin(big_a, BinaryOp::Concat, big_b);
        for _ in 0..512 {
            chain = bin(
                chain,
                BinaryOp::Concat,
                Expr::Literal(ScalarValue::Utf8("x".to_owned())),
            );
        }
        let start = std::time::Instant::now();
        let folded = folder.fold_expr(chain);
        let elapsed = start.elapsed();
        assert!(matches!(
            folded,
            Expr::Binary {
                op: BinaryOp::Concat,
                ..
            }
        ));
        assert!(
            elapsed < std::time::Duration::from_millis(50),
            "expected O(N) fold of failing chain in < 50ms, took {elapsed:?}"
        );
    }

    #[test]
    fn self_join_map_projections_keyed_by_column_index() {
        let plan = bind_and_opt(
            "SELECT a.trace_id, b.metadata['region'] FROM spans a LEFT JOIN spans b ON a.trace_id = b.trace_id AND a.metadata['env'] = 'prod'",
        );
        let mut scans = Vec::new();
        let mut stack = vec![&plan];
        while let Some(node) = stack.pop() {
            match node {
                LogicalPlan::Scan {
                    map_projections, ..
                } => scans.push(map_projections.clone()),
                LogicalPlan::Filter { input, .. }
                | LogicalPlan::Project { input, .. }
                | LogicalPlan::Aggregate { input, .. }
                | LogicalPlan::Window { input, .. }
                | LogicalPlan::Sort { input, .. }
                | LogicalPlan::Limit { input, .. }
                | LogicalPlan::TopK { input, .. }
                | LogicalPlan::Distinct { input }
                | LogicalPlan::Unnest { input, .. }
                | LogicalPlan::Explain { inner: input, .. } => stack.push(input),
                LogicalPlan::Join { left, right, .. } => {
                    stack.push(right);
                    stack.push(left);
                }
                LogicalPlan::Union { inputs, .. } => {
                    for b in inputs {
                        stack.push(b);
                    }
                }
                LogicalPlan::Values { .. } | LogicalPlan::EmptyRow { .. } => {}
            }
        }
        assert_eq!(scans.len(), 2);
        let left_keys: BTreeSet<String> = ["env".to_owned()].into_iter().collect();
        let right_keys: BTreeSet<String> = ["region".to_owned()].into_iter().collect();
        assert_eq!(
            scans[0].get("metadata"),
            Some(&MapProjection::Keys(left_keys))
        );
        assert_eq!(
            scans[1].get("metadata"),
            Some(&MapProjection::Keys(right_keys))
        );
    }

    #[test]
    fn or_hex_id_pushdown_and_id_lookup_hint_merging() {
        let hex1 = "0102030405060708090a0b0c0d0e0f10";
        let hex2 = "1112131415161718191a1b1c1d1e1f20";
        let b1 = [
            0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e,
            0x0f, 0x10,
        ];
        let b2 = [
            0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b, 0x1c, 0x1d, 0x1e,
            0x1f, 0x20,
        ];
        let span1 = "0102030405060708";
        let span2 = "1112131415161718";
        let s1 = [0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08];
        let s2 = [0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18];

        let find_scan_lookups = |plan: &LogicalPlan| -> Vec<IdLookupHint> {
            let mut stack = vec![plan];
            while let Some(node) = stack.pop() {
                match node {
                    LogicalPlan::Scan { id_lookups, .. } => return id_lookups.clone(),
                    LogicalPlan::Filter { input, .. }
                    | LogicalPlan::Project { input, .. }
                    | LogicalPlan::Aggregate { input, .. }
                    | LogicalPlan::Window { input, .. }
                    | LogicalPlan::Sort { input, .. }
                    | LogicalPlan::Limit { input, .. }
                    | LogicalPlan::TopK { input, .. }
                    | LogicalPlan::Distinct { input }
                    | LogicalPlan::Unnest { input, .. }
                    | LogicalPlan::Explain { inner: input, .. } => stack.push(input),
                    LogicalPlan::Join { left, right, .. } => {
                        stack.push(right);
                        stack.push(left);
                    }
                    LogicalPlan::Union { inputs, .. } => {
                        for b in inputs {
                            stack.push(b);
                        }
                    }
                    LogicalPlan::Values { .. } | LogicalPlan::EmptyRow { .. } => {}
                }
            }
            Vec::new()
        };

        // 1. trace_id OR pushes through view Project and merges into one Fixed16 IdLookupHint.
        let plan_or = bind_and_opt(&format!(
            "SELECT name FROM spans WHERE trace_id = '{hex1}' OR trace_id = '{hex2}'"
        ));
        assert_eq!(
            find_scan_lookups(&plan_or),
            vec![IdLookupHint::Fixed16 {
                column: "trace_id".to_owned(),
                values: vec![b1, b2],
            }]
        );

        // 2. Deduplicates values across Eq OR InList.
        let plan_dedup = bind_and_opt(&format!(
            "SELECT name FROM spans WHERE trace_id = '{hex1}' OR trace_id IN ('{hex1}', '{hex2}')"
        ));
        assert_eq!(
            find_scan_lookups(&plan_dedup),
            vec![IdLookupHint::Fixed16 {
                column: "trace_id".to_owned(),
                values: vec![b1, b2],
            }]
        );

        // 3. span_id Fixed8 OR merges into one Fixed8 IdLookupHint.
        let plan_span = bind_and_opt(&format!(
            "SELECT name FROM spans WHERE span_id = '{span1}' OR span_id = '{span2}'"
        ));
        assert_eq!(
            find_scan_lookups(&plan_span),
            vec![IdLookupHint::Fixed8 {
                column: "span_id".to_owned(),
                values: vec![s1, s2],
            }]
        );

        // 4. Utf8 ID column OR merges into one Utf8 IdLookupHint.
        let plan_user =
            bind_and_opt("SELECT name FROM spans WHERE user_id = 'alice' OR user_id = 'bob'");
        assert_eq!(
            find_scan_lookups(&plan_user),
            vec![IdLookupHint::Utf8 {
                column: "user_id".to_owned(),
                values: vec!["alice".to_owned(), "bob".to_owned()],
            }]
        );

        // 5. Negative cases: mismatched columns, non-ID disjuncts, or negations emit no hint.
        for sql in [
            format!("SELECT name FROM spans WHERE trace_id = '{hex1}' OR span_id = '{span1}'"),
            format!("SELECT name FROM spans WHERE trace_id = '{hex1}' OR name = 'foo'"),
            format!("SELECT name FROM spans WHERE trace_id = '{hex1}' OR trace_id != '{hex2}'"),
            format!(
                "SELECT name FROM spans WHERE trace_id = '{hex1}' OR NOT (trace_id = '{hex2}')"
            ),
            format!(
                "SELECT name FROM spans WHERE trace_id = '{hex1}' OR trace_id NOT IN ('{hex2}')"
            ),
            format!("SELECT name FROM spans WHERE trace_id = '{hex1}' OR trace_id = 'not-hex'"),
        ] {
            let plan = bind_and_opt(&sql);
            assert!(
                find_scan_lookups(&plan).is_empty(),
                "expected no IdLookupHint for `{sql}`, got {:?}",
                find_scan_lookups(&plan)
            );
        }

        // 6. Redacted InList in OR must never be extracted into IdLookupHint.
        let raw_spans = raw_spans_schema();
        let uid_idx = raw_spans.index_of("user_id").unwrap();
        let uid_col = Expr::Column {
            index: uid_idx,
            name: "user_id".to_owned(),
            data_type: DataType::Utf8,
        };
        let redacted_or = Expr::Binary {
            left: Box::new(Expr::Binary {
                left: Box::new(uid_col.clone()),
                op: BinaryOp::Eq,
                right: Box::new(Expr::Literal(ScalarValue::Utf8("alice".to_owned()))),
                data_type: DataType::Bool,
            }),
            op: BinaryOp::Or,
            right: Box::new(Expr::InList {
                expr: Box::new(uid_col),
                list: vec![Expr::Literal(ScalarValue::Utf8("secret".to_owned()))],
                negated: false,
                redacted: true,
            }),
            data_type: DataType::Bool,
        };
        let mut hints = Vec::new();
        extract_id_lookups_from_expr(&redacted_or, &raw_spans, &mut hints);
        assert!(hints.is_empty(), "redacted OR leaked hint: {hints:?}");

        // 7. `is_hex_encoded_column` rejects non-column `encode(expr, 'hex')`.
        let non_col_encode = Expr::Call {
            func: ScalarFn::Encode,
            args: vec![
                Expr::Literal(ScalarValue::FixedBytes16(b1)),
                Expr::Literal(ScalarValue::Utf8("hex".to_owned())),
            ],
            data_type: DataType::Utf8,
        };
        assert!(!is_hex_encoded_column(&non_col_encode));
    }
}
