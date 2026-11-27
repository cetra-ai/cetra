//! Track R M6: the model test with background tasks running and reader threads checking every
//! read against the model states it may have seen (docs/plans/r-hardening/06-m6-concurrency.md
//! § The rule).
//!
//! A case cuts its ops into segments at the barrier ops (`Restart`, `Crash`, `LossyCrash`,
//! `Settle`). Inside a segment every lane's ops run on a driver thread, while reader threads run
//! reads and a GC thread collects obsolete files. Between segments nothing else runs: a barrier
//! changes the life, and the checks of M1 run on the new one.

use std::collections::HashSet;
use std::collections::VecDeque;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::Write as _;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering::SeqCst};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use bytesize::ByteSize;
use proptest::prelude::*;
use uuid::Uuid;

use super::check::{
    Observed, Read, Strictness, check_full, check_quiet_state, check_settled_files, compare, days,
    observe,
};
use super::harness::{Harness, HarnessOptions, concurrent_tweaks, memory_store};
use super::model::Model;
use super::observe::ReadFailure;
use super::ops::{
    Concurrency, MILLI_NS, Op, Passes, SPANS, StepEvent, TRACES, all_deletions_applied, apply,
    apply_storage, drop_day_closed_scores, flushed, keep_newest_copies, new_model, ops_for, send,
    settle,
};
use super::shape::{HOUR_NS, P, day, midnight, span};
use crate::config::StorageConfig;
use crate::deletion::DeletionStatus;
use crate::hooks::HookPoint;
use crate::reads::AlertMeasure;
use crate::test_support::statam_rows;
use crate::wal::Table;

/// Tokio worker threads of a concurrent life: enough for the drivers' storage calls and the
/// readers' queries at once.
const WORKERS: usize = 4;

/// Passes of each settle step (M1's `SETTLE_PASSES`).
const PASSES: usize = 10;

/// How long a reader or a driver waits for a state before it skips the read, in total.
const WAIT_LIMIT: Duration = Duration::from_secs(1);

/// How often a waiting thread checks the stop flag.
const WAIT_TICK: Duration = Duration::from_millis(100);

/// How long one item of a thread may run before the watchdog fails the run (R6.5).
const STUCK_AFTER: Duration = Duration::from_secs(30);

/// How long the watchdog gives stopped threads to finish before it aborts the process.
const ABORT_AFTER: Duration = Duration::from_secs(10);

/// How long a background job may keep its claims after the settle passes (`settle_concurrent`).
const CLAIMS_WAIT: Duration = Duration::from_secs(30);

/// After the drivers stop, the readers keep reading for this long, and until each of them has
/// matched `TAIL_READS` reads: the reads of a quiet lane, with the background work still running.
const TAIL: Duration = Duration::from_millis(300);
const TAIL_READS: u64 = 20;

/// The most time the readers may spend on the tail, however few reads they match.
const TAIL_LIMIT: Duration = Duration::from_secs(10);

/// Parameters of one concurrent case.
#[derive(Debug, Clone)]
pub(super) struct ConcurrentOptions {
    /// Reader threads per segment.
    pub(super) readers: usize,
    /// Pause after each op of a lane, so background work overlaps the ops.
    pub(super) pace: Duration,
    /// The most a lane's driver holds a step's state back after its storage call returned: a
    /// random pause in `0..=step_hold`. While it holds, the step is in flight, so a read may
    /// match the state the step published before it (§ The rule).
    pub(super) step_hold: Duration,
    /// Sleep between two collections of the GC thread.
    pub(super) gc_every: Duration,
    /// States kept per lane; older ones are dropped (stress runs).
    pub(super) history_keep: usize,
    /// The case's objects live in an in-memory object store, with the read cache of
    /// [`concurrent_remote_tweaks`] (R6.6).
    pub(super) remote: bool,
}

/// What a lane shares with the readers. The driver publishes every step here.
pub(super) struct Lane {
    log: Mutex<LaneLog>,
    grew: Condvar,
    /// The number of steps whose storage call returned, so the last state of `log` is `S_published`.
    published: AtomicUsize,
    /// Set just before a step's storage call, cleared once its state is published.
    in_flight: AtomicBool,
    /// Deletions a read has seen `Applied`, for the rule that a later read never sees `Pending`.
    applied_seen: Mutex<HashSet<Uuid>>,
}

struct LaneLog {
    /// `states[i]` is `S_{base + i}`.
    states: VecDeque<Arc<Model>>,
    base: usize,
    /// No step is in flight and none will come in this segment.
    idle: bool,
}

/// Why `Lane::states` returned no states.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Miss {
    /// The state the read needs is older than the kept history.
    Stale,
    /// The step in flight did not return within `WAIT_LIMIT`.
    WaitLimit,
    /// The run is stopping.
    Stopped,
}

/// What a lane's driver owns.
pub(super) struct LaneState {
    pub(super) shared: Arc<Lane>,
    pub(super) model: Model,
    pub(super) clock: i64,
}

/// Counters of a case, printed when it passes.
#[derive(Debug, Default)]
pub(super) struct Stats {
    pub(super) steps: AtomicU64,
    /// Matched reads per kind (index: `Kind as usize`).
    pub(super) reads: [AtomicU64; 15],
    pub(super) busy: AtomicU64,
    /// Reads whose `lo` was below the kept history (stress only).
    pub(super) stale: AtomicU64,
    /// `states` calls that hit the wait limit.
    pub(super) wait_limit: AtomicU64,
    /// Reads that matched a state other than `S_lo`.
    pub(super) matched_later: AtomicU64,
    /// Obsolete files the collections deleted, and those still in use by a snapshot.
    pub(super) gc_deleted: AtomicU64,
    pub(super) gc_in_use: AtomicU64,
    /// Explicit compaction and deletion passes run, the ones that reported lost jobs, and the
    /// jobs they lost. A concurrent run tolerates lost jobs; it must not lose them all.
    pub(super) passes: AtomicU64,
    pub(super) lost_passes: AtomicU64,
    pub(super) lost_jobs: AtomicU64,
}

impl Stats {
    /// Adds the explicit passes of one `apply_storage` call.
    fn add_passes(&self, passes: Passes) {
        self.passes.fetch_add(passes.run, SeqCst);
        self.lost_passes.fetch_add(passes.lost_passes, SeqCst);
        self.lost_jobs.fetch_add(passes.lost_jobs, SeqCst);
    }

    /// Matched reads, over every kind.
    pub(super) fn matched_reads(&self) -> u64 {
        self.reads.iter().map(|count| count.load(SeqCst)).sum()
    }
}

/// The flags and counters that the threads of a run share.
#[derive(Clone, Copy)]
struct Run<'a> {
    /// Set to wind the threads down: when the run ends, or when one of them failed.
    stop: &'a AtomicBool,
    /// Set only when a thread failed (it panicked, or the watchdog stopped a stuck run). A read
    /// that mismatches after a normal stop is still a finding; after a failure it may be the
    /// victim of the stopped run.
    failed: &'a AtomicBool,
    stats: &'a Stats,
}

impl Lane {
    /// `states = [model]`, base 0, not idle.
    pub(super) fn new(model: &Model) -> Arc<Lane> {
        Arc::new(Lane {
            log: Mutex::new(LaneLog {
                states: VecDeque::from([Arc::new(model.clone())]),
                base: 0,
                idle: false,
            }),
            grew: Condvar::new(),
            published: AtomicUsize::new(0),
            in_flight: AtomicBool::new(false),
            applied_seen: Mutex::new(HashSet::new()),
        })
    }

    /// Publishes the state after a step: the step's call returned and the model is updated.
    pub(super) fn push(&self, model: &Model, keep: usize) {
        let mut log = lock(&self.log);
        log.states.push_back(Arc::new(model.clone()));
        while log.states.len() > keep {
            log.states.pop_front();
            log.base += 1;
        }
        self.published
            .store(log.base + log.states.len() - 1, SeqCst);
        self.in_flight.store(false, SeqCst);
        drop(log);
        self.grew.notify_all();
    }

    /// The states `lo..=hi`, waiting while `hi` is not in the history and the lane is not idle.
    /// Fails with `Stale` when `lo` is below the history, `WaitLimit` after `WAIT_LIMIT`, and
    /// `Stopped` when `stop` is set while it waits.
    pub(super) fn states(
        &self,
        lo: usize,
        hi: usize,
        stop: &AtomicBool,
    ) -> Result<Vec<Arc<Model>>, Miss> {
        let started = Instant::now();
        let mut log = lock(&self.log);
        while !log.idle && log.base + log.states.len() <= hi {
            if stop.load(SeqCst) {
                return Err(Miss::Stopped);
            }
            if started.elapsed() >= WAIT_LIMIT {
                return Err(Miss::WaitLimit);
            }
            log = self
                .grew
                .wait_timeout(log, WAIT_TICK)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        if lo < log.base {
            return Err(Miss::Stale);
        }
        let last = hi.min(log.base + log.states.len() - 1);
        if lo > last {
            return Ok(Vec::new());
        }
        Ok(log
            .states
            .range(lo - log.base..=last - log.base)
            .cloned()
            .collect())
    }

    /// Steps whose storage call has returned: the index of the lane's last published state.
    pub(super) fn steps(&self) -> usize {
        self.published.load(SeqCst)
    }

    /// Marks the lane idle (no step will come) or busy again, and wakes the waiting readers.
    pub(super) fn set_idle(&self, idle: bool) {
        lock(&self.log).idle = idle;
        self.grew.notify_all();
    }
}

/// Locks `mutex`. A panic elsewhere while it was held leaves the data usable: the run fails with
/// that panic anyway.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A 64-bit xorshift, seeded per reader so a run's read mix is reproducible.
struct XorShift(u64);

impl XorShift {
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    /// A value in `0..n`; `n` is not 0.
    fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n
    }
}

/// The kinds of read a reader thread runs, with their weights (the order is `Kind as usize`).
#[derive(Debug, Clone, Copy)]
pub(super) enum Kind {
    Spans,
    Traces,
    TraceDetail,
    SpanDetail,
    Scores,
    ScoresById,
    Latest,
    Windowed,
    NoEval,
    Sessions,
    Users,
    Metrics,
    FilterTraceIds,
    AlertValues,
    DeletionStatus,
}

pub(super) const KINDS: [(Kind, u64); 15] = [
    (Kind::Spans, 3),
    (Kind::Traces, 3),
    (Kind::TraceDetail, 2),
    (Kind::SpanDetail, 2),
    (Kind::Scores, 2),
    (Kind::ScoresById, 1),
    (Kind::Latest, 1),
    (Kind::Windowed, 1),
    (Kind::NoEval, 1),
    (Kind::Sessions, 1),
    (Kind::Users, 1),
    (Kind::Metrics, 1),
    (Kind::FilterTraceIds, 1),
    (Kind::AlertValues, 1),
    (Kind::DeletionStatus, 1),
];

/// The windows of `check_aggregates`, for the trace KPIs and `filter_trace_ids`.
const METRIC_DAYS: [(u8, u8); 2] = [(0, 4), (2, 4)];

/// The windows of `check_aggregates`, for `alert_value`.
const ALERT_DAYS: [(u8, u8); 2] = [(0, 4), (1, 2)];

/// One thread of a segment, for the watchdog: its name and the item it runs.
struct Thread {
    name: String,
    item: Mutex<Option<(String, Instant)>>,
}

impl Thread {
    fn new(name: String) -> Self {
        Self {
            name,
            item: Mutex::new(None),
        }
    }

    fn start(&self, item: String) {
        *lock(&self.item) = Some((item, Instant::now()));
    }

    fn idle(&self) {
        *lock(&self.item) = None;
    }
}

/// Runs on exit of a thread, also when it unwinds: it wakes the lane's readers (the driver's
/// lane is idle, so a step left in flight by a panic cannot block them), counts the thread as
/// finished, and stops the run when the thread panicked.
struct Exit<'a> {
    thread: &'a Thread,
    finished: &'a AtomicUsize,
    lane: Option<Arc<Lane>>,
    run: Run<'a>,
}

impl Drop for Exit<'_> {
    fn drop(&mut self) {
        if let Some(lane) = &self.lane {
            lane.set_idle(true);
        }
        if thread::panicking() {
            self.run.failed.store(true, SeqCst);
            self.run.stop.store(true, SeqCst);
        }
        self.thread.idle();
        self.finished.fetch_add(1, SeqCst);
    }
}

/// Publishes one step event of a lane: `Begin` marks the call in flight; `Done` holds the state
/// back for a random pause in `0..=step_hold` (the call has returned and published, `in_flight`
/// is still set and `published` is still the step before, so a read may match the state the
/// step published), then pushes it.
fn publish(
    lane: &Lane,
    event: StepEvent<'_>,
    keep: usize,
    stats: &Stats,
    step_hold: Duration,
    rng: &mut XorShift,
) {
    match event {
        StepEvent::Begin => lane.in_flight.store(true, SeqCst),
        StepEvent::Done(model) => {
            let max = u64::try_from(step_hold.as_nanos()).unwrap_or(u64::MAX);
            if !step_hold.is_zero() {
                thread::sleep(Duration::from_nanos(rng.below(max.saturating_add(1))));
            }
            lane.push(model, keep);
            stats.steps.fetch_add(1, SeqCst);
        }
    }
}

/// One lane's ops, on its driver thread: each op runs with the step callback of the lane, then
/// the pause.
#[allow(clippy::too_many_arguments)] // the run's shared flags, the exit counter and the hold's seed
fn driver(
    h: &Harness,
    lane: &mut LaneState,
    ops: &[Op],
    options: &ConcurrentOptions,
    run: Run<'_>,
    thread: &Thread,
    finished: &AtomicUsize,
    rng_seed: u64,
) {
    let _exit = Exit {
        thread,
        finished,
        lane: Some(Arc::clone(&lane.shared)),
        run,
    };
    let mut rng = XorShift(rng_seed.max(1));
    for (index, op) in ops.iter().enumerate() {
        if run.stop.load(SeqCst) {
            return;
        }
        thread.start(format!("op {index} {op:?}"));
        lane.model.ops_applied += 1;
        let passes = apply_storage(
            op,
            h,
            &mut lane.model,
            &mut lane.clock,
            Concurrency::Concurrent,
            &mut |event| {
                publish(
                    &lane.shared,
                    event,
                    options.history_keep,
                    run.stats,
                    options.step_hold,
                    &mut rng,
                );
            },
        );
        run.stats.add_passes(passes);
        thread.idle();
        thread::sleep(options.pace);
    }
}

/// The kind of the next read, by weight.
fn pick_kind(rng: &mut XorShift) -> Kind {
    let total: u64 = KINDS.iter().map(|(_, weight)| weight).sum();
    let mut roll = rng.below(total);
    for (kind, weight) in KINDS {
        if roll < weight {
            return kind;
        }
        roll -= weight;
    }
    unreachable!("the roll is below the total weight")
}

/// The read of `kind` on `project`, with its inputs chosen from `model`, the state the reader
/// read at `lo`. `DeletionStatus` is not a `Read`: the caller checks it.
fn build_read(kind: Kind, project: P, model: &Model, rng: &mut XorShift) -> Read {
    let trace = |rng: &mut XorShift| u8::try_from(rng.below(u64::from(TRACES))).unwrap_or(0);
    let span_no = |rng: &mut XorShift| u8::try_from(rng.below(u64::from(SPANS))).unwrap_or(0);
    match kind {
        Kind::Spans => Read::Spans(project),
        Kind::Traces => Read::Traces(project),
        Kind::TraceDetail => Read::TraceDetail(project, trace(rng)),
        Kind::SpanDetail => {
            let picked = trace(rng);
            Read::SpanDetail(project, picked, span_no(rng))
        }
        Kind::Scores => Read::Scores(project),
        Kind::ScoresById => Read::ScoresById(project, model.score_ids(project)),
        Kind::Latest => Read::Latest(project),
        Kind::Windowed => Read::Windowed(project),
        Kind::NoEval => Read::NoEval(project),
        Kind::Sessions => Read::Sessions(project),
        Kind::Users => Read::Users(project),
        Kind::Metrics => Read::Metrics(project, pick_window(rng, &METRIC_DAYS)),
        Kind::FilterTraceIds => Read::FilterTraceIds(project, pick_window(rng, &METRIC_DAYS)),
        Kind::AlertValues => {
            let window = pick_window(rng, &ALERT_DAYS);
            Read::AlertValue(project, window, pick_measure(rng))
        }
        Kind::DeletionStatus => unreachable!("a deletion status is checked by the reader"),
    }
}

/// One of the `windows` (whole days), as `days` gives it.
fn pick_window(rng: &mut XorShift, windows: &[(u8, u8)]) -> (i64, i64) {
    let index = usize::try_from(rng.below(windows.len() as u64)).unwrap_or(0);
    let (from, to) = windows[index];
    days(from, to)
}

/// One of the measures of `check_aggregates`: the two trace measures and three score averages.
fn pick_measure(rng: &mut XorShift) -> AlertMeasure {
    match rng.below(5) {
        0 => AlertMeasure::ErrorRate,
        1 => AlertMeasure::Cost,
        k => AlertMeasure::ScoreAvg {
            name: format!("n{}", k - 2),
        },
    }
}

/// Checks a deletion's status against what the lane's reads have seen: a status `Pending` after a
/// read saw `Applied` is a failure. The set is copied before the call, so only reads that
/// returned before this one count.
fn check_deletion_status(h: &Harness, lane: &Lane, lane_index: usize, id: Uuid, thread: &Thread) {
    let applied_before = lock(&lane.applied_seen).clone();
    thread.start(format!("lane {lane_index}, deletion {id}"));
    let status = h.block_on(h.storage().deletion_status(id));
    thread.idle();
    let status = status.unwrap_or_else(|error| {
        panic!("lane {lane_index}, deletion {id}: deletion_status failed: {error}")
    });
    match status {
        DeletionStatus::Pending => assert!(
            !applied_before.contains(&id),
            "lane {lane_index}: deletion {id} reads Pending after a read saw it Applied"
        ),
        DeletionStatus::Applied { .. } => {
            lock(&lane.applied_seen).insert(id);
        }
    }
}

/// The highest state a read may have seen, once it returned (§ The rule): `in_flight` is read
/// before `published`, so a step that was in flight may have published since.
fn upper_bound(lane: &Lane) -> usize {
    let flying = lane.in_flight.load(SeqCst);
    let done = lane.published.load(SeqCst);
    done + usize::from(flying)
}

/// The first of `states` (`states[i]` is `S_{lo + i}`) that `observed` matches, as its index in the
/// history. Otherwise, every state's difference.
fn first_match(
    states: &[Arc<Model>],
    lo: usize,
    read: &Read,
    observed: &Observed,
) -> Result<usize, Vec<String>> {
    let mut differences = Vec::new();
    for (offset, state) in states.iter().enumerate() {
        match compare(state, read, observed, Strictness::Concurrent) {
            Ok(()) => return Ok(lo + offset),
            Err(difference) => differences.push(format!("S_{}: {difference}", lo + offset)),
        }
    }
    Err(differences)
}

/// Checks `observed`, a read that started at `lo` and that `hi` bounds, against the states it may
/// have seen. Returns the index of the state it matched, or `None` when the check is skipped (a
/// state is missing, or the run stops). A mismatch panics, unless a thread failed: then the read
/// is the victim of the stopped run, not a finding of its own.
fn check_observed_read(
    lane: &Lane,
    lane_index: usize,
    read: &Read,
    (lo, hi): (usize, usize),
    observed: &Observed,
    run: Run<'_>,
) -> Option<usize> {
    let states = match lane.states(lo, hi, run.stop) {
        Ok(states) => states,
        Err(miss) => {
            count_miss(&miss, run.stats);
            return None;
        }
    };
    match first_match(&states, lo, read, observed) {
        Ok(matched) => Some(matched),
        Err(differences) => {
            if run.failed.load(SeqCst) {
                return None;
            }
            panic!(
                "lane {lane_index}, {read:?}, lo {lo}, hi {hi}: observed {observed:?} matches none \
                 of the {} states the read may have seen:\n{}",
                states.len(),
                differences.join("\n")
            );
        }
    }
}

/// One reader thread: reads until `stop`, each read on one snapshot, each checked against the
/// states its snapshot may have. `progress` counts the matched reads.
#[allow(clippy::too_many_arguments)] // the run's shared flags, the exit counter and the progress
fn reader(
    h: &Harness,
    lanes: &[Arc<Lane>],
    pairs: &[(P, P)],
    seed: u64,
    run: Run<'_>,
    thread: &Thread,
    finished: &AtomicUsize,
    progress: &AtomicU64,
) {
    let _exit = Exit {
        thread,
        finished,
        lane: None,
        run,
    };
    let mut rng = XorShift(seed.max(1));
    while !run.stop.load(SeqCst) {
        let lane_index = usize::try_from(rng.below(lanes.len() as u64)).unwrap_or(0);
        let lane = &lanes[lane_index];
        let (a, b) = pairs[lane_index];
        // The first project of a lane has weight 3, the second 1, as the ops.
        let project = if rng.below(4) < 3 { a } else { b };
        let kind = pick_kind(&mut rng);
        // `lo` is read before the snapshot: the state it names is in the snapshot.
        let lo = lane.published.load(SeqCst);
        let at_lo = match lane.states(lo, lo, run.stop) {
            Ok(states) => states,
            Err(miss) => {
                count_miss(&miss, run.stats);
                continue;
            }
        };
        let Some(model) = at_lo.first() else {
            continue;
        };
        if matches!(kind, Kind::DeletionStatus) {
            let deletions = &model.project(project).deletions;
            if deletions.is_empty() {
                continue;
            }
            let id = deletions[usize::try_from(rng.below(deletions.len() as u64)).unwrap_or(0)].id;
            check_deletion_status(h, lane, lane_index, id, thread);
            run.stats.reads[kind as usize].fetch_add(1, SeqCst);
            progress.fetch_add(1, SeqCst);
            continue;
        }
        let read = build_read(kind, project, model, &mut rng);
        thread.start(format!("lane {lane_index}, lo {lo}, {read:?}"));
        let observed = h.block_on(observe(h.storage(), &read));
        thread.idle();
        let observed = match observed {
            Ok(observed) => observed,
            Err(ReadFailure::Busy) => {
                run.stats.busy.fetch_add(1, SeqCst);
                continue;
            }
            Err(ReadFailure::Error(text)) => {
                panic!("lane {lane_index}, {read:?}, lo {lo}: the read failed: {text}")
            }
        };
        // `hi` is read after the read returned (§ The rule).
        let hi = upper_bound(lane);
        let Some(matched) = check_observed_read(lane, lane_index, &read, (lo, hi), &observed, run)
        else {
            continue;
        };
        run.stats.reads[kind as usize].fetch_add(1, SeqCst);
        if matched > lo {
            run.stats.matched_later.fetch_add(1, SeqCst);
        }
        progress.fetch_add(1, SeqCst);
    }
}

/// Counts a `states` call that returned nothing, unless the run is stopping.
fn count_miss(miss: &Miss, stats: &Stats) {
    match miss {
        Miss::Stale => stats.stale.fetch_add(1, SeqCst),
        Miss::WaitLimit => stats.wait_limit.fetch_add(1, SeqCst),
        Miss::Stopped => 0,
    };
}

/// The GC thread: an obsolete-file collection with no grace, every `every`. Only the reference
/// counts protect a reader's files (spec 03 §5).
fn gc_thread(h: &Harness, every: Duration, run: Run<'_>, thread: &Thread, finished: &AtomicUsize) {
    let _exit = Exit {
        thread,
        finished,
        lane: None,
        run,
    };
    while !run.stop.load(SeqCst) {
        thread.start("collecting obsolete files".to_owned());
        let report = h
            .block_on(h.storage().collect_obsolete(Duration::ZERO))
            .expect("obsolete-file collection");
        thread.idle();
        run.stats.gc_deleted.fetch_add(report.deleted, SeqCst);
        run.stats.gc_in_use.fetch_add(report.in_use, SeqCst);
        thread::sleep(every);
    }
}

/// Watches every thread of a segment until they have all finished. An item that runs for more
/// than `STUCK_AFTER` fails the run: the threads are told to stop, and a storage call that does
/// not return within `ABORT_AFTER` aborts the process, so a hung test fails rather than blocks
/// CI. The dump goes to stderr with `writeln!`, since libtest captures `eprintln!` and drops it
/// when the process aborts.
fn watchdog(
    threads: &[Thread],
    finished: &AtomicUsize,
    total: usize,
    stop: &AtomicBool,
    failed: &AtomicBool,
) {
    loop {
        if finished.load(SeqCst) >= total {
            return;
        }
        thread::sleep(WAIT_TICK);
        let stuck = threads.iter().find_map(|thread| {
            let item = lock(&thread.item);
            item.as_ref()
                .filter(|(_, since)| since.elapsed() > STUCK_AFTER)
                .map(|_| thread.name.clone())
        });
        let Some(stuck) = stuck else {
            continue;
        };
        dump(threads);
        failed.store(true, SeqCst);
        stop.store(true, SeqCst);
        let until = Instant::now() + ABORT_AFTER;
        while finished.load(SeqCst) < total && Instant::now() < until {
            thread::sleep(WAIT_TICK);
        }
        if finished.load(SeqCst) >= total {
            panic!("{stuck} ran one item for more than 30 s (the stderr dump names it)");
        }
        let _ = writeln!(std::io::stderr(), "watchdog: a storage call hung; aborting");
        crate::test_support::no_core_dumps();
        std::process::abort();
    }
}

/// Writes what each thread is doing, to stderr.
fn dump(threads: &[Thread]) {
    for thread in threads {
        let item = lock(&thread.item);
        let line = match item.as_ref() {
            Some((what, since)) => format!("{what}, running for {:?}", since.elapsed()),
            None => "between items".to_owned(),
        };
        let _ = writeln!(std::io::stderr(), "watchdog: {}: {line}", thread.name);
    }
}

/// Runs the ops of every lane at once, each lane on its driver thread, with the readers, the GC
/// thread and the watchdog. When the drivers are done the readers go on for the tail (`TAIL`),
/// then the run stops. No op may be a barrier.
pub(super) fn run_segment(
    h: &Harness,
    lanes: &mut [LaneState],
    ops: &[&[Op]],
    options: &ConcurrentOptions,
    seed: u64,
    stats: &Stats,
) {
    assert_eq!(lanes.len(), ops.len(), "one list of ops per lane");
    for lane in lanes.iter() {
        lane.shared.set_idle(false);
    }
    let shared: Vec<Arc<Lane>> = lanes.iter().map(|lane| Arc::clone(&lane.shared)).collect();
    let pairs: Vec<(P, P)> = lanes
        .iter()
        .map(|lane| {
            let [a, b] = lane.model.projects();
            (a, b)
        })
        .collect();
    let threads: Vec<Thread> = (0..lanes.len())
        .map(|n| Thread::new(format!("lane {n} driver")))
        .chain((0..options.readers).map(|r| Thread::new(format!("reader {r}"))))
        .chain(std::iter::once(Thread::new("GC thread".to_owned())))
        .collect();
    let total = threads.len();
    let finished = AtomicUsize::new(0);
    let stop = AtomicBool::new(false);
    let failed = AtomicBool::new(false);
    let progress: Vec<AtomicU64> = (0..options.readers).map(|_| AtomicU64::new(0)).collect();
    let run = Run {
        stop: &stop,
        failed: &failed,
        stats,
    };
    let (driver_threads, rest) = threads.split_at(lanes.len());
    let (reader_threads, gc) = rest.split_at(options.readers);
    let drivers: Vec<(&mut LaneState, &[Op])> = lanes.iter_mut().zip(ops.iter().copied()).collect();
    let (shared, pairs, threads, finished, progress, stop, failed) = (
        &shared, &pairs, &threads, &finished, &progress, &stop, &failed,
    );
    thread::scope(|scope| {
        let handles: Vec<_> = drivers
            .into_iter()
            .zip(driver_threads)
            .enumerate()
            .map(|(lane_index, ((lane, lane_ops), thread))| {
                let rng_seed = seed.wrapping_add(lane_index as u64);
                scope.spawn(move || {
                    driver(h, lane, lane_ops, options, run, thread, finished, rng_seed);
                })
            })
            .collect();
        for (index, thread) in reader_threads.iter().enumerate() {
            let reader_seed = seed.wrapping_add(index as u64);
            let count = &progress[index];
            scope.spawn(move || {
                reader(h, shared, pairs, reader_seed, run, thread, finished, count);
            });
        }
        scope.spawn(move || gc_thread(h, options.gc_every, run, &gc[0], finished));
        scope.spawn(move || watchdog(threads, finished, total, stop, failed));
        for handle in handles {
            if let Err(payload) = handle.join() {
                stop.store(true, SeqCst);
                failed.store(true, SeqCst);
                std::panic::resume_unwind(payload);
            }
        }
        // The tail: the readers keep reading on the quiet lanes until each has matched
        // `TAIL_READS` reads, and `TAIL` has passed, or `TAIL_LIMIT` has.
        let tail_from = Instant::now();
        let before: Vec<u64> = progress.iter().map(|count| count.load(SeqCst)).collect();
        while !stop.load(SeqCst) && tail_from.elapsed() < TAIL_LIMIT {
            let enough = progress
                .iter()
                .zip(&before)
                .all(|(count, before)| count.load(SeqCst) >= before + TAIL_READS);
            if enough && tail_from.elapsed() >= TAIL {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        stop.store(true, SeqCst);
    });
}

/// Waits until no background job holds a file claim (`settle_concurrent`), failing after
/// `CLAIMS_WAIT`.
fn wait_for_claims(harness: &Harness) {
    let started = Instant::now();
    while harness.storage().compaction_claims() != 0 {
        assert!(
            started.elapsed() < CLAIMS_WAIT,
            "a background job still holds {} claims {CLAIMS_WAIT:?} after the settle passes",
            harness.storage().compaction_claims()
        );
        thread::sleep(Duration::from_millis(10));
    }
}

/// M1's settle, safe with background tasks running, for every lane. It runs between segments: no
/// reader or GC thread is alive, and the background tasks of the life it ends are gone with it.
pub(super) fn settle_concurrent(
    h: &mut Harness,
    lanes: &mut [LaneState],
    options: &ConcurrentOptions,
    stats: &Stats,
) {
    // 1. Everything in files, the WAL models updated for the restart.
    h.restart();
    for lane in lanes.iter_mut() {
        flushed(h, &mut lane.model);
    }
    let harness: &Harness = h;
    // 2. One clock for every lane, past the newest ingest by `close_delay` too (M1's settle).
    let close_delay =
        i64::try_from(harness.config().compaction.close_delay.as_nanos()).unwrap_or(i64::MAX);
    let latest = lanes.iter().map(|lane| lane.clock).max().unwrap_or(0);
    let clock = latest
        .max(midnight(day(6)))
        .saturating_add(close_delay)
        .saturating_add(MILLI_NS);
    for lane in lanes.iter_mut() {
        lane.clock = clock;
    }
    // 3. Every deletion of every lane applied. A pass may lose a conflict with a background pass.
    for _ in 0..PASSES {
        harness
            .block_on(harness.storage().apply_deletions(clock))
            .expect("apply_deletions while settling");
        if lanes
            .iter()
            .all(|lane| all_deletions_applied(harness, &lane.model))
        {
            break;
        }
    }
    assert!(
        lanes
            .iter()
            .all(|lane| all_deletions_applied(harness, &lane.model)),
        "a deletion is still pending after {PASSES} passes"
    );
    // 4. Every partition merged. Passes may lose conflicts until a background job is done; then
    //    one strict pass must find nothing.
    for _ in 0..PASSES {
        let report = harness
            .block_on(harness.storage().compact(clock))
            .expect("compact while settling");
        if report.jobs == 0 && report.failed == 0 {
            break;
        }
    }
    wait_for_claims(harness);
    let report = harness
        .block_on(harness.storage().compact(clock))
        .expect("the last compaction pass while settling");
    assert_eq!(
        report.jobs, 0,
        "compaction still has jobs after the settle passes"
    );
    assert_eq!(report.failed, 0, "a compaction job failed while settling");
    // 5. Nothing staged, nothing obsolete.
    harness
        .block_on(harness.storage().collect_staged(Duration::ZERO))
        .expect("collect_staged while settling");
    let report = harness
        .block_on(harness.storage().collect_obsolete(Duration::ZERO))
        .expect("obsolete-file collection while settling");
    stats.gc_deleted.fetch_add(report.deleted, SeqCst);
    stats.gc_in_use.fetch_add(report.in_use, SeqCst);
    // 6. Each lane's model as storage has it now, and each lane's collapsed state published.
    for lane in lanes.iter_mut() {
        keep_newest_copies(&mut lane.model.a);
        keep_newest_copies(&mut lane.model.b);
        drop_day_closed_scores(&mut lane.model.a);
        drop_day_closed_scores(&mut lane.model.b);
        lane.shared.push(&lane.model, options.history_keep);
    }
    // 7. The files on disk hold what the models predict.
    let models: Vec<&Model> = lanes.iter().map(|lane| &lane.model).collect();
    check_settled_files(harness, &models);
    // 8. Nothing in flight, nothing leaked (R6.6). Last: its restart ends the life.
    check_quiet_state(h);
}

/// What a case's lane holds between its segments.
pub(super) fn is_barrier(op: &Op) -> bool {
    matches!(
        op,
        Op::Restart | Op::Crash | Op::LossyCrash { .. } | Op::Settle
    )
}

/// The seed of a segment's readers: the case's seed and the segment's barrier index, so a case's
/// read mix is reproducible and differs between cases.
fn segment_seed(case: u64, index: usize) -> u64 {
    case.wrapping_add((index as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15))
}

/// The seed of a case: a hash of its ops, which are printed the same way in every run.
fn case_seed(ops: &[Op]) -> u64 {
    let mut hasher = DefaultHasher::new();
    format!("{ops:?}").hash(&mut hasher);
    hasher.finish()
}

/// Lane 0 (A, B); its ops are cut into segments at the barriers, which run between segments.
/// Each barrier is followed by `check_full` (M1's, sequential: nothing of the test is in flight,
/// but the new life's background tasks are running).
///
/// Returns the case's counters; the caller checks them (`regression_concurrent_smoke`).
pub(super) fn run_concurrent_case(ops: &[Op], options: &ConcurrentOptions) -> Stats {
    let tweaks: fn(&mut StorageConfig) = if options.remote {
        concurrent_remote_tweaks
    } else {
        concurrent_tweaks
    };
    let mut harness = Harness::with_options(
        HarnessOptions {
            workers: WORKERS,
            store: options.remote.then(memory_store),
            ..HarnessOptions::default()
        },
        tweaks,
    );
    let model = new_model(&harness, (P::A, P::B));
    let mut lanes = vec![LaneState {
        shared: Lane::new(&model),
        model,
        clock: midnight(day(0)) + 12 * HOUR_NS,
    }];
    let stats = Stats::default();
    let seed = case_seed(ops);
    let mut start = 0;
    for (index, op) in ops.iter().enumerate() {
        if !is_barrier(op) {
            continue;
        }
        if index > start {
            run_segment(
                &harness,
                &mut lanes,
                &[&ops[start..index]],
                options,
                segment_seed(seed, index),
                &stats,
            );
        }
        match op {
            Op::Settle => settle_concurrent(&mut harness, &mut lanes, options, &stats),
            Op::LossyCrash { .. } => {
                panic!("a lossy crash has no concurrent check: see concurrent_ops")
            }
            _ => {
                let lane = &mut lanes[0];
                apply(op, &mut harness, &mut lane.model, &mut lane.clock);
            }
        }
        check_full(&harness, &mut lanes[0].model, index);
        start = index + 1;
    }
    if ops.len() > start {
        run_segment(
            &harness,
            &mut lanes,
            &[&ops[start..]],
            options,
            segment_seed(seed, ops.len()),
            &stats,
        );
    }
    settle_concurrent(&mut harness, &mut lanes, options, &stats);
    check_full(&harness, &mut lanes[0].model, ops.len());
    assert!(
        stats.passes.load(SeqCst) == 0
            || stats.lost_passes.load(SeqCst) < stats.passes.load(SeqCst),
        "every one of the {} explicit compaction and deletion passes lost its jobs",
        stats.passes.load(SeqCst)
    );
    eprintln!("concurrent case: {stats:?}");
    stats
}

/// `concurrent_tweaks` for a remote case (R6.6): a 64 KiB read cache. A model file is a few KiB,
/// so the cache holds a handful of files, and its readers evict each other's blocks.
pub(super) fn concurrent_remote_tweaks(config: &mut StorageConfig) {
    concurrent_tweaks(config);
    config.query.read_cache_bytes = Some(ByteSize::kib(64));
}

/// The step hold of the proptest cases (`ConcurrentOptions::step_hold`).
const PROPTEST_STEP_HOLD: Duration = Duration::from_millis(1);

/// Cases of the concurrent proptest: `CETRA_CONCURRENT_CASES`, else 4 (the budget of § R6.5).
fn concurrent_cases() -> u32 {
    std::env::var("CETRA_CONCURRENT_CASES")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(4)
}

/// The ops of a concurrent case: `ops_for`, with every `LossyCrash` made a `Crash`. A background
/// flush moves a life's un-flushed records into files and truncates the WAL, which the WAL model
/// does not follow, so the replay a lossy crash counts is not predictable here. M1's sequential
/// case checks lossy crashes; a concurrent case keeps the restart.
fn concurrent_ops() -> impl Strategy<Value = Vec<Op>> {
    ops_for(P::A, P::B).prop_map(|ops| {
        ops.into_iter()
            .map(|op| match op {
                Op::LossyCrash { .. } => Op::Crash,
                other => other,
            })
            .collect()
    })
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: concurrent_cases(),
        // A failure may not reproduce on the smaller cases shrinking tries; the first failure's
        // message carries every detail.
        max_shrink_iters: 32,
        ..ProptestConfig::default()
    })]

    /// Every read made while ops and background tasks run equals the model in a state the read
    /// may have seen.
    #[test]
    fn concurrent_matches_the_model(ops in concurrent_ops()) {
        run_concurrent_case(&ops, &ConcurrentOptions {
            readers: 2,
            pace: Duration::from_millis(20),
            step_hold: PROPTEST_STEP_HOLD,
            gc_every: Duration::from_millis(50),
            history_keep: usize::MAX,
            remote: false,
        });
    }

    /// `concurrent_matches_the_model` with the objects in an in-memory object store (R6.6).
    #[test]
    fn concurrent_matches_the_model_remote(ops in concurrent_ops()) {
        run_concurrent_case(&ops, &ConcurrentOptions {
            readers: 2,
            pace: Duration::from_millis(20),
            step_hold: PROPTEST_STEP_HOLD,
            gc_every: Duration::from_millis(50),
            history_keep: usize::MAX,
            remote: true,
        });
    }
}

#[test]
fn lane_states_wait_for_the_step_in_flight() {
    let model = Model::for_pair(P::A, P::B, 0, 0);
    let lane = Lane::new(&model);
    let stop = AtomicBool::new(false);
    let returned = AtomicBool::new(false);
    thread::scope(|scope| {
        let waiter = scope.spawn(|| {
            let states = lane.states(0, 1, &stop).map(|states| states.len());
            returned.store(true, SeqCst);
            states
        });
        thread::sleep(Duration::from_millis(50));
        assert!(
            !returned.load(SeqCst),
            "states(0, 1) returned before the step in flight was published"
        );
        lane.push(&model, usize::MAX);
        assert_eq!(waiter.join().expect("the waiting thread"), Ok(2));
    });
    lane.set_idle(true);
    assert_eq!(
        lane.states(1, 5, &stop).map(|states| states.len()),
        Ok(1),
        "an idle lane returns the states it has, at once"
    );
}

#[test]
fn states_return_on_stop() {
    let model = Model::for_pair(P::A, P::B, 0, 0);
    let lane = Lane::new(&model);
    let stop = AtomicBool::new(false);
    thread::scope(|scope| {
        let waiter = scope.spawn(|| {
            let started = Instant::now();
            let states = lane.states(0, 3, &stop).map(|states| states.len());
            (states, started.elapsed())
        });
        thread::sleep(Duration::from_millis(50));
        stop.store(true, SeqCst);
        let (states, waited) = waiter.join().expect("the waiting thread");
        assert_eq!(states, Err(Miss::Stopped));
        assert!(
            waited < Duration::from_secs(1),
            "a stop ends the wait within a second, not after {waited:?}"
        );
    });
}

#[test]
fn history_keep_drops_old_states() {
    let model = Model::for_pair(P::A, P::B, 0, 0);
    let lane = Lane::new(&model);
    for _ in 0..10 {
        lane.push(&model, 4);
    }
    let stop = AtomicBool::new(false);
    assert_eq!(lane.published.load(SeqCst), 10);
    assert_eq!(
        lane.states(2, 3, &stop).map(|states| states.len()),
        Err(Miss::Stale)
    );
    assert_eq!(lane.states(7, 9, &stop).map(|states| states.len()), Ok(3));
}

#[test]
fn background_flush_runs_without_ops() {
    let harness = Harness::with_options(
        HarnessOptions {
            workers: WORKERS,
            ..HarnessOptions::default()
        },
        concurrent_tweaks,
    );
    let clock = midnight(day(0)) + 12 * HOUR_NS;
    let spans: Vec<_> = (0..3)
        .map(|span_no| span(P::A, 0, span_no, clock))
        .collect();
    send(&harness, Concurrency::Sequential, P::A, &spans);
    let started = Instant::now();
    loop {
        let rows: u64 = harness
            .storage()
            .catalog_files(Table::Spans)
            .iter()
            .map(|file| file.rows)
            .sum();
        if rows == 3 {
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "the flush wrote {rows} of 3 rows within 30 s"
        );
        thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(harness.storage().published_rows(Table::Spans), 0);
}

/// The pause of a read is a state the model has not pushed yet: while the append waits at
/// `AfterPublish`, its publish is in storage and its state is not in the lane, so the reader's
/// `hi` is `S_{lo + 1}` and `flying` is what raised it. The check runs once the append returns.
#[test]
fn a_read_while_an_append_waits_after_its_publish_sees_the_state_after_it() {
    let harness = Harness::with_options(
        HarnessOptions {
            workers: WORKERS,
            ..HarnessOptions::default()
        },
        concurrent_tweaks,
    );
    let mut model = new_model(&harness, (P::A, P::B));
    let lane = Lane::new(&model);
    let mut clock = midnight(day(0)) + 12 * HOUR_NS;
    let stats = Stats::default();
    let stop = AtomicBool::new(false);
    let failed = AtomicBool::new(false);
    let run = Run {
        stop: &stop,
        failed: &failed,
        stats: &stats,
    };
    let op = Op::Append {
        project: P::A,
        trace: 0,
        count: 3,
        split: false,
    };
    let read = Read::Spans(P::A);
    let hooks = harness.storage().hooks();
    hooks.hold(HookPoint::AfterPublish);
    let (lo, hi, observed, matched) = thread::scope(|scope| {
        let append = scope.spawn(|| {
            let mut rng = XorShift(1);
            apply_storage(
                &op,
                &harness,
                &mut model,
                &mut clock,
                Concurrency::Concurrent,
                &mut |event| {
                    publish(&lane, event, usize::MAX, &stats, Duration::ZERO, &mut rng);
                },
            )
        });
        let started = Instant::now();
        while hooks.waiting_at(HookPoint::AfterPublish) == 0 {
            assert!(
                started.elapsed() < Duration::from_secs(30),
                "the append did not reach AfterPublish"
            );
            thread::sleep(Duration::from_millis(5));
        }
        // The reader, while the append is paused: `lo`, the read, then `hi`.
        let lo = lane.published.load(SeqCst);
        let observed: Observed = harness
            .block_on(observe(harness.storage(), &read))
            .expect("the read while the append is paused");
        let hi = upper_bound(&lane);
        hooks.release(HookPoint::AfterPublish);
        append.join().expect("the append");
        let matched = check_observed_read(&lane, 0, &read, (lo, hi), &observed, run);
        (lo, hi, observed, matched)
    });
    assert_eq!(lo, 0, "the lane has published only S_0 before the append");
    assert_eq!(
        hi,
        lo + 1,
        "the append in flight may have published S_1 only"
    );
    assert_eq!(
        matched,
        Some(lo + 1),
        "the read matches S_1, not S_0: {observed:?}"
    );
}

/// The smoke case, in a local or a remote store.
fn run_smoke(remote: bool) {
    // Every kind of step and barrier, on lane 0.
    let ops = [
        Op::Append {
            project: P::A,
            trace: 0,
            count: 6,
            split: true,
        },
        Op::Append {
            project: P::A,
            trace: 1,
            count: 6,
            split: false,
        },
        Op::Resend {
            project: P::A,
            trace: 1,
            pick: 3,
        },
        Op::Score {
            project: P::A,
            target: 0,
            name: 0,
        },
        Op::DeleteTraces {
            project: P::A,
            picks: vec![1],
        },
        Op::Compact { hours: 1 },
        Op::ApplyDeletions { hours: 1 },
        Op::Append {
            project: P::B,
            trace: 0,
            count: 6,
            split: false,
        },
        Op::Retention {
            project: P::B,
            keep: 2,
        },
        Op::Restart,
        Op::Append {
            project: P::A,
            trace: 0,
            count: 6,
            split: false,
        },
        Op::DropB,
        Op::Crash,
        Op::Settle,
    ];
    let stats = run_concurrent_case(
        &ops,
        &ConcurrentOptions {
            readers: 2,
            pace: Duration::from_millis(5),
            step_hold: Duration::ZERO,
            gc_every: Duration::from_millis(20),
            history_keep: usize::MAX,
            remote,
        },
    );
    assert!(stats.matched_reads() > 0, "no read was checked: {stats:?}");
    assert!(
        stats.gc_deleted.load(SeqCst) > 0,
        "no obsolete file was deleted: {stats:?}"
    );
    assert_eq!(
        stats.passes.load(SeqCst),
        2,
        "the explicit compaction and deletion passes: {stats:?}"
    );
}

#[test]
fn regression_concurrent_smoke() {
    run_smoke(false);
}

#[test]
fn regression_concurrent_smoke_remote() {
    run_smoke(true);
}

/// `object_bytes` reads what a settled file holds, on a local and on a remote harness (R6.6).
#[test]
fn object_bytes_reads_both_stores() {
    for remote in [false, true] {
        let mut harness = Harness::with_options(
            HarnessOptions {
                workers: WORKERS,
                store: remote.then(memory_store),
                ..HarnessOptions::default()
            },
            concurrent_tweaks,
        );
        let mut model = new_model(&harness, (P::A, P::B));
        let mut clock = midnight(day(0)) + 12 * HOUR_NS;
        let append = Op::Append {
            project: P::A,
            trace: 0,
            count: 3,
            split: false,
        };
        apply(&append, &mut harness, &mut model, &mut clock);
        settle(&mut harness, &mut model, &mut clock);
        let files = harness.storage().catalog_files(Table::Spans);
        assert!(
            !files.is_empty(),
            "no span file after the settle (remote {remote})"
        );
        for file in &files {
            let rows = statam_rows(harness.object_bytes(file), harness.pack_bytes(file)).num_rows();
            assert_eq!(
                u64::try_from(rows).expect("a row count"),
                file.rows,
                "the rows of {} (remote {remote})",
                file.path
            );
        }
    }
}
