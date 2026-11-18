//! Widget queries of custom dashboards (spec 05 § Dashboard queries).
//!
//! This module holds the request types and their checks. [`validate_shape`]
//! checks the fields without touching the store. [`validate_expressions`] parses
//! the filters against their views without running them.

use cetra_core::schema::{scores, spans, traces};

use super::present;
use crate::filter::{Filter, FilterView};
use crate::query_error::{InvalidQuery, RunError};
use crate::views::QueryContext;

/// Most measures in one widget query.
pub const MAX_MEASURES: usize = 6;
/// Largest breakdown `limit`.
pub const MAX_BREAKDOWN_LIMIT: usize = 20;
/// Breakdown `limit` when the request leaves it out. The caller applies it.
pub const DEFAULT_BREAKDOWN_LIMIT: usize = 5;
/// Longest map key of a `metadata['k']`-style field, in characters.
pub const MAX_MAP_KEY: usize = 256;

/// Columns that count distinct values of a list field (`tags`, `models`) are refused.
const LIST_FIELDS: [&str; 2] = [traces::TAGS, "models"];

/// Identifier columns that `count_distinct` accepts besides the breakdown fields. Traces,
/// spans and scores all have them.
const IDENTITY_FIELDS: [&str; 3] = [traces::TRACE_ID, traces::SESSION_ID, traces::USER_ID];

/// The view a widget query reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WidgetSource {
    /// The `traces` view.
    Traces,
    /// The `spans` view.
    Spans,
    /// The live rows of the `scores` view.
    Scores,
}

/// How a measure aggregates its rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Agg {
    /// Number of rows.
    Count,
    /// Number of distinct non-null values of `field`.
    CountDistinct,
    /// Sum of `field`.
    Sum,
    /// Mean of `field`.
    Avg,
    /// Smallest `field`.
    Min,
    /// Largest `field`.
    Max,
    /// Approximate 50th percentile of `field`.
    P50,
    /// Approximate 75th percentile of `field`.
    P75,
    /// Approximate 90th percentile of `field`.
    P90,
    /// Approximate 95th percentile of `field`.
    P95,
    /// Approximate 99th percentile of `field`.
    P99,
    /// Share of the rows that match `condition`, from 0 to 1.
    Rate,
}

/// One measure of a widget query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Measure {
    /// How the rows aggregate.
    pub agg: Agg,
    /// The field aggregated. Absent for `count` and `rate`.
    pub field: Option<String>,
    /// A filter expression over the source view (the spec's `where`). Required for
    /// `rate`, optional for the others.
    pub condition: Option<String>,
}

/// The split of a widget query by one field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Breakdown {
    /// The field the rows are grouped by.
    pub field: String,
    /// Number of groups kept, 1 to 20.
    pub limit: usize,
    /// Whether the rows of every other value form one more group.
    pub other: bool,
}

/// A builder query: one source, its measures and an optional breakdown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WidgetQuery {
    /// The view the query reads.
    pub source: WidgetSource,
    /// A filter expression over the source view. Blank means none.
    pub filter: Option<String>,
    /// One to six measures.
    pub measures: Vec<Measure>,
    /// The split, if any.
    pub breakdown: Option<Breakdown>,
}

/// Column the rows of a series are split by, for the `group_by` request parameter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupBy {
    /// One row per element of `models`.
    Model,
    /// The `environment` column.
    Environment,
    /// The `release` column.
    Release,
}

/// One invalid field of a widget query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldError {
    /// Path of the field, relative to the query, such as `measures[1].field`.
    pub path: String,
    /// Short message for the caller. It names no values.
    pub message: String,
}

/// A widget query that cannot be answered.
#[derive(Debug, thiserror::Error)]
pub enum WidgetError {
    /// One or more fields are invalid. Every error is listed, in path order.
    #[error("invalid widget query")]
    Fields(Vec<FieldError>),
    /// A filter expression does not parse or names an unknown column.
    #[error("invalid filter")]
    Expression {
        /// Path of the expression, such as `filter` or `measures[0].condition`.
        path: String,
        /// The parse or planning error, with its position.
        #[source]
        error: InvalidQuery,
    },
    /// The query could not be run.
    #[error(transparent)]
    Run(#[from] RunError),
}

/// The fields of one source, as spec 05 lists them.
struct SourceFields {
    numeric: &'static [&'static str],
    breakdown: &'static [&'static str],
    maps: &'static [&'static str],
}

/// `duration_ns` is a derived column of the `traces` view (spec 05 § Logical views).
const DURATION_NS: &str = "duration_ns";

const TRACES: SourceFields = SourceFields {
    numeric: &[
        DURATION_NS,
        traces::SPAN_COUNT,
        traces::ERROR_COUNT,
        traces::LLM_COUNT,
        traces::INPUT_TOKENS,
        traces::OUTPUT_TOKENS,
        traces::CACHE_READ_TOKENS,
        traces::CACHE_WRITE_TOKENS,
        traces::REASONING_TOKENS,
        traces::TOTAL_TOKENS,
        traces::COST_TOTAL,
    ],
    breakdown: &[
        traces::NAME,
        traces::STATUS,
        traces::ENVIRONMENT,
        traces::RELEASE,
        traces::USER_ID,
        traces::TAGS,
        traces::MODELS,
        traces::METADATA,
    ],
    maps: &[traces::METADATA],
};

const SPANS: SourceFields = SourceFields {
    numeric: &[
        spans::DURATION_NS,
        spans::TTFT_NS,
        spans::INPUT_TOKENS,
        spans::OUTPUT_TOKENS,
        spans::CACHE_READ_TOKENS,
        spans::CACHE_WRITE_TOKENS,
        spans::REASONING_TOKENS,
        spans::TOTAL_TOKENS,
        spans::COST_INPUT,
        spans::COST_OUTPUT,
        spans::COST_TOTAL,
    ],
    breakdown: &[
        spans::NAME,
        spans::SPAN_KIND,
        spans::STATUS,
        spans::SERVICE_NAME,
        spans::ENVIRONMENT,
        spans::RELEASE,
        spans::PROVIDER,
        spans::MODEL,
        spans::OPERATION,
        spans::USER_ID,
        spans::TAGS,
        spans::METADATA,
        spans::ATTRIBUTES,
        spans::RESOURCE_ATTRIBUTES,
    ],
    maps: &[
        spans::METADATA,
        spans::ATTRIBUTES,
        spans::RESOURCE_ATTRIBUTES,
    ],
};

const SCORES: SourceFields = SourceFields {
    numeric: &[scores::VALUE_NUM],
    breakdown: &[
        scores::NAME,
        scores::SOURCE,
        scores::DATA_TYPE,
        scores::TARGET,
        scores::VALUE_STR,
        scores::METADATA,
    ],
    maps: &[scores::METADATA],
};

fn fields(source: WidgetSource) -> &'static SourceFields {
    match source {
        WidgetSource::Traces => &TRACES,
        WidgetSource::Spans => &SPANS,
        WidgetSource::Scores => &SCORES,
    }
}

fn view(source: WidgetSource) -> FilterView {
    match source {
        WidgetSource::Traces => FilterView::Traces,
        WidgetSource::Spans => FilterView::Spans,
        WidgetSource::Scores => FilterView::Scores,
    }
}

/// `column['key']` where `column` is one of `maps` and the key has 1 to 256 characters
/// and no quote.
fn is_map_key(maps: &[&str], field: &str) -> bool {
    let Some(rest) = field.strip_suffix("']") else {
        return false;
    };
    let Some((column, key)) = rest.split_once("['") else {
        return false;
    };
    maps.contains(&column)
        && (1..=MAX_MAP_KEY).contains(&key.chars().count())
        && !key.contains('\'')
}

fn is_numeric(table: &SourceFields, field: &str) -> bool {
    table.numeric.contains(&field)
}

fn is_breakdown(table: &SourceFields, field: &str) -> bool {
    table.breakdown.contains(&field) || is_map_key(table.maps, field)
}

fn is_distinct(table: &SourceFields, field: &str) -> bool {
    IDENTITY_FIELDS.contains(&field)
        || (is_breakdown(table, field) && !LIST_FIELDS.contains(&field))
}

fn error(path: impl Into<String>, message: &str) -> FieldError {
    FieldError {
        path: path.into(),
        message: message.to_owned(),
    }
}

/// Checks the fields of `query` without running it. Blank filters and conditions count as
/// absent.
///
/// Every error is returned, in path order: `measures`, then each measure's `field` and
/// `condition` by index, then `breakdown.field` and `breakdown.limit`.
///
/// # Errors
///
/// [`WidgetError::Fields`] with one entry per invalid field.
pub fn validate_shape(query: &WidgetQuery) -> Result<(), WidgetError> {
    let table = fields(query.source);
    let mut errors = Vec::new();
    if !(1..=MAX_MEASURES).contains(&query.measures.len()) {
        errors.push(error("measures", "must have 1 to 6 measures"));
    }
    for (index, measure) in query.measures.iter().enumerate() {
        check_measure(table, measure, index, &mut errors);
    }
    if let Some(breakdown) = &query.breakdown {
        if !is_breakdown(table, &breakdown.field) {
            errors.push(error(
                "breakdown.field",
                "not a breakdown field of this source",
            ));
        }
        if !(1..=MAX_BREAKDOWN_LIMIT).contains(&breakdown.limit) {
            errors.push(error("breakdown.limit", "must be 1 to 20"));
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(WidgetError::Fields(errors))
    }
}

fn check_measure(
    table: &SourceFields,
    measure: &Measure,
    index: usize,
    errors: &mut Vec<FieldError>,
) {
    let field_path = format!("measures[{index}].field");
    let condition_path = format!("measures[{index}].condition");
    let has_condition = present(measure.condition.as_deref()).is_some();
    match measure.agg {
        Agg::Count => {
            if measure.field.is_some() {
                errors.push(error(field_path, "count takes no field"));
            }
        }
        Agg::Rate => {
            if measure.field.is_some() {
                errors.push(error(field_path, "rate takes no field"));
            }
            if !has_condition {
                errors.push(error(condition_path, "rate needs a condition"));
            }
        }
        Agg::CountDistinct => match measure.field.as_deref() {
            None => errors.push(error(field_path, "count_distinct needs a field")),
            Some(field) if !is_distinct(table, field) => {
                errors.push(error(field_path, "count_distinct does not take this field"));
            }
            Some(_) => {}
        },
        Agg::Sum
        | Agg::Avg
        | Agg::Min
        | Agg::Max
        | Agg::P50
        | Agg::P75
        | Agg::P90
        | Agg::P95
        | Agg::P99 => match measure.field.as_deref() {
            None => errors.push(error(field_path, "needs a numeric field")),
            Some(field) if !is_numeric(table, field) => {
                errors.push(error(field_path, "not a numeric field of this source"));
            }
            Some(_) => {}
        },
    }
}

/// Parses each filter of `query` against its view, without running it. The order is the
/// dashboard filter (a traces expression), the widget filter, then each measure's
/// condition by index. Score functions are allowed in the first two, not in a condition.
///
/// # Errors
///
/// [`WidgetError::Expression`] for the first expression that fails, with its path and the
/// position of the problem.
pub fn validate_expressions(
    ctx: &QueryContext,
    query: &WidgetQuery,
    dashboard_filter: Option<&str>,
) -> Result<(), WidgetError> {
    if let Some(text) = present(dashboard_filter) {
        expression(ctx, FilterView::Traces, "dashboard_filter", text)?;
    }
    if let Some(text) = present(query.filter.as_deref()) {
        expression(ctx, view(query.source), "filter", text)?;
    }
    for (index, measure) in query.measures.iter().enumerate() {
        let Some(text) = present(measure.condition.as_deref()) else {
            continue;
        };
        let path = format!("measures[{index}].condition");
        let filter = expression(ctx, view(query.source), &path, text)?;
        if !filter.scores.is_empty() {
            return Err(WidgetError::Expression {
                path,
                error: InvalidQuery {
                    message: "a measure condition cannot call score functions".to_owned(),
                    position: None,
                },
            });
        }
    }
    Ok(())
}

fn expression(
    ctx: &QueryContext,
    view: FilterView,
    path: &str,
    text: &str,
) -> Result<Filter, WidgetError> {
    ctx.parse_filter(view, text)
        .map_err(|error| WidgetError::Expression {
            path: path.to_owned(),
            error,
        })
}

#[cfg(test)]
mod tests {
    use super::{
        Agg, Breakdown, Measure, WidgetError, WidgetQuery, WidgetSource, validate_expressions,
        validate_shape,
    };
    use crate::test_support::example_query;
    use crate::views::QueryContext;

    fn measure(agg: Agg, field: Option<&str>, condition: Option<&str>) -> Measure {
        Measure {
            agg,
            field: field.map(str::to_owned),
            condition: condition.map(str::to_owned),
        }
    }

    fn query(source: WidgetSource, measures: Vec<Measure>) -> WidgetQuery {
        WidgetQuery {
            source,
            filter: None,
            measures,
            breakdown: None,
        }
    }

    fn breakdown(field: &str, limit: usize) -> Option<Breakdown> {
        Some(Breakdown {
            field: field.to_owned(),
            limit,
            other: true,
        })
    }

    /// The paths of a failed shape check, in order.
    fn paths(query: &WidgetQuery) -> Vec<String> {
        match validate_shape(query) {
            Err(WidgetError::Fields(errors)) => errors.into_iter().map(|e| e.path).collect(),
            other => panic!("expected field errors, got {other:?}"),
        }
    }

    fn one_path(query: &WidgetQuery) -> String {
        let found = paths(query);
        assert_eq!(found.len(), 1, "{found:?}");
        found.into_iter().next().expect("one path")
    }

    fn map_query(source: WidgetSource, field: &str, agg: Agg) -> WidgetQuery {
        query(source, vec![measure(agg, Some(field), None)])
    }

    #[test]
    fn measures_need_one_to_six() {
        assert_eq!(paths(&query(WidgetSource::Traces, vec![])), ["measures"]);
        let seven = vec![measure(Agg::Count, None, None); 7];
        assert_eq!(paths(&query(WidgetSource::Traces, seven)), ["measures"]);
        let six = vec![measure(Agg::Count, None, None); 6];
        assert!(validate_shape(&query(WidgetSource::Traces, six)).is_ok());
    }

    #[test]
    fn count_takes_no_field() {
        let bad = query(
            WidgetSource::Traces,
            vec![measure(Agg::Count, Some("duration_ns"), None)],
        );
        assert_eq!(one_path(&bad), "measures[0].field");
        match validate_shape(&bad) {
            Err(WidgetError::Fields(errors)) => {
                assert_eq!(errors[0].message, "count takes no field");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn rate_takes_no_field_and_needs_a_condition() {
        let with_field = query(
            WidgetSource::Traces,
            vec![measure(Agg::Rate, Some("status"), Some("status = 'error'"))],
        );
        assert_eq!(one_path(&with_field), "measures[0].field");
        let without = query(WidgetSource::Traces, vec![measure(Agg::Rate, None, None)]);
        assert_eq!(one_path(&without), "measures[0].condition");
        let blank = query(
            WidgetSource::Traces,
            vec![measure(Agg::Rate, None, Some("  "))],
        );
        assert_eq!(one_path(&blank), "measures[0].condition");
        let valid = query(
            WidgetSource::Traces,
            vec![measure(Agg::Rate, None, Some("status = 'error'"))],
        );
        assert!(validate_shape(&valid).is_ok());
    }

    #[test]
    fn numeric_aggregates_need_a_numeric_field() {
        for agg in [
            Agg::Sum,
            Agg::Avg,
            Agg::Min,
            Agg::Max,
            Agg::P50,
            Agg::P75,
            Agg::P90,
            Agg::P95,
            Agg::P99,
        ] {
            let missing = query(WidgetSource::Traces, vec![measure(agg, None, None)]);
            assert_eq!(one_path(&missing), "measures[0].field", "{agg:?}");
            let text = query(WidgetSource::Traces, vec![measure(agg, Some("name"), None)]);
            assert_eq!(one_path(&text), "measures[0].field", "{agg:?}");
            let ok = query(
                WidgetSource::Traces,
                vec![measure(agg, Some("duration_ns"), None)],
            );
            assert!(validate_shape(&ok).is_ok(), "{agg:?}");
        }
        let scores = query(
            WidgetSource::Scores,
            vec![measure(Agg::Avg, Some("value_num"), None)],
        );
        assert!(validate_shape(&scores).is_ok());
        let span_only = query(
            WidgetSource::Traces,
            vec![measure(Agg::Sum, Some("ttft_ns"), None)],
        );
        assert_eq!(one_path(&span_only), "measures[0].field");
    }

    #[test]
    fn count_distinct_refuses_list_fields_and_accepts_identities() {
        let missing = query(
            WidgetSource::Spans,
            vec![measure(Agg::CountDistinct, None, None)],
        );
        assert_eq!(one_path(&missing), "measures[0].field");
        for field in ["tags", "models", "duration_ns", "value_num"] {
            let bad = query(
                WidgetSource::Traces,
                vec![measure(Agg::CountDistinct, Some(field), None)],
            );
            assert_eq!(one_path(&bad), "measures[0].field", "{field}");
        }
        for source in [
            WidgetSource::Traces,
            WidgetSource::Spans,
            WidgetSource::Scores,
        ] {
            for field in ["trace_id", "session_id", "user_id", "name"] {
                let ok = query(source, vec![measure(Agg::CountDistinct, Some(field), None)]);
                assert!(validate_shape(&ok).is_ok(), "{source:?} {field}");
            }
        }
        let spans_tags = query(
            WidgetSource::Spans,
            vec![measure(Agg::CountDistinct, Some("tags"), None)],
        );
        assert_eq!(one_path(&spans_tags), "measures[0].field");
    }

    #[test]
    fn breakdown_field_and_limit_are_checked() {
        let mut bad_field = query(WidgetSource::Traces, vec![measure(Agg::Count, None, None)]);
        bad_field.breakdown = breakdown("duration_ns", 5);
        assert_eq!(one_path(&bad_field), "breakdown.field");
        for limit in [0, 21] {
            let mut bad = query(WidgetSource::Traces, vec![measure(Agg::Count, None, None)]);
            bad.breakdown = breakdown("name", limit);
            assert_eq!(one_path(&bad), "breakdown.limit", "{limit}");
        }
        for limit in [1, 20] {
            let mut ok = query(WidgetSource::Traces, vec![measure(Agg::Count, None, None)]);
            ok.breakdown = breakdown("name", limit);
            assert!(validate_shape(&ok).is_ok(), "{limit}");
        }
    }

    #[test]
    fn a_valid_query_per_source_passes() {
        let mut traces = query(
            WidgetSource::Traces,
            vec![
                measure(Agg::Count, None, None),
                measure(Agg::P95, Some("duration_ns"), None),
            ],
        );
        traces.breakdown = breakdown("models", 5);
        assert!(validate_shape(&traces).is_ok());
        let mut spans = query(
            WidgetSource::Spans,
            vec![measure(Agg::Sum, Some("cost_total"), None)],
        );
        spans.breakdown = breakdown("model", 20);
        assert!(validate_shape(&spans).is_ok());
        let scores = query(
            WidgetSource::Scores,
            vec![measure(Agg::Count, None, Some("name = 'x'"))],
        );
        assert!(validate_shape(&scores).is_ok());
    }

    #[test]
    fn blank_filters_count_as_absent() {
        let mut blank = query(
            WidgetSource::Traces,
            vec![measure(Agg::Count, None, Some("   "))],
        );
        blank.filter = Some(" ".to_owned());
        assert!(validate_shape(&blank).is_ok());
    }

    #[test]
    fn every_error_comes_back_in_path_order() {
        let mut bad = query(
            WidgetSource::Traces,
            vec![
                measure(Agg::Count, Some("name"), None),
                measure(Agg::Sum, None, None),
                measure(Agg::Rate, None, None),
            ],
        );
        bad.breakdown = breakdown("attributes['k']", 0);
        assert_eq!(
            paths(&bad),
            [
                "measures[0].field",
                "measures[1].field",
                "measures[2].condition",
                "breakdown.field",
                "breakdown.limit",
            ]
        );
    }

    #[test]
    fn map_keys_are_checked_for_their_source() {
        let key_256 = format!("metadata['{}']", "k".repeat(256));
        let key_257 = format!("metadata['{}']", "k".repeat(257));
        let ok = map_query(WidgetSource::Traces, &key_256, Agg::CountDistinct);
        assert!(validate_shape(&ok).is_ok());
        let long = map_query(WidgetSource::Traces, &key_257, Agg::CountDistinct);
        assert_eq!(one_path(&long), "measures[0].field");
        for bad in [
            "metadata['']",
            "metadata['a'b']",
            "metadata['a'",
            "metadata[a]",
        ] {
            let q = map_query(WidgetSource::Traces, bad, Agg::CountDistinct);
            assert_eq!(one_path(&q), "measures[0].field", "{bad}");
        }
        let lacking = map_query(WidgetSource::Traces, "attributes['k']", Agg::CountDistinct);
        assert_eq!(one_path(&lacking), "measures[0].field");
        let spans_attr = map_query(WidgetSource::Spans, "attributes['k']", Agg::CountDistinct);
        assert!(validate_shape(&spans_attr).is_ok());
        let numeric = map_query(WidgetSource::Spans, "metadata['k']", Agg::Sum);
        assert_eq!(one_path(&numeric), "measures[0].field");
        let mut scored = query(WidgetSource::Scores, vec![measure(Agg::Count, None, None)]);
        scored.breakdown = breakdown("metadata['k']", 5);
        assert!(validate_shape(&scored).is_ok());
        scored.breakdown = breakdown("attributes['k']", 5);
        assert_eq!(one_path(&scored), "breakdown.field");
    }

    fn context() -> QueryContext {
        QueryContext::new(example_query(), None).expect("context")
    }

    fn with_filter(source: WidgetSource, filter: &str, measures: Vec<Measure>) -> WidgetQuery {
        let mut query = query(source, measures);
        query.filter = Some(filter.to_owned());
        query
    }

    fn count_query() -> Vec<Measure> {
        vec![measure(Agg::Count, None, None)]
    }

    /// The path and position of a failed expression check.
    fn expression_failure(
        result: Result<(), WidgetError>,
    ) -> (String, Option<crate::query_error::Position>) {
        match result {
            Err(WidgetError::Expression { path, error }) => (path, error.position),
            other => panic!("expected an expression error, got {other:?}"),
        }
    }

    #[test]
    fn valid_expressions_pass() {
        let ctx = context();
        let q = with_filter(WidgetSource::Traces, "status = 'error'", count_query());
        assert!(validate_expressions(&ctx, &q, Some("error_count > 0")).is_ok());
        let spans = with_filter(
            WidgetSource::Spans,
            "span_kind = 'llm'",
            vec![measure(Agg::Count, None, Some("model = 'gpt-5'"))],
        );
        assert!(validate_expressions(&ctx, &spans, None).is_ok());
    }

    #[test]
    fn a_broken_widget_filter_names_its_path() {
        let ctx = context();
        let q = with_filter(WidgetSource::Traces, "error_count >", count_query());
        let (path, position) = expression_failure(validate_expressions(&ctx, &q, None));
        assert_eq!(path, "filter");
        assert!(position.is_some());
    }

    #[test]
    fn a_broken_condition_names_its_index() {
        let ctx = context();
        let q = query(
            WidgetSource::Traces,
            vec![
                measure(Agg::Count, None, Some("error_count > 0")),
                measure(Agg::Count, None, None),
                measure(Agg::Count, None, Some("error_count >")),
            ],
        );
        let (path, position) = expression_failure(validate_expressions(&ctx, &q, None));
        assert_eq!(path, "measures[2].condition");
        assert!(position.is_some());
    }

    #[test]
    fn a_broken_dashboard_filter_names_its_path() {
        let ctx = context();
        let q = query(WidgetSource::Traces, count_query());
        let (path, position) =
            expression_failure(validate_expressions(&ctx, &q, Some("error_count >")));
        assert_eq!(path, "dashboard_filter");
        assert!(position.is_some());
    }

    #[test]
    fn a_span_column_fails_on_the_traces_source() {
        let ctx = context();
        let q = with_filter(WidgetSource::Traces, "span_kind = 'llm'", count_query());
        let (path, _) = expression_failure(validate_expressions(&ctx, &q, None));
        assert_eq!(path, "filter");
        let condition = query(
            WidgetSource::Traces,
            vec![measure(Agg::Count, None, Some("span_kind = 'llm'"))],
        );
        let (path, _) = expression_failure(validate_expressions(&ctx, &condition, None));
        assert_eq!(path, "measures[0].condition");
    }

    #[test]
    fn score_functions_are_refused_in_a_condition_only() {
        let ctx = context();
        let condition = query(
            WidgetSource::Traces,
            vec![
                measure(Agg::Count, None, Some("error_count > 0")),
                measure(Agg::Count, None, Some("score('hallucination') >= 0.5")),
            ],
        );
        let (path, position) = expression_failure(validate_expressions(&ctx, &condition, None));
        assert_eq!(path, "measures[1].condition");
        assert_eq!(position, None);
        let widget_filter = with_filter(
            WidgetSource::Traces,
            "score('hallucination') >= 0.5",
            count_query(),
        );
        assert!(validate_expressions(&ctx, &widget_filter, None).is_ok());
        let dashboard = query(WidgetSource::Traces, count_query());
        assert!(
            validate_expressions(&ctx, &dashboard, Some("score('hallucination') >= 0.5")).is_ok()
        );
    }
}
