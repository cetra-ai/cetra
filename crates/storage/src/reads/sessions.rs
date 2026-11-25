//! Session list and session detail.
//!
//! A session's aggregates cover the traces inside the caller's window. Traces
//! are scoped first (the `traces` view, the window on `start_time`, and the
//! default exclusions). `references_environment` stays false: the sessions
//! view has no environment column. Spec 05 says the same.

use std::collections::BTreeSet;
use std::time::Duration;

use arrow::array::{Array, AsArray};
use arrow::compute::concat_batches;
use arrow::record_batch::RecordBatch;
use cetra_core::schema::{spans, traces};
use datafusion::common::ScalarValue;
use datafusion::error::DataFusionError;
use datafusion::functions_aggregate::expr_fn::{max, sum};
use datafusion::functions_window::expr_fn::row_number;
use datafusion::logical_expr::{Expr, ExprFunctionExt, col, lit, when};
use datafusion::prelude::DataFrame;
use futures::StreamExt;

use super::cursor::{after, assemble, decode, order_by};
use super::deletions::utf8;
use super::{ListScope, MAX_PAGE, Page, SortOrder, present, scoped_traces};
use crate::filter::{Filter, FilterView};
use crate::query_error::{RunError, classify};
use crate::views::{QueryContext, TimeWindow, sessions_from_traces};

const LIST: &str = "sessions";
const TRACE_COUNT: &str = "trace_count";
const TIES: &[&str] = &[traces::SESSION_ID];
const RANK_FIRST: &str = "first_rank";
const RANK_LAST: &str = "last_rank";

/// Output column of [`session_extras`]: traces of the session whose status is `error`.
pub const SESSION_ERROR_TRACE_COUNT: &str = "error_trace_count";
/// Output column of [`session_extras`]: `input_preview` of the session's earliest-starting trace.
pub const SESSION_FIRST_INPUT_PREVIEW: &str = "first_input_preview";
/// Output column of [`session_extras`]: `output_preview` of the session's latest-starting trace.
pub const SESSION_LAST_OUTPUT_PREVIEW: &str = "last_output_preview";

/// Most turns `session_detail` returns.
pub const MAX_SESSION_TURNS: usize = 1000;

/// Column a session list sorts by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionSort {
    /// `start_time`.
    StartTime,
    /// `end_time`.
    EndTime,
    /// `trace_count`.
    TraceCount,
    /// `cost_total`.
    CostTotal,
}

impl SessionSort {
    fn column(self) -> &'static str {
        match self {
            Self::StartTime => traces::START_TIME,
            Self::EndTime => traces::END_TIME,
            Self::TraceCount => TRACE_COUNT,
            Self::CostTotal => traces::COST_TOTAL,
        }
    }
}

/// One page of the sessions view.
#[derive(Debug, Clone)]
pub struct SessionListRequest<'a> {
    /// Filter text over the sessions view. Empty or whitespace means no filter.
    pub filter: Option<&'a str>,
    /// Window and default exclusions, applied to the traces before aggregation.
    pub scope: ListScope,
    /// Sort column.
    pub sort: SessionSort,
    /// Sort direction. Nulls are last.
    pub order: SortOrder,
    /// Rows to return, from 1 to [`super::MAX_PAGE`].
    pub limit: usize,
    /// Cursor from the previous page.
    pub cursor: Option<&'a str>,
    /// Time allowed for planning and running.
    pub timeout: Duration,
}

/// One session, its turns, and those turns' root payloads.
#[derive(Debug)]
pub struct SessionDetailData {
    /// The sessions-view row.
    pub session: RecordBatch,
    /// Scoped traces, `start_time ASC, trace_id ASC`, at most [`MAX_SESSION_TURNS`].
    pub turns: Vec<RecordBatch>,
    /// Root span `trace_id`, `input`, `input_format`, `output`, `output_format`.
    ///
    /// Turns without a root have no row.
    pub roots: Vec<RecordBatch>,
    /// True when more than [`MAX_SESSION_TURNS`] traces were stored.
    pub truncated: bool,
}

/// One page of sessions.
///
/// Traces are scoped first: the `traces` view, [`ListScope::window`] on
/// `start_time`, and the default exclusions. The environment exclusion always
/// applies, because a session filter cannot name `environment`. Aggregates
/// then cover the traces inside that window.
///
/// # Errors
///
/// [`RunError::InvalidRequest`] for a bad limit or cursor. [`RunError::Invalid`]
/// for filter text. [`RunError::Timeout`] when the query exceeds `timeout`.
pub async fn session_list(
    ctx: &QueryContext,
    request: SessionListRequest<'_>,
) -> Result<Page, RunError> {
    if !(1..=MAX_PAGE).contains(&request.limit) {
        return Err(RunError::InvalidRequest(
            "limit must be between 1 and 1000".to_owned(),
        ));
    }
    match tokio::time::timeout(request.timeout, list(ctx, &request)).await {
        Err(_) => Err(RunError::Timeout),
        Ok(result) => result,
    }
}

/// The session, its turns, and their root payloads. No time window.
///
/// An unknown `session_id` is `Ok(None)`.
///
/// # Errors
///
/// [`RunError::Timeout`] when the query exceeds `timeout`.
pub async fn session_detail(
    ctx: &QueryContext,
    session_id: &str,
    include_eval: bool,
    include_experiments: bool,
    timeout: Duration,
) -> Result<Option<SessionDetailData>, RunError> {
    match tokio::time::timeout(
        timeout,
        load(ctx, session_id, include_eval, include_experiments),
    )
    .await
    {
        Err(_) => Err(RunError::Timeout),
        Ok(result) => result,
    }
}

/// Inputs of [`session_extras`].
#[derive(Debug, Clone)]
pub struct SessionExtrasRequest<'a> {
    /// Same scope as the list request: the window and the default exclusions.
    pub scope: ListScope,
    /// Sessions to compute, at most [`MAX_PAGE`]. Empty runs no query.
    pub session_ids: &'a [&'a str],
    /// Time allowed for the query.
    pub timeout: Duration,
}

/// Per-session extras of a page of sessions: [`SESSION_ERROR_TRACE_COUNT`],
/// [`SESSION_FIRST_INPUT_PREVIEW`] and [`SESSION_LAST_OUTPUT_PREVIEW`].
///
/// Traces are scoped as the session list scopes them (window and default
/// exclusions, no filter: the list's filter applies to sessions), restricted to
/// `session_ids`. The earliest trace is the least `start_time`, then the least
/// `trace_id`; the latest is the greatest of both. A session without traces in
/// scope has no row.
///
/// # Errors
///
/// [`RunError::InvalidRequest`] for more than [`MAX_PAGE`] sessions.
/// [`RunError::Timeout`] when the query exceeds `timeout`.
pub async fn session_extras(
    ctx: &QueryContext,
    request: SessionExtrasRequest<'_>,
) -> Result<Vec<RecordBatch>, RunError> {
    if request.session_ids.len() > MAX_PAGE {
        return Err(RunError::InvalidRequest("too many sessions".to_owned()));
    }
    if request.session_ids.is_empty() {
        return Ok(Vec::new());
    }
    match tokio::time::timeout(request.timeout, extras(ctx, &request)).await {
        Err(_) => Err(RunError::Timeout),
        Ok(result) => result,
    }
}

async fn extras(
    ctx: &QueryContext,
    request: &SessionExtrasRequest<'_>,
) -> Result<Vec<RecordBatch>, RunError> {
    let ids: Vec<Expr> = request.session_ids.iter().map(|id| lit(*id)).collect();
    let first = row_number()
        .partition_by(vec![col(traces::SESSION_ID)])
        .order_by(vec![
            col(traces::START_TIME).sort(true, false),
            col(traces::TRACE_ID).sort(true, false),
        ])
        .build()
        .map_err(classify)?
        .alias(RANK_FIRST);
    let last = row_number()
        .partition_by(vec![col(traces::SESSION_ID)])
        .order_by(vec![
            col(traces::START_TIME).sort(false, true),
            col(traces::TRACE_ID).sort(false, true),
        ])
        .build()
        .map_err(classify)?
        .alias(RANK_LAST);
    let frame = scoped_traces(ctx, request.scope, false)
        .await?
        .filter(col(traces::SESSION_ID).in_list(ids, false))
        .map_err(classify)?
        // One window stage per sort order: DataFusion needs the expressions of a stage to share sort keys.
        .window(vec![first])
        .map_err(classify)?
        .window(vec![last])
        .map_err(classify)?
        .aggregate(
            vec![col(traces::SESSION_ID)],
            vec![
                sum(error_trace_flag()?).alias(SESSION_ERROR_TRACE_COUNT),
                ranked_value(RANK_FIRST, traces::INPUT_PREVIEW)?.alias(SESSION_FIRST_INPUT_PREVIEW),
                ranked_value(RANK_LAST, traces::OUTPUT_PREVIEW)?.alias(SESSION_LAST_OUTPUT_PREVIEW),
            ],
        )
        .map_err(classify)?;
    let batches = ctx.collect(frame).await.map_err(|error| ctx.fail(error))?;
    Ok(batches
        .into_iter()
        .filter(|batch| batch.num_rows() > 0)
        .collect())
}

/// 1 for a trace whose status is `error`, else 0.
fn error_trace_flag() -> Result<Expr, RunError> {
    when(col(traces::STATUS).eq(lit("error")), lit(1_i64))
        .otherwise(lit(0_i64))
        .map_err(classify)
}

/// `column` of the one row per group whose `rank` is 1, null elsewhere. Each group has
/// exactly one such row, so the maximum is that row's value (null when it is null).
fn ranked_value(rank: &str, column: &str) -> Result<Expr, RunError> {
    let value = when(col(rank).eq(lit(ScalarValue::UInt64(Some(1)))), col(column))
        .otherwise(lit(ScalarValue::Utf8(None)))
        .map_err(classify)?;
    Ok(max(value))
}

/// The sessions of the window, one row per session, with `filter` applied. No cursor, sort or
/// limit: the facets read these rows too.
pub(crate) async fn session_rows(
    ctx: &QueryContext,
    scope: ListScope,
    filter: Option<Filter>,
) -> Result<DataFrame, RunError> {
    let scoped = scoped_traces(ctx, scope, false).await?;
    let plan = sessions_from_traces(scoped.into_unoptimized_plan()).map_err(RunError::Internal)?;
    let mut frame = DataFrame::new(ctx.session().state(), plan);
    if let Some(filter) = filter {
        frame = filter.apply(frame, ctx).await.map_err(classify)?;
    }
    Ok(frame)
}

/// The distinct session ids the sessions list selects for `filter` over `window` (see
/// [`session_rows`]), at most `limit`. Which ids come back when more match is unspecified.
/// Null session ids are left out.
pub(crate) async fn filtered_session_ids(
    ctx: &QueryContext,
    filter: Option<Filter>,
    window: TimeWindow,
    include_experiments: bool,
    limit: usize,
) -> Result<Vec<String>, RunError> {
    let scope = ListScope {
        window: Some(window),
        include_eval: false,
        include_experiments,
    };
    let frame = session_rows(ctx, scope, filter)
        .await?
        .select(vec![col(traces::SESSION_ID)])
        .map_err(classify)?;
    let query_frame = frame;
    let mut stream = ctx.execute_stream(query_frame)
        .await
        .map_err(|error| ctx.fail(error))?;
    let mut found = BTreeSet::new();
    'read: while let Some(batch) = stream.next().await {
        let batch = batch.map_err(|error| ctx.fail(error))?;
        let sessions = utf8(batch.column(0))?;
        let sessions = sessions.as_string::<i32>();
        for row in 0..sessions.len() {
            if sessions.is_null(row) {
                continue;
            }
            found.insert(sessions.value(row).to_owned());
            if found.len() >= limit {
                break 'read;
            }
        }
    }
    Ok(found.into_iter().collect())
}

async fn list(ctx: &QueryContext, request: &SessionListRequest<'_>) -> Result<Page, RunError> {
    let filter = match present(request.filter) {
        Some(text) => Some(ctx.parse_filter(FilterView::Sessions, text)?),
        None => None,
    };
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
    let mut frame = session_rows(ctx, request.scope, filter).await?;
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
    let batches = ctx.collect(frame).await.map_err(|error| ctx.fail(error))?;
    assemble(
        batches,
        request.limit,
        LIST,
        request.sort.column(),
        request.order,
        TIES,
    )
}

async fn load(
    ctx: &QueryContext,
    session_id: &str,
    include_eval: bool,
    include_experiments: bool,
) -> Result<Option<SessionDetailData>, RunError> {
    let scope = ListScope {
        window: None,
        include_eval,
        include_experiments,
    };
    let scoped = scoped_traces(ctx, scope, false)
        .await?
        .filter(col(traces::SESSION_ID).eq(lit(session_id)))
        .map_err(classify)?;
    let plan = scoped.logical_plan().clone();
    let state = ctx.session().state();
    let session_plan = sessions_from_traces(plan.clone()).map_err(RunError::Internal)?;
    let query_frame = DataFrame::new(state.clone(), session_plan);
    let session = ctx.collect(query_frame)
        .await
        .map_err(|error| ctx.fail(error))?;
    let Some(session) = single_row(&session)? else {
        return Ok(None);
    };
    let query_frame = DataFrame::new(state, plan)
        .sort(order_by(
            traces::START_TIME,
            &[traces::TRACE_ID],
            SortOrder::Asc,
        ))
        .map_err(classify)?
        .limit(0, Some(MAX_SESSION_TURNS + 1))
        .map_err(classify)?;
    let turns = ctx.collect(query_frame)
        .await
        .map_err(|error| ctx.fail(error))?;
    let (turns, truncated) = cap_rows(turns, MAX_SESSION_TURNS);
    let roots = root_payloads(ctx, &turns).await?;
    Ok(Some(SessionDetailData {
        session,
        turns,
        roots,
        truncated,
    }))
}

async fn root_payloads(
    ctx: &QueryContext,
    turns: &[RecordBatch],
) -> Result<Vec<RecordBatch>, RunError> {
    let Some(schema) = turns
        .iter()
        .find(|batch| batch.num_rows() > 0)
        .map(RecordBatch::schema)
    else {
        return Ok(Vec::new());
    };
    let turns = concat_batches(&schema, turns).map_err(arrow_failure)?;
    let spans = ctx
        .session()
        .table("spans")
        .await
        .map_err(classify)?
        .select(vec![
            col(spans::TRACE_ID).alias("__root_trace"),
            col(spans::SPAN_ID).alias("__root_span"),
            col(spans::INPUT),
            col(spans::INPUT_FORMAT),
            col(spans::OUTPUT),
            col(spans::OUTPUT_FORMAT),
        ])
        .map_err(classify)?;
    let joined = ctx
        .session()
        .read_batch(turns)
        .map_err(RunError::Internal)?
        .join(
            spans,
            datafusion::logical_expr::JoinType::Inner,
            &[traces::TRACE_ID, traces::ROOT_SPAN_ID],
            &["__root_trace", "__root_span"],
            None,
        )
        .map_err(classify)?
        .select(vec![
            col(traces::TRACE_ID),
            col(spans::INPUT),
            col(spans::INPUT_FORMAT),
            col(spans::OUTPUT),
            col(spans::OUTPUT_FORMAT),
        ])
        .map_err(classify)?;
    ctx.collect(joined).await.map_err(|error| ctx.fail(error))
}

fn row_count(batches: &[RecordBatch]) -> usize {
    batches.iter().map(RecordBatch::num_rows).sum()
}

fn single_row(batches: &[RecordBatch]) -> Result<Option<RecordBatch>, RunError> {
    let Some(schema) = batches
        .iter()
        .find(|batch| batch.num_rows() > 0)
        .map(RecordBatch::schema)
    else {
        return Ok(None);
    };
    let batch = concat_batches(&schema, batches).map_err(arrow_failure)?;
    if batch.num_rows() == 0 {
        return Ok(None);
    }
    if batch.num_rows() == 1 {
        return Ok(Some(batch));
    }
    Ok(Some(batch.slice(0, 1)))
}

fn cap_rows(batches: Vec<RecordBatch>, limit: usize) -> (Vec<RecordBatch>, bool) {
    let total = row_count(&batches);
    if total <= limit {
        return (batches, false);
    }
    let mut kept = Vec::new();
    let mut remaining = limit;
    for batch in batches {
        if remaining == 0 {
            break;
        }
        if batch.num_rows() <= remaining {
            remaining -= batch.num_rows();
            kept.push(batch);
        } else {
            kept.push(batch.slice(0, remaining));
            remaining = 0;
        }
    }
    (kept, true)
}

fn arrow_failure(error: arrow::error::ArrowError) -> RunError {
    RunError::Internal(DataFusionError::ArrowError(Box::new(error), None))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use arrow::json::{ArrayWriter, WriterBuilder};
    use arrow::record_batch::RecordBatch;
    use cetra_core::time_serde::parse_rfc3339;
    use serde_json::Value;

    use cetra_core::enums::SpanStatus;
    use cetra_core::names::environment;
    use cetra_core::span::Span;

    use super::{
        SESSION_ERROR_TRACE_COUNT, SESSION_FIRST_INPUT_PREVIEW, SESSION_LAST_OUTPUT_PREVIEW,
        SessionExtrasRequest, SessionListRequest, SessionSort, session_detail, session_extras,
        session_list,
    };
    use crate::query_error::RunError;
    use crate::reads::{ListScope, MAX_PAGE, Page, SortOrder};
    use crate::test_support::{example_query, span_row, spans_table, traces_table};
    use crate::views::{QueryContext, TimeWindow};

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

    fn scope(window: Option<TimeWindow>, include_eval: bool) -> ListScope {
        ListScope {
            window,
            include_eval,
            include_experiments: false,
        }
    }

    fn request<'a>(
        scope: ListScope,
        sort: SessionSort,
        order: SortOrder,
    ) -> SessionListRequest<'a> {
        SessionListRequest {
            filter: None,
            scope,
            sort,
            order,
            limit: 50,
            cursor: None,
            timeout: TIMEOUT,
        }
    }

    fn fixture(name: &str) -> Value {
        let text = match name {
            "sessions.json" => include_str!("../../../../fixtures/api/responses/sessions.json"),
            "sessions-include-eval.json" => {
                include_str!("../../../../fixtures/api/responses/sessions-include-eval.json")
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

    fn assert_rows(actual: &[Value], expected: &[Value]) {
        assert_eq!(actual.len(), expected.len());
        for (row, fixture) in actual.iter().zip(expected) {
            for (key, value) in fixture.as_object().expect("object") {
                // Scores and the per-session extras are added by the server, not this view.
                if matches!(
                    key.as_str(),
                    "scores" | "error_trace_count" | "first_input_preview" | "last_output_preview"
                ) {
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

    fn ids(rows: &[Value]) -> Vec<String> {
        rows.iter()
            .map(|row| row["session_id"].as_str().expect("id").to_owned())
            .collect()
    }

    #[tokio::test]
    async fn session_lists_match_the_fixtures() {
        let ctx = context();
        let page = session_list(
            &ctx,
            request(scope(None, false), SessionSort::StartTime, SortOrder::Desc),
        )
        .await
        .expect("list");
        let listed = fixture("sessions.json");
        assert_rows(&rows(&page), listed["data"].as_array().expect("data"));
        assert!(page.next_cursor.is_none());

        let page = session_list(
            &ctx,
            request(scope(None, true), SessionSort::StartTime, SortOrder::Desc),
        )
        .await
        .expect("eval");
        let listed = fixture("sessions-include-eval.json");
        assert_rows(&rows(&page), listed["data"].as_array().expect("data"));
    }

    #[tokio::test]
    async fn session_detail_matches_the_fixture() {
        let ctx = context();
        for (include_eval, file) in [
            (false, "fixtures/api/responses/sessions/sess-001.json"),
            (
                true,
                "fixtures/api/responses/sessions/sess-001-include-eval.json",
            ),
        ] {
            let detail = session_detail(&ctx, "sess-001", include_eval, false, TIMEOUT)
                .await
                .expect("detail")
                .expect("session");
            let fixture = fixture_file(file);
            assert_rows(
                &json_rows(std::slice::from_ref(&detail.session)),
                std::slice::from_ref(&fixture["session"]),
            );
            let turns = json_rows(&detail.turns);
            let expected = fixture["turns"].as_array().expect("turns");
            let actual_ids: Vec<_> = turns
                .iter()
                .map(|row| row["trace_id"].as_str().expect("id").to_owned())
                .collect();
            let expected_ids: Vec<_> = expected
                .iter()
                .map(|turn| turn["trace"]["trace_id"].as_str().expect("id").to_owned())
                .collect();
            assert_eq!(actual_ids, expected_ids, "{file}");
            let roots = json_rows(&detail.roots);
            for turn in expected {
                if turn.get("input").is_none() {
                    continue;
                }
                let id = turn["trace"]["trace_id"].as_str().expect("id");
                let root = roots
                    .iter()
                    .find(|row| row["trace_id"] == id)
                    .unwrap_or_else(|| panic!("missing root {id}"));
                assert!(!root["input"].is_null(), "{id}");
                if turn["input"]["format"] == "text" {
                    assert_eq!(root["input"], turn["input"]["value"], "{id}");
                }
            }
            assert!(!detail.truncated, "{file}");
        }
        let missing = session_detail(&ctx, "missing", false, false, TIMEOUT)
            .await
            .expect("missing");
        assert!(missing.is_none());
    }

    #[tokio::test]
    async fn session_pages_cover_every_sort() {
        let ctx = context();
        let sorts = [
            SessionSort::StartTime,
            SessionSort::EndTime,
            SessionSort::TraceCount,
            SessionSort::CostTotal,
        ];
        for sort in sorts {
            for order in [SortOrder::Asc, SortOrder::Desc] {
                let mut full = request(scope(None, true), sort, order);
                full.limit = 50;
                let full = session_list(&ctx, full).await.expect("full");
                let expected = ids(&rows(&full));
                assert!(full.next_cursor.is_none());
                let mut walked = Vec::new();
                let mut cursor = None;
                loop {
                    let mut page_request = request(scope(None, true), sort, order);
                    page_request.limit = 1;
                    page_request.cursor = cursor.as_deref();
                    let page = session_list(&ctx, page_request).await.expect("page");
                    let page_ids = ids(&rows(&page));
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
        let costs = session_list(
            &ctx,
            request(scope(None, true), SessionSort::CostTotal, SortOrder::Desc),
        )
        .await
        .expect("costs");
        assert!(
            rows(&costs).iter().any(|row| row["cost_total"].is_null()),
            "cost_total has a null"
        );
    }

    #[tokio::test]
    async fn session_filters_keep_the_matching_sessions() {
        let ctx = context();
        for text in ["trace_count >= 2", "has_score('resolved')"] {
            let mut request = request(scope(None, false), SessionSort::StartTime, SortOrder::Desc);
            request.filter = Some(text);
            let page = session_list(&ctx, request).await.expect(text);
            assert_eq!(ids(&rows(&page)), ["sess-001"], "{text}");
        }
    }

    #[tokio::test]
    async fn a_window_counts_only_the_traces_inside_it() {
        let from = parse_rfc3339("2026-01-01T10:05:00Z").expect("from");
        let to = parse_rfc3339("2026-01-01T10:06:00Z").expect("to");
        let window = TimeWindow::new(from, to).expect("window");
        let page = session_list(
            &context(),
            request(
                scope(Some(window), false),
                SessionSort::StartTime,
                SortOrder::Desc,
            ),
        )
        .await
        .expect("window");
        let rows = rows(&page);
        assert_eq!(ids(&rows), ["sess-001"]);
        assert_eq!(rows[0]["trace_count"], 1);
    }

    #[tokio::test]
    async fn a_session_with_only_excluded_traces_is_none() {
        let mut root = span_row(7, 0, 1_000, 1_000);
        root.session_id = Some("eval-only".to_owned());
        root.environment = Some(environment::EVAL.to_owned());
        let spans = [root];
        let ctx = context_of(&spans);
        let hidden = session_detail(&ctx, "eval-only", false, false, TIMEOUT)
            .await
            .expect("hidden");
        assert!(hidden.is_none());
        let shown = session_detail(&ctx, "eval-only", true, false, TIMEOUT)
            .await
            .expect("eval");
        assert!(shown.is_some());
    }

    /// A span that is the root of its trace, in `session`, with the given previews.
    fn turn(trace: u16, start: i64, session: &str, previews: Option<(&str, &str)>) -> Span {
        let mut span = span_row(trace, 0, start, start);
        span.session_id = Some(session.to_owned());
        if let Some((input, output)) = previews {
            span.input_preview = Some(input.to_owned());
            span.output_preview = Some(output.to_owned());
        }
        span
    }

    fn extras_scope(window: Option<TimeWindow>) -> ListScope {
        ListScope {
            window,
            include_eval: false,
            include_experiments: false,
        }
    }

    async fn extras_of(
        ctx: &QueryContext,
        window: Option<TimeWindow>,
        session_ids: &[&str],
    ) -> Vec<Value> {
        let batches = session_extras(
            ctx,
            SessionExtrasRequest {
                scope: extras_scope(window),
                session_ids,
                timeout: TIMEOUT,
            },
        )
        .await
        .expect("extras");
        json_rows(&batches)
    }

    fn row_of<'a>(rows: &'a [Value], session: &str) -> &'a Value {
        rows.iter()
            .find(|row| row["session_id"] == session)
            .unwrap_or_else(|| panic!("no extras row for {session}"))
    }

    #[tokio::test]
    async fn session_extras_counts_errors_and_takes_the_first_and_last_previews() {
        let mut failed = turn(1, 1_000, "s", Some(("first", "out-1")));
        failed.status = SpanStatus::Error;
        let spans = [
            failed,
            // Same start as trace 1 with a larger trace_id: not the earliest.
            turn(5, 1_000, "s", Some(("tie-first", "out-5"))),
            turn(2, 2_000, "s", Some(("second", "out-2"))),
            turn(3, 3_000, "s", Some(("third", "out-3"))),
            // Same start as trace 3 with a larger trace_id: the latest.
            turn(4, 3_000, "s", Some(("fourth", "out-4"))),
            turn(6, 1_000, "quiet", None),
        ];
        let ctx = context_of(&spans);
        let rows = extras_of(&ctx, None, &["s", "quiet", "missing"]).await;
        assert_eq!(rows.len(), 2, "{rows:?}");

        let s = row_of(&rows, "s");
        assert_eq!(s[SESSION_ERROR_TRACE_COUNT], 1);
        assert_eq!(s[SESSION_FIRST_INPUT_PREVIEW], "first");
        assert_eq!(s[SESSION_LAST_OUTPUT_PREVIEW], "out-4");

        let quiet = row_of(&rows, "quiet");
        assert_eq!(quiet[SESSION_ERROR_TRACE_COUNT], 0);
        assert!(quiet[SESSION_FIRST_INPUT_PREVIEW].is_null(), "{quiet}");
        assert!(quiet[SESSION_LAST_OUTPUT_PREVIEW].is_null(), "{quiet}");
    }

    #[tokio::test]
    async fn session_extras_ignores_traces_outside_the_window() {
        let spans = [
            turn(1, 1_000, "s", Some(("in", "in-out"))),
            turn(2, 9_000, "s", Some(("late", "late-out"))),
        ];
        let ctx = context_of(&spans);
        let window = TimeWindow::new(0, 5_000).expect("window");
        let rows = extras_of(&ctx, Some(window), &["s"]).await;
        let s = row_of(&rows, "s");
        assert_eq!(s[SESSION_FIRST_INPUT_PREVIEW], "in");
        assert_eq!(s[SESSION_LAST_OUTPUT_PREVIEW], "in-out");
        assert_eq!(s[SESSION_ERROR_TRACE_COUNT], 0);
    }

    #[tokio::test]
    async fn session_extras_of_unknown_or_no_sessions_is_empty() {
        let spans = [turn(1, 1_000, "s", Some(("in", "out")))];
        let ctx = context_of(&spans);
        assert!(extras_of(&ctx, None, &["missing"]).await.is_empty());
        assert!(extras_of(&ctx, None, &[]).await.is_empty());
    }

    #[tokio::test]
    async fn session_extras_rejects_more_than_a_page_of_sessions() {
        let ids: Vec<&str> = vec!["s"; MAX_PAGE + 1];
        let result = session_extras(
            &context(),
            SessionExtrasRequest {
                scope: extras_scope(None),
                session_ids: &ids,
                timeout: TIMEOUT,
            },
        )
        .await;
        assert!(
            matches!(result, Err(RunError::InvalidRequest(_))),
            "{result:?}"
        );
    }
}
