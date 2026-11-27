//! Native query engine implementation of `validate_shape`, `validate_expressions`,
//! and `widget_query` (spec 05 § Dashboard queries).

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, BinaryHeap};
use std::sync::Arc;

use cetra_column::{Batch, DataType, Field, ScalarValue, Schema};
use cetra_core::enums::{ScoreSource, ScoreTargetKind};
use cetra_core::schema::{scores, spans, traces};
use cetra_query::exec::streaming::{FilterOp, ProjectOp, UnnestOp};
use cetra_query::exec::{
    AggExpr, AggFunc, ExecContext, HashAgg, HashAggSpec, MapProjection, PartitionStream,
    PushOperator, ScanSource, ScanSpec, TimeRangeHint, collect_expr_map_projections,
    run_pipeline as exec_run_pipeline,
};
use cetra_query::{BinaryOp, CompiledFilter, Expr, FilterView, QueryError, UnaryOp};

use crate::query_error::{InvalidQuery, RunError};
use crate::reads::buckets::{BUCKET, Bucket, BucketWidth, bucket_expr, resolve as resolve_bucket};
use crate::reads::columns::{
    active_rows, internal, raw_decimal_to_f64_at, raw_f64_at, raw_i64_at, raw_string_at,
    raw_time_at, require_col,
};
use crate::reads::context::{
    DURATION_MS, DURATION_NS, EngineContext, OperatorStream, duration_ns_from_bounds_expr,
    populate_id_lookups, query_err_to_run, remap_expr_by_name, rewrite_view_expr_for_raw,
    run_err_to_query, scores_raw_schema, spans_raw_schema, spans_view_projection,
    spans_view_schema, traces_raw_schema, traces_view_projection, traces_view_schema,
};
use crate::reads::present;
use crate::reads::scope::{TraceSemiJoinParams, apply_trace_id_semi_join, scoped_traces_spec};
use crate::reads::scores_join::{apply_compiled_filter, compile_filter, compile_optional_filter};
use crate::views::{QueryContext, TimeWindow};

/// Most measures in one widget query.
pub const MAX_MEASURES: usize = 6;
/// Largest breakdown `limit`.
pub const MAX_BREAKDOWN_LIMIT: usize = 20;
/// Breakdown `limit` when the request leaves it out. The caller applies it.
pub const DEFAULT_BREAKDOWN_LIMIT: usize = 5;
/// Longest map key of a `metadata['k']`-style field, in characters.
pub const MAX_MAP_KEY: usize = 256;

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
    pub timeout: std::time::Duration,
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

use crate::reads::ListScope;

const LIST_FIELDS: [&str; 2] = [traces::TAGS, "models"];
const IDENTITY_FIELDS: [&str; 3] = [traces::TRACE_ID, traces::SESSION_ID, traces::USER_ID];
const GROUP_BY_LIMIT: usize = 20;
const COST_FIELDS: [&str; 3] = [spans::COST_INPUT, spans::COST_OUTPUT, spans::COST_TOTAL];

const ROWS: &str = "rows";
const GROUP_INPUT: &str = "group_input";
const GROUP_SLOT: &str = "group_slot";
const DUMMY_ONE: &str = "_one";

struct SourceFields {
    numeric: &'static [&'static str],
    breakdown: &'static [&'static str],
    maps: &'static [&'static str],
}

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

fn fields_of(source: WidgetSource) -> &'static SourceFields {
    match source {
        WidgetSource::Traces => &TRACES,
        WidgetSource::Spans => &SPANS,
        WidgetSource::Scores => &SCORES,
    }
}

fn filter_view(source: WidgetSource) -> FilterView {
    match source {
        WidgetSource::Traces => FilterView::Traces,
        WidgetSource::Spans => FilterView::Spans,
        WidgetSource::Scores => FilterView::Scores,
    }
}

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

fn field_error(path: impl Into<String>, message: &str) -> FieldError {
    FieldError {
        path: path.into(),
        message: message.to_owned(),
    }
}

/// Validates widget shape without touching storage, collecting all field errors in path order.
pub fn validate_shape(query: &WidgetQuery) -> Result<(), WidgetError> {
    let table = fields_of(query.source);
    let mut errors = Vec::new();
    if !(1..=MAX_MEASURES).contains(&query.measures.len()) {
        errors.push(field_error("measures", "must have 1 to 6 measures"));
    }
    for (index, measure) in query.measures.iter().enumerate() {
        check_measure(table, measure, index, &mut errors);
    }
    if let Some(breakdown) = &query.breakdown {
        if !is_breakdown(table, &breakdown.field) {
            errors.push(field_error(
                "breakdown.field",
                "not a breakdown field of this source",
            ));
        }
        if !(1..=MAX_BREAKDOWN_LIMIT).contains(&breakdown.limit) {
            errors.push(field_error("breakdown.limit", "must be 1 to 20"));
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
                errors.push(field_error(field_path, "count takes no field"));
            }
        }
        Agg::Rate => {
            if measure.field.is_some() {
                errors.push(field_error(field_path, "rate takes no field"));
            }
            if !has_condition {
                errors.push(field_error(condition_path, "rate needs a where"));
            }
        }
        Agg::CountDistinct => match measure.field.as_deref() {
            None => errors.push(field_error(field_path, "count_distinct needs a field")),
            Some(field) if !is_distinct(table, field) => {
                errors.push(field_error(
                    field_path,
                    "count_distinct does not take this field",
                ));
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
            None => errors.push(field_error(field_path, "needs a numeric field")),
            Some(field) if !is_numeric(table, field) => {
                errors.push(field_error(
                    field_path,
                    "not a numeric field of this source",
                ));
            }
            Some(_) => {}
        },
    }
}

/// Validates `dashboard_filter`, `query.filter`, and every `measures[i].where` via `compile_filter`.
pub fn validate_expressions(
    _ctx: &QueryContext,
    query: &WidgetQuery,
    dashboard_filter: Option<&str>,
) -> Result<(), WidgetError> {
    if let Some(text) = present(dashboard_filter) {
        compile_expression(FilterView::Traces, "dashboard_filter", text)?;
    }
    if let Some(text) = present(query.filter.as_deref()) {
        compile_expression(filter_view(query.source), "filter", text)?;
    }
    for (index, measure) in query.measures.iter().enumerate() {
        let Some(text) = present(measure.condition.as_deref()) else {
            continue;
        };
        let path = format!("measures[{index}].where");
        let compiled = compile_expression(filter_view(query.source), &path, text)?;
        if !compiled.scores.is_empty() {
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

fn compile_expression(
    view: FilterView,
    path: &str,
    text: &str,
) -> Result<CompiledFilter, WidgetError> {
    compile_filter(view, text).map_err(|err| match err {
        RunError::Invalid(error) => WidgetError::Expression {
            path: path.to_owned(),
            error,
        },
        other => WidgetError::Run(other),
    })
}

/// Native query engine implementation of `widget_query`.
pub async fn widget_query(
    ctx: &QueryContext,
    request: WidgetRequest<'_>,
) -> Result<WidgetData, WidgetError> {
    validate_shape(request.query)?;
    validate_expressions(ctx, request.query, request.dashboard_filter)?;
    let ctx = ctx.engine();

    let width = match request.shape {
        Shape::Series => Some(resolve_bucket(request.window, request.bucket)?),
        Shape::Total => None,
    };

    let compiled = CompiledWidgetFilters::compile(&request)?;
    let spec_plan = WidgetPlan::new(&request, width);
    let dashboard_text = present(request.dashboard_filter).map(str::to_owned);
    let window = request.window;
    let include_eval = request.include_eval;
    let include_experiments = request.include_experiments;
    let compare = request.compare;

    let ctx_c = ctx.clone();
    let (data, _) = ctx
        .run_with_ctx(request.timeout, move |exec_ctx| {
            let scope_cfg = WidgetScopeCfg {
                include_eval,
                include_experiments,
                dashboard_text: dashboard_text.as_deref(),
                filters: &compiled,
                plan: &spec_plan,
            };
            run_widget(&ctx_c, exec_ctx, window, compare, width, &scope_cfg)
                .map_err(run_err_to_query)
        })
        .await?;
    Ok(data)
}

#[derive(Clone)]
struct CompiledWidgetFilters {
    dashboard: Option<CompiledFilter>,
    widget: Option<CompiledFilter>,
    conditions: Vec<Option<CompiledFilter>>,
    references_environment: bool,
}

impl CompiledWidgetFilters {
    fn compile(request: &WidgetRequest<'_>) -> Result<Self, RunError> {
        let dashboard = compile_optional_filter(FilterView::Traces, request.dashboard_filter)?;
        let source_view = filter_view(request.query.source);
        let widget = compile_optional_filter(source_view, request.query.filter.as_deref())?;
        let mut conditions = Vec::with_capacity(request.query.measures.len());
        for m in &request.query.measures {
            conditions.push(compile_optional_filter(
                source_view,
                m.condition.as_deref(),
            )?);
        }
        let references_environment = dashboard
            .iter()
            .chain(widget.iter())
            .chain(conditions.iter().flatten())
            .any(|f| f.references_environment);
        Ok(Self {
            dashboard,
            widget,
            conditions,
            references_environment,
        })
    }
}

#[derive(Clone)]
struct GroupSpec {
    field: String,
    list: bool,
    limit: usize,
    other: bool,
}

#[derive(Clone)]
struct MeasurePlan {
    agg: Agg,
    field: Option<String>,
    has_where: bool,
    is_cost: bool,
    col_name: String,
}

#[derive(Clone)]
struct WidgetPlan {
    source: WidgetSource,
    measures: Vec<MeasurePlan>,
    group: Option<GroupSpec>,
}

impl WidgetPlan {
    fn new(request: &WidgetRequest<'_>, _width: Option<i64>) -> Self {
        let query = request.query;
        let measures = query
            .measures
            .iter()
            .enumerate()
            .map(|(i, m)| MeasurePlan {
                agg: m.agg,
                field: m.field.clone(),
                has_where: present(m.condition.as_deref()).is_some(),
                is_cost: m
                    .field
                    .as_deref()
                    .is_some_and(|name| COST_FIELDS.contains(&name)),
                col_name: format!("m{i}"),
            })
            .collect();

        let split = match (&query.breakdown, request.group_by) {
            (Some(b), _) => Some((b.field.clone(), b.limit, b.other)),
            (None, Some(gb)) => {
                group_by_field(query.source, gb).map(|f| (f.to_owned(), GROUP_BY_LIMIT, false))
            }
            (None, None) => None,
        };
        let group = split.map(|(field, limit, other)| GroupSpec {
            list: LIST_FIELDS.contains(&field.as_str()),
            field,
            limit,
            other,
        });

        Self {
            source: query.source,
            measures,
            group,
        }
    }
}

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

fn time_column(source: WidgetSource) -> &'static str {
    match source {
        WidgetSource::Traces => traces::START_TIME,
        WidgetSource::Spans => spans::START_TIME,
        WidgetSource::Scores => scores::CREATED_AT,
    }
}

struct WidgetScopeCfg<'a> {
    include_eval: bool,
    include_experiments: bool,
    dashboard_text: Option<&'a str>,
    filters: &'a CompiledWidgetFilters,
    plan: &'a WidgetPlan,
}

struct SemiJoinScope<'a> {
    window: TimeWindow,
    include_eval: bool,
    include_experiments: bool,
    references_environment: bool,
    dashboard_text: Option<&'a str>,
}

fn run_widget(
    ctx: &EngineContext,
    exec_ctx: &ExecContext,
    window: TimeWindow,
    compare: bool,
    width: Option<i64>,
    cfg: &WidgetScopeCfg<'_>,
) -> Result<WidgetData, RunError> {
    match width {
        Some(bucket_ns) => run_series(ctx, exec_ctx, window, bucket_ns, cfg),
        None => run_total(ctx, exec_ctx, window, compare, cfg),
    }
}

struct PreparedStreams {
    streams: Vec<Box<dyn PartitionStream>>,
    schema: Arc<Schema>,
}

fn prepare_widget_streams(
    ctx: &EngineContext,
    exec_ctx: &ExecContext,
    window: TimeWindow,
    width: Option<i64>,
    cfg: &WidgetScopeCfg<'_>,
) -> Result<PreparedStreams, RunError> {
    let WidgetScopeCfg {
        include_eval,
        include_experiments,
        dashboard_text,
        filters,
        plan,
    } = *cfg;
    let uses_score_joins = filters
        .dashboard
        .as_ref()
        .is_some_and(|f| !f.scores.is_empty())
        || filters
            .widget
            .as_ref()
            .is_some_and(|f| !f.scores.is_empty());

    let (base_streams, base_schema, is_raw) = match plan.source {
        WidgetSource::Traces => {
            let mut spec = scoped_traces_spec(
                ctx,
                ListScope {
                    window: Some(window),
                    include_eval,
                    include_experiments,
                },
                filters.references_environment,
            )?;
            spec.time_range.around_ns = None;
            let raw_schema = traces_raw_schema()?;

            if uses_score_joins {
                let raw_streams = ctx
                    .traces
                    .open_partitions(&spec, exec_ctx)
                    .map_err(query_err_to_run)?;
                let view_schema = traces_view_schema()?;
                let mut streams: Vec<Box<dyn PartitionStream>> = raw_streams
                    .into_iter()
                    .map(|s| {
                        let proj = traces_view_projection(&raw_schema, exec_ctx.clone())?;
                        Ok(
                            Box::new(OperatorStream::new(s, vec![Box::new(proj)], exec_ctx))
                                as Box<dyn PartitionStream>,
                        )
                    })
                    .collect::<Result<Vec<_>, QueryError>>()
                    .map_err(query_err_to_run)?;
                if let Some(df) = &filters.dashboard {
                    streams = apply_compiled_filter(ctx, exec_ctx, streams, &view_schema, df)
                        .map_err(query_err_to_run)?;
                }
                if let Some(wf) = &filters.widget {
                    streams = apply_compiled_filter(ctx, exec_ctx, streams, &view_schema, wf)
                        .map_err(query_err_to_run)?;
                }
                (streams, view_schema, false)
            } else {
                for f in [&filters.dashboard, &filters.widget].into_iter().flatten() {
                    let rewritten = rewrite_view_expr_for_raw(
                        &f.expr,
                        &raw_schema,
                        &[traces::TRACE_ID, traces::ROOT_SPAN_ID],
                        true,
                    )
                    .map_err(query_err_to_run)?;
                    spec.filters.push(rewritten);
                }
                populate_id_lookups(&mut spec);
                let needed = collect_needed_raw_cols(
                    &raw_schema,
                    plan,
                    filters,
                    width.is_some(),
                    true,
                    false,
                    &mut spec.map_projections,
                )?;
                for f in &spec.filters {
                    collect_expr_map_projections(f, &raw_schema, &mut spec.map_projections);
                }
                spec.projection = Some(needed);
                let proj_schema = ctx
                    .traces
                    .projected_schema(&spec)
                    .map_err(query_err_to_run)?;
                let streams = ctx
                    .traces
                    .open_partitions(&spec, exec_ctx)
                    .map_err(query_err_to_run)?;
                (streams, proj_schema, true)
            }
        }
        WidgetSource::Spans => {
            let raw_schema = spans_raw_schema()?;
            let start_col = col_expr(&raw_schema, spans::START_TIME)?;
            let mut spec = ScanSpec {
                projection: None,
                map_projections: BTreeMap::new(),
                time_range: TimeRangeHint {
                    min_ns: Some(window.from()),
                    max_ns: window.to().checked_sub(1),
                    around_ns: None,
                },
                id_lookups: Vec::new(),
                filters: vec![
                    cmp_ts(&start_col, BinaryOp::GtEq, window.from()),
                    cmp_ts(&start_col, BinaryOp::Lt, window.to()),
                ],
                substrings: Vec::new(),
                sort_hints: Vec::new(),
                limit: None,
            };

            if uses_score_joins {
                let raw_streams = ctx
                    .spans
                    .open_partitions(&spec, exec_ctx)
                    .map_err(query_err_to_run)?;
                let view_schema = spans_view_schema()?;
                let mut streams: Vec<Box<dyn PartitionStream>> = raw_streams
                    .into_iter()
                    .map(|s| {
                        let proj = spans_view_projection(&raw_schema, exec_ctx.clone())?;
                        Ok(
                            Box::new(OperatorStream::new(s, vec![Box::new(proj)], exec_ctx))
                                as Box<dyn PartitionStream>,
                        )
                    })
                    .collect::<Result<Vec<_>, QueryError>>()
                    .map_err(query_err_to_run)?;
                streams = apply_widget_trace_semi_join(
                    ctx,
                    exec_ctx,
                    streams,
                    &view_schema,
                    &SemiJoinScope {
                        window,
                        include_eval,
                        include_experiments,
                        references_environment: filters.references_environment,
                        dashboard_text,
                    },
                )?;
                if let Some(wf) = &filters.widget {
                    streams = apply_compiled_filter(ctx, exec_ctx, streams, &view_schema, wf)
                        .map_err(query_err_to_run)?;
                }
                (streams, view_schema, false)
            } else {
                if let Some(wf) = &filters.widget {
                    let rewritten = rewrite_view_expr_for_raw(
                        &wf.expr,
                        &raw_schema,
                        &[spans::TRACE_ID, spans::SPAN_ID, spans::PARENT_SPAN_ID],
                        false,
                    )
                    .map_err(query_err_to_run)?;
                    spec.filters.push(rewritten);
                    populate_id_lookups(&mut spec);
                }
                let need_semi_join =
                    !(dashboard_text.is_none() && include_eval && include_experiments);
                let needed = collect_needed_raw_cols(
                    &raw_schema,
                    plan,
                    filters,
                    width.is_some(),
                    false,
                    need_semi_join,
                    &mut spec.map_projections,
                )?;
                for f in &spec.filters {
                    collect_expr_map_projections(f, &raw_schema, &mut spec.map_projections);
                }
                spec.projection = Some(needed);
                let proj_schema = ctx
                    .spans
                    .projected_schema(&spec)
                    .map_err(query_err_to_run)?;
                let raw_streams = ctx
                    .spans
                    .open_partitions(&spec, exec_ctx)
                    .map_err(query_err_to_run)?;
                let streams = apply_widget_trace_semi_join(
                    ctx,
                    exec_ctx,
                    raw_streams,
                    &proj_schema,
                    &SemiJoinScope {
                        window,
                        include_eval,
                        include_experiments,
                        references_environment: filters.references_environment,
                        dashboard_text,
                    },
                )?;
                (streams, proj_schema, true)
            }
        }
        WidgetSource::Scores => {
            let raw_schema = scores_raw_schema()?;
            let created_col = col_expr(&raw_schema, scores::CREATED_AT)?;
            let target_col = col_expr(&raw_schema, scores::TARGET)?;
            let source_col = col_expr(&raw_schema, scores::SOURCE)?;
            let mut spec = ScanSpec {
                projection: None,
                map_projections: BTreeMap::new(),
                time_range: TimeRangeHint {
                    min_ns: Some(window.from()),
                    max_ns: window.to().checked_sub(1),
                    around_ns: None,
                },
                id_lookups: Vec::new(),
                filters: vec![
                    cmp_ts(&created_col, BinaryOp::GtEq, window.from()),
                    cmp_ts(&created_col, BinaryOp::Lt, window.to()),
                    Expr::InList {
                        expr: Box::new(target_col),
                        list: vec![
                            Expr::Literal(ScalarValue::Utf8(
                                ScoreTargetKind::Trace.as_str().to_owned(),
                            )),
                            Expr::Literal(ScalarValue::Utf8(
                                ScoreTargetKind::Span.as_str().to_owned(),
                            )),
                        ],
                        negated: false,
                        redacted: false,
                    },
                    // Issue occurrences never show as score columns or briefs (D-IC-18).
                    Expr::Binary {
                        left: Box::new(source_col),
                        op: BinaryOp::NotEq,
                        right: Box::new(Expr::Literal(ScalarValue::Utf8(
                            ScoreSource::Issue.as_str().to_owned(),
                        ))),
                        data_type: DataType::Bool,
                    },
                ],
                substrings: Vec::new(),
                sort_hints: Vec::new(),
                limit: None,
            };
            if let Some(wf) = &filters.widget {
                let rewritten = rewrite_view_expr_for_raw(
                    &wf.expr,
                    &raw_schema,
                    &[scores::TRACE_ID, scores::SPAN_ID],
                    false,
                )
                .map_err(query_err_to_run)?;
                spec.filters.push(rewritten);
                populate_id_lookups(&mut spec);
            }
            let need_semi_join = !(dashboard_text.is_none() && include_eval && include_experiments);
            let needed = collect_needed_raw_cols(
                &raw_schema,
                plan,
                filters,
                width.is_some(),
                false,
                need_semi_join,
                &mut spec.map_projections,
            )?;
            for f in &spec.filters {
                collect_expr_map_projections(f, &raw_schema, &mut spec.map_projections);
            }
            spec.projection = Some(needed);
            let proj_schema = ctx
                .scores
                .projected_schema(&spec)
                .map_err(query_err_to_run)?;
            let raw_streams = ctx
                .scores
                .open_partitions(&spec, exec_ctx)
                .map_err(query_err_to_run)?;
            let streams = apply_widget_trace_semi_join(
                ctx,
                exec_ctx,
                raw_streams,
                &proj_schema,
                &SemiJoinScope {
                    window,
                    include_eval,
                    include_experiments,
                    references_environment: filters.references_environment,
                    dashboard_text,
                },
            )?;
            (streams, proj_schema, true)
        }
    };

    // Build pre-unnest projection: [bucket?, field_i?, where_i?, group_input?] (or dummy `_one` if empty).
    let (proj_exprs, proj_schema) =
        build_pre_unnest_projection(&base_schema, is_raw, width, plan, filters)?;

    if base_streams.is_empty() {
        let final_schema = post_unnest_schema(&proj_schema, plan)?;
        return Ok(PreparedStreams {
            streams: Vec::new(),
            schema: final_schema,
        });
    }

    let mut out_streams = Vec::with_capacity(base_streams.len());
    for stream in base_streams {
        let mut ops: Vec<Box<dyn PushOperator>> = Vec::new();
        let proj = ProjectOp::with_schema(
            proj_exprs.clone(),
            Arc::clone(&proj_schema),
            exec_ctx.clone(),
        )
        .map_err(query_err_to_run)?;
        ops.push(Box::new(proj));

        if let Some(group) = &plan.group {
            if group.list {
                let g_idx = col_index(&proj_schema, GROUP_INPUT)?;
                ops.push(Box::new(UnnestOp::new(
                    vec![g_idx],
                    false,
                    exec_ctx.clone(),
                )));
            }
            let group_col = Expr::Column {
                index: col_index(&proj_schema, GROUP_INPUT)?,
                name: GROUP_INPUT.to_owned(),
                data_type: DataType::Utf8,
            };
            let not_null = Expr::Unary {
                op: UnaryOp::IsNotNull,
                expr: Box::new(group_col),
                data_type: DataType::Bool,
            };
            ops.push(Box::new(FilterOp::new(not_null, exec_ctx.clone())));
        }

        out_streams
            .push(Box::new(OperatorStream::new(stream, ops, exec_ctx)) as Box<dyn PartitionStream>);
    }

    let final_schema = post_unnest_schema(&proj_schema, plan)?;
    Ok(PreparedStreams {
        streams: out_streams,
        schema: final_schema,
    })
}

fn post_unnest_schema(
    proj_schema: &Arc<Schema>,
    plan: &WidgetPlan,
) -> Result<Arc<Schema>, RunError> {
    let Some(group) = &plan.group else {
        return Ok(Arc::clone(proj_schema));
    };
    if !group.list {
        return Ok(Arc::clone(proj_schema));
    }
    let fields: Vec<Field> = proj_schema
        .fields()
        .iter()
        .map(|f| {
            if f.name == GROUP_INPUT {
                Field::new(GROUP_INPUT, DataType::Utf8, true)
            } else {
                f.clone()
            }
        })
        .collect();
    Schema::try_new_arc(fields).map_err(|e| query_err_to_run(e.into()))
}

fn apply_widget_trace_semi_join(
    ctx: &EngineContext,
    exec_ctx: &ExecContext,
    left_streams: Vec<Box<dyn PartitionStream>>,
    left_schema: &Arc<Schema>,
    scope: &SemiJoinScope<'_>,
) -> Result<Vec<Box<dyn PartitionStream>>, RunError> {
    let SemiJoinScope {
        window,
        include_eval,
        include_experiments,
        references_environment,
        dashboard_text,
    } = *scope;
    let left = crate::reads::scope::StreamSet {
        streams: left_streams,
        schema: Arc::clone(left_schema),
    };
    apply_trace_id_semi_join(
        ctx,
        exec_ctx,
        left,
        &TraceSemiJoinParams {
            lookback_ns: BucketWidth::D1.ns(),
            ends_after_from: true,
            scope: ListScope {
                window: Some(window),
                include_eval,
                include_experiments,
            },
            references_environment,
            dashboard_filter: dashboard_text,
        },
    )
}

fn collect_needed_raw_cols(
    raw_schema: &Schema,
    plan: &WidgetPlan,
    filters: &CompiledWidgetFilters,
    need_time: bool,
    is_traces: bool,
    need_trace_id: bool,
    map_projections: &mut BTreeMap<String, MapProjection>,
) -> Result<Vec<usize>, RunError> {
    let mut names = BTreeSet::new();
    if need_time {
        names.insert(time_column(plan.source).to_owned());
    }
    if need_trace_id {
        names.insert(traces::TRACE_ID.to_owned());
    }
    for m in &plan.measures {
        if let Some(f) = &m.field {
            add_field_cols(f, is_traces, &mut names, map_projections)?;
        }
    }
    for c in filters.conditions.iter().flatten() {
        collect_expr_cols(&c.expr, is_traces, &mut names)?;
    }
    if let Some(g) = &plan.group {
        add_field_cols(&g.field, is_traces, &mut names, map_projections)?;
    }
    if names.is_empty() {
        names.insert(time_column(plan.source).to_owned());
    }
    let mut indices = Vec::with_capacity(names.len());
    for name in names {
        indices.push(col_index(raw_schema, &name)?);
    }
    Ok(indices)
}

fn add_field_cols(
    field: &str,
    is_traces: bool,
    names: &mut BTreeSet<String>,
    map_projections: &mut BTreeMap<String, MapProjection>,
) -> Result<(), RunError> {
    if let Some((col_name, key)) = parse_map_field(field) {
        names.insert(col_name.to_owned());
        let proj = MapProjection::Keys(BTreeSet::from([key.to_owned()]));
        map_projections
            .entry(col_name.to_owned())
            .and_modify(|existing| existing.merge(proj.clone()))
            .or_insert(proj);
        return Ok(());
    }
    if is_traces && field == DURATION_NS {
        names.insert(traces::START_TIME.to_owned());
        names.insert(traces::END_TIME.to_owned());
        return Ok(());
    }
    names.insert(field.to_owned());
    Ok(())
}

fn collect_expr_cols(
    expr: &Expr,
    is_traces: bool,
    names: &mut BTreeSet<String>,
) -> Result<(), RunError> {
    match expr {
        Expr::Column { name, .. } => {
            if is_traces && (name == DURATION_NS || name == DURATION_MS) {
                names.insert(traces::START_TIME.to_owned());
                names.insert(traces::END_TIME.to_owned());
            } else if !is_traces && name == DURATION_MS {
                names.insert(spans::DURATION_NS.to_owned());
            } else {
                names.insert(name.clone());
            }
        }
        Expr::Literal(_) => {}
        Expr::Unary { expr, .. } | Expr::Cast { expr, .. } => {
            collect_expr_cols(expr, is_traces, names)?;
        }
        Expr::Binary { left, right, .. }
        | Expr::MapIndex {
            map: left,
            key: right,
        } => {
            collect_expr_cols(left, is_traces, names)?;
            collect_expr_cols(right, is_traces, names)?;
        }
        Expr::Between {
            expr, low, high, ..
        } => {
            collect_expr_cols(expr, is_traces, names)?;
            collect_expr_cols(low, is_traces, names)?;
            collect_expr_cols(high, is_traces, names)?;
        }
        Expr::InList { expr, list, .. } => {
            collect_expr_cols(expr, is_traces, names)?;
            for item in list {
                collect_expr_cols(item, is_traces, names)?;
            }
        }
        Expr::Like { expr, pattern, .. } => {
            collect_expr_cols(expr, is_traces, names)?;
            collect_expr_cols(pattern, is_traces, names)?;
        }
        Expr::Case {
            operand,
            when_then,
            else_expr,
            ..
        } => {
            if let Some(op) = operand {
                collect_expr_cols(op, is_traces, names)?;
            }
            for (w, t) in when_then {
                collect_expr_cols(w, is_traces, names)?;
                collect_expr_cols(t, is_traces, names)?;
            }
            if let Some(el) = else_expr {
                collect_expr_cols(el, is_traces, names)?;
            }
        }
        Expr::MakeArray { elems, .. } | Expr::Call { args: elems, .. } => {
            for e in elems {
                collect_expr_cols(e, is_traces, names)?;
            }
        }
    }
    Ok(())
}

fn build_pre_unnest_projection(
    base_schema: &Schema,
    is_raw: bool,
    width: Option<i64>,
    plan: &WidgetPlan,
    filters: &CompiledWidgetFilters,
) -> Result<(Vec<Expr>, Arc<Schema>), RunError> {
    let is_traces = plan.source == WidgetSource::Traces;
    let hex_cols: &[&str] = match plan.source {
        WidgetSource::Traces => &[traces::TRACE_ID, traces::ROOT_SPAN_ID],
        WidgetSource::Spans => &[spans::TRACE_ID, spans::SPAN_ID, spans::PARENT_SPAN_ID],
        WidgetSource::Scores => &[scores::TRACE_ID, scores::SPAN_ID],
    };

    let mut exprs = Vec::new();
    let mut fields = Vec::new();

    if let Some(w) = width {
        let t_col = col_expr(base_schema, time_column(plan.source))?;
        exprs.push(bucket_expr(w, t_col));
        fields.push(Field::new(BUCKET, DataType::timestamp_utc(), false));
    }

    for (i, m) in plan.measures.iter().enumerate() {
        if let Some(f) = &m.field {
            let expr = widget_field_expr(base_schema, f, is_raw && is_traces)?;
            let dt = expr.data_type();
            let name = format!("field_{i}");
            fields.push(Field::new(&name, dt, true));
            exprs.push(expr);
        }
        if let Some(cond) = &filters.conditions[i] {
            let remapped = if is_raw {
                rewrite_view_expr_for_raw(&cond.expr, base_schema, hex_cols, is_traces)
            } else {
                remap_expr_by_name(&cond.expr, base_schema)
            }
            .map_err(query_err_to_run)?;
            let name = format!("where_{i}");
            fields.push(Field::new(&name, DataType::Bool, true));
            exprs.push(remapped);
        }
    }

    if let Some(g) = &plan.group {
        let expr = widget_field_expr(base_schema, &g.field, is_raw && is_traces)?;
        let dt = expr.data_type();
        fields.push(Field::new(GROUP_INPUT, dt, true));
        exprs.push(expr);
    }

    if exprs.is_empty() {
        fields.push(Field::new(DUMMY_ONE, DataType::I64, false));
        exprs.push(Expr::Literal(ScalarValue::I64(1)));
    }

    let schema = Schema::try_new_arc(fields).map_err(|e| query_err_to_run(e.into()))?;
    Ok((exprs, schema))
}

fn widget_field_expr(
    schema: &Schema,
    field: &str,
    compute_trace_duration: bool,
) -> Result<Expr, RunError> {
    if let Some((col_name, key)) = parse_map_field(field) {
        let map_col = col_expr(schema, col_name)?;
        return Ok(Expr::MapIndex {
            map: Box::new(map_col),
            key: Box::new(Expr::Literal(ScalarValue::Utf8(key.to_owned()))),
        });
    }
    if compute_trace_duration && field == DURATION_NS {
        return duration_ns_from_bounds_expr(schema).map_err(query_err_to_run);
    }
    col_expr(schema, field)
}

fn parse_map_field(field: &str) -> Option<(&str, &str)> {
    let rest = field.strip_suffix("']")?;
    rest.split_once("['")
}

fn build_measure_aggs(schema: &Schema, plan: &WidgetPlan) -> Result<Vec<AggExpr>, RunError> {
    let mut aggs = Vec::with_capacity(plan.measures.len() + 1);
    for (i, m) in plan.measures.iter().enumerate() {
        let filter = if m.has_where {
            Some(col_expr(schema, &format!("where_{i}"))?)
        } else {
            None
        };
        let field_col = if m.field.is_some() {
            Some(col_expr(schema, &format!("field_{i}"))?)
        } else {
            None
        };
        let req_field = || {
            field_col
                .clone()
                .ok_or_else(|| internal("a measure without a field"))
        };
        let func = match m.agg {
            Agg::Count | Agg::Rate => AggFunc::CountStar,
            Agg::CountDistinct => AggFunc::CountDistinct(req_field()?),
            Agg::Sum if m.is_cost => AggFunc::SumCostDecimal(req_field()?),
            Agg::Sum => AggFunc::Sum(req_field()?),
            Agg::Avg => AggFunc::Avg(req_field()?),
            Agg::Min => AggFunc::Min(req_field()?),
            Agg::Max => AggFunc::Max(req_field()?),
            Agg::P50 => AggFunc::ApproxPercentileCont {
                expr: req_field()?,
                quantile: 0.5,
            },
            Agg::P75 => AggFunc::ApproxPercentileCont {
                expr: req_field()?,
                quantile: 0.75,
            },
            Agg::P90 => AggFunc::ApproxPercentileCont {
                expr: req_field()?,
                quantile: 0.90,
            },
            Agg::P95 => AggFunc::ApproxPercentileCont {
                expr: req_field()?,
                quantile: 0.95,
            },
            Agg::P99 => AggFunc::ApproxPercentileCont {
                expr: req_field()?,
                quantile: 0.99,
            },
        };
        aggs.push(AggExpr {
            func,
            filter,
            output_name: m.col_name.clone(),
        });
    }
    aggs.push(AggExpr {
        func: AggFunc::CountStar,
        filter: None,
        output_name: ROWS.to_owned(),
    });
    Ok(aggs)
}

#[derive(Clone)]
struct AggRow {
    time: i64,
    group: Option<String>,
    slot: Option<i64>,
    rows: u64,
    values: Vec<Option<f64>>,
}

fn finish_measure_values(plan: &WidgetPlan, raw: Vec<Option<f64>>, rows: u64) -> Vec<Option<f64>> {
    if rows == 0 {
        return vec![None; plan.measures.len()];
    }
    plan.measures
        .iter()
        .zip(raw)
        .map(|(m, v)| match m.agg {
            Agg::Rate => v.map(|matched| matched / rows as f64),
            _ => v,
        })
        .collect()
}

#[derive(Clone, Copy)]
struct AggKeyLayout {
    has_time: bool,
    has_group_str: bool,
    has_slot: bool,
}

fn read_agg_row(
    batch: &Batch,
    phys_row: usize,
    plan: &WidgetPlan,
    layout: AggKeyLayout,
) -> Result<AggRow, RunError> {
    let time = if layout.has_time {
        raw_time_at(require_col(batch, BUCKET)?, phys_row)?
    } else {
        0
    };
    let group = if layout.has_group_str {
        Some(
            raw_string_at(require_col(batch, GROUP_INPUT)?, phys_row)?
                .ok_or_else(|| internal("group key is null"))?,
        )
    } else {
        None
    };
    let slot = if layout.has_slot {
        Some(raw_i64_at(require_col(batch, GROUP_SLOT)?, phys_row)?)
    } else {
        None
    };
    let seen = u64::try_from(raw_i64_at(require_col(batch, ROWS)?, phys_row)?)
        .map_err(|_| internal("row count is negative"))?;
    let mut raw = Vec::with_capacity(plan.measures.len());
    for m in &plan.measures {
        let col = require_col(batch, &m.col_name)?;
        let val = if m.agg == Agg::Sum && m.is_cost {
            raw_decimal_to_f64_at(col, phys_row)?
        } else {
            raw_f64_at(col, phys_row)?
        };
        raw.push(val);
    }
    Ok(AggRow {
        time,
        group,
        slot,
        rows: seen,
        values: finish_measure_values(plan, raw, seen),
    })
}

fn run_agg_to_rows(
    streams: Vec<Box<dyn PartitionStream>>,
    schema: Arc<Schema>,
    agg_spec: HashAggSpec,
    plan: &WidgetPlan,
    exec_ctx: &ExecContext,
    layout: AggKeyLayout,
) -> Result<Vec<AggRow>, RunError> {
    if streams.is_empty() {
        if agg_spec.group_by.is_empty() {
            let agg = HashAgg::new(agg_spec, schema, exec_ctx.clone()).map_err(query_err_to_run)?;
            let batches = agg.into_batches().map_err(query_err_to_run)?;
            return read_agg_batches(&batches, plan, layout);
        }
        return Ok(Vec::new());
    }

    let sinks = exec_run_pipeline(exec_ctx, streams, |_| Ok(Vec::new()), {
        let spec_c = agg_spec.clone();
        let schema_c = Arc::clone(&schema);
        let ctx_c = exec_ctx.clone();
        move |_| HashAgg::new(spec_c.clone(), Arc::clone(&schema_c), ctx_c.clone())
    })
    .map_err(query_err_to_run)?;

    let mut stream = HashAgg::combine_sinks_to_stream(sinks, &agg_spec, &schema, exec_ctx)
        .map_err(query_err_to_run)?;
    let mut res = exec_ctx.pool.register("widget_agg_read", false);
    let mut rows = Vec::new();
    while let Some(batch) = stream
        .next_batch(&exec_ctx.eval.cancel, Some(&mut res))
        .map_err(query_err_to_run)?
    {
        for phys_row in active_rows(&batch) {
            rows.push(read_agg_row(&batch, phys_row, plan, layout)?);
        }
        drop(batch);
        res.free();
    }
    Ok(rows)
}

fn read_agg_batches(
    batches: &[Batch],
    plan: &WidgetPlan,
    layout: AggKeyLayout,
) -> Result<Vec<AggRow>, RunError> {
    let mut rows = Vec::new();
    for batch in batches {
        for phys_row in active_rows(batch) {
            rows.push(read_agg_row(batch, phys_row, plan, layout)?);
        }
    }
    Ok(rows)
}

/// Pass 1 for breakdown queries: spillable `HashAgg` grouped by `GROUP_INPUT`, keeping top `limit`
/// via bounded max-heap ordered by `(first measure DESC NULLS LAST, group ASC)`.
fn run_top_groups(
    prepared: PreparedStreams,
    plan: &WidgetPlan,
    limit: usize,
    exec_ctx: &ExecContext,
) -> Result<Vec<AggRow>, RunError> {
    if prepared.streams.is_empty() {
        return Ok(Vec::new());
    }
    let group_col = col_expr(&prepared.schema, GROUP_INPUT)?;
    let aggs = build_measure_aggs(&prepared.schema, plan)?;
    let agg_spec = HashAggSpec {
        group_by: vec![(GROUP_INPUT.to_owned(), group_col)],
        aggs,
    };

    let sinks = exec_run_pipeline(exec_ctx, prepared.streams, |_| Ok(Vec::new()), {
        let spec_c = agg_spec.clone();
        let schema_c = Arc::clone(&prepared.schema);
        let ctx_c = exec_ctx.clone();
        move |_| HashAgg::new(spec_c.clone(), Arc::clone(&schema_c), ctx_c.clone())
    })
    .map_err(query_err_to_run)?;

    let mut stream = HashAgg::combine_sinks_to_stream(sinks, &agg_spec, &prepared.schema, exec_ctx)
        .map_err(query_err_to_run)?;

    let mut heap: BinaryHeap<HeapGroupRow> = BinaryHeap::with_capacity(limit);
    let mut res = exec_ctx.pool.register("widget_top_groups", false);
    while let Some(batch) = stream
        .next_batch(&exec_ctx.eval.cancel, Some(&mut res))
        .map_err(query_err_to_run)?
    {
        for phys_row in active_rows(&batch) {
            let row = read_agg_row(
                &batch,
                phys_row,
                plan,
                AggKeyLayout {
                    has_time: false,
                    has_group_str: true,
                    has_slot: false,
                },
            )?;
            let item = HeapGroupRow(row);
            if heap.len() < limit {
                heap.push(item);
            } else if let Some(worst) = heap.peek()
                && cmp_top_group(&item.0, &worst.0) == Ordering::Less
            {
                let _ = heap.pop();
                heap.push(item);
            }
        }
        drop(batch);
        res.free();
    }

    let mut out: Vec<AggRow> = heap.into_iter().map(|h| h.0).collect();
    out.sort_by(cmp_top_group);
    Ok(out)
}

/// Best group compares `Ordering::Less` (`first_measure DESC NULLS LAST`, then `group ASC`).
fn cmp_top_group(a: &AggRow, b: &AggRow) -> Ordering {
    let ma = a.values.first().copied().flatten();
    let mb = b.values.first().copied().flatten();
    let ord = match (ma, mb) {
        (Some(va), Some(vb)) => cmp_f64_desc(va, vb),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    };
    ord.then_with(|| a.group.cmp(&b.group))
}

fn cmp_f64_desc(a: f64, b: f64) -> Ordering {
    let norm = |v: f64| if v == 0.0 { 0.0 } else { v };
    let (a, b) = (norm(a), norm(b));
    match (a.is_nan(), b.is_nan()) {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Greater,
        (false, true) => Ordering::Less,
        (false, false) => b.total_cmp(&a),
    }
}

struct HeapGroupRow(AggRow);

impl Eq for HeapGroupRow {}

impl PartialEq for HeapGroupRow {
    fn eq(&self, other: &Self) -> bool {
        cmp_top_group(&self.0, &other.0) == Ordering::Equal
    }
}

impl Ord for HeapGroupRow {
    fn cmp(&self, other: &Self) -> Ordering {
        // Max-heap keeps the WORST (Ordering::Greater in `cmp_top_group`) at root.
        cmp_top_group(&self.0, &other.0)
    }
}

impl PartialOrd for HeapGroupRow {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Wraps streams to classify `GROUP_INPUT` into `GROUP_SLOT` (`0..top_names.len()`, or `-1` for `other`).
/// When `include_other` is `false`, rows outside `top_names` are filtered out before classification.
fn attach_group_slot(
    prepared: PreparedStreams,
    top_names: &[String],
    include_other: bool,
    exec_ctx: &ExecContext,
) -> Result<PreparedStreams, RunError> {
    let group_col = col_expr(&prepared.schema, GROUP_INPUT)?;
    let slot_expr = Expr::Case {
        operand: None,
        when_then: top_names
            .iter()
            .enumerate()
            .map(|(idx, name)| {
                (
                    Expr::Binary {
                        left: Box::new(group_col.clone()),
                        op: BinaryOp::Eq,
                        right: Box::new(Expr::Literal(ScalarValue::Utf8(name.clone()))),
                        data_type: DataType::Bool,
                    },
                    Expr::Literal(ScalarValue::I64(idx as i64)),
                )
            })
            .collect(),
        else_expr: Some(Box::new(Expr::Literal(ScalarValue::I64(-1)))),
        data_type: DataType::I64,
    };

    let mut out_fields = Vec::with_capacity(prepared.schema.len() + 1);
    let mut proj_exprs = Vec::with_capacity(prepared.schema.len() + 1);
    for (idx, f) in prepared.schema.fields().iter().enumerate() {
        out_fields.push(f.clone());
        proj_exprs.push(Expr::Column {
            index: idx,
            name: f.name.clone(),
            data_type: f.data_type.clone(),
        });
    }
    out_fields.push(Field::new(GROUP_SLOT, DataType::I64, false));
    proj_exprs.push(slot_expr);
    let out_schema = Schema::try_new_arc(out_fields).map_err(|e| query_err_to_run(e.into()))?;

    let in_list_pred = if include_other {
        None
    } else {
        Some(Expr::InList {
            expr: Box::new(group_col),
            list: top_names
                .iter()
                .map(|n| Expr::Literal(ScalarValue::Utf8(n.clone())))
                .collect(),
            negated: false,
            redacted: false,
        })
    };

    let mut out_streams = Vec::with_capacity(prepared.streams.len());
    for stream in prepared.streams {
        let mut ops: Vec<Box<dyn PushOperator>> = Vec::new();
        if let Some(pred) = &in_list_pred {
            ops.push(Box::new(FilterOp::new(pred.clone(), exec_ctx.clone())));
        }
        let proj = ProjectOp::with_schema(
            proj_exprs.clone(),
            Arc::clone(&out_schema),
            exec_ctx.clone(),
        )
        .map_err(query_err_to_run)?;
        ops.push(Box::new(proj));
        out_streams
            .push(Box::new(OperatorStream::new(stream, ops, exec_ctx)) as Box<dyn PartitionStream>);
    }
    Ok(PreparedStreams {
        streams: out_streams,
        schema: out_schema,
    })
}

fn run_series(
    ctx: &EngineContext,
    exec_ctx: &ExecContext,
    window: TimeWindow,
    bucket_ns: i64,
    cfg: &WidgetScopeCfg<'_>,
) -> Result<WidgetData, RunError> {
    let plan = cfg.plan;
    let mut lines = Vec::new();
    match &plan.group {
        None => {
            let prepared = prepare_widget_streams(ctx, exec_ctx, window, Some(bucket_ns), cfg)?;
            let bucket_col = col_expr(&prepared.schema, BUCKET)?;
            let aggs = build_measure_aggs(&prepared.schema, plan)?;
            let agg_spec = HashAggSpec {
                group_by: vec![(BUCKET.to_owned(), bucket_col)],
                aggs,
            };
            let rows = run_agg_to_rows(
                prepared.streams,
                prepared.schema,
                agg_spec,
                plan,
                exec_ctx,
                AggKeyLayout {
                    has_time: true,
                    has_group_str: false,
                    has_slot: false,
                },
            )?;
            let refs: Vec<&AggRow> = rows.iter().collect();
            lines.extend(lines_of(plan, None, false, &refs));
        }
        Some(group) => {
            // Pass 1: top groups (no time bucket needed for breakdown ranking).
            let p1 = prepare_widget_streams(ctx, exec_ctx, window, None, cfg)?;
            let top = run_top_groups(p1, plan, group.limit, exec_ctx)?;
            let top_names: Vec<String> = top.iter().filter_map(|r| r.group.clone()).collect();
            if !top_names.is_empty() {
                // Pass 2: 1 unified scan over non-null groups classifying `top_names` + optional `other`.
                let p2 = prepare_widget_streams(ctx, exec_ctx, window, Some(bucket_ns), cfg)?;
                let slotted = attach_group_slot(p2, &top_names, group.other, exec_ctx)?;
                let bucket_col = col_expr(&slotted.schema, BUCKET)?;
                let slot_col = col_expr(&slotted.schema, GROUP_SLOT)?;
                let aggs = build_measure_aggs(&slotted.schema, plan)?;
                let agg_spec = HashAggSpec {
                    group_by: vec![
                        (BUCKET.to_owned(), bucket_col),
                        (GROUP_SLOT.to_owned(), slot_col),
                    ],
                    aggs,
                };
                let rows = run_agg_to_rows(
                    slotted.streams,
                    slotted.schema,
                    agg_spec,
                    plan,
                    exec_ctx,
                    AggKeyLayout {
                        has_time: true,
                        has_group_str: false,
                        has_slot: true,
                    },
                )?;

                for (idx, name) in top_names.iter().enumerate() {
                    let target = idx as i64;
                    let mine: Vec<&AggRow> =
                        rows.iter().filter(|r| r.slot == Some(target)).collect();
                    lines.extend(lines_of(plan, Some(name.as_str()), false, &mine));
                }
                if group.other {
                    let rest: Vec<&AggRow> = rows.iter().filter(|r| r.slot == Some(-1)).collect();
                    if !rest.is_empty() {
                        lines.extend(lines_of(plan, None, true, &rest));
                    }
                }
            }
        }
    }
    Ok(WidgetData::Series {
        bucket_ns,
        series: lines,
    })
}

fn lines_of(
    plan: &WidgetPlan,
    group: Option<&str>,
    other: bool,
    rows: &[&AggRow],
) -> Vec<WidgetSeries> {
    let mut ordered = rows.to_vec();
    ordered.sort_by_key(|row| row.time);
    (0..plan.measures.len())
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

fn run_total(
    ctx: &EngineContext,
    exec_ctx: &ExecContext,
    window: TimeWindow,
    compare: bool,
    cfg: &WidgetScopeCfg<'_>,
) -> Result<WidgetData, RunError> {
    let plan = cfg.plan;
    let current_prepared = prepare_widget_streams(ctx, exec_ctx, window, None, cfg)?;

    let prev_win = if compare {
        Some(previous_window(window)?)
    } else {
        None
    };

    let Some(group) = &plan.group else {
        let aggs = build_measure_aggs(&current_prepared.schema, plan)?;
        let agg_spec = HashAggSpec {
            group_by: Vec::new(),
            aggs,
        };
        let cur_rows = run_agg_to_rows(
            current_prepared.streams,
            current_prepared.schema,
            agg_spec,
            plan,
            exec_ctx,
            AggKeyLayout {
                has_time: false,
                has_group_str: false,
                has_slot: false,
            },
        )?;
        let cur = cur_rows.into_iter().next().unwrap_or_else(|| AggRow {
            time: 0,
            group: None,
            slot: None,
            rows: 0,
            values: vec![None; plan.measures.len()],
        });

        let previous = match prev_win {
            Some(pw) => {
                let prev_prep = prepare_widget_streams(ctx, exec_ctx, pw, None, cfg)?;
                let prev_aggs = build_measure_aggs(&prev_prep.schema, plan)?;
                let prev_spec = HashAggSpec {
                    group_by: Vec::new(),
                    aggs: prev_aggs,
                };
                let prev_rows = run_agg_to_rows(
                    prev_prep.streams,
                    prev_prep.schema,
                    prev_spec,
                    plan,
                    exec_ctx,
                    AggKeyLayout {
                        has_time: false,
                        has_group_str: false,
                        has_slot: false,
                    },
                )?;
                Some(
                    prev_rows
                        .into_iter()
                        .next()
                        .map_or_else(|| vec![None; plan.measures.len()], |r| r.values),
                )
            }
            None => None,
        };

        return Ok(WidgetData::Total {
            rows: vec![TotalRow {
                group: None,
                other: false,
                values: cur.values,
                previous,
            }],
        });
    };

    let groups = run_top_groups(current_prepared, plan, group.limit, exec_ctx)?;
    let top_names: Vec<String> = groups.iter().filter_map(|r| r.group.clone()).collect();
    if top_names.is_empty() {
        if let Some(pw) = prev_win {
            // Check semi-join lookback bounds for previous window when a semi-join applies.
            let need_semi_join = matches!(plan.source, WidgetSource::Spans | WidgetSource::Scores)
                && !(cfg.dashboard_text.is_none() && cfg.include_eval && cfg.include_experiments);
            if need_semi_join {
                let _ = pw
                    .from()
                    .checked_sub(BucketWidth::D1.ns())
                    .ok_or_else(out_of_range)?;
            }
        }
        return Ok(WidgetData::Total { rows: Vec::new() });
    }

    let other_cur = if group.other && groups.len() == group.limit {
        let p2 = prepare_widget_streams(ctx, exec_ctx, window, None, cfg)?;
        let g_col = col_expr(&p2.schema, GROUP_INPUT)?;
        let not_in = Expr::InList {
            expr: Box::new(g_col),
            list: top_names
                .iter()
                .map(|n| Expr::Literal(ScalarValue::Utf8(n.clone())))
                .collect(),
            negated: true,
            redacted: false,
        };
        let rest_streams: Vec<Box<dyn PartitionStream>> = p2
            .streams
            .into_iter()
            .map(|s| {
                Box::new(OperatorStream::new(
                    s,
                    vec![Box::new(FilterOp::new(not_in.clone(), exec_ctx.clone()))],
                    exec_ctx,
                )) as Box<dyn PartitionStream>
            })
            .collect();
        let aggs = build_measure_aggs(&p2.schema, plan)?;
        let agg_spec = HashAggSpec {
            group_by: Vec::new(),
            aggs,
        };
        let rows = run_agg_to_rows(
            rest_streams,
            p2.schema,
            agg_spec,
            plan,
            exec_ctx,
            AggKeyLayout {
                has_time: false,
                has_group_str: false,
                has_slot: false,
            },
        )?;
        rows.into_iter().next().filter(|r| r.rows > 0)
    } else {
        None
    };

    let prev_map: Option<BTreeMap<i64, Vec<Option<f64>>>> = match prev_win {
        Some(pw) => {
            let prev_prep = prepare_widget_streams(ctx, exec_ctx, pw, None, cfg)?;
            let slotted = attach_group_slot(prev_prep, &top_names, other_cur.is_some(), exec_ctx)?;
            let slot_col = col_expr(&slotted.schema, GROUP_SLOT)?;
            let aggs = build_measure_aggs(&slotted.schema, plan)?;
            let agg_spec = HashAggSpec {
                group_by: vec![(GROUP_SLOT.to_owned(), slot_col)],
                aggs,
            };
            let prev_rows = run_agg_to_rows(
                slotted.streams,
                slotted.schema,
                agg_spec,
                plan,
                exec_ctx,
                AggKeyLayout {
                    has_time: false,
                    has_group_str: false,
                    has_slot: true,
                },
            )?;
            let mut map = BTreeMap::new();
            for r in prev_rows {
                if let Some(s) = r.slot {
                    map.insert(s, r.values);
                }
            }
            Some(map)
        }
        None => None,
    };

    let mut rows = Vec::with_capacity(groups.len() + usize::from(other_cur.is_some()));
    for (idx, row) in groups.into_iter().enumerate() {
        let previous = prev_map.as_ref().map(|m| {
            m.get(&(idx as i64))
                .cloned()
                .unwrap_or_else(|| vec![None; plan.measures.len()])
        });
        rows.push(TotalRow {
            group: row.group,
            other: false,
            values: row.values,
            previous,
        });
    }
    if let Some(other) = other_cur {
        let previous = prev_map.as_ref().map(|m| {
            m.get(&-1)
                .cloned()
                .unwrap_or_else(|| vec![None; plan.measures.len()])
        });
        rows.push(TotalRow {
            group: None,
            other: true,
            values: other.values,
            previous,
        });
    }

    Ok(WidgetData::Total { rows })
}

fn out_of_range() -> RunError {
    RunError::InvalidRequest("from is out of range".to_owned())
}

fn previous_window(window: TimeWindow) -> Result<TimeWindow, RunError> {
    let length = window
        .to()
        .checked_sub(window.from())
        .ok_or_else(out_of_range)?;
    let start = window.from().checked_sub(length).ok_or_else(out_of_range)?;
    TimeWindow::new(start, window.from())
}

fn col_index(schema: &Schema, name: &str) -> Result<usize, RunError> {
    schema
        .index_of(name)
        .ok_or_else(|| internal(format!("missing column {name} in schema")))
}

fn col_expr(schema: &Schema, name: &str) -> Result<Expr, RunError> {
    let idx = col_index(schema, name)?;
    Ok(Expr::Column {
        index: idx,
        name: name.to_owned(),
        data_type: schema.fields()[idx].data_type.clone(),
    })
}

fn cmp_ts(col: &Expr, op: BinaryOp, ns: i64) -> Expr {
    Expr::Binary {
        left: Box::new(col.clone()),
        op,
        right: Box::new(Expr::Literal(ScalarValue::TimestampNs {
            value: ns,
            utc: true,
        })),
        data_type: DataType::Bool,
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use bytesize::ByteSize;
    use cetra_core::enums::{ScoreSource, SpanKind, SpanStatus};
    use cetra_core::ids::ProjectId;
    use cetra_core::names::environment;
    use cetra_core::score::ScoreValue;

    use super::*;
    use crate::QueryOptions;
    use crate::config::{
        FlushConfig, MemtableConfig, QueryConfig, S3Config, StorageConfig, WalConfig,
    };
    use crate::test_support::{score_row, score_rows, span_row, span_rows};
    use crate::views::QueryContext;

    const T0: i64 = 1_700_000_000_000_000_000;
    const MIN: i64 = 60_000_000_000;

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

    fn window() -> TimeWindow {
        TimeWindow::new(T0, T0 + 60 * MIN).expect("window")
    }

    fn measure(agg: Agg, field: Option<&str>, condition: Option<&str>) -> Measure {
        Measure {
            agg,
            field: field.map(str::to_owned),
            condition: condition.map(str::to_owned),
        }
    }

    #[test]
    fn shape_and_expression_validation() {
        let ctx = QueryContext::new(crate::test_support::example_query(), None).expect("ctx");

        let mut bad = WidgetQuery {
            source: WidgetSource::Traces,
            filter: None,
            measures: vec![
                measure(Agg::Count, Some("name"), None),
                measure(Agg::Sum, None, None),
                measure(Agg::Rate, None, None),
                measure(Agg::CountDistinct, Some("tags"), None),
                measure(Agg::CountDistinct, Some("metadata['a'b']"), None),
            ],
            breakdown: Some(Breakdown {
                field: "attributes['k']".to_owned(),
                limit: 0,
                other: true,
            }),
        };
        let err = validate_shape(&bad).expect_err("shape");
        match err {
            WidgetError::Fields(fields) => {
                assert_eq!(fields.len(), 7);
                assert_eq!(fields[0].path, "measures[0].field");
                assert_eq!(fields[1].path, "measures[1].field");
                assert_eq!(fields[2].path, "measures[2].where");
                assert_eq!(fields[3].path, "measures[3].field");
                assert_eq!(fields[4].path, "measures[4].field");
                assert_eq!(fields[5].path, "breakdown.field");
                assert_eq!(fields[6].path, "breakdown.limit");
            }
            other => panic!("unexpected: {other:?}"),
        }

        // Valid CountDistinct on identity + non-list breakdown.
        bad.measures = vec![
            measure(Agg::CountDistinct, Some("trace_id"), None),
            measure(Agg::CountDistinct, Some("status"), None),
        ];
        bad.breakdown = Some(Breakdown {
            field: "metadata['k']".to_owned(),
            limit: 5,
            other: false,
        });
        assert!(validate_shape(&bad).is_ok());

        // validate_expressions forbids score() in where.
        let with_score_where = WidgetQuery {
            source: WidgetSource::Traces,
            filter: Some("status = 'ok'".to_owned()),
            measures: vec![measure(Agg::Count, None, Some("score('quality') > 0.5"))],
            breakdown: None,
        };
        match validate_expressions(&ctx, &with_score_where, None) {
            Err(WidgetError::Expression { path, error }) => {
                assert_eq!(path, "measures[0].where");
                assert_eq!(error.message, "a where cannot call score functions");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn widgets_null_exclusion_zero_rows_and_compare() {
        let dir = tempfile::tempdir().expect("tempdir");
        let storage = crate::Storage::open(test_config(dir.path()))
            .await
            .expect("open");
        let project: ProjectId = "018f6b4e-7c1a-7c1a-8c1a-000000000001".parse().expect("pid");

        let mut spans = Vec::new();
        // Current window: [T0, T0 + 60m)
        // Models: m1 (3 rows), m2 (2 rows), m3 (1 row), NULL model (4 rows - must NOT enter `other`!)
        type SpecRow<'a> = (u16, i64, Option<&'a str>, Vec<&'a str>, f64, bool);
        let spec_rows: [SpecRow<'_>; 10] = [
            (1, 5, Some("m1"), vec!["a", "b"], 0.1, false),
            (2, 10, Some("m1"), vec!["a"], 0.2, true),
            (3, 15, Some("m1"), vec!["b"], 0.3, false),
            (4, 20, Some("m2"), vec!["c"], 0.4, false),
            (5, 25, Some("m2"), vec!["c"], 0.5, true),
            (6, 30, Some("m3"), vec!["d"], 0.6, false),
            (7, 35, None, vec![], 0.7, false),
            (8, 40, None, vec![], 0.8, false),
            (9, 45, None, vec![], 0.9, false),
            (10, 50, None, vec![], 1.0, false),
        ];
        for (t, at_min, model, tags, cost, is_err) in spec_rows {
            let mut s = span_row(t, 0, T0 + at_min * MIN, T0 + at_min * MIN);
            s.span_kind = SpanKind::Llm;
            s.response_model = model.map(str::to_owned);
            s.tags = tags.into_iter().map(str::to_owned).collect();
            s.cost_total = Some(cost);
            s.end_time = s.start_time + i64::from(t) * 1_000_000;
            if is_err {
                s.status = SpanStatus::Error;
            }
            s.environment = Some("prod".to_owned());
            spans.push(s);
        }

        // Previous window: [T0 - 60m, T0)
        for (t, at_min, model, tags, cost) in [
            (20u16, -50, Some("m1"), vec!["a"], 0.25),
            (21, -40, Some("m4"), vec!["z"], 0.5),
        ] {
            let mut s = span_row(t, 0, T0 + at_min * MIN, T0 + at_min * MIN);
            s.span_kind = SpanKind::Llm;
            s.response_model = model.map(str::to_owned);
            s.tags = tags.into_iter().map(str::to_owned).collect();
            s.cost_total = Some(cost);
            s.end_time = s.start_time + 5_000_000;
            s.environment = Some("prod".to_owned());
            spans.push(s);
        }

        // Eval span in window (excluded unless filter names environment or include_eval = true).
        let mut eval_s = span_row(30, 0, T0 + 5 * MIN, T0 + 5 * MIN);
        eval_s.environment = Some(environment::EVAL.to_owned());
        eval_s.response_model = Some("eval-model".to_owned());
        spans.push(eval_s);

        // Scores: trace score + issue score (D-IC-18 must exclude issue score on WidgetSource::Scores).
        let mut sc_api = score_row(1, 1, T0 + 5 * MIN, T0 + 5 * MIN, T0 + 5 * MIN, false, "q");
        sc_api.value = ScoreValue::Numeric(0.8);
        let mut sc_issue = score_row(
            2,
            2,
            T0 + 10 * MIN,
            T0 + 10 * MIN,
            T0 + 10 * MIN,
            false,
            "q",
        );
        sc_issue.source = ScoreSource::Issue;
        sc_issue.value = ScoreValue::Numeric(0.1);

        storage
            .append_spans(project, span_rows(&spans))
            .await
            .expect("append spans");
        storage
            .append_scores(project, score_rows(&[sc_api, sc_issue]))
            .await
            .expect("append scores");

        let opts = QueryOptions::new(None, None);
        let ctx = QueryContext::new(storage.query(project, opts).await.expect("query"), None)
            .expect("ctx");

        // 1. Breakdown by `model` with limit=1 and `other=true`: NULL model rows MUST NOT enter `other`.
        let q_model = WidgetQuery {
            source: WidgetSource::Spans,
            filter: None,
            measures: vec![
                measure(Agg::Count, None, None),
                measure(Agg::Sum, Some("cost_total"), None),
                measure(Agg::Rate, None, Some("status = 'error'")),
                measure(Agg::P95, Some("duration_ns"), None),
                measure(Agg::CountDistinct, Some("trace_id"), None),
            ],
            breakdown: Some(Breakdown {
                field: "model".to_owned(),
                limit: 1,
                other: true,
            }),
        };
        let req_total_cmp = WidgetRequest {
            window: window(),
            bucket: Bucket::Fixed(BucketWidth::M15),
            dashboard_filter: None,
            group_by: None,
            include_eval: false,
            include_experiments: false,
            query: &q_model,
            shape: Shape::Total,
            compare: true,
            timeout: Duration::from_secs(10),
        };
        let qe_t = widget_query(&ctx, req_total_cmp).await.expect("total");
        match &qe_t {
            WidgetData::Total { rows } => {
                // m1 (3) + other (m2: 2 + m3: 1 = 3), NULLs (4) excluded!
                assert_eq!(rows.len(), 2);
                assert_eq!(rows[0].group.as_deref(), Some("m1"));
                assert_eq!(rows[0].values[0], Some(3.0));
                assert!(rows[1].other);
                assert_eq!(rows[1].values[0], Some(3.0));
                assert_eq!(rows[1].previous.as_ref().expect("prev")[0], Some(1.0));
            }
            other => panic!("expected Total, got {other:?}"),
        }

        // 2. Unified Pass 2 Series with `other=true`.
        let req_series = WidgetRequest {
            shape: Shape::Series,
            compare: false,
            ..req_total_cmp
        };
        let qe_s = widget_query(&ctx, req_series).await.expect("series");
        assert!(matches!(
            qe_s,
            WidgetData::Series {
                bucket_ns: 900_000_000_000,
                ref series
            } if !series.is_empty()
        ));

        // 3. List breakdown (`tags`) with measure `where` evaluated BEFORE unnest.
        let q_tags = WidgetQuery {
            source: WidgetSource::Spans,
            filter: None,
            measures: vec![
                measure(Agg::Count, None, None),
                measure(Agg::Count, None, Some("tags @> ['b']")),
            ],
            breakdown: Some(Breakdown {
                field: "tags".to_owned(),
                limit: 2,
                other: true,
            }),
        };
        let req_tags = WidgetRequest {
            query: &q_tags,
            shape: Shape::Total,
            compare: true,
            ..req_total_cmp
        };
        let qe_tags = widget_query(&ctx, req_tags).await.expect("tags");
        assert!(matches!(qe_tags, WidgetData::Total { ref rows } if !rows.is_empty()));

        // 4. Empty window (`rows == 0`) -> `None` even for `Count`/`CountDistinct`, vs `rows > 0 && where == false` -> `Some(0.0)`.
        let q_zero = WidgetQuery {
            source: WidgetSource::Traces,
            filter: None,
            measures: vec![
                measure(Agg::Count, None, Some("name = 'never-matches'")),
                measure(
                    Agg::CountDistinct,
                    Some("user_id"),
                    Some("name = 'never-matches'"),
                ),
                measure(Agg::Rate, None, Some("name = 'never-matches'")),
            ],
            breakdown: None,
        };
        let req_nonzero_unmatched = WidgetRequest {
            query: &q_zero,
            shape: Shape::Total,
            compare: false,
            ..req_total_cmp
        };
        let qe_nz = widget_query(&ctx, req_nonzero_unmatched).await.expect("nz");
        if let WidgetData::Total { rows } = &qe_nz {
            assert_eq!(rows[0].values, vec![Some(0.0), Some(0.0), Some(0.0)]);
        }

        let empty_win = TimeWindow::new(T0 + 500 * MIN, T0 + 560 * MIN).expect("empty");
        let req_empty = WidgetRequest {
            window: empty_win,
            ..req_nonzero_unmatched
        };
        let qe_empty = widget_query(&ctx, req_empty).await.expect("empty");
        if let WidgetData::Total { rows } = &qe_empty {
            assert_eq!(rows[0].values, vec![None, None, None]);
        }

        // 5. Empty `other` omission when limit >= distinct non-null groups.
        let q_no_other = WidgetQuery {
            breakdown: Some(Breakdown {
                field: "model".to_owned(),
                limit: 10,
                other: true,
            }),
            ..q_model.clone()
        };
        let req_no_other = WidgetRequest {
            query: &q_no_other,
            shape: Shape::Total,
            compare: false,
            ..req_total_cmp
        };
        let qe_no = widget_query(&ctx, req_no_other).await.expect("no other");
        if let WidgetData::Total { rows } = &qe_no {
            assert!(rows.iter().all(|r| !r.other));
        }

        // 6. Scores source excludes `source = 'issue'` (D-IC-18).
        let q_scores = WidgetQuery {
            source: WidgetSource::Scores,
            filter: None,
            measures: vec![
                measure(Agg::Count, None, None),
                measure(Agg::Avg, Some("value_num"), None),
            ],
            breakdown: None,
        };
        let req_scores = WidgetRequest {
            query: &q_scores,
            shape: Shape::Total,
            compare: false,
            ..req_total_cmp
        };
        let qe_sc = widget_query(&ctx, req_scores).await.expect("scores");
        if let WidgetData::Total { rows } = &qe_sc {
            assert_eq!(rows[0].values, vec![Some(1.0), Some(0.8)]);
        }

        // 7. Spans widget_filter referencing `environment = 'cetra-eval'` with `include_eval = false`
        //    and `include_experiments = true`.
        let q_eval = WidgetQuery {
            source: WidgetSource::Spans,
            filter: Some("environment = 'cetra-eval'".to_owned()),
            measures: vec![measure(Agg::Count, None, None)],
            breakdown: None,
        };
        let req_eval = WidgetRequest {
            query: &q_eval,
            shape: Shape::Total,
            compare: false,
            include_eval: false,
            include_experiments: true,
            ..req_total_cmp
        };
        let qe_ev = widget_query(&ctx, req_eval).await.expect("eval");
        if let WidgetData::Total { rows } = &qe_ev {
            assert_eq!(rows[0].values, vec![Some(1.0)]);
        }

        // 8. `GroupBy::Model` on Traces (list field `models`) + stats + `score()` dashboard_filter.
        let q_traces_stats = WidgetQuery {
            source: WidgetSource::Traces,
            filter: None,
            measures: vec![
                measure(Agg::Min, Some("duration_ns"), None),
                measure(Agg::Max, Some("duration_ns"), None),
                measure(Agg::Avg, Some("duration_ns"), None),
                measure(Agg::P50, Some("duration_ns"), None),
                measure(Agg::P75, Some("duration_ns"), None),
                measure(Agg::P90, Some("duration_ns"), None),
            ],
            breakdown: None,
        };
        let req_stats = WidgetRequest {
            query: &q_traces_stats,
            shape: Shape::Series,
            bucket: Bucket::Auto,
            group_by: Some(GroupBy::Model),
            dashboard_filter: Some("score('q') > 0.5"),
            compare: false,
            ..req_total_cmp
        };
        let qe_st = widget_query(&ctx, req_stats).await.expect("stats");
        assert!(matches!(qe_st, WidgetData::Series { ref series, .. } if !series.is_empty()));

        // 9. `WidgetSource::Scores` + `CountDistinct(trace_id)` with `score(...)` `dashboard_filter`
        // (regression test for preserving `FixedBytes16` `trace_id` on `left` in semi-join).
        let q_scores_cd = WidgetQuery {
            source: WidgetSource::Scores,
            filter: None,
            measures: vec![measure(Agg::CountDistinct, Some("trace_id"), None)],
            breakdown: None,
        };
        let req_scores_cd = WidgetRequest {
            query: &q_scores_cd,
            shape: Shape::Total,
            dashboard_filter: Some("score('q') > 0.5"),
            compare: false,
            ..req_total_cmp
        };
        let qe_cd = widget_query(&ctx, req_scores_cd).await.expect("scores cd");
        if let WidgetData::Total { rows } = &qe_cd {
            assert_eq!(rows[0].values, vec![Some(1.0)]);
        }

        // 10. `from` underflow on `previous_window` and 1-day lookback -> InvalidRequest("from is out of range").
        let underflow_win = TimeWindow::new(i64::MIN + 10, i64::MIN + 100).expect("min win");
        let req_underflow = WidgetRequest {
            window: underflow_win,
            query: &q_zero,
            shape: Shape::Total,
            compare: true,
            ..req_total_cmp
        };
        let err = widget_query(&ctx, req_underflow)
            .await
            .expect_err("underflow");
        assert!(matches!(
            err,
            WidgetError::Run(RunError::InvalidRequest(ref m)) if m == "from is out of range"
        ));

        let _ = storage.shutdown().await;
    }
}
