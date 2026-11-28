//! Native query-engine (`cetra-query` + `cetra-column`) implementations of the
//! 10 `traces` and `spans` read entry points.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::time::Duration;

use cetra_column::{
    Batch, CancelToken, Column, DataType, Field, Reservation, ScalarValue, Schema, Selection,
};
use cetra_core::enums::SpanStatus;
use cetra_core::schema::{spans, traces};
use cetra_query::exec::hash_agg::{AggExpr, AggFunc, HashAgg, HashAggSpec};
use cetra_query::exec::hash_join::{HashJoinSpec, JoinType, execute_hash_join};
use cetra_query::exec::sort_window::{WindowFunc, WindowSpec, eval_window_group};
use cetra_query::exec::streaming::{FilterOp, LimitOp, ProjectOp, TopKSink};
use cetra_query::exec::{
    CollectSink, ControlFlow, ExecContext, IdLookupHint, PartitionStream, PushOperator, PushSink,
    ScanSource, ScanSpec, SortKey, TimeRangeHint, run_pipeline as exec_run_pipeline,
};
use cetra_query::{BinaryOp, CompiledFilter, Expr, FilterView, QueryError};

use crate::engine::statam_source::{ROW_COORD_FILE_COL, ROW_COORD_ROW_COL};
use crate::engine::traces_source::TraceListMode;
use crate::engine::{BufferedBatchStream, apply_visible_from};
use crate::query_error::RunError;
use crate::reads::columns::{
    active_rows, decimal_to_f64_at, i64_at, internal, raw_i64_at, raw_string_at, raw_time_at,
    require_col, string_at,
};
use crate::reads::context::{
    DURATION_MS, DURATION_NS, EngineContext, OperatorStream, duration_ns_from_bounds_expr,
    populate_id_lookups, project_view_columns, query_err_to_run, rewrite_view_expr_for_raw,
    run_err_to_query, spans_raw_schema, spans_view_projection, spans_view_schema,
    traces_raw_schema, traces_view_projection, traces_view_schema,
};
use crate::reads::cursor::{self, Cursor, CursorValue, Page};
use crate::reads::scope::{
    SearchPredicate, StrictExclusionsFilter, apply_strict_exclusions_spans,
    prepare_strict_exclusions_spans, scoped_spans_spec, scoped_traces_spec, search_predicate,
    strict_exclusions_spans,
};
use crate::reads::scores_join::{apply_compiled_filter, compile_optional_filter};
use crate::reads::{ListScope, MAX_PAGE, SortOrder, present};
use crate::views::{QueryContext, TimeWindow, ViewName};

const LIST: &str = "traces";
const SPANS: &str = "spans";
const TRACE_TIES: &[&str] = &[traces::TRACE_ID];
const SPAN_TIES: &[&str] = &[spans::SPAN_ID, spans::TRACE_ID];
const SPAN_KEY: &[&str] = &[spans::START_TIME, spans::SPAN_ID, spans::TRACE_ID];
const MAX_FETCH: usize = 64 * MAX_PAGE;
const SPAN_RANK: &str = "__rn";
const TOTAL_TRACES: &str = "trace_count";
const TOTAL_ERRORS: &str = "error_trace_count";
const TOTAL_COST: &str = "cost_total";

/// Most spans [`trace_detail`] returns for one trace.
pub const MAX_TRACE_SPANS: usize = 20_000;

/// Max ids of one [`trace_refresh`].
pub const MAX_REFRESH_IDS: usize = 500;

/// Most spans [`span_refresh`] returns for one trace (D-I-9).
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
    /// Apply strict exclusions: evaluation and experiment rows are left out even when the
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
    /// Apply strict exclusions instead of the default exclusions.
    pub strict_exclusions: bool,
    /// Time allowed for planning and running.
    pub timeout: Duration,
}

/// The columns [`span_refresh`] returns, in this order.
pub const SPAN_REFRESH_COLUMNS: &[&str] = &[spans::TRACE_ID, spans::SPAN_ID, spans::START_TIME];

/// Most error spans [`error_spans`] returns for one trace (D-IC-2).
pub const MAX_ERROR_SPANS_PER_TRACE: usize = 50;

/// Error spans of the given traces that match a span filter, for error issue sources (D-IC-2).
#[derive(Debug, Clone, Copy)]
pub struct ErrorSpansRequest<'a> {
    /// Filter text over `spans`. Empty or whitespace means no filter. `status = 'error'` is
    /// always added.
    pub filter: Option<&'a str>,
    /// Trace ids, 32 hex digits, either case. At most [`MAX_REFRESH_IDS`].
    pub trace_ids: &'a [&'a str],
    /// Time allowed for planning and running.
    pub timeout: Duration,
}

/// The columns [`error_spans`] returns, in this order.
pub const ERROR_SPAN_COLUMNS: &[&str] = &[
    spans::TRACE_ID,
    spans::SPAN_ID,
    spans::START_TIME,
    spans::SERVICE_NAME,
    spans::NAME,
    spans::STATUS_MESSAGE,
    spans::EVENTS,
    spans::USER_ID,
    spans::SESSION_ID,
];

/// Which spans of one trace [`trace_spans`] returns.
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

/// The columns [`trace_refresh`] returns with `strict_exclusions: true`, in this order.
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
    /// Apply strict exclusions instead of the default exclusions.
    pub strict_exclusions: bool,
    /// Time allowed for planning and running.
    pub timeout: Duration,
}

/// The trace list's rows to total (D-C-31): the list's filter, search, window and exclusions,
/// without sort, cursor or limit.
#[derive(Debug, Clone, Copy)]
pub struct TraceTotalsRequest<'a> {
    /// Filter text. Empty or whitespace means no filter.
    pub filter: Option<&'a str>,
    /// Search text. Empty or whitespace means no search.
    pub search: Option<&'a str>,
    /// Window and default exclusions.
    pub scope: ListScope,
    /// Time allowed for planning and running.
    pub timeout: Duration,
}

/// Totals of the trace list's rows (D-C-31).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TraceTotals {
    /// Traces the list would page through.
    pub trace_count: i64,
    /// Of those, the traces whose `status` is `error`.
    pub error_trace_count: i64,
    /// Exact sum of their `cost_total` (D-A2-4); a null cost adds nothing; 0 for no trace.
    pub cost_total: f64,
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
    spans::PROMPT_NAME,
    spans::PROMPT_VERSION,
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

/// One page of the traces view.
#[derive(Debug, Clone)]
pub struct TraceListRequest<'a> {
    /// Filter text. Empty or whitespace means no filter.
    pub filter: Option<&'a str>,
    /// Search text. Empty or whitespace means no search.
    pub search: Option<&'a str>,
    /// Window and default exclusions.
    pub scope: ListScope,
    /// Apply strict exclusions: evaluation and experiment rows are left out even when the
    /// filter names `environment` or the include flags are set (D-I-9).
    pub strict_exclusions: bool,
    /// Sort column.
    pub sort: TraceSort,
    /// Sort direction. Nulls are last.
    pub order: SortOrder,
    /// Rows to return, from 1 to [`MAX_PAGE`].
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

/// One page of the spans view, projected to [`SPAN_LIST_COLUMNS`].
#[derive(Debug, Clone)]
pub struct SpanListRequest<'a> {
    /// Filter text. Empty or whitespace means no filter.
    pub filter: Option<&'a str>,
    /// Search text. Empty or whitespace means no search.
    pub search: Option<&'a str>,
    /// Window and default exclusions.
    pub scope: ListScope,
    /// Apply strict exclusions: evaluation and experiment rows are left out even when the
    /// filter names `environment` or the include flags are set (D-I-9).
    pub strict_exclusions: bool,
    /// Sort column.
    pub sort: SpanSort,
    /// Sort direction. Nulls are last.
    pub order: SortOrder,
    /// Rows to return, from 1 to [`MAX_PAGE`].
    pub limit: usize,
    /// Cursor from the previous page.
    pub cursor: Option<&'a str>,
    /// Time allowed for planning and running.
    pub timeout: Duration,
}

/// One trace row and its spans.
pub struct TraceDetailData {
    /// The `traces` view row, all 29 columns.
    pub trace: Batch,
    /// [`SPAN_LIST_COLUMNS`], ordered `start_time ASC, span_id ASC`.
    pub spans: Vec<Batch>,
    /// True when more than [`MAX_TRACE_SPANS`] spans were stored.
    pub truncated: bool,
}

impl fmt::Debug for TraceDetailData {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let span_rows: usize = self.spans.iter().map(Batch::num_rows).sum();
        f.debug_struct("TraceDetailData")
            .field("trace_rows", &self.trace.num_rows())
            .field("span_batches", &self.spans.len())
            .field("span_rows", &span_rows)
            .field("truncated", &self.truncated)
            .finish()
    }
}

/// Every column of one trace's spans, in two parts.
pub struct TraceSpansData {
    /// Parentless spans and the target span. May repeat a span of `earliest`.
    pub required: Vec<Batch>,
    /// The earliest spans, `start_time ASC, span_id ASC`, at most `max_spans`.
    pub earliest: Vec<Batch>,
    /// True when more than `max_spans` spans were stored.
    pub truncated: bool,
}

impl fmt::Debug for TraceSpansData {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let req_rows: usize = self.required.iter().map(Batch::num_rows).sum();
        let early_rows: usize = self.earliest.iter().map(Batch::num_rows).sum();
        f.debug_struct("TraceSpansData")
            .field("required_rows", &req_rows)
            .field("earliest_rows", &early_rows)
            .field("truncated", &self.truncated)
            .finish()
    }
}

fn trace_sort_column(sort: TraceSort) -> &'static str {
    match sort {
        TraceSort::StartTime => traces::START_TIME,
        TraceSort::Duration => DURATION_NS,
        TraceSort::CostTotal => traces::COST_TOTAL,
        TraceSort::TotalTokens => traces::TOTAL_TOKENS,
    }
}

fn span_sort_column(sort: SpanSort) -> &'static str {
    match sort {
        SpanSort::StartTime => spans::START_TIME,
        SpanSort::Duration => DURATION_NS,
        SpanSort::CostTotal => spans::COST_TOTAL,
        SpanSort::TotalTokens => spans::TOTAL_TOKENS,
    }
}

struct PreparedTraceList {
    compiled: Option<CompiledFilter>,
    cursor: Option<Cursor>,
    search: Option<String>,
    effective_scope: ListScope,
    references_environment: bool,
    raw_spec: ScanSpec,
    list_mode: Option<TraceListMode>,
    use_two_phase_with_joins: bool,
}

fn prepare_trace_list(
    ctx: &EngineContext,
    request: &TraceListRequest<'_>,
) -> Result<PreparedTraceList, RunError> {
    let sort_col = trace_sort_column(request.sort);
    let compiled = compile_optional_filter(FilterView::Traces, request.filter)?;
    let cursor = match request.cursor {
        Some(text) => {
            let decoded = cursor::decode(text, LIST, sort_col, request.order)?;
            if decoded.tie_len() != TRACE_TIES.len() {
                return Err(RunError::InvalidRequest("invalid cursor".to_owned()));
            }
            Some(decoded)
        }
        None => None,
    };
    let search = present(request.search).map(str::to_owned);
    if search.is_some() && request.scope.window.is_none() {
        return Err(RunError::InvalidRequest(
            "search needs a time range".to_owned(),
        ));
    }

    let references_environment = !request.strict_exclusions
        && compiled
            .as_ref()
            .is_some_and(|filter| filter.references_environment);
    let effective_scope = if request.strict_exclusions {
        ListScope {
            window: request.scope.window,
            include_eval: false,
            include_experiments: false,
        }
    } else {
        request.scope
    };

    let has_scores = compiled.as_ref().is_some_and(|f| !f.scores.is_empty());
    let has_search = search.is_some();

    let traces_raw = traces_raw_schema()?;
    let mut raw_spec = scoped_traces_spec(ctx, effective_scope, references_environment)?;
    if !has_scores && !has_search {
        if let Some(filter) = &compiled {
            let rewritten = rewrite_view_expr_for_raw(
                &filter.expr,
                &traces_raw,
                &[traces::TRACE_ID, traces::ROOT_SPAN_ID],
                true,
            )
            .map_err(query_err_to_run)?;
            raw_spec.filters.push(rewritten);
        }
        if let Some(cur) = &cursor {
            raw_spec
                .filters
                .push(traces_raw_after_expr(cur, &traces_raw)?);
        }
        populate_id_lookups(&mut raw_spec);
    }

    let list_mode = if !ctx.force_range_mode && !has_scores && !has_search {
        match (request.sort, request.order) {
            (TraceSort::StartTime, SortOrder::Desc) => Some(TraceListMode::Widening {
                fetch: request.limit.saturating_add(1),
            }),
            (TraceSort::CostTotal | TraceSort::Duration | TraceSort::TotalTokens, _) => {
                let sort_keys = traces_raw_sort_keys(&traces_raw, request.sort, request.order)?;
                Some(TraceListMode::TwoPhase {
                    sort_keys,
                    limit: request.limit.saturating_add(1),
                    offset: 0,
                })
            }
            (TraceSort::StartTime, SortOrder::Asc) => None,
        }
    } else {
        None
    };

    let use_two_phase_with_joins = has_scores || has_search;

    Ok(PreparedTraceList {
        compiled,
        cursor,
        search,
        effective_scope,
        references_environment,
        raw_spec,
        list_mode,
        use_two_phase_with_joins,
    })
}

/// Summary of the physical trace list plan (`mode=list` or `mode=range`).
#[doc(hidden)]
pub async fn trace_list_plan(
    ctx: &QueryContext,
    request: TraceListRequest<'_>,
) -> Result<String, RunError> {
    let ctx = ctx.engine();
    let prepared = prepare_trace_list(ctx, &request)?;
    let mut source = (*ctx.traces).clone();
    if let Some(mode) = prepared.list_mode {
        source = source.with_list_mode(mode);
    }
    Ok(source.explain_summary(&prepared.raw_spec))
}

/// One page of traces, in `traces`-view column order (`29` columns).
pub async fn trace_list(
    ctx: &QueryContext,
    request: TraceListRequest<'_>,
) -> Result<Page, RunError> {
    let ctx = ctx.engine();
    if !(1..=MAX_PAGE).contains(&request.limit) {
        return Err(RunError::InvalidRequest(
            "limit must be between 1 and 1000".to_owned(),
        ));
    }

    let prepared = prepare_trace_list(ctx, &request)?;
    let sort_col = trace_sort_column(request.sort);
    let fetch = request.limit + 1;

    let batches = if prepared.use_two_phase_with_joins {
        run_trace_list_two_phase_with_joins(ctx, &request, &prepared, sort_col, fetch).await?
    } else {
        let mut source = (*ctx.traces).clone();
        let pre_sorted = prepared.list_mode.is_some();
        if let Some(mode) = prepared.list_mode {
            source = source.with_list_mode(mode);
        }
        let spec = prepared.raw_spec;
        let order = request.order;

        let (batches, _) = ctx
            .run_with_ctx(request.timeout, move |exec_ctx| {
                let raw_schema = traces_raw_schema().map_err(run_err_to_query)?;
                let partitions = source.open_partitions(&spec, exec_ctx)?;
                if partitions.is_empty() {
                    return Ok(Vec::new());
                }
                let view_schema = traces_view_schema().map_err(run_err_to_query)?;
                let view_streams: Vec<Box<dyn PartitionStream>> = partitions
                    .into_iter()
                    .map(|s| {
                        let proj = traces_view_projection(&raw_schema, exec_ctx.clone())?;
                        Ok(
                            Box::new(OperatorStream::new(s, vec![Box::new(proj)], exec_ctx))
                                as Box<dyn PartitionStream>,
                        )
                    })
                    .collect::<Result<Vec<_>, QueryError>>()?;

                if pre_sorted {
                    let pool = Arc::clone(&exec_ctx.pool);
                    let sinks = exec_run_pipeline(
                        exec_ctx,
                        view_streams,
                        |_| Ok(Vec::new()),
                        move |_| Ok(CollectSink::new(&pool)),
                    )?;
                    Ok(sinks
                        .into_iter()
                        .flat_map(CollectSink::into_batches)
                        .collect())
                } else {
                    let sort_keys = cursor::order_by(&view_schema, sort_col, TRACE_TIES, order)
                        .map_err(run_err_to_query)?;
                    let sink_schema = Arc::clone(&view_schema);
                    let sink_keys = sort_keys.clone();
                    let sink_ctx = exec_ctx.clone();
                    let sinks = exec_run_pipeline(
                        exec_ctx,
                        view_streams,
                        |_| Ok(Vec::new()),
                        move |_| {
                            Ok(TopKSink::new(
                                Arc::clone(&sink_schema),
                                sink_keys.clone(),
                                fetch,
                                0,
                                sink_ctx.clone(),
                            ))
                        },
                    )?;
                    TopKSink::merge(sinks, view_schema, &sort_keys, 0, fetch, exec_ctx)
                }
            })
            .await?;
        batches
    };

    cursor::assemble(
        batches,
        request.limit,
        LIST,
        sort_col,
        request.order,
        TRACE_TIES,
    )
}

async fn run_trace_list_two_phase_with_joins(
    ctx: &EngineContext,
    request: &TraceListRequest<'_>,
    prepared: &PreparedTraceList,
    sort_col: &'static str,
    fetch: usize,
) -> Result<Vec<Batch>, RunError> {
    let ctx_c = ctx.clone();
    let compiled = prepared.compiled.clone();
    let cursor = prepared.cursor.clone();
    let search = prepared.search.clone();
    let search_window = request.scope.window;
    let effective_scope = prepared.effective_scope;
    let references_environment = prepared.references_environment;
    let order = request.order;

    let (batches, _) = ctx
        .run_with_ctx(request.timeout, move |exec_ctx| {
            let traces_raw = traces_raw_schema().map_err(run_err_to_query)?;
            let traces_view = traces_view_schema().map_err(run_err_to_query)?;

            // Build Phase 1 narrow view schema: trace_id, sort_col, and any column referenced by filter.
            let mut needed_view_cols = BTreeSet::new();
            if let Some(idx) = traces_view.index_of(traces::TRACE_ID) {
                needed_view_cols.insert(idx);
            }
            if let Some(idx) = traces_view.index_of(sort_col) {
                needed_view_cols.insert(idx);
            }
            if let Some(filter) = &compiled {
                filter.expr.referenced_columns(&mut needed_view_cols);
                needed_view_cols.retain(|&idx| idx < traces_view.len());
            }

            let p1_view_indices: Vec<usize> = needed_view_cols.into_iter().collect();
            let p1_view_schema = Arc::new(traces_view.project(&p1_view_indices)?);
            let p1_view_names: Vec<&str> = p1_view_schema
                .fields()
                .iter()
                .map(|f| f.name.as_str())
                .collect();

            let mut p1_raw_set = BTreeSet::new();
            for &name in &p1_view_names {
                if name == DURATION_NS || name == DURATION_MS {
                    if let Some(i) = traces_raw.index_of(traces::START_TIME) {
                        p1_raw_set.insert(i);
                    }
                    if let Some(i) = traces_raw.index_of(traces::END_TIME) {
                        p1_raw_set.insert(i);
                    }
                } else if let Some(i) = traces_raw.index_of(name) {
                    p1_raw_set.insert(i);
                }
            }
            let p1_raw_indices: Vec<usize> = p1_raw_set.into_iter().collect();
            let mut p1_spec = scoped_traces_spec(&ctx_c, effective_scope, references_environment)
                .map_err(run_err_to_query)?;
            p1_spec.projection = Some(p1_raw_indices);

            if let Some(filter) = &compiled
                && filter.scores.is_empty()
            {
                let rewritten = rewrite_view_expr_for_raw(
                    &filter.expr,
                    &traces_raw,
                    &[traces::TRACE_ID, traces::ROOT_SPAN_ID],
                    true,
                )?;
                p1_spec.filters.push(rewritten);
                populate_id_lookups(&mut p1_spec);
            }

            let p1_scan_schema = ctx_c.traces.projected_schema(&p1_spec)?;
            let mut p1_streams = ctx_c.traces.open_partitions(&p1_spec, exec_ctx)?;
            if p1_streams.is_empty() {
                return Ok(Vec::new());
            }

            // Apply search semi-join on raw FixedBytes16 trace_id before hex projection.
            if let Some(text) = &search {
                match search_predicate(&ctx_c, exec_ctx, FilterView::Traces, text, search_window)
                    .map_err(run_err_to_query)?
                {
                    SearchPredicate::None => {}
                    SearchPredicate::Traces { trace_ids, schema } => {
                        if trace_ids.is_empty() {
                            return Ok(Vec::new());
                        }
                        let left_key = col_expr_q(&p1_scan_schema, traces::TRACE_ID)?;
                        let right_key = col_expr_q(&schema, traces::TRACE_ID)?;
                        let joined = execute_hash_join(
                            p1_streams,
                            Arc::clone(&p1_scan_schema),
                            trace_ids,
                            schema,
                            HashJoinSpec {
                                join_type: JoinType::LeftSemi,
                                left_keys: vec![left_key],
                                right_keys: vec![right_key],
                                null_equals_null: false,
                                residual_filter: None,
                            },
                            exec_ctx,
                        )?;
                        p1_streams = vec![joined];
                    }
                    SearchPredicate::Spans { .. } => {
                        return Err(QueryError::internal(
                            "unexpected Spans search predicate for Traces",
                        ));
                    }
                }
            }

            // Project Phase 1 streams to narrow view schema.
            let mut p1_view_streams: Vec<Box<dyn PartitionStream>> = p1_streams
                .into_iter()
                .map(|s| {
                    let proj = project_view_columns(
                        &p1_scan_schema,
                        Arc::clone(&p1_view_schema),
                        &p1_view_names,
                        true,
                        exec_ctx.clone(),
                    )?;
                    Ok(
                        Box::new(OperatorStream::new(s, vec![Box::new(proj)], exec_ctx))
                            as Box<dyn PartitionStream>,
                    )
                })
                .collect::<Result<Vec<_>, QueryError>>()?;

            // Apply score() left-joins and filter expression when score() is present.
            if let Some(filter) = &compiled
                && !filter.scores.is_empty()
            {
                p1_view_streams = apply_compiled_filter(
                    &ctx_c,
                    exec_ctx,
                    p1_view_streams,
                    &p1_view_schema,
                    filter,
                )?;
            }

            // Apply cursor predicate on Phase 1 view schema.
            if let Some(cur) = &cursor {
                let after = cursor::after_expr(cur, &p1_view_schema, sort_col, TRACE_TIES)
                    .map_err(run_err_to_query)?;
                p1_view_streams = p1_view_streams
                    .into_iter()
                    .map(|s| {
                        let f = FilterOp::new(after.clone(), exec_ctx.clone());
                        Box::new(OperatorStream::new(s, vec![Box::new(f)], exec_ctx))
                            as Box<dyn PartitionStream>
                    })
                    .collect();
            }

            // Rank Phase 1 via TopK(limit + 1).
            let p1_sort_keys = cursor::order_by(&p1_view_schema, sort_col, TRACE_TIES, order)
                .map_err(run_err_to_query)?;
            let sink_schema = Arc::clone(&p1_view_schema);
            let sink_keys = p1_sort_keys.clone();
            let sink_ctx = exec_ctx.clone();
            let sinks = exec_run_pipeline(
                exec_ctx,
                p1_view_streams,
                |_| Ok(Vec::new()),
                move |_| {
                    Ok(TopKSink::new(
                        Arc::clone(&sink_schema),
                        sink_keys.clone(),
                        fetch,
                        0,
                        sink_ctx.clone(),
                    ))
                },
            )?;
            let ranked = TopKSink::merge(sinks, p1_view_schema, &p1_sort_keys, 0, fetch, exec_ctx)?;

            let mut winning_ids: Vec<[u8; 16]> = Vec::new();
            for batch in &ranked {
                let id_col = require_col(batch, traces::TRACE_ID).map_err(run_err_to_query)?;
                for row in 0..batch.num_rows() {
                    let hex_str = string_at(batch, id_col, row)
                        .map_err(run_err_to_query)?
                        .ok_or_else(|| QueryError::internal("phase 1 trace_id is null"))?;
                    let bytes = decode_hex_16_q(&hex_str)?;
                    winning_ids.push(bytes);
                }
            }
            drop(ranked);
            if winning_ids.is_empty() {
                return Ok(Vec::new());
            }

            // Phase 2: fetch full trace view rows for the `<= limit + 1` winning trace_ids.
            let mut p2_spec = scoped_traces_spec(&ctx_c, effective_scope, references_environment)
                .map_err(run_err_to_query)?;
            p2_spec.time_range.around_ns = None;
            let trace_col = col_expr_q(&traces_raw, traces::TRACE_ID)?;
            let lits = winning_ids
                .iter()
                .map(|id| Expr::Literal(ScalarValue::FixedBytes16(*id)))
                .collect();
            p2_spec.filters.push(Expr::InList {
                expr: Box::new(trace_col),
                list: lits,
                negated: false,
                redacted: false,
            });
            p2_spec.id_lookups.push(IdLookupHint::Fixed16 {
                column: traces::TRACE_ID.to_owned(),
                values: winning_ids.clone(),
            });

            let p2_streams = ctx_c.traces.open_partitions(&p2_spec, exec_ctx)?;
            if p2_streams.is_empty() {
                return Ok(Vec::new());
            }
            let pool = Arc::clone(&exec_ctx.pool);
            let proj_ctx = exec_ctx.clone();
            let raw_c = Arc::clone(&traces_raw);
            let p2_sinks = exec_run_pipeline(
                exec_ctx,
                p2_streams,
                move |_| {
                    let proj = traces_view_projection(&raw_c, proj_ctx.clone())?;
                    Ok(vec![Box::new(proj) as Box<dyn PushOperator>])
                },
                move |_| Ok(CollectSink::new(&pool)),
            )?;
            let p2_batches: Vec<Batch> = p2_sinks
                .into_iter()
                .flat_map(CollectSink::into_batches)
                .filter(|b| !b.is_empty())
                .collect();
            if p2_batches.is_empty() {
                return Ok(Vec::new());
            }

            restore_trace_rank_order(traces_view, p2_batches, &winning_ids, exec_ctx)
        })
        .await?;

    Ok(batches)
}

fn restore_trace_rank_order(
    view_schema: Arc<Schema>,
    batches: Vec<Batch>,
    winning_ids: &[[u8; 16]],
    exec_ctx: &ExecContext,
) -> Result<Vec<Batch>, QueryError> {
    let mut res = exec_ctx.pool.register("trace_p2_order", false);
    let refs: Vec<&Batch> = batches.iter().collect();
    let combined = Batch::concat(Arc::clone(&view_schema), &refs, Some(&mut res))?;
    drop(batches);
    let id_col = require_col(&combined, traces::TRACE_ID).map_err(run_err_to_query)?;
    let mut row_by_id: HashMap<[u8; 16], u32> = HashMap::with_capacity(combined.num_rows());
    for row in 0..combined.num_rows() {
        if let Some(hex_str) = string_at(&combined, id_col, row).map_err(run_err_to_query)? {
            let bytes = decode_hex_16_q(&hex_str)?;
            row_by_id.insert(bytes, row as u32);
        }
    }
    let mut gather = Vec::with_capacity(winning_ids.len());
    for id in winning_ids {
        if let Some(&row_idx) = row_by_id.get(id) {
            gather.push(row_idx);
        }
    }
    if gather.is_empty() {
        res.free();
        return Ok(Vec::new());
    }
    let mut cols = Vec::with_capacity(combined.columns().len());
    for col in combined.columns() {
        cols.push(Column::Flat(col.take(&gather, Some(&mut res))?));
    }
    let ordered = Batch::try_new_all(view_schema, cols, gather.len())?;
    drop(combined);
    res.free();
    Ok(vec![ordered])
}

/// One page of spans, projected to [`SPAN_LIST_COLUMNS`] with adjacent-copy deduplication.
pub async fn span_list(ctx: &QueryContext, request: SpanListRequest<'_>) -> Result<Page, RunError> {
    span_list_capped(ctx, request, MAX_FETCH).await
}

/// [`span_list`] with a caller-chosen read cap (tests pass a small cap).
pub(crate) async fn span_list_capped(
    ctx: &QueryContext,
    request: SpanListRequest<'_>,
    cap: usize,
) -> Result<Page, RunError> {
    let ctx = ctx.engine();
    if !(1..=MAX_PAGE).contains(&request.limit) {
        return Err(RunError::InvalidRequest(
            "limit must be between 1 and 1000".to_owned(),
        ));
    }
    let sort_col = span_sort_column(request.sort);
    let compiled = compile_optional_filter(FilterView::Spans, request.filter)?;
    let cursor = match request.cursor {
        Some(text) => {
            let decoded = cursor::decode(text, SPANS, sort_col, request.order)?;
            if decoded.tie_len() != SPAN_TIES.len() {
                return Err(RunError::InvalidRequest("invalid cursor".to_owned()));
            }
            Some(decoded)
        }
        None => None,
    };
    let search = present(request.search).map(str::to_owned);
    if search.is_some() && request.scope.window.is_none() {
        return Err(RunError::InvalidRequest(
            "search needs a time range".to_owned(),
        ));
    }

    let deadline = tokio::time::Instant::now() + request.timeout;
    let mut fetch = (request.limit + 1).saturating_mul(2).min(cap);

    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err(RunError::Timeout);
        }

        let batches = run_span_list_attempt(
            ctx,
            &request,
            compiled.as_ref(),
            cursor.as_ref(),
            fetch,
            remaining,
        )
        .await?;

        let read: usize = batches.iter().map(Batch::num_rows).sum();
        let batches = drop_adjacent_span_copies(batches)?;
        let distinct: usize = batches.iter().map(Batch::num_rows).sum();

        if distinct > request.limit || read < fetch {
            return cursor::assemble(
                batches,
                request.limit,
                SPANS,
                sort_col,
                request.order,
                SPAN_TIES,
            );
        }
        if fetch >= cap {
            return capped_span_page(batches, request.limit, sort_col, request.order);
        }
        fetch = fetch.saturating_mul(2).min(cap);
    }
}

async fn run_span_list_attempt(
    ctx: &EngineContext,
    request: &SpanListRequest<'_>,
    compiled: Option<&CompiledFilter>,
    cursor: Option<&Cursor>,
    fetch: usize,
    timeout: Duration,
) -> Result<Vec<Batch>, RunError> {
    let ctx_c = ctx.clone();
    let compiled_c = compiled.cloned();
    let cursor_c = cursor.cloned();
    let search_c = present(request.search).map(str::to_owned);
    let sort = request.sort;
    let sort_col = span_sort_column(sort);
    let scope = request.scope;
    let strict = request.strict_exclusions;
    let order = request.order;

    let (batches, _) = ctx
        .run_with_ctx(timeout, move |exec_ctx| {
            let spans_raw = spans_raw_schema().map_err(run_err_to_query)?;
            let spans_view = spans_view_schema().map_err(run_err_to_query)?;
            let has_scores = compiled_c.as_ref().is_some_and(|f| !f.scores.is_empty());
            let references_environment = !strict
                && compiled_c
                    .as_ref()
                    .is_some_and(|filter| filter.references_environment);

            let effective_scope = if strict {
                ListScope {
                    window: scope.window,
                    include_eval: true,
                    include_experiments: true,
                }
            } else {
                scope
            };
            let mut spec = scoped_spans_spec(&ctx_c, effective_scope, references_environment)
                .map_err(run_err_to_query)?;

            if let Some(text) = &search_c {
                match search_predicate(&ctx_c, exec_ctx, FilterView::Spans, text, scope.window)
                    .map_err(run_err_to_query)?
                {
                    SearchPredicate::None => {}
                    SearchPredicate::Spans { filter, substrings } => {
                        spec.filters.push(filter);
                        if !substrings.is_empty() {
                            spec.substrings.push(substrings);
                        }
                    }
                    SearchPredicate::Traces { .. } => {
                        return Err(QueryError::internal(
                            "unexpected Traces search predicate for Spans",
                        ));
                    }
                }
            }

            let list_with_ingest_names: Vec<&str> = SPAN_LIST_COLUMNS
                .iter()
                .copied()
                .chain(std::iter::once(spans::INGEST_TIME))
                .collect();
            let list_with_ingest_schema =
                projected_view_schema(&spans_view, &list_with_ingest_names)?;
            let list_out_schema = projected_view_schema(&spans_view, SPAN_LIST_COLUMNS)?;

            if !has_scores {
                if let Some(filter) = &compiled_c {
                    let rewritten = rewrite_view_expr_for_raw(
                        &filter.expr,
                        &spans_raw,
                        &[spans::TRACE_ID, spans::SPAN_ID, spans::PARENT_SPAN_ID],
                        false,
                    )?;
                    spec.filters.push(rewritten);
                }
                if let Some(cur) = &cursor_c {
                    let after = cursor::after_expr(cur, &spans_raw, sort_col, SPAN_TIES)
                        .map_err(run_err_to_query)?;
                    spec.filters.push(after);
                }
                populate_id_lookups(&mut spec);

                let raw_proj = span_list_raw_indices(&spans_raw, true).map_err(run_err_to_query)?;

                // M3.5.5 / Opt 5: Two-phase late-materialization Top-K via `StatamSource::take_rows`
                // for all span list sorts (`StartTime` uses wave-ordered early-stop across files;
                // non-time sorts scan all pruned files in Phase 1).
                if !ctx_c.force_range_mode
                    && let Some(spans_statam) = &ctx_c.spans_statam
                {
                    if fetch == 0 {
                        return Ok(Vec::new());
                    }
                    if let Some(vf) = ctx_c.visible_from {
                        spec = apply_visible_from(&spec, &spans_raw, spans::START_TIME, vf)?;
                    }

                    // Phase 1 (disk): narrow Top-K over `(sort_col, span_id, trace_id, ingest_time)`
                    // plus row coordinates `(file_ordinal, row_idx)`.
                    let mut narrow_set = BTreeSet::new();
                    for col_name in [
                        sort_col,
                        spans::SPAN_ID,
                        spans::TRACE_ID,
                        spans::INGEST_TIME,
                    ] {
                        let idx = spans_raw.index_of(col_name).ok_or_else(|| {
                            QueryError::internal(format!("spans schema missing {col_name}"))
                        })?;
                        narrow_set.insert(idx);
                    }
                    let mut p1_spec = spec.clone();
                    p1_spec.projection = Some(narrow_set.into_iter().collect());

                    let mem_target = SpanListTopKTarget {
                        schema: &list_with_ingest_schema,
                        names: &list_with_ingest_names,
                        sort_col,
                        order,
                        fetch,
                    };

                    let (disk_coords, mem_winners) = if sort == SpanSort::StartTime {
                        // Prune disk files first so an empty disk+mem query skips scanning `traces`.
                        let mut kept_files = spans_statam.prune_files_for_spec(&p1_spec)?;
                        if kept_files.is_empty() && ctx_c.spans_mem.is_none() {
                            return Ok(Vec::new());
                        }

                        // Step 1: Pre-build strict exclusions filter once (reused across memtable + waves).
                        let strict_filter = if strict {
                            Some(prepare_strict_exclusions_spans(&ctx_c, exec_ctx)?)
                        } else {
                            None
                        };

                        // Step 2: Scan memtable first so its winners can bound/stop disk waves early.
                        let mem_winners = match &ctx_c.spans_mem {
                            Some(spans_mem) => {
                                let mut mem_spec = spec.clone();
                                mem_spec.projection = Some(raw_proj.clone());
                                let strict_mode = match &strict_filter {
                                    Some(sf) => SpanStrictMode::Prepared(sf),
                                    None => SpanStrictMode::Off,
                                };
                                scan_mem_span_winners(
                                    &ctx_c,
                                    exec_ctx,
                                    spans_mem.as_ref(),
                                    &mem_spec,
                                    &mem_target,
                                    strict_mode,
                                )?
                            }
                            None => Vec::new(),
                        };
                        let mem_times = extract_start_times(&mem_winners)?;
                        let mut combined_times =
                            rebuild_combined_times(&mem_times, &[], order, fetch);

                        // Step 3: Wave-ordered scan across pruned disk files.
                        match order {
                            SortOrder::Desc => kept_files.sort_by(|a, b| {
                                b.1.max_time
                                    .cmp(&a.1.max_time)
                                    .then_with(|| b.1.min_time.cmp(&a.1.min_time))
                                    .then_with(|| a.1.path.cmp(&b.1.path))
                            }),
                            SortOrder::Asc => kept_files.sort_by(|a, b| {
                                a.1.min_time
                                    .cmp(&b.1.min_time)
                                    .then_with(|| a.1.max_time.cmp(&b.1.max_time))
                                    .then_with(|| a.1.path.cmp(&b.1.path))
                            }),
                        }

                        let mut disk_winners: Vec<Batch> = Vec::new();
                        let mut idx = 0usize;
                        let mut wave_size = 1usize;
                        while idx < kept_files.len() {
                            if exec_ctx.eval.cancel.is_cancelled() {
                                return Err(QueryError::Cancelled);
                            }
                            if combined_times.len() >= fetch {
                                let kth_time = combined_times[fetch - 1];
                                let can_stop = match order {
                                    SortOrder::Desc => kth_time > kept_files[idx].1.max_time,
                                    SortOrder::Asc => kth_time < kept_files[idx].1.min_time,
                                };
                                if can_stop {
                                    break;
                                }
                            }

                            let end = (idx + wave_size).min(kept_files.len());
                            let wave_files = kept_files[idx..end].to_vec();
                            idx = end;
                            let remaining = kept_files.len() - idx;
                            wave_size = wave_size.saturating_mul(2).min(remaining);

                            let (mut wave_streams, p1_coord_schema) = spans_statam
                                .open_with_row_coords_for_files(&p1_spec, wave_files, exec_ctx)?;
                            if wave_streams.is_empty() {
                                continue;
                            }
                            if let Some(sf) = &strict_filter {
                                wave_streams = apply_strict_exclusions_spans(
                                    sf,
                                    &ctx_c,
                                    exec_ctx,
                                    wave_streams,
                                    Arc::clone(&p1_coord_schema),
                                )?;
                                if wave_streams.is_empty() {
                                    continue;
                                }
                            }

                            let p1_sort_keys =
                                span_p1_sort_keys(&p1_coord_schema, sort_col, order)?;
                            let sink_schema = Arc::clone(&p1_coord_schema);
                            let sink_keys = p1_sort_keys.clone();
                            let sink_ctx = exec_ctx.clone();
                            let mut wave_sinks = exec_run_pipeline(
                                exec_ctx,
                                wave_streams,
                                |_| Ok(Vec::new()),
                                move |_| {
                                    Ok(TopKSink::new(
                                        Arc::clone(&sink_schema),
                                        sink_keys.clone(),
                                        fetch,
                                        0,
                                        sink_ctx.clone(),
                                    ))
                                },
                            )?;
                            if wave_sinks.iter().all(|s| s.reserved_bytes() == 0) {
                                drop(wave_sinks);
                                continue;
                            }
                            if !disk_winners.is_empty() {
                                let mut prev_sink = TopKSink::new(
                                    Arc::clone(&p1_coord_schema),
                                    p1_sort_keys.clone(),
                                    fetch,
                                    0,
                                    exec_ctx.clone(),
                                );
                                let mut prev_res =
                                    exec_ctx.pool.register("span_list_wave_prev", false);
                                for batch in disk_winners.drain(..) {
                                    if batch.is_empty() {
                                        continue;
                                    }
                                    let bytes = batch.memory_bytes();
                                    prev_res.try_grow(bytes)?;
                                    prev_sink.consume(batch, &mut prev_res)?;
                                    prev_res.free();
                                }
                                wave_sinks.push(prev_sink);
                            }
                            disk_winners = TopKSink::merge(
                                wave_sinks,
                                p1_coord_schema,
                                &p1_sort_keys,
                                0,
                                fetch,
                                exec_ctx,
                            )?;
                            let disk_times = extract_start_times(&disk_winners)?;
                            combined_times =
                                rebuild_combined_times(&mem_times, &disk_times, order, fetch);
                        }

                        // Step 4: Drop strict filter reservation before Phase-2 `take_rows`.
                        drop(strict_filter);
                        let coords = extract_row_coords(&disk_winners)?;
                        drop(disk_winners);
                        (coords, mem_winners)
                    } else {
                        let (mut disk_streams, p1_coord_schema) =
                            spans_statam.open_with_row_coords(&p1_spec, exec_ctx)?;
                        let mut disk_coords: Vec<(u32, u64)> = Vec::new();
                        if !disk_streams.is_empty() {
                            if strict {
                                disk_streams = strict_exclusions_spans(
                                    &ctx_c,
                                    exec_ctx,
                                    disk_streams,
                                    Arc::clone(&p1_coord_schema),
                                    None,
                                )?;
                            }
                            let p1_sort_keys =
                                span_p1_sort_keys(&p1_coord_schema, sort_col, order)?;
                            let sink_schema = Arc::clone(&p1_coord_schema);
                            let sink_keys = p1_sort_keys.clone();
                            let sink_ctx = exec_ctx.clone();
                            let p1_sinks = exec_run_pipeline(
                                exec_ctx,
                                disk_streams,
                                |_| Ok(Vec::new()),
                                move |_| {
                                    Ok(TopKSink::new(
                                        Arc::clone(&sink_schema),
                                        sink_keys.clone(),
                                        fetch,
                                        0,
                                        sink_ctx.clone(),
                                    ))
                                },
                            )?;
                            let p1_merged = TopKSink::merge(
                                p1_sinks,
                                p1_coord_schema,
                                &p1_sort_keys,
                                0,
                                fetch,
                                exec_ctx,
                            )?;
                            disk_coords = extract_row_coords(&p1_merged)?;
                        }

                        let mem_winners = match &ctx_c.spans_mem {
                            Some(spans_mem) => {
                                let mut mem_spec = spec;
                                mem_spec.projection = Some(raw_proj.clone());
                                let strict_mode = if strict {
                                    SpanStrictMode::PerStream
                                } else {
                                    SpanStrictMode::Off
                                };
                                scan_mem_span_winners(
                                    &ctx_c,
                                    exec_ctx,
                                    spans_mem.as_ref(),
                                    &mem_spec,
                                    &mem_target,
                                    strict_mode,
                                )?
                            }
                            None => Vec::new(),
                        };
                        (disk_coords, mem_winners)
                    };

                    // Phase 2: random-access fetch winning disk rows via `take_rows`, combine
                    // with memtable winners, and re-sort into exact `(sort_col, span_id,
                    // trace_id, ingest_time DESC)` order before stripping `ingest_time`.
                    let mut combined_streams: Vec<Box<dyn PartitionStream>> = Vec::new();
                    if !disk_coords.is_empty() {
                        let mut take_res = exec_ctx.pool.register("span_list_p2_take", false);
                        let raw_batches = spans_statam.take_rows(
                            &disk_coords,
                            &raw_proj,
                            exec_ctx,
                            &mut take_res,
                        )?;
                        if !raw_batches.is_empty() {
                            let p2_raw_schema = Arc::new(spans_raw.project(&raw_proj)?);
                            let stream = Box::new(BufferedBatchStream::new(raw_batches, take_res))
                                as Box<dyn PartitionStream>;
                            let proj = project_view_columns(
                                &p2_raw_schema,
                                Arc::clone(&list_with_ingest_schema),
                                &list_with_ingest_names,
                                false,
                                exec_ctx.clone(),
                            )?;
                            combined_streams.push(Box::new(OperatorStream::new(
                                stream,
                                vec![Box::new(proj)],
                                exec_ctx,
                            )));
                        }
                    }
                    if !mem_winners.is_empty() {
                        let mut mem_res = exec_ctx.pool.register("span_list_p2_mem", false);
                        let total_mem = mem_winners
                            .iter()
                            .map(Batch::memory_bytes)
                            .fold(0usize, usize::saturating_add);
                        mem_res.try_grow(total_mem)?;
                        combined_streams
                            .push(Box::new(BufferedBatchStream::new(mem_winners, mem_res)));
                    }
                    if combined_streams.is_empty() {
                        return Ok(Vec::new());
                    }
                    return run_span_topk_and_strip_ingest(
                        combined_streams,
                        list_with_ingest_schema,
                        &list_out_schema,
                        sort_col,
                        order,
                        fetch,
                        exec_ctx,
                    );
                }

                spec.projection = Some(raw_proj);
                let scan_schema = ctx_c.spans.projected_schema(&spec)?;
                let mut streams = ctx_c.spans.open_partitions(&spec, exec_ctx)?;
                if streams.is_empty() {
                    return Ok(Vec::new());
                }
                if strict {
                    streams = strict_exclusions_spans(
                        &ctx_c,
                        exec_ctx,
                        streams,
                        Arc::clone(&scan_schema),
                        None,
                    )?;
                }

                let view_streams: Vec<Box<dyn PartitionStream>> = streams
                    .into_iter()
                    .map(|s| {
                        let proj = project_view_columns(
                            &scan_schema,
                            Arc::clone(&list_with_ingest_schema),
                            &list_with_ingest_names,
                            false,
                            exec_ctx.clone(),
                        )?;
                        Ok(
                            Box::new(OperatorStream::new(s, vec![Box::new(proj)], exec_ctx))
                                as Box<dyn PartitionStream>,
                        )
                    })
                    .collect::<Result<Vec<_>, QueryError>>()?;

                run_span_topk_and_strip_ingest(
                    view_streams,
                    list_with_ingest_schema,
                    &list_out_schema,
                    sort_col,
                    order,
                    fetch,
                    exec_ctx,
                )
            } else {
                let mut streams = ctx_c.spans.open_partitions(&spec, exec_ctx)?;
                if streams.is_empty() {
                    return Ok(Vec::new());
                }
                if strict {
                    streams = strict_exclusions_spans(
                        &ctx_c,
                        exec_ctx,
                        streams,
                        Arc::clone(&spans_raw),
                        None,
                    )?;
                }

                let mut view_streams: Vec<Box<dyn PartitionStream>> = streams
                    .into_iter()
                    .map(|s| {
                        let proj = spans_view_projection(&spans_raw, exec_ctx.clone())?;
                        Ok(
                            Box::new(OperatorStream::new(s, vec![Box::new(proj)], exec_ctx))
                                as Box<dyn PartitionStream>,
                        )
                    })
                    .collect::<Result<Vec<_>, QueryError>>()?;

                if let Some(filter) = &compiled_c {
                    view_streams =
                        apply_compiled_filter(&ctx_c, exec_ctx, view_streams, &spans_view, filter)?;
                }
                if let Some(cur) = &cursor_c {
                    let after = cursor::after_expr(cur, &spans_view, sort_col, SPAN_TIES)
                        .map_err(run_err_to_query)?;
                    view_streams = view_streams
                        .into_iter()
                        .map(|s| {
                            let f = FilterOp::new(after.clone(), exec_ctx.clone());
                            Box::new(OperatorStream::new(s, vec![Box::new(f)], exec_ctx))
                                as Box<dyn PartitionStream>
                        })
                        .collect();
                }

                let narrowed: Vec<Box<dyn PartitionStream>> = view_streams
                    .into_iter()
                    .map(|s| {
                        let proj = project_view_columns(
                            &spans_view,
                            Arc::clone(&list_with_ingest_schema),
                            &list_with_ingest_names,
                            false,
                            exec_ctx.clone(),
                        )?;
                        Ok(
                            Box::new(OperatorStream::new(s, vec![Box::new(proj)], exec_ctx))
                                as Box<dyn PartitionStream>,
                        )
                    })
                    .collect::<Result<Vec<_>, QueryError>>()?;

                run_span_topk_and_strip_ingest(
                    narrowed,
                    list_with_ingest_schema,
                    &list_out_schema,
                    sort_col,
                    order,
                    fetch,
                    exec_ctx,
                )
            }
        })
        .await?;

    Ok(batches)
}

struct SpanListTopKTarget<'a> {
    schema: &'a Arc<Schema>,
    names: &'a [&'a str],
    sort_col: &'a str,
    order: SortOrder,
    fetch: usize,
}

#[derive(Clone, Copy)]
enum SpanStrictMode<'a> {
    Off,
    PerStream,
    Prepared(&'a StrictExclusionsFilter),
}

fn span_p1_sort_keys(
    schema: &Arc<Schema>,
    sort_col: &str,
    order: SortOrder,
) -> Result<Vec<SortKey>, QueryError> {
    let mut sort_keys =
        cursor::order_by(schema, sort_col, SPAN_TIES, order).map_err(run_err_to_query)?;
    sort_keys.push(SortKey {
        expr: col_expr_q(schema, spans::INGEST_TIME)?,
        descending: true,
        nulls_first: false,
    });
    Ok(sort_keys)
}

fn extract_row_coords(batches: &[Batch]) -> Result<Vec<(u32, u64)>, QueryError> {
    let mut coords = Vec::new();
    for batch in batches {
        if batch.is_empty() {
            continue;
        }
        let file_col = batch
            .column_by_name(ROW_COORD_FILE_COL)
            .ok_or_else(|| QueryError::internal("missing row coord file_ordinal"))?;
        let row_col = batch
            .column_by_name(ROW_COORD_ROW_COL)
            .ok_or_else(|| QueryError::internal("missing row coord row_idx"))?;
        for phys_row in active_rows(batch) {
            let f_i64 = raw_i64_at(file_col, phys_row).map_err(run_err_to_query)?;
            let r_i64 = raw_i64_at(row_col, phys_row).map_err(run_err_to_query)?;
            let file_ord = u32::try_from(f_i64)
                .map_err(|_| QueryError::internal("invalid row coord file_ordinal"))?;
            let row_idx = u64::try_from(r_i64)
                .map_err(|_| QueryError::internal("invalid row coord row_idx"))?;
            coords.push((file_ord, row_idx));
        }
    }
    Ok(coords)
}

fn extract_start_times(batches: &[Batch]) -> Result<Vec<i64>, QueryError> {
    let mut times = Vec::new();
    for batch in batches {
        if batch.is_empty() {
            continue;
        }
        let start_col = require_col(batch, spans::START_TIME).map_err(run_err_to_query)?;
        for phys_row in active_rows(batch) {
            if !start_col.is_null(phys_row) {
                times.push(raw_time_at(start_col, phys_row).map_err(run_err_to_query)?);
            }
        }
    }
    Ok(times)
}

fn rebuild_combined_times(
    mem_times: &[i64],
    disk_times: &[i64],
    order: SortOrder,
    fetch: usize,
) -> Vec<i64> {
    let mut combined = Vec::with_capacity(mem_times.len() + disk_times.len());
    combined.extend_from_slice(mem_times);
    combined.extend_from_slice(disk_times);
    match order {
        SortOrder::Desc => combined.sort_unstable_by(|a, b| b.cmp(a)),
        SortOrder::Asc => combined.sort_unstable(),
    }
    combined.truncate(fetch);
    combined
}

fn scan_mem_span_winners(
    ctx: &EngineContext,
    exec_ctx: &ExecContext,
    spans_mem: &dyn ScanSource,
    mem_spec: &ScanSpec,
    target: &SpanListTopKTarget<'_>,
    strict_mode: SpanStrictMode<'_>,
) -> Result<Vec<Batch>, QueryError> {
    let mem_scan_schema = spans_mem.projected_schema(mem_spec)?;
    let mut mem_streams = spans_mem.open_partitions(mem_spec, exec_ctx)?;
    if mem_streams.is_empty() {
        return Ok(Vec::new());
    }
    match strict_mode {
        SpanStrictMode::Off => {}
        SpanStrictMode::PerStream => {
            mem_streams = strict_exclusions_spans(
                ctx,
                exec_ctx,
                mem_streams,
                Arc::clone(&mem_scan_schema),
                None,
            )?;
        }
        SpanStrictMode::Prepared(sf) => {
            mem_streams = apply_strict_exclusions_spans(
                sf,
                ctx,
                exec_ctx,
                mem_streams,
                Arc::clone(&mem_scan_schema),
            )?;
        }
    }
    if mem_streams.is_empty() {
        return Ok(Vec::new());
    }
    let mem_view_streams: Vec<Box<dyn PartitionStream>> = mem_streams
        .into_iter()
        .map(|s| {
            let proj = project_view_columns(
                &mem_scan_schema,
                Arc::clone(target.schema),
                target.names,
                false,
                exec_ctx.clone(),
            )?;
            Ok(
                Box::new(OperatorStream::new(s, vec![Box::new(proj)], exec_ctx))
                    as Box<dyn PartitionStream>,
            )
        })
        .collect::<Result<Vec<_>, QueryError>>()?;
    let mem_sort_keys = span_p1_sort_keys(target.schema, target.sort_col, target.order)?;
    let sink_schema = Arc::clone(target.schema);
    let sink_keys = mem_sort_keys.clone();
    let sink_ctx = exec_ctx.clone();
    let fetch = target.fetch;
    let mem_sinks = exec_run_pipeline(
        exec_ctx,
        mem_view_streams,
        |_| Ok(Vec::new()),
        move |_| {
            Ok(TopKSink::new(
                Arc::clone(&sink_schema),
                sink_keys.clone(),
                fetch,
                0,
                sink_ctx.clone(),
            ))
        },
    )?;
    TopKSink::merge(
        mem_sinks,
        Arc::clone(target.schema),
        &mem_sort_keys,
        0,
        fetch,
        exec_ctx,
    )
}

fn run_span_topk_and_strip_ingest(
    streams: Vec<Box<dyn PartitionStream>>,
    with_ingest_schema: Arc<Schema>,
    out_schema: &Arc<Schema>,
    sort_col: &str,
    order: SortOrder,
    fetch: usize,
    exec_ctx: &ExecContext,
) -> Result<Vec<Batch>, QueryError> {
    let sort_keys = span_p1_sort_keys(&with_ingest_schema, sort_col, order)?;

    let sink_schema = Arc::clone(&with_ingest_schema);
    let sink_keys = sort_keys.clone();
    let sink_ctx = exec_ctx.clone();
    let sinks = exec_run_pipeline(
        exec_ctx,
        streams,
        |_| Ok(Vec::new()),
        move |_| {
            Ok(TopKSink::new(
                Arc::clone(&sink_schema),
                sink_keys.clone(),
                fetch,
                0,
                sink_ctx.clone(),
            ))
        },
    )?;
    let merged = TopKSink::merge(sinks, with_ingest_schema, &sort_keys, 0, fetch, exec_ctx)?;
    if merged.is_empty() {
        return Ok(Vec::new());
    }

    let keep_indices: Vec<usize> = (0..out_schema.len()).collect();
    let mut out = Vec::with_capacity(merged.len());
    for batch in merged {
        if batch.is_empty() {
            continue;
        }
        out.push(batch.into_project(&keep_indices)?);
    }
    Ok(out)
}

fn drop_adjacent_span_copies(batches: Vec<Batch>) -> Result<Vec<Batch>, RunError> {
    let mut prev: Option<(Option<i64>, Option<String>, Option<String>)> = None;
    let mut kept = Vec::with_capacity(batches.len());

    for batch in batches {
        if batch.is_empty() {
            continue;
        }
        let start_col = require_col(&batch, SPAN_KEY[0])?;
        let span_col = require_col(&batch, SPAN_KEY[1])?;
        let trace_col = require_col(&batch, SPAN_KEY[2])?;

        let mut kept_indices: Vec<u32> = Vec::with_capacity(batch.num_rows());
        for phys_row in active_rows(&batch) {
            let start = if start_col.is_null(phys_row) {
                None
            } else {
                Some(raw_time_at(start_col, phys_row)?)
            };
            let span_id = raw_string_at(span_col, phys_row)?;
            let trace_id = raw_string_at(trace_col, phys_row)?;
            let cur = (start, span_id, trace_id);
            if prev.as_ref() != Some(&cur) {
                kept_indices.push(phys_row as u32);
            }
            prev = Some(cur);
        }

        if !kept_indices.is_empty() {
            let filtered = batch
                .into_with_selection(Selection::Indices(kept_indices))
                .map_err(|e| internal(format!("dropping span copies failed: {e}")))?;
            kept.push(filtered);
        }
    }
    Ok(kept)
}

fn capped_span_page(
    mut batches: Vec<Batch>,
    limit: usize,
    sort_col: &str,
    order: SortOrder,
) -> Result<Page, RunError> {
    let distinct: usize = batches.iter().map(Batch::num_rows).sum();
    if distinct > limit {
        return cursor::assemble(batches, limit, SPANS, sort_col, order, SPAN_TIES);
    }
    if let Some(last) = batches.iter().rev().find(|b| b.num_rows() > 0)
        && let Some(last_phys) = active_rows(last).last()
    {
        let tail = last
            .clone()
            .into_with_selection(Selection::Indices(vec![last_phys as u32]))
            .map_err(|e| internal(format!("duplicating tail span row failed: {e}")))?;
        batches.push(tail);
    }
    cursor::assemble(batches, distinct, SPANS, sort_col, order, SPAN_TIES)
}

/// One trace and its spans (`SPAN_LIST_COLUMNS`, ordered `start_time ASC, span_id ASC`,
/// capped at [`MAX_TRACE_SPANS`]). Default exclusions do NOT apply. Uses ONLY caller `ctx.around`.
pub async fn trace_detail(
    ctx: &QueryContext,
    trace_id: &str,
    timeout: Duration,
) -> Result<Option<TraceDetailData>, RunError> {
    trace_detail_limited(ctx, trace_id, timeout, MAX_TRACE_SPANS).await
}

/// [`trace_detail`] with a caller-chosen span cap (tests pass a small cap).
pub(crate) async fn trace_detail_limited(
    ctx: &QueryContext,
    trace_id: &str,
    timeout: Duration,
    cap: usize,
) -> Result<Option<TraceDetailData>, RunError> {
    let ctx = ctx.engine();
    let trace_bytes = decode_hex_16(trace_id, 32, "trace_id")?;
    let ctx_c = ctx.clone();

    let (detail, _) = ctx
        .run_with_ctx(timeout, move |exec_ctx| {
            let traces_raw = traces_raw_schema().map_err(run_err_to_query)?;
            let t_filters = vec![eq_fixed16(&traces_raw, traces::TRACE_ID, trace_bytes)?];
            let t_spec = ScanSpec {
                projection: None,
                map_projections: Default::default(),
                time_range: TimeRangeHint {
                    min_ns: None,
                    max_ns: None,
                    around_ns: ctx_c.around,
                },
                id_lookups: vec![IdLookupHint::Fixed16 {
                    column: traces::TRACE_ID.to_owned(),
                    values: vec![trace_bytes],
                }],
                filters: t_filters,
                substrings: Vec::new(),
                sort_hints: Vec::new(),
                limit: Some(1),
            };
            let t_streams = ctx_c.traces.open_partitions(&t_spec, exec_ctx)?;
            if t_streams.is_empty() {
                return Ok(None);
            }
            let pool = Arc::clone(&exec_ctx.pool);
            let proj_ctx = exec_ctx.clone();
            let raw_c = Arc::clone(&traces_raw);
            let t_sinks = exec_run_pipeline(
                exec_ctx,
                t_streams,
                move |_| {
                    let proj = traces_view_projection(&raw_c, proj_ctx.clone())?;
                    Ok(vec![Box::new(proj) as Box<dyn PushOperator>])
                },
                move |_| Ok(CollectSink::new(&pool)),
            )?;
            let t_batches: Vec<Batch> = t_sinks
                .into_iter()
                .flat_map(CollectSink::into_batches)
                .collect();
            let Some(trace) = single_row(&t_batches, exec_ctx)? else {
                return Ok(None);
            };

            // Spans query: SPAN_LIST_COLUMNS, ordered start_time ASC, span_id ASC, limit cap + 1.
            let spans_raw = spans_raw_schema().map_err(run_err_to_query)?;
            let spans_view = spans_view_schema().map_err(run_err_to_query)?;
            let s_proj = span_list_raw_indices(&spans_raw, false).map_err(run_err_to_query)?;
            let s_filters = vec![eq_fixed16(&spans_raw, spans::TRACE_ID, trace_bytes)?];
            let s_spec = ScanSpec {
                projection: Some(s_proj),
                map_projections: Default::default(),
                time_range: TimeRangeHint {
                    min_ns: None,
                    max_ns: None,
                    around_ns: ctx_c.around,
                },
                id_lookups: vec![IdLookupHint::Fixed16 {
                    column: spans::TRACE_ID.to_owned(),
                    values: vec![trace_bytes],
                }],
                filters: s_filters,
                substrings: Vec::new(),
                sort_hints: Vec::new(),
                limit: None,
            };

            let s_scan_schema = ctx_c.spans.projected_schema(&s_spec)?;
            let s_streams = ctx_c.spans.open_partitions(&s_spec, exec_ctx)?;
            if s_streams.is_empty() {
                return Ok(Some(TraceDetailData {
                    trace,
                    spans: Vec::new(),
                    truncated: false,
                }));
            }
            let list_schema = projected_view_schema(&spans_view, SPAN_LIST_COLUMNS)?;
            let s_view_streams: Vec<Box<dyn PartitionStream>> = s_streams
                .into_iter()
                .map(|s| {
                    let proj = project_view_columns(
                        &s_scan_schema,
                        Arc::clone(&list_schema),
                        SPAN_LIST_COLUMNS,
                        false,
                        exec_ctx.clone(),
                    )?;
                    Ok(
                        Box::new(OperatorStream::new(s, vec![Box::new(proj)], exec_ctx))
                            as Box<dyn PartitionStream>,
                    )
                })
                .collect::<Result<Vec<_>, QueryError>>()?;

            let sort_keys = cursor::order_by(
                &list_schema,
                spans::START_TIME,
                &[spans::SPAN_ID],
                SortOrder::Asc,
            )
            .map_err(run_err_to_query)?;
            let fetch = cap.saturating_add(1);
            let sink_schema = Arc::clone(&list_schema);
            let sink_keys = sort_keys.clone();
            let sink_ctx = exec_ctx.clone();
            let s_sinks = exec_run_pipeline(
                exec_ctx,
                s_view_streams,
                |_| Ok(Vec::new()),
                move |_| {
                    Ok(TopKSink::new(
                        Arc::clone(&sink_schema),
                        sink_keys.clone(),
                        fetch,
                        0,
                        sink_ctx.clone(),
                    ))
                },
            )?;
            let merged = TopKSink::merge(s_sinks, list_schema, &sort_keys, 0, fetch, exec_ctx)?;
            let (spans, truncated) = cap_rows(merged, cap).map_err(run_err_to_query)?;
            Ok(Some(TraceDetailData {
                trace,
                spans,
                truncated,
            }))
        })
        .await?;

    Ok(detail)
}

/// All 56 columns of the `spans` view for `(trace_id, span_id)` (greatest `ingest_time`).
pub async fn span_detail(
    ctx: &QueryContext,
    trace_id: &str,
    span_id: &str,
    timeout: Duration,
) -> Result<Option<Batch>, RunError> {
    let ctx = ctx.engine();
    let trace_bytes = decode_hex_16(trace_id, 32, "trace_id")?;
    let span_bytes = decode_hex_8(span_id, 16, "span_id")?;
    let ctx_c = ctx.clone();

    let (row, _) = ctx
        .run_with_ctx(timeout, move |exec_ctx| {
            let spans_raw = spans_raw_schema().map_err(run_err_to_query)?;
            let spans_view = spans_view_schema().map_err(run_err_to_query)?;
            let filters = vec![
                eq_fixed16(&spans_raw, spans::TRACE_ID, trace_bytes)?,
                eq_fixed8(&spans_raw, spans::SPAN_ID, span_bytes)?,
            ];
            let spec = ScanSpec {
                projection: None,
                map_projections: Default::default(),
                time_range: TimeRangeHint {
                    min_ns: None,
                    max_ns: None,
                    around_ns: ctx_c.around,
                },
                id_lookups: vec![
                    IdLookupHint::Fixed16 {
                        column: spans::TRACE_ID.to_owned(),
                        values: vec![trace_bytes],
                    },
                    IdLookupHint::Fixed8 {
                        column: spans::SPAN_ID.to_owned(),
                        values: vec![span_bytes],
                    },
                ],
                filters,
                substrings: Vec::new(),
                sort_hints: Vec::new(),
                limit: None,
            };
            let streams = ctx_c.spans.open_partitions(&spec, exec_ctx)?;
            if streams.is_empty() {
                return Ok(None);
            }
            let view_streams: Vec<Box<dyn PartitionStream>> = streams
                .into_iter()
                .map(|s| {
                    let proj = spans_view_projection(&spans_raw, exec_ctx.clone())?;
                    Ok(
                        Box::new(OperatorStream::new(s, vec![Box::new(proj)], exec_ctx))
                            as Box<dyn PartitionStream>,
                    )
                })
                .collect::<Result<Vec<_>, QueryError>>()?;

            let sort_keys = cursor::order_by(&spans_view, spans::INGEST_TIME, &[], SortOrder::Desc)
                .map_err(run_err_to_query)?;
            let sink_schema = Arc::clone(&spans_view);
            let sink_keys = sort_keys.clone();
            let sink_ctx = exec_ctx.clone();
            let sinks = exec_run_pipeline(
                exec_ctx,
                view_streams,
                |_| Ok(Vec::new()),
                move |_| {
                    Ok(TopKSink::new(
                        Arc::clone(&sink_schema),
                        sink_keys.clone(),
                        1,
                        0,
                        sink_ctx.clone(),
                    ))
                },
            )?;
            let merged = TopKSink::merge(sinks, spans_view, &sort_keys, 0, 1, exec_ctx)?;
            single_row(&merged, exec_ctx)
        })
        .await?;

    Ok(row)
}

/// Every column of one trace's spans: `required` (roots + target) and `earliest` (`max_spans`).
pub async fn trace_spans(
    ctx: &QueryContext,
    request: TraceSpansRequest<'_>,
) -> Result<TraceSpansData, RunError> {
    let ctx = ctx.engine();
    let trace_bytes = decode_hex_16(request.trace_id, 32, "trace_id")?;
    let target_bytes = request
        .target_span_id
        .map(|id| decode_hex_8(id, 16, "span_id"))
        .transpose()?;
    if request.max_spans == 0 || request.max_required == 0 {
        return Err(RunError::InvalidRequest(
            "max_spans and max_required must be at least 1".to_owned(),
        ));
    }

    let ctx_c = ctx.clone();
    let max_spans = request.max_spans;
    let max_required = request.max_required;

    let (data, _) = ctx
        .run_with_ctx(request.timeout, move |exec_ctx| {
            let spans_raw = spans_raw_schema().map_err(run_err_to_query)?;
            let spans_view = spans_view_schema().map_err(run_err_to_query)?;

            // 1. Required spans: parent_span_id IS NULL [OR span_id = target], limit max_required + 1.
            let parent_null = Expr::Unary {
                op: cetra_query::UnaryOp::IsNull,
                expr: Box::new(col_expr_q(&spans_raw, spans::PARENT_SPAN_ID)?),
                data_type: DataType::Bool,
            };
            let root_or_target = match target_bytes {
                Some(tb) => Expr::Binary {
                    left: Box::new(parent_null),
                    op: BinaryOp::Or,
                    right: Box::new(eq_fixed8(&spans_raw, spans::SPAN_ID, tb)?),
                    data_type: DataType::Bool,
                },
                None => parent_null,
            };
            let req_filters = vec![
                eq_fixed16(&spans_raw, spans::TRACE_ID, trace_bytes)?,
                root_or_target,
            ];
            let req_spec = ScanSpec {
                projection: None,
                map_projections: Default::default(),
                time_range: TimeRangeHint {
                    min_ns: None,
                    max_ns: None,
                    around_ns: ctx_c.around,
                },
                id_lookups: vec![IdLookupHint::Fixed16 {
                    column: spans::TRACE_ID.to_owned(),
                    values: vec![trace_bytes],
                }],
                filters: req_filters,
                substrings: Vec::new(),
                sort_hints: Vec::new(),
                limit: None,
            };

            let req_streams = ctx_c.spans.open_partitions(&req_spec, exec_ctx)?;
            let required = if req_streams.is_empty() {
                Vec::new()
            } else {
                let shared_rem = Arc::new(AtomicUsize::new(max_required.saturating_add(1)));
                let pool = Arc::clone(&exec_ctx.pool);
                let proj_ctx = exec_ctx.clone();
                let raw_c = Arc::clone(&spans_raw);
                let sinks = exec_run_pipeline(
                    exec_ctx,
                    req_streams,
                    move |_| {
                        let proj = spans_view_projection(&raw_c, proj_ctx.clone())?;
                        let lim = LimitOp::new(0, None, Some(Arc::clone(&shared_rem)));
                        Ok(vec![
                            Box::new(proj) as Box<dyn PushOperator>,
                            Box::new(lim) as Box<dyn PushOperator>,
                        ])
                    },
                    move |_| Ok(CollectSink::new(&pool)),
                )?;
                sinks
                    .into_iter()
                    .flat_map(CollectSink::into_batches)
                    .collect()
            };

            // 2. Earliest spans: start_time ASC, span_id ASC, limit max_spans + 1.
            let early_filters = vec![eq_fixed16(&spans_raw, spans::TRACE_ID, trace_bytes)?];
            let early_spec = ScanSpec {
                projection: None,
                map_projections: Default::default(),
                time_range: TimeRangeHint {
                    min_ns: None,
                    max_ns: None,
                    around_ns: ctx_c.around,
                },
                id_lookups: vec![IdLookupHint::Fixed16 {
                    column: spans::TRACE_ID.to_owned(),
                    values: vec![trace_bytes],
                }],
                filters: early_filters,
                substrings: Vec::new(),
                sort_hints: Vec::new(),
                limit: None,
            };
            let early_streams = ctx_c.spans.open_partitions(&early_spec, exec_ctx)?;
            let (earliest, truncated) = if early_streams.is_empty() {
                (Vec::new(), false)
            } else {
                let view_streams: Vec<Box<dyn PartitionStream>> = early_streams
                    .into_iter()
                    .map(|s| {
                        let proj = spans_view_projection(&spans_raw, exec_ctx.clone())?;
                        Ok(
                            Box::new(OperatorStream::new(s, vec![Box::new(proj)], exec_ctx))
                                as Box<dyn PartitionStream>,
                        )
                    })
                    .collect::<Result<Vec<_>, QueryError>>()?;
                let sort_keys = cursor::order_by(
                    &spans_view,
                    spans::START_TIME,
                    &[spans::SPAN_ID],
                    SortOrder::Asc,
                )
                .map_err(run_err_to_query)?;
                let fetch = max_spans.saturating_add(1);
                let sink_schema = Arc::clone(&spans_view);
                let sink_keys = sort_keys.clone();
                let sink_ctx = exec_ctx.clone();
                let sinks = exec_run_pipeline(
                    exec_ctx,
                    view_streams,
                    |_| Ok(Vec::new()),
                    move |_| {
                        Ok(TopKSink::new(
                            Arc::clone(&sink_schema),
                            sink_keys.clone(),
                            fetch,
                            0,
                            sink_ctx.clone(),
                        ))
                    },
                )?;
                let merged = TopKSink::merge(sinks, spans_view, &sort_keys, 0, fetch, exec_ctx)?;
                cap_rows(merged, max_spans).map_err(run_err_to_query)?
            };

            Ok(TraceSpansData {
                required,
                earliest,
                truncated,
            })
        })
        .await?;

    Ok(data)
}

/// Re-reads the `traces` view rows of `trace_ids` matching exclusions, filter, and search.
pub async fn trace_refresh(
    ctx: &QueryContext,
    request: TraceRefreshRequest<'_>,
) -> Result<Vec<Batch>, RunError> {
    let ctx = ctx.engine();
    let compiled = compile_optional_filter(FilterView::Traces, request.filter)?;
    if request.trace_ids.len() > MAX_REFRESH_IDS {
        return Err(RunError::InvalidRequest(format!(
            "at most {MAX_REFRESH_IDS} trace ids"
        )));
    }
    let ids = request
        .trace_ids
        .iter()
        .map(|id| decode_hex_16(id, 32, "trace_id"))
        .collect::<Result<Vec<_>, _>>()?;
    if ids.is_empty() {
        return Ok(Vec::new());
    }

    let ctx_c = ctx.clone();
    let search = request
        .search
        .and_then(|(text, window)| present(Some(text)).map(|t| (t.to_owned(), window)));
    let include_eval = request.include_eval;
    let include_experiments = request.include_experiments;
    let strict = request.strict_exclusions;

    let (batches, _) = ctx
        .run_with_ctx(request.timeout, move |exec_ctx| {
            let traces_raw = traces_raw_schema().map_err(run_err_to_query)?;
            let traces_view = traces_view_schema().map_err(run_err_to_query)?;
            let has_scores = compiled.as_ref().is_some_and(|f| !f.scores.is_empty());
            let references_environment = !strict
                && compiled
                    .as_ref()
                    .is_some_and(|filter| filter.references_environment);
            let scope = if strict {
                ListScope {
                    window: None,
                    include_eval: false,
                    include_experiments: false,
                }
            } else {
                ListScope {
                    window: None,
                    include_eval,
                    include_experiments,
                }
            };

            let mut spec = scoped_traces_spec(&ctx_c, scope, references_environment)
                .map_err(run_err_to_query)?;
            spec.filters
                .push(in_list_fixed16(&traces_raw, traces::TRACE_ID, &ids)?);
            spec.id_lookups.push(IdLookupHint::Fixed16 {
                column: traces::TRACE_ID.to_owned(),
                values: ids,
            });

            if !has_scores {
                if let Some(filter) = &compiled {
                    let rewritten = rewrite_view_expr_for_raw(
                        &filter.expr,
                        &traces_raw,
                        &[traces::TRACE_ID, traces::ROOT_SPAN_ID],
                        true,
                    )?;
                    spec.filters.push(rewritten);
                }
                if strict {
                    let strict_indices = TRACE_REFRESH_STRICT_COLUMNS
                        .iter()
                        .map(|name| {
                            traces_raw.index_of(name).ok_or_else(|| {
                                QueryError::internal(format!("traces schema missing column {name}"))
                            })
                        })
                        .collect::<Result<Vec<_>, _>>()?;
                    spec.projection = Some(strict_indices);
                }
            }

            let scan_schema = ctx_c.traces.projected_schema(&spec)?;
            let mut streams = ctx_c.traces.open_partitions(&spec, exec_ctx)?;
            if streams.is_empty() {
                return Ok(Vec::new());
            }

            if let Some((text, window)) = &search {
                match search_predicate(&ctx_c, exec_ctx, FilterView::Traces, text, Some(*window))
                    .map_err(run_err_to_query)?
                {
                    SearchPredicate::None => {}
                    SearchPredicate::Traces { trace_ids, schema } => {
                        if trace_ids.is_empty() {
                            return Ok(Vec::new());
                        }
                        let left_key = col_expr_q(&scan_schema, traces::TRACE_ID)?;
                        let right_key = col_expr_q(&schema, traces::TRACE_ID)?;
                        let joined = execute_hash_join(
                            streams,
                            Arc::clone(&scan_schema),
                            trace_ids,
                            schema,
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
                    SearchPredicate::Spans { .. } => {
                        return Err(QueryError::internal(
                            "unexpected Spans search predicate for Traces",
                        ));
                    }
                }
            }

            if !has_scores {
                let strict_schema = if strict {
                    Some(projected_view_schema(
                        &traces_view,
                        TRACE_REFRESH_STRICT_COLUMNS,
                    )?)
                } else {
                    None
                };
                let pool = Arc::clone(&exec_ctx.pool);
                let proj_ctx = exec_ctx.clone();
                let scan_c = Arc::clone(&scan_schema);
                let sinks = exec_run_pipeline(
                    exec_ctx,
                    streams,
                    move |_| {
                        let proj = if let Some(ss) = &strict_schema {
                            project_view_columns(
                                &scan_c,
                                Arc::clone(ss),
                                TRACE_REFRESH_STRICT_COLUMNS,
                                true,
                                proj_ctx.clone(),
                            )?
                        } else {
                            traces_view_projection(&scan_c, proj_ctx.clone())?
                        };
                        Ok(vec![Box::new(proj) as Box<dyn PushOperator>])
                    },
                    move |_| Ok(CollectSink::new(&pool)),
                )?;
                return Ok(sinks
                    .into_iter()
                    .flat_map(CollectSink::into_batches)
                    .collect());
            }

            let view_streams: Vec<Box<dyn PartitionStream>> = streams
                .into_iter()
                .map(|s| {
                    let proj = traces_view_projection(&scan_schema, exec_ctx.clone())?;
                    Ok(
                        Box::new(OperatorStream::new(s, vec![Box::new(proj)], exec_ctx))
                            as Box<dyn PartitionStream>,
                    )
                })
                .collect::<Result<Vec<_>, QueryError>>()?;

            let filtered = match &compiled {
                Some(filter) => {
                    apply_compiled_filter(&ctx_c, exec_ctx, view_streams, &traces_view, filter)?
                }
                None => view_streams,
            };

            let pool = Arc::clone(&exec_ctx.pool);
            let strict_schema = if strict {
                Some(projected_view_schema(
                    &traces_view,
                    TRACE_REFRESH_STRICT_COLUMNS,
                )?)
            } else {
                None
            };
            let tv = Arc::clone(&traces_view);
            let proj_ctx = exec_ctx.clone();
            let sinks = exec_run_pipeline(
                exec_ctx,
                filtered,
                move |_| {
                    if let Some(ss) = &strict_schema {
                        let proj = project_view_columns(
                            &tv,
                            Arc::clone(ss),
                            TRACE_REFRESH_STRICT_COLUMNS,
                            true,
                            proj_ctx.clone(),
                        )?;
                        Ok(vec![Box::new(proj) as Box<dyn PushOperator>])
                    } else {
                        Ok(Vec::new())
                    }
                },
                move |_| Ok(CollectSink::new(&pool)),
            )?;
            Ok(sinks
                .into_iter()
                .flat_map(CollectSink::into_batches)
                .collect())
        })
        .await?;

    Ok(batches)
}

/// First `per_trace` spans of each trace in `trace_ids`, ordered `(trace_id, start_time, span_id)` ASC.
pub async fn span_refresh(
    ctx: &QueryContext,
    request: SpanRefreshRequest<'_>,
) -> Result<Vec<Batch>, RunError> {
    let ctx = ctx.engine();
    let compiled = compile_optional_filter(FilterView::Spans, request.filter)?;
    if request.trace_ids.len() > MAX_REFRESH_IDS {
        return Err(RunError::InvalidRequest(format!(
            "at most {MAX_REFRESH_IDS} trace ids"
        )));
    }
    let ids = request
        .trace_ids
        .iter()
        .map(|id| decode_hex_16(id, 32, "trace_id"))
        .collect::<Result<Vec<_>, _>>()?;
    if !(1..=MAX_REFRESH_SPANS_PER_TRACE).contains(&request.per_trace) {
        return Err(RunError::InvalidRequest(format!(
            "per_trace must be between 1 and {MAX_REFRESH_SPANS_PER_TRACE}"
        )));
    }
    if ids.is_empty() {
        return Ok(Vec::new());
    }

    run_per_trace_spans(
        ctx,
        PerTraceSpanParams {
            compiled,
            ids,
            per_trace: request.per_trace,
            strict: request.strict_exclusions,
            only_errors: false,
            out_cols: SPAN_REFRESH_COLUMNS,
            timeout: request.timeout,
        },
    )
    .await
}

/// First 50 error spans per trace in `trace_ids` passing strict exclusions + filter.
pub async fn error_spans(
    ctx: &QueryContext,
    request: ErrorSpansRequest<'_>,
) -> Result<Vec<Batch>, RunError> {
    let ctx = ctx.engine();
    let compiled = compile_optional_filter(FilterView::Spans, request.filter)?;
    if request.trace_ids.len() > MAX_REFRESH_IDS {
        return Err(RunError::InvalidRequest(format!(
            "at most {MAX_REFRESH_IDS} trace ids"
        )));
    }
    let ids = request
        .trace_ids
        .iter()
        .map(|id| decode_hex_16(id, 32, "trace_id"))
        .collect::<Result<Vec<_>, _>>()?;
    if ids.is_empty() {
        return Ok(Vec::new());
    }

    run_per_trace_spans(
        ctx,
        PerTraceSpanParams {
            compiled,
            ids,
            per_trace: MAX_ERROR_SPANS_PER_TRACE,
            strict: true,
            only_errors: true,
            out_cols: ERROR_SPAN_COLUMNS,
            timeout: request.timeout,
        },
    )
    .await
}

struct PerTraceSpanParams {
    compiled: Option<CompiledFilter>,
    ids: Vec<[u8; 16]>,
    per_trace: usize,
    strict: bool,
    only_errors: bool,
    out_cols: &'static [&'static str],
    timeout: Duration,
}

async fn run_per_trace_spans(
    ctx: &EngineContext,
    params: PerTraceSpanParams,
) -> Result<Vec<Batch>, RunError> {
    let PerTraceSpanParams {
        compiled,
        ids,
        per_trace,
        strict,
        only_errors,
        out_cols,
        timeout,
    } = params;
    let ctx_c = ctx.clone();

    let (batches, _) = ctx
        .run_with_ctx(timeout, move |exec_ctx| {
            let spans_raw = spans_raw_schema().map_err(run_err_to_query)?;
            let spans_view = spans_view_schema().map_err(run_err_to_query)?;
            let has_scores = compiled.as_ref().is_some_and(|f| !f.scores.is_empty());
            let references_environment = !strict
                && compiled
                    .as_ref()
                    .is_some_and(|filter| filter.references_environment);

            let scope = ListScope {
                window: None,
                include_eval: strict,
                include_experiments: strict,
            };
            let mut spec = scoped_spans_spec(&ctx_c, scope, references_environment)
                .map_err(run_err_to_query)?;
            spec.filters
                .push(in_list_fixed16(&spans_raw, spans::TRACE_ID, &ids)?);
            spec.id_lookups.push(IdLookupHint::Fixed16 {
                column: spans::TRACE_ID.to_owned(),
                values: ids.clone(),
            });
            if only_errors {
                spec.filters.push(Expr::Binary {
                    left: Box::new(col_expr_q(&spans_raw, spans::STATUS)?),
                    op: BinaryOp::Eq,
                    right: Box::new(Expr::Literal(ScalarValue::Utf8(
                        SpanStatus::Error.as_str().to_owned(),
                    ))),
                    data_type: DataType::Bool,
                });
            }

            let out_schema = projected_view_schema(&spans_view, out_cols)?;

            let view_streams: Vec<Box<dyn PartitionStream>> = if !has_scores {
                if let Some(filter) = &compiled {
                    let rewritten = rewrite_view_expr_for_raw(
                        &filter.expr,
                        &spans_raw,
                        &[spans::TRACE_ID, spans::SPAN_ID, spans::PARENT_SPAN_ID],
                        false,
                    )?;
                    spec.filters.push(rewritten);
                }
                let proj_indices = out_cols
                    .iter()
                    .map(|name| {
                        spans_raw.index_of(name).ok_or_else(|| {
                            QueryError::internal(format!("spans schema missing column {name}"))
                        })
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                spec.projection = Some(proj_indices);
                let scan_schema = ctx_c.spans.projected_schema(&spec)?;
                let mut streams = ctx_c.spans.open_partitions(&spec, exec_ctx)?;
                if streams.is_empty() {
                    return Ok(Vec::new());
                }
                if strict {
                    streams = strict_exclusions_spans(
                        &ctx_c,
                        exec_ctx,
                        streams,
                        Arc::clone(&scan_schema),
                        Some(&ids),
                    )?;
                }
                streams
                    .into_iter()
                    .map(|s| {
                        let proj = project_view_columns(
                            &scan_schema,
                            Arc::clone(&out_schema),
                            out_cols,
                            false,
                            exec_ctx.clone(),
                        )?;
                        Ok(
                            Box::new(OperatorStream::new(s, vec![Box::new(proj)], exec_ctx))
                                as Box<dyn PartitionStream>,
                        )
                    })
                    .collect::<Result<Vec<_>, QueryError>>()?
            } else {
                let mut streams = ctx_c.spans.open_partitions(&spec, exec_ctx)?;
                if streams.is_empty() {
                    return Ok(Vec::new());
                }
                if strict {
                    streams = strict_exclusions_spans(
                        &ctx_c,
                        exec_ctx,
                        streams,
                        Arc::clone(&spans_raw),
                        Some(&ids),
                    )?;
                }
                let mut v_streams: Vec<Box<dyn PartitionStream>> = streams
                    .into_iter()
                    .map(|s| {
                        let proj = spans_view_projection(&spans_raw, exec_ctx.clone())?;
                        Ok(
                            Box::new(OperatorStream::new(s, vec![Box::new(proj)], exec_ctx))
                                as Box<dyn PartitionStream>,
                        )
                    })
                    .collect::<Result<Vec<_>, QueryError>>()?;
                if let Some(filter) = &compiled {
                    v_streams =
                        apply_compiled_filter(&ctx_c, exec_ctx, v_streams, &spans_view, filter)?;
                }
                v_streams
                    .into_iter()
                    .map(|s| {
                        let proj = project_view_columns(
                            &spans_view,
                            Arc::clone(&out_schema),
                            out_cols,
                            false,
                            exec_ctx.clone(),
                        )?;
                        Ok(
                            Box::new(OperatorStream::new(s, vec![Box::new(proj)], exec_ctx))
                                as Box<dyn PartitionStream>,
                        )
                    })
                    .collect::<Result<Vec<_>, QueryError>>()?
            };

            let chained = chain_streams(view_streams);
            let partition_by = vec![col_expr_q(&out_schema, spans::TRACE_ID)?];
            let order_by = vec![
                SortKey {
                    expr: col_expr_q(&out_schema, spans::START_TIME)?,
                    descending: false,
                    nulls_first: false,
                },
                SortKey {
                    expr: col_expr_q(&out_schema, spans::SPAN_ID)?,
                    descending: false,
                    nulls_first: false,
                },
            ];
            let win_spec = WindowSpec {
                partition_by: partition_by.clone(),
                order_by: order_by.clone(),
                frame: None,
                func: WindowFunc::RowNumber,
                output_name: SPAN_RANK.to_owned(),
            };
            let ranked = eval_window_group(
                chained,
                Arc::clone(&out_schema),
                partition_by,
                order_by,
                vec![win_spec],
                exec_ctx,
            )?;

            let mut win_fields = out_schema.fields().to_vec();
            win_fields.push(Field::new(SPAN_RANK, DataType::I64, false));
            let win_schema = Schema::try_new_arc(win_fields)?;
            let per_trace_i64 = i64::try_from(per_trace)
                .map_err(|_| QueryError::InvalidRequest("per_trace is out of range".to_owned()))?;
            let rank_pred = Expr::Binary {
                left: Box::new(col_expr_q(&win_schema, SPAN_RANK)?),
                op: BinaryOp::LtEq,
                right: Box::new(Expr::Literal(ScalarValue::I64(per_trace_i64))),
                data_type: DataType::Bool,
            };
            let proj_exprs: Vec<Expr> = out_cols
                .iter()
                .map(|name| col_expr_q(&win_schema, name))
                .collect::<Result<Vec<_>, _>>()?;

            let pool = Arc::clone(&exec_ctx.pool);
            let op_ctx = exec_ctx.clone();
            let sinks = exec_run_pipeline(
                exec_ctx,
                vec![ranked],
                move |_| {
                    let f = FilterOp::new(rank_pred.clone(), op_ctx.clone());
                    let p = ProjectOp::with_schema(
                        proj_exprs.clone(),
                        Arc::clone(&out_schema),
                        op_ctx.clone(),
                    )?;
                    Ok(vec![
                        Box::new(f) as Box<dyn PushOperator>,
                        Box::new(p) as Box<dyn PushOperator>,
                    ])
                },
                move |_| Ok(CollectSink::new(&pool)),
            )?;
            Ok(sinks
                .into_iter()
                .flat_map(CollectSink::into_batches)
                .collect())
        })
        .await?;

    Ok(batches)
}

/// Counts the rows of `traces` or `spans` matching `filter` in `window`, stopping at `cap + 1`.
pub async fn match_count(
    ctx: &QueryContext,
    request: MatchCountRequest<'_>,
) -> Result<usize, RunError> {
    let ctx = ctx.engine();
    let view = match request.view {
        ViewName::Traces => FilterView::Traces,
        ViewName::Spans => FilterView::Spans,
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
    let compiled = compile_optional_filter(view, request.filter)?;
    let ctx_c = ctx.clone();
    let window = request.window;
    let strict = request.strict_exclusions;
    let limit = request.cap.saturating_add(1);

    let (count, _) = ctx
        .run_with_ctx(request.timeout, move |exec_ctx| {
            let has_scores = compiled.as_ref().is_some_and(|f| !f.scores.is_empty());
            let references_environment = !strict
                && compiled
                    .as_ref()
                    .is_some_and(|filter| filter.references_environment);

            let streams: Vec<Box<dyn PartitionStream>> = match view {
                FilterView::Traces => {
                    let traces_raw = traces_raw_schema().map_err(run_err_to_query)?;
                    let scope = ListScope {
                        window: Some(window),
                        include_eval: false,
                        include_experiments: false,
                    };
                    let mut spec = scoped_traces_spec(&ctx_c, scope, references_environment)
                        .map_err(run_err_to_query)?;
                    if !has_scores {
                        if let Some(filter) = &compiled {
                            let rewritten = rewrite_view_expr_for_raw(
                                &filter.expr,
                                &traces_raw,
                                &[traces::TRACE_ID, traces::ROOT_SPAN_ID],
                                true,
                            )?;
                            spec.filters.push(rewritten);
                        }
                        populate_id_lookups(&mut spec);
                        let id_idx = traces_raw.index_of(traces::TRACE_ID).ok_or_else(|| {
                            QueryError::internal("traces schema missing trace_id")
                        })?;
                        spec.projection = Some(vec![id_idx]);
                        ctx_c.traces.open_partitions(&spec, exec_ctx)?
                    } else {
                        let traces_view = traces_view_schema().map_err(run_err_to_query)?;
                        let raw_streams = ctx_c.traces.open_partitions(&spec, exec_ctx)?;
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
                        match &compiled {
                            Some(filter) => apply_compiled_filter(
                                &ctx_c,
                                exec_ctx,
                                view_streams,
                                &traces_view,
                                filter,
                            )?,
                            None => view_streams,
                        }
                    }
                }
                FilterView::Spans => {
                    let spans_raw = spans_raw_schema().map_err(run_err_to_query)?;
                    let scope = ListScope {
                        window: Some(window),
                        include_eval: strict,
                        include_experiments: strict,
                    };
                    let mut spec = scoped_spans_spec(&ctx_c, scope, references_environment)
                        .map_err(run_err_to_query)?;
                    if !has_scores {
                        if let Some(filter) = &compiled {
                            let rewritten = rewrite_view_expr_for_raw(
                                &filter.expr,
                                &spans_raw,
                                &[spans::TRACE_ID, spans::SPAN_ID, spans::PARENT_SPAN_ID],
                                false,
                            )?;
                            spec.filters.push(rewritten);
                        }
                        populate_id_lookups(&mut spec);
                        let id_idx = spans_raw
                            .index_of(spans::TRACE_ID)
                            .ok_or_else(|| QueryError::internal("spans schema missing trace_id"))?;
                        spec.projection = Some(vec![id_idx]);
                        let scan_schema = ctx_c.spans.projected_schema(&spec)?;
                        let mut raw_streams = ctx_c.spans.open_partitions(&spec, exec_ctx)?;
                        if strict && !raw_streams.is_empty() {
                            raw_streams = strict_exclusions_spans(
                                &ctx_c,
                                exec_ctx,
                                raw_streams,
                                scan_schema,
                                None,
                            )?;
                        }
                        raw_streams
                    } else {
                        let spans_view = spans_view_schema().map_err(run_err_to_query)?;
                        let mut raw_streams = ctx_c.spans.open_partitions(&spec, exec_ctx)?;
                        if strict && !raw_streams.is_empty() {
                            raw_streams = strict_exclusions_spans(
                                &ctx_c,
                                exec_ctx,
                                raw_streams,
                                Arc::clone(&spans_raw),
                                None,
                            )?;
                        }
                        let view_streams: Vec<Box<dyn PartitionStream>> = raw_streams
                            .into_iter()
                            .map(|s| {
                                let proj = spans_view_projection(&spans_raw, exec_ctx.clone())?;
                                Ok(
                                    Box::new(OperatorStream::new(s, vec![Box::new(proj)], exec_ctx))
                                        as Box<dyn PartitionStream>,
                                )
                            })
                            .collect::<Result<Vec<_>, QueryError>>()?;
                        match &compiled {
                            Some(filter) => apply_compiled_filter(
                                &ctx_c,
                                exec_ctx,
                                view_streams,
                                &spans_view,
                                filter,
                            )?,
                            None => view_streams,
                        }
                    }
                }
                _ => {
                    return Err(QueryError::InvalidRequest(
                        "match counts are for traces and spans only".to_owned(),
                    ));
                }
            };

            if streams.is_empty() {
                return Ok(0usize);
            }

            let shared_rem = Arc::new(AtomicUsize::new(limit));
            let sinks = exec_run_pipeline(
                exec_ctx,
                streams,
                move |_| {
                    Ok(vec![
                        Box::new(LimitOp::new(0, None, Some(Arc::clone(&shared_rem))))
                            as Box<dyn PushOperator>,
                    ])
                },
                |_| Ok(CountSink { count: 0 }),
            )?;
            let total: usize = sinks.into_iter().map(|s| s.count).sum();
            Ok(total.min(limit))
        })
        .await?;

    Ok(count)
}

/// Totals of the rows [`trace_list`] would page through for the same filter, search and scope.
///
/// # Errors
///
/// [`RunError::Invalid`] for filter text; [`RunError::InvalidRequest`] for a search without a
/// window; [`RunError::Timeout`] past `timeout`; engine failures through `run_with_ctx`
/// (a non-finite or out-of-range cost fails the sum, as in the widget query).
pub async fn trace_totals(
    ctx: &QueryContext,
    request: TraceTotalsRequest<'_>,
) -> Result<TraceTotals, RunError> {
    let ctx = ctx.engine();
    let compiled = compile_optional_filter(FilterView::Traces, request.filter)?;
    let search = present(request.search).map(str::to_owned);
    if search.is_some() && request.scope.window.is_none() {
        return Err(RunError::InvalidRequest(
            "search needs a time range".to_owned(),
        ));
    }
    let references_environment = compiled
        .as_ref()
        .is_some_and(|filter| filter.references_environment);
    let scope = request.scope;
    let ctx_c = ctx.clone();

    let (batches, _) = ctx
        .run_with_ctx(request.timeout, move |exec_ctx| {
            let traces_raw = traces_raw_schema().map_err(run_err_to_query)?;
            let has_scores = compiled.as_ref().is_some_and(|f| !f.scores.is_empty());
            let mut spec = scoped_traces_spec(&ctx_c, scope, references_environment)
                .map_err(run_err_to_query)?;

            let (streams, agg_input_schema): (Vec<Box<dyn PartitionStream>>, Arc<Schema>) =
                if !has_scores {
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
                    let mut proj_set = BTreeSet::new();
                    if search.is_some()
                        && let Some(idx) = traces_raw.index_of(traces::TRACE_ID)
                    {
                        proj_set.insert(idx);
                    }
                    for name in [traces::STATUS, traces::COST_TOTAL] {
                        let idx = traces_raw.index_of(name).ok_or_else(|| {
                            QueryError::internal(format!("traces schema missing {name}"))
                        })?;
                        proj_set.insert(idx);
                    }
                    spec.projection = Some(proj_set.into_iter().collect());
                    let scan_schema = ctx_c.traces.projected_schema(&spec)?;
                    let mut raw_streams = ctx_c.traces.open_partitions(&spec, exec_ctx)?;
                    if raw_streams.is_empty() {
                        return Ok(Vec::new());
                    }
                    if let Some(text) = &search {
                        match search_predicate(
                            &ctx_c,
                            exec_ctx,
                            FilterView::Traces,
                            text,
                            scope.window,
                        )
                        .map_err(run_err_to_query)?
                        {
                            SearchPredicate::None => {}
                            SearchPredicate::Traces { trace_ids, schema } => {
                                if trace_ids.is_empty() {
                                    return Ok(Vec::new());
                                }
                                let left_key = col_expr_q(&scan_schema, traces::TRACE_ID)?;
                                let right_key = col_expr_q(&schema, traces::TRACE_ID)?;
                                let joined = execute_hash_join(
                                    raw_streams,
                                    Arc::clone(&scan_schema),
                                    trace_ids,
                                    schema,
                                    HashJoinSpec {
                                        join_type: JoinType::LeftSemi,
                                        left_keys: vec![left_key],
                                        right_keys: vec![right_key],
                                        null_equals_null: false,
                                        residual_filter: None,
                                    },
                                    exec_ctx,
                                )?;
                                raw_streams = vec![joined];
                            }
                            SearchPredicate::Spans { .. } => {
                                return Err(QueryError::internal(
                                    "unexpected Spans search predicate for Traces",
                                ));
                            }
                        }
                    }
                    (raw_streams, scan_schema)
                } else {
                    let traces_view = traces_view_schema().map_err(run_err_to_query)?;
                    let mut raw_streams = ctx_c.traces.open_partitions(&spec, exec_ctx)?;
                    if raw_streams.is_empty() {
                        return Ok(Vec::new());
                    }
                    if let Some(text) = &search {
                        match search_predicate(
                            &ctx_c,
                            exec_ctx,
                            FilterView::Traces,
                            text,
                            scope.window,
                        )
                        .map_err(run_err_to_query)?
                        {
                            SearchPredicate::None => {}
                            SearchPredicate::Traces { trace_ids, schema } => {
                                if trace_ids.is_empty() {
                                    return Ok(Vec::new());
                                }
                                let left_key = col_expr_q(&traces_raw, traces::TRACE_ID)?;
                                let right_key = col_expr_q(&schema, traces::TRACE_ID)?;
                                let joined = execute_hash_join(
                                    raw_streams,
                                    Arc::clone(&traces_raw),
                                    trace_ids,
                                    schema,
                                    HashJoinSpec {
                                        join_type: JoinType::LeftSemi,
                                        left_keys: vec![left_key],
                                        right_keys: vec![right_key],
                                        null_equals_null: false,
                                        residual_filter: None,
                                    },
                                    exec_ctx,
                                )?;
                                raw_streams = vec![joined];
                            }
                            SearchPredicate::Spans { .. } => {
                                return Err(QueryError::internal(
                                    "unexpected Spans search predicate for Traces",
                                ));
                            }
                        }
                    }
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
                    let filtered = match &compiled {
                        Some(filter) => apply_compiled_filter(
                            &ctx_c,
                            exec_ctx,
                            view_streams,
                            &traces_view,
                            filter,
                        )?,
                        None => view_streams,
                    };
                    (filtered, traces_view)
                };

            let status_col = col_expr_q(&agg_input_schema, traces::STATUS)?;
            let cost_col = col_expr_q(&agg_input_schema, traces::COST_TOTAL)?;
            let is_error_one = Expr::Case {
                operand: None,
                when_then: vec![(
                    Expr::Binary {
                        left: Box::new(status_col),
                        op: BinaryOp::Eq,
                        right: Box::new(Expr::Literal(ScalarValue::Utf8(
                            SpanStatus::Error.as_str().to_owned(),
                        ))),
                        data_type: DataType::Bool,
                    },
                    Expr::Literal(ScalarValue::I64(1)),
                )],
                else_expr: None,
                data_type: DataType::I64,
            };

            let agg_spec = HashAggSpec {
                group_by: Vec::new(),
                aggs: vec![
                    AggExpr {
                        func: AggFunc::CountStar,
                        filter: None,
                        output_name: TOTAL_TRACES.to_owned(),
                    },
                    AggExpr {
                        func: AggFunc::Count(is_error_one),
                        filter: None,
                        output_name: TOTAL_ERRORS.to_owned(),
                    },
                    AggExpr {
                        func: AggFunc::SumCostDecimal(cost_col),
                        filter: None,
                        output_name: TOTAL_COST.to_owned(),
                    },
                ],
            };

            let spec_c = agg_spec.clone();
            let schema_c = Arc::clone(&agg_input_schema);
            let ctx_agg = exec_ctx.clone();
            let sinks = exec_run_pipeline(
                exec_ctx,
                streams,
                |_| Ok(Vec::new()),
                move |_| HashAgg::new(spec_c.clone(), Arc::clone(&schema_c), ctx_agg.clone()),
            )?;
            HashAgg::merge_sinks(sinks, &agg_spec, &agg_input_schema, exec_ctx)
        })
        .await?;

    let Some(batch) = batches.iter().find(|b| b.num_rows() > 0) else {
        return Ok(TraceTotals {
            trace_count: 0,
            error_trace_count: 0,
            cost_total: 0.0,
        });
    };
    Ok(TraceTotals {
        trace_count: i64_at(batch, require_col(batch, TOTAL_TRACES)?, 0)?,
        error_trace_count: i64_at(batch, require_col(batch, TOTAL_ERRORS)?, 0)?,
        cost_total: decimal_to_f64_at(batch, require_col(batch, TOTAL_COST)?, 0)?.unwrap_or(0.0),
    })
}

struct CountSink {
    count: usize,
}

impl PushSink for CountSink {
    fn consume(
        &mut self,
        batch: Batch,
        _batch_res: &mut Reservation,
    ) -> Result<ControlFlow, QueryError> {
        self.count = self.count.saturating_add(batch.num_rows());
        Ok(ControlFlow::Continue)
    }
}

fn traces_raw_sort_keys(
    raw_schema: &Schema,
    sort: TraceSort,
    order: SortOrder,
) -> Result<Vec<SortKey>, RunError> {
    let descending = order == SortOrder::Desc;
    let primary_expr = match sort {
        TraceSort::StartTime => col_expr_q(raw_schema, traces::START_TIME),
        TraceSort::Duration => duration_ns_from_bounds_expr(raw_schema),
        TraceSort::CostTotal => col_expr_q(raw_schema, traces::COST_TOTAL),
        TraceSort::TotalTokens => col_expr_q(raw_schema, traces::TOTAL_TOKENS),
    }
    .map_err(query_err_to_run)?;
    let tie_expr = col_expr_q(raw_schema, traces::TRACE_ID).map_err(query_err_to_run)?;
    Ok(vec![
        SortKey {
            expr: primary_expr,
            descending,
            nulls_first: false,
        },
        SortKey {
            expr: tie_expr,
            descending,
            nulls_first: false,
        },
    ])
}

fn traces_raw_after_expr(cursor: &Cursor, raw_schema: &Schema) -> Result<Expr, RunError> {
    if cursor.sort != DURATION_NS {
        return cursor::after_expr(cursor, raw_schema, &cursor.sort, TRACE_TIES);
    }
    if cursor.tie.len() != 1 {
        return Err(RunError::InvalidRequest("invalid cursor".to_owned()));
    }
    let trace_col = col_expr_q(raw_schema, traces::TRACE_ID).map_err(query_err_to_run)?;
    let trace_bytes = decode_hex_16(&cursor.tie[0], 32, "trace_id")
        .map_err(|_| RunError::InvalidRequest("invalid cursor".to_owned()))?;
    let cmp_op = match cursor.order {
        SortOrder::Asc => BinaryOp::Gt,
        SortOrder::Desc => BinaryOp::Lt,
    };
    let tie_pred = Expr::Binary {
        left: Box::new(trace_col),
        op: cmp_op,
        right: Box::new(Expr::Literal(ScalarValue::FixedBytes16(trace_bytes))),
        data_type: DataType::Bool,
    };
    let dur_col = duration_ns_from_bounds_expr(raw_schema).map_err(query_err_to_run)?;
    match cursor.value {
        CursorValue::Null => Ok(Expr::Binary {
            left: Box::new(Expr::Unary {
                op: cetra_query::UnaryOp::IsNull,
                expr: Box::new(dur_col),
                data_type: DataType::Bool,
            }),
            op: BinaryOp::And,
            right: Box::new(tie_pred),
            data_type: DataType::Bool,
        }),
        CursorValue::Int(v) => {
            let lit = Expr::Literal(ScalarValue::I64(v));
            let cmp = Expr::Binary {
                left: Box::new(dur_col.clone()),
                op: cmp_op,
                right: Box::new(lit.clone()),
                data_type: DataType::Bool,
            };
            let eq_and_tie = Expr::Binary {
                left: Box::new(Expr::Binary {
                    left: Box::new(dur_col.clone()),
                    op: BinaryOp::Eq,
                    right: Box::new(lit),
                    data_type: DataType::Bool,
                }),
                op: BinaryOp::And,
                right: Box::new(tie_pred),
                data_type: DataType::Bool,
            };
            let is_null = Expr::Unary {
                op: cetra_query::UnaryOp::IsNull,
                expr: Box::new(dur_col),
                data_type: DataType::Bool,
            };
            Ok(Expr::Binary {
                left: Box::new(Expr::Binary {
                    left: Box::new(cmp),
                    op: BinaryOp::Or,
                    right: Box::new(eq_and_tie),
                    data_type: DataType::Bool,
                }),
                op: BinaryOp::Or,
                right: Box::new(is_null),
                data_type: DataType::Bool,
            })
        }
        CursorValue::Float(_) => Err(internal("duration_ns cursor cannot hold a float")),
    }
}

fn span_list_raw_indices(
    raw_schema: &Schema,
    include_ingest: bool,
) -> Result<Vec<usize>, RunError> {
    let mut set = BTreeSet::new();
    for &name in SPAN_LIST_COLUMNS {
        if name == DURATION_MS {
            let idx = raw_schema
                .index_of(spans::DURATION_NS)
                .ok_or_else(|| internal("spans schema missing duration_ns"))?;
            set.insert(idx);
        } else {
            let idx = raw_schema
                .index_of(name)
                .ok_or_else(|| internal(format!("spans schema missing column {name}")))?;
            set.insert(idx);
        }
    }
    if include_ingest {
        let idx = raw_schema
            .index_of(spans::INGEST_TIME)
            .ok_or_else(|| internal("spans schema missing ingest_time"))?;
        set.insert(idx);
    }
    Ok(set.into_iter().collect())
}

fn projected_view_schema(view_schema: &Schema, cols: &[&str]) -> Result<Arc<Schema>, QueryError> {
    let indices: Vec<usize> = cols
        .iter()
        .map(|name| {
            view_schema
                .index_of(name)
                .ok_or_else(|| QueryError::internal(format!("view schema missing column {name}")))
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Arc::new(view_schema.project(&indices)?))
}

fn single_row(batches: &[Batch], exec_ctx: &ExecContext) -> Result<Option<Batch>, QueryError> {
    let non_empty: Vec<&Batch> = batches.iter().filter(|b| b.num_rows() > 0).collect();
    let Some(first) = non_empty.first() else {
        return Ok(None);
    };
    let schema = Arc::clone(first.schema());
    let mut res = exec_ctx.pool.register("single_row", false);
    let combined = Batch::concat(schema, &non_empty, Some(&mut res))?;
    res.free();
    if combined.num_rows() == 0 {
        return Ok(None);
    }
    if combined.num_rows() == 1 {
        return Ok(Some(combined));
    }
    Ok(Some(combined.slice(0, 1, None)?))
}

fn cap_rows(batches: Vec<Batch>, limit: usize) -> Result<(Vec<Batch>, bool), RunError> {
    let total: usize = batches.iter().map(Batch::num_rows).sum();
    if total <= limit {
        return Ok((batches, false));
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
            let sliced = match batch.selection() {
                Selection::All => batch.slice(0, remaining, None),
                Selection::Mask(bm) => {
                    let indices: Vec<u32> =
                        bm.iter_ones().take(remaining).map(|i| i as u32).collect();
                    batch.with_selection(Selection::Indices(indices))
                }
                Selection::Indices(idx) => {
                    batch.with_selection(Selection::Indices(idx[..remaining].to_vec()))
                }
            }
            .map_err(|e| internal(format!("capping rows failed: {e}")))?;
            kept.push(sliced);
            remaining = 0;
        }
    }
    Ok((kept, true))
}

fn decode_hex_16(text: &str, width: usize, name: &str) -> Result<[u8; 16], RunError> {
    if text.len() == width && text.bytes().all(|b| b.is_ascii_hexdigit()) {
        let bytes = hex::decode(text)
            .map_err(|_| RunError::InvalidRequest(format!("{name} must be {width} hex digits")))?;
        return bytes
            .try_into()
            .map_err(|_| RunError::InvalidRequest(format!("{name} must be {width} hex digits")));
    }
    Err(RunError::InvalidRequest(format!(
        "{name} must be {width} hex digits"
    )))
}

fn decode_hex_16_q(text: &str) -> Result<[u8; 16], QueryError> {
    if text.len() == 32 && text.bytes().all(|b| b.is_ascii_hexdigit()) {
        let bytes =
            hex::decode(text).map_err(|_| QueryError::internal("invalid hex trace_id in batch"))?;
        return bytes
            .try_into()
            .map_err(|_| QueryError::internal("invalid trace_id length in batch"));
    }
    Err(QueryError::internal("invalid hex trace_id in batch"))
}

fn decode_hex_8(text: &str, width: usize, name: &str) -> Result<[u8; 8], RunError> {
    if text.len() == width && text.bytes().all(|b| b.is_ascii_hexdigit()) {
        let bytes = hex::decode(text)
            .map_err(|_| RunError::InvalidRequest(format!("{name} must be {width} hex digits")))?;
        return bytes
            .try_into()
            .map_err(|_| RunError::InvalidRequest(format!("{name} must be {width} hex digits")));
    }
    Err(RunError::InvalidRequest(format!(
        "{name} must be {width} hex digits"
    )))
}

fn col_expr_q(schema: &Schema, name: &str) -> Result<Expr, QueryError> {
    let idx = schema
        .index_of(name)
        .ok_or_else(|| QueryError::internal(format!("missing column {name} in schema")))?;
    Ok(Expr::Column {
        index: idx,
        name: name.to_owned(),
        data_type: schema.fields()[idx].data_type.clone(),
    })
}

fn eq_fixed16(schema: &Schema, col_name: &str, bytes: [u8; 16]) -> Result<Expr, QueryError> {
    Ok(Expr::Binary {
        left: Box::new(col_expr_q(schema, col_name)?),
        op: BinaryOp::Eq,
        right: Box::new(Expr::Literal(ScalarValue::FixedBytes16(bytes))),
        data_type: DataType::Bool,
    })
}

fn eq_fixed8(schema: &Schema, col_name: &str, bytes: [u8; 8]) -> Result<Expr, QueryError> {
    Ok(Expr::Binary {
        left: Box::new(col_expr_q(schema, col_name)?),
        op: BinaryOp::Eq,
        right: Box::new(Expr::Literal(ScalarValue::FixedBytes8(bytes))),
        data_type: DataType::Bool,
    })
}

fn in_list_fixed16(schema: &Schema, col_name: &str, ids: &[[u8; 16]]) -> Result<Expr, QueryError> {
    let list = ids
        .iter()
        .map(|b| Expr::Literal(ScalarValue::FixedBytes16(*b)))
        .collect();
    Ok(Expr::InList {
        expr: Box::new(col_expr_q(schema, col_name)?),
        list,
        negated: false,
        redacted: false,
    })
}

struct ChainedStream {
    streams: VecDeque<Box<dyn PartitionStream>>,
}

impl PartitionStream for ChainedStream {
    fn next_batch(
        &mut self,
        cancel: &CancelToken,
        mut res: Option<&mut Reservation>,
    ) -> Result<Option<Batch>, QueryError> {
        while let Some(front) = self.streams.front_mut() {
            if cancel.is_cancelled() {
                return Err(QueryError::Cancelled);
            }
            match front.next_batch(cancel, res.as_deref_mut())? {
                Some(batch) => return Ok(Some(batch)),
                None => {
                    self.streams.pop_front();
                }
            }
        }
        Ok(None)
    }
}

fn chain_streams(streams: Vec<Box<dyn PartitionStream>>) -> Box<dyn PartitionStream> {
    Box::new(ChainedStream {
        streams: streams.into(),
    })
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;
    use std::time::Duration;

    use bytesize::ByteSize;
    use cetra_core::enums::{ScoreSource, SpanKind, SpanStatus};
    use cetra_core::ids::{ProjectId, TraceId};
    use cetra_core::names::metadata_key;
    use cetra_core::score::{ScoreTarget, ScoreValue};
    use cetra_core::span::SpanEvent;
    use cetra_core::time_serde::parse_rfc3339;
    use cetra_query::encode::encode_json_result;
    use serde_json::Value;

    use super::*;
    use crate::config::{
        FlushConfig, MemtableConfig, QueryConfig, S3Config, StorageConfig, WalConfig,
    };
    use crate::sql::{SQL_MAX_RESULT_BYTES, SQL_MAX_ROWS};
    use crate::test_support::{
        dataset_scores, dataset_spans, score_row, score_rows, span_row, span_rows,
    };
    use crate::views::{QueryContext, TimeWindow, ViewName};
    use crate::{QueryOptions, Storage};

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

    fn qe_rows(batches: &[Batch]) -> Vec<Value> {
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

    async fn build_context(
        storage: &Storage,
        project: ProjectId,
        options: QueryOptions,
    ) -> QueryContext {
        let pq = storage.query(project, options).await.expect("query");
        QueryContext::new(pq, None).expect("ctx")
    }

    fn trace_req<'a>(
        scope: ListScope,
        sort: TraceSort,
        order: SortOrder,
        limit: usize,
    ) -> TraceListRequest<'a> {
        TraceListRequest {
            filter: None,
            search: None,
            scope,
            strict_exclusions: false,
            sort,
            order,
            limit,
            cursor: None,
            timeout: TIMEOUT,
        }
    }

    fn span_req<'a>(
        scope: ListScope,
        sort: SpanSort,
        order: SortOrder,
        limit: usize,
    ) -> SpanListRequest<'a> {
        SpanListRequest {
            filter: None,
            search: None,
            scope,
            strict_exclusions: false,
            sort,
            order,
            limit,
            cursor: None,
            timeout: TIMEOUT,
        }
    }

    #[tokio::test]
    async fn trace_and_span_reads_cover_list_detail_refresh_and_counts() {
        let dir = tempfile::tempdir().expect("tempdir");
        let storage = Storage::open(test_config(dir.path())).await.expect("open");
        let project: ProjectId = "018f6b4e-7c1a-7c1a-8c1a-000000000001".parse().expect("pid");

        // 1. Flush dataset spans + dataset scores to Parquet.
        let mut flushed_spans = dataset_spans();

        // Multi-day trace: root on 2026-01-01, child on 2026-01-02 (crosses UTC midnight within lookback).
        let multiday_tid = TraceId::from_str("c1000000000000000000000000000001").expect("tid");
        let mut md_root = span_row(
            50,
            0,
            parse_rfc3339("2026-01-01T23:50:00Z").expect("ts"),
            100,
        );
        md_root.trace_id = multiday_tid;
        md_root.name = "multiday-root".to_owned();
        md_root.input = Some("order confirmation".to_owned());
        let mut md_child = span_row(
            50,
            1,
            parse_rfc3339("2026-01-02T00:10:00Z").expect("ts"),
            200,
        );
        md_child.trace_id = multiday_tid;
        md_child.parent_span_id = Some(md_root.span_id);
        md_child.name = "multiday-child".to_owned();
        flushed_spans.extend([md_root.clone(), md_child.clone()]);

        // Root-only tagged experiment trace (tests strict_exclusions anti-join on Spans).
        let exp_tid = TraceId::from_str("c2000000000000000000000000000002").expect("tid");
        let mut exp_root = span_row(
            51,
            0,
            parse_rfc3339("2026-01-01T13:00:00Z").expect("ts"),
            100,
        );
        exp_root.trace_id = exp_tid;
        exp_root
            .metadata
            .insert(metadata_key::EXPERIMENT_ID.to_owned(), "exp-42".to_owned());
        let mut exp_child = span_row(
            51,
            1,
            parse_rfc3339("2026-01-01T13:00:01Z").expect("ts"),
            150,
        );
        exp_child.trace_id = exp_tid;
        exp_child.parent_span_id = Some(exp_root.span_id);
        flushed_spans.extend([exp_root, exp_child]);
        for s in &mut flushed_spans {
            s.project_id = project;
        }

        storage
            .append_spans(project, span_rows(&flushed_spans))
            .await
            .expect("append spans 1");

        let mut scores = dataset_scores();
        // Add an issue score on multiday trace to verify filter score() includes issue source.
        let mut issue_s = score_row(
            200,
            50,
            parse_rfc3339("2026-01-01T12:05:00Z").expect("ts"),
            100,
            100,
            false,
            "helpfulness",
        );
        issue_s.target = ScoreTarget::Trace {
            trace_id: multiday_tid,
        };
        issue_s.source = ScoreSource::Issue;
        issue_s.value = ScoreValue::Numeric(0.95);
        scores.push(issue_s);
        for s in &mut scores {
            s.project_id = project;
        }

        storage
            .append_scores(project, score_rows(&scores))
            .await
            .expect("append scores");
        storage
            .flush_now(Duration::from_secs(10))
            .await
            .expect("flush");

        // 2. Append memtable spans: duplicate span copies + >50 error spans + extra trace.
        let mut mem_spans = Vec::new();
        let dup_base = parse_rfc3339("2026-01-01T14:00:00Z").expect("ts");
        for copy in 1..=5i64 {
            let mut s = span_row(52, 0, dup_base, copy * 100);
            s.name = format!("dup-copy-{copy}");
            s.span_kind = if copy == 5 {
                SpanKind::Tool
            } else {
                SpanKind::Llm
            };
            mem_spans.push(s);
        }

        let err_tid = TraceId::from_str("c3000000000000000000000000000003").expect("tid");
        let err_base = parse_rfc3339("2026-01-01T15:00:00Z").expect("ts");
        for i in 0..55u8 {
            let mut s = span_row(53, i, err_base + i64::from(i), 100);
            s.trace_id = err_tid;
            s.status = SpanStatus::Error;
            s.status_message = Some(format!("err-{i}"));
            s.service_name = Some("svc-a".to_owned());
            s.events = vec![SpanEvent {
                time: err_base + i64::from(i),
                name: "exception".to_owned(),
                attributes: None,
            }];
            mem_spans.push(s);
        }
        for s in &mut mem_spans {
            s.project_id = project;
        }

        storage
            .append_spans(project, span_rows(&mem_spans))
            .await
            .expect("append spans 2");

        let mut ctx = build_context(&storage, project, QueryOptions::new(None, None)).await;

        // --- A. trace_list_plan ---
        let default_req = trace_req(
            ListScope {
                window: None,
                include_eval: false,
                include_experiments: false,
            },
            TraceSort::StartTime,
            SortOrder::Desc,
            50,
        );
        let plan_list = trace_list_plan(&ctx, default_req.clone())
            .await
            .expect("plan list");
        assert!(plan_list.contains("mode=list"), "{plan_list}");

        let cost_req = trace_req(default_req.scope, TraceSort::CostTotal, SortOrder::Desc, 50);
        let plan_cost = trace_list_plan(&ctx, cost_req).await.expect("plan cost");
        assert!(plan_cost.contains("mode=list"), "{plan_cost}");

        ctx.set_force_range_mode(true);
        let plan_range = trace_list_plan(&ctx, default_req.clone())
            .await
            .expect("plan range");
        assert!(plan_range.contains("mode=range"), "{plan_range}");
        ctx.set_force_range_mode(false);

        // --- B. trace_list (all 4 sorts x both directions + cursor walk + score + search + strict + force_range_mode) ---
        let day_win = TimeWindow::new(
            parse_rfc3339("2026-01-01T00:00:00Z").expect("from"),
            parse_rfc3339("2026-01-04T00:00:00Z").expect("to"),
        )
        .expect("window");

        for include_eval in [false, true] {
            let scope = ListScope {
                window: None,
                include_eval,
                include_experiments: false,
            };
            for sort in [
                TraceSort::StartTime,
                TraceSort::Duration,
                TraceSort::CostTotal,
                TraceSort::TotalTokens,
            ] {
                for order in [SortOrder::Asc, SortOrder::Desc] {
                    ctx.set_force_range_mode(false);
                    let mut list_cur: Option<String> = None;
                    let mut list_rows = Vec::new();
                    loop {
                        let mut r = trace_req(scope, sort, order, 2);
                        r.cursor = list_cur.as_deref();
                        let p = trace_list(&ctx, r).await.expect("list");
                        let rows = qe_rows(&p.batches);
                        assert!(rows.len() <= 2);
                        list_rows.extend(rows);
                        match p.next_cursor {
                            Some(c) => list_cur = Some(c),
                            None => break,
                        }
                    }

                    ctx.set_force_range_mode(true);
                    let mut range_cur: Option<String> = None;
                    let mut range_rows = Vec::new();
                    loop {
                        let mut r = trace_req(scope, sort, order, 2);
                        r.cursor = range_cur.as_deref();
                        let p = trace_list(&ctx, r).await.expect("range");
                        range_rows.extend(qe_rows(&p.batches));
                        match p.next_cursor {
                            Some(c) => range_cur = Some(c),
                            None => break,
                        }
                    }
                    assert_eq!(list_rows, range_rows);
                    assert!(!list_rows.is_empty());
                }
            }
        }
        ctx.set_force_range_mode(false);

        // Two-phase trace_list with score() and search across CostTotal, Duration, TotalTokens, StartTime
        for sort in [
            TraceSort::StartTime,
            TraceSort::CostTotal,
            TraceSort::Duration,
            TraceSort::TotalTokens,
        ] {
            for (filt, srch) in [
                (Some("score('helpfulness') >= 0.5"), None),
                (None, Some("order")),
                (Some("score('helpfulness') >= 0.5"), Some("order")),
            ] {
                let req = TraceListRequest {
                    filter: filt,
                    search: srch,
                    scope: ListScope {
                        window: Some(day_win),
                        include_eval: false,
                        include_experiments: false,
                    },
                    strict_exclusions: false,
                    sort,
                    order: SortOrder::Desc,
                    limit: 50,
                    cursor: None,
                    timeout: TIMEOUT,
                };
                let qp = trace_list(&ctx, req).await.expect("2p join");
                let rows = qe_rows(&qp.batches);
                assert!(
                    rows.iter().any(|r| r["trace_id"] == multiday_tid.to_hex()),
                    "expected multiday trace for {sort:?} f={filt:?} s={srch:?}"
                );
            }
        }

        // Strict trace_list ignoring include flags & environment filter
        let strict_t_req = TraceListRequest {
            filter: Some("environment = 'cetra-eval'"),
            search: None,
            scope: ListScope {
                window: None,
                include_eval: true,
                include_experiments: true,
            },
            strict_exclusions: true,
            sort: TraceSort::StartTime,
            order: SortOrder::Desc,
            limit: 50,
            cursor: None,
            timeout: TIMEOUT,
        };
        let strict_rows = qe_rows(
            &trace_list(&ctx, strict_t_req)
                .await
                .expect("strict")
                .batches,
        );
        assert!(strict_rows.is_empty());

        // --- C. span_list + doubling refetch + cap ---
        for sort in [
            SpanSort::StartTime,
            SpanSort::Duration,
            SpanSort::CostTotal,
            SpanSort::TotalTokens,
        ] {
            for order in [SortOrder::Asc, SortOrder::Desc] {
                let mut cur: Option<String> = None;
                let mut collected = Vec::new();
                loop {
                    let mut req = span_req(
                        ListScope {
                            window: None,
                            include_eval: true,
                            include_experiments: false,
                        },
                        sort,
                        order,
                        3,
                    );
                    req.cursor = cur.as_deref();
                    let qp = span_list(&ctx, req).await.expect("span_list");
                    let rows = qe_rows(&qp.batches);
                    assert!(rows.len() <= 3);
                    collected.extend(rows);
                    match qp.next_cursor {
                        Some(c) => cur = Some(c),
                        None => break,
                    }
                }
                assert!(!collected.is_empty());
            }
        }

        // Filter-before-dedup + strict_exclusions on span_list
        let filter_dedup_req = SpanListRequest {
            filter: Some("span_kind = 'llm'"),
            search: None,
            scope: ListScope {
                window: Some(day_win),
                include_eval: true,
                include_experiments: false,
            },
            strict_exclusions: true,
            sort: SpanSort::StartTime,
            order: SortOrder::Desc,
            limit: 50,
            cursor: None,
            timeout: TIMEOUT,
        };
        let dedup_rows = qe_rows(
            &span_list(&ctx, filter_dedup_req)
                .await
                .expect("span dedup")
                .batches,
        );
        assert!(dedup_rows.iter().all(|r| r["trace_id"] != exp_tid.to_hex()));

        // Doubling refetch + cap = 4 test (5 duplicate copies of span 52)
        let cap_req = SpanListRequest {
            filter: Some(&format!(
                "trace_id = '{}'",
                span_row(52, 0, 0, 0).trace_id.to_hex()
            )),
            search: None,
            scope: ListScope {
                window: None,
                include_eval: true,
                include_experiments: true,
            },
            strict_exclusions: false,
            sort: SpanSort::StartTime,
            order: SortOrder::Desc,
            limit: 3,
            cursor: None,
            timeout: TIMEOUT,
        };
        let capped = span_list_capped(&ctx, cap_req, 4).await.expect("capped");
        assert_eq!(qe_rows(&capped.batches).len(), 1);
        assert!(capped.next_cursor.is_some());

        // --- D. trace_detail (multi-day trace with around = None) & trace_detail_limited ---
        let md_hex = multiday_tid.to_hex().to_ascii_uppercase();
        let q_det = trace_detail(&ctx, &md_hex, TIMEOUT)
            .await
            .expect("detail")
            .expect("some");
        assert!(!q_det.truncated);
        assert_eq!(
            qe_rows(&[q_det.trace])[0]["trace_id"],
            multiday_tid.to_hex()
        );
        assert_eq!(qe_rows(&q_det.spans).len(), 2);

        let q_lim = trace_detail_limited(&ctx, &err_tid.to_hex(), TIMEOUT, 5)
            .await
            .expect("lim")
            .expect("some");
        assert!(q_lim.truncated);
        assert_eq!(qe_rows(&q_lim.spans).len(), 5);

        // --- E. span_detail (56 cols, duplicate keeps greatest ingest_time) ---
        let dup_tid = span_row(52, 0, 0, 0).trace_id.to_hex();
        let dup_sid = span_row(52, 0, 0, 0).span_id.to_hex();
        let q_sd = span_detail(&ctx, &dup_tid, &dup_sid, TIMEOUT)
            .await
            .expect("span_detail")
            .expect("some");
        assert_eq!(q_sd.schema().len(), 56);
        assert_eq!(qe_rows(&[q_sd])[0]["name"], "dup-copy-5");

        // --- F. trace_spans ---
        let ts_req = TraceSpansRequest {
            trace_id: &multiday_tid.to_hex(),
            target_span_id: Some(&md_child.span_id.to_hex()),
            max_spans: 1,
            max_required: 5,
            timeout: TIMEOUT,
        };
        let q_ts = trace_spans(&ctx, ts_req).await.expect("trace_spans");
        assert!(q_ts.truncated);
        assert_eq!(qe_rows(&q_ts.earliest).len(), 1);
        assert_eq!(qe_rows(&q_ts.required).len(), 2);

        // --- G. trace_refresh ---
        let ref_ids = [
            multiday_tid.to_hex(),
            exp_tid.to_hex(),
            "a1000000000000000000000000000001".to_owned(),
        ];
        let ref_slices: Vec<&str> = ref_ids.iter().map(String::as_str).collect();
        let ref_non_strict = qe_rows(
            &trace_refresh(
                &ctx,
                TraceRefreshRequest {
                    filter: Some("score('helpfulness') >= 0.5"),
                    search: Some(("order", day_win)),
                    trace_ids: &ref_slices,
                    include_eval: true,
                    include_experiments: true,
                    strict_exclusions: false,
                    timeout: TIMEOUT,
                },
            )
            .await
            .expect("trace_refresh"),
        );
        assert_eq!(ref_non_strict.len(), 2);
        let non_strict_ids: BTreeSet<&str> = ref_non_strict
            .iter()
            .filter_map(|r| r["trace_id"].as_str())
            .collect();
        assert!(non_strict_ids.contains(multiday_tid.to_hex().as_str()));
        assert!(non_strict_ids.contains("a1000000000000000000000000000001"));

        let ref_strict = qe_rows(
            &trace_refresh(
                &ctx,
                TraceRefreshRequest {
                    filter: Some("score('helpfulness') >= 0.5"),
                    search: Some(("order", day_win)),
                    trace_ids: &ref_slices,
                    include_eval: true,
                    include_experiments: true,
                    strict_exclusions: true,
                    timeout: TIMEOUT,
                },
            )
            .await
            .expect("trace_refresh strict"),
        );
        assert_eq!(ref_strict.len(), 2);
        let strict_ids: BTreeSet<&str> = ref_strict
            .iter()
            .filter_map(|r| r["trace_id"].as_str())
            .collect();
        assert!(strict_ids.contains(multiday_tid.to_hex().as_str()));
        assert!(strict_ids.contains("a1000000000000000000000000000001"));

        // Empty trace_ids parses filter without running query
        assert!(matches!(
            trace_refresh(
                &ctx,
                TraceRefreshRequest {
                    filter: Some("bad ="),
                    search: None,
                    trace_ids: &[],
                    include_eval: false,
                    include_experiments: false,
                    strict_exclusions: false,
                    timeout: TIMEOUT,
                }
            )
            .await,
            Err(RunError::Invalid(_))
        ));

        // --- H. span_refresh ---
        let sr_ids = [exp_tid.to_hex(), err_tid.to_hex(), multiday_tid.to_hex()];
        let sr_slices: Vec<&str> = sr_ids.iter().map(String::as_str).collect();
        let non_strict_sr = qe_rows(
            &span_refresh(
                &ctx,
                SpanRefreshRequest {
                    filter: None,
                    trace_ids: &sr_slices,
                    per_trace: 3,
                    strict_exclusions: false,
                    timeout: TIMEOUT,
                },
            )
            .await
            .expect("span_refresh"),
        );
        let strict_sr = qe_rows(
            &span_refresh(
                &ctx,
                SpanRefreshRequest {
                    filter: None,
                    trace_ids: &sr_slices,
                    per_trace: 3,
                    strict_exclusions: true,
                    timeout: TIMEOUT,
                },
            )
            .await
            .expect("span_refresh strict"),
        );
        assert!(
            non_strict_sr
                .iter()
                .any(|r| r["trace_id"] == exp_tid.to_hex())
        );
        assert!(!strict_sr.iter().any(|r| r["trace_id"] == exp_tid.to_hex()));

        // --- I. error_spans ---
        let es_req = ErrorSpansRequest {
            filter: Some("service_name = 'svc-a'"),
            trace_ids: &sr_slices,
            timeout: TIMEOUT,
        };
        let q_es = error_spans(&ctx, es_req).await.expect("error_spans");
        assert_eq!(qe_rows(&q_es).len(), MAX_ERROR_SPANS_PER_TRACE);

        // --- J. match_count ---
        let trace_cnt = match_count(
            &ctx,
            MatchCountRequest {
                view: ViewName::Traces,
                filter: Some("status = 'error'"),
                window: day_win,
                cap: 2,
                strict_exclusions: false,
                timeout: TIMEOUT,
            },
        )
        .await
        .expect("match_count");
        assert!(trace_cnt >= 1);
        let span_cnt = match_count(
            &ctx,
            MatchCountRequest {
                view: ViewName::Spans,
                filter: Some("status = 'error'"),
                window: day_win,
                cap: 2,
                strict_exclusions: true,
                timeout: TIMEOUT,
            },
        )
        .await
        .expect("match_count spans");
        assert_eq!(span_cnt, 3);

        // --- K. Error handling ---
        assert!(matches!(
            trace_list(
                &ctx,
                TraceListRequest {
                    search: Some("order"),
                    ..default_req.clone()
                }
            )
            .await,
            Err(RunError::InvalidRequest(m)) if m == "search needs a time range"
        ));
        assert!(matches!(
            trace_list(
                &ctx,
                TraceListRequest {
                    filter: Some("nme = 'a'"),
                    ..default_req.clone()
                }
            )
            .await,
            Err(RunError::Invalid(_))
        ));
        assert!(matches!(
            trace_detail(&ctx, "bad", TIMEOUT).await,
            Err(RunError::InvalidRequest(_))
        ));
        assert!(matches!(
            span_detail(&ctx, &multiday_tid.to_hex(), "bad", TIMEOUT).await,
            Err(RunError::InvalidRequest(_))
        ));
        assert!(matches!(
            match_count(
                &ctx,
                MatchCountRequest {
                    view: ViewName::Sessions,
                    filter: None,
                    window: day_win,
                    cap: 10,
                    strict_exclusions: false,
                    timeout: TIMEOUT,
                }
            )
            .await,
            Err(RunError::InvalidRequest(_))
        ));
        assert!(matches!(
            match_count(
                &ctx,
                MatchCountRequest {
                    view: ViewName::Traces,
                    filter: None,
                    window: day_win,
                    cap: 0,
                    strict_exclusions: false,
                    timeout: TIMEOUT,
                }
            )
            .await,
            Err(RunError::InvalidRequest(_))
        ));

        let _ = storage.shutdown().await;
    }

    #[tokio::test]
    async fn trace_detail_on_far_apart_multiday_trace_uses_only_caller_around() {
        let dir = tempfile::tempdir().expect("tempdir");
        let storage = Storage::open(test_config(dir.path())).await.expect("open");
        let project: ProjectId = "018f6b4e-7c1a-7c1a-8c1a-000000000001".parse().expect("pid");

        let tid = TraceId::from_str("d1000000000000000000000000000001").expect("tid");
        let day1_ts = parse_rfc3339("2026-01-01T12:00:00Z").expect("ts1");
        let day5_ts = parse_rfc3339("2026-01-05T12:00:00Z").expect("ts5");

        let mut s1 = span_row(1, 0, day1_ts, 100);
        s1.project_id = project;
        s1.trace_id = tid;
        s1.name = "day1".to_owned();

        let mut s2 = span_row(1, 1, day5_ts, 200);
        s2.project_id = project;
        s2.trace_id = tid;
        s2.parent_span_id = Some(s1.span_id);
        s2.name = "day5".to_owned();

        storage
            .append_spans(project, span_rows(&[s1, s2]))
            .await
            .expect("append");
        storage
            .flush_now(Duration::from_secs(10))
            .await
            .expect("flush");

        // With `around = None`, both spans across 4 days apart must be returned.
        let ctx_none = build_context(&storage, project, QueryOptions::new(None, None)).await;
        let det_none = trace_detail(&ctx_none, &tid.to_hex(), TIMEOUT)
            .await
            .expect("detail")
            .expect("some");
        assert_eq!(qe_rows(&det_none.spans).len(), 2);

        // With caller `around = Some(day1_ts)`, caller's `around` is honored.
        let ctx_around =
            build_context(&storage, project, QueryOptions::new(Some(day1_ts), None)).await;
        let det_around = trace_detail(&ctx_around, &tid.to_hex(), TIMEOUT)
            .await
            .expect("detail around")
            .expect("some");
        assert_eq!(qe_rows(&det_around.spans).len(), 1);

        let _ = storage.shutdown().await;
    }

    #[tokio::test]
    async fn two_phase_cursor_pagination_with_score_and_search() {
        let dir = tempfile::tempdir().expect("tempdir");
        let storage = Storage::open(test_config(dir.path())).await.expect("open");
        let project: ProjectId = "018f6b4e-7c1a-7c1a-8c1a-000000000001".parse().expect("pid");

        let mut spans = dataset_spans();
        for s in &mut spans {
            s.project_id = project;
        }
        let mut scores = dataset_scores();
        for s in &mut scores {
            s.project_id = project;
        }
        storage
            .append_spans(project, span_rows(&spans))
            .await
            .expect("spans");
        storage
            .append_scores(project, score_rows(&scores))
            .await
            .expect("scores");

        let ctx = build_context(&storage, project, QueryOptions::new(None, None)).await;
        let win = TimeWindow::new(
            parse_rfc3339("2026-01-01T00:00:00Z").expect("from"),
            parse_rfc3339("2026-01-02T00:00:00Z").expect("to"),
        )
        .expect("win");

        for sort in [
            TraceSort::CostTotal,
            TraceSort::Duration,
            TraceSort::TotalTokens,
        ] {
            let mut cur: Option<String> = None;
            let mut paged = Vec::new();
            loop {
                let req = TraceListRequest {
                    filter: Some("score('helpfulness') >= 0.5"),
                    search: Some("order"),
                    scope: ListScope {
                        window: Some(win),
                        include_eval: false,
                        include_experiments: false,
                    },
                    strict_exclusions: false,
                    sort,
                    order: SortOrder::Desc,
                    limit: 1,
                    cursor: cur.as_deref(),
                    timeout: TIMEOUT,
                };
                let p = trace_list(&ctx, req).await.expect("page");
                paged.extend(qe_rows(&p.batches));
                match p.next_cursor {
                    Some(c) => cur = Some(c),
                    None => break,
                }
            }
            let full = trace_list(
                &ctx,
                TraceListRequest {
                    filter: Some("score('helpfulness') >= 0.5"),
                    search: Some("order"),
                    scope: ListScope {
                        window: Some(win),
                        include_eval: false,
                        include_experiments: false,
                    },
                    strict_exclusions: false,
                    sort,
                    order: SortOrder::Desc,
                    limit: 50,
                    cursor: None,
                    timeout: TIMEOUT,
                },
            )
            .await
            .expect("full");
            assert_eq!(paged, qe_rows(&full.batches));
            assert!(!paged.is_empty());
        }

        // Also test span_list with score() + search + cursor pagination
        let mut cur: Option<String> = None;
        let mut paged_spans = Vec::new();
        loop {
            let req = SpanListRequest {
                filter: Some("score('correctness') >= 0.0 OR span_kind = 'llm'"),
                search: Some("order"),
                scope: ListScope {
                    window: Some(win),
                    include_eval: false,
                    include_experiments: false,
                },
                strict_exclusions: false,
                sort: SpanSort::CostTotal,
                order: SortOrder::Desc,
                limit: 1,
                cursor: cur.as_deref(),
                timeout: TIMEOUT,
            };
            let p = span_list(&ctx, req).await.expect("span page");
            paged_spans.extend(qe_rows(&p.batches));
            match p.next_cursor {
                Some(c) => cur = Some(c),
                None => break,
            }
        }
        let full_spans = span_list(
            &ctx,
            SpanListRequest {
                filter: Some("score('correctness') >= 0.0 OR span_kind = 'llm'"),
                search: Some("order"),
                scope: ListScope {
                    window: Some(win),
                    include_eval: false,
                    include_experiments: false,
                },
                strict_exclusions: false,
                sort: SpanSort::CostTotal,
                order: SortOrder::Desc,
                limit: 50,
                cursor: None,
                timeout: TIMEOUT,
            },
        )
        .await
        .expect("full span");
        assert_eq!(paged_spans, qe_rows(&full_spans.batches));
        assert!(!paged_spans.is_empty());

        let _ = storage.shutdown().await;
    }

    #[test]
    fn privacy_safe_debug_for_detail_and_spans_data() {
        let schema = spans_view_schema().expect("schema");
        let empty = Batch::empty(schema);
        let det = TraceDetailData {
            trace: empty.clone(),
            spans: vec![empty.clone()],
            truncated: false,
        };
        let sp = TraceSpansData {
            required: vec![empty.clone()],
            earliest: vec![empty],
            truncated: true,
        };
        assert!(format!("{det:?}").contains("TraceDetailData"));
        assert!(format!("{sp:?}").contains("TraceSpansData"));
    }

    fn totals_request<'a>(
        scope: ListScope,
        filter: Option<&'a str>,
        search: Option<&'a str>,
    ) -> TraceTotalsRequest<'a> {
        TraceTotalsRequest {
            filter,
            search,
            scope,
            timeout: TIMEOUT,
        }
    }

    #[tokio::test]
    async fn totals_match_the_listed_rows_and_handle_empty_exact_cost_and_errors() {
        let dir = tempfile::tempdir().expect("tempdir");
        let storage = Storage::open(test_config(dir.path())).await.expect("open");
        let project: ProjectId = "018f6b4e-7c1a-7c1a-8c1a-000000000009".parse().expect("pid");
        let mut spans = dataset_spans();
        for s in &mut spans {
            s.project_id = project;
        }
        storage
            .append_spans(project, span_rows(&spans))
            .await
            .expect("append spans");
        let ctx = build_context(&storage, project, QueryOptions::new(None, None)).await;

        let day = TimeWindow::new(
            parse_rfc3339("2026-01-01T00:00:00Z").expect("from"),
            parse_rfc3339("2026-01-02T00:00:00Z").expect("to"),
        )
        .expect("day");
        let mk_scope = |window: Option<TimeWindow>, include_eval: bool| ListScope {
            window,
            include_eval,
            include_experiments: false,
        };

        let cases: [(ListScope, Option<&str>, Option<&str>); 5] = [
            (mk_scope(None, false), None, None),
            (mk_scope(None, true), None, None),
            (mk_scope(None, false), Some("status = 'error'"), None),
            (mk_scope(None, false), Some("cost_total > 0.002"), None),
            (mk_scope(Some(day), false), None, Some("order")),
        ];
        for (index, (case_scope, filter, search)) in cases.into_iter().enumerate() {
            let mut request = trace_req(case_scope, TraceSort::StartTime, SortOrder::Desc, 1000);
            request.filter = filter;
            request.search = search;
            let page = trace_list(&ctx, request).await.expect("list");
            let listed_rows = qe_rows(&page.batches);
            let totals = trace_totals(&ctx, totals_request(case_scope, filter, search))
                .await
                .expect("totals");
            assert_eq!(
                totals.trace_count,
                i64::try_from(listed_rows.len()).expect("count"),
                "case {index}"
            );
            let errors = listed_rows
                .iter()
                .filter(|row| row["status"] == "error")
                .count();
            assert_eq!(
                totals.error_trace_count,
                i64::try_from(errors).expect("count"),
                "case {index}"
            );
            let cost: f64 = listed_rows
                .iter()
                .map(|row| row["cost_total"].as_f64().unwrap_or(0.0))
                .sum();
            assert!(
                (totals.cost_total - cost).abs() < 1e-12,
                "case {index}: {} vs {cost}",
                totals.cost_total
            );
            if index == 0 {
                assert!(totals.trace_count > 0);
                assert!(totals.error_trace_count > 0);
            }
        }

        // Empty result yields zeros.
        let empty = trace_totals(
            &ctx,
            totals_request(mk_scope(None, false), Some("name = 'no such trace'"), None),
        )
        .await
        .expect("empty totals");
        assert_eq!(
            empty,
            TraceTotals {
                trace_count: 0,
                error_trace_count: 0,
                cost_total: 0.0,
            }
        );

        // Bad filter and search without window fail cleanly.
        let bad = trace_totals(
            &ctx,
            totals_request(mk_scope(None, false), Some("error_count >"), None),
        )
        .await;
        assert!(matches!(bad, Err(RunError::Invalid(_))), "{bad:?}");
        let no_window = trace_totals(
            &ctx,
            totals_request(mk_scope(None, false), None, Some("order")),
        )
        .await;
        match no_window {
            Err(RunError::InvalidRequest(message)) => {
                assert_eq!(message, "search needs a time range");
            }
            other => panic!("{other:?}"),
        }

        // Exact decimal sum (10 * 0.1 == 1.0 exactly, no binary float drift).
        let exact_project: ProjectId = "018f6b4e-7c1a-7c1a-8c1a-00000000000a".parse().expect("pid");
        let mut exact_spans: Vec<_> = (1..=10_u16)
            .map(|trace| {
                let mut s = span_row(trace, 0, i64::from(trace) * 1_000, 1);
                s.project_id = exact_project;
                s.cost_total = Some(0.1);
                s
            })
            .collect();
        exact_spans[2].status = SpanStatus::Error;
        storage
            .append_spans(exact_project, span_rows(&exact_spans))
            .await
            .expect("append exact spans");
        let exact_ctx = build_context(&storage, exact_project, QueryOptions::new(None, None)).await;
        let exact_totals = trace_totals(
            &exact_ctx,
            totals_request(mk_scope(None, false), None, None),
        )
        .await
        .expect("exact totals");
        assert_eq!(
            exact_totals,
            TraceTotals {
                trace_count: 10,
                error_trace_count: 1,
                cost_total: 1.0,
            }
        );

        let _ = storage.shutdown().await;
    }

    #[tokio::test]
    async fn two_phase_span_list_matches_single_phase_across_sorts_ties_visible_from_strict_and_memtable()
     {
        let dir = tempfile::tempdir().expect("tempdir");
        let storage = Storage::open(test_config(dir.path())).await.expect("open");
        let project: ProjectId = "018f6b4e-7c1a-7c1a-8c1a-00000000000b".parse().expect("pid");

        let t_base = parse_rfc3339("2026-01-01T12:00:00Z").expect("ts");
        // Batch 1 (flushed to Statam disk): ties, NULLs, eval/experiment exclusions, and a span
        // that will later receive an updated copy in memtable with newer `ingest_time`.
        let mut disk_spans = Vec::new();
        for i in 1..=12_u16 {
            let start = t_base + i64::from(i) * 1_000_000_000;
            let mut s = span_row(i, 0, start, start + 1_000);
            s.project_id = project;
            s.name = format!("disk-span-{i}");
            // Deliberate ties on duration_ns, cost_total, total_tokens + NULLs.
            s.end_time = s.start_time + i64::from((i % 4) + 1) * 100_000;
            s.cost_total = if i % 5 == 0 {
                None
            } else {
                Some(f64::from(i % 3) * 0.25 + 0.1)
            };
            s.total_tokens = if i % 6 == 0 {
                None
            } else {
                Some(i64::from(i % 4) * 50)
            };
            if i == 11 {
                s.environment = Some(cetra_core::names::environment::EVAL.to_owned());
            }
            if i == 12 {
                s.metadata
                    .insert(metadata_key::EXPERIMENT_ID.to_owned(), "exp-1".to_owned());
            }
            if i % 2 == 0 {
                s.input = Some("order-search-needle".to_owned());
            }
            disk_spans.push(s);
        }
        storage
            .append_spans(project, span_rows(&disk_spans))
            .await
            .expect("append disk spans");
        storage.flush_now(TIMEOUT).await.expect("flush to statam");
        assert!(
            !storage.catalog_files(crate::wal::Table::Spans).is_empty(),
            "disk spans must be flushed"
        );

        // Batch 2 (stays in live MemtableSource): new spans + duplicate of span 3 with newer ingest_time.
        let mut mem_spans = Vec::new();
        let mut dup3 = disk_spans[2].clone();
        dup3.ingest_time += 50_000_000;
        dup3.name = "disk-span-3-updated".to_owned();
        mem_spans.push(dup3);
        for i in 13..=16_u16 {
            let start = t_base + i64::from(i) * 1_000_000_000;
            let mut s = span_row(i, 0, start, start + 50_000_000);
            s.project_id = project;
            s.name = format!("mem-span-{i}");
            s.end_time = s.start_time + i64::from((i % 4) + 1) * 100_000;
            s.cost_total = Some(f64::from(i % 3) * 0.25 + 0.1);
            s.total_tokens = Some(i64::from(i % 4) * 50);
            s.input = Some("order-search-needle".to_owned());
            mem_spans.push(s);
        }
        storage
            .append_spans(project, span_rows(&mem_spans))
            .await
            .expect("append mem spans");

        let win = TimeWindow::new(t_base, t_base + 30 * 1_000_000_000).expect("win");
        let vf = t_base + 3 * 1_000_000_000;

        for (visible_from, strict, search) in [
            (None, false, None),
            (Some(vf), false, None),
            (None, true, Some("order-search-needle")),
            (Some(vf), true, Some("order-search-needle")),
        ] {
            let two_phase_eng = EngineContext::from_storage(
                &storage,
                project,
                &QueryOptions::new(visible_from, None),
            )
            .expect("two_phase");
            assert!(two_phase_eng.spans_statam.is_some());
            assert!(two_phase_eng.spans_mem.is_some());
            let mut single_phase_eng = two_phase_eng.clone();
            single_phase_eng.set_force_range_mode(true);
            let ctx_2p = QueryContext::from_engine(two_phase_eng);
            let ctx_1p = QueryContext::from_engine(single_phase_eng);

            let scope = ListScope {
                window: Some(win),
                include_eval: false,
                include_experiments: false,
            };

            for sort in [
                SpanSort::Duration,
                SpanSort::CostTotal,
                SpanSort::TotalTokens,
            ] {
                for order in [SortOrder::Desc, SortOrder::Asc] {
                    // Page through with limit=2 using two-phase path.
                    let mut cur: Option<String> = None;
                    let mut rows_2p = Vec::new();
                    loop {
                        let page = span_list(
                            &ctx_2p,
                            SpanListRequest {
                                filter: None,
                                search,
                                scope,
                                strict_exclusions: strict,
                                sort,
                                order,
                                limit: 2,
                                cursor: cur.as_deref(),
                                timeout: TIMEOUT,
                            },
                        )
                        .await
                        .expect("2p page");
                        rows_2p.extend(qe_rows(&page.batches));
                        match page.next_cursor {
                            Some(next) => cur = Some(next),
                            None => break,
                        }
                    }

                    // Reference single-phase full list.
                    let ref_page = span_list(
                        &ctx_1p,
                        SpanListRequest {
                            filter: None,
                            search,
                            scope,
                            strict_exclusions: strict,
                            sort,
                            order,
                            limit: 100,
                            cursor: None,
                            timeout: TIMEOUT,
                        },
                    )
                    .await
                    .expect("1p ref");
                    let rows_1p = qe_rows(&ref_page.batches);

                    assert_eq!(
                        rows_2p, rows_1p,
                        "two-phase mismatch for sort={sort:?}, order={order:?}, vf={visible_from:?}, strict={strict}, search={search:?}"
                    );
                    assert!(!rows_2p.is_empty());
                }
            }
        }

        let _ = storage.shutdown().await;
    }

    async fn collect_span_list_pages(
        ctx: &QueryContext,
        scope: ListScope,
        strict_exclusions: bool,
        search: Option<&str>,
        order: SortOrder,
        limit: usize,
    ) -> Vec<Value> {
        let mut cur: Option<String> = None;
        let mut rows = Vec::new();
        loop {
            let page = span_list(
                ctx,
                SpanListRequest {
                    filter: None,
                    search,
                    scope,
                    strict_exclusions,
                    sort: SpanSort::StartTime,
                    order,
                    limit,
                    cursor: cur.as_deref(),
                    timeout: TIMEOUT,
                },
            )
            .await
            .expect("span_list page");
            rows.extend(qe_rows(&page.batches));
            match page.next_cursor {
                Some(next) => cur = Some(next),
                None => break,
            }
        }
        rows
    }

    #[tokio::test]
    async fn wave_ordered_span_list_stops_early_and_respects_cross_file_ties() {
        let project: ProjectId = "018f6b4e-7c1a-7c1a-8c1a-00000000000c".parse().expect("pid");
        let t_base = parse_rfc3339("2026-01-01T12:00:00Z").expect("ts");
        let sec = 1_000_000_000_i64;
        let win = TimeWindow::new(t_base, t_base + 100 * sec).expect("win");
        let scope = ListScope {
            window: Some(win),
            include_eval: false,
            include_experiments: false,
        };

        // --- A. Descending: early stop after Wave 1 + cross-file tie on kth_time == max_time ---
        {
            let dir = tempfile::tempdir().expect("tempdir");
            let storage = Storage::open(test_config(dir.path())).await.expect("open");

            let mk = |trace: u16, span: u8, s: i64| {
                let mut row = span_row(trace, span, t_base + s * sec, t_base + s * sec + 1_000);
                row.project_id = project;
                row.name = format!("span-{s}-{span}");
                row
            };

            // File 0 (oldest): [10s..13s]
            let f0 = vec![mk(1, 0, 10), mk(2, 0, 11), mk(3, 0, 12), mk(4, 0, 13)];
            // File 1: [20s..23s]
            let f1 = vec![mk(5, 0, 20), mk(6, 0, 21), mk(7, 0, 22), mk(8, 0, 23)];
            // File 2: [30s..40s], including winning tie rows at 40s with small span_id (1, 2).
            let f2 = vec![mk(9, 0, 30), mk(10, 0, 31), mk(11, 1, 40), mk(12, 2, 40)];
            // File 3 (newest): [40s..42s], 4 rows so fetch=4 fills with kth_time = 40s == f2.max_time!
            let f3 = vec![mk(13, 30, 40), mk(14, 31, 40), mk(15, 0, 41), mk(16, 0, 42)];

            for batch in [f0, f1, f2, f3] {
                storage
                    .append_spans(project, span_rows(&batch))
                    .await
                    .expect("append");
                storage.flush_now(TIMEOUT).await.expect("flush");
            }

            let mut files = storage.catalog_files(crate::wal::Table::Spans);
            files.sort_by_key(|f| f.min_time);
            assert_eq!(files.len(), 4);

            let two_phase_eng =
                EngineContext::from_storage(&storage, project, &QueryOptions::new(None, None))
                    .expect("2p");
            let mut single_phase_eng = two_phase_eng.clone();
            single_phase_eng.set_force_range_mode(true);
            let ctx_2p = QueryContext::from_engine(two_phase_eng);
            let ctx_1p = QueryContext::from_engine(single_phase_eng);

            // limit = 1 -> fetch = 4. File 3 yields [42s, 41s, 40s(30), 40s(31)], so kth_time = 40s.
            // Because File 2 has max_time == 40s, strict `>` MUST NOT stop before File 2; File 2's
            // winning tie rows 40s(1) and 40s(2) are picked up, and File 0 (max_time 13s < 40s) is skipped!
            let page_desc = span_list(
                &ctx_2p,
                SpanListRequest {
                    filter: None,
                    search: None,
                    scope,
                    strict_exclusions: false,
                    sort: SpanSort::StartTime,
                    order: SortOrder::Desc,
                    limit: 1,
                    cursor: None,
                    timeout: TIMEOUT,
                },
            )
            .await
            .expect("desc page");
            assert!(
                storage
                    .qe_runtime
                    .metadata_cache
                    .get_statam(files[3].id)
                    .is_some(),
                "newest file must be opened"
            );
            assert!(
                storage
                    .qe_runtime
                    .metadata_cache
                    .get_statam(files[2].id)
                    .is_some(),
                "tied file with max_time == kth_time must NOT be skipped"
            );
            assert!(
                storage
                    .qe_runtime
                    .metadata_cache
                    .get_statam(files[0].id)
                    .is_none(),
                "oldest file with max_time < kth_time must be stopped before"
            );
            assert_eq!(qe_rows(&page_desc.batches).len(), 1);

            // Verify tie-breaking order across the 40s boundary via full cursor pagination vs force_range_mode.
            let paged_2p =
                collect_span_list_pages(&ctx_2p, scope, false, None, SortOrder::Desc, 2).await;
            let full_1p = span_list(
                &ctx_1p,
                SpanListRequest {
                    filter: None,
                    search: None,
                    scope,
                    strict_exclusions: false,
                    sort: SpanSort::StartTime,
                    order: SortOrder::Desc,
                    limit: 100,
                    cursor: None,
                    timeout: TIMEOUT,
                },
            )
            .await
            .expect("1p desc");
            assert_eq!(paged_2p, qe_rows(&full_1p.batches));
            assert_eq!(paged_2p.len(), 16);
            assert_eq!(paged_2p[2]["name"], "span-40-31");
            assert_eq!(paged_2p[3]["name"], "span-40-30");
            assert_eq!(paged_2p[4]["name"], "span-40-2");
            assert_eq!(paged_2p[5]["name"], "span-40-1");

            let _ = storage.shutdown().await;
        }

        // --- B. Ascending: early stop + cross-file tie on kth_time == min_time ---
        {
            let dir = tempfile::tempdir().expect("tempdir");
            let storage = Storage::open(test_config(dir.path())).await.expect("open");

            let mk = |trace: u16, span: u8, s: i64| {
                let mut row = span_row(trace, span, t_base + s * sec, t_base + s * sec + 1_000);
                row.project_id = project;
                row.name = format!("span-{s}-{span}");
                row
            };

            // File 0 (oldest): [10s..12s], 4 rows so fetch=4 fills with kth_time = 12s == f1.min_time!
            let f0 = vec![mk(1, 0, 10), mk(2, 0, 11), mk(3, 30, 12), mk(4, 31, 12)];
            // File 1: [12s..15s], including winning tie rows at 12s with small span_id (1, 2).
            let f1 = vec![mk(5, 1, 12), mk(6, 2, 12), mk(7, 0, 14), mk(8, 0, 15)];
            // File 2: [20s..25s]
            let f2 = vec![mk(9, 0, 20), mk(10, 0, 25)];
            // File 3 (newest): [30s..35s]
            let f3 = vec![mk(11, 0, 30), mk(12, 0, 35)];

            for batch in [f0, f1, f2, f3] {
                storage
                    .append_spans(project, span_rows(&batch))
                    .await
                    .expect("append");
                storage.flush_now(TIMEOUT).await.expect("flush");
            }

            let mut files = storage.catalog_files(crate::wal::Table::Spans);
            files.sort_by_key(|f| f.min_time);
            assert_eq!(files.len(), 4);

            let two_phase_eng =
                EngineContext::from_storage(&storage, project, &QueryOptions::new(None, None))
                    .expect("2p");
            let mut single_phase_eng = two_phase_eng.clone();
            single_phase_eng.set_force_range_mode(true);
            let ctx_2p = QueryContext::from_engine(two_phase_eng);
            let ctx_1p = QueryContext::from_engine(single_phase_eng);

            // limit = 1 -> fetch = 4. File 0 yields kth_time = 12s == f1.min_time, so File 1 MUST be opened,
            // while File 3 (min_time 30s > 12s) must NOT be opened.
            let page_asc = span_list(
                &ctx_2p,
                SpanListRequest {
                    filter: None,
                    search: None,
                    scope,
                    strict_exclusions: false,
                    sort: SpanSort::StartTime,
                    order: SortOrder::Asc,
                    limit: 1,
                    cursor: None,
                    timeout: TIMEOUT,
                },
            )
            .await
            .expect("asc page");
            assert!(
                storage
                    .qe_runtime
                    .metadata_cache
                    .get_statam(files[0].id)
                    .is_some()
            );
            assert!(
                storage
                    .qe_runtime
                    .metadata_cache
                    .get_statam(files[1].id)
                    .is_some(),
                "tied file with min_time == kth_time must NOT be skipped"
            );
            assert!(
                storage
                    .qe_runtime
                    .metadata_cache
                    .get_statam(files[3].id)
                    .is_none(),
                "newest file with min_time > kth_time must be stopped before"
            );
            assert_eq!(qe_rows(&page_asc.batches).len(), 1);

            let paged_2p =
                collect_span_list_pages(&ctx_2p, scope, false, None, SortOrder::Asc, 2).await;
            let full_1p = span_list(
                &ctx_1p,
                SpanListRequest {
                    filter: None,
                    search: None,
                    scope,
                    strict_exclusions: false,
                    sort: SpanSort::StartTime,
                    order: SortOrder::Asc,
                    limit: 100,
                    cursor: None,
                    timeout: TIMEOUT,
                },
            )
            .await
            .expect("1p asc");
            assert_eq!(paged_2p, qe_rows(&full_1p.batches));
            assert_eq!(paged_2p.len(), 12);
            assert_eq!(paged_2p[2]["name"], "span-12-1");
            assert_eq!(paged_2p[3]["name"], "span-12-2");
            assert_eq!(paged_2p[4]["name"], "span-12-30");

            let _ = storage.shutdown().await;
        }
    }

    #[tokio::test]
    async fn wave_ordered_span_list_memtable_newer_and_older_stops_disk_waves_early() {
        let dir = tempfile::tempdir().expect("tempdir");
        let storage = Storage::open(test_config(dir.path())).await.expect("open");
        let project: ProjectId = "018f6b4e-7c1a-7c1a-8c1a-00000000000d".parse().expect("pid");

        let t_base = parse_rfc3339("2026-01-01T12:00:00Z").expect("ts");
        let sec = 1_000_000_000_i64;
        let mk = |trace: u16, s: i64| {
            let mut row = span_row(trace, 0, t_base + s * sec, t_base + s * sec + 1_000);
            row.project_id = project;
            row.name = format!("span-{s}");
            row
        };

        // Disk files span [20s..45s].
        for range in [20..=25_i64, 30..=35_i64, 40..=45_i64] {
            let batch: Vec<_> = range
                .enumerate()
                .map(|(i, s)| mk(u16::try_from(s).expect("tid") + i as u16 * 100, s))
                .collect();
            storage
                .append_spans(project, span_rows(&batch))
                .await
                .expect("append disk");
            storage.flush_now(TIMEOUT).await.expect("flush");
        }
        let disk_files = storage.catalog_files(crate::wal::Table::Spans);
        assert_eq!(disk_files.len(), 3);

        // Live memtable has 6 rows strictly older [1s..6s] AND 6 rows strictly newer [50s..55s].
        let mut mem_batch = Vec::new();
        for s in 1..=6_i64 {
            mem_batch.push(mk(500 + s as u16, s));
        }
        for s in 50..=55_i64 {
            mem_batch.push(mk(600 + s as u16, s));
        }
        storage
            .append_spans(project, span_rows(&mem_batch))
            .await
            .expect("append mem");

        let win = TimeWindow::new(t_base, t_base + 100 * sec).expect("win");
        let scope = ListScope {
            window: Some(win),
            include_eval: false,
            include_experiments: false,
        };

        let two_phase_eng =
            EngineContext::from_storage(&storage, project, &QueryOptions::new(None, None))
                .expect("2p");
        let mut single_phase_eng = two_phase_eng.clone();
        single_phase_eng.set_force_range_mode(true);
        let ctx_2p = QueryContext::from_engine(two_phase_eng);
        let ctx_1p = QueryContext::from_engine(single_phase_eng);

        // Both Desc and Asc with limit=2 (fetch=6) are satisfied entirely by memtable before idx=0,
        // so ZERO disk files are opened in metadata_cache!
        for order in [SortOrder::Desc, SortOrder::Asc] {
            let page_2p = span_list(
                &ctx_2p,
                SpanListRequest {
                    filter: None,
                    search: None,
                    scope,
                    strict_exclusions: false,
                    sort: SpanSort::StartTime,
                    order,
                    limit: 2,
                    cursor: None,
                    timeout: TIMEOUT,
                },
            )
            .await
            .expect("2p mem-only");
            assert_eq!(qe_rows(&page_2p.batches).len(), 2);
            for file in &disk_files {
                assert!(
                    storage
                        .qe_runtime
                        .metadata_cache
                        .get_statam(file.id)
                        .is_none(),
                    "memtable should satisfy Top-K for {order:?} without opening disk file {}",
                    file.path
                );
            }
        }

        // Now verify full cursor pagination parity across memtable + all disk files.
        for order in [SortOrder::Desc, SortOrder::Asc] {
            let paged_2p = collect_span_list_pages(&ctx_2p, scope, false, None, order, 3).await;
            let full_1p = span_list(
                &ctx_1p,
                SpanListRequest {
                    filter: None,
                    search: None,
                    scope,
                    strict_exclusions: false,
                    sort: SpanSort::StartTime,
                    order,
                    limit: 100,
                    cursor: None,
                    timeout: TIMEOUT,
                },
            )
            .await
            .expect("1p ref");
            assert_eq!(paged_2p, qe_rows(&full_1p.batches));
            assert_eq!(paged_2p.len(), 30);
        }

        let _ = storage.shutdown().await;
    }

    #[tokio::test]
    async fn wave_ordered_span_list_overlapping_l0_l1_strict_visible_from_and_search() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut cfg = test_config(dir.path());
        cfg.compaction.l0_files = 2;
        cfg.compaction.interval = Duration::from_secs(3_600);
        let storage = Storage::open(cfg).await.expect("open");
        let project: ProjectId = "018f6b4e-7c1a-7c1a-8c1a-00000000000e".parse().expect("pid");

        let t_base = parse_rfc3339("2026-01-01T12:00:00Z").expect("ts");
        let sec = 1_000_000_000_i64;
        let mk_span = |trace: u16, span: u8, s: i64, ingest_off: i64, needle: bool| {
            let start = t_base + s * sec;
            let mut row = span_row(trace, span, start, start + ingest_off);
            row.project_id = project;
            row.name = format!("span-{trace}-{span}-{s}");
            if needle {
                row.input = Some("chloroplast photosynthesis pathway".to_owned());
            }
            row
        };

        // 1. Two L0 flushes -> compact into L1 (with overlapping times, eval span, experiment root+child).
        let l1_a = vec![
            mk_span(1, 0, 10, 1_000, true),
            mk_span(2, 0, 15, 1_000, false),
            mk_span(3, 0, 20, 1_000, true),
            {
                let mut eval_s = mk_span(4, 0, 22, 1_000, true);
                eval_s.environment = Some(cetra_core::names::environment::EVAL.to_owned());
                eval_s
            },
        ];
        let mut exp_root = mk_span(5, 0, 12, 1_000, true);
        exp_root
            .metadata
            .insert(metadata_key::EXPERIMENT_ID.to_owned(), "exp-99".to_owned());
        let mut exp_child = mk_span(5, 1, 24, 1_000, true);
        exp_child.parent_span_id = Some(exp_root.span_id);
        let l1_b = vec![
            exp_root,
            exp_child,
            mk_span(6, 0, 25, 1_000, false),
            mk_span(7, 0, 30, 1_000, true),
        ];

        for batch in [l1_a, l1_b] {
            storage
                .append_spans(project, span_rows(&batch))
                .await
                .expect("append l1");
            storage.flush_now(TIMEOUT).await.expect("flush l1");
        }
        let compact_report = storage
            .compact(t_base + 2 * 3_600 * sec)
            .await
            .expect("compact");
        assert_eq!(compact_report.failed, 0);

        // 2. Two overlapping L0 files alongside the L1 file(s), including an updated copy of trace 3.
        let l0_a = vec![
            mk_span(3, 0, 20, 50_000, true),
            mk_span(8, 0, 18, 1_000, true),
            mk_span(9, 0, 28, 1_000, false),
        ];
        let mut eval_root = mk_span(10, 0, 14, 1_000, true);
        eval_root.environment = Some(cetra_core::names::environment::EVAL.to_owned());
        let mut eval_child = mk_span(10, 1, 32, 1_000, true);
        eval_child.parent_span_id = Some(eval_root.span_id);
        let l0_b = vec![eval_root, eval_child, mk_span(11, 0, 35, 1_000, true)];

        for batch in [l0_a, l0_b] {
            storage
                .append_spans(project, span_rows(&batch))
                .await
                .expect("append l0");
            storage.flush_now(TIMEOUT).await.expect("flush l0");
        }

        let catalog_files = storage.catalog_files(crate::wal::Table::Spans);
        assert!(
            catalog_files.iter().any(|f| f.level == 1),
            "expected at least one L1 file"
        );
        assert!(
            catalog_files.iter().any(|f| f.level == 0),
            "expected overlapping L0 files"
        );

        // 3. Live memtable batch: newer copy of trace 7 + older and newer spans.
        let mem_batch = vec![
            mk_span(7, 0, 30, 90_000, true),
            mk_span(12, 0, 8, 1_000, true),
            mk_span(13, 0, 40, 1_000, true),
        ];
        storage
            .append_spans(project, span_rows(&mem_batch))
            .await
            .expect("append mem");

        let win = TimeWindow::new(t_base, t_base + 100 * sec).expect("win");
        let vf = t_base + 14 * sec;

        for (visible_from, strict, include_eval_exp, search) in [
            (None, false, false, None),
            (Some(vf), false, false, None),
            (None, true, true, Some("photosynthesis")),
            (Some(vf), true, true, Some("photosynthesis")),
            (None, true, false, Some("cetra_nonexistent_needle_9x7z")),
        ] {
            let two_phase_eng = EngineContext::from_storage(
                &storage,
                project,
                &QueryOptions::new(visible_from, None),
            )
            .expect("2p");
            let mut single_phase_eng = two_phase_eng.clone();
            single_phase_eng.set_force_range_mode(true);
            let ctx_2p = QueryContext::from_engine(two_phase_eng);
            let ctx_1p = QueryContext::from_engine(single_phase_eng);

            let scope = ListScope {
                window: Some(win),
                include_eval: include_eval_exp,
                include_experiments: include_eval_exp,
            };

            for order in [SortOrder::Desc, SortOrder::Asc] {
                let rows_2p =
                    collect_span_list_pages(&ctx_2p, scope, strict, search, order, 2).await;
                let ref_page = span_list(
                    &ctx_1p,
                    SpanListRequest {
                        filter: None,
                        search,
                        scope,
                        strict_exclusions: strict,
                        sort: SpanSort::StartTime,
                        order,
                        limit: 100,
                        cursor: None,
                        timeout: TIMEOUT,
                    },
                )
                .await
                .expect("1p ref");
                let rows_1p = qe_rows(&ref_page.batches);

                assert_eq!(
                    rows_2p, rows_1p,
                    "wave StartTime parity mismatch for order={order:?}, vf={visible_from:?}, strict={strict}, search={search:?}"
                );
                if search == Some("cetra_nonexistent_needle_9x7z") {
                    assert!(rows_2p.is_empty());
                } else {
                    assert!(!rows_2p.is_empty());
                }
            }
        }

        // Also verify strict_exclusions=true on a clean project with zero bad traces (StrictExclusionsFilter::None).
        let clean_project: ProjectId = "018f6b4e-7c1a-7c1a-8c1a-00000000000f".parse().expect("pid");
        let clean_spans = vec![
            mk_span(20, 0, 10, 1_000, true),
            mk_span(21, 0, 20, 1_000, true),
        ];
        let mut clean_rows = clean_spans;
        for s in &mut clean_rows {
            s.project_id = clean_project;
        }
        storage
            .append_spans(clean_project, span_rows(&clean_rows))
            .await
            .expect("append clean");
        storage.flush_now(TIMEOUT).await.expect("flush clean");
        let clean_ctx_2p =
            build_context(&storage, clean_project, QueryOptions::new(None, None)).await;
        let clean_scope = ListScope {
            window: Some(win),
            include_eval: false,
            include_experiments: false,
        };
        let clean_rows_res =
            collect_span_list_pages(&clean_ctx_2p, clean_scope, true, None, SortOrder::Desc, 1)
                .await;
        assert_eq!(clean_rows_res.len(), 2);

        let _ = storage.shutdown().await;
    }
}
