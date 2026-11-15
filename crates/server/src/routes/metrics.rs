//! Dashboard metrics (spec 05 § Dashboard metrics, spec 09 § Observability data).

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex, PoisonError};

use axum::extract::{Extension, State};
use axum::http::Method;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use llmobs_core::ids::ProjectId;
use llmobs_storage as storage;
use tokio::sync::OnceCell;

use crate::api::convert;
use crate::api::metrics::{MetricsBucket, MetricsGroupBy, MetricsParams, MetricsResponse};
use crate::auth::roles::ProjectPerm;
use crate::http::AppState;
use crate::http::access::{Access, ProjectAccess, RouteDef};
use crate::http::error::{self, ApiError};
use crate::routes::read::{READ_TIMEOUT, open_query};

/// The in-flight marker of a slot's expiry: a computation that has not finished never expires.
const IN_FLIGHT: i64 = i64::MAX;

/// How long a `GET /metrics` answer is kept, in nanoseconds (spec 05).
const TTL_NS: i64 = 10_000_000_000;

/// Most cached answers. A full cache drops the expired ones, then the oldest.
const MAX_ENTRIES: usize = 256;

/// `GET /metrics` answers, kept for [`TTL_NS`] per project and parameters (spec 05 § Dashboard metrics).
#[derive(Debug, Default)]
pub(crate) struct MetricsCache {
    entries: Mutex<HashMap<MetricsKey, Slot>>,
}

/// Everything that changes the answer.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct MetricsKey {
    project: ProjectId,
    from: i64,
    to: i64,
    bucket: MetricsBucket,
    filter: Option<String>,
    group_by: Option<MetricsGroupBy>,
    include_eval: bool,
    include_experiments: bool,
}

/// One key's answer. The cell is filled once, so concurrent misses of the same key share one
/// computation (single-flight), and a failure is shared by everyone waiting on it. A failed
/// slot is removed as it settles, so the next request runs the query again.
#[derive(Debug)]
struct Slot {
    /// `IN_FLIGHT` until the computation finishes, then `TTL_NS` after that finish.
    expires_at: i64,
    answer: Arc<OnceCell<Cached>>,
}

/// A computation's outcome. An error is shared, so it is an `Arc`: waiters each answer with it.
type Cached = Result<Arc<MetricsResponse>, Arc<ApiError>>;

/// The error of a metrics request. It answers with the same problem response as the error it
/// wraps, whichever request it is sent to.
pub(crate) struct SharedError(Arc<ApiError>);

impl IntoResponse for SharedError {
    fn into_response(self) -> Response {
        self.0.to_response()
    }
}

impl MetricsCache {
    /// The answer for `key`: the cached one, or `compute`'s, which runs once even when
    /// identical requests miss together. `now` is read when the request starts and again when
    /// the computation finishes, which stamps the answer's expiry.
    ///
    /// # Errors
    ///
    /// Returns the error of `compute`, shared with every request that waited on it. Nothing
    /// stays cached for it.
    async fn get_or_compute<F, Fut>(
        &self,
        key: MetricsKey,
        now: impl Fn() -> i64,
        compute: F,
    ) -> Result<Arc<MetricsResponse>, Arc<ApiError>>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<MetricsResponse, ApiError>>,
    {
        let answer = self.slot(key.clone(), now());
        let cached = answer
            .get_or_init(|| async { compute().await.map(Arc::new).map_err(Arc::new) })
            .await
            .clone();
        self.settle(&key, &answer, cached.is_err(), now());
        cached
    }

    /// The cell of `key` while it is fresh at `now`. Otherwise a new empty one replaces it.
    fn slot(&self, key: MetricsKey, now: i64) -> Arc<OnceCell<Cached>> {
        let mut entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(slot) = entries.get(&key).filter(|slot| now < slot.expires_at) {
            return Arc::clone(&slot.answer);
        }
        if !entries.contains_key(&key) && entries.len() >= MAX_ENTRIES {
            evict(&mut entries, now);
        }
        let answer = Arc::new(OnceCell::new());
        entries.insert(
            key,
            Slot {
                expires_at: IN_FLIGHT,
                answer: Arc::clone(&answer),
            },
        );
        answer
    }

    /// Stamps the expiry of `answer` at `now`, the time its computation finished, or removes its
    /// slot when the computation failed. Only the slot that holds `answer` is touched.
    fn settle(&self, key: &MetricsKey, answer: &Arc<OnceCell<Cached>>, failed: bool, now: i64) {
        let mut entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(slot) = entries.get_mut(key) else {
            return;
        };
        if !Arc::ptr_eq(&slot.answer, answer) {
            return;
        }
        if !failed {
            if slot.expires_at == IN_FLIGHT {
                slot.expires_at = now.saturating_add(TTL_NS);
            }
            return;
        }
        entries.remove(key);
    }

    /// Drops every cached answer of `project` (D-M-11).
    pub(crate) fn clear_project(&self, project: ProjectId) {
        let mut entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        entries.retain(|key, _| key.project != project);
    }
}

/// Makes room for one entry in a full cache: the expired entries go first, then the oldest.
fn evict(entries: &mut HashMap<MetricsKey, Slot>, now: i64) {
    entries.retain(|_, slot| now < slot.expires_at);
    if entries.len() < MAX_ENTRIES {
        return;
    }
    let excess = entries.len() + 1 - MAX_ENTRIES;
    let mut by_age: Vec<(i64, MetricsKey)> = entries
        .iter()
        .map(|(key, slot)| (slot.expires_at, key.clone()))
        .collect();
    by_age.sort_unstable_by_key(|(expires_at, _)| *expires_at);
    for (_, key) in by_age.into_iter().take(excess) {
        entries.remove(&key);
    }
}

/// Metrics read route. Documented and needs project view.
pub(crate) fn routes() -> Vec<RouteDef> {
    vec![RouteDef {
        method: Method::GET,
        path: "/api/v1/projects/{project}/metrics",
        access: Access::Project {
            perm: ProjectPerm::View,
            session_only: false,
        },
        documented: true,
        handler: get(get_metrics),
    }]
}

async fn get_metrics(
    State(state): State<AppState>,
    Extension(access): Extension<ProjectAccess>,
    error::Query(params): error::Query<MetricsParams>,
) -> Result<error::Json<MetricsResponse>, SharedError> {
    let from = params.from.ns();
    let to = params.to.ns();
    let key = MetricsKey {
        project: access.project.id,
        from,
        to,
        bucket: params.bucket,
        filter: params.filter.clone(),
        group_by: params.group_by,
        include_eval: params.include_eval,
        include_experiments: params.include_experiments,
    };
    let project = access.project.id;
    let clock = &state.clock;
    let response = state
        .metrics_cache
        .get_or_compute(
            key,
            || clock.now_ns(),
            || async {
                let window = storage::TimeWindow::new(from, to)?;
                let ctx = open_query(&state, project, None).await?;
                let data = storage::metrics(
                    &ctx,
                    storage::MetricsRequest {
                        window,
                        bucket: bucket(params.bucket),
                        filter: params.filter.as_deref(),
                        group_by: params.group_by.map(storage_group_by),
                        include_eval: params.include_eval,
                        include_experiments: params.include_experiments,
                        timeout: READ_TIMEOUT,
                    },
                )
                .await?;
                Ok(convert::metrics_response(data, from, to, params.group_by))
            },
        )
        .await
        .map_err(SharedError)?;
    Ok(error::Json((*response).clone()))
}

fn bucket(bucket: MetricsBucket) -> storage::Bucket {
    use storage::BucketWidth;
    let width = match bucket {
        MetricsBucket::Auto => return storage::Bucket::Auto,
        MetricsBucket::M1 => BucketWidth::M1,
        MetricsBucket::M5 => BucketWidth::M5,
        MetricsBucket::M10 => BucketWidth::M10,
        MetricsBucket::M15 => BucketWidth::M15,
        MetricsBucket::M30 => BucketWidth::M30,
        MetricsBucket::H1 => BucketWidth::H1,
        MetricsBucket::H6 => BucketWidth::H6,
        MetricsBucket::H12 => BucketWidth::H12,
        MetricsBucket::D1 => BucketWidth::D1,
    };
    storage::Bucket::Fixed(width)
}

fn storage_group_by(group_by: MetricsGroupBy) -> storage::MetricsGroupBy {
    match group_by {
        MetricsGroupBy::Model => storage::MetricsGroupBy::Model,
        MetricsGroupBy::Environment => storage::MetricsGroupBy::Environment,
        MetricsGroupBy::Release => storage::MetricsGroupBy::Release,
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;
    use std::time::Duration;

    use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};

    use axum::http::{Method, StatusCode};
    use llmobs_core::batch::spans_to_record_batch;
    use llmobs_core::ids::TraceId;
    use llmobs_core::span::Span;
    use serde_json::Value as Json;

    use super::*;
    use crate::api::metrics::KpiMetric;
    use crate::test_support::{Seeded, TestApp};

    const WINDOW: &str = "from=2026-01-01T10:00:00Z&to=2026-01-01T11:00:00Z&bucket=10m";

    /// A seeded app with the example dataset loaded.
    async fn loaded() -> (TestApp, Seeded) {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        app.load_example_dataset(seeded.project.id).await;
        (app, seeded)
    }

    async fn metrics(app: &TestApp, seeded: &Seeded, query: &str) -> (StatusCode, Json) {
        let response = app
            .request(
                Method::GET,
                &format!("/api/v1/projects/{}/metrics?{query}", seeded.project.id),
            )
            .header("cookie", &seeded.cookie)
            .send()
            .await;
        let body = response.json::<Json>();
        (response.status, body)
    }

    fn kpi_values(response: &MetricsResponse) -> Vec<(KpiMetric, Option<f64>)> {
        response
            .kpis
            .iter()
            .map(|kpi| (kpi.metric, kpi.value))
            .collect()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn window_gives_kpis_and_buckets() {
        let (app, seeded) = loaded().await;
        let (status, body) = metrics(&app, &seeded, WINDOW).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let response: MetricsResponse = serde_json::from_value(body).unwrap();
        assert_eq!(response.bucket_ns, 600_000_000_000);
        assert!(response.group_by.is_none());

        let kpis = kpi_values(&response);
        assert_eq!(kpis[0], (KpiMetric::Traces, Some(5.0)));
        assert_eq!(
            kpis.iter().map(|(metric, _)| *metric).collect::<Vec<_>>(),
            [
                KpiMetric::Traces,
                KpiMetric::ErrorRate,
                KpiMetric::LatencyP95,
                KpiMetric::CostTotal,
                KpiMetric::TotalTokens,
            ]
        );
        let cost = kpis[3].1.unwrap();
        assert!((cost - 0.00955).abs() < 1e-12, "{cost}");
        assert!(response.kpis.iter().all(|kpi| kpi.previous.is_none()));
        assert!(
            response
                .series
                .iter()
                .all(|series| series.points.len() == 6)
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn missing_or_empty_range_is_bad_request() {
        let (app, seeded) = loaded().await;
        let (status, body) = metrics(&app, &seeded, "to=2026-01-01T11:00:00Z").await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(body["code"], "bad_request");

        let (status, body) = metrics(
            &app,
            &seeded,
            "from=2026-01-01T10:00:00Z&to=2026-01-01T10:00:00Z",
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(body["code"], "bad_request");
    }

    /// A key of `project` that differs only in its start, `from`.
    fn key_at(project: ProjectId, from: i64) -> MetricsKey {
        MetricsKey {
            project,
            from,
            to: from + 1,
            bucket: MetricsBucket::Auto,
            filter: None,
            group_by: None,
            include_eval: false,
            include_experiments: false,
        }
    }

    fn empty_response() -> MetricsResponse {
        serde_json::from_value(serde_json::json!({
            "from": "2026-01-01T10:00:00Z",
            "to": "2026-01-01T11:00:00Z",
            "bucket_ns": 600_000_000_000_i64,
            "kpis": [],
            "series": [],
            "top_models": [],
        }))
        .unwrap()
    }

    fn cached_keys(cache: &MetricsCache) -> Vec<MetricsKey> {
        let entries = cache.entries.lock().unwrap();
        let mut keys: Vec<MetricsKey> = entries.keys().cloned().collect();
        keys.sort_by_key(|key| (key.project, key.from));
        keys
    }

    #[tokio::test]
    async fn clear_project_drops_only_that_project() {
        let cache = MetricsCache::default();
        let (p, q) = (ProjectId::new_v7(), ProjectId::new_v7());
        for key in [key_at(p, 0), key_at(q, 0)] {
            cache
                .get_or_compute(key, || 0, || async { Ok(empty_response()) })
                .await
                .unwrap();
        }

        cache.clear_project(p);
        assert_eq!(cached_keys(&cache), vec![key_at(q, 0)]);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_identical_misses_run_the_query_once() {
        let cache = Arc::new(MetricsCache::default());
        let runs = Arc::new(AtomicUsize::new(0));
        let key = key_at(ProjectId::new_v7(), 0);
        let mut tasks = Vec::new();
        for _ in 0..8 {
            let (cache, runs, key) = (Arc::clone(&cache), Arc::clone(&runs), key.clone());
            tasks.push(tokio::spawn(async move {
                cache
                    .get_or_compute(
                        key,
                        || 0,
                        || async {
                            runs.fetch_add(1, Ordering::SeqCst);
                            // Long enough that the other requests arrive while this one runs.
                            tokio::time::sleep(Duration::from_millis(100)).await;
                            Ok(empty_response())
                        },
                    )
                    .await
            }));
        }
        for task in tasks {
            task.await.unwrap().unwrap();
        }
        assert_eq!(runs.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_waiters_share_one_failure_and_a_later_request_retries() {
        let cache = Arc::new(MetricsCache::default());
        let runs = Arc::new(AtomicUsize::new(0));
        let key = key_at(ProjectId::new_v7(), 0);
        let mut tasks = Vec::new();
        for _ in 0..8 {
            let (cache, runs, key) = (Arc::clone(&cache), Arc::clone(&runs), key.clone());
            tasks.push(tokio::spawn(async move {
                cache
                    .get_or_compute(
                        key,
                        || 0,
                        || async {
                            runs.fetch_add(1, Ordering::SeqCst);
                            tokio::time::sleep(Duration::from_millis(100)).await;
                            Err(ApiError::Query(storage::RunError::Timeout))
                        },
                    )
                    .await
            }));
        }
        for task in tasks {
            let error = task.await.unwrap().expect_err("the query failed");
            assert!(
                matches!(error.as_ref(), ApiError::Query(storage::RunError::Timeout)),
                "{error:?}"
            );
        }
        assert_eq!(runs.load(Ordering::SeqCst), 1, "the waiters share one run");

        // The failed slot was removed: the next request runs the query again and is cached.
        let answered = cache
            .get_or_compute(
                key.clone(),
                || 0,
                || async {
                    runs.fetch_add(1, Ordering::SeqCst);
                    Ok(empty_response())
                },
            )
            .await;
        assert!(answered.is_ok());
        assert_eq!(runs.load(Ordering::SeqCst), 2);
        assert_eq!(cached_keys(&cache), vec![key]);
    }

    #[tokio::test]
    async fn expiry_is_stamped_when_the_computation_finishes() {
        const SECOND: i64 = 1_000_000_000;
        let cache = MetricsCache::default();
        let key = key_at(ProjectId::new_v7(), 0);
        let clock = Arc::new(AtomicI64::new(0));
        let now = {
            let clock = Arc::clone(&clock);
            move || clock.load(Ordering::SeqCst)
        };
        let runs = AtomicUsize::new(0);
        // The query runs from 0 to 20 s, so its answer lives until 30 s, not until TTL_NS.
        cache
            .get_or_compute(key.clone(), &now, || async {
                runs.fetch_add(1, Ordering::SeqCst);
                clock.store(20 * SECOND, Ordering::SeqCst);
                Ok(empty_response())
            })
            .await
            .unwrap();
        clock.store(25 * SECOND, Ordering::SeqCst);
        cache
            .get_or_compute(key.clone(), &now, || async {
                runs.fetch_add(1, Ordering::SeqCst);
                Ok(empty_response())
            })
            .await
            .unwrap();
        assert_eq!(runs.load(Ordering::SeqCst), 1, "still fresh at 25 s");
        clock.store(31 * SECOND, Ordering::SeqCst);
        cache
            .get_or_compute(key, &now, || async {
                runs.fetch_add(1, Ordering::SeqCst);
                Ok(empty_response())
            })
            .await
            .unwrap();
        assert_eq!(runs.load(Ordering::SeqCst), 2, "expired at 31 s");
    }

    #[tokio::test]
    async fn a_full_cache_evicts_the_oldest_answers() {
        let cache = MetricsCache::default();
        let project = ProjectId::new_v7();
        for from in 0..MAX_ENTRIES as i64 {
            cache
                .get_or_compute(
                    key_at(project, from),
                    || from,
                    || async { Ok(empty_response()) },
                )
                .await
                .unwrap();
        }
        let newest = key_at(project, MAX_ENTRIES as i64);
        cache
            .get_or_compute(
                newest.clone(),
                || MAX_ENTRIES as i64,
                || async { Ok(empty_response()) },
            )
            .await
            .unwrap();

        let keys = cached_keys(&cache);
        assert_eq!(keys.len(), MAX_ENTRIES);
        assert!(!keys.contains(&key_at(project, 0)), "the oldest stayed");
        assert!(keys.contains(&key_at(project, 1)), "a newer answer went");
        assert!(keys.contains(&newest));
    }

    #[tokio::test]
    async fn a_full_cache_drops_expired_answers_before_live_ones() {
        let cache = MetricsCache::default();
        let project = ProjectId::new_v7();
        for from in 0..MAX_ENTRIES as i64 {
            cache
                .get_or_compute(
                    key_at(project, from),
                    || 0,
                    || async { Ok(empty_response()) },
                )
                .await
                .unwrap();
        }
        // Every answer above expires at TTL_NS, so none of them is live now.
        cache
            .get_or_compute(
                key_at(project, -1),
                || TTL_NS,
                || async { Ok(empty_response()) },
            )
            .await
            .unwrap();
        assert_eq!(cached_keys(&cache), vec![key_at(project, -1)]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn answers_are_cached_for_ten_seconds() {
        let (app, seeded) = loaded().await;
        let (status, body) = metrics(&app, &seeded, WINDOW).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let first: MetricsResponse = serde_json::from_value(body).unwrap();
        assert_eq!(kpi_values(&first)[0], (KpiMetric::Traces, Some(5.0)));

        // One more trace inside the window: a copy of span `…061` under a new root.
        let dataset = concat!(env!("CARGO_MANIFEST_DIR"), "/../../fixtures/api/dataset");
        let text = std::fs::read_to_string(format!("{dataset}/spans.json")).unwrap();
        let spans: Vec<Span> = serde_json::from_str(&text).unwrap();
        let mut span = spans
            .into_iter()
            .find(|span| span.span_id.to_string() == "b100000000000061")
            .unwrap();
        span.project_id = seeded.project.id;
        span.trace_id = TraceId::from_str("a1000000000000000000000000000007").unwrap();
        span.parent_span_id = None;
        span.start_time = 1_767_261_600_000_000_000 + 40 * 60_000_000_000;
        span.end_time = span.start_time + 2_000_000_000;
        let batch = spans_to_record_batch(&[span]).unwrap();
        app.state
            .storage
            .append_spans(seeded.project.id, batch)
            .await
            .unwrap();

        let (_, body) = metrics(&app, &seeded, WINDOW).await;
        let cached: MetricsResponse = serde_json::from_value(body).unwrap();
        assert_eq!(kpi_values(&cached)[0], (KpiMetric::Traces, Some(5.0)));

        app.state.clock.advance(Duration::from_secs(11));
        let (_, body) = metrics(&app, &seeded, WINDOW).await;
        let fresh: MetricsResponse = serde_json::from_value(body).unwrap();
        assert_eq!(kpi_values(&fresh)[0], (KpiMetric::Traces, Some(6.0)));
    }
}
