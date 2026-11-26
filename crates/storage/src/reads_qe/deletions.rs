//! Native query engine implementations of `filter_trace_ids`, `user_deletion_scope`,
//! `eval_trace_ids`, and `session_ids_of_traces`.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::time::Duration;

use cetra_column::{Batch, DataType, Field, ScalarValue, Schema};
use cetra_core::ids::TraceId;
use cetra_core::names::{environment, metadata_key};
use cetra_core::schema::{spans, traces};
use cetra_query::exec::hash_join::{HashJoinSpec, JoinType, execute_hash_join};
use cetra_query::exec::streaming::{LimitOp, ProjectOp};
use cetra_query::exec::{
    CollectSink, ExecContext, IdLookupHint, PartitionStream, ScanSource, ScanSpec, TimeRangeHint,
    run_pipeline as exec_run_pipeline,
};
use cetra_query::{BinaryOp, Expr, FilterView, QueryError, UnaryOp};

use crate::query_error::RunError;
use crate::reads::{FilterDeletionRequest, ListScope, TraceTarget, UserDeletionScope};
use crate::reads_qe::columns::{active_rows, raw_string_at, raw_time_at, require_col};
use crate::reads_qe::context::{
    EngineContext, OperatorStream, populate_id_lookups, query_err_to_run,
    rewrite_view_expr_for_raw, run_err_to_query, spans_raw_schema, traces_raw_schema,
    traces_view_projection, traces_view_schema,
};
use crate::reads_qe::scope::{SearchPredicate, StreamSet, scoped_traces_spec, search_predicate};
use crate::reads_qe::scores_join::{apply_compiled_filter, compile_optional_filter};
use crate::reads_qe::targets::raw_fixed16_at;

const TARGET_COL: &str = "target";

/// Traces of the traces-view rows passing window, default exclusions, search, and filter,
/// at most `request.limit`, sorted by `TraceId`.
pub(crate) async fn filter_trace_ids(
    ctx: &EngineContext,
    request: FilterDeletionRequest<'_>,
) -> Result<Vec<TraceTarget>, RunError> {
    let compiled = compile_optional_filter(FilterView::Traces, request.filter)?;
    let references_environment = compiled.as_ref().is_some_and(|f| f.references_environment);
    let scope = ListScope {
        window: Some(request.window),
        include_eval: request.include_eval,
        include_experiments: request.include_experiments,
    };
    let mut spec = scoped_traces_spec(ctx, scope, references_environment)?;
    let search_owned = request.search.map(str::to_owned);
    let window = request.window;
    let limit = request.limit;

    let ctx_c = ctx.clone();
    let (targets, _) = ctx
        .run_with_ctx(request.timeout, move |exec_ctx| {
            if limit == 0 {
                return Ok(Vec::new());
            }
            let search = if let Some(text) = search_owned.as_deref() {
                search_predicate(&ctx_c, exec_ctx, FilterView::Traces, text, Some(window))
                    .map_err(run_err_to_query)?
            } else {
                SearchPredicate::None
            };

            let traces_raw = traces_raw_schema().map_err(run_err_to_query)?;
            let needs_score_join = compiled.as_ref().is_some_and(|f| !f.scores.is_empty());

            let (mut streams, stream_schema, is_hex) = if !needs_score_join {
                if let Some(filter) = &compiled {
                    let rewritten = rewrite_view_expr_for_raw(
                        &filter.expr,
                        &traces_raw,
                        &[traces::TRACE_ID, traces::ROOT_SPAN_ID],
                        true,
                    )?;
                    spec.filters.push(rewritten);
                    populate_id_lookups(&mut spec);
                }
                let tid_idx = traces_raw
                    .index_of(traces::TRACE_ID)
                    .ok_or_else(|| QueryError::internal("trace_id missing from traces schema"))?;
                let start_idx = traces_raw
                    .index_of(traces::START_TIME)
                    .ok_or_else(|| QueryError::internal("start_time missing from traces schema"))?;
                spec.projection = Some(vec![tid_idx, start_idx]);
                if matches!(search, SearchPredicate::None) {
                    spec.limit = Some(limit);
                }
                let proj_schema = ctx_c.traces.projected_schema(&spec)?;
                let streams = ctx_c.traces.open_partitions(&spec, exec_ctx)?;
                (streams, proj_schema, false)
            } else {
                let raw_streams = ctx_c.traces.open_partitions(&spec, exec_ctx)?;
                let view_schema = traces_view_schema().map_err(run_err_to_query)?;
                let view_streams: Vec<Box<dyn PartitionStream>> = raw_streams
                    .into_iter()
                    .map(|s| {
                        let proj = traces_view_projection(&traces_raw, exec_ctx.clone())?;
                        Ok(
                            Box::new(OperatorStream::new(s, vec![Box::new(proj)], exec_ctx))
                                as Box<dyn PartitionStream>,
                        )
                    })
                    .collect::<Result<Vec<_>, QueryError>>()?;
                (view_streams, view_schema, true)
            };

            if streams.is_empty() {
                return Ok(Vec::new());
            }

            if let SearchPredicate::Traces {
                trace_ids: search_streams,
                schema: search_schema,
            } = search
            {
                if search_streams.is_empty() {
                    return Ok(Vec::new());
                }
                let encoded = if is_hex {
                    encode_trace_id_stream(search_streams, exec_ctx)?
                } else {
                    StreamSet {
                        streams: search_streams,
                        schema: search_schema,
                    }
                };
                let left_key = col_expr(&stream_schema, traces::TRACE_ID)?;
                let right_key = col_expr(&encoded.schema, traces::TRACE_ID)?;
                let joined = execute_hash_join(
                    streams,
                    Arc::clone(&stream_schema),
                    encoded.streams,
                    encoded.schema,
                    HashJoinSpec {
                        join_type: JoinType::LeftSemi,
                        left_keys: vec![left_key],
                        right_keys: vec![right_key],
                        null_equals_null: false,
                        residual_filter: None,
                    },
                    exec_ctx,
                )?;
                streams = vec![joined];
            }

            if needs_score_join && let Some(filter) = &compiled {
                streams = apply_compiled_filter(&ctx_c, exec_ctx, streams, &stream_schema, filter)?;
            }

            if streams.is_empty() {
                return Ok(Vec::new());
            }

            let shared_rem = Arc::new(AtomicUsize::new(limit));
            let pool_c = Arc::clone(&exec_ctx.pool);
            let sinks = exec_run_pipeline(
                exec_ctx,
                streams,
                move |_| {
                    Ok(vec![Box::new(LimitOp::new(
                        0,
                        Some(limit),
                        Some(Arc::clone(&shared_rem)),
                    ))])
                },
                move |_| Ok(CollectSink::new(&pool_c)),
            )?;
            let batches: Vec<Batch> = sinks
                .into_iter()
                .flat_map(CollectSink::into_batches)
                .collect();

            let mut found: BTreeMap<TraceId, i64> = BTreeMap::new();
            for batch in &batches {
                if batch.num_rows() == 0 {
                    continue;
                }
                let id_col = require_col(batch, traces::TRACE_ID).map_err(run_err_to_query)?;
                let start_col = require_col(batch, traces::START_TIME).map_err(run_err_to_query)?;
                for phys in active_rows(batch) {
                    let trace_id = if is_hex {
                        let s = raw_string_at(id_col, phys)
                            .map_err(run_err_to_query)?
                            .ok_or_else(bad_id)?;
                        TraceId::from_str(&s).map_err(|_| bad_id())?
                    } else {
                        let bytes = raw_fixed16_at(id_col, phys).ok_or_else(bad_id)?;
                        TraceId::from_bytes(bytes).map_err(|_| bad_id())?
                    };
                    let start = raw_time_at(start_col, phys).map_err(run_err_to_query)?;
                    found
                        .entry(trace_id)
                        .and_modify(|s| *s = (*s).min(start))
                        .or_insert(start);
                }
            }

            Ok(found
                .into_iter()
                .map(|(trace_id, start_time)| TraceTarget {
                    trace_id,
                    start_time,
                })
                .collect())
        })
        .await?;

    Ok(targets)
}

/// Trace IDs and session IDs of `spans` rows with `user_id = user_id` and `ingest_time <= cutoff`.
/// No window and no default exclusions.
pub(crate) async fn user_deletion_scope(
    ctx: &EngineContext,
    user_id: &str,
    cutoff: i64,
    timeout: Duration,
) -> Result<UserDeletionScope, RunError> {
    let spans_raw = spans_raw_schema()?;
    let tid_idx = spans_raw
        .index_of(spans::TRACE_ID)
        .ok_or_else(|| QueryError::internal("trace_id missing from spans schema"))
        .map_err(query_err_to_run)?;
    let sid_idx = spans_raw
        .index_of(spans::SESSION_ID)
        .ok_or_else(|| QueryError::internal("session_id missing from spans schema"))
        .map_err(query_err_to_run)?;

    let user_col = col_expr(&spans_raw, spans::USER_ID).map_err(query_err_to_run)?;
    let ingest_col = col_expr(&spans_raw, spans::INGEST_TIME).map_err(query_err_to_run)?;
    let mut filters = vec![
        Expr::Binary {
            left: Box::new(user_col),
            op: BinaryOp::Eq,
            right: Box::new(Expr::Literal(ScalarValue::Utf8(user_id.to_owned()))),
            data_type: DataType::Bool,
        },
        Expr::Binary {
            left: Box::new(ingest_col),
            op: BinaryOp::LtEq,
            right: Box::new(Expr::Literal(ScalarValue::TimestampNs {
                value: cutoff,
                utc: true,
            })),
            data_type: DataType::Bool,
        },
    ];
    if let Some(vf) = ctx.visible_from {
        let start_col = col_expr(&spans_raw, spans::START_TIME).map_err(query_err_to_run)?;
        filters.push(Expr::Binary {
            left: Box::new(start_col),
            op: BinaryOp::GtEq,
            right: Box::new(Expr::Literal(ScalarValue::TimestampNs {
                value: vf,
                utc: true,
            })),
            data_type: DataType::Bool,
        });
    }

    let spec = ScanSpec {
        projection: Some(vec![tid_idx, sid_idx]),
        map_keys: Vec::new(),
        time_range: TimeRangeHint {
            min_ns: ctx.visible_from,
            max_ns: None,
            around_ns: ctx.around,
        },
        id_lookups: vec![IdLookupHint::Utf8 {
            column: spans::USER_ID.to_owned(),
            values: vec![user_id.to_owned()],
        }],
        filters,
        substrings: Vec::new(),
        sort_hints: Vec::new(),
        limit: None,
    };

    let ctx_c = ctx.clone();
    let (scope, _) = ctx
        .run_with_ctx(timeout, move |exec_ctx| {
            let streams = ctx_c.spans.open_partitions(&spec, exec_ctx)?;
            if streams.is_empty() {
                return Ok(UserDeletionScope::default());
            }
            let pool_c = Arc::clone(&exec_ctx.pool);
            let sinks = exec_run_pipeline(
                exec_ctx,
                streams,
                |_| Ok(Vec::new()),
                move |_| Ok(CollectSink::new(&pool_c)),
            )?;
            let mut trace_ids = BTreeSet::new();
            let mut session_ids = BTreeSet::new();
            for sink in sinks {
                for batch in sink.into_batches() {
                    if batch.num_rows() == 0 {
                        continue;
                    }
                    let id_col = require_col(&batch, spans::TRACE_ID).map_err(run_err_to_query)?;
                    let sid_col =
                        require_col(&batch, spans::SESSION_ID).map_err(run_err_to_query)?;
                    for phys in active_rows(&batch) {
                        if let Some(bytes) = raw_fixed16_at(id_col, phys) {
                            trace_ids.insert(TraceId::from_bytes(bytes).map_err(|_| bad_id())?);
                        }
                        if let Some(sid) = raw_string_at(sid_col, phys).map_err(run_err_to_query)? {
                            session_ids.insert(sid);
                        }
                    }
                }
            }
            Ok(UserDeletionScope {
                trace_ids: trace_ids.into_iter().collect(),
                session_ids: session_ids.into_iter().collect(),
            })
        })
        .await?;

    Ok(scope)
}

/// Evaluation traces (`environment = 'cetra-eval'`) whose `metadata['cetra.eval.target_trace_id']`
/// is in `targets` (and whose own `trace_id` is not already in `targets`).
pub(crate) async fn eval_trace_ids(
    ctx: &EngineContext,
    targets: &HashSet<TraceId>,
    timeout: Duration,
) -> Result<Vec<TraceId>, RunError> {
    if targets.is_empty() {
        return Ok(Vec::new());
    }
    let spans_raw = spans_raw_schema()?;
    let tid_idx = spans_raw
        .index_of(spans::TRACE_ID)
        .ok_or_else(|| QueryError::internal("trace_id missing from spans schema"))
        .map_err(query_err_to_run)?;
    let meta_idx = spans_raw
        .index_of(spans::METADATA)
        .ok_or_else(|| QueryError::internal("metadata missing from spans schema"))
        .map_err(query_err_to_run)?;

    let env_col = col_expr(&spans_raw, spans::ENVIRONMENT).map_err(query_err_to_run)?;
    let meta_col = col_expr(&spans_raw, spans::METADATA).map_err(query_err_to_run)?;
    let target_access = Expr::MapIndex {
        map: Box::new(meta_col),
        key: Box::new(Expr::Literal(ScalarValue::Utf8(
            metadata_key::EVAL_TARGET_TRACE_ID.to_owned(),
        ))),
    };

    let mut filters = vec![
        Expr::Binary {
            left: Box::new(env_col),
            op: BinaryOp::Eq,
            right: Box::new(Expr::Literal(ScalarValue::Utf8(
                environment::EVAL.to_owned(),
            ))),
            data_type: DataType::Bool,
        },
        Expr::Unary {
            op: UnaryOp::IsNotNull,
            expr: Box::new(target_access),
            data_type: DataType::Bool,
        },
    ];
    if let Some(vf) = ctx.visible_from {
        let start_col = col_expr(&spans_raw, spans::START_TIME).map_err(query_err_to_run)?;
        filters.push(Expr::Binary {
            left: Box::new(start_col),
            op: BinaryOp::GtEq,
            right: Box::new(Expr::Literal(ScalarValue::TimestampNs {
                value: vf,
                utc: true,
            })),
            data_type: DataType::Bool,
        });
    }

    let spec = ScanSpec {
        projection: Some(vec![tid_idx, meta_idx]),
        map_keys: vec![(
            spans::METADATA.to_owned(),
            metadata_key::EVAL_TARGET_TRACE_ID.to_owned(),
        )],
        time_range: TimeRangeHint {
            min_ns: ctx.visible_from,
            max_ns: None,
            around_ns: ctx.around,
        },
        id_lookups: Vec::new(),
        filters,
        substrings: Vec::new(),
        sort_hints: Vec::new(),
        limit: None,
    };

    let ctx_c = ctx.clone();
    let targets_c = targets.clone();
    let (found, _) = ctx
        .run_with_ctx(timeout, move |exec_ctx| {
            let streams = ctx_c.spans.open_partitions(&spec, exec_ctx)?;
            if streams.is_empty() {
                return Ok(Vec::new());
            }
            let proj_schema = ctx_c.spans.projected_schema(&spec)?;
            let out_schema = Schema::try_new_arc(vec![
                Field::new(spans::TRACE_ID, DataType::FixedBytes16, false),
                Field::new(TARGET_COL, DataType::Utf8, true),
            ])?;
            let proj_exprs = vec![
                col_expr(&proj_schema, spans::TRACE_ID)?,
                Expr::MapIndex {
                    map: Box::new(col_expr(&proj_schema, spans::METADATA)?),
                    key: Box::new(Expr::Literal(ScalarValue::Utf8(
                        metadata_key::EVAL_TARGET_TRACE_ID.to_owned(),
                    ))),
                },
            ];

            let pool_c = Arc::clone(&exec_ctx.pool);
            let ctx_chain = exec_ctx.clone();
            let sinks = exec_run_pipeline(
                exec_ctx,
                streams,
                move |_| {
                    let proj = ProjectOp::with_schema(
                        proj_exprs.clone(),
                        Arc::clone(&out_schema),
                        ctx_chain.clone(),
                    )?;
                    Ok(vec![Box::new(proj)])
                },
                move |_| Ok(CollectSink::new(&pool_c)),
            )?;

            let mut found = BTreeSet::new();
            for sink in sinks {
                for batch in sink.into_batches() {
                    if batch.num_rows() == 0 {
                        continue;
                    }
                    let id_col = require_col(&batch, spans::TRACE_ID).map_err(run_err_to_query)?;
                    let target_col = require_col(&batch, TARGET_COL).map_err(run_err_to_query)?;
                    for phys in active_rows(&batch) {
                        let Some(target_text) =
                            raw_string_at(target_col, phys).map_err(run_err_to_query)?
                        else {
                            continue;
                        };
                        let Ok(target_id) = TraceId::from_str(&target_text) else {
                            continue;
                        };
                        if !targets_c.contains(&target_id) {
                            continue;
                        }
                        let bytes = raw_fixed16_at(id_col, phys).ok_or_else(bad_id)?;
                        let id = TraceId::from_bytes(bytes).map_err(|_| bad_id())?;
                        if !targets_c.contains(&id) {
                            found.insert(id);
                        }
                    }
                }
            }
            Ok(found.into_iter().collect())
        })
        .await?;

    Ok(found)
}

/// Distinct non-null `session_id`s of the spans belonging to `traces`, sorted.
pub(crate) async fn session_ids_of_traces(
    ctx: &EngineContext,
    traces: &HashSet<TraceId>,
    timeout: Duration,
) -> Result<Vec<String>, RunError> {
    if traces.is_empty() {
        return Ok(Vec::new());
    }
    let spans_raw = spans_raw_schema()?;
    let tid_idx = spans_raw
        .index_of(spans::TRACE_ID)
        .ok_or_else(|| QueryError::internal("trace_id missing from spans schema"))
        .map_err(query_err_to_run)?;
    let sid_idx = spans_raw
        .index_of(spans::SESSION_ID)
        .ok_or_else(|| QueryError::internal("session_id missing from spans schema"))
        .map_err(query_err_to_run)?;

    let sid_col = col_expr(&spans_raw, spans::SESSION_ID).map_err(query_err_to_run)?;
    let mut filters = vec![Expr::Unary {
        op: UnaryOp::IsNotNull,
        expr: Box::new(sid_col),
        data_type: DataType::Bool,
    }];
    if let Some(vf) = ctx.visible_from {
        let start_col = col_expr(&spans_raw, spans::START_TIME).map_err(query_err_to_run)?;
        filters.push(Expr::Binary {
            left: Box::new(start_col),
            op: BinaryOp::GtEq,
            right: Box::new(Expr::Literal(ScalarValue::TimestampNs {
                value: vf,
                utc: true,
            })),
            data_type: DataType::Bool,
        });
    }

    let id_lookups = if traces.len() <= 1_000 {
        vec![IdLookupHint::Fixed16 {
            column: spans::TRACE_ID.to_owned(),
            values: traces.iter().map(|t| *t.as_bytes()).collect(),
        }]
    } else {
        Vec::new()
    };

    let spec = ScanSpec {
        projection: Some(vec![tid_idx, sid_idx]),
        map_keys: Vec::new(),
        time_range: TimeRangeHint {
            min_ns: ctx.visible_from,
            max_ns: None,
            around_ns: ctx.around,
        },
        id_lookups,
        filters,
        substrings: Vec::new(),
        sort_hints: Vec::new(),
        limit: None,
    };

    let ctx_c = ctx.clone();
    let traces_c = traces.clone();
    let (found, _) = ctx
        .run_with_ctx(timeout, move |exec_ctx| {
            let streams = ctx_c.spans.open_partitions(&spec, exec_ctx)?;
            if streams.is_empty() {
                return Ok(Vec::new());
            }
            let pool_c = Arc::clone(&exec_ctx.pool);
            let sinks = exec_run_pipeline(
                exec_ctx,
                streams,
                |_| Ok(Vec::new()),
                move |_| Ok(CollectSink::new(&pool_c)),
            )?;
            let mut found = BTreeSet::new();
            for sink in sinks {
                for batch in sink.into_batches() {
                    if batch.num_rows() == 0 {
                        continue;
                    }
                    let id_col = require_col(&batch, spans::TRACE_ID).map_err(run_err_to_query)?;
                    let sid_col =
                        require_col(&batch, spans::SESSION_ID).map_err(run_err_to_query)?;
                    for phys in active_rows(&batch) {
                        let Some(sid) = raw_string_at(sid_col, phys).map_err(run_err_to_query)?
                        else {
                            continue;
                        };
                        let bytes = raw_fixed16_at(id_col, phys).ok_or_else(bad_id)?;
                        let trace_id = TraceId::from_bytes(bytes).map_err(|_| bad_id())?;
                        if traces_c.contains(&trace_id) {
                            found.insert(sid);
                        }
                    }
                }
            }
            Ok(found.into_iter().collect())
        })
        .await?;

    Ok(found)
}

fn encode_trace_id_stream(
    streams: Vec<Box<dyn PartitionStream>>,
    exec_ctx: &ExecContext,
) -> Result<StreamSet, QueryError> {
    let out_schema =
        Schema::try_new_arc(vec![Field::new(traces::TRACE_ID, DataType::Utf8, false)])?;
    let col = Expr::Column {
        index: 0,
        name: traces::TRACE_ID.to_owned(),
        data_type: DataType::FixedBytes16,
    };
    let encoded = Expr::Call {
        func: cetra_query::ScalarFn::Encode,
        args: vec![col, Expr::Literal(ScalarValue::Utf8("hex".to_owned()))],
        data_type: DataType::Utf8,
    };
    let out = streams
        .into_iter()
        .map(|s| {
            let proj = ProjectOp::with_schema(
                vec![encoded.clone()],
                Arc::clone(&out_schema),
                exec_ctx.clone(),
            )?;
            Ok(
                Box::new(OperatorStream::new(s, vec![Box::new(proj)], exec_ctx))
                    as Box<dyn PartitionStream>,
            )
        })
        .collect::<Result<Vec<_>, QueryError>>()?;
    Ok(StreamSet {
        streams: out,
        schema: out_schema,
    })
}

fn col_expr(schema: &Schema, name: &str) -> Result<Expr, QueryError> {
    let idx = schema
        .index_of(name)
        .ok_or_else(|| QueryError::internal(format!("missing column {name} in schema")))?;
    Ok(Expr::Column {
        index: idx,
        name: name.to_owned(),
        data_type: schema.fields()[idx].data_type.clone(),
    })
}

fn bad_id() -> QueryError {
    QueryError::internal("a view returned an invalid trace id")
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::time::Duration;

    use bytesize::ByteSize;
    use cetra_core::ids::{ProjectId, TraceId};
    use cetra_core::names::{environment, metadata_key};
    use cetra_core::span::Span;

    use super::*;
    use crate::QueryOptions;
    use crate::config::{
        FlushConfig, MemtableConfig, QueryConfig, S3Config, StorageConfig, WalConfig,
    };
    use crate::reads::{
        FilterDeletionRequest, eval_trace_ids as oracle_eval_trace_ids,
        filter_trace_ids as oracle_filter_trace_ids,
        session_ids_of_traces as oracle_session_ids_of_traces,
        user_deletion_scope as oracle_user_deletion_scope,
    };
    use crate::test_support::{span_row, span_rows};
    use crate::views::{QueryContext, TimeWindow};

    const TIMEOUT: Duration = Duration::from_secs(30);

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

    fn id(trace: u16) -> TraceId {
        span_row(trace, 0, 0, 0).trace_id
    }

    #[tokio::test]
    async fn deletions_diff_parity_all_4_functions() {
        let dir = tempfile::tempdir().expect("tempdir");
        let storage = crate::Storage::open(test_config(dir.path()))
            .await
            .expect("open");
        let project: ProjectId = "018f6b4e-7c1a-7c1a-8c1a-000000000001".parse().expect("pid");

        let mut s1 = span_row(1, 0, 1_000, 100);
        s1.name = "chat".to_owned();
        s1.user_id = Some("u1".to_owned());
        s1.session_id = Some("sess-1".to_owned());

        let mut s2 = span_row(2, 0, 2_000, 200);
        s2.name = "tool".to_owned();
        s2.input = Some("refund please".to_owned());
        s2.user_id = Some("u1".to_owned());
        s2.session_id = Some("sess-2".to_owned());

        let mut s3 = span_row(3, 0, 3_000, 500);
        s3.name = "chat".to_owned();
        s3.user_id = Some("u1".to_owned());
        s3.session_id = Some("sess-3".to_owned());

        // Eval trace targeting trace 1 (uppercase hex) and another targeting trace 10 (chained)
        let mut e1 = span_row(10, 0, 4_000, 100);
        e1.environment = Some(environment::EVAL.to_owned());
        e1.metadata.insert(
            metadata_key::EVAL_TARGET_TRACE_ID.to_owned(),
            id(1).to_hex().to_uppercase(),
        );
        e1.user_id = Some("u1".to_owned());

        let mut e2 = span_row(11, 0, 5_000, 100);
        e2.environment = Some(environment::EVAL.to_owned());
        e2.metadata.insert(
            metadata_key::EVAL_TARGET_TRACE_ID.to_owned(),
            id(10).to_hex(),
        );

        let mut bad_eval = span_row(12, 0, 6_000, 100);
        bad_eval.environment = Some(environment::EVAL.to_owned());
        bad_eval.metadata.insert(
            metadata_key::EVAL_TARGET_TRACE_ID.to_owned(),
            "zz".to_owned(),
        );

        let spans: Vec<Span> = vec![s1, s2, s3, e1, e2, bad_eval];
        storage
            .append_spans(project, span_rows(&spans))
            .await
            .expect("append");

        let opts = QueryOptions::new(None, None);
        let pq = storage.query(project, opts).await.expect("pq");
        let oracle_ctx = QueryContext::new(pq, None).expect("oracle");
        let qe_ctx = EngineContext::from_storage(&storage, project, &opts).expect("qe");

        // 1. filter_trace_ids
        let win = TimeWindow::new(0, 10_000).expect("win");
        for (filter, search, include_eval) in [
            (Some("name = 'chat'"), None, false),
            (None, Some("refund"), false),
            (Some("environment = 'cetra-eval'"), None, false),
            (None, None, true),
        ] {
            let req = FilterDeletionRequest {
                filter,
                search,
                window: win,
                include_eval,
                include_experiments: false,
                limit: 100,
                timeout: TIMEOUT,
            };
            assert_eq!(
                filter_trace_ids(&qe_ctx, req).await.expect("qe"),
                oracle_filter_trace_ids(&oracle_ctx, req)
                    .await
                    .expect("oracle")
            );
        }

        // 2. user_deletion_scope (cutoff = 200 drops s3 with ingest_time = 500, includes eval e1!)
        assert_eq!(
            user_deletion_scope(&qe_ctx, "u1", 200, TIMEOUT)
                .await
                .expect("qe"),
            oracle_user_deletion_scope(&oracle_ctx, "u1", 200, TIMEOUT)
                .await
                .expect("oracle")
        );

        // 3. eval_trace_ids
        let targets: HashSet<TraceId> = [id(1)].into_iter().collect();
        assert_eq!(
            eval_trace_ids(&qe_ctx, &targets, TIMEOUT)
                .await
                .expect("qe"),
            oracle_eval_trace_ids(&oracle_ctx, &targets, TIMEOUT)
                .await
                .expect("oracle")
        );
        let chained: HashSet<TraceId> = [id(1), id(10)].into_iter().collect();
        assert_eq!(
            eval_trace_ids(&qe_ctx, &chained, TIMEOUT)
                .await
                .expect("qe"),
            oracle_eval_trace_ids(&oracle_ctx, &chained, TIMEOUT)
                .await
                .expect("oracle")
        );
        assert!(
            eval_trace_ids(&qe_ctx, &HashSet::new(), TIMEOUT)
                .await
                .expect("empty")
                .is_empty()
        );

        // 4. session_ids_of_traces
        let trace_set: HashSet<TraceId> = [id(1), id(2)].into_iter().collect();
        assert_eq!(
            session_ids_of_traces(&qe_ctx, &trace_set, TIMEOUT)
                .await
                .expect("qe"),
            oracle_session_ids_of_traces(&oracle_ctx, &trace_set, TIMEOUT)
                .await
                .expect("oracle")
        );
        assert!(
            session_ids_of_traces(&qe_ctx, &HashSet::new(), TIMEOUT)
                .await
                .expect("empty")
                .is_empty()
        );

        let _ = storage.shutdown().await;
    }
}
