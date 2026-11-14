//! Running one evaluator: the shared path of test runs (D-I-21) and queued jobs (D-I-12).

use std::sync::Arc;

use llmobs_core::enums::SpanKind;
use llmobs_core::ids::TraceId;
use llmobs_eval::{
    Check, EvalOrigin, EvalRun, EvalTarget, Evaluator, EvaluatorConfig, JudgeConnection, RunInput,
    Secret, Variables,
};
use llmobs_otel::Normalized;
use serde_json::Value;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::http::AppState;
use crate::ingest::{CommitReceipt, CommitTarget, IngestError, commit_in_task};
use crate::meta::projects::ProjectRow;

/// Longest `output` and `expected` of a levenshtein check, in bytes (the distance is quadratic).
const MAX_LEVENSHTEIN_BYTES: usize = 64 * 1024;
/// A hash input, not an attribute name: the D46 rename must not change judge trace ids.
const JUDGE_TRACE_TAG: &[u8] = b"llmobs.judge-trace.v1";

/// What one run needs besides the evaluator.
pub(crate) struct RunArgs<'a> {
    pub(crate) project: &'a ProjectRow,
    /// Moved into the blocking task for a check.
    pub(crate) evaluator: Evaluator,
    pub(crate) connection: Option<Arc<JudgeConnection>>,
    pub(crate) webhook_secret: Option<Secret>,
    pub(crate) variables: Variables,
    pub(crate) target: EvalTarget,
    pub(crate) origin: EvalOrigin,
    pub(crate) judge_trace_id: TraceId,
    pub(crate) now: i64,
}

/// Runs the evaluator once. A check runs inside `spawn_blocking` (D-I-23).
///
/// # Errors
///
/// [`ExecuteError::Panicked`] when the blocking task panicked or was cancelled.
pub(crate) async fn run_evaluator(
    state: &AppState,
    args: RunArgs<'_>,
) -> Result<EvalRun, ExecuteError> {
    let RunArgs {
        project,
        evaluator,
        connection,
        webhook_secret,
        variables,
        target,
        origin,
        judge_trace_id,
        now,
    } = args;
    let ingest = state.ingest.context(project, now);
    let prices = state.ingest.prices();
    if matches!(evaluator.config, EvaluatorConfig::Check(_)) {
        // Checks are CPU work: they run off the async workers (D-I-23).
        let runner = Arc::clone(&state.eval_runner);
        let handle = tokio::runtime::Handle::current();
        return tokio::task::spawn_blocking(move || {
            handle.block_on(async move {
                runner
                    .run(RunInput {
                        evaluator: &evaluator,
                        variables: &variables,
                        connection: None,
                        webhook_secret: None,
                        target,
                        origin,
                        judge_trace_id,
                        ingest: &ingest,
                        prices: &prices,
                    })
                    .await
            })
        })
        .await
        .map_err(ExecuteError::Panicked);
    }
    Ok(state
        .eval_runner
        .run(RunInput {
            evaluator: &evaluator,
            variables: &variables,
            connection: connection.as_deref(),
            webhook_secret: webhook_secret.as_ref(),
            target,
            origin,
            judge_trace_id,
            ingest: &ingest,
            prices: &prices,
        })
        .await)
}

/// Tokens and cost of the run's `llm` span, when it has one (D-I-18).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct JudgeUsage {
    pub(crate) input_tokens: Option<u64>,
    pub(crate) output_tokens: Option<u64>,
    pub(crate) cost_usd: Option<f64>,
}

/// The usage of the run's `llm` span. `None` when the run has none.
pub(crate) fn judge_usage(run: &EvalRun) -> Option<JudgeUsage> {
    run.judge_trace
        .spans
        .iter()
        .find(|span| span.span_kind == SpanKind::Llm)
        .map(|span| JudgeUsage {
            input_tokens: span
                .input_tokens
                .and_then(|count| u64::try_from(count).ok()),
            output_tokens: span
                .output_tokens
                .and_then(|count| u64::try_from(count).ok()),
            cost_usd: span.cost_total,
        })
}

/// Adds the usage to `usage_daily` (zero for absent counts) (D-I-31).
pub(crate) fn count_judge_usage(
    state: &AppState,
    project: &ProjectRow,
    now: i64,
    usage: JudgeUsage,
) {
    state.usage.add_judge(
        project.org_id,
        project.id,
        now,
        usage.input_tokens.unwrap_or(0),
        usage.output_tokens.unwrap_or(0),
        usage.cost_usd.unwrap_or(0.0),
    );
}

/// Commits the judge trace in its own task (D-G-38). Its spans are not billable (D-I-31).
///
/// # Errors
///
/// Returns the ingest error when no write budget is free or the commit fails.
pub(crate) async fn commit_judge_trace(
    state: &AppState,
    project: &ProjectRow,
    now: i64,
    trace: Normalized,
) -> Result<CommitReceipt, IngestError> {
    let reservation = state.ingest.reserve_exact(0)?;
    commit_in_task(
        state,
        CommitTarget {
            org_id: project.org_id,
            project_id: project.id,
            request_bytes: 0,
            now_ns: now,
        },
        trace,
        reservation,
    )
    .await
}

/// The judge trace id of a job's run (D-I-16): the first 16 bytes of
/// SHA-256(`llmobs.judge-trace.v1` ‖ job id (16 bytes) ‖ the run's claims as u32 BE).
#[allow(dead_code)] // I3.6 uses it.
pub(crate) fn job_judge_trace_id(job: Uuid, claims: u32) -> TraceId {
    let mut hasher = Sha256::new();
    hasher.update(JUDGE_TRACE_TAG);
    hasher.update(job.as_bytes());
    hasher.update(claims.to_be_bytes());
    let digest = hasher.finalize();
    // SHA-256 has 32 bytes, so the first 16 always exist.
    let mut bytes: [u8; 16] = std::array::from_fn(|index| digest[index]);
    if bytes == [0; 16] {
        // Only all-zero bytes are refused. Setting the last byte makes them valid.
        bytes[15] = 1;
    }
    // The bytes are not all zero here (see above), so `from_bytes` cannot fail.
    #[allow(clippy::expect_used)]
    TraceId::from_bytes(bytes).expect("the bytes are not all zero")
}

/// A check's `output` or `expected` over 64 KiB (the distance is quadratic).
pub(crate) fn levenshtein_too_large(evaluator: &Evaluator, variables: &Variables) -> bool {
    matches!(
        evaluator.config,
        EvaluatorConfig::Check(Check::Levenshtein { .. })
    ) && variables.iter().any(|(name, value)| {
        matches!(name, "output" | "expected") && render(value).len() > MAX_LEVENSHTEIN_BYTES
    })
}

/// A variable as the answer shows it: a string as itself, anything else as compact JSON.
pub(crate) fn render(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum ExecuteError {
    #[error("the evaluator task failed")]
    Panicked(#[source] tokio::task::JoinError),
}

#[cfg(test)]
mod tests {
    use llmobs_eval::{CheckError, EvalError};
    use llmobs_otel::Rejections;

    use super::*;

    /// The first 16 bytes of the hash, computed outside this code.
    const PINNED_JUDGE_TRACE_ID: &str = "53d501b27e5b1ce8c268b9eb4d746a71";

    #[test]
    fn job_judge_trace_id_is_pinned() {
        // SHA-256 of the tag, then the 16 bytes of job 0800 (`…0800`), then claims 1 as u32 BE.
        let job = Uuid::from_u128(0x0190_0000_0000_7000_8000_0000_0000_0800);
        let id = job_judge_trace_id(job, 1);
        assert_eq!(id.to_string(), PINNED_JUDGE_TRACE_ID);
        assert_ne!(job_judge_trace_id(job, 2), id);
    }

    #[test]
    fn judge_usage_is_none_without_an_llm_span() {
        let run = EvalRun {
            outcome: Err(EvalError::Check(CheckError::MissingVariable {
                name: "output",
            })),
            judge_trace: Normalized {
                spans: Vec::new(),
                media: Vec::new(),
                rejections: Rejections::default(),
            },
        };
        assert_eq!(judge_usage(&run), None);
    }
}
