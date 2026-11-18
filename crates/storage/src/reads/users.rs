//! User list and user detail.
//!
//! The caller's filter runs on `traces` before the grouping, so it can name
//! trace columns such as `environment` (spec 09 `UserListParams`). Exclusions
//! see that reference and skip the environment clause when the filter already
//! names it.

use std::time::Duration;

use std::sync::Arc;

use arrow::compute::concat_batches;
use arrow::datatypes::DataType;
use arrow::record_batch::RecordBatch;
use cetra_core::schema::{scores, traces};
use datafusion::common::ScalarValue;
use datafusion::error::DataFusionError;
use datafusion::execution::SessionState;
use datafusion::functions::expr_fn::{coalesce, nullif};
use datafusion::functions_aggregate::expr_fn::{avg, count, count_distinct, max, min, sum};
use datafusion::functions_window::expr_fn::row_number;
use datafusion::logical_expr::{
    Expr, ExprFunctionExt, JoinType, LogicalPlan, cast, col, in_subquery, lit, when,
};
use datafusion::prelude::DataFrame;

use super::cursor::{after, assemble, decode, order_by};
use super::{ListScope, Page, SortOrder, present, scoped_traces};
use crate::filter::FilterView;
use crate::query_error::{RunError, classify};
use crate::views::{QueryContext, TimeWindow, exact_cost_sum, sessions_from_traces};

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
const BUCKETS_I64: i64 = 24;

/// Most `user.` metadata keys returned per user.
pub const MAX_USER_METADATA_KEYS: usize = 20;

const BUCKET: &str = "bucket";
const RANK: &str = "rank";
const SCORE_AVG: &str = "avg";
const SCORE_COUNT: &str = "count";
const SCORE_TRACE_ID: &str = "t_trace_id";

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

/// Most sessions `user_detail` returns.
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

impl UserSort {
    fn column(self) -> &'static str {
        match self {
            Self::LastSeen => LAST_SEEN,
            Self::TraceCount => TRACE_COUNT,
            Self::CostTotal => traces::COST_TOTAL,
            Self::SessionCount => SESSION_COUNT,
            Self::TotalTokens => traces::TOTAL_TOKENS,
            Self::ErrorCount => traces::ERROR_COUNT,
            Self::FirstSeen => FIRST_SEEN,
            Self::ErrorRate => ERROR_RATE,
            Self::TracesPerSession => TRACES_PER_SESSION,
        }
    }
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
    /// Rows to return, from 1 to [`super::MAX_PAGE`].
    pub limit: usize,
    /// Cursor from the previous page.
    pub cursor: Option<&'a str>,
    /// Time allowed for planning and running.
    pub timeout: Duration,
}

/// One user and the whole sessions that contain the user's traces.
#[derive(Debug)]
pub struct UserDetailData {
    /// The aggregate row. Columns are [`USER_COLUMNS`].
    pub user: RecordBatch,
    /// Sessions of the user's traces, `start_time DESC, session_id DESC`.
    ///
    /// Each session covers every excluded trace in it, not only this user's.
    pub sessions: Vec<RecordBatch>,
}

/// One page of users.
///
/// Traces are scoped by the window and the default exclusions, then by the
/// filter, and only then grouped. `user_id IS NOT NULL` drops anonymous traces.
///
/// # Errors
///
/// [`RunError::InvalidRequest`] for a bad limit or cursor. [`RunError::Invalid`]
/// for filter text. [`RunError::Timeout`] when the query exceeds `timeout`.
pub async fn user_list(ctx: &QueryContext, request: UserListRequest<'_>) -> Result<Page, RunError> {
    if !(1..=super::MAX_PAGE).contains(&request.limit) {
        return Err(RunError::InvalidRequest(
            "limit must be between 1 and 1000".to_owned(),
        ));
    }
    match tokio::time::timeout(request.timeout, list(ctx, &request)).await {
        Err(_) => Err(RunError::Timeout),
        Ok(result) => result,
    }
}

/// The user and the sessions of their traces.
///
/// The scope's window and default exclusions select the traces (spec 09
/// § Observability data, UP-1). An unknown `user_id`, or one with no trace in
/// scope, is `Ok(None)`.
///
/// # Errors
///
/// [`RunError::Timeout`] when the query exceeds `timeout`.
pub async fn user_detail(
    ctx: &QueryContext,
    user_id: &str,
    scope: ListScope,
    timeout: Duration,
) -> Result<Option<UserDetailData>, RunError> {
    match tokio::time::timeout(timeout, load(ctx, user_id, scope)).await {
        Err(_) => Err(RunError::Timeout),
        Ok(result) => result,
    }
}

async fn list(ctx: &QueryContext, request: &UserListRequest<'_>) -> Result<Page, RunError> {
    let cursor = match request.cursor {
        Some(text) => {
            let cursor = decode(text, LIST, request.sort.column(), request.order)?;
            if cursor.tie_len() != 1 {
                return Err(RunError::InvalidRequest("invalid cursor".to_owned()));
            }
            Some(cursor)
        }
        None => None,
    };
    let mut frame = user_frame(ctx, request).await?;
    if let Some(cursor) = &cursor {
        frame = frame
            .filter(after(cursor, request.sort.column(), TIES))
            .map_err(classify)?;
    }
    let frame = frame
        .sort(order_by(request.sort.column(), TIES, request.order))
        .map_err(classify)?
        .limit(0, Some(request.limit + 1))
        .map_err(classify)?;
    let batches = frame.collect().await.map_err(|error| ctx.fail(error))?;
    assemble(
        batches,
        request.limit,
        LIST,
        request.sort.column(),
        request.order,
        TIES,
    )
}

/// The grouped frame, before the cursor, sort, and limit. Tests read its plan.
async fn user_frame(
    ctx: &QueryContext,
    request: &UserListRequest<'_>,
) -> Result<DataFrame, RunError> {
    let filter = match present(request.filter) {
        Some(text) => Some(ctx.parse_filter(FilterView::Traces, text)?),
        None => None,
    };
    let references_environment = filter
        .as_ref()
        .is_some_and(|filter| filter.references_environment);
    let mut frame = scoped_traces(ctx, request.scope, references_environment).await?;
    if let Some(filter) = filter {
        frame = filter.apply(frame, ctx).await.map_err(classify)?;
    }
    frame = frame
        .filter(col(traces::USER_ID).is_not_null())
        .map_err(classify)?;
    aggregate_users(frame)
}

fn aggregate_users(frame: DataFrame) -> Result<DataFrame, RunError> {
    let grouped = frame
        .aggregate(
            vec![col(traces::USER_ID)],
            vec![
                min(col(traces::START_TIME)).alias(FIRST_SEEN),
                max(col(traces::END_TIME)).alias(LAST_SEEN),
                count(lit(1_i64)).alias(TRACE_COUNT),
                count_distinct(col(traces::SESSION_ID)).alias(SESSION_COUNT),
                sum(col(traces::ERROR_COUNT)).alias(traces::ERROR_COUNT),
                sum(error_trace_flag()?).alias(ERROR_TRACE_COUNT),
                sum(col(traces::TOTAL_TOKENS)).alias(traces::TOTAL_TOKENS),
                exact_cost_sum(col(traces::COST_TOTAL)).alias(traces::COST_TOTAL),
            ],
        )
        .map_err(classify)?;
    grouped
        .select(vec![
            col(traces::USER_ID),
            col(FIRST_SEEN),
            col(LAST_SEEN),
            col(TRACE_COUNT),
            col(SESSION_COUNT),
            coalesce(vec![col(traces::ERROR_COUNT), lit(0_i64)]).alias(traces::ERROR_COUNT),
            coalesce(vec![col(ERROR_TRACE_COUNT), lit(0_i64)]).alias(ERROR_TRACE_COUNT),
            col(traces::TOTAL_TOKENS),
            col(traces::COST_TOTAL),
            // trace_count is at least 1 in every group, so the divisor is never 0.
            (cast(col(ERROR_TRACE_COUNT), DataType::Float64)
                / cast(col(TRACE_COUNT), DataType::Float64))
            .alias(ERROR_RATE),
            (cast(col(TRACE_COUNT), DataType::Float64)
                / nullif(cast(col(SESSION_COUNT), DataType::Float64), lit(0.0_f64)))
            .alias(TRACES_PER_SESSION),
        ])
        .map_err(classify)
}

/// 1 for a trace whose status is `error`, else 0.
fn error_trace_flag() -> Result<Expr, RunError> {
    when(col(traces::STATUS).eq(lit("error")), lit(1_i64))
        .otherwise(lit(0_i64))
        .map_err(classify)
}

/// Per-user extras of a page of users (activity, score averages, latest metadata).
///
/// Each query runs over the same traces as the list: the request's scope and filter,
/// restricted to `user_ids`. Users without rows have no batch entries.
///
/// # Errors
///
/// [`RunError::InvalidRequest`] for more than [`super::MAX_PAGE`] users or a bad
/// filter. [`RunError::Invalid`] for filter text. [`RunError::Timeout`] when the
/// queries exceed `timeout`.
pub async fn user_extras(
    ctx: &QueryContext,
    request: UserExtrasRequest<'_>,
) -> Result<UserExtras, RunError> {
    if request.user_ids.len() > super::MAX_PAGE {
        return Err(RunError::InvalidRequest("too many users".to_owned()));
    }
    if request.user_ids.is_empty() {
        return Ok(UserExtras::default());
    }
    match tokio::time::timeout(request.timeout, extras(ctx, &request)).await {
        Err(_) => Err(RunError::Timeout),
        Ok(result) => result,
    }
}

/// The traces of the listed users, scoped and filtered as the user list is.
async fn scoped_user_traces(
    ctx: &QueryContext,
    request: &UserExtrasRequest<'_>,
) -> Result<DataFrame, RunError> {
    let filter = match present(request.filter) {
        Some(text) => Some(ctx.parse_filter(FilterView::Traces, text)?),
        None => None,
    };
    let references_environment = filter
        .as_ref()
        .is_some_and(|filter| filter.references_environment);
    let mut frame = scoped_traces(ctx, request.scope, references_environment).await?;
    if let Some(filter) = filter {
        frame = filter.apply(frame, ctx).await.map_err(classify)?;
    }
    let ids: Vec<Expr> = request.user_ids.iter().map(|id| lit(*id)).collect();
    frame
        .filter(col(traces::USER_ID).in_list(ids, false))
        .map_err(classify)
}

async fn extras(
    ctx: &QueryContext,
    request: &UserExtrasRequest<'_>,
) -> Result<UserExtras, RunError> {
    let base = scoped_user_traces(ctx, request).await?;
    Ok(UserExtras {
        activity: activity(ctx, &base, request.activity).await?,
        scores: scores_of(ctx, &base).await?,
        metadata: metadata_of(ctx, &base).await?,
    })
}

/// Trace counts per user and bucket of `window`. Buckets are not clamped here.
async fn activity(
    ctx: &QueryContext,
    base: &DataFrame,
    window: TimeWindow,
) -> Result<Vec<RecordBatch>, RunError> {
    let width = window
        .to()
        .saturating_sub(window.from())
        .saturating_add(BUCKETS_I64 - 1)
        / BUCKETS_I64;
    let bucket = (cast(col(traces::START_TIME), DataType::Int64) - lit(window.from())) / lit(width);
    let frame = base
        .clone()
        .select(vec![col(traces::USER_ID), bucket.alias(BUCKET)])
        .map_err(classify)?
        .aggregate(
            vec![col(traces::USER_ID), col(BUCKET)],
            vec![count(lit(1_i64)).alias(TRACE_COUNT)],
        )
        .map_err(classify)?;
    let batches = frame.collect().await.map_err(|error| ctx.fail(error))?;
    Ok(non_empty(batches))
}

/// Average of each numeric and boolean score name over the user's traces, per user.
async fn scores_of(ctx: &QueryContext, base: &DataFrame) -> Result<Vec<RecordBatch>, RunError> {
    let traces_side = base
        .clone()
        .select(vec![
            col(traces::TRACE_ID).alias(SCORE_TRACE_ID),
            col(traces::USER_ID),
        ])
        .map_err(classify)?;
    let scores_side = ctx
        .session()
        .table("scores")
        .await
        .map_err(classify)?
        .filter(
            col(scores::DATA_TYPE)
                .in_list(vec![lit("numeric"), lit("boolean")], false)
                .and(col(scores::VALUE_NUM).is_not_null())
                .and(col(scores::TRACE_ID).is_not_null()),
        )
        .map_err(classify)?
        .select(vec![
            col(scores::TRACE_ID),
            col(scores::NAME),
            col(scores::DATA_TYPE),
            col(scores::VALUE_NUM),
        ])
        .map_err(classify)?;
    let frame = scores_side
        .join(
            traces_side,
            JoinType::Inner,
            &[scores::TRACE_ID],
            &[SCORE_TRACE_ID],
            None,
        )
        .map_err(classify)?
        .aggregate(
            vec![
                col(traces::USER_ID),
                col(scores::NAME),
                col(scores::DATA_TYPE),
            ],
            vec![
                avg(col(scores::VALUE_NUM)).alias(SCORE_AVG),
                count(lit(1_i64)).alias(SCORE_COUNT),
            ],
        )
        .map_err(classify)?
        .sort(vec![
            col(traces::USER_ID).sort(true, false),
            col(scores::NAME).sort(true, false),
            col(scores::DATA_TYPE).sort(true, false),
        ])
        .map_err(classify)?;
    let batches = frame.collect().await.map_err(|error| ctx.fail(error))?;
    Ok(non_empty(batches))
}

/// The `metadata` map of each user's latest trace (greatest `start_time`, then `trace_id`).
async fn metadata_of(ctx: &QueryContext, base: &DataFrame) -> Result<Vec<RecordBatch>, RunError> {
    let rank = row_number()
        .partition_by(vec![col(traces::USER_ID)])
        .order_by(vec![
            col(traces::START_TIME).sort(false, true),
            col(traces::TRACE_ID).sort(false, true),
        ])
        .build()
        .map_err(classify)?
        .alias(RANK);
    let frame = base
        .clone()
        .window(vec![rank])
        .map_err(classify)?
        .filter(col(RANK).eq(lit(ScalarValue::UInt64(Some(1)))))
        .map_err(classify)?
        .select(vec![col(traces::USER_ID), col(traces::METADATA)])
        .map_err(classify)?;
    let batches = frame.collect().await.map_err(|error| ctx.fail(error))?;
    Ok(non_empty(batches))
}

fn non_empty(batches: Vec<RecordBatch>) -> Vec<RecordBatch> {
    batches
        .into_iter()
        .filter(|batch| batch.num_rows() > 0)
        .collect()
}

/// Inputs of [`user_extras`].
#[derive(Debug, Clone)]
pub struct UserExtrasRequest<'a> {
    /// Same filter as the list request.
    pub filter: Option<&'a str>,
    /// Same scope as the list request (for `user_detail`: no window).
    pub scope: ListScope,
    /// Users to compute, at most [`super::MAX_PAGE`]. Empty runs no query.
    pub user_ids: &'a [&'a str],
    /// Window the activity buckets split.
    pub activity: TimeWindow,
    /// Time allowed for all three queries together.
    pub timeout: Duration,
}

/// Results of [`user_extras`].
#[derive(Debug, Default)]
pub struct UserExtras {
    /// `user_id` Utf8, `bucket` Int64 (0..ACTIVITY_BUCKETS), `trace_count` Int64. Buckets with no traces are absent.
    pub activity: Vec<RecordBatch>,
    /// `user_id` Utf8, `name` Utf8, `data_type` Utf8, `avg` Float64, `count` Int64,
    /// sorted by `user_id`, `name`, `data_type`.
    pub scores: Vec<RecordBatch>,
    /// `user_id` Utf8 and `metadata` (the traces' Map column): one row per user, from their latest trace.
    pub metadata: Vec<RecordBatch>,
}

async fn load(
    ctx: &QueryContext,
    user_id: &str,
    scope: ListScope,
) -> Result<Option<UserDetailData>, RunError> {
    let excluded = scoped_traces(ctx, scope, false).await?;
    let excluded_plan = excluded.logical_plan().clone();
    let state = ctx.session().state();
    let user_plan = DataFrame::new(state.clone(), excluded_plan.clone())
        .filter(col(traces::USER_ID).eq(lit(user_id)))
        .map_err(classify)?
        .logical_plan()
        .clone();
    let user = aggregate_users(DataFrame::new(state.clone(), user_plan.clone()))?
        .collect()
        .await
        .map_err(|error| ctx.fail(error))?;
    let Some(user) = single_row(&user)? else {
        return Ok(None);
    };
    let sessions = user_sessions_plan(&state, excluded_plan, user_plan)?;
    let sessions = DataFrame::new(state, sessions)
        .sort(order_by(
            traces::START_TIME,
            &[traces::SESSION_ID],
            SortOrder::Desc,
        ))
        .map_err(classify)?
        .limit(0, Some(MAX_USER_SESSIONS))
        .map_err(classify)?
        .collect()
        .await
        .map_err(|error| ctx.fail(error))?;
    let sessions: Vec<RecordBatch> = sessions
        .into_iter()
        .filter(|batch| batch.num_rows() > 0)
        .collect();
    Ok(Some(UserDetailData { user, sessions }))
}

/// Sessions of the user whose traces `user_plan` selects, over `excluded_plan`
/// (all scoped traces), through an `IN (subquery)` that plans as a semi join.
fn user_sessions_plan(
    state: &SessionState,
    excluded_plan: LogicalPlan,
    user_plan: LogicalPlan,
) -> Result<LogicalPlan, RunError> {
    let session_ids = DataFrame::new(state.clone(), user_plan)
        .filter(col(traces::SESSION_ID).is_not_null())
        .map_err(classify)?
        .select(vec![col(traces::SESSION_ID)])
        .map_err(classify)?
        .distinct()
        .map_err(classify)?
        .into_unoptimized_plan();
    let session_traces = DataFrame::new(state.clone(), excluded_plan)
        .filter(in_subquery(col(traces::SESSION_ID), Arc::new(session_ids)))
        .map_err(classify)?;
    sessions_from_traces(session_traces.into_unoptimized_plan()).map_err(RunError::Internal)
}

fn single_row(batches: &[RecordBatch]) -> Result<Option<RecordBatch>, RunError> {
    let Some(schema) = batches
        .iter()
        .find(|batch| batch.num_rows() > 0)
        .map(RecordBatch::schema)
    else {
        return Ok(None);
    };
    let batch = concat_batches(&schema, batches)
        .map_err(|error| RunError::Internal(DataFusionError::ArrowError(Box::new(error), None)))?;
    if batch.num_rows() == 0 {
        return Ok(None);
    }
    if batch.num_rows() == 1 {
        return Ok(Some(batch));
    }
    Ok(Some(batch.slice(0, 1)))
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroUsize;
    use std::sync::Arc;
    use std::time::Duration;

    use arrow::array::AsArray;
    use arrow::datatypes::Float64Type;
    use arrow::json::{ArrayWriter, WriterBuilder};
    use arrow::record_batch::RecordBatch;
    use cetra_core::enums::SpanStatus;
    use cetra_core::names::environment;
    use cetra_core::schema::{traces, traces_schema};
    use cetra_core::score::{Score, ScoreValue};
    use cetra_core::span::Span;
    use datafusion::datasource::MemTable;
    use datafusion::logical_expr::{col, lit};
    use datafusion::prelude::DataFrame;
    use serde_json::Value;

    use super::{
        ERROR_RATE, ERROR_TRACE_COUNT, SESSION_COUNT, TRACE_COUNT, TRACES_PER_SESSION,
        UserExtrasRequest, UserListRequest, UserSort, user_detail, user_extras, user_frame,
        user_list, user_sessions_plan,
    };
    use crate::reads::{ListScope, Page, SortOrder, scoped_traces};
    use crate::test_support::{
        example_query, live_scores_table, score_row, span_row, spans_table, trace_batch,
        traces_table,
    };
    use crate::views::{ProjectQuery, QueryContext, TimeWindow};

    const TIMEOUT: Duration = Duration::from_secs(30);

    fn context() -> QueryContext {
        QueryContext::new(example_query(), None).expect("context")
    }

    fn context_of(spans: &[Span]) -> QueryContext {
        let mut query = example_query();
        query.spans = spans_table(spans);
        query.traces = traces_table(spans);
        QueryContext::new(query, None).expect("context")
    }

    fn scope(include_eval: bool) -> ListScope {
        ListScope {
            window: None,
            include_eval,
            include_experiments: false,
        }
    }

    fn request<'a>(include_eval: bool, sort: UserSort, order: SortOrder) -> UserListRequest<'a> {
        UserListRequest {
            filter: None,
            scope: scope(include_eval),
            sort,
            order,
            limit: 50,
            cursor: None,
            timeout: TIMEOUT,
        }
    }

    fn fixture(name: &str) -> Value {
        let text = match name {
            "users.json" => include_str!("../../../../fixtures/api/responses/users.json"),
            "users-include-eval.json" => {
                include_str!("../../../../fixtures/api/responses/users-include-eval.json")
            }
            other => panic!("unknown fixture {other}"),
        };
        serde_json::from_str(text).expect(name)
    }

    fn fixture_file(path: &str) -> Value {
        let full = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join(path);
        let text = std::fs::read_to_string(&full)
            .unwrap_or_else(|error| panic!("{}: {error}", full.display()));
        serde_json::from_str(&text).unwrap_or_else(|error| panic!("{path}: {error}"))
    }

    fn json_rows(batches: &[RecordBatch]) -> Vec<Value> {
        if batches.iter().all(|batch| batch.num_rows() == 0) {
            return Vec::new();
        }
        let mut buffer = Vec::new();
        let mut writer: ArrayWriter<&mut Vec<u8>> = WriterBuilder::new()
            .with_explicit_nulls(true)
            .with_timestamp_tz_format("%Y-%m-%dT%H:%M:%S%.9fZ".to_owned())
            .with_timestamp_format("%Y-%m-%dT%H:%M:%S%.9f".to_owned())
            .build(&mut buffer);
        for batch in batches {
            writer.write(batch).expect("encode");
        }
        writer.finish().expect("finish");
        drop(writer);
        serde_json::from_slice(&buffer).expect("rows")
    }

    fn rows(page: &Page) -> Vec<Value> {
        json_rows(&page.batches)
    }

    fn values_match(actual: &Value, expected: &Value) -> bool {
        match (actual, expected) {
            (Value::Number(left), Value::Number(right)) => match (left.as_i64(), right.as_i64()) {
                (Some(left), Some(right)) => left == right,
                _ => {
                    let left = left.as_f64().expect("left");
                    let right = right.as_f64().expect("right");
                    (left - right).abs() < 1e-12
                }
            },
            (Value::Array(left), Value::Array(right)) => {
                left.len() == right.len()
                    && left
                        .iter()
                        .zip(right)
                        .all(|(left, right)| values_match(left, right))
            }
            (Value::Object(left), Value::Object(right)) => {
                left.len() == right.len()
                    && left.iter().all(|(key, value)| {
                        right
                            .get(key)
                            .is_some_and(|expected| values_match(value, expected))
                    })
            }
            _ => actual == expected,
        }
    }

    /// Keys of a user row that the server composes (activity, score averages, latest metadata).
    const USER_SERVER_KEYS: &[&str] = &["activity", "scores", "metadata"];

    /// Keys of a session row that the server adds from the session extras query.
    const SESSION_SERVER_KEYS: &[&str] = &[
        "scores",
        "error_trace_count",
        "first_input_preview",
        "last_output_preview",
    ];

    /// Checks each fixture key except `server_keys`, which the server composes from
    /// other queries and which the storage rows therefore never have.
    fn assert_rows(actual: &[Value], expected: &[Value], server_keys: &[&str]) {
        assert_eq!(actual.len(), expected.len());
        for (row, fixture) in actual.iter().zip(expected) {
            for (key, value) in fixture.as_object().expect("object") {
                if server_keys.contains(&key.as_str()) {
                    continue;
                }
                assert!(
                    values_match(&row[key], value),
                    "{key}: {} vs {value}",
                    row[key]
                );
            }
        }
    }

    fn ids(rows: &[Value], column: &str) -> Vec<String> {
        rows.iter()
            .map(|row| row[column].as_str().expect(column).to_owned())
            .collect()
    }

    #[tokio::test]
    async fn user_lists_match_the_fixtures() {
        let ctx = context();
        let page = user_list(&ctx, request(false, UserSort::LastSeen, SortOrder::Desc))
            .await
            .expect("list");
        let listed = fixture("users.json");
        assert_rows(
            &rows(&page),
            listed["data"].as_array().expect("data"),
            USER_SERVER_KEYS,
        );
        assert!(page.next_cursor.is_none());

        let page = user_list(&ctx, request(true, UserSort::LastSeen, SortOrder::Desc))
            .await
            .expect("eval");
        let listed = fixture("users-include-eval.json");
        assert_rows(
            &rows(&page),
            listed["data"].as_array().expect("data"),
            USER_SERVER_KEYS,
        );
    }

    #[tokio::test]
    async fn user_detail_matches_the_fixtures() {
        let ctx = context();
        for (user_id, include_eval, file) in [
            (
                "user-42",
                false,
                "fixtures/api/responses/users/user-42.json",
            ),
            (
                "user-42",
                true,
                "fixtures/api/responses/users/user-42-include-eval.json",
            ),
            ("user-7", false, "fixtures/api/responses/users/user-7.json"),
        ] {
            let detail = user_detail(&ctx, user_id, scope(include_eval), TIMEOUT)
                .await
                .expect("detail")
                .unwrap_or_else(|| panic!("missing {user_id}"));
            let fixture = fixture_file(file);
            assert_rows(
                &json_rows(std::slice::from_ref(&detail.user)),
                std::slice::from_ref(&fixture["user"]),
                USER_SERVER_KEYS,
            );
            let sessions = json_rows(&detail.sessions);
            let expected = fixture["sessions"].as_array().expect("sessions");
            assert_eq!(
                ids(&sessions, "session_id"),
                ids(expected, "session_id"),
                "{file}"
            );
            assert_rows(&sessions, expected, SESSION_SERVER_KEYS);
        }
        let missing = user_detail(&ctx, "nobody", scope(false), TIMEOUT)
            .await
            .expect("missing");
        assert!(missing.is_none());
    }

    /// D-A2-4: the user cost is the same bit for bit however the traces are
    /// split into partitions.
    #[tokio::test]
    async fn user_cost_does_not_depend_on_partitioning() {
        let trace_of = |trace: u16, cost: f64| {
            let mut span = span_row(trace, 0, 1_000 * i64::from(trace), 2_000);
            span.user_id = Some("u".to_owned());
            span.cost_total = Some(cost);
            trace_batch(&[span])
        };
        let (t1, t2, t3) = (trace_of(1, 0.1), trace_of(2, 0.2), trace_of(3, 0.3));
        let layouts = vec![
            vec![vec![t1.clone(), t2.clone(), t3.clone()]],
            vec![vec![t3.clone()], vec![t2.clone()], vec![t1.clone()]],
            vec![vec![t2.clone()], vec![t3.clone(), t1.clone()]],
        ];
        let mut bits = Vec::new();
        for partitions in layouts {
            let mut query = example_query();
            query.traces =
                Arc::new(MemTable::try_new(traces_schema(), partitions).expect("traces table"));
            query.target_partitions = NonZeroUsize::new(3).expect("three partitions");
            let ctx = QueryContext::new(query, None).expect("context");
            let page = user_list(&ctx, request(false, UserSort::CostTotal, SortOrder::Desc))
                .await
                .expect("list");
            assert_eq!(ids(&rows(&page), "user_id"), ["u"]);
            let batch = page
                .batches
                .iter()
                .find(|batch| batch.num_rows() > 0)
                .expect("one user");
            let value = batch
                .column_by_name(traces::COST_TOTAL)
                .expect("cost_total")
                .as_primitive::<Float64Type>()
                .value(0);
            assert!((value - 0.6).abs() < 1e-15, "cost {value}");
            bits.push(value.to_bits());
        }
        assert!(
            bits.iter().all(|value| *value == bits[0]),
            "cost depends on partitioning: {bits:?}"
        );
    }

    #[tokio::test]
    async fn user_pages_cover_every_sort() {
        let ctx = context();
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
                let full = user_list(&ctx, request(true, sort, order))
                    .await
                    .expect("full");
                let expected = ids(&rows(&full), "user_id");
                assert!(full.next_cursor.is_none());
                let mut walked = Vec::new();
                let mut cursor = None;
                loop {
                    let mut page_request = request(true, sort, order);
                    page_request.limit = 1;
                    page_request.cursor = cursor.as_deref();
                    let page = user_list(&ctx, page_request).await.expect("page");
                    let page_ids = ids(&rows(&page), "user_id");
                    assert!(page_ids.len() <= 1);
                    walked.extend(page_ids);
                    match page.next_cursor {
                        Some(next) => cursor = Some(next),
                        None => break,
                    }
                }
                assert_eq!(walked, expected, "{sort:?} {order:?}");
            }
        }
    }

    #[tokio::test]
    async fn computed_rates_match_their_counts() {
        let ctx = context();
        let page = user_list(&ctx, request(true, UserSort::ErrorRate, SortOrder::Desc))
            .await
            .expect("list");
        let rows = rows(&page);
        assert!(!rows.is_empty());
        for row in &rows {
            let trace_count = row[TRACE_COUNT].as_i64().expect("trace_count");
            let session_count = row[SESSION_COUNT].as_i64().expect("session_count");
            let error_traces = row[ERROR_TRACE_COUNT].as_i64().expect("error_trace_count");
            let error_rate = row[ERROR_RATE].as_f64().expect("error_rate");
            assert!(
                (error_rate - error_traces as f64 / trace_count as f64).abs() < 1e-12,
                "{row}"
            );
            if session_count == 0 {
                assert!(row[TRACES_PER_SESSION].is_null(), "{row}");
            } else {
                let per_session = row[TRACES_PER_SESSION]
                    .as_f64()
                    .expect("traces_per_session");
                assert!(
                    (per_session - trace_count as f64 / session_count as f64).abs() < 1e-12,
                    "{row}"
                );
            }
        }
        // The fixtures have no user without sessions, so the null case is not
        // exercised here; `user_pages_cover_every_sort` covers the order.
    }

    #[tokio::test]
    async fn an_environment_filter_is_visible_to_the_exclusions() {
        let ctx = context();
        let mut production = request(false, UserSort::LastSeen, SortOrder::Desc);
        production.filter = Some("environment = 'production'");
        let page = user_list(&ctx, production).await.expect("production");
        let production_rows = rows(&page);
        // user-42's 10:20 trace is staging, and the anonymous 10:30 trace has
        // no user. last_seen desc is user-7 (10:10) then user-42 (two
        // production traces). The eval trace is not one of them.
        assert_eq!(ids(&production_rows, "user_id"), ["user-7", "user-42"]);
        assert_eq!(production_rows[0]["trace_count"], 1);
        assert_eq!(production_rows[1]["trace_count"], 2);

        let mut eval_only = request(false, UserSort::LastSeen, SortOrder::Desc);
        let eval_filter = format!("environment = '{}'", environment::EVAL);
        eval_only.filter = Some(&eval_filter);
        let page = user_list(&ctx, eval_only).await.expect("eval");
        let eval_rows = rows(&page);
        assert_eq!(ids(&eval_rows, "user_id"), ["user-42"]);
        assert_eq!(eval_rows[0]["trace_count"], 1);
    }

    #[tokio::test]
    async fn count_distinct_session_id_plans_as_an_aggregate() {
        let request = request(false, UserSort::LastSeen, SortOrder::Desc);
        let frame = user_frame(&context(), &request).await.expect("frame");
        let logical = format!("{}", frame.logical_plan().display_indent());
        assert!(
            logical.contains("count") && logical.to_lowercase().contains("distinct"),
            "{logical}"
        );
    }

    #[tokio::test]
    async fn a_user_with_only_excluded_traces_is_none() {
        let mut root = span_row(7, 0, 1_000, 1_000);
        root.user_id = Some("eval-user".to_owned());
        root.environment = Some(environment::EVAL.to_owned());
        let spans = [root];
        let ctx = context_of(&spans);
        let hidden = user_detail(&ctx, "eval-user", scope(false), TIMEOUT)
            .await
            .expect("hidden");
        assert!(hidden.is_none());
        let shown = user_detail(&ctx, "eval-user", scope(true), TIMEOUT)
            .await
            .expect("eval");
        assert!(shown.is_some());
    }

    #[tokio::test]
    async fn a_user_without_sessions_has_an_empty_session_list() {
        let mut root = span_row(7, 0, 1_000, 1_000);
        root.user_id = Some("lonely".to_owned());
        let spans = [root];
        let detail = user_detail(&context_of(&spans), "lonely", scope(false), TIMEOUT)
            .await
            .expect("detail")
            .expect("user");
        assert!(detail.sessions.is_empty());
    }

    #[tokio::test]
    async fn user_detail_follows_the_window() {
        let mut first = user_span(1, 1_000, "u");
        first.session_id = Some("s1".to_owned());
        let mut second = user_span(2, 2_000, "u");
        second.session_id = Some("s1".to_owned());
        let mut third = user_span(3, 3_000, "u");
        third.session_id = Some("s2".to_owned());
        let spans = [first, second, third];
        let ctx = context_of(&spans);

        // [1000, 3000) holds the first two traces only, and they share session s1.
        let windowed = ListScope {
            window: Some(window_of(1_000, 3_000)),
            include_eval: false,
            include_experiments: false,
        };
        let detail = user_detail(&ctx, "u", windowed, TIMEOUT)
            .await
            .expect("detail")
            .expect("user in window");
        let user = json_rows(std::slice::from_ref(&detail.user));
        assert_eq!(user.len(), 1);
        assert_eq!(user[0]["trace_count"], 2);
        let first_seen = detail
            .user
            .column_by_name("first_seen")
            .expect("first_seen")
            .as_primitive::<arrow::datatypes::TimestampNanosecondType>()
            .value(0);
        assert_eq!(first_seen, 1_000);
        let last_seen = detail
            .user
            .column_by_name("last_seen")
            .expect("last_seen")
            .as_primitive::<arrow::datatypes::TimestampNanosecondType>()
            .value(0);
        // last_seen is the latest end_time, and span_row ends a span 100 ns after its start.
        assert_eq!(last_seen, 2_100);
        let sessions = json_rows(&detail.sessions);
        assert_eq!(sessions.len(), 1, "{sessions:?}");
        assert_eq!(sessions[0]["session_id"], "s1");
        assert_eq!(sessions[0]["trace_count"], 2);

        // No trace of the user starts in [5000, 6000).
        let empty = ListScope {
            window: Some(window_of(5_000, 6_000)),
            include_eval: false,
            include_experiments: false,
        };
        let missing = user_detail(&ctx, "u", empty, TIMEOUT)
            .await
            .expect("empty window");
        assert!(missing.is_none());
    }

    #[tokio::test]
    async fn user_detail_counts_only_in_window_traces_of_a_straddling_session() {
        // One session with three traces: two inside [1000, 3000), one after it.
        let mut first = user_span(1, 1_000, "u");
        first.session_id = Some("s1".to_owned());
        let mut second = user_span(2, 2_000, "u");
        second.session_id = Some("s1".to_owned());
        let mut after = user_span(3, 3_000, "u");
        after.session_id = Some("s1".to_owned());
        let spans = [first, second, after];
        let ctx = context_of(&spans);

        let windowed = ListScope {
            window: Some(window_of(1_000, 3_000)),
            include_eval: false,
            include_experiments: false,
        };
        let detail = user_detail(&ctx, "u", windowed, TIMEOUT)
            .await
            .expect("detail")
            .expect("user in window");
        let user = json_rows(std::slice::from_ref(&detail.user));
        assert_eq!(user.len(), 1);
        assert_eq!(user[0]["trace_count"], 2);
        let sessions = json_rows(&detail.sessions);
        assert_eq!(sessions.len(), 1, "{sessions:?}");
        assert_eq!(sessions[0]["session_id"], "s1");
        assert_eq!(sessions[0]["trace_count"], 2);
    }

    #[tokio::test]
    async fn user_detail_plans_a_semi_join() {
        let ctx = context();
        let state = ctx.session().state();
        let excluded = scoped_traces(&ctx, scope(false), false)
            .await
            .expect("scoped");
        let excluded_plan = excluded.logical_plan().clone();
        let user_plan = DataFrame::new(state.clone(), excluded_plan.clone())
            .filter(col(traces::USER_ID).eq(lit("user-42")))
            .expect("user filter")
            .logical_plan()
            .clone();
        let plan = user_sessions_plan(&state, excluded_plan, user_plan).expect("plan");
        let optimized = state.optimize(&plan).expect("optimize");
        let text = format!("{}", optimized.display_indent());
        assert!(text.contains("LeftSemi"), "{text}");
    }

    fn user_span(trace: u16, start: i64, user: &str) -> Span {
        let mut span = span_row(trace, 0, start, start);
        span.user_id = Some(user.to_owned());
        span
    }

    fn extras_context(spans: &[Span], scores: &[Score]) -> QueryContext {
        let query = ProjectQuery {
            spans: spans_table(spans),
            traces: traces_table(spans),
            scores: live_scores_table(scores),
            ..example_query()
        };
        QueryContext::new(query, None).expect("context")
    }

    fn window_of(from: i64, to: i64) -> TimeWindow {
        TimeWindow::new(from, to).expect("window")
    }

    #[tokio::test]
    async fn error_trace_count_counts_error_traces() {
        let mut failed = user_span(1, 1_000, "u");
        failed.status = SpanStatus::Error;
        let spans = [failed, user_span(2, 2_000, "u")];
        let ctx = context_of(&spans);
        let page = user_list(&ctx, request(false, UserSort::LastSeen, SortOrder::Desc))
            .await
            .expect("list");
        let rows = rows(&page);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["trace_count"], 2);
        assert_eq!(rows[0]["error_trace_count"], 1);
    }

    #[tokio::test]
    async fn activity_buckets_traces_by_start_time() {
        let (from, width) = (10_000_i64, 1_000_i64);
        let spans = [
            user_span(1, from, "u"),
            user_span(2, from + width, "u"),
            user_span(3, from + 2 * width + 1, "u"),
        ];
        let ctx = context_of(&spans);
        let window = window_of(from, from + 24 * width);
        let extras = user_extras(
            &ctx,
            UserExtrasRequest {
                filter: None,
                scope: ListScope {
                    window: Some(window),
                    include_eval: false,
                    include_experiments: false,
                },
                user_ids: &["u"],
                activity: window,
                timeout: TIMEOUT,
            },
        )
        .await
        .expect("extras");
        let activity = json_rows(&extras.activity);
        let mut buckets: Vec<(i64, i64)> = activity
            .iter()
            .map(|row| {
                assert_eq!(row["user_id"], "u");
                (
                    row["bucket"].as_i64().expect("bucket"),
                    row["trace_count"].as_i64().expect("trace_count"),
                )
            })
            .collect();
        buckets.sort_unstable();
        assert_eq!(buckets, [(0, 1), (1, 1), (2, 1)]);
    }

    #[tokio::test]
    async fn scores_average_numeric_and_boolean_per_user() {
        let spans = [user_span(1, 1_000, "u")];
        let mut quality = score_row(1, 1, 100, 100, 100, false, "quality");
        quality.value = ScoreValue::Numeric(0.2);
        let mut quality_two = score_row(2, 1, 100, 100, 100, false, "quality");
        quality_two.value = ScoreValue::Numeric(0.6);
        let mut helpful = score_row(3, 1, 100, 100, 100, false, "helpful");
        helpful.value = ScoreValue::Boolean(true);
        let mut label = score_row(4, 1, 100, 100, 100, false, "label");
        label.value = ScoreValue::Categorical {
            label: "good".to_owned(),
            value: None,
        };
        let ctx = extras_context(&spans, &[quality, quality_two, helpful, label]);
        let window = window_of(0, 10_000);
        let extras = user_extras(
            &ctx,
            UserExtrasRequest {
                filter: None,
                scope: ListScope {
                    window: None,
                    include_eval: false,
                    include_experiments: false,
                },
                user_ids: &["u"],
                activity: window,
                timeout: TIMEOUT,
            },
        )
        .await
        .expect("extras");
        let scores = json_rows(&extras.scores);
        assert_eq!(scores.len(), 2, "{scores:?}");
        assert_eq!(scores[0]["name"], "helpful");
        assert_eq!(scores[0]["data_type"], "boolean");
        assert!(values_match(&scores[0]["avg"], &serde_json::json!(1.0)));
        assert_eq!(scores[0]["count"], 1);
        assert_eq!(scores[1]["name"], "quality");
        assert_eq!(scores[1]["data_type"], "numeric");
        assert!(values_match(&scores[1]["avg"], &serde_json::json!(0.4)));
        assert_eq!(scores[1]["count"], 2);
    }

    #[tokio::test]
    async fn metadata_comes_from_the_latest_trace() {
        let mut early = user_span(1, 1_000, "u");
        early
            .metadata
            .insert("user.plan".to_owned(), "\"free\"".to_owned());
        let mut late = user_span(2, 2_000, "u");
        late.metadata
            .insert("user.plan".to_owned(), "\"team\"".to_owned());
        let spans = [early, late];
        let ctx = context_of(&spans);
        let window = window_of(0, 10_000);
        let extras = user_extras(
            &ctx,
            UserExtrasRequest {
                filter: None,
                scope: ListScope {
                    window: None,
                    include_eval: false,
                    include_experiments: false,
                },
                user_ids: &["u"],
                activity: window,
                timeout: TIMEOUT,
            },
        )
        .await
        .expect("extras");
        let metadata = json_rows(&extras.metadata);
        assert_eq!(metadata.len(), 1);
        assert_eq!(metadata[0]["user_id"], "u");
        assert_eq!(metadata[0]["metadata"]["user.plan"], "\"team\"");
    }

    #[tokio::test]
    async fn empty_user_ids_run_no_query() {
        let ctx = context();
        let window = window_of(0, 10_000);
        let extras = user_extras(
            &ctx,
            UserExtrasRequest {
                filter: None,
                scope: ListScope {
                    window: None,
                    include_eval: false,
                    include_experiments: false,
                },
                user_ids: &[],
                activity: window,
                timeout: TIMEOUT,
            },
        )
        .await
        .expect("extras");
        assert!(extras.activity.is_empty());
        assert!(extras.scores.is_empty());
        assert!(extras.metadata.is_empty());
    }
}
