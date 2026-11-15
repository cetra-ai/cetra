//! Per-org span rate (D-P-19, D-P-35): a token bucket per org.

use std::collections::HashMap;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use llmobs_core::ids::OrgId;

use crate::auth::limits::RetryAfter;

/// Orgs kept at once. Past this, one bucket is dropped to admit a new org.
const MAX_ORGS: usize = 100_000;

/// The wait for an org whose `ingest_spans_per_sec` is 0.
const ZERO_RATE_WAIT: Duration = Duration::from_secs(60);

const NS_PER_SEC: f64 = 1e9;

/// One org's bucket: the tokens left at `updated_ns`, and the limits it was last checked with.
#[derive(Debug, Clone, Copy)]
struct Bucket {
    tokens: f64,
    updated_ns: i64,
    rate: u64,
    burst: u64,
}

/// A token bucket per org, keyed by org id, in memory.
#[derive(Debug, Default)]
pub(crate) struct IngestRate {
    buckets: Mutex<HashMap<OrgId, Bucket>>,
}

impl IngestRate {
    /// Takes `spans` tokens for `org` at `rate` per second with room for `burst` (0 means
    /// `rate`). A batch larger than `burst` costs `burst`, so it needs a full bucket.
    ///
    /// # Errors
    ///
    /// Returns [`RetryAfter`] when the bucket holds fewer tokens than the batch costs.
    pub(crate) fn take(
        &self,
        org: OrgId,
        spans: u64,
        rate: u64,
        burst: u64,
        now_ns: i64,
    ) -> Result<(), RetryAfter> {
        if spans == 0 {
            return Ok(());
        }
        if rate == 0 {
            return Err(RetryAfter(ZERO_RATE_WAIT));
        }
        let burst = if burst == 0 { rate } else { burst };
        let cost = spans.min(burst) as f64;
        let mut buckets = lock(&self.buckets);
        if !buckets.contains_key(&org) {
            make_room(&mut buckets, now_ns);
        }
        let full = Bucket {
            tokens: burst as f64,
            updated_ns: now_ns,
            rate,
            burst,
        };
        let bucket = buckets.entry(org).or_insert(full);
        let tokens = refilled(bucket.tokens, bucket.updated_ns, now_ns, rate, burst);
        *bucket = Bucket {
            tokens,
            updated_ns: now_ns,
            rate,
            burst,
        };
        if tokens >= cost {
            bucket.tokens = tokens - cost;
            Ok(())
        } else {
            Err(RetryAfter(wait_for(cost - tokens, rate)))
        }
    }
}

/// A poisoned mutex still holds plain counters, so the guard is recovered.
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The tokens after the time since `updated_ns` has refilled them at `rate`, up to `burst`.
fn refilled(tokens: f64, updated_ns: i64, now_ns: i64, rate: u64, burst: u64) -> f64 {
    let elapsed_s = now_ns.saturating_sub(updated_ns).max(0) as f64 / NS_PER_SEC;
    (tokens + rate as f64 * elapsed_s).min(burst as f64)
}

/// Whole seconds until `missing` tokens refill at `rate`, rounded up and at least 1.
fn wait_for(missing: f64, rate: u64) -> Duration {
    let seconds = (missing / rate as f64).ceil();
    // `as` saturates for huge values and the value is finite and positive here.
    Duration::from_secs(if seconds >= 1.0 { seconds as u64 } else { 1 })
}

/// Before a new org is added to a full map, drops one bucket: a bucket that has refilled to
/// its burst if there is one (it holds nothing a new request would not get), else any.
fn make_room(buckets: &mut HashMap<OrgId, Bucket>, now_ns: i64) {
    if buckets.len() < MAX_ORGS {
        return;
    }
    let full = buckets
        .iter()
        .find(|(_, bucket)| {
            refilled(
                bucket.tokens,
                bucket.updated_ns,
                now_ns,
                bucket.rate,
                bucket.burst,
            ) >= bucket.burst as f64
        })
        .map(|(org, _)| *org);
    if let Some(org) = full.or_else(|| buckets.keys().next().copied()) {
        buckets.remove(&org);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEC: i64 = 1_000_000_000;

    fn new_org() -> OrgId {
        OrgId::new_v7()
    }

    #[test]
    fn a_new_org_starts_full_and_refuses_past_the_burst() {
        let rate = IngestRate::default();
        let org = new_org();
        for _ in 0..3 {
            assert_eq!(rate.take(org, 1, 1, 3, 0), Ok(()));
        }
        assert_eq!(
            rate.take(org, 1, 1, 3, 0),
            Err(RetryAfter(Duration::from_secs(1)))
        );
    }

    #[test]
    fn the_retry_covers_the_missing_tokens_rounded_up() {
        let rate = IngestRate::default();
        let org = new_org();
        // Empty bucket at rate 1: three spans miss three tokens, so three seconds.
        assert_eq!(rate.take(org, 3, 1, 3, 0), Ok(()));
        assert_eq!(
            rate.take(org, 3, 1, 3, 0),
            Err(RetryAfter(Duration::from_secs(3)))
        );
        // At rate 2 the same miss is 1.5 s, rounded up to 2.
        let fast = new_org();
        assert_eq!(rate.take(fast, 3, 2, 3, 0), Ok(()));
        assert_eq!(
            rate.take(fast, 3, 2, 3, 0),
            Err(RetryAfter(Duration::from_secs(2)))
        );
    }

    #[test]
    fn the_bucket_refills_with_time_up_to_the_burst() {
        let rate = IngestRate::default();
        let org = new_org();
        assert_eq!(rate.take(org, 2, 1, 2, 0), Ok(()));
        assert!(rate.take(org, 1, 1, 2, 0).is_err());
        assert_eq!(rate.take(org, 1, 1, 2, SEC), Ok(()));
        // Ten idle seconds refill only to the burst of 2.
        assert_eq!(rate.take(org, 2, 1, 2, 11 * SEC), Ok(()));
        assert!(rate.take(org, 1, 1, 2, 11 * SEC).is_err());
    }

    #[test]
    fn a_batch_larger_than_the_burst_needs_a_full_bucket() {
        let rate = IngestRate::default();
        let org = new_org();
        assert_eq!(rate.take(org, 2, 1, 2, 0), Ok(()));
        // Costs the burst (2), and one token is left: refused for one more second.
        assert_eq!(
            rate.take(org, 10, 1, 2, SEC),
            Err(RetryAfter(Duration::from_secs(1)))
        );
        // With the bucket full it passes and empties the bucket.
        assert_eq!(rate.take(org, 10, 1, 2, 3 * SEC), Ok(()));
        assert!(rate.take(org, 1, 1, 2, 3 * SEC).is_err());
    }

    #[test]
    fn zero_spans_pass_without_touching_the_bucket() {
        let rate = IngestRate::default();
        let org = new_org();
        assert_eq!(rate.take(org, 2, 1, 2, 0), Ok(()));
        assert_eq!(rate.take(org, 0, 1, 2, 0), Ok(()));
        assert!(rate.take(org, 1, 1, 2, 0).is_err());
    }

    #[test]
    fn rate_zero_refuses_for_a_minute() {
        let rate = IngestRate::default();
        assert_eq!(
            rate.take(new_org(), 1, 0, 5, 0),
            Err(RetryAfter(Duration::from_secs(60)))
        );
        assert_eq!(rate.take(new_org(), 0, 0, 5, 0), Ok(()));
    }

    #[test]
    fn burst_zero_means_the_rate() {
        let rate = IngestRate::default();
        let org = new_org();
        for _ in 0..2 {
            assert_eq!(rate.take(org, 1, 2, 0, 0), Ok(()));
        }
        assert!(rate.take(org, 1, 2, 0, 0).is_err());
    }

    #[test]
    fn a_full_map_drops_one_bucket_for_a_new_org() {
        let rate = IngestRate::default();
        // Each org takes its only token, and one second later the previous one is full again.
        for index in 0..=MAX_ORGS {
            let now = i64::try_from(index).expect("small") * SEC;
            assert_eq!(rate.take(new_org(), 1, 1, 1, now), Ok(()));
        }
        assert_eq!(lock(&rate.buckets).len(), MAX_ORGS);
    }
}
