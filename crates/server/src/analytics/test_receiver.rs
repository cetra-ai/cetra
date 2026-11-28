//! An in-process PostHog stand-in for tests: records `POST /batch/` bodies.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};

/// Serves `POST /batch/` on `127.0.0.1:0` and answers `status` (with
/// `Location: /elsewhere` for a 3xx). `/elsewhere` counts hits. Aborts its task on drop.
pub(crate) struct Receiver {
    pub(crate) base: String,
    requests: Arc<Mutex<Vec<(HeaderMap, Bytes)>>>,
    pub(crate) elsewhere: Arc<AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
}

#[derive(Clone)]
struct ReceiverState {
    status: u16,
    requests: Arc<Mutex<Vec<(HeaderMap, Bytes)>>>,
    elsewhere: Arc<AtomicUsize>,
}

impl Receiver {
    pub(crate) async fn start(status: u16) -> Self {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let elsewhere = Arc::new(AtomicUsize::new(0));
        let state = ReceiverState {
            status,
            requests: Arc::clone(&requests),
            elsewhere: Arc::clone(&elsewhere),
        };
        let router = Router::new()
            .route("/batch/", post(batch))
            .route("/elsewhere", get(other).post(other))
            .with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("test listener binds");
        let addr = listener.local_addr().expect("test listener has an address");
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        Self {
            base: format!("http://{addr}"),
            requests,
            elsewhere,
            task,
        }
    }

    /// Every request so far.
    pub(crate) fn requests(&self) -> Vec<(HeaderMap, Bytes)> {
        self.requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The JSON bodies so far.
    pub(crate) fn bodies(&self) -> Vec<serde_json::Value> {
        self.requests()
            .iter()
            .map(|(_, body)| serde_json::from_slice(body).expect("the body is JSON"))
            .collect()
    }

    /// Waits up to `within` for at least `count` requests, then returns the bodies.
    pub(crate) async fn wait_for(&self, count: usize, within: Duration) -> Vec<serde_json::Value> {
        let deadline = tokio::time::Instant::now() + within;
        while self.requests().len() < count && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        self.bodies()
    }
}

impl Drop for Receiver {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn batch(State(state): State<ReceiverState>, headers: HeaderMap, body: Bytes) -> Response {
    state
        .requests
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push((headers, body));
    let status = StatusCode::from_u16(state.status).expect("the test status is valid");
    if status.is_redirection() {
        (status, [(header::LOCATION, "/elsewhere")]).into_response()
    } else {
        status.into_response()
    }
}

async fn other(State(state): State<ReceiverState>) -> StatusCode {
    state.elsewhere.fetch_add(1, Ordering::SeqCst);
    StatusCode::OK
}
