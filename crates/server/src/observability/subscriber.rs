//! The `tracing` subscriber of `cetra serve` (track TL § Architecture): one registry, one layer per sink, each with its own filter. The stdout layer is `log.level` with `rmcp` capped; it sees events only, so the spans never change its lines. The OTLP log layer is `self_monitoring.level` with the same cap, minus the exporter's own stack. The OTLP span layer exports cetra's own spans (track TL M2) with the events the log layer would export. Later milestones add their layer to [`build`]'s list and their flush to [`Observability::shutdown`]. The flush runs after `serve::run` returns, outside spec 12's 60 s shutdown bound. The ring buffer layer (track TL M3) keeps the stdout-level lines as JSON for the admin socket's `debug_info`.

use std::sync::Arc;
use std::time::Duration;

use opentelemetry_sdk::error::OTelSdkResult;
use opentelemetry_sdk::trace::SdkTracer;
use tokio::runtime::Handle;
use tracing::Level;
use tracing_opentelemetry::OpenTelemetryLayer;
use tracing_subscriber::filter::{EnvFilter, FilterExt, LevelFilter, ParseError, filter_fn};
use tracing_subscriber::fmt::writer::BoxMakeWriter;
use tracing_subscriber::layer::{Layered, SubscriberExt};
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{Layer, Registry};

use crate::config::{LogFormat, ServerConfig};
use crate::observability::crash::spool::Spool;
use crate::observability::crash::{self, Reporter};
use crate::observability::switchboard::{NOTICE_DIRECTIVE, Switch, for_config};

use super::ObservabilityError;
use super::otlp::{EXPORT_FAILURE_TARGET, LogPipeline, SpanPipeline};
use super::ring::{self, LogRing};

#[cfg(test)]
use super::otlp::receiver::{Captured, Receiver};
#[cfg(test)]
use std::future::Future;

/// How long [`Observability::shutdown`] waits for the export to flush.
pub(crate) const FLUSH_TIMEOUT: Duration = Duration::from_secs(2);

/// The `rmcp` crate logs whole MCP requests and results below `warn`; they can hold prompts
/// and SQL text, so its level is capped whatever `log.level` says (README § Privacy).
const RMCP_DIRECTIVE: &str = "rmcp=warn";

/// Target prefixes the export never carries: the crates of the exporter's own stack that log
/// per request (an export they log would be exported, and so on) and the export's failure
/// lines. A prefix covers longer names: `hyper` covers `hyper_util`, `tower` covers
/// `tower_http`, `http_body` covers `http_body_util`. The crash layer skips the same targets
/// (track TL M5).
pub(crate) const EXPORT_DROPPED_TARGETS: &[&str] = &[
    "opentelemetry",
    "hyper",
    "reqwest",
    "h2",
    "tower",
    "rustls",
    "tokio_rustls",
    "webpki",
    "want",
    "http_body",
    EXPORT_FAILURE_TARGET,
];

/// One layer of the composed subscriber.
pub(crate) type BoxedLayer = Box<dyn Layer<Registry> + Send + Sync + 'static>;

/// The registry with every layer.
pub(crate) type ComposedSubscriber = Layered<Vec<BoxedLayer>, Registry>;

/// What the installed subscriber exports through. Dropping it does not flush: call
/// [`Observability::shutdown`].
#[derive(Debug)]
pub struct Observability {
    logs: Option<LogPipeline>,
    traces: Option<SpanPipeline>,
    /// The ring buffer of the last log lines (track TL M3), installed by [`init`].
    ring: LogRing,
    /// The crash reporter when crash reports are on (track TL M5).
    crash: Option<Arc<Reporter>>,
}

impl Observability {
    /// Whether log records are exported.
    pub fn exporting_logs(&self) -> bool {
        self.logs.is_some()
    }

    /// Whether spans are exported.
    pub fn exporting_traces(&self) -> bool {
        self.traces.is_some()
    }

    /// Whether the crash layer is installed.
    pub fn reporting_crashes(&self) -> bool {
        self.crash.is_some()
    }

    /// The ring buffer this subscriber writes.
    #[cfg(test)]
    pub(crate) fn log_ring(&self) -> &LogRing {
        &self.ring
    }

    /// Flushes and stops both exports in parallel on blocking threads, waiting at most
    /// [`FLUSH_TIMEOUT`] (plus 500 ms for the hand-off). Never fails: a flush that errs or
    /// times out writes one `warn` line under [`EXPORT_FAILURE_TARGET`] (stdout only).
    pub async fn shutdown(self) {
        let logs = self
            .logs
            .map(|pipeline| tokio::task::spawn_blocking(move || pipeline.shutdown(FLUSH_TIMEOUT)));
        let traces = self
            .traces
            .map(|pipeline| tokio::task::spawn_blocking(move || pipeline.shutdown(FLUSH_TIMEOUT)));
        let (logs_flushed, traces_flushed) = tokio::join!(flushed(logs), flushed(traces));
        if !logs_flushed {
            tracing::warn!(
                target: EXPORT_FAILURE_TARGET,
                "self-monitoring: the log export did not flush within 2 s"
            );
        }
        if !traces_flushed {
            tracing::warn!(
                target: EXPORT_FAILURE_TARGET,
                "self-monitoring: the span export did not flush within 2 s"
            );
        }
    }
}

/// Whether a flush started on a blocking thread succeeded within [`FLUSH_TIMEOUT`] plus
/// 500 ms. No flush counts as success. The SDK's error is not logged: it can hold `Debug`
/// of internals.
async fn flushed(flush: Option<tokio::task::JoinHandle<OTelSdkResult>>) -> bool {
    let Some(flush) = flush else {
        return true;
    };
    matches!(
        tokio::time::timeout(FLUSH_TIMEOUT + Duration::from_millis(500), flush).await,
        Ok(Ok(Ok(())))
    )
}

/// Builds the subscriber of `cetra serve` and installs it as the global default (with the
/// `log` bridge). `format` is the resolved stdout format. Call it inside the tokio runtime.
pub fn init(config: &ServerConfig, format: LogFormat) -> Result<Observability, ObservabilityError> {
    let runtime = Handle::try_current().map_err(|_| ObservabilityError::NoRuntime)?;
    let (subscriber, observability) =
        build(config, format, BoxMakeWriter::new(std::io::stdout), runtime)?;
    subscriber
        .try_init()
        .map_err(|_| ObservabilityError::AlreadyInstalled)?;
    if let Some(reporter) = &observability.crash {
        crash::enable_panic_reports(Arc::clone(reporter));
    }
    ring::install(observability.ring.clone());
    if let Some(pipeline) = &observability.logs {
        tracing::info!(
            host = pipeline.host(),
            level = %config.self_monitoring.level,
            "self-monitoring: exporting logs over OTLP"
        );
    }
    if let Some(pipeline) = &observability.traces {
        tracing::info!(
            host = pipeline.host(),
            ratio = config.self_monitoring.trace_sample_ratio.get(),
            "self-monitoring: exporting traces over OTLP"
        );
    }
    Ok(observability)
}

/// The subscriber and its guard, not installed. `writer` is stdout in production.
pub(crate) fn build(
    config: &ServerConfig,
    format: LogFormat,
    writer: BoxMakeWriter,
    runtime: Handle,
) -> Result<(ComposedSubscriber, Observability), ObservabilityError> {
    let stdout_filter = log_filter(&config.log.level).map_err(ObservabilityError::LogLevel)?;
    // No other option: the defaults are those of the former `fmt().pretty()` and
    // `fmt().json()` builders, so stdout lines are unchanged.
    // Events only: a span this layer does not see is never printed (json's `span` and
    // `spans` keys, pretty's `in` lines), so stdout lines are the same with spans (track TL
    // M2). The json span options are off as well.
    // The hint keeps `log.level`'s: `And` takes the more verbose of its two filters'
    // hints, and a `filter_fn` without one would count as TRACE, so every disabled
    // `debug!` and `trace!` callsite would be evaluated again.
    let stdout_events =
        filter_fn(|metadata| metadata.is_event()).with_max_level_hint(LevelFilter::TRACE);
    let stdout: BoxedLayer = match format {
        LogFormat::Pretty => tracing_subscriber::fmt::layer()
            .pretty()
            .with_writer(writer)
            .with_filter(FilterExt::<Registry>::and(stdout_filter, stdout_events))
            .boxed(),
        LogFormat::Json => tracing_subscriber::fmt::layer()
            .json()
            .with_current_span(false)
            .with_span_list(false)
            .with_writer(writer)
            .with_filter(FilterExt::<Registry>::and(stdout_filter, stdout_events))
            .boxed(),
    };
    let export_level_filter =
        export_filter(&config.self_monitoring.level).map_err(ObservabilityError::ExportLevel)?;
    let mut layers: Vec<BoxedLayer> = vec![stdout];
    // The ring keeps what stdout shows, minus the libraries' own lines, as JSON whatever
    // the stdout format (track TL M3).
    let ring = LogRing::new();
    let ring_filter = log_filter(&config.log.level).map_err(ObservabilityError::LogLevel)?;
    // Events only: the ring formats events, and a span it enabled would cost a registry slot
    // and a fields extension per request with nothing recorded (M2's notes for M3).
    let ring_targets =
        filter_fn(|metadata| metadata.is_event() && ring_target_allowed(metadata.target()));
    layers.push(ring.layer(FilterExt::<Registry>::and(ring_filter, ring_targets)));
    let logs = match config.self_monitoring.logs_endpoint() {
        Some(endpoint) => {
            let pipeline = LogPipeline::new(&config.self_monitoring, endpoint, runtime.clone())?;
            // The log bridge ignores spans; without `is_event` here, `self_monitoring.level =
            // "info"` would enable every `info` span even with traces off.
            let targets = filter_fn(|metadata| {
                metadata.is_event() && export_target_allowed(metadata.target(), *metadata.level())
            });
            let export: BoxedLayer = pipeline
                .bridge()
                .with_filter(FilterExt::<Registry>::and(export_level_filter, targets))
                .boxed();
            layers.push(export);
            Some(pipeline)
        }
        None => None,
    };
    let traces = match config.self_monitoring.traces_endpoint() {
        Some(endpoint) => {
            let events = export_filter(&config.self_monitoring.level)
                .map_err(ObservabilityError::ExportLevel)?;
            let pipeline = SpanPipeline::new(&config.self_monitoring, endpoint, runtime)?;
            layers.push(span_layer(pipeline.tracer(), events));
            Some(pipeline)
        }
        None => None,
    };
    // Crash reports to us, when the switchboard allows them (track TL M5, D-TL-11). The
    // decision is pure, so `serve::bind` reaches the same one for the sender.
    let crash = match for_config(config).crash_reports {
        Switch::On => {
            let reporter = Arc::new(Reporter::system(Spool::for_data_dir(&config.data_dir)));
            layers.push(crash::layer::boxed(Arc::clone(&reporter)));
            Some(reporter)
        }
        Switch::Off(_) => None,
    };
    Ok((
        tracing_subscriber::registry().with(layers),
        Observability {
            logs,
            traces,
            ring,
            crash,
        },
    ))
}

/// `level` as an `EnvFilter`, with [`RMCP_DIRECTIVE`] added last so it replaces any
/// `rmcp=…` directive in `level`. Shared by the stdout and export filters.
fn capped_filter(level: &str) -> Result<EnvFilter, ParseError> {
    let rmcp = RMCP_DIRECTIVE.parse()?;
    Ok(EnvFilter::try_new(level)?.add_directive(rmcp))
}

/// The stdout layer's filter: [`capped_filter`] plus the telemetry notice directive, which
/// lets the start-up notice through at `info` whatever `log.level` says. The export filter
/// never gets it, so the notice is not exported over OTLP (D-TL-11, design § Startup notice).
pub(crate) fn log_filter(level: &str) -> Result<EnvFilter, ParseError> {
    let notice = NOTICE_DIRECTIVE.parse()?;
    Ok(capped_filter(level)?.add_directive(notice))
}

/// The export layer's filter: `self_monitoring.level`, capped, and nothing else.
fn export_filter(level: &str) -> Result<EnvFilter, ParseError> {
    capped_filter(level)
}

/// Whether an event of `target` at `level` may be exported: not under a dropped prefix, and
/// `rmcp` at `warn` or above whatever directive matched.
fn export_target_allowed(target: &str, level: Level) -> bool {
    if EXPORT_DROPPED_TARGETS
        .iter()
        .any(|dropped| target.starts_with(dropped))
    {
        return false;
    }
    !(target.starts_with("rmcp") && level > Level::WARN)
}

/// Target prefix of cetra's own crates.
const CETRA_TARGET_PREFIX: &str = "cetra_";

/// Whether a span of `target` at `level` is exported: cetra's own, at `info` or above. Spans
/// of other crates never are: rmcp's carry whole MCP requests (D-TL-4).
fn span_target_allowed(target: &str, level: Level) -> bool {
    target.starts_with(CETRA_TARGET_PREFIX) && level <= Level::INFO
}

/// The span layer of `tracer`. It sees cetra's spans and the events that `events` (the
/// export filter of `self_monitoring.level`) and [`export_target_allowed`] pass, so a span
/// records what the log export carries (D-TL-3). An ERROR event inside a span sets its status
/// to error (`OpenTelemetryLayer::on_event`, whatever the settings). Everything else the layer
/// can add is off: code location, thread, target, level and busy/idle attributes, OTel context
/// activation (a parent is the enclosing `tracing` span this layer sees, or an explicit
/// `parent:`), and the exception mappings, which would copy error text into span attributes
/// and statuses. No propagator exists, so no incoming `traceparent` is read (D-TL-7).
fn span_layer(tracer: SdkTracer, events: EnvFilter) -> BoxedLayer {
    let spans = filter_fn(|metadata| {
        metadata.is_span() && span_target_allowed(metadata.target(), *metadata.level())
    });
    let events = FilterExt::<Registry>::and(
        events,
        filter_fn(|metadata| {
            metadata.is_event() && export_target_allowed(metadata.target(), *metadata.level())
        }),
    );
    OpenTelemetryLayer::<Registry, SdkTracer>::new(tracer)
        .with_location(false)
        .with_threads(false)
        .with_target(false)
        .with_level(false)
        .with_tracked_inactivity(false)
        .with_context_activation(false)
        .with_error_fields_to_exceptions(false)
        .with_error_records_to_exceptions(false)
        .with_error_events_to_exceptions(false)
        .with_error_events_to_status(false)
        .with_filter(FilterExt::<Registry>::or(spans, events))
        .boxed()
}

/// Whether an event of `target` may enter the ring buffer, which goes into a file meant for a
/// public issue: not the exporter stack of [`EXPORT_DROPPED_TARGETS`] (HTTP, TLS and
/// OpenTelemetry crates, whose `debug`/`trace` output can carry headers and URLs) and not
/// `sqlx` (statement text), whatever `log.level` lets through. The export's own failure
/// lines ([`EXPORT_FAILURE_TARGET`]) are kept: they help a bug report. The `rmcp` cap is in
/// the stdout filter already.
fn ring_target_allowed(target: &str) -> bool {
    target == EXPORT_FAILURE_TARGET
        || (export_target_allowed(target, Level::ERROR) && !target.starts_with("sqlx"))
}

/// Runs `future` under a subscriber that exports every span (ratio 1.0, logs off, events at
/// `info` on spans) to an in-process receiver, then flushes. Returns the future's output and
/// what arrived. The subscriber is this thread's default while `future` runs: a task spawned
/// on another worker thread is not captured, so tests whose spans start in a spawned task
/// (rmcp's tool calls) use a current-thread runtime. Works on both runtime flavours.
#[cfg(test)]
pub(crate) async fn capture_spans<T>(future: impl Future<Output = T>) -> (T, Captured) {
    let receiver = Receiver::start(axum::http::StatusCode::OK, Duration::ZERO).await;
    let toml = format!(
        "[self_monitoring]\notlp_endpoint = \"{}\"\nlogs = false\ntrace_sample_ratio = 1.0\nlevel = \"info\"\n",
        receiver.base
    );
    let config = crate::config::load_from(Some(&toml), &[]).expect("capture config");
    let (subscriber, observability) = build(
        &config,
        LogFormat::Json,
        BoxMakeWriter::new(std::io::sink),
        Handle::current(),
    )
    .expect("capture subscriber");
    crate::test_support::ensure_global_subscriber();
    let dispatch = tracing::Dispatch::new(subscriber);
    let value = {
        let _guard = tracing::dispatcher::set_default(&dispatch);
        future.await
    };
    observability.shutdown().await;
    (
        value,
        Captured {
            requests: receiver.trace_requests(),
        },
    )
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::sync::{Arc, Mutex};
    use std::time::Instant;

    use opentelemetry_proto::tonic::common::v1::any_value;
    use opentelemetry_proto::tonic::logs::v1::LogRecord;
    use opentelemetry_proto::tonic::trace::v1::Span as ProtoSpan;
    use opentelemetry_proto::tonic::trace::v1::span::SpanKind;
    use opentelemetry_proto::tonic::trace::v1::status::StatusCode as ProtoStatusCode;
    use reqwest::StatusCode;
    use tracing::Dispatch;

    use super::*;
    use crate::config::load_from;
    use crate::observability::otlp::receiver::{attribute_keys, int_attribute, string_attribute};
    use crate::observability::switchboard::NOTICE_TARGET;
    use crate::test_support;

    /// `io::Write` over a shared buffer, used as the stdout writer of a test subscriber.
    #[derive(Clone, Default)]
    struct Buffer(Arc<Mutex<Vec<u8>>>);

    impl Buffer {
        fn text(&self) -> String {
            String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
        }
    }

    impl std::io::Write for Buffer {
        fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().write(data)
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn stdout_writer(buffer: &Buffer) -> BoxMakeWriter {
        let writer = buffer.clone();
        BoxMakeWriter::new(move || writer.clone())
    }

    /// The defaults, or `toml` over them, built with a stdout buffer, in the pretty format.
    fn build_for(
        toml: Option<&str>,
    ) -> Result<(ComposedSubscriber, Observability), ObservabilityError> {
        let config = load_from(toml, &[]).unwrap();
        build(
            &config,
            LogFormat::Pretty,
            stdout_writer(&Buffer::default()),
            Handle::current(),
        )
    }

    fn error_of(
        result: Result<(ComposedSubscriber, Observability), ObservabilityError>,
    ) -> ObservabilityError {
        match result {
            Ok(_) => panic!("expected a start-up error"),
            Err(error) => error,
        }
    }

    /// The one event every stdout comparison emits. Both subscribers see the same call site.
    fn emit() {
        tracing::info!(project_id = 7, "same line");
    }

    fn body_text(record: &LogRecord) -> Option<String> {
        match record.body.as_ref()?.value.as_ref()? {
            any_value::Value::StringValue(text) => Some(text.clone()),
            _ => None,
        }
    }

    /// Each stdout line as JSON, without its `timestamp` key.
    fn json_lines(text: &str) -> Vec<serde_json::Value> {
        text.lines()
            .map(|line| {
                let mut value: serde_json::Value = serde_json::from_str(line).unwrap();
                value.as_object_mut().unwrap().remove("timestamp");
                value
            })
            .collect()
    }

    /// `text` without its first whitespace-separated token: the timestamp of a pretty event.
    fn without_timestamp(text: &str) -> String {
        match text.trim_start().split_once(char::is_whitespace) {
            Some((_, rest)) => rest.to_owned(),
            None => String::new(),
        }
    }

    #[test]
    fn log_filter_caps_rmcp() {
        for level in ["info", "trace", "info,rmcp=trace"] {
            let filter = log_filter(level).unwrap().to_string();
            assert!(filter.contains("rmcp=warn"), "{level}: {filter}");
            assert!(!filter.contains("rmcp=trace"), "{level}: {filter}");
        }
    }

    /// `log_filter("debug")` admits debug events, but not rmcp's: a subscriber built from it
    /// drops an rmcp event (rmcp logs whole requests and results at debug) and still emits a
    /// debug event for another target.
    #[test]
    fn a_debug_filter_drops_rmcp_debug_events() {
        let buffer = Buffer::default();
        let layer = tracing_subscriber::fmt::layer()
            .with_writer(stdout_writer(&buffer))
            .with_ansi(false)
            .with_target(true)
            .with_filter(log_filter("debug").unwrap());
        let dispatch = Dispatch::new(tracing_subscriber::registry().with(layer));
        let guard = tracing::dispatcher::set_default(&dispatch);
        tracing::debug!(target: "rmcp::service", "dropped-canary");
        tracing::debug!(target: "cetra::serve", "kept-canary");
        drop(guard);
        let logs = buffer.text();
        assert!(!logs.contains("dropped-canary"), "{logs}");
        assert!(logs.contains("kept-canary"), "{logs}");
        assert!(logs.contains("cetra::serve"), "{logs}");
    }

    #[test]
    fn the_stdout_filter_has_the_notice_directive() {
        for level in ["info", "warn", "error"] {
            let filter = log_filter(level).unwrap().to_string();
            assert!(
                filter.contains("cetra_server::observability::notice=info"),
                "{level}: {filter}"
            );
            assert!(filter.contains("rmcp=warn"), "{level}: {filter}");
        }
    }

    #[test]
    fn the_export_filter_has_no_notice_directive() {
        for level in ["warn", "info"] {
            let filter = export_filter(level).unwrap().to_string();
            assert!(
                !filter.contains("cetra_server::observability::notice"),
                "{level}: {filter}"
            );
        }
    }

    /// At `log.level = "warn"` the stdout subscriber writes the notice canary and drops an
    /// ordinary `info` event; the export is off (no endpoint), so only stdout is checked.
    #[tokio::test]
    async fn the_notice_shows_on_stdout_at_warn() {
        let config = load_from(Some("[log]\nlevel = \"warn\"\n"), &[]).unwrap();
        let stdout = Buffer::default();
        let (subscriber, _observability) = build(
            &config,
            LogFormat::Pretty,
            stdout_writer(&stdout),
            Handle::current(),
        )
        .unwrap();

        test_support::ensure_global_subscriber();
        let guard = tracing::dispatcher::set_default(&Dispatch::new(subscriber));
        tracing::info!(target: NOTICE_TARGET, "notice-canary");
        tracing::info!(target: "cetra_server::serve", "dropped-canary");
        drop(guard);

        let logs = stdout.text();
        assert!(logs.contains("notice-canary"), "{logs}");
        assert!(!logs.contains("dropped-canary"), "{logs}");
    }

    /// The notice directive is added last, so it replaces an `off` written for its own target
    /// in `log.level`: the notice still shows.
    #[test]
    fn the_notice_directive_wins_over_the_level() {
        let buffer = Buffer::default();
        let layer = tracing_subscriber::fmt::layer()
            .with_writer(stdout_writer(&buffer))
            .with_ansi(false)
            .with_target(true)
            .with_filter(log_filter("warn,cetra_server::observability::notice=off").unwrap());
        let dispatch = Dispatch::new(tracing_subscriber::registry().with(layer));
        let guard = tracing::dispatcher::set_default(&dispatch);
        tracing::info!(target: NOTICE_TARGET, "notice-canary");
        drop(guard);
        let logs = buffer.text();
        assert!(logs.contains("notice-canary"), "{logs}");
    }

    #[test]
    fn export_filter_caps_rmcp() {
        for level in ["warn", "info,rmcp=trace", "trace"] {
            let filter = export_filter(level).unwrap().to_string();
            assert!(filter.contains("rmcp=warn"), "{level}: {filter}");
            assert!(!filter.contains("rmcp=trace"), "{level}: {filter}");
        }
    }

    #[test]
    fn export_filter_adds_only_the_rmcp_cap() {
        let mut directives: Vec<String> = export_filter("warn")
            .unwrap()
            .to_string()
            .split(',')
            .map(str::to_owned)
            .collect();
        directives.sort();
        assert_eq!(directives, ["rmcp=warn", "warn"]);
    }

    #[test]
    fn export_target_allowed_drops_the_exporter_stack() {
        let dropped = [
            ("opentelemetry_sdk", Level::INFO),
            ("opentelemetry_appender_tracing", Level::INFO),
            ("hyper_util::client::legacy", Level::WARN),
            ("hyper::proto", Level::WARN),
            ("reqwest::connect", Level::WARN),
            ("h2::codec", Level::WARN),
            ("tower::buffer", Level::WARN),
            ("tower_http::trace", Level::WARN),
            ("rustls::conn", Level::WARN),
            ("tokio_rustls::client", Level::WARN),
            ("webpki::verify", Level::WARN),
            ("want", Level::WARN),
            ("http_body_util", Level::WARN),
            (EXPORT_FAILURE_TARGET, Level::WARN),
            ("rmcp::service", Level::DEBUG),
            ("rmcp::service", Level::INFO),
        ];
        for (target, level) in dropped {
            assert!(!export_target_allowed(target, level), "{target} {level}");
        }
        let kept = [
            ("rmcp::service", Level::WARN),
            ("rmcp::service", Level::ERROR),
            ("cetra_server::serve", Level::INFO),
            ("cetra_storage::wal", Level::ERROR),
        ];
        for (target, level) in kept {
            assert!(export_target_allowed(target, level), "{target} {level}");
        }
    }

    #[tokio::test]
    async fn stdout_json_matches_the_former_builder() {
        test_support::ensure_global_subscriber();
        let former = Buffer::default();
        let former_dispatch = Dispatch::new(
            tracing_subscriber::fmt()
                .json()
                .with_writer(stdout_writer(&former))
                .finish(),
        );
        let current = Buffer::default();
        let config = load_from(None, &[]).unwrap();
        let (subscriber, _observability) = build(
            &config,
            LogFormat::Json,
            stdout_writer(&current),
            Handle::current(),
        )
        .unwrap();

        {
            let _guard = tracing::dispatcher::set_default(&former_dispatch);
            emit();
        }
        {
            let _guard = tracing::dispatcher::set_default(&Dispatch::new(subscriber));
            emit();
        }

        let expected = json_lines(&former.text());
        assert_eq!(expected.len(), 1);
        assert_eq!(json_lines(&current.text()), expected);
    }

    #[tokio::test]
    async fn stdout_pretty_matches_the_former_builder() {
        test_support::ensure_global_subscriber();
        let former = Buffer::default();
        let former_dispatch = Dispatch::new(
            tracing_subscriber::fmt()
                .pretty()
                .with_writer(stdout_writer(&former))
                .finish(),
        );
        let current = Buffer::default();
        let config = load_from(None, &[]).unwrap();
        let (subscriber, _observability) = build(
            &config,
            LogFormat::Pretty,
            stdout_writer(&current),
            Handle::current(),
        )
        .unwrap();

        {
            let _guard = tracing::dispatcher::set_default(&former_dispatch);
            emit();
        }
        {
            let _guard = tracing::dispatcher::set_default(&Dispatch::new(subscriber));
            emit();
        }

        let expected = without_timestamp(&former.text());
        assert!(expected.contains("same line"), "{expected}");
        assert_eq!(without_timestamp(&current.text()), expected);
    }

    #[tokio::test]
    async fn no_endpoint_means_no_export_layer() {
        let (_subscriber, observability) = build_for(None).unwrap();
        assert!(!observability.exporting_logs());
    }

    #[tokio::test]
    async fn logs_false_means_no_export_layer() {
        let (_subscriber, observability) = build_for(Some(
            "[self_monitoring]\notlp_endpoint = \"http://127.0.0.1:1/otlp\"\nlogs = false\n",
        ))
        .unwrap();
        assert!(!observability.exporting_logs());
    }

    #[tokio::test]
    async fn an_invalid_export_level_is_an_error() {
        for toml in [
            "[self_monitoring]\notlp_endpoint = \"http://127.0.0.1:1/otlp\"\nlevel = \"cetra_server=nonsense\"\n",
            "[self_monitoring]\nlevel = \"cetra_server=nonsense\"\n",
            "[self_monitoring]\notlp_endpoint = \"http://127.0.0.1:1/otlp\"\nlogs = false\nlevel = \"cetra_server=nonsense\"\n",
        ] {
            let error = error_of(build_for(Some(toml)));
            assert!(
                matches!(error, ObservabilityError::ExportLevel(_)),
                "{toml}"
            );
            assert!(
                error.to_string().starts_with("self_monitoring.level: "),
                "{error}"
            );
        }
    }

    #[tokio::test]
    async fn an_invalid_log_level_is_an_error() {
        let error = error_of(build_for(Some(
            "[log]\nlevel = \"cetra_server=nonsense\"\n",
        )));
        assert!(matches!(error, ObservabilityError::LogLevel(_)), "{error}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_export_layer_applies_its_own_filter() {
        let receiver = Receiver::start(StatusCode::OK, Duration::ZERO).await;
        let toml = format!(
            "[log]\nlevel = \"info\"\n[self_monitoring]\notlp_endpoint = \"{}\"\nlevel = \"warn\"\n",
            receiver.base
        );
        let stdout = Buffer::default();
        let config = load_from(Some(&toml), &[]).unwrap();
        let (subscriber, observability) = build(
            &config,
            LogFormat::Pretty,
            stdout_writer(&stdout),
            Handle::current(),
        )
        .unwrap();
        assert!(observability.exporting_logs());

        test_support::ensure_global_subscriber();
        let guard = tracing::dispatcher::set_default(&Dispatch::new(subscriber));
        tracing::info!("info-canary");
        tracing::warn!("warn-canary");
        tracing::debug!(target: "rmcp::service", "rmcp-canary");
        tracing::warn!(target: "rmcp::service", "rmcp-warn-canary");
        tracing::warn!(target: "hyper_util::client", "hyper-canary");
        tracing::warn!(target: EXPORT_FAILURE_TARGET, "failure-canary");
        drop(guard);
        observability.shutdown().await;

        let mut bodies: Vec<String> = receiver.records().iter().filter_map(body_text).collect();
        bodies.sort();
        assert_eq!(bodies, ["rmcp-warn-canary", "warn-canary"]);

        let logs = stdout.text();
        for canary in [
            "info-canary",
            "warn-canary",
            "hyper-canary",
            "failure-canary",
        ] {
            assert!(logs.contains(canary), "{canary} missing from {logs}");
        }
        assert!(!logs.contains("rmcp-canary"), "{logs}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shutdown_returns_within_the_flush_timeout_when_the_endpoint_hangs() {
        let receiver = Receiver::start(StatusCode::OK, Duration::from_secs(30)).await;
        let toml = format!("[self_monitoring]\notlp_endpoint = \"{}\"\n", receiver.base);
        let config = load_from(Some(&toml), &[]).unwrap();
        let (subscriber, observability) = build(
            &config,
            LogFormat::Pretty,
            stdout_writer(&Buffer::default()),
            Handle::current(),
        )
        .unwrap();

        test_support::ensure_global_subscriber();
        let guard = tracing::dispatcher::set_default(&Dispatch::new(subscriber));
        tracing::warn!("hang-canary");
        drop(guard);
        let started = Instant::now();
        observability.shutdown().await;
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "{:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn shutdown_without_export_returns_at_once() {
        let (_subscriber, observability) = build_for(None).unwrap();
        let started = Instant::now();
        observability.shutdown().await;
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    /// The lines the ring holds, parsed as JSON.
    fn ring_values(observability: &Observability) -> Vec<serde_json::Value> {
        observability
            .log_ring()
            .snapshot()
            .lines
            .iter()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    fn ring_has(observability: &Observability, canary: &str) -> bool {
        observability
            .log_ring()
            .snapshot()
            .lines
            .iter()
            .any(|line| line.contains(canary))
    }

    #[tokio::test]
    async fn the_ring_keeps_stdout_level_lines_as_json() {
        let config = load_from(Some("[log]\nlevel = \"info\"\n"), &[]).unwrap();
        let (subscriber, observability) = build(
            &config,
            LogFormat::Json,
            stdout_writer(&Buffer::default()),
            Handle::current(),
        )
        .unwrap();

        test_support::ensure_global_subscriber();
        let guard = tracing::dispatcher::set_default(&Dispatch::new(subscriber));
        tracing::info!("ring-canary");
        tracing::debug!("ring-debug");
        tracing::warn!(target: "rmcp::service", "rmcp-warn");
        tracing::info!(target: "rmcp::service", "rmcp-info");
        drop(guard);

        let lines = observability.log_ring().snapshot().lines;
        assert!(
            lines.iter().any(|line| line.contains("ring-canary")),
            "{lines:?}"
        );
        assert!(
            lines.iter().any(|line| line.contains("rmcp-warn")),
            "{lines:?}"
        );
        assert!(
            !lines.iter().any(|line| line.contains("ring-debug")),
            "{lines:?}"
        );
        assert!(
            !lines.iter().any(|line| line.contains("rmcp-info")),
            "{lines:?}"
        );
        for value in ring_values(&observability) {
            for key in ["timestamp", "level", "fields", "target"] {
                assert!(value.get(key).is_some(), "{key} missing from {value}");
            }
        }
    }

    #[tokio::test]
    async fn the_ring_is_json_when_stdout_is_pretty() {
        let config = load_from(None, &[]).unwrap();
        let (subscriber, observability) = build(
            &config,
            LogFormat::Pretty,
            stdout_writer(&Buffer::default()),
            Handle::current(),
        )
        .unwrap();

        test_support::ensure_global_subscriber();
        let guard = tracing::dispatcher::set_default(&Dispatch::new(subscriber));
        tracing::info!("pretty-canary");
        drop(guard);

        let lines = observability.log_ring().snapshot().lines;
        let line = lines
            .iter()
            .find(|line| line.contains("pretty-canary"))
            .unwrap();
        let value: serde_json::Value = serde_json::from_str(line).unwrap();
        assert!(value.get("fields").is_some(), "{value}");
    }

    #[test]
    fn the_ring_enables_no_span() {
        let ring = LogRing::new();
        let layer = ring.layer(FilterExt::<Registry>::and(
            log_filter("info").unwrap(),
            filter_fn(|metadata| metadata.is_event() && ring_target_allowed(metadata.target())),
        ));
        let dispatch = Dispatch::new(tracing_subscriber::registry().with(layer));
        let guard = tracing::dispatcher::set_default(&dispatch);
        assert!(tracing::info_span!("ring-span").is_disabled());
        {
            let _entered = tracing::info_span!("ring-span").entered();
            tracing::info!("ring-event");
        }
        drop(guard);

        let lines = ring.snapshot().lines;
        assert_eq!(lines.len(), 1, "{lines:?}");
        let value: serde_json::Value = serde_json::from_str(&lines[0]).unwrap();
        assert!(value.get("span").is_none(), "{value}");
        assert!(value.get("spans").is_none(), "{value}");
    }

    #[test]
    fn ring_target_allowed_drops_library_lines() {
        for target in [
            "hyper_util::client",
            "h2::codec",
            "reqwest::connect",
            "rustls::conn",
            "opentelemetry_sdk",
            "sqlx::query",
        ] {
            assert!(!ring_target_allowed(target), "{target}");
        }
        for target in [
            "cetra_self_monitoring",
            "cetra_server::serve",
            "cetra_storage::wal",
            "rmcp::service",
        ] {
            assert!(ring_target_allowed(target), "{target}");
        }
    }

    #[tokio::test]
    async fn the_ring_drops_library_lines_at_debug() {
        let config = load_from(Some("[log]\nlevel = \"debug\"\n"), &[]).unwrap();
        let stdout = Buffer::default();
        let (subscriber, observability) = build(
            &config,
            LogFormat::Pretty,
            stdout_writer(&stdout),
            Handle::current(),
        )
        .unwrap();

        test_support::ensure_global_subscriber();
        let guard = tracing::dispatcher::set_default(&Dispatch::new(subscriber));
        tracing::debug!(target: "hyper_util::client", "hyper-canary");
        tracing::debug!(target: "sqlx::query", "sqlx-canary");
        tracing::debug!("cetra-canary");
        drop(guard);

        assert!(ring_has(&observability, "cetra-canary"));
        assert!(!ring_has(&observability, "hyper-canary"));
        assert!(!ring_has(&observability, "sqlx-canary"));
        let logs = stdout.text();
        assert!(logs.contains("hyper-canary"), "{logs}");
        assert!(logs.contains("sqlx-canary"), "{logs}");
        assert!(logs.contains("cetra-canary"), "{logs}");
    }

    #[tokio::test]
    async fn the_notice_reaches_the_ring_at_warn() {
        let config = load_from(Some("[log]\nlevel = \"warn\"\n"), &[]).unwrap();
        let (subscriber, observability) = build(
            &config,
            LogFormat::Pretty,
            stdout_writer(&Buffer::default()),
            Handle::current(),
        )
        .unwrap();

        test_support::ensure_global_subscriber();
        let guard = tracing::dispatcher::set_default(&Dispatch::new(subscriber));
        tracing::info!(target: NOTICE_TARGET, "notice-canary");
        drop(guard);

        assert!(ring_has(&observability, "notice-canary"));
    }

    /// The one span named `name` that a test captured.
    fn only(captured: &Captured, name: &str) -> ProtoSpan {
        let mut spans = captured.named(name);
        assert_eq!(spans.len(), 1, "{name}: {spans:?}");
        spans.remove(0)
    }

    /// The status code of `span`; no status is unset.
    fn status_code(span: &ProtoSpan) -> i32 {
        span.status
            .as_ref()
            .map_or(ProtoStatusCode::Unset as i32, |status| status.code)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stdout_json_is_unchanged_inside_a_span() {
        test_support::ensure_global_subscriber();
        let former = Buffer::default();
        let former_dispatch = Dispatch::new(
            tracing_subscriber::fmt()
                .json()
                .with_writer(stdout_writer(&former))
                .finish(),
        );
        let receiver = Receiver::start(StatusCode::OK, Duration::ZERO).await;
        let toml = format!(
            "[log]\nlevel = \"info\"\n[self_monitoring]\notlp_endpoint = \"{}\"\nlogs = false\ntrace_sample_ratio = 1.0\n",
            receiver.base
        );
        let current = Buffer::default();
        let config = load_from(Some(&toml), &[]).unwrap();
        let (subscriber, observability) = build(
            &config,
            LogFormat::Json,
            stdout_writer(&current),
            Handle::current(),
        )
        .unwrap();

        {
            let _guard = tracing::dispatcher::set_default(&former_dispatch);
            emit();
        }
        {
            let _guard = tracing::dispatcher::set_default(&Dispatch::new(subscriber));
            tracing::info_span!("test.outer", a = 1_i64).in_scope(emit);
        }
        observability.shutdown().await;

        let expected = json_lines(&former.text());
        assert_eq!(expected.len(), 1);
        assert_eq!(json_lines(&current.text()), expected);
        let text = current.text();
        assert!(!text.contains("\"span\""), "{text}");
        assert!(!text.contains("\"spans\""), "{text}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stdout_pretty_is_unchanged_inside_a_span() {
        test_support::ensure_global_subscriber();
        let former = Buffer::default();
        let former_dispatch = Dispatch::new(
            tracing_subscriber::fmt()
                .pretty()
                .with_writer(stdout_writer(&former))
                .finish(),
        );
        let receiver = Receiver::start(StatusCode::OK, Duration::ZERO).await;
        let toml = format!(
            "[log]\nlevel = \"info\"\n[self_monitoring]\notlp_endpoint = \"{}\"\nlogs = false\ntrace_sample_ratio = 1.0\n",
            receiver.base
        );
        let current = Buffer::default();
        let config = load_from(Some(&toml), &[]).unwrap();
        let (subscriber, observability) = build(
            &config,
            LogFormat::Pretty,
            stdout_writer(&current),
            Handle::current(),
        )
        .unwrap();

        {
            let _guard = tracing::dispatcher::set_default(&former_dispatch);
            emit();
        }
        {
            let _guard = tracing::dispatcher::set_default(&Dispatch::new(subscriber));
            tracing::info_span!("test.outer", a = 1_i64).in_scope(emit);
        }
        observability.shutdown().await;

        let expected = without_timestamp(&former.text());
        assert!(expected.contains("same line"), "{expected}");
        assert_eq!(without_timestamp(&current.text()), expected);
        assert!(!current.text().contains("test.outer"), "{}", current.text());
    }

    #[test]
    fn span_target_allowed_keeps_only_cetra_info_spans() {
        for (target, level) in [
            ("cetra_server::http", Level::INFO),
            ("cetra_storage::spans", Level::INFO),
            ("cetra_storage::spans", Level::WARN),
        ] {
            assert!(span_target_allowed(target, level), "{target} {level}");
        }
        for (target, level) in [
            ("cetra_server::x", Level::DEBUG),
            ("rmcp::service", Level::INFO),
            ("hyper::proto", Level::INFO),
            ("tower_http::trace", Level::INFO),
        ] {
            assert!(!span_target_allowed(target, level), "{target} {level}");
        }
    }

    #[tokio::test]
    async fn no_span_layer_without_an_endpoint_when_off_or_at_ratio_zero() {
        for (toml, exporting) in [
            (None, false),
            (
                Some(
                    "[self_monitoring]\notlp_endpoint = \"http://127.0.0.1:1/otlp\"\ntraces = false\n",
                ),
                false,
            ),
            (
                Some(
                    "[self_monitoring]\notlp_endpoint = \"http://127.0.0.1:1/otlp\"\ntrace_sample_ratio = 0.0\n",
                ),
                false,
            ),
            (
                Some("[self_monitoring]\notlp_endpoint = \"http://127.0.0.1:1/otlp\"\n"),
                true,
            ),
        ] {
            let (_subscriber, observability) = build_for(toml).unwrap();
            assert_eq!(observability.exporting_traces(), exporting, "{toml:?}");
        }
    }

    #[tokio::test]
    async fn traces_only_builds_no_log_layer() {
        let (_subscriber, observability) = build_for(Some(
            "[self_monitoring]\notlp_endpoint = \"http://127.0.0.1:1/otlp\"\nlogs = false\n",
        ))
        .unwrap();
        assert!(!observability.exporting_logs());
        assert!(observability.exporting_traces());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn spans_carry_their_declared_attributes_only() {
        let ((), captured) = capture_spans(async {
            tracing::info_span!("test.root", a = 1_i64, b = "x").in_scope(|| {});
        })
        .await;
        let span = only(&captured, "test.root");
        assert_eq!(
            attribute_keys(&span.attributes),
            BTreeSet::from(["a".to_owned(), "b".to_owned()])
        );
        assert_eq!(int_attribute(&span.attributes, "a"), Some(1));
        assert_eq!(span.kind, SpanKind::Internal as i32);
        assert!(span.parent_span_id.is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_error_event_marks_its_span_as_error() {
        let ((), captured) = capture_spans(async {
            tracing::info_span!("test.root").in_scope(|| {
                tracing::error!(error = "boom", "it failed");
            });
            tracing::info_span!("test.warned").in_scope(|| {
                tracing::warn!(error = "boom");
            });
        })
        .await;

        let root = only(&captured, "test.root");
        assert_eq!(status_code(&root), ProtoStatusCode::Error as i32);
        assert!(root.status.as_ref().unwrap().message.is_empty());
        assert_eq!(root.events.len(), 1, "{:?}", root.events);
        let event = &root.events[0];
        assert_eq!(event.name, "it failed");
        assert_eq!(
            attribute_keys(&event.attributes),
            BTreeSet::from(["level".to_owned(), "target".to_owned(), "error".to_owned()])
        );
        assert_eq!(
            string_attribute(&event.attributes, "error"),
            Some("boom".to_owned())
        );

        let warned = only(&captured, "test.warned");
        assert_eq!(status_code(&warned), ProtoStatusCode::Unset as i32);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn span_events_follow_the_export_level() {
        let receiver = Receiver::start(StatusCode::OK, Duration::ZERO).await;
        let toml = format!(
            "[self_monitoring]\notlp_endpoint = \"{}\"\nlogs = false\ntrace_sample_ratio = 1.0\nlevel = \"warn\"\n",
            receiver.base
        );
        let config = load_from(Some(&toml), &[]).unwrap();
        let (subscriber, observability) = build(
            &config,
            LogFormat::Json,
            stdout_writer(&Buffer::default()),
            Handle::current(),
        )
        .unwrap();

        test_support::ensure_global_subscriber();
        {
            let _guard = tracing::dispatcher::set_default(&Dispatch::new(subscriber));
            tracing::info_span!("test.root").in_scope(|| {
                tracing::info!("info-event");
                tracing::warn!(n = 2_i64, "warn-event");
                tracing::warn!(target: "hyper_util::client", "hyper-event");
                tracing::warn!(target: EXPORT_FAILURE_TARGET, "failure-event");
            });
        }
        observability.shutdown().await;

        let captured = Captured {
            requests: receiver.trace_requests(),
        };
        let root = only(&captured, "test.root");
        let names: BTreeSet<String> = root.events.iter().map(|event| event.name.clone()).collect();
        assert_eq!(names, BTreeSet::from(["warn-event".to_owned()]));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn children_follow_their_root() {
        let ((), captured) = capture_spans(async {
            let root = tracing::info_span!("test.root");
            root.in_scope(|| tracing::info_span!("test.child").in_scope(|| {}));
        })
        .await;
        let root = only(&captured, "test.root");
        let child = only(&captured, "test.child");
        assert_eq!(child.trace_id, root.trace_id);
        assert_eq!(child.parent_span_id, root.span_id);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_explicit_parent_is_kept() {
        let ((), captured) = capture_spans(async {
            let root = tracing::info_span!("test.root");
            let child = tracing::info_span!(parent: &root, "test.child");
            drop(child);
            drop(root);
        })
        .await;
        let root = only(&captured, "test.root");
        let child = only(&captured, "test.child");
        assert_eq!(child.parent_span_id, root.span_id);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn other_crates_spans_are_not_exported() {
        let ((), captured) = capture_spans(async {
            tracing::info_span!(target: "rmcp::service", "rmcp.span")
                .in_scope(|| tracing::info_span!("test.child").in_scope(|| {}));
        })
        .await;
        assert!(captured.named("rmcp.span").is_empty());
        assert!(only(&captured, "test.child").parent_span_id.is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_ratio_between_zero_and_one_keeps_whole_traces() {
        let receiver = Receiver::start(StatusCode::OK, Duration::ZERO).await;
        let toml = format!(
            "[self_monitoring]\notlp_endpoint = \"{}\"\nlogs = false\ntrace_sample_ratio = 0.5\n",
            receiver.base
        );
        let config = load_from(Some(&toml), &[]).unwrap();
        let (subscriber, observability) = build(
            &config,
            LogFormat::Json,
            stdout_writer(&Buffer::default()),
            Handle::current(),
        )
        .unwrap();

        test_support::ensure_global_subscriber();
        {
            let _guard = tracing::dispatcher::set_default(&Dispatch::new(subscriber));
            for _ in 0..200 {
                let root = tracing::info_span!("test.root");
                root.in_scope(|| tracing::info_span!("test.child").in_scope(|| {}));
            }
        }
        observability.shutdown().await;

        let captured = Captured {
            requests: receiver.trace_requests(),
        };
        let roots = captured.named("test.root");
        let children = captured.named("test.child");
        assert!(!roots.is_empty() && roots.len() < 200, "{}", roots.len());
        let root_traces: BTreeSet<Vec<u8>> =
            roots.iter().map(|span| span.trace_id.clone()).collect();
        let child_traces: BTreeSet<Vec<u8>> =
            children.iter().map(|span| span.trace_id.clone()).collect();
        assert_eq!(child_traces, root_traces);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shutdown_flushes_both_exports_within_the_flush_timeout() {
        let receiver = Receiver::start(StatusCode::OK, Duration::from_secs(30)).await;
        let toml = format!(
            "[self_monitoring]\notlp_endpoint = \"{}\"\ntrace_sample_ratio = 1.0\n",
            receiver.base
        );
        let config = load_from(Some(&toml), &[]).unwrap();
        let (subscriber, observability) = build(
            &config,
            LogFormat::Pretty,
            stdout_writer(&Buffer::default()),
            Handle::current(),
        )
        .unwrap();
        assert!(observability.exporting_logs());
        assert!(observability.exporting_traces());

        test_support::ensure_global_subscriber();
        {
            let _guard = tracing::dispatcher::set_default(&Dispatch::new(subscriber));
            tracing::info_span!("test.root").in_scope(|| tracing::warn!("hang-canary"));
        }
        let started = Instant::now();
        observability.shutdown().await;
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "{:?}",
            started.elapsed()
        );
    }

    /// The text of every report file in `dir`'s crash spool (none when the spool is absent).
    fn spooled(dir: &std::path::Path) -> Vec<String> {
        let Ok(entries) = std::fs::read_dir(dir.join("telemetry").join("spool")) else {
            return Vec::new();
        };
        entries
            .flatten()
            .map(|entry| std::fs::read_to_string(entry.path()).unwrap())
            .collect()
    }

    #[tokio::test]
    async fn crash_reports_off_means_no_crash_layer() {
        let (_subscriber, observability) = build_for(None).unwrap();
        assert!(!observability.reporting_crashes());
    }

    #[tokio::test]
    async fn crash_reports_on_adds_the_crash_layer() {
        let dir = tempfile::tempdir().unwrap();
        let toml = format!(
            "data_dir = '{}'\n[telemetry]\nendpoint = \"http://127.0.0.1:9\"\ncrash_reports = true\n",
            dir.path().display()
        );
        let config = load_from(Some(&toml), &[]).unwrap();
        let (subscriber, observability) = build(
            &config,
            LogFormat::Pretty,
            stdout_writer(&Buffer::default()),
            Handle::current(),
        )
        .unwrap();
        assert!(observability.reporting_crashes());

        {
            let _guard = tracing::dispatcher::set_default(&Dispatch::new(subscriber));
            tracing::error!("crash-canary");
        }
        let reports = spooled(dir.path());
        assert_eq!(reports.len(), 1, "{reports:?}");
        assert!(!reports[0].contains("crash-canary"), "{}", reports[0]);
    }

    #[tokio::test]
    async fn cetra_telemetry_off_means_no_crash_layer() {
        let dir = tempfile::tempdir().unwrap();
        let toml = format!(
            "data_dir = '{}'\n[telemetry]\ncrash_reports = true\n",
            dir.path().display()
        );
        let config = load_from(Some(&toml), &[("CETRA_TELEMETRY", "off")]).unwrap();
        let (subscriber, observability) = build(
            &config,
            LogFormat::Pretty,
            stdout_writer(&Buffer::default()),
            Handle::current(),
        )
        .unwrap();
        assert!(!observability.reporting_crashes());

        {
            let _guard = tracing::dispatcher::set_default(&Dispatch::new(subscriber));
            tracing::error!("crash-canary");
        }
        assert!(!dir.path().join("telemetry").exists());
    }

    #[tokio::test]
    async fn restore_mode_means_no_crash_layer() {
        let dir = tempfile::tempdir().unwrap();
        let toml = format!(
            "data_dir = '{}'\nrestore_mode = true\n[telemetry]\ncrash_reports = true\n",
            dir.path().display()
        );
        let config = load_from(Some(&toml), &[]).unwrap();
        let (_subscriber, observability) = build(
            &config,
            LogFormat::Pretty,
            stdout_writer(&Buffer::default()),
            Handle::current(),
        )
        .unwrap();
        assert!(!observability.reporting_crashes());
    }

    #[tokio::test]
    async fn the_crash_layer_ignores_the_log_level() {
        let dir = tempfile::tempdir().unwrap();
        let toml = format!(
            "data_dir = '{}'\n[log]\nlevel = \"off\"\n[telemetry]\ncrash_reports = true\n",
            dir.path().display()
        );
        let config = load_from(Some(&toml), &[]).unwrap();
        let stdout = Buffer::default();
        let (subscriber, observability) = build(
            &config,
            LogFormat::Pretty,
            stdout_writer(&stdout),
            Handle::current(),
        )
        .unwrap();
        assert!(observability.reporting_crashes());

        {
            let _guard = tracing::dispatcher::set_default(&Dispatch::new(subscriber));
            tracing::error!("crash-canary");
        }
        assert_eq!(spooled(dir.path()).len(), 1);
        assert!(stdout.text().is_empty(), "{}", stdout.text());
    }
}
