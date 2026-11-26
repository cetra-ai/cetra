//! GDPR-redacted `EXPLAIN` plan formatting and two-column output batch builder.
//!
//! All expression formatting delegates strictly to [`Expr::fmt_plan`] so redacted
//! `InList` / tombstone predicates render as `user_id IN (<n redacted>)` /
//! `session_id IN (<n redacted>)` and string/binary literals never leak payloads.

use std::fmt::Write as _;

use cetra_column::{Batch, Column, FlatColumn, FlatData, Utf8Data};

use crate::error::QueryError;
use crate::exec::hash_agg::{AggExpr, AggFunc};
use crate::exec::sort_key::SortKey;
use crate::exec::sort_window::{FrameBound, WindowFunc, WindowSpec};
use crate::exec::source::TimeRangeHint;
use crate::ir::Expr;

use super::logical::{LogicalJoinKind, LogicalPlan, explain_schema};

/// Formats a [`LogicalPlan`] tree as an indented, privacy-safe string.
///
/// Every expression is formatted via [`Expr::fmt_plan`], ensuring redacted `InList`
/// predicates render as `<col> IN (<n redacted>)` and literal values never leak.
pub fn format_logical_plan(plan: &LogicalPlan) -> String {
    let mut out = String::new();
    fmt_node(plan, 0, &mut out);
    out
}

/// Builds the two-column `["plan_type", "plan"]` (`Utf8`, `Utf8`) [`Batch`] for `EXPLAIN`.
pub fn explain_batch(
    logical_plan_text: &str,
    physical_plan_text: &str,
) -> Result<Batch, QueryError> {
    let plan_type_col = Column::Flat(FlatColumn::try_new(
        FlatData::Utf8(Utf8Data::try_from_slices([
            "logical_plan",
            "physical_plan",
        ])?),
        None,
    )?);
    let plan_col = Column::Flat(FlatColumn::try_new(
        FlatData::Utf8(Utf8Data::try_from_slices([
            logical_plan_text,
            physical_plan_text,
        ])?),
        None,
    )?);
    Ok(Batch::try_new_all(
        explain_schema(),
        vec![plan_type_col, plan_col],
        2,
    )?)
}

fn fmt_indent(depth: usize, out: &mut String) {
    for _ in 0..depth {
        out.push_str("  ");
    }
}

fn fmt_node(plan: &LogicalPlan, depth: usize, out: &mut String) {
    if !out.is_empty() {
        out.push('\n');
    }
    fmt_indent(depth, out);

    match plan {
        LogicalPlan::Scan {
            table,
            projection,
            filters,
            time_range,
            id_lookups,
            substrings,
            map_keys,
            limit,
            schema,
            ..
        } => {
            let proj_names: Vec<&str> = schema.fields().iter().map(|f| f.name.as_str()).collect();
            let _ = write!(out, "Scan: {table} projection=[{}]", proj_names.join(", "));
            if let Some(proj) = projection {
                let _ = write!(out, " (cols={})", proj.len());
            }
            if !filters.is_empty() {
                let rendered: Vec<String> = filters.iter().map(Expr::fmt_plan).collect();
                let _ = write!(out, " filters=[{}]", rendered.join(", "));
            }
            if has_time_range(*time_range) {
                let _ = write!(
                    out,
                    " time_range=(min={:?}, max={:?}, around={:?})",
                    time_range.min_ns, time_range.max_ns, time_range.around_ns
                );
            }
            if !id_lookups.is_empty() {
                let _ = write!(out, " id_lookups={id_lookups:?}");
            }
            if !substrings.is_empty() {
                let _ = write!(out, " substrings={substrings:?}");
            }
            if !map_keys.is_empty() {
                let _ = write!(out, " map_keys={}", map_keys.len());
            }
            if let Some(lim) = limit {
                let _ = write!(out, " limit={lim}");
            }
        }
        LogicalPlan::Values { schema, rows } => {
            let _ = write!(out, "Values: rows={}, cols={}", rows.len(), schema.len());
        }
        LogicalPlan::EmptyRow { .. } => {
            out.push_str("EmptyRow");
        }
        LogicalPlan::Filter { input, predicate } => {
            let _ = write!(out, "Filter: {}", predicate.fmt_plan());
            fmt_node(input, depth + 1, out);
        }
        LogicalPlan::Project {
            input,
            exprs,
            schema,
        } => {
            let items: Vec<String> = exprs
                .iter()
                .zip(schema.fields().iter())
                .map(|(expr, field)| {
                    let rendered = expr.fmt_plan();
                    if rendered == field.name {
                        rendered
                    } else {
                        format!("{rendered} AS {}", field.name)
                    }
                })
                .collect();
            let _ = write!(out, "Project: [{}]", items.join(", "));
            fmt_node(input, depth + 1, out);
        }
        LogicalPlan::Aggregate {
            input,
            group_by,
            aggs,
            ..
        } => {
            let groups: Vec<String> = group_by
                .iter()
                .map(|(name, expr)| {
                    let rendered = expr.fmt_plan();
                    if rendered == *name {
                        rendered
                    } else {
                        format!("{rendered} AS {name}")
                    }
                })
                .collect();
            let agg_strs: Vec<String> = aggs.iter().map(fmt_agg).collect();
            let _ = write!(
                out,
                "Aggregate: group_by=[{}], aggs=[{}]",
                groups.join(", "),
                agg_strs.join(", ")
            );
            fmt_node(input, depth + 1, out);
        }
        LogicalPlan::Window {
            input,
            partition_by,
            order_by,
            specs,
            ..
        } => {
            let parts: Vec<String> = partition_by.iter().map(Expr::fmt_plan).collect();
            let ords: Vec<String> = order_by.iter().map(fmt_sort_key).collect();
            let spec_strs: Vec<String> = specs.iter().map(fmt_window_spec).collect();
            let _ = write!(
                out,
                "Window: partition_by=[{}], order_by=[{}], specs=[{}]",
                parts.join(", "),
                ords.join(", "),
                spec_strs.join(", ")
            );
            fmt_node(input, depth + 1, out);
        }
        LogicalPlan::Sort { input, keys } => {
            let key_strs: Vec<String> = keys.iter().map(fmt_sort_key).collect();
            let _ = write!(out, "Sort: [{}]", key_strs.join(", "));
            fmt_node(input, depth + 1, out);
        }
        LogicalPlan::Limit { input, skip, fetch } => {
            match fetch {
                Some(n) => {
                    let _ = write!(out, "Limit: skip={skip}, fetch={n}");
                }
                None => {
                    let _ = write!(out, "Limit: skip={skip}, fetch=none");
                }
            }
            fmt_node(input, depth + 1, out);
        }
        LogicalPlan::TopK {
            input,
            keys,
            limit,
            offset,
        } => {
            let key_strs: Vec<String> = keys.iter().map(fmt_sort_key).collect();
            let _ = write!(
                out,
                "TopK: limit={limit}, offset={offset}, keys=[{}]",
                key_strs.join(", ")
            );
            fmt_node(input, depth + 1, out);
        }
        LogicalPlan::Distinct { input } => {
            out.push_str("Distinct");
            fmt_node(input, depth + 1, out);
        }
        LogicalPlan::Join {
            left,
            right,
            kind,
            left_keys,
            right_keys,
            null_equals_null,
            residual,
            ..
        } => {
            let kind_str = match kind {
                LogicalJoinKind::Inner => "Inner",
                LogicalJoinKind::Left => "Left",
                LogicalJoinKind::Right => "Right",
                LogicalJoinKind::Full => "Full",
                LogicalJoinKind::Semi => "Semi",
                LogicalJoinKind::Anti => "Anti",
                LogicalJoinKind::AntiNullAware => "AntiNullAware",
                LogicalJoinKind::Cross => "Cross",
            };
            let pairs: Vec<String> = left_keys
                .iter()
                .zip(right_keys.iter())
                .map(|(l, r)| format!("{} = {}", l.fmt_plan(), r.fmt_plan()))
                .collect();
            let _ = write!(
                out,
                "Join ({kind_str}): on=[{}], null_equals_null={null_equals_null}",
                pairs.join(", ")
            );
            if let Some(res) = residual {
                let _ = write!(out, ", filter={}", res.fmt_plan());
            }
            fmt_node(left, depth + 1, out);
            fmt_node(right, depth + 1, out);
        }
        LogicalPlan::Union { inputs, all, .. } => {
            let _ = write!(out, "Union (all={all})");
            for branch in inputs {
                fmt_node(branch, depth + 1, out);
            }
        }
        LogicalPlan::Unnest {
            input,
            columns,
            preserve_nulls,
            ..
        } => {
            let _ = write!(
                out,
                "Unnest: columns={columns:?}, preserve_nulls={preserve_nulls}"
            );
            fmt_node(input, depth + 1, out);
        }
        LogicalPlan::Explain { inner, .. } => {
            out.push_str("Explain");
            fmt_node(inner, depth + 1, out);
        }
    }
}

fn has_time_range(tr: TimeRangeHint) -> bool {
    tr.min_ns.is_some() || tr.max_ns.is_some() || tr.around_ns.is_some()
}

fn fmt_sort_key(key: &SortKey) -> String {
    let dir = if key.descending { "DESC" } else { "ASC" };
    let nulls = if key.nulls_first {
        "NULLS FIRST"
    } else {
        "NULLS LAST"
    };
    format!("{} {dir} {nulls}", key.expr.fmt_plan())
}

fn fmt_agg(agg: &AggExpr) -> String {
    let base = match &agg.func {
        AggFunc::CountStar => "count(*)".to_owned(),
        AggFunc::Count(e) => format!("count({})", e.fmt_plan()),
        AggFunc::CountDistinct(e) => format!("count(DISTINCT {})", e.fmt_plan()),
        AggFunc::Sum(e) => format!("sum({})", e.fmt_plan()),
        AggFunc::SumCostDecimal(e) => format!("sum_cost_decimal({})", e.fmt_plan()),
        AggFunc::Avg(e) => format!("avg({})", e.fmt_plan()),
        AggFunc::Min(e) => format!("min({})", e.fmt_plan()),
        AggFunc::Max(e) => format!("max({})", e.fmt_plan()),
        AggFunc::BoolOr(e) => format!("bool_or({})", e.fmt_plan()),
        AggFunc::BoolAnd(e) => format!("bool_and({})", e.fmt_plan()),
        AggFunc::ApproxPercentileCont { expr, quantile } => {
            format!("approx_percentile_cont({}, {quantile})", expr.fmt_plan())
        }
        AggFunc::PercentileCont { expr, quantile } => {
            format!(
                "percentile_cont({quantile}) WITHIN GROUP ({})",
                expr.fmt_plan()
            )
        }
        AggFunc::ArrayAgg { expr, distinct } => {
            if *distinct {
                format!("array_agg(DISTINCT {})", expr.fmt_plan())
            } else {
                format!("array_agg({})", expr.fmt_plan())
            }
        }
        AggFunc::FirstNonNullByOrder { value, order_by } => {
            let ords: Vec<String> = order_by.iter().map(fmt_sort_key).collect();
            format!(
                "first_non_null({}, [{}])",
                value.fmt_plan(),
                ords.join(", ")
            )
        }
        AggFunc::FirstByOrder { value, order_by } => {
            let ords: Vec<String> = order_by.iter().map(fmt_sort_key).collect();
            format!("first({}, [{}])", value.fmt_plan(), ords.join(", "))
        }
        AggFunc::TagUnion(e) => format!("tag_union({})", e.fmt_plan()),
    };
    let with_filter = match &agg.filter {
        Some(f) => format!("{base} FILTER (WHERE {})", f.fmt_plan()),
        None => base,
    };
    format!("{with_filter} AS {}", agg.output_name)
}

fn fmt_window_spec(spec: &WindowSpec) -> String {
    let f = match &spec.func {
        WindowFunc::RowNumber => "row_number()".to_owned(),
        WindowFunc::Rank => "rank()".to_owned(),
        WindowFunc::DenseRank => "dense_rank()".to_owned(),
        WindowFunc::Lag {
            expr,
            offset,
            default,
        } => match default {
            Some(d) => format!("lag({}, {offset}, {})", expr.fmt_plan(), d.fmt_plan()),
            None => format!("lag({}, {offset})", expr.fmt_plan()),
        },
        WindowFunc::Lead {
            expr,
            offset,
            default,
        } => match default {
            Some(d) => format!("lead({}, {offset}, {})", expr.fmt_plan(), d.fmt_plan()),
            None => format!("lead({}, {offset})", expr.fmt_plan()),
        },
        WindowFunc::FirstValue(e) => format!("first_value({})", e.fmt_plan()),
        WindowFunc::LastValue(e) => format!("last_value({})", e.fmt_plan()),
        WindowFunc::Sum(e) => format!("sum({})", e.fmt_plan()),
        WindowFunc::Count(Some(e)) => format!("count({})", e.fmt_plan()),
        WindowFunc::Count(None) => "count(*)".to_owned(),
        WindowFunc::Avg(e) => format!("avg({})", e.fmt_plan()),
        WindowFunc::Min(e) => format!("min({})", e.fmt_plan()),
        WindowFunc::Max(e) => format!("max({})", e.fmt_plan()),
    };
    let frame = match spec.frame {
        Some((start, end)) => format!(" ROWS BETWEEN {} AND {}", fmt_bound(start), fmt_bound(end)),
        None => String::new(),
    };
    format!("{f}{frame} AS {}", spec.output_name)
}

fn fmt_bound(b: FrameBound) -> String {
    match b {
        FrameBound::UnboundedPreceding => "UNBOUNDED PRECEDING".to_owned(),
        FrameBound::Preceding(n) => format!("{n} PRECEDING"),
        FrameBound::CurrentRow => "CURRENT ROW".to_owned(),
        FrameBound::Following(n) => format!("{n} FOLLOWING"),
        FrameBound::UnboundedFollowing => "UNBOUNDED FOLLOWING".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use cetra_column::{DataType, Field, ScalarValue, Schema};

    use super::*;
    use crate::eval::read_str;
    use crate::exec::source::{IdLookupHint, SubstringHint, TimeRangeHint};
    use crate::ir::{BinaryOp, UnaryOp};
    use crate::sql::catalog::{SqlViewKind, raw_scores_schema};
    use crate::sql::optimize::optimize;
    use crate::tombstone::{TombstoneSpec, TombstoneTable, covered_predicate};

    #[test]
    fn explain_batch_two_column_schema_and_rows() {
        let batch = explain_batch("Scan: spans", "Pipeline[Scan -> Collect]").unwrap();
        assert_eq!(batch.schema().as_ref(), explain_schema().as_ref());
        assert_eq!(batch.num_rows(), 2);
        let plan_type = batch.column(0).unwrap();
        let plan_col = batch.column(1).unwrap();
        assert_eq!(read_str(plan_type, 0), Some("logical_plan"));
        assert_eq!(read_str(plan_type, 1), Some("physical_plan"));
        assert_eq!(read_str(plan_col, 0), Some("Scan: spans"));
        assert_eq!(read_str(plan_col, 1), Some("Pipeline[Scan -> Collect]"));
    }

    #[test]
    fn format_logical_plan_redacts_gdpr_tombstones_and_payloads() {
        let scores_schema = raw_scores_schema();
        let deletions = vec![TombstoneSpec {
            cutoff: 1_000_000,
            trace_ids: vec![],
            users: vec![
                "erased-user-alice@example.com".to_owned(),
                "erased-user-bob@example.com".to_owned(),
            ],
            sessions: vec!["erased-session-secret-xyz".to_owned()],
        }];
        let covered = covered_predicate(TombstoneTable::Scores, &scores_schema, &deletions)
            .unwrap()
            .unwrap();
        let tombstone_filter = Expr::Unary {
            op: UnaryOp::Not,
            expr: Box::new(covered),
            data_type: DataType::Bool,
        };
        let secret_user_filter = Expr::Binary {
            left: Box::new(Expr::Column {
                index: scores_schema.index_of("name").unwrap(),
                name: "name".to_owned(),
                data_type: DataType::Utf8,
            }),
            op: BinaryOp::Eq,
            right: Box::new(Expr::Literal(ScalarValue::Utf8(
                "secret-prompt-literal".to_owned(),
            ))),
            data_type: DataType::Bool,
        };

        let plan = LogicalPlan::Filter {
            input: Box::new(LogicalPlan::Scan {
                table: "scores".to_owned(),
                kind: SqlViewKind::Scores,
                full_schema: Arc::clone(&scores_schema),
                projection: None,
                filters: vec![tombstone_filter],
                time_range: TimeRangeHint::default(),
                id_lookups: vec![IdLookupHint::Utf8 {
                    column: "user_id".to_owned(),
                    values: vec!["lookup-user-secret".to_owned()],
                }],
                substrings: vec![SubstringHint {
                    column: "reason".to_owned(),
                    needle: "secret-substring-needle".to_owned(),
                    case_insensitive: true,
                }],
                map_keys: vec![("metadata".to_owned(), "secret-map-key".to_owned())],
                limit: None,
                schema: scores_schema,
            }),
            predicate: secret_user_filter,
        };

        let optimized = optimize(plan).unwrap();
        let rendered = format_logical_plan(&optimized);
        assert!(
            rendered.contains("user_id IN (<2 redacted>)"),
            "missing user_id redaction in:\n{rendered}"
        );
        assert!(
            rendered.contains("session_id IN (<1 redacted>)"),
            "missing session_id redaction in:\n{rendered}"
        );
        for forbidden in [
            "erased-user-alice@example.com",
            "erased-user-bob@example.com",
            "erased-session-secret-xyz",
            "secret-prompt-literal",
            "lookup-user-secret",
            "secret-substring-needle",
            "secret-map-key",
        ] {
            assert!(
                !rendered.contains(forbidden),
                "GDPR/payload leak `{forbidden}` in:\n{rendered}"
            );
        }

        let batch = explain_batch(&rendered, "PhysicalPlan").unwrap();
        let text = read_str(batch.column(1).unwrap(), 0).unwrap();
        assert!(text.contains("user_id IN (<2 redacted>)"));
        assert!(!text.contains("erased-user-alice@example.com"));
    }

    #[test]
    fn format_logical_plan_covers_topk_join_values_and_explain() {
        let s = Schema::try_new_arc(vec![Field::new("n", DataType::I64, false)]).unwrap();
        let values = LogicalPlan::Values {
            schema: Arc::clone(&s),
            rows: vec![vec![Expr::Literal(ScalarValue::I64(42))]],
        };
        let topk = LogicalPlan::TopK {
            input: Box::new(values),
            keys: vec![SortKey {
                expr: Expr::Column {
                    index: 0,
                    name: "n".to_owned(),
                    data_type: DataType::I64,
                },
                descending: true,
                nulls_first: true,
            }],
            limit: 5,
            offset: 2,
        };
        let exp = LogicalPlan::Explain {
            inner: Box::new(topk),
            schema: explain_schema(),
        };
        let out = format_logical_plan(&exp);
        assert!(out.contains("Explain"));
        assert!(out.contains("TopK: limit=5, offset=2"));
        assert!(out.contains("Values: rows=1, cols=1"));
    }
}
