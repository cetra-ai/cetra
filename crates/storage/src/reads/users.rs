//! Native query engine implementations of `user_list`, `user_detail`, and `user_extras`.

use std::collections::BTreeSet;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use cetra_column::{Batch, DataType, Field, ScalarValue, Schema};
use cetra_core::enums::{ScoreDataType, ScoreSource};
use cetra_core::schema::{scores, traces};
use cetra_query::exec::hash_agg::execute_hash_agg_streams;
use cetra_query::exec::hash_join::{HashJoinSpec, JoinType, execute_hash_join, join_output_schema};
use cetra_query::exec::sort_window::ExternalSort;
use cetra_query::exec::streaming::{FilterOp, ProjectOp, TopKSink};
use cetra_query::exec::{
    AggExpr, AggFunc, BatchSource, CollectSink, ControlFlow, ExecContext, HashAgg, HashAggSpec,
    IdLookupHint, PartitionStream, PushSink, ScanSource, ScanSpec, SortKey, TimeRangeHint,
    run_pipeline as exec_run_pipeline,
};
use cetra_query::{
    BinaryOp, CastMode, CompiledFilter, Expr, FilterView, QueryError, ScalarFn, UnaryOp,
};

use crate::query_error::RunError;
use crate::reads::context::{
    EngineContext, OperatorStream, populate_id_lookups, query_err_to_run,
    rewrite_view_expr_for_raw, run_err_to_query, scores_raw_schema, traces_raw_schema,
    traces_view_projection, traces_view_schema,
};
use crate::reads::cursor::{self, Page};
use crate::reads::scope::{
    StreamSet, scoped_traces_spec, sessions_from_traces, sessions_from_traces_projection,
};
use crate::reads::scores_join::{apply_compiled_filter, compile_optional_filter};
use crate::reads::{ListScope, MAX_PAGE, SortOrder};
use crate::views::{QueryContext, TimeWindow};

const LIST: &str = "users";
const FIRST_SEEN: &str = "first_seen";
const LAST_SEEN: &str = "last_seen";
const TRACE_COUNT: &str = "trace_count";
const SESSION_COUNT: &str = "session_count";
const ERROR_RATE: &str = "error_rate";
const TRACES_PER_SESSION: &str = "traces_per_session";
const TIES: &[&str] = &[traces::USER_ID];

/// Name of the `traces` error count of a user: traces whose status is `error`.
pub const ERROR_TRACE_COUNT: &str = "error_trace_count";

/// Number of activity buckets of a user row (spec 09 `EndUserSummary.activity`).
pub const ACTIVITY_BUCKETS: usize = 24;

/// [`ACTIVITY_BUCKETS`] as the `i64` the bucket arithmetic uses.
const BUCKETS_I64: i64 = ACTIVITY_BUCKETS as i64;

/// Most `user.` metadata keys returned per user.
pub const MAX_USER_METADATA_KEYS: usize = 20;

const BUCKET: &str = "bucket";
const SCORE_AVG: &str = "avg";
const SCORE_COUNT: &str = "count";
const SCORE_TRACE_ALIAS: &str = "__u_trace_id";

/// Columns of a user list, in order.
pub const USER_COLUMNS: &[&str] = &[
    traces::USER_ID,
    FIRST_SEEN,
    LAST_SEEN,
    TRACE_COUNT,
    SESSION_COUNT,
    traces::ERROR_COUNT,
    ERROR_TRACE_COUNT,
    traces::TOTAL_TOKENS,
    traces::COST_TOTAL,
    ERROR_RATE,
    TRACES_PER_SESSION,
];

/// Most sessions [`user_detail`] returns.
pub const MAX_USER_SESSIONS: usize = 100;

/// Column a user list sorts by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserSort {
    /// `last_seen`.
    LastSeen,
    /// `trace_count`.
    TraceCount,
    /// `cost_total`.
    CostTotal,
    /// `session_count`.
    SessionCount,
    /// `total_tokens`.
    TotalTokens,
    /// `error_count`.
    ErrorCount,
    /// `first_seen`.
    FirstSeen,
    /// `error_rate`: `error_trace_count / trace_count`.
    ErrorRate,
    /// `traces_per_session`: `trace_count / session_count`, null without sessions.
    TracesPerSession,
}

/// One page of users.
#[derive(Debug, Clone)]
pub struct UserListRequest<'a> {
    /// Filter text over `traces`, applied before grouping. Empty means no filter.
    pub filter: Option<&'a str>,
    /// Window and default exclusions, applied to the traces before grouping.
    pub scope: ListScope,
    /// Sort column.
    pub sort: UserSort,
    /// Sort direction. Nulls are last.
    pub order: SortOrder,
    /// Rows to return, from 1 to [`MAX_PAGE`].
    pub limit: usize,
    /// Cursor from the previous page.
    pub cursor: Option<&'a str>,
    /// Time allowed for planning and running.
    pub timeout: Duration,
}

/// Inputs of [`user_extras`].
#[derive(Debug, Clone)]
pub struct UserExtrasRequest<'a> {
    /// Same filter as the list request.
    pub filter: Option<&'a str>,
    /// Same scope as the list request (for `user_detail`: no window).
    pub scope: ListScope,
    /// Users to compute, at most [`MAX_PAGE`]. Empty runs no query.
    pub user_ids: &'a [&'a str],
    /// Window the activity buckets split.
    pub activity: TimeWindow,
    /// Time allowed for all three queries together.
    pub timeout: Duration,
}

/// One user and the whole sessions that contain the user's traces.
pub struct UserDetailData {
    /// The aggregate row. Columns are [`USER_COLUMNS`].
    pub user: Batch,
    /// Sessions of the user's traces, `start_time DESC, session_id DESC`.
    pub sessions: Vec<Batch>,
}

impl fmt::Debug for UserDetailData {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let session_rows: usize = self.sessions.iter().map(Batch::num_rows).sum();
        f.debug_struct("UserDetailData")
            .field("user_rows", &self.user.num_rows())
            .field("sessions_batches", &self.sessions.len())
            .field("session_rows", &session_rows)
            .finish()
    }
}

/// Results of [`user_extras`].
#[derive(Default)]
pub struct UserExtras {
    /// `user_id` Utf8, `bucket` I64, `trace_count` I64.
    pub activity: Vec<Batch>,
    /// `user_id` Utf8, `name` Utf8, `data_type` Utf8, `avg` F64, `count` I64,
    /// sorted by `user_id`, `name`, `data_type`.
    pub scores: Vec<Batch>,
    /// `user_id` Utf8 and `metadata` `MapUtf8`: one row per user, from their latest trace.
    pub metadata: Vec<Batch>,
}

impl fmt::Debug for UserExtras {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let activity_rows: usize = self.activity.iter().map(Batch::num_rows).sum();
        let scores_rows: usize = self.scores.iter().map(Batch::num_rows).sum();
        let metadata_rows: usize = self.metadata.iter().map(Batch::num_rows).sum();
        f.debug_struct("UserExtras")
            .field("activity_rows", &activity_rows)
            .field("scores_rows", &scores_rows)
            .field("metadata_rows", &metadata_rows)
            .finish()
    }
}

fn user_sort_column(sort: UserSort) -> &'static str {
    match sort {
        UserSort::LastSeen => LAST_SEEN,
        UserSort::TraceCount => TRACE_COUNT,
        UserSort::CostTotal => traces::COST_TOTAL,
        UserSort::SessionCount => SESSION_COUNT,
        UserSort::TotalTokens => traces::TOTAL_TOKENS,
        UserSort::ErrorCount => traces::ERROR_COUNT,
        UserSort::FirstSeen => FIRST_SEEN,
        UserSort::ErrorRate => ERROR_RATE,
        UserSort::TracesPerSession => TRACES_PER_SESSION,
    }
}

fn user_output_schema() -> Result<Arc<Schema>, QueryError> {
    let ts = DataType::timestamp_utc();
    Ok(Schema::try_new_arc(vec![
        Field::new(USER_COLUMNS[0], DataType::Utf8, true),
        Field::new(USER_COLUMNS[1], ts.clone(), true),
        Field::new(USER_COLUMNS[2], ts, true),
        Field::new(USER_COLUMNS[3], DataType::I64, false),
        Field::new(USER_COLUMNS[4], DataType::I64, false),
        Field::new(USER_COLUMNS[5], DataType::I64, false),
        Field::new(USER_COLUMNS[6], DataType::I64, false),
        Field::new(USER_COLUMNS[7], DataType::I64, true),
        Field::new(USER_COLUMNS[8], DataType::F64, true),
        Field::new(USER_COLUMNS[9], DataType::F64, true),
        Field::new(USER_COLUMNS[10], DataType::F64, true),
    ])?)
}

/// One page of users.
pub async fn user_list(ctx: &QueryContext, request: UserListRequest<'_>) -> Result<Page, RunError> {
    let ctx = ctx.engine();
    if !(1..=MAX_PAGE).contains(&request.limit) {
        return Err(RunError::InvalidRequest(
            "limit must be between 1 and 1000".to_owned(),
        ));
    }
    let sort_col = user_sort_column(request.sort);
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
    let compiled = compile_optional_filter(FilterView::Traces, request.filter)?;
    let references_environment = compiled.as_ref().is_some_and(|f| f.references_environment);
    let spec = scoped_traces_spec(ctx, request.scope, references_environment)?;

    let ctx_c = ctx.clone();
    let cursor_c = cursor.clone();
    let limit = request.limit;
    let order = request.order;

    let (batches, _) = ctx
        .run_with_ctx(request.timeout, move |exec_ctx| {
            let filtered_set =
                open_filtered_user_traces(&ctx_c, exec_ctx, spec, compiled.as_ref(), None)?;
            if filtered_set.streams.is_empty() {
                return Ok(Vec::new());
            }
            let agg_streams =
                aggregate_users(filtered_set.streams, &filtered_set.schema, exec_ctx)?;
            let out_schema = user_output_schema()?;
            let mut filtered_streams = agg_streams;
            if let Some(cur) = &cursor_c {
                let pred = cursor::after_expr(cur, &out_schema, sort_col, TIES)
                    .map_err(run_err_to_query)?;
                filtered_streams = filtered_streams
                    .into_iter()
                    .map(|s| {
                        let op = FilterOp::new(pred.clone(), exec_ctx.clone());
                        Box::new(OperatorStream::new(s, vec![Box::new(op)], exec_ctx))
                            as Box<dyn PartitionStream>
                    })
                    .collect();
            }
            if filtered_streams.is_empty() {
                return Ok(Vec::new());
            }
            let sort_keys =
                cursor::order_by(&out_schema, sort_col, TIES, order).map_err(run_err_to_query)?;
            let fetch = limit + 1;
            let schema_c = Arc::clone(&out_schema);
            let keys_c = sort_keys.clone();
            let ctx_sink = exec_ctx.clone();
            let sinks = exec_run_pipeline(
                exec_ctx,
                filtered_streams,
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
            TopKSink::merge(sinks, out_schema, &sort_keys, 0, fetch, exec_ctx)
        })
        .await?;

    cursor::assemble(batches, request.limit, LIST, sort_col, request.order, TIES)
}

/// The user and the whole sessions of their traces (`references_environment = false`).
pub async fn user_detail(
    ctx: &QueryContext,
    user_id: &str,
    scope: ListScope,
    timeout: Duration,
) -> Result<Option<UserDetailData>, RunError> {
    let ctx = ctx.engine();
    let base_spec = scoped_traces_spec(ctx, scope, false)?;
    let traces_raw = traces_raw_schema()?;
    let user_raw_col = col_expr(&traces_raw, traces::USER_ID).map_err(query_err_to_run)?;

    let mut user_spec = base_spec.clone();
    user_spec.filters.push(Expr::Binary {
        left: Box::new(user_raw_col),
        op: BinaryOp::Eq,
        right: Box::new(Expr::Literal(ScalarValue::Utf8(user_id.to_owned()))),
        data_type: DataType::Bool,
    });
    user_spec.id_lookups.push(IdLookupHint::Utf8 {
        column: traces::USER_ID.to_owned(),
        values: vec![user_id.to_owned()],
    });

    let needed = [
        traces::USER_ID,
        traces::START_TIME,
        traces::END_TIME,
        traces::SESSION_ID,
        traces::ERROR_COUNT,
        traces::STATUS,
        traces::TOTAL_TOKENS,
        traces::COST_TOTAL,
    ];
    let mut proj_indices = Vec::with_capacity(needed.len());
    for name in needed {
        let idx = traces_raw
            .index_of(name)
            .ok_or_else(|| crate::reads::columns::internal(format!("missing {name}")))?;
        proj_indices.push(idx);
    }
    user_spec.projection = Some(proj_indices);

    let ctx_c = ctx.clone();
    let (detail, _) = ctx
        .run_with_ctx(timeout, move |exec_ctx| {
            let proj_schema = ctx_c.traces.projected_schema(&user_spec)?;
            let user_streams = ctx_c.traces.open_partitions(&user_spec, exec_ctx)?;
            if user_streams.is_empty() {
                return Ok(None);
            }
            let agg_streams = aggregate_users(user_streams, &proj_schema, exec_ctx)?;
            let pool_c = Arc::clone(&exec_ctx.pool);
            let user_sinks = exec_run_pipeline(
                exec_ctx,
                agg_streams,
                |_| Ok(Vec::new()),
                move |_| Ok(CollectSink::new(&pool_c)),
            )?;
            let user_batches: Vec<Batch> = user_sinks
                .into_iter()
                .flat_map(CollectSink::into_batches)
                .collect();
            let Some(user) = single_row(&user_batches, exec_ctx)? else {
                return Ok(None);
            };

            // Collect distinct non-null session_ids of this user's traces in scope.
            let mut sid_spec = user_spec.clone();
            let sid_idx = traces_raw
                .index_of(traces::SESSION_ID)
                .ok_or_else(|| QueryError::internal("session_id missing from traces schema"))?;
            sid_spec.projection = Some(vec![sid_idx]);
            let sid_col = col_expr(&traces_raw, traces::SESSION_ID)?;
            sid_spec.filters.push(Expr::Unary {
                op: UnaryOp::IsNotNull,
                expr: Box::new(sid_col),
                data_type: DataType::Bool,
            });
            let sid_streams = ctx_c.traces.open_partitions(&sid_spec, exec_ctx)?;
            let sid_schema = ctx_c.traces.projected_schema(&sid_spec)?;
            let sid_agg_spec = HashAggSpec {
                group_by: vec![(
                    traces::SESSION_ID.to_owned(),
                    col_expr(&sid_schema, traces::SESSION_ID)?,
                )],
                aggs: Vec::new(),
            };
            let distinct_sid_stream =
                execute_hash_agg_streams(sid_streams, sid_schema, sid_agg_spec, exec_ctx)?;
            let pool_c = Arc::clone(&exec_ctx.pool);
            let sid_sinks = exec_run_pipeline(
                exec_ctx,
                vec![distinct_sid_stream],
                |_| Ok(Vec::new()),
                move |_| Ok(CollectSink::new(&pool_c)),
            )?;
            let sid_batches: Vec<Batch> = sid_sinks
                .into_iter()
                .flat_map(CollectSink::into_batches)
                .filter(|b| b.num_rows() > 0)
                .collect();
            if sid_batches.is_empty() {
                return Ok(Some(UserDetailData {
                    user,
                    sessions: Vec::new(),
                }));
            }
            let sid_only_schema = Arc::clone(sid_batches[0].schema());
            let sid_src = BatchSource::new(Arc::clone(&sid_only_schema), sid_batches)?;
            let right_streams = sid_src.open_partitions(&ScanSpec::default(), exec_ctx)?;

            // Left semi-join all scoped traces against those session_ids, then sessions_from_traces.
            let mut all_spec = base_spec;
            let all_sid_col = col_expr(&traces_raw, traces::SESSION_ID)?;
            all_spec.filters.push(Expr::Unary {
                op: UnaryOp::IsNotNull,
                expr: Box::new(all_sid_col),
                data_type: DataType::Bool,
            });
            all_spec.projection =
                Some(sessions_from_traces_projection(&traces_raw).map_err(run_err_to_query)?);
            let all_schema = ctx_c.traces.projected_schema(&all_spec)?;
            let all_streams = ctx_c.traces.open_partitions(&all_spec, exec_ctx)?;
            if all_streams.is_empty() {
                return Ok(Some(UserDetailData {
                    user,
                    sessions: Vec::new(),
                }));
            }
            let joined = execute_hash_join(
                all_streams,
                Arc::clone(&all_schema),
                right_streams,
                Arc::clone(&sid_only_schema),
                HashJoinSpec {
                    join_type: JoinType::LeftSemi,
                    left_keys: vec![col_expr(&all_schema, traces::SESSION_ID)?],
                    right_keys: vec![col_expr(&sid_only_schema, traces::SESSION_ID)?],
                    null_equals_null: false,
                    residual_filter: None,
                },
                exec_ctx,
            )?;

            let sessions = sessions_from_traces(vec![joined], &all_schema, exec_ctx)?;
            if sessions.streams.is_empty() {
                return Ok(Some(UserDetailData {
                    user,
                    sessions: Vec::new(),
                }));
            }
            let sort_keys = cursor::order_by(
                &sessions.schema,
                traces::START_TIME,
                &[traces::SESSION_ID],
                SortOrder::Desc,
            )
            .map_err(run_err_to_query)?;
            let schema_c = Arc::clone(&sessions.schema);
            let keys_c = sort_keys.clone();
            let ctx_sink = exec_ctx.clone();
            let sess_sinks = exec_run_pipeline(
                exec_ctx,
                sessions.streams,
                |_| Ok(Vec::new()),
                move |_| {
                    Ok(TopKSink::new(
                        Arc::clone(&schema_c),
                        keys_c.clone(),
                        MAX_USER_SESSIONS,
                        0,
                        ctx_sink.clone(),
                    ))
                },
            )?;
            let session_batches = TopKSink::merge(
                sess_sinks,
                sessions.schema,
                &sort_keys,
                0,
                MAX_USER_SESSIONS,
                exec_ctx,
            )?;
            let sessions = session_batches
                .into_iter()
                .filter(|b| b.num_rows() > 0)
                .collect();
            Ok(Some(UserDetailData { user, sessions }))
        })
        .await?;

    Ok(detail)
}

/// Per-user extras of a page of users (activity, score averages, latest metadata).
pub async fn user_extras(
    ctx: &QueryContext,
    request: UserExtrasRequest<'_>,
) -> Result<UserExtras, RunError> {
    let ctx = ctx.engine();
    if request.user_ids.len() > MAX_PAGE {
        return Err(RunError::InvalidRequest("too many users".to_owned()));
    }
    if request.user_ids.is_empty() {
        return Ok(UserExtras::default());
    }
    let compiled = compile_optional_filter(FilterView::Traces, request.filter)?;
    let references_environment = compiled.as_ref().is_some_and(|f| f.references_environment);
    let base_spec = scoped_traces_spec(ctx, request.scope, references_environment)?;
    let distinct_users: Vec<String> = request
        .user_ids
        .iter()
        .map(|id| (*id).to_owned())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let activity_window = request.activity;

    let ctx_c = ctx.clone();
    let (extras, _) = ctx
        .run_with_ctx(request.timeout, move |exec_ctx| {
            // 1. Shared scan for `activity` and `metadata` (`[user_id, start_time, trace_id, metadata]`).
            let base_set = open_user_extras_base(
                &ctx_c,
                exec_ctx,
                base_spec.clone(),
                compiled.as_ref(),
                &distinct_users,
                true,
            )?;
            if base_set.streams.is_empty() {
                return Ok(UserExtras::default());
            }
            let base_streams = base_set.streams;
            let base_schema = base_set.schema;

            let (activity_spec, metadata_spec) = extras_agg_specs(&base_schema, activity_window)?;
            let act_spec_c = activity_spec.clone();
            let meta_spec_c = metadata_spec.clone();
            let base_schema_c = Arc::clone(&base_schema);
            let ctx_sink = exec_ctx.clone();
            let fan_sinks = exec_run_pipeline(
                exec_ctx,
                base_streams,
                |_| Ok(Vec::new()),
                move |_| {
                    Ok(ExtrasFanSink {
                        activity: HashAgg::new(
                            act_spec_c.clone(),
                            Arc::clone(&base_schema_c),
                            ctx_sink.clone(),
                        )?,
                        metadata: HashAgg::new(
                            meta_spec_c.clone(),
                            Arc::clone(&base_schema_c),
                            ctx_sink.clone(),
                        )?,
                    })
                },
            )?;
            let mut act_sinks = Vec::with_capacity(fan_sinks.len());
            let mut meta_sinks = Vec::with_capacity(fan_sinks.len());
            for s in fan_sinks {
                act_sinks.push(s.activity);
                meta_sinks.push(s.metadata);
            }
            let activity = HashAgg::merge_sinks(act_sinks, &activity_spec, &base_schema, exec_ctx)?
                .into_iter()
                .filter(|b| b.num_rows() > 0)
                .collect();
            let metadata =
                HashAgg::merge_sinks(meta_sinks, &metadata_spec, &base_schema, exec_ctx)?
                    .into_iter()
                    .filter(|b| b.num_rows() > 0)
                    .collect();

            // 2. Scores average per `(user_id, name, data_type)` excluding `source = 'issue'`.
            let scores = compute_user_scores(
                &ctx_c,
                exec_ctx,
                base_spec,
                compiled.as_ref(),
                &distinct_users,
            )?;

            Ok(UserExtras {
                activity,
                scores,
                metadata,
            })
        })
        .await?;

    Ok(extras)
}

struct ExtrasFanSink {
    activity: HashAgg,
    metadata: HashAgg,
}

impl PushSink for ExtrasFanSink {
    fn consume(&mut self, batch: Batch) -> Result<ControlFlow, QueryError> {
        let f1 = self.activity.consume(batch.clone())?;
        let f2 = self.metadata.consume(batch)?;
        if f1 == ControlFlow::Stop && f2 == ControlFlow::Stop {
            Ok(ControlFlow::Stop)
        } else {
            Ok(ControlFlow::Continue)
        }
    }

    fn finish(&mut self) -> Result<(), QueryError> {
        self.activity.finish()?;
        self.metadata.finish()?;
        Ok(())
    }
}

fn open_filtered_user_traces(
    ctx: &EngineContext,
    exec_ctx: &ExecContext,
    mut spec: ScanSpec,
    compiled: Option<&CompiledFilter>,
    user_ids: Option<&[String]>,
) -> Result<StreamSet, QueryError> {
    let traces_raw = traces_raw_schema().map_err(run_err_to_query)?;
    let user_raw_col = col_expr(&traces_raw, traces::USER_ID)?;

    if let Some(ids) = user_ids {
        spec.filters.push(Expr::InList {
            expr: Box::new(user_raw_col),
            list: ids
                .iter()
                .map(|id| Expr::Literal(ScalarValue::Utf8(id.clone())))
                .collect(),
            negated: false,
            redacted: false,
        });
        spec.id_lookups.push(IdLookupHint::Utf8 {
            column: traces::USER_ID.to_owned(),
            values: ids.to_vec(),
        });
    } else {
        spec.filters.push(Expr::Unary {
            op: UnaryOp::IsNotNull,
            expr: Box::new(user_raw_col),
            data_type: DataType::Bool,
        });
    }

    let needs_score_join = compiled.is_some_and(|f| !f.scores.is_empty());
    if !needs_score_join {
        if let Some(filter) = compiled {
            let rewritten = rewrite_view_expr_for_raw(
                &filter.expr,
                &traces_raw,
                &[traces::TRACE_ID, traces::ROOT_SPAN_ID],
                true,
            )?;
            spec.filters.push(rewritten);
            populate_id_lookups(&mut spec);
        }
        let needed = [
            traces::USER_ID,
            traces::START_TIME,
            traces::END_TIME,
            traces::SESSION_ID,
            traces::ERROR_COUNT,
            traces::STATUS,
            traces::TOTAL_TOKENS,
            traces::COST_TOTAL,
        ];
        let mut proj_indices = Vec::with_capacity(needed.len());
        for name in needed {
            let idx = traces_raw
                .index_of(name)
                .ok_or_else(|| QueryError::internal(format!("missing {name} in traces schema")))?;
            proj_indices.push(idx);
        }
        spec.projection = Some(proj_indices);
        let proj_schema = ctx.traces.projected_schema(&spec)?;
        let streams = ctx.traces.open_partitions(&spec, exec_ctx)?;
        return Ok(StreamSet {
            streams,
            schema: proj_schema,
        });
    }

    let raw_streams = ctx.traces.open_partitions(&spec, exec_ctx)?;
    if raw_streams.is_empty() {
        let view_schema = traces_view_schema().map_err(run_err_to_query)?;
        return Ok(StreamSet {
            streams: Vec::new(),
            schema: view_schema,
        });
    }
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
    let Some(filter) = compiled else {
        return Ok(StreamSet {
            streams: view_streams,
            schema: view_schema,
        });
    };
    let filtered = apply_compiled_filter(ctx, exec_ctx, view_streams, &view_schema, filter)?;
    Ok(StreamSet {
        streams: filtered,
        schema: view_schema,
    })
}

fn aggregate_users(
    streams: Vec<Box<dyn PartitionStream>>,
    input_schema: &Arc<Schema>,
    exec_ctx: &ExecContext,
) -> Result<Vec<Box<dyn PartitionStream>>, QueryError> {
    let user_col = col_expr(input_schema, traces::USER_ID)?;
    let start_col = col_expr(input_schema, traces::START_TIME)?;
    let end_col = col_expr(input_schema, traces::END_TIME)?;
    let session_col = col_expr(input_schema, traces::SESSION_ID)?;
    let error_col = col_expr(input_schema, traces::ERROR_COUNT)?;
    let status_col = col_expr(input_schema, traces::STATUS)?;
    let tokens_col = col_expr(input_schema, traces::TOTAL_TOKENS)?;
    let cost_col = col_expr(input_schema, traces::COST_TOTAL)?;

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

    let agg_spec = HashAggSpec {
        group_by: vec![(traces::USER_ID.to_owned(), user_col)],
        aggs: vec![
            AggExpr {
                func: AggFunc::Min(start_col),
                filter: None,
                output_name: FIRST_SEEN.to_owned(),
            },
            AggExpr {
                func: AggFunc::Max(end_col),
                filter: None,
                output_name: LAST_SEEN.to_owned(),
            },
            AggExpr {
                func: AggFunc::Count(Expr::Literal(ScalarValue::I64(1))),
                filter: None,
                output_name: TRACE_COUNT.to_owned(),
            },
            AggExpr {
                func: AggFunc::CountDistinct(session_col),
                filter: None,
                output_name: SESSION_COUNT.to_owned(),
            },
            AggExpr {
                func: AggFunc::Sum(error_col),
                filter: None,
                output_name: traces::ERROR_COUNT.to_owned(),
            },
            AggExpr {
                func: AggFunc::Sum(error_flag),
                filter: None,
                output_name: ERROR_TRACE_COUNT.to_owned(),
            },
            AggExpr {
                func: AggFunc::Sum(tokens_col),
                filter: None,
                output_name: traces::TOTAL_TOKENS.to_owned(),
            },
            AggExpr {
                func: AggFunc::SumCostDecimal(cost_col),
                filter: None,
                output_name: traces::COST_TOTAL.to_owned(),
            },
        ],
    };

    let agg_stream =
        execute_hash_agg_streams(streams, Arc::clone(input_schema), agg_spec, exec_ctx)?;

    let ts = DataType::timestamp_utc();
    let agg_user = col_at(0, traces::USER_ID, DataType::Utf8);
    let agg_first = col_at(1, FIRST_SEEN, ts.clone());
    let agg_last = col_at(2, LAST_SEEN, ts);
    let agg_trace_count = col_at(3, TRACE_COUNT, DataType::I64);
    let agg_session_count = col_at(4, SESSION_COUNT, DataType::I64);
    let agg_error_count = col_at(5, traces::ERROR_COUNT, DataType::I64);
    let agg_error_traces = col_at(6, ERROR_TRACE_COUNT, DataType::I64);
    let agg_total_tokens = col_at(7, traces::TOTAL_TOKENS, DataType::I64);
    let agg_cost_total = col_at(8, traces::COST_TOTAL, DataType::cost_decimal());

    let coalesced_errors = coalesce_expr(agg_error_count, Expr::Literal(ScalarValue::I64(0)));
    let coalesced_err_traces = coalesce_expr(agg_error_traces, Expr::Literal(ScalarValue::I64(0)));

    let trace_count_f64 = Expr::Cast {
        expr: Box::new(agg_trace_count.clone()),
        target: DataType::F64,
        mode: CastMode::Strict,
    };
    let session_count_f64 = Expr::Cast {
        expr: Box::new(agg_session_count.clone()),
        target: DataType::F64,
        mode: CastMode::Strict,
    };
    let err_traces_f64 = Expr::Cast {
        expr: Box::new(coalesced_err_traces.clone()),
        target: DataType::F64,
        mode: CastMode::Strict,
    };

    let error_rate = Expr::Binary {
        left: Box::new(err_traces_f64),
        op: BinaryOp::Div,
        right: Box::new(trace_count_f64.clone()),
        data_type: DataType::F64,
    };
    let non_zero_sessions = Expr::Call {
        func: ScalarFn::Nullif,
        args: vec![session_count_f64, Expr::Literal(ScalarValue::F64(0.0))],
        data_type: DataType::F64,
    };
    let traces_per_session = Expr::Binary {
        left: Box::new(trace_count_f64),
        op: BinaryOp::Div,
        right: Box::new(non_zero_sessions),
        data_type: DataType::F64,
    };

    let out_schema = user_output_schema()?;
    let proj_exprs = vec![
        agg_user,
        agg_first,
        agg_last,
        agg_trace_count,
        agg_session_count,
        coalesced_errors,
        coalesced_err_traces,
        agg_total_tokens,
        Expr::Cast {
            expr: Box::new(agg_cost_total),
            target: DataType::F64,
            mode: CastMode::Strict,
        },
        error_rate,
        traces_per_session,
    ];
    let project_op = ProjectOp::with_schema(proj_exprs, out_schema, exec_ctx.clone())?;
    Ok(vec![Box::new(OperatorStream::new(
        agg_stream,
        vec![Box::new(project_op)],
        exec_ctx,
    )) as Box<dyn PartitionStream>])
}

fn open_user_extras_base(
    ctx: &EngineContext,
    exec_ctx: &ExecContext,
    mut spec: ScanSpec,
    compiled: Option<&CompiledFilter>,
    user_ids: &[String],
    include_metadata: bool,
) -> Result<StreamSet, QueryError> {
    let traces_raw = traces_raw_schema().map_err(run_err_to_query)?;
    let user_raw_col = col_expr(&traces_raw, traces::USER_ID)?;
    spec.filters.push(Expr::InList {
        expr: Box::new(user_raw_col),
        list: user_ids
            .iter()
            .map(|id| Expr::Literal(ScalarValue::Utf8(id.clone())))
            .collect(),
        negated: false,
        redacted: false,
    });
    spec.id_lookups.push(IdLookupHint::Utf8 {
        column: traces::USER_ID.to_owned(),
        values: user_ids.to_vec(),
    });

    let needs_score_join = compiled.is_some_and(|f| !f.scores.is_empty());
    if !needs_score_join {
        if let Some(filter) = compiled {
            let rewritten = rewrite_view_expr_for_raw(
                &filter.expr,
                &traces_raw,
                &[traces::TRACE_ID, traces::ROOT_SPAN_ID],
                true,
            )?;
            spec.filters.push(rewritten);
            populate_id_lookups(&mut spec);
        }
        let mut needed = vec![traces::USER_ID, traces::START_TIME, traces::TRACE_ID];
        if include_metadata {
            needed.push(traces::METADATA);
        }
        let mut proj_indices = Vec::with_capacity(needed.len());
        for name in needed {
            let idx = traces_raw
                .index_of(name)
                .ok_or_else(|| QueryError::internal(format!("missing {name} in traces schema")))?;
            proj_indices.push(idx);
        }
        spec.projection = Some(proj_indices);
        let proj_schema = ctx.traces.projected_schema(&spec)?;
        let streams = ctx.traces.open_partitions(&spec, exec_ctx)?;
        return Ok(StreamSet {
            streams,
            schema: proj_schema,
        });
    }

    let raw_streams = ctx.traces.open_partitions(&spec, exec_ctx)?;
    if raw_streams.is_empty() {
        let view_schema = traces_view_schema().map_err(run_err_to_query)?;
        return Ok(StreamSet {
            streams: Vec::new(),
            schema: view_schema,
        });
    }
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
    let filtered = if let Some(filter) = compiled {
        apply_compiled_filter(ctx, exec_ctx, view_streams, &view_schema, filter)?
    } else {
        view_streams
    };

    let mut out_fields = vec![
        Field::new(traces::USER_ID, DataType::Utf8, true),
        Field::new(traces::START_TIME, DataType::timestamp_utc(), false),
        Field::new(traces::TRACE_ID, DataType::Utf8, false),
    ];
    let mut out_exprs = vec![
        col_expr(&view_schema, traces::USER_ID)?,
        col_expr(&view_schema, traces::START_TIME)?,
        col_expr(&view_schema, traces::TRACE_ID)?,
    ];
    if include_metadata {
        out_fields.push(Field::new(traces::METADATA, DataType::MapUtf8, false));
        out_exprs.push(col_expr(&view_schema, traces::METADATA)?);
    }
    let out_schema = Schema::try_new_arc(out_fields)?;
    let projected: Vec<Box<dyn PartitionStream>> = filtered
        .into_iter()
        .map(|s| {
            let proj = ProjectOp::with_schema(
                out_exprs.clone(),
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
        streams: projected,
        schema: out_schema,
    })
}

fn extras_agg_specs(
    base_schema: &Schema,
    activity_window: TimeWindow,
) -> Result<(HashAggSpec, HashAggSpec), QueryError> {
    let user_col = col_expr(base_schema, traces::USER_ID)?;
    let start_col = col_expr(base_schema, traces::START_TIME)?;
    let trace_col = col_expr(base_schema, traces::TRACE_ID)?;
    let metadata_col = col_expr(base_schema, traces::METADATA)?;

    // Signed truncating `(start_time - from) / width` (NOT `div_euclid`).
    let width = activity_window
        .to()
        .saturating_sub(activity_window.from())
        .saturating_add(BUCKETS_I64 - 1)
        / BUCKETS_I64;
    let diff = Expr::Binary {
        left: Box::new(Expr::Cast {
            expr: Box::new(start_col.clone()),
            target: DataType::I64,
            mode: CastMode::Strict,
        }),
        op: BinaryOp::Sub,
        right: Box::new(Expr::Literal(ScalarValue::I64(activity_window.from()))),
        data_type: DataType::I64,
    };
    let bucket_expr = Expr::Binary {
        left: Box::new(diff),
        op: BinaryOp::Div,
        right: Box::new(Expr::Literal(ScalarValue::I64(width))),
        data_type: DataType::I64,
    };

    let activity_spec = HashAggSpec {
        group_by: vec![
            (traces::USER_ID.to_owned(), user_col.clone()),
            (BUCKET.to_owned(), bucket_expr),
        ],
        aggs: vec![AggExpr {
            func: AggFunc::Count(Expr::Literal(ScalarValue::I64(1))),
            filter: None,
            output_name: TRACE_COUNT.to_owned(),
        }],
    };

    let latest_order = vec![
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
    let metadata_spec = HashAggSpec {
        group_by: vec![(traces::USER_ID.to_owned(), user_col)],
        aggs: vec![AggExpr {
            func: AggFunc::FirstByOrder {
                value: metadata_col,
                order_by: latest_order,
            },
            filter: None,
            output_name: traces::METADATA.to_owned(),
        }],
    };

    Ok((activity_spec, metadata_spec))
}

fn compute_user_scores(
    ctx: &EngineContext,
    exec_ctx: &ExecContext,
    base_spec: ScanSpec,
    compiled: Option<&CompiledFilter>,
    user_ids: &[String],
) -> Result<Vec<Batch>, QueryError> {
    let trace_set = open_user_extras_base(ctx, exec_ctx, base_spec, compiled, user_ids, false)?;
    if trace_set.streams.is_empty() {
        return Ok(Vec::new());
    }
    let trace_streams = trace_set.streams;
    let trace_schema = trace_set.schema;

    let trace_id_dt = col_expr(&trace_schema, traces::TRACE_ID)?.data_type();
    let right_schema = Schema::try_new_arc(vec![
        Field::new(SCORE_TRACE_ALIAS, trace_id_dt.clone(), false),
        Field::new(traces::USER_ID, DataType::Utf8, true),
    ])?;
    let right_exprs = vec![
        col_expr(&trace_schema, traces::TRACE_ID)?,
        col_expr(&trace_schema, traces::USER_ID)?,
    ];
    let right_streams: Vec<Box<dyn PartitionStream>> = trace_streams
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

    let scores_raw = scores_raw_schema().map_err(run_err_to_query)?;
    let mut scan_indices = Vec::with_capacity(4);
    for name in [
        scores::TRACE_ID,
        scores::NAME,
        scores::DATA_TYPE,
        scores::VALUE_NUM,
    ] {
        let idx = scores_raw
            .index_of(name)
            .ok_or_else(|| QueryError::internal(format!("missing {name} in scores schema")))?;
        scan_indices.push(idx);
    }

    let mut filters = vec![
        Expr::InList {
            expr: Box::new(col_expr(&scores_raw, scores::DATA_TYPE)?),
            list: vec![
                Expr::Literal(ScalarValue::Utf8(
                    ScoreDataType::Numeric.as_str().to_owned(),
                )),
                Expr::Literal(ScalarValue::Utf8(
                    ScoreDataType::Boolean.as_str().to_owned(),
                )),
            ],
            negated: false,
            redacted: false,
        },
        Expr::Unary {
            op: UnaryOp::IsNotNull,
            expr: Box::new(col_expr(&scores_raw, scores::VALUE_NUM)?),
            data_type: DataType::Bool,
        },
        Expr::Unary {
            op: UnaryOp::IsNotNull,
            expr: Box::new(col_expr(&scores_raw, scores::TRACE_ID)?),
            data_type: DataType::Bool,
        },
        // Issue occurrences never show as score columns or briefs (D-IC-18).
        Expr::Binary {
            left: Box::new(col_expr(&scores_raw, scores::SOURCE)?),
            op: BinaryOp::NotEq,
            right: Box::new(Expr::Literal(ScalarValue::Utf8(
                ScoreSource::Issue.as_str().to_owned(),
            ))),
            data_type: DataType::Bool,
        },
    ];
    if let Some(vf) = ctx.visible_from {
        filters.push(Expr::Binary {
            left: Box::new(col_expr(&scores_raw, scores::CREATED_AT)?),
            op: BinaryOp::GtEq,
            right: Box::new(Expr::Literal(ScalarValue::TimestampNs {
                value: vf,
                utc: true,
            })),
            data_type: DataType::Bool,
        });
    }

    let score_spec = ScanSpec {
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

    let raw_score_streams = ctx.scores.open_partitions(&score_spec, exec_ctx)?;
    if raw_score_streams.is_empty() {
        return Ok(Vec::new());
    }
    let score_proj_schema = ctx.scores.projected_schema(&score_spec)?;

    let (left_streams, left_schema) = if trace_id_dt == DataType::Utf8 {
        let encoded_schema = Schema::try_new_arc(vec![
            Field::new(scores::TRACE_ID, DataType::Utf8, true),
            Field::new(scores::NAME, DataType::Utf8, false),
            Field::new(scores::DATA_TYPE, DataType::Utf8, false),
            Field::new(scores::VALUE_NUM, DataType::F64, true),
        ])?;
        let enc_exprs = vec![
            Expr::Call {
                func: ScalarFn::Encode,
                args: vec![
                    col_expr(&score_proj_schema, scores::TRACE_ID)?,
                    Expr::Literal(ScalarValue::Utf8("hex".to_owned())),
                ],
                data_type: DataType::Utf8,
            },
            col_expr(&score_proj_schema, scores::NAME)?,
            col_expr(&score_proj_schema, scores::DATA_TYPE)?,
            col_expr(&score_proj_schema, scores::VALUE_NUM)?,
        ];
        let streams: Vec<Box<dyn PartitionStream>> = raw_score_streams
            .into_iter()
            .map(|s| {
                let proj = ProjectOp::with_schema(
                    enc_exprs.clone(),
                    Arc::clone(&encoded_schema),
                    exec_ctx.clone(),
                )?;
                Ok(
                    Box::new(OperatorStream::new(s, vec![Box::new(proj)], exec_ctx))
                        as Box<dyn PartitionStream>,
                )
            })
            .collect::<Result<Vec<_>, QueryError>>()?;
        (streams, encoded_schema)
    } else {
        (raw_score_streams, score_proj_schema)
    };

    let joined_schema = join_output_schema(JoinType::Inner, &left_schema, &right_schema);
    let joined_stream = execute_hash_join(
        left_streams,
        Arc::clone(&left_schema),
        right_streams,
        Arc::clone(&right_schema),
        HashJoinSpec {
            join_type: JoinType::Inner,
            left_keys: vec![col_expr(&left_schema, scores::TRACE_ID)?],
            right_keys: vec![col_expr(&right_schema, SCORE_TRACE_ALIAS)?],
            null_equals_null: false,
            residual_filter: None,
        },
        exec_ctx,
    )?;

    let agg_spec = HashAggSpec {
        group_by: vec![
            (
                traces::USER_ID.to_owned(),
                col_expr(&joined_schema, traces::USER_ID)?,
            ),
            (
                scores::NAME.to_owned(),
                col_expr(&joined_schema, scores::NAME)?,
            ),
            (
                scores::DATA_TYPE.to_owned(),
                col_expr(&joined_schema, scores::DATA_TYPE)?,
            ),
        ],
        aggs: vec![
            AggExpr {
                func: AggFunc::Avg(col_expr(&joined_schema, scores::VALUE_NUM)?),
                filter: None,
                output_name: SCORE_AVG.to_owned(),
            },
            AggExpr {
                func: AggFunc::Count(Expr::Literal(ScalarValue::I64(1))),
                filter: None,
                output_name: SCORE_COUNT.to_owned(),
            },
        ],
    };

    let agg_stream =
        execute_hash_agg_streams(vec![joined_stream], joined_schema, agg_spec, exec_ctx)?;

    let agg_schema = Schema::try_new_arc(vec![
        Field::new(traces::USER_ID, DataType::Utf8, true),
        Field::new(scores::NAME, DataType::Utf8, false),
        Field::new(scores::DATA_TYPE, DataType::Utf8, false),
        Field::new(SCORE_AVG, DataType::F64, true),
        Field::new(SCORE_COUNT, DataType::I64, false),
    ])?;

    let sort_keys = vec![
        SortKey {
            expr: col_expr(&agg_schema, traces::USER_ID)?,
            descending: false,
            nulls_first: false,
        },
        SortKey {
            expr: col_expr(&agg_schema, scores::NAME)?,
            descending: false,
            nulls_first: false,
        },
        SortKey {
            expr: col_expr(&agg_schema, scores::DATA_TYPE)?,
            descending: false,
            nulls_first: false,
        },
    ];

    let sort_schema_c = Arc::clone(&agg_schema);
    let sort_keys_c = sort_keys.clone();
    let ctx_sink = exec_ctx.clone();
    let sort_sinks = exec_run_pipeline(
        exec_ctx,
        vec![agg_stream],
        |_| Ok(Vec::new()),
        move |_| {
            Ok(ExternalSort::new(
                Arc::clone(&sort_schema_c),
                sort_keys_c.clone(),
                None,
                ctx_sink.clone(),
            ))
        },
    )?;
    let sorted_stream =
        ExternalSort::merge_sinks(sort_sinks, agg_schema, sort_keys, None, exec_ctx)?;
    let pool_c = Arc::clone(&exec_ctx.pool);
    let out_sinks = exec_run_pipeline(
        exec_ctx,
        vec![sorted_stream],
        |_| Ok(Vec::new()),
        move |_| Ok(CollectSink::new(&pool_c)),
    )?;
    Ok(out_sinks
        .into_iter()
        .flat_map(CollectSink::into_batches)
        .filter(|b| b.num_rows() > 0)
        .collect())
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

fn col_at(index: usize, name: &str, data_type: DataType) -> Expr {
    Expr::Column {
        index,
        name: name.to_owned(),
        data_type,
    }
}

fn coalesce_expr(primary: Expr, fallback: Expr) -> Expr {
    let dt = primary.data_type();
    Expr::Call {
        func: ScalarFn::Coalesce,
        args: vec![primary, fallback],
        data_type: dt,
    }
}

fn single_row(batches: &[Batch], exec_ctx: &ExecContext) -> Result<Option<Batch>, QueryError> {
    let non_empty: Vec<&Batch> = batches.iter().filter(|b| b.num_rows() > 0).collect();
    let Some(first) = non_empty.first() else {
        return Ok(None);
    };
    let schema = Arc::clone(first.schema());
    let mut res = exec_ctx.pool.register("user_single_row", false);
    let merged = Batch::concat(schema, &non_empty, Some(&mut res))?;
    res.free();
    if merged.num_rows() == 0 {
        return Ok(None);
    }
    if merged.num_rows() == 1 {
        return Ok(Some(merged));
    }
    Ok(Some(merged.slice(0, 1, None)?))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use bytesize::ByteSize;
    use cetra_core::enums::{ScoreSource, SpanStatus};
    use cetra_core::ids::ProjectId;
    use cetra_core::names::environment;
    use cetra_core::score::ScoreValue;
    use cetra_query::encode::encode_json_result;
    use serde_json::Value;

    use super::*;
    use crate::QueryOptions;
    use crate::config::{
        FlushConfig, MemtableConfig, QueryConfig, S3Config, StorageConfig, WalConfig,
    };
    use crate::reads::{ListScope, SortOrder};
    use crate::sql::{SQL_MAX_RESULT_BYTES, SQL_MAX_ROWS};
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
        let non_empty: Vec<Batch> = batches
            .iter()
            .filter(|b| b.num_rows() > 0)
            .cloned()
            .collect();
        let Some(first) = non_empty.first() else {
            return Vec::new();
        };
        let encoded = encode_json_result(
            first.schema(),
            &non_empty,
            SQL_MAX_ROWS,
            SQL_MAX_RESULT_BYTES,
            false,
            None,
        )
        .expect("encode json");
        encoded.rows.into_iter().map(Value::Object).collect()
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
    async fn user_list_all_9_sorts_filters_and_cursors() {
        let (_dir, storage, project) = setup_storage_with_dataset().await;
        let pq = storage
            .query(project, QueryOptions::new(None, None))
            .await
            .expect("pq");
        let ctx = QueryContext::new(pq, None).expect("ctx");

        for include_eval in [false, true] {
            for sort in [
                UserSort::LastSeen,
                UserSort::TraceCount,
                UserSort::CostTotal,
                UserSort::SessionCount,
                UserSort::TotalTokens,
                UserSort::ErrorCount,
                UserSort::FirstSeen,
                UserSort::ErrorRate,
                UserSort::TracesPerSession,
            ] {
                for order in [SortOrder::Asc, SortOrder::Desc] {
                    let mut cursor: Option<String> = None;
                    let mut collected = Vec::new();
                    loop {
                        let req = UserListRequest {
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
                        let page = user_list(&ctx, req).await.expect("list");
                        let rows = batches_to_json(&page.batches);
                        assert!(rows.len() <= 1);
                        collected.extend(rows);
                        match page.next_cursor {
                            Some(next) => cursor = Some(next),
                            None => break,
                        }
                    }
                    let full = user_list(
                        &ctx,
                        UserListRequest {
                            filter: None,
                            scope: ListScope {
                                window: None,
                                include_eval,
                                include_experiments: false,
                            },
                            sort,
                            order,
                            limit: 50,
                            cursor: None,
                            timeout: TIMEOUT,
                        },
                    )
                    .await
                    .expect("full");
                    assert_eq!(collected, batches_to_json(&full.batches));
                    assert!(!collected.is_empty());
                }
            }
        }

        // Environment filter visibility + score() filter before grouping
        for filter in [
            "environment = 'production'",
            "environment = 'cetra-eval'",
            "score('helpfulness') > 0.5",
        ] {
            let req = UserListRequest {
                filter: Some(filter),
                scope: ListScope {
                    window: None,
                    include_eval: false,
                    include_experiments: false,
                },
                sort: UserSort::LastSeen,
                order: SortOrder::Desc,
                limit: 50,
                cursor: None,
                timeout: TIMEOUT,
            };
            let q = user_list(&ctx, req).await.expect("list");
            let rows = batches_to_json(&q.batches);
            assert!(!rows.is_empty(), "expected matching rows for {filter}");
        }

        let _ = storage.shutdown().await;
    }

    #[tokio::test]
    async fn user_detail_whole_sessions_and_lonely_user() {
        let dir = tempfile::tempdir().expect("tempdir");
        let storage = crate::Storage::open(test_config(dir.path()))
            .await
            .expect("open");
        let project: ProjectId = "018f6b4e-7c1a-7c1a-8c1a-000000000001".parse().expect("pid");

        // Session s1 has trace 1 (u1), trace 2 (u2), trace 3 (u1, eval env).
        // `user_detail("u1", include_eval=false)` must return u1's stats (1 trace) AND whole session s1
        // covering both trace 1 and trace 2 (but NOT eval trace 3)!
        let mut s1 = span_row(1, 0, 1_000, 1_000);
        s1.user_id = Some("u1".to_owned());
        s1.session_id = Some("s1".to_owned());
        s1.cost_total = Some(0.1);

        let mut s2 = span_row(2, 0, 2_000, 2_000);
        s2.user_id = Some("u2".to_owned());
        s2.session_id = Some("s1".to_owned());
        s2.cost_total = Some(0.2);

        let mut s3 = span_row(3, 0, 3_000, 3_000);
        s3.user_id = Some("u1".to_owned());
        s3.session_id = Some("s1".to_owned());
        s3.environment = Some(environment::EVAL.to_owned());

        // Lonely user with no session_id -> empty sessions list, traces_per_session = NULL
        let mut s4 = span_row(4, 0, 4_000, 4_000);
        s4.user_id = Some("lonely".to_owned());

        storage
            .append_spans(project, span_rows(&[s1, s2, s3, s4]))
            .await
            .expect("append");

        let pq = storage
            .query(project, QueryOptions::new(None, None))
            .await
            .expect("pq");
        let ctx = QueryContext::new(pq, None).expect("ctx");

        let u1_no_eval = user_detail(
            &ctx,
            "u1",
            ListScope {
                window: None,
                include_eval: false,
                include_experiments: false,
            },
            TIMEOUT,
        )
        .await
        .expect("detail")
        .expect("some");
        let u1_row = &batches_to_json(&[u1_no_eval.user])[0];
        assert_eq!(u1_row["user_id"], "u1");
        assert_eq!(u1_row["trace_count"], 1);
        let u1_sessions = batches_to_json(&u1_no_eval.sessions);
        assert_eq!(u1_sessions.len(), 1);
        assert_eq!(u1_sessions[0]["session_id"], "s1");
        assert_eq!(u1_sessions[0]["trace_count"], 2);

        let u1_with_eval = user_detail(
            &ctx,
            "u1",
            ListScope {
                window: None,
                include_eval: true,
                include_experiments: false,
            },
            TIMEOUT,
        )
        .await
        .expect("detail")
        .expect("some");
        assert_eq!(batches_to_json(&[u1_with_eval.user])[0]["trace_count"], 2);
        assert_eq!(batches_to_json(&u1_with_eval.sessions)[0]["trace_count"], 3);

        let lonely = user_detail(
            &ctx,
            "lonely",
            ListScope {
                window: None,
                include_eval: false,
                include_experiments: false,
            },
            TIMEOUT,
        )
        .await
        .expect("detail")
        .expect("some");
        let lonely_row = &batches_to_json(&[lonely.user])[0];
        assert!(lonely_row["traces_per_session"].is_null());
        assert!(batches_to_json(&lonely.sessions).is_empty());

        assert!(
            user_detail(
                &ctx,
                "nobody",
                ListScope {
                    window: None,
                    include_eval: false,
                    include_experiments: false,
                },
                TIMEOUT,
            )
            .await
            .expect("missing")
            .is_none()
        );

        let _ = storage.shutdown().await;
    }

    #[tokio::test]
    async fn user_extras_empty_invalid_filter_signed_activity_issue_and_metadata() {
        let dir = tempfile::tempdir().expect("tempdir");
        let storage = crate::Storage::open(test_config(dir.path()))
            .await
            .expect("open");
        let project: ProjectId = "018f6b4e-7c1a-7c1a-8c1a-000000000001".parse().expect("pid");

        let from = 10_000_i64;
        let width = 1_000_i64;
        let activity_win = TimeWindow::new(from, from + 24 * width).expect("win");

        // Trace 1: before `activity.from()` so `(start_time - from) / width` is negative (-500 / 1000 == 0,
        // -1500 / 1000 == -1 via truncating division, NOT div_euclid!).
        let mut t1 = span_row(1, 0, from - 1_500, 1_000);
        t1.user_id = Some("u1".to_owned());
        t1.metadata
            .insert("user.plan".to_owned(), "free".to_owned());

        let mut t2 = span_row(2, 0, from - 500, 2_000);
        t2.user_id = Some("u1".to_owned());
        t2.metadata.insert("user.plan".to_owned(), "pro".to_owned());

        let mut t3 = span_row(3, 0, from + 2 * width, 3_000);
        t3.user_id = Some("u1".to_owned());
        t3.status = SpanStatus::Error;
        t3.metadata
            .insert("user.plan".to_owned(), "enterprise".to_owned());

        // Scores: one normal API score, one Issue occurrence (MUST be excluded per D-IC-18!)
        let mut sc_api = score_row(1, 1, 100, 100, 100, false, "quality");
        sc_api.value = ScoreValue::Numeric(0.25);
        let mut sc_issue = score_row(2, 2, 100, 100, 100, false, "quality");
        sc_issue.source = ScoreSource::Issue;
        sc_issue.value = ScoreValue::Numeric(0.95);
        let mut sc_bool = score_row(3, 3, 100, 100, 100, false, "passed");
        sc_bool.value = ScoreValue::Boolean(true);

        storage
            .append_spans(project, span_rows(&[t1, t2, t3]))
            .await
            .expect("spans");
        storage
            .append_scores(project, score_rows(&[sc_api, sc_issue, sc_bool]))
            .await
            .expect("scores");

        let pq = storage
            .query(project, QueryOptions::new(None, None))
            .await
            .expect("pq");
        let ctx = QueryContext::new(pq, None).expect("ctx");

        // 1. Empty user_ids MUST return Default BEFORE parsing filter (even invalid filter)!
        let empty_req = UserExtrasRequest {
            filter: Some("1 +"),
            scope: ListScope {
                window: None,
                include_eval: false,
                include_experiments: false,
            },
            user_ids: &[],
            activity: activity_win,
            timeout: TIMEOUT,
        };
        let q_empty = user_extras(&ctx, empty_req).await.expect("empty");
        assert!(q_empty.activity.is_empty());
        assert!(q_empty.scores.is_empty());
        assert!(q_empty.metadata.is_empty());

        // 2. Activity (including signed truncating buckets), scores (excl. issue), metadata
        let req = UserExtrasRequest {
            filter: None,
            scope: ListScope {
                window: None,
                include_eval: false,
                include_experiments: false,
            },
            user_ids: &["u1"],
            activity: activity_win,
            timeout: TIMEOUT,
        };
        let q = user_extras(&ctx, req).await.expect("extras");

        let mut q_act = batches_to_json(&q.activity);
        q_act.sort_by_key(|v| v["bucket"].as_i64().unwrap_or(0));
        assert_eq!(q_act.len(), 3);
        assert_eq!(q_act[0]["bucket"], -1);
        assert_eq!(q_act[1]["bucket"], 0);
        assert_eq!(q_act[2]["bucket"], 2);

        let q_scores = batches_to_json(&q.scores);
        assert_eq!(q_scores.len(), 2);
        let quality = q_scores
            .iter()
            .find(|r| r["name"] == "quality")
            .expect("quality");
        assert_eq!(quality["count"], 1);
        assert!((quality["avg"].as_f64().expect("avg") - 0.25).abs() < 1e-12);

        let q_meta = batches_to_json(&q.metadata);
        assert_eq!(q_meta.len(), 1);
        assert_eq!(q_meta[0]["user_id"], "u1");
        assert_eq!(q_meta[0]["metadata"]["user.plan"], "enterprise");

        // 3. user_extras with a `score(...)` filter (exercises traces_view + hex trace_id join path)
        let scored_req = UserExtrasRequest {
            filter: Some("score('quality') > 0.1"),
            scope: ListScope {
                window: None,
                include_eval: false,
                include_experiments: false,
            },
            user_ids: &["u1"],
            activity: activity_win,
            timeout: TIMEOUT,
        };
        let q_scored = user_extras(&ctx, scored_req).await.expect("scored");
        assert_eq!(batches_to_json(&q_scored.scores).len(), 1);
        assert_eq!(batches_to_json(&q_scored.metadata).len(), 1);

        let _ = storage.shutdown().await;
    }
}
