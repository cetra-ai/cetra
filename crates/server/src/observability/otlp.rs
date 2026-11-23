//! cetra's OTLP/HTTP log exporter for self-monitoring (track TL, D-TL-2): opentelemetry-proto's transform, prost, gzip and reqwest 0.12. Not opentelemetry-otlp, which merges the host's OTEL_EXPORTER_OTLP_* variables into the request. The SDK's batch processor runs on its own thread and calls [`OtlpLogExporter::export`] through futures_executor::block_on, so the request itself runs on the tokio runtime.

use std::fmt;
use std::future::Future;
use std::io::Write as _;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use flate2::{Compression, write::GzEncoder};
use opentelemetry::KeyValue;
use opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge;
use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
use opentelemetry_proto::transform::common::tonic::ResourceAttributesWithSchema;
use opentelemetry_proto::transform::logs::tonic::group_logs_by_resource_and_scope;
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::error::{OTelSdkError, OTelSdkResult};
use opentelemetry_sdk::logs::{
    BatchConfig, BatchConfigBuilder, BatchLogProcessor, LogBatch, LogExporter, SdkLogger,
    SdkLoggerProvider,
};
use prost::Message;
use reqwest::header::{
    CONTENT_ENCODING, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue, USER_AGENT,
};
use tokio::runtime::Handle;

use crate::config::{
    OtlpEndpoint, Secret, SelfMonitoringConfig, parse_otlp_headers, parse_resource,
};

use super::ObservabilityError;

/// The target of the export's own failure lines. The export filter drops it, so a failing
/// export is never exported (design § Feedback loops).
pub(crate) const EXPORT_FAILURE_TARGET: &str = "cetra_self_monitoring";

/// Timeout of one export request, connection included.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// At most one failure line per interval.
const FAILURE_LOG_INTERVAL: Duration = Duration::from_secs(60);

/// Records buffered before new ones are dropped.
const MAX_QUEUE_SIZE: usize = 2_048;

/// Delay between two exports.
const SCHEDULED_DELAY: Duration = Duration::from_secs(1);

/// Records per request.
const MAX_EXPORT_BATCH_SIZE: usize = 512;

/// Every setting given: `BatchConfigBuilder::default()` reads `OTEL_BLRP_*`, and a value set
/// here replaces the one it read. (`OTEL_BLRP_EXPORT_TIMEOUT` is read only behind an SDK
/// feature cetra does not enable; [`REQUEST_TIMEOUT`] bounds an export.)
fn batch_config() -> BatchConfig {
    BatchConfigBuilder::default()
        .with_max_queue_size(MAX_QUEUE_SIZE)
        .with_scheduled_delay(SCHEDULED_DELAY)
        .with_max_export_batch_size(MAX_EXPORT_BATCH_SIZE)
        .build()
}

/// cetra's resource attributes, then the operator's (which win on the same key). Built from
/// `Resource::builder_empty()`: `Resource::builder()` reads `OTEL_RESOURCE_ATTRIBUTES` and
/// `OTEL_SERVICE_NAME`.
fn resource(extra: Vec<(String, String)>) -> Resource {
    Resource::builder_empty()
        .with_attributes([
            KeyValue::new("service.name", "cetra"),
            KeyValue::new("service.version", env!("CARGO_PKG_VERSION")),
            KeyValue::new("service.instance.id", uuid::Uuid::now_v7().to_string()),
            KeyValue::new("host.arch", host_arch()),
            KeyValue::new("os.type", os_type()),
        ])
        .with_attributes(
            extra
                .into_iter()
                .map(|(key, value)| KeyValue::new(key, value)),
        )
        .build()
}

/// `host.arch` in the OTel semantic-convention spelling.
fn host_arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        "arm" => "arm32",
        "powerpc" => "ppc32",
        "powerpc64" => "ppc64",
        other => other,
    }
}

/// `os.type` in the OTel semantic-convention spelling.
fn os_type() -> &'static str {
    match std::env::consts::OS {
        "macos" => "darwin",
        other => other,
    }
}

/// The operator's headers (marked sensitive), then cetra's own. `config::parse_otlp_headers`
/// already checked the names and values at load.
fn request_headers(raw: Option<&Secret<String>>) -> Result<HeaderMap, ObservabilityError> {
    let mut headers = HeaderMap::new();
    if let Some(raw) = raw {
        let pairs = parse_otlp_headers(raw.expose()).map_err(|_| ObservabilityError::Header)?;
        for (name, value) in pairs {
            let name =
                HeaderName::from_bytes(name.as_bytes()).map_err(|_| ObservabilityError::Header)?;
            let mut value =
                HeaderValue::from_str(&value).map_err(|_| ObservabilityError::Header)?;
            value.set_sensitive(true);
            headers.append(name, value);
        }
    }
    // `from_static` panics only on bytes outside visible ASCII; these literals are ASCII.
    headers.insert(
        CONTENT_TYPE,
        HeaderValue::from_static("application/x-protobuf"),
    );
    headers.insert(CONTENT_ENCODING, HeaderValue::from_static("gzip"));
    headers.insert(
        USER_AGENT,
        HeaderValue::from_static(concat!("cetra/", env!("CARGO_PKG_VERSION"))),
    );
    Ok(headers)
}

/// Lets one failure line through per [`FAILURE_LOG_INTERVAL`].
#[derive(Debug, Default)]
struct FailureLog {
    last: Mutex<Option<Instant>>,
}

impl FailureLog {
    /// Whether a failure seen at `now` is logged. A poisoned lock is recovered: the value is
    /// only a time.
    fn should_log(&self, now: Instant) -> bool {
        let mut last = self.last.lock().unwrap_or_else(PoisonError::into_inner);
        match *last {
            Some(at) if now.saturating_duration_since(at) < FAILURE_LOG_INTERVAL => false,
            _ => {
                *last = Some(now);
                true
            }
        }
    }
}

/// Why an export failed. `reason` is one of `encode`, `status`, `timeout`, `connect`,
/// `request`, `runtime`; never error text, which can quote the URL.
#[derive(Debug, PartialEq, Eq)]
struct Failure {
    reason: &'static str,
    status: Option<u16>,
}

impl Failure {
    fn new(reason: &'static str, status: Option<u16>) -> Self {
        Self { reason, status }
    }
}

/// The outcome of one request, as the runtime returned it.
fn outcome(
    result: Result<Result<reqwest::StatusCode, reqwest::Error>, tokio::task::JoinError>,
) -> Result<(), Failure> {
    match result {
        Ok(Ok(status)) if status.is_success() => Ok(()),
        Ok(Ok(status)) => Err(Failure::new("status", Some(status.as_u16()))),
        Ok(Err(error)) if error.is_timeout() => Err(Failure::new("timeout", None)),
        Ok(Err(error)) if error.is_connect() => Err(Failure::new("connect", None)),
        Ok(Err(_)) => Err(Failure::new("request", None)),
        Err(_) => Err(Failure::new("runtime", None)),
    }
}

fn gzip(bytes: &[u8]) -> std::io::Result<Vec<u8>> {
    let buffer = Vec::with_capacity(bytes.len() / 4);
    let mut encoder = GzEncoder::new(buffer, Compression::default());
    encoder.write_all(bytes)?;
    encoder.finish()
}

/// No redirects, [`REQUEST_TIMEOUT`]. `HTTP(S)_PROXY` is honoured, like `identity_client`,
/// except in this crate's tests, whose receivers listen on 127.0.0.1.
fn http_client() -> Result<reqwest::Client, ObservabilityError> {
    let builder = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(REQUEST_TIMEOUT);
    #[cfg(test)]
    let builder = builder.no_proxy();
    builder.build().map_err(|_| ObservabilityError::Client)
}

/// Posts each batch to `<otlp_endpoint>/v1/logs` (D-TL-2).
pub(crate) struct OtlpLogExporter {
    client: reqwest::Client,
    url: String,
    headers: HeaderMap,
    resource: ResourceAttributesWithSchema,
    runtime: Handle,
    failures: Arc<FailureLog>,
}

impl fmt::Debug for OtlpLogExporter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let header_names: Vec<&str> = self.headers.keys().map(HeaderName::as_str).collect();
        f.debug_struct("OtlpLogExporter")
            .field("url", &self.url)
            .field("header_names", &header_names)
            .finish_non_exhaustive()
    }
}

impl LogExporter for OtlpLogExporter {
    // `export` is a plain `fn`, not an `async fn`, so the `LogBatch` borrow ends before the
    // returned future.
    fn export(&self, batch: LogBatch<'_>) -> impl Future<Output = OTelSdkResult> + Send {
        let records = batch.iter().count();
        let request = ExportLogsServiceRequest {
            resource_logs: group_logs_by_resource_and_scope(&batch, &self.resource),
        };
        // `send()` runs inside the spawned task: reqwest creates its timeout timer there, which
        // needs the runtime's context, and this (the SDK's batch) thread has none.
        let sent = gzip(&request.encode_to_vec()).map(|body| {
            let builder = self
                .client
                .post(&self.url)
                .headers(self.headers.clone())
                .body(body);
            self.runtime
                .spawn(async move { builder.send().await.map(|response| response.status()) })
        });
        let failures = Arc::clone(&self.failures);
        async move {
            let result = match sent {
                Ok(handle) => outcome(handle.await),
                Err(_) => Err(Failure::new("encode", None)),
            };
            let Err(failure) = result else {
                return Ok(());
            };
            if failures.should_log(Instant::now()) {
                tracing::warn!(
                    target: EXPORT_FAILURE_TARGET,
                    reason = failure.reason,
                    status = failure.status,
                    records,
                    "self-monitoring log export failed"
                );
            }
            Err(OTelSdkError::InternalFailure(failure.reason.to_owned()))
        }
    }

    fn set_resource(&mut self, resource: &Resource) {
        self.resource = resource.into();
    }
}

/// The provider of the self-monitoring log export. The bridge layer emits into it; the
/// subscriber's guard shuts it down at exit.
pub(crate) struct LogPipeline {
    provider: SdkLoggerProvider,
    host: String,
}

impl LogPipeline {
    /// Builds the exporter, its batch processor and the provider. No `OTEL_*` variable is read.
    pub(crate) fn new(
        config: &SelfMonitoringConfig,
        endpoint: &OtlpEndpoint,
        runtime: Handle,
    ) -> Result<Self, ObservabilityError> {
        let headers = request_headers(config.otlp_headers.as_ref())?;
        let extra = parse_resource(&config.resource).map_err(|_| ObservabilityError::Resource)?;
        let resource = resource(extra);
        let client = http_client()?;
        let exporter = OtlpLogExporter {
            client,
            url: endpoint.logs_url(),
            headers,
            resource: (&resource).into(),
            runtime,
            failures: Arc::new(FailureLog::default()),
        };
        // Never `LoggerProviderBuilder::with_batch_exporter`, which uses the env-reading config.
        let processor = BatchLogProcessor::builder(exporter)
            .with_batch_config(batch_config())
            .build();
        // `build()` evaluates the env-reading `Resource::builder()` as its fallback even when a
        // resource is given, and drops that result. Only `with_resource` decides the attributes.
        let provider = SdkLoggerProvider::builder()
            .with_resource(resource)
            .with_log_processor(processor)
            .build();
        Ok(Self {
            provider,
            host: endpoint.host().to_owned(),
        })
    }

    /// The `tracing` layer that turns events into log records of this pipeline.
    pub(crate) fn bridge(&self) -> OpenTelemetryTracingBridge<SdkLoggerProvider, SdkLogger> {
        OpenTelemetryTracingBridge::new(&self.provider)
    }

    /// The endpoint's host, for the start-up line.
    pub(crate) fn host(&self) -> &str {
        &self.host
    }

    /// Exports what is queued and stops, waiting at most `timeout`. Blocks the calling
    /// thread: call it from a blocking thread, never from an async task.
    pub(crate) fn shutdown(&self, timeout: Duration) -> OTelSdkResult {
        self.provider.shutdown_with_timeout(timeout)
    }
}

impl fmt::Debug for LogPipeline {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LogPipeline")
            .field("host", &self.host)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
pub(crate) mod receiver {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use axum::Router;
    use axum::extract::State;
    use axum::http::{HeaderMap, StatusCode};
    use axum::routing::post;
    use bytes::Bytes;
    use flate2::read::GzDecoder;
    use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
    use opentelemetry_proto::tonic::logs::v1::LogRecord;
    use prost::Message;
    use std::io::Read as _;

    /// One request the receiver accepted.
    #[derive(Debug, Clone)]
    pub(crate) struct Received {
        pub(crate) headers: HeaderMap,
        pub(crate) request: ExportLogsServiceRequest,
    }

    /// An OTLP/HTTP logs receiver on 127.0.0.1:0 that answers `status` after `delay`.
    pub(crate) struct Receiver {
        /// The base URL to configure, `http://127.0.0.1:<port>/otlp`.
        pub(crate) base: String,
        received: Arc<Mutex<Vec<Received>>>,
        task: tokio::task::JoinHandle<()>,
    }

    struct ReceiverState {
        status: StatusCode,
        delay: Duration,
        received: Arc<Mutex<Vec<Received>>>,
    }

    impl Receiver {
        pub(crate) async fn start(status: StatusCode, delay: Duration) -> Receiver {
            let received = Arc::new(Mutex::new(Vec::new()));
            let state = Arc::new(ReceiverState {
                status,
                delay,
                received: Arc::clone(&received),
            });
            let router = Router::new()
                .route("/otlp/v1/logs", post(accept))
                .with_state(state);
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base = format!("http://{}/otlp", listener.local_addr().unwrap());
            let task = tokio::spawn(async move {
                let _ = axum::serve(listener, router).await;
            });
            Receiver {
                base,
                received,
                task,
            }
        }

        /// The requests so far, in arrival order.
        pub(crate) fn received(&self) -> Vec<Received> {
            self.received.lock().unwrap().clone()
        }

        /// Every log record of every request, in order.
        pub(crate) fn records(&self) -> Vec<LogRecord> {
            self.received()
                .into_iter()
                .flat_map(|received| received.request.resource_logs)
                .flat_map(|resource_logs| resource_logs.scope_logs)
                .flat_map(|scope_logs| scope_logs.log_records)
                .collect()
        }
    }

    impl Drop for Receiver {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    async fn accept(
        State(state): State<Arc<ReceiverState>>,
        headers: HeaderMap,
        body: Bytes,
    ) -> StatusCode {
        tokio::time::sleep(state.delay).await;
        let gzipped = headers
            .get(axum::http::header::CONTENT_ENCODING)
            .is_some_and(|value| value.as_bytes() == b"gzip");
        let bytes = if gzipped {
            let mut out = Vec::new();
            if GzDecoder::new(&body[..]).read_to_end(&mut out).is_err() {
                return StatusCode::BAD_REQUEST;
            }
            out
        } else {
            body.to_vec()
        };
        match ExportLogsServiceRequest::decode(bytes.as_slice()) {
            Ok(request) => {
                state
                    .received
                    .lock()
                    .unwrap()
                    .push(Received { headers, request });
                state.status
            }
            Err(_) => StatusCode::BAD_REQUEST,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime};

    use opentelemetry::logs::{LogRecord as _, Logger as _, LoggerProvider as _};
    use opentelemetry::{InstrumentationScope, Key};
    use opentelemetry_proto::tonic::common::v1::{AnyValue, KeyValue as ProtoKeyValue, any_value};
    use opentelemetry_proto::tonic::logs::v1::LogRecord as ProtoLogRecord;
    use opentelemetry_sdk::logs::SdkLogRecord;
    use reqwest::StatusCode;
    use tracing::Dispatch;
    use tracing_subscriber::layer::SubscriberExt as _;

    use super::receiver::Receiver;
    use super::*;
    use crate::config::parse_otlp_endpoint;

    /// The exporter as `LogPipeline::new` builds it, pointed at `receiver_base`.
    fn exporter_for(receiver_base: &str) -> OtlpLogExporter {
        OtlpLogExporter {
            client: http_client().unwrap(),
            url: parse_otlp_endpoint(receiver_base).unwrap().logs_url(),
            headers: request_headers(None).unwrap(),
            resource: (&resource(vec![])).into(),
            runtime: Handle::current(),
            failures: Arc::new(FailureLog::default()),
        }
    }

    /// A record built the way the SDK's logger builds one, with the observed timestamp the
    /// transform needs.
    fn test_record(body: &'static str) -> SdkLogRecord {
        let provider = SdkLoggerProvider::builder()
            .with_resource(Resource::builder_empty().build())
            .build();
        let mut record = provider.logger("test").create_log_record();
        record.set_body(body.into());
        record.set_observed_timestamp(SystemTime::now());
        record
    }

    fn string_value(value: Option<&AnyValue>) -> Option<&str> {
        match value?.value.as_ref()? {
            any_value::Value::StringValue(text) => Some(text.as_str()),
            _ => None,
        }
    }

    fn string_attr<'a>(attributes: &'a [ProtoKeyValue], key: &str) -> Option<&'a str> {
        let attribute = attributes.iter().find(|kv| kv.key == key)?;
        string_value(attribute.value.as_ref())
    }

    fn int_attr(attributes: &[ProtoKeyValue], key: &str) -> Option<i64> {
        let attribute = attributes.iter().find(|kv| kv.key == key)?;
        match attribute.value.as_ref()?.value.as_ref()? {
            any_value::Value::IntValue(number) => Some(*number),
            _ => None,
        }
    }

    fn record_with_body<'a>(
        records: &'a [ProtoLogRecord],
        body: &str,
    ) -> Option<&'a ProtoLogRecord> {
        records
            .iter()
            .find(|record| string_value(record.body.as_ref()) == Some(body))
    }

    /// A self-monitoring config that exports to `base`, with the given resource and headers.
    fn config_for(base: &str, resource: &str, headers: Option<&str>) -> SelfMonitoringConfig {
        SelfMonitoringConfig {
            otlp_endpoint: Some(parse_otlp_endpoint(base).unwrap()),
            otlp_headers: headers.map(|raw| Secret::new(raw.to_owned())),
            resource: resource.to_owned(),
            ..SelfMonitoringConfig::default()
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn records_arrive_with_fields_severity_resource_and_headers() {
        let receiver = Receiver::start(StatusCode::OK, Duration::ZERO).await;
        let config = config_for(
            &receiver.base,
            "deployment.environment=test",
            Some("authorization=Bearer t0k3n,x-scope-orgid=7"),
        );
        let pipeline =
            LogPipeline::new(&config, config.logs_endpoint().unwrap(), Handle::current()).unwrap();
        crate::test_support::ensure_global_subscriber();
        let guard = tracing::dispatcher::set_default(&Dispatch::new(
            tracing_subscriber::registry().with(pipeline.bridge()),
        ));
        tracing::warn!(project_id = 42, outcome = "failed", "flush failed");
        tracing::error!(error = "boom", "write failed");
        drop(guard);
        let result = tokio::task::spawn_blocking(move || pipeline.shutdown(Duration::from_secs(2)))
            .await
            .unwrap();
        assert!(result.is_ok());

        let received = receiver.received();
        assert!(!received.is_empty());
        let headers = &received[0].headers;
        assert_eq!(headers["authorization"], "Bearer t0k3n");
        assert_eq!(headers["x-scope-orgid"], "7");
        assert_eq!(headers["content-type"], "application/x-protobuf");
        assert_eq!(headers["content-encoding"], "gzip");
        assert_eq!(
            headers["user-agent"],
            concat!("cetra/", env!("CARGO_PKG_VERSION"))
        );

        let resource = received[0].request.resource_logs[0]
            .resource
            .as_ref()
            .unwrap();
        let attributes = &resource.attributes;
        assert_eq!(string_attr(attributes, "service.name"), Some("cetra"));
        assert_eq!(
            string_attr(attributes, "service.version"),
            Some(env!("CARGO_PKG_VERSION"))
        );
        assert!(string_attr(attributes, "service.instance.id").is_some_and(|id| !id.is_empty()));
        assert!(string_attr(attributes, "host.arch").is_some());
        assert!(string_attr(attributes, "os.type").is_some());
        assert_eq!(
            string_attr(attributes, "deployment.environment"),
            Some("test")
        );

        let records = receiver.records();
        let flush = record_with_body(&records, "flush failed").unwrap();
        assert_eq!(flush.severity_number, 13);
        assert_eq!(flush.severity_text, "WARN");
        assert_eq!(int_attr(&flush.attributes, "project_id"), Some(42));
        assert_eq!(string_attr(&flush.attributes, "outcome"), Some("failed"));
        let write = record_with_body(&records, "write failed").unwrap();
        assert_eq!(write.severity_number, 17);
        assert_eq!(string_attr(&write.attributes, "error"), Some("boom"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn records_carry_only_the_event_fields() {
        let receiver = Receiver::start(StatusCode::OK, Duration::ZERO).await;
        let config = config_for(&receiver.base, "", None);
        let pipeline =
            LogPipeline::new(&config, config.logs_endpoint().unwrap(), Handle::current()).unwrap();
        crate::test_support::ensure_global_subscriber();
        let guard = tracing::dispatcher::set_default(&Dispatch::new(
            tracing_subscriber::registry().with(pipeline.bridge()),
        ));
        tracing::warn!(a = 1, b = "x", "fields");
        drop(guard);
        tokio::task::spawn_blocking(move || pipeline.shutdown(Duration::from_secs(2)))
            .await
            .unwrap()
            .unwrap();

        let records = receiver.records();
        let record = record_with_body(&records, "fields").unwrap();
        let mut keys: Vec<&str> = record.attributes.iter().map(|kv| kv.key.as_str()).collect();
        keys.sort_unstable();
        assert_eq!(keys, ["a", "b"]);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "run by otel_environment_variables_change_nothing in a child process"]
    async fn env_child_exports_three_records() {
        let Ok(base) = std::env::var("CETRA_TEST_OTLP_CHILD_ENDPOINT") else {
            return;
        };
        let config = config_for(&base, "", Some("x-cetra-test=1"));
        let pipeline =
            LogPipeline::new(&config, config.logs_endpoint().unwrap(), Handle::current()).unwrap();
        let dispatch = Dispatch::new(tracing_subscriber::registry().with(pipeline.bridge()));
        let guard = tracing::dispatcher::set_default(&dispatch);
        tracing::warn!("child-1");
        tracing::warn!("child-2");
        tracing::warn!("child-3");
        drop(guard);
        tokio::task::spawn_blocking(move || pipeline.shutdown(Duration::from_secs(2)))
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn otel_environment_variables_change_nothing() {
        let receiver = Receiver::start(StatusCode::OK, Duration::ZERO).await;
        let base = receiver.base.clone();
        let output =
            tokio::task::spawn_blocking(move || -> std::io::Result<std::process::Output> {
                std::process::Command::new(std::env::current_exe()?)
                    .args([
                        "observability::otlp::tests::env_child_exports_three_records",
                        "--exact",
                        "--ignored",
                        "--nocapture",
                        "--test-threads=1",
                    ])
                    .env("CETRA_TEST_OTLP_CHILD_ENDPOINT", base)
                    .env("OTEL_EXPORTER_OTLP_HEADERS", "x-leaked=env")
                    .env("OTEL_EXPORTER_OTLP_LOGS_HEADERS", "x-leaked-logs=env")
                    .env("OTEL_RESOURCE_ATTRIBUTES", "leaked.attr=1")
                    .env("OTEL_SERVICE_NAME", "leaked-service")
                    .env("OTEL_EXPORTER_OTLP_ENDPOINT", "http://127.0.0.1:9")
                    .env("OTEL_EXPORTER_OTLP_COMPRESSION", "none")
                    .env("OTEL_BLRP_MAX_QUEUE_SIZE", "1")
                    .env("OTEL_BLRP_MAX_EXPORT_BATCH_SIZE", "1")
                    .env("OTEL_BLRP_SCHEDULE_DELAY", "600000")
                    .output()
            })
            .await
            .unwrap()
            .unwrap();
        assert!(output.status.success());
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("1 passed"), "child did not run: {stdout}");

        let bodies: Vec<String> = receiver
            .records()
            .iter()
            .filter_map(|record| string_value(record.body.as_ref()).map(str::to_owned))
            .collect();
        for expected in ["child-1", "child-2", "child-3"] {
            assert!(
                bodies.iter().any(|body| body == expected),
                "missing {expected}"
            );
        }

        let received = receiver.received();
        assert!(!received.is_empty());
        for request in &received {
            assert_eq!(request.headers["x-cetra-test"], "1");
            assert_eq!(request.headers["content-encoding"], "gzip");
            assert!(request.headers.get("x-leaked").is_none());
            assert!(request.headers.get("x-leaked-logs").is_none());
        }
        let attributes = &received[0].request.resource_logs[0]
            .resource
            .as_ref()
            .unwrap()
            .attributes;
        assert_eq!(string_attr(attributes, "service.name"), Some("cetra"));
        assert!(string_attr(attributes, "leaked.attr").is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn export_reports_an_error_status_as_a_failure() {
        let receiver = Receiver::start(StatusCode::INTERNAL_SERVER_ERROR, Duration::ZERO).await;
        let exporter = exporter_for(&receiver.base);
        let record = test_record("x");
        let scope = InstrumentationScope::builder("test").build();
        let result = exporter.export(LogBatch::new(&[(&record, &scope)])).await;
        assert!(
            matches!(result, Err(OTelSdkError::InternalFailure(ref reason)) if reason == "status")
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn export_reports_a_closed_port_as_a_failure() {
        let exporter = exporter_for("http://127.0.0.1:1/otlp");
        let record = test_record("x");
        let scope = InstrumentationScope::builder("test").build();
        let result = exporter.export(LogBatch::new(&[(&record, &scope)])).await;
        assert!(
            matches!(result, Err(OTelSdkError::InternalFailure(ref reason)) if reason == "connect")
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn outcome_classifies_each_result() {
        assert_eq!(outcome(Ok(Ok(StatusCode::NO_CONTENT))), Ok(()));
        assert_eq!(
            outcome(Ok(Ok(StatusCode::TOO_MANY_REQUESTS))),
            Err(Failure::new("status", Some(429)))
        );
        let panicked: tokio::task::JoinHandle<Result<StatusCode, reqwest::Error>> =
            tokio::spawn(async { panic!("boom") });
        assert_eq!(outcome(panicked.await), Err(Failure::new("runtime", None)));
        let refused = reqwest::Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .post("http://127.0.0.1:1")
            .send()
            .await
            .map(|response| response.status());
        assert_eq!(outcome(Ok(refused)), Err(Failure::new("connect", None)));
    }

    #[test]
    fn failures_are_logged_once_a_minute() {
        let log = FailureLog::default();
        let t0 = Instant::now();
        assert!(log.should_log(t0));
        assert!(!log.should_log(t0 + Duration::from_secs(59)));
        assert!(log.should_log(t0 + Duration::from_secs(60)));
        assert!(!log.should_log(t0 + Duration::from_secs(61)));
    }

    #[test]
    fn debug_hides_the_header_values() {
        let mut exporter = exporter_for("http://127.0.0.1:1/otlp");
        exporter.headers =
            request_headers(Some(&Secret::new("authorization=Bearer t0k3n".to_owned()))).unwrap();
        let shown = format!("{exporter:?}");
        assert!(shown.contains("authorization"));
        assert!(!shown.contains("t0k3n"));
    }

    #[test]
    fn the_operator_resource_extends_and_overrides() {
        let extended = resource(vec![
            ("service.name".into(), "cetra-eu1".into()),
            ("cetra.cell".into(), "eu1".into()),
        ]);
        assert_eq!(
            extended.get(&Key::new("service.name")).unwrap().as_str(),
            "cetra-eu1"
        );
        assert_eq!(
            extended.get(&Key::new("cetra.cell")).unwrap().as_str(),
            "eu1"
        );
        assert_eq!(
            extended.get(&Key::new("service.version")).unwrap().as_str(),
            env!("CARGO_PKG_VERSION")
        );

        let plain = resource(vec![]);
        for key in [
            "service.name",
            "service.version",
            "service.instance.id",
            "host.arch",
            "os.type",
        ] {
            assert!(plain.get(&Key::new(key)).is_some(), "missing {key}");
        }
        assert_eq!(plain.get(&Key::new("cetra.cell")), None);
    }

    #[test]
    fn host_arch_and_os_type_use_semantic_convention_names() {
        #[cfg(target_arch = "x86_64")]
        assert_eq!(host_arch(), "amd64");
        #[cfg(target_arch = "aarch64")]
        assert_eq!(host_arch(), "arm64");
        assert!(!host_arch().is_empty());
        #[cfg(target_os = "linux")]
        assert_eq!(os_type(), "linux");
        assert!(!os_type().is_empty());
    }

    #[test]
    fn batch_config_sets_every_field() {
        let shown = format!("{:?}", batch_config());
        assert!(shown.contains("max_queue_size: 2048"), "{shown}");
        assert!(shown.contains("scheduled_delay: 1s"), "{shown}");
        assert!(shown.contains("max_export_batch_size: 512"), "{shown}");
    }

    #[test]
    fn request_headers_put_cetras_own_last() {
        let headers = request_headers(None).unwrap();
        let mut names: Vec<&str> = headers.keys().map(HeaderName::as_str).collect();
        names.sort_unstable();
        assert_eq!(names, ["content-encoding", "content-type", "user-agent"]);

        let with_operator = request_headers(Some(&Secret::new("x-a=1".to_owned()))).unwrap();
        assert_eq!(with_operator["x-a"], "1");
        assert!(with_operator["x-a"].is_sensitive());
    }
}
