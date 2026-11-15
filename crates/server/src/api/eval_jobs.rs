//! Eval job API types: the queue's jobs, their list query and the manual run request
//! (spec 09 § Evaluation & alerts, D-I-22).

use std::collections::BTreeMap;

use llmobs_core::ids::{SpanId, TraceId};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use super::common::{Limit, TimeParam};
use super::connections::TestErrorClass;

/// Where a job came from. Stored as `eval_jobs.kind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum EvalJobKind {
    /// Written by an online rule for a matching trace or span.
    Rule,
    /// Started by `POST /evaluators/{id}/run`.
    Manual,
    /// Written by a backfill over stored traces.
    Backfill,
}

impl EvalJobKind {
    /// The stored job kind.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Rule => "rule",
            Self::Manual => "manual",
            Self::Backfill => "backfill",
        }
    }

    /// Parses the stored job kind. Any other text is `None`.
    pub fn from_name(text: &str) -> Option<Self> {
        match text {
            "rule" => Some(Self::Rule),
            "manual" => Some(Self::Manual),
            "backfill" => Some(Self::Backfill),
            _ => None,
        }
    }
}

/// Where a job is in its life. Stored as `eval_jobs.status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum EvalJobStatus {
    /// Waiting to run.
    Queued,
    /// Claimed by a worker.
    Running,
    /// Finished with a score.
    Done,
    /// Failed for good.
    Failed,
    /// Left out before it ran.
    Skipped,
}

impl EvalJobStatus {
    /// The stored job status.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Skipped => "skipped",
        }
    }

    /// Parses the stored job status. Any other text is `None`.
    pub fn from_name(text: &str) -> Option<Self> {
        match text {
            "queued" => Some(Self::Queued),
            "running" => Some(Self::Running),
            "done" => Some(Self::Done),
            "failed" => Some(Self::Failed),
            "skipped" => Some(Self::Skipped),
            _ => None,
        }
    }
}

/// An eval job. Spec 09 § Evaluation & alerts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct EvalJob {
    /// Job id.
    pub id: Uuid,
    /// Where the job came from.
    pub kind: EvalJobKind,
    /// Where the job is in its life.
    pub status: EvalJobStatus,
    /// The rule that queued the job. Omitted for a manual run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rule_id: Option<Uuid>,
    /// The evaluator the job runs.
    pub evaluator_id: Uuid,
    /// Target trace, 32 lowercase hex digits.
    pub trace_id: TraceId,
    /// Target span, 16 lowercase hex digits. Omitted for a trace target.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub span_id: Option<SpanId>,
    /// Attempts made so far.
    pub attempts: u32,
    /// What the caller should do with the failure. Omitted unless the job failed or was requeued.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_class: Option<TestErrorClass>,
    /// The failure text, without payload content or secrets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The judge trace of the run, 32 lowercase hex digits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub judge_trace_id: Option<TraceId>,
    /// The score the run wrote.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub score_id: Option<Uuid>,
    /// Input tokens of the judge call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    /// Output tokens of the judge call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    /// Cost of the judge call, in USD.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    /// Latency of the judge call, in milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<u64>,
    /// Earliest time the job may run. JSON is RFC 3339.
    #[serde(with = "llmobs_core::time_serde")]
    #[schema(value_type = String, format = DateTime)]
    pub not_before: i64,
    /// Creation time. JSON is RFC 3339.
    #[serde(with = "llmobs_core::time_serde")]
    #[schema(value_type = String, format = DateTime)]
    pub created_at: i64,
    /// Time the job finished. Omitted while it is queued or running.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::api::keys::optional_time"
    )]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub finished_at: Option<i64>,
}

/// `GET /api/v1/projects/{project}/eval-jobs` query. Newest first.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, IntoParams)]
#[serde(deny_unknown_fields)]
pub struct EvalJobListParams {
    /// Jobs queued by this online rule.
    #[serde(default)]
    pub rule_id: Option<Uuid>,
    /// Jobs of this evaluator.
    #[serde(default)]
    pub evaluator_id: Option<Uuid>,
    /// Jobs with this status.
    #[serde(default)]
    pub status: Option<EvalJobStatus>,
    /// Jobs of this kind.
    #[serde(default)]
    pub kind: Option<EvalJobKind>,
    /// Maximum number of jobs to return (1–1000, default 50).
    #[serde(default)]
    #[param(minimum = 1, maximum = 1000, default = 50)]
    pub limit: Limit,
    /// Opaque cursor from a previous page.
    #[serde(default)]
    pub cursor: Option<String>,
}

/// `POST /api/v1/projects/{project}/evaluators/{id}/run` body.
///
/// Deserialize only, because `TimeParam` has no `Serialize`. Its `Debug` is derived: the
/// mapping holds expressions, not payload.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RunEvaluatorRequest {
    /// Trace to run on, 32 hex digits.
    pub trace_id: String,
    /// Span of the trace to target, 16 hex digits. Omitted for a trace target; `null` is refused.
    #[serde(default, deserialize_with = "super::online_rules::present")]
    pub span_id: Option<String>,
    /// The trace's `start_time`, from the trace detail the UI loaded.
    pub trace_start: TimeParam,
    /// Prompt variable to mapping expression, 1 to 32 variables.
    pub variable_mapping: BTreeMap<String, String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_params_parse_the_filters_and_the_limit() {
        let query =
            "rule_id=01900000-0000-7000-8000-000000000700&status=failed&kind=manual&limit=10";
        let params: EvalJobListParams = serde_urlencoded::from_str(query).unwrap();
        assert_eq!(
            params.rule_id,
            Some(Uuid::parse_str("01900000-0000-7000-8000-000000000700").unwrap())
        );
        assert_eq!(params.status, Some(EvalJobStatus::Failed));
        assert_eq!(params.kind, Some(EvalJobKind::Manual));
        assert_eq!(params.limit.get(), 10);
        assert!(params.cursor.is_none());
    }

    #[test]
    fn list_params_refuse_an_unknown_status() {
        assert!(serde_urlencoded::from_str::<EvalJobListParams>("status=stuck").is_err());
    }

    #[test]
    fn job_enums_round_trip_their_names() {
        for status in [
            EvalJobStatus::Queued,
            EvalJobStatus::Running,
            EvalJobStatus::Done,
            EvalJobStatus::Failed,
            EvalJobStatus::Skipped,
        ] {
            assert_eq!(EvalJobStatus::from_name(status.as_str()), Some(status));
        }
        for kind in [
            EvalJobKind::Rule,
            EvalJobKind::Manual,
            EvalJobKind::Backfill,
        ] {
            assert_eq!(EvalJobKind::from_name(kind.as_str()), Some(kind));
        }
        assert_eq!(EvalJobStatus::from_name("stuck"), None);
    }

    #[test]
    fn run_request_reads_the_fixture() {
        let text = include_str!("../../../../fixtures/api/requests/evaluator-run.json");
        let request: RunEvaluatorRequest = serde_json::from_str(text).unwrap();
        assert_eq!(request.trace_id, "a1000000000000000000000000000001");
        assert!(request.span_id.is_none());
    }

    #[test]
    fn run_request_refuses_a_null_span_id() {
        let text = include_str!("../../../../fixtures/api/requests/evaluator-run.json");
        let mut value: serde_json::Value = serde_json::from_str(text).unwrap();
        value["span_id"] = serde_json::Value::Null;
        assert!(serde_json::from_value::<RunEvaluatorRequest>(value).is_err());
    }

    #[test]
    fn run_request_refuses_unknown_fields() {
        let text = r#"{"trace_id":"a1000000000000000000000000000001","trace_start":"2026-01-01T10:00:00.000Z","variable_mapping":{},"evaluator_id":"01900000-0000-7000-8000-000000000500"}"#;
        assert!(serde_json::from_str::<RunEvaluatorRequest>(text).is_err());
    }
}
