//! `/healthz` and `/readyz` (spec 09, spec 12).
//!
//! `/readyz` checks, in this order: `meta.db`, the spans and scores WALs, the local
//! object fsync, the catalog, free disk, and the object store (D-N-30). The object-store
//! probe is cached for [`PROBE_TTL`] (D-N-36). `/healthz` fails only on the sticky storage
//! failures that a restart clears (D-N-35).

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::response::{IntoResponse, Response};
use cetra_storage::StorageHealth;
use serde::Serialize;
use tokio::time::Instant;

use super::AppState;
use super::error;
use crate::api::common::{Problem, ProblemCode};

/// How long an object-store probe result is reused, success or failure (spec 12).
const PROBE_TTL: Duration = Duration::from_secs(10);
/// One probe's whole time budget. A timeout counts as unreachable.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Serialize)]
struct Probe {
    status: &'static str,
    /// Shown only in restore mode (N M3, D-N-25).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    restore_mode: bool,
}

/// The last object-store probe. The mutex is held across the probe, so concurrent
/// `/readyz` requests wait for one probe instead of each sending their own.
#[derive(Default)]
pub(crate) struct ReadyCache {
    last: tokio::sync::Mutex<Option<(Instant, bool)>>,
}

impl ReadyCache {
    /// `true` when the store is reachable. Calls `probe` only when the cached result is
    /// missing or at least [`PROBE_TTL`] old at `now`.
    async fn reachable<F, Fut>(&self, now: Instant, probe: F) -> bool
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = bool>,
    {
        let mut last = self.last.lock().await;
        if let Some((at, ok)) = *last
            && now.saturating_duration_since(at) < PROBE_TTL
        {
            return ok;
        }
        let ok = probe().await;
        *last = Some((now, ok));
        ok
    }
}

/// The sticky storage failures, in the order `/readyz` lists them (D-N-35). A restart
/// clears them (D-R-6), so only these fail `/healthz`. Free disk and the object store are
/// not sticky.
fn sticky_failures(health: &StorageHealth) -> Vec<&'static str> {
    let mut failed = Vec::new();
    if health.spans_wal_failed {
        failed.push("spans WAL");
    }
    if health.scores_wal_failed {
        failed.push("scores WAL");
    }
    if health.objects_sync_failed {
        failed.push("object fsync");
    }
    if health.catalog_failed {
        failed.push("catalog");
    }
    failed
}

/// Process is alive, and no storage failure that only a restart clears is set (D-R-6).
/// Otherwise 503 `busy` naming them, so a liveness probe restarts the server (D-N-35).
pub(crate) async fn healthz(State(state): State<AppState>) -> Response {
    let failed = sticky_failures(&state.storage.health());
    if failed.is_empty() {
        axum::Json(Probe {
            status: "ok",
            restore_mode: false,
        })
        .into_response()
    } else {
        let detail = format!("restart needed: {}", failed.join(", "));
        error::problem_response(&Problem::new(ProblemCode::Busy, Some(detail)))
    }
}

/// The readiness checks that fail now, in `/readyz` order (D-N-30): empty when ready. The
/// object-store probe goes through the cache. Also read by the admin socket's `debug_info`.
pub(crate) async fn failed_checks(state: &AppState) -> Vec<&'static str> {
    let mut failed = Vec::new();
    if state.meta.ping().await.is_err() {
        failed.push("meta.db");
    }
    let health = state.storage.health();
    failed.extend(sticky_failures(&health));
    if health.disk_low {
        failed.push("free disk");
    }
    // Probed even when another check failed, so the cache stays warm.
    let storage = Arc::clone(&state.storage);
    let reachable = state
        .ready
        .reachable(Instant::now(), || async move {
            matches!(
                tokio::time::timeout(PROBE_TIMEOUT, storage.probe_objects()).await,
                Ok(Ok(()))
            )
        })
        .await;
    if !reachable {
        failed.push("object store");
    }
    failed
}

/// Every readiness check passes (D-N-30). Each failed check adds its name to the 503 detail.
pub(crate) async fn readyz(State(state): State<AppState>) -> Response {
    let failed = failed_checks(&state).await;
    if failed.is_empty() {
        axum::Json(Probe {
            status: "ready",
            restore_mode: state.config.restore_mode,
        })
        .into_response()
    } else {
        let problem = Problem::new(ProblemCode::Busy, Some(failed.join(", ")));
        error::problem_response(&problem)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use axum::http::Method;
    use bytesize::ByteSize;
    use cetra_storage::StorageHealth;
    use tokio::time::Instant;

    use super::{PROBE_TTL, ReadyCache, failed_checks, sticky_failures};
    use crate::api::common::ProblemCode;
    use crate::test_support::TestApp;

    fn clear() -> StorageHealth {
        StorageHealth {
            spans_wal_failed: false,
            scores_wal_failed: false,
            objects_sync_failed: false,
            catalog_failed: false,
            memtable_bytes: 0,
            memtable_hard_limit: 0,
            disk_low: false,
            disk_free_bytes: u64::MAX,
            disk_min_free_bytes: 0,
        }
    }

    #[test]
    fn sticky_failures_list_only_the_restart_clears_in_table_order() {
        assert!(sticky_failures(&clear()).is_empty());

        let catalog = StorageHealth {
            catalog_failed: true,
            ..clear()
        };
        assert_eq!(sticky_failures(&catalog), vec!["catalog"]);

        let two = StorageHealth {
            scores_wal_failed: true,
            objects_sync_failed: true,
            disk_low: true,
            ..clear()
        };
        // Disk is readiness-only (D-N-35), so it is not sticky.
        assert_eq!(sticky_failures(&two), vec!["scores WAL", "object fsync"]);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn healthy_server_is_alive() {
        let app = TestApp::new().await;
        let live = app.request(Method::GET, "/healthz").send().await;
        assert_eq!(live.status, 200);
        let body: serde_json::Value = live.json();
        assert_eq!(body, serde_json::json!({"status": "ok"}));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn low_disk_fails_readiness_but_not_liveness() {
        let app = TestApp::with_config(|config| {
            config.storage.min_free_bytes = ByteSize::b(u64::MAX);
        })
        .await;
        // The monitor measures every second; wait until it reports low.
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while !app.state.storage.health().disk_low {
            assert!(
                std::time::Instant::now() < deadline,
                "disk never reported low"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        let ready = app.request(Method::GET, "/readyz").send().await;
        assert_eq!(ready.status, 503);
        let problem = ready.problem();
        assert_eq!(problem.code, ProblemCode::Busy);
        assert!(
            problem
                .detail
                .as_deref()
                .unwrap_or("")
                .contains("free disk"),
            "{problem:?}"
        );

        let live = app.request(Method::GET, "/healthz").send().await;
        assert_eq!(live.status, 200);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn failed_checks_is_empty_when_ready() {
        let app = TestApp::new().await;
        assert!(failed_checks(&app.state).await.is_empty());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn failed_checks_names_low_disk() {
        let app = TestApp::with_config(|config| {
            config.storage.min_free_bytes = ByteSize::b(u64::MAX);
        })
        .await;
        // The monitor measures every second; wait until it reports low.
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while !app.state.storage.health().disk_low {
            assert!(
                std::time::Instant::now() < deadline,
                "disk never reported low"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(failed_checks(&app.state).await.contains(&"free disk"));
    }

    #[tokio::test(start_paused = true)]
    async fn probe_result_is_cached_for_ten_seconds() {
        let cache = ReadyCache::default();
        let calls = Arc::new(AtomicUsize::new(0));
        let probe = |calls: Arc<AtomicUsize>| {
            move || async move {
                calls.fetch_add(1, Ordering::SeqCst);
                true
            }
        };

        let t0 = Instant::now();
        assert!(cache.reachable(t0, probe(calls.clone())).await);
        assert!(cache.reachable(Instant::now(), probe(calls.clone())).await);
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        tokio::time::advance(Duration::from_secs(9)).await;
        assert!(cache.reachable(Instant::now(), probe(calls.clone())).await);
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        tokio::time::advance(Duration::from_secs(2)).await;
        assert!(cache.reachable(Instant::now(), probe(calls.clone())).await);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(PROBE_TTL, Duration::from_secs(10));
    }

    #[tokio::test(start_paused = true)]
    async fn failed_probe_is_cached_too() {
        let cache = ReadyCache::default();
        let calls = Arc::new(AtomicUsize::new(0));
        let failing = |calls: Arc<AtomicUsize>| {
            move || async move {
                calls.fetch_add(1, Ordering::SeqCst);
                false
            }
        };

        assert!(
            !cache
                .reachable(Instant::now(), failing(calls.clone()))
                .await
        );
        assert!(
            !cache
                .reachable(Instant::now(), failing(calls.clone()))
                .await
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn concurrent_requests_share_one_probe() {
        let cache = Arc::new(ReadyCache::default());
        let calls = Arc::new(AtomicUsize::new(0));
        let mut requests = Vec::new();
        for _ in 0..10 {
            let cache = Arc::clone(&cache);
            let calls = Arc::clone(&calls);
            requests.push(tokio::spawn(async move {
                cache
                    .reachable(Instant::now(), || async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        true
                    })
                    .await
            }));
        }
        for request in requests {
            assert!(request.await.unwrap());
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn unreachable_store_fails_readiness() {
        let app = TestApp::new().await;
        // A recent failed probe is cached, so `/readyz` reports it without probing.
        *app.state.ready.last.lock().await = Some((Instant::now(), false));

        let ready = app.request(Method::GET, "/readyz").send().await;
        assert_eq!(ready.status, 503);
        assert_eq!(ready.problem().detail.as_deref(), Some("object store"));
    }
}
