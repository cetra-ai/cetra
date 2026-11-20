//! Online evaluation in the server: connections, test runs, the queue and its loops (track I).

pub(crate) mod backfill;
pub(crate) mod breaker;
pub(crate) mod budget;
pub(crate) mod connection;
pub(crate) mod definitions;
pub(crate) mod dispatch;
pub(crate) mod evaluator;
pub(crate) mod execute;
pub(crate) mod hourly;
pub(crate) mod pending;
pub(crate) mod retry;
pub(crate) mod rule_check;
pub(crate) mod rule_limit;
pub(crate) mod sampling;
pub(crate) mod stats;
pub(crate) mod test_run;
pub(crate) mod trace_load;
pub(crate) mod trigger;

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use cetra_core::ids::ProjectId;
use tokio::sync::{Notify, Semaphore, watch};
use tokio::task::{JoinHandle, JoinSet};
use uuid::Uuid;

use crate::http::AppState;
use crate::issues;
use crate::meta::MetaError;
use crate::meta::backfills;
use crate::meta::eval_jobs;
use crate::meta::issues::{close_open_refits, interrupt_candidates};

/// Extra time `EvalTasks::shutdown` allows the loops after their drain, before it aborts them.
const SHUTDOWN_MARGIN: Duration = Duration::from_secs(5);

/// The eval queue's in-memory state (D-I-6, D-I-14, D-I-15). One per process, in `AppInner`.
pub(crate) struct EvalQueue {
    /// Wakes the dispatcher: a job was queued, or a worker finished.
    wake: Notify,
    /// Wakes the backfill runner: a backfill was created (D-I-39).
    backfill_wake: Notify,
    /// Set by `stop`; every loop waits on a receiver of it.
    stop: watch::Sender<bool>,
    /// `eval.workers` slots (D-I-6).
    workers: Arc<Semaphore>,
    /// Per connection: its slots and its token bucket, rebuilt when the connection's
    /// `max_concurrency` or `requests_per_minute` changes (not on other edits, such as a key change).
    /// Entries are never removed: the map is bounded by the number of connections ever seen.
    connections: Mutex<HashMap<Uuid, Arc<dispatch::ConnectionLimit>>>,
    /// Per webhook evaluator: 4 slots. Entries are never removed: the map is bounded by the number
    /// of webhook evaluators ever seen.
    webhooks: Mutex<HashMap<Uuid, Arc<Semaphore>>>,
    /// Per priority: the project whose job was claimed last, where the next pass's rotation starts (D-I-6).
    last_project: Mutex<HashMap<u8, ProjectId>>,
    pub(crate) budgets: budget::Budgets,
    pub(crate) breakers: breaker::Breakers,
    pub(crate) hourly: hourly::HourlyCaps,
    /// Per rule: when its unreadable-filter warning was last logged. Entries older than an hour
    /// are dropped as new ones come in, so the map holds only the rules warned about lately.
    unreadable_filters: Mutex<HashMap<Uuid, i64>>,
}

impl std::fmt::Debug for EvalQueue {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("EvalQueue")
            .field("free_workers", &self.workers.available_permits())
            .finish_non_exhaustive()
    }
}

impl EvalQueue {
    /// A queue with `workers` worker slots.
    pub(crate) fn new(workers: u32) -> Self {
        Self {
            wake: Notify::new(),
            backfill_wake: Notify::new(),
            stop: watch::channel(false).0,
            workers: Arc::new(Semaphore::new(
                usize::try_from(workers).unwrap_or(usize::MAX),
            )),
            connections: Mutex::new(HashMap::new()),
            webhooks: Mutex::new(HashMap::new()),
            last_project: Mutex::new(HashMap::new()),
            budgets: budget::Budgets::default(),
            breakers: breaker::Breakers::default(),
            hourly: hourly::HourlyCaps::default(),
            unreadable_filters: Mutex::new(HashMap::new()),
        }
    }

    /// Wakes the dispatcher.
    pub(crate) fn wake(&self) {
        self.wake.notify_one();
    }

    /// Wakes the backfill runner.
    pub(crate) fn wake_backfills(&self) {
        self.backfill_wake.notify_one();
    }

    /// Tells every eval loop to stop.
    pub(crate) fn stop(&self) {
        self.stop.send_replace(true);
    }

    /// Whether `stop` was called. The sweeper checks it between projects and batches.
    pub(crate) fn is_stopped(&self) -> bool {
        *self.stop.borrow()
    }

    /// Whether `rule`'s unreadable-filter warning is due at `now`: at most one per rule per hour.
    /// Returns true and records `now` when it is.
    pub(crate) fn filter_warning_due(&self, rule: Uuid, now: i64) -> bool {
        const HOUR_NS: i64 = 3_600_000_000_000;
        let mut warned = lock(&self.unreadable_filters);
        warned.retain(|_, at| now.saturating_sub(*at) < HOUR_NS);
        if warned.contains_key(&rule) {
            return false;
        }
        warned.insert(rule, now);
        true
    }

    /// A receiver of the stop flag. Loops wait with `receiver.wait_for(|stopped| *stopped)`, which
    /// also returns at once when `stop` was called before the receiver was made.
    pub(crate) fn stopped(&self) -> watch::Receiver<bool> {
        self.stop.subscribe()
    }

    /// Drops every in-memory state of a rule that was deleted (I3.8).
    pub(crate) fn forget_rule(&self, rule: Uuid) {
        self.budgets.forget(rule);
        self.breakers.reset(rule);
        self.hourly.forget(rule);
    }
}

/// Locks one of the queue's maps. A poisoned lock still guards consistent data: every change is one
/// insert or one bucket update.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// How often the eval loops run (D-I-6, D-I-12, D-I-15, D-I-28).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct EvalTiming {
    /// Dispatch tick; a wake-up also starts a pass.
    pub(crate) dispatch_every: Duration,
    /// Janitor tick.
    pub(crate) janitor_every: Duration,
    /// Pruning tick.
    pub(crate) prune_every: Duration,
    /// Budget ticker.
    pub(crate) budget_every: Duration,
    /// Pending-traces flusher (D-I-8).
    pub(crate) flush_every: Duration,
    /// Online-rule sweeper (D-I-9).
    pub(crate) sweep_every: Duration,
    /// Backfill runner tick; a created backfill also wakes it (D-I-39).
    pub(crate) backfill_every: Duration,
    /// How long the dispatcher gives its workers at shutdown before it aborts them.
    pub(crate) drain: Duration,
}

impl EvalTiming {
    /// Production timing.
    pub(crate) const DEFAULT: Self = Self {
        dispatch_every: Duration::from_secs(1),
        janitor_every: Duration::from_secs(60),
        prune_every: Duration::from_secs(3_600),
        budget_every: Duration::from_secs(30),
        flush_every: Duration::from_secs(5),
        sweep_every: Duration::from_secs(2),
        backfill_every: Duration::from_secs(5),
        drain: Duration::from_secs(3),
    };

    /// Fast timing for tests that run the loops in real time.
    #[cfg(test)]
    pub(crate) const TEST: Self = Self {
        dispatch_every: Duration::from_millis(20),
        janitor_every: Duration::from_millis(100),
        prune_every: Duration::from_secs(3_600),
        budget_every: Duration::from_millis(100),
        flush_every: Duration::from_millis(100),
        sweep_every: Duration::from_millis(50),
        backfill_every: Duration::from_millis(50),
        drain: Duration::from_secs(3),
    };
}

/// Startup recovery (D-I-12): `running` jobs go back to `queued` with one more interruption,
/// except those already interrupted 3 times, which fail. `running` backfills fail as
/// `interrupted` (D-I-39).
///
/// # Errors
///
/// Returns [`MetaError`] when the write fails.
pub(crate) async fn recover(state: &AppState) -> Result<(), MetaError> {
    let now = state.clock.now_ns();
    let (requeued, failed, interrupted, candidates, refits) = state
        .meta
        .write(move |conn| {
            Box::pin(async move {
                let (requeued, failed) = eval_jobs::recover(conn, now).await?;
                let interrupted = backfills::fail_running(conn, now).await?;
                // Nothing of the last process still runs: every `running` candidate is
                // interrupted and every open refit is closed (D-IC-12, A8).
                let candidates = interrupt_candidates(conn, i64::MAX, now).await?;
                let refits = close_open_refits(conn, now).await?;
                Ok((requeued, failed, interrupted, candidates, refits))
            })
        })
        .await?;
    if requeued + failed > 0 {
        tracing::info!(requeued, failed, "eval jobs recovered at startup");
    }
    if interrupted > 0 {
        tracing::info!(
            backfills = interrupted,
            "running backfills marked interrupted at startup"
        );
    }
    if candidates > 0 || refits > 0 {
        tracing::info!(
            candidates,
            refits,
            "issue candidates and refits interrupted at startup"
        );
    }
    Ok(())
}

/// The running eval loops. [`Self::shutdown`] stops them; [`Self::abort`] stops them now.
pub(crate) struct EvalTasks {
    handles: Vec<JoinHandle<()>>,
    timing: EvalTiming,
}

/// Starts the dispatcher, janitor, pruner, budget ticker, pending-traces flusher, online-rule
/// sweeper and backfill runner. Each runs one
/// pass as soon as it starts (the first tick of its interval is not skipped), so a restart prunes
/// and clears at once.
///
/// In restore mode the dispatcher and the backfill runner are not started (N M3, D-N-25): they
/// run judges and webhooks. The other five only touch `meta.db`.
pub(crate) fn spawn(state: &AppState, timing: EvalTiming) -> EvalTasks {
    let stop = state.eval.stopped();
    let mut handles = Vec::new();
    if !state.config.restore_mode {
        handles.push(tokio::spawn(run_dispatcher(
            state.clone(),
            stop.clone(),
            timing,
        )));
    }
    handles.push(tokio::spawn(run_janitor(
        state.clone(),
        stop.clone(),
        timing.janitor_every,
    )));
    handles.push(tokio::spawn(run_prune(
        state.clone(),
        stop.clone(),
        timing.prune_every,
    )));
    handles.push(tokio::spawn(run_flusher(
        state.clone(),
        stop.clone(),
        timing.flush_every,
    )));
    handles.push(tokio::spawn(run_sweeper(
        state.clone(),
        stop.clone(),
        timing.sweep_every,
    )));
    if !state.config.restore_mode {
        handles.push(tokio::spawn(run_backfills(
            state.clone(),
            stop.clone(),
            timing.backfill_every,
        )));
        handles.push(tokio::spawn(issues::worker::run_workers(
            state.clone(),
            stop.clone(),
        )));
    }
    handles.push(tokio::spawn(run_budget(
        state.clone(),
        stop,
        timing.budget_every,
    )));
    EvalTasks { handles, timing }
}

impl EvalTasks {
    /// Stops every loop and waits for it. The dispatcher gives its workers `timing.drain`, then
    /// aborts them; their jobs stay `running` until the next start (D-I-12). Loops still running
    /// after `drain` plus 5 s are aborted.
    pub(crate) async fn shutdown(self, state: &AppState) {
        state.eval.stop();
        let Self {
            mut handles,
            timing,
        } = self;
        let joined = tokio::time::timeout(timing.drain + SHUTDOWN_MARGIN, async {
            for handle in &mut handles {
                let _ = handle.await;
            }
        })
        .await;
        if joined.is_err() {
            tracing::error!("eval loops did not stop in time; aborting them");
            for handle in &handles {
                handle.abort();
            }
        }
    }

    /// Aborts every loop now. Aborting the dispatcher drops its job set, which aborts its workers.
    pub(crate) fn abort(&self) {
        for handle in &self.handles {
            handle.abort();
        }
    }
}

fn ticker(every: Duration) -> tokio::time::Interval {
    let mut interval = tokio::time::interval(every);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    interval
}

async fn run_dispatcher(state: AppState, mut stop: watch::Receiver<bool>, timing: EvalTiming) {
    let mut running: JoinSet<()> = JoinSet::new();
    let mut interval = ticker(timing.dispatch_every);
    loop {
        tokio::select! {
            biased;
            _ = stop.wait_for(|stopped| *stopped) => break,
            _ = interval.tick() => {}
            () = state.eval.wake.notified() => {}
        }
        while running.try_join_next().is_some() {}
        let now = state.clock.now_ns();
        if let Err(error) = dispatch::dispatch_once(&state, now, &mut running).await {
            tracing::error!(error = %error, "eval dispatch pass failed");
        }
    }
    let drained = tokio::time::timeout(timing.drain, async {
        while running.join_next().await.is_some() {}
    })
    .await;
    if drained.is_err() {
        let aborted = running.len();
        running.abort_all();
        while running.join_next().await.is_some() {}
        tracing::warn!(aborted, "eval workers aborted at shutdown");
    }
}

async fn run_janitor(state: AppState, mut stop: watch::Receiver<bool>, every: Duration) {
    let mut interval = ticker(every);
    loop {
        tokio::select! {
            biased;
            _ = stop.wait_for(|stopped| *stopped) => return,
            _ = interval.tick() => {}
        }
        let now = state.clock.now_ns();
        match dispatch::janitor_once(&state, now).await {
            Ok((requeued, failed)) if requeued + failed > 0 => {
                tracing::info!(requeued, failed, "eval janitor pass");
            }
            Ok(_) => {}
            Err(error) => tracing::error!(error = %error, "eval janitor pass failed"),
        }
        match issues::worker::janitor_once(&state, now).await {
            Ok(0) => {}
            Ok(interrupted) => {
                tracing::info!(interrupted, "issue candidates interrupted by the janitor");
            }
            Err(error) => tracing::error!(error = %error, "issue janitor pass failed"),
        }
    }
}

async fn run_prune(state: AppState, mut stop: watch::Receiver<bool>, every: Duration) {
    let mut interval = ticker(every);
    loop {
        tokio::select! {
            biased;
            _ = stop.wait_for(|stopped| *stopped) => return,
            _ = interval.tick() => {}
        }
        let now = state.clock.now_ns();
        if let Err(error) = dispatch::prune_once(&state, now).await {
            tracing::error!(error = %error, "eval prune failed");
        }
    }
}

async fn run_flusher(state: AppState, mut stop: watch::Receiver<bool>, every: Duration) {
    let mut interval = ticker(every);
    loop {
        tokio::select! {
            biased;
            _ = stop.wait_for(|stopped| *stopped) => return,
            _ = interval.tick() => {}
        }
        match trigger::flush_once(&state).await {
            Ok(report) if report != trigger::FlushReport::default() => {
                tracing::debug!(
                    upserts = report.upserts,
                    deletes = report.deletes,
                    pending = state.pending.len(),
                    "pending traces flushed"
                );
            }
            Ok(_) => {}
            Err(error) => {
                tracing::warn!(error = %error, "pending traces could not be flushed");
            }
        }
    }
}

/// The online-rule sweeper (D-I-9). A tick awaits its pass to the end, so passes never overlap.
async fn run_sweeper(state: AppState, mut stop: watch::Receiver<bool>, every: Duration) {
    let mut interval = ticker(every);
    loop {
        tokio::select! {
            biased;
            _ = stop.wait_for(|stopped| *stopped) => return,
            _ = interval.tick() => {}
        }
        let now = state.clock.now_ns();
        match trigger::sweep_once(&state, now).await {
            Ok(report) if report != trigger::SweepReport::default() => {
                tracing::debug!(
                    completed = report.completed,
                    matched = report.matched,
                    sampled = report.sampled,
                    capped = report.capped,
                    enqueued = report.enqueued,
                    failed_batches = report.failed_batches,
                    dropped = report.dropped,
                    "online-rule sweep"
                );
            }
            Ok(_) => {}
            Err(error) => tracing::error!(error = %error, "online-rule sweep failed"),
        }
    }
}

/// The backfill runner (D-I-39). A pass runs to its end before the next; it stops between pages
/// when the server stops.
async fn run_backfills(state: AppState, mut stop: watch::Receiver<bool>, every: Duration) {
    let mut interval = ticker(every);
    loop {
        tokio::select! {
            biased;
            _ = stop.wait_for(|stopped| *stopped) => return,
            _ = interval.tick() => {}
            () = state.eval.backfill_wake.notified() => {}
        }
        match backfill::backfill_once(&state).await {
            Ok(finished) if finished > 0 => tracing::debug!(finished, "backfill pass"),
            Ok(_) => {}
            Err(error) => tracing::error!(error = %error, "backfill pass failed"),
        }
    }
}

async fn run_budget(state: AppState, mut stop: watch::Receiver<bool>, every: Duration) {
    let mut interval = ticker(every);
    loop {
        tokio::select! {
            biased;
            _ = stop.wait_for(|stopped| *stopped) => return,
            _ = interval.tick() => {}
        }
        let now = state.clock.now_ns();
        if let Err(error) = dispatch::budget_once(&state, now).await {
            tracing::error!(error = %error, "eval budget pass failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::EvalTiming;
    use crate::test_support::TestApp;

    #[tokio::test]
    async fn restore_mode_spawns_no_dispatcher() {
        let app = TestApp::with_config(|config| config.restore_mode = true).await;
        let tasks = super::spawn(&app.state, EvalTiming::TEST);
        assert_eq!(tasks.handles.len(), 5);
        let limit = EvalTiming::TEST.drain + Duration::from_secs(5);
        tokio::time::timeout(limit, tasks.shutdown(&app.state))
            .await
            .expect("the loops stop in time");
    }
}
