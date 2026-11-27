//! Queries behind the read endpoints (D92).
//!
//! Each function takes a [`QueryContext`] and a request and runs under the
//! request's timeout. Lists return `cetra_column::Batch` batches in view-column order.

mod alerts;
mod buckets;
pub(crate) mod columns;
pub(crate) mod context;
mod cursor;
mod deletions;
mod experiments;
mod facets;
mod prompts;
mod scope;
mod scores;
pub(crate) mod scores_join;
mod sessions;
mod targets;
mod traces;
mod users;
mod widgets;

use crate::views::TimeWindow;

pub use alerts::{AlertMeasure, AlertValue, AlertValueRequest, alert_value};
pub use buckets::{Bucket, BucketWidth};
pub use cursor::Page;
pub use deletions::{
    FilterDeletionRequest, MAX_FILTER_DELETION_TRACES, UserDeletionScope, eval_trace_ids,
    filter_trace_ids, session_ids_of_traces, user_deletion_scope,
};
pub use experiments::{
    ExperimentScore, ExperimentScoreMean, ExperimentTrace, MAX_EXPERIMENT_SCORES,
    MAX_EXPERIMENT_TRACES, MAX_MEAN_EXPERIMENTS, experiment_score_means, experiment_scores,
    experiment_traces,
};
pub use facets::{
    FacetData, FacetField, FacetKind, FacetValue, FacetView, FacetsRequest, HISTOGRAM_BUCKETS,
    Histogram, HistogramBucket, MAX_FACET_FIELDS, MAX_FACET_KEY, MAX_FACET_LIMIT, MAX_FACET_QUERY,
    facets,
};
pub use prompts::{
    MAX_PROMPT_VERSIONS, PromptMetricsRequest, PromptScoreMean, PromptVersionMetrics,
    prompt_metrics,
};
pub use scores::{
    LATEST_SCORE_COLUMNS, MAX_COUNT_NAMES, ScoreListRequest, ScoreNameCount,
    ScoreNameCountsRequest, ScoreTargets, latest_scores, score_list, score_name_counts,
    scores_by_id,
};
pub use scores_join::validate_filter;
pub use sessions::{
    MAX_SESSION_TURNS, SESSION_ERROR_TRACE_COUNT, SESSION_FIRST_INPUT_PREVIEW,
    SESSION_LAST_OUTPUT_PREVIEW, SessionDetailData, SessionExtrasRequest, SessionListRequest,
    SessionSort, session_detail, session_extras, session_list,
};
pub use targets::{
    FilteredTargets, MAX_TARGET_IDS, SessionTarget, SessionTargetFilter, SpanTarget,
    SpanTargetFilter, TraceTarget, filter_session_targets, filter_span_targets, session_targets,
    span_targets, trace_targets,
};
pub use traces::{
    ERROR_SPAN_COLUMNS, ErrorSpansRequest, MAX_ERROR_SPANS_PER_TRACE, MAX_MATCH_COUNT,
    MAX_REFRESH_IDS, MAX_REFRESH_SPANS_PER_TRACE, MAX_TRACE_SPANS, MatchCountRequest,
    SPAN_LIST_COLUMNS, SPAN_REFRESH_COLUMNS, SpanListRequest, SpanRefreshRequest, SpanSort,
    TRACE_REFRESH_STRICT_COLUMNS, TraceDetailData, TraceListRequest, TraceRefreshRequest,
    TraceSort, TraceSpansData, TraceSpansRequest, TraceTotals, TraceTotalsRequest, error_spans,
    match_count, span_detail, span_list, span_refresh, trace_detail, trace_list, trace_list_plan,
    trace_refresh, trace_spans, trace_totals,
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

pub(crate) use context::EngineContext;

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

/// Most rows one list page returns.
pub const MAX_PAGE: usize = 1000;

/// Empty or all-whitespace text is no filter and no search.
pub(crate) fn present(text: Option<&str>) -> Option<&str> {
    text.filter(|text| !text.chars().all(char::is_whitespace))
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
