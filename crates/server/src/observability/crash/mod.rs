//! Opt-in crash reports (track TL § 3, D-TL-5, D-TL-12). With `telemetry.crash_reports` on and
//! the switchboard agreeing, every panic and every ERROR `tracing` event becomes a report of
//! code-defined facts, spooled to `<data_dir>/telemetry/spool/` at once and sent to
//! `<telemetry.endpoint>/v1/crash` by a background task. Nothing in a report comes from a
//! message or a field value.

pub(crate) mod layer;
pub(crate) mod limiter;
pub(crate) mod report;
pub(crate) mod spool;

use std::cell::Cell;
use std::fmt;
use std::panic::{Location, PanicHookInfo};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::observability::facts::{self, Deployment};

use self::limiter::{Limiter, ReportKey};
use self::report::{
    CrashReport, EventSite, ReportKind, capture_frames, event_report, location_of_event,
    location_of_panic, panic_report, unix_hour,
};
use self::spool::Spool;

/// The clock a [`Reporter`] reads: nanoseconds since the Unix epoch.
pub(crate) type NowNs = Arc<dyn Fn() -> i64 + Send + Sync>;

/// The reporter the panic hook uses, set once by `subscriber::init` when the switchboard
/// turned crash reports on. Unset: the hook spools nothing (design § Panics).
static REPORTER: OnceLock<Arc<Reporter>> = OnceLock::new();

thread_local! {
    /// Set while this thread builds or spools a report: an event or a panic raised meanwhile
    /// is skipped instead of re-entering (and deadlocking on) the limiter.
    static REPORTING: Cell<bool> = const { Cell::new(false) };
}

/// Marks this thread as reporting until dropped.
struct ReportGuard;

impl ReportGuard {
    /// `None` when this thread is already reporting, or its thread-local storage is gone.
    fn enter() -> Option<Self> {
        let entered = REPORTING
            .try_with(|flag| !flag.replace(true))
            .unwrap_or(false);
        entered.then_some(Self)
    }
}

impl Drop for ReportGuard {
    fn drop(&mut self) {
        let _ = REPORTING.try_with(|flag| flag.set(false));
    }
}

/// Builds, deduplicates and spools crash reports. Never logs.
pub(crate) struct Reporter {
    spool: Spool,
    limiter: Mutex<Limiter>,
    deployment: Deployment,
    now_ns: NowNs,
}

impl fmt::Debug for Reporter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Reporter")
            .field("spool", &self.spool)
            .field("deployment", &self.deployment)
            .finish_non_exhaustive()
    }
}

impl Reporter {
    /// A reporter that spools to `spool`, reads `now_ns` as its clock and tags reports with
    /// `deployment`.
    pub(crate) fn new(spool: Spool, deployment: Deployment, now_ns: NowNs) -> Self {
        Self {
            spool,
            limiter: Mutex::new(Limiter::default()),
            deployment,
            now_ns,
        }
    }

    /// The reporter of `cetra serve`: the probed deployment and the system clock. Not
    /// `crate::clock::Clock::System`: its `system_now_ns` uses `expect`, and a panic inside the
    /// panic hook aborts the process.
    pub(crate) fn system(spool: Spool) -> Self {
        Self::new(
            spool,
            facts::deployment(facts::probe_host()),
            Arc::new(system_now_ns),
        )
    }

    /// The limiter, locked. A poisoned lock is taken anyway: the limiter holds counts only.
    fn limiter(&self) -> MutexGuard<'_, Limiter> {
        self.limiter.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Records one occurrence of `key`. `build` runs only for a location the limiter does not
    /// know yet, so its backtrace is captured once per location, outside the lock.
    fn occurrence(&self, key: &ReportKey, build: impl FnOnce() -> CrashReport) {
        let Some(_guard) = ReportGuard::enter() else {
            return;
        };
        let hour = unix_hour((self.now_ns)());
        let known = self.limiter().knows(key);
        // The backtrace is captured outside the lock, and only for a new location.
        let template = if known { None } else { Some(build()) };
        let due = self.limiter().record(key, template, hour);
        if let Some(report) = due {
            let _ = self.spool.write(&report, (self.now_ns)());
        }
    }

    /// Records an ERROR event at `site`.
    pub(crate) fn report_event(&self, site: &EventSite<'_>) {
        let key = ReportKey {
            kind: ReportKind::ErrorEvent,
            location: location_of_event(site.file, site.line),
        };
        self.occurrence(&key, || {
            event_report(
                site,
                capture_frames(),
                self.deployment,
                facts::process_uptime(),
            )
        });
    }

    /// Records a panic at `location`. The message is never read.
    pub(crate) fn report_panic(&self, location: Option<&Location<'_>>) {
        let key = ReportKey {
            kind: ReportKind::Panic,
            location: location_of_panic(location),
        };
        self.occurrence(&key, || {
            panic_report(
                location,
                capture_frames(),
                self.deployment,
                facts::process_uptime(),
            )
        });
    }

    /// Spools the reports of repeats that are due: the minute tick of the sender's loop
    /// (`flush` in the limiter). Repeats not yet spooled when the process exits are lost.
    pub(crate) fn flush_pending(&self) {
        let Some(_guard) = ReportGuard::enter() else {
            return;
        };
        let hour = unix_hour((self.now_ns)());
        let reports = self.limiter().flush(hour);
        for report in reports {
            let _ = self.spool.write(&report, (self.now_ns)());
        }
    }
}

/// Nanoseconds since the Unix epoch, or 0 when the clock is before it or out of range.
/// Never panics: the panic hook reads it.
fn system_now_ns() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| i64::try_from(elapsed.as_nanos()).ok())
        .unwrap_or(0)
}

/// Lets the panic hook spool reports through `reporter`. The first call wins; `cetra serve`
/// makes one, from `subscriber::init`, after the switchboard turned crash reports on.
pub(crate) fn enable_panic_reports(reporter: Arc<Reporter>) {
    let _ = REPORTER.set(reporter);
}

/// The panic hook's crash report (M1's `panic::report` seam). Does nothing until
/// [`enable_panic_reports`] ran. Reads the panic's location, never its message.
pub(crate) fn report_panic(info: &PanicHookInfo<'_>) {
    if let Some(reporter) = REPORTER.get() {
        reporter.report_panic(info.location());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_guard_refuses_a_second_entry() {
        let first = ReportGuard::enter();
        assert!(first.is_some());
        assert!(ReportGuard::enter().is_none());
        drop(first);
        assert!(ReportGuard::enter().is_some());
    }

    #[test]
    fn an_occurrence_while_reporting_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let reporter = Reporter::new(
            Spool::new(dir.path().join("spool")),
            Deployment::Binary,
            Arc::new(|| 0_i64),
        );
        let site = EventSite {
            file: Some("crates/x.rs"),
            line: Some(1),
            target: "cetra_x",
            module_path: None,
            fields: Vec::new(),
        };
        let held = ReportGuard::enter();
        assert!(held.is_some());
        reporter.report_event(&site);
        assert!(Spool::new(dir.path().join("spool")).list().unwrap().is_empty());
        drop(held);
        reporter.report_event(&site);
        assert_eq!(Spool::new(dir.path().join("spool")).list().unwrap().len(), 1);
    }

    #[test]
    fn the_global_reporter_is_unset_in_tests() {
        assert!(REPORTER.get().is_none());
    }
}
