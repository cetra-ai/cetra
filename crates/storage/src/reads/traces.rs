//! Trace and span lists, plus trace and span detail.
//!
//! Without score functions, the default list is `Limit(n+1) ← Sort(start_time DESC NULLS LAST, trace_id DESC) ← Filter ← traces view`.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use arrow::array::BooleanArray;
use arrow::compute::{concat_batches, filter_record_batch};
use arrow::record_batch::RecordBatch;
use cetra_core::schema::{spans, traces};
use datafusion::common::ScalarValue;
use datafusion::error::DataFusionError;
use datafusion::functions_window::expr_fn::row_number;
use datafusion::logical_expr::{Expr, ExprFunctionExt, col, lit};
use datafusion::prelude::DataFrame;

use super::cursor::{after, assemble, decode, order_by};
use super::{ListScope, Page, SortOrder, present};
use crate::filter::{Filter, FilterView, exclusions, search, strict_exclusions, time_window};
use crate::query_error::{RunError, classify};
use crate::views::{QueryContext, TimeWindow, ViewName};

const LIST: &str = "traces";
const SPANS: &str = "spans";
const DURATION_NS: &str = "duration_ns";
const DURATION_MS: &str = "duration_ms";
const TRACE_TIES: &[&str] = &[traces::TRACE_ID];
const SPAN_TIES: &[&str] = &[spans::SPAN_ID, spans::TRACE_ID];
/// The columns that name one span in a list: copies agree on all three.
const SPAN_KEY: &[&str] = &[spans::START_TIME, spans::SPAN_ID, spans::TRACE_ID];
/// Most rows one list attempt reads, 64 × [`super::MAX_PAGE`]. A span stored many times, or a
/// filter matching many copies, can't make the refetch read more than this.
const MAX_FETCH: usize = 64 * super::MAX_PAGE;
const SPAN_RANK: &str = "rn";

/// Most spans `trace_detail` returns for one trace.
pub const MAX_TRACE_SPANS: usize = 20_000;

/// Max ids of one [`trace_refresh`].
pub const MAX_REFRESH_IDS: usize = 500;

/// Most spans `span_refresh` returns for one trace (D-I-9).
pub const MAX_REFRESH_SPANS_PER_TRACE: usize = 200;

/// Traces the live stream re-reads (D-G-13, D-G-31).
#[derive(Debug, Clone, Copy)]
pub struct TraceRefreshRequest<'a> {
    /// Filter text over `traces`. Empty or whitespace means no filter.
    pub filter: Option<&'a str>,
    /// Search text and the `start_time` window its spans must fall in (D-G-33).
    pub search: Option<(&'a str, TimeWindow)>,
    /// Trace ids, 32 hex digits, either case. At most [`MAX_REFRESH_IDS`].
    pub trace_ids: &'a [&'a str],
    /// Keep rows whose environment is the eval environment.
    pub include_eval: bool,
    /// Keep rows that belong to an experiment.
    pub include_experiments: bool,
    /// Apply [`strict_exclusions`]: evaluation and experiment rows are left out even when the
    /// filter names `environment` or the include flags are set (D-I-9).
    pub strict_exclusions: bool,
    /// Time allowed for planning and running.
    pub timeout: Duration,
}

/// Spans of the given traces that match a span filter, for the online-rule sweeper (D-I-9).
#[derive(Debug, Clone, Copy)]
pub struct SpanRefreshRequest<'a> {
    /// Filter text over `spans`. Empty or whitespace means no filter.
    pub filter: Option<&'a str>,
    /// Trace ids, 32 hex digits, either case. At most [`MAX_REFRESH_IDS`].
    pub trace_ids: &'a [&'a str],
    /// Most spans kept per trace, 1 to [`MAX_REFRESH_SPANS_PER_TRACE`].
    pub per_trace: usize,
    /// Apply [`strict_exclusions`] instead of the default exclusions.
    pub strict_exclusions: bool,
    /// Time allowed for planning and running.
    pub timeout: Duration,
}

/// The columns `span_refresh` returns, in this order.
pub const SPAN_REFRESH_COLUMNS: &[&str] = &[spans::TRACE_ID, spans::SPAN_ID, spans::START_TIME];

/// The columns `trace_refresh` returns with `strict_exclusions: true`, in this order.
pub const TRACE_REFRESH_STRICT_COLUMNS: &[&str] = &[traces::TRACE_ID, traces::START_TIME];

/// Most rows [`match_count`] counts (D-I-9's preview).
pub const MAX_MATCH_COUNT: usize = 10_000;

/// Rows of a view that match a filter in a window, for the online-rule preview (D-I-9).
#[derive(Debug, Clone, Copy)]
pub struct MatchCountRequest<'a> {
    /// `ViewName::Traces` or `ViewName::Spans`.
    pub view: ViewName,
    /// Filter text over the view. Empty or whitespace means no filter.
    pub filter: Option<&'a str>,
    /// `start_time ∈ [from, to)`.
    pub window: TimeWindow,
    /// The count stops at `cap + 1`. From 1 to [`MAX_MATCH_COUNT`].
    pub cap: usize,
    /// Apply [`strict_exclusions`] instead of the default exclusions.
    pub strict_exclusions: bool,
    /// Time allowed for planning and running.
    pub timeout: Duration,
}

/// Span list columns: [`cetra_core::schema::SPANS_LIGHT_COLUMNS`] without
/// `project_id`, with `duration_ms` after `duration_ns`.
pub const SPAN_LIST_COLUMNS: &[&str] = &[
    spans::TRACE_ID,
    spans::SPAN_ID,
    spans::PARENT_SPAN_ID,
    spans::NAME,
    spans::SPAN_KIND,
    spans::START_TIME,
    spans::END_TIME,
    spans::DURATION_NS,
    DURATION_MS,
    spans::STATUS,
    spans::STATUS_MESSAGE,
    spans::SERVICE_NAME,
    spans::SESSION_ID,
    spans::USER_ID,
    spans::ENVIRONMENT,
    spans::RELEASE,
    spans::TAGS,
    spans::METADATA,
    spans::PROVIDER,
    spans::REQUEST_MODEL,
    spans::RESPONSE_MODEL,
    spans::MODEL,
    spans::INPUT_TOKENS,
    spans::OUTPUT_TOKENS,
    spans::CACHE_READ_TOKENS,
    spans::CACHE_WRITE_TOKENS,
    spans::REASONING_TOKENS,
    spans::TOTAL_TOKENS,
    spans::COST_INPUT,
    spans::COST_OUTPUT,
    spans::COST_TOTAL,
    spans::TTFT_NS,
    spans::INPUT_PREVIEW,
    spans::OUTPUT_PREVIEW,
];

/// Column a trace list sorts by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TraceSort {
    /// `start_time`.
    StartTime,
    /// `duration_ns`.
    Duration,
    /// `cost_total`.
    CostTotal,
    /// `total_tokens`.
    TotalTokens,
}

impl TraceSort {
    fn column(self) -> &'static str {
        match self {
            Self::StartTime => traces::START_TIME,
            Self::Duration => DURATION_NS,
            Self::CostTotal => traces::COST_TOTAL,
            Self::TotalTokens => traces::TOTAL_TOKENS,
        }
    }
}

/// One page of the traces view.
#[derive(Debug, Clone)]
pub struct TraceListRequest<'a> {
    /// Filter text. Empty or whitespace means no filter.
    pub filter: Option<&'a str>,
    /// Search text. Empty or whitespace means no search.
    pub search: Option<&'a str>,
    /// Window and default exclusions.
    pub scope: ListScope,
    /// Apply [`strict_exclusions`]: evaluation and experiment rows are left out even when the
    /// filter names `environment` or the include flags are set (D-I-9).
    pub strict_exclusions: bool,
    /// Sort column.
    pub sort: TraceSort,
    /// Sort direction. Nulls are last.
    pub order: SortOrder,
    /// Rows to return, from 1 to [`super::MAX_PAGE`].
    pub limit: usize,
    /// Cursor from the previous page.
    pub cursor: Option<&'a str>,
    /// Time allowed for planning and running.
    pub timeout: Duration,
}

/// Column a span list sorts by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpanSort {
    /// `start_time`.
    StartTime,
    /// `duration_ns`.
    Duration,
    /// `cost_total`.
    CostTotal,
    /// `total_tokens`.
    TotalTokens,
}

impl SpanSort {
    fn column(self) -> &'static str {
        match self {
            Self::StartTime => spans::START_TIME,
            Self::Duration => DURATION_NS,
            Self::CostTotal => spans::COST_TOTAL,
            Self::TotalTokens => spans::TOTAL_TOKENS,
        }
    }
}

/// One page of the spans view, projected to [`SPAN_LIST_COLUMNS`].
#[derive(Debug, Clone)]
pub struct SpanListRequest<'a> {
    /// Filter text. Empty or whitespace means no filter.
    pub filter: Option<&'a str>,
    /// Search text. Empty or whitespace means no search.
    pub search: Option<&'a str>,
    /// Window and default exclusions.
    pub scope: ListScope,
    /// Apply [`strict_exclusions`]: evaluation and experiment rows are left out even when the
    /// filter names `environment` or the include flags are set (D-I-9).
    pub strict_exclusions: bool,
    /// Sort column.
    pub sort: SpanSort,
    /// Sort direction. Nulls are last.
    pub order: SortOrder,
    /// Rows to return, from 1 to [`super::MAX_PAGE`].
    pub limit: usize,
    /// Cursor from the previous page.
    pub cursor: Option<&'a str>,
    /// Time allowed for planning and running.
    pub timeout: Duration,
}

/// One trace row and its spans.
#[derive(Debug)]
pub struct TraceDetailData {
    /// The `traces` view row, all columns.
    pub trace: RecordBatch,
    /// [`SPAN_LIST_COLUMNS`], ordered `start_time ASC, span_id ASC`.
    pub spans: Vec<RecordBatch>,
    /// True when more than [`MAX_TRACE_SPANS`] spans were stored.
    pub truncated: bool,
}

/// One page of traces, in traces-view column order.
///
/// # Errors
///
/// [`RunError::InvalidRequest`] for a bad limit, cursor, or search window.
/// [`RunError::Invalid`] for filter text. [`RunError::Timeout`] when the query
/// exceeds `timeout`. DataFusion failures go through [`classify`].
pub async fn trace_list(
    ctx: &QueryContext,
    request: TraceListRequest<'_>,
) -> Result<Page, RunError> {
    run_list(ctx, ListPlan::traces(&request), request.timeout, MAX_FETCH).await
}

/// One page of spans. Exclusions use the span's own environment and metadata.
///
/// A span stored more than once (a re-send not yet compacted) is listed once, as its copy with
/// the greatest `ingest_time`.
///
/// # Errors
///
/// Same as [`trace_list`].
///
/// A page reads at most [`MAX_FETCH`] rows to find `limit` distinct spans. When that cap comes
/// first, the page holds the distinct spans read so far: fewer than `limit` is possible, and the
/// cursor is still set, so paging goes on from the last span returned.
///
/// The filter, the search and the default exclusions apply before de-duplication. If the newest
/// copy of a span fails them and an older copy passes, the older copy is listed.
pub async fn span_list(ctx: &QueryContext, request: SpanListRequest<'_>) -> Result<Page, RunError> {
    span_list_capped(ctx, request, MAX_FETCH).await
}

/// [`span_list`] with a caller-chosen read cap. Tests pass a small one.
async fn span_list_capped(
    ctx: &QueryContext,
    request: SpanListRequest<'_>,
    cap: usize,
) -> Result<Page, RunError> {
    run_list(ctx, ListPlan::spans(&request), request.timeout, cap).await
}

/// The trace and its spans. Default exclusions do not apply.
///
/// `trace_id` is 32 hex digits, either case. An unknown id is `Ok(None)`.
///
/// # Errors
///
/// [`RunError::InvalidRequest`] when `trace_id` is not 32 hex digits.
/// [`RunError::Timeout`] when the query exceeds `timeout`.
pub async fn trace_detail(
    ctx: &QueryContext,
    trace_id: &str,
    timeout: Duration,
) -> Result<Option<TraceDetailData>, RunError> {
    trace_detail_limited(ctx, trace_id, timeout, MAX_TRACE_SPANS).await
}

/// `trace_detail` with a caller-chosen span cap. Tests pass a small cap.
async fn trace_detail_limited(
    ctx: &QueryContext,
    trace_id: &str,
    timeout: Duration,
    cap: usize,
) -> Result<Option<TraceDetailData>, RunError> {
    let trace_id = hex_id(trace_id, 32, "trace_id")?;
    match tokio::time::timeout(timeout, load_trace(ctx, &trace_id, cap)).await {
        Err(_) => Err(RunError::Timeout),
        Ok(result) => result,
    }
}

/// All columns of the span with this `(trace_id, span_id)`.
///
/// Duplicate rows keep the greatest `ingest_time`. An unknown pair is `Ok(None)`.
///
/// # Errors
///
/// [`RunError::InvalidRequest`] when an id has the wrong number of hex digits.
/// [`RunError::Timeout`] when the query exceeds `timeout`.
pub async fn span_detail(
    ctx: &QueryContext,
    trace_id: &str,
    span_id: &str,
    timeout: Duration,
) -> Result<Option<RecordBatch>, RunError> {
    let trace_id = hex_id(trace_id, 32, "trace_id")?;
    let span_id = hex_id(span_id, 16, "span_id")?;
    match tokio::time::timeout(timeout, load_span(ctx, &trace_id, &span_id)).await {
        Err(_) => Err(RunError::Timeout),
        Ok(result) => result,
    }
}

/// One trace's spans for an evaluator (D-I-23).
#[derive(Debug, Clone, Copy)]
pub struct TraceSpansRequest<'a> {
    /// 32 hex digits, either case.
    pub trace_id: &'a str,
    /// 16 hex digits, either case. Always returned when stored.
    pub target_span_id: Option<&'a str>,
    /// Most spans of the "earliest others" part.
    pub max_spans: usize,
    /// Most rows of the required part. The read stops at `max_required + 1` rows, so the caller
    /// can tell "over the cap" from "exactly at it".
    pub max_required: usize,
    /// Time allowed for both queries together.
    pub timeout: Duration,
}

/// The spans, every column of the `spans` view, in two parts.
#[derive(Debug)]
pub struct TraceSpansData {
    /// Parentless spans and the target span. May repeat a span of `earliest`.
    pub required: Vec<RecordBatch>,
    /// The earliest spans, `start_time ASC, span_id ASC`, at most `max_spans`.
    pub earliest: Vec<RecordBatch>,
    /// True when more than `max_spans` spans were stored.
    pub truncated: bool,
}

/// Every column of one trace's spans: the required ones and the earliest `max_spans`. No
/// default exclusion applies. An unknown trace gives two empty parts.
///
/// # Errors
///
/// [`RunError::InvalidRequest`] for a malformed id, `max_spans == 0` or `max_required == 0`;
/// [`RunError::Timeout`]; [`RunError::TooLarge`] when the query memory pool is exceeded.
pub async fn trace_spans(
    ctx: &QueryContext,
    request: TraceSpansRequest<'_>,
) -> Result<TraceSpansData, RunError> {
    let trace_id = hex_id(request.trace_id, 32, "trace_id")?;
    let target = request
        .target_span_id
        .map(|id| hex_id(id, 16, "span_id"))
        .transpose()?;
    if request.max_spans == 0 || request.max_required == 0 {
        return Err(RunError::InvalidRequest(
            "max_spans and max_required must be at least 1".to_owned(),
        ));
    }
    let load = load_trace_spans(
        ctx,
        &trace_id,
        target.as_deref(),
        request.max_spans,
        request.max_required,
    );
    match tokio::time::timeout(request.timeout, load).await {
        Err(_) => Err(RunError::Timeout),
        Ok(result) => result,
    }
}

async fn load_trace_spans(
    ctx: &QueryContext,
    trace_id: &str,
    target: Option<&str>,
    max_spans: usize,
    max_required: usize,
) -> Result<TraceSpansData, RunError> {
    let root_or_target = match target {
        Some(target) => col(spans::PARENT_SPAN_ID)
            .is_null()
            .or(col(spans::SPAN_ID).eq(lit(target))),
        None => col(spans::PARENT_SPAN_ID).is_null(),
    };
    let required = ctx
        .session()
        .table(SPANS)
        .await
        .map_err(classify)?
        .filter(col(spans::TRACE_ID).eq(lit(trace_id)).and(root_or_target))
        .map_err(classify)?
        .limit(0, Some(max_required.saturating_add(1)))
        .map_err(classify)?;
    let required = required.collect().await.map_err(|error| ctx.fail(error))?;
    let earliest = ctx
        .session()
        .table(SPANS)
        .await
        .map_err(classify)?
        .filter(col(spans::TRACE_ID).eq(lit(trace_id)))
        .map_err(classify)?
        .sort(order_by(
            spans::START_TIME,
            &[spans::SPAN_ID],
            SortOrder::Asc,
        ))
        .map_err(classify)?
        .limit(0, Some(max_spans.saturating_add(1)))
        .map_err(classify)?;
    let earliest = earliest.collect().await.map_err(|error| ctx.fail(error))?;
    let (earliest, truncated) = cap_rows(earliest, max_spans);
    Ok(TraceSpansData {
        required,
        earliest,
        truncated,
    })
}

/// The traces-view rows (every column, no order) of `trace_ids` that pass the default
/// exclusions, the filter and the search. With `strict_exclusions`, only the
/// [`TRACE_REFRESH_STRICT_COLUMNS`] are returned. No time window applies to the traces: the query
/// scans the project's traces once per call.
///
/// An empty `trace_ids` parses the filter and returns no batches without running a query:
/// the live stream validates its filter this way when it opens.
///
/// # Errors
///
/// [`RunError::Invalid`] for filter text; [`RunError::InvalidRequest`] for more than
/// [`MAX_REFRESH_IDS`] ids or an id that is not 32 hex digits; [`RunError::Timeout`].
pub async fn trace_refresh(
    ctx: &QueryContext,
    request: TraceRefreshRequest<'_>,
) -> Result<Vec<RecordBatch>, RunError> {
    let filter = match present(request.filter) {
        Some(text) => Some(ctx.parse_filter(FilterView::Traces, text)?),
        None => None,
    };
    if request.trace_ids.len() > MAX_REFRESH_IDS {
        return Err(RunError::InvalidRequest(format!(
            "at most {MAX_REFRESH_IDS} trace ids"
        )));
    }
    let ids = request
        .trace_ids
        .iter()
        .map(|id| hex_id(id, 32, "trace_id"))
        .collect::<Result<Vec<_>, _>>()?;
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    match tokio::time::timeout(request.timeout, refresh(ctx, &request, filter, &ids)).await {
        Err(_) => Err(RunError::Timeout),
        Ok(result) => result,
    }
}

async fn refresh(
    ctx: &QueryContext,
    request: &TraceRefreshRequest<'_>,
    filter: Option<Filter>,
    ids: &[String],
) -> Result<Vec<RecordBatch>, RunError> {
    let references_environment = filter
        .as_ref()
        .is_some_and(|filter| filter.references_environment);
    let mut frame = ctx.session().table(LIST).await.map_err(classify)?;
    let mut conjuncts = Vec::new();
    if request.strict_exclusions {
        conjuncts.push(strict_exclusions(ctx, FilterView::Traces, Some(ids)).await?);
    } else if let Some(expr) = exclusions(
        ctx,
        FilterView::Traces,
        request.include_eval,
        request.include_experiments,
        references_environment,
    )? {
        conjuncts.push(expr);
    }
    let literals = ids.iter().map(|id| lit(id.as_str())).collect();
    conjuncts.push(col(traces::TRACE_ID).in_list(literals, false));
    if let Some((text, window)) = request.search
        && let Some(text) = present(Some(text))
        && let Some(expr) = search(FilterView::Traces, text, Some(window), ctx).await?
    {
        conjuncts.push(expr);
    }
    if let Some(predicate) = conjuncts.into_iter().reduce(Expr::and) {
        frame = frame.filter(predicate).map_err(classify)?;
    }
    if let Some(filter) = filter {
        frame = filter.apply(frame, ctx).await.map_err(classify)?;
    }
    if request.strict_exclusions {
        frame = frame
            .select(TRACE_REFRESH_STRICT_COLUMNS.iter().copied().map(col))
            .map_err(classify)?;
    }
    frame.collect().await.map_err(|error| ctx.fail(error))
}

/// Spans of `trace_ids` that pass the exclusions and the filter, the first `per_trace` of each
/// trace by `start_time`, then `span_id` (D-I-9). The rows are the [`SPAN_REFRESH_COLUMNS`],
/// ordered by trace, then `start_time`, then `span_id`.
///
/// An empty `trace_ids` parses the filter and returns no batches without running a query.
///
/// # Errors
///
/// [`RunError::Invalid`] for filter text; [`RunError::InvalidRequest`] for more than
/// [`MAX_REFRESH_IDS`] ids, an id that is not 32 hex digits, or a `per_trace` outside
/// `1..=`[`MAX_REFRESH_SPANS_PER_TRACE`]; [`RunError::Timeout`].
pub async fn span_refresh(
    ctx: &QueryContext,
    request: SpanRefreshRequest<'_>,
) -> Result<Vec<RecordBatch>, RunError> {
    let filter = match present(request.filter) {
        Some(text) => Some(ctx.parse_filter(FilterView::Spans, text)?),
        None => None,
    };
    if request.trace_ids.len() > MAX_REFRESH_IDS {
        return Err(RunError::InvalidRequest(format!(
            "at most {MAX_REFRESH_IDS} trace ids"
        )));
    }
    let ids = request
        .trace_ids
        .iter()
        .map(|id| hex_id(id, 32, "trace_id"))
        .collect::<Result<Vec<_>, _>>()?;
    if !(1..=MAX_REFRESH_SPANS_PER_TRACE).contains(&request.per_trace) {
        return Err(RunError::InvalidRequest(format!(
            "per_trace must be between 1 and {MAX_REFRESH_SPANS_PER_TRACE}"
        )));
    }
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    match tokio::time::timeout(request.timeout, refresh_spans(ctx, &request, filter, &ids)).await {
        Err(_) => Err(RunError::Timeout),
        Ok(result) => result,
    }
}

async fn refresh_spans(
    ctx: &QueryContext,
    request: &SpanRefreshRequest<'_>,
    filter: Option<Filter>,
    ids: &[String],
) -> Result<Vec<RecordBatch>, RunError> {
    let references_environment = filter
        .as_ref()
        .is_some_and(|filter| filter.references_environment);
    let mut frame = ctx.session().table(SPANS).await.map_err(classify)?;
    let mut conjuncts = Vec::new();
    if request.strict_exclusions {
        conjuncts.push(strict_exclusions(ctx, FilterView::Spans, Some(ids)).await?);
    } else if let Some(expr) =
        exclusions(ctx, FilterView::Spans, false, false, references_environment)?
    {
        conjuncts.push(expr);
    }
    let literals = ids.iter().map(|id| lit(id.as_str())).collect();
    conjuncts.push(col(spans::TRACE_ID).in_list(literals, false));
    if let Some(predicate) = conjuncts.into_iter().reduce(Expr::and) {
        frame = frame.filter(predicate).map_err(classify)?;
    }
    if let Some(filter) = filter {
        frame = filter.apply(frame, ctx).await.map_err(classify)?;
    }
    let rank = row_number()
        .partition_by(vec![col(spans::TRACE_ID)])
        .order_by(vec![
            col(spans::START_TIME).sort(true, false),
            col(spans::SPAN_ID).sort(true, false),
        ])
        .build()
        .map_err(classify)?
        .alias(SPAN_RANK);
    frame = frame.window(vec![rank]).map_err(classify)?;
    let per_trace = u64::try_from(request.per_trace)
        .map_err(|error| RunError::InvalidRequest(format!("per_trace is out of range: {error}")))?;
    frame = frame
        .filter(col(SPAN_RANK).lt_eq(lit(per_trace)))
        .map_err(classify)?
        .sort(order_by(
            spans::TRACE_ID,
            &[spans::START_TIME, spans::SPAN_ID],
            SortOrder::Asc,
        ))
        .map_err(classify)?
        .select(SPAN_REFRESH_COLUMNS.iter().copied().map(col))
        .map_err(classify)?;
    frame.collect().await.map_err(|error| ctx.fail(error))
}

/// Counts the rows of the traces or spans view that match a filter in a window, at most
/// `cap + 1` (D-I-9). A result above `cap` means "more than `cap`".
///
/// # Errors
///
/// [`RunError::InvalidRequest`] for another view or a `cap` outside `1..=`[`MAX_MATCH_COUNT`];
/// [`RunError::Invalid`] for filter text; [`RunError::Timeout`]; DataFusion failures through
/// [`classify`] and `ctx.fail`.
pub async fn match_count(
    ctx: &QueryContext,
    request: MatchCountRequest<'_>,
) -> Result<usize, RunError> {
    let (table, view) = match request.view {
        ViewName::Traces => (LIST, FilterView::Traces),
        ViewName::Spans => (SPANS, FilterView::Spans),
        _ => {
            return Err(RunError::InvalidRequest(
                "match counts are for traces and spans only".to_owned(),
            ));
        }
    };
    if !(1..=MAX_MATCH_COUNT).contains(&request.cap) {
        return Err(RunError::InvalidRequest(format!(
            "cap must be between 1 and {MAX_MATCH_COUNT}"
        )));
    }
    let filter = match present(request.filter) {
        Some(text) => Some(ctx.parse_filter(view, text)?),
        None => None,
    };
    match tokio::time::timeout(
        request.timeout,
        count_matches(ctx, &request, table, view, filter),
    )
    .await
    {
        Err(_) => Err(RunError::Timeout),
        Ok(result) => result,
    }
}

async fn count_matches(
    ctx: &QueryContext,
    request: &MatchCountRequest<'_>,
    table: &str,
    view: FilterView,
    filter: Option<Filter>,
) -> Result<usize, RunError> {
    let references_environment = filter
        .as_ref()
        .is_some_and(|filter| filter.references_environment);
    let mut frame = ctx.session().table(table).await.map_err(classify)?;
    let mut conjuncts = vec![time_window(request.window)];
    if request.strict_exclusions {
        conjuncts.push(strict_exclusions(ctx, view, None).await?);
    } else if let Some(expr) = exclusions(ctx, view, false, false, references_environment)? {
        conjuncts.push(expr);
    }
    if let Some(predicate) = conjuncts.into_iter().reduce(Expr::and) {
        frame = frame.filter(predicate).map_err(classify)?;
    }
    if let Some(filter) = filter {
        frame = filter.apply(frame, ctx).await.map_err(classify)?;
    }
    frame
        .limit(0, Some(request.cap.saturating_add(1)))
        .map_err(classify)?
        .count()
        .await
        .map_err(|error| ctx.fail(error))
}

/// Copies of one row. Rows that agree on `key` and sort next to each other are copies; the
/// sort puts the copy with the greatest `newest` first, and that copy is the one kept.
struct Dedup {
    key: &'static [&'static str],
    newest: &'static str,
}

struct ListPlan<'a> {
    table: &'static str,
    view: FilterView,
    list: &'static str,
    sort: &'static str,
    ties: &'static [&'static str],
    dedup: Option<Dedup>,
    filter: Option<&'a str>,
    search: Option<&'a str>,
    scope: ListScope,
    strict: bool,
    order: SortOrder,
    limit: usize,
    cursor: Option<&'a str>,
    columns: Option<&'static [&'static str]>,
}

impl<'a> ListPlan<'a> {
    fn traces(request: &TraceListRequest<'a>) -> Self {
        Self {
            table: LIST,
            view: FilterView::Traces,
            list: LIST,
            sort: request.sort.column(),
            ties: TRACE_TIES,
            dedup: None,
            filter: request.filter,
            search: request.search,
            scope: request.scope,
            strict: request.strict_exclusions,
            order: request.order,
            limit: request.limit,
            cursor: request.cursor,
            columns: None,
        }
    }

    fn spans(request: &SpanListRequest<'a>) -> Self {
        Self {
            table: SPANS,
            view: FilterView::Spans,
            list: SPANS,
            sort: request.sort.column(),
            ties: SPAN_TIES,
            dedup: Some(Dedup {
                key: SPAN_KEY,
                newest: spans::INGEST_TIME,
            }),
            filter: request.filter,
            search: request.search,
            scope: request.scope,
            strict: request.strict_exclusions,
            order: request.order,
            limit: request.limit,
            cursor: request.cursor,
            columns: Some(SPAN_LIST_COLUMNS),
        }
    }
}

async fn run_list(
    ctx: &QueryContext,
    plan: ListPlan<'_>,
    timeout: Duration,
    cap: usize,
) -> Result<Page, RunError> {
    run_timed(plan.limit, timeout, async {
        // A de-duplicating list reads a margin for the copies it drops. When the rows left after
        // de-duplication don't fill the page and the read stopped at its own limit, read again
        // with a doubled limit, up to `cap`. Without de-duplication the first read always settles it.
        let mut fetch = plan.limit + 1;
        if plan.dedup.is_some() {
            fetch *= 2;
        }
        fetch = fetch.min(cap);
        loop {
            let frame = list_frame(ctx, &plan, fetch).await?;
            let batches = frame.collect().await.map_err(|error| ctx.fail(error))?;
            let read: usize = batches.iter().map(RecordBatch::num_rows).sum();
            let batches = match &plan.dedup {
                Some(dedup) => drop_adjacent_copies(&batches, dedup.key)?,
                None => batches,
            };
            let distinct: usize = batches.iter().map(RecordBatch::num_rows).sum();
            // `limit + 1` distinct rows, or a read that ended before its limit, settle the page.
            if distinct > plan.limit || read < fetch {
                return assemble(
                    batches, plan.limit, plan.list, plan.sort, plan.order, plan.ties,
                );
            }
            if fetch >= cap {
                return capped_page(batches, &plan);
            }
            fetch = fetch.saturating_mul(2).min(cap);
        }
    })
    .await
}

/// The page when the read cap comes first: the distinct rows read, at most `limit`. When fewer
/// than `limit` rows came back, the page has a cursor anyway, taken from its last row. The
/// cursor's strict `after` skips the rest of that row's copies, and copies that were read but
/// dropped share its key.
fn capped_page(batches: Vec<RecordBatch>, plan: &ListPlan<'_>) -> Result<Page, RunError> {
    let distinct: usize = batches.iter().map(RecordBatch::num_rows).sum();
    if distinct > plan.limit {
        return assemble(
            batches, plan.limit, plan.list, plan.sort, plan.order, plan.ties,
        );
    }
    // One more row than the page: `assemble` then cuts the page at `distinct` rows and cursors
    // from its last row, which is the last distinct row.
    let mut batches = batches;
    if let Some(last) = batches.iter().rev().find(|batch| batch.num_rows() > 0) {
        let tail = last.slice(last.num_rows() - 1, 1);
        batches.push(tail);
    }
    assemble(
        batches, distinct, plan.list, plan.sort, plan.order, plan.ties,
    )
}

/// Drops each row whose `key` columns equal the previous row's. The rows arrive in sort order,
/// so the copies of one row are adjacent and the first of each run is kept. One pass with no
/// window or aggregate: the batches keep their order and only lose rows.
fn drop_adjacent_copies(
    batches: &[RecordBatch],
    key: &[&str],
) -> Result<Vec<RecordBatch>, RunError> {
    let mut previous: Option<Vec<ScalarValue>> = None;
    let mut kept = Vec::with_capacity(batches.len());
    for batch in batches {
        let columns = key
            .iter()
            .map(|name| {
                batch.column_by_name(name).map(Arc::clone).ok_or_else(|| {
                    RunError::Internal(DataFusionError::Internal(format!(
                        "missing key column {name}"
                    )))
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let mut keep = Vec::with_capacity(batch.num_rows());
        for row in 0..batch.num_rows() {
            let values = columns
                .iter()
                .map(|column| ScalarValue::try_from_array(column.as_ref(), row))
                .collect::<Result<Vec<_>, _>>()
                .map_err(RunError::Internal)?;
            keep.push(previous.as_ref() != Some(&values));
            previous = Some(values);
        }
        let filtered = filter_record_batch(batch, &BooleanArray::from(keep)).map_err(|error| {
            RunError::Internal(DataFusionError::Internal(format!(
                "dropping copies: {error}"
            )))
        })?;
        if filtered.num_rows() > 0 {
            kept.push(filtered);
        }
    }
    Ok(kept)
}

async fn run_timed<F>(limit: usize, timeout: Duration, future: F) -> Result<Page, RunError>
where
    F: Future<Output = Result<Page, RunError>>,
{
    if !(1..=super::MAX_PAGE).contains(&limit) {
        return Err(RunError::InvalidRequest(
            "limit must be between 1 and 1000".to_owned(),
        ));
    }
    match tokio::time::timeout(timeout, future).await {
        Err(_) => Err(RunError::Timeout),
        Ok(result) => result,
    }
}

/// The planned frame, before it is collected. Tests read the plan from this.
async fn trace_frame(
    ctx: &QueryContext,
    request: &TraceListRequest<'_>,
) -> Result<DataFrame, RunError> {
    list_frame(ctx, &ListPlan::traces(request), request.limit + 1).await
}

/// The physical plan A2 builds for `trace_list`, as a display string.
///
/// Exposed for the `#[ignore]` M3 benchmark (`tests/trace_bench.rs`) to confirm
/// the real list query — the `traces` view plus its default environment and
/// experiment exclusions — is rewritten to list mode, not just a hand-written
/// SQL query. Reads call `trace_list` itself, never this.
#[doc(hidden)]
pub async fn trace_list_plan(
    ctx: &QueryContext,
    request: TraceListRequest<'_>,
) -> Result<String, RunError> {
    let frame = trace_frame(ctx, &request).await?;
    let plan = frame.create_physical_plan().await.map_err(classify)?;
    Ok(format!(
        "{}",
        datafusion::physical_plan::displayable(plan.as_ref()).indent(true)
    ))
}

/// The rows of `table` that a list of `view` selects before its cursor, sort and limit: the
/// window on `start_time`, the default exclusions (the strict ones when `strict`), the search
/// and the filter. The annotation-queue scans of spans select through this too, so they select
/// the same rows as the lists.
pub(crate) async fn scope_frame(
    ctx: &QueryContext,
    view: FilterView,
    table: &str,
    filter: Option<Filter>,
    search_text: Option<&str>,
    scope: ListScope,
    strict: bool,
) -> Result<DataFrame, RunError> {
    let references_environment = filter
        .as_ref()
        .is_some_and(|filter| filter.references_environment);
    let mut frame = ctx.session().table(table).await.map_err(classify)?;
    let mut conjuncts = Vec::new();
    if let Some(window) = scope.window {
        conjuncts.push(time_window(window));
    }
    if strict {
        conjuncts.push(strict_exclusions(ctx, view, None).await?);
    } else if let Some(expr) = exclusions(
        ctx,
        view,
        scope.include_eval,
        scope.include_experiments,
        references_environment,
    )? {
        conjuncts.push(expr);
    }
    if let Some(text) = present(search_text)
        && let Some(expr) = search(view, text, scope.window, ctx).await?
    {
        conjuncts.push(expr);
    }
    if let Some(predicate) = conjuncts.into_iter().reduce(Expr::and) {
        frame = frame.filter(predicate).map_err(classify)?;
    }
    if let Some(filter) = filter {
        frame = filter.apply(frame, ctx).await.map_err(classify)?;
    }
    Ok(frame)
}

/// The list's frame, sorted and cut to `fetch` rows. A de-duplicating plan also sorts by the
/// newest copy's `ingest_time` last, and the frame keeps only the listed columns.
async fn list_frame(
    ctx: &QueryContext,
    plan: &ListPlan<'_>,
    fetch: usize,
) -> Result<DataFrame, RunError> {
    let filter = match present(plan.filter) {
        Some(text) => Some(ctx.parse_filter(plan.view, text)?),
        None => None,
    };
    let cursor = match plan.cursor {
        Some(text) => {
            let cursor = decode(text, plan.list, plan.sort, plan.order)?;
            if cursor.tie_len() != plan.ties.len() {
                return Err(RunError::InvalidRequest("invalid cursor".to_owned()));
            }
            Some(cursor)
        }
        None => None,
    };

    let mut frame = scope_frame(
        ctx,
        plan.view,
        plan.table,
        filter,
        plan.search,
        plan.scope,
        plan.strict,
    )
    .await?;
    if let Some(cursor) = &cursor {
        frame = frame
            .filter(after(cursor, plan.sort, plan.ties))
            .map_err(classify)?;
    }
    let mut sorts = order_by(plan.sort, plan.ties, plan.order);
    if let Some(dedup) = &plan.dedup {
        sorts.push(col(dedup.newest).sort(false, false));
    }
    frame = frame
        .sort(sorts)
        .map_err(classify)?
        .limit(0, Some(fetch))
        .map_err(classify)?;
    if let Some(columns) = plan.columns {
        frame = frame
            .select(columns.iter().copied().map(col))
            .map_err(classify)?;
    }
    Ok(frame)
}

async fn load_trace(
    ctx: &QueryContext,
    trace_id: &str,
    cap: usize,
) -> Result<Option<TraceDetailData>, RunError> {
    let trace = ctx
        .session()
        .table(LIST)
        .await
        .map_err(classify)?
        .filter(col(traces::TRACE_ID).eq(lit(trace_id)))
        .map_err(classify)?;
    let trace = trace.collect().await.map_err(|error| ctx.fail(error))?;
    let Some(trace) = single_row(&trace)? else {
        return Ok(None);
    };
    let spans = ctx
        .session()
        .table(SPANS)
        .await
        .map_err(classify)?
        .filter(col(spans::TRACE_ID).eq(lit(trace_id)))
        .map_err(classify)?
        .select(SPAN_LIST_COLUMNS.iter().copied().map(col))
        .map_err(classify)?
        .sort(order_by(
            spans::START_TIME,
            &[spans::SPAN_ID],
            SortOrder::Asc,
        ))
        .map_err(classify)?
        .limit(0, Some(cap + 1))
        .map_err(classify)?;
    let (spans, truncated) = cap_rows(spans.collect().await.map_err(|error| ctx.fail(error))?, cap);
    Ok(Some(TraceDetailData {
        trace,
        spans,
        truncated,
    }))
}

async fn load_span(
    ctx: &QueryContext,
    trace_id: &str,
    span_id: &str,
) -> Result<Option<RecordBatch>, RunError> {
    let frame = ctx
        .session()
        .table(SPANS)
        .await
        .map_err(classify)?
        .filter(
            col(spans::TRACE_ID)
                .eq(lit(trace_id))
                .and(col(spans::SPAN_ID).eq(lit(span_id))),
        )
        .map_err(classify)?
        .sort(order_by(spans::INGEST_TIME, &[], SortOrder::Desc))
        .map_err(classify)?
        .limit(0, Some(1))
        .map_err(classify)?;
    let batches = frame.collect().await.map_err(|error| ctx.fail(error))?;
    single_row(&batches)
}

fn hex_id(text: &str, width: usize, name: &str) -> Result<String, RunError> {
    if text.len() == width && text.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Ok(text.to_ascii_lowercase());
    }
    Err(RunError::InvalidRequest(format!(
        "{name} must be {width} hex digits"
    )))
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

fn cap_rows(batches: Vec<RecordBatch>, limit: usize) -> (Vec<RecordBatch>, bool) {
    let total: usize = batches.iter().map(RecordBatch::num_rows).sum();
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

#[cfg(test)]
mod tests {
    use std::num::NonZeroUsize;
    use std::path::PathBuf;
    use std::str::FromStr;
    use std::sync::Arc;
    use std::time::Duration;

    use arrow::json::{ArrayWriter, WriterBuilder};
    use arrow::record_batch::RecordBatch;
    use cetra_core::enums::{SpanKind, SpanStatus};
    use cetra_core::ids::{SpanId, TraceId};
    use cetra_core::names::{environment, metadata_key};
    use cetra_core::schema::{SPANS_LIGHT_COLUMNS, spans};
    use cetra_core::span::Span;
    use cetra_core::time_serde::parse_rfc3339;
    use datafusion::execution::runtime_env::RuntimeEnv;
    use datafusion::physical_plan::displayable;
    use serde_json::Value;

    use super::{
        MAX_FETCH, MAX_MATCH_COUNT, MAX_REFRESH_IDS, MAX_REFRESH_SPANS_PER_TRACE,
        MatchCountRequest, SPAN_LIST_COLUMNS, SPAN_REFRESH_COLUMNS, SpanListRequest,
        SpanRefreshRequest, SpanSort, TRACE_REFRESH_STRICT_COLUMNS, TraceListRequest,
        TraceRefreshRequest, TraceSort, TraceSpansRequest, match_count, span_detail, span_list,
        span_list_capped, span_refresh, trace_detail, trace_detail_limited, trace_frame,
        trace_list, trace_refresh, trace_spans,
    };
    use crate::query_error::{Position, RunError};
    use crate::reads::{ListScope, Page, SortOrder};
    use crate::test_support::{
        dataset_scores, dataset_spans, example_query, live_scores_table, span_row, spans_table,
        traces_table,
    };
    use crate::views::{ProjectQuery, QueryContext, TimeWindow, ViewName};

    const TIMEOUT: Duration = Duration::from_secs(30);

    fn context() -> QueryContext {
        QueryContext::new(example_query(), None).expect("context")
    }

    fn scope(window: Option<TimeWindow>, include_eval: bool) -> ListScope {
        ListScope {
            window,
            include_eval,
            include_experiments: false,
        }
    }

    fn listed<'a>(
        scope: ListScope,
        sort: TraceSort,
        order: SortOrder,
        limit: usize,
    ) -> TraceListRequest<'a> {
        TraceListRequest {
            filter: None,
            search: None,
            strict_exclusions: false,
            scope,
            sort,
            order,
            limit,
            cursor: None,
            timeout: TIMEOUT,
        }
    }

    fn fixture(name: &str) -> Value {
        let text = match name {
            "traces.json" => include_str!("../../../../fixtures/api/responses/traces.json"),
            "traces-include-eval.json" => {
                include_str!("../../../../fixtures/api/responses/traces-include-eval.json")
            }
            "spans.json" => include_str!("../../../../fixtures/api/responses/spans.json"),
            other => panic!("unknown fixture {other}"),
        };
        serde_json::from_str(text).expect(name)
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

    fn ids(page: &Page) -> Vec<String> {
        rows(page)
            .iter()
            .map(|row| row["trace_id"].as_str().expect("trace_id").to_owned())
            .collect()
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

    fn assert_scalars(page: &Page, fixture: &Value) {
        let expected = fixture["data"].as_array().expect("data");
        let actual = rows(page);
        assert_eq!(actual.len(), expected.len());
        let fields = [
            "name",
            "status",
            "span_count",
            "error_count",
            "cost_total",
            "total_tokens",
            "tags",
            "models",
            "metadata",
            "session_id",
            "user_id",
            "environment",
        ];
        for (row, trace) in actual.iter().zip(expected) {
            assert_eq!(row["trace_id"], trace["trace_id"]);
            for field in fields {
                let expected = trace.get(field).unwrap_or(&Value::Null);
                assert!(
                    values_match(&row[field], expected),
                    "{field}: {} vs {expected}",
                    row[field]
                );
            }
        }
    }

    fn nanos(text: &str) -> i64 {
        parse_rfc3339(text).expect(text)
    }

    fn day() -> TimeWindow {
        TimeWindow::new(nanos("2026-01-01T00:00:00Z"), nanos("2026-01-02T00:00:00Z")).expect("day")
    }

    fn text_has(span: &Span, needle: &str) -> bool {
        let needle = needle.to_lowercase();
        [
            span.input.as_deref(),
            span.output.as_deref(),
            Some(span.name.as_str()),
            span.system_instructions.as_deref(),
        ]
        .into_iter()
        .flatten()
        .any(|text| text.to_lowercase().contains(&needle))
    }

    const ID_ANSWER: &str = "a1000000000000000000000000000001";
    const ID_DELIVERY: &str = "a1000000000000000000000000000002";
    const ID_QUESTION: &str = "a1000000000000000000000000000003";
    const ID_EVAL: &str = "a1000000000000000000000000000005";

    fn refresh_request<'a>(
        trace_ids: &'a [&'a str],
        filter: Option<&'a str>,
        search: Option<(&'a str, TimeWindow)>,
        include_eval: bool,
    ) -> TraceRefreshRequest<'a> {
        TraceRefreshRequest {
            filter,
            search,
            trace_ids,
            include_eval,
            include_experiments: false,
            strict_exclusions: false,
            timeout: TIMEOUT,
        }
    }

    /// Trace ids of the rows `trace_refresh` returned, sorted.
    fn refreshed_ids(batches: &[RecordBatch]) -> Vec<String> {
        let mut ids: Vec<String> = json_rows(batches)
            .iter()
            .map(|row| row["trace_id"].as_str().expect("trace_id").to_owned())
            .collect();
        ids.sort();
        ids
    }

    #[tokio::test]
    async fn refresh_keeps_the_listed_traces_and_the_default_exclusions() {
        let ctx = context();
        let ids = [ID_ANSWER, ID_QUESTION, ID_EVAL];
        let batches = trace_refresh(&ctx, refresh_request(&ids, None, None, false))
            .await
            .expect("refresh");
        assert_eq!(refreshed_ids(&batches), [ID_ANSWER, ID_QUESTION]);

        let batches = trace_refresh(&ctx, refresh_request(&ids, None, None, true))
            .await
            .expect("refresh with eval");
        assert_eq!(refreshed_ids(&batches), [ID_ANSWER, ID_QUESTION, ID_EVAL]);
    }

    #[tokio::test]
    async fn refresh_applies_the_filter() {
        let ctx = context();
        let ids = [ID_ANSWER, ID_QUESTION];
        let filter = "name = 'answer-question'";
        let batches = trace_refresh(&ctx, refresh_request(&ids, Some(filter), None, false))
            .await
            .expect("filter");
        assert_eq!(refreshed_ids(&batches), [ID_QUESTION]);
    }

    #[tokio::test]
    async fn refresh_search_uses_its_window() {
        let ctx = context();
        let ids = [ID_ANSWER, ID_DELIVERY];
        let inside = TimeWindow::new(nanos("2026-01-01T10:05:00Z"), nanos("2026-01-01T10:06:00Z"))
            .expect("window");
        let batches = trace_refresh(
            &ctx,
            refresh_request(&ids, None, Some(("delivery", inside)), false),
        )
        .await
        .expect("search inside");
        assert_eq!(refreshed_ids(&batches), [ID_DELIVERY]);

        let before = TimeWindow::new(nanos("2026-01-01T10:00:00Z"), nanos("2026-01-01T10:01:00Z"))
            .expect("window");
        let batches = trace_refresh(
            &ctx,
            refresh_request(&ids, None, Some(("delivery", before)), false),
        )
        .await
        .expect("search before");
        assert!(refreshed_ids(&batches).is_empty());
    }

    #[tokio::test]
    async fn refresh_without_ids_only_parses_the_filter() {
        let ctx = context();
        let batches = trace_refresh(&ctx, refresh_request(&[], Some("name = 'x'"), None, false))
            .await
            .expect("valid filter");
        assert!(batches.is_empty());

        match trace_refresh(&ctx, refresh_request(&[], Some("name ="), None, false)).await {
            Err(RunError::Invalid(_)) => {}
            other => panic!("expected an invalid filter, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn refresh_rejects_too_many_ids_and_bad_ids() {
        let ctx = context();
        let many = vec![ID_ANSWER; MAX_REFRESH_IDS + 1];
        match trace_refresh(&ctx, refresh_request(&many, None, None, false)).await {
            Err(RunError::InvalidRequest(_)) => {}
            other => panic!("expected too many ids, got {other:?}"),
        }
        let bad = ["zz"];
        match trace_refresh(&ctx, refresh_request(&bad, None, None, false)).await {
            Err(RunError::InvalidRequest(_)) => {}
            other => panic!("expected a bad id, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn default_list_matches_the_trace_fixture() {
        let page = trace_list(
            &context(),
            listed(
                scope(None, false),
                TraceSort::StartTime,
                SortOrder::Desc,
                50,
            ),
        )
        .await
        .expect("list");
        let fixture = fixture("traces.json");
        let expected: Vec<_> = fixture["data"]
            .as_array()
            .expect("data")
            .iter()
            .map(|row| row["trace_id"].as_str().expect("id").to_owned())
            .collect();
        assert_eq!(ids(&page), expected);
        assert_scalars(&page, &fixture);
        assert!(page.next_cursor.is_none());
    }

    #[tokio::test]
    async fn include_eval_matches_the_fixture_order() {
        let page = trace_list(
            &context(),
            listed(scope(None, true), TraceSort::StartTime, SortOrder::Desc, 50),
        )
        .await
        .expect("list");
        let fixture = fixture("traces-include-eval.json");
        let expected: Vec<_> = fixture["data"]
            .as_array()
            .expect("data")
            .iter()
            .map(|row| row["trace_id"].as_str().expect("id").to_owned())
            .collect();
        assert_eq!(ids(&page), expected);
    }

    #[tokio::test]
    async fn pages_cover_every_sort_including_nulls() {
        let ctx = context();
        let sorts = [
            TraceSort::StartTime,
            TraceSort::Duration,
            TraceSort::CostTotal,
            TraceSort::TotalTokens,
        ];
        let orders = [SortOrder::Asc, SortOrder::Desc];
        for sort in sorts {
            for order in orders {
                let full = trace_list(&ctx, listed(scope(None, true), sort, order, 50))
                    .await
                    .expect("full");
                let expected = ids(&full);
                assert!(full.next_cursor.is_none());
                let mut walked = Vec::new();
                let mut cursor = None;
                loop {
                    let mut request = listed(scope(None, true), sort, order, 2);
                    request.cursor = cursor.as_deref();
                    let page = trace_list(&ctx, request).await.expect("page");
                    let page_ids = ids(&page);
                    assert!(page_ids.len() <= 2);
                    walked.extend(page_ids);
                    match page.next_cursor {
                        Some(next) => cursor = Some(next),
                        None => break,
                    }
                }
                assert_eq!(walked, expected, "{sort:?} {order:?}");
            }
        }
        let costs = trace_list(
            &ctx,
            listed(scope(None, true), TraceSort::CostTotal, SortOrder::Desc, 50),
        )
        .await
        .expect("costs");
        assert!(
            rows(&costs).iter().any(|row| row["cost_total"].is_null()),
            "cost_total has a null"
        );
        let tokens = trace_list(
            &ctx,
            listed(
                scope(None, true),
                TraceSort::TotalTokens,
                SortOrder::Asc,
                50,
            ),
        )
        .await
        .expect("tokens");
        assert!(
            rows(&tokens)
                .iter()
                .any(|row| row["total_tokens"].is_null()),
            "total_tokens has a null"
        );
    }

    #[tokio::test]
    async fn a_cursor_for_another_sort_is_rejected() {
        let ctx = context();
        let mut first = listed(scope(None, true), TraceSort::CostTotal, SortOrder::Desc, 2);
        let page = trace_list(&ctx, first.clone()).await.expect("page");
        let cursor = page.next_cursor.expect("next");
        first.sort = TraceSort::StartTime;
        first.cursor = Some(&cursor);
        let error = trace_list(&ctx, first).await.expect_err("sort");
        assert_eq!(
            error.to_string(),
            "the cursor belongs to another list or sort order"
        );
        for text in ["zz", hex::encode(b"{}").as_str()] {
            let mut request = listed(scope(None, false), TraceSort::StartTime, SortOrder::Desc, 2);
            request.cursor = Some(text);
            let error = trace_list(&ctx, request).await.expect_err(text);
            assert_eq!(error.to_string(), "invalid cursor");
        }
    }

    #[tokio::test]
    async fn a_filter_error_keeps_its_position_and_a_score_filter_matches() {
        let ctx = context();
        let mut request = listed(
            scope(None, false),
            TraceSort::StartTime,
            SortOrder::Desc,
            50,
        );
        request.filter = Some("nme = 'a'");
        match trace_list(&ctx, request).await {
            Err(RunError::Invalid(error)) => {
                assert_eq!(error.message, "unknown column `nme`");
                assert_eq!(error.position, Some(Position { line: 1, column: 1 }));
            }
            other => panic!("expected an invalid filter, got {other:?}"),
        }

        let mut request = listed(
            scope(None, false),
            TraceSort::StartTime,
            SortOrder::Desc,
            50,
        );
        request.filter = Some("score('helpfulness') >= 0.5");
        let page = trace_list(&ctx, request).await.expect("score");
        let fixture = fixture("traces.json");
        let expected: Vec<_> = fixture["data"]
            .as_array()
            .expect("data")
            .iter()
            .filter(|row| {
                row["scores"].as_array().is_some_and(|scores| {
                    scores.iter().any(|score| {
                        score["name"] == "helpfulness"
                            && score["value"]["value"]
                                .as_f64()
                                .is_some_and(|value| value >= 0.5)
                    })
                })
            })
            .map(|row| row["trace_id"].as_str().expect("id").to_owned())
            .collect();
        assert!(!expected.is_empty());
        assert_eq!(ids(&page), expected);
    }

    #[tokio::test]
    async fn search_uses_the_window_and_rejects_a_missing_one() {
        let ctx = context();
        let window = day();
        let mut request = listed(
            scope(Some(window), false),
            TraceSort::StartTime,
            SortOrder::Desc,
            50,
        );
        request.search = Some("order");
        let page = trace_list(&ctx, request).await.expect("search");
        let matching: std::collections::BTreeSet<String> = dataset_spans()
            .iter()
            .filter(|span| {
                text_has(span, "order") && (window.from()..window.to()).contains(&span.start_time)
            })
            .map(|span| span.trace_id.to_hex())
            .collect();
        let fixture = fixture("traces.json");
        let expected: Vec<_> = fixture["data"]
            .as_array()
            .expect("data")
            .iter()
            .filter(|row| {
                let id = row["trace_id"].as_str().expect("id");
                let start = nanos(row["start_time"].as_str().expect("start"));
                matching.contains(id) && (window.from()..window.to()).contains(&start)
            })
            .map(|row| row["trace_id"].as_str().expect("id").to_owned())
            .collect();
        assert!(!expected.is_empty());
        assert_eq!(ids(&page), expected);

        let mut request = listed(
            scope(None, false),
            TraceSort::StartTime,
            SortOrder::Desc,
            50,
        );
        request.search = Some("order");
        match trace_list(&ctx, request).await {
            Err(RunError::InvalidRequest(message)) => {
                assert_eq!(message, "search needs a time range");
            }
            other => panic!("expected a missing window, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_window_keeps_traces_starting_inside_it() {
        let from = nanos("2026-01-01T10:05:00Z");
        let to = nanos("2026-01-01T10:21:00Z");
        let window = TimeWindow::new(from, to).expect("window");
        let page = trace_list(
            &context(),
            listed(
                scope(Some(window), false),
                TraceSort::StartTime,
                SortOrder::Desc,
                50,
            ),
        )
        .await
        .expect("window");
        let fixture = fixture("traces.json");
        let expected: Vec<_> = fixture["data"]
            .as_array()
            .expect("data")
            .iter()
            .filter(|row| {
                let start = nanos(row["start_time"].as_str().expect("start"));
                (from..to).contains(&start)
            })
            .map(|row| row["trace_id"].as_str().expect("id").to_owned())
            .collect();
        assert_eq!(
            expected,
            [
                "a1000000000000000000000000000004",
                "a1000000000000000000000000000003",
                "a1000000000000000000000000000002",
            ]
        );
        assert_eq!(ids(&page), expected);
    }

    #[tokio::test]
    async fn limit_outside_the_page_is_rejected() {
        let ctx = context();
        for limit in [0, 1001] {
            let error = trace_list(
                &ctx,
                listed(
                    scope(None, false),
                    TraceSort::StartTime,
                    SortOrder::Desc,
                    limit,
                ),
            )
            .await
            .expect_err("limit");
            assert_eq!(
                error.to_string(),
                "limit must be between 1 and 1000",
                "{limit}"
            );
        }
    }

    #[tokio::test]
    async fn the_default_plan_sorts_with_a_fetch() {
        let ctx = context();
        let request = listed(
            scope(None, false),
            TraceSort::StartTime,
            SortOrder::Desc,
            50,
        );
        let frame = trace_frame(&ctx, &request).await.expect("frame");
        let physical = frame.create_physical_plan().await.expect("physical");
        let physical = format!("{}", displayable(physical.as_ref()).indent(true));
        assert!(
            physical.contains("SortExec") && physical.contains("fetch="),
            "{physical}"
        );
    }

    fn context_for(spans: &[Span]) -> QueryContext {
        QueryContext::new(
            ProjectQuery {
                spans: spans_table(spans),
                traces: traces_table(spans),
                scores: live_scores_table(&dataset_scores()),
                runtime: Arc::new(RuntimeEnv::default()),
                target_partitions: NonZeroUsize::new(1).expect("one partition"),
                optimizer_rules: Vec::new(),

                suspects: crate::verify::Suspects::detached(),
                visible_from: None,
            },
            None,
        )
        .expect("context")
    }

    fn fixture_file(path: &str) -> Value {
        let full = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join(path);
        let text = std::fs::read_to_string(&full)
            .unwrap_or_else(|error| panic!("{}: {error}", full.display()));
        serde_json::from_str(&text).unwrap_or_else(|error| panic!("{path}: {error}"))
    }

    fn span_request<'a>(
        scope: ListScope,
        sort: SpanSort,
        order: SortOrder,
        limit: usize,
    ) -> SpanListRequest<'a> {
        SpanListRequest {
            filter: None,
            search: None,
            strict_exclusions: false,
            scope,
            sort,
            order,
            limit,
            cursor: None,
            timeout: TIMEOUT,
        }
    }

    fn pairs(rows: &[Value], left: &str, right: &str) -> Vec<(String, String)> {
        rows.iter()
            .map(|row| {
                (
                    row[left].as_str().expect(left).to_owned(),
                    row[right].as_str().expect(right).to_owned(),
                )
            })
            .collect()
    }

    #[test]
    fn span_list_columns_follow_the_light_projection() {
        let mut expected = Vec::new();
        for name in SPANS_LIGHT_COLUMNS {
            if *name == spans::PROJECT_ID {
                continue;
            }
            expected.push(*name);
            if *name == spans::DURATION_NS {
                expected.push("duration_ms");
            }
        }
        assert_eq!(SPAN_LIST_COLUMNS, expected.as_slice());
    }

    #[tokio::test]
    async fn default_span_list_matches_the_fixture() {
        let page = span_list(
            &context(),
            span_request(scope(None, false), SpanSort::StartTime, SortOrder::Desc, 50),
        )
        .await
        .expect("list");
        let fixture = fixture("spans.json");
        let expected = fixture["data"].as_array().expect("data");
        let actual = rows(&page);
        assert_eq!(
            pairs(&actual, "trace_id", "span_id"),
            pairs(expected, "trace_id", "span_id")
        );
        assert_eq!(actual.len(), expected.len());
        for (row, span) in actual.iter().zip(expected) {
            let object = span.as_object().expect("span");
            for (key, value) in object {
                assert!(
                    values_match(&row[key], value),
                    "{key}: {} vs {value}",
                    row[key]
                );
            }
        }
        let schema = page.batches[0].schema();
        let names: Vec<_> = schema
            .fields()
            .iter()
            .map(|field| field.name().as_str())
            .collect();
        assert_eq!(names, SPAN_LIST_COLUMNS);
        assert!(page.next_cursor.is_none());
    }

    #[tokio::test]
    async fn span_pages_cover_every_sort_including_nulls() {
        let ctx = context();
        let sorts = [
            SpanSort::StartTime,
            SpanSort::Duration,
            SpanSort::CostTotal,
            SpanSort::TotalTokens,
        ];
        for sort in sorts {
            for order in [SortOrder::Asc, SortOrder::Desc] {
                let full = span_list(&ctx, span_request(scope(None, true), sort, order, 50))
                    .await
                    .expect("full");
                let expected = pairs(&rows(&full), "trace_id", "span_id");
                assert!(full.next_cursor.is_none());
                let mut walked = Vec::new();
                let mut cursor = None;
                loop {
                    let mut request = span_request(scope(None, true), sort, order, 2);
                    request.cursor = cursor.as_deref();
                    let page = span_list(&ctx, request).await.expect("page");
                    let page_ids = pairs(&rows(&page), "trace_id", "span_id");
                    assert!(page_ids.len() <= 2);
                    walked.extend(page_ids);
                    match page.next_cursor {
                        Some(next) => cursor = Some(next),
                        None => break,
                    }
                }
                assert_eq!(walked, expected, "{sort:?} {order:?}");
            }
        }
        let costs = span_list(
            &ctx,
            span_request(scope(None, true), SpanSort::CostTotal, SortOrder::Desc, 50),
        )
        .await
        .expect("costs");
        assert!(
            rows(&costs).iter().any(|row| row["cost_total"].is_null()),
            "cost_total has a null"
        );
        let tokens = span_list(
            &ctx,
            span_request(scope(None, true), SpanSort::TotalTokens, SortOrder::Asc, 50),
        )
        .await
        .expect("tokens");
        assert!(
            rows(&tokens)
                .iter()
                .any(|row| row["total_tokens"].is_null()),
            "total_tokens has a null"
        );
    }

    /// `copies` copies of one span starting at `start`. Copy `i` (from 1) has `ingest_time = i`
    /// and `name = "copy i"`, so the newest copy is the last one.
    fn copies_of(trace: u16, span: u8, start: i64, copies: i64) -> Vec<Span> {
        (1..=copies)
            .map(|ingest| {
                let mut copy = span_row(trace, span, start, ingest);
                copy.name = format!("copy {ingest}");
                copy
            })
            .collect()
    }

    /// (trace_id, span_id, name) of a span row of the test project.
    fn expected_row(trace: u16, span: u8, name: &str) -> (String, String, String) {
        let row = span_row(trace, span, 0, 0);
        (row.trace_id.to_hex(), row.span_id.to_hex(), name.to_owned())
    }

    fn listed_triples(rows: &[Value]) -> Vec<(String, String, String)> {
        rows.iter()
            .map(|row| {
                (
                    row["trace_id"].as_str().expect("trace_id").to_owned(),
                    row["span_id"].as_str().expect("span_id").to_owned(),
                    row["name"].as_str().expect("name").to_owned(),
                )
            })
            .collect()
    }

    /// Every row of the span list read in pages of `limit`, with a read cap of `cap`, in order.
    async fn walk_spans(ctx: &QueryContext, limit: usize, cap: usize) -> Vec<Value> {
        let mut walked = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let mut request = span_request(
                scope(None, true),
                SpanSort::StartTime,
                SortOrder::Desc,
                limit,
            );
            request.cursor = cursor.as_deref();
            let page = span_list_capped(ctx, request, cap).await.expect("page");
            assert!(row_count(&page.batches) <= limit);
            walked.extend(rows(&page));
            match page.next_cursor {
                Some(next) => cursor = Some(next),
                None => return walked,
            }
        }
    }

    #[tokio::test]
    async fn span_list_shows_a_resent_span_once() {
        let ctx = context_for(&copies_of(0, 0, 1_000, 2));
        let page = span_list(
            &ctx,
            span_request(scope(None, true), SpanSort::StartTime, SortOrder::Desc, 50),
        )
        .await
        .expect("list");
        assert_eq!(
            listed_triples(&rows(&page)),
            vec![expected_row(0, 0, "copy 2")]
        );
        assert!(page.next_cursor.is_none());
    }

    #[tokio::test]
    async fn span_list_pages_are_independent_of_page_size() {
        // Traces 0 and 1 start together, so ties cut across pages. Trace 0 and 2 re-send
        // span 1; trace 3 span 0 is sent five times, so one page needs more rows than it asks.
        let mut spans = Vec::new();
        let mut expected = Vec::new();
        for trace in 0..4u16 {
            let start = 1_000 * (1 + i64::from(trace / 2));
            for span in 0..3u8 {
                let copies = match (trace, span) {
                    (3, 0) => 5,
                    (0 | 2, 1) => 2,
                    _ => 1,
                };
                spans.extend(copies_of(trace, span, start, copies));
                expected.push(expected_row(trace, span, &format!("copy {copies}")));
            }
        }
        let ctx = context_for(&spans);
        let single = listed_triples(&walk_spans(&ctx, 1000, MAX_FETCH).await);
        let mut sorted = single.clone();
        sorted.sort();
        expected.sort();
        assert_eq!(sorted, expected, "one row per span, the newest copy");
        for limit in [1, 2, 3, 1000] {
            assert_eq!(
                listed_triples(&walk_spans(&ctx, limit, MAX_FETCH).await),
                single,
                "pages of {limit}"
            );
        }
    }

    #[tokio::test]
    async fn span_list_boundary_between_copies() {
        // Start order: B, then the two copies of X, then A. A page of two rows ends between the
        // copies of X unless the list shows X once.
        let mut spans = copies_of(2, 0, 3_000, 1);
        spans.extend(copies_of(1, 0, 2_000, 2));
        spans.extend(copies_of(0, 0, 1_000, 1));
        let ctx = context_for(&spans);
        let expected = vec![
            expected_row(2, 0, "copy 1"),
            expected_row(1, 0, "copy 2"),
            expected_row(0, 0, "copy 1"),
        ];
        for limit in [1, 2, 3, 1000] {
            assert_eq!(
                listed_triples(&walk_spans(&ctx, limit, MAX_FETCH).await),
                expected,
                "pages of {limit}"
            );
        }
    }

    #[tokio::test]
    async fn span_list_bounds_the_refetch_and_keeps_paging() {
        // Newest first: Z (one copy), X (five copies), Y (one copy). With a read cap of 4, a page
        // reads Z and the first three copies of X, so it stops with two distinct spans.
        let mut spans = copies_of(2, 0, 3_000, 1);
        spans.extend(copies_of(1, 0, 2_000, 5));
        spans.extend(copies_of(0, 0, 1_000, 1));
        let ctx = context_for(&spans);
        let expected = vec![
            expected_row(2, 0, "copy 1"),
            expected_row(1, 0, "copy 5"),
            expected_row(0, 0, "copy 1"),
        ];
        for limit in [1, 2, 3, 1000] {
            assert_eq!(
                listed_triples(&walk_spans(&ctx, limit, MAX_FETCH).await),
                expected,
                "pages of {limit}, no cap"
            );
            assert_eq!(
                listed_triples(&walk_spans(&ctx, limit, 4).await),
                expected,
                "pages of {limit}, a read cap of 4"
            );
        }
        let request = span_request(scope(None, true), SpanSort::StartTime, SortOrder::Desc, 3);
        let page = span_list_capped(&ctx, request, 4).await.expect("page");
        assert_eq!(
            listed_triples(&rows(&page)),
            vec![expected[0].clone(), expected[1].clone()],
            "the short page at the cap"
        );
        assert!(page.next_cursor.is_some(), "the capped page still pages on");
    }

    #[tokio::test]
    async fn trace_detail_matches_each_dataset_trace() {
        let ctx = context();
        let listed = fixture("traces-include-eval.json");
        for trace in listed["data"].as_array().expect("data") {
            let id = trace["trace_id"].as_str().expect("id");
            let detail = trace_detail(&ctx, id, TIMEOUT)
                .await
                .expect("detail")
                .unwrap_or_else(|| panic!("missing {id}"));
            let row = &json_rows(std::slice::from_ref(&detail.trace))[0];
            for (key, expected) in trace.as_object().expect("trace") {
                if key == "scores" {
                    continue;
                }
                assert!(
                    values_match(&row[key], expected),
                    "{id} {key}: {} vs {expected}",
                    row[key]
                );
            }
            let file = fixture_file(&format!("fixtures/api/responses/traces/{id}.json"));
            let mut expected_spans: Vec<(String, String)> = file["spans"]
                .as_array()
                .expect("spans")
                .iter()
                .map(|node| {
                    let span = &node["span"];
                    (
                        span["start_time"].as_str().expect("start").to_owned(),
                        span["span_id"].as_str().expect("span").to_owned(),
                    )
                })
                .collect();
            expected_spans.sort();
            let actual = pairs(&json_rows(&detail.spans), "start_time", "span_id");
            assert_eq!(actual, expected_spans, "{id}");
            assert!(!detail.truncated, "{id}");
        }
    }

    #[tokio::test]
    async fn trace_detail_accepts_uppercase_and_rejects_a_short_id() {
        let ctx = context();
        let id = "a1000000000000000000000000000001";
        let lower = trace_detail(&ctx, id, TIMEOUT)
            .await
            .expect("lower")
            .expect("row");
        let upper = trace_detail(&ctx, &id.to_ascii_uppercase(), TIMEOUT)
            .await
            .expect("upper")
            .expect("row");
        assert_eq!(
            pairs(&json_rows(&lower.spans), "start_time", "span_id"),
            pairs(&json_rows(&upper.spans), "start_time", "span_id")
        );
        match trace_detail(&ctx, "abc", TIMEOUT).await {
            Err(RunError::InvalidRequest(message)) => {
                assert_eq!(message, "trace_id must be 32 hex digits");
            }
            other => panic!("expected an invalid id, got {other:?}"),
        }
        let unknown = trace_detail(&ctx, "0123456789abcdef0123456789abcdef", TIMEOUT)
            .await
            .expect("unknown");
        assert!(unknown.is_none());
    }

    #[tokio::test]
    async fn trace_detail_truncates_at_the_span_cap() {
        let base = dataset_spans().into_iter().next().expect("span");
        let trace_id = TraceId::from_str("a10000000000000000000000000000aa").expect("trace");
        let mut spans = Vec::new();
        for index in 0..5 {
            let mut span = base.clone();
            span.trace_id = trace_id;
            span.span_id = SpanId::from_str(&format!("b1000000000000a{index}")).expect("span");
            span.parent_span_id = None;
            span.start_time = base.start_time + i64::from(index);
            span.name = format!("span-{index}");
            spans.push(span);
        }
        let detail = trace_detail_limited(
            &context_for(&spans),
            "a10000000000000000000000000000aa",
            TIMEOUT,
            3,
        )
        .await
        .expect("detail")
        .expect("trace");
        assert!(detail.truncated);
        assert_eq!(
            pairs(&json_rows(&detail.spans), "span_id", "name"),
            [
                ("b1000000000000a0".to_owned(), "span-0".to_owned()),
                ("b1000000000000a1".to_owned(), "span-1".to_owned()),
                ("b1000000000000a2".to_owned(), "span-2".to_owned()),
            ]
        );
    }

    #[tokio::test]
    async fn span_detail_matches_the_fixture_and_keeps_the_latest_duplicate() {
        let ctx = context();
        let batch = span_detail(
            &ctx,
            "a1000000000000000000000000000001",
            "b100000000000012",
            TIMEOUT,
        )
        .await
        .expect("detail")
        .expect("span");
        let row = &json_rows(std::slice::from_ref(&batch))[0];
        let fixture = fixture_file(
            "fixtures/api/responses/spans/a1000000000000000000000000000001/b100000000000012.json",
        );
        for field in [
            "name",
            "span_kind",
            "start_time",
            "end_time",
            "input_tokens",
            "output_tokens",
            "cache_read_tokens",
            "cache_write_tokens",
            "reasoning_tokens",
            "total_tokens",
            "cost_input",
            "cost_output",
            "cost_total",
            "status_message",
        ] {
            let expected = fixture.get(field).unwrap_or(&Value::Null);
            assert!(
                values_match(&row[field], expected),
                "{field}: {} vs {expected}",
                row[field]
            );
        }
        let stored: Value =
            serde_json::from_str(row["input"].as_str().expect("input text")).expect("input json");
        assert_eq!(stored, fixture["input"]["value"]);
        assert_eq!(row["input_format"], fixture["input"]["format"]);

        let base = dataset_spans().into_iter().next().expect("span");
        let trace_id = TraceId::from_str("a2000000000000000000000000000001").expect("trace");
        let span_id = SpanId::from_str("b200000000000001").expect("span");
        let mut older = base.clone();
        let mut newer = base.clone();
        older.trace_id = trace_id;
        newer.trace_id = trace_id;
        older.span_id = span_id;
        newer.span_id = span_id;
        older.parent_span_id = None;
        newer.parent_span_id = None;
        older.ingest_time = 1_000;
        newer.ingest_time = 2_000;
        older.name = "older".to_owned();
        newer.name = "newer".to_owned();
        let winner = span_detail(
            &context_for(&[older, newer]),
            "a2000000000000000000000000000001",
            "b200000000000001",
            TIMEOUT,
        )
        .await
        .expect("duplicate")
        .expect("row");
        let winner = &json_rows(std::slice::from_ref(&winner))[0];
        assert_eq!(winner["name"], "newer");

        match span_detail(&ctx, "a1000000000000000000000000000001", "abc", TIMEOUT).await {
            Err(RunError::InvalidRequest(message)) => {
                assert_eq!(message, "span_id must be 16 hex digits");
            }
            other => panic!("expected an invalid span id, got {other:?}"),
        }
        let missing = span_detail(
            &ctx,
            "a1000000000000000000000000000001",
            "b1000000000000ff",
            TIMEOUT,
        )
        .await
        .expect("missing");
        assert!(missing.is_none());
    }

    fn row_count(batches: &[RecordBatch]) -> usize {
        batches.iter().map(RecordBatch::num_rows).sum()
    }

    #[tokio::test]
    async fn trace_spans_returns_roots_target_and_earliest() {
        let root = span_row(7, 0, 100, 1);
        let mut spans = vec![root.clone()];
        for n in 1..=5_u8 {
            let mut child = span_row(7, n, 100 + i64::from(n) * 10, 1);
            child.parent_span_id = Some(root.span_id);
            spans.push(child);
        }
        let trace_id = root.trace_id.to_hex();
        let target = spans[5].span_id.to_hex();
        let ctx = context_for(&spans);
        let data = trace_spans(
            &ctx,
            TraceSpansRequest {
                trace_id: &trace_id,
                target_span_id: Some(&target),
                max_spans: 3,
                max_required: 10,
                timeout: TIMEOUT,
            },
        )
        .await
        .expect("trace spans");
        // The root and the target. The other children are neither parentless nor the target.
        assert_eq!(row_count(&data.required), 2);
        assert_eq!(row_count(&data.earliest), 3);
        assert!(data.truncated);
    }

    #[tokio::test]
    async fn trace_spans_caps_the_required_part() {
        let spans: Vec<Span> = (0..3_u8).map(|n| span_row(7, n, 100, 1)).collect();
        let trace_id = spans[0].trace_id.to_hex();
        let ctx = context_for(&spans);
        let data = trace_spans(
            &ctx,
            TraceSpansRequest {
                trace_id: &trace_id,
                target_span_id: None,
                max_spans: 10,
                max_required: 2,
                timeout: TIMEOUT,
            },
        )
        .await
        .expect("trace spans");
        // max_required + 1 rows are read, so the caller sees the cap was exceeded.
        assert_eq!(row_count(&data.required), 3);
        assert!(!data.truncated);
    }

    #[tokio::test]
    async fn trace_spans_unknown_trace_is_empty() {
        let spans = vec![span_row(7, 0, 100, 1)];
        let unknown = span_row(8, 0, 100, 1).trace_id.to_hex();
        let ctx = context_for(&spans);
        let data = trace_spans(
            &ctx,
            TraceSpansRequest {
                trace_id: &unknown,
                target_span_id: None,
                max_spans: 10,
                max_required: 10,
                timeout: TIMEOUT,
            },
        )
        .await
        .expect("trace spans");
        assert_eq!(row_count(&data.required), 0);
        assert_eq!(row_count(&data.earliest), 0);
        assert!(!data.truncated);
    }

    #[tokio::test]
    async fn trace_spans_rejects_bad_ids() {
        let spans = vec![span_row(7, 0, 100, 1)];
        let trace_id = spans[0].trace_id.to_hex();
        let ctx = context_for(&spans);
        let short_trace = trace_spans(&ctx, bad_request("abc", None, 10)).await;
        assert!(matches!(short_trace, Err(RunError::InvalidRequest(_))));
        let bad_target = trace_spans(&ctx, bad_request(&trace_id, Some("xyz"), 10)).await;
        assert!(matches!(bad_target, Err(RunError::InvalidRequest(_))));
        let no_spans = trace_spans(&ctx, bad_request(&trace_id, None, 0)).await;
        assert!(matches!(no_spans, Err(RunError::InvalidRequest(_))));
    }

    fn bad_request<'a>(
        trace_id: &'a str,
        target_span_id: Option<&'a str>,
        max_spans: usize,
    ) -> TraceSpansRequest<'a> {
        TraceSpansRequest {
            trace_id,
            target_span_id,
            max_spans,
            max_required: 10,
            timeout: TIMEOUT,
        }
    }

    /// Two spans of one trace: a `root` and a `child` whose parent is the root. An experiment
    /// trace has the experiment key on both spans, so span-level exclusions see it too.
    fn trace_pair(trace: u16, environment: &str, experiment: bool) -> Vec<Span> {
        let mut root = span_row(trace, 0, 100 + i64::from(trace) * 1_000, 100);
        let mut child = span_row(trace, 1, 200 + i64::from(trace) * 1_000, 200);
        child.name = "child".to_owned();
        for span in [&mut root, &mut child] {
            span.environment = Some(environment.to_owned());
            if experiment {
                span.metadata
                    .insert(metadata_key::EXPERIMENT_ID.to_owned(), "exp-1".to_owned());
            }
        }
        child.parent_span_id = Some(root.span_id);
        vec![root, child]
    }

    /// Trace 0 is plain, trace 1 is an evaluation trace, trace 2 is an experiment trace whose
    /// root is in `production`.
    fn strict_spans() -> Vec<Span> {
        let mut spans = trace_pair(0, "production", false);
        spans.extend(trace_pair(1, environment::EVAL, false));
        spans.extend(trace_pair(2, "production", true));
        spans
    }

    /// The trace ids of [`strict_spans`]: plain, evaluation and experiment.
    fn strict_trace_ids(spans: &[Span]) -> [String; 3] {
        [
            spans[0].trace_id.to_string(),
            spans[2].trace_id.to_string(),
            spans[4].trace_id.to_string(),
        ]
    }

    /// Like `trace_pair(trace, environment, true)`, but only the root carries the experiment
    /// key. The child has none, so a span-level default exclusion keeps it and only a
    /// trace-level test (strict) drops it.
    fn root_tagged_pair(trace: u16, environment: &str) -> Vec<Span> {
        let mut spans = trace_pair(trace, environment, false);
        spans[0]
            .metadata
            .insert(metadata_key::EXPERIMENT_ID.to_owned(), "exp-1".to_owned());
        spans
    }

    /// Like [`strict_spans`], but trace 2 is tagged on its root span only.
    fn root_tagged_spans() -> Vec<Span> {
        let mut spans = trace_pair(0, "production", false);
        spans.extend(trace_pair(1, environment::EVAL, false));
        spans.extend(root_tagged_pair(2, "production"));
        spans
    }

    #[tokio::test]
    async fn refresh_strict_drops_eval_and_experiment_traces() {
        let spans = strict_spans();
        let ctx = context_for(&spans);
        let owned = strict_trace_ids(&spans);
        let ids = [owned[0].as_str(), owned[1].as_str(), owned[2].as_str()];
        let filter = "environment = 'cetra-eval'";

        let open = TraceRefreshRequest {
            include_experiments: true,
            ..refresh_request(&ids, None, None, true)
        };
        let batches = trace_refresh(&ctx, open).await.expect("open");
        assert_eq!(refreshed_ids(&batches), [ids[0], ids[1], ids[2]]);

        let strict = TraceRefreshRequest {
            strict_exclusions: true,
            ..open
        };
        let batches = trace_refresh(&ctx, strict).await.expect("strict");
        assert_eq!(refreshed_ids(&batches), [ids[0]]);

        let strict_named = TraceRefreshRequest {
            strict_exclusions: true,
            ..refresh_request(&ids, Some(filter), None, true)
        };
        let batches = trace_refresh(&ctx, strict_named)
            .await
            .expect("strict filter");
        assert!(refreshed_ids(&batches).is_empty());

        let named = refresh_request(&ids, Some(filter), None, true);
        let batches = trace_refresh(&ctx, named).await.expect("named");
        assert_eq!(refreshed_ids(&batches), [ids[1]]);
    }

    #[tokio::test]
    async fn trace_list_strict_ignores_the_include_flags() {
        let spans = strict_spans();
        let ctx = context_for(&spans);
        let owned = strict_trace_ids(&spans);
        let scope = ListScope {
            window: None,
            include_eval: true,
            include_experiments: true,
        };
        let open = listed(scope, TraceSort::StartTime, SortOrder::Asc, 50);
        let page = trace_list(&ctx, open).await.expect("open");
        assert_eq!(ids(&page), owned.to_vec());

        let strict = TraceListRequest {
            strict_exclusions: true,
            ..listed(scope, TraceSort::StartTime, SortOrder::Asc, 50)
        };
        let page = trace_list(&ctx, strict).await.expect("strict");
        assert_eq!(ids(&page), [owned[0].clone()]);
    }

    #[tokio::test]
    async fn span_list_strict_tests_the_trace() {
        let spans = strict_spans();
        let ctx = context_for(&spans);
        let owned = strict_trace_ids(&spans);
        let scope = ListScope {
            window: None,
            include_eval: true,
            include_experiments: true,
        };
        let open = span_request(scope, SpanSort::StartTime, SortOrder::Asc, 50);
        let page = span_list(&ctx, open).await.expect("open");
        assert_eq!(ids(&page).len(), 6);

        let strict = SpanListRequest {
            strict_exclusions: true,
            ..span_request(scope, SpanSort::StartTime, SortOrder::Asc, 50)
        };
        let page = span_list(&ctx, strict).await.expect("strict");
        assert_eq!(ids(&page), [owned[0].clone(), owned[0].clone()]);
    }

    /// A `span_refresh` request over `trace_ids`.
    fn span_refresh_request<'a>(
        filter: Option<&'a str>,
        trace_ids: &'a [&'a str],
        per_trace: usize,
        strict_exclusions: bool,
    ) -> SpanRefreshRequest<'a> {
        SpanRefreshRequest {
            filter,
            trace_ids,
            per_trace,
            strict_exclusions,
            timeout: TIMEOUT,
        }
    }

    #[tokio::test]
    async fn span_refresh_caps_each_trace_and_orders_by_start() {
        // Trace 0 has five spans, started out of order; trace 1 has two.
        let spans = vec![
            span_row(0, 0, 500, 100),
            span_row(0, 1, 100, 100),
            span_row(0, 2, 300, 100),
            span_row(0, 3, 200, 100),
            span_row(0, 4, 400, 100),
            span_row(1, 0, 50, 100),
            span_row(1, 1, 60, 100),
        ];
        let ctx = context_for(&spans);
        let a = spans[0].trace_id.to_string();
        let b = spans[5].trace_id.to_string();
        let ids = [a.as_str(), b.as_str()];
        let batches = span_refresh(&ctx, span_refresh_request(None, &ids, 3, false))
            .await
            .expect("refresh");

        let schema = batches.first().expect("a batch").schema();
        let columns: Vec<&str> = schema
            .fields()
            .iter()
            .map(|field| field.name().as_str())
            .collect();
        assert_eq!(columns, SPAN_REFRESH_COLUMNS);
        assert_eq!(
            pairs(&json_rows(&batches), "trace_id", "span_id"),
            [
                (a.clone(), "0202020202020202".to_owned()),
                (a.clone(), "0404040404040404".to_owned()),
                (a.clone(), "0303030303030303".to_owned()),
                (b.clone(), "0101010101010101".to_owned()),
                (b.clone(), "0202020202020202".to_owned()),
            ]
        );
    }

    #[tokio::test]
    async fn span_refresh_applies_the_filter_and_strict_exclusions() {
        let spans = strict_spans();
        let ctx = context_for(&spans);
        let owned = strict_trace_ids(&spans);
        let ids = [owned[0].as_str(), owned[1].as_str(), owned[2].as_str()];
        let named = "name = 'child'";
        let eval = "environment = 'cetra-eval'";

        let batches = span_refresh(&ctx, span_refresh_request(Some(named), &ids, 10, false))
            .await
            .expect("named");
        assert_eq!(
            pairs(&json_rows(&batches), "trace_id", "span_id"),
            [(owned[0].clone(), "0202020202020202".to_owned())]
        );

        let batches = span_refresh(&ctx, span_refresh_request(Some(eval), &ids, 10, false))
            .await
            .expect("eval by default");
        assert_eq!(
            pairs(&json_rows(&batches), "trace_id", "span_id"),
            [
                (owned[1].clone(), "0101010101010101".to_owned()),
                (owned[1].clone(), "0202020202020202".to_owned()),
            ]
        );

        let batches = span_refresh(&ctx, span_refresh_request(Some(eval), &ids, 10, true))
            .await
            .expect("eval strict");
        assert!(json_rows(&batches).is_empty());

        let batches = span_refresh(&ctx, span_refresh_request(Some(named), &ids, 10, true))
            .await
            .expect("named strict");
        assert_eq!(
            pairs(&json_rows(&batches), "trace_id", "span_id"),
            [(owned[0].clone(), "0202020202020202".to_owned())]
        );
    }

    #[tokio::test]
    async fn span_list_keeps_a_child_of_a_root_tagged_experiment_unless_strict() {
        let spans = root_tagged_spans();
        let ctx = context_for(&spans);
        let owned = strict_trace_ids(&spans);
        let scope = ListScope {
            window: None,
            include_eval: true,
            include_experiments: false,
        };
        let default = span_request(scope, SpanSort::StartTime, SortOrder::Asc, 50);
        let page = span_list(&ctx, default).await.expect("default");
        let mut kept = pairs(&rows(&page), "trace_id", "span_id");
        kept.sort();
        let mut expected = vec![
            (owned[0].clone(), "0101010101010101".to_owned()),
            (owned[0].clone(), "0202020202020202".to_owned()),
            (owned[1].clone(), "0101010101010101".to_owned()),
            (owned[1].clone(), "0202020202020202".to_owned()),
            (owned[2].clone(), "0202020202020202".to_owned()),
        ];
        expected.sort();
        assert_eq!(kept, expected);

        let strict = SpanListRequest {
            strict_exclusions: true,
            ..span_request(scope, SpanSort::StartTime, SortOrder::Asc, 50)
        };
        let page = span_list(&ctx, strict).await.expect("strict");
        let mut kept = pairs(&rows(&page), "trace_id", "span_id");
        kept.sort();
        assert_eq!(
            kept,
            [
                (owned[0].clone(), "0101010101010101".to_owned()),
                (owned[0].clone(), "0202020202020202".to_owned()),
            ]
        );
    }

    #[tokio::test]
    async fn span_refresh_keeps_a_child_of_a_root_tagged_experiment_unless_strict() {
        let spans = root_tagged_spans();
        let ctx = context_for(&spans);
        let owned = strict_trace_ids(&spans);
        let ids = [owned[0].as_str(), owned[1].as_str(), owned[2].as_str()];

        let batches = span_refresh(&ctx, span_refresh_request(None, &ids, 10, false))
            .await
            .expect("default");
        let mut kept = pairs(&json_rows(&batches), "trace_id", "span_id");
        kept.sort();
        let mut expected = vec![
            (owned[0].clone(), "0101010101010101".to_owned()),
            (owned[0].clone(), "0202020202020202".to_owned()),
            (owned[2].clone(), "0202020202020202".to_owned()),
        ];
        expected.sort();
        assert_eq!(kept, expected);

        let batches = span_refresh(&ctx, span_refresh_request(None, &ids, 10, true))
            .await
            .expect("strict");
        let mut kept = pairs(&json_rows(&batches), "trace_id", "span_id");
        kept.sort();
        let mut expected = vec![
            (owned[0].clone(), "0101010101010101".to_owned()),
            (owned[0].clone(), "0202020202020202".to_owned()),
        ];
        expected.sort();
        assert_eq!(kept, expected);
    }

    #[tokio::test]
    async fn span_refresh_validates_its_request() {
        let spans = strict_spans();
        let ctx = context_for(&spans);
        let owned = strict_trace_ids(&spans);
        let one = [owned[0].as_str()];
        let many = vec![owned[0].as_str(); MAX_REFRESH_IDS + 1];
        let short = [&ID_ANSWER[1..]];

        for request in [
            span_refresh_request(None, &many, 3, false),
            span_refresh_request(None, &short, 3, false),
            span_refresh_request(None, &one, 0, false),
            span_refresh_request(None, &one, MAX_REFRESH_SPANS_PER_TRACE + 1, false),
        ] {
            let result = span_refresh(&ctx, request).await;
            assert!(
                matches!(result, Err(RunError::InvalidRequest(_))),
                "{result:?}"
            );
        }
        assert!(matches!(
            span_refresh(
                &ctx,
                span_refresh_request(Some("nonsense_column = 1"), &one, 3, false)
            )
            .await,
            Err(RunError::Invalid(_))
        ));
        let none: [&str; 0] = [];
        let batches = span_refresh(&ctx, span_refresh_request(None, &none, 3, false))
            .await
            .expect("no ids");
        assert!(batches.is_empty());
    }

    /// A `match_count` request over `view` and `window`.
    fn count_request<'a>(
        view: ViewName,
        filter: Option<&'a str>,
        window: TimeWindow,
        cap: usize,
        strict_exclusions: bool,
    ) -> MatchCountRequest<'a> {
        MatchCountRequest {
            view,
            filter,
            window,
            cap,
            strict_exclusions,
            timeout: TIMEOUT,
        }
    }

    /// One trace of one span per start time, the trace index being the position.
    fn one_span_traces(starts: &[i64]) -> Vec<Span> {
        starts
            .iter()
            .enumerate()
            .map(|(index, start)| {
                span_row(u16::try_from(index).expect("few traces"), 0, *start, 100)
            })
            .collect()
    }

    #[tokio::test]
    async fn match_count_counts_in_the_window() {
        let spans = one_span_traces(&[
            nanos("2026-01-01T09:00:00Z"),
            nanos("2026-01-01T10:00:00Z"),
            nanos("2026-01-01T10:30:00Z"),
            nanos("2026-01-01T11:00:00Z"),
        ]);
        let ctx = context_for(&spans);
        let window = TimeWindow::new(nanos("2026-01-01T10:00:00Z"), nanos("2026-01-01T11:00:00Z"))
            .expect("window");
        let count = match_count(
            &ctx,
            count_request(ViewName::Traces, None, window, 100, false),
        )
        .await
        .expect("count");
        assert_eq!(count, 2);
    }

    #[tokio::test]
    async fn match_count_applies_the_filter() {
        let base = nanos("2026-01-01T10:00:00Z");
        let mut failed = span_row(2, 0, base, 100);
        failed.status = SpanStatus::Error;
        let spans = vec![
            span_row(0, 0, base, 100),
            span_row(1, 0, base + 1, 100),
            failed,
        ];
        let ctx = context_for(&spans);
        let count = match_count(
            &ctx,
            count_request(
                ViewName::Traces,
                Some("status = 'error'"),
                day(),
                100,
                false,
            ),
        )
        .await
        .expect("filtered");
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn match_count_strict_drops_eval_and_experiment_traces() {
        let spans = strict_spans();
        let ctx = context_for(&spans);
        // The strict test traces start within the first microsecond after the epoch.
        let window = TimeWindow::new(0, 1_000_000).expect("window");
        let filter = Some("environment = 'cetra-eval'");
        let named = match_count(
            &ctx,
            count_request(ViewName::Traces, filter, window, 100, false),
        )
        .await
        .expect("named");
        assert_eq!(named, 1);
        let strict = match_count(
            &ctx,
            count_request(ViewName::Traces, filter, window, 100, true),
        )
        .await
        .expect("strict");
        assert_eq!(strict, 0);
    }

    #[tokio::test]
    async fn match_count_counts_spans() {
        let base = nanos("2026-01-01T10:00:00Z");
        let mut first = span_row(0, 0, base, 100);
        first.span_kind = SpanKind::Llm;
        let mut second = span_row(0, 1, base + 1, 100);
        second.span_kind = SpanKind::Llm;
        let other = span_row(0, 2, base + 2, 100);
        let spans = vec![first, second, other];
        let ctx = context_for(&spans);
        let count = match_count(
            &ctx,
            count_request(
                ViewName::Spans,
                Some("span_kind = 'llm'"),
                day(),
                100,
                false,
            ),
        )
        .await
        .expect("spans");
        assert_eq!(count, 2);
    }

    #[tokio::test]
    async fn match_count_stops_at_cap_plus_one() {
        let base = nanos("2026-01-01T10:00:00Z");
        let spans: Vec<Span> = (0..5_u16)
            .map(|trace| span_row(trace, 0, base + i64::from(trace), 100))
            .collect();
        let ctx = context_for(&spans);
        let capped = match_count(&ctx, count_request(ViewName::Traces, None, day(), 2, false))
            .await
            .expect("capped");
        assert_eq!(capped, 3);
        let whole = match_count(
            &ctx,
            count_request(ViewName::Traces, None, day(), 10, false),
        )
        .await
        .expect("whole");
        assert_eq!(whole, 5);
    }

    #[tokio::test]
    async fn match_count_validates_its_request() {
        let ctx = context();
        let window = day();
        let sessions = match_count(
            &ctx,
            count_request(ViewName::Sessions, None, window, 10, false),
        )
        .await;
        assert!(
            matches!(sessions, Err(RunError::InvalidRequest(_))),
            "{sessions:?}"
        );
        for cap in [0, MAX_MATCH_COUNT + 1] {
            let result = match_count(
                &ctx,
                count_request(ViewName::Traces, None, window, cap, false),
            )
            .await;
            assert!(
                matches!(result, Err(RunError::InvalidRequest(_))),
                "{result:?}"
            );
        }
        let bad = match_count(
            &ctx,
            count_request(ViewName::Traces, Some("error_count >"), window, 10, false),
        )
        .await;
        assert!(matches!(bad, Err(RunError::Invalid(_))), "{bad:?}");
        let largest = match_count(
            &ctx,
            count_request(ViewName::Traces, None, window, MAX_MATCH_COUNT, false),
        )
        .await;
        assert!(largest.is_ok(), "{largest:?}");
    }

    #[tokio::test]
    async fn strict_refresh_returns_the_two_trigger_columns() {
        let ctx = context();
        let ids = [ID_ANSWER];
        let strict = TraceRefreshRequest {
            strict_exclusions: true,
            ..refresh_request(&ids, None, None, false)
        };
        let batches = trace_refresh(&ctx, strict).await.expect("strict");
        let schema = batches.first().expect("a batch").schema();
        let names: Vec<&str> = schema
            .fields()
            .iter()
            .map(|field| field.name().as_str())
            .collect();
        assert_eq!(names, TRACE_REFRESH_STRICT_COLUMNS);
        assert_eq!(refreshed_ids(&batches), [ID_ANSWER]);

        let full = trace_refresh(&ctx, refresh_request(&ids, None, None, false))
            .await
            .expect("full");
        assert!(full.first().expect("a batch").num_columns() > TRACE_REFRESH_STRICT_COLUMNS.len());
    }
}
