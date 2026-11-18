//! The HTTP side of the generator: the backfill check and the batches sent to
//! `POST /v1/traces` (DO5 steps 5 and 6). The key is never logged or printed.

use std::cell::Cell;
use std::fmt;
use std::time::Duration;

use chrono::{DateTime, Utc};
use reqwest::{Client, StatusCode, header};
use serde::Deserialize;
use tracing::{info, warn};

use crate::error::DemoError;
use crate::otlp::ExportRequest;
use crate::templates::Trace;

/// The most traces in one `POST /v1/traces`.
pub(crate) const MAX_BATCH_TRACES: usize = 500;
/// The most JSON bytes in one `POST /v1/traces`. A cell refuses a body over its
/// `ingest.max_request_bytes` (10 MiB by default), so this stays under it.
pub(crate) const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;
/// The bytes of a request body around its traces: `{"resourceSpans":[` and `]}`.
const BODY_ENVELOPE_BYTES: usize = 20;
/// The wait after a 429 or 503 that has no usable `Retry-After`.
const DEFAULT_RETRY: Duration = Duration::from_secs(5);
/// A `Retry-After` is clamped to this band: `0` must not spin, a huge value must not stall the run.
const MIN_RETRY: Duration = Duration::from_secs(1);
const MAX_RETRY: Duration = Duration::from_secs(300);
/// The wait after a 402 (the units cap) before the same batch is sent again.
const UNITS_CAP_WAIT: Duration = Duration::from_secs(600);
/// The first wait after a transient failure (a transport error or a 5xx other than 503).
/// It doubles up to [`BACKOFF_MAX`] and goes back to this after a success.
const BACKOFF_FIRST: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(300);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

/// Talks to one cell with one project key.
pub(crate) struct Sink {
    http: Client,
    base: String,
    key: String,
    /// The wait the next transient failure gets.
    backoff: Cell<Duration>,
}

impl fmt::Debug for Sink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Sink")
            .field("base", &self.base)
            .finish_non_exhaustive()
    }
}

#[derive(Deserialize)]
struct TracePage {
    data: Vec<TraceStart>,
}

#[derive(Deserialize)]
struct TraceStart {
    start_time: DateTime<Utc>,
}

impl Sink {
    pub(crate) fn new(base: &str, key: String) -> Result<Self, DemoError> {
        Ok(Self {
            http: Client::builder().timeout(REQUEST_TIMEOUT).build()?,
            base: base.trim_end_matches('/').to_owned(),
            key,
            backoff: Cell::new(BACKOFF_FIRST),
        })
    }

    /// When the newest trace in `[from, to)` starts, or `None` when there is none.
    pub(crate) async fn newest_trace_start(
        &self,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Option<DateTime<Utc>>, DemoError> {
        let (from, to) = (from.to_rfc3339(), to.to_rfc3339());
        let response = self
            .http
            .get(format!("{}/api/v1/projects/current/traces", self.base))
            .bearer_auth(&self.key)
            .query(&[
                ("limit", "1"),
                ("sort", "start_time"),
                ("order", "desc"),
                ("from", from.as_str()),
                ("to", to.as_str()),
            ])
            .send()
            .await?;
        let status = response.status();
        if !status.is_success() {
            return Err(DemoError::Rejected {
                status: status.as_u16(),
            });
        }
        let page: TracePage = response.json().await?;
        Ok(page.data.first().map(|trace| trace.start_time))
    }

    /// Sends `traces` in requests of at most [`MAX_BATCH_TRACES`] traces and
    /// [`MAX_BODY_BYTES`] bytes each. A trace that alone is over the byte cap is skipped.
    pub(crate) async fn send(&self, traces: &[Trace]) -> Result<(), DemoError> {
        let mut batch: Vec<&Trace> = Vec::new();
        let mut batch_bytes = BODY_ENVELOPE_BYTES;
        let mut sent = 0_usize;
        let mut requests = 0_usize;
        for trace in traces {
            let size = encoded_len(trace)?;
            if size + BODY_ENVELOPE_BYTES > MAX_BODY_BYTES {
                warn!(
                    bytes = size,
                    "a trace is over the request size cap, skipped"
                );
                continue;
            }
            if !batch.is_empty()
                && (batch.len() == MAX_BATCH_TRACES || batch_bytes + 1 + size > MAX_BODY_BYTES)
            {
                self.post(&batch).await?;
                requests += 1;
                sent += batch.len();
                batch.clear();
                batch_bytes = BODY_ENVELOPE_BYTES;
            }
            batch_bytes += usize::from(!batch.is_empty()) + size;
            batch.push(trace);
        }
        if !batch.is_empty() {
            self.post(&batch).await?;
            requests += 1;
            sent += batch.len();
        }
        if sent > 0 {
            info!(traces = sent, requests, "sent traces");
        }
        Ok(())
    }

    /// One batch. A 429 or 503 waits for `Retry-After`, a 402 waits ten minutes, a
    /// transport error or a 5xx other than 503 waits with exponential backoff, and all
    /// of them send the same batch again. Any other failure ends the run.
    async fn post(&self, chunk: &[&Trace]) -> Result<(), DemoError> {
        let body = ExportRequest {
            resource_spans: chunk.iter().flat_map(|trace| trace.iter()).collect(),
        };
        loop {
            let response = match self
                .http
                .post(format!("{}/v1/traces", self.base))
                .bearer_auth(&self.key)
                .json(&body)
                .send()
                .await
            {
                Ok(response) => response,
                Err(_) => {
                    let wait = self.next_backoff();
                    warn!(
                        wait_s = wait.as_secs(),
                        "the request did not complete, retrying"
                    );
                    tokio::time::sleep(wait).await;
                    continue;
                }
            };
            let status = response.status();
            if status.is_success() {
                self.backoff.set(BACKOFF_FIRST);
                return Ok(());
            }
            match status {
                StatusCode::PAYMENT_REQUIRED => {
                    warn!("the org's units cap is reached, waiting 10 minutes");
                    tokio::time::sleep(UNITS_CAP_WAIT).await;
                }
                StatusCode::TOO_MANY_REQUESTS | StatusCode::SERVICE_UNAVAILABLE => {
                    let wait = retry_after(&response);
                    warn!(
                        status = status.as_u16(),
                        wait_s = wait.as_secs(),
                        "the server is busy, retrying"
                    );
                    tokio::time::sleep(wait).await;
                }
                other if other.is_server_error() => {
                    let wait = self.next_backoff();
                    warn!(
                        status = other.as_u16(),
                        wait_s = wait.as_secs(),
                        "the server failed, retrying"
                    );
                    tokio::time::sleep(wait).await;
                }
                other => {
                    return Err(DemoError::Rejected {
                        status: other.as_u16(),
                    });
                }
            }
        }
    }

    /// The wait for the next transient failure; the one after it is twice as long, up to the cap.
    fn next_backoff(&self) -> Duration {
        let wait = self.backoff.get();
        self.backoff.set(wait.saturating_mul(2).min(BACKOFF_MAX));
        wait
    }
}

/// The bytes a trace adds to a request body: its JSON array without the brackets.
/// Traces in one body are joined with one comma each (see [`BODY_ENVELOPE_BYTES`]).
fn encoded_len(trace: &Trace) -> Result<usize, DemoError> {
    Ok(serde_json::to_vec(trace)?.len().saturating_sub(2))
}

fn retry_after(response: &reqwest::Response) -> Duration {
    retry_wait(
        response
            .headers()
            .get(header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok()),
    )
}

/// The wait a `Retry-After` value asks for, clamped to [`MIN_RETRY`, `MAX_RETRY`].
fn retry_wait(value: Option<&str>) -> Duration {
    value
        .and_then(|text| text.trim().parse::<u64>().ok())
        .map_or(DEFAULT_RETRY, Duration::from_secs)
        .clamp(MIN_RETRY, MAX_RETRY)
}

/// Fake cell for the tests of this crate: records what it receives and answers as told.
#[cfg(test)]
pub(crate) mod fake_cell {
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    use axum::body::Bytes;
    use axum::extract::{DefaultBodyLimit, RawQuery, State};
    use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
    use axum::response::{IntoResponse, Response};
    use axum::routing::{get, post};
    use axum::{Json, Router};
    use serde_json::{Value, json};

    #[derive(Default)]
    pub(crate) struct CellState {
        pub(crate) posts: Mutex<Vec<Seen>>,
        /// Answers of the next posts, in order, before the cell accepts: a status and an optional `Retry-After`.
        pub(crate) refusals: Mutex<VecDeque<(u16, Option<&'static str>)>>,
        /// The `start_time` of the newest trace the list answers with, when there is one.
        pub(crate) newest_start: Mutex<Option<String>>,
        pub(crate) last_query: Mutex<Option<String>>,
    }

    #[derive(Debug, Clone)]
    pub(crate) struct Seen {
        pub(crate) authorization: Option<String>,
        pub(crate) content_type: Option<String>,
        pub(crate) traces: usize,
        pub(crate) bytes: usize,
    }

    async fn ingest(
        State(cell): State<Arc<CellState>>,
        headers: HeaderMap,
        body: Bytes,
    ) -> Response {
        let read = |name: header::HeaderName| {
            headers
                .get(name)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned)
        };
        let value: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
        cell.posts.lock().expect("lock").push(Seen {
            authorization: read(header::AUTHORIZATION),
            content_type: read(header::CONTENT_TYPE),
            traces: value["resourceSpans"].as_array().map_or(0, Vec::len),
            bytes: body.len(),
        });
        let refusal = cell.refusals.lock().expect("lock").pop_front();
        match refusal {
            Some((status, retry_after)) => {
                let mut response = StatusCode::from_u16(status)
                    .expect("a status")
                    .into_response();
                if let Some(value) = retry_after {
                    response
                        .headers_mut()
                        .insert(header::RETRY_AFTER, HeaderValue::from_static(value));
                }
                response
            }
            None => Json(json!({})).into_response(),
        }
    }

    async fn trace_page(
        State(cell): State<Arc<CellState>>,
        RawQuery(query): RawQuery,
    ) -> Json<Value> {
        *cell.last_query.lock().expect("lock") = query;
        let data: Vec<Value> = cell
            .newest_start
            .lock()
            .expect("lock")
            .iter()
            .map(|start| json!({ "start_time": start }))
            .collect();
        Json(json!({ "data": data, "next_cursor": null }))
    }

    /// Starts the fake cell on a free loopback port and returns its base URL.
    pub(crate) async fn start(cell: Arc<CellState>) -> String {
        let app = Router::new()
            .route("/v1/traces", post(ingest))
            .route("/api/v1/projects/current/traces", get(trace_page))
            .layer(DefaultBodyLimit::max(64 * 1024 * 1024))
            .with_state(cell);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind a loopback port");
        let addr = listener.local_addr().expect("local address");
        tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });
        format!("http://{addr}")
    }
}

#[cfg(test)]
mod tests {
    use super::fake_cell::{CellState, start};
    use super::*;
    use serde_json::json;
    use std::sync::Arc;

    fn small_trace() -> Trace {
        serde_json::from_value(json!([{"scopeSpans": [{"spans": [{
            "traceId": "00000000000000000000000000000001",
            "spanId": "0000000000000001",
            "startTimeUnixNano": "1",
            "endTimeUnixNano": "2"
        }]}]}]))
        .expect("a valid trace")
    }

    /// A trace whose one attribute is a string of `bytes` bytes.
    fn blob_trace(bytes: usize) -> Trace {
        serde_json::from_value(json!([{"scopeSpans": [{"spans": [{
            "traceId": "00000000000000000000000000000002",
            "spanId": "0000000000000002",
            "startTimeUnixNano": "1",
            "endTimeUnixNano": "2",
            "attributes": [{"key": "blob", "value": {"stringValue": "a".repeat(bytes)}}]
        }]}]}]))
        .expect("a valid trace")
    }

    #[tokio::test]
    async fn one_batch_carries_the_bearer_key_and_json() {
        let cell = Arc::new(CellState::default());
        let base = start(cell.clone()).await;
        let sink = Sink::new(&base, "sk-cetra-test".to_owned()).expect("client");
        sink.send(&[small_trace()]).await.expect("sent");
        let posts = cell.posts.lock().expect("lock").clone();
        assert_eq!(posts.len(), 1);
        assert_eq!(
            posts[0].authorization.as_deref(),
            Some("Bearer sk-cetra-test")
        );
        assert!(
            posts[0]
                .content_type
                .as_deref()
                .is_some_and(|value| value.starts_with("application/json"))
        );
        assert_eq!(posts[0].traces, 1);
    }

    #[tokio::test]
    async fn a_busy_answer_is_retried_with_the_same_batch() {
        let cell = Arc::new(CellState::default());
        cell.refusals
            .lock()
            .expect("lock")
            .push_back((429, Some("0")));
        let base = start(cell.clone()).await;
        let sink = Sink::new(&base, "sk-cetra-test".to_owned()).expect("client");
        sink.send(&[small_trace(), small_trace()])
            .await
            .expect("sent");
        let posts = cell.posts.lock().expect("lock").clone();
        assert_eq!(posts.len(), 2, "one refused, one accepted");
        assert!(posts.iter().all(|seen| seen.traces == 2));
    }

    #[tokio::test]
    async fn a_502_then_200_is_retried_with_the_same_batch() {
        let cell = Arc::new(CellState::default());
        cell.refusals.lock().expect("lock").push_back((502, None));
        let base = start(cell.clone()).await;
        let sink = Sink::new(&base, "sk-cetra-test".to_owned()).expect("client");
        sink.send(&[small_trace()]).await.expect("sent");
        let posts = cell.posts.lock().expect("lock").clone();
        assert_eq!(posts.len(), 2, "one failed, one accepted");
        assert!(posts.iter().all(|seen| seen.traces == 1));
    }

    #[tokio::test]
    async fn a_success_resets_the_backoff() {
        let cell = Arc::new(CellState::default());
        let base = start(cell.clone()).await;
        let sink = Sink::new(&base, "sk-cetra-test".to_owned()).expect("client");
        sink.backoff.set(Duration::from_secs(8));
        sink.send(&[small_trace()]).await.expect("sent");
        assert_eq!(sink.backoff.get(), BACKOFF_FIRST);
    }

    #[tokio::test]
    async fn a_refusal_other_than_429_402_and_5xx_ends_the_run() {
        let cell = Arc::new(CellState::default());
        cell.refusals.lock().expect("lock").push_back((400, None));
        let base = start(cell.clone()).await;
        let sink = Sink::new(&base, "sk-cetra-test".to_owned()).expect("client");
        let result = sink.send(&[small_trace()]).await;
        assert!(matches!(result, Err(DemoError::Rejected { status: 400 })));
        assert_eq!(cell.posts.lock().expect("lock").len(), 1);
    }

    #[test]
    fn backoff_doubles_to_the_cap() {
        let sink = Sink::new("http://127.0.0.1:9", "sk-cetra-test".to_owned()).expect("client");
        let waits: Vec<u64> = (0..10).map(|_| sink.next_backoff().as_secs()).collect();
        assert_eq!(waits, vec![1, 2, 4, 8, 16, 32, 64, 128, 256, 300]);
    }

    #[test]
    fn retry_after_is_clamped_and_a_bad_value_gets_the_default() {
        assert_eq!(retry_wait(None), DEFAULT_RETRY);
        assert_eq!(retry_wait(Some("soon")), DEFAULT_RETRY);
        assert_eq!(retry_wait(Some("0")), Duration::from_secs(1));
        assert_eq!(retry_wait(Some(" 30 ")), Duration::from_secs(30));
        assert_eq!(retry_wait(Some("100000")), Duration::from_secs(300));
    }

    #[test]
    fn the_body_size_is_the_envelope_plus_each_trace() {
        let traces = [small_trace(), blob_trace(1_000)];
        let body = ExportRequest {
            resource_spans: traces.iter().flat_map(|trace| trace.iter()).collect(),
        };
        let real = serde_json::to_vec(&body).expect("encodes").len();
        let counted = BODY_ENVELOPE_BYTES
            + encoded_len(&traces[0]).expect("sized")
            + 1
            + encoded_len(&traces[1]).expect("sized");
        assert_eq!(real, counted);
    }

    #[tokio::test]
    async fn batches_split_before_the_byte_cap() {
        let cell = Arc::new(CellState::default());
        let base = start(cell.clone()).await;
        let sink = Sink::new(&base, "sk-cetra-test".to_owned()).expect("client");
        let mib = 1024 * 1024;
        let traces: Vec<Trace> = (0..3).map(|_| blob_trace(3 * mib)).collect();
        sink.send(&traces).await.expect("sent");
        let posts = cell.posts.lock().expect("lock").clone();
        let sizes: Vec<usize> = posts.iter().map(|seen| seen.traces).collect();
        assert_eq!(sizes, vec![2, 1]);
        assert!(posts.iter().all(|seen| seen.bytes <= MAX_BODY_BYTES));
    }

    #[tokio::test]
    async fn a_trace_over_the_byte_cap_is_skipped_and_the_rest_is_sent() {
        let cell = Arc::new(CellState::default());
        let base = start(cell.clone()).await;
        let sink = Sink::new(&base, "sk-cetra-test".to_owned()).expect("client");
        let mib = 1024 * 1024;
        sink.send(&[blob_trace(9 * mib), small_trace()])
            .await
            .expect("sent");
        let posts = cell.posts.lock().expect("lock").clone();
        let sizes: Vec<usize> = posts.iter().map(|seen| seen.traces).collect();
        assert_eq!(sizes, vec![1], "only the small trace is sent");
        assert!(posts.iter().all(|seen| seen.bytes <= MAX_BODY_BYTES));
    }

    #[tokio::test]
    async fn batches_hold_at_most_500_traces() {
        let cell = Arc::new(CellState::default());
        let base = start(cell.clone()).await;
        let sink = Sink::new(&base, "sk-cetra-test".to_owned()).expect("client");
        let traces: Vec<Trace> = (0..1_200).map(|_| small_trace()).collect();
        sink.send(&traces).await.expect("sent");
        let sizes: Vec<usize> = cell
            .posts
            .lock()
            .expect("lock")
            .iter()
            .map(|seen| seen.traces)
            .collect();
        assert_eq!(sizes, vec![500, 500, 200]);
    }

    #[tokio::test]
    async fn the_newest_trace_query_reads_one_item_newest_first() {
        let cell = Arc::new(CellState::default());
        let base = start(cell.clone()).await;
        let sink = Sink::new(&base, "sk-cetra-test".to_owned()).expect("client");
        let now = Utc::now();
        let from = now - chrono::Duration::days(30);
        assert!(
            sink.newest_trace_start(from, now)
                .await
                .expect("checked")
                .is_none()
        );
        let query = cell
            .last_query
            .lock()
            .expect("lock")
            .clone()
            .unwrap_or_default();
        assert!(query.contains("limit=1"), "query was {query}");
        assert!(query.contains("sort=start_time"), "query was {query}");
        assert!(query.contains("order=desc"), "query was {query}");
        assert!(query.contains("from="), "query was {query}");
        *cell.newest_start.lock().expect("lock") =
            Some("2026-11-08T12:00:00.000000001Z".to_owned());
        let newest = sink
            .newest_trace_start(from, now)
            .await
            .expect("checked")
            .expect("a trace");
        assert_eq!(newest.to_rfc3339(), "2026-11-08T12:00:00.000000001+00:00");
    }
}
