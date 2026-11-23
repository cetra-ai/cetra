//! The `tracing` subscriber of `cetra serve` (track TL § Architecture): one registry, one layer per sink, each with its own filter. The stdout layer is `log.level` with `rmcp` capped (unchanged output). The OTLP log layer is `self_monitoring.level` with the same cap, minus the exporter's own stack. Later milestones add their layer to [`build`]'s list and their flush to [`Observability::shutdown`]. The flush runs after `serve::run` returns, outside spec 12's 60 s shutdown bound.

use std::time::Duration;

use tokio::runtime::Handle;
use tracing::Level;
use tracing_subscriber::filter::{EnvFilter, FilterExt, ParseError, filter_fn};
use tracing_subscriber::fmt::writer::BoxMakeWriter;
use tracing_subscriber::layer::{Layered, SubscriberExt};
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{Layer, Registry};

use crate::config::{LogFormat, ServerConfig};
use crate::observability::switchboard::NOTICE_DIRECTIVE;

use super::ObservabilityError;
use super::otlp::{EXPORT_FAILURE_TARGET, LogPipeline};

/// How long [`Observability::shutdown`] waits for the export to flush.
pub(crate) const FLUSH_TIMEOUT: Duration = Duration::from_secs(2);

/// The `rmcp` crate logs whole MCP requests and results below `warn`; they can hold prompts
/// and SQL text, so its level is capped whatever `log.level` says (README § Privacy).
const RMCP_DIRECTIVE: &str = "rmcp=warn";

/// Target prefixes the export never carries: the crates of the exporter's own stack that log
/// per request (an export they log would be exported, and so on) and the export's failure
/// lines. A prefix covers longer names: `hyper` covers `hyper_util`, `tower` covers
/// `tower_http`, `http_body` covers `http_body_util`.
const EXPORT_DROPPED_TARGETS: &[&str] = &[
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
}

impl Observability {
    /// Whether log records are exported.
    pub fn exporting_logs(&self) -> bool {
        self.logs.is_some()
    }

    /// Flushes and stops the export on a blocking thread, waiting at most [`FLUSH_TIMEOUT`]
    /// (plus 500 ms for the thread hand-off). Never fails: a flush that errs or times out
    /// writes one `warn` line under [`EXPORT_FAILURE_TARGET`] (stdout only).
    pub async fn shutdown(self) {
        let Some(pipeline) = self.logs else {
            return;
        };
        let flush = tokio::task::spawn_blocking(move || pipeline.shutdown(FLUSH_TIMEOUT));
        match tokio::time::timeout(FLUSH_TIMEOUT + Duration::from_millis(500), flush).await {
            Ok(Ok(Ok(()))) => {}
            _ => tracing::warn!(
                target: EXPORT_FAILURE_TARGET,
                "self-monitoring: the log export did not flush within 2 s"
            ),
        }
    }
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
    if let Some(pipeline) = &observability.logs {
        tracing::info!(
            host = pipeline.host(),
            level = %config.self_monitoring.level,
            "self-monitoring: exporting logs over OTLP"
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
    let stdout: BoxedLayer = match format {
        LogFormat::Pretty => tracing_subscriber::fmt::layer()
            .pretty()
            .with_writer(writer)
            .with_filter(stdout_filter)
            .boxed(),
        LogFormat::Json => tracing_subscriber::fmt::layer()
            .json()
            .with_writer(writer)
            .with_filter(stdout_filter)
            .boxed(),
    };
    let export_level_filter =
        export_filter(&config.self_monitoring.level).map_err(ObservabilityError::ExportLevel)?;
    let mut layers: Vec<BoxedLayer> = vec![stdout];
    let logs = match config.self_monitoring.logs_endpoint() {
        Some(endpoint) => {
            let pipeline = LogPipeline::new(&config.self_monitoring, endpoint, runtime)?;
            let targets =
                filter_fn(|metadata| export_target_allowed(metadata.target(), *metadata.level()));
            let export: BoxedLayer = pipeline
                .bridge()
                .with_filter(FilterExt::<Registry>::and(export_level_filter, targets))
                .boxed();
            layers.push(export);
            Some(pipeline)
        }
        None => None,
    };
    Ok((
        tracing_subscriber::registry().with(layers),
        Observability { logs },
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

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::Instant;

    use opentelemetry_proto::tonic::common::v1::any_value;
    use opentelemetry_proto::tonic::logs::v1::LogRecord;
    use reqwest::StatusCode;
    use tracing::Dispatch;

    use super::*;
    use crate::config::load_from;
    use crate::observability::otlp::receiver::Receiver;
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
}
