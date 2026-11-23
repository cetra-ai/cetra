//! Leak tests for crash reports (design § 3 Never). The child-process test runs the real panic hook, which is process-global, in its own process, so no other test sees it.

use std::panic::Location;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tracing::Dispatch;
use tracing_subscriber::layer::SubscriberExt;
use uuid::Uuid;

use super::report::{CrashReport, EventSite, ReportKind};
use super::sender::{crash_url, send_spool};
use super::spool::Spool;
use super::{Reporter, enable_panic_reports, layer};
use crate::observability::facts::Deployment;
use crate::observability::heartbeat;
use crate::observability::test_receiver::Receiver;

const PLANTED_KEY: &str = "sk-cetra-planted0000000000000000000000000";
const PLANTED_EMAIL: &str = "planted-user@example.com";
const PLANTED_PROMPT: &str = "planted prompt text";
/// Set by the parent: the child's spool directory.
const CHILD_SPOOL_VAR: &str = "CETRA_TEST_CRASH_CHILD_SPOOL";
/// What no spool file or request body may contain.
const FORBIDDEN: [&str; 5] = [
    PLANTED_KEY,
    "sk-cetra-",
    PLANTED_EMAIL,
    "@example.com",
    PLANTED_PROMPT,
];
/// The reporter's fixed clock in the in-process tests: 2026-01-01T00:00Z. `send_spool` gets
/// the same value, so no report is dropped as older than 7 days.
const FIXED_NOW_NS: i64 = 1_767_225_600_000_000_000;

/// Fails when `text` contains one of [`FORBIDDEN`]; the message names the needle, not the text.
fn assert_clean(text: &str) {
    for needle in FORBIDDEN {
        assert!(
            !text.contains(needle),
            "a crash report contains the forbidden needle {needle:?}"
        );
    }
}

/// The text of every file in the spool at `spool`, oldest first.
fn spool_texts(spool: &Path) -> Vec<String> {
    Spool::new(spool.to_path_buf())
        .list()
        .unwrap()
        .iter()
        .map(|file| std::fs::read_to_string(&file.path).unwrap())
        .collect()
}

/// The reports in the spool at `spool`, parsed, oldest first.
fn spool_reports(spool: &Path) -> Vec<CrashReport> {
    spool_texts(spool)
        .iter()
        .map(|text| serde_json::from_str::<CrashReport>(text).unwrap())
        .collect()
}

/// A reporter that spools to `<dir>/spool` on the fixed clock.
fn fixed_reporter(dir: &Path) -> Arc<Reporter> {
    Arc::new(Reporter::new(
        Spool::new(dir.join("spool")),
        Deployment::Binary,
        Arc::new(|| FIXED_NOW_NS),
    ))
}

/// A dispatcher whose only layer is the crash layer of `reporter`.
fn dispatch_to(reporter: &Arc<Reporter>) -> Dispatch {
    Dispatch::new(tracing_subscriber::registry().with(vec![layer::boxed(Arc::clone(reporter))]))
}

/// Posts every report in the spool at `spool` to `receiver` on the fixed clock; returns how
/// many the receiver accepted.
async fn send_to(receiver: &Receiver, spool: &Path) -> u32 {
    let client = heartbeat::client(heartbeat::REQUEST_TIMEOUT).unwrap();
    send_spool(
        &client,
        &crash_url(&receiver.base),
        &Spool::new(spool.to_path_buf()),
        None,
        FIXED_NOW_NS,
    )
    .await
    .sent
}

/// Reports a panic at this line, with a planted value in a local variable the reporter never
/// receives.
fn record_panic(reporter: &Reporter) {
    let _planted = PLANTED_KEY.to_owned();
    reporter.report_panic(Some(Location::caller()));
}

/// Run by `a_real_panic_and_error_leak_nothing` in a child process; returns at once
/// otherwise (for example under `--run-ignored`).
#[test]
#[ignore = "a child process of a_real_panic_and_error_leak_nothing"]
fn crash_child_process() {
    let Some(dir) = std::env::var_os(CHILD_SPOOL_VAR) else {
        return;
    };
    let reporter = Arc::new(Reporter::system(Spool::new(PathBuf::from(dir))));
    let subscriber = tracing_subscriber::registry().with(vec![layer::boxed(Arc::clone(&reporter))]);
    tracing::subscriber::set_global_default(subscriber).unwrap();
    enable_panic_reports(reporter);
    crate::observability::panic::install();
    tracing::error!(
        key = PLANTED_KEY,
        user = PLANTED_EMAIL,
        input = PLANTED_PROMPT,
        "failed for {PLANTED_EMAIL}: {PLANTED_PROMPT}"
    );
    // `unwrap` puts the error's Debug, planted values included, in the panic message.
    let failed: Result<(), String> = Err(format!("{PLANTED_KEY} {PLANTED_EMAIL} {PLANTED_PROMPT}"));
    failed.unwrap();
}

/// Re-runs this test binary, filtered to `crash_child_process`, as a child process: the child
/// gets its own process, its own global subscriber and its own panic hook, under `cargo test`
/// and nextest alike.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_real_panic_and_error_leak_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let spool = dir.path().join("spool");
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "observability::crash::leak_tests::crash_child_process",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD_SPOOL_VAR, &spool)
        .env("RUST_BACKTRACE", "0")
        .stdout(Stdio::null())
        .output()
        .unwrap();
    assert!(!output.status.success(), "the child panics on purpose");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(PLANTED_KEY),
        "the panic message reached stderr, so the planted value was really in it"
    );

    let texts = spool_texts(&spool);
    assert_eq!(texts.len(), 2, "one error event and one panic");
    for text in &texts {
        assert_clean(text);
    }
    let reports = spool_reports(&spool);
    let event = reports
        .iter()
        .find(|report| report.kind == ReportKind::ErrorEvent)
        .expect("an error-event report");
    assert_eq!(event.fields, ["key", "user", "input"]);
    let panicked = reports
        .iter()
        .find(|report| report.kind == ReportKind::Panic)
        .expect("a panic report");
    assert!(
        panicked
            .location
            .starts_with("crates/server/src/observability/crash/leak_tests.rs:"),
        "{}",
        panicked.location
    );
    assert!(panicked.fields.is_empty());
    assert!(
        panicked
            .backtrace
            .first()
            .is_some_and(|frame| frame.contains("crash_child_process")),
        "{:?}",
        panicked.backtrace
    );

    let receiver = Receiver::start("/v1/crash", 204, Duration::ZERO).await;
    let now_ns = i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
    )
    .unwrap();
    let client = heartbeat::client(heartbeat::REQUEST_TIMEOUT).unwrap();
    let summary = send_spool(
        &client,
        &crash_url(&receiver.base),
        &Spool::new(spool),
        Some(Uuid::parse_str("8f14e45f-ceea-4e7a-9d2b-1c3f5a6b7c8d").unwrap()),
        now_ns,
    )
    .await;
    assert_eq!(summary.sent, 2);
    let bodies = receiver.bodies();
    assert_eq!(bodies.len(), 2);
    for body in &bodies {
        assert_clean(&String::from_utf8_lossy(body));
    }
}

#[tokio::test]
async fn event_values_reach_neither_spool_nor_request() {
    let dir = tempfile::tempdir().unwrap();
    let spool = dir.path().join("spool");
    let reporter = fixed_reporter(dir.path());
    tracing::dispatcher::with_default(&dispatch_to(&reporter), || {
        tracing::error!(
            key = PLANTED_KEY,
            user = PLANTED_EMAIL,
            "failed for {PLANTED_EMAIL} with {PLANTED_PROMPT}"
        );
    });
    let texts = spool_texts(&spool);
    assert_eq!(texts.len(), 1);
    for text in &texts {
        assert_clean(text);
    }

    let receiver = Receiver::start("/v1/crash", 204, Duration::ZERO).await;
    let sent = send_to(&receiver, &spool).await;
    assert_eq!(sent as usize, texts.len());
    let bodies = receiver.bodies();
    assert_eq!(bodies.len(), texts.len());
    for body in &bodies {
        assert_clean(&String::from_utf8_lossy(body));
    }
}

#[tokio::test]
async fn debug_and_display_field_values_stay_out() {
    let dir = tempfile::tempdir().unwrap();
    let spool = dir.path().join("spool");
    let reporter = fixed_reporter(dir.path());
    tracing::dispatcher::with_default(&dispatch_to(&reporter), || {
        tracing::error!(error = %PLANTED_EMAIL, detail = ?PLANTED_PROMPT, key = PLANTED_KEY);
    });
    let texts = spool_texts(&spool);
    assert_eq!(texts.len(), 1);
    assert_clean(&texts[0]);
    let reports = spool_reports(&spool);
    assert_eq!(reports[0].fields, ["error", "detail", "key"]);
}

#[test]
fn a_home_directory_never_reaches_a_report() {
    let dir = tempfile::tempdir().unwrap();
    let spool = dir.path().join("spool");
    let reporter = fixed_reporter(dir.path());
    let main = EventSite {
        file: Some("/home/planted-user/work/main.rs"),
        line: Some(1),
        target: "cetra_server",
        module_path: None,
        fields: Vec::new(),
    };
    let array = EventSite {
        file: Some(
            "/home/planted-user/.cargo/registry/src/index.crates.io-x/arrow-55.0.0/src/array.rs",
        ),
        line: Some(2),
        target: "cetra_server",
        module_path: None,
        fields: Vec::new(),
    };
    reporter.report_event(&main);
    reporter.report_event(&array);
    let texts = spool_texts(&spool);
    assert_eq!(texts.len(), 2);
    for text in &texts {
        assert_clean(text);
        assert!(!text.contains("planted-user"));
    }
    let mut locations: Vec<String> = spool_reports(&spool)
        .into_iter()
        .map(|report| report.location)
        .collect();
    locations.sort();
    assert_eq!(locations, ["arrow-55.0.0/src/array.rs:2", "main.rs:1"]);
}

#[test]
fn a_panic_report_has_no_message_input() {
    let dir = tempfile::tempdir().unwrap();
    let spool = dir.path().join("spool");
    let reporter = fixed_reporter(dir.path());
    record_panic(&reporter);
    let texts = spool_texts(&spool);
    assert_eq!(texts.len(), 1);
    assert_clean(&texts[0]);
}
