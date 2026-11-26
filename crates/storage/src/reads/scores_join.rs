//! Filter DSL validation/compilation and latest-score `Left` hash joins for
//! `score()`, `score_label()`, and `has_score()` (D97).

use std::sync::Arc;

use cetra_column::{DataType, Field, ScalarValue, Schema};
use cetra_core::enums::ScoreTargetKind;
use cetra_core::schema::scores;
use cetra_query::exec::hash_agg::execute_hash_agg_streams;
use cetra_query::exec::hash_join::{HashJoinSpec, JoinType, execute_hash_join, join_output_schema};
use cetra_query::exec::streaming::{FilterOp, ProjectOp};
use cetra_query::exec::{
    AggExpr, AggFunc, ExecContext, PartitionStream, ScanSource, ScanSpec, SortKey, TimeRangeHint,
};
use cetra_query::{BinaryOp, CompiledFilter, Expr, FilterView, QueryError, ScalarFn, UnaryOp};

use crate::query_error::RunError;
use crate::reads::context::{
    OperatorStream, invalid_err_to_run, remap_expr_by_name, run_err_to_query, scores_raw_schema,
    view_schema,
};
use crate::reads::present;
use crate::views::{QueryContext, ViewName};

/// Parses `text` as a Filter DSL expression of `view` without running anything (D-I-30).
/// Empty or whitespace-only text is accepted (it means no filter).
///
/// # Errors
///
/// [`RunError::Invalid`] when the text does not parse or names an unknown column;
/// [`RunError::InvalidRequest`] for a view other than traces, spans, sessions or scores.
pub fn validate_filter(_ctx: &QueryContext, view: ViewName, text: &str) -> Result<(), RunError> {
    let filter_view = match view {
        ViewName::Traces => FilterView::Traces,
        ViewName::Spans => FilterView::Spans,
        ViewName::Sessions => FilterView::Sessions,
        ViewName::Scores => FilterView::Scores,
        _ => {
            return Err(RunError::InvalidRequest(
                "filters are checked for traces, spans, sessions and scores only".to_owned(),
            ));
        }
    };
    match present(Some(text)) {
        None => Ok(()),
        Some(text) => {
            compile_filter(filter_view, text)?;
            Ok(())
        }
    }
}

/// Parses, validates, and compiles a non-empty Filter DSL string against `view`'s schema.
pub(crate) fn compile_filter(view: FilterView, text: &str) -> Result<CompiledFilter, RunError> {
    let schema = view_schema(view)?;
    cetra_query::compile_filter(view, &schema, text).map_err(invalid_err_to_run)
}

/// Compiles an optional Filter DSL string, returning `Ok(None)` when `text` is `None` or blank.
pub(crate) fn compile_optional_filter(
    view: FilterView,
    text: Option<&str>,
) -> Result<Option<CompiledFilter>, RunError> {
    match present(text) {
        None => Ok(None),
        Some(text) => compile_filter(view, text).map(Some),
    }
}

/// Applies `compiled` over `left_streams` (whose schema is `left_schema`), left-joining up to
/// 8 latest live scores ordered `(updated_at DESC, score_id DESC)` (without filtering
/// `source != 'issue'`) when `compiled.scores` is non-empty, evaluating the filter predicate,
/// and stripping `__score_{i}_*` helper columns so the output matches `left_schema`.
pub(crate) fn apply_compiled_filter(
    ctx: &crate::reads::context::EngineContext,
    exec_ctx: &ExecContext,
    left_streams: Vec<Box<dyn PartitionStream>>,
    left_schema: &Arc<Schema>,
    compiled: &CompiledFilter,
) -> Result<Vec<Box<dyn PartitionStream>>, QueryError> {
    if left_streams.is_empty() {
        return Ok(Vec::new());
    }

    if compiled.scores.is_empty() {
        let pred = remap_expr_by_name(&compiled.expr, left_schema)?;
        let out = left_streams
            .into_iter()
            .map(|stream| {
                let filter_op = FilterOp::new(pred.clone(), exec_ctx.clone());
                Box::new(OperatorStream::new(
                    stream,
                    vec![Box::new(filter_op)],
                    exec_ctx,
                )) as Box<dyn PartitionStream>
            })
            .collect();
        return Ok(out);
    }

    let (target_kind, key_cols) = join_on(compiled.view)?;
    let scores_raw = scores_raw_schema().map_err(run_err_to_query)?;

    let mut current_streams = left_streams;
    let mut current_schema = Arc::clone(left_schema);

    for (index, score_name) in compiled.scores.iter().enumerate() {
        let latest = latest_score_stream(&LatestScoreParams {
            ctx,
            exec_ctx,
            scores_raw: &scores_raw,
            left_schema: &current_schema,
            target_kind,
            key_cols,
            index,
            score_name,
        })?;

        let left_keys = key_cols
            .iter()
            .map(|k| col_by_name(&current_schema, k))
            .collect::<Result<Vec<_>, _>>()?;
        let right_keys = latest
            .key_names
            .iter()
            .map(|k| col_by_name(&latest.schema, k))
            .collect::<Result<Vec<_>, _>>()?;

        let join_spec = HashJoinSpec {
            join_type: JoinType::Left,
            left_keys,
            right_keys,
            null_equals_null: false,
            residual_filter: None,
        };
        let joined_schema = join_output_schema(JoinType::Left, &current_schema, &latest.schema);
        let joined = execute_hash_join(
            current_streams,
            Arc::clone(&current_schema),
            vec![latest.stream],
            latest.schema,
            join_spec,
            exec_ctx,
        )?;
        current_schema = joined_schema;
        current_streams = vec![joined];
    }

    let remapped_pred = remap_expr_by_name(&compiled.expr, &current_schema)?;
    let strip_exprs: Vec<Expr> = left_schema
        .fields()
        .iter()
        .enumerate()
        .map(|(i, f)| Expr::Column {
            index: i,
            name: f.name.clone(),
            data_type: f.data_type.clone(),
        })
        .collect();

    let out = current_streams
        .into_iter()
        .map(|stream| {
            let filter_op = FilterOp::new(remapped_pred.clone(), exec_ctx.clone());
            let project_op = ProjectOp::with_schema(
                strip_exprs.clone(),
                Arc::clone(left_schema),
                exec_ctx.clone(),
            )?;
            Ok(Box::new(OperatorStream::new(
                stream,
                vec![Box::new(filter_op), Box::new(project_op)],
                exec_ctx,
            )) as Box<dyn PartitionStream>)
        })
        .collect::<Result<Vec<_>, QueryError>>()?;
    Ok(out)
}

struct LatestScoreStream {
    stream: Box<dyn PartitionStream>,
    schema: Arc<Schema>,
    key_names: Vec<String>,
}

struct LatestScoreParams<'a> {
    ctx: &'a crate::reads::context::EngineContext,
    exec_ctx: &'a ExecContext,
    scores_raw: &'a Arc<Schema>,
    left_schema: &'a Schema,
    target_kind: &'a str,
    key_cols: &'a [&'a str],
    index: usize,
    score_name: &'a str,
}

fn latest_score_stream(params: &LatestScoreParams<'_>) -> Result<LatestScoreStream, QueryError> {
    let LatestScoreParams {
        ctx,
        exec_ctx,
        scores_raw,
        left_schema,
        target_kind,
        key_cols,
        index,
        score_name,
    } = *params;
    let mut scan_indices = Vec::with_capacity(key_cols.len() + 4);
    for &k in key_cols {
        scan_indices.push(
            scores_raw
                .index_of(k)
                .ok_or_else(|| QueryError::internal(format!("missing score key column {k}")))?,
        );
    }
    for col_name in [
        scores::VALUE_NUM,
        scores::VALUE_STR,
        scores::UPDATED_AT,
        scores::SCORE_ID,
    ] {
        scan_indices.push(
            scores_raw
                .index_of(col_name)
                .ok_or_else(|| QueryError::internal(format!("missing score column {col_name}")))?,
        );
    }

    let mut filters = vec![
        eq_lit_str(scores_raw, scores::TARGET, target_kind)?,
        eq_lit_str(scores_raw, scores::NAME, score_name)?,
    ];
    for &k in key_cols {
        let key_col = col_by_name(scores_raw, k)?;
        filters.push(Expr::Unary {
            op: UnaryOp::IsNotNull,
            expr: Box::new(key_col),
            data_type: DataType::Bool,
        });
    }
    if let Some(vf) = ctx.visible_from {
        let created_col = col_by_name(scores_raw, scores::CREATED_AT)?;
        filters.push(Expr::Binary {
            left: Box::new(created_col),
            op: BinaryOp::GtEq,
            right: Box::new(Expr::Literal(ScalarValue::TimestampNs {
                value: vf,
                utc: true,
            })),
            data_type: DataType::Bool,
        });
    }

    let scan_spec = ScanSpec {
        projection: Some(scan_indices),
        map_projections: Default::default(),
        time_range: TimeRangeHint {
            min_ns: ctx.visible_from,
            max_ns: None,
            around_ns: None,
        },
        id_lookups: Vec::new(),
        filters,
        substrings: Vec::new(),
        sort_hints: Vec::new(),
        limit: None,
    };

    let proj_schema = ctx.scores.projected_schema(&scan_spec)?;
    let partitions = ctx.scores.open_partitions(&scan_spec, exec_ctx)?;

    let mut group_by = Vec::with_capacity(key_cols.len());
    let mut right_key_names = Vec::with_capacity(key_cols.len());
    let mut out_fields = Vec::with_capacity(key_cols.len() + 3);
    for &k in key_cols {
        let left_col = col_by_name(left_schema, k)?;
        let right_col = col_by_name(&proj_schema, k)?;
        let key_expr = if left_col.data_type() == DataType::Utf8
            && matches!(
                right_col.data_type(),
                DataType::FixedBytes8 | DataType::FixedBytes16
            ) {
            Expr::Call {
                func: ScalarFn::Encode,
                args: vec![
                    right_col,
                    Expr::Literal(ScalarValue::Utf8("hex".to_owned())),
                ],
                data_type: DataType::Utf8,
            }
        } else {
            right_col
        };
        let alias = format!("__score_{index}_{k}");
        out_fields.push(Field::new(alias.clone(), key_expr.data_type(), true));
        right_key_names.push(alias.clone());
        group_by.push((alias, key_expr));
    }

    let order_by = vec![
        SortKey {
            expr: col_by_name(&proj_schema, scores::UPDATED_AT)?,
            descending: true,
            nulls_first: true,
        },
        SortKey {
            expr: col_by_name(&proj_schema, scores::SCORE_ID)?,
            descending: true,
            nulls_first: true,
        },
    ];
    let num_name = format!("__score_{index}_num");
    let str_name = format!("__score_{index}_str");
    let present_name = format!("__score_{index}_present");

    let agg_spec = cetra_query::exec::HashAggSpec {
        group_by,
        aggs: vec![
            AggExpr {
                func: AggFunc::FirstByOrder {
                    value: col_by_name(&proj_schema, scores::VALUE_NUM)?,
                    order_by: order_by.clone(),
                },
                filter: None,
                output_name: num_name.clone(),
            },
            AggExpr {
                func: AggFunc::FirstByOrder {
                    value: col_by_name(&proj_schema, scores::VALUE_STR)?,
                    order_by,
                },
                filter: None,
                output_name: str_name.clone(),
            },
        ],
    };

    out_fields.push(Field::new(num_name, DataType::F64, true));
    out_fields.push(Field::new(str_name, DataType::Utf8, true));
    out_fields.push(Field::new(present_name, DataType::Bool, true));
    let right_schema = Schema::try_new_arc(out_fields)?;

    let agg_stream = execute_hash_agg_streams(partitions, proj_schema, agg_spec, exec_ctx)?;
    let mut proj_exprs = Vec::with_capacity(right_schema.len());
    for (i, field) in right_schema.fields().iter().enumerate() {
        if i + 1 < right_schema.len() {
            proj_exprs.push(Expr::Column {
                index: i,
                name: field.name.clone(),
                data_type: field.data_type.clone(),
            });
        } else {
            proj_exprs.push(Expr::Literal(ScalarValue::Bool(true)));
        }
    }
    let project_op =
        ProjectOp::with_schema(proj_exprs, Arc::clone(&right_schema), exec_ctx.clone())?;
    let wrapped = Box::new(OperatorStream::new(
        agg_stream,
        vec![Box::new(project_op)],
        exec_ctx,
    )) as Box<dyn PartitionStream>;

    Ok(LatestScoreStream {
        stream: wrapped,
        schema: right_schema,
        key_names: right_key_names,
    })
}

fn join_on(view: FilterView) -> Result<(&'static str, &'static [&'static str]), QueryError> {
    match view {
        FilterView::Traces => Ok((ScoreTargetKind::Trace.as_str(), &[scores::TRACE_ID])),
        FilterView::Spans => Ok((
            ScoreTargetKind::Span.as_str(),
            &[scores::TRACE_ID, scores::SPAN_ID],
        )),
        FilterView::Sessions => Ok((ScoreTargetKind::Session.as_str(), &[scores::SESSION_ID])),
        FilterView::Scores => Err(QueryError::internal(
            "score functions are not available on the scores view",
        )),
    }
}

fn col_by_name(schema: &Schema, name: &str) -> Result<Expr, QueryError> {
    let idx = schema
        .index_of(name)
        .ok_or_else(|| QueryError::internal(format!("missing column {name} in schema")))?;
    Ok(Expr::Column {
        index: idx,
        name: name.to_owned(),
        data_type: schema.fields()[idx].data_type.clone(),
    })
}

fn eq_lit_str(schema: &Schema, col_name: &str, value: &str) -> Result<Expr, QueryError> {
    Ok(Expr::Binary {
        left: Box::new(col_by_name(schema, col_name)?),
        op: BinaryOp::Eq,
        right: Box::new(Expr::Literal(ScalarValue::Utf8(value.to_owned()))),
        data_type: DataType::Bool,
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use bytesize::ByteSize;
    use cetra_core::enums::ScoreSource as CoreScoreSource;
    use cetra_core::ids::{ProjectId, TraceId};
    use cetra_core::score::{Score, ScoreTarget, ScoreValue};
    use cetra_core::span::Span;
    use cetra_query::exec::{CollectSink, run_pipeline};

    use super::*;
    use crate::QueryOptions;
    use crate::config::{
        FlushConfig, MemtableConfig, QueryConfig, S3Config, StorageConfig, WalConfig,
    };
    use crate::reads::columns::{require_col, string_at};
    use crate::reads::context::{EngineContext, traces_raw_schema, traces_view_projection};
    use crate::test_support::{score_row, score_rows, span_row, span_rows};
    use crate::views::ViewName;

    fn test_config(dir: &std::path::Path) -> StorageConfig {
        StorageConfig {
            delete_objects: true,
            data_dir: dir.to_path_buf(),
            min_free_bytes: ByteSize::b(0),
            object_store: None,
            s3: S3Config::default(),
            wal: WalConfig::default(),
            flush: FlushConfig::default(),
            memtable: MemtableConfig::default(),
            query: QueryConfig {
                target_partitions: 2,
                ..QueryConfig::default()
            },
            anchor: Default::default(),
            gc: Default::default(),
            compaction: Default::default(),
            deletion: Default::default(),
        }
    }

    #[test]
    fn validate_filter_rejects_non_boolean_and_unsupported_views() {
        let ctx = QueryContext::new(crate::test_support::example_query(), None).unwrap();
        assert!(validate_filter(&ctx, ViewName::Traces, "").is_ok());
        assert!(validate_filter(&ctx, ViewName::Traces, "   ").is_ok());
        assert!(validate_filter(&ctx, ViewName::Traces, "error_count > 0").is_ok());
        assert!(validate_filter(&ctx, ViewName::Spans, "span_kind = 'llm'").is_ok());
        assert!(validate_filter(&ctx, ViewName::Sessions, "trace_count > 1").is_ok());
        assert!(validate_filter(&ctx, ViewName::Scores, "name = 'x'").is_ok());

        match validate_filter(&ctx, ViewName::Traces, "1 + 2") {
            Err(RunError::Invalid(inv)) => {
                assert_eq!(inv.message, "the filter must be a true/false expression");
            }
            other => panic!("expected Invalid, got {other:?}"),
        }

        match validate_filter(&ctx, ViewName::ExperimentItems, "name = 'x'") {
            Err(RunError::InvalidRequest(msg)) => {
                assert_eq!(
                    msg,
                    "filters are checked for traces, spans, sessions and scores only"
                );
            }
            other => panic!("expected InvalidRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn apply_compiled_filter_joins_latest_score_including_issue_source_and_null_win() {
        let dir = tempfile::tempdir().expect("tempdir");
        let storage = crate::Storage::open(test_config(dir.path()))
            .await
            .expect("open");
        let project: ProjectId = "018f6b4e-7c1a-7c1a-8c1a-000000000001".parse().expect("pid");

        let s1: Span = span_row(1, 1, 1_000_000, 2_000_000);
        let s2: Span = span_row(2, 1, 2_000_000, 3_000_000);
        let t1: TraceId = s1.trace_id;
        let t2: TraceId = s2.trace_id;

        storage
            .append_spans(project, span_rows(&[s1, s2]))
            .await
            .expect("append spans");

        // Trace 1: latest score has source='issue' (MUST be included by filter join!) and value_num = 0.9.
        let mut sc1_old: Score = score_row(1, 1, 1_000, 1_000, 1_000, false, "quality");
        sc1_old.target = ScoreTarget::Trace { trace_id: t1 };
        sc1_old.value = ScoreValue::Numeric(0.1);
        let mut sc1_new: Score = score_row(2, 1, 1_000, 2_000, 2_000, false, "quality");
        sc1_new.target = ScoreTarget::Trace { trace_id: t1 };
        sc1_new.source = CoreScoreSource::Issue;
        sc1_new.value = ScoreValue::Numeric(0.9);

        // Trace 2: older score has value_num = 0.9, newer score has value_num = NULL, value_str = "bad".
        // FirstByOrder must preserve NULL on winning row so `score('quality') > 0.5` does NOT match Trace 2!
        let mut sc2_old: Score = score_row(3, 2, 2_000, 1_000, 1_000, false, "quality");
        sc2_old.target = ScoreTarget::Trace { trace_id: t2 };
        sc2_old.value = ScoreValue::Numeric(0.9);
        let mut sc2_new: Score = score_row(4, 2, 2_000, 2_000, 2_000, false, "quality");
        sc2_new.target = ScoreTarget::Trace { trace_id: t2 };
        sc2_new.value = ScoreValue::Categorical {
            label: "bad".to_owned(),
            value: None,
        };

        storage
            .append_scores(project, score_rows(&[sc1_old, sc1_new, sc2_old, sc2_new]))
            .await
            .expect("append scores");

        let ctx = EngineContext::from_storage(&storage, project, &QueryOptions::new(None, None))
            .expect("ctx");
        let compiled =
            compile_filter(FilterView::Traces, "score('quality') > 0.5").expect("compile");

        let ctx_clone = ctx.clone();
        let compiled_clone = compiled.clone();
        let (batches, _) = ctx
            .run_with_ctx(Duration::from_secs(5), move |exec_ctx| {
                let raw_schema = traces_raw_schema().map_err(run_err_to_query)?;
                let raw_streams = ctx_clone
                    .traces
                    .open_partitions(&ScanSpec::default(), exec_ctx)?;
                let view_streams: Vec<Box<dyn PartitionStream>> = raw_streams
                    .into_iter()
                    .map(|s| {
                        let proj = traces_view_projection(&raw_schema, exec_ctx.clone())?;
                        Ok(
                            Box::new(OperatorStream::new(s, vec![Box::new(proj)], exec_ctx))
                                as Box<dyn PartitionStream>,
                        )
                    })
                    .collect::<Result<Vec<_>, QueryError>>()?;
                let view_schema =
                    crate::reads::context::traces_view_schema().map_err(run_err_to_query)?;
                let filtered = apply_compiled_filter(
                    &ctx_clone,
                    exec_ctx,
                    view_streams,
                    &view_schema,
                    &compiled_clone,
                )?;
                let pool = Arc::clone(&exec_ctx.pool);
                let sinks = run_pipeline(
                    exec_ctx,
                    filtered,
                    |_| Ok(Vec::new()),
                    move |_| Ok(CollectSink::new(&pool)),
                )?;
                Ok(sinks
                    .into_iter()
                    .flat_map(CollectSink::into_batches)
                    .collect::<Vec<_>>())
            })
            .await
            .expect("run");

        let mut matched_ids = Vec::new();
        for batch in &batches {
            assert_eq!(batch.schema().len(), 29, "helper columns must be stripped");
            let col = require_col(batch, "trace_id").expect("trace_id");
            for r in 0..batch.num_rows() {
                if let Some(id) = string_at(batch, col, r).expect("id") {
                    matched_ids.push(id);
                }
            }
        }
        assert_eq!(matched_ids, vec![t1.to_hex()]);
        let _ = storage.shutdown().await;
    }
}
