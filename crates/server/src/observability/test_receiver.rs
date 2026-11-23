//! A local HTTP receiver for the telemetry tests (crash reports; the heartbeat's tests keep
//! their own copy in `heartbeat.rs`). Serves `POST <path>` on `127.0.0.1:0`: waits `delay`,
//! records the request, answers `status`, with `Location: /elsewhere` when `status` is a 3xx.
//! `GET` and `POST /elsewhere` count hits and answer 200. Aborts its task on drop.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};

/// A running receiver.
#[derive(Debug)]
pub(crate) struct Receiver {
    /// `http://127.0.0.1:<port>`, without a trailing slash.
    pub(crate) base: String,
    requests: Arc<Mutex<Vec<(HeaderMap, Bytes)>>>,
    elsewhere: Arc<AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
}

#[derive(Clone, Debug)]
struct ReceiverState {
    status: u16,
    delay: Duration,
    requests: Arc<Mutex<Vec<(HeaderMap, Bytes)>>>,
    elsewhere: Arc<AtomicUsize>,
}

impl Receiver {
    /// Serves `POST path` (for example `/v1/crash`).
    pub(crate) async fn start(path: &str, status: u16, delay: Duration) -> Self {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let elsewhere = Arc::new(AtomicUsize::new(0));
        let state = ReceiverState {
            status,
            delay,
            requests: Arc::clone(&requests),
            elsewhere: Arc::clone(&elsewhere),
        };
        let router = Router::new()
            .route(path, post(record))
            .route("/elsewhere", get(other).post(other))
            .with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
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

    /// The requests received so far, in order.
    pub(crate) fn requests(&self) -> Vec<(HeaderMap, Bytes)> {
        self.requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The request bodies received so far, in order.
    pub(crate) fn bodies(&self) -> Vec<Bytes> {
        self.requests().into_iter().map(|(_, body)| body).collect()
    }

    /// Hits on `/elsewhere` (a followed redirect).
    pub(crate) fn elsewhere_hits(&self) -> usize {
        self.elsewhere.load(Ordering::SeqCst)
    }
}

impl Drop for Receiver {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn record(State(state): State<ReceiverState>, headers: HeaderMap, body: Bytes) -> Response {
    tokio::time::sleep(state.delay).await;
    state
        .requests
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push((headers, body));
    let status = StatusCode::from_u16(state.status).unwrap();
    if status.is_redirection() {
        (status, [(axum::http::header::LOCATION, "/elsewhere")]).into_response()
    } else {
        status.into_response()
    }
}

async fn other(State(state): State<ReceiverState>) -> StatusCode {
    state.elsewhere.fetch_add(1, Ordering::SeqCst);
    StatusCode::OK
}
