//! Retry policy for eval jobs (D-I-13, D-I-23): attempt limits, fixed retry delays and the
//! jittered exponential backoff.

use std::time::Duration;

use sha2::{Digest, Sha256};
use uuid::Uuid;

/// Attempts a retryable job gets before it fails.
pub(crate) const MAX_ATTEMPTS: u32 = 6;
/// Retries of a trace load that failed (timeout or busy storage).
pub(crate) const LOAD_RETRIES: u32 = 3;
/// Delay before a load retry: 30 s.
pub(crate) const LOAD_RETRY_NS: i64 = 30_000_000_000;
/// Tries of a failed result write before the job takes the retryable path.
pub(crate) const WRITE_TRIES: u32 = 3;
/// Delay between result write tries.
pub(crate) const WRITE_RETRY: Duration = Duration::from_secs(5);
/// A job that got no trace-load permit in 30 s goes back to the queue this much later, without
/// spending a load retry: the wait measured the other jobs, not this trace (D-I-23).
pub(crate) const SATURATED_REQUEUE_NS: i64 = 5_000_000_000;

/// The first backoff step: 5 s.
const BASE_NS: i64 = 5_000_000_000;
/// The longest backoff before jitter: 10 min.
const CAP_NS: i64 = 600_000_000_000;
/// 2^64, the size of the jitter sample.
const TWO_POW_64: f64 = 18_446_744_073_709_551_616.0;

/// `min(5 s · 2^(attempts−1), 10 min)` scaled by a factor in `[0.8, 1.2)` taken from
/// SHA-256(`cetra.retry.v1` ‖ job id ‖ attempts as u32 BE): deterministic, so a test can pin it.
/// `attempts` 0 is treated as 1.
pub(crate) fn backoff_ns(job: Uuid, attempts: u32) -> i64 {
    let attempts = attempts.max(1);
    let mut delay = BASE_NS;
    for _ in 1..attempts {
        if delay >= CAP_NS {
            break;
        }
        delay = delay.saturating_mul(2);
    }
    let delay = delay.min(CAP_NS);

    let mut hasher = Sha256::new();
    hasher.update(b"cetra.retry.v1");
    hasher.update(job.as_bytes());
    hasher.update(attempts.to_be_bytes());
    let digest = hasher.finalize();
    let mut sample = [0_u8; 8];
    sample.copy_from_slice(&digest[..8]);
    let fraction = u64::from_be_bytes(sample) as f64 / TWO_POW_64;
    let factor = 0.8 + 0.4 * fraction;
    (delay as f64 * factor) as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_then_caps_within_the_jitter_band() {
        let job = Uuid::from_u128(3);
        for attempts in 1..=8_u32 {
            let base = BASE_NS
                .saturating_mul(1_i64 << (attempts - 1).min(20))
                .min(CAP_NS);
            let value = backoff_ns(job, attempts) as f64;
            let base = base as f64;
            assert!(
                value >= 0.8 * base && value < 1.2 * base,
                "attempt {attempts}: {value} is outside [0.8, 1.2) x {base}"
            );
        }
    }

    #[test]
    fn the_same_job_and_attempt_give_the_same_delay() {
        let job = Uuid::from_u128(4);
        assert_eq!(backoff_ns(job, 3), backoff_ns(job, 3));
        assert_eq!(backoff_ns(job, 0), backoff_ns(job, 1));
    }

    #[test]
    fn two_jobs_get_different_delays() {
        assert_ne!(
            backoff_ns(Uuid::from_u128(5), 1),
            backoff_ns(Uuid::from_u128(6), 1)
        );
    }
}
