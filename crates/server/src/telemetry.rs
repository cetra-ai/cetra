//! Prometheus metrics served on `metrics_addr` (N M3, D-N-28, D-N-29).
//!
//! One root [`Registry`] holds the `process_*` gauges. Every `llmobs_*` metric is
//! registered on a sub-registry with [`PREFIX`], so one encode gives one document with a
//! single `# EOF`. Labels take values from the fixed sets in this module only (D-N-29):
//! never an org, project, user, key, email, model, URL, host or evaluator name.
//!
//! Rules from prometheus-client 0.26.0 (`00-design.md` § `/metrics`): counters are
//! registered without `_total`, build info is a gauge family, and labelled histograms are
//! built with `Family::new_with_constructor`.

use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use prometheus_client::encoding::text;
use prometheus_client::encoding::{EncodeLabelSet, EncodeLabelValue, LabelValueEncoder};
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::metrics::histogram::{Histogram, exponential_buckets};
use prometheus_client::registry::Registry;
use tokio::sync::Mutex;
use tokio::time::Instant;

use crate::http::AppState;

/// The prefix of every `llmobs_*` metric. The product rename (D46) changes this line.
const PREFIX: &str = "llmobs";

/// How long a queue count is reused. Scrapes every second must not load `meta.db` each time.
const EVAL_COUNT_TTL: Duration = Duration::from_secs(10);

/// The Prometheus registry and every metric of the `/metrics` table (N M3).
///
/// Fields are `pub(crate)` so the request, ingest, eval, alert and email code can count
/// into them (phase 06). Scrape-time values are refreshed by [`Self::render`].
#[derive(Debug)]
pub(crate) struct Telemetry {
    registry: Registry,
    /// `llmobs_storage_memtable_bytes`, from `StorageHealth`.
    memtable_bytes: Gauge,
    /// `llmobs_storage_memtable_hard_limit_bytes`, from `StorageHealth`.
    memtable_hard_limit_bytes: Gauge,
    /// `llmobs_storage_failed{check}`: 1 while the flag is set.
    storage_failed: Family<CheckLabel, Gauge>,
    /// `llmobs_storage_read_cache_hits_total`. Stays at 0 in local mode.
    read_cache_hits: Counter,
    /// `llmobs_storage_read_cache_misses_total`. Stays at 0 in local mode.
    read_cache_misses: Counter,
    /// `llmobs_storage_read_cache_bytes`.
    read_cache_bytes: Gauge,
    /// The last hits total copied into [`Self::read_cache_hits`], so it only grows.
    read_cache_seen_hits: AtomicU64,
    /// The last misses total copied into [`Self::read_cache_misses`], so it only grows.
    read_cache_seen_misses: AtomicU64,
    /// `llmobs_eval_jobs{status}`, from `meta.db`, cached for [`EVAL_COUNT_TTL`].
    eval_jobs: Family<EvalStatusLabel, Gauge>,
    /// The last queue count and when it was read.
    eval_open: Mutex<Option<(Instant, [i64; 2])>>,
    /// `process_resident_memory_bytes`. Linux only; left as it was elsewhere.
    process_resident_memory_bytes: Gauge,
    /// `process_open_fds`. Linux only; left as it was elsewhere.
    process_open_fds: Gauge,
    /// `llmobs_http_requests_total`. Counted from phase 06 (N M3).
    #[expect(dead_code, reason = "counted from phase 06 (N M3)")]
    pub(crate) http_requests: Family<HttpLabels, Counter>,
    /// `llmobs_http_request_duration_seconds`. Counted from phase 06 (N M3).
    #[expect(dead_code, reason = "counted from phase 06 (N M3)")]
    pub(crate) http_request_duration_seconds: Family<HttpRouteLabels, Histogram, fn() -> Histogram>,
    /// `llmobs_ingest_requests_total`. Counted from phase 06 (N M3).
    #[expect(dead_code, reason = "counted from phase 06 (N M3)")]
    pub(crate) ingest_requests: Family<IngestLabels, Counter>,
    /// `llmobs_ingest_spans_total`. Counted from phase 06 (N M3).
    #[expect(dead_code, reason = "counted from phase 06 (N M3)")]
    pub(crate) ingest_spans: Family<ProtocolLabel, Counter>,
    /// `llmobs_ingest_bytes_total`. Counted from phase 06 (N M3).
    #[expect(dead_code, reason = "counted from phase 06 (N M3)")]
    pub(crate) ingest_bytes: Family<ProtocolLabel, Counter>,
    /// `llmobs_eval_jobs_finished_total`. Counted from phase 06 (N M3).
    #[expect(dead_code, reason = "counted from phase 06 (N M3)")]
    pub(crate) eval_jobs_finished: Family<EvalOutcomeLabel, Counter>,
    /// `llmobs_eval_run_duration_seconds`. Counted from phase 06 (N M3).
    #[expect(dead_code, reason = "counted from phase 06 (N M3)")]
    pub(crate) eval_run_duration_seconds: Family<EvalKindLabel, Histogram, fn() -> Histogram>,
    /// `llmobs_alert_evaluations_total`. Counted from phase 06 (N M3).
    #[expect(dead_code, reason = "counted from phase 06 (N M3)")]
    pub(crate) alert_evaluations: Family<AlertOutcomeLabel, Counter>,
    /// `llmobs_alert_deliveries_total`. Counted from phase 06 (N M3).
    #[expect(dead_code, reason = "counted from phase 06 (N M3)")]
    pub(crate) alert_deliveries: Family<DeliveryLabels, Counter>,
    /// `llmobs_emails_total`. Counted from phase 06 (N M3).
    #[expect(dead_code, reason = "counted from phase 06 (N M3)")]
    pub(crate) emails: Family<EmailOutcomeLabel, Counter>,
}

impl Telemetry {
    /// Builds the registry. `restore_mode` is set once, and `llmobs_build_info` is 1.
    pub(crate) fn new(restore_mode: bool) -> Self {
        let mut registry = Registry::default();

        // Root-level process metrics, without the `llmobs_` prefix (Prometheus convention).
        let process_resident_memory_bytes = Gauge::default();
        let process_open_fds = Gauge::default();
        let process_start_time_seconds: Gauge = Gauge::default();
        let started = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| {
                i64::try_from(since.as_secs()).unwrap_or(i64::MAX)
            });
        process_start_time_seconds.set(started);
        registry.register(
            "process_resident_memory_bytes",
            "Resident memory of the server process in bytes",
            process_resident_memory_bytes.clone(),
        );
        registry.register(
            "process_open_fds",
            "Open file descriptors of the server process",
            process_open_fds.clone(),
        );
        registry.register(
            "process_start_time_seconds",
            "Start time of the server process in seconds since the Unix epoch",
            process_start_time_seconds,
        );

        let sub = registry.sub_registry_with_prefix(PREFIX);

        // Set once: registered, but not stored, since nothing reads them after this.
        let build_info = Family::<VersionLabel, Gauge>::default();
        build_info
            .get_or_create(&VersionLabel {
                version: env!("CARGO_PKG_VERSION").to_owned(),
            })
            .set(1);
        sub.register(
            "build_info",
            "Build information: the version of the server",
            build_info,
        );
        let restore: Gauge = Gauge::default();
        restore.set(i64::from(restore_mode));
        sub.register(
            "restore_mode",
            "1 when the server runs in restore mode (N M3, D-N-25)",
            restore,
        );

        let memtable_bytes = Gauge::default();
        sub.register(
            "storage_memtable_bytes",
            "Accounted bytes of both storage memtables",
            memtable_bytes.clone(),
        );
        let memtable_hard_limit_bytes = Gauge::default();
        sub.register(
            "storage_memtable_hard_limit_bytes",
            "The storage memtable hard limit in bytes",
            memtable_hard_limit_bytes.clone(),
        );
        let storage_failed = Family::<CheckLabel, Gauge>::default();
        sub.register(
            "storage_failed",
            "1 while a sticky storage failure flag is set (D-R-6)",
            storage_failed.clone(),
        );
        let read_cache_hits = Counter::default();
        sub.register(
            "storage_read_cache_hits",
            "Read cache blocks served from disk",
            read_cache_hits.clone(),
        );
        let read_cache_misses = Counter::default();
        sub.register(
            "storage_read_cache_misses",
            "Read cache blocks fetched from the object store",
            read_cache_misses.clone(),
        );
        let read_cache_bytes = Gauge::default();
        sub.register(
            "storage_read_cache_bytes",
            "Bytes of the read cache on disk, CRCs included",
            read_cache_bytes.clone(),
        );
        let eval_jobs = Family::<EvalStatusLabel, Gauge>::default();
        sub.register(
            "eval_jobs",
            "Evaluation jobs waiting or running in meta.db, cached 10 seconds",
            eval_jobs.clone(),
        );

        let http_requests = Family::<HttpLabels, Counter>::default();
        sub.register(
            "http_requests",
            "HTTP requests answered, by route, method and status class",
            http_requests.clone(),
        );
        let http_request_duration_seconds =
            Family::<HttpRouteLabels, Histogram, fn() -> Histogram>::new_with_constructor(
                http_seconds,
            );
        sub.register(
            "http_request_duration_seconds",
            "HTTP request duration in seconds, by route and method",
            http_request_duration_seconds.clone(),
        );
        let ingest_requests = Family::<IngestLabels, Counter>::default();
        sub.register(
            "ingest_requests",
            "OTLP ingest requests, by protocol and outcome",
            ingest_requests.clone(),
        );
        let ingest_spans = Family::<ProtocolLabel, Counter>::default();
        sub.register(
            "ingest_spans",
            "Spans accepted by OTLP ingest, by protocol",
            ingest_spans.clone(),
        );
        let ingest_bytes = Family::<ProtocolLabel, Counter>::default();
        sub.register(
            "ingest_bytes",
            "Bytes of accepted OTLP ingest requests, by protocol",
            ingest_bytes.clone(),
        );
        let eval_jobs_finished = Family::<EvalOutcomeLabel, Counter>::default();
        sub.register(
            "eval_jobs_finished",
            "Evaluation jobs that reached a final status, by outcome",
            eval_jobs_finished.clone(),
        );
        let eval_run_duration_seconds =
            Family::<EvalKindLabel, Histogram, fn() -> Histogram>::new_with_constructor(
                eval_seconds,
            );
        sub.register(
            "eval_run_duration_seconds",
            "Evaluation run duration in seconds, by kind",
            eval_run_duration_seconds.clone(),
        );
        let alert_evaluations = Family::<AlertOutcomeLabel, Counter>::default();
        sub.register(
            "alert_evaluations",
            "Alert rule evaluation passes, by outcome",
            alert_evaluations.clone(),
        );
        let alert_deliveries = Family::<DeliveryLabels, Counter>::default();
        sub.register(
            "alert_deliveries",
            "Alert deliveries, by channel and outcome",
            alert_deliveries.clone(),
        );
        let emails = Family::<EmailOutcomeLabel, Counter>::default();
        sub.register(
            "emails",
            "Emails sent by the mailer, by outcome",
            emails.clone(),
        );

        Self {
            registry,
            memtable_bytes,
            memtable_hard_limit_bytes,
            storage_failed,
            read_cache_hits,
            read_cache_misses,
            read_cache_bytes,
            read_cache_seen_hits: AtomicU64::new(0),
            read_cache_seen_misses: AtomicU64::new(0),
            eval_jobs,
            eval_open: Mutex::new(None),
            process_resident_memory_bytes,
            process_open_fds,
            http_requests,
            http_request_duration_seconds,
            ingest_requests,
            ingest_spans,
            ingest_bytes,
            eval_jobs_finished,
            eval_run_duration_seconds,
            alert_evaluations,
            alert_deliveries,
            emails,
        }
    }

    /// Refreshes the scrape-time gauges from `state`, then encodes the registry.
    pub(crate) async fn render(&self, state: &AppState) -> String {
        self.refresh_storage(state);
        self.refresh_eval_jobs(state).await;
        self.refresh_process();
        let mut out = String::new();
        if text::encode(&mut out, &self.registry).is_err() {
            tracing::error!("metrics encoding failed");
        }
        out
    }

    fn refresh_storage(&self, state: &AppState) {
        let health = state.storage.health();
        self.memtable_bytes.set(gauge_value(health.memtable_bytes));
        self.memtable_hard_limit_bytes
            .set(gauge_value(health.memtable_hard_limit));
        let checks = [
            (StorageCheck::spans_wal, health.spans_wal_failed),
            (StorageCheck::scores_wal, health.scores_wal_failed),
            (StorageCheck::objects_sync, health.objects_sync_failed),
            (StorageCheck::catalog, health.catalog_failed),
        ];
        for (check, failed) in checks {
            self.storage_failed
                .get_or_create(&CheckLabel { check })
                .set(i64::from(failed));
        }
        if let Some(stats) = state.storage.read_cache_stats() {
            let hits = self
                .read_cache_seen_hits
                .swap(stats.hits, Ordering::Relaxed);
            self.read_cache_hits.inc_by(stats.hits.saturating_sub(hits));
            let misses = self
                .read_cache_seen_misses
                .swap(stats.misses, Ordering::Relaxed);
            self.read_cache_misses
                .inc_by(stats.misses.saturating_sub(misses));
            self.read_cache_bytes.set(gauge_value(stats.bytes));
        }
    }

    async fn refresh_eval_jobs(&self, state: &AppState) {
        let [queued, running] = self.eval_open(state).await;
        self.eval_jobs
            .get_or_create(&EvalStatusLabel {
                status: EvalStatus::queued,
            })
            .set(queued);
        self.eval_jobs
            .get_or_create(&EvalStatusLabel {
                status: EvalStatus::running,
            })
            .set(running);
    }

    /// `[queued, running]` from `meta.db`, reused for [`EVAL_COUNT_TTL`]. A failed query keeps
    /// the last values (zeros before the first success) and logs the error, which has no payload.
    async fn eval_open(&self, state: &AppState) -> [i64; 2] {
        let mut cached = self.eval_open.lock().await;
        if let Some((read_at, counts)) = *cached
            && read_at.elapsed() < EVAL_COUNT_TTL
        {
            return counts;
        }
        match crate::meta::eval_jobs::count_open(state.meta.reader()).await {
            Ok(counts) => {
                *cached = Some((Instant::now(), counts));
                counts
            }
            Err(error) => {
                tracing::warn!(error = %error, "eval queue count failed; keeping the last value");
                cached.map_or([0, 0], |(_, counts)| counts)
            }
        }
    }

    fn refresh_process(&self) {
        if let Some(bytes) = resident_memory_bytes() {
            self.process_resident_memory_bytes.set(bytes);
        }
        if let Some(fds) = open_fds() {
            self.process_open_fds.set(fds);
        }
    }
}

/// A value that cannot exceed `i64::MAX` in practice; a larger one saturates.
fn gauge_value(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

/// `VmRSS` of `/proc/self/status`, in bytes (kB × 1024). The page size is not assumed.
#[cfg(target_os = "linux")]
fn resident_memory_bytes() -> Option<i64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find(|line| line.starts_with("VmRSS:"))?;
    let kib: i64 = line.split_whitespace().nth(1)?.parse().ok()?;
    kib.checked_mul(1024)
}

#[cfg(not(target_os = "linux"))]
fn resident_memory_bytes() -> Option<i64> {
    None
}

/// The number of entries in `/proc/self/fd`.
#[cfg(target_os = "linux")]
fn open_fds() -> Option<i64> {
    let count = std::fs::read_dir("/proc/self/fd").ok()?.count();
    i64::try_from(count).ok()
}

#[cfg(not(target_os = "linux"))]
fn open_fds() -> Option<i64> {
    None
}

fn http_seconds() -> Histogram {
    Histogram::new(exponential_buckets(0.005, 2.0, 12))
}

fn eval_seconds() -> Histogram {
    Histogram::new(exponential_buckets(0.05, 2.0, 12))
}

/// The `version` label of `llmobs_build_info`.
#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub(crate) struct VersionLabel {
    version: String,
}

/// The `check` label of `llmobs_storage_failed`.
#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub(crate) struct CheckLabel {
    pub(crate) check: StorageCheck,
}

/// The `status` label of `llmobs_eval_jobs`.
#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub(crate) struct EvalStatusLabel {
    pub(crate) status: EvalStatus,
}

/// `llmobs_http_requests_total` labels.
#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub(crate) struct HttpLabels {
    /// The axum route template, or `other` for requests that match no route.
    pub(crate) route: String,
    pub(crate) method: Method,
    pub(crate) status: StatusClass,
}

/// `llmobs_http_request_duration_seconds` labels.
#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub(crate) struct HttpRouteLabels {
    /// The axum route template, or `other` for requests that match no route.
    pub(crate) route: String,
    pub(crate) method: Method,
}

/// `llmobs_ingest_requests_total` labels.
#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub(crate) struct IngestLabels {
    pub(crate) protocol: Protocol,
    pub(crate) outcome: IngestOutcome,
}

/// The `protocol` label of the ingest span and byte counters.
#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub(crate) struct ProtocolLabel {
    pub(crate) protocol: Protocol,
}

/// `llmobs_eval_jobs_finished_total` labels.
#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub(crate) struct EvalOutcomeLabel {
    pub(crate) outcome: EvalOutcome,
}

/// `llmobs_eval_run_duration_seconds` labels.
#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub(crate) struct EvalKindLabel {
    pub(crate) kind: EvalKind,
}

/// `llmobs_alert_evaluations_total` labels.
#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub(crate) struct AlertOutcomeLabel {
    pub(crate) outcome: Outcome,
}

/// `llmobs_alert_deliveries_total` labels.
#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub(crate) struct DeliveryLabels {
    pub(crate) channel: Channel,
    pub(crate) outcome: DeliveryOutcome,
}

/// `llmobs_emails_total` labels.
#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub(crate) struct EmailOutcomeLabel {
    pub(crate) outcome: EmailOutcome,
}

// The enums below write their variant identifiers as label values, so each variant is named
// exactly like its value (D-N-29).

/// The HTTP method label. Any other method is `other`.
#[allow(non_camel_case_types, clippy::upper_case_acronyms)]
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, EncodeLabelValue)]
#[expect(dead_code, reason = "counted from phase 06 (N M3)")]
pub(crate) enum Method {
    GET,
    POST,
    PUT,
    PATCH,
    DELETE,
    HEAD,
    OPTIONS,
    other,
}

/// The status class label: `2xx`, `3xx`, `4xx` or `5xx`. Not an identifier, so its encoding is
/// written by hand.
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
#[expect(dead_code, reason = "counted from phase 06 (N M3)")]
pub(crate) enum StatusClass {
    Class2xx,
    Class3xx,
    Class4xx,
    Class5xx,
}

impl EncodeLabelValue for StatusClass {
    fn encode(&self, encoder: &mut LabelValueEncoder) -> Result<(), std::fmt::Error> {
        let value = match self {
            Self::Class2xx => "2xx",
            Self::Class3xx => "3xx",
            Self::Class4xx => "4xx",
            Self::Class5xx => "5xx",
        };
        encoder.write_str(value)
    }
}

/// The protocol label of the OTLP ingest counters.
#[allow(non_camel_case_types)]
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, EncodeLabelValue)]
#[expect(dead_code, reason = "counted from phase 06 (N M3)")]
pub(crate) enum Protocol {
    otlp_http,
    otlp_grpc,
}

/// The outcome label of `llmobs_ingest_requests_total`.
#[allow(non_camel_case_types)]
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, EncodeLabelValue)]
#[expect(dead_code, reason = "counted from phase 06 (N M3)")]
pub(crate) enum IngestOutcome {
    ok,
    invalid,
    unauthorized,
    too_large,
    rate_limited,
    quota,
    unavailable,
    disk_full,
}

/// The `check` label of `llmobs_storage_failed`.
#[allow(non_camel_case_types)]
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, EncodeLabelValue)]
pub(crate) enum StorageCheck {
    spans_wal,
    scores_wal,
    objects_sync,
    catalog,
}

/// The `status` label of `llmobs_eval_jobs`.
#[allow(non_camel_case_types)]
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, EncodeLabelValue)]
pub(crate) enum EvalStatus {
    queued,
    running,
}

/// The outcome label of `llmobs_eval_jobs_finished_total`: the final `eval_jobs` statuses.
#[allow(non_camel_case_types)]
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, EncodeLabelValue)]
#[expect(dead_code, reason = "counted from phase 06 (N M3)")]
pub(crate) enum EvalOutcome {
    done,
    failed,
    skipped,
}

/// The `kind` label of `llmobs_eval_run_duration_seconds`.
#[allow(non_camel_case_types)]
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, EncodeLabelValue)]
#[expect(dead_code, reason = "counted from phase 06 (N M3)")]
pub(crate) enum EvalKind {
    llm_judge,
    webhook,
    check,
}

/// The outcome label of the alert evaluation counter and of the flush counters.
#[allow(non_camel_case_types)]
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, EncodeLabelValue)]
#[expect(dead_code, reason = "counted from phase 06 (N M3)")]
pub(crate) enum Outcome {
    ok,
    failed,
}

/// The `channel` label of `llmobs_alert_deliveries_total`.
#[allow(non_camel_case_types)]
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, EncodeLabelValue)]
#[expect(dead_code, reason = "counted from phase 06 (N M3)")]
pub(crate) enum Channel {
    email,
    webhook,
    slack,
}

/// The `outcome` label of `llmobs_alert_deliveries_total`.
#[allow(non_camel_case_types)]
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, EncodeLabelValue)]
#[expect(dead_code, reason = "counted from phase 06 (N M3)")]
pub(crate) enum DeliveryOutcome {
    sent,
    failed,
}

/// The `outcome` label of `llmobs_emails_total`.
#[allow(non_camel_case_types)]
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, EncodeLabelValue)]
#[expect(dead_code, reason = "counted from phase 06 (N M3)")]
pub(crate) enum EmailOutcome {
    sent,
    failed,
    refused,
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use uuid::Uuid;

    use super::*;
    use crate::meta::eval_jobs::{KIND_MANUAL, NewJob, insert_job};
    use crate::test_support::{Seeded, TestApp};

    fn sample(text: &str, series: &str) -> Option<i64> {
        text.lines().find_map(|line| {
            line.strip_prefix(series)?
                .strip_prefix(' ')?
                .parse::<i64>()
                .ok()
        })
    }

    async fn render(app: &TestApp) -> String {
        app.state.telemetry.render(&app.state).await
    }

    async fn create_webhook(app: &TestApp, seeded: &Seeded) -> Uuid {
        let response = app
            .request(
                axum::http::Method::POST,
                &format!("/api/v1/projects/{}/evaluators", seeded.project.id),
            )
            .header("cookie", &seeded.cookie)
            .json(&json!({
                "name": "Hook",
                "kind": "webhook",
                "config": {"url": "http://127.0.0.1:9/hook"},
                "output_score": {"name": "hook", "data_type": "numeric"},
                "webhook_secret": "0123456789abcdef"
            }))
            .send()
            .await;
        assert_eq!(
            response.status,
            201,
            "{}",
            String::from_utf8_lossy(&response.body)
        );
        let value: serde_json::Value = response.json();
        Uuid::parse_str(value["id"].as_str().expect("id")).expect("uuid")
    }

    /// Queues one manual job of `evaluator` on `trace`.
    async fn enqueue(app: &TestApp, seeded: &Seeded, evaluator: Uuid, trace: &str) {
        let now = app.state.clock.now_ns();
        let job = NewJob {
            id: Uuid::now_v7(),
            project_id: seeded.project.id,
            kind: KIND_MANUAL,
            rule_id: None,
            evaluator_id: evaluator,
            variable_mapping: Some(r#"{"input":"trace.input"}"#.to_owned()),
            trace_id: trace.parse().expect("trace id"),
            span_id: None,
            trace_start_ns: now,
        };
        app.state
            .meta
            .write(move |conn| Box::pin(async move { insert_job(conn, job, now).await }))
            .await
            .expect("job inserted");
    }

    #[tokio::test]
    async fn render_lists_the_base_metrics() {
        let app = TestApp::new().await;
        let text = render(&app).await;
        let build = format!(
            "llmobs_build_info{{version=\"{}\"}}",
            env!("CARGO_PKG_VERSION")
        );
        assert_eq!(sample(&text, &build), Some(1), "{text}");
        assert_eq!(sample(&text, "llmobs_restore_mode"), Some(0), "{text}");
        assert!(
            text.contains("llmobs_storage_memtable_hard_limit_bytes "),
            "{text}"
        );
        assert_eq!(
            sample(&text, "llmobs_storage_failed{check=\"catalog\"}"),
            Some(0),
            "{text}"
        );
        assert_eq!(
            sample(&text, "llmobs_eval_jobs{status=\"queued\"}"),
            Some(0),
            "{text}"
        );
        assert_eq!(text.matches("# EOF").count(), 1, "{text}");
        assert!(text.trim_end().ends_with("# EOF"), "{text}");
    }

    #[tokio::test]
    async fn restore_mode_gauge_is_one() {
        let app = TestApp::with_config(|config| config.restore_mode = true).await;
        let text = render(&app).await;
        assert_eq!(sample(&text, "llmobs_restore_mode"), Some(1), "{text}");
    }

    #[tokio::test]
    async fn eval_queue_count_is_cached() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let evaluator = create_webhook(&app, &seeded).await;
        enqueue(&app, &seeded, evaluator, "a1000000000000000000000000000081").await;
        tokio::time::pause();
        let queued = "llmobs_eval_jobs{status=\"queued\"}";
        assert_eq!(sample(&render(&app).await, queued), Some(1));

        enqueue(&app, &seeded, evaluator, "a1000000000000000000000000000082").await;
        assert_eq!(
            sample(&render(&app).await, queued),
            Some(1),
            "reused within 10 s"
        );

        tokio::time::advance(Duration::from_secs(11)).await;
        assert_eq!(sample(&render(&app).await, queued), Some(2), "read again");
    }

    #[tokio::test]
    async fn labels_carry_no_tenant_data() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let now = app.state.clock.now_ns();
        let trace = "a1000000000000000000000000000090"
            .parse()
            .expect("trace id");
        app.commit_trace_at(&seeded.project, trace, 0, now).await;
        let text = render(&app).await;
        let project_id = seeded.project.id.to_string();
        for secret in [
            "acme",
            "support-bot",
            "ada@example.com",
            project_id.as_str(),
        ] {
            assert!(!text.contains(secret), "{secret} is in the output");
        }
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn process_metrics_on_linux() {
        let app = TestApp::new().await;
        let text = render(&app).await;
        assert!(
            sample(&text, "process_open_fds").is_some_and(|fds| fds > 0),
            "{text}"
        );
        assert!(
            sample(&text, "process_resident_memory_bytes").is_some_and(|bytes| bytes > 0),
            "{text}"
        );
    }
}
