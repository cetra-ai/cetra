//! The soak's driver tasks (track R M7, R7.5): ingest, scores, deletions,
//! retention and queries, plus the candidate ring they share.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BinaryHeap, HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use arrow::array::RecordBatch;
use chrono::{DateTime, NaiveDate, Utc};
use llmobs_core::batch::{scores_to_record_batch, spans_to_record_batch};
use llmobs_core::enums::ScoreSource;
use llmobs_core::ids::{ProjectId, TraceId};
use llmobs_core::score::{Score, ScoreTarget, ScoreValue};
use llmobs_core::span::Span;
use llmobs_storage::error::AppendError;
use llmobs_storage::{
    Bucket, BucketWidth, Deletion, DeletionScope, DeletionStatus, ListScope, MetricsRequest,
    QueryContext, QueryOptions, ScoreListRequest, SessionListRequest, SessionSort, SortOrder,
    Storage, TimeWindow, TraceListRequest, TraceSort, metrics, score_list, session_list,
    trace_detail, trace_list,
};
use tokio::sync::watch;
use uuid::Uuid;

use super::config::{QueryKind, SoakConfig};
use super::failures::{FailureKind, Failures};
use super::registry::{Registry, near_midnight};
use super::sample::{QuerySample, now_ns};
use crate::common::payload::{self, GeneratedTrace, Payloads, span_payload_bytes};
use crate::common::rng::{Rng, mix};

/// Independent of the app choice (`mix(trace_seed) % APPS`): salted.
const SAMPLE_SALT: u64 = 0x5A3C_96E1_0F2B_7D48;
/// Completed traces kept per project for the deletion and the oracle.
const RING: usize = 10_000;
pub(crate) const QUERY_TIMEOUT: Duration = Duration::from_secs(30);
const HOUR: Duration = Duration::from_secs(3600);
const HOUR_NS: i64 = 3_600 * 1_000_000_000;

/// Whether a trace is one the oracle checks (R7.6).
pub(crate) fn is_sampled(trace_seed: u64) -> bool {
    mix(trace_seed ^ SAMPLE_SALT).is_multiple_of(500)
}

/// The project with index `index`: `018f6b4e-7c1a-7c1a-8c1a-{0x50a + index}`.
pub(crate) fn project_id(index: usize) -> ProjectId {
    format!("018f6b4e-7c1a-7c1a-8c1a-{:012x}", 0x50a + index)
        .parse()
        .expect("a project id")
}

/// A trace id from the project index and the trace's counter.
fn trace_id(index: usize, counter: u64) -> TraceId {
    let mut bytes = [0_u8; 16];
    bytes[0] = index as u8 + 1;
    bytes[8..].copy_from_slice(&counter.to_be_bytes());
    TraceId::from_bytes(bytes).expect("a 16-byte trace id")
}

/// The project index for a weighted draw: project `i` has weight `2^(n-1-i)`.
pub(crate) fn pick_project(rng: &mut Rng, projects: usize) -> usize {
    let total = (1_usize << projects) - 1;
    let mut draw = rng.below(total);
    for index in 0..projects {
        let weight = 1_usize << (projects - 1 - index);
        if draw < weight {
            return index;
        }
        draw -= weight;
    }
    projects - 1
}

/// A completed trace: every group appended.
#[derive(Debug, Clone)]
pub(crate) struct Candidate {
    pub(crate) trace_id: TraceId,
    pub(crate) start: i64,
    pub(crate) user_id: Option<String>,
    pub(crate) session_id: Option<String>,
    pub(crate) sampled: bool,
}

#[derive(Debug, Default)]
struct CandidateState {
    rings: Vec<VecDeque<Candidate>>,
    fresh: Vec<Vec<Candidate>>,
}

/// The last `RING` complete traces per project, and those completed since the scores task last looked.
#[derive(Debug)]
pub(crate) struct Candidates {
    inner: Mutex<CandidateState>,
}

impl Candidates {
    pub(crate) fn new(projects: usize) -> Self {
        Self {
            inner: Mutex::new(CandidateState {
                rings: (0..projects).map(|_| VecDeque::new()).collect(),
                fresh: vec![Vec::new(); projects],
            }),
        }
    }

    fn lock(&self) -> MutexGuard<'_, CandidateState> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn push(&self, index: usize, candidate: Candidate) {
        let mut state = self.lock();
        if state.rings[index].len() == RING {
            state.rings[index].pop_front();
        }
        state.fresh[index].push(candidate.clone());
        state.rings[index].push_back(candidate);
    }

    pub(crate) fn take_fresh(&self, index: usize) -> Vec<Candidate> {
        std::mem::take(&mut self.lock().fresh[index])
    }

    /// A random candidate of a project, for a detail query.
    pub(crate) fn pick_any(&self, index: usize, rng: &mut Rng) -> Option<Candidate> {
        let state = self.lock();
        let ring = &state.rings[index];
        (!ring.is_empty()).then(|| ring[rng.below(ring.len())].clone())
    }

    /// `k` candidates of a project, at least `ceil(k / 2)` of them sampled when
    /// the ring has that many. Ids may repeat.
    pub(crate) fn pick_for_deletion(
        &self,
        index: usize,
        k: usize,
        rng: &mut Rng,
    ) -> Vec<Candidate> {
        let state = self.lock();
        let ring = &state.rings[index];
        let sampled: Vec<&Candidate> = ring.iter().filter(|c| c.sampled).collect();
        let mut picked = Vec::with_capacity(k);
        let want_sampled = k.div_ceil(2).min(sampled.len());
        for _ in 0..want_sampled {
            picked.push(sampled[rng.below(sampled.len())].clone());
        }
        while picked.len() < k && !ring.is_empty() {
            picked.push(ring[rng.below(ring.len())].clone());
        }
        picked
    }

    /// A user with a candidate, from the sampled candidates when there are any,
    /// and every candidate of that user in the ring.
    pub(crate) fn pick_user(
        &self,
        index: usize,
        rng: &mut Rng,
    ) -> Option<(String, Vec<Candidate>)> {
        let state = self.lock();
        let ring = &state.rings[index];
        let with_user: Vec<&Candidate> = ring.iter().filter(|c| c.user_id.is_some()).collect();
        let sampled: Vec<&Candidate> = with_user.iter().copied().filter(|c| c.sampled).collect();
        let pool = if sampled.is_empty() {
            with_user
        } else {
            sampled
        };
        if pool.is_empty() {
            return None;
        }
        let user = pool[rng.below(pool.len())].user_id.clone()?;
        let traces = ring
            .iter()
            .filter(|c| c.user_id.as_deref() == Some(user.as_str()))
            .cloned()
            .collect();
        Some((user, traces))
    }
}

/// Counters every task bumps; the sampler reads them.
#[derive(Debug, Default)]
pub(crate) struct Counters {
    pub(crate) appended_spans: AtomicU64,
    pub(crate) late_spans: AtomicU64,
    pub(crate) appended_scores: AtomicU64,
    pub(crate) sampled_traces: AtomicU64,
    pub(crate) backpressure: AtomicU64,
    pub(crate) backpressure_wait_ms: AtomicU64,
    pub(crate) generated_spans: AtomicU64,
    pub(crate) payload_bytes: AtomicU64,
}

#[derive(Debug, Default)]
pub(crate) struct QueryStats {
    latencies: BTreeMap<QueryKind, Vec<f64>>,
    errors: BTreeMap<QueryKind, u64>,
}

#[derive(Debug, Default)]
pub(crate) struct DeletionStats {
    /// Registered and not yet seen as applied: id and registration time.
    pub(crate) pending: Vec<(Uuid, Instant)>,
    /// Seconds from registration to the poll that first saw `Applied`.
    pub(crate) lags_s: Vec<f64>,
    pub(crate) registered: u64,
}

/// What every task shares.
pub(crate) struct Shared {
    pub(crate) storage: Arc<Storage>,
    pub(crate) payloads: Arc<Payloads>,
    pub(crate) config: SoakConfig,
    pub(crate) failures: Failures,
    pub(crate) counters: Counters,
    pub(crate) candidates: Candidates,
    /// Held by a deletion or retention call and by the oracle's check (R7.6).
    pub(crate) gate: tokio::sync::Mutex<()>,
    pub(crate) queries: Mutex<QueryStats>,
    pub(crate) deletions: Mutex<DeletionStats>,
    /// (files, bytes) removed by each retention call.
    pub(crate) retention: Mutex<Vec<(u64, u64)>>,
    /// The sampled traces and what storage must show for each (R7.6).
    pub(crate) registry: Mutex<Registry>,
}

impl Shared {
    pub(crate) fn new(storage: Arc<Storage>, payloads: Arc<Payloads>, config: SoakConfig) -> Self {
        let projects = config.projects;
        Self {
            storage,
            payloads,
            config,
            failures: Failures::default(),
            counters: Counters::default(),
            candidates: Candidates::new(projects),
            gate: tokio::sync::Mutex::new(()),
            queries: Mutex::new(QueryStats::default()),
            deletions: Mutex::new(DeletionStats::default()),
            retention: Mutex::new(Vec::new()),
            registry: Mutex::new(Registry::default()),
        }
    }

    /// The query latencies and errors since the last call, per kind name.
    pub(crate) fn take_query_stats(&self) -> BTreeMap<String, QuerySample> {
        let mut stats = self.queries.lock().unwrap_or_else(PoisonError::into_inner);
        let latencies = std::mem::take(&mut stats.latencies);
        let errors = std::mem::take(&mut stats.errors);
        QueryKind::ALL
            .into_iter()
            .map(|kind| {
                let mut values = latencies.get(&kind).cloned().unwrap_or_default();
                values.sort_by(f64::total_cmp);
                let sample = QuerySample {
                    n: values.len() as u64,
                    p50_ms: percentile(&values, 0.50),
                    p95_ms: percentile(&values, 0.95),
                    max_ms: values.last().copied().unwrap_or(0.0),
                    errors: errors.get(&kind).copied().unwrap_or(0),
                };
                (kind.name().to_owned(), sample)
            })
            .collect()
    }
}

fn percentile(sorted: &[f64], fraction: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let index = ((sorted.len() - 1) as f64 * fraction).round() as usize;
    sorted[index]
}

/// Appends `batch`, waiting out `Backpressure` (10 ms steps). Counts the retries and the wait.
async fn append_with_retry(
    shared: &Shared,
    spans: bool,
    project: ProjectId,
    batch: RecordBatch,
) -> Result<(), AppendError> {
    loop {
        let result = if spans {
            shared.storage.append_spans(project, batch.clone()).await
        } else {
            shared.storage.append_scores(project, batch.clone()).await
        };
        match result {
            Ok(_) => return Ok(()),
            Err(AppendError::Backpressure) => {
                shared.counters.backpressure.fetch_add(1, Ordering::Relaxed);
                let waited = Instant::now();
                tokio::time::sleep(Duration::from_millis(10)).await;
                shared
                    .counters
                    .backpressure_wait_ms
                    .fetch_add(waited.elapsed().as_millis() as u64, Ordering::Relaxed);
            }
            Err(error) => return Err(error),
        }
    }
}

/// A late group waiting for its due time, and the candidate it completes.
struct LateGroup {
    spans: Vec<Span>,
    complete: Vec<Candidate>,
}

/// Appends one batch of spans and, once it is acknowledged, counts it, updates
/// the registry and publishes the traces it completes.
async fn flush_spans(
    shared: &Shared,
    index: usize,
    mut batch: Vec<Span>,
    first: u64,
    late: u64,
    complete: Vec<Candidate>,
) {
    if batch.is_empty() {
        return;
    }
    let ingest = now_ns();
    for span in &mut batch {
        span.ingest_time = ingest;
    }
    let rows = match spans_to_record_batch(&batch) {
        Ok(rows) => rows,
        Err(error) => {
            shared
                .failures
                .record(FailureKind::Append, format!("project {index}: {error}"));
            return;
        }
    };
    match append_with_retry(shared, true, project_id(index), rows).await {
        Ok(()) => {
            shared
                .counters
                .appended_spans
                .fetch_add(first, Ordering::Relaxed);
            shared
                .counters
                .late_spans
                .fetch_add(late, Ordering::Relaxed);
            {
                let mut registry = shared
                    .registry
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                for span in &batch {
                    registry.touch(index, span.trace_id, ingest);
                }
                for candidate in &complete {
                    registry.complete(index, candidate.trace_id);
                }
            }
            for candidate in complete {
                if candidate.sampled {
                    shared
                        .counters
                        .sampled_traces
                        .fetch_add(1, Ordering::Relaxed);
                }
                shared.candidates.push(index, candidate);
            }
        }
        Err(error) => shared.failures.record(
            FailureKind::Append,
            format!("spans of project {index}: {error}"),
        ),
    }
}

/// Ingest for one project: traces at the project's share of the rate, with their
/// late groups appended when due.
pub(crate) async fn ingest(
    shared: Arc<Shared>,
    index: usize,
    rate: f64,
    stop: watch::Receiver<bool>,
) {
    let project = project_id(index);
    let mut interval = tokio::time::interval(Duration::from_millis(100));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut budget = 0.0_f64;
    let mut counter = 0_u64;
    let mut late_seq = 0_u64;
    let mut next: Option<(u64, GeneratedTrace)> = None;
    let mut due: BinaryHeap<Reverse<(Instant, u64)>> = BinaryHeap::new();
    let mut late: HashMap<u64, LateGroup> = HashMap::new();
    while !*stop.borrow() {
        interval.tick().await;
        budget += rate * 0.1;
        let mut batch: Vec<Span> = Vec::new();
        let mut first_spans = 0_u64;
        let mut late_spans = 0_u64;
        let mut complete: Vec<Candidate> = Vec::new();

        let now = Instant::now();
        while let Some(Reverse((at, id))) = due.peek().copied() {
            if at > now {
                break;
            }
            due.pop();
            if let Some(group) = late.remove(&id) {
                late_spans += group.spans.len() as u64;
                batch.extend(group.spans);
                complete.extend(group.complete);
            }
        }

        loop {
            let (seed, generated) = match next.take() {
                Some(pending) => pending,
                None => {
                    let id = counter;
                    counter += 1;
                    let seed = mix(id ^ ((index as u64) << 56));
                    let generated = payload::trace(
                        &shared.payloads,
                        project,
                        trace_id(index, id),
                        seed,
                        now_ns(),
                    );
                    (seed, generated)
                }
            };
            let first_len = generated.first.len() as f64;
            if budget < first_len {
                next = Some((seed, generated));
                break;
            }
            budget -= first_len;
            let spans_total = (generated.first.len() + generated.late.len()) as u64;
            let bytes: usize = generated
                .first
                .iter()
                .chain(&generated.late)
                .map(span_payload_bytes)
                .sum();
            shared
                .counters
                .generated_spans
                .fetch_add(spans_total, Ordering::Relaxed);
            shared
                .counters
                .payload_bytes
                .fetch_add(bytes as u64, Ordering::Relaxed);
            let sampled = is_sampled(seed);
            let near_day_end = generated
                .first
                .iter()
                .chain(&generated.late)
                .any(|s| near_midnight(s.start_time) || near_midnight(s.end_time));
            if sampled && !near_day_end {
                shared
                    .registry
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .insert(index, generated.summary.clone(), generated.late.is_empty());
            }
            let candidate = Candidate {
                trace_id: generated.summary.trace_id,
                start: generated.summary.start_time,
                user_id: generated.summary.user_id.clone(),
                session_id: generated.summary.session_id.clone(),
                sampled,
            };
            first_spans += first_len as u64;
            batch.extend(generated.first);
            if generated.late.is_empty() {
                complete.push(candidate);
            } else {
                late_seq += 1;
                due.push(Reverse((now + generated.delay, late_seq)));
                late.insert(
                    late_seq,
                    LateGroup {
                        spans: generated.late,
                        complete: vec![candidate],
                    },
                );
            }
        }
        flush_spans(&shared, index, batch, first_spans, late_spans, complete).await;
    }

    // At stop, every pending late group is appended at once.
    let mut batch: Vec<Span> = Vec::new();
    let mut late_spans = 0_u64;
    let mut complete: Vec<Candidate> = Vec::new();
    while let Some(Reverse((_, id))) = due.pop() {
        if let Some(group) = late.remove(&id) {
            late_spans += group.spans.len() as u64;
            batch.extend(group.spans);
            complete.extend(group.complete);
        }
    }
    flush_spans(&shared, index, batch, 0, late_spans, complete).await;
}

/// A judge score on a trace, as the scores task writes it.
fn judge_score(project: ProjectId, candidate: &Candidate, now: i64, value: f64) -> Score {
    Score {
        project_id: project,
        score_id: Uuid::now_v7(),
        target: ScoreTarget::Trace {
            trace_id: candidate.trace_id,
        },
        session_id: candidate.session_id.clone(),
        user_id: candidate.user_id.clone(),
        name: "judge".to_owned(),
        value: ScoreValue::Numeric(value),
        reason: None,
        source: ScoreSource::LlmJudge,
        evaluator_id: None,
        rule_id: None,
        experiment_id: None,
        dataset_item_id: None,
        author_user_id: None,
        metadata: BTreeMap::new(),
        created_at: now,
        updated_at: now,
        deleted: false,
        ingest_time: now,
    }
}

/// Scores: one per completed trace with probability 0.2, then new versions and tombstones of recent ones.
pub(crate) async fn scores(shared: Arc<Shared>, stop: watch::Receiver<bool>) {
    let projects = shared.config.projects;
    let mut interval = tokio::time::interval(Duration::from_secs(1));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut rng = Rng::new(0x5C0_4E5);
    let mut recent: Vec<VecDeque<Score>> = (0..projects).map(|_| VecDeque::new()).collect();
    while !*stop.borrow() {
        interval.tick().await;
        let now = now_ns();
        for (index, ring) in recent.iter_mut().enumerate() {
            let mut batch: Vec<Score> = Vec::new();
            for candidate in shared.candidates.take_fresh(index) {
                if rng.unit() >= 0.2 {
                    continue;
                }
                let score = judge_score(project_id(index), &candidate, now, rng.unit());
                if ring.len() == RING {
                    ring.pop_front();
                }
                ring.push_back(score.clone());
                batch.push(score);
            }
            if !ring.is_empty() {
                let written = batch.len();
                for _ in 0..written {
                    if rng.unit() < 0.02 {
                        let mut version = ring[rng.below(ring.len())].clone();
                        version.updated_at = now;
                        batch.push(version);
                    }
                    if rng.unit() < 0.005 {
                        let mut tombstone = ring[rng.below(ring.len())].clone();
                        tombstone.updated_at = now;
                        tombstone.deleted = true;
                        batch.push(tombstone);
                    }
                }
            }
            if batch.is_empty() {
                continue;
            }
            let rows = match scores_to_record_batch(&batch) {
                Ok(rows) => rows,
                Err(error) => {
                    shared.failures.record(
                        FailureKind::Append,
                        format!("scores of project {index}: {error}"),
                    );
                    continue;
                }
            };
            let count = batch.len() as u64;
            match append_with_retry(&shared, false, project_id(index), rows).await {
                Ok(()) => {
                    shared
                        .counters
                        .appended_scores
                        .fetch_add(count, Ordering::Relaxed);
                }
                Err(error) => shared.failures.record(
                    FailureKind::Append,
                    format!("scores of project {index}: {error}"),
                ),
            }
        }
    }
}

/// A query context over `project`, with the `around` time hint when given.
pub(crate) async fn context(
    storage: &Storage,
    project: ProjectId,
    around: Option<i64>,
) -> Result<QueryContext, String> {
    let query = storage
        .query(project, QueryOptions { around })
        .await
        .map_err(|error| error.to_string())?;
    QueryContext::new(query, None).map_err(|error| error.to_string())
}

/// Registers one deletion under the gate. Right after it returns, up to five
/// covered traces must not be found.
async fn register(shared: &Shared, index: usize, scope: DeletionScope, covered: &[Candidate]) {
    let _gate = shared.gate.lock().await;
    let (listed, user) = match &scope {
        DeletionScope::Traces { trace_ids } => (trace_ids.clone(), None),
        DeletionScope::User {
            user_id, trace_ids, ..
        } => (trace_ids.clone(), Some(user_id.clone())),
    };
    let deletion = Deletion {
        id: Uuid::now_v7(),
        project: project_id(index),
        cutoff: now_ns(),
        scope,
    };
    let (id, project, cutoff) = (deletion.id, deletion.project, deletion.cutoff);
    if let Err(error) = shared.storage.register_deletion(deletion).await {
        shared
            .failures
            .record(FailureKind::Deletion, format!("register {id}: {error}"));
        return;
    }
    {
        let mut stats = shared
            .deletions
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        stats.pending.push((id, Instant::now()));
        stats.registered += 1;
    }
    shared
        .registry
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .apply_deletion(index, &listed, user.as_deref(), cutoff);
    for candidate in covered.iter().take(5) {
        let hex = candidate.trace_id.to_hex();
        let visible = match context(&shared.storage, project, Some(candidate.start)).await {
            Ok(ctx) => trace_detail(&ctx, &hex, QUERY_TIMEOUT)
                .await
                .map(|detail| detail.is_some())
                .map_err(|error| error.to_string()),
            Err(error) => Err(error),
        };
        match visible {
            Ok(false) => {}
            Ok(true) => shared.failures.record(
                FailureKind::DeletionVisible,
                format!("deletion {id}: trace {hex} is still visible"),
            ),
            Err(error) => shared.failures.record(
                FailureKind::Query,
                format!("visibility check {hex}: {error}"),
            ),
        }
    }
}

/// Deletions: a `Traces` deletion every 10 minutes, a `User` deletion every hour,
/// and the status of every pending one once a minute.
pub(crate) async fn deletions(shared: Arc<Shared>, stop: watch::Receiver<bool>) {
    let projects = shared.config.projects;
    let mut rng = Rng::new(0x00DE_1E7E);
    let started = Instant::now();
    let mut next_traces = started + Duration::from_secs(600);
    let mut next_users = started + HOUR;
    let mut next_status = started + Duration::from_secs(60);
    let mut interval = tokio::time::interval(Duration::from_secs(1));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    while !*stop.borrow() {
        interval.tick().await;
        let now = Instant::now();
        if now >= next_traces {
            next_traces += Duration::from_secs(600);
            let index = pick_project(&mut rng, projects);
            let k = 1 + rng.below(20);
            let picked = shared.candidates.pick_for_deletion(index, k, &mut rng);
            if !picked.is_empty() {
                let mut ids: Vec<TraceId> = picked.iter().map(|c| c.trace_id).collect();
                ids.sort();
                ids.dedup();
                let scope = DeletionScope::Traces {
                    trace_ids: ids.clone(),
                };
                register(&shared, index, scope, &picked).await;
            }
        }
        if now >= next_users {
            next_users += HOUR;
            let index = pick_project(&mut rng, projects);
            if let Some((user_id, traces)) = shared.candidates.pick_user(index, &mut rng) {
                let mut trace_ids: Vec<TraceId> = traces.iter().map(|c| c.trace_id).collect();
                trace_ids.extend(
                    shared
                        .registry
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .user_trace_ids(index, &user_id),
                );
                trace_ids.sort();
                trace_ids.dedup();
                let mut session_ids: Vec<String> =
                    traces.iter().filter_map(|c| c.session_id.clone()).collect();
                session_ids.sort();
                session_ids.dedup();
                let scope = DeletionScope::User {
                    user_id,
                    trace_ids,
                    session_ids,
                };
                register(&shared, index, scope, &traces).await;
            }
        }
        if now >= next_status {
            next_status += Duration::from_secs(60);
            poll_status(&shared).await;
        }
    }
}

/// Polls every pending deletion once; each that is `Applied` records its lag.
pub(crate) async fn poll_status(shared: &Shared) {
    let pending = shared
        .deletions
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .pending
        .clone();
    for (id, registered) in pending {
        match shared.storage.deletion_status(id).await {
            Ok(DeletionStatus::Applied { .. }) => {
                let mut stats = shared
                    .deletions
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                stats.pending.retain(|(pending_id, _)| *pending_id != id);
                stats.lags_s.push(registered.elapsed().as_secs_f64());
            }
            Ok(_) => {}
            Err(error) => shared
                .failures
                .record(FailureKind::Deletion, format!("status {id}: {error}")),
        }
    }
}

/// The lagged retention boundary: `(now − 4 h)` in UTC, minus `retention_days`.
/// The lag lets a day close (spec 03 §5) before retention drops it.
pub(crate) fn keep_from(now: DateTime<Utc>, retention_days: u32) -> NaiveDate {
    (now - chrono::TimeDelta::hours(4))
        .date_naive()
        .checked_sub_days(chrono::Days::new(u64::from(retention_days)))
        .expect("the retention date is in range")
}

/// Retention: once at start, then every hour, for every project, under the gate.
pub(crate) async fn retention(shared: Arc<Shared>, stop: watch::Receiver<bool>) {
    let projects = shared.config.projects;
    loop {
        let keep = keep_from(Utc::now(), shared.config.retention_days);
        for index in 0..projects {
            let _gate = shared.gate.lock().await;
            match shared
                .storage
                .apply_retention(project_id(index), keep)
                .await
            {
                Ok(report) => {
                    shared
                        .registry
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .apply_retention(index, keep);
                    shared
                        .retention
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .push((report.files, report.bytes));
                }
                Err(error) => shared
                    .failures
                    .record(FailureKind::Retention, format!("project {index}: {error}")),
            }
        }
        for _ in 0..3600 {
            if *stop.borrow() {
                return;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }
}

/// A query kind drawn by the mix weights.
fn pick_kind(weights: &[(QueryKind, u32)], rng: &mut Rng) -> QueryKind {
    let total: u32 = weights.iter().map(|(_, weight)| weight).sum();
    let mut draw = rng.below(total as usize) as u32;
    for (kind, weight) in weights {
        if draw < *weight {
            return *kind;
        }
        draw -= weight;
    }
    weights[0].0
}

/// Runs one query. `Ok(false)` when there was nothing to query (no candidate yet).
async fn run_query(
    shared: &Shared,
    kind: QueryKind,
    index: usize,
    rng: &mut Rng,
) -> Result<bool, String> {
    let project = project_id(index);
    let now = now_ns();
    let hour = TimeWindow::new(now - HOUR_NS, now).map_err(|error| error.to_string())?;
    let day = TimeWindow::new(now - 24 * HOUR_NS, now).map_err(|error| error.to_string())?;
    let timeout = QUERY_TIMEOUT;
    match kind {
        QueryKind::List | QueryKind::Filter => {
            let ctx = context(&shared.storage, project, None).await?;
            let filter = (kind == QueryKind::Filter).then_some("cost_total > 0.015");
            trace_list(
                &ctx,
                TraceListRequest {
                    filter,
                    search: None,
                    scope: ListScope {
                        window: Some(hour),
                        include_eval: false,
                        include_experiments: false,
                    },
                    sort: TraceSort::StartTime,
                    order: SortOrder::Desc,
                    limit: 50,
                    cursor: None,
                    timeout,
                },
            )
            .await
            .map_err(|error| error.to_string())?;
        }
        QueryKind::Detail => {
            let Some(candidate) = shared.candidates.pick_any(index, rng) else {
                return Ok(false);
            };
            let ctx = context(&shared.storage, project, Some(candidate.start)).await?;
            trace_detail(&ctx, &candidate.trace_id.to_hex(), timeout)
                .await
                .map_err(|error| error.to_string())?;
        }
        QueryKind::Sessions => {
            let ctx = context(&shared.storage, project, None).await?;
            session_list(
                &ctx,
                SessionListRequest {
                    filter: None,
                    scope: ListScope {
                        window: Some(hour),
                        include_eval: false,
                        include_experiments: false,
                    },
                    sort: SessionSort::StartTime,
                    order: SortOrder::Desc,
                    limit: 50,
                    cursor: None,
                    timeout,
                },
            )
            .await
            .map_err(|error| error.to_string())?;
        }
        QueryKind::Metrics => {
            let ctx = context(&shared.storage, project, None).await?;
            metrics(
                &ctx,
                MetricsRequest {
                    window: day,
                    bucket: Bucket::Fixed(BucketWidth::H1),
                    filter: None,
                    group_by: None,
                    include_eval: false,
                    include_experiments: true,
                    timeout,
                },
            )
            .await
            .map_err(|error| error.to_string())?;
        }
        QueryKind::Scores => {
            let ctx = context(&shared.storage, project, None).await?;
            score_list(
                &ctx,
                ScoreListRequest {
                    target: None,
                    trace_id: None,
                    span_id: None,
                    session_id: None,
                    name: None,
                    source: None,
                    window: Some(hour),
                    limit: 50,
                    cursor: None,
                    timeout,
                },
            )
            .await
            .map_err(|error| error.to_string())?;
        }
    }
    Ok(true)
}

/// The sampled-trace oracle: 200 live traces and up to 50 deleted and 50 retained, hourly.
pub(crate) async fn oracle(shared: Arc<Shared>, stop: watch::Receiver<bool>) {
    let mut round = 0_u64;
    loop {
        for _ in 0..3600 {
            if *stop.borrow() {
                return;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        round += 1;
        super::registry::check_sample(
            &shared.storage,
            &shared.registry,
            &shared.gate,
            &shared.failures,
            200,
            50,
            round,
        )
        .await;
    }
}

/// Queries at `qps` per second, each kind by the mix weights, each project by rate share.
pub(crate) async fn queries(shared: Arc<Shared>, stop: watch::Receiver<bool>) {
    let mut rng = Rng::new(0x9_0E_41);
    let projects = shared.config.projects;
    let mut interval = tokio::time::interval(Duration::from_secs_f64(1.0 / shared.config.qps));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    while !*stop.borrow() {
        interval.tick().await;
        let kind = pick_kind(&shared.config.mix, &mut rng);
        let index = pick_project(&mut rng, projects);
        let started = Instant::now();
        let outcome = run_query(&shared, kind, index, &mut rng).await;
        let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
        match outcome {
            Ok(false) => {}
            Ok(true) => {
                let mut stats = shared
                    .queries
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                stats.latencies.entry(kind).or_default().push(elapsed_ms);
            }
            Err(error) => {
                {
                    let mut stats = shared
                        .queries
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner);
                    stats.latencies.entry(kind).or_default().push(elapsed_ms);
                    *stats.errors.entry(kind).or_default() += 1;
                }
                shared
                    .failures
                    .record(FailureKind::Query, format!("{}: {error}", kind.name()));
            }
        }
    }
}
