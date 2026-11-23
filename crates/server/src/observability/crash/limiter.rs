//! Dedup and caps for crash reports (design § 3): the first occurrence of a location in a
//! clock hour is spooled at once; later ones are counted and spooled as one report when the
//! hour is over; at most [`MAX_REPORTS_PER_HOUR`] reports an hour. Hours are passed in, so
//! tests drive the clock. Never logs.

use std::collections::HashMap;

use super::report::{CrashReport, ReportKind};

/// At most this many reports are spooled in one clock hour, all locations together.
pub(crate) const MAX_REPORTS_PER_HOUR: u32 = 20;
/// At most this many locations are tracked. Real ones are a few hundred; each holds a
/// template of up to 48 × 512 bytes of names, so this bounds the memory to a few MB.
pub(crate) const MAX_TRACKED: usize = 512;

/// What occurrences are grouped by.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct ReportKey {
    /// Panic or error event.
    pub(crate) kind: ReportKind,
    /// The report's `location`.
    pub(crate) location: String,
}

/// One location's state.
#[derive(Debug)]
struct Entry {
    /// The report built at the first occurrence; counts and hours are filled per spool.
    template: CrashReport,
    /// The hour a report of this location was last spooled.
    last_spooled_hour: Option<u64>,
    /// Occurrences not yet in a spooled report.
    pending: u64,
    /// Hour of the first pending occurrence.
    pending_first_hour: u64,
    /// Hour of the last pending occurrence.
    pending_last_hour: u64,
}

impl Entry {
    /// Pending occurrences may be spooled now.
    fn is_due(&self, hour: u64) -> bool {
        self.pending > 0 && self.last_spooled_hour != Some(hour)
    }

    /// The report of the pending occurrences; resets them.
    fn take(&mut self, hour: u64) -> CrashReport {
        let mut report = self.template.clone();
        report.count = self.pending;
        report.first_hour = self.pending_first_hour;
        report.last_hour = self.pending_last_hour;
        self.pending = 0;
        self.last_spooled_hour = Some(hour);
        report
    }
}

/// The limiter. One per [`super::Reporter`], behind its mutex.
#[derive(Debug, Default)]
pub(crate) struct Limiter {
    /// The clock hour `spooled_this_hour` counts.
    hour: u64,
    /// Reports spooled in `hour`.
    spooled_this_hour: u32,
    /// Tracked locations.
    entries: HashMap<ReportKey, Entry>,
}

impl Limiter {
    /// Whether `key` is tracked, so the caller can skip building a template (and a backtrace).
    pub(crate) fn knows(&self, key: &ReportKey) -> bool {
        self.entries.contains_key(key)
    }

    /// Counts one occurrence of `key` at `hour`, and returns the report to spool now, if any.
    /// `template` is the report built for an unknown key. An unknown key without one is
    /// dropped: that happens only when [`Self::flush`] forgot the key between the caller's
    /// [`Self::knows`] and this call, which is rare and loses one occurrence. A new key
    /// beyond [`MAX_TRACKED`] is dropped too.
    pub(crate) fn record(
        &mut self,
        key: &ReportKey,
        template: Option<CrashReport>,
        hour: u64,
    ) -> Option<CrashReport> {
        self.roll(hour);
        if !self.entries.contains_key(key) {
            let template = template?;
            if self.entries.len() >= MAX_TRACKED {
                self.forget_quiet(hour);
                if self.entries.len() >= MAX_TRACKED {
                    return None;
                }
            }
            self.entries.insert(
                key.clone(),
                Entry {
                    template,
                    last_spooled_hour: None,
                    pending: 0,
                    pending_first_hour: hour,
                    pending_last_hour: hour,
                },
            );
        }
        let entry = self.entries.get_mut(key)?;
        if entry.pending == 0 {
            entry.pending_first_hour = hour;
        }
        entry.pending = entry.pending.saturating_add(1);
        entry.pending_last_hour = hour;
        if entry.is_due(hour) && self.spooled_this_hour < MAX_REPORTS_PER_HOUR {
            self.spooled_this_hour += 1;
            Some(entry.take(hour))
        } else {
            None
        }
    }

    /// The minute tick: the reports of pending occurrences that may go now, oldest first
    /// (by first pending hour, then location, then kind), within the cap. Then forgets
    /// locations with nothing pending and no report this hour.
    pub(crate) fn flush(&mut self, hour: u64) -> Vec<CrashReport> {
        self.roll(hour);
        let mut due: Vec<(u64, ReportKey)> = self
            .entries
            .iter()
            .filter(|(_, entry)| entry.is_due(hour))
            .map(|(key, entry)| (entry.pending_first_hour, key.clone()))
            .collect();
        due.sort_by(|a, b| (a.0, &a.1.location, a.1.kind).cmp(&(b.0, &b.1.location, b.1.kind)));
        let mut reports = Vec::new();
        for (_, key) in due {
            if self.spooled_this_hour >= MAX_REPORTS_PER_HOUR {
                break;
            }
            if let Some(entry) = self.entries.get_mut(&key) {
                self.spooled_this_hour += 1;
                reports.push(entry.take(hour));
            }
        }
        self.forget_quiet(hour);
        reports
    }

    /// Starts a new cap window when the hour changed (a clock going back counts as a change).
    fn roll(&mut self, hour: u64) {
        if hour != self.hour {
            self.hour = hour;
            self.spooled_this_hour = 0;
        }
    }

    /// Drops locations with nothing pending and no report in `hour`.
    fn forget_quiet(&mut self, hour: u64) {
        self.entries
            .retain(|_, entry| entry.pending > 0 || entry.last_spooled_hour == Some(hour));
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::observability::crash::report::{EventSite, event_report};
    use crate::observability::facts::Deployment;

    /// The clock hour of the tests.
    const H: u64 = 490_896;

    fn key(n: u32) -> ReportKey {
        ReportKey {
            kind: ReportKind::ErrorEvent,
            location: format!("crates/x.rs:{n}"),
        }
    }

    /// The template of location `key(n)`. Built from a fixed site, so no backtrace is captured.
    fn template(n: u32) -> CrashReport {
        let site = EventSite {
            file: Some("crates/x.rs"),
            line: Some(n),
            target: "x",
            module_path: None,
            fields: Vec::new(),
        };
        event_report(&site, Vec::new(), Deployment::Binary, Duration::ZERO)
    }

    #[test]
    fn the_first_occurrence_is_spooled_at_once() {
        let mut limiter = Limiter::default();
        let report = limiter
            .record(&key(1), Some(template(1)), H)
            .expect("the first occurrence is spooled");
        assert_eq!(report.count, 1);
        assert_eq!(report.first_hour, H);
        assert_eq!(report.last_hour, H);
        assert!(limiter.knows(&key(1)));
    }

    #[test]
    fn repeats_in_the_hour_are_counted_not_spooled() {
        let mut limiter = Limiter::default();
        assert!(limiter.record(&key(1), Some(template(1)), H).is_some());
        for _ in 0..3 {
            assert!(limiter.record(&key(1), None, H).is_none());
        }
    }

    #[test]
    fn the_next_hour_flush_spools_the_repeats() {
        let mut limiter = Limiter::default();
        assert!(limiter.record(&key(1), Some(template(1)), H).is_some());
        for _ in 0..3 {
            assert!(limiter.record(&key(1), None, H).is_none());
        }
        let reports = limiter.flush(H + 1);
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].count, 3);
        assert_eq!(reports[0].first_hour, H);
        assert_eq!(reports[0].last_hour, H);
        assert!(limiter.flush(H + 1).is_empty());
    }

    #[test]
    fn a_repeat_in_a_later_hour_is_spooled_at_once_with_the_pending_count() {
        let mut limiter = Limiter::default();
        assert!(limiter.record(&key(1), Some(template(1)), H).is_some());
        assert!(limiter.record(&key(1), None, H).is_none());
        assert!(limiter.record(&key(1), None, H).is_none());
        let report = limiter
            .record(&key(1), None, H + 1)
            .expect("a repeat in a later hour is spooled");
        assert_eq!(report.count, 3);
        assert_eq!(report.first_hour, H);
        assert_eq!(report.last_hour, H + 1);
    }

    #[test]
    fn at_most_twenty_reports_an_hour() {
        let mut limiter = Limiter::default();
        let mut spooled = 0;
        for n in 0..25 {
            if limiter.record(&key(n), Some(template(n)), H).is_some() {
                spooled += 1;
            }
        }
        assert_eq!(spooled, 20);
        assert!(limiter.flush(H).is_empty());
        let reports = limiter.flush(H + 1);
        assert_eq!(reports.len(), 5);
        for (report, n) in reports.iter().zip(20u32..25) {
            assert_eq!(report.location, format!("crates/x.rs:{n}"));
            assert_eq!(report.count, 1);
            assert_eq!(report.first_hour, H);
            assert_eq!(report.last_hour, H);
        }
    }

    #[test]
    fn the_cap_resets_each_hour() {
        let mut limiter = Limiter::default();
        for n in 0..20 {
            assert!(limiter.record(&key(n), Some(template(n)), H).is_some());
        }
        assert!(limiter.record(&key(99), Some(template(99)), H).is_none());
        assert!(
            limiter
                .record(&key(99), Some(template(99)), H + 1)
                .is_some()
        );
    }

    #[test]
    fn a_known_key_without_a_template_still_counts() {
        let mut limiter = Limiter::default();
        assert!(limiter.record(&key(1), Some(template(1)), H).is_some());
        assert!(limiter.record(&key(1), None, H).is_none());
        let reports = limiter.flush(H + 1);
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].count, 1);
    }

    #[test]
    fn an_unknown_key_without_a_template_is_dropped() {
        let mut limiter = Limiter::default();
        assert!(limiter.record(&key(9), None, H).is_none());
        assert!(!limiter.knows(&key(9)));
    }

    #[test]
    fn flush_forgets_quiet_locations() {
        let mut limiter = Limiter::default();
        assert!(limiter.record(&key(1), Some(template(1)), H).is_some());
        assert!(limiter.flush(H + 1).is_empty());
        assert!(!limiter.knows(&key(1)));
    }

    #[test]
    fn panics_and_events_at_one_location_are_separate() {
        let mut limiter = Limiter::default();
        let panic = ReportKey {
            kind: ReportKind::Panic,
            location: "crates/x.rs:1".to_owned(),
        };
        assert!(limiter.record(&key(1), Some(template(1)), H).is_some());
        assert!(limiter.record(&panic, Some(template(1)), H).is_some());
    }

    #[test]
    fn a_clock_going_back_is_a_new_window() {
        let mut limiter = Limiter::default();
        assert!(limiter.record(&key(1), Some(template(1)), H).is_some());
        let report = limiter
            .record(&key(1), None, H - 1)
            .expect("a clock going back starts a new window");
        assert_eq!(report.count, 1);
        assert_eq!(report.first_hour, H - 1);
        assert_eq!(report.last_hour, H - 1);
    }

    #[test]
    fn counts_saturate() {
        let mut limiter = Limiter::default();
        assert!(limiter.record(&key(1), Some(template(1)), H).is_some());
        limiter
            .entries
            .get_mut(&key(1))
            .expect("the location is tracked")
            .pending = u64::MAX;
        assert!(limiter.record(&key(1), None, H).is_none());
        let reports = limiter.flush(H + 1);
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].count, u64::MAX);
    }

    #[test]
    fn tracked_locations_are_bounded() {
        let mut limiter = Limiter::default();
        let total = u32::try_from(MAX_TRACKED + 10).expect("a small count");
        for n in 0..total {
            let spooled = limiter.record(&key(n), Some(template(n)), H);
            assert_eq!(spooled.is_some(), n < MAX_REPORTS_PER_HOUR, "location {n}");
        }
        assert_eq!(limiter.entries.len(), MAX_TRACKED);
        for n in u32::try_from(MAX_TRACKED).expect("a small count")..total {
            assert!(!limiter.knows(&key(n)), "location {n}");
        }
    }
}
