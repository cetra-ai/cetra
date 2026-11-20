//! The eval dispatcher (D-I-6): claims queued jobs within the worker, connection, rate and
//! webhook limits and runs each in its own task. The janitor, the pruner and the budget ticker
//! run their passes here too (D-I-12, D-I-15, D-I-28).

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::panic::AssertUnwindSafe;
use std::sync::{Arc, Mutex};

use cetra_core::ids::{OrgId, ProjectId};
use futures::FutureExt;
use sqlx::SqliteConnection;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinSet;
use uuid::Uuid;

use super::execute::{self, ClaimedJob};
use super::{EvalQueue, budget, lock};
use crate::api::evaluators::EvaluatorKind;
use crate::entitlements::Visibility;
use crate::http::AppState;
use crate::meta::MetaError;
use crate::meta::eval_jobs::{self, Candidate, Finish, JobUsage, PruneReport};
use crate::meta::issues;
use crate::meta::online_rules;
use crate::meta::projects;

/// Webhook evaluators run at most this many jobs at once (D-I-6).
pub(crate) const WEBHOOK_SLOTS: usize = 4;
/// Most candidates one pass reads; a pass reads `min(CANDIDATES, CANDIDATES_PER_SLOT × free workers)`.
const CANDIDATES: i64 = 400;
/// Candidates read per free worker slot: some candidates are skipped by the pick (a connection
/// that runs out of slots or tokens during the pass).
const CANDIDATES_PER_SLOT: i64 = 4;
/// A job running longer than this goes back to the queue (D-I-12).
const STALE_NS: i64 = 600_000_000_000;
const NANOS_PER_MINUTE: f64 = 60_000_000_000.0;

/// A connection's slots and token bucket (D-I-6).
#[derive(Debug)]
pub(crate) struct ConnectionLimit {
    /// `(max_concurrency, requests_per_minute)` it was built for.
    limits: (u32, u32),
    slots: Arc<Semaphore>,
    bucket: Mutex<TokenBucket>,
}

impl ConnectionLimit {
    fn new(limits: (u32, u32), now: i64) -> Self {
        Self {
            limits,
            slots: Arc::new(Semaphore::new(
                usize::try_from(limits.0).unwrap_or(usize::MAX),
            )),
            bucket: Mutex::new(TokenBucket::new(limits.1, now)),
        }
    }

    /// The connection's concurrency slots. Interactive calls wait on them (D-V-35).
    pub(super) fn slots(&self) -> &Arc<Semaphore> {
        &self.slots
    }

    /// Refills the bucket for the time since its last use, then takes one token when there is one.
    pub(super) fn take_token(&self, now: i64) -> bool {
        lock(&self.bucket).try_take(now)
    }
}

/// The shared limits of `id` (D-I-4, D-I-6, D-V-35): created on first use, and rebuilt when
/// `limits` (`(max_concurrency, requests_per_minute)`) changed. Running holders keep their old
/// permits.
pub(crate) fn connection_limit(
    eval: &EvalQueue,
    id: Uuid,
    limits: (u32, u32),
    now: i64,
) -> Arc<ConnectionLimit> {
    let mut map = lock(&eval.connections);
    match map.get(&id) {
        Some(existing) if existing.limits == limits => Arc::clone(existing),
        _ => {
            let fresh = Arc::new(ConnectionLimit::new(limits, now));
            map.insert(id, Arc::clone(&fresh));
            fresh
        }
    }
}

/// `capacity = max(1, rpm / 6)` tokens (10 s of burst), refilled at `rpm / 60` per second.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TokenBucket {
    capacity: f64,
    tokens: f64,
    requests_per_minute: f64,
    last: i64,
}

impl TokenBucket {
    /// A full bucket at `now`.
    pub(crate) fn new(requests_per_minute: u32, now: i64) -> Self {
        let capacity = f64::from((requests_per_minute / 6).max(1));
        Self {
            capacity,
            tokens: capacity,
            requests_per_minute: f64::from(requests_per_minute),
            last: now,
        }
    }

    fn refill(&mut self, now: i64) {
        let elapsed = now.saturating_sub(self.last).max(0);
        self.tokens = (self.tokens + elapsed as f64 * self.requests_per_minute / NANOS_PER_MINUTE)
            .min(self.capacity);
        self.last = self.last.max(now);
    }

    /// Refills for the time since `last`, then takes one token when there is one.
    pub(crate) fn try_take(&mut self, now: i64) -> bool {
        self.refill(now);
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }

    /// Whether a token is there now, without taking it.
    pub(crate) fn has_token(&mut self, now: i64) -> bool {
        self.refill(now);
        self.tokens >= 1.0
    }
}

/// The permits a running job holds. Dropping it frees them.
#[derive(Debug)]
pub(crate) struct Permits {
    _worker: OwnedSemaphorePermit,
    _slot: Option<OwnedSemaphorePermit>,
}

/// What `pick` reads: the worker slots, the connection and webhook limits of the candidates, and
/// where the rotation starts for each priority (D-I-6).
#[derive(Debug)]
struct PickLimits {
    workers: Arc<Semaphore>,
    connections: HashMap<Uuid, Arc<ConnectionLimit>>,
    webhooks: HashMap<Uuid, Arc<Semaphore>>,
    last_project: HashMap<u8, ProjectId>,
}

/// One dispatch pass: reads candidates, claims what the limits allow, and spawns each job into
/// `running`. Returns how many jobs it started.
///
/// # Errors
///
/// Returns [`MetaError`] when the candidates cannot be read or the claims cannot be written.
pub(crate) async fn dispatch_once(
    state: &AppState,
    now: i64,
    running: &mut JoinSet<()>,
) -> Result<usize, MetaError> {
    let free = state.eval.workers.available_permits();
    if free == 0 {
        return Ok(0);
    }
    let saturated = saturated_ids(state, now);
    let read = i64::try_from(free)
        .unwrap_or(i64::MAX)
        .saturating_mul(CANDIDATES_PER_SLOT)
        .min(CANDIDATES);
    let candidates = eval_jobs::candidates(state.meta.reader(), now, &saturated, read).await?;
    if candidates.is_empty() {
        return Ok(0);
    }
    let limits = pick_limits(state, &candidates, now);
    let picked = pick(candidates, &limits, now);
    if picked.is_empty() {
        return Ok(0);
    }
    let ids: Vec<Uuid> = picked
        .iter()
        .map(|(candidate, _)| candidate.job.id)
        .collect();
    let claimed = state
        .meta
        .write(move |conn| Box::pin(async move { claim_all(conn, ids, now).await }))
        .await?;

    let mut started = 0;
    let mut last = HashMap::new();
    for ((candidate, permits), claim) in picked.into_iter().zip(claimed) {
        // A `None` claim: the job was claimed or removed since the read. Its permits drop here.
        let Some(claims) = claim else { continue };
        last.insert(candidate.job.priority, candidate.job.project_id);
        let connection_updated_at = candidate
            .connection_limits
            .map(|(_, _, updated_at)| updated_at);
        let job = ClaimedJob {
            row: candidate.job,
            claims,
            connection_updated_at,
        };
        running.spawn(run_claimed(state.clone(), job, permits));
        started += 1;
    }
    lock(&state.eval.last_project).extend(last);
    Ok(started)
}

impl EvalQueue {
    /// A slot and a token of connection `id` for an IC call (D-IC-12), sharing the judges' limits.
    /// `None` when no slot or no token is free now; the slot is taken before the token, as `admit`
    /// does, so a token is never spent without a slot.
    pub(crate) fn try_connection_permit(
        &self,
        id: Uuid,
        limits: (u32, u32),
        now: i64,
    ) -> Option<OwnedSemaphorePermit> {
        let limit = connection_limit(self, id, limits, now);
        let slot = Arc::clone(limit.slots()).try_acquire_owned().ok()?;
        if limit.take_token(now) {
            Some(slot)
        } else {
            None
        }
    }

    /// Connections with no free slot or token now (what `saturated_ids` lists, without webhooks).
    pub(crate) fn saturated_connections(&self, now: i64) -> Vec<Uuid> {
        lock(&self.connections)
            .iter()
            .filter(|(_, limit)| {
                limit.slots.available_permits() == 0 || !lock(&limit.bucket).has_token(now)
            })
            .map(|(id, _)| *id)
            .collect()
    }
}

/// Connections whose slots or bucket are used up, and webhook evaluators at their slots.
fn saturated_ids(state: &AppState, now: i64) -> Vec<Uuid> {
    let mut ids: Vec<Uuid> = state.eval.saturated_connections(now);
    ids.extend(
        lock(&state.eval.webhooks)
            .iter()
            .filter(|(_, slots)| slots.available_permits() == 0)
            .map(|(id, _)| *id),
    );
    ids
}

/// The limits of the candidates' connections and webhook evaluators, created when missing.
/// A connection whose `max_concurrency` or `requests_per_minute` changed gets new ones; running
/// jobs keep the old permits until they finish (D-I-6).
fn pick_limits(state: &AppState, candidates: &[Candidate], now: i64) -> PickLimits {
    let mut connections = HashMap::new();
    let mut webhooks = HashMap::new();
    for candidate in candidates {
        let (Some(id), Some((max_concurrency, requests_per_minute, _))) =
            (candidate.connection_id, candidate.connection_limits)
        else {
            continue;
        };
        let limit = connection_limit(&state.eval, id, (max_concurrency, requests_per_minute), now);
        connections.insert(id, limit);
    }
    {
        let mut map = lock(&state.eval.webhooks);
        for candidate in candidates {
            if EvaluatorKind::from_name(&candidate.evaluator_kind) == Some(EvaluatorKind::Webhook) {
                let id = candidate.job.evaluator_id;
                let slots = map
                    .entry(id)
                    .or_insert_with(|| Arc::new(Semaphore::new(WEBHOOK_SLOTS)))
                    .clone();
                webhooks.insert(id, slots);
            }
        }
    }
    PickLimits {
        workers: Arc::clone(&state.eval.workers),
        connections,
        webhooks,
        last_project: lock(&state.eval.last_project).clone(),
    }
}

/// Picks the candidates the limits allow, in order: strict priority, then round-robin across
/// projects, starting after the project claimed last at that priority (D-I-6). Synchronous, so
/// nothing else runs between the reads of the limits and the permits taken here.
fn pick(candidates: Vec<Candidate>, limits: &PickLimits, now: i64) -> Vec<(Candidate, Permits)> {
    let mut by_priority: BTreeMap<u8, BTreeMap<ProjectId, VecDeque<Candidate>>> = BTreeMap::new();
    for candidate in candidates {
        by_priority
            .entry(candidate.job.priority)
            .or_default()
            .entry(candidate.job.project_id)
            .or_default()
            .push_back(candidate);
    }
    let mut picked = Vec::new();
    let mut blocked: HashSet<Uuid> = HashSet::new();
    for (priority, mut projects) in by_priority {
        let keys: Vec<ProjectId> = projects.keys().copied().collect();
        let start = limits
            .last_project
            .get(&priority)
            .map_or(0, |last| keys.partition_point(|key| key <= last));
        let order: Vec<ProjectId> = keys[start..]
            .iter()
            .chain(&keys[..start])
            .copied()
            .collect();
        loop {
            let mut progressed = false;
            for project in &order {
                let Some(queue) = projects.get_mut(project) else {
                    continue;
                };
                while let Some(candidate) = queue.pop_front() {
                    if limits.workers.available_permits() == 0 {
                        return picked;
                    }
                    if let Some(permits) = admit(&candidate, limits, &mut blocked, now) {
                        picked.push((candidate, permits));
                        progressed = true;
                        break;
                    }
                }
            }
            if !progressed {
                break;
            }
        }
    }
    picked
}

/// Takes the permits of one candidate, or `None`. A connection or webhook that has no permit or
/// token now is blocked for the rest of the pass, so no later candidate of it takes a token.
fn admit(
    candidate: &Candidate,
    limits: &PickLimits,
    blocked: &mut HashSet<Uuid>,
    now: i64,
) -> Option<Permits> {
    let judge = candidate
        .connection_id
        .and_then(|id| limits.connections.get(&id).map(|limit| (id, limit)));
    let webhook = limits
        .webhooks
        .get(&candidate.job.evaluator_id)
        .map(|slots| (candidate.job.evaluator_id, slots));
    if let Some((id, _)) = judge
        && blocked.contains(&id)
    {
        return None;
    }
    if let Some((id, _)) = webhook
        && blocked.contains(&id)
    {
        return None;
    }
    let worker = Arc::clone(&limits.workers).try_acquire_owned().ok()?;
    let slot = match (judge, webhook) {
        (Some((id, limit)), _) => {
            // The slot first: a token is never spent on a job that has no slot.
            let slot = Arc::clone(&limit.slots).try_acquire_owned().ok();
            let token = slot.is_some() && lock(&limit.bucket).try_take(now);
            if !token {
                blocked.insert(id);
                return None;
            }
            slot
        }
        (None, Some((id, slots))) => match Arc::clone(slots).try_acquire_owned() {
            Ok(permit) => Some(permit),
            Err(_) => {
                blocked.insert(id);
                return None;
            }
        },
        (None, None) => None,
    };
    Some(Permits {
        _worker: worker,
        _slot: slot,
    })
}

async fn claim_all(
    conn: &mut SqliteConnection,
    ids: Vec<Uuid>,
    now: i64,
) -> Result<Vec<Option<u32>>, MetaError> {
    let mut claims = Vec::with_capacity(ids.len());
    for id in ids {
        claims.push(eval_jobs::claim(conn, id, now).await?);
    }
    Ok(claims)
}

/// Runs one claimed job and then frees its permits. A panic is recorded as a permanent failure.
async fn run_claimed(state: AppState, job: ClaimedJob, permits: Permits) {
    let id = job.row.id;
    let claims = job.claims;
    let outcome = AssertUnwindSafe(execute::run_job(&state, job))
        .catch_unwind()
        .await;
    match outcome {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            tracing::error!(job_id = %id, error = %error, "eval job could not be recorded");
        }
        Err(_panic) => {
            tracing::error!(job_id = %id, "eval job panicked");
            if let Err(error) = finish_panicked(&state, id, claims).await {
                tracing::error!(job_id = %id, error = %error, "eval job could not be failed");
            }
        }
    }
    drop(permits);
    state.eval.wake();
}

/// Fails a job whose run panicked: `failed`, permanent, `internal error`, fenced by `claims`.
/// Returns whether the claim was still current.
///
/// # Errors
///
/// Returns [`MetaError`] when the write fails.
async fn finish_panicked(state: &AppState, id: Uuid, claims: u32) -> Result<bool, MetaError> {
    let now = state.clock.now_ns();
    state
        .meta
        .write(move |conn| {
            Box::pin(async move {
                eval_jobs::finish(
                    conn,
                    id,
                    claims,
                    Finish::Failed {
                        class: "permanent",
                        error: "internal error".to_owned(),
                        judge_trace_id: None,
                        evaluator_version: None,
                        usage: JobUsage::default(),
                    },
                    now,
                )
                .await
            })
        })
        .await
}

/// Re-queues jobs running for more than 10 minutes, and fails those interrupted too often
/// (D-I-12). Returns `(requeued, failed)`.
///
/// # Errors
///
/// Returns [`MetaError`] when the write fails.
pub(crate) async fn janitor_once(state: &AppState, now: i64) -> Result<(u64, u64), MetaError> {
    let older_than = now.saturating_sub(STALE_NS);
    let (requeued, failed) = state
        .meta
        .write(move |conn| {
            Box::pin(async move { eval_jobs::requeue_stale(conn, older_than, now).await })
        })
        .await?;
    if requeued > 0 {
        state.eval.wake();
    }
    Ok((requeued, failed))
}

/// Hourly pruning (D-I-28): the eval jobs, then the issue candidates (D-IC-6, A7), then the
/// candidates older than the data window of each org that has one.
///
/// # Errors
///
/// Returns [`MetaError`] when a write fails.
pub(crate) async fn prune_once(state: &AppState, now: i64) -> Result<PruneReport, MetaError> {
    let report = state
        .meta
        .write(move |conn| Box::pin(async move { eval_jobs::prune(conn, now).await }))
        .await?;
    let pruned = state
        .meta
        .write(move |conn| Box::pin(async move { issues::prune_candidates(conn, now).await }))
        .await?;
    if pruned > 0 {
        tracing::info!(removed = pruned, "issue candidates pruned");
    }
    let mut orgs: Vec<OrgId> = projects::projects_for_retention(state.meta.reader())
        .await?
        .into_iter()
        .map(|(org, _, _)| org)
        .collect();
    orgs.sort_unstable();
    orgs.dedup();
    for org in orgs {
        let Some(visible_from) = Visibility::window(state, org).visible_from() else {
            continue;
        };
        let removed = state
            .meta
            .write(move |conn| {
                Box::pin(async move { issues::prune_to_window(conn, org, visible_from).await })
            })
            .await?;
        if removed > 0 {
            tracing::info!(org_id = %org, removed, "issue candidates outside the data window pruned");
        }
    }
    Ok(report)
}

/// Clears `'budget'` pauses whose period has turned (D-I-15) on rules and on issue sources, and
/// wakes the dispatcher and the issue workers when it cleared one. Returns how many rules were
/// resumed.
///
/// # Errors
///
/// Returns [`MetaError`] when the write fails.
pub(crate) async fn budget_once(state: &AppState, now: i64) -> Result<usize, MetaError> {
    let day = budget::day_start(now);
    let month = budget::month_start(now);
    let cleared = state
        .meta
        .write(move |conn| {
            Box::pin(async move { online_rules::clear_budget_pauses(conn, day, month, now).await })
        })
        .await?;
    for rule in &cleared {
        state.eval.budgets.forget(*rule);
        tracing::info!(
            target: "audit",
            rule_id = %rule,
            reason = "budget period",
            "online rule resumed"
        );
    }
    if !cleared.is_empty() {
        state.eval.wake();
    }
    let sources = state
        .meta
        .write(move |conn| {
            Box::pin(async move { issues::clear_budget_pauses(conn, day, month, now).await })
        })
        .await?;
    if sources > 0 {
        tracing::info!(
            target: "audit",
            sources,
            reason = "budget period",
            "issue sources resumed"
        );
        state.issues_wake();
    }
    Ok(cleared.len())
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use axum::http::{Method, header};
    use cetra_core::enums::PayloadFormat;
    use cetra_core::ids::{SpanId, TraceId};
    use cetra_core::span::Span;
    use serde_json::{Value, json};

    use super::*;
    use crate::eval::EvalTiming;
    use crate::meta::eval_jobs::{JobRow, KIND_MANUAL, KIND_RULE, NewJob, get_job, insert_job};
    use crate::meta::online_rules::{
        NewRule, PAUSE_ERRORS, RuleFields, get_rule, insert_rule, pause_rule,
    };
    use crate::meta::projects::ProjectRow;
    use crate::test_support::mock_llm::{MockLlm, completion};
    use crate::test_support::{Seeded, TestApp, dataset_span};

    const MAPPING: &str = r#"{"input": "trace.input", "output": "trace.output"}"#;
    const CHECK_MAPPING: &str = r#"{"output": "trace.output", "expected": "trace.input"}"#;
    const JUDGE_ANSWER: &str = r#"{"explanation": "Relevant.", "value": 0.9}"#;
    const QUESTION: &str = "What is the refund window?";

    fn trace(n: u128) -> TraceId {
        TraceId::from_bytes(*Uuid::from_u128(n).as_bytes()).expect("not all zero")
    }

    fn span(n: u64) -> SpanId {
        SpanId::from_bytes(n.to_be_bytes()).expect("not all zero")
    }

    fn real_now() -> i64 {
        i64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("after the epoch")
                .as_nanos(),
        )
        .expect("fits i64")
    }

    /// An app with `workers` eval slots, reaching `127.0.0.1`, with the clock at the real time and
    /// one seeded project (§ Conventions: the clock is real before anything is seeded).
    async fn seeded_app(workers: u32) -> (TestApp, Seeded) {
        let app = TestApp::with_config(move |config| {
            config.eval.workers = workers;
            config.security.allow_private_network_egress = true;
        })
        .await;
        app.state.clock.set(real_now());
        let seeded = app.seed().await;
        (app, seeded)
    }

    /// Stores a root span of `trace` in the seeded project: a question and its answer.
    async fn store_trace(app: &TestApp, seeded: &Seeded, trace: TraceId) {
        let now = app.state.clock.now_ns();
        let mut row: Span = dataset_span(0);
        row.trace_id = trace;
        row.span_id = span(1);
        row.parent_span_id = None;
        row.input = Some(QUESTION.to_owned());
        row.input_format = Some(PayloadFormat::Text);
        row.output = Some("30 days.".to_owned());
        row.output_format = Some(PayloadFormat::Text);
        row.environment = Some("production".to_owned());
        row.start_time = now - 30_000_000_000;
        row.end_time = now - 20_000_000_000;
        app.commit_spans(&seeded.project, vec![row]).await;
    }

    /// An `exact_match` check, named `Same`.
    async fn check(app: &TestApp, seeded: &Seeded) -> Uuid {
        create_evaluator(
            app,
            seeded,
            json!({
                "name": "Same",
                "kind": "check",
                "config": {"check": "exact_match"},
                "output_score": {"name": "same", "data_type": "boolean"}
            }),
        )
        .await
    }

    /// A webhook evaluator at an unreachable URL. Only the dispatcher's claims are tested with it.
    async fn webhook(app: &TestApp, seeded: &Seeded) -> Uuid {
        create_evaluator(
            app,
            seeded,
            json!({
                "name": "Hook",
                "kind": "webhook",
                "config": {"url": "http://127.0.0.1:9/hook"},
                "output_score": {"name": "hook", "data_type": "numeric"},
                "webhook_secret": "0123456789abcdef"
            }),
        )
        .await
    }

    async fn create_evaluator(app: &TestApp, seeded: &Seeded, body: Value) -> Uuid {
        let response = app
            .request(
                Method::POST,
                &format!("/api/v1/projects/{}/evaluators", seeded.project.id),
            )
            .header(header::COOKIE.as_str(), &seeded.cookie)
            .json(&body)
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

    /// `seeded` with `project` in place of the seeded one, for a second project of the same org.
    fn in_project(seeded: &Seeded, project: ProjectRow) -> Seeded {
        Seeded {
            user_id: seeded.user_id,
            org_id: seeded.org_id,
            project,
            cookie: seeded.cookie.clone(),
            ingest_key: seeded.ingest_key.clone(),
            read_key: seeded.read_key.clone(),
            public_key: seeded.public_key.clone(),
        }
    }

    /// A connection to `mock` with the given limits.
    async fn connection(
        app: &TestApp,
        seeded: &Seeded,
        mock: &MockLlm,
        max_concurrency: u32,
        requests_per_minute: u32,
    ) -> Uuid {
        let response = app
            .request(Method::POST, "/api/v1/orgs/acme/llm-connections")
            .header(header::COOKIE.as_str(), &seeded.cookie)
            .json(&json!({
                "name": "Mocked",
                "adapter": "openai",
                "api_key": "sk-test-key-0123456789",
                "base_url": mock.base_url(),
                "default_model": "gpt-test",
                "max_concurrency": max_concurrency,
                "requests_per_minute": requests_per_minute
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

    /// A judge on `connection`, named `Relevance`.
    async fn judge(app: &TestApp, seeded: &Seeded, connection: Uuid) -> Uuid {
        create_evaluator(
            app,
            seeded,
            json!({
                "name": "Relevance",
                "kind": "llm_judge",
                "config": {
                    "connection_id": connection,
                    "model": "gpt-test",
                    "prompt": "Question: {{ input }}\nAnswer: {{ output }}",
                    "output": {"type": "numeric", "min": 0.0, "max": 1.0},
                    "include_reason": true,
                    "temperature": null,
                    "max_output_tokens": 1024,
                    "timeout_ms": 60000
                },
                "output_score": {"name": "relevance", "data_type": "numeric"}
            }),
        )
        .await
    }

    /// A trace rule on `evaluator` with no budget.
    async fn rule(app: &TestApp, seeded: &Seeded, evaluator: Uuid) -> Uuid {
        let id = Uuid::now_v7();
        let project = seeded.project.id;
        let now = app.state.clock.now_ns();
        let fields = RuleFields {
            name: format!("rule-{id}"),
            filter: None,
            variable_mapping: MAPPING.to_owned(),
            sample_rate: 1.0,
            budget_usd: None,
            budget_period: None,
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

    /// Queues a manual run of `evaluator` on `trace`, and returns its id.
    async fn manual_job(
        app: &TestApp,
        seeded: &Seeded,
        evaluator: Uuid,
        trace: TraceId,
        mapping: &str,
    ) -> Uuid {
        let id = Uuid::now_v7();
        let now = app.state.clock.now_ns();
        enqueue(
            app,
            NewJob {
                id,
                project_id: seeded.project.id,
                kind: KIND_MANUAL,
                rule_id: None,
                evaluator_id: evaluator,
                variable_mapping: Some(mapping.to_owned()),
                trace_id: trace,
                span_id: None,
                trace_start_ns: now - 30_000_000_000,
            },
        )
        .await;
        id
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

    async fn job_row(app: &TestApp, seeded: &Seeded, id: Uuid) -> JobRow {
        get_job(app.state.meta.reader(), seeded.project.id, id)
            .await
            .expect("read")
            .expect("job")
    }

    /// Polls until the job has `status`, for at most 5 s.
    async fn wait_for_status(app: &TestApp, seeded: &Seeded, id: Uuid, status: &str) -> JobRow {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let row = job_row(app, seeded, id).await;
            if row.status == status {
                return row;
            }
            assert!(
                Instant::now() < deadline,
                "job {id} stayed {} instead of {status}",
                row.status
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    async fn pass(app: &TestApp, running: &mut JoinSet<()>) -> usize {
        dispatch_once(&app.state, app.state.clock.now_ns(), running)
            .await
            .expect("dispatch pass")
    }

    async fn settle(running: &mut JoinSet<()>) {
        while running.join_next().await.is_some() {}
    }

    fn candidate(
        project: ProjectId,
        priority: u8,
        evaluator: Uuid,
        kind: &str,
        connection: Option<(Uuid, u32, u32)>,
    ) -> Candidate {
        Candidate {
            job: JobRow {
                id: Uuid::now_v7(),
                project_id: project,
                kind: KIND_MANUAL.to_owned(),
                priority,
                rule_id: None,
                evaluator_id: evaluator,
                variable_mapping: None,
                trace_id: trace(1),
                span_id: None,
                trace_start_ns: 0,
                status: "queued".to_owned(),
                attempts: 0,
                claims: 0,
                load_retries: 0,
                interruptions: 0,
                not_before: 0,
                error_class: None,
                error: None,
                judge_trace_id: None,
                score_id: None,
                evaluator_version: None,
                input_tokens: None,
                output_tokens: None,
                cost_usd: None,
                latency_ms: None,
                created_at: 0,
                updated_at: 0,
                finished_at: None,
            },
            evaluator_kind: kind.to_owned(),
            connection_id: connection.map(|(id, _, _)| id),
            connection_limits: connection.map(|(_, max, rpm)| (max, rpm, 1)),
        }
    }

    fn project(n: u128) -> ProjectId {
        ProjectId::from_uuid(Uuid::from_u128(n))
    }

    fn limits(workers: usize) -> PickLimits {
        PickLimits {
            workers: Arc::new(Semaphore::new(workers)),
            connections: HashMap::new(),
            webhooks: HashMap::new(),
            last_project: HashMap::new(),
        }
    }

    fn add_connection(limits: &mut PickLimits, id: Uuid, max: u32, rpm: u32) {
        limits
            .connections
            .insert(id, Arc::new(ConnectionLimit::new((max, rpm), 0)));
    }

    #[test]
    fn token_bucket_bursts_then_refills() {
        let mut bucket = TokenBucket::new(60, 0);
        for _ in 0..10 {
            assert!(bucket.try_take(0));
        }
        assert!(!bucket.try_take(0));
        assert!(
            bucket.try_take(1_000_000_000),
            "one token after 1 s at 60 rpm"
        );
        assert!(!bucket.try_take(1_000_000_000));

        let mut slow = TokenBucket::new(3, 0);
        assert!(slow.try_take(0));
        assert!(!slow.try_take(0), "3 rpm has a capacity of 1");
    }

    #[test]
    fn try_connection_permit_shares_judge_slots() {
        let queue = EvalQueue::new(4);
        let id = Uuid::from_u128(7);
        let first = queue.try_connection_permit(id, (1, 600), 0);
        assert!(first.is_some(), "a free slot and a token");
        assert!(
            queue.try_connection_permit(id, (1, 600), 0).is_none(),
            "one slot, held by the first permit"
        );
        assert!(queue.saturated_connections(0).contains(&id));
        drop(first);
        assert!(
            !queue.saturated_connections(0).contains(&id),
            "the slot is free once the permit drops"
        );
        assert!(queue.try_connection_permit(id, (1, 600), 0).is_some());
    }

    #[test]
    fn try_connection_permit_respects_the_bucket() {
        let queue = EvalQueue::new(4);
        let id = Uuid::from_u128(8);
        let first = queue.try_connection_permit(id, (4, 1), 0);
        assert!(first.is_some(), "one token at 1 rpm");
        drop(first);
        assert!(
            queue.try_connection_permit(id, (4, 1), 0).is_none(),
            "no token left at 1 rpm"
        );
        assert!(queue.saturated_connections(0).contains(&id));
        let free_slots = lock(&queue.connections)[&id].slots.available_permits();
        assert_eq!(free_slots, 4, "a refused permit takes no slot");
        assert!(
            queue
                .try_connection_permit(id, (4, 1), 60_000_000_000)
                .is_some(),
            "one token refilled after 60 s at 1 rpm"
        );
    }

    #[test]
    fn pick_is_strict_priority_then_round_robin() {
        let (a, b) = (project(1), project(2));
        let evaluator = Uuid::from_u128(9);
        let candidates = vec![
            candidate(a, 0, evaluator, "check", None),
            candidate(a, 0, evaluator, "check", None),
            candidate(a, 1, evaluator, "check", None),
            candidate(a, 1, evaluator, "check", None),
            candidate(a, 1, evaluator, "check", None),
            candidate(b, 1, evaluator, "check", None),
        ];
        let picked = pick(candidates, &limits(4), 0);
        let order: Vec<(u8, ProjectId)> = picked
            .iter()
            .map(|(c, _)| (c.job.priority, c.job.project_id))
            .collect();
        assert_eq!(order, vec![(0, a), (0, a), (1, a), (1, b)]);
    }

    #[test]
    fn pick_rotates_across_passes() {
        let (a, b) = (project(1), project(2));
        let evaluator = Uuid::from_u128(9);
        let mut last = HashMap::new();
        let mut served = Vec::new();
        for _ in 0..4 {
            let candidates = (0..5)
                .flat_map(|_| {
                    [
                        candidate(a, 1, evaluator, "check", None),
                        candidate(b, 1, evaluator, "check", None),
                    ]
                })
                .collect();
            let mut pass_limits = limits(1);
            pass_limits.last_project = last.clone();
            let picked = pick(candidates, &pass_limits, 0);
            let (chosen, _) = &picked[0];
            last.insert(1, chosen.job.project_id);
            served.push(chosen.job.project_id);
        }
        assert_eq!(served, vec![a, b, a, b]);
    }

    #[test]
    fn pick_skips_a_full_connection() {
        let (a, connection, evaluator) = (project(1), Uuid::from_u128(5), Uuid::from_u128(9));
        let mut pick_limits = limits(4);
        add_connection(&mut pick_limits, connection, 1, 6_000);
        let candidates = vec![
            candidate(a, 1, evaluator, "llm_judge", Some((connection, 1, 6_000))),
            candidate(a, 1, evaluator, "llm_judge", Some((connection, 1, 6_000))),
            candidate(a, 1, evaluator, "check", None),
        ];
        let picked = pick(candidates, &pick_limits, 0);
        let kinds: Vec<&str> = picked
            .iter()
            .map(|(c, _)| c.evaluator_kind.as_str())
            .collect();
        assert_eq!(kinds, vec!["llm_judge", "check"]);
    }

    #[test]
    fn pick_takes_no_token_without_a_slot() {
        let (a, connection, evaluator) = (project(1), Uuid::from_u128(5), Uuid::from_u128(9));
        let mut pick_limits = limits(4);
        add_connection(&mut pick_limits, connection, 1, 60);
        let held = Arc::clone(&pick_limits.connections[&connection].slots)
            .try_acquire_owned()
            .expect("the only slot");
        let picked = pick(
            vec![candidate(
                a,
                1,
                evaluator,
                "llm_judge",
                Some((connection, 1, 60)),
            )],
            &pick_limits,
            0,
        );
        assert!(picked.is_empty());
        let bucket = lock(&pick_limits.connections[&connection].bucket).clone();
        assert_eq!(bucket, TokenBucket::new(60, 0), "no token was taken");
        drop(held);
    }

    #[test]
    fn pick_skips_a_webhook_at_four() {
        let (a, evaluator) = (project(1), Uuid::from_u128(9));
        let mut pick_limits = limits(10);
        pick_limits
            .webhooks
            .insert(evaluator, Arc::new(Semaphore::new(WEBHOOK_SLOTS)));
        let candidates = (0..6)
            .map(|_| candidate(a, 1, evaluator, "webhook", None))
            .collect();
        assert_eq!(pick(candidates, &pick_limits, 0).len(), WEBHOOK_SLOTS);
    }

    #[tokio::test]
    async fn dispatch_runs_a_manual_job_to_done() {
        let (app, seeded) = seeded_app(16).await;
        let trace_id = trace(0x10);
        store_trace(&app, &seeded, trace_id).await;
        let evaluator = check(&app, &seeded).await;
        let job = manual_job(&app, &seeded, evaluator, trace_id, CHECK_MAPPING).await;

        let mut running = JoinSet::new();
        assert_eq!(pass(&app, &mut running).await, 1);
        settle(&mut running).await;
        assert_eq!(job_row(&app, &seeded, job).await.status, "done");
    }

    #[tokio::test]
    async fn dispatch_counts_finished_jobs() {
        let (app, seeded) = seeded_app(16).await;
        let trace_id = trace(0x14);
        store_trace(&app, &seeded, trace_id).await;
        let evaluator = check(&app, &seeded).await;
        manual_job(&app, &seeded, evaluator, trace_id, CHECK_MAPPING).await;
        manual_job(&app, &seeded, evaluator, trace_id, "not json").await;

        let mut running = JoinSet::new();
        assert_eq!(pass(&app, &mut running).await, 2);
        settle(&mut running).await;
        let text = app
            .state
            .telemetry
            .render(&app.state)
            .await
            .expect("metrics render");
        for line in [
            "cetra_eval_jobs_finished_total{outcome=\"done\"} 1",
            "cetra_eval_jobs_finished_total{outcome=\"failed\"} 1",
            "cetra_eval_run_duration_seconds_count{kind=\"check\"} 1",
        ] {
            assert!(
                text.lines().any(|candidate| candidate == line),
                "{line}\n{text}"
            );
        }
    }

    #[tokio::test]
    async fn dispatch_respects_eval_workers() {
        let (app, seeded) = seeded_app(2).await;
        let trace_id = trace(0x11);
        store_trace(&app, &seeded, trace_id).await;
        let evaluator = check(&app, &seeded).await;
        for _ in 0..5 {
            manual_job(&app, &seeded, evaluator, trace_id, CHECK_MAPPING).await;
        }

        let mut running = JoinSet::new();
        assert_eq!(pass(&app, &mut running).await, 2);
        settle(&mut running).await;
    }

    #[tokio::test]
    async fn dispatch_respects_the_connection_slots() {
        let (app, seeded) = seeded_app(16).await;
        let trace_id = trace(0x12);
        store_trace(&app, &seeded, trace_id).await;
        let mock =
            MockLlm::start_with_delay(vec![completion(JUDGE_ANSWER)], Duration::from_millis(500))
                .await;
        let connection = connection(&app, &seeded, &mock, 1, 300).await;
        let evaluator = judge(&app, &seeded, connection).await;
        let first = manual_job(&app, &seeded, evaluator, trace_id, MAPPING).await;
        let second = manual_job(&app, &seeded, evaluator, trace_id, MAPPING).await;

        let mut running = JoinSet::new();
        assert_eq!(pass(&app, &mut running).await, 1);
        assert_eq!(pass(&app, &mut running).await, 0, "the slot is taken");
        running.join_next().await;
        assert_eq!(pass(&app, &mut running).await, 1);
        settle(&mut running).await;
        let statuses = [
            job_row(&app, &seeded, first).await.status,
            job_row(&app, &seeded, second).await.status,
        ];
        assert_eq!(statuses, ["done", "done"]);
    }

    #[tokio::test]
    async fn dispatch_respects_the_bucket() {
        let (app, seeded) = seeded_app(16).await;
        let trace_id = trace(0x13);
        store_trace(&app, &seeded, trace_id).await;
        let mock = MockLlm::start(vec![completion(JUDGE_ANSWER)]).await;
        // 6 requests a minute: a capacity of one token, refilled at one every 10 s.
        let connection = connection(&app, &seeded, &mock, 4, 6).await;
        let evaluator = judge(&app, &seeded, connection).await;
        for _ in 0..3 {
            manual_job(&app, &seeded, evaluator, trace_id, MAPPING).await;
        }

        let mut running = JoinSet::new();
        assert_eq!(pass(&app, &mut running).await, 1);
        assert_eq!(pass(&app, &mut running).await, 0, "no token left");
        settle(&mut running).await;
        app.state
            .clock
            .set(app.state.clock.now_ns() + 10_000_000_000);
        assert_eq!(pass(&app, &mut running).await, 1, "one token refilled");
        settle(&mut running).await;
    }

    #[tokio::test]
    async fn dispatch_holds_a_webhook_at_four() {
        let (app, seeded) = seeded_app(16).await;
        let trace_id = trace(0x14);
        store_trace(&app, &seeded, trace_id).await;
        let evaluator = webhook(&app, &seeded).await;
        for _ in 0..6 {
            manual_job(&app, &seeded, evaluator, trace_id, MAPPING).await;
        }

        let mut running = JoinSet::new();
        assert_eq!(pass(&app, &mut running).await, WEBHOOK_SLOTS);
        settle(&mut running).await;
    }

    #[tokio::test]
    async fn the_rotation_serves_every_project() {
        let (app, seeded) = seeded_app(1).await;
        let trace_a = trace(0x15);
        store_trace(&app, &seeded, trace_a).await;
        let (other, _) = app.add_project(&seeded, "beta", None).await;
        let other_seeded = in_project(&seeded, other);
        let trace_b = trace(0x16);
        store_trace(&app, &other_seeded, trace_b).await;

        let evaluator_a = check(&app, &seeded).await;
        let evaluator_b = check(&app, &other_seeded).await;
        for _ in 0..25 {
            manual_job(&app, &seeded, evaluator_a, trace_a, CHECK_MAPPING).await;
        }
        let job_b = manual_job(&app, &other_seeded, evaluator_b, trace_b, CHECK_MAPPING).await;

        // Two passes, each after the previous job finished: the second serves the other project.
        let mut running = JoinSet::new();
        assert_eq!(pass(&app, &mut running).await, 1);
        settle(&mut running).await;
        assert_eq!(pass(&app, &mut running).await, 1);
        settle(&mut running).await;
        assert_eq!(job_row(&app, &other_seeded, job_b).await.status, "done");
    }

    #[tokio::test]
    async fn the_dispatcher_wakes_on_notify() {
        let (app, seeded) = seeded_app(16).await;
        let trace_id = trace(0x17);
        store_trace(&app, &seeded, trace_id).await;
        let evaluator = check(&app, &seeded).await;
        let probe = manual_job(&app, &seeded, evaluator, trace_id, CHECK_MAPPING).await;
        let timing = EvalTiming {
            dispatch_every: Duration::from_secs(3_600),
            ..EvalTiming::TEST
        };
        let tasks = crate::eval::spawn(&app.state, timing);
        wait_for_status(&app, &seeded, probe, "done").await;
        // Let the worker's own pass after its job run out, so no earlier wake-up can pick up the
        // next job.
        tokio::time::sleep(Duration::from_millis(200)).await;

        // The hour-long tick cannot run the next pass: only the wake-up can.
        let second = manual_job(&app, &seeded, evaluator, trace_id, CHECK_MAPPING).await;
        let woken = Instant::now();
        app.state.eval.wake();
        wait_for_status(&app, &seeded, second, "done").await;
        assert!(
            woken.elapsed() < timing.dispatch_every,
            "the wake-up ran the job, not a tick"
        );
        tasks.shutdown(&app.state).await;
    }

    #[tokio::test]
    async fn a_paused_rules_backlog_does_not_hold_back_another_rule() {
        let (app, seeded) = seeded_app(16).await;
        let project = seeded.project.id;
        let backlog_evaluator = check(&app, &seeded).await;
        let paused = rule(&app, &seeded, backlog_evaluator).await;
        let now = app.state.clock.now_ns();
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    pause_rule(conn, paused, PAUSE_ERRORS, Some("test"), now)
                        .await
                        .map(|_| ())
                })
            })
            .await
            .expect("pause");
        // 5 000 queued jobs of the paused rule: the candidate scan must not return them.
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    for index in 0..5_000_u128 {
                        insert_job(
                            conn,
                            NewJob {
                                id: Uuid::now_v7(),
                                project_id: project,
                                kind: KIND_RULE,
                                rule_id: Some(paused),
                                evaluator_id: backlog_evaluator,
                                variable_mapping: None,
                                trace_id: trace(0x10_000 + index),
                                span_id: None,
                                trace_start_ns: now - 30_000_000_000,
                            },
                            now,
                        )
                        .await?;
                    }
                    Ok(())
                })
            })
            .await
            .expect("backlog");

        let other_evaluator = create_evaluator(
            &app,
            &seeded,
            json!({
                "name": "Other",
                "kind": "check",
                "config": {"check": "exact_match"},
                "output_score": {"name": "other", "data_type": "boolean"}
            }),
        )
        .await;
        let other_rule = rule(&app, &seeded, other_evaluator).await;
        let trace_id = trace(0x19);
        store_trace(&app, &seeded, trace_id).await;
        let ready = Uuid::now_v7();
        enqueue(
            &app,
            NewJob {
                id: ready,
                project_id: project,
                kind: KIND_RULE,
                rule_id: Some(other_rule),
                evaluator_id: other_evaluator,
                variable_mapping: None,
                trace_id,
                span_id: None,
                trace_start_ns: now - 30_000_000_000,
            },
        )
        .await;

        let mut running = JoinSet::new();
        assert_eq!(
            pass(&app, &mut running).await,
            1,
            "the ready job is the only claim"
        );
        settle(&mut running).await;
        assert_ne!(job_row(&app, &seeded, ready).await.status, "queued");
        let backlog: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM eval_jobs WHERE rule_id = ? AND status = 'queued'",
        )
        .bind(paused.to_string())
        .fetch_one(app.state.meta.reader())
        .await
        .expect("count");
        assert_eq!(backlog, 5_000, "the paused rule's jobs wait");
    }

    #[tokio::test]
    async fn dispatch_skips_paused_rules_and_archived_projects() {
        let (app, seeded) = seeded_app(16).await;
        let trace_id = trace(0x18);
        store_trace(&app, &seeded, trace_id).await;
        let evaluator = check(&app, &seeded).await;
        let rule_id = rule(&app, &seeded, evaluator).await;
        let now = app.state.clock.now_ns();
        let rule_job = Uuid::now_v7();
        enqueue(
            &app,
            NewJob {
                id: rule_job,
                project_id: seeded.project.id,
                kind: KIND_RULE,
                rule_id: Some(rule_id),
                evaluator_id: evaluator,
                variable_mapping: None,
                trace_id,
                span_id: None,
                trace_start_ns: now - 30_000_000_000,
            },
        )
        .await;
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    pause_rule(conn, rule_id, PAUSE_ERRORS, Some("test"), now)
                        .await
                        .map(|_| ())
                })
            })
            .await
            .expect("pause");

        let mut running = JoinSet::new();
        assert_eq!(
            pass(&app, &mut running).await,
            0,
            "a paused rule's job waits"
        );
        assert_eq!(job_row(&app, &seeded, rule_job).await.status, "queued");

        let manual = manual_job(&app, &seeded, evaluator, trace_id, CHECK_MAPPING).await;
        set_project_state(&app, &seeded, "archived").await;
        assert_eq!(
            pass(&app, &mut running).await,
            0,
            "an archived project waits"
        );
        assert_eq!(job_row(&app, &seeded, manual).await.status, "queued");
    }

    async fn set_project_state(app: &TestApp, seeded: &Seeded, state: &'static str) {
        let project = seeded.project.id;
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    sqlx::query("UPDATE projects SET state = ? WHERE id = ?")
                        .bind(state)
                        .bind(project.to_string())
                        .execute(&mut *conn)
                        .await
                        .map(|_| ())
                        .map_err(MetaError::from)
                })
            })
            .await
            .expect("project state");
    }

    #[tokio::test]
    async fn a_changed_connection_gets_new_limits() {
        let app = TestApp::new().await;
        let (id, project_id, evaluator) = (Uuid::from_u128(5), project(1), Uuid::from_u128(9));
        let with = |max: u32, rpm: u32, updated_at: i64| {
            let mut c = candidate(project_id, 1, evaluator, "llm_judge", Some((id, max, rpm)));
            c.connection_limits = Some((max, rpm, updated_at));
            vec![c]
        };
        let first = pick_limits(&app.state, &with(4, 300, 1), 0).connections[&id].clone();
        let raised = pick_limits(&app.state, &with(8, 300, 1), 0).connections[&id].clone();
        assert!(!Arc::ptr_eq(&first, &raised), "a new max_concurrency");
        let key_changed = pick_limits(&app.state, &with(8, 300, 2), 0).connections[&id].clone();
        assert!(
            Arc::ptr_eq(&raised, &key_changed),
            "a key change keeps the limits"
        );
    }

    #[tokio::test]
    async fn a_panicked_job_fails_with_internal_error() {
        let (app, seeded) = seeded_app(16).await;
        let trace_id = trace(0x19);
        store_trace(&app, &seeded, trace_id).await;
        let evaluator = check(&app, &seeded).await;
        let job = manual_job(&app, &seeded, evaluator, trace_id, CHECK_MAPPING).await;
        let now = app.state.clock.now_ns();
        app.state
            .meta
            .write(move |conn| Box::pin(async move { eval_jobs::claim(conn, job, now).await }))
            .await
            .expect("claim");

        assert!(!finish_panicked(&app.state, job, 0).await.expect("stale"));
        assert_eq!(job_row(&app, &seeded, job).await.status, "running");

        assert!(finish_panicked(&app.state, job, 1).await.expect("finish"));
        let row = job_row(&app, &seeded, job).await;
        assert_eq!(row.status, "failed");
        assert_eq!(row.error_class.as_deref(), Some("permanent"));
        assert_eq!(row.error.as_deref(), Some("internal error"));
    }

    #[tokio::test]
    async fn janitor_requeues_stale_jobs() {
        let (app, seeded) = seeded_app(16).await;
        let trace_id = trace(0x1a);
        store_trace(&app, &seeded, trace_id).await;
        let evaluator = check(&app, &seeded).await;
        let job = manual_job(&app, &seeded, evaluator, trace_id, CHECK_MAPPING).await;
        let now = app.state.clock.now_ns();
        app.state
            .meta
            .write(move |conn| Box::pin(async move { eval_jobs::claim(conn, job, now).await }))
            .await
            .expect("claim");
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    sqlx::query("UPDATE eval_jobs SET updated_at = 0 WHERE id = ?")
                        .bind(job.to_string())
                        .execute(&mut *conn)
                        .await
                        .map(|_| ())
                        .map_err(MetaError::from)
                })
            })
            .await
            .expect("age the job");

        let later = now + STALE_NS + 1_000_000_000;
        assert_eq!(
            janitor_once(&app.state, later).await.expect("janitor"),
            (1, 0)
        );
        assert_eq!(job_row(&app, &seeded, job).await.status, "queued");
    }

    #[tokio::test]
    async fn prune_once_deletes_old_finished_jobs() {
        let (app, seeded) = seeded_app(16).await;
        let trace_id = trace(0x1b);
        store_trace(&app, &seeded, trace_id).await;
        let evaluator = check(&app, &seeded).await;
        let job = manual_job(&app, &seeded, evaluator, trace_id, CHECK_MAPPING).await;
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    sqlx::query(
                        "UPDATE eval_jobs SET status = 'done', finished_at = 0, updated_at = 0 WHERE id = ?",
                    )
                    .bind(job.to_string())
                    .execute(&mut *conn)
                    .await
                    .map(|_| ())
                    .map_err(MetaError::from)
                })
            })
            .await
            .expect("finish the job long ago");

        let report = prune_once(&app.state, app.state.clock.now_ns())
            .await
            .expect("prune");
        assert_eq!(report.deleted, 1);
    }

    #[tokio::test]
    async fn budget_once_clears_a_turned_day_and_keeps_today() {
        let (app, seeded) = seeded_app(16).await;
        let evaluator = check(&app, &seeded).await;
        let turned = rule(&app, &seeded, evaluator).await;
        let today = rule(&app, &seeded, evaluator).await;
        let now = app.state.clock.now_ns();
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    for (id, paused_at) in [(turned, 0), (today, now)] {
                        sqlx::query(
                            "UPDATE online_rules SET budget_usd = 5, budget_period = 'day', \
                             paused_reason = 'budget', paused_at = ? WHERE id = ?",
                        )
                        .bind(paused_at)
                        .bind(id.to_string())
                        .execute(&mut *conn)
                        .await
                        .map_err(MetaError::from)?;
                    }
                    Ok(())
                })
            })
            .await
            .expect("pause both rules on the budget");

        assert_eq!(budget_once(&app.state, now).await.expect("budget"), 1);
        let turned_row = get_rule(app.state.meta.reader(), seeded.project.id, turned)
            .await
            .expect("read")
            .expect("rule");
        let today_row = get_rule(app.state.meta.reader(), seeded.project.id, today)
            .await
            .expect("read")
            .expect("rule");
        assert_eq!(turned_row.paused_reason, None);
        assert_eq!(today_row.paused_reason.as_deref(), Some("budget"));
    }

    #[tokio::test]
    async fn recover_requeues_and_fails_crash_loops() {
        let (app, seeded) = seeded_app(16).await;
        let trace_id = trace(0x1c);
        store_trace(&app, &seeded, trace_id).await;
        let evaluator = check(&app, &seeded).await;
        let fresh = manual_job(&app, &seeded, evaluator, trace_id, CHECK_MAPPING).await;
        let looping = manual_job(&app, &seeded, evaluator, trace_id, CHECK_MAPPING).await;
        let now = app.state.clock.now_ns();
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    for job in [fresh, looping] {
                        eval_jobs::claim(conn, job, now).await?;
                    }
                    sqlx::query("UPDATE eval_jobs SET interruptions = 3 WHERE id = ?")
                        .bind(looping.to_string())
                        .execute(&mut *conn)
                        .await
                        .map(|_| ())
                        .map_err(MetaError::from)
                })
            })
            .await
            .expect("claim both");

        crate::eval::recover(&app.state).await.expect("recover");
        assert_eq!(job_row(&app, &seeded, fresh).await.status, "queued");
        assert_eq!(job_row(&app, &seeded, looping).await.status, "failed");
    }

    #[tokio::test]
    async fn recover_fails_running_backfills() {
        use crate::meta::backfills::{
            BackfillEnd, NewBackfill, finish_backfill, insert_backfill, list_backfills,
        };

        let (app, seeded) = seeded_app(16).await;
        let evaluator = check(&app, &seeded).await;
        let rule = Uuid::now_v7();
        let project = seeded.project.id;
        let (done, running) = (Uuid::now_v7(), Uuid::now_v7());
        let now = app.state.clock.now_ns();
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    insert_rule(
                        conn,
                        NewRule {
                            id: rule,
                            project_id: project,
                            evaluator_id: evaluator,
                            target: "trace".to_owned(),
                            created_by: None,
                        },
                        RuleFields {
                            name: "Same".to_owned(),
                            filter: None,
                            variable_mapping: "{}".to_owned(),
                            sample_rate: 1.0,
                            budget_usd: None,
                            budget_period: None,
                            enabled: true,
                        },
                        now,
                    )
                    .await?;
                    let new = |id: Uuid| NewBackfill {
                        id,
                        rule_id: rule,
                        from_ns: now - 60_000_000_000,
                        to_ns: now - 20_000_000_000,
                        max_targets: 10,
                        sample_rate: 1.0,
                        created_by: None,
                    };
                    insert_backfill(conn, new(done), now).await?;
                    finish_backfill(conn, done, BackfillEnd::Done, now).await?;
                    insert_backfill(conn, new(running), now).await?;
                    Ok::<_, MetaError>(())
                })
            })
            .await
            .expect("a rule and two backfills");

        crate::eval::recover(&app.state).await.expect("recover");
        let rows = list_backfills(app.state.meta.reader(), rule)
            .await
            .expect("list");
        let find = |id: Uuid| rows.iter().find(|row| row.id == id).expect("listed");
        let interrupted = find(running);
        assert_eq!(interrupted.status, "failed");
        assert_eq!(interrupted.error.as_deref(), Some("interrupted"));
        assert!(interrupted.finished_at.is_some());
        assert_eq!(find(done).status, "done");
    }

    #[tokio::test]
    async fn shutdown_drains_then_aborts() {
        let (app, seeded) = seeded_app(16).await;
        let trace_id = trace(0x1d);
        store_trace(&app, &seeded, trace_id).await;
        let mock =
            MockLlm::start_with_delay(vec![completion(JUDGE_ANSWER)], Duration::from_secs(10))
                .await;
        let connection = connection(&app, &seeded, &mock, 4, 300).await;
        let evaluator = judge(&app, &seeded, connection).await;
        let job = manual_job(&app, &seeded, evaluator, trace_id, MAPPING).await;

        let tasks = crate::eval::spawn(&app.state, EvalTiming::TEST);
        wait_for_status(&app, &seeded, job, "running").await;
        let started = Instant::now();
        tasks.shutdown(&app.state).await;
        let elapsed = started.elapsed();
        assert!(elapsed >= Duration::from_secs(2), "drained for {elapsed:?}");
        assert!(
            elapsed < Duration::from_secs(6),
            "stopped after {elapsed:?}"
        );
        assert_eq!(job_row(&app, &seeded, job).await.status, "running");
    }
}
