//! Running one evaluator: the shared path of test runs (D-I-21) and queued jobs (D-I-12).

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use cetra_core::enums::SpanKind;
use cetra_core::ids::{OrgId, ProjectId, TraceId};
use cetra_core::score::ScoreTarget;
use cetra_core::span::Span;
use cetra_eval::{
    Adapter, Check, ErrorClass, EvalError, EvalOrigin, EvalOutcome, EvalRun, EvalTarget, Evaluator,
    EvaluatorConfig, JudgeConfig, JudgeConnection, JudgeError, PromptTemplate, RunInput, Secret,
    TraceData, VariableMapping, Variables, resolve,
};
use cetra_otel::Normalized;
use serde_json::Value;
use sha2::{Digest, Sha256};
use sqlx::SqliteConnection;
use uuid::Uuid;

use crate::api::evaluators::EvaluatorKind;
use crate::api::online_rules::BudgetPeriod;
use crate::api::scores::ScoreCreate;
use crate::deletion;
use crate::entitlements::{Visibility, units};
use crate::eval::budget::{self, Reservation, Reserve};
use crate::eval::definitions::fits_definition;
use crate::eval::evaluator;
use crate::eval::retry;
use crate::eval::trace_load::{self, LoadError};
use crate::http::AppState;
use crate::http::error::ApiError;
use crate::ingest::{CommitReceipt, CommitTarget, IngestError, commit_in_task};
use crate::meta::MetaError;
use crate::meta::connections;
use crate::meta::eval_jobs::{self, Finish, JobRow, JobUsage};
use crate::meta::evaluators as store;
use crate::meta::online_rules::{self, PAUSE_BUDGET, PAUSE_CONNECTION, PAUSE_ERRORS, RuleRow};
use crate::meta::orgs;
use crate::meta::projects::{self, ProjectRow};
use crate::meta::rule_spend;
use crate::scores::{self, Writer};

/// The tags of the score a run writes (D-I-18): the job, the evaluator version, a truncated trace.
const JOB_ID_KEY: &str = cetra_core::cetra_name!(".eval.job_id");
const EVALUATOR_VERSION_KEY: &str = cetra_core::cetra_name!(".eval.evaluator_version");
const TRUNCATED_KEY: &str = cetra_core::cetra_name!(".eval.truncated");
/// The job error of a write that kept failing (D-I-13).
const STORE_ERROR: &str = "could not store the result";
/// The job error of a score refused at the org's monthly units cap (D-I-35).
const UNITS_CAP_ERROR: &str = "monthly units cap reached";
/// The breaker's pause detail (D-I-14).
const BREAKER_DETAIL: &str = "half or more of the last 50 jobs failed";

/// Longest `output` and `expected` of a levenshtein check, in bytes (the distance is quadratic).
const MAX_LEVENSHTEIN_BYTES: usize = 64 * 1024;
/// A hash input, not an attribute name: changing it changes judge trace ids.
const JUDGE_TRACE_TAG: &[u8] = b"cetra.judge-trace.v1";

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
    // A judge trace is written now, so the D85 age bound never applies (D-P-22).
    let ingest = state.ingest.context(project, None, now);
    let prices = state
        .ingest
        .prices()
        .for_project(project.org_id, project.id);
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

/// The distinct end users of `spans`, each with the smallest `ingest_time` of its spans (D-M-6).
/// Empty user ids are not users.
pub(crate) fn users_of(spans: &[Span]) -> Vec<(String, i64)> {
    let mut oldest: BTreeMap<&str, i64> = BTreeMap::new();
    for span in spans {
        if let Some(user) = span.user_id.as_deref().filter(|user| !user.is_empty()) {
            oldest
                .entry(user)
                .and_modify(|at| *at = (*at).min(span.ingest_time))
                .or_insert(span.ingest_time);
        }
    }
    oldest
        .into_iter()
        .map(|(user, at)| (user.to_owned(), at))
        .collect()
}

/// [`users_of`] borrowed, as the deletion guard takes it.
pub(crate) fn borrowed_users(users: &[(String, i64)]) -> Vec<(&str, i64)> {
    users
        .iter()
        .map(|(user, at)| (user.as_str(), *at))
        .collect()
}

/// D-M-6, after a commit: asks the deletion guard whether a run about `target` is now blocked by a
/// deletion, for the copy it just committed (`judge_trace_id`, and the score's target). A block
/// means that copy is stamped after its deletion's cut-off, which storage does not cover, so this
/// stores a follow-up deletion of `judge_trace_id` and `target` (`deletion::follow_up`). Returns
/// `true` when it blocked; a failed follow-up is logged and still counts as blocked.
pub(crate) async fn after_commit(
    state: &AppState,
    project: ProjectId,
    target: TraceId,
    users: &[(&str, i64)],
    judge_trace_id: TraceId,
) -> bool {
    let now = state.clock.now_ns();
    // D-M-6: the check after a commit.
    let Some(blocking) = state.deletions.guard.blocking(project, target, users, now) else {
        return false;
    };
    if let Err(error) = deletion::follow_up(state, blocking, &[judge_trace_id, target]).await {
        tracing::warn!(
            deletion_id = %blocking,
            project_id = %project,
            error = %error,
            "follow-up deletion failed; the copy waits for a later deletion"
        );
    }
    true
}

/// The judge trace id of a job's run (D-I-16): the first 16 bytes of
/// SHA-256(`cetra.judge-trace.v1` ‖ job id (16 bytes) ‖ the run's claims as u32 BE).
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

/// A job the dispatcher claimed. `claims` is the value after the claim; `finish` is fenced by it.
#[derive(Debug)]
pub(crate) struct ClaimedJob {
    pub(crate) row: JobRow,
    pub(crate) claims: u32,
    /// The judge connection's `updated_at` when the job was claimed (D-I-14); `None` for checks and webhooks.
    pub(crate) connection_updated_at: Option<i64>,
}

/// Runs one claimed job and records its outcome (§ Job execution). Every step's failure is a job
/// outcome; `Err` is only a `meta.db` failure, which leaves the job `running` for the janitor.
pub(crate) async fn run_job(state: &AppState, job: ClaimedJob) -> Result<(), MetaError> {
    // D-M-6, check 0: a job re-queued from `running` (a retry, the janitor, a restart) whose target
    // was deleted. Only the trace id is known here, so the trace is not loaded.
    if state
        .deletions
        .guard
        .blocking(
            job.row.project_id,
            job.row.trace_id,
            &[],
            state.clock.now_ns(),
        )
        .is_some()
    {
        return finish_job(
            state,
            &job,
            Ended::plain(Finish::Deleted {
                usage: JobUsage::default(),
            }),
            None,
            job.row.attempts,
        )
        .await;
    }
    let ready = match prepare(state, &job).await {
        Ok(ready) => ready,
        Err(Exit::Gone) => return Ok(()),
        Err(Exit::Ended(ended)) => {
            // No attempt was started: the count is the row's own.
            return finish_job(state, &job, *ended, None, job.row.attempts).await;
        }
        Err(Exit::Meta(error)) => return Err(error),
    };
    let Ready { reservation, call } = ready;
    let attempts = call.attempts;
    let ended = call_evaluator(state, &job, call).await;
    finish_job(state, &job, ended, reservation, attempts).await
}

/// Where a step leaves the job early: nothing to write, an outcome to write, or a `meta.db` error.
enum Exit {
    /// The job, its evaluator or its rule is gone: nothing to write (the rows cascade).
    Gone,
    /// The job ends here, without a call.
    Ended(Box<Ended>),
    /// `meta.db` failed; the job stays `running` for the janitor.
    Meta(MetaError),
}

impl From<MetaError> for Exit {
    fn from(error: MetaError) -> Self {
        Self::Meta(error)
    }
}

/// What the job's finish writes: the `finish` of § Job execution, the pause it causes, and the
/// spend of the call.
struct Ended {
    finish: Finish,
    /// The breaker counts this outcome (§ Job execution › The breaker).
    reached_evaluator: bool,
    pause: Pause,
    /// The call's cost, settled against the reservation (D-I-15).
    actual: f64,
}

impl Ended {
    fn plain(finish: Finish) -> Self {
        Self {
            finish,
            reached_evaluator: false,
            pause: Pause::None,
            actual: 0.0,
        }
    }
}

/// A pause written with the job's finish (D-I-14, D-I-15).
enum Pause {
    None,
    /// A judge's connection error on a rule job: its rules pause with `connection`, and the
    /// connection is `failing`.
    Connection {
        org: OrgId,
        connection_id: Uuid,
        error_text: String,
    },
    /// A manual judge job's connection error: the connection is `failing`; no rule pauses.
    MarkConnection {
        org: OrgId,
        connection_id: Uuid,
        error_text: String,
    },
    /// A webhook's error on a rule job: the evaluator's rules pause with `errors`.
    Evaluator {
        evaluator_id: Uuid,
        error_text: String,
    },
    /// One rule pauses (a budget, or a judge model without a price). `skip_queued` also skips the
    /// rule's queued jobs with `budget`.
    Rule {
        rule_id: Uuid,
        reason: &'static str,
        detail: &'static str,
        skip_queued: bool,
    },
}

/// Everything the call needs, decided by `prepare`.
struct Call {
    project: ProjectRow,
    evaluator: Evaluator,
    evaluator_version: u32,
    kind: EvaluatorKind,
    connection: Option<Arc<JudgeConnection>>,
    /// The judge's org and connection id (judge jobs only).
    judge_link: Option<(OrgId, Uuid)>,
    /// The judge's model is not its connection's default: a 404 for it pauses only the rules of
    /// the evaluator (I-2, D-I-14).
    judge_model_differs: bool,
    webhook_secret: Option<Secret>,
    variables: Variables,
    target: EvalTarget,
    origin: EvalOrigin,
    judge_trace_id: TraceId,
    session_id: Option<String>,
    user_id: Option<String>,
    /// The target's end users with their oldest ingest times (D-M-6, checks B to C).
    users: Vec<(String, i64)>,
    truncated: bool,
    /// The estimate reserved for a budgeted judge job.
    estimate: Option<f64>,
    /// The attempt count after this attempt was counted.
    attempts: u32,
}

/// What `prepare` hands the call: the call's data and, for a budgeted job, its reservation.
struct Ready<'a> {
    reservation: Option<Reservation<'a>>,
    call: Call,
}

/// A judge's connection, read and opened for a job (§ Job execution, step 4).
struct JudgeLink {
    org: OrgId,
    connection_id: Uuid,
    adapter: Adapter,
    connection: Arc<JudgeConnection>,
    /// The judge's model is not the connection's `default_model` (D-I-14).
    model_differs: bool,
}

/// Steps 1 to 11 of § Job execution: everything before the call.
async fn prepare<'a>(state: &'a AppState, job: &ClaimedJob) -> Result<Ready<'a>, Exit> {
    let row = &job.row;
    let project = active_project(state, row.project_id).await?;
    let reader = state.meta.reader();
    // Step 2.
    let evaluator_row = store::get_evaluator(reader, project.id, row.evaluator_id)
        .await?
        .ok_or(Exit::Gone)?;
    // Step 3.
    let rule = match row.rule_id {
        Some(id) => {
            let rule = online_rules::get_rule(reader, project.id, id)
                .await?
                .ok_or(Exit::Gone)?;
            if !rule.enabled || rule.paused_reason.is_some() {
                return Err(ended(Finish::Requeue {
                    not_before: state.clock.now_ns(),
                }));
            }
            Some(rule)
        }
        None => None,
    };
    // Step 4.
    let (evaluator, opened) = evaluator::load(&state.secrets, &evaluator_row)
        .map_err(|_| permanent("evaluator could not be loaded"))?;
    let judge = match &evaluator.config {
        EvaluatorConfig::LlmJudge(judge) => Some(
            judge_link(
                state,
                job,
                project.org_id,
                judge.connection_id,
                &judge.model,
            )
            .await?,
        ),
        EvaluatorConfig::Check(_) | EvaluatorConfig::Webhook(_) => None,
    };
    // Step 5.
    let mapping_text = match &rule {
        Some(rule) => Some(rule.variable_mapping.as_str()),
        None => row.variable_mapping.as_deref(),
    };
    let mapping = parse_mapping(mapping_text)?;
    // Step 6.
    let loaded = match trace_load::load_trace(
        state,
        project.id,
        Visibility::window(state, project.org_id),
        row.trace_id,
        row.span_id,
        Some(row.trace_start_ns),
    )
    .await
    {
        Ok(loaded) => loaded,
        Err(LoadError::Saturated) => {
            return Err(ended(Finish::Requeue {
                not_before: state.clock.now_ns() + retry::SATURATED_REQUEUE_NS,
            }));
        }
        Err(LoadError::NotFound | LoadError::NoTargetSpan) => {
            return Err(load_retry(state, job, "trace not found"));
        }
        Err(LoadError::Busy) => return Err(load_retry(state, job, "storage unavailable")),
        Err(LoadError::TooLarge) => return Err(permanent("trace too large")),
        Err(LoadError::EvaluationTrace) => {
            return Err(permanent("target is an evaluation trace"));
        }
        Err(LoadError::Failed(_) | LoadError::Convert(_)) => {
            return Err(permanent("storage read failed"));
        }
    };
    // D-M-6, check A: the target is loaded; nothing is reserved or sent yet.
    let users = users_of(&loaded.spans);
    if state
        .deletions
        .guard
        .blocking(
            project.id,
            row.trace_id,
            &borrowed_users(&users),
            state.clock.now_ns(),
        )
        .is_some()
    {
        return Err(ended(Finish::Deleted {
            usage: JobUsage::default(),
        }));
    }
    // Step 7.
    let data = TraceData::new(&loaded.spans, row.span_id);
    let variables =
        resolve(&mapping, Some(&data), None).map_err(|error| permanent(&error.to_string()))?;
    let root = loaded
        .spans
        .iter()
        .find(|span| span.parent_span_id.is_none());
    let session_id = root.and_then(|span| span.session_id.clone());
    let user_id = root.and_then(|span| span.user_id.clone());
    // Step 8.
    let now = state.clock.now_ns();
    let definitions = state.definitions.live(&state.meta, project.id, now).await?;
    if let Some(definition) = definitions
        .iter()
        .find(|definition| definition.name == evaluator.output.name)
    {
        let judge_output = match &evaluator.config {
            EvaluatorConfig::LlmJudge(judge) => Some(&judge.output),
            EvaluatorConfig::Check(_) | EvaluatorConfig::Webhook(_) => None,
        };
        if !fits_definition(&evaluator.output, judge_output, definition) {
            return Err(permanent("score definition mismatch"));
        }
    }
    // Step 9.
    if levenshtein_too_large(&evaluator, &variables) {
        return Err(permanent("levenshtein inputs must be at most 64 KiB each"));
    }
    // Before step 10 (D-I-35): a judge or webhook call is not made at the org's monthly units cap.
    if !matches!(evaluator.config, EvaluatorConfig::Check(_))
        && let Err(cap) = units::check_cap(state, project.org_id).await
    {
        tracing::debug!(job_id = %row.id, detail = %cap.detail, "eval job skipped at the units cap");
        return Err(ended(Finish::Skipped { error: "quota" }));
    }
    // Step 10.
    let mut estimate = None;
    let mut reservation = None;
    if let (Some(rule), Some(link), EvaluatorConfig::LlmJudge(judge)) =
        (rule.as_ref(), judge.as_ref(), &evaluator.config)
        && let (Some(limit), Some(period)) = (
            rule.budget_usd,
            rule.budget_period
                .as_deref()
                .and_then(BudgetPeriod::from_name),
        )
    {
        let (amount, guard) = reserve(
            state,
            project.org_id,
            rule,
            judge,
            link.adapter,
            limit,
            period,
            &variables,
        )
        .await?;
        estimate = Some(amount);
        reservation = Some(guard);
    }
    // Step 11.
    let now = state.clock.now_ns();
    let id = row.id;
    let claims = job.claims;
    let attempts = state
        .meta
        .write(move |conn| {
            Box::pin(async move { eval_jobs::start_attempt(conn, id, claims, now).await })
        })
        .await?
        .ok_or(Exit::Gone)?;
    // Step 12 is the call itself, in `call_evaluator`.
    let origin = match row.rule_id {
        Some(rule) => EvalOrigin::Rule(rule),
        None => EvalOrigin::Test,
    };
    let (connection, judge_link, judge_model_differs) = match judge {
        Some(link) => (
            Some(link.connection),
            Some((link.org, link.connection_id)),
            link.model_differs,
        ),
        None => (None, None, false),
    };
    let kind = kind_of(&evaluator.config);
    Ok(Ready {
        reservation,
        call: Call {
            project,
            evaluator_version: evaluator_row.version,
            kind,
            connection,
            judge_link,
            judge_model_differs,
            webhook_secret: opened.map(|values| Secret::new(values.signing_secret)),
            variables,
            target: EvalTarget {
                trace_id: Some(row.trace_id),
                span_id: row.span_id,
            },
            origin,
            judge_trace_id: job_judge_trace_id(row.id, job.claims),
            session_id,
            user_id,
            users,
            truncated: loaded.truncated,
            estimate,
            attempts,
            evaluator,
        },
    })
}

/// Step 1: the project and its org are both active.
async fn active_project(state: &AppState, id: ProjectId) -> Result<ProjectRow, Exit> {
    let reader = state.meta.reader();
    let Some(project) = projects::project_by_id(reader, id).await? else {
        return Err(skipped());
    };
    let org = orgs::org_by_id(reader, project.org_id).await?;
    if project.state != "active" || org.is_none_or(|org| org.state != "active") {
        return Err(skipped());
    }
    Ok(project)
}

/// Step 4 for a judge: its connection row (unchanged since the claim), adapter and key. `model` is
/// the judge's model; `model_differs` says whether it is not the connection's default (D-I-14).
async fn judge_link(
    state: &AppState,
    job: &ClaimedJob,
    org: OrgId,
    connection_id: Uuid,
    model: &str,
) -> Result<JudgeLink, Exit> {
    let row = connections::get_connection(state.meta.reader(), org, connection_id)
        .await?
        .ok_or_else(|| permanent("connection not found"))?;
    if job.connection_updated_at != Some(row.updated_at) {
        return Err(ended(Finish::Requeue {
            not_before: state.clock.now_ns(),
        }));
    }
    let adapter = Adapter::from_name(&row.adapter).map_err(|_| permanent("unknown adapter"))?;
    let connection = state
        .connections
        .get(&state.secrets, &row)
        .map_err(|_| permanent("connection could not be opened"))?;
    Ok(JudgeLink {
        org,
        connection_id,
        adapter,
        connection,
        model_differs: row.default_model.as_deref() != Some(model),
    })
}

/// A judge's 404 for its model, from the provider's status or its `model_not_found` code (I-2).
fn is_missing_model(error: &EvalError) -> bool {
    matches!(
        error,
        EvalError::Judge(JudgeError::Provider { status: 404, .. })
    ) || matches!(
        error,
        EvalError::Judge(JudgeError::Provider { provider_code: Some(code), .. })
            if code == "model_not_found"
    )
}

/// Step 5: the job's mapping, a rule's or a manual run's.
fn parse_mapping(text: Option<&str>) -> Result<VariableMapping, Exit> {
    let text = text.ok_or_else(|| permanent("variable mapping is missing"))?;
    let value: Value =
        serde_json::from_str(text).map_err(|_| permanent("variable mapping is not JSON"))?;
    VariableMapping::from_json(&value).map_err(|error| permanent(&error.to_string()))
}

/// Step 6: a trace load failure retries while the job has load retries left.
fn load_retry(state: &AppState, job: &ClaimedJob, error: &'static str) -> Exit {
    if job.row.load_retries < retry::LOAD_RETRIES {
        ended(Finish::LoadRetry {
            not_before: state.clock.now_ns() + retry::LOAD_RETRY_NS,
            error,
        })
    } else {
        permanent(error)
    }
}

/// Step 10: prices the call, reserves its estimate and returns the estimate with the guard.
#[allow(clippy::too_many_arguments)] // the state, the org, the rule, its judge and budget, and the variables
async fn reserve<'a>(
    state: &'a AppState,
    org: OrgId,
    rule: &RuleRow,
    judge: &JudgeConfig,
    adapter: Adapter,
    limit: f64,
    period: BudgetPeriod,
    variables: &Variables,
) -> Result<(f64, Reservation<'a>), Exit> {
    let now = state.clock.now_ns();
    let prompt = PromptTemplate::compile(&judge.prompt)
        .and_then(|template| template.render(variables))
        .map_err(|error| permanent(&error.to_string()))?;
    let prices = state.ingest.prices().for_project(org, rule.project_id);
    let Some(price) = budget::model_price(&prices, &judge.model, adapter, now) else {
        return Err(unpriced(rule.id));
    };
    let estimate = budget::estimate_usd(prompt.chars().count(), judge.max_output_tokens, price);
    if !estimate.is_finite() {
        return Err(unpriced(rule.id));
    }
    let period_start = budget::period_start(period, now);
    let budgets = &state.eval.budgets;
    let mut attempt = budgets.try_reserve(rule.id, period_start, limit, estimate);
    if matches!(attempt, Reserve::NotLoaded) {
        let spent = rule_spend::spent_between(
            state.meta.reader(),
            rule.id,
            period_start,
            budget::day_start(now) + budget::DAY_NS,
        )
        .await?;
        budgets.load(rule.id, period_start, spent);
        attempt = budgets.try_reserve(rule.id, period_start, limit, estimate);
    }
    match attempt {
        Reserve::Reserved(guard) => Ok((estimate, guard)),
        Reserve::Over => Err(over_budget(rule.id)),
        Reserve::NotLoaded => {
            tracing::debug!(rule_id = %rule.id, "budget not loaded; job requeued");
            Err(ended(Finish::Requeue { not_before: now }))
        }
    }
}

/// Steps 12 to 15: runs the evaluator, commits its judge trace and writes its score, and says how
/// the job ends. A write that keeps failing takes the retryable path (D-I-13).
async fn call_evaluator(state: &AppState, job: &ClaimedJob, call: Call) -> Ended {
    let Call {
        project,
        evaluator,
        evaluator_version,
        kind,
        connection,
        judge_link,
        judge_model_differs,
        webhook_secret,
        variables,
        target,
        origin,
        judge_trace_id,
        session_id,
        user_id,
        users,
        truncated,
        estimate,
        attempts,
    } = call;
    let row = &job.row;
    // The call's own time (its prices and windows). Every decision after the call reads a fresh one.
    let call_now = state.clock.now_ns();
    let output_name = evaluator.output.name.clone();
    let evaluator_id = evaluator.id;
    let started = Instant::now();
    let run = run_evaluator(
        state,
        RunArgs {
            project: &project,
            evaluator,
            connection,
            webhook_secret,
            variables,
            target,
            origin,
            judge_trace_id,
            now: call_now,
        },
    )
    .await;
    let latency_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    state
        .telemetry
        .eval_run(eval_kind_of(&kind), started.elapsed().as_secs_f64());
    // The call can take seconds: the retry, the writes and the finish use the time after it.
    let now = state.clock.now_ns();
    let run = match run {
        Ok(run) => run,
        Err(ExecuteError::Panicked(_)) => {
            return Ended::plain(Finish::Failed {
                class: "permanent",
                error: "internal error".to_owned(),
                judge_trace_id: None,
                evaluator_version: Some(evaluator_version),
                usage: JobUsage {
                    latency_ms: Some(latency_ms),
                    ..JobUsage::default()
                },
            });
        }
    };
    // Step 13: the usage counts before any write.
    let usage = judge_usage(&run);
    if let Some(usage) = usage {
        count_judge_usage(state, &project, now, usage);
    }
    let job_usage = JobUsage {
        input_tokens: usage.and_then(|usage| usage.input_tokens),
        output_tokens: usage.and_then(|usage| usage.output_tokens),
        cost_usd: usage.and_then(|usage| usage.cost_usd),
        latency_ms: Some(latency_ms),
    };
    let actual = spent(usage, estimate);
    let EvalRun {
        outcome,
        judge_trace,
    } = run;
    let guard_users = borrowed_users(&users);
    // Step 14: the judge trace, whether the run succeeded or not (D-I-16). Check B runs before each
    // try: a deletion that landed during the call commits nothing.
    let committed = retry_write(
        retry::WRITE_TRIES,
        retry::WRITE_RETRY,
        |_: &IngestError| true,
        || {
            commit_unless_deleted(
                state,
                &project,
                now,
                row.trace_id,
                &guard_users,
                judge_trace.clone(),
            )
        },
    )
    .await;
    match committed {
        Ok(true) => {}
        Ok(false) => return deleted(job_usage, actual),
        Err(_) => {
            return Ended {
                finish: retryable(job, attempts, now, STORE_ERROR, None, job_usage),
                reached_evaluator: false,
                pause: Pause::None,
                actual,
            };
        }
    }
    // D-M-6: the judge trace is stored, whether the evaluation succeeded or failed (D-I-16). A
    // block here stores the follow-up deletion of the judge trace and the target.
    if after_commit(
        state,
        project.id,
        row.trace_id,
        &guard_users,
        judge_trace_id,
    )
    .await
    {
        return deleted(job_usage, actual);
    }
    match outcome {
        // Step 15: the score.
        Ok(outcome) => {
            let EvalOutcome {
                value,
                reason,
                metadata,
                ..
            } = outcome;
            let item = ScoreCreate {
                score_id: None,
                target: score_target(row),
                session_id,
                user_id,
                name: output_name,
                value,
                reason,
                source: None,
                metadata: score_metadata(metadata, row.id, evaluator_version, truncated),
                experiment_id: None,
                dataset_item_id: None,
                queue_id: None,
            };
            let writer = Writer::Evaluator {
                kind,
                evaluator_id,
                rule_id: row.rule_id,
            };
            // A refusal (a bad request, the units cap) is final: it is not retried (D-I-35).
            // Check B2 runs before each try of the write.
            let written = retry_write(
                retry::WRITE_TRIES,
                retry::WRITE_RETRY,
                |error: &ApiError| {
                    !matches!(
                        error,
                        ApiError::BadRequest { .. } | ApiError::QuotaExceeded { .. }
                    )
                },
                || {
                    write_score_unless_deleted(
                        state,
                        &project,
                        writer,
                        &item,
                        row.trace_id,
                        &guard_users,
                        judge_trace_id,
                    )
                },
            )
            .await;
            let written = match written {
                // D-M-6, check B2 blocked: no score was written.
                Ok(ScoreWrite::Blocked) => return deleted(job_usage, actual),
                Ok(ScoreWrite::Written(score)) => Ok(score),
                Err(error) => Err(error),
            };
            match written {
                Ok(Some(score_id)) => {
                    // D-M-6, check C: the score is stored; a block here stores its follow-up too.
                    if after_commit(
                        state,
                        project.id,
                        row.trace_id,
                        &guard_users,
                        judge_trace_id,
                    )
                    .await
                    {
                        return deleted(job_usage, actual);
                    }
                    Ended {
                        finish: Finish::Done {
                            judge_trace_id,
                            score_id,
                            evaluator_version,
                            usage: job_usage,
                        },
                        reached_evaluator: true,
                        pause: Pause::None,
                        actual,
                    }
                }
                Err(ApiError::BadRequest { .. }) => Ended {
                    finish: Finish::Failed {
                        class: "permanent",
                        error: "score definition mismatch".to_owned(),
                        judge_trace_id: Some(judge_trace_id),
                        evaluator_version: Some(evaluator_version),
                        usage: job_usage,
                    },
                    reached_evaluator: false,
                    pause: Pause::None,
                    actual,
                },
                Err(ApiError::QuotaExceeded { .. }) => Ended {
                    finish: Finish::Failed {
                        class: "permanent",
                        error: UNITS_CAP_ERROR.to_owned(),
                        judge_trace_id: Some(judge_trace_id),
                        evaluator_version: Some(evaluator_version),
                        usage: job_usage,
                    },
                    reached_evaluator: false,
                    pause: Pause::None,
                    actual,
                },
                Ok(None) | Err(_) => Ended {
                    finish: retryable(
                        job,
                        attempts,
                        now,
                        STORE_ERROR,
                        Some(judge_trace_id),
                        job_usage,
                    ),
                    reached_evaluator: false,
                    pause: Pause::None,
                    actual,
                },
            }
        }
        Err(error) => {
            let reached_evaluator = matches!(
                error,
                EvalError::Judge(_) | EvalError::Webhook(_) | EvalError::Egress(_)
            );
            let text = error.to_string();
            let (finish, pause) = match error.class() {
                ErrorClass::Retryable => (
                    retryable(job, attempts, now, &text, Some(judge_trace_id), job_usage),
                    Pause::None,
                ),
                ErrorClass::Permanent => (
                    Finish::Failed {
                        class: "permanent",
                        error: text,
                        judge_trace_id: Some(judge_trace_id),
                        evaluator_version: Some(evaluator_version),
                        usage: job_usage,
                    },
                    Pause::None,
                ),
                ErrorClass::Pause => {
                    let failed = Finish::Failed {
                        class: "pause",
                        error: text.clone(),
                        judge_trace_id: Some(judge_trace_id),
                        evaluator_version: Some(evaluator_version),
                        usage: job_usage,
                    };
                    // A missing model on a judge that is not the connection's default names the
                    // model, not the connection (I-2): the evaluator's rules pause, as for a webhook.
                    let model_only = judge_model_differs && is_missing_model(&error);
                    match (kind, row.rule_id, judge_link) {
                        (EvaluatorKind::LlmJudge, Some(_), Some(_)) if model_only => (
                            Finish::PauseRequeue { not_before: now },
                            Pause::Evaluator {
                                evaluator_id,
                                error_text: text,
                            },
                        ),
                        (EvaluatorKind::LlmJudge, None, Some(_)) if model_only => {
                            (failed, Pause::None)
                        }
                        (EvaluatorKind::LlmJudge, Some(_), Some((org, connection_id))) => (
                            Finish::PauseRequeue { not_before: now },
                            Pause::Connection {
                                org,
                                connection_id,
                                error_text: text,
                            },
                        ),
                        (EvaluatorKind::LlmJudge, None, Some((org, connection_id))) => (
                            failed,
                            Pause::MarkConnection {
                                org,
                                connection_id,
                                error_text: text,
                            },
                        ),
                        (EvaluatorKind::Webhook, Some(_), _) => (
                            Finish::PauseRequeue { not_before: now },
                            Pause::Evaluator {
                                evaluator_id,
                                error_text: text,
                            },
                        ),
                        _ => (failed, Pause::None),
                    }
                }
            };
            Ended {
                finish,
                reached_evaluator,
                pause,
                actual,
            }
        }
    }
}

/// Step 16: writes the job's outcome, its pauses and its spend in one write, then the breaker
/// (§ Job execution). `reservation`, when there is one, settles with `ended.actual`.
///
/// # Errors
///
/// Returns [`MetaError`] when the write fails; the job then stays `running` for the janitor.
async fn finish_job(
    state: &AppState,
    job: &ClaimedJob,
    ended: Ended,
    reservation: Option<Reservation<'_>>,
    attempts: u32,
) -> Result<(), MetaError> {
    let Ended {
        finish,
        reached_evaluator,
        pause,
        actual,
    } = ended;
    let now = state.clock.now_ns();
    let row = &job.row;
    let failed = !matches!(finish, Finish::Done { .. });
    // The breaker counts only the job's final outcome: a retry, a requeue or a pause that
    // downgraded to none is not a result of the evaluator (§ Job execution › The breaker).
    let counts = reached_evaluator && matches!(finish, Finish::Done { .. } | Finish::Failed { .. });
    let label = finish_label(&finish);
    let counted = eval_outcome(&finish);
    // Only a budgeted job holds a reservation, and its spend goes to its rule (D-I-15).
    let spend = match (&reservation, row.rule_id) {
        (Some(_), Some(rule)) if actual > 0.0 => Some((rule, budget::day_start(now), actual)),
        _ => None,
    };
    let settle = Settle {
        job: row.id,
        claims: job.claims,
        now,
        spend,
        connection_updated_at: job.connection_updated_at,
        finish,
        pause,
    };
    let result = state
        .meta
        .write(move |conn| Box::pin(async move { write_outcome(conn, settle).await }))
        .await;
    if let Some(reservation) = reservation {
        reservation.settle(actual);
    }
    let settled = result?;
    if settled.matched
        && let Some(outcome) = counted
    {
        state.telemetry.finish_eval(outcome);
    }
    tracing::debug!(
        job_id = %row.id,
        kind = %row.kind,
        status = label,
        attempts,
        matched = settled.matched,
        "eval job finished"
    );
    for (rule_id, reason) in &settled.paused {
        audit_pause(rule_id, row.project_id, reason);
    }
    if let Some(rule) = settled.skip_rule {
        // A budget pause skips the rule's backlog in chunks of 1 000 per write (I-3).
        eval_jobs::skip_queued_of_rule(&state.meta, rule, "budget", now).await?;
    }
    let Some(rule) = row.rule_id else {
        return Ok(());
    };
    if settled.matched && counts && state.eval.breakers.record(rule, failed) {
        let tripped = state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    online_rules::pause_rule(conn, rule, PAUSE_ERRORS, Some(BREAKER_DETAIL), now)
                        .await
                })
            })
            .await?;
        if tripped {
            audit_pause(&rule, row.project_id, PAUSE_ERRORS);
        }
    }
    Ok(())
}

/// What `finish_job` writes, owned so the write closure can take it.
struct Settle {
    job: Uuid,
    claims: u32,
    now: i64,
    /// `(rule, day, amount)` to add to `rule_spend`.
    spend: Option<(Uuid, i64, f64)>,
    connection_updated_at: Option<i64>,
    finish: Finish,
    pause: Pause,
}

/// What the write did: whether the job's row matched, the rules it paused, and a rule whose queued
/// jobs are still to be skipped with `budget` (in chunks, after this write).
struct Settled {
    matched: bool,
    paused: Vec<(Uuid, &'static str)>,
    skip_rule: Option<Uuid>,
}

/// The write of § Job execution step 16, on the writer.
async fn write_outcome(conn: &mut SqliteConnection, settle: Settle) -> Result<Settled, MetaError> {
    let Settle {
        job,
        claims,
        now,
        spend,
        connection_updated_at,
        finish,
        pause,
    } = settle;
    if let Some((rule, day, amount)) = spend {
        rule_spend::add_spend(conn, rule, day, amount).await?;
    }
    // A connection that changed during the call has nothing to pause (D-I-14).
    let pause = match pause {
        Pause::Connection {
            org,
            connection_id,
            error_text,
        } => {
            if connection_unchanged(conn, org, connection_id, connection_updated_at).await? {
                Pause::Connection {
                    org,
                    connection_id,
                    error_text,
                }
            } else {
                Pause::None
            }
        }
        Pause::MarkConnection {
            org,
            connection_id,
            error_text,
        } => {
            if connection_unchanged(conn, org, connection_id, connection_updated_at).await? {
                Pause::MarkConnection {
                    org,
                    connection_id,
                    error_text,
                }
            } else {
                Pause::None
            }
        }
        other => other,
    };
    if !eval_jobs::finish(conn, job, claims, finish, now).await? {
        return Ok(Settled {
            matched: false,
            paused: Vec::new(),
            skip_rule: None,
        });
    }
    let mut paused = Vec::new();
    let mut skip_rule = None;
    match pause {
        Pause::None => {}
        Pause::Connection {
            connection_id,
            error_text,
            ..
        } => {
            for rule in online_rules::pause_rules_of_connection(
                conn,
                connection_id,
                Some(error_text.as_str()),
                now,
            )
            .await?
            {
                paused.push((rule, PAUSE_CONNECTION));
            }
            connections::set_connection_status(
                conn,
                connection_id,
                "failing",
                Some(error_text.as_str()),
                now,
            )
            .await?;
        }
        Pause::MarkConnection {
            connection_id,
            error_text,
            ..
        } => {
            connections::set_connection_status(
                conn,
                connection_id,
                "failing",
                Some(error_text.as_str()),
                now,
            )
            .await?;
        }
        Pause::Evaluator {
            evaluator_id,
            error_text,
        } => {
            for rule in online_rules::pause_rules_of_evaluator(
                conn,
                evaluator_id,
                PAUSE_ERRORS,
                Some(error_text.as_str()),
                now,
            )
            .await?
            {
                paused.push((rule, PAUSE_ERRORS));
            }
        }
        Pause::Rule {
            rule_id,
            reason,
            detail,
            skip_queued,
        } => {
            if online_rules::pause_rule(conn, rule_id, reason, Some(detail), now).await? {
                paused.push((rule_id, reason));
            }
            if skip_queued {
                skip_rule = Some(rule_id);
            }
        }
    }
    Ok(Settled {
        matched: true,
        paused,
        skip_rule,
    })
}

/// True when the connection row still has the `updated_at` the job was claimed with.
pub(crate) async fn connection_unchanged(
    conn: &mut SqliteConnection,
    org: OrgId,
    connection_id: Uuid,
    claimed_at: Option<i64>,
) -> Result<bool, MetaError> {
    let row = connections::get_connection_tx(conn, org, connection_id).await?;
    Ok(row.is_some_and(|row| Some(row.updated_at) == claimed_at))
}

fn audit_pause(rule_id: &Uuid, project_id: ProjectId, reason: &str) {
    tracing::info!(
        target: "audit",
        rule_id = %rule_id,
        project_id = %project_id,
        reason,
        "online rule paused"
    );
}

/// The retry of a job that did not finish: a later attempt, or `failed` once the attempts run out.
fn retryable(
    job: &ClaimedJob,
    attempts: u32,
    now: i64,
    error: &str,
    judge_trace_id: Option<TraceId>,
    usage: JobUsage,
) -> Finish {
    if attempts < retry::MAX_ATTEMPTS {
        Finish::Retry {
            not_before: now + retry::backoff_ns(job.row.id, attempts),
            error: error.to_owned(),
            judge_trace_id,
            usage,
        }
    } else {
        Finish::Failed {
            class: "retryable",
            error: error.to_owned(),
            judge_trace_id,
            evaluator_version: None,
            usage,
        }
    }
}

/// Check B (D-M-6): commits the judge trace unless the deletion guard blocks its target now.
/// `Ok(false)` when it blocks: nothing is committed.
pub(crate) async fn commit_unless_deleted(
    state: &AppState,
    project: &ProjectRow,
    now: i64,
    target: TraceId,
    users: &[(&str, i64)],
    judge_trace: Normalized,
) -> Result<bool, IngestError> {
    // D-M-6: check B, before this try of the commit.
    if state
        .deletions
        .guard
        .blocking(project.id, target, users, state.clock.now_ns())
        .is_some()
    {
        return Ok(false);
    }
    commit_judge_trace(state, project, now, judge_trace)
        .await
        .map(|_| true)
}

/// What one try of the score write of step 15 did (D-M-6).
enum ScoreWrite {
    /// Check B2 blocked the write: no score was written, and the follow-up deletion of the judge
    /// trace and the target is stored.
    Blocked,
    /// The write went through; the id is the first score it stored, `None` when it returned none.
    Written(Option<Uuid>),
}

/// Check B2 (D-M-6) and the score write: after the judge trace committed, writes `item` unless the
/// guard blocks its target now.
async fn write_score_unless_deleted(
    state: &AppState,
    project: &ProjectRow,
    writer: Writer,
    item: &ScoreCreate,
    target: TraceId,
    users: &[(&str, i64)],
    judge_trace_id: TraceId,
) -> Result<ScoreWrite, ApiError> {
    // D-M-6: check B2, before this try of the write.
    if after_commit(state, project.id, target, users, judge_trace_id).await {
        return Ok(ScoreWrite::Blocked);
    }
    let ids = scores::write(state, project, writer, vec![item.clone()]).await?;
    Ok(ScoreWrite::Written(ids.first().copied()))
}

/// An outcome of a run that a deletion stopped after the call (D-M-6): the call's usage and cost
/// are kept, the breaker and the pauses are not touched (D-I-14).
fn deleted(usage: JobUsage, actual: f64) -> Ended {
    Ended {
        finish: Finish::Deleted { usage },
        reached_evaluator: false,
        pause: Pause::None,
        actual,
    }
}

/// Runs `write` up to `tries` times, `delay` apart, while `retry_on` says the error is worth another
/// try (D-I-13).
pub(crate) async fn retry_write<T, E, F, Fut>(
    tries: u32,
    delay: Duration,
    retry_on: impl Fn(&E) -> bool,
    mut write: F,
) -> Result<T, E>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, E>>,
{
    let mut attempt = 1;
    loop {
        match write().await {
            Ok(value) => return Ok(value),
            Err(error) if attempt >= tries || !retry_on(&error) => return Err(error),
            Err(_) => {
                attempt += 1;
                tokio::time::sleep(delay).await;
            }
        }
    }
}

/// The budget's cost of one run (D-I-15): the call's cost, else the estimate when the call used
/// tokens, else 0.
pub(crate) fn spent(usage: Option<JudgeUsage>, estimate: Option<f64>) -> f64 {
    let Some(usage) = usage else {
        return 0.0;
    };
    match usage.cost_usd {
        Some(cost) => cost,
        None if usage.input_tokens.is_some() || usage.output_tokens.is_some() => {
            estimate.unwrap_or(0.0)
        }
        None => 0.0,
    }
}

/// The score target of a job: its span when it has one, else its trace.
fn score_target(row: &JobRow) -> ScoreTarget {
    match row.span_id {
        Some(span_id) => ScoreTarget::Span {
            trace_id: row.trace_id,
            span_id,
        },
        None => ScoreTarget::Trace {
            trace_id: row.trace_id,
        },
    }
}

/// The metadata of a queued job's score: the run's `judge.*` keys, then the job's own (D-I-18).
fn score_metadata(
    mut metadata: BTreeMap<String, String>,
    job: Uuid,
    evaluator_version: u32,
    truncated: bool,
) -> BTreeMap<String, String> {
    metadata.insert(JOB_ID_KEY.to_owned(), job.to_string());
    metadata.insert(
        EVALUATOR_VERSION_KEY.to_owned(),
        evaluator_version.to_string(),
    );
    if truncated {
        metadata.insert(TRUNCATED_KEY.to_owned(), "true".to_owned());
    }
    metadata
}

/// The telemetry outcome of a finish that leaves the job in a final status (phase 06). `None`
/// for a finish that puts the job back in the queue. A deleted job ends as `skipped`.
fn eval_outcome(finish: &Finish) -> Option<crate::telemetry::EvalOutcome> {
    use crate::telemetry::EvalOutcome as Counted;
    match finish {
        Finish::Done { .. } => Some(Counted::done),
        Finish::Failed { .. } => Some(Counted::failed),
        Finish::Skipped { .. } | Finish::Deleted { .. } => Some(Counted::skipped),
        _ => None,
    }
}

/// The `kind` label of an evaluator's run time (phase 06).
fn eval_kind_of(kind: &EvaluatorKind) -> crate::telemetry::EvalKind {
    use crate::telemetry::EvalKind as Counted;
    match kind {
        EvaluatorKind::LlmJudge => Counted::llm_judge,
        EvaluatorKind::Check => Counted::check,
        EvaluatorKind::Webhook => Counted::webhook,
    }
}

fn kind_of(config: &EvaluatorConfig) -> EvaluatorKind {
    match config {
        EvaluatorConfig::LlmJudge(_) => EvaluatorKind::LlmJudge,
        EvaluatorConfig::Check(_) => EvaluatorKind::Check,
        EvaluatorConfig::Webhook(_) => EvaluatorKind::Webhook,
    }
}

/// The job's last status as a word for the debug log.
fn finish_label(finish: &Finish) -> &'static str {
    match finish {
        Finish::Done { .. } => "done",
        Finish::Failed { .. } => "failed",
        Finish::Retry { .. } => "retry",
        Finish::LoadRetry { .. } => "load_retry",
        Finish::Requeue { .. } | Finish::PauseRequeue { .. } => "requeued",
        Finish::Skipped { .. } => "skipped",
        Finish::Deleted { .. } => "deleted",
    }
}

/// An early outcome of § Job execution that writes `finish` and nothing else.
fn ended(finish: Finish) -> Exit {
    Exit::Ended(Box::new(Ended::plain(finish)))
}

/// A permanent failure before the call.
fn permanent(error: &str) -> Exit {
    ended(Finish::Failed {
        class: "permanent",
        error: error.to_owned(),
        judge_trace_id: None,
        evaluator_version: None,
        usage: JobUsage::default(),
    })
}

/// The project is gone or inactive: the job is skipped.
fn skipped() -> Exit {
    ended(Finish::Skipped {
        error: "project unavailable",
    })
}

/// A judge model with no usable price (§ Job execution, step 10): the job fails and the rule
/// pauses with `errors`. Also used when the estimate is not finite, since it cannot be budgeted.
fn unpriced(rule_id: Uuid) -> Exit {
    Exit::Ended(Box::new(Ended {
        finish: Finish::Failed {
            class: "permanent",
            error: "model has no price".to_owned(),
            judge_trace_id: None,
            evaluator_version: None,
            usage: JobUsage::default(),
        },
        reached_evaluator: false,
        pause: Pause::Rule {
            rule_id,
            reason: PAUSE_ERRORS,
            detail: "model has no price",
            skip_queued: false,
        },
        actual: 0.0,
    }))
}

/// Over budget: the job is skipped and the rule pauses (D-I-15).
fn over_budget(rule_id: Uuid) -> Exit {
    Exit::Ended(Box::new(Ended {
        finish: Finish::Skipped { error: "budget" },
        reached_evaluator: false,
        pause: Pause::Rule {
            rule_id,
            reason: PAUSE_BUDGET,
            detail: "budget reached",
            skip_queued: true,
        },
        actual: 0.0,
    }))
}

#[cfg(test)]
mod tests {
    use cetra_eval::{CheckError, EvalError};
    use cetra_otel::Rejections;
    use serde_json::json;

    use super::*;

    /// The first 16 bytes of the hash, computed outside this code.
    const PINNED_JUDGE_TRACE_ID: &str = "fec6474a00764aaf4c87b3c771237ac1";

    #[test]
    fn job_judge_trace_id_is_pinned() {
        // SHA-256 of the tag, then the 16 bytes of job 0800 (`…0800`), then claims 1 as u32 BE.
        let job = Uuid::from_u128(0x0190_0000_0000_7000_8000_0000_0000_0800);
        let id = job_judge_trace_id(job, 1);
        assert_eq!(id.to_string(), PINNED_JUDGE_TRACE_ID);
        assert_ne!(job_judge_trace_id(job, 2), id);
    }

    #[test]
    fn levenshtein_inputs_are_capped_at_64_kib_each() {
        use cetra_eval::{Evaluator, EvaluatorConfig, OutputScore, Variables};
        let evaluator = Evaluator {
            id: Uuid::nil(),
            name: "Similar".to_owned(),
            config: EvaluatorConfig::from_parts("check", &json!({"check": "levenshtein"}))
                .expect("config"),
            output: serde_json::from_value::<OutputScore>(
                json!({"name": "similar", "data_type": "numeric"}),
            )
            .expect("output"),
        };
        let with_output = |len: usize| {
            Variables::from_map(BTreeMap::from([
                ("output".to_owned(), json!("x".repeat(len))),
                ("expected".to_owned(), json!("x")),
            ]))
        };
        assert!(!levenshtein_too_large(
            &evaluator,
            &with_output(MAX_LEVENSHTEIN_BYTES)
        ));
        assert!(levenshtein_too_large(
            &evaluator,
            &with_output(MAX_LEVENSHTEIN_BYTES + 1)
        ));
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
                discarded: 0,
            },
        };
        assert_eq!(judge_usage(&run), None);
    }
}

#[cfg(test)]
mod job_tests {
    use std::cell::Cell;
    use std::time::Duration;

    use axum::http::{Method, header};
    use cetra_core::enums::PayloadFormat;
    use cetra_core::ids::{SpanId, TraceId};
    use cetra_core::score::ScoreValue;
    use cetra_core::span::Span;
    use serde_json::{Value, json};

    use super::*;
    use crate::eval::trace_load::MAX_LOADS;
    use crate::meta::connections::{ConnectionRow, get_connection};
    use crate::meta::deletions::{
        ActorKind, DeletionKind, DeletionState, NewDeletion, RequestDetails, deletion_by_id,
        insert_deletion, insert_traces, scope_traces,
    };
    use crate::meta::eval_jobs::{KIND_MANUAL, KIND_RULE, NewJob, insert_job};
    use crate::meta::evaluators;
    use crate::meta::online_rules::{NewRule, RuleFields, insert_rule};
    use crate::test_support::mock_llm::{MockLlm, Reply, completion};
    use crate::test_support::mock_webhook::MockWebhook;
    use crate::test_support::set_limits;
    use crate::test_support::{Seeded, TestApp, dataset_span};

    const MAPPING: &str = r#"{"input": "trace.input", "output": "trace.output"}"#;
    const QUESTION: &str = "What is the refund window?";
    const ANSWER: &str = "30 days.";
    /// A priced model, so a budget can be set; the mock's replies name `gpt-test`, which is not.
    const PRICED: &str = "gpt-4o-mini";
    const JUDGE_ANSWER: &str = r#"{"explanation": "Relevant.", "value": 0.9}"#;

    fn trace(n: u128) -> TraceId {
        TraceId::from_bytes(*Uuid::from_u128(n).as_bytes()).expect("not all zero")
    }

    fn span(n: u64) -> SpanId {
        SpanId::from_bytes(n.to_be_bytes()).expect("not all zero")
    }

    /// A test app that may reach the mock on `127.0.0.1`. The clock is the real time before
    /// anything is seeded (`crate::routes::evaluators` tests, § Conventions).
    async fn test_app() -> (TestApp, Seeded) {
        let app = TestApp::with_config(|config| {
            config.security.allow_private_network_egress = true;
        })
        .await;
        seeded_in(app).await
    }

    /// [`test_app`]'s clock and seed, for an app built some other way.
    async fn seeded_in(app: TestApp) -> (TestApp, Seeded) {
        let real_now = i64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("after the epoch")
                .as_nanos(),
        )
        .expect("fits i64");
        app.state.clock.set(real_now);
        let seeded = app.seed().await;
        (app, seeded)
    }

    /// Stores a root span of `trace` (the question and its answer) in `environment`.
    async fn store_trace(app: &TestApp, project: &ProjectRow, trace: TraceId, environment: &str) {
        store_span_with(app, project, trace, environment, ANSWER).await;
    }

    /// [`store_trace`] with `output` as the answer.
    async fn store_span_with(
        app: &TestApp,
        project: &ProjectRow,
        trace: TraceId,
        environment: &str,
        output: &str,
    ) {
        let now = app.state.clock.now_ns();
        let mut row: Span = dataset_span(0);
        row.trace_id = trace;
        row.span_id = span(1);
        row.parent_span_id = None;
        row.input = Some(QUESTION.to_owned());
        row.input_format = Some(PayloadFormat::Text);
        row.output = Some(output.to_owned());
        row.output_format = Some(PayloadFormat::Text);
        row.environment = Some(environment.to_owned());
        row.session_id = Some("session-7".to_owned());
        row.user_id = Some("user-7".to_owned());
        row.start_time = now - 30_000_000_000;
        row.end_time = now - 20_000_000_000;
        app.commit_spans(project, vec![row]).await;
    }

    /// `POST …/llm-connections` for `acme`, reaching `mock` with the default model `gpt-test`.
    async fn connection(app: &TestApp, cookie: &str, mock: &MockLlm) -> Uuid {
        let response = app
            .request(Method::POST, "/api/v1/orgs/acme/llm-connections")
            .header(header::COOKIE.as_str(), cookie)
            .json(&json!({
                "name": "Mocked",
                "adapter": "openai",
                "api_key": "sk-test-key-0123456789",
                "base_url": mock.base_url(),
                "default_model": "gpt-test"
            }))
            .send()
            .await;
        assert_eq!(
            response.status,
            201,
            "{}",
            String::from_utf8_lossy(&response.body)
        );
        let value: Value = response.json();
        Uuid::parse_str(value["id"].as_str().expect("id")).expect("uuid")
    }

    /// A judge on `connection` with `model`, named `Relevance`.
    async fn judge(app: &TestApp, seeded: &Seeded, connection: Uuid, model: &str) -> Uuid {
        let response = app
            .request(
                Method::POST,
                &format!("/api/v1/projects/{}/evaluators", seeded.project.id),
            )
            .header(header::COOKIE.as_str(), &seeded.cookie)
            .json(&json!({
                "name": "Relevance",
                "kind": "llm_judge",
                "config": {
                    "connection_id": connection,
                    "model": model,
                    "prompt": "Question: {{ input }}\nAnswer: {{ output }}",
                    "output": {"type": "numeric", "min": 0.0, "max": 1.0},
                    "include_reason": true,
                    "temperature": null,
                    "max_output_tokens": 1024,
                    "timeout_ms": 60000
                },
                "output_score": {"name": "relevance", "data_type": "numeric"}
            }))
            .send()
            .await;
        assert_eq!(
            response.status,
            201,
            "{}",
            String::from_utf8_lossy(&response.body)
        );
        let value: Value = response.json();
        Uuid::parse_str(value["id"].as_str().expect("id")).expect("uuid")
    }

    /// An `exact_match` check that writes a boolean named `same`.
    async fn check(app: &TestApp, seeded: &Seeded) -> Uuid {
        let response = app
            .request(
                Method::POST,
                &format!("/api/v1/projects/{}/evaluators", seeded.project.id),
            )
            .header(header::COOKIE.as_str(), &seeded.cookie)
            .json(&json!({
                "name": "Same",
                "kind": "check",
                "config": {"check": "exact_match"},
                "output_score": {"name": "same", "data_type": "boolean"}
            }))
            .send()
            .await;
        assert_eq!(
            response.status,
            201,
            "{}",
            String::from_utf8_lossy(&response.body)
        );
        let value: Value = response.json();
        Uuid::parse_str(value["id"].as_str().expect("id")).expect("uuid")
    }

    /// A rule on `evaluator`, enabled, with an optional budget `(usd, period)`.
    async fn rule(
        app: &TestApp,
        seeded: &Seeded,
        evaluator: Uuid,
        budget: Option<(f64, &str)>,
        mapping: &str,
    ) -> Uuid {
        let id = Uuid::now_v7();
        let project = seeded.project.id;
        let now = app.state.clock.now_ns();
        let fields = RuleFields {
            name: format!("rule-{id}"),
            filter: None,
            variable_mapping: mapping.to_owned(),
            sample_rate: 1.0,
            budget_usd: budget.map(|(usd, _)| usd),
            budget_period: budget.map(|(_, period)| period.to_owned()),
            enabled: true,
        };
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    insert_rule(
                        conn,
                        NewRule {
                            id,
                            project_id: project,
                            evaluator_id: evaluator,
                            target: "trace".to_owned(),
                            created_by: None,
                        },
                        fields,
                        now,
                    )
                    .await
                    .map(|_| ())
                })
            })
            .await
            .expect("rule");
        id
    }

    /// A queued job of `kind` for `trace`, and its id.
    fn new_job(
        project: ProjectId,
        kind: &'static str,
        rule_id: Option<Uuid>,
        evaluator: Uuid,
        mapping: Option<&str>,
        trace: TraceId,
        now: i64,
    ) -> (Uuid, NewJob) {
        let id = Uuid::now_v7();
        (
            id,
            NewJob {
                id,
                project_id: project,
                kind,
                rule_id,
                evaluator_id: evaluator,
                variable_mapping: mapping.map(str::to_owned),
                trace_id: trace,
                span_id: None,
                trace_start_ns: now - 30_000_000_000,
            },
        )
    }

    async fn enqueue(app: &TestApp, job: NewJob) {
        let now = app.state.clock.now_ns();
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move { insert_job(conn, job, now).await.map(|_| ()) })
            })
            .await
            .expect("enqueue");
    }

    /// Claims `job` as the dispatcher would and reads the claimed row.
    async fn claimed(
        app: &TestApp,
        project: &ProjectRow,
        job: Uuid,
        judge_connection: Option<Uuid>,
    ) -> ClaimedJob {
        let now = app.state.clock.now_ns();
        let claims = app
            .state
            .meta
            .write(move |conn| Box::pin(async move { eval_jobs::claim(conn, job, now).await }))
            .await
            .expect("claim write")
            .expect("claimed");
        let row = job_row(app, project, job).await;
        let connection_updated_at = match judge_connection {
            Some(id) => Some(
                get_connection(app.state.meta.reader(), project.org_id, id)
                    .await
                    .expect("read")
                    .expect("connection")
                    .updated_at,
            ),
            None => None,
        };
        ClaimedJob {
            row,
            claims,
            connection_updated_at,
        }
    }

    async fn job_row(app: &TestApp, project: &ProjectRow, job: Uuid) -> JobRow {
        eval_jobs::get_job(app.state.meta.reader(), project.id, job)
            .await
            .expect("read")
            .expect("job")
    }

    async fn rule_row(app: &TestApp, project: &ProjectRow, id: Uuid) -> RuleRow {
        online_rules::get_rule(app.state.meta.reader(), project.id, id)
            .await
            .expect("read")
            .expect("rule")
    }

    async fn connection_row(app: &TestApp, project: &ProjectRow, id: Uuid) -> ConnectionRow {
        get_connection(app.state.meta.reader(), project.org_id, id)
            .await
            .expect("read")
            .expect("connection")
    }

    /// Claims and runs one job. Returns the row after the run.
    async fn run_once(
        app: &TestApp,
        project: &ProjectRow,
        job: Uuid,
        judge_connection: Option<Uuid>,
    ) -> JobRow {
        let claimed = claimed(app, project, job, judge_connection).await;
        run_job(&app.state, claimed).await.expect("run");
        job_row(app, project, job).await
    }

    /// Moves the clock to the job's `not_before`, so the next run is due.
    async fn due(app: &TestApp, project: &ProjectRow, job: Uuid) {
        let row = job_row(app, project, job).await;
        app.state.clock.set(row.not_before);
    }

    async fn set_paused(app: &TestApp, rule: Uuid) {
        let now = app.state.clock.now_ns();
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    online_rules::pause_rule(conn, rule, PAUSE_ERRORS, Some("test"), now)
                        .await
                        .map(|_| ())
                })
            })
            .await
            .expect("pause");
    }

    fn reply_status(status: u16, message: &str) -> Reply {
        Reply {
            status,
            body: json!({"error": {"message": message}}),
        }
    }

    /// A 200 completion whose response names `model`, so the call's cost is that model's.
    fn completion_for(model: &str, content: &str) -> Reply {
        let mut reply = completion(content);
        reply.body["model"] = json!(model);
        reply
    }

    /// A webhook evaluator named `Hook` at `url`, with a signing secret, outputting `hook`.
    async fn webhook(app: &TestApp, seeded: &Seeded, url: &str) -> Uuid {
        let response = app
            .request(
                Method::POST,
                &format!("/api/v1/projects/{}/evaluators", seeded.project.id),
            )
            .header(header::COOKIE.as_str(), &seeded.cookie)
            .json(&json!({
                "name": "Hook",
                "kind": "webhook",
                "config": {"url": url},
                "output_score": {"name": "hook", "data_type": "numeric"},
                "webhook_secret": "0123456789abcdef"
            }))
            .send()
            .await;
        assert_eq!(
            response.status,
            201,
            "{}",
            String::from_utf8_lossy(&response.body)
        );
        let value: Value = response.json();
        Uuid::parse_str(value["id"].as_str().expect("id")).expect("uuid")
    }

    /// A live categorical definition of `name` in the project (the judge's score is numeric).
    async fn categorical_definition(app: &TestApp, seeded: &Seeded, name: &str) {
        let response = app
            .request(
                Method::POST,
                &format!("/api/v1/projects/{}/score-definitions", seeded.project.id),
            )
            .header(header::COOKIE.as_str(), &seeded.cookie)
            .json(&json!({
                "name": name,
                "data_type": "categorical",
                "categories": [{"label": "good"}]
            }))
            .send()
            .await;
        assert_eq!(
            response.status,
            201,
            "{}",
            String::from_utf8_lossy(&response.body)
        );
    }

    /// Runs a job until it leaves `queued`, making each retry due in turn. Returns the last row.
    async fn run_until_done(app: &TestApp, project: &ProjectRow, job: Uuid, conn: Uuid) -> JobRow {
        let mut row = run_once(app, project, job, Some(conn)).await;
        while row.status == "queued" {
            due(app, project, job).await;
            row = run_once(app, project, job, Some(conn)).await;
        }
        row
    }

    /// The scores of `evaluator`, read through `GET /scores`.
    async fn scores_of(app: &TestApp, seeded: &Seeded, evaluator: Uuid) -> Vec<Value> {
        let response = app
            .request(
                Method::GET,
                &format!(
                    "/api/v1/projects/{}/scores?evaluator_id={evaluator}",
                    seeded.project.id
                ),
            )
            .header(header::COOKIE.as_str(), &seeded.cookie)
            .send()
            .await;
        assert_eq!(response.status, 200);
        let page: Value = response.json();
        page["data"].as_array().expect("data").clone()
    }

    /// Whether the judge trace `id` is stored (its trace page answers).
    async fn judge_trace_exists(app: &TestApp, seeded: &Seeded, id: TraceId) -> bool {
        let response = app
            .request(
                Method::GET,
                &format!(
                    "/api/v1/projects/{}/traces/{}",
                    seeded.project.id,
                    id.to_hex()
                ),
            )
            .header(header::COOKIE.as_str(), &seeded.cookie)
            .send()
            .await;
        response.status == 200
    }

    /// The `usage_daily` sums of the project, after a flush: judge input and output tokens, the
    /// judge cost, and the scores.
    async fn usage_sums(app: &TestApp, seeded: &Seeded) -> (i64, i64, f64, i64) {
        app.state
            .usage
            .flush(&app.state.meta)
            .await
            .expect("usage flushes");
        sqlx::query_as(
            "SELECT COALESCE(SUM(judge_input_tokens), 0), COALESCE(SUM(judge_output_tokens), 0), \
             COALESCE(SUM(judge_cost_usd), 0), COALESCE(SUM(scores), 0) \
             FROM usage_daily WHERE project_id = ?",
        )
        .bind(seeded.project.id.to_string())
        .fetch_one(app.state.meta.reader())
        .await
        .expect("usage reads")
    }

    #[tokio::test]
    async fn a_rule_job_writes_the_score_and_the_judge_trace() {
        let mock = MockLlm::start(vec![completion(JUDGE_ANSWER)]).await;
        let (app, seeded) = test_app().await;
        let conn = connection(&app, &seeded.cookie, &mock).await;
        let evaluator = judge(&app, &seeded, conn, "gpt-test").await;
        let rule_id = rule(&app, &seeded, evaluator, None, MAPPING).await;
        store_trace(&app, &seeded.project, trace(1), "production").await;
        let (job, new) = new_job(
            seeded.project.id,
            KIND_RULE,
            Some(rule_id),
            evaluator,
            None,
            trace(1),
            app.state.clock.now_ns(),
        );
        enqueue(&app, new).await;

        let row = run_once(&app, &seeded.project, job, Some(conn)).await;
        assert_eq!(row.status, "done", "{:?}", row.error);
        assert_eq!(row.judge_trace_id, Some(job_judge_trace_id(job, 1)));
        let score_id = row.score_id.expect("a score");
        assert_eq!(row.attempts, 1);

        let response = app
            .request(
                Method::GET,
                &format!(
                    "/api/v1/projects/{}/scores?rule_id={rule_id}",
                    seeded.project.id
                ),
            )
            .header(header::COOKIE.as_str(), &seeded.cookie)
            .send()
            .await;
        assert_eq!(response.status, 200);
        let page: Value = response.json();
        let scores = page["data"].as_array().expect("data");
        assert_eq!(scores.len(), 1, "{page}");
        assert_eq!(scores[0]["score_id"], json!(score_id));
        assert_eq!(scores[0]["source"], json!("llm_judge"));
        assert_eq!(scores[0]["session_id"], json!("session-7"));
        assert_eq!(scores[0]["user_id"], json!("user-7"));
        assert_eq!(
            scores[0]["metadata"]["cetra.eval.job_id"],
            json!(job.to_string())
        );
    }

    #[tokio::test]
    async fn a_manual_job_has_no_rule_id_and_no_budget() {
        let mock = MockLlm::start(vec![completion(JUDGE_ANSWER)]).await;
        let (app, seeded) = test_app().await;
        let conn = connection(&app, &seeded.cookie, &mock).await;
        let evaluator = judge(&app, &seeded, conn, "gpt-test").await;
        store_trace(&app, &seeded.project, trace(2), "production").await;
        let (job, new) = new_job(
            seeded.project.id,
            KIND_MANUAL,
            None,
            evaluator,
            Some(MAPPING),
            trace(2),
            app.state.clock.now_ns(),
        );
        enqueue(&app, new).await;

        let row = run_once(&app, &seeded.project, job, Some(conn)).await;
        assert_eq!(row.status, "done", "{:?}", row.error);
        assert_eq!(row.rule_id, None);
        assert_eq!(row.judge_trace_id, Some(job_judge_trace_id(job, 1)));
        assert!(app.state.eval.budgets.view(job, 0).is_none());
    }

    #[tokio::test]
    async fn a_retryable_error_backs_off_then_fails_after_six_attempts() {
        let replies = (0..6).map(|_| reply_status(503, "overloaded")).collect();
        let mock = MockLlm::start(replies).await;
        let (app, seeded) = test_app().await;
        let conn = connection(&app, &seeded.cookie, &mock).await;
        let evaluator = judge(&app, &seeded, conn, "gpt-test").await;
        store_trace(&app, &seeded.project, trace(3), "production").await;
        let (job, new) = new_job(
            seeded.project.id,
            KIND_MANUAL,
            None,
            evaluator,
            Some(MAPPING),
            trace(3),
            app.state.clock.now_ns(),
        );
        enqueue(&app, new).await;

        for attempt in 1..=5_u32 {
            let row = run_once(&app, &seeded.project, job, Some(conn)).await;
            assert_eq!(row.status, "queued", "attempt {attempt}");
            assert_eq!(row.attempts, attempt);
            assert_eq!(row.error_class.as_deref(), Some("retryable"));
            let now = app.state.clock.now_ns();
            assert_eq!(
                row.not_before,
                now + retry::backoff_ns(job, attempt),
                "the backoff of attempt {attempt}"
            );
            due(&app, &seeded.project, job).await;
        }
        let row = run_once(&app, &seeded.project, job, Some(conn)).await;
        assert_eq!(row.status, "failed");
        assert_eq!(row.attempts, 6);
        assert_eq!(row.error_class.as_deref(), Some("retryable"));
        assert_eq!(row.error.as_deref(), Some("provider status 503"));
    }

    #[tokio::test]
    async fn a_pause_error_pauses_the_connection_rules_and_requeues() {
        let mock = MockLlm::start(vec![reply_status(401, "bad key")]).await;
        let (app, seeded) = test_app().await;
        let conn = connection(&app, &seeded.cookie, &mock).await;
        let evaluator = judge(&app, &seeded, conn, "gpt-test").await;
        let rule_id = rule(&app, &seeded, evaluator, None, MAPPING).await;
        store_trace(&app, &seeded.project, trace(4), "production").await;
        let (job, new) = new_job(
            seeded.project.id,
            KIND_RULE,
            Some(rule_id),
            evaluator,
            None,
            trace(4),
            app.state.clock.now_ns(),
        );
        enqueue(&app, new).await;

        let row = run_once(&app, &seeded.project, job, Some(conn)).await;
        assert_eq!(row.status, "queued");
        assert_eq!(row.attempts, 0, "the attempt is given back");
        let rule = rule_row(&app, &seeded.project, rule_id).await;
        assert_eq!(rule.paused_reason.as_deref(), Some("connection"));
        let connection = connection_row(&app, &seeded.project, conn).await;
        assert_eq!(connection.status, "failing");
        assert_eq!(
            connection.last_error.as_deref(),
            Some("provider status 401")
        );
    }

    #[tokio::test]
    async fn a_manual_pause_fails_and_marks_the_connection() {
        let mock = MockLlm::start(vec![reply_status(401, "bad key")]).await;
        let (app, seeded) = test_app().await;
        let conn = connection(&app, &seeded.cookie, &mock).await;
        let evaluator = judge(&app, &seeded, conn, "gpt-test").await;
        let rule_id = rule(&app, &seeded, evaluator, None, MAPPING).await;
        store_trace(&app, &seeded.project, trace(5), "production").await;
        let (job, new) = new_job(
            seeded.project.id,
            KIND_MANUAL,
            None,
            evaluator,
            Some(MAPPING),
            trace(5),
            app.state.clock.now_ns(),
        );
        enqueue(&app, new).await;

        let row = run_once(&app, &seeded.project, job, Some(conn)).await;
        assert_eq!(row.status, "failed");
        assert_eq!(row.error_class.as_deref(), Some("pause"));
        assert_eq!(
            rule_row(&app, &seeded.project, rule_id).await.paused_reason,
            None,
            "no rule pauses for a manual run"
        );
        assert_eq!(
            connection_row(&app, &seeded.project, conn).await.status,
            "failing"
        );
    }

    #[tokio::test]
    async fn a_key_change_during_the_call_requeues_without_pausing() {
        let mock = MockLlm::start(vec![]).await;
        let (app, seeded) = test_app().await;
        let conn = connection(&app, &seeded.cookie, &mock).await;
        let evaluator = judge(&app, &seeded, conn, "gpt-test").await;
        let rule_id = rule(&app, &seeded, evaluator, None, MAPPING).await;
        let (job, new) = new_job(
            seeded.project.id,
            KIND_RULE,
            Some(rule_id),
            evaluator,
            None,
            trace(6),
            app.state.clock.now_ns(),
        );
        enqueue(&app, new).await;
        let mut claimed = claimed(&app, &seeded.project, job, Some(conn)).await;
        // The key changed after the claim: the stored `updated_at` is newer.
        claimed.connection_updated_at = Some(claimed.connection_updated_at.expect("judge") - 1);
        let before = connection_row(&app, &seeded.project, conn).await;
        let ended = Ended {
            finish: Finish::PauseRequeue {
                not_before: app.state.clock.now_ns(),
            },
            reached_evaluator: true,
            pause: Pause::Connection {
                org: seeded.org_id,
                connection_id: conn,
                error_text: "provider status 401".to_owned(),
            },
            actual: 0.0,
        };
        finish_job(&app.state, &claimed, ended, None, claimed.row.attempts)
            .await
            .expect("finish");

        let row = job_row(&app, &seeded.project, job).await;
        assert_eq!(row.status, "queued");
        assert_eq!(row.attempts, 0);
        assert_eq!(
            rule_row(&app, &seeded.project, rule_id).await.paused_reason,
            None
        );
        let after = connection_row(&app, &seeded.project, conn).await;
        assert_eq!(after.status, before.status);
        assert_eq!(after.last_error, before.last_error);
    }

    #[tokio::test]
    async fn an_unknown_adapter_fails() {
        let mock = MockLlm::start(vec![]).await;
        let (app, seeded) = test_app().await;
        let conn = connection(&app, &seeded.cookie, &mock).await;
        let evaluator = judge(&app, &seeded, conn, "gpt-test").await;
        store_trace(&app, &seeded.project, trace(7), "production").await;
        app.state
            .meta
            .write(|conn| {
                Box::pin(async move {
                    sqlx::query("UPDATE llm_connections SET adapter = 'nope'")
                        .execute(&mut *conn)
                        .await
                        .map(|_| ())
                        .map_err(MetaError::from)
                })
            })
            .await
            .expect("update");
        let (job, new) = new_job(
            seeded.project.id,
            KIND_MANUAL,
            None,
            evaluator,
            Some(MAPPING),
            trace(7),
            app.state.clock.now_ns(),
        );
        enqueue(&app, new).await;

        let row = run_once(&app, &seeded.project, job, Some(conn)).await;
        assert_eq!(row.status, "failed");
        assert_eq!(row.error_class.as_deref(), Some("permanent"));
        assert_eq!(row.error.as_deref(), Some("unknown adapter"));
        assert!(
            mock.requests().is_empty(),
            "no request reached the provider"
        );
    }

    #[tokio::test]
    async fn a_changed_connection_before_the_call_requeues() {
        let mock = MockLlm::start(vec![]).await;
        let (app, seeded) = test_app().await;
        let conn = connection(&app, &seeded.cookie, &mock).await;
        let evaluator = judge(&app, &seeded, conn, "gpt-test").await;
        store_trace(&app, &seeded.project, trace(8), "production").await;
        let (job, new) = new_job(
            seeded.project.id,
            KIND_MANUAL,
            None,
            evaluator,
            Some(MAPPING),
            trace(8),
            app.state.clock.now_ns(),
        );
        enqueue(&app, new).await;
        let mut claimed = claimed(&app, &seeded.project, job, Some(conn)).await;
        claimed.connection_updated_at = Some(claimed.connection_updated_at.expect("judge") - 1);
        run_job(&app.state, claimed).await.expect("run");

        let row = job_row(&app, &seeded.project, job).await;
        assert_eq!(row.status, "queued");
        assert_eq!(row.attempts, 0);
        assert!(
            mock.requests().is_empty(),
            "no request reached the provider"
        );
    }

    #[tokio::test]
    async fn a_permanent_error_fails_at_once() {
        let mock = MockLlm::start(vec![completion("not json")]).await;
        let (app, seeded) = test_app().await;
        let conn = connection(&app, &seeded.cookie, &mock).await;
        let evaluator = judge(&app, &seeded, conn, "gpt-test").await;
        store_trace(&app, &seeded.project, trace(9), "production").await;
        let (job, new) = new_job(
            seeded.project.id,
            KIND_MANUAL,
            None,
            evaluator,
            Some(MAPPING),
            trace(9),
            app.state.clock.now_ns(),
        );
        enqueue(&app, new).await;

        let row = run_once(&app, &seeded.project, job, Some(conn)).await;
        assert_eq!(row.status, "failed");
        assert_eq!(row.error_class.as_deref(), Some("permanent"));
        assert_eq!(row.attempts, 1);
    }

    #[tokio::test]
    async fn a_missing_trace_retries_three_times_then_fails() {
        let mock = MockLlm::start(vec![]).await;
        let (app, seeded) = test_app().await;
        let conn = connection(&app, &seeded.cookie, &mock).await;
        let evaluator = judge(&app, &seeded, conn, "gpt-test").await;
        let (job, new) = new_job(
            seeded.project.id,
            KIND_MANUAL,
            None,
            evaluator,
            Some(MAPPING),
            trace(10),
            app.state.clock.now_ns(),
        );
        enqueue(&app, new).await;

        for load_retries in 1..=3_u32 {
            let row = run_once(&app, &seeded.project, job, Some(conn)).await;
            assert_eq!(row.status, "queued");
            assert_eq!(row.load_retries, load_retries);
            due(&app, &seeded.project, job).await;
        }
        let row = run_once(&app, &seeded.project, job, Some(conn)).await;
        assert_eq!(row.status, "failed");
        assert_eq!(row.error.as_deref(), Some("trace not found"));
        assert_eq!(
            row.attempts, 0,
            "a failed load is not an attempt: it fails before the count"
        );
        assert!(mock.requests().is_empty());
    }

    #[tokio::test]
    async fn an_evaluation_trace_is_refused() {
        let mock = MockLlm::start(vec![]).await;
        let (app, seeded) = test_app().await;
        let conn = connection(&app, &seeded.cookie, &mock).await;
        let evaluator = judge(&app, &seeded, conn, "gpt-test").await;
        store_trace(&app, &seeded.project, trace(11), "cetra-eval").await;
        let (job, new) = new_job(
            seeded.project.id,
            KIND_MANUAL,
            None,
            evaluator,
            Some(MAPPING),
            trace(11),
            app.state.clock.now_ns(),
        );
        enqueue(&app, new).await;

        let row = run_once(&app, &seeded.project, job, Some(conn)).await;
        assert_eq!(row.status, "failed");
        assert_eq!(row.error.as_deref(), Some("target is an evaluation trace"));
        assert_eq!(row.judge_trace_id, None);
        assert!(mock.requests().is_empty());
    }

    #[tokio::test]
    async fn a_mapping_error_writes_no_judge_trace() {
        let mock = MockLlm::start(vec![]).await;
        let (app, seeded) = test_app().await;
        let conn = connection(&app, &seeded.cookie, &mock).await;
        let evaluator = judge(&app, &seeded, conn, "gpt-test").await;
        store_trace(&app, &seeded.project, trace(12), "production").await;
        let mapping = r#"{"input": "trace.input", "output": "span[name=nope].output"}"#;
        let (job, new) = new_job(
            seeded.project.id,
            KIND_MANUAL,
            None,
            evaluator,
            Some(mapping),
            trace(12),
            app.state.clock.now_ns(),
        );
        enqueue(&app, new).await;

        let row = run_once(&app, &seeded.project, job, Some(conn)).await;
        assert_eq!(row.status, "failed");
        assert_eq!(row.error_class.as_deref(), Some("permanent"));
        assert_eq!(row.judge_trace_id, None);
        assert!(mock.requests().is_empty());
    }

    #[tokio::test]
    async fn a_model_without_a_price_fails_and_pauses_the_rule() {
        let mock = MockLlm::start(vec![]).await;
        let (app, seeded) = test_app().await;
        let conn = connection(&app, &seeded.cookie, &mock).await;
        let evaluator = judge(&app, &seeded, conn, "gpt-test").await;
        let rule_id = rule(&app, &seeded, evaluator, Some((5.0, "day")), MAPPING).await;
        store_trace(&app, &seeded.project, trace(13), "production").await;
        let (job, new) = new_job(
            seeded.project.id,
            KIND_RULE,
            Some(rule_id),
            evaluator,
            None,
            trace(13),
            app.state.clock.now_ns(),
        );
        enqueue(&app, new).await;

        let row = run_once(&app, &seeded.project, job, Some(conn)).await;
        assert_eq!(row.status, "failed");
        assert_eq!(row.error.as_deref(), Some("model has no price"));
        let rule = rule_row(&app, &seeded.project, rule_id).await;
        assert_eq!(rule.paused_reason.as_deref(), Some("errors"));
        assert_eq!(rule.paused_detail.as_deref(), Some("model has no price"));
        assert!(mock.requests().is_empty());
    }

    #[tokio::test]
    async fn budget_over_pauses_and_skips() {
        let mock = MockLlm::start(vec![]).await;
        let (app, seeded) = test_app().await;
        let conn = connection(&app, &seeded.cookie, &mock).await;
        let evaluator = judge(&app, &seeded, conn, PRICED).await;
        let rule_id = rule(&app, &seeded, evaluator, Some((0.000_001, "day")), MAPPING).await;
        store_trace(&app, &seeded.project, trace(14), "production").await;
        let now = app.state.clock.now_ns();
        let (job, new) = new_job(
            seeded.project.id,
            KIND_RULE,
            Some(rule_id),
            evaluator,
            None,
            trace(14),
            now,
        );
        let (queued, queued_new) = new_job(
            seeded.project.id,
            KIND_RULE,
            Some(rule_id),
            evaluator,
            None,
            trace(15),
            now,
        );
        enqueue(&app, new).await;
        enqueue(&app, queued_new).await;

        let row = run_once(&app, &seeded.project, job, Some(conn)).await;
        assert_eq!(row.status, "skipped");
        assert_eq!(row.error.as_deref(), Some("budget"));
        assert_eq!(
            job_row(&app, &seeded.project, queued).await.status,
            "skipped"
        );
        let rule = rule_row(&app, &seeded.project, rule_id).await;
        assert_eq!(rule.paused_reason.as_deref(), Some("budget"));
        assert!(mock.requests().is_empty());
    }

    #[tokio::test]
    async fn a_budgeted_run_settles_its_reservation_into_the_rule_spend() {
        let mock = MockLlm::start(vec![completion_for(PRICED, JUDGE_ANSWER)]).await;
        let (app, seeded) = test_app().await;
        let conn = connection(&app, &seeded.cookie, &mock).await;
        let evaluator = judge(&app, &seeded, conn, PRICED).await;
        let rule_id = rule(&app, &seeded, evaluator, Some((5.0, "day")), MAPPING).await;
        store_trace(&app, &seeded.project, trace(16), "production").await;
        let (job, new) = new_job(
            seeded.project.id,
            KIND_RULE,
            Some(rule_id),
            evaluator,
            None,
            trace(16),
            app.state.clock.now_ns(),
        );
        enqueue(&app, new).await;

        let row = run_once(&app, &seeded.project, job, Some(conn)).await;
        assert_eq!(row.status, "done", "{:?}", row.error);
        let now = app.state.clock.now_ns();
        let spent = rule_spend::spent_between(
            app.state.meta.reader(),
            rule_id,
            budget::day_start(now),
            budget::day_start(now) + budget::DAY_NS,
        )
        .await
        .expect("read");
        assert!(spent > 0.0, "the priced model's cost is spent");
        // The judge's `llm` span cost is what `usage_daily` counts for the same call.
        let (_, _, judge_cost, _) = usage_sums(&app, &seeded).await;
        assert_eq!(spent, judge_cost, "the spend is the call's cost");
        let (in_memory, reserved) = app
            .state
            .eval
            .budgets
            .view(rule_id, budget::day_start(now))
            .expect("loaded");
        assert_eq!(in_memory, spent, "the in-memory budget equals the table");
        assert_eq!(reserved, 0.0, "the reservation is settled");
    }

    #[tokio::test]
    async fn a_stale_claim_drops_the_result_but_keeps_the_judge_trace() {
        let mock = MockLlm::start(vec![completion(JUDGE_ANSWER)]).await;
        let (app, seeded) = test_app().await;
        let conn = connection(&app, &seeded.cookie, &mock).await;
        let evaluator = judge(&app, &seeded, conn, "gpt-test").await;
        store_trace(&app, &seeded.project, trace(17), "production").await;
        let (job, new) = new_job(
            seeded.project.id,
            KIND_MANUAL,
            None,
            evaluator,
            Some(MAPPING),
            trace(17),
            app.state.clock.now_ns(),
        );
        enqueue(&app, new).await;
        let first = claimed(&app, &seeded.project, job, Some(conn)).await;
        let prepared = prepare(&app.state, &first).await.ok().expect("prepared");
        let Ready { reservation, call } = prepared;
        // The janitor re-queues the running job and the dispatcher claims it again.
        let now = app.state.clock.now_ns();
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    eval_jobs::requeue_stale(conn, i64::MAX, now)
                        .await
                        .map(|_| ())
                })
            })
            .await
            .expect("requeue");
        let second = claimed(&app, &seeded.project, job, Some(conn)).await;
        assert_eq!(second.claims, 2);

        let ended = call_evaluator(&app.state, &first, call).await;
        finish_job(&app.state, &first, ended, reservation, 1)
            .await
            .expect("finish");

        let row = job_row(&app, &seeded.project, job).await;
        assert_eq!(row.status, "running", "the stale finish changed nothing");
        assert_eq!(row.claims, 2);
        assert_eq!(mock.requests().len(), 1);
        // D-I-12 keeps the result's writes: the score and the judge trace of the first claim.
        let scores = scores_of(&app, &seeded, evaluator).await;
        assert_eq!(scores.len(), 1, "the score is written");
        assert_eq!(
            scores[0]["metadata"]["cetra.eval.job_id"],
            json!(job.to_string())
        );
        assert!(
            judge_trace_exists(&app, &seeded, job_judge_trace_id(job, 1)).await,
            "the judge trace of claim 1 exists"
        );
    }

    #[tokio::test]
    async fn the_breaker_pauses_after_twenty_failures() {
        let replies = (0..20).map(|_| completion("not json")).collect();
        let mock = MockLlm::start(replies).await;
        let (app, seeded) = test_app().await;
        let conn = connection(&app, &seeded.cookie, &mock).await;
        let evaluator = judge(&app, &seeded, conn, "gpt-test").await;
        let rule_id = rule(&app, &seeded, evaluator, None, MAPPING).await;
        let mut jobs = Vec::new();
        for n in 0..20_u128 {
            store_trace(&app, &seeded.project, trace(100 + n), "production").await;
            let (job, new) = new_job(
                seeded.project.id,
                KIND_RULE,
                Some(rule_id),
                evaluator,
                None,
                trace(100 + n),
                app.state.clock.now_ns(),
            );
            enqueue(&app, new).await;
            jobs.push(job);
        }
        for job in &jobs[..19] {
            run_once(&app, &seeded.project, *job, Some(conn)).await;
        }
        assert_eq!(
            rule_row(&app, &seeded.project, rule_id).await.paused_reason,
            None,
            "nineteen failures do not trip"
        );
        run_once(&app, &seeded.project, jobs[19], Some(conn)).await;
        let rule = rule_row(&app, &seeded.project, rule_id).await;
        assert_eq!(rule.paused_reason.as_deref(), Some("errors"));
        assert_eq!(
            rule.paused_detail.as_deref(),
            Some("half or more of the last 50 jobs failed")
        );
    }

    #[tokio::test]
    async fn a_paused_rule_requeues_its_claimed_job() {
        let mock = MockLlm::start(vec![]).await;
        let (app, seeded) = test_app().await;
        let conn = connection(&app, &seeded.cookie, &mock).await;
        let evaluator = judge(&app, &seeded, conn, "gpt-test").await;
        let rule_id = rule(&app, &seeded, evaluator, None, MAPPING).await;
        store_trace(&app, &seeded.project, trace(18), "production").await;
        let (job, new) = new_job(
            seeded.project.id,
            KIND_RULE,
            Some(rule_id),
            evaluator,
            None,
            trace(18),
            app.state.clock.now_ns(),
        );
        enqueue(&app, new).await;
        set_paused(&app, rule_id).await;

        let row = run_once(&app, &seeded.project, job, Some(conn)).await;
        assert_eq!(row.status, "queued");
        assert_eq!(row.attempts, 0);
        assert!(mock.requests().is_empty());
    }

    #[tokio::test]
    async fn a_deleted_evaluator_ends_quietly() {
        let mock = MockLlm::start(vec![]).await;
        let (app, seeded) = test_app().await;
        let conn = connection(&app, &seeded.cookie, &mock).await;
        let evaluator = judge(&app, &seeded, conn, "gpt-test").await;
        let (job, new) = new_job(
            seeded.project.id,
            KIND_MANUAL,
            None,
            evaluator,
            Some(MAPPING),
            trace(19),
            app.state.clock.now_ns(),
        );
        enqueue(&app, new).await;
        let claimed = claimed(&app, &seeded.project, job, Some(conn)).await;
        let project = seeded.project.id;
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    evaluators::delete_evaluator(conn, project, evaluator)
                        .await
                        .map(|_| ())
                })
            })
            .await
            .expect("delete");

        run_job(&app.state, claimed).await.expect("ends quietly");
        assert!(job_row_opt(&app, &seeded.project, job).await.is_none());
        assert!(mock.requests().is_empty());
    }

    async fn job_row_opt(app: &TestApp, project: &ProjectRow, job: Uuid) -> Option<JobRow> {
        eval_jobs::get_job(app.state.meta.reader(), project.id, job)
            .await
            .expect("read")
    }

    #[tokio::test]
    async fn a_rule_deleted_during_the_run_ends_quietly() {
        let mock = MockLlm::start(vec![completion(JUDGE_ANSWER)]).await;
        let (app, seeded) = test_app().await;
        let conn = connection(&app, &seeded.cookie, &mock).await;
        let evaluator = judge(&app, &seeded, conn, PRICED).await;
        let rule_id = rule(&app, &seeded, evaluator, Some((5.0, "day")), MAPPING).await;
        store_trace(&app, &seeded.project, trace(20), "production").await;
        let (job, new) = new_job(
            seeded.project.id,
            KIND_RULE,
            Some(rule_id),
            evaluator,
            None,
            trace(20),
            app.state.clock.now_ns(),
        );
        enqueue(&app, new).await;
        let first = claimed(&app, &seeded.project, job, Some(conn)).await;
        let Ready { reservation, call } = prepare(&app.state, &first).await.ok().expect("prepared");
        let project = seeded.project.id;
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    online_rules::delete_rule(conn, project, rule_id)
                        .await
                        .map(|_| ())
                })
            })
            .await
            .expect("delete rule");

        let ended = call_evaluator(&app.state, &first, call).await;
        finish_job(&app.state, &first, ended, reservation, 1)
            .await
            .expect("ends quietly");
        assert!(job_row_opt(&app, &seeded.project, job).await.is_none());
        let now = app.state.clock.now_ns();
        let spent = rule_spend::spent_between(
            app.state.meta.reader(),
            rule_id,
            budget::day_start(now),
            budget::day_start(now) + budget::DAY_NS,
        )
        .await
        .expect("read");
        assert_eq!(spent, 0.0, "no spend row for a deleted rule");
    }

    #[tokio::test]
    async fn a_check_job_runs_to_done() {
        let mock = MockLlm::start(vec![]).await;
        let (app, seeded) = test_app().await;
        let evaluator = check(&app, &seeded).await;
        store_trace(&app, &seeded.project, trace(21), "production").await;
        let mapping = r#"{"output": "trace.output", "expected": "trace.output"}"#;
        let (job, new) = new_job(
            seeded.project.id,
            KIND_MANUAL,
            None,
            evaluator,
            Some(mapping),
            trace(21),
            app.state.clock.now_ns(),
        );
        enqueue(&app, new).await;

        let row = run_once(&app, &seeded.project, job, None).await;
        assert_eq!(row.status, "done", "{:?}", row.error);
        assert!(row.score_id.is_some());
        assert_eq!(row.judge_trace_id, Some(job_judge_trace_id(job, 1)));
        assert!(mock.requests().is_empty());
    }

    /// Puts the org at `cap` units this month, so the next units check refuses.
    fn at_units_cap(app: &TestApp, seeded: &Seeded, cap: u64) {
        set_limits(
            app,
            seeded.org_id,
            "2025-12-01T00:00:00Z",
            &json!({"units_cap": cap}),
        );
        app.state.usage.add(
            seeded.org_id,
            seeded.project.id,
            app.state.clock.now_ns(),
            cap,
            0,
            0,
        );
    }

    #[tokio::test]
    async fn a_judge_at_the_units_cap_is_skipped_before_the_call() {
        let mock = MockLlm::start(vec![completion(JUDGE_ANSWER)]).await;
        let (app, seeded) = test_app().await;
        let conn = connection(&app, &seeded.cookie, &mock).await;
        let evaluator = judge(&app, &seeded, conn, "gpt-test").await;
        store_trace(&app, &seeded.project, trace(60), "production").await;
        at_units_cap(&app, &seeded, 10);
        let (job, new) = new_job(
            seeded.project.id,
            KIND_MANUAL,
            None,
            evaluator,
            Some(MAPPING),
            trace(60),
            app.state.clock.now_ns(),
        );
        enqueue(&app, new).await;

        let row = run_once(&app, &seeded.project, job, Some(conn)).await;
        assert_eq!(row.status, "skipped");
        assert_eq!(row.error.as_deref(), Some("quota"));
        assert_eq!(row.attempts, 0, "no attempt was started");
        assert!(
            mock.requests().is_empty(),
            "no request reaches the provider"
        );
    }

    #[tokio::test]
    async fn a_score_refused_at_the_units_cap_fails_without_a_retry() {
        let mock = MockLlm::start(vec![]).await;
        let (app, seeded) = test_app().await;
        let evaluator = check(&app, &seeded).await;
        store_trace(&app, &seeded.project, trace(61), "production").await;
        // A check is not held back before its run (only judges and webhooks are), so the
        // refusal comes from the score write (D-I-35).
        at_units_cap(&app, &seeded, 10);
        let mapping = r#"{"output": "trace.output", "expected": "trace.output"}"#;
        let (job, new) = new_job(
            seeded.project.id,
            KIND_MANUAL,
            None,
            evaluator,
            Some(mapping),
            trace(61),
            app.state.clock.now_ns(),
        );
        enqueue(&app, new).await;

        let row = run_once(&app, &seeded.project, job, None).await;
        assert_eq!(row.status, "failed", "a refusal is final, not queued again");
        assert_eq!(row.error_class.as_deref(), Some("permanent"));
        assert_eq!(row.error.as_deref(), Some("monthly units cap reached"));
        assert_eq!(row.score_id, None);
        assert_eq!(row.attempts, 1);
        assert!(mock.requests().is_empty());
    }

    /// A rule job of `judge` with `model` on the connection, and a bystander rule of a check. The
    /// judge's provider answers 404 for `model`. Returns the job, the judge's rule and the
    /// bystander rule.
    async fn a_404_for_the_judge_model(
        app: &TestApp,
        seeded: &Seeded,
        conn: Uuid,
        model: &str,
    ) -> (Uuid, Uuid, Uuid) {
        let evaluator = judge(app, seeded, conn, model).await;
        let rule_id = rule(app, seeded, evaluator, None, MAPPING).await;
        let bystander_check = check(app, seeded).await;
        let bystander = rule(
            app,
            seeded,
            bystander_check,
            None,
            r#"{"output": "trace.output", "expected": "trace.output"}"#,
        )
        .await;
        store_trace(app, &seeded.project, trace(62), "production").await;
        let (job, new) = new_job(
            seeded.project.id,
            KIND_RULE,
            Some(rule_id),
            evaluator,
            None,
            trace(62),
            app.state.clock.now_ns(),
        );
        enqueue(app, new).await;
        (job, rule_id, bystander)
    }

    #[tokio::test]
    async fn a_404_for_a_model_other_than_the_default_pauses_only_its_rules() {
        let mock = MockLlm::start(vec![reply_status(404, "model not found")]).await;
        let (app, seeded) = test_app().await;
        let conn = connection(&app, &seeded.cookie, &mock).await;
        let (job, rule_id, bystander) =
            a_404_for_the_judge_model(&app, &seeded, conn, "gpt-other").await;

        let row = run_once(&app, &seeded.project, job, Some(conn)).await;
        assert_eq!(row.status, "queued");
        assert_eq!(row.attempts, 0, "the attempt is given back");
        let paused = rule_row(&app, &seeded.project, rule_id).await;
        assert_eq!(paused.paused_reason.as_deref(), Some("errors"));
        assert_eq!(paused.paused_detail.as_deref(), Some("provider status 404"));
        assert_eq!(
            rule_row(&app, &seeded.project, bystander)
                .await
                .paused_reason,
            None,
            "other rules keep running"
        );
        let connection = connection_row(&app, &seeded.project, conn).await;
        assert_eq!(connection.status, "ok");
        assert_eq!(connection.last_error, None);
    }

    #[tokio::test]
    async fn a_404_for_the_default_model_takes_the_connection_path() {
        let mock = MockLlm::start(vec![reply_status(404, "model not found")]).await;
        let (app, seeded) = test_app().await;
        let conn = connection(&app, &seeded.cookie, &mock).await;
        let (job, rule_id, bystander) =
            a_404_for_the_judge_model(&app, &seeded, conn, "gpt-test").await;

        let row = run_once(&app, &seeded.project, job, Some(conn)).await;
        assert_eq!(row.status, "queued");
        assert_eq!(
            rule_row(&app, &seeded.project, rule_id)
                .await
                .paused_reason
                .as_deref(),
            Some("connection")
        );
        assert_eq!(
            connection_row(&app, &seeded.project, conn).await.status,
            "failing"
        );
        assert_eq!(
            rule_row(&app, &seeded.project, bystander)
                .await
                .paused_reason,
            None,
            "a check does not use the connection, so it keeps running"
        );
    }

    #[test]
    fn a_truncated_trace_marks_the_score() {
        let job = Uuid::from_u128(0x0190_0000_0000_7000_8000_0000_0000_0800);
        let marked = score_metadata(BTreeMap::new(), job, 3, true);
        assert_eq!(marked.get(TRUNCATED_KEY).map(String::as_str), Some("true"));
        assert_eq!(
            marked.get(JOB_ID_KEY).map(String::as_str),
            Some(job.to_string().as_str())
        );
        assert_eq!(
            marked.get(EVALUATOR_VERSION_KEY).map(String::as_str),
            Some("3")
        );
        let whole = score_metadata(BTreeMap::new(), job, 3, false);
        assert!(!whole.contains_key(TRUNCATED_KEY));
    }

    #[tokio::test]
    async fn a_store_failure_does_not_count_for_the_breaker() {
        let mock = MockLlm::start(vec![]).await;
        let (app, seeded) = test_app().await;
        let conn = connection(&app, &seeded.cookie, &mock).await;
        let evaluator = judge(&app, &seeded, conn, "gpt-test").await;
        let rule_id = rule(&app, &seeded, evaluator, None, MAPPING).await;
        for n in 0..20_u128 {
            let (job, new) = new_job(
                seeded.project.id,
                KIND_RULE,
                Some(rule_id),
                evaluator,
                None,
                trace(200 + n),
                app.state.clock.now_ns(),
            );
            enqueue(&app, new).await;
            let claimed = claimed(&app, &seeded.project, job, Some(conn)).await;
            let ended = Ended {
                finish: Finish::Failed {
                    class: "permanent",
                    error: STORE_ERROR.to_owned(),
                    judge_trace_id: None,
                    evaluator_version: None,
                    usage: JobUsage::default(),
                },
                reached_evaluator: false,
                pause: Pause::None,
                actual: 0.0,
            };
            finish_job(&app.state, &claimed, ended, None, claimed.row.attempts)
                .await
                .expect("finish");
        }
        assert_eq!(
            rule_row(&app, &seeded.project, rule_id).await.paused_reason,
            None
        );
    }

    #[tokio::test]
    async fn retry_write_retries_until_the_write_succeeds() {
        let calls = Cell::new(0_u32);
        let result: Result<u32, &str> = retry_write(
            3,
            Duration::from_millis(1),
            |_| true,
            || {
                calls.set(calls.get() + 1);
                let call = calls.get();
                async move { if call < 3 { Err("busy") } else { Ok(call) } }
            },
        )
        .await;
        assert_eq!(result, Ok(3));
        assert_eq!(calls.get(), 3);
    }

    #[tokio::test]
    async fn retry_write_gives_up_after_its_tries() {
        let calls = Cell::new(0_u32);
        let result: Result<(), &str> = retry_write(
            3,
            Duration::from_millis(1),
            |_| true,
            || {
                calls.set(calls.get() + 1);
                async { Err("busy") }
            },
        )
        .await;
        assert_eq!(result, Err("busy"));
        assert_eq!(calls.get(), 3);
    }

    /// Re-queues the running job and claims it again, as the janitor and the dispatcher do: the
    /// claim count of the new claim is one more than the old one's.
    async fn reclaim(
        app: &TestApp,
        project: &ProjectRow,
        job: Uuid,
        judge_connection: Option<Uuid>,
    ) -> ClaimedJob {
        let now = app.state.clock.now_ns();
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    eval_jobs::requeue_stale(conn, i64::MAX, now)
                        .await
                        .map(|_| ())
                })
            })
            .await
            .expect("requeue");
        claimed(app, project, job, judge_connection).await
    }

    #[tokio::test]
    async fn the_retry_is_due_from_the_time_after_the_call() {
        let (app, seeded) = test_app().await;
        let before = app.state.clock.now_ns();
        // The judge answers 503 while the clock moves on: the call takes a second.
        let later = before + 1_000_000_000;
        let clock = app.state.clock.clone();
        let mock = MockLlm::start_with_hook(vec![reply_status(503, "overloaded")], move || {
            clock.set(later);
        })
        .await;
        let conn = connection(&app, &seeded.cookie, &mock).await;
        let evaluator = judge(&app, &seeded, conn, "gpt-test").await;
        store_trace(&app, &seeded.project, trace(30), "production").await;
        let (job, new) = new_job(
            seeded.project.id,
            KIND_MANUAL,
            None,
            evaluator,
            Some(MAPPING),
            trace(30),
            before,
        );
        enqueue(&app, new).await;

        let row = run_once(&app, &seeded.project, job, Some(conn)).await;
        assert_eq!(row.status, "queued");
        assert_eq!(row.attempts, 1);
        assert_eq!(
            row.not_before,
            later + retry::backoff_ns(job, 1),
            "the backoff counts from the time after the call, not from before it"
        );
    }

    #[tokio::test]
    async fn a_retried_attempt_does_not_count_for_the_breaker() {
        // Every attempt is a 503. A job is retried five times before it fails; only that final
        // failure is a breaker record, so nineteen failed jobs do not trip (they would at 20).
        let mock = MockLlm::start(vec![reply_status(503, "overloaded")]).await;
        let (app, seeded) = test_app().await;
        let conn = connection(&app, &seeded.cookie, &mock).await;
        let evaluator = judge(&app, &seeded, conn, "gpt-test").await;
        let rule_id = rule(&app, &seeded, evaluator, None, MAPPING).await;
        let mut jobs = Vec::new();
        for n in 0..20_u128 {
            store_trace(&app, &seeded.project, trace(300 + n), "production").await;
            let (job, new) = new_job(
                seeded.project.id,
                KIND_RULE,
                Some(rule_id),
                evaluator,
                None,
                trace(300 + n),
                app.state.clock.now_ns(),
            );
            enqueue(&app, new).await;
            jobs.push(job);
        }
        for job in &jobs[..19] {
            let row = run_until_done(&app, &seeded.project, *job, conn).await;
            assert_eq!(row.status, "failed");
            assert_eq!(row.attempts, 6);
        }
        assert_eq!(
            rule_row(&app, &seeded.project, rule_id).await.paused_reason,
            None,
            "nineteen final failures do not trip; the retries do not count"
        );
        let row = run_until_done(&app, &seeded.project, jobs[19], conn).await;
        assert_eq!(row.status, "failed");
        assert_eq!(
            rule_row(&app, &seeded.project, rule_id)
                .await
                .paused_reason
                .as_deref(),
            Some("errors"),
            "the twentieth final failure trips the breaker"
        );
    }

    #[tokio::test]
    async fn a_saturated_trace_load_requeues_without_spending_a_load_retry() {
        let mock = MockLlm::start(vec![]).await;
        let (app, seeded) =
            seeded_in(TestApp::with_permit_wait(Duration::from_millis(10)).await).await;
        let conn = connection(&app, &seeded.cookie, &mock).await;
        let evaluator = judge(&app, &seeded, conn, "gpt-test").await;
        store_trace(&app, &seeded.project, trace(31), "production").await;
        let (job, new) = new_job(
            seeded.project.id,
            KIND_MANUAL,
            None,
            evaluator,
            Some(MAPPING),
            trace(31),
            app.state.clock.now_ns(),
        );
        enqueue(&app, new).await;
        let _held = app
            .state
            .trace_loads
            .semaphore()
            .try_acquire_many(MAX_LOADS)
            .expect("permits");

        let now = app.state.clock.now_ns();
        let row = run_once(&app, &seeded.project, job, Some(conn)).await;
        assert_eq!(row.status, "queued");
        assert_eq!(row.not_before, now + retry::SATURATED_REQUEUE_NS);
        assert_eq!(row.load_retries, 0, "no load retry is spent");
        assert!(mock.requests().is_empty());
    }

    #[test]
    fn an_unpriced_estimate_ends_the_job_as_unpriced() {
        let rule = Uuid::from_u128(77);
        let Exit::Ended(ended) = unpriced(rule) else {
            panic!("an unpriced job ends before the call");
        };
        let Ended {
            finish,
            pause,
            actual,
            ..
        } = *ended;
        assert!(matches!(
            finish,
            Finish::Failed { class: "permanent", ref error, .. } if error == "model has no price"
        ));
        assert!(matches!(
            pause,
            Pause::Rule { rule_id, reason: PAUSE_ERRORS, .. } if rule_id == rule
        ));
        assert_eq!(actual, 0.0);
    }

    #[tokio::test]
    async fn a_webhook_job_writes_its_score() {
        let hook = MockWebhook::start(200, json!({"value": 0.5})).await;
        let (app, seeded) = test_app().await;
        let evaluator = webhook(&app, &seeded, &hook.url()).await;
        let rule_id = rule(&app, &seeded, evaluator, None, MAPPING).await;
        store_trace(&app, &seeded.project, trace(43), "production").await;
        let (job, new) = new_job(
            seeded.project.id,
            KIND_RULE,
            Some(rule_id),
            evaluator,
            None,
            trace(43),
            app.state.clock.now_ns(),
        );
        enqueue(&app, new).await;

        let row = run_once(&app, &seeded.project, job, None).await;
        assert_eq!(row.status, "done", "{:?}", row.error);
        assert!(row.score_id.is_some());
        assert_eq!(hook.requests().len(), 1);
    }

    #[tokio::test]
    async fn a_webhook_401_pauses_the_evaluator_rules_and_requeues() {
        let hook = MockWebhook::start(401, json!({"error": "no"})).await;
        let (app, seeded) = test_app().await;
        let mock = MockLlm::start(vec![]).await;
        let conn = connection(&app, &seeded.cookie, &mock).await;
        let evaluator = webhook(&app, &seeded, &hook.url()).await;
        let rule_id = rule(&app, &seeded, evaluator, None, MAPPING).await;
        store_trace(&app, &seeded.project, trace(44), "production").await;
        let (job, new) = new_job(
            seeded.project.id,
            KIND_RULE,
            Some(rule_id),
            evaluator,
            None,
            trace(44),
            app.state.clock.now_ns(),
        );
        enqueue(&app, new).await;
        let before = connection_row(&app, &seeded.project, conn).await;

        let row = run_once(&app, &seeded.project, job, None).await;
        assert_eq!(row.status, "queued");
        assert_eq!(row.attempts, 0, "the attempt is given back");
        assert_eq!(
            rule_row(&app, &seeded.project, rule_id)
                .await
                .paused_reason
                .as_deref(),
            Some("errors")
        );
        let after = connection_row(&app, &seeded.project, conn).await;
        assert_eq!(after.status, before.status);
        assert_eq!(after.last_error, before.last_error);
    }

    #[tokio::test]
    async fn a_definition_mismatch_fails_before_the_call() {
        let mock = MockLlm::start(vec![]).await;
        let (app, seeded) = test_app().await;
        let conn = connection(&app, &seeded.cookie, &mock).await;
        let evaluator = judge(&app, &seeded, conn, "gpt-test").await;
        // The judge's score is numeric; a live categorical definition of its name is a mismatch.
        categorical_definition(&app, &seeded, "relevance").await;
        store_trace(&app, &seeded.project, trace(45), "production").await;
        let (job, new) = new_job(
            seeded.project.id,
            KIND_MANUAL,
            None,
            evaluator,
            Some(MAPPING),
            trace(45),
            app.state.clock.now_ns(),
        );
        enqueue(&app, new).await;

        let row = run_once(&app, &seeded.project, job, Some(conn)).await;
        assert_eq!(row.status, "failed");
        assert_eq!(row.error.as_deref(), Some("score definition mismatch"));
        assert_eq!(
            row.attempts, 0,
            "the check runs before the attempt is counted"
        );
        assert!(mock.requests().is_empty());
    }

    #[tokio::test]
    async fn a_levenshtein_input_over_64_kib_fails_before_the_call() {
        let mock = MockLlm::start(vec![]).await;
        let (app, seeded) = test_app().await;
        let response = app
            .request(
                Method::POST,
                &format!("/api/v1/projects/{}/evaluators", seeded.project.id),
            )
            .header(header::COOKIE.as_str(), &seeded.cookie)
            .json(&json!({
                "name": "Similar",
                "kind": "check",
                "config": {"check": "levenshtein"},
                "output_score": {"name": "similar", "data_type": "numeric"}
            }))
            .send()
            .await;
        assert_eq!(response.status, 201);
        let value: Value = response.json();
        let evaluator = Uuid::parse_str(value["id"].as_str().expect("id")).expect("uuid");
        let long = "x".repeat(64 * 1024 + 1);
        store_span_with(&app, &seeded.project, trace(46), "production", &long).await;
        let mapping = r#"{"output": "trace.output", "expected": "trace.output"}"#;
        let (job, new) = new_job(
            seeded.project.id,
            KIND_MANUAL,
            None,
            evaluator,
            Some(mapping),
            trace(46),
            app.state.clock.now_ns(),
        );
        enqueue(&app, new).await;

        let row = run_once(&app, &seeded.project, job, None).await;
        assert_eq!(row.status, "failed");
        assert_eq!(
            row.error.as_deref(),
            Some("levenshtein inputs must be at most 64 KiB each")
        );
        assert_eq!(row.judge_trace_id, None, "no judge trace is written");
        assert!(mock.requests().is_empty());
    }

    #[tokio::test]
    async fn a_month_budget_sums_the_days_of_this_month() {
        let mock = MockLlm::start(vec![completion(JUDGE_ANSWER)]).await;
        let (app, seeded) = test_app().await;
        let conn = connection(&app, &seeded.cookie, &mock).await;
        let evaluator = judge(&app, &seeded, conn, PRICED).await;
        let this_month = budget::month_start(app.state.clock.now_ns());
        let mid_month = this_month + 14 * budget::DAY_NS + 12 * 3_600_000_000_000;
        app.state.clock.set(mid_month);
        let limit = 5.0;
        let current = rule(&app, &seeded, evaluator, Some((limit, "month")), MAPPING).await;
        let previous = rule(&app, &seeded, evaluator, Some((limit, "month")), MAPPING).await;
        let day = budget::DAY_NS;
        // This month's spend is just under the limit, so the estimate pushes it over. The other
        // rule's spend is the same amount, but dated in the month before: it does not count.
        let just_under = limit - 0.000_001;
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    rule_spend::add_spend(conn, current, this_month + 2 * day, 2.0).await?;
                    rule_spend::add_spend(conn, current, this_month + 4 * day, just_under - 2.0)
                        .await?;
                    rule_spend::add_spend(conn, previous, this_month - 2 * day, 2.0).await?;
                    rule_spend::add_spend(conn, previous, this_month - 3 * day, just_under - 2.0)
                        .await?;
                    Ok::<(), MetaError>(())
                })
            })
            .await
            .expect("spend");
        store_trace(&app, &seeded.project, trace(40), "production").await;
        store_trace(&app, &seeded.project, trace(41), "production").await;
        let (current_job, new) = new_job(
            seeded.project.id,
            KIND_RULE,
            Some(current),
            evaluator,
            None,
            trace(40),
            mid_month,
        );
        let (previous_job, previous_new) = new_job(
            seeded.project.id,
            KIND_RULE,
            Some(previous),
            evaluator,
            None,
            trace(41),
            mid_month,
        );
        enqueue(&app, new).await;
        enqueue(&app, previous_new).await;

        let row = run_once(&app, &seeded.project, current_job, Some(conn)).await;
        assert_eq!(row.status, "skipped");
        assert_eq!(row.error.as_deref(), Some("budget"));
        let row = run_once(&app, &seeded.project, previous_job, Some(conn)).await;
        assert_eq!(row.status, "done", "{:?}", row.error);
    }

    #[tokio::test]
    async fn a_judge_run_counts_its_tokens_and_its_score() {
        let mock = MockLlm::start(vec![completion(JUDGE_ANSWER)]).await;
        let (app, seeded) = test_app().await;
        let conn = connection(&app, &seeded.cookie, &mock).await;
        let evaluator = judge(&app, &seeded, conn, "gpt-test").await;
        let rule_id = rule(&app, &seeded, evaluator, None, MAPPING).await;
        store_trace(&app, &seeded.project, trace(42), "production").await;
        let (job, new) = new_job(
            seeded.project.id,
            KIND_RULE,
            Some(rule_id),
            evaluator,
            None,
            trace(42),
            app.state.clock.now_ns(),
        );
        enqueue(&app, new).await;

        let row = run_once(&app, &seeded.project, job, Some(conn)).await;
        assert_eq!(row.status, "done", "{:?}", row.error);
        let (input, output, _, scores) = usage_sums(&app, &seeded).await;
        // The mock's completion reports 10 input and 3 output tokens.
        assert_eq!((input, output), (10, 3));
        assert_eq!(scores, 1);
    }

    #[tokio::test]
    async fn a_lost_claim_still_records_the_spend() {
        let mock = MockLlm::start(vec![completion_for(PRICED, JUDGE_ANSWER)]).await;
        let (app, seeded) = test_app().await;
        let conn = connection(&app, &seeded.cookie, &mock).await;
        let evaluator = judge(&app, &seeded, conn, PRICED).await;
        let rule_id = rule(&app, &seeded, evaluator, Some((5.0, "day")), MAPPING).await;
        store_trace(&app, &seeded.project, trace(47), "production").await;
        let (job, new) = new_job(
            seeded.project.id,
            KIND_RULE,
            Some(rule_id),
            evaluator,
            None,
            trace(47),
            app.state.clock.now_ns(),
        );
        enqueue(&app, new).await;
        let first = claimed(&app, &seeded.project, job, Some(conn)).await;
        let Ready { reservation, call } = prepare(&app.state, &first).await.ok().expect("prepared");
        let _second = reclaim(&app, &seeded.project, job, Some(conn)).await;

        let ended = call_evaluator(&app.state, &first, call).await;
        finish_job(&app.state, &first, ended, reservation, 1)
            .await
            .expect("finish");

        let row = job_row(&app, &seeded.project, job).await;
        assert_eq!(row.status, "running", "the stale finish changed nothing");
        let now = app.state.clock.now_ns();
        let spent = rule_spend::spent_between(
            app.state.meta.reader(),
            rule_id,
            budget::day_start(now),
            budget::day_start(now) + budget::DAY_NS,
        )
        .await
        .expect("read");
        assert!(
            spent > 0.0,
            "the provider was paid, so the spend is recorded"
        );
        let (_, reserved) = app
            .state
            .eval
            .budgets
            .view(rule_id, budget::day_start(now))
            .expect("loaded");
        assert_eq!(reserved, 0.0, "the reservation is settled");
    }

    #[tokio::test]
    async fn a_lost_start_attempt_releases_the_reservation() {
        let mock = MockLlm::start(vec![]).await;
        let (app, seeded) = test_app().await;
        let conn = connection(&app, &seeded.cookie, &mock).await;
        let evaluator = judge(&app, &seeded, conn, PRICED).await;
        let rule_id = rule(&app, &seeded, evaluator, Some((5.0, "day")), MAPPING).await;
        store_trace(&app, &seeded.project, trace(48), "production").await;
        let (job, new) = new_job(
            seeded.project.id,
            KIND_RULE,
            Some(rule_id),
            evaluator,
            None,
            trace(48),
            app.state.clock.now_ns(),
        );
        enqueue(&app, new).await;
        let first = claimed(&app, &seeded.project, job, Some(conn)).await;
        // The job is claimed again before this claim starts its attempt: the reservation made in
        // step 10 must go back when the start is refused.
        let _second = reclaim(&app, &seeded.project, job, Some(conn)).await;

        let outcome = prepare(&app.state, &first).await;
        assert!(matches!(outcome, Err(Exit::Gone)));
        let now = app.state.clock.now_ns();
        assert_eq!(
            app.state.eval.budgets.view(rule_id, budget::day_start(now)),
            Some((0.0, 0.0))
        );
        assert!(mock.requests().is_empty());
    }

    #[tokio::test]
    async fn an_unpriced_answer_spends_the_estimate() {
        // The judge's model is priced, but the answer names `gpt-test`, which is not: the call's
        // cost is unknown, so the spend is the estimate.
        let mock = MockLlm::start(vec![completion(JUDGE_ANSWER)]).await;
        let (app, seeded) = test_app().await;
        let conn = connection(&app, &seeded.cookie, &mock).await;
        let evaluator = judge(&app, &seeded, conn, PRICED).await;
        let rule_id = rule(&app, &seeded, evaluator, Some((5.0, "day")), MAPPING).await;
        store_trace(&app, &seeded.project, trace(49), "production").await;
        let (job, new) = new_job(
            seeded.project.id,
            KIND_RULE,
            Some(rule_id),
            evaluator,
            None,
            trace(49),
            app.state.clock.now_ns(),
        );
        enqueue(&app, new).await;

        let row = run_once(&app, &seeded.project, job, Some(conn)).await;
        assert_eq!(row.status, "done", "{:?}", row.error);
        let now = app.state.clock.now_ns();
        let prices = app.state.ingest.prices().builtin();
        let adapter = Adapter::from_name("openai").expect("adapter");
        let price = budget::model_price(&prices, PRICED, adapter, now).expect("priced");
        let prompt = format!("Question: {QUESTION}\nAnswer: {ANSWER}");
        let estimate = budget::estimate_usd(prompt.chars().count(), 1024, price);
        let spent = rule_spend::spent_between(
            app.state.meta.reader(),
            rule_id,
            budget::day_start(now),
            budget::day_start(now) + budget::DAY_NS,
        )
        .await
        .expect("read");
        assert_eq!(spent, estimate);
    }

    /// Stores one span of `trace` per time in `ingest_times`, all of `user`: the first is the root.
    /// Each span keeps its `ingest_time`, so a deletion's cut-off can fall between them.
    async fn store_user_spans(
        app: &TestApp,
        project: &ProjectRow,
        trace: TraceId,
        user: &str,
        ingest_times: &[i64],
    ) {
        let now = app.state.clock.now_ns();
        let mut rows = Vec::new();
        for (index, at) in ingest_times.iter().enumerate() {
            let mut row: Span = dataset_span(0);
            row.trace_id = trace;
            row.span_id = span(index as u64 + 1);
            row.parent_span_id = (index > 0).then(|| span(1));
            row.input = Some(QUESTION.to_owned());
            row.input_format = Some(PayloadFormat::Text);
            row.output = Some(ANSWER.to_owned());
            row.output_format = Some(PayloadFormat::Text);
            row.environment = Some("production".to_owned());
            row.session_id = Some("session-7".to_owned());
            row.user_id = Some(user.to_owned());
            row.start_time = now - 30_000_000_000;
            row.end_time = now - 20_000_000_000;
            row.ingest_time = *at;
            rows.push(row);
        }
        app.commit_spans(project, rows).await;
    }

    /// Waits until the mock has seen `count` requests (a judge call in flight).
    async fn wait_for_requests(mock: &MockLlm, count: usize) {
        for _ in 0..500 {
            if mock.requests().len() >= count {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("the judge was not called");
    }

    #[tokio::test]
    async fn a_job_on_a_deleted_trace_is_skipped() {
        let mock = MockLlm::start(vec![completion(JUDGE_ANSWER)]).await;
        let (app, seeded) = test_app().await;
        let conn = connection(&app, &seeded.cookie, &mock).await;
        let evaluator = judge(&app, &seeded, conn, "gpt-test").await;
        store_trace(&app, &seeded.project, trace(10), "production").await;
        let now = app.state.clock.now_ns();
        // D-M-6: the guard holds the trace from a deletion made just now.
        app.state
            .deletions
            .guard
            .add_traces(seeded.project.id, &[trace(10)], Uuid::now_v7(), now);
        let (job, new) = new_job(
            seeded.project.id,
            KIND_MANUAL,
            None,
            evaluator,
            Some(MAPPING),
            trace(10),
            now,
        );
        enqueue(&app, new).await;

        let row = run_once(&app, &seeded.project, job, Some(conn)).await;
        assert_eq!(row.status, "skipped");
        assert_eq!(row.error.as_deref(), Some("deleted"));
        assert_eq!(row.input_tokens, None, "no call, no usage");
        assert!(scores_of(&app, &seeded, evaluator).await.is_empty());
        assert!(!judge_trace_exists(&app, &seeded, job_judge_trace_id(job, 1)).await);
        assert!(
            mock.requests().is_empty(),
            "no request reaches the provider"
        );
    }

    #[tokio::test]
    async fn a_job_on_a_deleted_users_old_trace_is_skipped() {
        let mock = MockLlm::start(vec![completion(JUDGE_ANSWER)]).await;
        let (app, seeded) = test_app().await;
        let conn = connection(&app, &seeded.cookie, &mock).await;
        let evaluator = judge(&app, &seeded, conn, "gpt-test").await;
        let t = app.state.clock.now_ns() - 1_000_000_000;
        // Older data of u1, and newer data of u1 in the same trace.
        store_user_spans(&app, &seeded.project, trace(11), "u1", &[t, t + 2]).await;
        let now = app.state.clock.now_ns();
        // D-M-6: the user was deleted after the older data was ingested.
        app.state
            .deletions
            .guard
            .add_user(seeded.project.id, "u1", Uuid::now_v7(), t + 1);
        let (job, new) = new_job(
            seeded.project.id,
            KIND_MANUAL,
            None,
            evaluator,
            Some(MAPPING),
            trace(11),
            now,
        );
        enqueue(&app, new).await;

        let row = run_once(&app, &seeded.project, job, Some(conn)).await;
        assert_eq!(row.status, "skipped");
        assert_eq!(row.error.as_deref(), Some("deleted"));
        assert!(mock.requests().is_empty());
    }

    #[tokio::test]
    async fn new_data_of_a_deleted_user_is_still_evaluated() {
        let mock = MockLlm::start(vec![completion(JUDGE_ANSWER)]).await;
        let (app, seeded) = test_app().await;
        let conn = connection(&app, &seeded.cookie, &mock).await;
        let evaluator = judge(&app, &seeded, conn, "gpt-test").await;
        let t = app.state.clock.now_ns() - 1_000_000_000;
        store_user_spans(&app, &seeded.project, trace(12), "u1", &[t, t]).await;
        let now = app.state.clock.now_ns();
        // The deletion predates all the data of u1 in the trace.
        app.state
            .deletions
            .guard
            .add_user(seeded.project.id, "u1", Uuid::now_v7(), t - 1);
        let (job, new) = new_job(
            seeded.project.id,
            KIND_MANUAL,
            None,
            evaluator,
            Some(MAPPING),
            trace(12),
            now,
        );
        enqueue(&app, new).await;

        let row = run_once(&app, &seeded.project, job, Some(conn)).await;
        assert_eq!(row.status, "done", "{:?}", row.error);
        assert_eq!(mock.requests().len(), 1);
        assert_eq!(scores_of(&app, &seeded, evaluator).await.len(), 1);
    }

    #[tokio::test]
    async fn a_trace_deleted_during_the_judge_call_is_not_committed() {
        let mock =
            MockLlm::start_with_delay(vec![completion(JUDGE_ANSWER)], Duration::from_millis(1500))
                .await;
        let (app, seeded) = test_app().await;
        let conn = connection(&app, &seeded.cookie, &mock).await;
        let evaluator = judge(&app, &seeded, conn, "gpt-test").await;
        store_trace(&app, &seeded.project, trace(13), "production").await;
        let now = app.state.clock.now_ns();
        let (job, new) = new_job(
            seeded.project.id,
            KIND_MANUAL,
            None,
            evaluator,
            Some(MAPPING),
            trace(13),
            now,
        );
        enqueue(&app, new).await;

        let claimed = claimed(&app, &seeded.project, job, Some(conn)).await;
        let claims = claimed.claims;
        let state = app.state.clone();
        let task = tokio::spawn(async move { run_job(&state, claimed).await });
        wait_for_requests(&mock, 1).await;
        // D-M-6: the deletion lands while the provider is answering.
        app.state.deletions.guard.add_traces(
            seeded.project.id,
            &[trace(13)],
            Uuid::now_v7(),
            app.state.clock.now_ns(),
        );
        task.await.expect("the task").expect("run");

        let row = job_row(&app, &seeded.project, job).await;
        assert_eq!(row.status, "skipped");
        assert_eq!(row.error.as_deref(), Some("deleted"));
        // The call's usage is kept; its cost is unpriced (`gpt-test`).
        assert_eq!(row.input_tokens, Some(10));
        assert_eq!(row.output_tokens, Some(3));
        assert_eq!(row.cost_usd, None);
        assert_eq!(mock.requests().len(), 1, "the provider was called once");
        assert!(scores_of(&app, &seeded, evaluator).await.is_empty());
        assert!(!judge_trace_exists(&app, &seeded, job_judge_trace_id(job, claims)).await);
    }

    #[tokio::test]
    async fn deleted_after_the_call_spends_the_actual_cost() {
        let mock = MockLlm::start_with_delay(
            vec![completion_for(PRICED, JUDGE_ANSWER)],
            Duration::from_millis(1500),
        )
        .await;
        let (app, seeded) = test_app().await;
        let conn = connection(&app, &seeded.cookie, &mock).await;
        let evaluator = judge(&app, &seeded, conn, PRICED).await;
        let rule_id = rule(&app, &seeded, evaluator, Some((5.0, "day")), MAPPING).await;
        store_trace(&app, &seeded.project, trace(14), "production").await;
        let now = app.state.clock.now_ns();
        let (job, new) = new_job(
            seeded.project.id,
            KIND_RULE,
            Some(rule_id),
            evaluator,
            None,
            trace(14),
            now,
        );
        enqueue(&app, new).await;

        let claimed = claimed(&app, &seeded.project, job, Some(conn)).await;
        let state = app.state.clone();
        let task = tokio::spawn(async move { run_job(&state, claimed).await });
        wait_for_requests(&mock, 1).await;
        app.state.deletions.guard.add_traces(
            seeded.project.id,
            &[trace(14)],
            Uuid::now_v7(),
            app.state.clock.now_ns(),
        );
        task.await.expect("the task").expect("run");

        let row = job_row(&app, &seeded.project, job).await;
        assert_eq!(row.status, "skipped");
        assert_eq!(row.error.as_deref(), Some("deleted"));
        let cost = row.cost_usd.expect("the priced call has a cost");
        assert!(cost > 0.0);
        let day = budget::day_start(app.state.clock.now_ns());
        let spent =
            rule_spend::spent_between(app.state.meta.reader(), rule_id, day, day + budget::DAY_NS)
                .await
                .expect("read");
        assert_eq!(spent, cost, "the rule pays the call's actual cost");
        let (_, reserved) = app.state.eval.budgets.view(rule_id, day).expect("loaded");
        assert_eq!(reserved, 0.0, "the reservation is settled");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_copy_committed_after_the_cut_off_gets_a_follow_up() {
        let (app, seeded) = test_app().await;
        let project = seeded.project.id;
        let now = app.state.clock.now_ns();
        let (target, judge_copy) = (trace(15), trace(16));
        store_trace(&app, &seeded.project, target, "production").await;
        // A stand-in judge trace of `target`, committed with its own ingest time.
        let mut copy: Span = dataset_span(0);
        copy.trace_id = judge_copy;
        copy.span_id = span(1);
        copy.parent_span_id = None;
        copy.environment = Some("cetra-eval".to_owned());
        copy.metadata.clear();
        copy.metadata.insert(
            cetra_core::names::metadata_key::EVAL_TARGET_TRACE_ID.to_owned(),
            target.to_hex(),
        );
        copy.ingest_time = now - 1_000_000_000;
        app.commit_spans(&seeded.project, vec![copy]).await;
        assert!(judge_trace_exists(&app, &seeded, judge_copy).await);

        // The deletion of `target` (its row P), and the guard entry the request makes.
        let parent = Uuid::now_v7();
        let parent_row = NewDeletion {
            id: parent,
            org_id: seeded.org_id,
            project_id: Some(project),
            kind: DeletionKind::TraceIds,
            user_id: None,
            request: RequestDetails::default(),
            actor_kind: ActorKind::User,
            actor_id: seeded.user_id,
            state: DeletionState::Resolving,
            created_at: now,
        };
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    insert_deletion(conn, &parent_row).await?;
                    insert_traces(conn, parent, &[target]).await
                })
            })
            .await
            .expect("parent");
        app.state
            .deletions
            .guard
            .add_traces(project, &[target], parent, now);

        assert!(after_commit(&app.state, project, target, &[], judge_copy).await);
        let follow_up: String =
            sqlx::query_scalar("SELECT id FROM deletions WHERE project_id = ? AND id <> ?")
                .bind(project.to_string())
                .bind(parent.to_string())
                .fetch_one(app.state.meta.reader())
                .await
                .expect("the follow-up row");
        let follow_up = Uuid::parse_str(&follow_up).expect("uuid");
        let row = deletion_by_id(app.state.meta.reader(), follow_up)
            .await
            .expect("read")
            .expect("row");
        assert_eq!(row.state, DeletionState::Resolving);
        assert_eq!(row.actor_id, seeded.user_id, "the parent's actor");
        assert_eq!(row.request.requested, Some(2));
        let mut expected = vec![target, judge_copy];
        expected.sort();
        assert_eq!(
            scope_traces(app.state.meta.reader(), follow_up)
                .await
                .expect("scope"),
            expected
        );

        deletion::run_once(&app.state).await.expect("worker");
        let row = deletion_by_id(app.state.meta.reader(), follow_up)
            .await
            .expect("read")
            .expect("row");
        assert_eq!(row.state, DeletionState::Pending);
        assert!(!judge_trace_exists(&app, &seeded, judge_copy).await);

        // A trace nobody guards blocks nothing and stores nothing.
        let deletions_before: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM deletions")
            .fetch_one(app.state.meta.reader())
            .await
            .expect("count");
        assert!(!after_commit(&app.state, project, trace(17), &[], judge_copy).await);
        let deletions_after: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM deletions")
            .fetch_one(app.state.meta.reader())
            .await
            .expect("count");
        assert_eq!(deletions_before, deletions_after);
    }

    #[tokio::test]
    async fn a_failed_evaluation_of_a_deleted_trace_commits_nothing() {
        // The provider refuses the call, so the evaluation fails (D-I-16 still commits the judge
        // trace when it runs). The deletion lands during the call and check B stops the commit.
        let mock = MockLlm::start_with_delay(
            vec![reply_status(400, "bad request")],
            Duration::from_millis(1500),
        )
        .await;
        let (app, seeded) = test_app().await;
        let conn = connection(&app, &seeded.cookie, &mock).await;
        let evaluator = judge(&app, &seeded, conn, "gpt-test").await;
        store_trace(&app, &seeded.project, trace(20), "production").await;
        let now = app.state.clock.now_ns();
        let (job, new) = new_job(
            seeded.project.id,
            KIND_MANUAL,
            None,
            evaluator,
            Some(MAPPING),
            trace(20),
            now,
        );
        enqueue(&app, new).await;

        let claimed = claimed(&app, &seeded.project, job, Some(conn)).await;
        let claims = claimed.claims;
        let state = app.state.clone();
        let task = tokio::spawn(async move { run_job(&state, claimed).await });
        wait_for_requests(&mock, 1).await;
        app.state.deletions.guard.add_traces(
            seeded.project.id,
            &[trace(20)],
            Uuid::now_v7(),
            app.state.clock.now_ns(),
        );
        task.await.expect("the task").expect("run");

        let row = job_row(&app, &seeded.project, job).await;
        assert_eq!(row.status, "skipped", "{:?}", row.error);
        assert_eq!(row.error.as_deref(), Some("deleted"));
        assert!(scores_of(&app, &seeded, evaluator).await.is_empty());
        assert!(!judge_trace_exists(&app, &seeded, job_judge_trace_id(job, claims)).await);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_score_write_blocked_by_a_deletion_stores_one_follow_up() {
        let (app, seeded) = test_app().await;
        let project = seeded.project.id;
        let now = app.state.clock.now_ns();
        let (target, judge_copy) = (trace(21), trace(22));
        store_trace(&app, &seeded.project, target, "production").await;
        // The deletion of `target` is stored, and the guard knows it (as `create_deletion` does).
        let parent = Uuid::now_v7();
        let parent_row = NewDeletion {
            id: parent,
            org_id: seeded.org_id,
            project_id: Some(project),
            kind: DeletionKind::TraceIds,
            user_id: None,
            request: RequestDetails::default(),
            actor_kind: ActorKind::User,
            actor_id: seeded.user_id,
            state: DeletionState::Resolving,
            created_at: now,
        };
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    insert_deletion(conn, &parent_row).await?;
                    insert_traces(conn, parent, &[target]).await
                })
            })
            .await
            .expect("parent");
        app.state
            .deletions
            .guard
            .add_traces(project, &[target], parent, now);

        let item = ScoreCreate {
            score_id: None,
            target: ScoreTarget::Trace { trace_id: target },
            session_id: None,
            user_id: None,
            name: "relevance".to_owned(),
            value: ScoreValue::Numeric(0.5),
            reason: None,
            source: None,
            metadata: BTreeMap::new(),
            experiment_id: None,
            dataset_item_id: None,
            queue_id: None,
        };
        let writer = Writer::Evaluator {
            kind: EvaluatorKind::LlmJudge,
            evaluator_id: Uuid::now_v7(),
            rule_id: None,
        };
        let outcome = write_score_unless_deleted(
            &app.state,
            &seeded.project,
            writer,
            &item,
            target,
            &[],
            judge_copy,
        )
        .await
        .expect("no write error");
        assert!(matches!(outcome, ScoreWrite::Blocked));

        let response = app
            .request(Method::GET, &format!("/api/v1/projects/{project}/scores"))
            .header(header::COOKIE.as_str(), &seeded.cookie)
            .send()
            .await;
        assert_eq!(response.status, 200);
        let page: Value = response.json();
        assert!(
            page["data"].as_array().expect("data").is_empty(),
            "no score row"
        );

        let follow_ups: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM deletions WHERE project_id = ? AND id <> ?")
                .bind(project.to_string())
                .bind(parent.to_string())
                .fetch_one(app.state.meta.reader())
                .await
                .expect("count");
        assert_eq!(follow_ups, 1, "exactly one follow-up");
    }
}
