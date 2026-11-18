//! The daily curve of the generator (DO5 step 4): the share of the peak rate at a
//! time of day. The peak is at 14:00 UTC and the trough, a quarter of the peak,
//! at 03:00 UTC. Both sides are cosine ramps, so the curve is smooth at both ends.

const NS_PER_SEC: i64 = 1_000_000_000;
const SECS_PER_DAY: i64 = 86_400;
const PEAK_HOUR: f64 = 14.0;
/// Hours from the peak (14:00) to the trough (03:00), going forward.
const TROUGH_AFTER_PEAK: f64 = 13.0;
const TROUGH_LEVEL: f64 = 0.25;

/// The share of the peak rate at `ns` (nanoseconds since the Unix epoch, UTC).
/// It is 1.0 at 14:00 UTC, 0.25 at 03:00 UTC and never below 0.25.
pub(crate) fn level(ns: i64) -> f64 {
    let secs_of_day = ns.div_euclid(NS_PER_SEC).rem_euclid(SECS_PER_DAY) as f64;
    let hour = secs_of_day / 3600.0;
    // Hours after the peak, in [0, 24).
    let since_peak = (hour - PEAK_HOUR).rem_euclid(24.0);
    let toward_peak = if since_peak <= TROUGH_AFTER_PEAK {
        // Peak down to trough: 1 at the peak, 0 at the trough.
        let x = since_peak / TROUGH_AFTER_PEAK;
        0.5 * (1.0 + (std::f64::consts::PI * x).cos())
    } else {
        // Trough back up to the peak: 0 at the trough, 1 at the peak.
        let x = (since_peak - TROUGH_AFTER_PEAK) / (24.0 - TROUGH_AFTER_PEAK);
        0.5 * (1.0 - (std::f64::consts::PI * x).cos())
    };
    TROUGH_LEVEL + (1.0 - TROUGH_LEVEL) * toward_peak
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Nanoseconds for `hour` (may be fractional) on 2026-11-18 UTC.
    fn at_hour(hour: f64) -> i64 {
        let day_start: i64 = 1_763_424_000 * NS_PER_SEC; // 2025-11-18T00:00:00Z
        day_start + (hour * 3600.0 * NS_PER_SEC as f64) as i64
    }

    #[test]
    fn peak_is_at_fourteen_and_trough_at_three() {
        assert!((level(at_hour(14.0)) - 1.0).abs() < 1e-9);
        assert!((level(at_hour(3.0)) - TROUGH_LEVEL).abs() < 1e-9);
    }

    #[test]
    fn no_time_of_day_is_below_the_trough() {
        let lowest = (0..24 * 60)
            .map(|minute| level(at_hour(f64::from(minute) / 60.0)))
            .fold(f64::MAX, f64::min);
        assert!((lowest - TROUGH_LEVEL).abs() < 1e-9, "lowest was {lowest}");
        for minute in 0..24 * 60 {
            let value = level(at_hour(f64::from(minute) / 60.0));
            assert!((TROUGH_LEVEL..=1.0).contains(&value));
        }
    }

    #[test]
    fn the_curve_is_continuous_across_the_trough_and_the_peak() {
        for hour in [2.99, 3.0, 3.01, 13.99, 14.0, 14.01] {
            let here = level(at_hour(hour));
            let next = level(at_hour(hour + 0.001));
            assert!((here - next).abs() < 1e-3, "jump at {hour}");
        }
    }
}
