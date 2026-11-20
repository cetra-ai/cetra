//! The IC candidate workers (track IC M3, D-IC-12): a loop that claims ready candidates and runs
//! each one on a worker slot, and the startup and janitor interruption of candidates left
//! `running`.

use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::Duration;

use cetra_core::ids::{OrgId, TraceId};
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
/// Members a centroid averages; past this a join no longer moves it (D-IC-7).
pub(crate) const CENTROID_MEMBERS: u64 = 500;
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

/// How a run stops early: with the candidate's finish to write, with that finish and a source pause
/// to write with it, with nothing to write because the claim is no longer current, or with a
/// `meta.db` failure that leaves the candidate `running` for the janitor (A5, A12).
enum Exit {
    End(CandidateFinish),
    Paused(Box<(CandidateFinish, SourcePause)>),
    Lost,
    Meta(MetaError),
}

/// The source pause a run's finish writes with it (M3.6a, D-I-14, D-I-15).
enum SourcePause {
    /// The budget is reached: the source pauses with `budget`.
    Budget,
    /// A connection's pause error: the sources that use `connection` pause with `connection`, and
    /// the connection is `failing`, when it still has the `updated_at` the call read.
    Connection {
        org: OrgId,
        connection: Uuid,
        updated_at: i64,
        detail: String,
    },
}

/// The `budget` pause detail of a source (as a rule's, D-I-15).
pub(crate) const BUDGET_DETAIL: &str = "budget reached";

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
    let (finish, pause) = match steps(state, candidate, claims, &mut spent_usd).await {
        Ok(()) => return Ok(()),
        Err(Exit::End(finish)) => (finish, None),
        Err(Exit::Paused(paused)) => {
            let (finish, pause) = *paused;
            (finish, Some(pause))
        }
        Err(Exit::Lost) => {
            tracing::debug!(candidate_id = %candidate.id, "issue candidate claim is no longer current");
            return Ok(());
        }
        Err(Exit::Meta(error)) => {
            // The run ends without a finish write. The spend it has not stored yet is recorded
            // here: a failed write rolled back its own spend, and a stored finish reset `spent_usd`.
            record_spend(state, candidate.source_id, spent_usd, state.clock.now_ns()).await;
            return Err(error);
        }
    };
    let status = finish_label(&finish);
    let class = match &finish {
        CandidateFinish::Failed { class } | CandidateFinish::Skipped { class } => *class,
        _ => "none",
    };
    let stored = write_end(state, candidate, claims, finish, pause, spent_usd).await?;
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

/// Writes a run's finish, its spend and, when there is one, its source pause, in one write. The
/// pause follows only a finish that stored, as an eval job's does (D-I-14). Returns whether the
/// finish stored.
async fn write_end(
    state: &AppState,
    candidate: &CandidateRow,
    claims: u32,
    finish: CandidateFinish,
    pause: Option<SourcePause>,
    spent_usd: f64,
) -> Result<bool, MetaError> {
    let source = candidate.source_id;
    let id = candidate.id;
    let now = state.clock.now_ns();
    state
        .meta
        .write(move |conn| {
            Box::pin(async move {
                let stored =
                    finish_with_spend(conn, source, id, claims, finish, spent_usd, now).await?;
                if stored && let Some(pause) = pause {
                    apply_pause(conn, source, pause, now).await?;
                }
                Ok(stored)
            })
        })
        .await
}

/// The source pause of a run, in the caller's write (M3.6a).
async fn apply_pause(
    conn: &mut SqliteConnection,
    source: Uuid,
    pause: SourcePause,
    now: i64,
) -> Result<(), MetaError> {
    match pause {
        SourcePause::Budget => {
            issues::pause_source(conn, source, "budget", Some(BUDGET_DETAIL), now).await?;
            tracing::info!(
                target: "audit",
                source_id = %source,
                reason = "budget",
                "issue source paused"
            );
            Ok(())
        }
        SourcePause::Connection {
            org,
            connection,
            updated_at,
            detail,
        } => pause_connection_sources(conn, org, connection, updated_at, &detail, now).await,
    }
}

/// Pauses the sources that use `connection` with reason `connection`, and marks the connection
/// `failing` with `detail`. Nothing changes when the connection no longer has `updated_at`, the
/// one the call read (D-I-14). Runs in the caller's write; shared with the refits.
pub(crate) async fn pause_connection_sources(
    conn: &mut SqliteConnection,
    org: OrgId,
    connection: Uuid,
    updated_at: i64,
    detail: &str,
    now: i64,
) -> Result<(), MetaError> {
    if !execute::connection_unchanged(conn, org, connection, Some(updated_at)).await? {
        return Ok(());
    }
    let paused = issues::pause_sources_of_connection(conn, connection, Some(detail), now).await?;
    connections::set_connection_status(conn, connection, "failing", Some(detail), now).await?;
    tracing::info!(
        target: "audit",
        connection_id = %connection,
        sources = paused.len(),
        "issue sources paused by a connection error"
    );
    Ok(())
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
    #[cfg(test)]
    if faults::take_panic() {
        panic!("the injected panic of a test");
    }
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
    if source.kind != SourceKind::Topic && !filter_matches(&job, &loaded).await? {
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
    // The time hint of the lookup is the trace's own start (M-1). A score's first load has none, so
    // it reads every file of the project (D154 reads only the day around a hint).
    let (trace, span, around) = match candidate.trace_id {
        Some(trace) => (trace, candidate.span_id, candidate.trace_start),
        None => {
            let (trace, start) = resolve_session(job).await?;
            (trace, None, Some(start))
        }
    };
    let loaded = match trace_load::load_trace(
        state,
        project.id,
        Visibility::window(state, project.org_id),
        trace,
        span,
        around,
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
    let root = prompts::root(&loaded.spans).ok_or_else(|| fail("permanent"))?;
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
/// trace. Returns the trace and its start, which is the time hint of its load. No score or no trace
/// in the window drops the candidate.
async fn resolve_session(job: &Job<'_>) -> Result<(TraceId, i64), Exit> {
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
        .map(|trace| (trace.trace_id, trace.start_time))
        .ok_or(Exit::End(CandidateFinish::Dropped))
}

/// Step 4: whether the trace still matches the source's filter (`trace_refresh`, strict
/// exclusions). The read's time hint is the trace's own start, as the load's is (M-1). An invalid
/// filter fails the candidate; a read failure retries the load.
async fn filter_matches(job: &Job<'_>, loaded: &Loaded) -> Result<bool, Exit> {
    let Job {
        state,
        source,
        project,
        candidate,
        ..
    } = *job;
    let now = state.clock.now_ns();
    let ctx = read_context(state, project, loaded.trace_start)
        .await
        .ok_or_else(|| load_retry(candidate, now, "storage"))?;
    let id = loaded.trace.to_string();
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
        Some(estimate) => reserve(state, source, estimate, now)
            .await
            .map_err(|stop| stop.exit(now))?,
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
            return Err(failure(
                candidate,
                attempts,
                state.clock.now_ns(),
                error.class(),
                || SourcePause::Connection {
                    org: project.org_id,
                    connection: chat_id,
                    updated_at: row.updated_at,
                    detail: error.to_string(),
                },
            ));
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
        Some(estimate) => reserve(state, source, estimate, now)
            .await
            .map_err(|stop| stop.exit(now))?,
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
            dimensions: None,
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
        Err(error) => Err(failure(
            candidate,
            attempts,
            state.clock.now_ns(),
            error.class(),
            || SourcePause::Connection {
                org: project.org_id,
                connection: connection_id,
                updated_at: row.updated_at,
                detail: error.to_string(),
            },
        )),
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
    let source_id = source.id;
    let spent = *spent_usd;
    // Step 7: the dimension is fixed by the first embedding of the source (D-IC-5). A refused
    // dimension ends the run with its spend (`write_end`); a failed store is an `Exit::Meta`, whose
    // spend `execute_candidate` records.
    let stored_dimension = store_dimension(state, source_id, dimension).await?;
    if stored_dimension != dimension {
        return Err(fail("embedding_dimension_changed"));
    }
    // Step 8: one write. The finish is fenced first; the join follows only when it stored.
    let floor = source.floor;
    let project_id = project.id;
    let id = candidate.id;
    let trace_start = loaded.trace_start;
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
    // The occurrence of an assigned candidate (A3). A failed write keeps the assignment. An
    // occurrence older than the project's retention is not written, as the error sweep does; the
    // candidate stays assigned.
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
        if errors::too_old(errors::retention_cutoff(project, now), occurrence.at) {
            tracing::debug!(
                candidate_id = %id,
                issue_id = %issue_id,
                "issue occurrence older than the project's retention was not written"
            );
            return Ok(());
        }
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

/// Why a budget reservation does not hold (D-I-15): the budget is spent, the spend of the period
/// is not loaded yet, or `meta.db` failed.
pub(crate) enum BudgetStop {
    Over,
    NotLoaded,
    Meta(MetaError),
}

impl BudgetStop {
    /// The exit of a candidate that could not reserve: a skip that pauses the source when over
    /// budget (M3.6a), a requeue while the spend loads.
    fn exit(self, now: i64) -> Exit {
        match self {
            Self::Over => Exit::Paused(Box::new((
                CandidateFinish::Skipped { class: "budget" },
                SourcePause::Budget,
            ))),
            Self::NotLoaded => Exit::End(CandidateFinish::Requeue { not_before: now }),
            Self::Meta(error) => Exit::Meta(error),
        }
    }
}

/// Reserves `estimate` against the source's budget (D-I-15, as for rules). A source without a
/// budget reserves nothing.
pub(crate) async fn reserve<'a>(
    state: &'a AppState,
    source: &SourceRow,
    estimate: f64,
    now: i64,
) -> Result<Option<Reservation<'a>>, BudgetStop> {
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
        .await
        .map_err(BudgetStop::Meta)?;
        budgets.load(source.id, period_start, spent);
        attempt = budgets.try_reserve(source.id, period_start, limit, estimate);
    }
    match attempt {
        Reserve::Reserved(guard) => Ok(Some(guard)),
        Reserve::Over => Err(BudgetStop::Over),
        Reserve::NotLoaded => Err(BudgetStop::NotLoaded),
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

/// The exit of a failed call (A5, M3.6a): a retry while attempts are left, else a failure by class.
/// A `Pause` requeues the candidate without its attempt (`PauseRequeue`) and pauses the sources of
/// the call's connection, with `pause` built only then.
fn failure(
    candidate: &CandidateRow,
    attempts: u32,
    now: i64,
    class: ErrorClass,
    pause: impl FnOnce() -> SourcePause,
) -> Exit {
    match class {
        ErrorClass::Retryable => Exit::End(retry_or_fail(candidate, attempts, now)),
        ErrorClass::Permanent => Exit::End(CandidateFinish::Failed { class: "permanent" }),
        ErrorClass::Pause => Exit::Paused(Box::new((
            CandidateFinish::PauseRequeue { not_before: now },
            pause(),
        ))),
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

/// Writes a finish with the spend of the run, in its own meta write.
async fn write_finish(
    state: &AppState,
    source: Uuid,
    id: Uuid,
    claims: u32,
    finish: CandidateFinish,
    spent_usd: f64,
) -> Result<bool, MetaError> {
    #[cfg(test)]
    if faults::take_finish_write_failure() {
        return Err(MetaError::from(sqlx::Error::RowNotFound));
    }
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

/// Stores `dimension` as the embedding dimension of `source` unless one is stored, and returns the
/// stored one (D-IC-5), in its own write.
async fn store_dimension(state: &AppState, source: Uuid, dimension: u32) -> Result<u32, MetaError> {
    #[cfg(test)]
    if faults::take_dimension_store_failure() {
        return Err(MetaError::from(sqlx::Error::RowNotFound));
    }
    state
        .meta
        .write(move |conn| {
            Box::pin(async move { issues::set_embed_dimension(conn, source, dimension).await })
        })
        .await
}

/// Adds `spent_usd` to the source's spend of today, for a run that ends without a finish write.
/// A failure is logged: the janitor's next run of the candidate then pays again, which is the
/// lesser error.
async fn record_spend(state: &AppState, source: Uuid, spent_usd: f64, now: i64) {
    if spent_usd <= 0.0 {
        return;
    }
    let written = state
        .meta
        .write(move |conn| {
            Box::pin(async move {
                issues::add_spend(conn, source, budget::day_start(now), spent_usd).await
            })
        })
        .await;
    if let Err(error) = written {
        tracing::error!(source_id = %source, error = %error, "issue spend could not be recorded");
    }
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
    #[allow(clippy::expect_used)]
    TraceId::from_bytes(bytes).expect("a non-zero trace id")
}

/// Test-only faults of the candidate worker. Each is armed by a test on its own thread and fires
/// once; the tests run their candidates on that thread.
///
/// The flags are thread-local, so a test that arms one must run its candidates on the same thread:
/// use the current-thread `#[tokio::test]` runtime, which runs every task on the test's thread.
#[cfg(test)]
mod faults {
    use std::cell::Cell;

    thread_local! {
        static PANIC: Cell<bool> = const { Cell::new(false) };
        static DIMENSION_STORE: Cell<bool> = const { Cell::new(false) };
        static FINISH_WRITE: Cell<bool> = const { Cell::new(false) };
    }

    /// The next job panics, at its start.
    pub(super) fn panic_next_job() {
        PANIC.with(|flag| flag.set(true));
    }

    pub(super) fn take_panic() -> bool {
        PANIC.with(|flag| flag.replace(false))
    }

    /// The next store of an embedding dimension fails.
    pub(super) fn fail_next_dimension_store() {
        DIMENSION_STORE.with(|flag| flag.set(true));
    }

    pub(super) fn take_dimension_store_failure() -> bool {
        DIMENSION_STORE.with(|flag| flag.replace(false))
    }

    /// The next finish write fails.
    pub(super) fn fail_next_finish_write() {
        FINISH_WRITE.with(|flag| flag.set(true));
    }

    pub(super) fn take_finish_write_failure() -> bool {
        FINISH_WRITE.with(|flag| flag.replace(false))
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use axum::http::{Method, header};
    use cetra_core::enums::PayloadFormat;
    use cetra_core::ids::{ProjectId, SpanId, TraceId};
    use cetra_core::score::ScoreValue;
    use cetra_core::span::Span;
    use serde_json::{Value, json};
    use uuid::Uuid;

    use super::{
        REQUEUE_NS, STALE_NS, call_trace_id, faults, janitor_once, run_candidate, run_pass,
    };
    use crate::eval::budget;
    use crate::issues::triggers::{Place, queued};
    use crate::meta::issues::{
        CandidateRow, IssueRow, IssueState, SourceKind, SourceRow, claim_candidate, get_issue,
        insert_candidate, insert_issue, insert_source,
    };
    use crate::meta::projects::{ProjectPatch, ProjectRow, update_project};
    use crate::test_support::mock_llm::{MockLlm, completion, embedding, error as error_reply};
    use crate::test_support::{
        Seeded, TestApp, TestScore, assert_logs_clean, dataset_span, put_score, raw_span_count,
    };

    const QUESTION: &str = "Why was my refund refused?";
    const ANSWER: &str = "Refunds are allowed within 30 days.";
    /// A chat model with a price, so a budget applies (as the eval tests' `PRICED`).
    const PRICED: &str = "gpt-4o-mini";
    /// An embedding model with a built-in price (OpenAI), so an embedding call has a cost.
    const PRICED_EMBEDDING: &str = "text-embedding-3-small";
    const SUMMARY: &str = "Login fails on refresh.";
    const SUMMARY_JSON: &str = r#"{"summary": "Login fails on refresh.", "is_issue": true}"#;

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

    /// An app that may reach a mock on `127.0.0.1`, its clock at the real time, and its seed (as
    /// the eval execute tests do).
    async fn llm_app() -> (TestApp, Seeded) {
        let app = TestApp::with_config(|config| {
            config.security.allow_private_network_egress = true;
        })
        .await;
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

    /// A chat and embedding connection of `acme` that reaches `mock`, created through the API.
    async fn connection(app: &TestApp, seeded: &Seeded, mock: &MockLlm) -> Uuid {
        let response = app
            .request(Method::POST, "/api/v1/orgs/acme/llm-connections")
            .header(header::COOKIE.as_str(), &seeded.cookie)
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

    /// A `score` source that summarizes with `gpt-test` and embeds with `text-embedding-test`,
    /// both through `connection`, with floor 0.9 and no budget. Not stored.
    fn judged_row(project: ProjectId, connection: Uuid, now: i64) -> SourceRow {
        SourceRow {
            id: Uuid::now_v7(),
            project_id: project,
            name: format!("Judged {}", Uuid::now_v7()),
            kind: SourceKind::Score,
            filter: None,
            enabled: true,
            paused_reason: None,
            paused_at: None,
            created_by: None,
            created_at: now,
            updated_at: now,
            sample_rate: Some(1.0),
            daily_cap: Some(100),
            score_names: vec!["quality".to_owned()],
            feedback_sources: Vec::new(),
            chat_connection_id: Some(connection),
            chat_model: Some("gpt-test".to_owned()),
            embed_connection_id: Some(connection),
            embed_model: Some("text-embedding-test".to_owned()),
            embed_dimension: None,
            floor: Some(0.9),
            min_issue_size: Some(2),
            budget_usd: None,
            budget_period: None,
            paused_detail: None,
            next_refit_at: None,
        }
    }

    /// `row`, stored.
    async fn store_source(app: &TestApp, row: SourceRow) -> SourceRow {
        let stored = row.clone();
        app.state
            .meta
            .write(move |conn| Box::pin(async move { insert_source(conn, &row).await }))
            .await
            .expect("source stored");
        stored
    }

    /// An open issue of `source` with `centroid` and `member_count` members, stored.
    async fn existing_issue(
        app: &TestApp,
        project: ProjectId,
        source: Uuid,
        centroid: Vec<f32>,
        member_count: u64,
    ) -> Uuid {
        let now = app.state.clock.now_ns();
        let id = Uuid::now_v7();
        let row = IssueRow {
            id,
            project_id: project,
            source_id: source,
            fingerprint: None,
            title: "Refund questions".to_owned(),
            description: None,
            state: IssueState::Open,
            regressed: false,
            merged_into: None,
            first_seen: now,
            last_seen: now,
            resolved_at: None,
            resolved_by: None,
            created_at: now,
            updated_at: now,
            centroid: Some(centroid),
            member_count,
        };
        app.state
            .meta
            .write(move |conn| Box::pin(async move { insert_issue(conn, &row).await }))
            .await
            .expect("issue stored");
        id
    }

    /// A root span of `on` (a question and its answer) that started half a minute ago, stored.
    async fn store_trace(app: &TestApp, project: &ProjectRow, on: TraceId) {
        let now = app.state.clock.now_ns();
        store_trace_at(app, project, on, now - 30_000_000_000).await;
    }

    /// A root span of `on` (a question and its answer) that started at `start`, stored. Its
    /// session is `session-7` and its user `user-7`.
    async fn store_trace_at(app: &TestApp, project: &ProjectRow, on: TraceId, start: i64) {
        let mut row: Span = dataset_span(0);
        row.trace_id = on;
        row.span_id = SpanId::from_bytes(1_u64.to_be_bytes()).expect("not all zero");
        row.parent_span_id = None;
        row.input = Some(QUESTION.to_owned());
        row.input_format = Some(PayloadFormat::Text);
        row.output = Some(ANSWER.to_owned());
        row.output_format = Some(PayloadFormat::Text);
        row.session_id = Some("session-7".to_owned());
        row.user_id = Some("user-7".to_owned());
        row.start_time = start;
        row.end_time = start + 10_000_000_000;
        app.commit_spans(project, vec![row]).await;
    }

    /// Flushes the stored spans to Parquet, so a lookup's time hint decides which files it reads.
    async fn flushed(app: &TestApp) {
        app.state
            .storage
            .flush_now(Duration::from_secs(10))
            .await
            .expect("storage flushes");
    }

    /// The trace id of candidate `id`, as stored.
    async fn stored_trace_id(app: &TestApp, id: Uuid) -> Option<Vec<u8>> {
        sqlx::query_scalar("SELECT trace_id FROM issue_candidates WHERE id = ?")
            .bind(id.to_string())
            .fetch_one(app.state.meta.reader())
            .await
            .expect("candidate read")
    }

    /// The cost spent today by `source`, as the budget reads it.
    async fn spent_today(app: &TestApp, source: Uuid) -> f64 {
        let now = app.state.clock.now_ns();
        crate::meta::issues::spent_between(
            app.state.meta.reader(),
            source,
            budget::day_start(now),
            budget::day_start(now) + budget::DAY_NS,
        )
        .await
        .expect("spend read")
    }

    /// A stored boolean score `quality` = false, created now. Returns its id.
    async fn bad_score(app: &TestApp, project: ProjectId) -> Uuid {
        let id = Uuid::now_v7();
        put_score(
            app,
            project,
            TestScore {
                name: "quality",
                value: ScoreValue::Boolean(false),
                evaluator_id: None,
                experiment_id: None,
                created_at: app.state.clock.now_ns(),
                deleted: false,
                score_id: Some(id),
                updated_at: None,
            },
        )
        .await;
        id
    }

    /// A queued candidate of `source` for trace `on` and score `score`, stored.
    async fn scored_candidate(
        app: &TestApp,
        source: &SourceRow,
        on: TraceId,
        score: Uuid,
    ) -> CandidateRow {
        let now = app.state.clock.now_ns();
        let row = queued(
            now,
            source.project_id,
            source.id,
            Place::Trace {
                trace: on,
                session: None,
            },
            None,
            None,
            Some(score),
        );
        let stored = row.clone();
        app.state
            .meta
            .write(move |conn| Box::pin(async move { insert_candidate(conn, &row).await }))
            .await
            .expect("candidate stored");
        stored
    }

    /// Claims `row` and runs it once, as the loop does.
    async fn run_once(app: &TestApp, row: &CandidateRow) {
        let claims = claim(app, row.id).await;
        run_candidate(&app.state, row.clone(), claims).await;
    }

    /// The status, error class, issue id, summary and attempts of candidate `id`.
    async fn outcome_of(
        app: &TestApp,
        id: Uuid,
    ) -> (String, Option<String>, Option<String>, Option<String>, i64) {
        sqlx::query_as(
            "SELECT status, error_class, issue_id, summary, attempts \
             FROM issue_candidates WHERE id = ?",
        )
        .bind(id.to_string())
        .fetch_one(app.state.meta.reader())
        .await
        .expect("candidate read")
    }

    /// The embedding dimension stored on `source`.
    async fn stored_dimension(app: &TestApp, source: Uuid) -> Option<i64> {
        let stored: Option<i64> =
            sqlx::query_scalar("SELECT embed_dimension FROM issue_sources WHERE id = ?")
                .bind(source.to_string())
                .fetch_one(app.state.meta.reader())
                .await
                .expect("source read");
        stored
    }

    /// The names of the occurrence scores of `on`, through the scores route.
    async fn occurrence_names(app: &TestApp, seeded: &Seeded, on: TraceId) -> Vec<String> {
        let response = app
            .request(
                Method::GET,
                &format!(
                    "/api/v1/projects/{}/scores?trace_id={on}&source=issue",
                    seeded.project.id
                ),
            )
            .header(header::COOKIE.as_str(), &seeded.cookie)
            .send()
            .await;
        assert_eq!(response.status, 200);
        let page: Value = response.json();
        page["data"]
            .as_array()
            .expect("data")
            .iter()
            .map(|item| item["name"].as_str().expect("a name").to_owned())
            .collect()
    }

    #[tokio::test]
    async fn a_bad_score_is_summarized_embedded_and_left_pending_without_an_issue() {
        let mock = MockLlm::start_with_embeddings(
            vec![completion(SUMMARY_JSON)],
            vec![embedding(&[1.0, 0.0, 0.0])],
        )
        .await;
        let (app, seeded) = llm_app().await;
        let conn = connection(&app, &seeded, &mock).await;
        let now = app.state.clock.now_ns();
        let source = store_source(&app, judged_row(seeded.project.id, conn, now)).await;
        let on_trace = trace(10);
        store_trace(&app, &seeded.project, on_trace).await;
        let score = bad_score(&app, seeded.project.id).await;
        let row = scored_candidate(&app, &source, on_trace, score).await;
        let spans_before = raw_span_count(&app.state.storage, seeded.project.id).await;

        run_once(&app, &row).await;

        let (status, error, issue, summary, _) = outcome_of(&app, row.id).await;
        assert_eq!(status, "pending", "{error:?}");
        assert_eq!(summary.as_deref(), Some(SUMMARY));
        assert_eq!(issue, None);
        assert_eq!(stored_dimension(&app, source.id).await, Some(3));
        let chat = mock.requests();
        assert_eq!(chat.len(), 1);
        assert_eq!(chat[0].1["model"], "gpt-test");
        let embeds = mock.embedding_requests();
        assert_eq!(embeds.len(), 1);
        assert_eq!(embeds[0].1["model"], "text-embedding-test");
        assert!(
            embeds[0].1.get("dimensions").is_none(),
            "the provider's native size is kept: {}",
            embeds[0].1
        );
        assert_eq!(embeds[0].1["input"], SUMMARY);
        // Both call traces are committed to the project.
        assert!(raw_span_count(&app.state.storage, seeded.project.id).await >= spans_before + 2);
    }

    #[tokio::test]
    async fn a_candidate_joins_the_nearest_issue_above_the_floor() {
        let mock = MockLlm::start_with_embeddings(
            vec![completion(SUMMARY_JSON)],
            vec![embedding(&[0.6, 0.8, 0.0])],
        )
        .await;
        let (app, seeded) = llm_app().await;
        let conn = connection(&app, &seeded, &mock).await;
        let now = app.state.clock.now_ns();
        let source = store_source(
            &app,
            SourceRow {
                floor: Some(0.5),
                ..judged_row(seeded.project.id, conn, now)
            },
        )
        .await;
        let issue =
            existing_issue(&app, seeded.project.id, source.id, vec![1.0, 0.0, 0.0], 4).await;
        let on_trace = trace(11);
        store_trace(&app, &seeded.project, on_trace).await;
        let score = bad_score(&app, seeded.project.id).await;
        let row = scored_candidate(&app, &source, on_trace, score).await;

        run_once(&app, &row).await;

        let (status, error, assigned, _, _) = outcome_of(&app, row.id).await;
        assert_eq!(status, "assigned", "{error:?}");
        assert_eq!(assigned, Some(issue.to_string()));
        let joined = get_issue(app.state.meta.reader(), seeded.project.id, issue)
            .await
            .expect("issue read")
            .expect("the issue exists");
        assert_eq!(joined.member_count, 5);
        let centroid = joined.centroid.expect("a centroid");
        assert!(
            centroid[1] > 0.0,
            "the centroid moved toward the new member: {centroid:?}"
        );
        assert_eq!(
            occurrence_names(&app, &seeded, on_trace).await,
            vec![format!("issue:{issue}")]
        );
    }

    #[tokio::test]
    async fn a_candidate_below_the_floor_stays_pending_beside_an_issue() {
        let mock = MockLlm::start_with_embeddings(
            vec![completion(SUMMARY_JSON)],
            vec![embedding(&[0.0, 1.0, 0.0])],
        )
        .await;
        let (app, seeded) = llm_app().await;
        let conn = connection(&app, &seeded, &mock).await;
        let now = app.state.clock.now_ns();
        let source = store_source(
            &app,
            SourceRow {
                floor: Some(0.5),
                ..judged_row(seeded.project.id, conn, now)
            },
        )
        .await;
        let issue =
            existing_issue(&app, seeded.project.id, source.id, vec![1.0, 0.0, 0.0], 4).await;
        let on_trace = trace(12);
        store_trace(&app, &seeded.project, on_trace).await;
        let score = bad_score(&app, seeded.project.id).await;
        let row = scored_candidate(&app, &source, on_trace, score).await;

        run_once(&app, &row).await;

        let (status, error, assigned, _, _) = outcome_of(&app, row.id).await;
        assert_eq!(status, "pending", "{error:?}");
        assert_eq!(assigned, None);
        let untouched = get_issue(app.state.meta.reader(), seeded.project.id, issue)
            .await
            .expect("issue read")
            .expect("the issue exists");
        assert_eq!(untouched.member_count, 4);
        assert!(occurrence_names(&app, &seeded, on_trace).await.is_empty());
    }

    #[tokio::test]
    async fn a_summary_that_is_not_an_issue_is_dropped_before_the_embedding() {
        let mock = MockLlm::start(vec![completion(r#"{"summary": "", "is_issue": false}"#)]).await;
        let (app, seeded) = llm_app().await;
        let conn = connection(&app, &seeded, &mock).await;
        let now = app.state.clock.now_ns();
        let source = store_source(&app, judged_row(seeded.project.id, conn, now)).await;
        let on_trace = trace(13);
        store_trace(&app, &seeded.project, on_trace).await;
        let score = bad_score(&app, seeded.project.id).await;
        let row = scored_candidate(&app, &source, on_trace, score).await;

        run_once(&app, &row).await;

        let (status, error, _, _, _) = outcome_of(&app, row.id).await;
        assert_eq!(status, "dropped", "{error:?}");
        assert_eq!(mock.requests().len(), 1);
        assert!(mock.embedding_requests().is_empty());
    }

    #[tokio::test]
    async fn a_score_whose_trace_the_source_filter_does_not_match_is_dropped_before_the_summary() {
        let mock = MockLlm::start(vec![completion(SUMMARY_JSON)]).await;
        let (app, seeded) = llm_app().await;
        let conn = connection(&app, &seeded, &mock).await;
        let now = app.state.clock.now_ns();
        let mut judged = judged_row(seeded.project.id, conn, now);
        // The stored trace is in `session-7`, so a filter on another session does not match it.
        judged.filter = Some("session_id = 'session-8'".to_owned());
        let source = store_source(&app, judged).await;
        let on_trace = trace(14);
        store_trace(&app, &seeded.project, on_trace).await;
        let score = bad_score(&app, seeded.project.id).await;
        let row = scored_candidate(&app, &source, on_trace, score).await;

        run_once(&app, &row).await;

        let (status, error, _, _, _) = outcome_of(&app, row.id).await;
        assert_eq!(status, "dropped", "{error:?}");
        assert!(mock.requests().is_empty(), "no summary is asked");
        assert!(mock.embedding_requests().is_empty());
    }

    /// A `status` on the summary call is retried; the second attempt stores the summary and the
    /// candidate is pending.
    async fn retried_then_pending(status: u16) {
        let mock = MockLlm::start_with_embeddings(
            vec![error_reply(status, "try again"), completion(SUMMARY_JSON)],
            vec![embedding(&[1.0, 0.0, 0.0])],
        )
        .await;
        let (app, seeded) = llm_app().await;
        let conn = connection(&app, &seeded, &mock).await;
        let now = app.state.clock.now_ns();
        let source = store_source(&app, judged_row(seeded.project.id, conn, now)).await;
        let on_trace = trace(14);
        store_trace(&app, &seeded.project, on_trace).await;
        let score = bad_score(&app, seeded.project.id).await;
        let row = scored_candidate(&app, &source, on_trace, score).await;

        run_once(&app, &row).await;
        let (state_after, error, _, _, attempts) = outcome_of(&app, row.id).await;
        assert_eq!(state_after, "queued", "{error:?}");
        assert_eq!(attempts, 1);

        run_once(&app, &row).await;
        let (state_after, error, _, summary, _) = outcome_of(&app, row.id).await;
        assert_eq!(state_after, "pending", "{error:?}");
        assert_eq!(summary.as_deref(), Some(SUMMARY));
        assert_eq!(mock.requests().len(), 2);
    }

    #[tokio::test]
    async fn a_server_error_on_the_summary_is_retried_then_stored() {
        retried_then_pending(500).await;
    }

    #[tokio::test]
    async fn a_rate_limit_on_the_summary_is_retried_then_stored() {
        retried_then_pending(429).await;
    }

    #[tokio::test]
    async fn an_unauthorized_key_requeues_the_candidate_and_pauses_the_source() {
        let mock = MockLlm::start(vec![error_reply(401, "bad key")]).await;
        let (app, seeded) = llm_app().await;
        let conn = connection(&app, &seeded, &mock).await;
        let now = app.state.clock.now_ns();
        let source = store_source(&app, judged_row(seeded.project.id, conn, now)).await;
        let on_trace = trace(15);
        store_trace(&app, &seeded.project, on_trace).await;
        let score = bad_score(&app, seeded.project.id).await;
        let row = scored_candidate(&app, &source, on_trace, score).await;

        run_once(&app, &row).await;

        // A pause requeues the candidate without the attempt it made (A5).
        let (status, error_class, _, _, attempts) = outcome_of(&app, row.id).await;
        assert_eq!(status, "queued");
        assert_eq!(error_class, None);
        assert_eq!(attempts, 0);
        let stored =
            crate::meta::issues::get_source(app.state.meta.reader(), seeded.project.id, source.id)
                .await
                .expect("source read")
                .expect("source");
        assert_eq!(stored.paused_reason.as_deref(), Some("connection"));
        assert!(stored.paused_detail.is_some());
        let connection_status: String =
            sqlx::query_scalar("SELECT status FROM llm_connections WHERE id = ?")
                .bind(conn.to_string())
                .fetch_one(app.state.meta.reader())
                .await
                .expect("connection read");
        assert_eq!(connection_status, "failing");
        assert_eq!(mock.requests().len(), 1);
        assert!(mock.embedding_requests().is_empty());
    }

    #[tokio::test]
    async fn a_run_over_the_budget_is_skipped_and_pauses_the_source() {
        let mock = MockLlm::start(vec![completion(SUMMARY_JSON)]).await;
        let (app, seeded) = llm_app().await;
        let conn = connection(&app, &seeded, &mock).await;
        let now = app.state.clock.now_ns();
        let source = store_source(
            &app,
            SourceRow {
                chat_model: Some(PRICED.to_owned()),
                budget_usd: Some(0.0000001),
                budget_period: Some("day".to_owned()),
                ..judged_row(seeded.project.id, conn, now)
            },
        )
        .await;
        let on_trace = trace(16);
        store_trace(&app, &seeded.project, on_trace).await;
        let score = bad_score(&app, seeded.project.id).await;
        let row = scored_candidate(&app, &source, on_trace, score).await;

        run_once(&app, &row).await;

        let (status, error_class, _, _, _) = outcome_of(&app, row.id).await;
        assert_eq!(status, "skipped");
        assert_eq!(error_class.as_deref(), Some("budget"));
        assert!(mock.requests().is_empty());
        let stored =
            crate::meta::issues::get_source(app.state.meta.reader(), seeded.project.id, source.id)
                .await
                .expect("source read")
                .expect("source");
        assert_eq!(stored.paused_reason.as_deref(), Some("budget"));
    }

    #[tokio::test]
    async fn a_candidate_on_a_saturated_connection_is_not_claimed() {
        let mock = MockLlm::start(vec![completion(SUMMARY_JSON)]).await;
        let (app, seeded) = llm_app().await;
        let conn = connection(&app, &seeded, &mock).await;
        let now = app.state.clock.now_ns();
        let source = store_source(&app, judged_row(seeded.project.id, conn, now)).await;
        let on_trace = trace(17);
        store_trace(&app, &seeded.project, on_trace).await;
        let score = bad_score(&app, seeded.project.id).await;
        let row = scored_candidate(&app, &source, on_trace, score).await;
        // Takes the connection's only slot, so the connection is saturated for the pass.
        let _slot = app
            .state
            .eval
            .try_connection_permit(conn, (1, 1000), now)
            .expect("the slot is free");

        let started = run_pass(&app.state).await.expect("the pass runs");

        assert_eq!(started, 0);
        let (status, _, _, _, attempts) = outcome_of(&app, row.id).await;
        assert_eq!(status, "queued");
        assert_eq!(attempts, 0);
        assert!(mock.requests().is_empty());
    }

    #[tokio::test]
    async fn an_embedding_failure_after_the_summary_retries_without_a_second_summary() {
        let mock = MockLlm::start_with_embeddings(
            vec![completion(SUMMARY_JSON)],
            vec![error_reply(500, "busy"), embedding(&[1.0, 0.0, 0.0])],
        )
        .await;
        let (app, seeded) = llm_app().await;
        let conn = connection(&app, &seeded, &mock).await;
        let now = app.state.clock.now_ns();
        let source = store_source(&app, judged_row(seeded.project.id, conn, now)).await;
        let on_trace = trace(18);
        store_trace(&app, &seeded.project, on_trace).await;
        let score = bad_score(&app, seeded.project.id).await;
        let row = scored_candidate(&app, &source, on_trace, score).await;

        run_once(&app, &row).await;
        let (state_after, error, _, summary, _) = outcome_of(&app, row.id).await;
        assert_eq!(state_after, "queued", "{error:?}");
        assert_eq!(summary.as_deref(), Some(SUMMARY));

        // The loop reads the row again with its stored summary, so the summary is not asked again.
        let mut again = row.clone();
        again.summary = summary;
        run_once(&app, &again).await;
        let (state_after, error, _, _, _) = outcome_of(&app, row.id).await;
        assert_eq!(state_after, "pending", "{error:?}");
        assert_eq!(mock.requests().len(), 1);
        assert_eq!(mock.embedding_requests().len(), 2);
    }

    #[tokio::test]
    async fn a_changed_embedding_dimension_fails_the_candidate() {
        let mock = MockLlm::start_with_embeddings(
            vec![completion(SUMMARY_JSON)],
            vec![embedding(&[1.0, 0.0, 0.0])],
        )
        .await;
        let (app, seeded) = llm_app().await;
        let conn = connection(&app, &seeded, &mock).await;
        let now = app.state.clock.now_ns();
        let source = store_source(
            &app,
            SourceRow {
                embed_dimension: Some(2),
                ..judged_row(seeded.project.id, conn, now)
            },
        )
        .await;
        let on_trace = trace(19);
        store_trace(&app, &seeded.project, on_trace).await;
        let score = bad_score(&app, seeded.project.id).await;
        let row = scored_candidate(&app, &source, on_trace, score).await;

        run_once(&app, &row).await;

        let (status, error_class, _, _, _) = outcome_of(&app, row.id).await;
        assert_eq!(status, "failed");
        assert_eq!(error_class.as_deref(), Some("embedding_dimension_changed"));
        assert_eq!(stored_dimension(&app, source.id).await, Some(2));
    }

    #[tokio::test]
    async fn a_refused_dimension_still_records_the_embedding_cost() {
        let mock = MockLlm::start_with_embeddings(
            vec![completion(SUMMARY_JSON)],
            vec![embedding(&[1.0, 0.0, 0.0])],
        )
        .await;
        let (app, seeded) = llm_app().await;
        let conn = connection(&app, &seeded, &mock).await;
        let now = app.state.clock.now_ns();
        let source = store_source(
            &app,
            SourceRow {
                embed_model: Some(PRICED_EMBEDDING.to_owned()),
                embed_dimension: Some(2),
                ..judged_row(seeded.project.id, conn, now)
            },
        )
        .await;
        let on_trace = trace(32);
        store_trace(&app, &seeded.project, on_trace).await;
        let score = bad_score(&app, seeded.project.id).await;
        let row = scored_candidate(&app, &source, on_trace, score).await;

        run_once(&app, &row).await;

        let (status, error_class, _, _, _) = outcome_of(&app, row.id).await;
        assert_eq!(status, "failed");
        assert_eq!(error_class.as_deref(), Some("embedding_dimension_changed"));
        assert!(
            spent_today(&app, source.id).await > 0.0,
            "the priced embedding is spent"
        );
    }

    #[tokio::test]
    async fn an_embedding_whose_dimension_cannot_be_stored_still_records_its_cost() {
        let mock = MockLlm::start_with_embeddings(
            vec![completion(SUMMARY_JSON)],
            vec![embedding(&[1.0, 0.0, 0.0])],
        )
        .await;
        let (app, seeded) = llm_app().await;
        let conn = connection(&app, &seeded, &mock).await;
        let now = app.state.clock.now_ns();
        let source = store_source(
            &app,
            SourceRow {
                embed_model: Some(PRICED_EMBEDDING.to_owned()),
                ..judged_row(seeded.project.id, conn, now)
            },
        )
        .await;
        let on_trace = trace(33);
        store_trace(&app, &seeded.project, on_trace).await;
        let score = bad_score(&app, seeded.project.id).await;
        let row = scored_candidate(&app, &source, on_trace, score).await;
        faults::fail_next_dimension_store();

        run_once(&app, &row).await;

        // The run ends without a finish: the candidate stays `running` for the janitor.
        let (status, _, _, _, _) = outcome_of(&app, row.id).await;
        assert_eq!(status, "running");
        assert_eq!(mock.embedding_requests().len(), 1);
        assert_eq!(stored_dimension(&app, source.id).await, None);
        assert!(
            spent_today(&app, source.id).await > 0.0,
            "the paid embedding is spent"
        );
    }

    #[tokio::test]
    async fn a_summary_whose_finish_cannot_be_stored_still_records_its_cost() {
        let mock = MockLlm::start(vec![completion(SUMMARY_JSON)]).await;
        let (app, seeded) = llm_app().await;
        let conn = connection(&app, &seeded, &mock).await;
        let now = app.state.clock.now_ns();
        let source = store_source(
            &app,
            SourceRow {
                chat_model: Some(PRICED.to_owned()),
                budget_usd: Some(100.0),
                budget_period: Some("day".to_owned()),
                ..judged_row(seeded.project.id, conn, now)
            },
        )
        .await;
        let on_trace = trace(34);
        store_trace(&app, &seeded.project, on_trace).await;
        let score = bad_score(&app, seeded.project.id).await;
        let row = scored_candidate(&app, &source, on_trace, score).await;
        faults::fail_next_finish_write();

        run_once(&app, &row).await;

        // The summary's finish is not stored, so the run ends without one: the candidate stays
        // `running` for the janitor, and the paid summary is still spent.
        let (status, _, _, _, _) = outcome_of(&app, row.id).await;
        assert_eq!(status, "running");
        assert_eq!(mock.requests().len(), 1);
        assert!(
            spent_today(&app, source.id).await > 0.0,
            "the paid summary is spent"
        );
    }

    #[tokio::test]
    async fn a_topic_candidate_is_summarized_embedded_and_assigned() {
        let mock = MockLlm::start_with_embeddings(
            vec![completion(SUMMARY_JSON)],
            vec![embedding(&[1.0, 0.0, 0.0])],
        )
        .await;
        let (app, seeded) = llm_app().await;
        let conn = connection(&app, &seeded, &mock).await;
        let now = app.state.clock.now_ns();
        let source = store_source(
            &app,
            SourceRow {
                kind: SourceKind::Topic,
                score_names: Vec::new(),
                ..judged_row(seeded.project.id, conn, now)
            },
        )
        .await;
        let issue =
            existing_issue(&app, seeded.project.id, source.id, vec![1.0, 0.0, 0.0], 4).await;
        let on_trace = trace(34);
        store_trace(&app, &seeded.project, on_trace).await;
        let row = queued(
            now,
            seeded.project.id,
            source.id,
            Place::Trace {
                trace: on_trace,
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

        run_once(&app, &stored).await;

        let (status, error, assigned, summary, _) = outcome_of(&app, stored.id).await;
        assert_eq!(status, "assigned", "{error:?}");
        assert_eq!(assigned, Some(issue.to_string()));
        assert_eq!(summary.as_deref(), Some(SUMMARY));
        assert_eq!(mock.requests().len(), 1);
        assert_eq!(mock.embedding_requests().len(), 1);
        assert_eq!(
            occurrence_names(&app, &seeded, on_trace).await,
            vec![format!("issue:{issue}")]
        );
    }

    #[tokio::test]
    async fn a_session_candidate_resolves_to_its_latest_trace_however_old() {
        let mock = MockLlm::start_with_embeddings(
            vec![completion(SUMMARY_JSON)],
            vec![embedding(&[1.0, 0.0, 0.0])],
        )
        .await;
        let (app, seeded) = llm_app().await;
        let conn = connection(&app, &seeded, &mock).await;
        let now = app.state.clock.now_ns();
        let source = store_source(&app, judged_row(seeded.project.id, conn, now)).await;
        // The session's only trace started five days before the score, in a file of its own day.
        let on_trace = trace(35);
        store_trace_at(&app, &seeded.project, on_trace, now - 5 * budget::DAY_NS).await;
        flushed(&app).await;
        let score = bad_score(&app, seeded.project.id).await;
        let row = queued(
            now,
            seeded.project.id,
            source.id,
            Place::Session("session-7".to_owned()),
            None,
            None,
            Some(score),
        );
        let stored = row.clone();
        app.state
            .meta
            .write(move |conn| Box::pin(async move { insert_candidate(conn, &row).await }))
            .await
            .expect("candidate stored");

        run_once(&app, &stored).await;

        let (status, error, _, summary, _) = outcome_of(&app, stored.id).await;
        assert_eq!(status, "pending", "{error:?}");
        assert_eq!(summary.as_deref(), Some(SUMMARY));
        assert_eq!(
            stored_trace_id(&app, stored.id).await,
            Some(on_trace.as_bytes().to_vec())
        );
    }

    #[tokio::test]
    async fn a_score_written_days_after_its_trace_loads_the_trace() {
        let mock = MockLlm::start_with_embeddings(
            vec![completion(SUMMARY_JSON)],
            vec![embedding(&[1.0, 0.0, 0.0])],
        )
        .await;
        let (app, seeded) = llm_app().await;
        let conn = connection(&app, &seeded, &mock).await;
        let now = app.state.clock.now_ns();
        let source = store_source(&app, judged_row(seeded.project.id, conn, now)).await;
        // The trace started three days before the score, in a file of its own day.
        let on_trace = trace(36);
        store_trace_at(&app, &seeded.project, on_trace, now - 3 * budget::DAY_NS).await;
        flushed(&app).await;
        let score = bad_score(&app, seeded.project.id).await;
        let row = scored_candidate(&app, &source, on_trace, score).await;

        run_once(&app, &row).await;

        let (status, error, _, summary, _) = outcome_of(&app, row.id).await;
        assert_eq!(status, "pending", "{error:?}");
        assert_eq!(summary.as_deref(), Some(SUMMARY));
    }

    #[tokio::test]
    async fn an_occurrence_older_than_the_project_retention_is_not_written() {
        let mock = MockLlm::start_with_embeddings(
            vec![completion(SUMMARY_JSON)],
            vec![embedding(&[1.0, 0.0, 0.0])],
        )
        .await;
        let (app, seeded) = llm_app().await;
        let conn = connection(&app, &seeded, &mock).await;
        let now = app.state.clock.now_ns();
        let source = store_source(&app, judged_row(seeded.project.id, conn, now)).await;
        let issue =
            existing_issue(&app, seeded.project.id, source.id, vec![1.0, 0.0, 0.0], 4).await;
        let project_id = seeded.project.id;
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    update_project(
                        conn,
                        project_id,
                        ProjectPatch {
                            name: None,
                            retention_days: Some(Some(1)),
                            trace_completion_timeout_s: None,
                        },
                        now,
                    )
                    .await
                })
            })
            .await
            .expect("retention set");
        // The trace started three days ago, so its occurrence is older than the one-day retention.
        let on_trace = trace(37);
        store_trace_at(&app, &seeded.project, on_trace, now - 3 * budget::DAY_NS).await;
        let score = bad_score(&app, seeded.project.id).await;
        let row = scored_candidate(&app, &source, on_trace, score).await;

        // Without the retention check the write is attempted and refused, which logs a warning.
        assert_logs_clean(
            &["issue occurrences could not be written"],
            Box::pin(run_once(&app, &row)),
        )
        .await;

        let (status, error, assigned, _, _) = outcome_of(&app, row.id).await;
        assert_eq!(status, "assigned", "{error:?}");
        assert_eq!(assigned, Some(issue.to_string()));
        assert!(occurrence_names(&app, &seeded, on_trace).await.is_empty());
    }

    #[tokio::test]
    async fn a_candidate_whose_user_a_deletion_blocks_is_skipped_after_its_load() {
        let mock = MockLlm::start(vec![completion(SUMMARY_JSON)]).await;
        let (app, seeded) = llm_app().await;
        let conn = connection(&app, &seeded, &mock).await;
        let now = app.state.clock.now_ns();
        let source = store_source(&app, judged_row(seeded.project.id, conn, now)).await;
        let on_trace = trace(38);
        store_trace(&app, &seeded.project, on_trace).await;
        let score = bad_score(&app, seeded.project.id).await;
        let row = scored_candidate(&app, &source, on_trace, score).await;
        // D-M-6, check A: the deletion names the trace's user, which only the loaded trace reveals.
        app.state
            .deletions
            .guard
            .add_user(seeded.project.id, "user-7", Uuid::now_v7(), now);

        run_once(&app, &row).await;

        let (status, error_class, _, _, _) = outcome_of(&app, row.id).await;
        assert_eq!(status, "skipped");
        assert_eq!(error_class.as_deref(), Some("deleted"));
        assert!(mock.requests().is_empty(), "no summary is asked");
        assert!(occurrence_names(&app, &seeded, on_trace).await.is_empty());
    }

    #[tokio::test]
    async fn a_panicking_job_fails_its_candidate_as_internal_and_the_worker_goes_on() {
        let mock = MockLlm::start_with_embeddings(
            vec![completion(SUMMARY_JSON)],
            vec![embedding(&[1.0, 0.0, 0.0])],
        )
        .await;
        let (app, seeded) = llm_app().await;
        let conn = connection(&app, &seeded, &mock).await;
        let now = app.state.clock.now_ns();
        let source = store_source(&app, judged_row(seeded.project.id, conn, now)).await;
        let on_trace = trace(39);
        store_trace(&app, &seeded.project, on_trace).await;
        let score = bad_score(&app, seeded.project.id).await;
        let row = scored_candidate(&app, &source, on_trace, score).await;
        faults::panic_next_job();

        run_once(&app, &row).await;

        let (status, error_class, _, _, _) = outcome_of(&app, row.id).await;
        assert_eq!(status, "failed");
        assert_eq!(error_class.as_deref(), Some("internal"));
        // The loop goes on: the next candidate runs to its end.
        let next_trace = trace(40);
        store_trace(&app, &seeded.project, next_trace).await;
        let next_score = bad_score(&app, seeded.project.id).await;
        let next = scored_candidate(&app, &source, next_trace, next_score).await;
        run_once(&app, &next).await;
        let (status, error, _, summary, _) = outcome_of(&app, next.id).await;
        assert_eq!(status, "pending", "{error:?}");
        assert_eq!(summary.as_deref(), Some(SUMMARY));
    }
}
