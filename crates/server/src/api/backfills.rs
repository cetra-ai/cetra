//! Online rule backfill API types (spec 09 § Evaluation & alerts; D-I-25, D-I-38 to D-I-42).

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use super::common::TimeParam;

/// Where a backfill stands (D-I-39).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum BackfillStatus {
    /// The runner is reading the window and queueing targets.
    Running,
    /// Every target of the window was read and queued.
    Done,
    /// The run stopped early: an error, a rule change or a restart. See `error`.
    Failed,
}

impl BackfillStatus {
    /// The stored backfill status.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Done => "done",
            Self::Failed => "failed",
        }
    }

    /// Parses the stored backfill status. Any other text is `None`.
    pub fn from_name(text: &str) -> Option<Self> {
        match text {
            "running" => Some(Self::Running),
            "done" => Some(Self::Done),
            "failed" => Some(Self::Failed),
            _ => None,
        }
    }
}

// Deserialize only, because `TimeParam` has no `Serialize`. Its `Debug` is derived: the request
// holds no payload content.
/// `POST /api/v1/projects/{project}/online-rules/{id}/backfills` body, and the body of its
/// `estimate` operation (D-I-38, D-I-42). Absent `sample_rate` takes the rule's rate.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateBackfillRequest {
    /// Window start, inclusive. RFC 3339 timestamp or epoch milliseconds.
    pub from: TimeParam,
    /// Window end, exclusive. At least 5 minutes ago; `to - from` at most 30 days. Same
    /// formats as `from`.
    pub to: TimeParam,
    /// Most targets the backfill reads, 1 to 10 000 (D-I-25).
    pub max_targets: u32,
    /// Sample rate, above 0 and at most 1. Omitted: the rule's sample rate (D-I-40).
    #[serde(default, deserialize_with = "super::online_rules::present")]
    pub sample_rate: Option<f64>,
}

/// A backfill of one online rule (D-I-25).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct Backfill {
    /// Backfill id.
    pub id: Uuid,
    /// The rule the backfill runs for.
    pub rule_id: Uuid,
    /// Where the backfill stands.
    pub status: BackfillStatus,
    /// Window start, inclusive. RFC 3339.
    #[serde(with = "llmobs_core::time_serde")]
    #[schema(value_type = String, format = DateTime)]
    pub from: i64,
    /// Window end, exclusive. RFC 3339.
    #[serde(with = "llmobs_core::time_serde")]
    #[schema(value_type = String, format = DateTime)]
    pub to: i64,
    /// Most targets the backfill reads.
    pub max_targets: u32,
    /// Sample rate the backfill used.
    pub sample_rate: f64,
    /// Targets read so far, at most `max_targets`.
    pub matched: u64,
    /// Jobs inserted or re-queued so far.
    pub enqueued: u64,
    /// Why the backfill failed. Present exactly when `status` is `failed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// User who started the backfill. Omitted when that user is unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_by: Option<Uuid>,
    /// Start time. RFC 3339.
    #[serde(with = "llmobs_core::time_serde")]
    #[schema(value_type = String, format = DateTime)]
    pub created_at: i64,
    /// End time. Omitted while the backfill runs. RFC 3339.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::api::keys::optional_time"
    )]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub finished_at: Option<i64>,
}

/// What a backfill would read and cost (D-I-42). The estimate is an upper bound: it does not
/// subtract targets the rule already scored.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct BackfillEstimate {
    /// Targets in the window, at most `max_targets`.
    pub matched: u64,
    /// More than `max_targets` matched.
    pub capped: bool,
    /// Targets the sample rate keeps.
    pub sampled: u64,
    /// Input tokens the sampled targets would send. Judges only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated_input_tokens: Option<u64>,
    /// Output tokens the sampled targets would ask for. Judges only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated_output_tokens: Option<u64>,
    /// Estimated cost in USD. Judges with a priced model only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated_cost_usd: Option<f64>,
    /// The window that was counted. RFC 3339.
    #[serde(with = "llmobs_core::time_serde")]
    #[schema(value_type = String, format = DateTime)]
    pub from: i64,
    /// The end of the window that was counted. RFC 3339.
    #[serde(with = "llmobs_core::time_serde")]
    #[schema(value_type = String, format = DateTime)]
    pub to: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backfill_round_trips_the_fixture() {
        let text = include_str!("../../../../fixtures/api/static/backfill.json");
        let backfill: Backfill = serde_json::from_str(text).unwrap();
        let back: serde_json::Value = serde_json::to_value(&backfill).unwrap();
        let fixture: serde_json::Value = serde_json::from_str(text).unwrap();
        assert_eq!(back, fixture);
    }

    #[test]
    fn backfill_omits_absent_error_and_finish_time() {
        let text = include_str!("../../../../fixtures/api/static/backfill.json");
        let mut value: serde_json::Value = serde_json::from_str(text).unwrap();
        assert!(value.get("error").is_none());
        assert!(value.get("finished_at").is_none());
        value["status"] = serde_json::json!("failed");
        value["error"] = serde_json::json!("interrupted");
        let failed: Backfill = serde_json::from_value(value).unwrap();
        assert_eq!(failed.status, BackfillStatus::Failed);
        assert_eq!(failed.error.as_deref(), Some("interrupted"));
        assert!(failed.finished_at.is_none());
        let encoded = serde_json::to_value(&failed).unwrap();
        assert_eq!(encoded["error"], "interrupted");
        assert!(encoded.get("finished_at").is_none());
    }

    #[test]
    fn create_request_parses_the_fixture() {
        let text = include_str!("../../../../fixtures/api/requests/backfill-create.json");
        let request: CreateBackfillRequest = serde_json::from_str(text).unwrap();
        assert_eq!(request.max_targets, 1000);
        assert_eq!(request.sample_rate, Some(0.25));
    }

    #[test]
    fn create_request_refuses_unknown_and_null_fields() {
        let base = r#"{"from":"2025-12-25T00:00:00Z","to":"2026-01-01T00:00:00Z","max_targets":10"#;
        let dry_run = format!(r#"{base},"dry_run":true}}"#);
        assert!(serde_json::from_str::<CreateBackfillRequest>(&dry_run).is_err());
        let null_rate = format!(r#"{base},"sample_rate":null}}"#);
        assert!(serde_json::from_str::<CreateBackfillRequest>(&null_rate).is_err());
        let absent_rate = format!("{base}}}");
        let request: CreateBackfillRequest = serde_json::from_str(&absent_rate).unwrap();
        assert_eq!(request.sample_rate, None);
    }

    #[test]
    fn status_names_round_trip() {
        for status in [
            BackfillStatus::Running,
            BackfillStatus::Done,
            BackfillStatus::Failed,
        ] {
            assert_eq!(BackfillStatus::from_name(status.as_str()), Some(status));
        }
        assert_eq!(BackfillStatus::from_name("paused"), None);
    }
}
