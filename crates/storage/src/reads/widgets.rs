//! Widget queries of custom dashboards (spec 05 § Dashboard queries).
//!
//! This module holds the request types and their checks. [`validate_shape`]
//! checks the fields without touching the store. [`validate_expressions`] parses
//! the filters against their views without running them.

use std::sync::Arc;
use std::time::Duration;

use arrow::record_batch::RecordBatch;
use cetra_core::enums::{ScoreSource, ScoreTargetKind};
use cetra_core::schema::{scores, spans, traces};
use datafusion::arrow::datatypes::DataType;
use datafusion::common::{NullHandling, ScalarValue, UnnestOptions};
use datafusion::functions_aggregate::expr_fn::{avg, count, count_distinct, max, min, sum};
use datafusion::logical_expr::expr_fn::when;
use datafusion::logical_expr::{Expr, LogicalPlan, SortExpr, cast, col, in_subquery, lit};
use datafusion::prelude::DataFrame;

use super::buckets::{BUCKET, Bucket, BucketWidth, bucket_expr, resolve};
use super::columns::{f64_at, i64_at, internal, percentile_of, require, string_at, time_at};
use super::{ListScope, present, scoped_traces};
use crate::filter::{Filter, FilterView};
use crate::query_error::{InvalidQuery, RunError, classify};
use crate::views::{QueryContext, TimeWindow, exact_cost_sum};

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
        /// Path of the expression, such as `filter` or `measures[0].where`.
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
        traces::SESSION_ID,
        traces::TAGS,
        traces::MODELS,
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
/// `where` by index, then `breakdown.field` and `breakdown.limit`.
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
    let condition_path = format!("measures[{index}].where");
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
                errors.push(error(condition_path, "rate needs a where"));
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
/// `where` by index. Score functions are allowed in the first two, not in a `where`.
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
        let path = format!("measures[{index}].where");
        let filter = expression(ctx, view(query.source), &path, text)?;
        if !filter.scores.is_empty() {
            return Err(WidgetError::Expression {
                path,
                error: InvalidQuery {
                    message: "a where cannot call score functions".to_owned(),
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

/// The shape of a widget answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    /// One value per bucket.
    Series,
    /// One value over `[from, to)`.
    Total,
}

/// A widget query to run over one window.
#[derive(Debug, Clone, Copy)]
pub struct WidgetRequest<'a> {
    /// `[from, to)`. The time column is `start_time`, or `created_at` for scores.
    pub window: TimeWindow,
    /// Bucket width. Ignored for [`Shape::Total`].
    pub bucket: Bucket,
    /// The dashboard's filter, a traces expression. Blank means none.
    pub dashboard_filter: Option<&'a str>,
    /// The dashboard's split. Ignored with a breakdown, and for scores.
    pub group_by: Option<GroupBy>,
    /// Keep rows whose environment is the eval environment.
    pub include_eval: bool,
    /// Keep rows that belong to an experiment.
    pub include_experiments: bool,
    /// The widget's source, filter, measures and breakdown.
    pub query: &'a WidgetQuery,
    /// One value per bucket, or one value over the window.
    pub shape: Shape,
    /// [`Shape::Total`] only: also compute the values over the previous range of the same
    /// length.
    pub compare: bool,
    /// Time allowed for planning and running.
    pub timeout: Duration,
}

/// One bucket of a series.
#[derive(Debug, Clone, PartialEq)]
pub struct WidgetPoint {
    /// Bucket start, nanoseconds since the Unix epoch, UTC.
    pub time: i64,
    /// Absent when the aggregate is null over the bucket's rows.
    pub value: Option<f64>,
    /// Source rows of the group in the bucket.
    pub rows: u64,
}

/// One measure of one group over the buckets.
#[derive(Debug, Clone, PartialEq)]
pub struct WidgetSeries {
    /// Index of the measure in the query.
    pub measure: usize,
    /// The breakdown value. Absent without a breakdown, and for the other group.
    pub group: Option<String>,
    /// Whether this is the group of every other value.
    pub other: bool,
    /// One point per bucket with rows, in time order.
    pub points: Vec<WidgetPoint>,
}

/// One group of a total, with every measure.
#[derive(Debug, Clone, PartialEq)]
pub struct TotalRow {
    /// The breakdown value. Absent without a breakdown, and for the other group.
    pub group: Option<String>,
    /// Whether this is the group of every other value.
    pub other: bool,
    /// The value of each measure, in query order. `None` when no row counted.
    pub values: Vec<Option<f64>>,
    /// With `compare`, the same values over the previous range of the same length.
    pub previous: Option<Vec<Option<f64>>>,
}

/// A widget answer.
#[derive(Debug, Clone, PartialEq)]
pub enum WidgetData {
    /// [`Shape::Series`]: the series in breakdown order (largest first, other last), then
    /// measure order.
    Series {
        /// Width of every bucket, nanoseconds.
        bucket_ns: i64,
        /// The series.
        series: Vec<WidgetSeries>,
    },
    /// [`Shape::Total`]: one row per group, in breakdown order (other last). One row
    /// without a breakdown.
    Total {
        /// The rows.
        rows: Vec<TotalRow>,
    },
}

/// Column of the row count of every aggregate.
const ROWS: &str = "rows";
/// Column of the group value of an aggregate.
const GROUP: &str = "group_key";
/// Column of a breakdown's value over the narrowed rows. A list field is unnested from it.
const GROUP_INPUT: &str = "group_input";
/// Groups kept when `group_by` splits a query.
const GROUP_BY_LIMIT: usize = 20;
/// Cost columns, summed exactly (D-A2-4).
const COST_FIELDS: [&str; 3] = [spans::COST_INPUT, spans::COST_OUTPUT, spans::COST_TOTAL];

/// Answers a widget query over `request.window`.
///
/// # Errors
///
/// [`WidgetError::Fields`] and [`WidgetError::Expression`] before any query runs;
/// [`WidgetError::Run`] for a bad window or bucket count, a failed query, or a query that runs
/// past `request.timeout` ([`RunError::Timeout`]).
pub async fn widget_query(
    ctx: &QueryContext,
    request: WidgetRequest<'_>,
) -> Result<WidgetData, WidgetError> {
    validate_shape(request.query)?;
    validate_expressions(ctx, request.query, request.dashboard_filter)?;
    match tokio::time::timeout(request.timeout, compute(ctx, request)).await {
        Err(_) => Err(WidgetError::Run(RunError::Timeout)),
        Ok(answer) => Ok(answer?),
    }
}

async fn compute(ctx: &QueryContext, request: WidgetRequest<'_>) -> Result<WidgetData, RunError> {
    let width = match request.shape {
        Shape::Series => Some(resolve(request.window, request.bucket)?),
        Shape::Total => None,
    };
    let (filters, conditions) = parse(ctx, &request)?;
    let base = base(ctx, &request, filters, request.window).await?;
    let plan = Plan::new(&request, &base, &conditions, width)?;
    let rows = plan.project(base)?;
    match width {
        Some(width) => series(ctx, &plan, &rows, width).await,
        None => total(ctx, &request, &plan, &rows).await,
    }
}

/// The filters of a request. Applying a filter consumes it, so each window parses its own.
struct Filters {
    dashboard: Option<Filter>,
    widget: Option<Filter>,
    /// True when the dashboard filter, the widget filter or a condition names `environment`.
    references_environment: bool,
}

/// Parses the filters of `request`, and the expression of each measure's condition (by
/// measure index). Filters were checked by [`validate_expressions`].
fn parse(
    ctx: &QueryContext,
    request: &WidgetRequest<'_>,
) -> Result<(Filters, Vec<Option<Expr>>), RunError> {
    let query = request.query;
    let dashboard = present(request.dashboard_filter)
        .map(|text| ctx.parse_filter(FilterView::Traces, text))
        .transpose()?;
    let widget = present(query.filter.as_deref())
        .map(|text| ctx.parse_filter(view(query.source), text))
        .transpose()?;
    let mut conditions = Vec::with_capacity(query.measures.len());
    for measure in &query.measures {
        conditions.push(
            present(measure.condition.as_deref())
                .map(|text| ctx.parse_filter(view(query.source), text))
                .transpose()?,
        );
    }
    let references_environment = dashboard
        .iter()
        .chain(widget.iter())
        .chain(conditions.iter().flatten())
        .any(|filter| filter.references_environment);
    let exprs = conditions
        .into_iter()
        .map(|condition| condition.map(|filter| filter.expr))
        .collect();
    Ok((
        Filters {
            dashboard,
            widget,
            references_environment,
        },
        exprs,
    ))
}

/// The rows of the source over `window`, with the filters applied. Traces take the window
/// and the default exclusions, then the dashboard filter. Spans and scores take the window and
/// the trace semi-join of the dashboard filter. Then the widget filter applies to every
/// source.
async fn base(
    ctx: &QueryContext,
    request: &WidgetRequest<'_>,
    filters: Filters,
    window: TimeWindow,
) -> Result<DataFrame, RunError> {
    let Filters {
        dashboard,
        widget,
        references_environment,
    } = filters;
    let mut frame = match request.query.source {
        WidgetSource::Traces => {
            let frame = scoped_traces(
                ctx,
                ListScope {
                    window: Some(window),
                    include_eval: request.include_eval,
                    include_experiments: request.include_experiments,
                },
                references_environment,
            )
            .await?;
            match dashboard {
                Some(filter) => filter.apply(frame, ctx).await.map_err(classify)?,
                None => frame,
            }
        }
        WidgetSource::Spans => {
            let ids = trace_ids(
                ctx,
                window,
                request.dashboard_filter,
                request.include_eval,
                request.include_experiments,
                references_environment,
            )
            .await?;
            let mut predicate = window.predicate(spans::START_TIME);
            if let Some(plan) = ids {
                predicate = predicate.and(in_subquery(col(spans::TRACE_ID), Arc::new(plan)));
            }
            ctx.session()
                .table("spans")
                .await
                .map_err(classify)?
                .filter(predicate)
                .map_err(classify)?
        }
        WidgetSource::Scores => {
            let ids = trace_ids(
                ctx,
                window,
                request.dashboard_filter,
                request.include_eval,
                request.include_experiments,
                references_environment,
            )
            .await?;
            let mut predicate = window
                    .predicate(scores::CREATED_AT)
                    .and(col(scores::TARGET).in_list(
                        vec![
                            lit(ScoreTargetKind::Trace.as_str()),
                            lit(ScoreTargetKind::Span.as_str()),
                        ],
                        false,
                    ))
                    // Issue occurrences never show as score columns or briefs (D-IC-18).
                    .and(col(scores::SOURCE).not_eq(lit(ScoreSource::Issue.as_str())));
            if let Some(plan) = ids {
                predicate = predicate.and(in_subquery(col(scores::TRACE_ID), Arc::new(plan)));
            }
            ctx.session()
                .table("scores")
                .await
                .map_err(classify)?
                .filter(predicate)
                .map_err(classify)?
        }
    };
    if let Some(filter) = widget {
        frame = filter.apply(frame, ctx).await.map_err(classify)?;
    }
    Ok(frame)
}

/// Scoped trace ids for the spans and scores semi-join: traces that start in `[from - 1 day, to)`
/// and end at or after `from`, that pass the default exclusions and the dashboard `filter`.
/// `None` skips the semi-join: the filter is absent and both include flags are on.
/// `references_environment` is OR-ed with the filter's own flag.
pub(crate) async fn trace_ids(
    ctx: &QueryContext,
    window: TimeWindow,
    filter: Option<&str>,
    include_eval: bool,
    include_experiments: bool,
    references_environment: bool,
) -> Result<Option<LogicalPlan>, RunError> {
    if present(filter).is_none() && include_eval && include_experiments {
        return Ok(None);
    }
    let start = window
        .from()
        .checked_sub(BucketWidth::D1.ns())
        .ok_or_else(out_of_range)?;
    let lookback = TimeWindow::new(start, window.to())?;
    let filter = present(filter)
        .map(|text| ctx.parse_filter(FilterView::Traces, text))
        .transpose()?;
    let references_environment = references_environment
        || filter
            .as_ref()
            .is_some_and(|filter| filter.references_environment);
    let mut frame = scoped_traces(
        ctx,
        ListScope {
            window: Some(lookback),
            include_eval,
            include_experiments,
        },
        references_environment,
    )
    .await?;
    frame = frame
        .filter(at_least(traces::END_TIME, window.from()))
        .map_err(classify)?;
    if let Some(filter) = filter {
        frame = filter.apply(frame, ctx).await.map_err(classify)?;
    }
    let frame = frame
        .select(vec![col(traces::TRACE_ID)])
        .map_err(classify)?;
    Ok(Some(frame.logical_plan().clone()))
}

fn at_least(column: &str, ns: i64) -> Expr {
    col(column).gt_eq(Expr::Literal(
        ScalarValue::TimestampNanosecond(Some(ns), Some("UTC".into())),
        None,
    ))
}

fn time_column(source: WidgetSource) -> &'static str {
    match source {
        WidgetSource::Traces => traces::START_TIME,
        WidgetSource::Spans => spans::START_TIME,
        WidgetSource::Scores => scores::CREATED_AT,
    }
}

/// The breakdown field a `group_by` splits by. Scores are not split.
fn group_by_field(source: WidgetSource, group_by: GroupBy) -> Option<&'static str> {
    match (source, group_by) {
        (WidgetSource::Scores, _) => None,
        (WidgetSource::Traces, GroupBy::Model) => Some(traces::MODELS),
        (WidgetSource::Spans, GroupBy::Model) => Some(spans::MODEL),
        (WidgetSource::Traces, GroupBy::Environment) => Some(traces::ENVIRONMENT),
        (WidgetSource::Spans, GroupBy::Environment) => Some(spans::ENVIRONMENT),
        (WidgetSource::Traces, GroupBy::Release) => Some(traces::RELEASE),
        (WidgetSource::Spans, GroupBy::Release) => Some(spans::RELEASE),
    }
}

/// The expression of a field over `frame`: a column, or a map key read the way facets read it.
fn field_expr(frame: &DataFrame, field: &str) -> Result<Expr, RunError> {
    if field.contains("['") {
        frame.parse_sql_expr(field).map_err(classify)
    } else {
        Ok(col(field))
    }
}

/// The aggregate of one measure. Its condition keeps only the rows that match.
fn aggregate_expr(
    measure: &Measure,
    field: Option<&Expr>,
    condition: Option<&Expr>,
) -> Result<Expr, RunError> {
    let masked = |value: Expr| -> Result<Expr, RunError> {
        match condition {
            Some(condition) => when(condition.clone(), value).end().map_err(classify),
            None => Ok(value),
        }
    };
    let value = || {
        field
            .cloned()
            .ok_or_else(|| internal("a measure without a field".to_owned()))
    };
    let cost = measure
        .field
        .as_deref()
        .is_some_and(|name| COST_FIELDS.contains(&name));
    // Every aggregate but a count and the exact cost sum returns Float64, so `f64_at` sees one
    // type. A count stays an integer; `f64_at` reads it too.
    let as_float = |expr: Expr| cast(expr, DataType::Float64);
    Ok(match measure.agg {
        Agg::Count => count(masked(lit(1_i64))?),
        Agg::Rate => {
            if condition.is_none() {
                return Err(internal("a rate without a condition".to_owned()));
            }
            count(masked(lit(1_i64))?)
        }
        Agg::CountDistinct => as_float(count_distinct(masked(value()?)?)),
        Agg::Sum if cost => exact_cost_sum(masked(value()?)?),
        Agg::Sum => as_float(sum(masked(value()?)?)),
        Agg::Avg => as_float(avg(masked(value()?)?)),
        Agg::Min => as_float(min(masked(value()?)?)),
        Agg::Max => as_float(max(masked(value()?)?)),
        Agg::P50 => as_float(percentile_of(masked(value()?)?, 0.5)),
        Agg::P75 => as_float(percentile_of(masked(value()?)?, 0.75)),
        Agg::P90 => as_float(percentile_of(masked(value()?)?, 0.9)),
        Agg::P95 => as_float(percentile_of(masked(value()?)?, 0.95)),
        Agg::P99 => as_float(percentile_of(masked(value()?)?, 0.99)),
    })
}

/// A breakdown field: whether it is a list, and how many groups it keeps.
struct Group {
    /// A list column: each row counts under each of its elements.
    list: bool,
    /// The value of the split: [`GROUP_INPUT`] over the narrowed rows.
    expr: Expr,
    /// The number of groups kept.
    limit: usize,
    /// Whether the other values form one more group.
    other: bool,
}

/// What a request computes, resolved against its base rows.
struct Plan {
    /// The aggregate of each measure, by index, then the row count.
    kinds: Vec<Agg>,
    /// The output column of each measure's aggregate.
    columns: Vec<String>,
    /// One aggregate per measure, aliased as `columns`, then `count(*)` as `ROWS`.
    aggregates: Vec<Expr>,
    /// The columns the aggregates read, computed on the base rows before the unnest: the
    /// bucket, the group input, and each measure's field and `where`.
    inputs: Vec<Expr>,
    /// The bucket of each row, for a series: [`BUCKET`] over the narrowed rows.
    bucket: Option<Expr>,
    /// The split, if any.
    group: Option<Group>,
}

impl Plan {
    fn new(
        request: &WidgetRequest<'_>,
        base: &DataFrame,
        conditions: &[Option<Expr>],
        width: Option<i64>,
    ) -> Result<Self, RunError> {
        let query = request.query;
        let mut inputs = Vec::new();
        if let Some(width) = width {
            inputs.push(bucket_expr(width, time_column(query.source)));
        }
        let mut aggregates = Vec::with_capacity(query.measures.len() + 1);
        let mut columns = Vec::with_capacity(query.measures.len());
        for (index, (measure, condition)) in query.measures.iter().zip(conditions).enumerate() {
            let field = match measure.field.as_deref() {
                Some(name) => {
                    let input = format!("field_{index}");
                    inputs.push(field_expr(base, name)?.alias(input.as_str()));
                    Some(col(input.as_str()))
                }
                None => None,
            };
            let condition = match condition {
                Some(expr) => {
                    let input = format!("where_{index}");
                    inputs.push(expr.clone().alias(input.as_str()));
                    Some(col(input.as_str()))
                }
                None => None,
            };
            let column = format!("m{index}");
            let aggregate = aggregate_expr(measure, field.as_ref(), condition.as_ref())?;
            aggregates.push(aggregate.alias(column.as_str()));
            columns.push(column);
        }
        aggregates.push(count(lit(1_i64)).alias(ROWS));
        let split = match (&query.breakdown, request.group_by) {
            (Some(breakdown), _) => {
                Some((breakdown.field.as_str(), breakdown.limit, breakdown.other))
            }
            (None, Some(group_by)) => {
                group_by_field(query.source, group_by).map(|field| (field, GROUP_BY_LIMIT, false))
            }
            (None, None) => None,
        };
        let group = match split {
            Some((field, limit, other)) => {
                inputs.push(field_expr(base, field)?.alias(GROUP_INPUT));
                Some(Group {
                    list: LIST_FIELDS.contains(&field),
                    expr: col(GROUP_INPUT),
                    limit,
                    other,
                })
            }
            None => None,
        };
        Ok(Self {
            kinds: query.measures.iter().map(|measure| measure.agg).collect(),
            columns,
            aggregates,
            inputs,
            bucket: width.map(|_| col(BUCKET)),
            group,
        })
    }

    /// The base rows narrowed to the columns the aggregates read. The expressions are computed
    /// here, before the unnest of a list breakdown, so a `where` on `tags` or `models` still sees
    /// the list. Wide columns of the base rows are not carried on.
    fn project(&self, base: DataFrame) -> Result<DataFrame, RunError> {
        base.select(self.inputs.clone()).map_err(classify)
    }

    fn bucket(&self) -> Result<Expr, RunError> {
        self.bucket
            .clone()
            .ok_or_else(|| internal("a total has no buckets".to_owned()))
    }

    fn breakdown(&self) -> Result<&Group, RunError> {
        self.group
            .as_ref()
            .ok_or_else(|| internal("the query has no breakdown".to_owned()))
    }

    /// The first measure, descending with nulls last. A rate sorts by its share.
    fn first_sort(&self) -> Result<SortExpr, RunError> {
        let first = self
            .columns
            .first()
            .ok_or_else(|| internal("a query without measures".to_owned()))?;
        let key = match self.kinds.first() {
            Some(Agg::Rate) => {
                cast(col(first.as_str()), DataType::Float64) / cast(col(ROWS), DataType::Float64)
            }
            _ => col(first.as_str()),
        };
        Ok(key.sort(false, false))
    }
}

/// Which rows of a pass count: every row, the rows of the top groups, or the rest.
#[derive(Clone, Copy)]
enum Members<'a> {
    All,
    Top(&'a [String]),
    Rest(&'a [String]),
}

/// The keys of a pass: the bucket, the group, both or neither.
#[derive(Clone, Copy)]
struct Keys {
    time: bool,
    group: bool,
}

const NO_KEYS: Keys = Keys {
    time: false,
    group: false,
};
const BY_GROUP: Keys = Keys {
    time: false,
    group: true,
};
const BY_TIME: Keys = Keys {
    time: true,
    group: false,
};
const BY_TIME_AND_GROUP: Keys = Keys {
    time: true,
    group: true,
};

/// One row of a pass: its keys and the value of every measure.
struct Row {
    time: i64,
    group: Option<String>,
    rows: u64,
    values: Vec<Option<f64>>,
}

/// The aggregate of one pass over `base`, before it is ordered or collected.
fn pass_frame(
    plan: &Plan,
    base: &DataFrame,
    members: Members<'_>,
    keys: Keys,
) -> Result<DataFrame, RunError> {
    let mut frame = base.clone();
    let mut by = Vec::new();
    if keys.time {
        by.push(plan.bucket()?);
    }
    match (&plan.group, members) {
        (None, Members::All) => {}
        (None, _) => return Err(internal("a selection needs a breakdown".to_owned())),
        (Some(group), members) => {
            if group.list {
                frame = frame
                    .unnest_columns_with_options(
                        &[GROUP_INPUT],
                        UnnestOptions::new().with_null_handling(NullHandling::Drop),
                    )
                    .map_err(classify)?;
            }
            frame = frame
                .filter(group.expr.clone().is_not_null())
                .map_err(classify)?;
            let predicate = match members {
                Members::All => None,
                Members::Top(values) => Some(group.expr.clone().in_list(literals(values), false)),
                Members::Rest(values) => Some(group.expr.clone().in_list(literals(values), true)),
            };
            if let Some(predicate) = predicate {
                frame = frame.filter(predicate).map_err(classify)?;
            }
            if keys.group {
                by.push(group.expr.clone().alias(GROUP));
            }
        }
    }
    frame
        .aggregate(by, plan.aggregates.clone())
        .map_err(classify)
}

fn literals(values: &[String]) -> Vec<Expr> {
    values.iter().map(|value| lit(value.as_str())).collect()
}

async fn collect(
    ctx: &QueryContext,
    frame: DataFrame,
    plan: &Plan,
    keys: Keys,
) -> Result<Vec<Row>, RunError> {
    let batches = ctx.collect(frame).await.map_err(|error| ctx.fail(error))?;
    read_rows(&batches, plan, keys)
}

async fn pass(
    ctx: &QueryContext,
    plan: &Plan,
    base: &DataFrame,
    members: Members<'_>,
    keys: Keys,
) -> Result<Vec<Row>, RunError> {
    collect(ctx, pass_frame(plan, base, members, keys)?, plan, keys).await
}

/// The groups of the breakdown, in answer order: the first measure descending with nulls last,
/// then the value ascending, at most the breakdown's limit.
async fn top_groups(
    ctx: &QueryContext,
    plan: &Plan,
    base: &DataFrame,
) -> Result<Vec<Row>, RunError> {
    let limit = plan.breakdown()?.limit;
    let frame = pass_frame(plan, base, Members::All, BY_GROUP)?
        .sort(vec![plan.first_sort()?, col(GROUP).sort(true, false)])
        .map_err(classify)?
        .limit(0, Some(limit))
        .map_err(classify)?;
    collect(ctx, frame, plan, BY_GROUP).await
}

fn group_names(groups: &[Row]) -> Vec<String> {
    groups.iter().filter_map(|row| row.group.clone()).collect()
}

/// The one row of a pass without keys. A pass without keys always answers one row; an empty
/// answer reads as no rows.
fn single(rows: Vec<Row>, plan: &Plan) -> Row {
    rows.into_iter().next().unwrap_or_else(|| Row {
        time: 0,
        group: None,
        rows: 0,
        values: vec![None; plan.kinds.len()],
    })
}

fn read_rows(batches: &[RecordBatch], plan: &Plan, keys: Keys) -> Result<Vec<Row>, RunError> {
    let mut rows = Vec::new();
    for batch in batches {
        if batch.num_rows() == 0 {
            continue;
        }
        let times = if keys.time {
            Some(require(batch, BUCKET)?)
        } else {
            None
        };
        let groups = if keys.group {
            Some(require(batch, GROUP)?)
        } else {
            None
        };
        let counts = require(batch, ROWS)?;
        let measures = plan
            .columns
            .iter()
            .map(|name| require(batch, name))
            .collect::<Result<Vec<_>, _>>()?;
        for index in 0..batch.num_rows() {
            let time = match times {
                Some(column) => time_at(column, index)?,
                None => 0,
            };
            let group = match groups {
                Some(column) => Some(
                    string_at(column, index)?
                        .ok_or_else(|| internal("group key is null".to_owned()))?,
                ),
                None => None,
            };
            let seen = u64::try_from(i64_at(counts, index)?)
                .map_err(|_| internal("row count is negative".to_owned()))?;
            let raw = measures
                .iter()
                .map(|column| f64_at(*column, index))
                .collect::<Result<Vec<_>, _>>()?;
            rows.push(Row {
                time,
                group,
                rows: seen,
                values: finish(&plan.kinds, raw, seen),
            });
        }
    }
    Ok(rows)
}

/// The values of a row. A row with no source rows has no values; a rate is its matches over
/// the row count.
fn finish(kinds: &[Agg], raw: Vec<Option<f64>>, rows: u64) -> Vec<Option<f64>> {
    if rows == 0 {
        return vec![None; kinds.len()];
    }
    kinds
        .iter()
        .zip(raw)
        .map(|(agg, value)| match agg {
            Agg::Rate => value.map(|matched| matched / rows as f64),
            _ => value,
        })
        .collect()
}

async fn series(
    ctx: &QueryContext,
    plan: &Plan,
    base: &DataFrame,
    width: i64,
) -> Result<WidgetData, RunError> {
    let mut lines = Vec::new();
    match &plan.group {
        None => {
            let rows = pass(ctx, plan, base, Members::All, BY_TIME).await?;
            let rows: Vec<&Row> = rows.iter().collect();
            lines.extend(lines_of(plan, None, false, &rows));
        }
        Some(group) => {
            let groups = top_groups(ctx, plan, base).await?;
            let names = group_names(&groups);
            if !names.is_empty() {
                let rows = pass(ctx, plan, base, Members::Top(&names), BY_TIME_AND_GROUP).await?;
                for name in &names {
                    let mine: Vec<&Row> = rows
                        .iter()
                        .filter(|row| row.group.as_deref() == Some(name.as_str()))
                        .collect();
                    lines.extend(lines_of(plan, Some(name.as_str()), false, &mine));
                }
                if group.other {
                    let rest = pass(ctx, plan, base, Members::Rest(&names), BY_TIME).await?;
                    let rest: Vec<&Row> = rest.iter().collect();
                    if !rest.is_empty() {
                        lines.extend(lines_of(plan, None, true, &rest));
                    }
                }
            }
        }
    }
    Ok(WidgetData::Series {
        bucket_ns: width,
        series: lines,
    })
}

/// One series per measure of one group, its points in time order.
fn lines_of(plan: &Plan, group: Option<&str>, other: bool, rows: &[&Row]) -> Vec<WidgetSeries> {
    let mut ordered = rows.to_vec();
    ordered.sort_by_key(|row| row.time);
    (0..plan.kinds.len())
        .map(|measure| WidgetSeries {
            measure,
            group: group.map(str::to_owned),
            other,
            points: ordered
                .iter()
                .map(|row| WidgetPoint {
                    time: row.time,
                    value: row.values.get(measure).copied().flatten(),
                    rows: row.rows,
                })
                .collect(),
        })
        .collect()
}

async fn total(
    ctx: &QueryContext,
    request: &WidgetRequest<'_>,
    plan: &Plan,
    base: &DataFrame,
) -> Result<WidgetData, RunError> {
    let previous = if request.compare {
        let (filters, _) = parse(ctx, request)?;
        Some(plan.project(base_of_previous(ctx, request, filters).await?)?)
    } else {
        None
    };
    let Some(group) = &plan.group else {
        let current = single(pass(ctx, plan, base, Members::All, NO_KEYS).await?, plan);
        let before = match &previous {
            Some(frame) => {
                Some(single(pass(ctx, plan, frame, Members::All, NO_KEYS).await?, plan).values)
            }
            None => None,
        };
        return Ok(WidgetData::Total {
            rows: vec![TotalRow {
                group: None,
                other: false,
                values: current.values,
                previous: before,
            }],
        });
    };
    let groups = top_groups(ctx, plan, base).await?;
    let names = group_names(&groups);
    let before = match &previous {
        Some(frame) if !names.is_empty() => {
            pass(ctx, plan, frame, Members::Top(&names), BY_GROUP).await?
        }
        _ => Vec::new(),
    };
    let mut rows = Vec::with_capacity(groups.len() + 1);
    for row in groups {
        let earlier = before
            .iter()
            .find(|candidate| candidate.group == row.group)
            .map_or_else(
                || vec![None; plan.kinds.len()],
                |found| found.values.clone(),
            );
        rows.push(TotalRow {
            group: row.group,
            other: false,
            values: row.values,
            previous: previous.is_some().then_some(earlier),
        });
    }
    if group.other && !names.is_empty() {
        let current = single(
            pass(ctx, plan, base, Members::Rest(&names), NO_KEYS).await?,
            plan,
        );
        if current.rows > 0 {
            let before = match &previous {
                Some(frame) => Some(
                    single(
                        pass(ctx, plan, frame, Members::Rest(&names), NO_KEYS).await?,
                        plan,
                    )
                    .values,
                ),
                None => None,
            };
            rows.push(TotalRow {
                group: None,
                other: true,
                values: current.values,
                previous: before,
            });
        }
    }
    Ok(WidgetData::Total { rows })
}

/// The base rows of the previous range of the same length as `request.window`.
async fn base_of_previous(
    ctx: &QueryContext,
    request: &WidgetRequest<'_>,
    filters: Filters,
) -> Result<DataFrame, RunError> {
    base(ctx, request, filters, previous_window(request.window)?).await
}

/// The error of a range that reaches past the representable time.
fn out_of_range() -> RunError {
    RunError::InvalidRequest("from is out of range".to_owned())
}

/// The window of the same length just before `window`.
fn previous_window(window: TimeWindow) -> Result<TimeWindow, RunError> {
    let length = window
        .to()
        .checked_sub(window.from())
        .ok_or_else(out_of_range)?;
    let start = window.from().checked_sub(length).ok_or_else(out_of_range)?;
    TimeWindow::new(start, window.from())
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
        assert_eq!(one_path(&without), "measures[0].where");
        let blank = query(
            WidgetSource::Traces,
            vec![measure(Agg::Rate, None, Some("  "))],
        );
        assert_eq!(one_path(&blank), "measures[0].where");
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
        let mut session = query(WidgetSource::Traces, vec![measure(Agg::Count, None, None)]);
        session.breakdown = breakdown("session_id", 5);
        assert!(validate_shape(&session).is_ok());
        let mut spans_session = query(WidgetSource::Spans, vec![measure(Agg::Count, None, None)]);
        spans_session.breakdown = breakdown("session_id", 5);
        match validate_shape(&spans_session) {
            Err(WidgetError::Fields(errors)) => {
                assert_eq!(errors.len(), 1, "{errors:?}");
                assert_eq!(errors[0].path, "breakdown.field");
                assert_eq!(errors[0].message, "not a breakdown field of this source");
            }
            other => panic!("expected field errors, got {other:?}"),
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
                "measures[2].where",
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
        // A bare map column is not a field: only one of its keys is.
        for (source, column) in [
            (WidgetSource::Traces, "metadata"),
            (WidgetSource::Spans, "attributes"),
            (WidgetSource::Spans, "resource_attributes"),
            (WidgetSource::Scores, "metadata"),
        ] {
            let mut bare = query(source, vec![measure(Agg::Count, None, None)]);
            bare.breakdown = breakdown(column, 5);
            assert_eq!(one_path(&bare), "breakdown.field", "{column}");
            let distinct = map_query(source, column, Agg::CountDistinct);
            assert_eq!(one_path(&distinct), "measures[0].field", "{column}");
        }
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
        assert_eq!(path, "measures[2].where");
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
        assert_eq!(path, "measures[0].where");
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
        assert_eq!(path, "measures[1].where");
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

    // Answers: the headline and series values, breakdowns, filters, costs, compare, errors.

    use std::collections::{BTreeMap, BTreeSet};
    use std::num::NonZeroUsize;
    use std::sync::Arc;
    use std::time::Duration;

    use cetra_core::enums::{ScoreSource, SpanKind};
    use cetra_core::names::environment;
    use cetra_core::score::Score;
    use cetra_core::span::Span;
    use cetra_core::time_serde::parse_rfc3339;
    use datafusion::execution::runtime_env::RuntimeEnv;

    use super::{Shape, TotalRow, WidgetData, WidgetRequest, WidgetSeries, widget_query};
    use crate::query_error::RunError;
    use crate::reads::{Bucket, BucketWidth, GroupBy};
    use crate::test_support::{live_scores_table, span_row, spans_table, traces_table};
    use crate::views::{ProjectQuery, TimeWindow};

    const TIMEOUT: Duration = Duration::from_secs(30);
    const MINUTE: i64 = 60_000_000_000;
    const TOLERANCE: f64 = 1e-9;

    fn context_with(spans: &[Span], scores: &[Score]) -> QueryContext {
        QueryContext::new(
            ProjectQuery {
                spans: spans_table(spans),
                traces: traces_table(spans),
                scores: live_scores_table(scores),
                runtime: Arc::new(RuntimeEnv::default()),
                target_partitions: NonZeroUsize::new(2).expect("two partitions"),
                optimizer_rules: Vec::new(),
                suspects: crate::verify::Suspects::detached(),
                visible_from: None,
                executor: crate::test_support::query_runtime(),
            },
            None,
        )
        .expect("context")
    }

    fn context_of(spans: &[Span]) -> QueryContext {
        context_with(spans, &[])
    }

    fn at(text: &str) -> i64 {
        parse_rfc3339(text).unwrap_or_else(|_| panic!("time {text}"))
    }

    fn window(from: &str, to: &str) -> TimeWindow {
        TimeWindow::new(at(from), at(to)).expect("window")
    }

    fn hour() -> TimeWindow {
        window("2026-01-01T10:00:00Z", "2026-01-01T11:00:00Z")
    }

    fn split(field: &str, limit: usize, other: bool) -> Option<Breakdown> {
        Some(Breakdown {
            field: field.to_owned(),
            limit,
            other,
        })
    }

    /// A query over `source` with no filter.
    fn build(
        source: WidgetSource,
        measures: Vec<Measure>,
        breakdown: Option<Breakdown>,
    ) -> WidgetQuery {
        WidgetQuery {
            source,
            filter: None,
            measures,
            breakdown,
        }
    }

    fn filtered(mut query: WidgetQuery, text: &str) -> WidgetQuery {
        query.filter = Some(text.to_owned());
        query
    }

    fn ask(query: &WidgetQuery, window: TimeWindow, shape: Shape) -> WidgetRequest<'_> {
        WidgetRequest {
            window,
            bucket: Bucket::Fixed(BucketWidth::M10),
            dashboard_filter: None,
            group_by: None,
            include_eval: false,
            include_experiments: false,
            query,
            shape,
            compare: false,
            timeout: TIMEOUT,
        }
    }

    async fn answer(ctx: &QueryContext, request: WidgetRequest<'_>) -> WidgetData {
        widget_query(ctx, request)
            .await
            .unwrap_or_else(|error| panic!("{error:?}"))
    }

    fn rows_of(data: &WidgetData) -> &[TotalRow] {
        match data {
            WidgetData::Total { rows } => rows,
            other => panic!("expected totals, got {other:?}"),
        }
    }

    fn series_of(data: &WidgetData) -> &[WidgetSeries] {
        match data {
            WidgetData::Series { series, .. } => series,
            other => panic!("expected series, got {other:?}"),
        }
    }

    fn line<'a>(
        data: &'a WidgetData,
        measure: usize,
        group: Option<&str>,
        other: bool,
    ) -> &'a WidgetSeries {
        series_of(data)
            .iter()
            .find(|line| {
                line.measure == measure && line.group.as_deref() == group && line.other == other
            })
            .unwrap_or_else(|| panic!("no line {measure} {group:?} {other}"))
    }

    fn total_row<'a>(data: &'a WidgetData, group: Option<&str>, other: bool) -> &'a TotalRow {
        rows_of(data)
            .iter()
            .find(|row| row.group.as_deref() == group && row.other == other)
            .unwrap_or_else(|| panic!("no row {group:?} {other}"))
    }

    /// The groups of a series' measure, without the other group.
    fn groups_of(data: &WidgetData, measure: usize) -> BTreeSet<String> {
        series_of(data)
            .iter()
            .filter(|line| line.measure == measure && !line.other)
            .filter_map(|line| line.group.clone())
            .collect()
    }

    fn assert_close(actual: Option<f64>, expected: Option<f64>) {
        match (actual, expected) {
            (None, None) => {}
            (Some(actual), Some(expected)) => {
                assert!(
                    (actual - expected).abs() < TOLERANCE,
                    "{actual} vs {expected}"
                );
            }
            other => panic!("{other:?}"),
        }
    }

    /// The line's value at each ten-minute bucket of the hour, 10:00 to 10:50. A bucket not in
    /// `expected` has no value. Every point must sit on that grid.
    fn assert_line(widget: &WidgetSeries, expected: &[(i64, f64)]) {
        for point in &widget.points {
            assert!(
                (0..60)
                    .step_by(10)
                    .any(|bucket| point.time == minute(bucket)),
                "point {} is off the bucket grid",
                point.time
            );
        }
        for bucket in (0..60).step_by(10) {
            let found = widget
                .points
                .iter()
                .find(|point| point.time == minute(bucket))
                .and_then(|point| point.value);
            let want = expected
                .iter()
                .find(|(at_minute, _)| *at_minute == bucket)
                .map(|(_, value)| *value);
            assert_close(found, want);
        }
    }

    /// The `ScoreAvg` lines: each named score has its expected values, and no other score name
    /// has any value.
    fn assert_scores(data: &WidgetData, expected: &[(&str, &[(i64, f64)])]) {
        let names: BTreeSet<String> = expected
            .iter()
            .map(|(name, _)| (*name).to_owned())
            .collect();
        for (name, points) in expected {
            assert_line(line(data, 0, Some(*name), false), points);
        }
        for name in groups_of(data, 0).difference(&names) {
            assert!(
                line(data, 0, Some(name.as_str()), false)
                    .points
                    .iter()
                    .all(|point| point.value.is_none()),
                "{name} has values that no expected score line has"
            );
        }
    }

    fn span(trace: u16, index: u8, start: i64, model: Option<&str>) -> Span {
        let mut span = span_row(trace, index, start, start);
        span.span_kind = SpanKind::Llm;
        span.environment = Some("production".to_owned());
        span.response_model = model.map(str::to_owned);
        span.end_time = start + 100;
        span
    }

    fn minute(at_minute: i64) -> i64 {
        at("2026-01-01T10:00:00Z") + at_minute * MINUTE
    }

    #[tokio::test]
    async fn trace_series_match_the_fixed_dashboard() {
        let ctx = context();
        let window = hour();
        let counts = build(
            WidgetSource::Traces,
            vec![
                measure(Agg::Count, None, None),
                measure(Agg::Count, None, Some("error_count > 0")),
                measure(Agg::Rate, None, Some("error_count > 0")),
                measure(Agg::P50, Some("duration_ns"), None),
                measure(Agg::P95, Some("duration_ns"), None),
                measure(Agg::P99, Some("duration_ns"), None),
            ],
            None,
        );
        let data = answer(&ctx, ask(&counts, window, Shape::Series)).await;
        assert_line(
            line(&data, 0, None, false),
            &[(0, 2.0), (10, 1.0), (20, 1.0), (30, 1.0)],
        );
        assert_line(
            line(&data, 1, None, false),
            &[(0, 0.0), (10, 1.0), (20, 0.0), (30, 0.0)],
        );
        assert_line(
            line(&data, 2, None, false),
            &[(0, 0.0), (10, 1.0), (20, 0.0), (30, 0.0)],
        );
        let latency_p50 = [
            (0, 3_350_000_000.0),
            (10, 800_000_000.0),
            (20, 3_000_000_000.0),
            (30, 2_000_000_000.0),
        ];
        let latency_p95 = [
            (0, 4_200_000_000.0),
            (10, 800_000_000.0),
            (20, 3_000_000_000.0),
            (30, 2_000_000_000.0),
        ];
        assert_line(line(&data, 3, None, false), &latency_p50);
        assert_line(line(&data, 4, None, false), &latency_p95);
        assert_line(line(&data, 5, None, false), &latency_p95);
        for point in &line(&data, 0, None, false).points {
            assert_eq!(point.value, Some(point.rows as f64));
        }

        let cost = build(
            WidgetSource::Traces,
            vec![measure(Agg::Sum, Some("cost_total"), None)],
            None,
        );
        let data = answer(&ctx, ask(&cost, window, Shape::Series)).await;
        assert_line(
            line(&data, 0, None, false),
            &[(0, 0.00552), (20, 0.00133), (30, 0.0027)],
        );
    }

    #[tokio::test]
    async fn span_series_match_the_fixed_dashboard() {
        let ctx = context();
        let window = hour();
        let tokens = filtered(
            build(
                WidgetSource::Spans,
                vec![
                    measure(Agg::Sum, Some("input_tokens"), None),
                    measure(Agg::Sum, Some("output_tokens"), None),
                    measure(Agg::Sum, Some("cache_read_tokens"), None),
                    measure(Agg::Sum, Some("cache_write_tokens"), None),
                ],
                None,
            ),
            "span_kind = 'llm' OR span_kind = 'embedding'",
        );
        let data = answer(&ctx, ask(&tokens, window, Shape::Series)).await;
        assert_line(
            line(&data, 0, None, false),
            &[(0, 4000.0), (20, 850.0), (30, 400.0)],
        );
        assert_line(
            line(&data, 1, None, false),
            &[(0, 180.0), (20, 120.0), (30, 100.0)],
        );
        assert_line(line(&data, 2, None, false), &[(0, 3400.0)]);
        assert_line(line(&data, 3, None, false), &[]);

        let ttft = filtered(
            build(
                WidgetSource::Spans,
                vec![
                    measure(Agg::P50, Some("ttft_ns"), None),
                    measure(Agg::P95, Some("ttft_ns"), None),
                ],
                None,
            ),
            "span_kind = 'llm' AND ttft_ns IS NOT NULL",
        );
        let data = answer(&ctx, ask(&ttft, window, Shape::Series)).await;
        assert_line(
            line(&data, 0, None, false),
            &[(0, 400_000_000.0), (20, 600_000_000.0)],
        );
        assert_line(
            line(&data, 1, None, false),
            &[(0, 600_000_000.0), (20, 600_000_000.0)],
        );

        let by_model = filtered(
            build(
                WidgetSource::Spans,
                vec![
                    measure(Agg::P50, Some("duration_ns"), None),
                    measure(Agg::P95, Some("duration_ns"), None),
                    measure(Agg::Sum, Some("cost_total"), None),
                ],
                split("model", 20, false),
            ),
            "span_kind = 'llm' AND model IS NOT NULL",
        );
        let data = answer(&ctx, ask(&by_model, window, Shape::Series)).await;
        let models = groups_of(&data, 0);
        let expected_models: BTreeSet<String> = ["claude-sonnet-4-5", "gpt-4o-mini"]
            .into_iter()
            .map(str::to_owned)
            .collect();
        assert_eq!(models, expected_models);
        assert_line(
            line(&data, 0, Some("claude-sonnet-4-5"), false),
            &[(0, 1_800_000_000.0), (30, 2_000_000_000.0)],
        );
        assert_line(
            line(&data, 1, Some("claude-sonnet-4-5"), false),
            &[(0, 2_350_000_000.0), (30, 2_000_000_000.0)],
        );
        assert_line(
            line(&data, 2, Some("claude-sonnet-4-5"), false),
            &[(0, 0.00552), (30, 0.0027)],
        );
        assert_line(
            line(&data, 0, Some("gpt-4o-mini"), false),
            &[(10, 700_000_000.0), (20, 2_700_000_000.0)],
        );
        assert_line(
            line(&data, 1, Some("gpt-4o-mini"), false),
            &[(10, 700_000_000.0), (20, 2_700_000_000.0)],
        );
        assert_line(line(&data, 2, Some("gpt-4o-mini"), false), &[(20, 0.00133)]);
    }

    #[tokio::test]
    async fn score_and_eval_series_match_the_fixed_dashboard() {
        let ctx = context();
        let window = hour();
        let scores = build(
            WidgetSource::Scores,
            vec![measure(Agg::Avg, Some("value_num"), None)],
            split("name", 20, false),
        );
        let data = answer(&ctx, ask(&scores, window, Shape::Series)).await;
        assert_scores(
            &data,
            &[
                ("helpfulness", &[(0, 0.7)]),
                ("thumbs", &[(0, 1.0)]),
                ("tone", &[(0, 1.0)]),
            ],
        );

        let eval = filtered(
            build(
                WidgetSource::Traces,
                vec![measure(Agg::Sum, Some("cost_total"), None)],
                None,
            ),
            "environment = 'cetra-eval'",
        );
        let mut request = ask(&eval, window, Shape::Series);
        request.include_eval = true;
        let data = answer(&ctx, request).await;
        assert_line(line(&data, 0, None, false), &[(0, 0.0008)]);
    }

    #[tokio::test]
    async fn score_widgets_leave_out_issue_occurrences() {
        let value = r#"{"data_type": "numeric", "value": 0.5}"#;
        let mut occurrence = score_row(1, 0, value, "{}", 30);
        occurrence.source = ScoreSource::Issue;
        let api = score_row(2, 0, value, "{}", 30);
        // The scores' trace has a span in the window, so the scores semi-join keeps it.
        let spans = [span(0, 0, minute(30), None)];
        let ctx = context_with(&spans, &[occurrence, api]);
        let counted = build(WidgetSource::Scores, count_query(), None);
        let rows = rows_of(&answer(&ctx, ask(&counted, hour(), Shape::Total)).await).to_vec();
        assert_eq!(rows[0].values[0], Some(1.0));
    }

    #[tokio::test]
    async fn dashboard_filter_restricts_spans_and_scores() {
        let ctx = context();
        let window = hour();
        let text = "status = 'error'";
        let counted = build(
            WidgetSource::Spans,
            vec![measure(Agg::Count, None, None)],
            None,
        );
        let all = rows_of(&answer(&ctx, ask(&counted, window, Shape::Total)).await)[0].values[0];
        let mut request = ask(&counted, window, Shape::Total);
        request.dashboard_filter = Some(text);
        let errors = rows_of(&answer(&ctx, request).await)[0].values[0];
        assert!(
            errors.is_some_and(|count| { count > 0.0 && all.is_some_and(|total| count < total) })
        );

        let tokens = filtered(
            build(
                WidgetSource::Spans,
                vec![measure(Agg::Sum, Some("input_tokens"), None)],
                None,
            ),
            "span_kind = 'llm' OR span_kind = 'embedding'",
        );
        let mut request = ask(&tokens, window, Shape::Series);
        request.dashboard_filter = Some(text);
        let data = answer(&ctx, request).await;
        assert_line(line(&data, 0, None, false), &[]);

        let scores = build(
            WidgetSource::Scores,
            vec![measure(Agg::Avg, Some("value_num"), None)],
            split("name", 20, false),
        );
        let mut request = ask(&scores, window, Shape::Series);
        request.dashboard_filter = Some(text);
        let data = answer(&ctx, request).await;
        assert_scores(&data, &[]);
    }

    #[tokio::test]
    async fn totals_and_compare_match_the_kpis() {
        let ctx = context();
        let window = window("2026-01-01T10:15:00Z", "2026-01-01T10:30:00Z");
        let kpis = build(
            WidgetSource::Traces,
            vec![
                measure(Agg::Count, None, None),
                measure(Agg::Rate, None, Some("error_count > 0")),
                measure(Agg::P95, Some("duration_ns"), None),
                measure(Agg::Sum, Some("cost_total"), None),
                measure(Agg::Sum, Some("total_tokens"), None),
            ],
            None,
        );
        let mut request = ask(&kpis, window, Shape::Total);
        request.compare = true;
        let data = answer(&ctx, request).await;
        let row = total_row(&data, None, false);
        let previous = row.previous.as_ref().expect("previous");
        // Value and previous range of traces, error rate, p95 latency, cost and total tokens.
        let expected = [
            (1.0, 3.0),
            (0.0, 1.0 / 3.0),
            (3_000_000_000.0, 4_200_000_000.0),
            (0.00133, 0.00552),
            (970.0, 4180.0),
        ];
        for (index, (value, before)) in expected.into_iter().enumerate() {
            assert_close(row.values[index], Some(value));
            assert_close(previous[index], Some(before));
        }
        assert!(row.values[0].is_some());
        assert!(previous[0].is_some_and(|value| value > 0.0));
    }

    #[tokio::test]
    async fn top_models_match_the_fixed_ranking() {
        let ctx = context();
        let window = hour();
        let top = filtered(
            build(
                WidgetSource::Spans,
                vec![
                    measure(Agg::Sum, Some("cost_total"), None),
                    measure(Agg::Count, None, None),
                    measure(Agg::Sum, Some("total_tokens"), None),
                ],
                split("model", 10, false),
            ),
            "span_kind = 'llm' AND model IS NOT NULL",
        );
        let data = answer(&ctx, ask(&top, window, Shape::Total)).await;
        // Model, cost, llm spans and total tokens, by cost descending.
        let expected = [
            ("claude-sonnet-4-5", 0.00822, 4.0, 4680.0),
            ("gpt-4o-mini", 0.00133, 2.0, 970.0),
        ];
        let rows = rows_of(&data);
        assert_eq!(rows.len(), expected.len());
        for (row, (model, cost, llm_count, tokens)) in rows.iter().zip(expected) {
            assert_eq!(row.group.as_deref(), Some(model));
            assert_close(row.values[0], Some(cost));
            assert_eq!(row.values[1], Some(llm_count));
            assert_close(row.values[2], Some(tokens));
        }
    }

    /// The expected series of one `group_by`: each group's `Traces` and `Errors` values.
    type Split = (&'static str, &'static [(i64, f64)], &'static [(i64, f64)]);

    #[tokio::test]
    async fn group_by_matches_the_fixed_splits() {
        let ctx = context();
        let window = hour();
        let cases: [(GroupBy, bool, &[Split]); 3] = [
            (
                GroupBy::Model,
                false,
                &[
                    (
                        "claude-sonnet-4-5",
                        &[(0, 2.0), (30, 1.0)],
                        &[(0, 0.0), (30, 0.0)],
                    ),
                    (
                        "gpt-4o-mini",
                        &[(10, 1.0), (20, 1.0)],
                        &[(10, 1.0), (20, 0.0)],
                    ),
                ],
            ),
            (
                GroupBy::Environment,
                true,
                &[
                    ("cetra-eval", &[(0, 1.0)], &[(0, 0.0)]),
                    (
                        "production",
                        &[(0, 2.0), (10, 1.0), (30, 1.0)],
                        &[(0, 0.0), (10, 1.0), (30, 0.0)],
                    ),
                    ("staging", &[(20, 1.0)], &[(20, 0.0)]),
                ],
            ),
            (
                GroupBy::Release,
                false,
                &[(
                    "v1.4.0",
                    &[(0, 2.0), (10, 1.0), (20, 1.0), (30, 1.0)],
                    &[(0, 0.0), (10, 1.0), (20, 0.0), (30, 0.0)],
                )],
            ),
        ];
        for (group_by, include_eval, expected) in cases {
            let counted = build(
                WidgetSource::Traces,
                vec![
                    measure(Agg::Count, None, None),
                    measure(Agg::Count, None, Some("error_count > 0")),
                ],
                None,
            );
            let mut request = ask(&counted, window, Shape::Series);
            request.group_by = Some(group_by);
            request.include_eval = include_eval;
            let data = answer(&ctx, request).await;
            let groups = groups_of(&data, 0);
            let expected_groups: BTreeSet<String> = expected
                .iter()
                .map(|(group, _, _)| (*group).to_owned())
                .collect();
            assert_eq!(groups, expected_groups, "{group_by:?}");
            for (group, traces, errors) in expected {
                assert_line(line(&data, 0, Some(group), false), traces);
                assert_line(line(&data, 1, Some(group), false), errors);
            }
        }
    }

    #[tokio::test]
    async fn breakdown_keeps_the_top_groups_and_counts_the_rest() {
        let models = [("m1", 3u16), ("m2", 3), ("m3", 2), ("m4", 1)];
        let mut spans = Vec::new();
        let mut trace = 0u16;
        for (model, count) in models {
            for _ in 0..count {
                trace += 1;
                spans.push(span(trace, 0, minute(i64::from(trace)), Some(model)));
            }
        }
        for _ in 0..2 {
            trace += 1;
            spans.push(span(trace, 0, minute(i64::from(trace)), None));
        }
        let ctx = context_of(&spans);
        let window = hour();
        let counted = |breakdown| {
            build(
                WidgetSource::Spans,
                vec![measure(Agg::Count, None, None)],
                breakdown,
            )
        };

        let data = answer(
            &ctx,
            ask(&counted(split("model", 2, true)), window, Shape::Total),
        )
        .await;
        let order: Vec<(Option<String>, bool, Option<f64>)> = rows_of(&data)
            .iter()
            .map(|row| (row.group.clone(), row.other, row.values[0]))
            .collect();
        assert_eq!(
            order,
            vec![
                (Some("m1".to_owned()), false, Some(3.0)),
                (Some("m2".to_owned()), false, Some(3.0)),
                (None, true, Some(3.0)),
            ]
        );

        let data = answer(
            &ctx,
            ask(&counted(split("model", 1, true)), window, Shape::Total),
        )
        .await;
        assert_eq!(total_row(&data, Some("m1"), false).values[0], Some(3.0));
        assert_eq!(total_row(&data, None, true).values[0], Some(6.0));

        let data = answer(
            &ctx,
            ask(&counted(split("model", 2, false)), window, Shape::Total),
        )
        .await;
        assert_eq!(rows_of(&data).len(), 2);
        assert!(rows_of(&data).iter().all(|row| !row.other));

        let data = answer(
            &ctx,
            ask(&counted(split("model", 2, true)), window, Shape::Series),
        )
        .await;
        assert_eq!(
            groups_of(&data, 0),
            BTreeSet::from(["m1".to_owned(), "m2".to_owned()])
        );
        assert_eq!(line(&data, 0, None, true).points.len(), 1);
        assert_eq!(line(&data, 0, None, true).points[0].value, Some(3.0));
    }

    #[tokio::test]
    async fn the_other_group_is_computed_over_its_own_rows() {
        let durations = [
            ("m1", 100i64),
            ("m1", 200),
            ("m1", 300),
            ("m2", 1_000),
            ("m2", 50_000),
            ("m3", 7),
            ("m3", 9_000_000),
            ("m4", 123_456),
        ];
        let mut spans = Vec::new();
        for (trace, (model, duration)) in durations.into_iter().enumerate() {
            let trace = u16::try_from(trace + 1).expect("trace");
            let mut span = span(trace, 0, minute(i64::from(trace)), Some(model));
            span.end_time = span.start_time + duration;
            spans.push(span);
        }
        let ctx = context_of(&spans);
        let window = hour();
        let grouped = build(
            WidgetSource::Spans,
            vec![
                measure(Agg::Count, None, None),
                measure(Agg::P95, Some("duration_ns"), None),
            ],
            split("model", 1, true),
        );
        let data = answer(&ctx, ask(&grouped, window, Shape::Total)).await;
        let other = total_row(&data, None, true);
        let rest = filtered(
            build(
                WidgetSource::Spans,
                vec![measure(Agg::P95, Some("duration_ns"), None)],
                None,
            ),
            "model = 'm2' OR model = 'm3' OR model = 'm4'",
        );
        let expected = answer(&ctx, ask(&rest, window, Shape::Total)).await;
        assert_close(other.values[1], rows_of(&expected)[0].values[0]);
        assert_eq!(other.values[0], Some(5.0));
    }

    #[tokio::test]
    async fn list_fields_count_a_row_under_each_value() {
        let mut first = span(1, 0, minute(1), None);
        first.tags = vec!["x".to_owned(), "y".to_owned()];
        let mut second = span(2, 0, minute(2), None);
        second.tags = vec!["x".to_owned()];
        let third = span(3, 0, minute(3), None);
        let ctx = context_of(&[first, second, third]);
        let tagged = build(
            WidgetSource::Spans,
            vec![measure(Agg::Count, None, None)],
            split("tags", 5, false),
        );
        let data = answer(&ctx, ask(&tagged, hour(), Shape::Total)).await;
        let found: Vec<(Option<String>, Option<f64>)> = rows_of(&data)
            .iter()
            .map(|row| (row.group.clone(), row.values[0]))
            .collect();
        assert_eq!(
            found,
            vec![
                (Some("x".to_owned()), Some(2.0)),
                (Some("y".to_owned()), Some(1.0))
            ]
        );
    }

    #[tokio::test]
    async fn map_key_breakdown_drops_missing_keys() {
        let mut spans = Vec::new();
        for (trace, user) in [
            (1u16, Some("alice")),
            (2, Some("alice")),
            (3, Some("bob")),
            (4, None),
        ] {
            let mut span = span(trace, 0, minute(i64::from(trace)), None);
            if let Some(user) = user {
                span.metadata = BTreeMap::from([("user".to_owned(), user.to_owned())]);
            }
            spans.push(span);
        }
        let ctx = context_of(&spans);
        let by_user = build(
            WidgetSource::Spans,
            vec![
                measure(Agg::Count, None, None),
                measure(Agg::CountDistinct, Some("trace_id"), None),
            ],
            split("metadata['user']", 5, true),
        );
        let data = answer(&ctx, ask(&by_user, hour(), Shape::Total)).await;
        let found: Vec<(Option<String>, Option<f64>)> = rows_of(&data)
            .iter()
            .map(|row| (row.group.clone(), row.values[0]))
            .collect();
        assert_eq!(
            found,
            vec![
                (Some("alice".to_owned()), Some(2.0)),
                (Some("bob".to_owned()), Some(1.0))
            ]
        );
        assert!(rows_of(&data).iter().all(|row| !row.other));
    }

    #[tokio::test]
    async fn group_by_is_ignored_with_a_breakdown_and_for_scores() {
        let mut spans = Vec::new();
        for (trace, model) in [(1u16, "m1"), (2, "m1"), (3, "m2")] {
            spans.push(span(trace, 0, minute(i64::from(trace)), Some(model)));
        }
        let ctx = context_of(&spans);
        let grouped = build(
            WidgetSource::Spans,
            vec![measure(Agg::Count, None, None)],
            split("model", 5, false),
        );
        let mut request = ask(&grouped, hour(), Shape::Total);
        request.group_by = Some(GroupBy::Environment);
        let data = answer(&ctx, request).await;
        let groups: Vec<_> = rows_of(&data)
            .iter()
            .filter_map(|row| row.group.clone())
            .collect();
        assert_eq!(groups, vec!["m1".to_owned(), "m2".to_owned()]);

        let ctx = context();
        let scored = build(
            WidgetSource::Scores,
            vec![measure(Agg::Count, None, None)],
            None,
        );
        let mut request = ask(&scored, hour(), Shape::Total);
        request.group_by = Some(GroupBy::Model);
        let data = answer(&ctx, request).await;
        assert_eq!(rows_of(&data).len(), 1);
        assert_eq!(rows_of(&data)[0].group, None);
    }

    #[tokio::test]
    async fn default_exclusions_hide_eval_traces_unless_a_filter_names_environment() {
        let mut spans = Vec::new();
        for (trace, environment_name) in [
            (1u16, "production"),
            (2, "production"),
            (3, environment::EVAL),
        ] {
            let mut span = span(trace, 0, minute(i64::from(trace)), None);
            span.environment = Some(environment_name.to_owned());
            spans.push(span);
        }
        let ctx = context_of(&spans);
        let window = hour();
        let traces = build(
            WidgetSource::Traces,
            vec![measure(Agg::Count, None, None)],
            None,
        );
        let data = answer(&ctx, ask(&traces, window, Shape::Total)).await;
        assert_eq!(total_row(&data, None, false).values[0], Some(2.0));

        let mut request = ask(&traces, window, Shape::Total);
        request.include_eval = true;
        let data = answer(&ctx, request).await;
        assert_eq!(total_row(&data, None, false).values[0], Some(3.0));

        let named_eval = filtered(
            build(
                WidgetSource::Traces,
                vec![measure(Agg::Count, None, None)],
                None,
            ),
            "environment = 'cetra-eval'",
        );
        let data = answer(&ctx, ask(&named_eval, window, Shape::Total)).await;
        assert_eq!(total_row(&data, None, false).values[0], Some(1.0));

        let in_a_condition = build(
            WidgetSource::Traces,
            vec![measure(
                Agg::Count,
                None,
                Some("environment = 'cetra-eval'"),
            )],
            None,
        );
        let data = answer(&ctx, ask(&in_a_condition, window, Shape::Total)).await;
        assert_eq!(total_row(&data, None, false).values[0], Some(1.0));
    }

    #[tokio::test]
    async fn span_counts_follow_the_dashboard_filter_through_traces() {
        let mut spans = Vec::new();
        let mut trace_span = |trace: u16, environment_name: &str, count: u8| {
            for index in 0..count {
                let mut span = span(trace, index, minute(i64::from(trace)), None);
                span.environment = Some(environment_name.to_owned());
                spans.push(span);
            }
        };
        trace_span(1, "production", 2);
        trace_span(2, "production", 1);
        trace_span(3, environment::EVAL, 4);
        let ctx = context_of(&spans);
        let window = hour();
        let counted = build(
            WidgetSource::Spans,
            vec![measure(Agg::Count, None, None)],
            None,
        );
        let data = answer(&ctx, ask(&counted, window, Shape::Total)).await;
        assert_eq!(total_row(&data, None, false).values[0], Some(3.0));

        let mut request = ask(&counted, window, Shape::Total);
        request.dashboard_filter = Some("environment = 'cetra-eval'");
        request.include_eval = true;
        let data = answer(&ctx, request).await;
        assert_eq!(total_row(&data, None, false).values[0], Some(4.0));
    }

    #[tokio::test]
    async fn exact_cost_sums_add_up_to_one() {
        let mut spans = Vec::new();
        for trace in 1..=10u16 {
            let mut span = span(trace, 0, minute(i64::from(trace)), Some("m1"));
            span.cost_total = Some(0.1);
            spans.push(span);
        }
        let ctx = context_of(&spans);
        let costs = build(
            WidgetSource::Spans,
            vec![measure(Agg::Sum, Some("cost_total"), None)],
            None,
        );
        let data = answer(&ctx, ask(&costs, hour(), Shape::Total)).await;
        assert_eq!(total_row(&data, None, false).values[0], Some(1.0));
    }

    #[tokio::test]
    async fn compare_reads_the_previous_range_and_missing_groups_get_none() {
        let spans = [
            span(1, 0, minute(5), Some("m1")),
            span(2, 0, minute(20), Some("m2")),
            span(3, 0, minute(30), Some("m1")),
            span(4, 0, minute(40), Some("m4")),
            span(5, 0, minute(-30), Some("m1")),
            span(6, 0, minute(-20), Some("m3")),
        ];
        let ctx = context_of(&spans);
        let counted = build(
            WidgetSource::Spans,
            vec![measure(Agg::Count, None, None)],
            split("model", 2, true),
        );
        let mut request = ask(&counted, hour(), Shape::Total);
        request.compare = true;
        let data = answer(&ctx, request).await;
        let m1 = total_row(&data, Some("m1"), false);
        assert_eq!(m1.values, vec![Some(2.0)]);
        assert_eq!(m1.previous, Some(vec![Some(1.0)]));
        let m2 = total_row(&data, Some("m2"), false);
        assert_eq!(m2.values, vec![Some(1.0)]);
        assert_eq!(m2.previous, Some(vec![None]));
        let other = total_row(&data, None, true);
        assert_eq!(other.values, vec![Some(1.0)]);
        assert_eq!(other.previous, Some(vec![Some(1.0)]));
    }

    #[tokio::test]
    async fn an_empty_window_has_no_values() {
        let ctx = context();
        let window = window("2030-01-01T00:00:00Z", "2030-01-01T01:00:00Z");
        let counted = build(
            WidgetSource::Traces,
            vec![
                measure(Agg::Count, None, None),
                measure(Agg::Rate, None, Some("error_count > 0")),
            ],
            None,
        );
        let mut request = ask(&counted, window, Shape::Total);
        request.compare = true;
        let data = answer(&ctx, request).await;
        let row = total_row(&data, None, false);
        assert_eq!(row.values, vec![None, None]);
        assert_eq!(row.previous, Some(vec![None, None]));

        let split_counted = build(
            WidgetSource::Traces,
            vec![measure(Agg::Count, None, None)],
            split("name", 5, true),
        );
        let data = answer(&ctx, ask(&split_counted, window, Shape::Total)).await;
        assert!(rows_of(&data).is_empty());
    }

    #[tokio::test]
    async fn series_points_skip_empty_buckets_and_carry_rows() {
        let spans = [
            span(1, 0, minute(5), None),
            span(2, 0, minute(6), None),
            span(3, 0, minute(35), None),
        ];
        let ctx = context_of(&spans);
        let counted = build(
            WidgetSource::Spans,
            vec![
                measure(Agg::Count, None, None),
                measure(Agg::Min, Some("duration_ns"), None),
                measure(Agg::Max, Some("duration_ns"), None),
                measure(Agg::Avg, Some("duration_ns"), None),
            ],
            None,
        );
        let data = answer(&ctx, ask(&counted, hour(), Shape::Series)).await;
        let counts = line(&data, 0, None, false);
        let times: Vec<(i64, Option<f64>, u64)> = counts
            .points
            .iter()
            .map(|point| (point.time, point.value, point.rows))
            .collect();
        assert_eq!(
            times,
            vec![(minute(0), Some(2.0), 2), (minute(30), Some(1.0), 1),]
        );
        assert_eq!(line(&data, 1, None, false).points[0].value, Some(100.0));
        assert_eq!(line(&data, 2, None, false).points[0].value, Some(100.0));
        assert_eq!(line(&data, 3, None, false).points[0].value, Some(100.0));
    }

    #[tokio::test]
    async fn bad_queries_fail_before_any_run() {
        let ctx = context();
        let sum_without_field = build(
            WidgetSource::Traces,
            vec![measure(Agg::Sum, None, None)],
            None,
        );
        match widget_query(&ctx, ask(&sum_without_field, hour(), Shape::Total)).await {
            Err(WidgetError::Fields(errors)) => {
                assert_eq!(errors[0].path, "measures[0].field");
            }
            other => panic!("{other:?}"),
        }

        let broken = filtered(
            build(
                WidgetSource::Traces,
                vec![measure(Agg::Count, None, None)],
                None,
            ),
            "error_count >",
        );
        match widget_query(&ctx, ask(&broken, hour(), Shape::Total)).await {
            Err(WidgetError::Expression { path, .. }) => assert_eq!(path, "filter"),
            other => panic!("{other:?}"),
        }

        let counted = build(
            WidgetSource::Traces,
            vec![measure(Agg::Count, None, None)],
            None,
        );
        let two_thousand_one = TimeWindow::new(0, 2_001 * MINUTE).expect("window");
        let mut request = ask(&counted, two_thousand_one, Shape::Series);
        request.bucket = Bucket::Fixed(BucketWidth::M1);
        match widget_query(&ctx, request).await {
            Err(WidgetError::Run(RunError::InvalidRequest(message))) => {
                assert_eq!(message, "too many buckets; pick a wider bucket");
            }
            other => panic!("{other:?}"),
        }
    }

    /// The `(time, value)` points of one series line.
    fn values_of(
        data: &WidgetData,
        measure: usize,
        group: Option<&str>,
        other: bool,
    ) -> Vec<(i64, Option<f64>)> {
        line(data, measure, group, other)
            .points
            .iter()
            .map(|point| (point.time, point.value))
            .collect()
    }

    /// The trace id that `span_row` and `trace_id_of` give trace `trace`, as lowercase hex.
    fn trace_hex(trace: u16) -> String {
        format!("{:04x}", trace + 1).repeat(8)
    }

    /// A live score on trace `trace`, created at minute `at_minute` of the test hour.
    fn score_row(index: u16, trace: u16, value: &str, metadata: &str, at_minute: u32) -> Score {
        let text = format!(
            r#"{{
                "project_id": "01900000-0000-7000-8000-000000000001",
                "score_id": "01900000-0000-7000-8000-{index:012x}",
                "target": {{"type": "trace", "trace_id": "{hex}"}},
                "name": "q",
                "value": {value},
                "metadata": {metadata},
                "source": "api",
                "created_at": "2026-01-01T10:{at_minute:02}:00Z",
                "updated_at": "2026-01-01T10:{at_minute:02}:00Z",
                "deleted": false,
                "ingest_time": "2026-01-01T10:{at_minute:02}:00Z"
            }}"#,
            hex = trace_hex(trace),
        );
        serde_json::from_str(&text).unwrap_or_else(|error| panic!("{error}"))
    }

    /// The names and values of a total, one row per group: `(group, other, values)`.
    fn total_pairs(data: &WidgetData) -> Vec<(Option<String>, bool, Vec<Option<f64>>)> {
        rows_of(data)
            .iter()
            .map(|row| (row.group.clone(), row.other, row.values.clone()))
            .collect()
    }

    #[tokio::test]
    async fn a_list_where_sees_the_list_before_the_unnest() {
        let mut spans = Vec::new();
        for (trace, tags) in [(1u16, vec!["x", "beta"]), (2, vec!["x"]), (3, vec!["y"])] {
            let mut span = span(trace, 0, minute(i64::from(trace) * 5), None);
            span.tags = tags.into_iter().map(str::to_owned).collect();
            spans.push(span);
        }
        let ctx = context_of(&spans);
        let tagged = build(
            WidgetSource::Spans,
            vec![
                measure(Agg::Count, None, None),
                measure(Agg::Count, None, Some("tags @> ['beta']")),
            ],
            split("tags", 5, false),
        );
        let data = answer(&ctx, ask(&tagged, hour(), Shape::Total)).await;
        let found: Vec<(Option<String>, Vec<Option<f64>>)> = rows_of(&data)
            .iter()
            .map(|row| (row.group.clone(), row.values.clone()))
            .collect();
        assert_eq!(
            found,
            vec![
                (Some("x".to_owned()), vec![Some(2.0), Some(1.0)]),
                (Some("beta".to_owned()), vec![Some(1.0), Some(1.0)]),
                (Some("y".to_owned()), vec![Some(1.0), Some(0.0)]),
            ]
        );
    }

    #[tokio::test]
    async fn breakdown_ranks_by_a_rate_first_measure() {
        let mut spans = Vec::new();
        let mut trace = 0u16;
        for (model, hits, total) in [("a", 1u16, 4u16), ("b", 2, 2), ("c", 3, 4)] {
            for index in 0..total {
                trace += 1;
                let mut span = span(trace, 0, minute(i64::from(trace)), Some(model));
                if index < hits {
                    span.name = "hit".to_owned();
                }
                spans.push(span);
            }
        }
        let ctx = context_of(&spans);
        let ranked = build(
            WidgetSource::Spans,
            vec![
                measure(Agg::Rate, None, Some("name = 'hit'")),
                measure(Agg::Count, None, None),
            ],
            split("model", 2, true),
        );
        let data = answer(&ctx, ask(&ranked, hour(), Shape::Total)).await;
        assert_eq!(
            total_pairs(&data),
            vec![
                (Some("b".to_owned()), false, vec![Some(1.0), Some(2.0)]),
                (Some("c".to_owned()), false, vec![Some(0.75), Some(4.0)]),
                (None, true, vec![Some(0.25), Some(4.0)]),
            ]
        );
    }

    #[tokio::test]
    async fn null_first_measures_rank_last() {
        let mut spans = Vec::new();
        for (trace, model, cost) in [(1u16, "n", None), (2, "p", Some(2.0)), (3, "q", Some(1.0))] {
            let mut span = span(trace, 0, minute(i64::from(trace)), Some(model));
            span.cost_total = cost;
            spans.push(span);
        }
        let ctx = context_of(&spans);
        let costs = build(
            WidgetSource::Spans,
            vec![
                measure(Agg::Sum, Some("cost_total"), None),
                measure(Agg::Count, None, None),
            ],
            split("model", 3, false),
        );
        let data = answer(&ctx, ask(&costs, hour(), Shape::Total)).await;
        assert_eq!(
            total_pairs(&data),
            vec![
                (Some("p".to_owned()), false, vec![Some(2.0), Some(1.0)]),
                (Some("q".to_owned()), false, vec![Some(1.0), Some(1.0)]),
                (Some("n".to_owned()), false, vec![None, Some(1.0)]),
            ]
        );
    }

    #[tokio::test]
    async fn ties_on_the_first_measure_sort_by_value() {
        let mut spans = Vec::new();
        for (trace, model) in [
            (1u16, "zeta"),
            (2, "zeta"),
            (3, "alpha"),
            (4, "alpha"),
            (5, "mid"),
        ] {
            spans.push(span(trace, 0, minute(i64::from(trace)), Some(model)));
        }
        let ctx = context_of(&spans);
        let counted = build(
            WidgetSource::Spans,
            vec![measure(Agg::Count, None, None)],
            split("model", 2, false),
        );
        let data = answer(&ctx, ask(&counted, hour(), Shape::Total)).await;
        assert_eq!(
            total_pairs(&data),
            vec![
                (Some("alpha".to_owned()), false, vec![Some(2.0)]),
                (Some("zeta".to_owned()), false, vec![Some(2.0)]),
            ]
        );
    }

    #[tokio::test]
    async fn count_distinct_user_ids_on_traces() {
        let mut spans = Vec::new();
        for (trace, user, environment_name) in [
            (0u16, Some("alice"), "production"),
            (1, Some("alice"), "production"),
            (2, Some("bob"), "staging"),
            (3, None, "staging"),
        ] {
            let mut span = span(trace, 0, minute(i64::from(trace) * 5 + 5), None);
            span.user_id = user.map(str::to_owned);
            span.environment = Some(environment_name.to_owned());
            spans.push(span);
        }
        let ctx = context_of(&spans);
        let measures = || {
            vec![
                measure(Agg::CountDistinct, Some("user_id"), None),
                measure(Agg::Count, None, None),
            ]
        };
        let total = build(WidgetSource::Traces, measures(), None);
        let data = answer(&ctx, ask(&total, hour(), Shape::Total)).await;
        assert_eq!(
            total_pairs(&data),
            vec![(None, false, vec![Some(2.0), Some(4.0)])]
        );

        let by_environment = build(
            WidgetSource::Traces,
            measures(),
            split("environment", 5, false),
        );
        let data = answer(&ctx, ask(&by_environment, hour(), Shape::Total)).await;
        assert_eq!(
            total_pairs(&data),
            vec![
                (
                    Some("production".to_owned()),
                    false,
                    vec![Some(1.0), Some(2.0)]
                ),
                (
                    Some("staging".to_owned()),
                    false,
                    vec![Some(1.0), Some(2.0)]
                ),
            ]
        );
    }

    #[tokio::test]
    async fn breakdown_by_session_id_on_traces() {
        let mut spans = Vec::new();
        for (trace, session, cost) in [
            (0u16, Some("s1"), 1.0),
            (1, Some("s1"), 2.0),
            (2, Some("s2"), 5.0),
            (3, None, 9.0),
        ] {
            let mut span = span(trace, 0, minute(i64::from(trace) * 5 + 5), None);
            span.session_id = session.map(str::to_owned);
            span.cost_total = Some(cost);
            spans.push(span);
        }
        let ctx = context_of(&spans);
        let by_session = build(
            WidgetSource::Traces,
            vec![measure(Agg::Sum, Some("cost_total"), None)],
            split("session_id", 5, false),
        );
        let data = answer(&ctx, ask(&by_session, hour(), Shape::Total)).await;
        assert_eq!(
            total_pairs(&data),
            vec![
                (Some("s2".to_owned()), false, vec![Some(5.0)]),
                (Some("s1".to_owned()), false, vec![Some(3.0)]),
            ]
        );
    }

    #[tokio::test]
    async fn measure_where_on_a_percentile_and_a_cost_sum() {
        let placed = [
            ("m1", 100i64, Some(0.5)),
            ("m1", 200, Some(0.25)),
            ("m1", 300, Some(0.25)),
            ("m2", 10_000, Some(4.0)),
        ];
        let mut spans = Vec::new();
        for (trace, (model, duration, cost)) in placed.into_iter().enumerate() {
            let trace = u16::try_from(trace + 1).expect("trace");
            let mut span = span(trace, 0, minute(i64::from(trace)), Some(model));
            span.end_time = span.start_time + duration;
            span.cost_total = cost;
            spans.push(span);
        }
        let ctx = context_of(&spans);
        let only_m1 = "model = 'm1'";
        let mixed = build(
            WidgetSource::Spans,
            vec![
                measure(Agg::P95, Some("duration_ns"), Some(only_m1)),
                measure(Agg::P95, Some("duration_ns"), None),
                measure(Agg::Sum, Some("cost_total"), Some(only_m1)),
                measure(Agg::Sum, Some("cost_total"), None),
            ],
            None,
        );
        let data = answer(&ctx, ask(&mixed, hour(), Shape::Total)).await;
        let row = total_row(&data, None, false);
        // The percentile is approximate, but it lies between the values it was given.
        assert!(row.values[0].is_some_and(|value| (100.0..=300.0).contains(&value)));
        assert!(row.values[1].is_some_and(|value| value > 1_000.0));
        assert_close(row.values[2], Some(1.0));
        assert_close(row.values[3], Some(5.0));
    }

    #[tokio::test]
    async fn series_with_other_two_measures_and_a_where() {
        let placed: [(&str, i64, &str); 8] = [
            ("m1", 5, "hit"),
            ("m1", 5, "miss"),
            ("m1", 35, "hit"),
            ("m2", 6, "miss"),
            ("m2", 36, "hit"),
            ("m2", 36, "hit"),
            ("m3", 7, "miss"),
            ("m4", 8, "hit"),
        ];
        let mut spans = Vec::new();
        for (trace, (model, at_minute, name)) in placed.into_iter().enumerate() {
            let trace = u16::try_from(trace + 1).expect("trace");
            let mut span = span(trace, 0, minute(at_minute), Some(model));
            span.name = name.to_owned();
            spans.push(span);
        }
        let ctx = context_of(&spans);
        let grouped = build(
            WidgetSource::Spans,
            vec![
                measure(Agg::Count, None, None),
                measure(Agg::Count, None, Some("name = 'hit'")),
            ],
            split("model", 2, true),
        );
        let data = answer(&ctx, ask(&grouped, hour(), Shape::Series)).await;
        assert_eq!(
            groups_of(&data, 0),
            BTreeSet::from(["m1".to_owned(), "m2".to_owned()])
        );
        assert_eq!(
            values_of(&data, 0, Some("m1"), false),
            vec![(minute(0), Some(2.0)), (minute(30), Some(1.0))]
        );
        assert_eq!(
            values_of(&data, 1, Some("m1"), false),
            vec![(minute(0), Some(1.0)), (minute(30), Some(1.0))]
        );
        assert_eq!(
            values_of(&data, 0, Some("m2"), false),
            vec![(minute(0), Some(1.0)), (minute(30), Some(2.0))]
        );
        // A bucket with rows and no matching row is 0, not null.
        assert_eq!(
            values_of(&data, 1, Some("m2"), false),
            vec![(minute(0), Some(0.0)), (minute(30), Some(2.0))]
        );
        assert_eq!(
            values_of(&data, 0, None, true),
            vec![(minute(0), Some(2.0))]
        );
        assert_eq!(
            values_of(&data, 1, None, true),
            vec![(minute(0), Some(1.0))]
        );
    }

    #[tokio::test]
    async fn score_breakdowns_on_value_str_and_a_metadata_key() {
        let mut spans = Vec::new();
        for trace in 0..4u16 {
            spans.push(span(trace, 0, minute(i64::from(trace) * 5 + 5), None));
        }
        let scores = [
            score_row(
                1,
                0,
                r#"{"data_type":"text","value":"yes"}"#,
                r#"{"k": "a"}"#,
                5,
            ),
            score_row(
                2,
                1,
                r#"{"data_type":"text","value":"yes"}"#,
                r#"{"k": "b"}"#,
                10,
            ),
            score_row(3, 2, r#"{"data_type":"text","value":"no"}"#, "{}", 15),
            score_row(
                4,
                3,
                r#"{"data_type":"numeric","value":0.5}"#,
                r#"{"k": "a"}"#,
                20,
            ),
        ];
        let ctx = context_with(&spans, &scores);
        let by_label = build(
            WidgetSource::Scores,
            vec![
                measure(Agg::Count, None, None),
                measure(Agg::Count, None, Some("value_num IS NOT NULL")),
            ],
            split("value_str", 5, false),
        );
        let data = answer(&ctx, ask(&by_label, hour(), Shape::Total)).await;
        // The numeric score has no value_str, so it is left out.
        assert_eq!(
            total_pairs(&data),
            vec![
                (Some("yes".to_owned()), false, vec![Some(2.0), Some(0.0)]),
                (Some("no".to_owned()), false, vec![Some(1.0), Some(0.0)]),
            ]
        );

        let by_key = build(
            WidgetSource::Scores,
            vec![measure(Agg::Count, None, None)],
            split("metadata['k']", 5, false),
        );
        let data = answer(&ctx, ask(&by_key, hour(), Shape::Total)).await;
        // The score with no key `k` is left out.
        assert_eq!(
            total_pairs(&data),
            vec![
                (Some("a".to_owned()), false, vec![Some(2.0)]),
                (Some("b".to_owned()), false, vec![Some(1.0)]),
            ]
        );
    }

    #[tokio::test]
    async fn list_breakdown_with_other_and_compare() {
        let placed = [
            (1u16, vec!["a", "b"], 5i64),
            (2, vec!["a"], 10),
            (3, vec!["b"], 15),
            (4, vec!["c"], 20),
            (5, vec!["d"], 25),
            (6, vec!["a"], -30),
            (7, vec!["z"], -20),
        ];
        let mut spans = Vec::new();
        for (trace, tags, at_minute) in placed {
            let mut span = span(trace, 0, minute(at_minute), None);
            span.tags = tags.into_iter().map(str::to_owned).collect();
            spans.push(span);
        }
        let ctx = context_of(&spans);
        let tagged = build(
            WidgetSource::Spans,
            vec![measure(Agg::Count, None, None)],
            split("tags", 2, true),
        );
        let mut request = ask(&tagged, hour(), Shape::Total);
        request.compare = true;
        let data = answer(&ctx, request).await;
        // Each row counts under each of its tags. The other group holds the tags c and d.
        let a = total_row(&data, Some("a"), false);
        assert_eq!(a.values, vec![Some(2.0)]);
        assert_eq!(a.previous, Some(vec![Some(1.0)]));
        let b = total_row(&data, Some("b"), false);
        assert_eq!(b.values, vec![Some(2.0)]);
        assert_eq!(b.previous, Some(vec![None]));
        let other = total_row(&data, None, true);
        assert_eq!(other.values, vec![Some(2.0)]);
        assert_eq!(other.previous, Some(vec![Some(1.0)]));
    }

    #[tokio::test]
    async fn a_count_whose_where_matches_nothing_is_zero() {
        let mut spans = Vec::new();
        for (trace, name) in [(1u16, "hit"), (2, "miss"), (3, "miss")] {
            let mut span = span(trace, 0, minute(i64::from(trace)), Some("m1"));
            span.name = name.to_owned();
            spans.push(span);
        }
        let ctx = context_of(&spans);
        let counts = vec![
            measure(Agg::Count, None, Some("name = 'nope'")),
            measure(Agg::Count, None, None),
        ];
        let grouped = build(
            WidgetSource::Spans,
            counts.clone(),
            split("model", 5, false),
        );
        let data = answer(&ctx, ask(&grouped, hour(), Shape::Total)).await;
        assert_eq!(
            total_row(&data, Some("m1"), false).values,
            vec![Some(0.0), Some(3.0)]
        );

        let total = build(WidgetSource::Spans, counts.clone(), None);
        let data = answer(&ctx, ask(&total, hour(), Shape::Total)).await;
        assert_eq!(
            total_row(&data, None, false).values,
            vec![Some(0.0), Some(3.0)]
        );

        // A total with no rows has no values at all, the conditioned count included.
        let empty = build(WidgetSource::Spans, counts, None);
        let data = answer(
            &ctx,
            ask(
                &empty,
                window("2030-01-01T00:00:00Z", "2030-01-01T01:00:00Z"),
                Shape::Total,
            ),
        )
        .await;
        assert_eq!(total_row(&data, None, false).values, vec![None, None]);
    }
}
