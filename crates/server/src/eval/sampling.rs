//! Deterministic sampling of rule targets (D-I-10).

use cetra_core::ids::{SpanId, TraceId};
use sha2::{Digest, Sha256};
use uuid::Uuid;

/// A hash input, not an attribute name: changing it changes which targets are sampled.
const SAMPLE_TAG: &[u8] = b"cetra.sample.v1";

/// 2^64 as a float, the scale of a sampling rate.
const TWO_POW_64: f64 = 18_446_744_073_709_551_616.0;

/// Whether `rule` samples this target. `rate >= 1.0` always does; `rate` that is not above 0
/// (or NaN) never does. Otherwise the first 8 bytes, big-endian, of SHA-256(tag ‖ rule id (16 bytes)
/// ‖ trace id (16) ‖ span id (8, span rules only)) must be below `(rate · 2^64) as u64`.
pub(crate) fn sampled(rate: f64, rule: Uuid, trace: TraceId, span: Option<SpanId>) -> bool {
    if rate >= 1.0 {
        return true;
    }
    if rate.is_nan() || rate <= 0.0 {
        return false;
    }
    // The float-to-int cast saturates; rates in (0, 1) stay below `u64::MAX`.
    let threshold = (rate * TWO_POW_64) as u64;
    draw(rule, trace, span) < threshold
}

/// The first 8 bytes of the sampling hash, big-endian.
fn draw(rule: Uuid, trace: TraceId, span: Option<SpanId>) -> u64 {
    let mut hasher = Sha256::new();
    hasher.update(SAMPLE_TAG);
    hasher.update(rule.as_bytes());
    hasher.update(trace.as_bytes());
    if let Some(span) = span {
        hasher.update(span.as_bytes());
    }
    let digest = hasher.finalize();
    // SHA-256 has 32 bytes, so the first 8 always exist.
    u64::from_be_bytes(std::array::from_fn(|index| digest[index]))
}

#[cfg(test)]
mod tests {
    use super::*;

    const RULE: &str = "01900000-0000-7000-8000-000000000700";
    const TRACE: &str = "a1000000000000000000000000000001";

    /// First 8 bytes of SHA-256(`cetra.sample.v1` ‖ rule (16 bytes) ‖ trace (16 bytes)), big-endian.
    /// Computed with: `python3 -c "import hashlib; d = hashlib.sha256(b'cetra.sample.v1' +
    /// bytes.fromhex('01900000000070008000000000000700') + bytes.fromhex('a1000000000000000000000000000001')).digest(); print(int.from_bytes(d[:8], 'big'))"`.
    const H: u64 = 0xa8cc_2968_268d_0da5;
    // The margins of `sampling_is_pinned` must fit: 1e6 around H, away from both ends of the range.
    const _: () = assert!(H > 1_000_000 && u64::MAX - H > 1_000_000);

    fn rule() -> Uuid {
        Uuid::parse_str(RULE).unwrap()
    }

    fn trace() -> TraceId {
        TRACE.parse().unwrap()
    }

    #[test]
    fn rate_one_always_samples() {
        assert!(sampled(1.0, rule(), trace(), None));
        assert!(sampled(2.0, rule(), trace(), None));
    }

    #[test]
    fn rate_zero_never_samples() {
        assert!(!sampled(0.0, rule(), trace(), None));
        assert!(!sampled(-1.0, rule(), trace(), None));
        assert!(!sampled(f64::NAN, rule(), trace(), None));
    }

    #[test]
    fn sampling_is_pinned() {
        let above = (H as f64 + 1e6) / 2f64.powi(64);
        let below = (H as f64 - 1e6) / 2f64.powi(64);
        assert!(sampled(above, rule(), trace(), None));
        assert!(!sampled(below, rule(), trace(), None));
        assert_eq!(draw(rule(), trace(), None), H);
    }

    #[test]
    fn the_span_changes_the_draw() {
        let span_a: SpanId = "0000000000000001".parse().unwrap();
        let span_b: SpanId = "0000000000000002".parse().unwrap();
        assert_ne!(
            draw(rule(), trace(), Some(span_a)),
            draw(rule(), trace(), Some(span_b))
        );
    }

    #[test]
    fn a_quarter_samples_about_a_quarter() {
        let rule = rule();
        let mut sampled_count = 0;
        for index in 1..=10_000_u64 {
            let mut bytes = [0u8; 16];
            bytes[8..].copy_from_slice(&index.to_be_bytes());
            let trace = TraceId::from_bytes(bytes).unwrap();
            if sampled(0.25, rule, trace, None) {
                sampled_count += 1;
            }
        }
        assert!(
            (2_300..=2_700).contains(&sampled_count),
            "sampled {sampled_count} of 10000"
        );
    }
}
