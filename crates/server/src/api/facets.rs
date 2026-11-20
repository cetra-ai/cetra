//! Facets: the values, keys or histogram of one field over a list's rows (filtering plan D1).
//!
//! `GET /api/v1/projects/{project}/facets`. The field grammar and the counting rules are in the
//! operation's description.

use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

use super::common::TimeParam;

/// Query parameters of `GET /api/v1/projects/{project}/facets`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, IntoParams)]
#[serde(deny_unknown_fields)]
pub struct FacetsParams {
    /// The list the facet describes.
    pub view: FacetView,
    /// `column` or `map['key']`. A bare map column facets its keys.
    pub field: String,
    /// Filter DSL expression over `view`. Blank means no filter.
    #[serde(default)]
    pub filter: Option<String>,
    /// Full-text search. `traces` and `spans` only. At most 1,024 bytes.
    #[serde(default)]
    pub search: Option<String>,
    /// Inclusive range start. RFC 3339 or epoch milliseconds.
    #[serde(default)]
    pub from: Option<TimeParam>,
    /// Exclusive range end. RFC 3339 or epoch milliseconds.
    #[serde(default)]
    pub to: Option<TimeParam>,
    /// Include traces from the evaluation and playground environments (`cetra-eval`, `cetra-playground`). Default false.
    #[serde(default)]
    pub include_eval: bool,
    /// Include experiment traces. Default false.
    #[serde(default)]
    pub include_experiments: bool,
    /// Case-insensitive substring the value or key must contain. 1 to 200 characters after
    /// trimming. Ignored by histograms.
    #[serde(default)]
    pub q: Option<String>,
    /// Most values or keys returned (1 to 100, default 10).
    #[serde(default)]
    #[param(minimum = 1, maximum = 100, default = 10)]
    pub limit: Option<u32>,
}

/// The list a facet describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum FacetView {
    /// The traces list.
    Traces,
    /// The spans list.
    Spans,
    /// The sessions list, one row per session.
    Sessions,
}

/// What a facet describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum FacetKind {
    /// Values of a string, boolean, list or `map['key']` field.
    Values,
    /// The keys of a bare map field.
    Keys,
    /// A histogram of a number field.
    Histogram,
}

/// One value or key and the rows under it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct FacetValue {
    /// The value or key. Booleans are `true` or `false`.
    pub value: String,
    /// Rows counted under it.
    pub count: u64,
}

/// One histogram bucket. `[lo, hi)`, except the last bucket, which is `[lo, hi]`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct FacetBucket {
    /// Lower bound.
    pub lo: f64,
    /// Upper bound.
    pub hi: f64,
    /// Rows in the bucket.
    pub count: u64,
}

/// A histogram of a number field. `min` and `max` are exact; the percentiles are approximate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct FacetHistogram {
    /// Smallest value.
    pub min: f64,
    /// Largest value.
    pub max: f64,
    /// Median.
    pub p50: f64,
    /// 95th percentile.
    pub p95: f64,
    /// 99th percentile.
    pub p99: f64,
    /// Every bucket, empty ones included.
    pub buckets: Vec<FacetBucket>,
}

/// The answer of a facet request.
///
/// `values` and `more` are present for kinds `values` and `keys`. `histogram` is present for
/// kind `histogram` when at least one row has a value.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct FacetsResponse {
    /// The list the facet describes.
    pub view: FacetView,
    /// The field, as requested.
    pub field: String,
    /// What the answer describes.
    pub kind: FacetKind,
    /// Rows counted.
    pub total: u64,
    /// Rows with no value: null, or an empty list or map.
    pub missing: u64,
    /// At most `limit` values or keys, by count descending then value ascending.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub values: Option<Vec<FacetValue>>,
    /// True when more distinct values matched than `limit`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub more: Option<bool>,
    /// The histogram of a number field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub histogram: Option<FacetHistogram>,
}
