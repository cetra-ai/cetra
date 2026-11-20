//! An in-process OpenAI-compatible provider for server tests (I1.6).
//!
//! `crates/eval/tests/support/mock.rs` is the model; this one is smaller and
//! lives in the server crate so the route tests can use it.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use serde_json::Value;

/// One scripted reply.
pub(crate) struct Reply {
    /// HTTP status.
    pub(crate) status: u16,
    /// JSON body, serialized as-is.
    pub(crate) body: Value,
}

/// Serves `POST /v1/chat/completions` and `POST /v1/embeddings` on `127.0.0.1:0`. Each route
/// replies from its own script, in order; the last reply repeats. Records each request's
/// headers and JSON body per route. Aborts its task on drop.
pub(crate) struct MockLlm {
    addr: SocketAddr,
    state: Arc<MockState>,
    task: tokio::task::JoinHandle<()>,
}

struct MockState {
    /// Chat replies still to serve. The last one repeats.
    replies: Arc<Mutex<Vec<Reply>>>,
    /// Each chat request's headers and parsed JSON body, in arrival order.
    requests: Arc<Mutex<Vec<(HeaderMap, Value)>>>,
    /// Embedding replies still to serve. The last one repeats.
    embeddings: Arc<Mutex<Vec<Reply>>>,
    /// Each embedding request's headers and parsed JSON body, in arrival order.
    embedding_requests: Arc<Mutex<Vec<(HeaderMap, Value)>>>,
    /// Runs on each request before its reply (a test moves the clock while a call is in flight).
    hook: Box<dyn Fn() + Send + Sync>,
    /// How long each reply waits before it is sent.
    delay: Duration,
}

impl MockLlm {
    /// Binds the listener and serves it in the background.
    pub(crate) async fn start(replies: Vec<Reply>) -> Self {
        Self::start_with_embeddings(replies, Vec::new()).await
    }

    /// [`Self::start`], and each reply is sent `delay` after its request arrives.
    pub(crate) async fn start_with_delay(replies: Vec<Reply>, delay: Duration) -> Self {
        Self::spawn(replies, Vec::new(), Box::new(|| {}), delay).await
    }

    /// [`Self::start`], and `hook` runs on each request before its reply.
    pub(crate) async fn start_with_hook(
        replies: Vec<Reply>,
        hook: impl Fn() + Send + Sync + 'static,
    ) -> Self {
        Self::spawn(replies, Vec::new(), Box::new(hook), Duration::ZERO).await
    }

    /// [`Self::start`], with `embeddings` as the replies of `POST /v1/embeddings`.
    pub(crate) async fn start_with_embeddings(replies: Vec<Reply>, embeddings: Vec<Reply>) -> Self {
        Self::spawn(replies, embeddings, Box::new(|| {}), Duration::ZERO).await
    }

    async fn spawn(
        replies: Vec<Reply>,
        embeddings: Vec<Reply>,
        hook: Box<dyn Fn() + Send + Sync>,
        delay: Duration,
    ) -> Self {
        let state = Arc::new(MockState {
            replies: Arc::new(Mutex::new(replies)),
            requests: Arc::new(Mutex::new(Vec::new())),
            embeddings: Arc::new(Mutex::new(embeddings)),
            embedding_requests: Arc::new(Mutex::new(Vec::new())),
            hook,
            delay,
        });
        let router = Router::new()
            .route("/v1/chat/completions", post(handle))
            .route("/v1/embeddings", post(handle_embedding))
            .with_state(Arc::clone(&state));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind the mock provider");
        let addr = listener.local_addr().expect("the mock provider's address");
        let task = tokio::spawn(async move {
            // The MockLlm aborts this task when it is dropped.
            let _ = axum::serve(listener, router).await;
        });
        Self { addr, state, task }
    }

    /// `http://127.0.0.1:<port>/v1`, a connection's `base_url`.
    pub(crate) fn base_url(&self) -> String {
        format!("http://{}/v1", self.addr)
    }

    /// Chat requests received so far, in arrival order.
    pub(crate) fn requests(&self) -> Vec<(HeaderMap, Value)> {
        self.state
            .requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Embedding requests received so far, in arrival order. Each body has `model`, `input`,
    /// `encoding_format` and, when the caller asked for one, `dimensions`.
    pub(crate) fn embedding_requests(&self) -> Vec<(HeaderMap, Value)> {
        self.state
            .embedding_requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl Drop for MockLlm {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn handle(State(state): State<Arc<MockState>>, headers: HeaderMap, body: Bytes) -> Response {
    serve(&state, &state.replies, &state.requests, headers, &body).await
}

async fn handle_embedding(
    State(state): State<Arc<MockState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    serve(
        &state,
        &state.embeddings,
        &state.embedding_requests,
        headers,
        &body,
    )
    .await
}

/// Records one request, then sends the next reply of `queue`.
async fn serve(
    state: &MockState,
    queue: &Mutex<Vec<Reply>>,
    log: &Mutex<Vec<(HeaderMap, Value)>>,
    headers: HeaderMap,
    body: &[u8],
) -> Response {
    let parsed = serde_json::from_slice(body).unwrap_or(Value::Null);
    log.lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push((headers, parsed));
    (state.hook)();
    let reply = {
        let mut replies = queue.lock().unwrap_or_else(PoisonError::into_inner);
        // Replies are served in order; the last one repeats.
        match replies.len() {
            0 => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    r#"{"error":{"message":"no scripted reply"}}"#,
                )
                    .into_response();
            }
            1 => clone_last(&replies),
            _ => replies.remove(0),
        }
    };
    tokio::time::sleep(state.delay).await;
    let status = StatusCode::from_u16(reply.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let bytes = serde_json::to_vec(&reply.body).unwrap_or_default();
    (status, bytes).into_response()
}

/// Clones the reply that repeats.
fn clone_last(replies: &[Reply]) -> Reply {
    let reply = replies.last().expect("the caller checked for one");
    Reply {
        status: reply.status,
        body: reply.body.clone(),
    }
}

/// A 200 chat completion whose message content is `content`, with usage 10 in
/// and 3 out.
pub(crate) fn completion(content: &str) -> Reply {
    Reply {
        status: 200,
        body: serde_json::json!({
            "id": "chatcmpl-test",
            "object": "chat.completion",
            "model": "gpt-test",
            "choices": [{
                "index": 0,
                "finish_reason": "stop",
                "message": {"role": "assistant", "content": content}
            }],
            "usage": {"prompt_tokens": 10, "completion_tokens": 3, "total_tokens": 13}
        }),
    }
}

/// A 200 embedding of `vector`; its dimension is the vector's length. The model is
/// `text-embedding-test`, with usage 5 in.
pub(crate) fn embedding(vector: &[f32]) -> Reply {
    Reply {
        status: 200,
        body: serde_json::json!({
            "object": "list",
            "data": [{"object": "embedding", "index": 0, "embedding": vector}],
            "model": "text-embedding-test",
            "usage": {"prompt_tokens": 5, "total_tokens": 5}
        }),
    }
}

/// An error reply with `status` and an OpenAI-style error body carrying `message`.
pub(crate) fn error(status: u16, message: &str) -> Reply {
    Reply {
        status,
        body: serde_json::json!({"error": {"message": message}}),
    }
}
