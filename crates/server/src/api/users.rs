//! End-user response types.
//!
//! Spec 09 § Observability data and § Response shapes.

use std::collections::BTreeMap;

use cetra_core::enums::ScoreDataType;
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

use super::common::{Limit, SortOrder, TimeParam};
use super::sessions::SessionSummary;

/// Most sessions a user detail response includes. Spec 09 § Response shapes.
pub const MAX_USER_SESSIONS: usize = 100;

/// Average of one score name over a user's traces (spec 09 § Observability data).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct UserScore {
    /// Score name.
    pub name: String,
    /// `numeric` or `boolean`.
    pub data_type: ScoreDataType,
    /// Mean `value_num`; for booleans the share of `true`.
    pub avg: f64,
    /// Number of scores averaged.
    pub count: i64,
}

/// One row of the end-user list.
///
/// `first_seen` is the minimum trace `start_time` and `last_seen` the maximum `end_time`.
/// `error_count` is the sum of the traces' `error_count`. Spec 09 § Observability data.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct EndUserSummary {
    /// End-user id.
    pub user_id: String,
    /// Earliest trace start, nanoseconds since the Unix epoch. JSON is RFC 3339.
    #[serde(with = "cetra_core::time_serde")]
    #[schema(value_type = String, format = DateTime)]
    pub first_seen: i64,
    /// Latest trace end, nanoseconds since the Unix epoch. JSON is RFC 3339.
    #[serde(with = "cetra_core::time_serde")]
    #[schema(value_type = String, format = DateTime)]
    pub last_seen: i64,
    /// Number of traces.
    pub trace_count: i64,
    /// Number of sessions.
    pub session_count: i64,
    /// Sum of the traces' `error_count`.
    pub error_count: i64,
    /// Traces whose status is `error`.
    pub error_trace_count: i64,
    /// Sum of trace token totals. Omitted when every trace omitted the field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_tokens: Option<i64>,
    /// Sum of trace costs, USD. Omitted when every trace omitted the field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_total: Option<f64>,
    /// Trace counts in 24 equal buckets: the request's `[from, to)`, or without one
    /// `[first_seen, last_seen]` of the returned rows.
    pub activity: Vec<i64>,
    /// Average of each numeric and boolean score name over the user's traces.
    pub scores: Vec<UserScore>,
    /// `user.*` keys of the latest trace's metadata, without the prefix. Omitted when empty.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, String>,
}

/// User summary plus the most recent sessions.
///
/// Spec 09 § Response shapes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct EndUserDetail {
    /// User row.
    pub user: EndUserSummary,
    /// The user's most recent sessions. At most [`MAX_USER_SESSIONS`].
    pub sessions: Vec<SessionSummary>,
}

/// Sort key for user lists.
///
/// Spec 09 § Observability data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum UserSort {
    /// `last_seen`.
    #[default]
    LastSeen,
    /// `trace_count`.
    TraceCount,
    /// `cost_total`.
    CostTotal,
    /// `session_count`.
    SessionCount,
    /// `total_tokens`.
    TotalTokens,
    /// `error_count`.
    ErrorCount,
    /// `error_rate`: `error_trace_count / trace_count`.
    ErrorRate,
    /// `traces_per_session`: `trace_count / session_count`, null without sessions.
    TracesPerSession,
    /// `first_seen`.
    FirstSeen,
}

/// Query parameters of `GET /api/v1/projects/{project}/users`.
///
/// The filter runs on the `traces` view before traces are grouped by user. Spec 09 § Observability data.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, IntoParams)]
#[serde(deny_unknown_fields)]
pub struct UserListParams {
    /// Filter DSL expression over the `traces` view, applied before grouping (spec 05).
    #[serde(default)]
    pub filter: Option<String>,
    /// Inclusive range start. RFC 3339 or epoch milliseconds.
    #[serde(default)]
    pub from: Option<TimeParam>,
    /// Exclusive range end. RFC 3339 or epoch milliseconds.
    #[serde(default)]
    pub to: Option<TimeParam>,
    /// Sort key. Defaults to last seen.
    #[serde(default)]
    pub sort: UserSort,
    /// Sort direction. Defaults to descending.
    #[serde(default)]
    pub order: SortOrder,
    /// Maximum number of items to return (1–1000, default 50).
    #[serde(default)]
    #[param(minimum = 1, maximum = 1000, default = 50)]
    pub limit: Limit,
    /// Opaque cursor from a previous page.
    #[serde(default)]
    pub cursor: Option<String>,
    /// Include traces from the evaluation environment. Default false. Spec 05 § Default exclusions.
    #[serde(default)]
    pub include_eval: bool,
    /// Include experiment traces. Default false. Spec 05 § Default exclusions.
    #[serde(default)]
    pub include_experiments: bool,
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use cetra_core::enums::{ScoreDataType, ScoreSource};
    use cetra_core::score::ScoreValue;
    use uuid::Uuid;

    use super::{EndUserDetail, EndUserSummary, UserScore};
    use crate::api::sessions::SessionSummary;
    use crate::api::traces::ScoreBrief;

    const AT: i64 = 1_767_261_600_000_000_000;

    #[test]
    fn end_user_detail_snapshot() {
        let detail = EndUserDetail {
            user: EndUserSummary {
                user_id: "user-42".to_owned(),
                first_seen: AT,
                last_seen: AT + 3_000_000_000,
                trace_count: 2,
                session_count: 1,
                error_count: 0,
                error_trace_count: 0,
                total_tokens: Some(2600),
                cost_total: Some(0.00306),
                activity: vec![
                    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1,
                ],
                scores: vec![UserScore {
                    name: "resolved".to_owned(),
                    data_type: ScoreDataType::Boolean,
                    avg: 1.0,
                    count: 1,
                }],
                metadata: BTreeMap::from([
                    ("name".to_owned(), "\"Ada Example\"".to_owned()),
                    ("plan".to_owned(), "\"pro\"".to_owned()),
                ]),
            },
            sessions: vec![SessionSummary {
                session_id: "sess-001".to_owned(),
                start_time: AT,
                end_time: AT + 4_200_000_000,
                duration_ns: 4_200_000_000,
                trace_count: 2,
                error_count: 0,
                total_tokens: Some(2600),
                cost_total: Some(0.00306),
                user_id: Some("user-42".to_owned()),
                tags: vec!["support".to_owned()],
                scores: vec![ScoreBrief {
                    score_id: Uuid::parse_str("01900000-0000-7000-8000-000000000305").unwrap(),
                    name: "resolved".to_owned(),
                    value: ScoreValue::Boolean(true),
                    source: ScoreSource::Human,
                }],
                error_trace_count: 0,
                first_input_preview: None,
                last_output_preview: None,
            }],
        };

        insta::with_settings!({
            snapshot_path => "../../tests/snapshots",
            prepend_module_to_snapshot => false,
        }, {
            insta::assert_snapshot!(
                "end_user_detail",
                serde_json::to_string_pretty(&detail).unwrap()
            );
        });
    }
}
