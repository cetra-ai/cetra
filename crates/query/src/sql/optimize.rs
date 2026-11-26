//! Rule-based optimizer for Shape B SQL [`LogicalPlan`]s.
//!
//! Executes six deterministic passes in fixed order:
//! 1. Constant folding and boolean simplification (preserving division-by-zero/overflow errors).
//! 2. Filter pushdown through `Project` and safe `Join` sides (before hex-ID rewrite).
//! 3. Hex-literal ID rewrite (`encode(id, 'hex') =/<>/IN` -> `FixedBytes8`/`FixedBytes16`, D89).
//! 4. Storage-safe `Scan` filter pushdown (`Traces`: `start_time`/`trace_id`, `Scores`: `created_at`)
//!    and pruning hint extraction (`time_range`, `id_lookups`, `substrings`), retaining full
//!    predicates in `Scan.filters`.
//! 5. Top-down projection pruning and `MapIndex` key pushdown into `Scan.map_keys`.
//! 6. Bounded `ORDER BY ... LIMIT k [OFFSET n]` -> `TopK` rewrite when `offset + limit <= 65_536`.

use std::collections::BTreeSet;
use std::sync::Arc;

use cetra_column::{Batch, CancelToken, DataType, ScalarValue, Schema};

use crate::error::QueryError;
use crate::eval::eval_expr;
use crate::exec::hash_agg::{AggExpr, AggFunc};
use crate::exec::sort_key::SortKey;
use crate::exec::sort_window::{WindowFunc, WindowSpec};
use crate::exec::source::{IdLookupHint, SubstringHint, TimeRangeHint};
use crate::ir::{BinaryOp, CastMode, EvalContext, Expr, ScalarFn, UnaryOp};

use super::catalog::SqlViewKind;
use super::logical::{LogicalJoinKind, LogicalPlan};

/// Maximum `offset + limit` rewritten to unspillable in-memory `TopK` (above this, keep spillable `ExternalSort`).
pub const TOPK_MAX_ROWS: usize = 65_536;

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

struct ConstFolder {
    batch: Batch,
    eval_ctx: EvalContext,
}

impl ConstFolder {
    fn new() -> Result<Self, QueryError> {
        let empty = Schema::try_new_arc(Vec::new())?;
        let batch = Batch::try_new_all(empty, Vec::new(), 1)?;
        let eval_ctx = EvalContext {
            now_ns: 0,
            cancel: CancelToken::new(),
        };
        Ok(Self { batch, eval_ctx })
    }

    fn fold_plan(&self, plan: LogicalPlan) -> Result<LogicalPlan, QueryError> {
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
                map_keys,
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
                map_keys,
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

    fn fold_sort_key(&self, mut key: SortKey) -> SortKey {
        key.expr = self.fold_expr(key.expr);
        key
    }

    fn fold_agg(&self, agg: AggExpr) -> AggExpr {
        AggExpr {
            func: map_agg_func_exprs(agg.func, &mut |e| self.fold_expr(e)),
            filter: agg.filter.map(|f| self.fold_expr(f)),
            output_name: agg.output_name,
        }
    }

    fn fold_window(&self, spec: WindowSpec) -> WindowSpec {
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

    fn fold_expr(&self, expr: Expr) -> Expr {
        let folded = match expr {
            Expr::Column { .. } | Expr::Literal(_) => return expr,
            Expr::Unary {
                op,
                expr: inner,
                data_type,
            } => {
                let inner = self.fold_expr(*inner);
                if op == UnaryOp::Not
                    && let Expr::Unary {
                        op: UnaryOp::Not,
                        expr: grand,
                        ..
                    } = inner
                {
                    return *grand;
                }
                Expr::Unary {
                    op,
                    expr: Box::new(inner),
                    data_type,
                }
            }
            Expr::Binary {
                left,
                op,
                right,
                data_type,
            } => {
                let left = self.fold_expr(*left);
                let right = self.fold_expr(*right);
                match op {
                    BinaryOp::And => {
                        if is_bool_lit(&left, true) {
                            return right;
                        }
                        if is_bool_lit(&right, true) {
                            return left;
                        }
                        if is_bool_lit(&left, false) {
                            return Expr::Literal(ScalarValue::Bool(false));
                        }
                        if is_bool_lit(&right, false) && is_infallible(&left) {
                            return Expr::Literal(ScalarValue::Bool(false));
                        }
                    }
                    BinaryOp::Or => {
                        if is_bool_lit(&left, false) {
                            return right;
                        }
                        if is_bool_lit(&right, false) {
                            return left;
                        }
                        if is_bool_lit(&left, true) {
                            return Expr::Literal(ScalarValue::Bool(true));
                        }
                        if is_bool_lit(&right, true) && is_infallible(&left) {
                            return Expr::Literal(ScalarValue::Bool(true));
                        }
                    }
                    _ => {}
                }
                Expr::Binary {
                    left: Box::new(left),
                    op,
                    right: Box::new(right),
                    data_type,
                }
            }
            Expr::Cast {
                expr: inner,
                target,
                mode,
            } => Expr::Cast {
                expr: Box::new(self.fold_expr(*inner)),
                target,
                mode,
            },
            Expr::Between {
                expr: inner,
                low,
                high,
                negated,
            } => Expr::Between {
                expr: Box::new(self.fold_expr(*inner)),
                low: Box::new(self.fold_expr(*low)),
                high: Box::new(self.fold_expr(*high)),
                negated,
            },
            Expr::InList {
                expr: inner,
                list,
                negated,
                redacted,
            } => {
                let inner = self.fold_expr(*inner);
                let list = if redacted {
                    list
                } else {
                    list.into_iter().map(|e| self.fold_expr(e)).collect()
                };
                Expr::InList {
                    expr: Box::new(inner),
                    list,
                    negated,
                    redacted,
                }
            }
            Expr::Like {
                expr: inner,
                pattern,
                escape,
                case_insensitive,
                negated,
            } => Expr::Like {
                expr: Box::new(self.fold_expr(*inner)),
                pattern: Box::new(self.fold_expr(*pattern)),
                escape,
                case_insensitive,
                negated,
            },
            Expr::Case {
                operand,
                when_then,
                else_expr,
                data_type,
            } => Expr::Case {
                operand: operand.map(|o| Box::new(self.fold_expr(*o))),
                when_then: when_then
                    .into_iter()
                    .map(|(w, t)| (self.fold_expr(w), self.fold_expr(t)))
                    .collect(),
                else_expr: else_expr.map(|e| Box::new(self.fold_expr(*e))),
                data_type,
            },
            Expr::MapIndex { map, key } => Expr::MapIndex {
                map: Box::new(self.fold_expr(*map)),
                key: Box::new(self.fold_expr(*key)),
            },
            Expr::MakeArray { elems, item_type } => Expr::MakeArray {
                elems: elems.into_iter().map(|e| self.fold_expr(e)).collect(),
                item_type,
            },
            Expr::Call {
                func,
                args,
                data_type,
            } => Expr::Call {
                func,
                args: args.into_iter().map(|a| self.fold_expr(a)).collect(),
                data_type,
            },
        };

        if can_const_fold(&folded) {
            let orig_dt = folded.data_type();
            if let Ok(col) = eval_expr(&folded, &self.batch, &self.eval_ctx, None)
                && let Ok(sv) = col.get_scalar(0)
            {
                let sv = if sv.is_null() && sv.data_type() != orig_dt {
                    ScalarValue::Null(orig_dt)
                } else {
                    sv
                };
                return Expr::Literal(sv);
            }
        }
        folded
    }
}

fn pass1_constant_fold(plan: LogicalPlan) -> Result<LogicalPlan, QueryError> {
    let folder = ConstFolder::new()?;
    folder.fold_plan(plan)
}

fn is_bool_lit(expr: &Expr, expected: bool) -> bool {
    matches!(expr, Expr::Literal(ScalarValue::Bool(b)) if *b == expected)
}

fn is_null_lit(expr: &Expr) -> bool {
    matches!(expr, Expr::Literal(ScalarValue::Null(_)))
}

fn can_const_fold(expr: &Expr) -> bool {
    let mut cols = BTreeSet::new();
    expr.referenced_columns(&mut cols);
    cols.is_empty() && !contains_now(expr) && !contains_redacted_in_list(expr)
}

fn contains_now(expr: &Expr) -> bool {
    match expr {
        Expr::Column { .. } | Expr::Literal(_) => false,
        Expr::Unary { expr, .. } | Expr::Cast { expr, .. } => contains_now(expr),
        Expr::Binary { left, right, .. } => contains_now(left) || contains_now(right),
        Expr::Between {
            expr, low, high, ..
        } => contains_now(expr) || contains_now(low) || contains_now(high),
        Expr::InList { expr, list, .. } => contains_now(expr) || list.iter().any(contains_now),
        Expr::Like { expr, pattern, .. } => contains_now(expr) || contains_now(pattern),
        Expr::Case {
            operand,
            when_then,
            else_expr,
            ..
        } => {
            operand.as_deref().is_some_and(contains_now)
                || when_then
                    .iter()
                    .any(|(w, t)| contains_now(w) || contains_now(t))
                || else_expr.as_deref().is_some_and(contains_now)
        }
        Expr::MapIndex { map, key } => contains_now(map) || contains_now(key),
        Expr::MakeArray { elems, .. } => elems.iter().any(contains_now),
        Expr::Call { func, args, .. } => *func == ScalarFn::Now || args.iter().any(contains_now),
    }
}

fn contains_redacted_in_list(expr: &Expr) -> bool {
    match expr {
        Expr::Column { .. } | Expr::Literal(_) => false,
        Expr::Unary { expr, .. } | Expr::Cast { expr, .. } => contains_redacted_in_list(expr),
        Expr::Binary { left, right, .. } => {
            contains_redacted_in_list(left) || contains_redacted_in_list(right)
        }
        Expr::Between {
            expr, low, high, ..
        } => {
            contains_redacted_in_list(expr)
                || contains_redacted_in_list(low)
                || contains_redacted_in_list(high)
        }
        Expr::InList {
            expr,
            list,
            redacted,
            ..
        } => {
            *redacted
                || contains_redacted_in_list(expr)
                || list.iter().any(contains_redacted_in_list)
        }
        Expr::Like { expr, pattern, .. } => {
            contains_redacted_in_list(expr) || contains_redacted_in_list(pattern)
        }
        Expr::Case {
            operand,
            when_then,
            else_expr,
            ..
        } => {
            operand.as_deref().is_some_and(contains_redacted_in_list)
                || when_then
                    .iter()
                    .any(|(w, t)| contains_redacted_in_list(w) || contains_redacted_in_list(t))
                || else_expr.as_deref().is_some_and(contains_redacted_in_list)
        }
        Expr::MapIndex { map, key } => {
            contains_redacted_in_list(map) || contains_redacted_in_list(key)
        }
        Expr::MakeArray { elems, .. } => elems.iter().any(contains_redacted_in_list),
        Expr::Call { args, .. } => args.iter().any(contains_redacted_in_list),
    }
}

/// Conservative infallibility check so boolean identities never hide division-by-zero,
/// integer overflow, strict cast errors, or regex/JSONPath errors.
fn is_infallible(expr: &Expr) -> bool {
    match expr {
        Expr::Column { .. } | Expr::Literal(_) => true,
        Expr::Unary { op, expr, .. } => !matches!(op, UnaryOp::Neg) && is_infallible(expr),
        Expr::Binary {
            left, op, right, ..
        } => {
            let safe_op = matches!(
                op,
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
                    | BinaryOp::Concat
                    | BinaryOp::ArrayContains
            );
            safe_op && is_infallible(left) && is_infallible(right)
        }
        Expr::Cast { mode, expr, .. } => *mode == CastMode::Try && is_infallible(expr),
        Expr::Between {
            expr, low, high, ..
        } => is_infallible(expr) && is_infallible(low) && is_infallible(high),
        Expr::InList { expr, list, .. } => is_infallible(expr) && list.iter().all(is_infallible),
        Expr::Like { expr, pattern, .. } => is_infallible(expr) && is_infallible(pattern),
        Expr::Case {
            operand,
            when_then,
            else_expr,
            ..
        } => {
            operand.as_deref().is_none_or(is_infallible)
                && when_then
                    .iter()
                    .all(|(w, t)| is_infallible(w) && is_infallible(t))
                && else_expr.as_deref().is_none_or(is_infallible)
        }
        Expr::MapIndex { map, key } => is_infallible(map) && is_infallible(key),
        Expr::MakeArray { elems, .. } => elems.iter().all(is_infallible),
        Expr::Call { func, args, .. } => {
            let safe_fn = matches!(
                func,
                ScalarFn::Lower
                    | ScalarFn::Upper
                    | ScalarFn::Length
                    | ScalarFn::Trim
                    | ScalarFn::Ltrim
                    | ScalarFn::Rtrim
                    | ScalarFn::Concat
                    | ScalarFn::ConcatWs
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
                    | ScalarFn::ArrayDistinct
                    | ScalarFn::MapKeys
                    | ScalarFn::MapValues
            );
            safe_fn && args.iter().all(is_infallible)
        }
    }
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
            let mut substituted = Vec::with_capacity(conjuncts.len());
            for conj in conjuncts {
                substituted.push(substitute_expr(conj, &exprs)?);
            }
            let pushed_input = push_filter(substituted, *input)?;
            Ok(LogicalPlan::Project {
                input: Box::new(pushed_input),
                exprs,
                schema,
            })
        }
        LogicalPlan::Sort { input, keys } => {
            let pushed_input = push_filter(conjuncts, *input)?;
            Ok(LogicalPlan::Sort {
                input: Box::new(pushed_input),
                keys,
            })
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

fn substitute_expr(expr: Expr, project_exprs: &[Expr]) -> Result<Expr, QueryError> {
    match expr {
        Expr::Column { index, .. } => project_exprs.get(index).cloned().ok_or_else(|| {
            QueryError::internal(format!(
                "filter column index {index} out of bounds for projection ({} cols)",
                project_exprs.len()
            ))
        }),
        Expr::Literal(v) => Ok(Expr::Literal(v)),
        Expr::Unary {
            op,
            expr: inner,
            data_type,
        } => Ok(Expr::Unary {
            op,
            expr: Box::new(substitute_expr(*inner, project_exprs)?),
            data_type,
        }),
        Expr::Binary {
            left,
            op,
            right,
            data_type,
        } => Ok(Expr::Binary {
            left: Box::new(substitute_expr(*left, project_exprs)?),
            op,
            right: Box::new(substitute_expr(*right, project_exprs)?),
            data_type,
        }),
        Expr::Cast {
            expr: inner,
            target,
            mode,
        } => Ok(Expr::Cast {
            expr: Box::new(substitute_expr(*inner, project_exprs)?),
            target,
            mode,
        }),
        Expr::Between {
            expr: inner,
            low,
            high,
            negated,
        } => Ok(Expr::Between {
            expr: Box::new(substitute_expr(*inner, project_exprs)?),
            low: Box::new(substitute_expr(*low, project_exprs)?),
            high: Box::new(substitute_expr(*high, project_exprs)?),
            negated,
        }),
        Expr::InList {
            expr: inner,
            list,
            negated,
            redacted,
        } => {
            let mut new_list = Vec::with_capacity(list.len());
            for item in list {
                new_list.push(substitute_expr(item, project_exprs)?);
            }
            Ok(Expr::InList {
                expr: Box::new(substitute_expr(*inner, project_exprs)?),
                list: new_list,
                negated,
                redacted,
            })
        }
        Expr::Like {
            expr: inner,
            pattern,
            escape,
            case_insensitive,
            negated,
        } => Ok(Expr::Like {
            expr: Box::new(substitute_expr(*inner, project_exprs)?),
            pattern: Box::new(substitute_expr(*pattern, project_exprs)?),
            escape,
            case_insensitive,
            negated,
        }),
        Expr::Case {
            operand,
            when_then,
            else_expr,
            data_type,
        } => {
            let operand = match operand {
                Some(op) => Some(Box::new(substitute_expr(*op, project_exprs)?)),
                None => None,
            };
            let mut wt = Vec::with_capacity(when_then.len());
            for (w, t) in when_then {
                wt.push((
                    substitute_expr(w, project_exprs)?,
                    substitute_expr(t, project_exprs)?,
                ));
            }
            let else_expr = match else_expr {
                Some(el) => Some(Box::new(substitute_expr(*el, project_exprs)?)),
                None => None,
            };
            Ok(Expr::Case {
                operand,
                when_then: wt,
                else_expr,
                data_type,
            })
        }
        Expr::MapIndex { map, key } => Ok(Expr::MapIndex {
            map: Box::new(substitute_expr(*map, project_exprs)?),
            key: Box::new(substitute_expr(*key, project_exprs)?),
        }),
        Expr::MakeArray { elems, item_type } => {
            let mut out = Vec::with_capacity(elems.len());
            for e in elems {
                out.push(substitute_expr(e, project_exprs)?);
            }
            Ok(Expr::MakeArray {
                elems: out,
                item_type,
            })
        }
        Expr::Call {
            func,
            args,
            data_type,
        } => {
            let mut out = Vec::with_capacity(args.len());
            for a in args {
                out.push(substitute_expr(a, project_exprs)?);
            }
            Ok(Expr::Call {
                func,
                args: out,
                data_type,
            })
        }
    }
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
    match expr {
        Expr::Binary {
            left,
            op: BinaryOp::And,
            right,
            ..
        } => {
            split_conjuncts_into(*left, out);
            split_conjuncts_into(*right, out);
        }
        other => out.push(other),
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
    if text.len() != expected || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
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
                map_keys,
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
                    if is_bool_lit(&full_conj, false) {
                        limit = Some(0);
                    }
                    if is_scan_filter_safe(kind, &full_conj, &full_schema) {
                        filters.push(full_conj);
                    } else {
                        keep_above.push(conj);
                    }
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
                    map_keys,
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
            map_keys,
            mut limit,
            schema,
        } => {
            if filters.iter().any(|f| is_bool_lit(f, false)) {
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
                map_keys,
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
                && cols.iter().all(|&idx| {
                    full_schema
                        .fields()
                        .get(idx)
                        .is_some_and(|f| f.name == "created_at")
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
    substrings: &mut Vec<SubstringHint>,
) {
    let time_col = match kind {
        SqlViewKind::Spans | SqlViewKind::Traces => Some("start_time"),
        SqlViewKind::Scores => Some("created_at"),
        SqlViewKind::Batch | SqlViewKind::Sessions => None,
    };
    if let Some(col) = time_col {
        let extracted = extract_time_range(filters, col);
        *time_range = intersect_time_range(*time_range, extracted);
    }

    for filter in filters {
        extract_id_lookups_from_expr(filter, full_schema, id_lookups);
        extract_substrings_from_expr(filter, substrings);
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

fn extract_time_range(filters: &[Expr], column: &str) -> TimeRangeHint {
    let mut bounds = TimeBounds::default();
    for filter in filters {
        walk_time_bounds(&mut bounds, filter, column);
    }
    TimeRangeHint {
        min_ns: bounds.min,
        max_ns: bounds.max,
        around_ns: None,
    }
}

fn walk_time_bounds(bounds: &mut TimeBounds, expr: &Expr, column: &str) {
    match expr {
        Expr::Binary {
            left,
            op: BinaryOp::And,
            right,
            ..
        } => {
            walk_time_bounds(bounds, left, column);
            walk_time_bounds(bounds, right, column);
        }
        Expr::Binary {
            op: BinaryOp::Or, ..
        } => {
            let mut disjuncts = Vec::new();
            collect_or_disjuncts(expr, &mut disjuncts);
            let mut union: Option<TimeBounds> = None;
            for disjunct in disjuncts {
                if matches!(
                    disjunct,
                    Expr::Unary {
                        op: UnaryOp::IsNull,
                        expr: inner,
                        ..
                    } if matches!(inner.as_ref(), Expr::Column { name, .. } if name == column)
                ) {
                    continue;
                }
                let mut sep = TimeBounds::default();
                walk_time_bounds(&mut sep, disjunct, column);
                union = Some(match union {
                    None => sep,
                    Some(acc) => acc.union_with(sep),
                });
            }
            if let Some(u) = union {
                *bounds = bounds.intersect_with(u);
            }
        }
        Expr::Binary {
            left, op, right, ..
        } => match (left.as_ref(), right.as_ref()) {
            (Expr::Column { name, .. }, other) if name == column => {
                if let Some(ns) = expr_timestamp_ns(other) {
                    apply_time_cmp(bounds, *op, ns, true);
                }
            }
            (other, Expr::Column { name, .. }) if name == column => {
                if let Some(ns) = expr_timestamp_ns(other) {
                    apply_time_cmp(bounds, *op, ns, false);
                }
            }
            _ => {}
        },
        Expr::Between {
            expr: inner,
            low,
            high,
            negated: false,
        } => {
            if matches!(inner.as_ref(), Expr::Column { name, .. } if name == column) {
                if let Some(lo) = expr_timestamp_ns(low) {
                    bounds.raise_min(lo);
                }
                if let Some(hi) = expr_timestamp_ns(high) {
                    bounds.lower_max(hi);
                }
            }
        }
        _ => {}
    }
}

fn collect_or_disjuncts<'a>(expr: &'a Expr, out: &mut Vec<&'a Expr>) {
    match expr {
        Expr::Binary {
            left,
            op: BinaryOp::Or,
            right,
            ..
        } => {
            collect_or_disjuncts(left, out);
            collect_or_disjuncts(right, out);
        }
        other => out.push(other),
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

fn extract_id_lookups_from_expr(expr: &Expr, full_schema: &Schema, out: &mut Vec<IdLookupHint>) {
    match expr {
        Expr::Binary {
            left,
            op: BinaryOp::And,
            right,
            ..
        } => {
            extract_id_lookups_from_expr(left, full_schema, out);
            extract_id_lookups_from_expr(right, full_schema, out);
        }
        Expr::Binary {
            left,
            op: BinaryOp::Eq,
            right,
            ..
        } => {
            let (col_idx, lit) = match (left.as_ref(), right.as_ref()) {
                (Expr::Column { index, .. }, Expr::Literal(sv)) => (*index, sv),
                (Expr::Literal(sv), Expr::Column { index, .. }) => (*index, sv),
                _ => return,
            };
            let Some(field) = full_schema.fields().get(col_idx) else {
                return;
            };
            let hint = match lit {
                ScalarValue::FixedBytes16(b) if field.data_type == DataType::FixedBytes16 => {
                    IdLookupHint::Fixed16 {
                        column: field.name.clone(),
                        values: vec![*b],
                    }
                }
                ScalarValue::FixedBytes8(b) if field.data_type == DataType::FixedBytes8 => {
                    IdLookupHint::Fixed8 {
                        column: field.name.clone(),
                        values: vec![*b],
                    }
                }
                ScalarValue::Utf8(s) if is_utf8_id_column(&field.name) => IdLookupHint::Utf8 {
                    column: field.name.clone(),
                    values: vec![s.clone()],
                },
                _ => return,
            };
            push_unique_id_lookup(out, hint);
        }
        Expr::InList {
            expr: inner,
            list,
            negated: false,
            redacted: false,
        } => {
            let Expr::Column { index, .. } = inner.as_ref() else {
                return;
            };
            let Some(field) = full_schema.fields().get(*index) else {
                return;
            };
            let hint = match &field.data_type {
                DataType::FixedBytes16 => {
                    let mut vals = Vec::new();
                    for item in list {
                        let Expr::Literal(sv) = item else {
                            return;
                        };
                        match sv {
                            ScalarValue::Null(_) => {}
                            ScalarValue::FixedBytes16(b) => vals.push(*b),
                            _ => return,
                        }
                    }
                    IdLookupHint::Fixed16 {
                        column: field.name.clone(),
                        values: vals,
                    }
                }
                DataType::FixedBytes8 => {
                    let mut vals = Vec::new();
                    for item in list {
                        let Expr::Literal(sv) = item else {
                            return;
                        };
                        match sv {
                            ScalarValue::Null(_) => {}
                            ScalarValue::FixedBytes8(b) => vals.push(*b),
                            _ => return,
                        }
                    }
                    IdLookupHint::Fixed8 {
                        column: field.name.clone(),
                        values: vals,
                    }
                }
                DataType::Utf8 if is_utf8_id_column(&field.name) => {
                    let mut vals = Vec::new();
                    for item in list {
                        let Expr::Literal(sv) = item else {
                            return;
                        };
                        match sv {
                            ScalarValue::Null(_) => {}
                            ScalarValue::Utf8(s) => vals.push(s.clone()),
                            _ => return,
                        }
                    }
                    IdLookupHint::Utf8 {
                        column: field.name.clone(),
                        values: vals,
                    }
                }
                _ => return,
            };
            push_unique_id_lookup(out, hint);
        }
        _ => {}
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

fn extract_substrings_from_expr(expr: &Expr, out: &mut Vec<SubstringHint>) {
    match expr {
        Expr::Binary {
            left,
            op: BinaryOp::And,
            right,
            ..
        } => {
            extract_substrings_from_expr(left, out);
            extract_substrings_from_expr(right, out);
        }
        Expr::Like {
            expr: inner,
            pattern,
            escape,
            case_insensitive,
            negated: false,
        } => {
            let Expr::Column { name, .. } = inner.as_ref() else {
                return;
            };
            let Expr::Literal(ScalarValue::Utf8(pat)) = pattern.as_ref() else {
                return;
            };
            if let Some(needle) = longest_like_literal(pat, *escape) {
                let hint = SubstringHint {
                    column: name.clone(),
                    needle,
                    case_insensitive: *case_insensitive,
                };
                if !out.contains(&hint) {
                    out.push(hint);
                }
            }
        }
        Expr::Call {
            func: ScalarFn::Contains | ScalarFn::StartsWith | ScalarFn::EndsWith,
            args,
            ..
        } => {
            if let [
                Expr::Column { name, .. },
                Expr::Literal(ScalarValue::Utf8(needle)),
            ] = args.as_slice()
                && !needle.is_empty()
            {
                let hint = SubstringHint {
                    column: name.clone(),
                    needle: needle.clone(),
                    case_insensitive: false,
                };
                if !out.contains(&hint) {
                    out.push(hint);
                }
            }
        }
        _ => {}
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
// Pass 5: Top-down projection & MapIndex key pushdown
// ---------------------------------------------------------------------------

fn pass5_projection_and_map_keys(plan: LogicalPlan) -> Result<LogicalPlan, QueryError> {
    let n = plan.schema().len();
    let required: BTreeSet<usize> = (0..n).collect();
    let map_keys = vec![BTreeSet::new(); n];
    let (pruned, _) = prune_node(plan, &required, &map_keys)?;
    Ok(pruned)
}

fn prune_node(
    plan: LogicalPlan,
    required: &BTreeSet<usize>,
    required_map_keys: &[BTreeSet<String>],
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
            mut map_keys,
            limit,
            schema,
        } => {
            let out_len = schema.len();
            let kept_out: Vec<usize> = required.iter().copied().filter(|&i| i < out_len).collect();

            let mut collected_keys: BTreeSet<(String, String)> = map_keys.drain(..).collect();

            let mut filter_keys = vec![BTreeSet::new(); full_schema.len()];
            for f in &filters {
                collect_expr_map_keys(f, &mut filter_keys);
            }
            for (full_idx, keys) in filter_keys.into_iter().enumerate() {
                if let Some(field) = full_schema.fields().get(full_idx) {
                    for k in keys {
                        collected_keys.insert((field.name.clone(), k));
                    }
                }
            }

            for (out_idx, keys) in required_map_keys.iter().enumerate() {
                if keys.is_empty() || out_idx >= out_len {
                    continue;
                }
                let full_idx = projection.as_ref().map_or(out_idx, |p| p[out_idx]);
                if let Some(field) = full_schema.fields().get(full_idx) {
                    for k in keys {
                        collected_keys.insert((field.name.clone(), k.clone()));
                    }
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
                    map_keys: collected_keys.into_iter().collect(),
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
            let in_len = input.schema().len();
            let mut child_needed = required.clone();
            predicate.referenced_columns(&mut child_needed);

            let mut child_keys = vec![BTreeSet::new(); in_len];
            merge_map_keys(&mut child_keys, required_map_keys);
            collect_expr_map_keys(&predicate, &mut child_keys);

            let (new_input, child_map) = prune_node(*input, &child_needed, &child_keys)?;
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
            let in_len = input.schema().len();
            let kept_out: Vec<usize> = required.iter().copied().filter(|&i| i < out_len).collect();

            let mut child_needed = BTreeSet::new();
            let mut child_keys = vec![BTreeSet::new(); in_len];
            for &out_idx in &kept_out {
                let expr = &exprs[out_idx];
                expr.referenced_columns(&mut child_needed);
                collect_expr_map_keys(expr, &mut child_keys);
                if let Expr::Column { index, .. } = expr
                    && let Some(keys) = required_map_keys.get(out_idx)
                    && let Some(dst) = child_keys.get_mut(*index)
                {
                    dst.extend(keys.iter().cloned());
                }
            }

            let (new_input, child_map) = prune_node(*input, &child_needed, &child_keys)?;
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
            let in_len = input.schema().len();

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
            let mut child_keys = vec![BTreeSet::new(); in_len];
            for (g_idx, (_, g_expr)) in group_by.iter().enumerate() {
                g_expr.referenced_columns(&mut child_needed);
                collect_expr_map_keys(g_expr, &mut child_keys);
                if let Expr::Column { index, .. } = g_expr
                    && let Some(keys) = required_map_keys.get(g_idx)
                    && let Some(dst) = child_keys.get_mut(*index)
                {
                    dst.extend(keys.iter().cloned());
                }
            }
            for &agg_i in &kept_agg_indices {
                collect_agg_refs(&aggs[agg_i], &mut child_needed, &mut child_keys);
            }

            let (new_input, child_map) = prune_node(*input, &child_needed, &child_keys)?;
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
            let in_len = input.schema().len();
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
                let in_keys = required_map_keys[..in_len.min(required_map_keys.len())].to_vec();
                let (new_input, child_map) = prune_node(*input, &in_required, &in_keys)?;
                let mut old_to_new = vec![None; out_len];
                for (i, mapped) in child_map.into_iter().enumerate() {
                    old_to_new[i] = mapped;
                }
                return Ok((new_input, old_to_new));
            }

            let mut child_needed: BTreeSet<usize> =
                required.iter().copied().filter(|&i| i < in_len).collect();
            let mut child_keys = vec![BTreeSet::new(); in_len];
            merge_map_keys(
                &mut child_keys,
                &required_map_keys[..in_len.min(required_map_keys.len())],
            );

            for p in &partition_by {
                p.referenced_columns(&mut child_needed);
                collect_expr_map_keys(p, &mut child_keys);
            }
            for k in &order_by {
                k.expr.referenced_columns(&mut child_needed);
                collect_expr_map_keys(&k.expr, &mut child_keys);
            }
            for &s_idx in &kept_specs_idx {
                collect_window_refs(&specs[s_idx], &mut child_needed, &mut child_keys);
            }

            let (new_input, child_map) = prune_node(*input, &child_needed, &child_keys)?;
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
            let in_len = input.schema().len();
            let mut child_needed = required.clone();
            let mut child_keys = vec![BTreeSet::new(); in_len];
            merge_map_keys(&mut child_keys, required_map_keys);
            for k in &keys {
                k.expr.referenced_columns(&mut child_needed);
                collect_expr_map_keys(&k.expr, &mut child_keys);
            }
            let (new_input, child_map) = prune_node(*input, &child_needed, &child_keys)?;
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
            let in_len = input.schema().len();
            let mut child_needed = required.clone();
            let mut child_keys = vec![BTreeSet::new(); in_len];
            merge_map_keys(&mut child_keys, required_map_keys);
            for k in &keys {
                k.expr.referenced_columns(&mut child_needed);
                collect_expr_map_keys(&k.expr, &mut child_keys);
            }
            let (new_input, child_map) = prune_node(*input, &child_needed, &child_keys)?;
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
            let (new_input, child_map) = prune_node(*input, required, required_map_keys)?;
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
            let in_len = input.schema().len();
            let all_needed: BTreeSet<usize> = (0..in_len).collect();
            let mut child_keys = vec![BTreeSet::new(); in_len];
            merge_map_keys(&mut child_keys, required_map_keys);
            let (new_input, child_map) = prune_node(*input, &all_needed, &child_keys)?;
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
            let left_len = left.schema().len();
            let right_len = right.schema().len();
            let out_len = schema.len();

            let is_semi_anti = matches!(
                kind,
                LogicalJoinKind::Semi | LogicalJoinKind::Anti | LogicalJoinKind::AntiNullAware
            );

            let mut left_needed = BTreeSet::new();
            let mut right_needed = BTreeSet::new();
            let mut left_map_keys = vec![BTreeSet::new(); left_len];
            let mut right_map_keys = vec![BTreeSet::new(); right_len];

            for &idx in required {
                if idx < left_len {
                    left_needed.insert(idx);
                    if let Some(keys) = required_map_keys.get(idx) {
                        left_map_keys[idx].extend(keys.iter().cloned());
                    }
                } else if !is_semi_anti && idx < left_len + right_len {
                    let r_idx = idx - left_len;
                    right_needed.insert(r_idx);
                    if let Some(keys) = required_map_keys.get(idx) {
                        right_map_keys[r_idx].extend(keys.iter().cloned());
                    }
                }
            }

            for lk in &left_keys {
                lk.referenced_columns(&mut left_needed);
                collect_expr_map_keys(lk, &mut left_map_keys);
            }
            for rk in &right_keys {
                rk.referenced_columns(&mut right_needed);
                collect_expr_map_keys(rk, &mut right_map_keys);
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
                let mut combined_keys = vec![BTreeSet::new(); left_len + right_len];
                collect_expr_map_keys(res, &mut combined_keys);
                for (idx, keys) in combined_keys.into_iter().enumerate() {
                    if idx < left_len {
                        left_map_keys[idx].extend(keys);
                    } else if idx < left_len + right_len {
                        right_map_keys[idx - left_len].extend(keys);
                    }
                }
            }

            let (new_left, left_map) = prune_node(*left, &left_needed, &left_map_keys)?;
            let (new_right, right_map) = prune_node(*right, &right_needed, &right_map_keys)?;
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
            let effective_required: BTreeSet<usize> = if all {
                required.iter().copied().filter(|&i| i < out_len).collect()
            } else {
                // `UNION` deduplicates across all columns.
                (0..out_len).collect()
            };
            let kept_out: Vec<usize> = effective_required.iter().copied().collect();
            let mut new_inputs = Vec::with_capacity(inputs.len());
            for branch in inputs {
                let (new_branch, _) = prune_node(branch, &effective_required, required_map_keys)?;
                new_inputs.push(new_branch);
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
            let mut child_keys = vec![BTreeSet::new(); in_len];
            merge_map_keys(&mut child_keys, required_map_keys);

            let (new_input, child_map) = prune_node(*input, &child_needed, &child_keys)?;
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
            let inner_len = inner.schema().len();
            let inner_req: BTreeSet<usize> = (0..inner_len).collect();
            let inner_keys = vec![BTreeSet::new(); inner_len];
            let (new_inner, _) = prune_node(*inner, &inner_req, &inner_keys)?;
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

fn merge_map_keys(dst: &mut [BTreeSet<String>], src: &[BTreeSet<String>]) {
    for (d, s) in dst.iter_mut().zip(src.iter()) {
        d.extend(s.iter().cloned());
    }
}

fn collect_expr_map_keys(expr: &Expr, out: &mut [BTreeSet<String>]) {
    match expr {
        Expr::Column { .. } | Expr::Literal(_) => {}
        Expr::Unary { expr, .. } | Expr::Cast { expr, .. } => {
            collect_expr_map_keys(expr, out);
        }
        Expr::Binary { left, right, .. } => {
            collect_expr_map_keys(left, out);
            collect_expr_map_keys(right, out);
        }
        Expr::Between {
            expr, low, high, ..
        } => {
            collect_expr_map_keys(expr, out);
            collect_expr_map_keys(low, out);
            collect_expr_map_keys(high, out);
        }
        Expr::InList { expr, list, .. } => {
            collect_expr_map_keys(expr, out);
            for item in list {
                collect_expr_map_keys(item, out);
            }
        }
        Expr::Like { expr, pattern, .. } => {
            collect_expr_map_keys(expr, out);
            collect_expr_map_keys(pattern, out);
        }
        Expr::Case {
            operand,
            when_then,
            else_expr,
            ..
        } => {
            if let Some(op) = operand {
                collect_expr_map_keys(op, out);
            }
            for (w, t) in when_then {
                collect_expr_map_keys(w, out);
                collect_expr_map_keys(t, out);
            }
            if let Some(el) = else_expr {
                collect_expr_map_keys(el, out);
            }
        }
        Expr::MapIndex { map, key } => {
            if let Expr::Column { index, .. } = map.as_ref()
                && let Expr::Literal(ScalarValue::Utf8(k)) = key.as_ref()
                && let Some(slot) = out.get_mut(*index)
            {
                slot.insert(k.clone());
            }
            collect_expr_map_keys(map, out);
            collect_expr_map_keys(key, out);
        }
        Expr::MakeArray { elems, .. } => {
            for e in elems {
                collect_expr_map_keys(e, out);
            }
        }
        Expr::Call { args, .. } => {
            for a in args {
                collect_expr_map_keys(a, out);
            }
        }
    }
}

fn collect_agg_refs(agg: &AggExpr, cols: &mut BTreeSet<usize>, map_keys: &mut [BTreeSet<String>]) {
    if let Some(f) = &agg.filter {
        f.referenced_columns(cols);
        collect_expr_map_keys(f, map_keys);
    }
    visit_agg_func_exprs(&agg.func, &mut |e| {
        e.referenced_columns(cols);
        collect_expr_map_keys(e, map_keys);
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
    cols: &mut BTreeSet<usize>,
    map_keys: &mut [BTreeSet<String>],
) {
    for p in &spec.partition_by {
        p.referenced_columns(cols);
        collect_expr_map_keys(p, map_keys);
    }
    for k in &spec.order_by {
        k.expr.referenced_columns(cols);
        collect_expr_map_keys(&k.expr, map_keys);
    }
    visit_window_func_exprs(&spec.func, &mut |e| {
        e.referenced_columns(cols);
        collect_expr_map_keys(e, map_keys);
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
    let mapped = match expr {
        Expr::Column { .. } | Expr::Literal(_) => expr,
        Expr::Unary {
            op,
            expr: inner,
            data_type,
        } => Expr::Unary {
            op,
            expr: Box::new(map_expr_bottom_up(*inner, f)),
            data_type,
        },
        Expr::Binary {
            left,
            op,
            right,
            data_type,
        } => Expr::Binary {
            left: Box::new(map_expr_bottom_up(*left, f)),
            op,
            right: Box::new(map_expr_bottom_up(*right, f)),
            data_type,
        },
        Expr::Cast {
            expr: inner,
            target,
            mode,
        } => Expr::Cast {
            expr: Box::new(map_expr_bottom_up(*inner, f)),
            target,
            mode,
        },
        Expr::Between {
            expr: inner,
            low,
            high,
            negated,
        } => Expr::Between {
            expr: Box::new(map_expr_bottom_up(*inner, f)),
            low: Box::new(map_expr_bottom_up(*low, f)),
            high: Box::new(map_expr_bottom_up(*high, f)),
            negated,
        },
        Expr::InList {
            expr: inner,
            list,
            negated,
            redacted,
        } => {
            let inner = map_expr_bottom_up(*inner, f);
            let list = if redacted {
                list
            } else {
                list.into_iter()
                    .map(|item| map_expr_bottom_up(item, f))
                    .collect()
            };
            Expr::InList {
                expr: Box::new(inner),
                list,
                negated,
                redacted,
            }
        }
        Expr::Like {
            expr: inner,
            pattern,
            escape,
            case_insensitive,
            negated,
        } => Expr::Like {
            expr: Box::new(map_expr_bottom_up(*inner, f)),
            pattern: Box::new(map_expr_bottom_up(*pattern, f)),
            escape,
            case_insensitive,
            negated,
        },
        Expr::Case {
            operand,
            when_then,
            else_expr,
            data_type,
        } => Expr::Case {
            operand: operand.map(|o| Box::new(map_expr_bottom_up(*o, f))),
            when_then: when_then
                .into_iter()
                .map(|(w, t)| (map_expr_bottom_up(w, f), map_expr_bottom_up(t, f)))
                .collect(),
            else_expr: else_expr.map(|e| Box::new(map_expr_bottom_up(*e, f))),
            data_type,
        },
        Expr::MapIndex { map, key } => Expr::MapIndex {
            map: Box::new(map_expr_bottom_up(*map, f)),
            key: Box::new(map_expr_bottom_up(*key, f)),
        },
        Expr::MakeArray { elems, item_type } => Expr::MakeArray {
            elems: elems
                .into_iter()
                .map(|e| map_expr_bottom_up(e, f))
                .collect(),
            item_type,
        },
        Expr::Call {
            func,
            args,
            data_type,
        } => Expr::Call {
            func,
            args: args.into_iter().map(|a| map_expr_bottom_up(a, f)).collect(),
            data_type,
        },
    };
    f(mapped)
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
            map_keys,
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
            map_keys,
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
            map_keys: Vec::new(),
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
        let LogicalPlan::Scan { filters, .. } = keep_err else {
            panic!("expected Scan");
        };
        assert_eq!(filters.len(), 2);
        assert_eq!(filters[0], div_zero);
        assert_eq!(filters[1], Expr::Literal(ScalarValue::Bool(false)));

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
        let hex_upper = "A1000000000000000000000000000001";
        let span_hex = "0123456789abcdef";

        let base_scan = raw_scan("spans", SqlViewKind::Spans, raw_spans_schema());
        let view_proj = spans_view_project(base_scan);

        // Filter on view columns: trace_id = <32-hex> AND span_id IN (<16-hex>) AND start_time >= 100 AND name LIKE '%hello%'
        let predicate = combine_and(vec![
            bin(
                col_expr(&view, "trace_id"),
                BinaryOp::Eq,
                Expr::Literal(ScalarValue::Utf8(hex_upper.to_owned())),
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
        let expected_trace: [u8; 16] = hex::decode(hex_upper).unwrap().try_into().unwrap();
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
            vec![SubstringHint {
                column: "name".to_owned(),
                needle: "hello".to_owned(),
                case_insensitive: true,
            }]
        );
    }

    #[test]
    fn invalid_and_mixed_hex_literals_stay_as_strings() {
        let view = spans_view_schema();
        let view_proj =
            spans_view_project(raw_scan("spans", SqlViewKind::Spans, raw_spans_schema()));
        let pred = bin(
            bin(
                col_expr(&view, "trace_id"),
                BinaryOp::Eq,
                Expr::Literal(ScalarValue::Utf8("not-hex".to_owned())),
            ),
            BinaryOp::And,
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
        );
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
        assert_eq!(filters.len(), 2);
        assert!(filters[0].fmt_plan().contains("encode("));
        assert!(filters[1].fmt_plan().contains("encode("));
        assert!(id_lookups.is_empty());
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
            map_keys,
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
            map_keys,
            vec![
                ("attributes".to_owned(), "http.status_code".to_owned()),
                ("metadata".to_owned(), "customer".to_owned()),
            ]
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
}
