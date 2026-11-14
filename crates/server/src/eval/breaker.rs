//! The error breaker (D-I-14): a rule pauses when at least half of its recent jobs failed.

use std::collections::{HashMap, VecDeque};
use std::sync::{Mutex, MutexGuard, PoisonError};

use uuid::Uuid;

/// Jobs kept per rule.
pub(crate) const WINDOW: usize = 50;
/// Jobs a window needs before it can trip.
pub(crate) const MIN_JOBS: usize = 20;

/// The recent outcomes of each rule's jobs, `true` for a failure.
#[derive(Debug, Default)]
pub(crate) struct Breakers {
    windows: Mutex<HashMap<Uuid, VecDeque<bool>>>,
}

impl Breakers {
    fn lock(&self) -> MutexGuard<'_, HashMap<Uuid, VecDeque<bool>>> {
        self.windows.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Records one job that reached the evaluator (`failed` true when it failed). True when the
    /// rule must pause: at least `MIN_JOBS` recorded and half or more failed. A trip clears the window.
    pub(crate) fn record(&self, rule: Uuid, failed: bool) -> bool {
        let mut windows = self.lock();
        let window = windows.entry(rule).or_default();
        window.push_back(failed);
        if window.len() > WINDOW {
            window.pop_front();
        }
        let failures = window.iter().filter(|failed| **failed).count();
        if window.len() >= MIN_JOBS && failures * 2 >= window.len() {
            window.clear();
            return true;
        }
        false
    }

    /// Clears the rule's window (resume, a connection key change, delete).
    #[allow(dead_code)] // I3.8 uses it.
    pub(crate) fn reset(&self, rule: Uuid) {
        self.lock().remove(&rule);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RULE: Uuid = Uuid::from_u128(2);

    /// True when any of these outcomes trips the breaker.
    fn record_all(breakers: &Breakers, outcomes: &[bool]) -> bool {
        outcomes.iter().any(|failed| breakers.record(RULE, *failed))
    }

    #[test]
    fn nineteen_failures_do_not_trip_and_the_twentieth_does() {
        let breakers = Breakers::default();
        assert!(!record_all(&breakers, &[true; 19]));
        assert!(breakers.record(RULE, true));
        // The trip cleared the window, so the next failure starts over.
        assert!(!breakers.record(RULE, true));
    }

    #[test]
    fn fifty_successes_then_twenty_failures_do_not_trip() {
        let breakers = Breakers::default();
        assert!(!record_all(&breakers, &[false; 50]));
        // The window is the last 50: 30 ok and 20 failed is 40 %.
        assert!(!record_all(&breakers, &[true; 20]));
    }

    #[test]
    fn twenty_five_ok_and_twenty_five_failed_trips() {
        let breakers = Breakers::default();
        assert!(!record_all(&breakers, &[false; 25]));
        assert!(!record_all(&breakers, &[true; 24]));
        assert!(breakers.record(RULE, true));
    }

    #[test]
    fn reset_forgets_the_window() {
        let breakers = Breakers::default();
        assert!(!record_all(&breakers, &[true; 19]));
        breakers.reset(RULE);
        assert!(!record_all(&breakers, &[true; 19]));
        assert!(breakers.record(RULE, true));
    }
}
