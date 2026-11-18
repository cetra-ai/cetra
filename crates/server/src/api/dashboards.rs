//! Custom dashboards: their definitions and the CRUD bodies.
//!
//! Spec 04 § Dashboards, spec 05 § Dashboard queries, spec 09. The widget query types are
//! `pub(crate)` because the query endpoint (CD2) reads them too.

use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use super::common::Limit;
use super::metrics::{MetricsBucket, MetricsGroupBy};

/// The view a widget query reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum WidgetSourceBody {
    /// One row per trace.
    Traces,
    /// One row per span.
    Spans,
    /// One row per live score.
    Scores,
}

/// How a measure aggregates its rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum AggBody {
    /// Number of rows.
    Count,
    /// Number of distinct values of a field.
    CountDistinct,
    /// Sum of a numeric field.
    Sum,
    /// Mean of a numeric field.
    Avg,
    /// Smallest value of a numeric field.
    Min,
    /// Largest value of a numeric field.
    Max,
    /// 50th percentile of a numeric field.
    P50,
    /// 75th percentile of a numeric field.
    P75,
    /// 90th percentile of a numeric field.
    P90,
    /// 95th percentile of a numeric field.
    P95,
    /// 99th percentile of a numeric field.
    P99,
    /// Share of the rows that match `where`, from 0 to 1.
    Rate,
}

/// One measure of a widget query.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct MeasureBody {
    /// How the rows aggregate.
    pub agg: AggBody,
    /// The field aggregated. Absent for `count` and `rate`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
    /// A filter over the source view. Required for `rate`.
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "where")]
    pub condition: Option<String>,
    /// Name shown in the legend, 0 to 60 characters. Not part of the query.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// The split of a widget query by one field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct BreakdownBody {
    /// The field the rows are grouped by.
    pub field: String,
    /// Groups kept, 1 to 20. Default 5.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
    /// Whether the rows of every other value form one more group. Default true.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub other: Option<bool>,
}

/// A widget's query: one source, its measures and an optional breakdown.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct WidgetQueryBody {
    /// The view the query reads.
    pub source: WidgetSourceBody,
    /// A filter over the source view.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter: Option<String>,
    /// One to six measures.
    pub measures: Vec<MeasureBody>,
    /// The split, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub breakdown: Option<BreakdownBody>,
}

impl WidgetQueryBody {
    /// The query the storage layer checks and runs. Labels stay here; the default breakdown
    /// limit and `other` are applied.
    pub(crate) fn to_storage(&self) -> cetra_storage::WidgetQuery {
        cetra_storage::WidgetQuery {
            source: match self.source {
                WidgetSourceBody::Traces => cetra_storage::WidgetSource::Traces,
                WidgetSourceBody::Spans => cetra_storage::WidgetSource::Spans,
                WidgetSourceBody::Scores => cetra_storage::WidgetSource::Scores,
            },
            filter: self.filter.clone(),
            measures: self
                .measures
                .iter()
                .map(|measure| cetra_storage::Measure {
                    agg: match measure.agg {
                        AggBody::Count => cetra_storage::Agg::Count,
                        AggBody::CountDistinct => cetra_storage::Agg::CountDistinct,
                        AggBody::Sum => cetra_storage::Agg::Sum,
                        AggBody::Avg => cetra_storage::Agg::Avg,
                        AggBody::Min => cetra_storage::Agg::Min,
                        AggBody::Max => cetra_storage::Agg::Max,
                        AggBody::P50 => cetra_storage::Agg::P50,
                        AggBody::P75 => cetra_storage::Agg::P75,
                        AggBody::P90 => cetra_storage::Agg::P90,
                        AggBody::P95 => cetra_storage::Agg::P95,
                        AggBody::P99 => cetra_storage::Agg::P99,
                        AggBody::Rate => cetra_storage::Agg::Rate,
                    },
                    field: measure.field.clone(),
                    condition: measure.condition.clone(),
                })
                .collect(),
            breakdown: self
                .breakdown
                .as_ref()
                .map(|breakdown| cetra_storage::Breakdown {
                    field: breakdown.field.clone(),
                    limit: breakdown
                        .limit
                        .unwrap_or(cetra_storage::DEFAULT_BREAKDOWN_LIMIT),
                    other: breakdown.other.unwrap_or(true),
                }),
        }
    }
}

/// Whether a widget query answers one value per bucket or one value over the range.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum WidgetShape {
    /// One value per bucket.
    Series,
    /// One value over `[from, to)`.
    Total,
}

/// Body of `POST /api/v1/projects/{project}/dashboards/query`. Spec 05 § Dashboard queries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct WidgetQueryRequest {
    /// Inclusive range start, RFC 3339.
    #[schema(value_type = String, format = DateTime)]
    pub from: String,
    /// Exclusive range end, RFC 3339.
    #[schema(value_type = String, format = DateTime)]
    pub to: String,
    /// Bucket width. Defaults to `auto`. Ignored for `total`.
    #[serde(default)]
    pub bucket: MetricsBucket,
    /// The dashboard's filter, a traces expression.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter: Option<String>,
    /// The dashboard's split. Ignored when the query has its own breakdown, and for scores.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_by: Option<MetricsGroupBy>,
    /// Include traces from the evaluation environment. Default false.
    #[serde(default)]
    pub include_eval: bool,
    /// Include experiment traces. Default false.
    #[serde(default)]
    pub include_experiments: bool,
    /// The widget's query.
    pub query: WidgetQueryBody,
    /// `series` or `total`.
    pub shape: WidgetShape,
    /// `total` only: also the value over the previous range of the same length. Default false.
    #[serde(default)]
    pub compare: bool,
}

/// One bucket of a series. Spec 05 § Dashboard queries.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct WidgetPoint {
    /// Bucket start, nanoseconds since the Unix epoch. JSON is RFC 3339.
    #[serde(with = "cetra_core::time_serde")]
    #[schema(value_type = String, format = DateTime)]
    pub time: i64,
    /// The aggregate over the bucket. Omitted when it is null.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<f64>,
    /// Source rows of the group in the bucket.
    pub rows: u64,
}

/// One measure of one group over the buckets. Spec 05 § Dashboard queries.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct WidgetSeries {
    /// Index of the measure in the query.
    pub measure: usize,
    /// The breakdown value. Omitted without a breakdown, and for the other group.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    /// Whether this is the group of every other value.
    pub other: bool,
    /// One point per bucket with rows, in time order.
    pub points: Vec<WidgetPoint>,
}

/// A series answer. `shape` is `series`. Spec 05 § Dashboard queries.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct WidgetSeriesResponse {
    /// Width of every bucket, nanoseconds.
    pub bucket_ns: i64,
    /// The series: breakdown order (largest first, other last), then measure order.
    pub series: Vec<WidgetSeries>,
}

/// One group of a total, with every measure. Spec 05 § Dashboard queries.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct TotalRow {
    /// The breakdown value. Omitted without a breakdown, and for the other group.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    /// Whether this is the group of every other value.
    pub other: bool,
    /// The value of each measure, in query order. Null when the group has no rows.
    pub values: Vec<Option<f64>>,
    /// With `compare`, the same values over the previous range of the same length.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous: Option<Vec<Option<f64>>>,
}

/// A total answer. `shape` is `total`. Spec 05 § Dashboard queries.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct WidgetTotalResponse {
    /// One row per group, in breakdown order (other last). One row without a breakdown.
    pub rows: Vec<TotalRow>,
}

/// A widget answer, tagged by `shape`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(tag = "shape", rename_all = "snake_case")]
pub enum WidgetQueryResponse {
    /// `shape: series`.
    Series(WidgetSeriesResponse),
    /// `shape: total`.
    Total(WidgetTotalResponse),
}

/// Position and size of a widget on the 12-column grid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct Layout {
    /// Column, 0 to 11.
    pub x: i64,
    /// Row, 0 to 1000.
    pub y: i64,
    /// Width in columns, 1 to 12; `x + w` is at most 12.
    pub w: i64,
    /// Height in rows, 1 to 40.
    pub h: i64,
}

/// Type of a chart widget.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ChartType {
    /// Lines over time.
    Line,
    /// Columns over time.
    Bar,
    /// Filled lines over time.
    Area,
    /// Horizontal bars, one per breakdown group.
    BarH,
    /// One big number.
    Number,
    /// A table of groups.
    Table,
}

/// How a chart widget draws its data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct Chart {
    /// The chart type.
    #[serde(rename = "type")]
    pub chart_type: ChartType,
    /// Stacks the series. Bar and area only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stacked: Option<bool>,
    /// One small chart per group. Line only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub split: Option<bool>,
    /// Fades the buckets with fewer rows than this, 1 to 1000. Bar only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fade_below: Option<i64>,
    /// Shows each row's share of the first measure's total. Table only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub share: Option<bool>,
}

/// Kind of a widget.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum WidgetKind {
    /// A chart with a query.
    Chart,
    /// A text heading.
    Heading,
}

/// One widget of a dashboard.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct Widget {
    /// 1 to 64 characters of `[A-Za-z0-9_-]`, unique in the dashboard.
    pub id: String,
    /// Title, 0 to 100 characters (1 to 100 for a heading).
    pub title: String,
    /// Position and size.
    pub layout: Layout,
    /// Chart or heading.
    pub kind: WidgetKind,
    /// Chart settings. Chart widgets only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chart: Option<Chart>,
    /// The data. Chart widgets only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<WidgetQueryBody>,
}

/// The stored definition of a dashboard: the widgets, in order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct Definition {
    /// Up to 50 widgets.
    pub widgets: Vec<Widget>,
}

/// A dashboard with its definition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Dashboard {
    /// Dashboard id.
    pub id: Uuid,
    /// Display name. Names are not unique.
    pub name: String,
    /// Free text, 0 to 500 characters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The widgets.
    pub definition: Definition,
    /// Incremented by every change. A `PATCH` names the version it read.
    pub version: i64,
    /// Creator. Omitted when the user is gone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_by: Option<Uuid>,
    /// Last editor. Omitted when the user is gone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_by: Option<Uuid>,
    /// Creation time, nanoseconds since the Unix epoch. JSON is RFC 3339.
    #[serde(with = "cetra_core::time_serde")]
    #[schema(value_type = String, format = DateTime)]
    pub created_at: i64,
    /// Last update, nanoseconds since the Unix epoch. JSON is RFC 3339.
    #[serde(with = "cetra_core::time_serde")]
    #[schema(value_type = String, format = DateTime)]
    pub updated_at: i64,
}

/// A dashboard without its definition, as the list returns it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct DashboardSummary {
    /// Dashboard id.
    pub id: Uuid,
    /// Display name.
    pub name: String,
    /// Free text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Incremented by every change.
    pub version: i64,
    /// Creator. Omitted when the user is gone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_by: Option<Uuid>,
    /// Last editor. Omitted when the user is gone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_by: Option<Uuid>,
    /// Creation time, nanoseconds since the Unix epoch. JSON is RFC 3339.
    #[serde(with = "cetra_core::time_serde")]
    #[schema(value_type = String, format = DateTime)]
    pub created_at: i64,
    /// Last update, nanoseconds since the Unix epoch. JSON is RFC 3339.
    #[serde(with = "cetra_core::time_serde")]
    #[schema(value_type = String, format = DateTime)]
    pub updated_at: i64,
}

/// Body of `POST /api/v1/projects/{project}/dashboards`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateDashboard {
    /// Display name, 1 to 100 characters after trimming.
    pub name: String,
    /// Free text, 0 to 500 characters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The widgets.
    pub definition: Definition,
}

/// Body of `PATCH /api/v1/projects/{project}/dashboards/{id}`. Omitted fields are unchanged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdateDashboard {
    /// The version the caller read. Must be the stored one, else `409`.
    pub version: i64,
    /// Display name, 1 to 100 characters after trimming.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Free text, 0 to 500 characters. Omitted: unchanged. `""`: cleared (stored as none).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The widgets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub definition: Option<Definition>,
}

/// The body of `PUT /api/v1/projects/{project}/dashboards/overview`.
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SaveOverview {
    /// The version the caller read; absent when the project shows the built-in preset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<i64>,
    /// The widgets.
    pub definition: Definition,
}

/// Query parameters of `GET /api/v1/projects/{project}/dashboards`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, IntoParams)]
#[serde(deny_unknown_fields)]
pub struct DashboardListParams {
    /// Maximum number of items to return (1–1000, default 50).
    #[serde(default)]
    #[param(minimum = 1, maximum = 1000, default = 50)]
    pub limit: Limit,
    /// Opaque cursor from a previous page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}
