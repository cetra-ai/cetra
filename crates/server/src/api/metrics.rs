//! Shared time-series types: bucket widths, the `group_by` split and one point of a series.
//!
//! Spec 05 § Dashboard queries (buckets and splits), spec 06 (score summaries).

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Width of each bucket of a widget query.
///
/// Spec 05 § Dashboard queries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum MetricsBucket {
    /// About 120 buckets across the range.
    #[default]
    Auto,
    /// One minute.
    #[serde(rename = "1m")]
    M1,
    /// Five minutes.
    #[serde(rename = "5m")]
    M5,
    /// Ten minutes.
    #[serde(rename = "10m")]
    M10,
    /// Fifteen minutes.
    #[serde(rename = "15m")]
    M15,
    /// Thirty minutes.
    #[serde(rename = "30m")]
    M30,
    /// One hour.
    #[serde(rename = "1h")]
    H1,
    /// Six hours.
    #[serde(rename = "6h")]
    H6,
    /// Twelve hours.
    #[serde(rename = "12h")]
    H12,
    /// One day.
    #[serde(rename = "1d")]
    D1,
}

/// Dimension that splits a widget's series.
///
/// Spec 05 § Dashboard queries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum MetricsGroupBy {
    /// Each model named on the trace.
    Model,
    /// `environment`.
    Environment,
    /// `release`.
    Release,
}

/// One bucket of a time series.
///
/// Spec 05 § Dashboard queries, spec 06 (score summaries).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct MetricPoint {
    /// Bucket start, nanoseconds since the Unix epoch. JSON is RFC 3339.
    #[serde(with = "cetra_core::time_serde")]
    #[schema(value_type = String, format = DateTime)]
    pub time: i64,
    /// Bucket value. Omitted when the bucket has no data.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<f64>,
}

#[cfg(test)]
mod tests {
    use super::MetricsBucket;

    #[test]
    fn metrics_bucket_spellings_round_trip() {
        let spellings = [
            (MetricsBucket::Auto, "auto"),
            (MetricsBucket::M1, "1m"),
            (MetricsBucket::M5, "5m"),
            (MetricsBucket::M10, "10m"),
            (MetricsBucket::M15, "15m"),
            (MetricsBucket::M30, "30m"),
            (MetricsBucket::H1, "1h"),
            (MetricsBucket::H6, "6h"),
            (MetricsBucket::H12, "12h"),
            (MetricsBucket::D1, "1d"),
        ];
        for (bucket, spelling) in spellings {
            let json = serde_json::to_string(&bucket).unwrap();
            assert_eq!(json, format!("\"{spelling}\""));
            assert_eq!(
                serde_json::from_str::<MetricsBucket>(&json).unwrap(),
                bucket
            );
        }
    }
}
