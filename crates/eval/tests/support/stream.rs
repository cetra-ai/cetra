//! A scripted HTTP/1.1 server for streamed responses: raw TCP, chunked transfer encoding.
//! Each connection serves the next scripted reply, then closes.
#![allow(clippy::expect_used)]

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use reqwest::Url;
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// Longest request head the server reads before it drops the connection.
const HEAD_MAX_BYTES: usize = 64 * 1024;

/// One step of a scripted body.
pub(crate) enum Step {
    /// One chunk, written and flushed. An empty vector writes nothing.
    Send(Vec<u8>),
    /// A pause before the next step.
    Sleep(Duration),
}

/// One scripted response.
pub(crate) struct StreamReply {
    /// HTTP status.
    pub(crate) status: u16,
    /// Wait before the status line is written.
    pub(crate) head_delay: Duration,
    /// The body, step by step.
    pub(crate) steps: Vec<Step>,
}

/// A request the server received.
#[derive(Clone)]
pub(crate) struct StreamRequest {
    /// Path without the query.
    pub(crate) path: String,
    /// Query without `?`.
    // Used by the chat stream tests.
    #[allow(dead_code)]
    pub(crate) query: Option<String>,
    /// Header names in lowercase, with their values.
    // Used by the chat stream tests.
    #[allow(dead_code)]
    pub(crate) headers: Vec<(String, String)>,
    /// Parsed JSON body, or `Null`.
    // Used by the chat stream tests.
    #[allow(dead_code)]
    pub(crate) body: Value,
}

/// A listener on `127.0.0.1:0` that serves scripted replies in order.
pub(crate) struct StreamServer {
    port: u16,
    queue: Arc<Mutex<VecDeque<StreamReply>>>,
    seen: Arc<Mutex<Vec<StreamRequest>>>,
}

impl StreamServer {
    /// Binds a listener and accepts connections in the background.
    pub(crate) async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind stream server");
        let port = listener.local_addr().expect("stream server port").port();
        let queue = Arc::new(Mutex::new(VecDeque::new()));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let accept_queue = Arc::clone(&queue);
        let accept_seen = Arc::clone(&seen);
        tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                let queue = Arc::clone(&accept_queue);
                let seen = Arc::clone(&accept_seen);
                tokio::spawn(serve(socket, queue, seen));
            }
        });
        Self { port, queue, seen }
    }

    /// `http://127.0.0.1:{port}{path}`.
    pub(crate) fn url(&self, path: &str) -> Url {
        Url::parse(&format!("http://127.0.0.1:{}{path}", self.port)).expect("stream url")
    }

    /// `http://127.0.0.1:{port}/v1` (OpenAI-compatible base).
    // Used by the chat stream tests.
    #[allow(dead_code)]
    pub(crate) fn base_v1(&self) -> Url {
        self.url("/v1")
    }

    /// `http://127.0.0.1:{port}` (Anthropic and Gemini base).
    // Used by the chat stream tests.
    #[allow(dead_code)]
    pub(crate) fn origin(&self) -> Url {
        self.url("")
    }

    /// Appends one scripted reply.
    pub(crate) fn enqueue(&self, reply: StreamReply) {
        self.queue.lock().expect("queue").push_back(reply);
    }

    /// Requests received so far, in arrival order.
    pub(crate) fn requests(&self) -> Vec<StreamRequest> {
        self.seen.lock().expect("seen").clone()
    }
}

/// `data: {data}\n\n`.
// Used by the chat stream tests.
#[allow(dead_code)]
pub(crate) fn sse(data: &str) -> Vec<u8> {
    format!("data: {data}\n\n").into_bytes()
}

/// `event: {event}\ndata: {data}\n\n`.
// Used by the chat stream tests.
#[allow(dead_code)]
pub(crate) fn sse_named(event: &str, data: &str) -> Vec<u8> {
    format!("event: {event}\ndata: {data}\n\n").into_bytes()
}

/// A 200 reply that sends each item as one chunk, with no pauses.
// Used by the chat stream tests.
#[allow(dead_code)]
pub(crate) fn ok_chunks(chunks: Vec<Vec<u8>>) -> StreamReply {
    StreamReply {
        status: 200,
        head_delay: Duration::ZERO,
        steps: chunks.into_iter().map(Step::Send).collect(),
    }
}

/// Serves one connection: reads the request, records it, writes the next reply.
async fn serve(
    mut socket: TcpStream,
    queue: Arc<Mutex<VecDeque<StreamReply>>>,
    seen: Arc<Mutex<Vec<StreamRequest>>>,
) {
    let Some(request) = read_request(&mut socket).await else {
        return;
    };
    seen.lock().expect("seen").push(request);
    let reply = queue
        .lock()
        .expect("queue")
        .pop_front()
        .unwrap_or(StreamReply {
            status: 500,
            head_delay: Duration::ZERO,
            steps: Vec::new(),
        });

    tokio::time::sleep(reply.head_delay).await;
    let head = format!(
        "HTTP/1.1 {} X\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n",
        reply.status
    );
    if socket.write_all(head.as_bytes()).await.is_err() {
        return;
    }
    for step in reply.steps {
        match step {
            Step::Send(bytes) => {
                if bytes.is_empty() {
                    continue;
                }
                let mut frame = format!("{:x}\r\n", bytes.len()).into_bytes();
                frame.extend_from_slice(&bytes);
                frame.extend_from_slice(b"\r\n");
                if socket.write_all(&frame).await.is_err() || socket.flush().await.is_err() {
                    return;
                }
            }
            Step::Sleep(pause) => tokio::time::sleep(pause).await,
        }
    }
    if socket.write_all(b"0\r\n\r\n").await.is_ok() {
        let _ = socket.shutdown().await;
    }
}

/// Reads one request: the head up to the blank line, then `content-length` body bytes.
/// `None` when the connection closes early, the head is too long, or it is not HTTP.
async fn read_request(socket: &mut TcpStream) -> Option<StreamRequest> {
    let mut buf = Vec::new();
    let head_end = loop {
        if let Some(position) = buf.windows(4).position(|window| window == b"\r\n\r\n") {
            break position;
        }
        if buf.len() > HEAD_MAX_BYTES {
            return None;
        }
        let mut chunk = [0_u8; 4096];
        let read = socket.read(&mut chunk).await.ok()?;
        if read == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..read]);
    };

    let head = std::str::from_utf8(buf.get(..head_end)?).ok()?;
    let mut lines = head.split("\r\n");
    let target = lines.next()?.split(' ').nth(1)?;
    let (path, query) = match target.split_once('?') {
        Some((path, query)) => (path.to_owned(), Some(query.to_owned())),
        None => (target.to_owned(), None),
    };
    let headers: Vec<(String, String)> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_owned()))
        .collect();
    let length = headers
        .iter()
        .find(|(name, _)| name == "content-length")
        .and_then(|(_, value)| value.parse::<usize>().ok())
        .unwrap_or(0);

    let body_start = head_end + 4;
    while buf.len() < body_start + length {
        let mut chunk = [0_u8; 4096];
        let read = socket.read(&mut chunk).await.ok()?;
        if read == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..read]);
    }
    let available = buf.get(body_start..).unwrap_or_default();
    let body =
        serde_json::from_slice(&available[..length.min(available.len())]).unwrap_or(Value::Null);
    Some(StreamRequest {
        path,
        query,
        headers,
        body,
    })
}
