//! Alerting: conditions, channels, evaluation and delivery (spec 07, track K).

pub(crate) mod channel;
pub(crate) mod condition;
pub(crate) mod deliver;
pub(crate) mod evaluate;
pub(crate) mod issue_notify;
pub(crate) mod message;
pub(crate) mod quota;

#[cfg(test)]
mod issue_links_tests;

use std::time::Duration;

use tokio::task::JoinHandle;

use crate::http::AppState;

/// How often the alert loops run (D-K-3, D-K-7, D-K-10).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AlertTiming {
    /// Evaluation tick.
    pub(crate) evaluate_every: Duration,
    /// Delivery tick.
    pub(crate) deliver_every: Duration,
    /// Pruning tick.
    pub(crate) prune_every: Duration,
}

impl AlertTiming {
    /// Production timing.
    pub(crate) const DEFAULT: Self = Self {
        evaluate_every: Duration::from_secs(60),
        deliver_every: Duration::from_secs(10),
        prune_every: Duration::from_secs(3_600),
    };

    /// Fast timing for tests that run the loops in real time.
    #[cfg(test)]
    pub(crate) const TEST: Self = Self {
        evaluate_every: Duration::from_millis(100),
        deliver_every: Duration::from_millis(100),
        prune_every: Duration::from_secs(3_600),
    };
}

/// Starts the evaluation (which also sends issue notifications, D-IC-21), delivery and pruning
/// loops. Each loop waits for its pass to finish before the next tick
/// (`MissedTickBehavior::Skip`), so passes never overlap. The first pass runs one period after
/// the start. Errors are logged and the loop goes on.
///
/// In restore mode only the pruning loop starts (N M3, D-N-25): evaluation and delivery send
/// alerts outward, issue notifications included.
pub(crate) fn spawn_loops(state: &AppState, timing: AlertTiming) -> Vec<JoinHandle<()>> {
    if state.config.restore_mode {
        return vec![spawn_history_prune_loop(state.clone(), timing.prune_every)];
    }
    vec![
        spawn_evaluation_loop(state.clone(), timing.evaluate_every),
        spawn_delivery_loop(state.clone(), timing.deliver_every),
        spawn_history_prune_loop(state.clone(), timing.prune_every),
    ]
}

fn spawn_evaluation_loop(state: AppState, every: Duration) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(every);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        interval.tick().await;
        loop {
            interval.tick().await;
            let now = state.clock.now_ns();
            match evaluate::evaluate_once(&state, now).await {
                Ok(report) if report.evaluated > 0 => tracing::debug!(
                    evaluated = report.evaluated,
                    failed = report.failed,
                    events = report.events,
                    stale = report.stale,
                    "alert evaluation pass"
                ),
                Ok(_) => {}
                Err(error) => {
                    tracing::error!(error = %error, "alert evaluation pass failed");
                }
            }
            match issue_notify::notify_issues(&state, now).await {
                Ok(report) if report.notifications > 0 || report.failed > 0 => tracing::debug!(
                    due = report.due,
                    notifications = report.notifications,
                    failed = report.failed,
                    "issue notification pass"
                ),
                Ok(_) => {}
                Err(error) => {
                    tracing::error!(error = %error, "issue notification pass failed");
                }
            }
        }
    })
}

fn spawn_delivery_loop(state: AppState, every: Duration) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(every);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        interval.tick().await;
        loop {
            interval.tick().await;
            let now = state.clock.now_ns();
            match deliver::deliver_once(&state, now).await {
                Ok(report) if report.sent + report.retried + report.failed > 0 => {
                    tracing::debug!(
                        sent = report.sent,
                        retried = report.retried,
                        failed = report.failed,
                        "alert delivery pass"
                    );
                }
                Ok(_) => {}
                Err(error) => {
                    tracing::error!(error = %error, "alert delivery pass failed");
                }
            }
        }
    })
}

fn spawn_history_prune_loop(state: AppState, every: Duration) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(every);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        interval.tick().await;
        loop {
            interval.tick().await;
            let now = state.clock.now_ns();
            if let Err(error) = deliver::prune_once(&state, now).await {
                tracing::error!(error = %error, "alert prune failed");
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use crate::alerts::{AlertTiming, spawn_loops};
    use crate::test_support::TestApp;

    #[tokio::test]
    async fn restore_mode_spawns_only_the_prune_loop() {
        let app = TestApp::with_config(|config| config.restore_mode = true).await;
        let handles = spawn_loops(&app.state, AlertTiming::TEST);
        assert_eq!(handles.len(), 1);
        for handle in handles {
            handle.abort();
        }
    }
}
