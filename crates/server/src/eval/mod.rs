//! Online evaluation in the server: connections, test runs, the queue and its loops (track I).

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
pub(crate) mod test_run;
pub(crate) mod trace_load;
pub(crate) mod trigger;

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use llmobs_core::ids::ProjectId;
use tokio::sync::{Notify, Semaphore, watch};
use tokio::task::{JoinHandle, JoinSet};
use uuid::Uuid;

use crate::http::AppState;
use crate::meta::MetaError;
use crate::meta::eval_jobs;

/// Extra time `EvalTasks::shutdown` allows the loops after their drain, before it aborts them.
const SHUTDOWN_MARGIN: Duration = Duration::from_secs(5);

/// The eval queue's in-memory state (D-I-6, D-I-14, D-I-15). One per process, in `AppInner`.
pub(crate) struct EvalQueue {
    /// Wakes the dispatcher: a job was queued, or a worker finished.
    wake: Notify,
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
        }
    }

    /// Wakes the dispatcher.
    pub(crate) fn wake(&self) {
        self.wake.notify_one();
    }

    /// Tells every eval loop to stop.
    pub(crate) fn stop(&self) {
        self.stop.send_replace(true);
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
        drain: Duration::from_secs(3),
    };
}

/// Startup recovery (D-I-12): `running` jobs go back to `queued` with one more interruption,
/// except those already interrupted 3 times, which fail.
///
/// # Errors
///
/// Returns [`MetaError`] when the write fails.
pub(crate) async fn recover(state: &AppState) -> Result<(), MetaError> {
    let now = state.clock.now_ns();
    let (requeued, failed) = state
        .meta
        .write(move |conn| Box::pin(async move { eval_jobs::recover(conn, now).await }))
        .await?;
    if requeued + failed > 0 {
        tracing::info!(requeued, failed, "eval jobs recovered at startup");
    }
    Ok(())
}

/// The running eval loops. [`Self::shutdown`] stops them; [`Self::abort`] stops them now.
pub(crate) struct EvalTasks {
    handles: Vec<JoinHandle<()>>,
    timing: EvalTiming,
}

/// Starts the dispatcher, janitor, pruner, budget ticker and pending-traces flusher. Each runs one
/// pass as soon as it starts (the first tick of its interval is not skipped), so a restart prunes
/// and clears at once.
pub(crate) fn spawn(state: &AppState, timing: EvalTiming) -> EvalTasks {
    let stop = state.eval.stopped();
    let handles = vec![
        tokio::spawn(run_dispatcher(state.clone(), stop.clone(), timing)),
        tokio::spawn(run_janitor(
            state.clone(),
            stop.clone(),
            timing.janitor_every,
        )),
        tokio::spawn(run_prune(state.clone(), stop.clone(), timing.prune_every)),
        tokio::spawn(run_flusher(state.clone(), stop.clone(), timing.flush_every)),
        tokio::spawn(run_budget(state.clone(), stop, timing.budget_every)),
    ];
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
