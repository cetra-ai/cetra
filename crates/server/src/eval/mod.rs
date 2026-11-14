//! Online evaluation in the server: connections and test runs now; the queue and triggers later (track I).

pub(crate) mod breaker;
pub(crate) mod budget;
pub(crate) mod connection;
pub(crate) mod definitions;
pub(crate) mod evaluator;
pub(crate) mod execute;
pub(crate) mod hourly;
pub(crate) mod retry;
pub(crate) mod rule_check;
pub(crate) mod rule_limit;
pub(crate) mod test_run;
pub(crate) mod trace_load;

/// The eval queue's in-memory state (D-I-6, D-I-14, D-I-15). One per process, in `AppInner`.
#[derive(Debug, Default)]
pub(crate) struct EvalQueue {
    /// Wakes the dispatcher: a job was queued, or a worker finished.
    wake: tokio::sync::Notify,
    pub(crate) budgets: budget::Budgets,
    pub(crate) breakers: breaker::Breakers,
    pub(crate) hourly: hourly::HourlyCaps,
}

impl EvalQueue {
    /// Wakes the dispatcher (I3.7 waits on it).
    #[allow(dead_code)] // I3.7 calls it.
    pub(crate) fn wake(&self) {
        self.wake.notify_one();
    }

    /// Drops every in-memory state of a rule that was deleted (I3.8).
    pub(crate) fn forget_rule(&self, rule: uuid::Uuid) {
        self.budgets.forget(rule);
        self.breakers.reset(rule);
        self.hourly.forget(rule);
    }
}
