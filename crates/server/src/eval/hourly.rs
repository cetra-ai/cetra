//! The live-match cap per rule and UTC hour (D-I-11).

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard, PoisonError};

use uuid::Uuid;

/// Live matches one rule may enqueue per UTC hour. The rest are counted as dropped.
pub(crate) const HOURLY_CAP: u32 = 10_000;

const HOUR_NS: i64 = 3_600_000_000_000;

/// The matches of each rule in its current UTC hour.
#[derive(Debug, Default)]
#[allow(dead_code)] // M4's sweeper calls it (D-I-11).
pub(crate) struct HourlyCaps {
    rules: Mutex<HashMap<Uuid, HourCount>>,
}

#[derive(Debug, Clone, Copy)]
struct HourCount {
    hour: i64,
    admitted: u32,
    dropped: u64,
}

impl HourlyCaps {
    fn lock(&self) -> MutexGuard<'_, HashMap<Uuid, HourCount>> {
        self.rules.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Counts one live match of `rule` in `now`'s UTC hour. False (and counted as dropped) past the cap.
    #[allow(dead_code)] // M4's sweeper calls it (D-I-11).
    pub(crate) fn admit(&self, rule: Uuid, now: i64) -> bool {
        let hour = now.div_euclid(HOUR_NS);
        let mut rules = self.lock();
        let count = rules.entry(rule).or_insert(HourCount {
            hour,
            admitted: 0,
            dropped: 0,
        });
        if count.hour != hour {
            *count = HourCount {
                hour,
                admitted: 0,
                dropped: 0,
            };
        }
        if count.admitted < HOURLY_CAP {
            count.admitted += 1;
            true
        } else {
            count.dropped += 1;
            false
        }
    }

    /// Matches dropped in `now`'s UTC hour.
    #[allow(dead_code)] // I3.8 uses it.
    pub(crate) fn dropped(&self, rule: Uuid, now: i64) -> u64 {
        let hour = now.div_euclid(HOUR_NS);
        self.lock()
            .get(&rule)
            .filter(|count| count.hour == hour)
            .map_or(0, |count| count.dropped)
    }

    /// Drops the rule's count (deleted).
    #[allow(dead_code)] // I3.8 uses it.
    pub(crate) fn forget(&self, rule: Uuid) {
        self.lock().remove(&rule);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RULE: Uuid = Uuid::from_u128(7);
    const NOON: i64 = 12 * HOUR_NS;

    #[test]
    fn the_cap_admits_then_counts_drops() {
        let caps = HourlyCaps::default();
        for _ in 0..HOURLY_CAP {
            assert!(caps.admit(RULE, NOON));
        }
        assert!(!caps.admit(RULE, NOON));
        assert!(!caps.admit(RULE, NOON + 1));
        assert_eq!(caps.dropped(RULE, NOON), 2);
    }

    #[test]
    fn a_new_hour_resets_the_count() {
        let caps = HourlyCaps::default();
        for _ in 0..=HOURLY_CAP {
            caps.admit(RULE, NOON);
        }
        assert_eq!(caps.dropped(RULE, NOON), 1);
        assert!(caps.admit(RULE, NOON + HOUR_NS));
        assert_eq!(caps.dropped(RULE, NOON + HOUR_NS), 0);
        // The previous hour no longer reports its drops.
        assert_eq!(caps.dropped(RULE, NOON), 0);
    }
}
