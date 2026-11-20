//! The IC candidate workers (track IC M3, D-IC-12): a loop that claims ready candidates and runs
//! each one on a worker slot, and the startup and janitor interruption of candidates left
//! `running`.

use std::collections::HashSet;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::Duration;

use cetra_core::ids::{SpanId, TraceId};
use cetra_core::score::Score;
use cetra_core::span::Span;
use cetra_eval::issue::cluster;
use cetra_eval::{Adapter, CallTrace, CompletionInput, EmbeddingInput, ErrorClass};
use cetra_otel::Normalized;
use cetra_storage::{
    ListScope, QueryContext, RunError, SortOrder, TimeWindow, TraceListRequest,
    TraceRefreshRequest, TraceSort,
};
use futures::FutureExt;
use serde_json::Value;
use sha2::{Digest, Sha256};
use sqlx::SqliteConnection;
use tokio::sync::watch;
use uuid::Uuid;

use super::errors::{self, Occurrence};
use super::prompts;
use crate::api::convert;
use crate::api::online_rules::BudgetPeriod;
use crate::entitlements::{Visibility, query_options};
use crate::eval::budget::{self, Reservation, Reserve};
use crate::eval::execute::{self, JudgeUsage};
use crate::eval::retry;
use crate::eval::trace_load::{self, LoadError};
use crate::eval::trigger::READ_TIMEOUT;
use crate::http::AppState;
use crate::ingest::IngestError;
use crate::meta::MetaError;
use crate::meta::connections::{self, ConnectionRow};
use crate::meta::issues::{
    self, CandidateFinish, CandidateRow, CandidateTarget, SourceKind, SourceRow, claim_candidate,
    finish_candidate, interrupt_candidates, ready_candidates,
};
use crate::meta::projects::{self, ProjectRow};

/// How often the loop looks for ready candidates when nothing wakes it.
const TICK: Duration = Duration::from_secs(1);

/// Candidates read from the store per free worker slot.
const CANDIDATES_PER_SLOT: u32 = 4;

/// How long a candidate waits when its source is missing or paused, before it is looked at again.
const REQUEUE_NS: i64 = 60_000_000_000;

/// Most characters of a summary kept (M3.4c step 5).
const SUMMARY_CHARS: usize = 300;
/// Output cap of the summary call, in tokens (M3.4c step 5).
const SUMMARY_TOKENS: u32 = 200;
/// Whole-call timeout of the summary call.
const SUMMARY_TIMEOUT: Duration = Duration::from_secs(60);
/// Whole-call timeout of the embedding call.
const EMBED_TIMEOUT: Duration = Duration::from_secs(30);
/// Embedding size requested from the provider (D-IC-5).
const EMBED_DIMENSIONS: u32 = 512;
/// Members a centroid averages; past this a join no longer moves it (D-IC-7).
const CENTROID_MEMBERS: u64 = 500;
/// How far before a session candidate's score its latest trace may start (A4).
const SESSION_WINDOW_NS: i64 = 30 * budget::DAY_NS;

/// A candidate `running` for longer than this is interrupted by the janitor (D-I-12). The same
/// window as the eval janitor's `STALE_NS`.
const STALE_NS: i64 = 600_000_000_000;

/// Runs the candidate loop until `stop` is set. A pass runs when the tick fires or a worker
/// wakes the loop. Each claimed candidate runs on its own task, holding one `issue_workers`
/// slot.
pub(crate) async fn run_workers(state: AppState, mut stop: watch::Receiver<bool>) {
    let mut interval = tokio::time::interval(TICK);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            biased;
            _ = stop.wait_for(|stopped| *stopped) => return,
            _ = interval.tick() => {}
            () = state.issue_wake.notified() => {}
        }
        if let Err(error) = run_pass(&state).await {
            tracing::error!(error = %error, "issue worker pass failed");
        }
    }
}

/// One pass: reads the ready candidates of connections with a free slot and token, claims as
/// many as there are free worker slots, and starts each. Returns how many started.
async fn run_pass(state: &AppState) -> Result<usize, MetaError> {
    let now = state.clock.now_ns();
    let free = state.issue_workers.available_permits();
    if free == 0 {
        return Ok(0);
    }
    let saturated = state.eval.saturated_connections(now);
    let read = u32::try_from(free)
        .unwrap_or(u32::MAX)
        .saturating_mul(CANDIDATES_PER_SLOT);
    let ready = ready_candidates(state.meta.reader(), now, &saturated, read).await?;
    if ready.is_empty() {
        return Ok(0);
    }
    let mut picked = Vec::new();
    for candidate in ready {
        let Ok(permit) = Arc::clone(&state.issue_workers).try_acquire_owned() else {
            break;
        };
        picked.push((candidate, permit));
    }
    let ids: Vec<Uuid> = picked.iter().map(|(candidate, _)| candidate.id).collect();
    let claims = state
        .meta
        .write(move |conn| Box::pin(async move { claim_all(conn, ids, now).await }))
        .await?;

    let mut started = 0;
    for ((candidate, permit), claim) in picked.into_iter().zip(claims) {
        // A `None` claim: the candidate left `queued` since the read. Its permit drops here.
        let Some(claims) = claim else { continue };
        let state = state.clone();
        tokio::spawn(async move {
            run_candidate(&state, candidate, claims).await;
            drop(permit);
            state.issues_wake();
        });
        started += 1;
    }
    Ok(started)
}

async fn claim_all(
    conn: &mut SqliteConnection,
    ids: Vec<Uuid>,
    now: i64,
) -> Result<Vec<Option<u32>>, MetaError> {
    let mut claims = Vec::with_capacity(ids.len());
    for id in ids {
        claims.push(claim_candidate(conn, id, now).await?);
    }
    Ok(claims)
}

/// How a run stops early: with the candidate's finish to write, with nothing to write because the
/// claim is no longer current, or with a `meta.db` failure that leaves the candidate `running` for
/// the janitor (A5, A12).
enum Exit {
    End(CandidateFinish),
    Lost,
    Meta(MetaError),
}

impl From<MetaError> for Exit {
    fn from(error: MetaError) -> Self {
        Self::Meta(error)
    }
}

/// What a run works on: the source, project and candidate as read, and the claim of this run.
#[derive(Clone, Copy)]
struct Job<'a> {
    state: &'a AppState,
    source: &'a SourceRow,
    project: &'a ProjectRow,
    candidate: &'a CandidateRow,
    claims: u32,
}

/// The trace of a run, once loaded (A4, step 3).
struct Loaded {
    trace: TraceId,
    /// The root span's start, nanoseconds.
    trace_start: i64,
    spans: Vec<Span>,
    /// The end users of the trace with their oldest ingest times (D-M-6).
    users: Vec<(String, i64)>,
    /// The root span's user and session.
    user_id: Option<String>,
    session_id: Option<String>,
}

/// Runs one claimed candidate (M3.4c). `candidate` is the row as read before the claim: the claim
/// changes only its status and claim count. A panic fails the candidate with `internal` (A12).
async fn run_candidate(state: &AppState, candidate: CandidateRow, claims: u32) {
    let id = candidate.id;
    let source_id = candidate.source_id;
    let outcome = AssertUnwindSafe(execute_candidate(state, &candidate, claims))
        .catch_unwind()
        .await;
    match outcome {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            tracing::error!(candidate_id = %id, error = %error, "issue candidate could not be recorded");
        }
        Err(_panic) => {
            tracing::error!(candidate_id = %id, "issue candidate panicked");
            let failed = CandidateFinish::Failed { class: "internal" };
            if let Err(error) = write_finish(state, source_id, id, claims, failed, 0.0).await {
                tracing::error!(candidate_id = %id, error = %error, "issue candidate could not be failed");
            }
        }
    }
}

/// Runs the steps of a candidate and writes its finish, unless a step wrote it already (the
/// assignment does, in its own write). A `meta.db` failure is returned so the candidate stays
/// `running` for the janitor.
async fn execute_candidate(
    state: &AppState,
    candidate: &CandidateRow,
    claims: u32,
) -> Result<(), MetaError> {
    let mut spent_usd = 0.0;
    match steps(state, candidate, claims, &mut spent_usd).await {
        Ok(()) => Ok(()),
        Err(Exit::End(finish)) => {
            let status = finish_label(&finish);
            let class = match &finish {
                CandidateFinish::Failed { class } | CandidateFinish::Skipped { class } => *class,
                _ => "none",
            };
            let stored = write_finish(
                state,
                candidate.source_id,
                candidate.id,
                claims,
                finish,
                spent_usd,
            )
            .await?;
            tracing::debug!(
                candidate_id = %candidate.id,
                source_id = %candidate.source_id,
                status,
                class,
                stored,
                "issue candidate finished"
            );
            Ok(())
        }
        Err(Exit::Lost) => {
            tracing::debug!(candidate_id = %candidate.id, "issue candidate claim is no longer current");
            Ok(())
        }
        Err(Exit::Meta(error)) => Err(error),
    }
}

/// The status name of a finish, for the debug log.
fn finish_label(finish: &CandidateFinish) -> &'static str {
    match finish {
        CandidateFinish::Summarized { .. } => "summarized",
        CandidateFinish::Pending { .. } => "pending",
        CandidateFinish::Assigned { .. } => "assigned",
        CandidateFinish::Dropped => "dropped",
        CandidateFinish::Retry { .. } => "retry",
        CandidateFinish::LoadRetry { .. } => "load_retry",
        CandidateFinish::Requeue { .. } => "requeue",
        CandidateFinish::PauseRequeue { .. } => "pause_requeue",
        CandidateFinish::Failed { .. } => "failed",
        CandidateFinish::Skipped { .. } => "skipped",
    }
}

/// Steps 1 to 8 of M3.4c. Returns `Ok` only when the assignment has written the candidate's finish.
async fn steps(
    state: &AppState,
    candidate: &CandidateRow,
    claims: u32,
    spent_usd: &mut f64,
) -> Result<(), Exit> {
    let now = state.clock.now_ns();
    let reader = state.meta.reader();
    // Step 1: a missing, disabled or paused source waits.
    let Some(source) =
        issues::get_source(reader, candidate.project_id, candidate.source_id).await?
    else {
        return Err(requeue(now, REQUEUE_NS));
    };
    let Some(project) = projects::project_by_id(reader, candidate.project_id).await? else {
        return Err(requeue(now, REQUEUE_NS));
    };
    if !source.enabled || source.paused_reason.is_some() {
        return Err(requeue(now, REQUEUE_NS));
    }
    let job = Job {
        state,
        source: &source,
        project: &project,
        candidate,
        claims,
    };
    // Step 2, check 0 (D-M-6): the trace is deleted before the run starts. A session candidate
    // has no trace yet; its check comes after the load.
    if let Some(trace) = candidate.trace_id
        && state
            .deletions
            .guard
            .blocking(project.id, trace, &[], now)
            .is_some()
    {
        return Err(skip("deleted"));
    }
    // Step 3: the target loads, and its target fields are set (A4).
    let loaded = load(&job).await?;
    // Step 4: a score or feedback candidate must still match the source's filter.
    if source.kind != SourceKind::Topic && !filter_matches(&job, loaded.trace).await? {
        return Err(Exit::End(CandidateFinish::Dropped));
    }
    // Step 5: the summary, unless an earlier attempt stored one.
    let summary = match candidate.summary.clone() {
        Some(summary) => summary,
        None => summarize(&job, &loaded, spent_usd).await?,
    };
    // Step 6 and 7: the embedding and its dimension.
    let vector = embed(&job, &loaded, &summary, spent_usd).await?;
    // Step 8: the assignment.
    assign(&job, &loaded, &summary, vector, spent_usd).await
}

/// Step 3: resolves a session candidate to its latest trace (A4), loads the trace, and sets the
/// candidate's trace fields. The deletion check A follows the load (D-M-6).
async fn load(job: &Job<'_>) -> Result<Loaded, Exit> {
    let Job {
        state,
        project,
        candidate,
        claims,
        ..
    } = *job;
    let now = state.clock.now_ns();
    let (trace, span) = match candidate.trace_id {
        Some(trace) => (trace, candidate.span_id),
        None => (resolve_session(job).await?, None),
    };
    let around = candidate.trace_start.unwrap_or(candidate.created_at);
    let loaded = match trace_load::load_trace(
        state,
        project.id,
        Visibility::window(state, project.org_id),
        trace,
        span,
        Some(around),
    )
    .await
    {
        Ok(loaded) => loaded,
        Err(LoadError::Saturated) => {
            return Err(requeue(now, retry::SATURATED_REQUEUE_NS));
        }
        Err(LoadError::NotFound | LoadError::Busy) => {
            return Err(load_retry(candidate, now, "trace_not_found"));
        }
        Err(_) => return Err(fail("permanent")),
    };
    let root = root_span(&loaded.spans).ok_or_else(|| fail("permanent"))?;
    let trace_start = root.start_time;
    let user_id = root.user_id.clone();
    let session_id = root.session_id.clone();
    let users = execute::users_of(&loaded.spans);
    let id = candidate.id;
    let (write_user, write_session) = (user_id.clone(), session_id.clone());
    let fenced = state
        .meta
        .write(move |conn| {
            Box::pin(async move {
                issues::set_candidate_target(
                    conn,
                    id,
                    claims,
                    trace,
                    trace_start,
                    write_user.as_deref(),
                    write_session.as_deref(),
                    now,
                )
                .await
            })
        })
        .await?;
    if !fenced {
        return Err(Exit::Lost);
    }
    // D-M-6, check A: the trace's users are known now.
    if state
        .deletions
        .guard
        .blocking(
            project.id,
            trace,
            &execute::borrowed_users(&users),
            state.clock.now_ns(),
        )
        .is_some()
    {
        return Err(skip("deleted"));
    }
    Ok(Loaded {
        trace,
        trace_start,
        spans: loaded.spans,
        users,
        user_id,
        session_id,
    })
}

/// A session candidate's trace (A4): the score's `created_at` bounds the session's traces to
/// `[created_at − 30 days, created_at + 1 ns)`, and the newest one starting in that window is the
/// trace. No score or no trace in the window drops the candidate.
async fn resolve_session(job: &Job<'_>) -> Result<TraceId, Exit> {
    let Job {
        state,
        project,
        candidate,
        ..
    } = *job;
    if candidate.target != CandidateTarget::Session {
        return Err(fail("permanent"));
    }
    let now = state.clock.now_ns();
    let Some(score) = read_score(job, candidate.score_id).await? else {
        return Err(Exit::End(CandidateFinish::Dropped));
    };
    let Some(session) = candidate.session_id.as_deref() else {
        return Err(fail("permanent"));
    };
    let window = TimeWindow::new(
        score.created_at.saturating_sub(SESSION_WINDOW_NS),
        score.created_at.saturating_add(1),
    )
    .map_err(|_| fail("permanent"))?;
    let ctx = read_context(state, project, score.created_at)
        .await
        .ok_or_else(|| load_retry(candidate, now, "storage"))?;
    let filter = format!("session_id = '{}'", session.replace('\'', "''"));
    let page = cetra_storage::trace_list(
        &ctx,
        TraceListRequest {
            filter: Some(filter.as_str()),
            search: None,
            scope: ListScope {
                window: Some(window),
                include_eval: false,
                include_experiments: false,
            },
            strict_exclusions: false,
            sort: TraceSort::StartTime,
            order: SortOrder::Desc,
            limit: 1,
            cursor: None,
            timeout: READ_TIMEOUT,
        },
    )
    .await
    .map_err(|_| load_retry(candidate, now, "storage"))?;
    let traces = convert::trace_summaries(&page.batches).map_err(|_| fail("permanent"))?;
    traces
        .first()
        .map(|trace| trace.trace_id)
        .ok_or(Exit::End(CandidateFinish::Dropped))
}

/// Step 4: whether the trace still matches the source's filter (`trace_refresh`, strict
/// exclusions). An invalid filter fails the candidate; a read failure retries the load.
async fn filter_matches(job: &Job<'_>, trace: TraceId) -> Result<bool, Exit> {
    let Job {
        state,
        source,
        project,
        candidate,
        ..
    } = *job;
    let now = state.clock.now_ns();
    let ctx = read_context(state, project, candidate.created_at)
        .await
        .ok_or_else(|| load_retry(candidate, now, "storage"))?;
    let id = trace.to_string();
    let read = cetra_storage::trace_refresh(
        &ctx,
        TraceRefreshRequest {
            filter: source.filter.as_deref(),
            search: None,
            trace_ids: &[id.as_str()],
            include_eval: false,
            include_experiments: false,
            strict_exclusions: true,
            timeout: READ_TIMEOUT,
        },
    )
    .await;
    match read {
        Ok(batches) => Ok(batches.iter().any(|batch| batch.num_rows() > 0)),
        Err(RunError::Invalid(_) | RunError::InvalidRequest(_)) => Err(fail("permanent")),
        Err(_) => Err(load_retry(candidate, now, "storage")),
    }
}

/// Step 5: the summary call. A problem candidate is summarized from its score and the trace's
/// previews, a topic candidate from its first user message. The summary is stored as soon as it
/// is made (a failed embedding retries without it), so this returns `Ok` only with a stored
/// summary.
async fn summarize(job: &Job<'_>, loaded: &Loaded, spent_usd: &mut f64) -> Result<String, Exit> {
    let Job {
        state,
        source,
        project,
        candidate,
        claims,
    } = *job;
    let now = state.clock.now_ns();
    // Step 5.1: the prompt.
    let prompt = if source.kind == SourceKind::Topic {
        let message = prompts::first_user_message(&loaded.spans);
        if message.trim().is_empty() {
            return Err(Exit::End(CandidateFinish::Dropped));
        }
        prompts::topic_prompt(&message)
    } else {
        let Some(score) = read_score(job, candidate.score_id).await? else {
            return Err(Exit::End(CandidateFinish::Dropped));
        };
        let (input, output) = prompts::previews(&loaded.spans);
        prompts::problem_prompt(
            &score.name,
            &prompts::score_text(&score.value),
            score.reason.as_deref(),
            &input,
            &output,
        )
    };
    let chat_id = source.chat_connection_id.ok_or_else(|| fail("permanent"))?;
    let chat_model = source
        .chat_model
        .as_deref()
        .ok_or_else(|| fail("permanent"))?;
    let row = connection_row(job, chat_id).await?;
    let adapter = Adapter::from_name(&row.adapter).map_err(|_| fail("permanent"))?;
    let connection = state
        .connections
        .get(&state.secrets, &row)
        .map_err(|_| fail("permanent"))?;
    let prices = state
        .ingest
        .prices()
        .for_project(project.org_id, project.id);
    // Step 5.2: the budget. A source with no price for its model has no budget check.
    let estimate = budget::model_price(&prices, chat_model, adapter, now)
        .map(|price| budget::estimate_usd(prompt.chars().count(), SUMMARY_TOKENS, price));
    let reservation = match estimate {
        Some(estimate) => reserve(state, source, estimate, now).await?,
        None => None,
    };
    // Step 5.3: a slot and a token of the chat connection.
    let _slot = state
        .eval
        .try_connection_permit(
            chat_id,
            (row.max_concurrency, row.requests_per_minute),
            state.clock.now_ns(),
        )
        .ok_or_else(|| requeue(now, retry::SATURATED_REQUEUE_NS))?;
    // Step 5.4: the attempt.
    let attempts = start_attempt(job).await?;
    // Step 5.5 and 5.6: the call and its trace.
    let schema = prompts::summary_schema();
    let source_text = source.id.to_string();
    let candidate_text = candidate.id.to_string();
    let metadata = call_metadata(&source_text, &candidate_text);
    let ingest = state.ingest.context(project, None, now);
    let trace_id = call_trace_id(candidate.id, claims, 1);
    let run = state
        .eval_runner
        .complete(CompletionInput {
            connection: &connection,
            model: chat_model,
            prompt: &prompt,
            schema: &schema,
            name: prompts::SUMMARY_NAME,
            description: "Summarize the issue.",
            temperature: Some(0.0),
            max_output_tokens: SUMMARY_TOKENS,
            timeout: SUMMARY_TIMEOUT,
            trace: CallTrace {
                trace_id,
                root_name: "cetra.issue.summarize",
                metadata: &metadata,
                ingest: &ingest,
                prices: &prices,
            },
        })
        .await;
    // Step 5.7: the usage counts, and the reservation settles with the actual cost.
    let usage = JudgeUsage {
        input_tokens: run.usage.as_ref().map(|usage| usage.input),
        output_tokens: run.usage.as_ref().map(|usage| usage.output),
        cost_usd: run.cost_usd,
    };
    let actual = execute::spent(Some(usage), estimate);
    execute::count_judge_usage(state, project, state.clock.now_ns(), usage);
    if let Some(reservation) = reservation {
        reservation.settle(actual);
    }
    *spent_usd += actual;
    commit_call(job, loaded, trace_id, run.trace, attempts).await?;
    // Step 5.8 and 5.9.
    let answer = match run.outcome {
        Ok(answer) => answer,
        Err(error) => {
            return Err(Exit::End(failure(
                candidate,
                attempts,
                state.clock.now_ns(),
                error.class(),
            )));
        }
    };
    let is_issue = answer
        .get("is_issue")
        .and_then(Value::as_bool)
        .ok_or_else(|| fail("permanent"))?;
    let text = answer
        .get("summary")
        .and_then(Value::as_str)
        .ok_or_else(|| fail("permanent"))?;
    if !is_issue {
        return Err(Exit::End(CandidateFinish::Dropped));
    }
    let summary: String = text.trim().chars().take(SUMMARY_CHARS).collect();
    if summary.is_empty() {
        return Err(Exit::End(CandidateFinish::Dropped));
    }
    let finish = CandidateFinish::Summarized {
        summary: summary.clone(),
        prompt_version: prompts::SUMMARY_PROMPT_VERSION,
        call_trace_id: trace_id,
    };
    let stored = write_finish(state, source.id, candidate.id, claims, finish, *spent_usd).await?;
    *spent_usd = 0.0;
    if !stored {
        return Err(Exit::Lost);
    }
    Ok(summary)
}

/// Step 6: the embedding call, with the same budget, slot, attempt and errors as the summary.
/// Returns the vector, which is L2-normalized by the provider's adapter.
async fn embed(
    job: &Job<'_>,
    loaded: &Loaded,
    summary: &str,
    spent_usd: &mut f64,
) -> Result<Vec<f32>, Exit> {
    let Job {
        state,
        source,
        project,
        candidate,
        claims,
    } = *job;
    let now = state.clock.now_ns();
    let (Some(connection_id), Some(model)) =
        (source.embed_connection_id, source.embed_model.as_deref())
    else {
        return Err(fail("permanent"));
    };
    let row = connection_row(job, connection_id).await?;
    let adapter = Adapter::from_name(&row.adapter).map_err(|_| fail("permanent"))?;
    let connection = state
        .connections
        .get(&state.secrets, &row)
        .map_err(|_| fail("permanent"))?;
    let prices = state
        .ingest
        .prices()
        .for_project(project.org_id, project.id);
    let estimate = prices
        .price_for(model, Some(adapter.provider_name()), now)
        .and_then(|price| price.price_input)
        .map(|price_input| embed_estimate_usd(summary, price_input));
    let reservation = match estimate {
        Some(estimate) => reserve(state, source, estimate, now).await?,
        None => None,
    };
    let _slot = state
        .eval
        .try_connection_permit(
            connection_id,
            (row.max_concurrency, row.requests_per_minute),
            state.clock.now_ns(),
        )
        .ok_or_else(|| requeue(now, retry::SATURATED_REQUEUE_NS))?;
    let attempts = start_attempt(job).await?;
    let source_text = source.id.to_string();
    let candidate_text = candidate.id.to_string();
    let metadata = call_metadata(&source_text, &candidate_text);
    let ingest = state.ingest.context(project, None, now);
    let trace_id = call_trace_id(candidate.id, claims, 2);
    let run = state
        .eval_runner
        .embed(EmbeddingInput {
            connection: &connection,
            model,
            text: summary,
            dimensions: Some(EMBED_DIMENSIONS),
            timeout: EMBED_TIMEOUT,
            trace: CallTrace {
                trace_id,
                root_name: "cetra.issue.embed",
                metadata: &metadata,
                ingest: &ingest,
                prices: &prices,
            },
        })
        .await;
    let usage = JudgeUsage {
        input_tokens: run.input_tokens,
        output_tokens: None,
        cost_usd: run.cost_usd,
    };
    let actual = execute::spent(Some(usage), estimate);
    execute::count_judge_usage(state, project, state.clock.now_ns(), usage);
    if let Some(reservation) = reservation {
        reservation.settle(actual);
    }
    *spent_usd += actual;
    commit_call(job, loaded, trace_id, run.trace, attempts).await?;
    match run.outcome {
        Ok(vector) => Ok(vector),
        Err(error) => Err(Exit::End(failure(
            candidate,
            attempts,
            state.clock.now_ns(),
            error.class(),
        ))),
    }
}

/// Steps 7 and 8: the embedding dimension (set once per source), then the assignment. A candidate
/// whose summary is within the source's floor of a centroid joins that issue; otherwise it waits
/// for a refit. The finish and the join are one meta write, and the occurrence is written after it.
async fn assign(
    job: &Job<'_>,
    loaded: &Loaded,
    summary: &str,
    vector: Vec<f32>,
    spent_usd: &mut f64,
) -> Result<(), Exit> {
    let Job {
        state,
        source,
        project,
        candidate,
        claims,
    } = *job;
    let now = state.clock.now_ns();
    let Some(model) = source.embed_model.clone() else {
        return Err(fail("permanent"));
    };
    let dimension = u32::try_from(vector.len()).map_err(|_| fail("permanent"))?;
    // Step 7: the dimension is fixed by the first embedding of the source (D-IC-5).
    let source_id = source.id;
    let stored_dimension = state
        .meta
        .write(move |conn| {
            Box::pin(async move { issues::set_embed_dimension(conn, source_id, dimension).await })
        })
        .await?;
    if stored_dimension != dimension {
        return Err(fail("embedding_dimension_changed"));
    }
    // Step 8: one write. The finish is fenced first; the join follows only when it stored.
    let floor = source.floor;
    let project_id = project.id;
    let id = candidate.id;
    let trace_start = loaded.trace_start;
    let spent = *spent_usd;
    let (stored, assigned) = state
        .meta
        .write(move |conn| {
            Box::pin(async move {
                let centroids = issues::source_centroids(conn, source_id).await?;
                let nearest = cluster::nearest(
                    &vector,
                    centroids
                        .iter()
                        .map(|(_, _, centroid, _)| centroid.as_slice()),
                );
                let Ok(nearest) = nearest else {
                    let failed = CandidateFinish::Failed { class: "permanent" };
                    let stored =
                        finish_with_spend(conn, source_id, id, claims, failed, spent, now).await?;
                    return Ok((stored, None::<Uuid>));
                };
                let joining = match (nearest, floor) {
                    (Some((index, similarity)), Some(floor)) if f64::from(similarity) >= floor => {
                        Some(centroids[index].0)
                    }
                    _ => None,
                };
                let issue = match joining {
                    Some(issue_id) => issues::get_issue_tx(conn, project_id, issue_id).await?,
                    None => None,
                };
                let finish = match &issue {
                    Some(issue) => CandidateFinish::Assigned {
                        issue_id: issue.id,
                        vector: vector.clone(),
                        embed_model: model.clone(),
                        dimension,
                    },
                    None => CandidateFinish::Pending {
                        vector: vector.clone(),
                        embed_model: model.clone(),
                        dimension,
                    },
                };
                let stored =
                    finish_with_spend(conn, source_id, id, claims, finish, spent, now).await?;
                if !stored {
                    return Ok((false, None::<Uuid>));
                }
                let Some(mut issue) = issue else {
                    return Ok((true, None::<Uuid>));
                };
                let issue_id = issue.id;
                errors::join_issue(conn, &mut issue, (trace_start, trace_start), now).await?;
                let members = issue.member_count;
                if members < CENTROID_MEMBERS
                    && let Some(centroid) = issue.centroid.as_mut()
                    && cluster::add_to_centroid(centroid, members, &vector).is_err()
                {
                    tracing::warn!(
                        candidate_id = %id,
                        issue_id = %issue_id,
                        "issue centroid could not be moved; the join keeps the old centroid"
                    );
                }
                issue.member_count = members.saturating_add(1);
                issues::update_issue(conn, &issue).await?;
                Ok((true, Some(issue_id)))
            })
        })
        .await?;
    if !stored {
        return Ok(());
    }
    // The occurrence of an assigned candidate (A3). A failed write keeps the assignment.
    if let Some(issue_id) = assigned {
        let occurrence = Occurrence {
            source_id,
            issue_id,
            trace_id: loaded.trace,
            span_id: candidate.span_id,
            at: loaded.trace_start.min(now),
            user_id: candidate.user_id.clone().or_else(|| loaded.user_id.clone()),
            session_id: candidate
                .session_id
                .clone()
                .or_else(|| loaded.session_id.clone()),
            reason: Some(summary.to_owned()),
        };
        if errors::write_occurrences(state, project, source_id, vec![occurrence])
            .await
            .is_err()
        {
            tracing::warn!(
                candidate_id = %id,
                issue_id = %issue_id,
                "issue occurrence of an assigned candidate was not written"
            );
        }
    }
    Ok(())
}

/// Reserves `estimate` against the source's budget (D-I-15, as for rules). A source without a
/// budget reserves nothing.
async fn reserve<'a>(
    state: &'a AppState,
    source: &SourceRow,
    estimate: f64,
    now: i64,
) -> Result<Option<Reservation<'a>>, Exit> {
    let (Some(limit), Some(period)) = (
        source.budget_usd,
        source
            .budget_period
            .as_deref()
            .and_then(BudgetPeriod::from_name),
    ) else {
        return Ok(None);
    };
    let period_start = budget::period_start(period, now);
    let budgets = &state.eval.budgets;
    let mut attempt = budgets.try_reserve(source.id, period_start, limit, estimate);
    if matches!(attempt, Reserve::NotLoaded) {
        let spent = issues::spent_between(
            state.meta.reader(),
            source.id,
            period_start,
            budget::day_start(now) + budget::DAY_NS,
        )
        .await?;
        budgets.load(source.id, period_start, spent);
        attempt = budgets.try_reserve(source.id, period_start, limit, estimate);
    }
    match attempt {
        Reserve::Reserved(guard) => Ok(Some(guard)),
        Reserve::Over => Err(skip("budget")),
        Reserve::NotLoaded => Err(Exit::End(CandidateFinish::Requeue { not_before: now })),
    }
}

/// Counts one attempt of the candidate for the call that follows, or stops when its claim is no
/// longer current.
async fn start_attempt(job: &Job<'_>) -> Result<u32, Exit> {
    let (id, claims) = (job.candidate.id, job.claims);
    let now = job.state.clock.now_ns();
    job.state
        .meta
        .write(move |conn| {
            Box::pin(async move { issues::start_candidate_attempt(conn, id, claims, now).await })
        })
        .await?
        .ok_or(Exit::Lost)
}

/// Commits a call's trace unless a deletion blocks the trace (A6, D-M-6), then runs the follow-up
/// check. A block is `Skipped { deleted }`; a write that keeps failing takes the retry path.
async fn commit_call(
    job: &Job<'_>,
    loaded: &Loaded,
    call_trace: TraceId,
    trace: Normalized,
    attempts: u32,
) -> Result<(), Exit> {
    let Job {
        state,
        project,
        candidate,
        ..
    } = *job;
    let users = execute::borrowed_users(&loaded.users);
    let committed = execute::retry_write(
        retry::WRITE_TRIES,
        retry::WRITE_RETRY,
        |_: &IngestError| true,
        || {
            execute::commit_unless_deleted(
                state,
                project,
                state.clock.now_ns(),
                loaded.trace,
                &users,
                trace.clone(),
            )
        },
    )
    .await;
    match committed {
        Ok(true) => {}
        Ok(false) => return Err(skip("deleted")),
        Err(_) => {
            return Err(Exit::End(retry_or_fail(
                candidate,
                attempts,
                state.clock.now_ns(),
            )));
        }
    }
    if execute::after_commit(state, project.id, loaded.trace, &users, call_trace).await {
        return Err(skip("deleted"));
    }
    Ok(())
}

/// The finish of a failed call (A5): a retry while attempts are left, else a failure by class.
/// `Pause` fails the candidate until the source pauses (M3.6).
fn failure(
    candidate: &CandidateRow,
    attempts: u32,
    now: i64,
    class: ErrorClass,
) -> CandidateFinish {
    match class {
        ErrorClass::Retryable => retry_or_fail(candidate, attempts, now),
        ErrorClass::Permanent => CandidateFinish::Failed { class: "permanent" },
        ErrorClass::Pause => CandidateFinish::Failed { class: "pause" },
    }
}

/// `Retry` after the backoff of the attempt, or `Failed { retryable }` once the attempts run out
/// (the same limit as eval jobs).
fn retry_or_fail(candidate: &CandidateRow, attempts: u32, now: i64) -> CandidateFinish {
    if attempts < retry::MAX_ATTEMPTS {
        CandidateFinish::Retry {
            not_before: now.saturating_add(retry::backoff_ns(candidate.id, attempts)),
        }
    } else {
        CandidateFinish::Failed { class: "retryable" }
    }
}

/// A failed trace or storage read: `LoadRetry` while the candidate has load retries left, else
/// `Failed` with `exhausted`.
fn load_retry(candidate: &CandidateRow, now: i64, exhausted: &'static str) -> Exit {
    if candidate.load_retries < retry::LOAD_RETRIES {
        Exit::End(CandidateFinish::LoadRetry {
            not_before: now.saturating_add(retry::LOAD_RETRY_NS),
        })
    } else {
        fail(exhausted)
    }
}

fn requeue(now: i64, delay: i64) -> Exit {
    Exit::End(CandidateFinish::Requeue {
        not_before: now.saturating_add(delay),
    })
}

fn fail(class: &'static str) -> Exit {
    Exit::End(CandidateFinish::Failed { class })
}

fn skip(class: &'static str) -> Exit {
    Exit::End(CandidateFinish::Skipped { class })
}

/// The metadata of a call's trace: the ids of its source and candidate, never payload.
fn call_metadata<'a>(source: &'a str, candidate: &'a str) -> [(&'a str, &'a str); 2] {
    [
        ("cetra.issue.source_id", source),
        ("cetra.issue.candidate_id", candidate),
    ]
}

/// The estimate of an embedding call (M3.4c step 6): `ceil(chars / 4)` input tokens at the input
/// price.
fn embed_estimate_usd(text: &str, price_input: f64) -> f64 {
    let tokens = text.chars().count().div_ceil(4) as f64;
    tokens * price_input / 1e6
}

/// The connection row of `id` in the project's org.
async fn connection_row(job: &Job<'_>, id: Uuid) -> Result<ConnectionRow, Exit> {
    connections::get_connection(job.state.meta.reader(), job.project.org_id, id)
        .await?
        .ok_or_else(|| fail("permanent"))
}

/// The score a candidate was queued for, as storage has it now. `None` when it is gone.
async fn read_score(job: &Job<'_>, score_id: Option<Uuid>) -> Result<Option<Score>, Exit> {
    let Job {
        state,
        project,
        candidate,
        ..
    } = *job;
    let now = state.clock.now_ns();
    let Some(score_id) = score_id else {
        return Err(fail("permanent"));
    };
    let ctx = read_context(state, project, candidate.created_at)
        .await
        .ok_or_else(|| load_retry(candidate, now, "storage"))?;
    let id = score_id.to_string();
    let batches = cetra_storage::scores_by_id(&ctx, &[id.as_str()], READ_TIMEOUT)
        .await
        .map_err(|_| load_retry(candidate, now, "storage"))?;
    let scores = convert::scores(&batches, project.id).map_err(|_| fail("permanent"))?;
    Ok(scores.into_iter().next())
}

/// A storage read of the project, around `around`. `None` when it cannot be opened.
async fn read_context(state: &AppState, project: &ProjectRow, around: i64) -> Option<QueryContext> {
    let source = state
        .storage
        .query(
            project.id,
            query_options(Visibility::window(state, project.org_id), Some(around)),
        )
        .await
        .ok()?;
    QueryContext::new(source, None).ok()
}

/// The root span of a trace: the earliest, by `(start_time, span_id)`, of the spans without a
/// parent or whose parent is not among them. The same rule as the prompts' root.
fn root_span(spans: &[Span]) -> Option<&Span> {
    let ids: HashSet<SpanId> = spans.iter().map(|span| span.span_id).collect();
    spans
        .iter()
        .filter(|span| {
            span.parent_span_id
                .is_none_or(|parent| !ids.contains(&parent))
        })
        .min_by_key(|span| (span.start_time, span.span_id))
}

/// Writes a finish with the spend of the run, in its own meta write.
async fn write_finish(
    state: &AppState,
    source: Uuid,
    id: Uuid,
    claims: u32,
    finish: CandidateFinish,
    spent_usd: f64,
) -> Result<bool, MetaError> {
    let now = state.clock.now_ns();
    state
        .meta
        .write(move |conn| {
            Box::pin(async move {
                finish_with_spend(conn, source, id, claims, finish, spent_usd, now).await
            })
        })
        .await
}

/// A finish and its spend, in the caller's write (A5). The spend counts whether or not the fence
/// held: the money was spent.
async fn finish_with_spend(
    conn: &mut SqliteConnection,
    source: Uuid,
    id: Uuid,
    claims: u32,
    finish: CandidateFinish,
    spent_usd: f64,
    now: i64,
) -> Result<bool, MetaError> {
    let stored = finish_candidate(conn, id, claims, finish, spent_usd, now).await?;
    if spent_usd > 0.0 {
        issues::add_spend(conn, source, budget::day_start(now), spent_usd).await?;
    }
    Ok(stored)
}

/// Interrupts the candidates `running` since before `now − STALE_NS` (D-I-12, the janitor's
/// pass). Returns how many rows changed.
///
/// # Errors
///
/// Returns [`MetaError`] when the write fails.
pub(crate) async fn janitor_once(state: &AppState, now: i64) -> Result<u64, MetaError> {
    let older_than = now.saturating_sub(STALE_NS);
    let changed = state
        .meta
        .write(move |conn| {
            Box::pin(async move { interrupt_candidates(conn, older_than, now).await })
        })
        .await?;
    if changed > 0 {
        state.issues_wake();
    }
    Ok(changed)
}

/// The trace id of one IC call (A10, D-IC-17): the first 16 bytes of SHA-256 over
/// `cetra.issue-call.v1`, the owner (a candidate or refit id), `claims` and `call`, both as `u32`
/// big-endian. `call` is 1 for the summary, 2 for the embedding and `3 + index` for a naming call.
/// A refit uses `claims` 0.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn call_trace_id(owner: Uuid, claims: u32, call: u32) -> TraceId {
    let mut hasher = Sha256::new();
    hasher.update(b"cetra.issue-call.v1");
    hasher.update(owner.as_bytes());
    hasher.update(claims.to_be_bytes());
    hasher.update(call.to_be_bytes());
    let digest = hasher.finalize();
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    if bytes == [0u8; 16] {
        bytes[15] = 1;
    }
    // The all-zero case is replaced above, so the only error of `from_bytes` cannot happen.
    TraceId::from_bytes(bytes).expect("a non-zero trace id")
}

#[cfg(test)]
mod tests {
    use cetra_core::ids::{ProjectId, TraceId};
    use uuid::Uuid;

    use super::{REQUEUE_NS, STALE_NS, call_trace_id, janitor_once, run_candidate};
    use crate::issues::triggers::{Place, queued};
    use crate::meta::issues::{
        CandidateRow, SourceKind, SourceRow, claim_candidate, insert_candidate, insert_source,
    };
    use crate::test_support::TestApp;

    fn trace(n: u8) -> TraceId {
        TraceId::from_slice(&[n; 16]).expect("not all zero")
    }

    /// A `score` source of `project`, enabled or not, stored.
    async fn source(app: &TestApp, project: ProjectId, enabled: bool) -> SourceRow {
        let now = app.state.clock.now_ns();
        let row = SourceRow {
            id: Uuid::now_v7(),
            project_id: project,
            name: format!("Source {}", Uuid::now_v7()),
            kind: SourceKind::Score,
            filter: None,
            enabled,
            paused_reason: None,
            paused_at: None,
            created_by: None,
            created_at: now,
            updated_at: now,
            sample_rate: Some(1.0),
            daily_cap: Some(100),
            score_names: vec!["quality".to_owned()],
            feedback_sources: Vec::new(),
            chat_connection_id: None,
            chat_model: None,
            embed_connection_id: None,
            embed_model: None,
            embed_dimension: None,
            floor: None,
            min_issue_size: None,
            budget_usd: None,
            budget_period: None,
            paused_detail: None,
            next_refit_at: None,
        };
        let stored = row.clone();
        app.state
            .meta
            .write(move |conn| Box::pin(async move { insert_source(conn, &row).await }))
            .await
            .expect("source stored");
        stored
    }

    /// A queued candidate of `source` on `trace`, stored.
    async fn candidate(app: &TestApp, source: &SourceRow, trace: TraceId) -> CandidateRow {
        let now = app.state.clock.now_ns();
        let row = queued(
            now,
            source.project_id,
            source.id,
            Place::Trace {
                trace,
                session: None,
            },
            None,
            None,
            None,
        );
        let stored = row.clone();
        app.state
            .meta
            .write(move |conn| Box::pin(async move { insert_candidate(conn, &row).await }))
            .await
            .expect("candidate stored");
        stored
    }

    /// Claims the queued candidate `id` and returns its claim count.
    async fn claim(app: &TestApp, id: Uuid) -> u32 {
        let now = app.state.clock.now_ns();
        app.state
            .meta
            .write(move |conn| Box::pin(async move { claim_candidate(conn, id, now).await }))
            .await
            .expect("claim written")
            .expect("the candidate is queued")
    }

    /// Makes candidate `id` `running` since long before the janitor's `now`, with `interruptions`.
    async fn make_stale(app: &TestApp, id: Uuid, interruptions: i64) {
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    sqlx::query(
                        "UPDATE issue_candidates SET status = 'running', claims = 1, \
                         interruptions = ?, updated_at = 1 WHERE id = ?",
                    )
                    .bind(interruptions)
                    .bind(id.to_string())
                    .execute(&mut *conn)
                    .await?;
                    Ok(())
                })
            })
            .await
            .expect("candidate made stale");
    }

    /// The status, interruptions, load retries and error class of candidate `id`.
    async fn state_of(app: &TestApp, id: Uuid) -> (String, i64, i64, Option<String>) {
        sqlx::query_as(
            "SELECT status, interruptions, load_retries, error_class \
             FROM issue_candidates WHERE id = ?",
        )
        .bind(id.to_string())
        .fetch_one(app.state.meta.reader())
        .await
        .expect("candidate read")
    }

    #[tokio::test]
    async fn the_janitor_interrupts_a_stale_candidate_and_fails_it_at_the_limit() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let source = source(&app, seeded.project.id, true).await;
        let requeued = candidate(&app, &source, trace(1)).await;
        let exhausted = candidate(&app, &source, trace(2)).await;
        make_stale(&app, requeued.id, 0).await;
        make_stale(&app, exhausted.id, 3).await;

        let changed = janitor_once(&app.state, STALE_NS + 10)
            .await
            .expect("janitor runs");
        assert_eq!(changed, 2);

        let (status, interruptions, _, class) = state_of(&app, requeued.id).await;
        assert_eq!(status, "queued");
        assert_eq!(interruptions, 1);
        assert_eq!(class, None);
        let (status, _, _, class) = state_of(&app, exhausted.id).await;
        assert_eq!(status, "failed");
        assert_eq!(class.as_deref(), Some("interrupted"));
    }

    #[tokio::test]
    async fn a_missing_trace_is_retried_then_fails_as_trace_not_found() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let source = source(&app, seeded.project.id, true).await;
        // No spans are stored for the trace, so each load is `NotFound`.
        let row = candidate(&app, &source, trace(3)).await;
        for load_retries in 0..4_u32 {
            let claims = claim(&app, row.id).await;
            let mut attempt = row.clone();
            attempt.load_retries = load_retries;
            run_candidate(&app.state, attempt, claims).await;
        }
        let (status, _, load_retries, class) = state_of(&app, row.id).await;
        assert_eq!(status, "failed");
        assert_eq!(load_retries, 3);
        assert_eq!(class.as_deref(), Some("trace_not_found"));
    }

    #[tokio::test]
    async fn a_disabled_source_requeues_its_candidate_without_an_attempt() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let source = source(&app, seeded.project.id, false).await;
        let row = candidate(&app, &source, trace(4)).await;
        let claims = claim(&app, row.id).await;
        let before = app.state.clock.now_ns();
        run_candidate(&app.state, row.clone(), claims).await;

        let (status, interruptions, load_retries, class) = state_of(&app, row.id).await;
        assert_eq!(status, "queued");
        assert_eq!((interruptions, load_retries), (0, 0));
        assert_eq!(class, None);
        let not_before: i64 =
            sqlx::query_scalar("SELECT not_before FROM issue_candidates WHERE id = ?")
                .bind(row.id.to_string())
                .fetch_one(app.state.meta.reader())
                .await
                .expect("not_before read");
        assert!(not_before >= before.saturating_add(REQUEUE_NS));
    }

    #[test]
    fn call_trace_id_is_deterministic() {
        let owner = Uuid::from_u128(7);
        assert_eq!(call_trace_id(owner, 1, 1), call_trace_id(owner, 1, 1));
    }

    #[test]
    fn call_trace_id_differs_by_owner_claims_and_call() {
        let owner = Uuid::from_u128(7);
        let other = Uuid::from_u128(8);
        let base = call_trace_id(owner, 1, 1);
        assert_ne!(base, call_trace_id(other, 1, 1));
        assert_ne!(base, call_trace_id(owner, 2, 1));
        assert_ne!(base, call_trace_id(owner, 1, 2));
        assert_ne!(call_trace_id(owner, 1, 3), call_trace_id(owner, 1, 4));
    }

    #[test]
    fn call_trace_id_is_never_all_zero() {
        for n in 0..256_u128 {
            let id = call_trace_id(Uuid::from_u128(n), 0, u32::try_from(n).unwrap_or(0));
            assert_ne!(id.as_bytes(), &[0u8; 16]);
        }
    }
}
