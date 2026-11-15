//! `/healthz` and `/readyz` (spec 09, spec 12).
//!
//! spec 12 also lists object-store reachability and free disk. Those checks
//! arrive with the tracks that own them.
#![cfg_attr(not(test), allow(dead_code))]

use axum::extract::State;
use axum::response::{IntoResponse, Response};
use serde::Serialize;

use super::AppState;
use super::error;
use crate::api::common::{Problem, ProblemCode};

#[derive(Serialize)]
struct Probe {
    status: &'static str,
    /// Shown only in restore mode (N M3, D-N-25).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    restore_mode: bool,
}

/// Process is alive.
pub(crate) async fn healthz() -> Response {
    axum::Json(Probe {
        status: "ok",
        restore_mode: false,
    })
    .into_response()
}

/// `meta.db` answers and neither WAL has failed.
pub(crate) async fn readyz(State(state): State<AppState>) -> Response {
    let mut failed = Vec::new();
    if state.meta.ping().await.is_err() {
        failed.push("meta.db");
    }
    let health = state.storage.health();
    if health.spans_wal_failed {
        failed.push("spans WAL");
    }
    if health.scores_wal_failed {
        failed.push("scores WAL");
    }
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
    use axum::http::Method;

    use crate::test_support::TestApp;

    #[tokio::test]
    async fn readyz_reports_restore_mode() {
        let app = TestApp::with_config(|config| config.restore_mode = true).await;
        let ready = app.request(Method::GET, "/readyz").send().await;
        assert_eq!(ready.status, 200);
        let body: serde_json::Value = ready.json();
        assert_eq!(
            body,
            serde_json::json!({"status": "ready", "restore_mode": true})
        );

        let plain = TestApp::new().await;
        let ready = plain.request(Method::GET, "/readyz").send().await;
        let body: serde_json::Value = ready.json();
        assert_eq!(body, serde_json::json!({"status": "ready"}));
    }
}
