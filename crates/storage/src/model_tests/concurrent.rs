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
use std::io::Write as _;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering::SeqCst};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use proptest::prelude::*;
use uuid::Uuid;

use super::check::{Read, check_full, check_settled_files, compare, days, observe};
use super::harness::{Harness, HarnessOptions, concurrent_tweaks};
use super::model::Model;
use super::observe::ReadFailure;
use super::ops::{
    Concurrency, MILLI_NS, Op, SPANS, StepEvent, TRACES, all_deletions_applied, apply,
    apply_storage, drop_day_closed_scores, flushed, keep_newest_copies, new_model, ops_for, send,
};
use super::shape::{HOUR_NS, P, day, midnight, span};
use crate::deletion::DeletionStatus;
use crate::reads::AlertMeasure;
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

/// Parameters of one concurrent case.
#[derive(Debug, Clone)]
pub(super) struct ConcurrentOptions {
    /// Reader threads per segment.
    pub(super) readers: usize,
    /// Pause after each op of a lane, so background work overlaps the ops.
    pub(super) pace: Duration,
    /// Sleep between two collections of the GC thread.
    pub(super) gc_every: Duration,
    /// States kept per lane; older ones are dropped (stress runs).
    pub(super) history_keep: usize,
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
    pub(super) gc_deleted: AtomicU64,
    pub(super) gc_in_use: AtomicU64,
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
enum Kind {
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

const KINDS: [(Kind, u64); 15] = [
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
    stop: &'a AtomicBool,
}

impl Drop for Exit<'_> {
    fn drop(&mut self) {
        if let Some(lane) = &self.lane {
            lane.set_idle(true);
        }
        if thread::panicking() {
            self.stop.store(true, SeqCst);
        }
        self.thread.idle();
        self.finished.fetch_add(1, SeqCst);
    }
}

/// Publishes one step event of a lane: `Begin` marks the call in flight, `Done` pushes the state.
fn publish(lane: &Lane, event: StepEvent<'_>, keep: usize, stats: &Stats) {
    match event {
        StepEvent::Begin => lane.in_flight.store(true, SeqCst),
        StepEvent::Done(model) => {
            lane.push(model, keep);
            stats.steps.fetch_add(1, SeqCst);
        }
    }
}

/// One lane's ops, on its driver thread: each op runs with the step callback of the lane, then
/// the pause.
#[allow(clippy::too_many_arguments)] // the run's shared stop flag, stats and exit counter
fn driver(
    h: &Harness,
    lane: &mut LaneState,
    ops: &[Op],
    options: &ConcurrentOptions,
    stop: &AtomicBool,
    stats: &Stats,
    thread: &Thread,
    finished: &AtomicUsize,
) {
    let _exit = Exit {
        thread,
        finished,
        lane: Some(Arc::clone(&lane.shared)),
        stop,
    };
    for (index, op) in ops.iter().enumerate() {
        if stop.load(SeqCst) {
            return;
        }
        thread.start(format!("op {index} {op:?}"));
        lane.model.ops_applied += 1;
        apply_storage(
            op,
            h,
            &mut lane.model,
            &mut lane.clock,
            Concurrency::Concurrent,
            &mut |event| publish(&lane.shared, event, options.history_keep, stats),
        );
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

/// One reader thread: reads until `stop`, each read on one snapshot, each checked against the
/// states its snapshot may have.
#[allow(clippy::too_many_arguments)] // the run's shared stop flag, stats and exit counter
fn reader(
    h: &Harness,
    lanes: &[Arc<Lane>],
    pairs: &[(P, P)],
    seed: u64,
    stop: &AtomicBool,
    stats: &Stats,
    thread: &Thread,
    finished: &AtomicUsize,
) {
    let _exit = Exit {
        thread,
        finished,
        lane: None,
        stop,
    };
    let mut rng = XorShift(seed.max(1));
    while !stop.load(SeqCst) {
        let lane_index = usize::try_from(rng.below(lanes.len() as u64)).unwrap_or(0);
        let lane = &lanes[lane_index];
        let (a, b) = pairs[lane_index];
        // The first project of a lane has weight 3, the second 1, as the ops.
        let project = if rng.below(4) < 3 { a } else { b };
        let kind = pick_kind(&mut rng);
        // `lo` is read before the snapshot: the state it names is in the snapshot.
        let lo = lane.published.load(SeqCst);
        let at_lo = match lane.states(lo, lo, stop) {
            Ok(states) => states,
            Err(miss) => {
                count_miss(&miss, stats);
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
            stats.reads[kind as usize].fetch_add(1, SeqCst);
            continue;
        }
        let read = build_read(kind, project, model, &mut rng);
        thread.start(format!("lane {lane_index}, lo {lo}, {read:?}"));
        let observed = h.block_on(observe(h.storage(), &read));
        thread.idle();
        let observed = match observed {
            Ok(observed) => observed,
            Err(ReadFailure::Busy) => {
                stats.busy.fetch_add(1, SeqCst);
                continue;
            }
            Err(ReadFailure::Error(text)) => {
                panic!("lane {lane_index}, {read:?}, lo {lo}: the read failed: {text}")
            }
        };
        // `hi` is read after the read returned, in this order (§ The rule).
        let flying = lane.in_flight.load(SeqCst);
        let done = lane.published.load(SeqCst);
        let hi = done + usize::from(flying);
        let states = match lane.states(lo, hi, stop) {
            Ok(states) => states,
            Err(miss) => {
                count_miss(&miss, stats);
                continue;
            }
        };
        let mut matched = None;
        let mut differences = Vec::new();
        for (offset, state) in states.iter().enumerate() {
            match compare(state, &read, &observed) {
                Ok(()) => {
                    matched = Some(offset);
                    break;
                }
                Err(difference) => differences.push(format!("S_{}: {difference}", lo + offset)),
            }
        }
        match matched {
            Some(offset) => {
                stats.reads[kind as usize].fetch_add(1, SeqCst);
                if offset > 0 {
                    stats.matched_later.fetch_add(1, SeqCst);
                }
            }
            None => {
                // Another thread failed and stopped the run: this read may be the stopped
                // step's victim, not a finding of its own.
                if stop.load(SeqCst) {
                    break;
                }
                panic!(
                    "lane {lane_index}, {read:?}, lo {lo}, hi {hi}: observed {observed:?} \
                     matches none of the {} states the read may have seen:\n{}",
                    states.len(),
                    differences.join("\n")
                );
            }
        }
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
fn gc_thread(
    h: &Harness,
    every: Duration,
    stop: &AtomicBool,
    stats: &Stats,
    thread: &Thread,
    finished: &AtomicUsize,
) {
    let _exit = Exit {
        thread,
        finished,
        lane: None,
        stop,
    };
    while !stop.load(SeqCst) {
        thread.start("collecting obsolete files".to_owned());
        let report = h
            .block_on(h.storage().collect_obsolete(Duration::ZERO))
            .expect("obsolete-file collection");
        thread.idle();
        stats.gc_deleted.fetch_add(report.deleted, SeqCst);
        stats.gc_in_use.fetch_add(report.in_use, SeqCst);
        thread::sleep(every);
    }
}

/// Watches every thread of a segment until they have all finished. An item that runs for more
/// than `STUCK_AFTER` fails the run: the threads are told to stop, and a storage call that does
/// not return within `ABORT_AFTER` aborts the process, so a hung test fails rather than blocks
/// CI. The dump goes to stderr with `writeln!`, since libtest captures `eprintln!` and drops it
/// when the process aborts.
fn watchdog(threads: &[Thread], finished: &AtomicUsize, total: usize, stop: &AtomicBool) {
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
        stop.store(true, SeqCst);
        let until = Instant::now() + ABORT_AFTER;
        while finished.load(SeqCst) < total && Instant::now() < until {
            thread::sleep(WAIT_TICK);
        }
        if finished.load(SeqCst) >= total {
            panic!("{stuck} ran one item for more than 30 s (the stderr dump names it)");
        }
        let _ = writeln!(std::io::stderr(), "watchdog: a storage call hung; aborting");
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
/// thread and the watchdog, until every driver is done. No op may be a barrier.
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
    let (driver_threads, rest) = threads.split_at(lanes.len());
    let (reader_threads, gc) = rest.split_at(options.readers);
    let drivers: Vec<(&mut LaneState, &[Op])> = lanes.iter_mut().zip(ops.iter().copied()).collect();
    let (shared, pairs, threads, finished, stop) = (&shared, &pairs, &threads, &finished, &stop);
    thread::scope(|scope| {
        let handles: Vec<_> = drivers
            .into_iter()
            .zip(driver_threads)
            .map(|((lane, lane_ops), thread)| {
                scope.spawn(move || {
                    driver(h, lane, lane_ops, options, stop, stats, thread, finished);
                })
            })
            .collect();
        for (index, thread) in reader_threads.iter().enumerate() {
            let reader_seed = seed.wrapping_add(index as u64);
            scope.spawn(move || {
                reader(h, shared, pairs, reader_seed, stop, stats, thread, finished);
            });
        }
        scope.spawn(move || gc_thread(h, options.gc_every, stop, stats, &gc[0], finished));
        scope.spawn(move || watchdog(threads, finished, total, stop));
        for handle in handles {
            if let Err(payload) = handle.join() {
                stop.store(true, SeqCst);
                std::panic::resume_unwind(payload);
            }
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
    harness
        .block_on(harness.storage().collect_obsolete(Duration::ZERO))
        .expect("obsolete-file collection while settling");
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
}

/// What a case's lane holds between its segments.
fn is_barrier(op: &Op) -> bool {
    matches!(
        op,
        Op::Restart | Op::Crash | Op::LossyCrash { .. } | Op::Settle
    )
}

/// The seed of a segment's readers: one per barrier index, so a case's read mix is reproducible.
fn segment_seed(index: usize) -> u64 {
    0x5eed_u64.wrapping_add(index as u64)
}

/// Lane 0 (A, B); its ops are cut into segments at the barriers, which run between segments.
/// Each barrier is followed by `check_full` (M1's, sequential: nothing of the test is in flight,
/// but the new life's background tasks are running).
pub(super) fn run_concurrent_case(ops: &[Op], options: &ConcurrentOptions) {
    let mut harness = Harness::with_options(
        HarnessOptions {
            workers: WORKERS,
            ..HarnessOptions::default()
        },
        concurrent_tweaks,
    );
    let model = new_model(&harness, (P::A, P::B));
    let mut lanes = vec![LaneState {
        shared: Lane::new(&model),
        model,
        clock: midnight(day(0)) + 12 * HOUR_NS,
    }];
    let stats = Stats::default();
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
                segment_seed(index),
                &stats,
            );
        }
        match op {
            Op::Settle => settle_concurrent(&mut harness, &mut lanes, options),
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
            segment_seed(ops.len()),
            &stats,
        );
    }
    settle_concurrent(&mut harness, &mut lanes, options);
    check_full(&harness, &mut lanes[0].model, ops.len());
    eprintln!("concurrent case: {stats:?}");
}

/// Cases of the concurrent proptest: `LLMOBS_CONCURRENT_CASES`, else 4 (the budget of § R6.5).
fn concurrent_cases() -> u32 {
    std::env::var("LLMOBS_CONCURRENT_CASES")
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
            gc_every: Duration::from_millis(50),
            history_keep: usize::MAX,
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

#[test]
fn regression_concurrent_smoke() {
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
    run_concurrent_case(
        &ops,
        &ConcurrentOptions {
            readers: 2,
            pace: Duration::from_millis(5),
            gc_every: Duration::from_millis(20),
            history_keep: usize::MAX,
        },
    );
}
