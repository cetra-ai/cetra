//! The crash layer: every ERROR `tracing` event except the panic hook's own becomes a crash
//! report (track TL § 3). It reads the event's metadata only; the event's values are never
//! visited.

use std::sync::Arc;

use tracing::{Event, Level, Metadata, Subscriber};
use tracing_subscriber::Layer;
use tracing_subscriber::filter::filter_fn;
use tracing_subscriber::layer::Context;

use super::Reporter;
use super::report::EventSite;
use crate::observability::panic::PANIC_EVENT_TARGET;
use crate::observability::subscriber::{BoxedLayer, EXPORT_DROPPED_TARGETS};

/// Turns ERROR events into crash reports.
#[derive(Debug, Clone)]
pub(crate) struct CrashLayer {
    reporter: Arc<Reporter>,
}

impl CrashLayer {
    /// A layer that reports to `reporter`.
    pub(crate) fn new(reporter: Arc<Reporter>) -> Self {
        Self { reporter }
    }
}

impl<S: Subscriber> Layer<S> for CrashLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let metadata = event.metadata();
        if is_reported(metadata) {
            self.reporter
                .report_event(&EventSite::from_metadata(metadata));
        }
    }
}

/// Whether the crash layer reports this callsite: [`reportable`] over its metadata.
pub(crate) fn is_reported(metadata: &Metadata<'_>) -> bool {
    reportable(
        metadata.is_event(),
        *metadata.level(),
        metadata.target(),
        metadata.file().is_some(),
    )
}

/// An ERROR event that is not the panic hook's `panic` event (the hook reports panics itself,
/// so a panic is reported once), has a source file, and does not come from the OTLP
/// exporter's own stack. Events bridged from the `log` crate have no file: they would all
/// share one report saying nothing (target `log`, location `unknown`). The exporter's stack
/// ([`EXPORT_DROPPED_TARGETS`]) logs errors when the operator's own endpoint is down: that is
/// their network, not a cetra bug, and it would eat the hourly cap.
fn reportable(is_event: bool, level: Level, target: &str, has_file: bool) -> bool {
    is_event
        && level == Level::ERROR
        && target != PANIC_EVENT_TARGET
        && has_file
        && !EXPORT_DROPPED_TARGETS
            .iter()
            .any(|dropped| target.starts_with(dropped))
}

/// The crash layer as one of the subscriber's layers, with its own filter: it sees ERROR
/// events whatever `log.level` says, and no span.
pub(crate) fn boxed(reporter: Arc<Reporter>) -> BoxedLayer {
    CrashLayer::new(reporter)
        .with_filter(filter_fn(is_reported))
        .boxed()
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::atomic::{AtomicI64, Ordering};

    use tracing::Dispatch;
    use tracing_subscriber::layer::SubscriberExt;

    use super::*;
    use crate::observability::crash::report::{CrashReport, ReportKind};
    use crate::observability::crash::spool::Spool;
    use crate::observability::facts::Deployment;

    /// 2026-01-01T00:00Z in nanoseconds: an hour boundary (Unix hour 490 896).
    const CLOCK_START_NS: i64 = 1_767_225_600_000_000_000;
    const ONE_HOUR_NS: i64 = 3_600_000_000_000;
    const START_HOUR: u64 = 490_896;

    fn reporter(dir: &Path, now: &Arc<AtomicI64>) -> Arc<Reporter> {
        let clock = Arc::clone(now);
        Arc::new(Reporter::new(
            Spool::new(dir.join("spool")),
            Deployment::Binary,
            Arc::new(move || clock.load(Ordering::Relaxed)),
        ))
    }

    fn dispatch_to(reporter: &Arc<Reporter>) -> Dispatch {
        Dispatch::new(tracing_subscriber::registry().with(vec![boxed(Arc::clone(reporter))]))
    }

    /// Each spooled file's text and parsed report, oldest first.
    fn read_all(dir: &Path) -> Vec<(String, CrashReport)> {
        let mut files = Spool::new(dir.join("spool")).list().unwrap();
        files.sort_by_key(|file| file.written_at_ns);
        files
            .iter()
            .map(|file| {
                let text = std::fs::read_to_string(&file.path).unwrap();
                let report: CrashReport = serde_json::from_str(&text).unwrap();
                (text, report)
            })
            .collect()
    }

    #[inline(never)]
    fn emit_one() {
        tracing::error!(project_id = 7, "repeated");
    }

    #[test]
    fn an_error_event_is_spooled_with_its_field_names_only() {
        let dir = tempfile::tempdir().unwrap();
        let now = Arc::new(AtomicI64::new(CLOCK_START_NS));
        let reporter = reporter(dir.path(), &now);
        let dispatch = dispatch_to(&reporter);
        tracing::dispatcher::with_default(&dispatch, || {
            tracing::error!(
                key = "sk-cetra-planted0000000000000000000000000",
                user = "planted-user@example.com",
                input = "planted prompt text",
                "failed for planted-user@example.com"
            );
        });
        let files = read_all(dir.path());
        assert_eq!(files.len(), 1);
        let (text, report) = &files[0];
        assert_eq!(report.kind, ReportKind::ErrorEvent);
        assert!(
            report
                .location
                .starts_with("crates/server/src/observability/crash/layer.rs:")
        );
        assert_eq!(report.event_target, Some(module_path!().to_owned()));
        assert_eq!(report.fields, ["key", "user", "input"]);
        assert_eq!(report.count, 1);
        assert_eq!(report.first_hour, START_HOUR);
        for forbidden in [
            "sk-cetra-",
            "planted",
            "example.com",
            "prompt text",
            "failed for",
        ] {
            assert!(!text.contains(forbidden), "report holds {forbidden}");
        }
    }

    #[test]
    fn warn_and_info_events_are_not_reported() {
        let dir = tempfile::tempdir().unwrap();
        let now = Arc::new(AtomicI64::new(CLOCK_START_NS));
        let reporter = reporter(dir.path(), &now);
        let dispatch = dispatch_to(&reporter);
        tracing::dispatcher::with_default(&dispatch, || {
            tracing::warn!("a warning");
            tracing::info!("an info line");
        });
        assert!(read_all(dir.path()).is_empty());
    }

    #[test]
    fn the_panic_event_is_not_reported() {
        let dir = tempfile::tempdir().unwrap();
        let now = Arc::new(AtomicI64::new(CLOCK_START_NS));
        let reporter = reporter(dir.path(), &now);
        let dispatch = dispatch_to(&reporter);
        tracing::dispatcher::with_default(&dispatch, || {
            tracing::error!(target: PANIC_EVENT_TARGET, location = "x", "panic");
        });
        assert!(read_all(dir.path()).is_empty());
    }

    #[test]
    fn reportable_rules() {
        assert!(reportable(true, Level::ERROR, "cetra_x", true));
        // A span, not an event.
        assert!(!reportable(false, Level::ERROR, "cetra_x", true));
        assert!(!reportable(true, Level::WARN, "cetra_x", true));
        assert!(!reportable(true, Level::ERROR, PANIC_EVENT_TARGET, true));
        // A `log`-bridged event has no file.
        assert!(!reportable(true, Level::ERROR, "log", false));
        for target in [
            "opentelemetry_sdk::logs",
            "hyper_util::client::legacy",
            "reqwest::connect",
            "cetra_self_monitoring",
        ] {
            assert!(!reportable(true, Level::ERROR, target, true), "{target}");
        }
    }

    #[test]
    fn errors_below_a_quiet_stdout_level_are_still_seen() {
        use tracing_subscriber::filter::LevelFilter;

        let dir = tempfile::tempdir().unwrap();
        let now = Arc::new(AtomicI64::new(CLOCK_START_NS));
        let reporter = reporter(dir.path(), &now);
        let subscriber = tracing_subscriber::registry()
            .with(vec![boxed(Arc::clone(&reporter))])
            .with(
                tracing_subscriber::fmt::layer()
                    .with_writer(std::io::sink)
                    .with_filter(LevelFilter::OFF),
            );
        tracing::dispatcher::with_default(&Dispatch::new(subscriber), || {
            tracing::error!("quiet stdout, still reported");
        });
        assert_eq!(read_all(dir.path()).len(), 1);
    }

    #[test]
    fn one_callsite_is_reported_once_an_hour() {
        let dir = tempfile::tempdir().unwrap();
        let now = Arc::new(AtomicI64::new(CLOCK_START_NS));
        let reporter = reporter(dir.path(), &now);
        let dispatch = dispatch_to(&reporter);
        tracing::dispatcher::with_default(&dispatch, || {
            for _ in 0..5 {
                emit_one();
            }
        });
        assert_eq!(read_all(dir.path()).len(), 1);
        now.fetch_add(ONE_HOUR_NS, Ordering::Relaxed);
        reporter.flush_pending();
        let files = read_all(dir.path());
        assert_eq!(files.len(), 2);
        let (_, second) = &files[1];
        assert_eq!(second.count, 4);
        assert_eq!(second.first_hour, START_HOUR);
        assert_eq!(second.last_hour, START_HOUR);
    }

    #[test]
    fn distinct_callsites_get_distinct_reports() {
        let dir = tempfile::tempdir().unwrap();
        let now = Arc::new(AtomicI64::new(CLOCK_START_NS));
        let reporter = reporter(dir.path(), &now);
        let dispatch = dispatch_to(&reporter);
        tracing::dispatcher::with_default(&dispatch, || {
            tracing::error!("first callsite");
            tracing::error!("second callsite");
        });
        assert_eq!(read_all(dir.path()).len(), 2);
    }

    #[test]
    fn the_reporter_caps_reports_per_hour() {
        let dir = tempfile::tempdir().unwrap();
        let now = Arc::new(AtomicI64::new(CLOCK_START_NS));
        let reporter = reporter(dir.path(), &now);
        for line in 1..=25 {
            reporter.report_event(&EventSite {
                file: Some("crates/x.rs"),
                line: Some(line),
                target: "cetra_x",
                module_path: None,
                fields: Vec::new(),
            });
        }
        assert_eq!(read_all(dir.path()).len(), 20);
    }

    #[test]
    fn concurrent_errors_from_one_callsite_spool_one_report() {
        let dir = tempfile::tempdir().unwrap();
        let now = Arc::new(AtomicI64::new(CLOCK_START_NS));
        let reporter = reporter(dir.path(), &now);
        let dispatch = dispatch_to(&reporter);
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let dispatch = dispatch.clone();
                std::thread::spawn(move || {
                    tracing::dispatcher::with_default(&dispatch, || {
                        for _ in 0..50 {
                            emit_one();
                        }
                    });
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }
        assert_eq!(read_all(dir.path()).len(), 1);
        now.fetch_add(ONE_HOUR_NS, Ordering::Relaxed);
        reporter.flush_pending();
        let files = read_all(dir.path());
        assert_eq!(files.len(), 2);
        assert_eq!(files[1].1.count, 399);
    }

    #[test]
    fn a_spool_failure_is_swallowed() {
        let dir = tempfile::tempdir().unwrap();
        let now = Arc::new(AtomicI64::new(CLOCK_START_NS));
        // The spool "directory" is a regular file: every write fails.
        let blocked = dir.path().join("spool");
        std::fs::write(&blocked, b"not a directory").unwrap();
        let reporter = Arc::new(Reporter::new(
            Spool::new(blocked.clone()),
            Deployment::Binary,
            {
                let clock = Arc::clone(&now);
                Arc::new(move || clock.load(Ordering::Relaxed))
            },
        ));
        let dispatch = dispatch_to(&reporter);
        tracing::dispatcher::with_default(&dispatch, || {
            tracing::error!("cannot be spooled");
        });
        assert_eq!(std::fs::read(&blocked).unwrap(), b"not a directory");
    }

    #[test]
    fn report_panic_writes_a_panic_report() {
        let dir = tempfile::tempdir().unwrap();
        let now = Arc::new(AtomicI64::new(CLOCK_START_NS));
        let reporter = reporter(dir.path(), &now);
        reporter.report_panic(Some(std::panic::Location::caller()));
        let files = read_all(dir.path());
        assert_eq!(files.len(), 1);
        let (text, report) = &files[0];
        assert_eq!(report.kind, ReportKind::Panic);
        assert!(
            report
                .location
                .starts_with("crates/server/src/observability/crash/layer.rs:")
        );
        assert!(report.fields.is_empty());
        assert!(report.event_target.is_none());
        assert!(report.module.is_none());
        assert!(!text.contains("\"event_target\""));
        assert!(!text.contains("\"module\""));
        let first = report.backtrace.first();
        assert!(first.is_none_or(|frame| {
            !frame.starts_with("backtrace::")
                && !frame.starts_with("std::")
                && !frame.starts_with("cetra_server::observability::crash::Reporter")
        }));
    }
}
