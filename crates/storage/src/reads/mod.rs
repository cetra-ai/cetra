//! Queries behind the read endpoints (D92).
//!
//! Each function takes a [`QueryContext`] and a request and runs under the
//! request's timeout. Lists return Arrow batches in view-column order.
//! Dashboard metrics return plain structs. The server maps either to API types.

mod alerts;
mod cursor;
mod deletions;
pub(crate) mod metrics;
mod scores;
mod sessions;
mod traces;
mod users;

use arrow::record_batch::RecordBatch;
use datafusion::logical_expr::Expr;
use datafusion::prelude::DataFrame;

use crate::filter::{FilterView, exclusions, time_window};
use crate::query_error::{RunError, classify};
use crate::views::{QueryContext, TimeWindow, ViewName};

pub use alerts::{AlertMeasure, AlertValue, AlertValueRequest, alert_value};
pub use deletions::{
    FilterDeletionRequest, MAX_FILTER_DELETION_TRACES, UserDeletionScope, eval_trace_ids,
    filter_trace_ids, user_deletion_scope,
};
pub use metrics::{
    Bucket, BucketWidth, Kpi, KpiName, MetricsData, MetricsGroupBy, MetricsRequest, ModelUsageRow,
    Point, Series, SeriesName,
};
pub use scores::{
    LATEST_SCORE_COLUMNS, ScoreListRequest, ScoreTargets, latest_scores, score_list, scores_by_id,
};
pub use sessions::{
    MAX_SESSION_TURNS, SessionDetailData, SessionListRequest, SessionSort, session_detail,
    session_list,
};
pub use traces::{
    MAX_MATCH_COUNT, MAX_REFRESH_IDS, MAX_REFRESH_SPANS_PER_TRACE, MAX_TRACE_SPANS,
    MatchCountRequest, SPAN_LIST_COLUMNS, SPAN_REFRESH_COLUMNS, SpanListRequest,
    SpanRefreshRequest, SpanSort, TRACE_REFRESH_STRICT_COLUMNS, TraceDetailData, TraceListRequest,
    TraceRefreshRequest, TraceSort, TraceSpansData, TraceSpansRequest, match_count, span_detail,
    span_list, span_refresh, trace_detail, trace_list, trace_list_plan, trace_refresh, trace_spans,
};
pub use users::{
    MAX_USER_SESSIONS, USER_COLUMNS, UserDetailData, UserListRequest, UserSort, user_detail,
    user_list,
};

/// Sort direction for a list. Nulls are last in both directions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortOrder {
    /// Smallest values first.
    Asc,
    /// Largest values first.
    Desc,
}

/// Which rows a list includes before the caller's filter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ListScope {
    /// `start_time ∈ [from, to)` when set. The context itself is not windowed.
    pub window: Option<TimeWindow>,
    /// Keep rows whose environment is the eval environment.
    pub include_eval: bool,
    /// Keep rows that belong to an experiment.
    pub include_experiments: bool,
}

/// One page of a list.
#[derive(Debug)]
pub struct Page {
    /// Rows in sort order, at most [`MAX_PAGE`].
    pub batches: Vec<RecordBatch>,
    /// Cursor for the next page. `None` when this page is the last.
    pub next_cursor: Option<String>,
}

/// Most rows one list page returns.
pub const MAX_PAGE: usize = 1000;

/// Empty or all-whitespace text is no filter and no search.
pub(crate) fn present(text: Option<&str>) -> Option<&str> {
    text.filter(|text| !text.chars().all(char::is_whitespace))
}

/// Parses `text` as a filter of `view` without running anything (D-I-30). Empty or blank text
/// is accepted (it means no filter).
///
/// # Errors
///
/// [`RunError::Invalid`] when the text does not parse or names an unknown column;
/// [`RunError::InvalidRequest`] for a view other than traces or spans.
pub fn validate_filter(ctx: &QueryContext, view: ViewName, text: &str) -> Result<(), RunError> {
    let view = match view {
        ViewName::Traces => FilterView::Traces,
        ViewName::Spans => FilterView::Spans,
        _ => {
            return Err(RunError::InvalidRequest(
                "filters are checked for traces and spans only".to_owned(),
            ));
        }
    };
    match present(Some(text)) {
        None => Ok(()),
        Some(text) => ctx
            .parse_filter(view, text)
            .map(|_| ())
            .map_err(RunError::from),
    }
}

/// The `traces` view restricted by a list's window and default exclusions.
pub(crate) async fn scoped_traces(
    ctx: &QueryContext,
    scope: ListScope,
    references_environment: bool,
) -> Result<DataFrame, RunError> {
    let mut frame = ctx.session().table("traces").await.map_err(classify)?;
    let mut conjuncts = Vec::new();
    if let Some(window) = scope.window {
        conjuncts.push(time_window(window));
    }
    if let Some(expr) = exclusions(
        ctx,
        FilterView::Traces,
        scope.include_eval,
        scope.include_experiments,
        references_environment,
    )? {
        conjuncts.push(expr);
    }
    if let Some(predicate) = conjuncts.into_iter().reduce(Expr::and) {
        frame = frame.filter(predicate).map_err(classify)?;
    }
    Ok(frame)
}

#[cfg(test)]
mod tests {
    use super::validate_filter;
    use crate::query_error::RunError;
    use crate::test_support::example_query;
    use crate::views::{QueryContext, ViewName};

    #[test]
    fn validate_filter_accepts_blank_and_rejects_bad_text() {
        let ctx = QueryContext::new(example_query(), None).expect("context");
        assert!(validate_filter(&ctx, ViewName::Traces, "").is_ok());
        assert!(validate_filter(&ctx, ViewName::Traces, "  ").is_ok());
        assert!(matches!(
            validate_filter(&ctx, ViewName::Traces, "error_count >"),
            Err(RunError::Invalid(_))
        ));
        assert!(validate_filter(&ctx, ViewName::Traces, "error_count > 0").is_ok());
        assert!(validate_filter(&ctx, ViewName::Spans, "span_kind = 'llm'").is_ok());
        assert!(matches!(
            validate_filter(&ctx, ViewName::Sessions, "error_count > 0"),
            Err(RunError::InvalidRequest(_))
        ));
    }
}
