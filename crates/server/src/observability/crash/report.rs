//! The crash report (D-TL-5): a fixed type whose values are constants, buckets, code locations, callsite metadata and function names. Its constructors take no message and no field value, so neither can reach a report.

use std::panic::Location;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::observability::buckets::{UptimeBucket, uptime_bucket};
use crate::observability::facts::{self, Deployment, Profile};

/// The report's schema version (TL-Q1: the relay refuses other versions).
pub(crate) const CRASH_SCHEMA: u32 = 1;
/// At most this many function names in `backtrace`.
pub(crate) const MAX_FRAMES: usize = 48;
/// The stack walk stops after this many frames, before cleanup.
const MAX_RAW_FRAMES: usize = 256;
/// A function name is cut to this many bytes.
pub(crate) const MAX_NAME_BYTES: usize = 512;
/// `location` is cut to this many bytes.
pub(crate) const MAX_LOCATION_BYTES: usize = 512;
/// `event_target`, `module` and each field name are cut to this many bytes.
pub(crate) const MAX_TARGET_BYTES: usize = 256;
/// At most this many field names (tracing's own limit is 32).
pub(crate) const MAX_FIELDS: usize = 32;
/// A spooled body is at most this large; the sender adds `instance_id` and stays under the
/// relay's 64 KiB (TL-Q1).
pub(crate) const MAX_SPOOLED_BYTES: usize = 63 * 1024;
const NANOS_PER_HOUR: i64 = 3_600_000_000_000;

/// Frames of the reporter itself, dropped from the top of a backtrace.
const REPORTER_PREFIXES: [&str; 6] = [
    "cetra_server::observability::crash::report::capture_frames",
    "cetra_server::observability::crash::Reporter::",
    "cetra_server::observability::crash::report_panic",
    "cetra_server::observability::crash::layer::CrashLayer",
    "cetra_server::observability::panic::report",
    "cetra_server::observability::panic::install",
];

/// Crates whose frames above the first kept frame are dropped (design § 3: runtime frames
/// collapsed). `backtrace` and the `tracing` crates are the reporter's own path.
const RUNTIME_ROOTS: [&str; 8] = [
    "std",
    "core",
    "alloc",
    "tokio",
    "backtrace",
    "tracing",
    "tracing_core",
    "tracing_subscriber",
];

/// What a report is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ReportKind {
    /// A panic, from the panic hook.
    Panic,
    /// An ERROR `tracing` event, from the crash layer.
    ErrorEvent,
}

/// The crash report body (D-TL-5). Field order is the JSON key order. The spool holds it
/// without `instance_id`; the sender adds it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CrashReport {
    /// [`CRASH_SCHEMA`].
    pub(crate) schema: u32,
    /// `instance_settings['telemetry.instance_id']`, added by the sender.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) instance_id: Option<Uuid>,
    /// [`facts::VERSION`].
    pub(crate) version: String,
    /// [`facts::TARGET`]: the build's target triple.
    pub(crate) target: String,
    /// [`facts::profile`].
    pub(crate) profile: Profile,
    /// [`facts::deployment`].
    pub(crate) deployment: Deployment,
    /// Time since the process started, at the first occurrence.
    pub(crate) uptime: UptimeBucket,
    /// Panic or error event.
    pub(crate) kind: ReportKind,
    /// `path:line:column` of a panic or `path:line` of an event, path cut by [`report_path`].
    pub(crate) location: String,
    /// The event's `tracing` target; absent for a panic.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) event_target: Option<String>,
    /// The event's module path; absent for a panic or when unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) module: Option<String>,
    /// The callsite's field names, never values; `message` left out. Empty for a panic.
    pub(crate) fields: Vec<String>,
    /// Function names, innermost first, cleaned by [`clean_frames`].
    pub(crate) backtrace: Vec<String>,
    /// Occurrences this report stands for.
    pub(crate) count: u64,
    /// Unix hour of the first of them.
    pub(crate) first_hour: u64,
    /// Unix hour of the last of them.
    pub(crate) last_hour: u64,
}

/// An event callsite, as the crash layer reads it from `tracing` metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EventSite<'a> {
    /// `Metadata::file`.
    pub(crate) file: Option<&'a str>,
    /// `Metadata::line`.
    pub(crate) line: Option<u32>,
    /// `Metadata::target`.
    pub(crate) target: &'a str,
    /// `Metadata::module_path`.
    pub(crate) module_path: Option<&'a str>,
    /// The names of `Metadata::fields`.
    pub(crate) fields: Vec<&'static str>,
}

impl<'a> EventSite<'a> {
    /// The callsite of `metadata`. Reads names only: an event's values are never visited.
    pub(crate) fn from_metadata(metadata: &'a tracing::Metadata<'a>) -> Self {
        Self {
            file: metadata.file(),
            line: metadata.line(),
            target: metadata.target(),
            module_path: metadata.module_path(),
            fields: metadata.fields().iter().map(|field| field.name()).collect(),
        }
    }
}

/// The Unix hour of `now_ns`; 0 before the epoch.
pub(crate) fn unix_hour(now_ns: i64) -> u64 {
    u64::try_from(now_ns.max(0) / NANOS_PER_HOUR).unwrap_or(0)
}

/// The design's path rule: a workspace path (`crates/…`) is kept; a path under
/// `/.cargo/registry/src/<index>/` or `/.cargo/git/checkouts/` keeps what follows; anything
/// else, or any path with a `..` component, keeps its file name only. So no home directory
/// reaches a report. Fails closed: an unknown shape gives the file name, an empty one `unknown`.
pub(crate) fn report_path(file: &str) -> String {
    const REGISTRY: &str = "/.cargo/registry/src/";
    const GIT: &str = "/.cargo/git/checkouts/";
    let file = file.replace('\\', "/");
    let has_parent = file.split('/').any(|part| part == "..");
    if !has_parent {
        if file.starts_with("crates/") {
            return truncate_utf8(&file, MAX_LOCATION_BYTES);
        }
        if let Some(index) = file.find(REGISTRY) {
            // What follows starts with the index directory (`index.crates.io-…`): dropped.
            let rest = &file[index + REGISTRY.len()..];
            if let Some((_, kept)) = rest.split_once('/')
                && !kept.is_empty()
            {
                return truncate_utf8(kept, MAX_LOCATION_BYTES);
            }
        }
        if let Some(index) = file.find(GIT) {
            let kept = &file[index + GIT.len()..];
            if !kept.is_empty() {
                return truncate_utf8(kept, MAX_LOCATION_BYTES);
            }
        }
    }
    match file.rsplit('/').next() {
        Some(name) if !name.is_empty() => truncate_utf8(name, MAX_LOCATION_BYTES),
        _ => "unknown".to_owned(),
    }
}

/// `path:line:column` of a panic, or `unknown`.
pub(crate) fn location_of_panic(location: Option<&Location<'_>>) -> String {
    location.map_or_else(
        || "unknown".to_owned(),
        |location| {
            let text = format!(
                "{}:{}:{}",
                report_path(location.file()),
                location.line(),
                location.column()
            );
            truncate_utf8(&text, MAX_LOCATION_BYTES)
        },
    )
}

/// `path:line` of an event, `path` without a line, or `unknown` without a file.
pub(crate) fn location_of_event(file: Option<&str>, line: Option<u32>) -> String {
    let text = match (file, line) {
        (Some(file), Some(line)) => format!("{}:{line}", report_path(file)),
        (Some(file), None) => report_path(file),
        (None, _) => return "unknown".to_owned(),
    };
    truncate_utf8(&text, MAX_LOCATION_BYTES)
}

/// `text` cut to at most `max` bytes, on a character boundary.
pub(crate) fn truncate_utf8(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let mut end = max;
    // Index 0 is always a boundary, so the loop ends.
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

/// `name` without a legacy-mangling hash suffix (`::h` and 16 hex digits).
pub(crate) fn strip_hash(name: &str) -> &str {
    const SUFFIX_LEN: usize = 19;
    let Some(start) = name.len().checked_sub(SUFFIX_LEN) else {
        return name;
    };
    match name.get(start..) {
        Some(tail)
            if tail.starts_with("::h")
                && tail
                    .get(3..)
                    .is_some_and(|hex| hex.bytes().all(|byte| byte.is_ascii_hexdigit())) =>
        {
            name.get(..start).unwrap_or(name)
        }
        _ => name,
    }
}

/// The first path segment of a function name, ignoring leading `<` (`<alloc::boxed::Box<F>
/// as …>::call` is `alloc`); empty for a name without `::` (a C symbol).
fn root(name: &str) -> &str {
    let trimmed = name.trim_start_matches('<');
    trimmed.find("::").map_or("", |end| &trimmed[..end])
}

/// A frame of a runtime crate or a C symbol.
fn is_runtime(name: &str) -> bool {
    let root = root(name);
    root.is_empty() || RUNTIME_ROOTS.contains(&root)
}

/// A frame of the reporter itself.
fn is_reporter(name: &str) -> bool {
    let trimmed = name.trim_start_matches('<');
    REPORTER_PREFIXES
        .iter()
        .any(|prefix| trimmed.starts_with(prefix))
}

/// Raw demangled names, innermost first, to the report's `backtrace`: hash suffix removed,
/// each cut to [`MAX_NAME_BYTES`], empty names skipped, the leading run of reporter and
/// runtime frames dropped, then at most [`MAX_FRAMES`].
pub(crate) fn clean_frames(raw: Vec<String>) -> Vec<String> {
    raw.into_iter()
        .map(|name| truncate_utf8(strip_hash(&name), MAX_NAME_BYTES))
        .filter(|name| !name.is_empty())
        .skip_while(|name| is_runtime(name) || is_reporter(name))
        .take(MAX_FRAMES)
        .collect()
}

/// The current thread's stack as cleaned function names. Symbols come from the binary's
/// symbol table (release archives keep it, D-N-16); a frame without a name is skipped.
pub(crate) fn capture_frames() -> Vec<String> {
    let mut names = Vec::new();
    backtrace::trace(|frame| {
        // The global lock `trace` holds is re-entrant on this thread (backtrace's `mod lock`).
        backtrace::resolve_frame(frame, |symbol| {
            if let Some(name) = symbol.name() {
                // `{:#}` drops the hash suffix of legacy mangling.
                names.push(format!("{name:#}"));
            }
        });
        names.len() < MAX_RAW_FRAMES
    });
    clean_frames(names)
}

/// A panic report: the location, the frames and the build facts. There is no message
/// parameter: Rust's panic messages embed data, so the message is dropped whole (D-TL-5).
pub(crate) fn panic_report(
    location: Option<&Location<'_>>,
    frames: Vec<String>,
    deployment: Deployment,
    uptime: Duration,
) -> CrashReport {
    CrashReport {
        schema: CRASH_SCHEMA,
        instance_id: None,
        version: facts::VERSION.to_owned(),
        target: facts::TARGET.to_owned(),
        profile: facts::profile(),
        deployment,
        uptime: uptime_bucket(uptime),
        kind: ReportKind::Panic,
        location: location_of_panic(location),
        event_target: None,
        module: None,
        fields: Vec::new(),
        backtrace: frames,
        count: 1,
        first_hour: 0,
        last_hour: 0,
    }
}

/// An error-event report: the callsite's location, target, module and field names.
pub(crate) fn event_report(
    site: &EventSite<'_>,
    frames: Vec<String>,
    deployment: Deployment,
    uptime: Duration,
) -> CrashReport {
    CrashReport {
        schema: CRASH_SCHEMA,
        instance_id: None,
        version: facts::VERSION.to_owned(),
        target: facts::TARGET.to_owned(),
        profile: facts::profile(),
        deployment,
        uptime: uptime_bucket(uptime),
        kind: ReportKind::ErrorEvent,
        location: location_of_event(site.file, site.line),
        event_target: Some(truncate_utf8(site.target, MAX_TARGET_BYTES)),
        module: site
            .module_path
            .map(|module| truncate_utf8(module, MAX_TARGET_BYTES)),
        fields: site
            .fields
            .iter()
            .filter(|name| **name != "message")
            .take(MAX_FIELDS)
            .map(|name| truncate_utf8(name, MAX_TARGET_BYTES))
            .collect(),
        backtrace: frames,
        count: 1,
        first_hour: 0,
        last_hour: 0,
    }
}

/// The spooled body of `report`: its JSON if at most [`MAX_SPOOLED_BYTES`], else the JSON
/// without its backtrace if that fits, else `None`.
pub(crate) fn encode(report: &CrashReport) -> Option<Vec<u8>> {
    let body = serde_json::to_vec(report).ok()?;
    if body.len() <= MAX_SPOOLED_BYTES {
        return Some(body);
    }
    let mut smaller = report.clone();
    smaller.backtrace.clear();
    let body = serde_json::to_vec(&smaller).ok()?;
    (body.len() <= MAX_SPOOLED_BYTES).then_some(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-01-01T00:00Z in Unix hours.
    const HOUR0: u64 = 490_896;

    /// The event report of [`golden_event`], pretty-printed.
    const EVENT_GOLDEN: &str = r#"{
  "schema": 1,
  "instance_id": "8f14e45f-ceea-4e7a-9d2b-1c3f5a6b7c8d",
  "version": "0.1.0",
  "target": "x86_64-unknown-linux-musl",
  "profile": "release",
  "deployment": "docker",
  "uptime": "1h-1d",
  "kind": "error_event",
  "location": "crates/storage/src/wal.rs:412",
  "event_target": "cetra_storage::wal",
  "module": "cetra_storage::wal",
  "fields": [
    "error",
    "project_id"
  ],
  "backtrace": [
    "cetra_storage::wal::Wal::sync",
    "cetra_storage::flush::run"
  ],
  "count": 3,
  "first_hour": 490896,
  "last_hour": 490897
}"#;

    /// The spooled body of [`golden_panic`] (compact: [`encode`] uses `to_vec`).
    const PANIC_GOLDEN: &str = r#"{"schema":1,"version":"0.1.0","target":"x86_64-unknown-linux-musl","profile":"release","deployment":"binary","uptime":"<1h","kind":"panic","location":"crates/storage/src/wal.rs:412:9","fields":[],"backtrace":["cetra_storage::wal::Wal::sync"],"count":1,"first_hour":490896,"last_hour":490896}"#;

    fn golden_event() -> CrashReport {
        CrashReport {
            schema: 1,
            instance_id: Some(Uuid::parse_str("8f14e45f-ceea-4e7a-9d2b-1c3f5a6b7c8d").unwrap()),
            version: "0.1.0".into(),
            target: "x86_64-unknown-linux-musl".into(),
            profile: Profile::Release,
            deployment: Deployment::Docker,
            uptime: UptimeBucket::HourToDay,
            kind: ReportKind::ErrorEvent,
            location: "crates/storage/src/wal.rs:412".into(),
            event_target: Some("cetra_storage::wal".into()),
            module: Some("cetra_storage::wal".into()),
            fields: vec!["error".into(), "project_id".into()],
            backtrace: vec![
                "cetra_storage::wal::Wal::sync".into(),
                "cetra_storage::flush::run".into(),
            ],
            count: 3,
            first_hour: HOUR0,
            last_hour: HOUR0 + 1,
        }
    }

    fn golden_panic() -> CrashReport {
        CrashReport {
            schema: 1,
            instance_id: None,
            version: "0.1.0".into(),
            target: "x86_64-unknown-linux-musl".into(),
            profile: Profile::Release,
            deployment: Deployment::Binary,
            uptime: UptimeBucket::UnderHour,
            kind: ReportKind::Panic,
            location: "crates/storage/src/wal.rs:412:9".into(),
            event_target: None,
            module: None,
            fields: Vec::new(),
            backtrace: vec!["cetra_storage::wal::Wal::sync".into()],
            count: 1,
            first_hour: HOUR0,
            last_hour: HOUR0,
        }
    }

    #[test]
    fn the_event_report_matches_the_golden_json() {
        let text = serde_json::to_string_pretty(&golden_event()).unwrap();
        assert_eq!(text, EVENT_GOLDEN);
    }

    #[test]
    fn the_spooled_panic_report_matches_the_golden_json() {
        let bytes = encode(&golden_panic()).unwrap();
        assert_eq!(String::from_utf8(bytes).unwrap(), PANIC_GOLDEN);
    }

    #[test]
    fn the_key_set_is_fixed() {
        let value: serde_json::Value = serde_json::from_str(EVENT_GOLDEN).unwrap();
        let keys: Vec<&str> = value
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            [
                "schema",
                "instance_id",
                "version",
                "target",
                "profile",
                "deployment",
                "uptime",
                "kind",
                "location",
                "event_target",
                "module",
                "fields",
                "backtrace",
                "count",
                "first_hour",
                "last_hour",
            ]
        );
        for key in keys {
            assert!(!key.contains("message"), "{key}");
            assert!(!key.contains("value"), "{key}");
            assert!(!key.contains("licen"), "{key}");
            assert!(!key.contains("host"), "{key}");
        }
    }

    #[test]
    fn a_report_round_trips_through_json() {
        for report in [golden_event(), golden_panic()] {
            let text = serde_json::to_string(&report).unwrap();
            let back: CrashReport = serde_json::from_str(&text).unwrap();
            assert_eq!(back, report);
        }
    }

    #[test]
    fn an_unknown_key_is_refused() {
        let text = PANIC_GOLDEN.replace(r#""schema":1,"#, r#""schema":1,"message":"x","#);
        assert!(serde_json::from_str::<CrashReport>(&text).is_err());
    }

    #[test]
    fn a_missing_instance_id_reads_as_none() {
        let back: CrashReport = serde_json::from_str(PANIC_GOLDEN).unwrap();
        assert_eq!(back.instance_id, None);
        assert_eq!(back, golden_panic());
    }

    #[test]
    fn report_path_rules() {
        let cases = [
            ("crates/storage/src/wal.rs", "crates/storage/src/wal.rs"),
            (
                "/home/planted-user/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/arrow-55.0.0/src/array.rs",
                "arrow-55.0.0/src/array.rs",
            ),
            (
                "/home/planted-user/.cargo/git/checkouts/foo-abc123/def456/src/lib.rs",
                "foo-abc123/def456/src/lib.rs",
            ),
            (
                "/home/planted-user/src/cetra/crates/storage/src/wal.rs",
                "wal.rs",
            ),
            (
                "/usr/local/cargo/registry/src/index.crates.io-x/tokio-1.54.0/src/lib.rs",
                "lib.rs",
            ),
            (
                "/rustc/90b35a6239c3d8bdabc530a6a0816f7ff89a0aaf/library/core/src/option.rs",
                "option.rs",
            ),
            ("crates/../../home/planted-user/secret.rs", "secret.rs"),
            (
                "/home/planted-user/.cargo/registry/src/index/../../../etc/passwd",
                "passwd",
            ),
            (
                r"C:\Users\planted-user\.cargo\registry\src\index.crates.io-x\tokio-1.54.0\src\lib.rs",
                "tokio-1.54.0/src/lib.rs",
            ),
            (
                "/home/planted-user/.cargo/registry/src/index.crates.io-x",
                "index.crates.io-x",
            ),
            ("src/main.rs", "main.rs"),
            ("", "unknown"),
            ("/home/planted-user/", "unknown"),
        ];
        for (input, expected) in cases {
            let out = report_path(input);
            assert_eq!(out, expected, "input {input}");
            assert!(!out.contains("planted-user"), "output {out}");
        }
        let long = format!("crates/{}", "a".repeat(600));
        assert_eq!(report_path(&long).len(), MAX_LOCATION_BYTES);
    }

    #[test]
    fn location_of_panic_rules() {
        let here = Location::caller();
        let text = location_of_panic(Some(here));
        assert!(
            text.starts_with("crates/server/src/observability/crash/report.rs:"),
            "{text}"
        );
        assert_eq!(text.split(':').count(), 3, "{text}");
        assert_eq!(location_of_panic(None), "unknown");
    }

    #[test]
    fn location_of_event_rules() {
        assert_eq!(
            location_of_event(Some("crates/x.rs"), Some(7)),
            "crates/x.rs:7"
        );
        assert_eq!(location_of_event(Some("crates/x.rs"), None), "crates/x.rs");
        assert_eq!(location_of_event(None, Some(7)), "unknown");
    }

    #[test]
    fn truncate_utf8_cuts_on_a_boundary() {
        let text = "aé".repeat(300);
        let cut = truncate_utf8(&text, 11);
        assert!(cut.len() <= 11);
        assert!(text.starts_with(&cut));
        assert_eq!(truncate_utf8("short", 11), "short");
        assert_eq!(truncate_utf8(&text, 0), "");
    }

    #[test]
    fn strip_hash_rules() {
        assert_eq!(strip_hash("a::b::h0123456789abcdef"), "a::b");
        assert_eq!(strip_hash("a::b::h0123"), "a::b::h0123");
        assert_eq!(
            strip_hash("a::b::hXYZ0123456789abcd"),
            "a::b::hXYZ0123456789abcd"
        );
        assert_eq!(strip_hash("a::{{closure}}"), "a::{{closure}}");
        assert_eq!(strip_hash("::h0123456789abcdef"), "");
        assert_eq!(strip_hash("ab"), "ab");
    }

    #[test]
    fn a_panic_stack_starts_at_the_first_cetra_frame() {
        let raw = [
            "backtrace::backtrace::libunwind::trace::h0123456789abcdef",
            "_Unwind_Backtrace",
            "cetra_server::observability::crash::report::capture_frames",
            "cetra_server::observability::crash::Reporter::report_panic",
            "cetra_server::observability::crash::report_panic",
            "cetra_server::observability::panic::report",
            "cetra_server::observability::panic::install::{{closure}}",
            "<alloc::boxed::Box<F,A> as core::ops::function::Fn<Args>>::call",
            "std::panicking::rust_panic_with_hook",
            "rust_begin_unwind",
            "core::panicking::panic_fmt",
            "core::result::unwrap_failed",
            "cetra_storage::wal::Wal::sync::h0123456789abcdef",
            "cetra_storage::flush::run::{{closure}}",
            "<core::pin::Pin<P> as core::future::future::Future>::poll",
            "tokio::runtime::task::harness::poll_future",
            "cetra_server::serve::bind",
            "main",
        ];
        let frames = clean_frames(raw.iter().map(|name| (*name).to_owned()).collect());
        assert_eq!(
            frames,
            [
                "cetra_storage::wal::Wal::sync",
                "cetra_storage::flush::run::{{closure}}",
                "<core::pin::Pin<P> as core::future::future::Future>::poll",
                "tokio::runtime::task::harness::poll_future",
                "cetra_server::serve::bind",
                "main",
            ]
        );
    }

    #[test]
    fn an_event_stack_drops_the_tracing_frames() {
        let raw = [
            "backtrace::backtrace::trace",
            "cetra_server::observability::crash::report::capture_frames",
            "cetra_server::observability::crash::Reporter::report_event",
            "<cetra_server::observability::crash::layer::CrashLayer as tracing_subscriber::layer::Layer<S>>::on_event",
            "<alloc::vec::Vec<L> as tracing_subscriber::layer::Layer<S>>::on_event",
            "<tracing_subscriber::layer::layered::Layered<L,S> as tracing_core::subscriber::Subscriber>::event",
            "tracing_core::event::Event::dispatch",
            "cetra_storage::wal::Wal::sync",
        ];
        let frames = clean_frames(raw.iter().map(|name| (*name).to_owned()).collect());
        assert_eq!(frames, ["cetra_storage::wal::Wal::sync"]);
    }

    #[test]
    fn a_dependency_frame_above_cetra_is_kept() {
        let raw = [
            "core::panicking::panic_bounds_check",
            "arrow_array::array::PrimitiveArray<T>::value",
            "cetra_storage::query::scan",
        ];
        let frames = clean_frames(raw.iter().map(|name| (*name).to_owned()).collect());
        assert_eq!(
            frames,
            [
                "arrow_array::array::PrimitiveArray<T>::value",
                "cetra_storage::query::scan"
            ]
        );
    }

    #[test]
    fn at_most_48_frames() {
        let raw: Vec<String> = (0..100).map(|i| format!("cetra_x::f{i}")).collect();
        let frames = clean_frames(raw);
        assert_eq!(frames.len(), MAX_FRAMES);
        assert_eq!(frames[0], "cetra_x::f0");
    }

    #[test]
    fn long_names_are_cut() {
        let raw = vec![format!("cetra_x::{}", "é".repeat(400))];
        let frames = clean_frames(raw);
        assert_eq!(frames.len(), 1);
        assert!(frames[0].len() <= MAX_NAME_BYTES);
    }

    #[test]
    fn empty_names_are_skipped() {
        let raw = vec![
            String::new(),
            "cetra_x::a".to_owned(),
            String::new(),
            "cetra_x::b".to_owned(),
        ];
        assert_eq!(clean_frames(raw), ["cetra_x::a", "cetra_x::b"]);
    }

    #[test]
    fn an_all_runtime_stack_is_empty() {
        let raw = vec![
            "std::rt::lang_start".to_owned(),
            "core::ops::function::FnOnce::call_once".to_owned(),
            "tokio::runtime::park".to_owned(),
        ];
        assert!(clean_frames(raw).is_empty());
    }

    #[test]
    fn capture_frames_names_its_caller() {
        #[inline(never)]
        fn marker_for_capture_test() -> Vec<String> {
            capture_frames()
        }
        let frames = marker_for_capture_test();
        let position = frames
            .iter()
            .position(|frame| frame.contains("marker_for_capture_test"))
            .expect("the caller is in the captured stack");
        assert!(
            frames[..position]
                .iter()
                .all(|frame| !frame.starts_with("backtrace::"))
        );
        assert!(
            frames
                .iter()
                .all(|frame| strip_hash(frame) == frame.as_str())
        );
    }

    #[test]
    fn panic_report_has_only_the_location_and_frames() {
        let report = panic_report(
            Some(Location::caller()),
            vec!["cetra_x::f".into()],
            Deployment::Binary,
            Duration::from_secs(10),
        );
        assert_eq!(report.kind, ReportKind::Panic);
        assert!(
            report
                .location
                .starts_with("crates/server/src/observability/crash/report.rs:"),
            "{}",
            report.location
        );
        assert!(report.fields.is_empty());
        assert_eq!(report.event_target, None);
        assert_eq!(report.module, None);
        assert_eq!(report.uptime, UptimeBucket::UnderHour);
        assert_eq!(report.version, facts::VERSION);
        assert_eq!(report.target, facts::TARGET);
        assert_eq!(report.count, 1);
        assert_eq!(report.backtrace, ["cetra_x::f"]);
    }

    #[test]
    fn event_report_reads_names_only() {
        let site = EventSite {
            file: Some("crates/x.rs"),
            line: Some(3),
            target: "cetra_x",
            module_path: None,
            fields: vec!["message", "error", "project_id"],
        };
        let report = event_report(
            &site,
            Vec::new(),
            Deployment::Binary,
            Duration::from_secs(10),
        );
        assert_eq!(report.fields, ["error", "project_id"]);
        assert_eq!(report.module, None);
        assert_eq!(report.event_target, Some("cetra_x".to_owned()));
        assert_eq!(report.location, "crates/x.rs:3");
    }

    #[test]
    fn event_report_caps_fields() {
        const NAMES: [&str; 40] = [
            "n00", "n01", "n02", "n03", "n04", "n05", "n06", "n07", "n08", "n09", "n10", "n11",
            "n12", "n13", "n14", "n15", "n16", "n17", "n18", "n19", "n20", "n21", "n22", "n23",
            "n24", "n25", "n26", "n27", "n28", "n29", "n30", "n31", "n32", "n33", "n34", "n35",
            "n36", "n37", "n38", "n39",
        ];
        let site = EventSite {
            file: Some("crates/x.rs"),
            line: Some(3),
            target: "cetra_x",
            module_path: Some("cetra_x"),
            fields: NAMES.to_vec(),
        };
        let report = event_report(
            &site,
            Vec::new(),
            Deployment::Binary,
            Duration::from_secs(10),
        );
        assert_eq!(report.fields.len(), MAX_FIELDS);
        assert_eq!(report.fields[0], "n00");
    }

    #[test]
    fn encode_keeps_a_normal_report() {
        let report = golden_event();
        let bytes = encode(&report).unwrap();
        let back: CrashReport = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(back, report);
    }

    #[test]
    fn encode_drops_the_backtrace_when_too_large() {
        let mut report = golden_event();
        report.backtrace = vec!["\u{1}".repeat(MAX_NAME_BYTES); MAX_FRAMES];
        let bytes = encode(&report).unwrap();
        assert!(bytes.len() <= MAX_SPOOLED_BYTES);
        let back: CrashReport = serde_json::from_slice(&bytes).unwrap();
        assert!(back.backtrace.is_empty());
    }

    #[test]
    fn encode_refuses_what_never_fits() {
        let mut report = golden_event();
        report.fields = vec!["x".repeat(256); 300];
        assert_eq!(encode(&report), None);
    }

    #[test]
    fn unix_hour_rules() {
        assert_eq!(unix_hour(0), 0);
        assert_eq!(unix_hour(-1), 0);
        assert_eq!(unix_hour(1_767_225_600_000_000_000), HOUR0);
        let _ = unix_hour(i64::MAX);
    }
}
