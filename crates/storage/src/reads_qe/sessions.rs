//! Native query engine implementations of `session_list`, `session_detail`, and `session_extras`.

use std::collections::BTreeSet;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use cetra_column::{Batch, DataType, Field, ScalarValue, Schema, Selection};
use cetra_core::schema::{spans, traces};
use cetra_query::exec::hash_join::{HashJoinSpec, JoinType, execute_hash_join};
use cetra_query::exec::streaming::{FilterOp, ProjectOp, TopKSink};
use cetra_query::exec::{
    AggExpr, AggFunc, BatchSource, CollectSink, ExecContext, HashAgg, HashAggSpec, IdLookupHint,
    PartitionStream, ScanSource, ScanSpec, SortKey, TimeRangeHint,
    run_pipeline as exec_run_pipeline,
};
use cetra_query::{BinaryOp, Expr, FilterView, QueryError, ScalarFn};

use crate::query_error::RunError;
use crate::reads::{
    ListScope, MAX_PAGE, MAX_SESSION_TURNS, SESSION_ERROR_TRACE_COUNT, SESSION_FIRST_INPUT_PREVIEW,
    SESSION_LAST_OUTPUT_PREVIEW, SessionExtrasRequest, SessionListRequest, SessionSort, SortOrder,
};
use crate::reads_qe::columns::{internal, physical_row, raw_string_at, require_col};
use crate::reads_qe::context::{
    EngineContext, OperatorStream, query_err_to_run, run_err_to_query, spans_raw_schema,
    traces_raw_schema, traces_view_projection, traces_view_schema,
};
use crate::reads_qe::cursor::{self, QePage};
use crate::reads_qe::scope::{
    scoped_traces_spec, sessions_from_traces, sessions_from_traces_projection,
};
use crate::reads_qe::scores_join::{apply_compiled_filter, compile_optional_filter};

const LIST: &str = "sessions";
const TRACE_COUNT: &str = "trace_count";
const TIES: &[&str] = &[traces::SESSION_ID];
const ROOT_TRACE_ALIAS: &str = "__root_trace";
const ROOT_SPAN_ALIAS: &str = "__root_span";

/// One session, its turns, and those turns' root payloads (`cetra-column` native).
pub(crate) struct QeSessionDetailData {
    /// The sessions-view row (11 columns).
    pub(crate) session: Batch,
    /// Scoped traces, `start_time ASC, trace_id ASC`, at most [`MAX_SESSION_TURNS`].
    pub(crate) turns: Vec<Batch>,
    /// Root span `trace_id`, `input`, `input_format`, `output`, `output_format`.
    ///
    /// Turns without a root have no row.
    pub(crate) roots: Vec<Batch>,
    /// True when more than [`MAX_SESSION_TURNS`] traces were stored.
    pub(crate) truncated: bool,
}

impl fmt::Debug for QeSessionDetailData {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let turn_rows: usize = self.turns.iter().map(Batch::num_rows).sum();
        let root_rows: usize = self.roots.iter().map(Batch::num_rows).sum();
        f.debug_struct("QeSessionDetailData")
            .field("session_rows", &self.session.num_rows())
            .field("turns_batches", &self.turns.len())
            .field("turn_rows", &turn_rows)
            .field("roots_batches", &self.roots.len())
            .field("root_rows", &root_rows)
            .field("truncated", &self.truncated)
            .finish()
    }
}

fn session_sort_column(sort: SessionSort) -> &'static str {
    match sort {
        SessionSort::StartTime => traces::START_TIME,
        SessionSort::EndTime => traces::END_TIME,
        SessionSort::TraceCount => TRACE_COUNT,
        SessionSort::CostTotal => traces::COST_TOTAL,
    }
}

/// One page of sessions (`references_environment = false`, `FilterView::Sessions`).
pub(crate) async fn session_list(
    ctx: &EngineContext,
    request: SessionListRequest<'_>,
) -> Result<QePage, RunError> {
    if !(1..=MAX_PAGE).contains(&request.limit) {
        return Err(RunError::InvalidRequest(
            "limit must be between 1 and 1000".to_owned(),
        ));
    }
    let sort_col = session_sort_column(request.sort);
    let compiled = compile_optional_filter(FilterView::Sessions, request.filter)?;
    let cursor = match request.cursor {
        Some(text) => {
            let decoded = cursor::decode(text, LIST, sort_col, request.order)?;
            if decoded.tie_len() != 1 {
                return Err(RunError::InvalidRequest("invalid cursor".to_owned()));
            }
            Some(decoded)
        }
        None => None,
    };
    let mut spec = scoped_traces_spec(ctx, request.scope, false)?;
    let traces_raw = traces_raw_schema()?;
    spec.projection = Some(sessions_from_traces_projection(&traces_raw)?);

    let ctx_c = ctx.clone();
    let cursor_c = cursor.clone();
    let limit = request.limit;
    let order = request.order;

    let (batches, _) = ctx
        .run_with_ctx(request.timeout, move |exec_ctx| {
            let proj_schema = ctx_c.traces.projected_schema(&spec)?;
            let raw_streams = ctx_c.traces.open_partitions(&spec, exec_ctx)?;
            if raw_streams.is_empty() {
                return Ok(Vec::new());
            }
            let sessions = sessions_from_traces(raw_streams, &proj_schema, exec_ctx)?;
            let mut streams = sessions.streams;
            if let Some(filter) = &compiled {
                streams =
                    apply_compiled_filter(&ctx_c, exec_ctx, streams, &sessions.schema, filter)?;
            }
            if let Some(cur) = &cursor_c {
                let pred = cursor::after_expr(cur, &sessions.schema, sort_col, TIES)
                    .map_err(run_err_to_query)?;
                streams = streams
                    .into_iter()
                    .map(|s| {
                        let op = FilterOp::new(pred.clone(), exec_ctx.clone());
                        Box::new(OperatorStream::new(s, vec![Box::new(op)], exec_ctx))
                            as Box<dyn PartitionStream>
                    })
                    .collect();
            }
            if streams.is_empty() {
                return Ok(Vec::new());
            }
            let sort_keys = cursor::order_by(&sessions.schema, sort_col, TIES, order)
                .map_err(run_err_to_query)?;
            let fetch = limit.saturating_add(1);
            let schema_c = Arc::clone(&sessions.schema);
            let keys_c = sort_keys.clone();
            let ctx_sink = exec_ctx.clone();
            let sinks = exec_run_pipeline(
                exec_ctx,
                streams,
                |_| Ok(Vec::new()),
                move |_| {
                    Ok(TopKSink::new(
                        Arc::clone(&schema_c),
                        keys_c.clone(),
                        fetch,
                        0,
                        ctx_sink.clone(),
                    ))
                },
            )?;
            TopKSink::merge(sinks, sessions.schema, &sort_keys, 0, fetch, exec_ctx)
        })
        .await?;

    cursor::assemble(batches, request.limit, LIST, sort_col, request.order, TIES)
}

/// The session, its turns, and their root payloads. No time window.
pub(crate) async fn session_detail(
    ctx: &EngineContext,
    session_id: &str,
    include_eval: bool,
    include_experiments: bool,
    timeout: Duration,
) -> Result<Option<QeSessionDetailData>, RunError> {
    let scope = ListScope {
        window: None,
        include_eval,
        include_experiments,
    };
    let mut spec = scoped_traces_spec(ctx, scope, false)?;
    let traces_raw = traces_raw_schema()?;
    let session_col = col_expr(&traces_raw, traces::SESSION_ID).map_err(query_err_to_run)?;
    spec.filters.push(Expr::Binary {
        left: Box::new(session_col),
        op: BinaryOp::Eq,
        right: Box::new(Expr::Literal(ScalarValue::Utf8(session_id.to_owned()))),
        data_type: DataType::Bool,
    });
    spec.id_lookups.push(IdLookupHint::Utf8 {
        column: traces::SESSION_ID.to_owned(),
        values: vec![session_id.to_owned()],
    });

    let mut sess_spec = spec.clone();
    sess_spec.projection = Some(sessions_from_traces_projection(&traces_raw)?);

    let ctx_c = ctx.clone();
    let (detail, _) = ctx
        .run_with_ctx(timeout, move |exec_ctx| {
            // 1. Session aggregate row (1 row, or None if session does not exist in scope).
            let sess_proj_schema = ctx_c.traces.projected_schema(&sess_spec)?;
            let raw_streams = ctx_c.traces.open_partitions(&sess_spec, exec_ctx)?;
            if raw_streams.is_empty() {
                return Ok(None);
            }
            let sessions = sessions_from_traces(raw_streams, &sess_proj_schema, exec_ctx)?;
            let pool_c = Arc::clone(&exec_ctx.pool);
            let sess_sinks = exec_run_pipeline(
                exec_ctx,
                sessions.streams,
                |_| Ok(Vec::new()),
                move |_| Ok(CollectSink::new(&pool_c)),
            )?;
            let sess_batches: Vec<Batch> = sess_sinks
                .into_iter()
                .flat_map(CollectSink::into_batches)
                .collect();
            let Some(session) = single_row(&sess_batches, exec_ctx)? else {
                return Ok(None);
            };

            // 2. Turns ordered (start_time ASC, trace_id ASC), capped at MAX_SESSION_TURNS + 1.
            let view_schema = traces_view_schema().map_err(run_err_to_query)?;
            let turn_sort_keys = cursor::order_by(
                &view_schema,
                traces::START_TIME,
                &[traces::TRACE_ID],
                SortOrder::Asc,
            )
            .map_err(run_err_to_query)?;

            let turn_raw_streams = ctx_c.traces.open_partitions(&spec, exec_ctx)?;
            let fetch = MAX_SESSION_TURNS + 1;
            let turn_batches = if turn_raw_streams.is_empty() {
                Vec::new()
            } else {
                let raw_c = Arc::clone(&traces_raw);
                let view_c = Arc::clone(&view_schema);
                let keys_c = turn_sort_keys.clone();
                let ctx_chain = exec_ctx.clone();
                let ctx_sink = exec_ctx.clone();
                let sinks = exec_run_pipeline(
                    exec_ctx,
                    turn_raw_streams,
                    move |_| {
                        let proj = traces_view_projection(&raw_c, ctx_chain.clone())?;
                        Ok(vec![Box::new(proj)])
                    },
                    move |_| {
                        Ok(TopKSink::new(
                            Arc::clone(&view_c),
                            keys_c.clone(),
                            fetch,
                            0,
                            ctx_sink.clone(),
                        ))
                    },
                )?;
                TopKSink::merge(
                    sinks,
                    Arc::clone(&view_schema),
                    &turn_sort_keys,
                    0,
                    fetch,
                    exec_ctx,
                )?
            };
            let (turns, truncated) = cap_rows(turn_batches, MAX_SESSION_TURNS)?;

            // 3. Root span payloads via inner join on (trace_id, root_span_id) = (trace_id, span_id)
            // pushing <= 1,000 turn trace_id/span_id values as IdLookupHints.
            let roots = load_root_payloads(&ctx_c, exec_ctx, &view_schema, &turns)?;

            Ok(Some(QeSessionDetailData {
                session,
                turns,
                roots,
                truncated,
            }))
        })
        .await?;

    Ok(detail)
}

fn load_root_payloads(
    ctx: &EngineContext,
    exec_ctx: &ExecContext,
    turns_schema: &Arc<Schema>,
    turns: &[Batch],
) -> Result<Vec<Batch>, QueryError> {
    let mut trace_ids = BTreeSet::<[u8; 16]>::new();
    let mut span_ids = BTreeSet::<[u8; 8]>::new();
    for batch in turns {
        if batch.num_rows() == 0 {
            continue;
        }
        let trace_col = require_col(batch, traces::TRACE_ID).map_err(run_err_to_query)?;
        let root_col = require_col(batch, traces::ROOT_SPAN_ID).map_err(run_err_to_query)?;
        for active_row in 0..batch.num_rows() {
            let phys = physical_row(batch, active_row).map_err(run_err_to_query)?;
            let Some(root_hex) = raw_string_at(root_col, phys).map_err(run_err_to_query)? else {
                continue;
            };
            let Some(trace_hex) = raw_string_at(trace_col, phys).map_err(run_err_to_query)? else {
                continue;
            };
            let Ok(trace_bytes) = hex::decode(&trace_hex) else {
                continue;
            };
            let Ok(span_bytes) = hex::decode(&root_hex) else {
                continue;
            };
            if let (Ok(t), Ok(s)) = (trace_bytes.try_into(), span_bytes.try_into()) {
                trace_ids.insert(t);
                span_ids.insert(s);
            }
        }
    }
    if trace_ids.is_empty() || span_ids.is_empty() {
        return Ok(Vec::new());
    }

    let spans_raw = spans_raw_schema().map_err(run_err_to_query)?;
    let trace_vec: Vec<[u8; 16]> = trace_ids.into_iter().collect();
    let span_vec: Vec<[u8; 8]> = span_ids.into_iter().collect();

    let proj_names = [
        spans::TRACE_ID,
        spans::SPAN_ID,
        spans::INPUT,
        spans::INPUT_FORMAT,
        spans::OUTPUT,
        spans::OUTPUT_FORMAT,
    ];
    let mut proj_indices = Vec::with_capacity(proj_names.len());
    for name in proj_names {
        let idx = spans_raw.index_of(name).ok_or_else(|| {
            QueryError::internal(format!("missing column {name} in spans schema"))
        })?;
        proj_indices.push(idx);
    }

    let trace_raw_col = col_expr(&spans_raw, spans::TRACE_ID)?;
    let span_raw_col = col_expr(&spans_raw, spans::SPAN_ID)?;
    let mut filters = vec![
        Expr::InList {
            expr: Box::new(trace_raw_col),
            list: trace_vec
                .iter()
                .map(|b| Expr::Literal(ScalarValue::FixedBytes16(*b)))
                .collect(),
            negated: false,
            redacted: false,
        },
        Expr::InList {
            expr: Box::new(span_raw_col),
            list: span_vec
                .iter()
                .map(|b| Expr::Literal(ScalarValue::FixedBytes8(*b)))
                .collect(),
            negated: false,
            redacted: false,
        },
    ];
    if let Some(vf) = ctx.visible_from {
        let start_col = col_expr(&spans_raw, spans::START_TIME)?;
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

    let span_spec = ScanSpec {
        projection: Some(proj_indices),
        map_keys: Vec::new(),
        time_range: TimeRangeHint {
            min_ns: ctx.visible_from,
            max_ns: None,
            around_ns: ctx.around,
        },
        id_lookups: vec![
            IdLookupHint::Fixed16 {
                column: spans::TRACE_ID.to_owned(),
                values: trace_vec,
            },
            IdLookupHint::Fixed8 {
                column: spans::SPAN_ID.to_owned(),
                values: span_vec,
            },
        ],
        filters,
        substrings: Vec::new(),
        sort_hints: Vec::new(),
        limit: None,
    };

    let raw_span_streams = ctx.spans.open_partitions(&span_spec, exec_ctx)?;
    if raw_span_streams.is_empty() {
        return Ok(Vec::new());
    }
    let span_proj_schema = ctx.spans.projected_schema(&span_spec)?;

    let right_schema = Schema::try_new_arc(vec![
        Field::new(ROOT_TRACE_ALIAS, DataType::Utf8, false),
        Field::new(ROOT_SPAN_ALIAS, DataType::Utf8, false),
        Field::new(spans::INPUT, DataType::Utf8, true),
        Field::new(spans::INPUT_FORMAT, DataType::Utf8, true),
        Field::new(spans::OUTPUT, DataType::Utf8, true),
        Field::new(spans::OUTPUT_FORMAT, DataType::Utf8, true),
    ])?;

    let right_exprs = vec![
        encode_hex_expr(col_expr(&span_proj_schema, spans::TRACE_ID)?),
        encode_hex_expr(col_expr(&span_proj_schema, spans::SPAN_ID)?),
        col_expr(&span_proj_schema, spans::INPUT)?,
        col_expr(&span_proj_schema, spans::INPUT_FORMAT)?,
        col_expr(&span_proj_schema, spans::OUTPUT)?,
        col_expr(&span_proj_schema, spans::OUTPUT_FORMAT)?,
    ];
    let right_streams: Vec<Box<dyn PartitionStream>> = raw_span_streams
        .into_iter()
        .map(|s| {
            let proj = ProjectOp::with_schema(
                right_exprs.clone(),
                Arc::clone(&right_schema),
                exec_ctx.clone(),
            )?;
            Ok(
                Box::new(OperatorStream::new(s, vec![Box::new(proj)], exec_ctx))
                    as Box<dyn PartitionStream>,
            )
        })
        .collect::<Result<Vec<_>, QueryError>>()?;

    let turns_src = BatchSource::new(Arc::clone(turns_schema), turns.to_vec())?;
    let left_streams = turns_src.open_partitions(&ScanSpec::default(), exec_ctx)?;
    if left_streams.is_empty() {
        return Ok(Vec::new());
    }

    let left_trace = col_expr(turns_schema, traces::TRACE_ID)?;
    let left_root = col_expr(turns_schema, traces::ROOT_SPAN_ID)?;
    let right_trace = col_expr(&right_schema, ROOT_TRACE_ALIAS)?;
    let right_span = col_expr(&right_schema, ROOT_SPAN_ALIAS)?;

    let joined_stream = execute_hash_join(
        left_streams,
        Arc::clone(turns_schema),
        right_streams,
        Arc::clone(&right_schema),
        HashJoinSpec {
            join_type: JoinType::Inner,
            left_keys: vec![left_trace, left_root],
            right_keys: vec![right_trace, right_span],
            null_equals_null: false,
            residual_filter: None,
        },
        exec_ctx,
    )?;

    let joined_schema = cetra_query::exec::hash_join::join_output_schema(
        JoinType::Inner,
        turns_schema,
        &right_schema,
    );
    let out_schema = Schema::try_new_arc(vec![
        Field::new(traces::TRACE_ID, DataType::Utf8, false),
        Field::new(spans::INPUT, DataType::Utf8, true),
        Field::new(spans::INPUT_FORMAT, DataType::Utf8, true),
        Field::new(spans::OUTPUT, DataType::Utf8, true),
        Field::new(spans::OUTPUT_FORMAT, DataType::Utf8, true),
    ])?;
    let out_exprs = vec![
        col_expr(&joined_schema, traces::TRACE_ID)?,
        col_expr(&joined_schema, spans::INPUT)?,
        col_expr(&joined_schema, spans::INPUT_FORMAT)?,
        col_expr(&joined_schema, spans::OUTPUT)?,
        col_expr(&joined_schema, spans::OUTPUT_FORMAT)?,
    ];
    let pool_c = Arc::clone(&exec_ctx.pool);
    let ctx_chain = exec_ctx.clone();
    let out_sinks = exec_run_pipeline(
        exec_ctx,
        vec![joined_stream],
        move |_| {
            let proj = ProjectOp::with_schema(
                out_exprs.clone(),
                Arc::clone(&out_schema),
                ctx_chain.clone(),
            )?;
            Ok(vec![Box::new(proj)])
        },
        move |_| Ok(CollectSink::new(&pool_c)),
    )?;
    Ok(out_sinks
        .into_iter()
        .flat_map(CollectSink::into_batches)
        .filter(|b| b.num_rows() > 0)
        .collect())
}

/// Per-session extras (`error_trace_count`, `first_input_preview`, `last_output_preview`) in 1 pass
/// using NULL-preserving [`AggFunc::FirstByOrder`].
pub(crate) async fn session_extras(
    ctx: &EngineContext,
    request: SessionExtrasRequest<'_>,
) -> Result<Vec<Batch>, RunError> {
    if request.session_ids.len() > MAX_PAGE {
        return Err(RunError::InvalidRequest("too many sessions".to_owned()));
    }
    if request.session_ids.is_empty() {
        return Ok(Vec::new());
    }

    let mut spec = scoped_traces_spec(ctx, request.scope, false)?;
    let traces_raw = traces_raw_schema()?;
    let distinct_ids: Vec<String> = request
        .session_ids
        .iter()
        .map(|s| (*s).to_owned())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let session_raw_col = col_expr(&traces_raw, traces::SESSION_ID).map_err(query_err_to_run)?;
    spec.filters.push(Expr::InList {
        expr: Box::new(session_raw_col),
        list: distinct_ids
            .iter()
            .map(|id| Expr::Literal(ScalarValue::Utf8(id.clone())))
            .collect(),
        negated: false,
        redacted: false,
    });
    spec.id_lookups.push(IdLookupHint::Utf8 {
        column: traces::SESSION_ID.to_owned(),
        values: distinct_ids,
    });

    let needed = [
        traces::SESSION_ID,
        traces::START_TIME,
        traces::TRACE_ID,
        traces::STATUS,
        traces::INPUT_PREVIEW,
        traces::OUTPUT_PREVIEW,
    ];
    let mut proj_indices = Vec::with_capacity(needed.len());
    for name in needed {
        let idx = traces_raw
            .index_of(name)
            .ok_or_else(|| internal(format!("missing column {name} in traces schema")))?;
        proj_indices.push(idx);
    }
    spec.projection = Some(proj_indices);

    let ctx_c = ctx.clone();
    let (batches, _) = ctx
        .run_with_ctx(request.timeout, move |exec_ctx| {
            let proj_schema = ctx_c.traces.projected_schema(&spec)?;
            let streams = ctx_c.traces.open_partitions(&spec, exec_ctx)?;
            if streams.is_empty() {
                return Ok(Vec::new());
            }

            let session_col = col_expr(&proj_schema, traces::SESSION_ID)?;
            let start_col = col_expr(&proj_schema, traces::START_TIME)?;
            let trace_col = col_expr(&proj_schema, traces::TRACE_ID)?;
            let status_col = col_expr(&proj_schema, traces::STATUS)?;
            let input_preview_col = col_expr(&proj_schema, traces::INPUT_PREVIEW)?;
            let output_preview_col = col_expr(&proj_schema, traces::OUTPUT_PREVIEW)?;

            let error_flag = Expr::Case {
                operand: None,
                when_then: vec![(
                    Expr::Binary {
                        left: Box::new(status_col),
                        op: BinaryOp::Eq,
                        right: Box::new(Expr::Literal(ScalarValue::Utf8("error".to_owned()))),
                        data_type: DataType::Bool,
                    },
                    Expr::Literal(ScalarValue::I64(1)),
                )],
                else_expr: Some(Box::new(Expr::Literal(ScalarValue::I64(0)))),
                data_type: DataType::I64,
            };

            let first_order = vec![
                SortKey {
                    expr: start_col.clone(),
                    descending: false,
                    nulls_first: false,
                },
                SortKey {
                    expr: trace_col.clone(),
                    descending: false,
                    nulls_first: false,
                },
            ];
            let last_order = vec![
                SortKey {
                    expr: start_col,
                    descending: true,
                    nulls_first: true,
                },
                SortKey {
                    expr: trace_col,
                    descending: true,
                    nulls_first: true,
                },
            ];

            let agg_spec = HashAggSpec {
                group_by: vec![(traces::SESSION_ID.to_owned(), session_col)],
                aggs: vec![
                    AggExpr {
                        func: AggFunc::Sum(error_flag),
                        filter: None,
                        output_name: SESSION_ERROR_TRACE_COUNT.to_owned(),
                    },
                    AggExpr {
                        func: AggFunc::FirstByOrder {
                            value: input_preview_col,
                            order_by: first_order,
                        },
                        filter: None,
                        output_name: SESSION_FIRST_INPUT_PREVIEW.to_owned(),
                    },
                    AggExpr {
                        func: AggFunc::FirstByOrder {
                            value: output_preview_col,
                            order_by: last_order,
                        },
                        filter: None,
                        output_name: SESSION_LAST_OUTPUT_PREVIEW.to_owned(),
                    },
                ],
            };

            let spec_c = agg_spec.clone();
            let schema_c = Arc::clone(&proj_schema);
            let ctx_sink = exec_ctx.clone();
            let sinks = exec_run_pipeline(
                exec_ctx,
                streams,
                |_| Ok(Vec::new()),
                move |_| HashAgg::new(spec_c.clone(), Arc::clone(&schema_c), ctx_sink.clone()),
            )?;
            let out = HashAgg::merge_sinks(sinks, &agg_spec, &proj_schema, exec_ctx)?;
            Ok(out.into_iter().filter(|b| b.num_rows() > 0).collect())
        })
        .await?;

    Ok(batches)
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

fn encode_hex_expr(inner: Expr) -> Expr {
    if inner.data_type() == DataType::Utf8 {
        return inner;
    }
    Expr::Call {
        func: ScalarFn::Encode,
        args: vec![inner, Expr::Literal(ScalarValue::Utf8("hex".to_owned()))],
        data_type: DataType::Utf8,
    }
}

fn single_row(batches: &[Batch], exec_ctx: &ExecContext) -> Result<Option<Batch>, QueryError> {
    let non_empty: Vec<&Batch> = batches.iter().filter(|b| b.num_rows() > 0).collect();
    let Some(first) = non_empty.first() else {
        return Ok(None);
    };
    let schema = Arc::clone(first.schema());
    let mut res = exec_ctx.pool.register("session_single_row", false);
    let merged = Batch::concat(schema, &non_empty, Some(&mut res))?;
    res.free();
    if merged.num_rows() == 0 {
        return Ok(None);
    }
    if merged.num_rows() == 1 {
        return Ok(Some(merged));
    }
    Ok(Some(slice_active_batch(&merged, 0, 1)?))
}

fn cap_rows(batches: Vec<Batch>, limit: usize) -> Result<(Vec<Batch>, bool), QueryError> {
    let total: usize = batches.iter().map(Batch::num_rows).sum();
    if total <= limit {
        let non_empty = batches.into_iter().filter(|b| b.num_rows() > 0).collect();
        return Ok((non_empty, false));
    }
    let mut kept = Vec::new();
    let mut remaining = limit;
    for batch in batches {
        if remaining == 0 {
            break;
        }
        let n = batch.num_rows();
        if n == 0 {
            continue;
        }
        if n <= remaining {
            remaining -= n;
            kept.push(batch);
        } else {
            kept.push(slice_active_batch(&batch, 0, remaining)?);
            remaining = 0;
        }
    }
    Ok((kept, true))
}

fn slice_active_batch(batch: &Batch, start: usize, len: usize) -> Result<Batch, QueryError> {
    let sliced = match batch.selection() {
        Selection::All => batch.slice(start, len),
        Selection::Mask(bm) => {
            let indices: Vec<u32> = bm
                .iter_ones()
                .skip(start)
                .take(len)
                .map(|i| i as u32)
                .collect();
            batch.with_selection(Selection::Indices(indices))
        }
        Selection::Indices(idx) => {
            batch.with_selection(Selection::Indices(idx[start..start + len].to_vec()))
        }
    }?;
    Ok(sliced)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use arrow::array::AsArray;
    use arrow::json::{ArrayWriter, WriterBuilder};
    use arrow::record_batch::RecordBatch;
    use bytesize::ByteSize;
    use cetra_core::enums::SpanStatus;
    use cetra_core::ids::ProjectId;
    use cetra_core::names::environment;
    use cetra_core::score::{ScoreTarget, ScoreValue};
    use serde_json::Value;

    use super::*;
    use crate::QueryOptions;
    use crate::column_bridge::column_to_record_batch;
    use crate::config::{
        FlushConfig, MemtableConfig, QueryConfig, S3Config, StorageConfig, WalConfig,
    };
    use crate::reads::{
        ListScope, SortOrder, session_detail as oracle_session_detail,
        session_extras as oracle_session_extras, session_list as oracle_session_list,
    };
    use crate::test_support::{
        dataset_scores, dataset_spans, score_row, score_rows, span_row, span_rows,
    };
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

    fn batches_to_json(batches: &[Batch]) -> Vec<Value> {
        let rbs: Vec<RecordBatch> = batches
            .iter()
            .filter(|b| b.num_rows() > 0)
            .map(|b| column_to_record_batch(b, None).expect("bridge"))
            .collect();
        record_batches_to_json(&rbs)
    }

    fn record_batches_to_json(batches: &[RecordBatch]) -> Vec<Value> {
        if batches.iter().all(|b| b.num_rows() == 0) {
            return Vec::new();
        }
        let mut buf = Vec::new();
        let mut writer: ArrayWriter<&mut Vec<u8>> = WriterBuilder::new()
            .with_explicit_nulls(true)
            .with_timestamp_tz_format("%Y-%m-%dT%H:%M:%S%.9fZ".to_owned())
            .with_timestamp_format("%Y-%m-%dT%H:%M:%S%.9f".to_owned())
            .build(&mut buf);
        for b in batches {
            if b.num_rows() > 0 {
                writer.write(b).expect("write");
            }
        }
        writer.finish().expect("finish");
        drop(writer);
        serde_json::from_slice(&buf).expect("json")
    }

    fn assert_json_rows_eq(actual: &[Value], expected: &[Value]) {
        assert_eq!(actual.len(), expected.len(), "row count mismatch");
        for (i, (a, e)) in actual.iter().zip(expected).enumerate() {
            assert!(
                values_match(a, e),
                "row {i} mismatch:\nactual:   {a}\nexpected: {e}"
            );
        }
    }

    fn values_match(actual: &Value, expected: &Value) -> bool {
        match (actual, expected) {
            (Value::Number(l), Value::Number(r)) => match (l.as_i64(), r.as_i64()) {
                (Some(l), Some(r)) => l == r,
                _ => {
                    let lf = l.as_f64().expect("lf");
                    let rf = r.as_f64().expect("rf");
                    (lf - rf).abs() < 1e-12
                }
            },
            (Value::Array(l), Value::Array(r)) => {
                l.len() == r.len() && l.iter().zip(r).all(|(a, b)| values_match(a, b))
            }
            (Value::Object(l), Value::Object(r)) => {
                l.len() == r.len()
                    && l.iter()
                        .all(|(k, v)| r.get(k).is_some_and(|rv| values_match(v, rv)))
            }
            _ => actual == expected,
        }
    }

    async fn setup_storage_with_dataset() -> (tempfile::TempDir, crate::Storage, ProjectId) {
        let dir = tempfile::tempdir().expect("tempdir");
        let storage = crate::Storage::open(test_config(dir.path()))
            .await
            .expect("open");
        let spans = dataset_spans();
        let scores = dataset_scores();
        let project = spans[0].project_id;
        storage
            .append_spans(project, span_rows(&spans))
            .await
            .expect("spans");
        storage
            .append_scores(project, score_rows(&scores))
            .await
            .expect("scores");
        (dir, storage, project)
    }

    #[tokio::test]
    async fn session_list_diff_parity_all_sorts_orders_filters_and_cursors() {
        let (_dir, storage, project) = setup_storage_with_dataset().await;
        let opts = QueryOptions::new(None, None);
        let pq = storage.query(project, opts).await.expect("pq");
        let oracle_ctx = QueryContext::new(pq, None).expect("oracle");
        let qe_ctx = EngineContext::from_storage(&storage, project, &opts).expect("qe");

        for include_eval in [false, true] {
            for sort in [
                SessionSort::StartTime,
                SessionSort::EndTime,
                SessionSort::TraceCount,
                SessionSort::CostTotal,
            ] {
                for order in [SortOrder::Asc, SortOrder::Desc] {
                    let mut cursor: Option<String> = None;
                    loop {
                        let req = SessionListRequest {
                            filter: None,
                            scope: ListScope {
                                window: None,
                                include_eval,
                                include_experiments: false,
                            },
                            sort,
                            order,
                            limit: 1,
                            cursor: cursor.as_deref(),
                            timeout: TIMEOUT,
                        };
                        let oracle_page = oracle_session_list(&oracle_ctx, req.clone())
                            .await
                            .expect("oracle");
                        let qe_page = session_list(&qe_ctx, req).await.expect("qe");
                        assert_json_rows_eq(
                            &batches_to_json(&qe_page.batches),
                            &record_batches_to_json(&oracle_page.batches),
                        );
                        assert_eq!(qe_page.next_cursor, oracle_page.next_cursor);
                        match qe_page.next_cursor {
                            Some(next) => cursor = Some(next),
                            None => break,
                        }
                    }
                }
            }
        }

        // With Session filter including score() function
        for filter in [
            "trace_count >= 2",
            "has_score('resolved')",
            "environment = 'cetra-eval'",
        ] {
            let req = SessionListRequest {
                filter: Some(filter),
                scope: ListScope {
                    window: None,
                    include_eval: false,
                    include_experiments: false,
                },
                sort: SessionSort::StartTime,
                order: SortOrder::Desc,
                limit: 50,
                cursor: None,
                timeout: TIMEOUT,
            };
            let oracle_res = oracle_session_list(&oracle_ctx, req.clone()).await;
            let qe_res = session_list(&qe_ctx, req).await;
            match (oracle_res, qe_res) {
                (Ok(o), Ok(q)) => {
                    assert_json_rows_eq(
                        &batches_to_json(&q.batches),
                        &record_batches_to_json(&o.batches),
                    );
                    assert_eq!(q.next_cursor, o.next_cursor);
                }
                (Err(RunError::Invalid(_)), Err(RunError::Invalid(_))) => {}
                (o, q) => panic!("mismatch for filter {filter}: {o:?} vs {q:?}"),
            }
        }

        let _ = storage.shutdown().await;
    }

    #[tokio::test]
    async fn session_detail_diff_parity_and_missing() {
        let (_dir, storage, project) = setup_storage_with_dataset().await;
        let opts = QueryOptions::new(None, None);
        let pq = storage.query(project, opts).await.expect("pq");
        let oracle_ctx = QueryContext::new(pq, None).expect("oracle");
        let qe_ctx = EngineContext::from_storage(&storage, project, &opts).expect("qe");

        for include_eval in [false, true] {
            let o = oracle_session_detail(&oracle_ctx, "sess-001", include_eval, false, TIMEOUT)
                .await
                .expect("oracle")
                .expect("some");
            let q = session_detail(&qe_ctx, "sess-001", include_eval, false, TIMEOUT)
                .await
                .expect("qe")
                .expect("some");
            assert_eq!(q.truncated, o.truncated);
            assert_json_rows_eq(
                &batches_to_json(&[q.session]),
                &record_batches_to_json(&[o.session]),
            );
            assert_json_rows_eq(
                &batches_to_json(&q.turns),
                &record_batches_to_json(&o.turns),
            );

            let mut q_roots = batches_to_json(&q.roots);
            let mut o_roots = record_batches_to_json(&o.roots);
            q_roots.sort_by(|a, b| a["trace_id"].as_str().cmp(&b["trace_id"].as_str()));
            o_roots.sort_by(|a, b| a["trace_id"].as_str().cmp(&b["trace_id"].as_str()));
            assert_json_rows_eq(&q_roots, &o_roots);
        }

        assert!(
            session_detail(&qe_ctx, "does-not-exist", false, false, TIMEOUT)
                .await
                .expect("qe")
                .is_none()
        );

        let _ = storage.shutdown().await;
    }

    #[tokio::test]
    async fn session_extras_preserves_null_on_earliest_and_latest_trace() {
        let dir = tempfile::tempdir().expect("tempdir");
        let storage = crate::Storage::open(test_config(dir.path()))
            .await
            .expect("open");
        let project: ProjectId = "018f6b4e-7c1a-7c1a-8c1a-000000000001".parse().expect("pid");

        // Trace 1 (earliest): input_preview = None (MUST remain NULL, not fall back to trace 2!)
        let mut t1 = span_row(1, 0, 1_000, 1_000);
        t1.session_id = Some("s1".to_owned());
        t1.status = SpanStatus::Error;
        t1.input_preview = None;
        t1.output_preview = Some("out1".to_owned());

        // Trace 2 (middle): both previews present
        let mut t2 = span_row(2, 0, 2_000, 2_000);
        t2.session_id = Some("s1".to_owned());
        t2.input_preview = Some("in2".to_owned());
        t2.output_preview = Some("out2".to_owned());

        // Trace 3 (latest): output_preview = None (MUST remain NULL, not fall back to trace 2!)
        let mut t3 = span_row(3, 0, 3_000, 3_000);
        t3.session_id = Some("s1".to_owned());
        t3.input_preview = Some("in3".to_owned());
        t3.output_preview = None;

        // Trace 4 (another session with both present + eval excluded trace)
        let mut t4 = span_row(4, 0, 4_000, 4_000);
        t4.session_id = Some("s2".to_owned());
        t4.input_preview = Some("in4".to_owned());
        t4.output_preview = Some("out4".to_owned());

        let mut t5 = span_row(5, 0, 5_000, 5_000);
        t5.session_id = Some("s2".to_owned());
        t5.environment = Some(environment::EVAL.to_owned());
        t5.output_preview = Some("eval-out".to_owned());

        storage
            .append_spans(project, span_rows(&[t1, t2, t3, t4, t5]))
            .await
            .expect("append");

        let opts = QueryOptions::new(None, None);
        let pq = storage.query(project, opts).await.expect("pq");
        let oracle_ctx = QueryContext::new(pq, None).expect("oracle");
        let qe_ctx = EngineContext::from_storage(&storage, project, &opts).expect("qe");

        let req = SessionExtrasRequest {
            scope: ListScope {
                window: None,
                include_eval: false,
                include_experiments: false,
            },
            session_ids: &["s1", "s2", "missing"],
            timeout: TIMEOUT,
        };
        let o_batches = oracle_session_extras(&oracle_ctx, req.clone())
            .await
            .expect("oracle");
        let q_batches = session_extras(&qe_ctx, req).await.expect("qe");

        let mut o_rows = record_batches_to_json(&o_batches);
        let mut q_rows = batches_to_json(&q_batches);
        o_rows.sort_by(|a, b| a["session_id"].as_str().cmp(&b["session_id"].as_str()));
        q_rows.sort_by(|a, b| a["session_id"].as_str().cmp(&b["session_id"].as_str()));
        assert_json_rows_eq(&q_rows, &o_rows);
        assert_eq!(q_rows.len(), 2);
        assert_eq!(q_rows[0]["session_id"], "s1");
        assert_eq!(q_rows[0][SESSION_ERROR_TRACE_COUNT], 1);
        assert!(q_rows[0][SESSION_FIRST_INPUT_PREVIEW].is_null());
        assert!(q_rows[0][SESSION_LAST_OUTPUT_PREVIEW].is_null());
        assert_eq!(q_rows[1]["session_id"], "s2");
        assert_eq!(q_rows[1][SESSION_FIRST_INPUT_PREVIEW], "in4");
        assert_eq!(q_rows[1][SESSION_LAST_OUTPUT_PREVIEW], "out4");

        // Verify limit validation and empty session_ids
        let too_many = vec!["s"; MAX_PAGE + 1];
        assert!(matches!(
            session_extras(
                &qe_ctx,
                SessionExtrasRequest {
                    scope: ListScope {
                        window: None,
                        include_eval: false,
                        include_experiments: false,
                    },
                    session_ids: &too_many,
                    timeout: TIMEOUT,
                }
            )
            .await,
            Err(RunError::InvalidRequest(_))
        ));
        assert!(
            session_extras(
                &qe_ctx,
                SessionExtrasRequest {
                    scope: ListScope {
                        window: None,
                        include_eval: false,
                        include_experiments: false,
                    },
                    session_ids: &[],
                    timeout: TIMEOUT,
                }
            )
            .await
            .expect("empty")
            .is_empty()
        );

        let _ = storage.shutdown().await;
    }

    #[tokio::test]
    async fn session_list_cost_null_and_window_parity() {
        let dir = tempfile::tempdir().expect("tempdir");
        let storage = crate::Storage::open(test_config(dir.path()))
            .await
            .expect("open");
        let project: ProjectId = "018f6b4e-7c1a-7c1a-8c1a-000000000001".parse().expect("pid");

        let mut s1 = span_row(1, 0, 1_000, 1_000);
        s1.session_id = Some("s1".to_owned());
        s1.cost_total = Some(0.1);
        let mut s2 = span_row(2, 0, 2_000, 2_000);
        s2.session_id = Some("s2".to_owned());
        s2.cost_total = None;

        let mut sc = score_row(1, 1, 1_000, 1_000, 1_000, false, "resolved");
        sc.target = ScoreTarget::Session {
            session_id: "s1".to_owned(),
        };
        sc.value = ScoreValue::Boolean(true);

        storage
            .append_spans(project, span_rows(&[s1, s2]))
            .await
            .expect("append spans");
        storage
            .append_scores(project, score_rows(&[sc]))
            .await
            .expect("append scores");

        let opts = QueryOptions::new(None, None);
        let pq = storage.query(project, opts).await.expect("pq");
        let oracle_ctx = QueryContext::new(pq, None).expect("oracle");
        let qe_ctx = EngineContext::from_storage(&storage, project, &opts).expect("qe");

        for order in [SortOrder::Asc, SortOrder::Desc] {
            let req = SessionListRequest {
                filter: None,
                scope: ListScope {
                    window: Some(TimeWindow::new(0, 5_000).expect("win")),
                    include_eval: false,
                    include_experiments: false,
                },
                sort: SessionSort::CostTotal,
                order,
                limit: 10,
                cursor: None,
                timeout: TIMEOUT,
            };
            let o = oracle_session_list(&oracle_ctx, req.clone())
                .await
                .expect("oracle");
            let q = session_list(&qe_ctx, req).await.expect("qe");
            let q_json = batches_to_json(&q.batches);
            let o_json = record_batches_to_json(&o.batches);
            assert_json_rows_eq(&q_json, &o_json);
            assert_eq!(q_json[0]["session_id"], "s1");
            assert!(q_json[1]["cost_total"].is_null());
        }

        // Verify filter with score() works on session_list
        let req = SessionListRequest {
            filter: Some("score('resolved') = 1"),
            scope: ListScope {
                window: None,
                include_eval: false,
                include_experiments: false,
            },
            sort: SessionSort::StartTime,
            order: SortOrder::Desc,
            limit: 10,
            cursor: None,
            timeout: TIMEOUT,
        };
        let o = oracle_session_list(&oracle_ctx, req.clone())
            .await
            .expect("oracle");
        let q = session_list(&qe_ctx, req).await.expect("qe");
        assert_json_rows_eq(
            &batches_to_json(&q.batches),
            &record_batches_to_json(&o.batches),
        );
        // Ensure we used arrow array helper types cleanly in test
        let rb = column_to_record_batch(&q.batches[0], None).expect("rb");
        assert_eq!(rb.column(0).as_string::<i32>().value(0), "s1");

        let _ = storage.shutdown().await;
    }
}
