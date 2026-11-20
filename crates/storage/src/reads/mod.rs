//! Queries behind the read endpoints (D92).
//!
//! Each function takes a [`QueryContext`] and a request and runs under the
//! request's timeout. Lists return Arrow batches in view-column order.

mod alerts;
mod buckets;
mod columns;
mod cursor;
mod deletions;
mod experiments;
mod facets;
mod prompts;
mod scores;
mod sessions;
mod traces;
mod users;
mod widgets;

use arrow::record_batch::RecordBatch;
use datafusion::logical_expr::Expr;
use datafusion::prelude::DataFrame;

use crate::filter::{FilterView, exclusions, time_window};
use crate::query_error::{RunError, classify};
use crate::views::{QueryContext, TimeWindow, ViewName};

pub use alerts::{AlertMeasure, AlertValue, AlertValueRequest, alert_value};
pub use buckets::{Bucket, BucketWidth};
pub use deletions::{
    FilterDeletionRequest, MAX_FILTER_DELETION_TRACES, UserDeletionScope, eval_trace_ids,
    filter_trace_ids, user_deletion_scope,
};
pub use experiments::{
    ExperimentScore, ExperimentScoreMean, ExperimentTrace, MAX_EXPERIMENT_SCORES,
    MAX_EXPERIMENT_TRACES, MAX_MEAN_EXPERIMENTS, experiment_score_means, experiment_scores,
    experiment_traces,
};
pub use facets::{
    FacetData, FacetKind, FacetRequest, FacetValue, FacetView, HISTOGRAM_BUCKETS, Histogram,
    HistogramBucket, MAX_FACET_KEY, MAX_FACET_LIMIT, MAX_FACET_QUERY, facet,
};
pub use prompts::{
    MAX_PROMPT_VERSIONS, PromptMetricsRequest, PromptScoreMean, PromptVersionMetrics,
    prompt_metrics,
};
pub use scores::{
    LATEST_SCORE_COLUMNS, ScoreListRequest, ScoreTargets, latest_scores, score_list, scores_by_id,
};
pub use sessions::{
    MAX_SESSION_TURNS, SESSION_ERROR_TRACE_COUNT, SESSION_FIRST_INPUT_PREVIEW,
    SESSION_LAST_OUTPUT_PREVIEW, SessionDetailData, SessionExtrasRequest, SessionListRequest,
    SessionSort, session_detail, session_extras, session_list,
};
pub use traces::{
    MAX_MATCH_COUNT, MAX_REFRESH_IDS, MAX_REFRESH_SPANS_PER_TRACE, MAX_TRACE_SPANS,
    MatchCountRequest, SPAN_LIST_COLUMNS, SPAN_REFRESH_COLUMNS, SpanListRequest,
    SpanRefreshRequest, SpanSort, TRACE_REFRESH_STRICT_COLUMNS, TraceDetailData, TraceListRequest,
    TraceRefreshRequest, TraceSort, TraceSpansData, TraceSpansRequest, match_count, span_detail,
    span_list, span_refresh, trace_detail, trace_list, trace_list_plan, trace_refresh, trace_spans,
};
pub use users::{
    ACTIVITY_BUCKETS, ERROR_TRACE_COUNT, MAX_USER_METADATA_KEYS, MAX_USER_SESSIONS, USER_COLUMNS,
    UserDetailData, UserExtras, UserExtrasRequest, UserListRequest, UserSort, user_detail,
    user_extras, user_list,
};
pub use widgets::{
    Agg, Breakdown, DEFAULT_BREAKDOWN_LIMIT, FieldError, GroupBy, MAX_BREAKDOWN_LIMIT, MAX_MAP_KEY,
    MAX_MEASURES, Measure, Shape, TotalRow, WidgetData, WidgetError, WidgetPoint, WidgetQuery,
    WidgetRequest, WidgetSeries, WidgetSource, validate_expressions, validate_shape, widget_query,
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
/// [`RunError::InvalidRequest`] for a view other than traces, spans, sessions or scores.
pub fn validate_filter(ctx: &QueryContext, view: ViewName, text: &str) -> Result<(), RunError> {
    let view = match view {
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
    }

    #[test]
    fn validate_filter_accepts_sessions_and_scores() {
        let ctx = QueryContext::new(example_query(), None).expect("context");
        assert!(validate_filter(&ctx, ViewName::Sessions, "trace_count > 1").is_ok());
        assert!(validate_filter(&ctx, ViewName::Scores, "name = 'x'").is_ok());
        for view in [ViewName::Sessions, ViewName::Scores] {
            match validate_filter(&ctx, view, "no_such_column > 0") {
                Err(RunError::Invalid(error)) => assert!(error.position.is_some(), "{view:?}"),
                other => panic!("{view:?}: {other:?}"),
            }
        }
    }

    #[test]
    fn validate_filter_refuses_other_views() {
        let ctx = QueryContext::new(example_query(), None).expect("context");
        match validate_filter(&ctx, ViewName::ExperimentItems, "name = 'x'") {
            Err(RunError::InvalidRequest(message)) => assert_eq!(
                message,
                "filters are checked for traces, spans, sessions and scores only"
            ),
            other => panic!("{other:?}"),
        }
    }
}
