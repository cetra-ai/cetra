//! The HTTP side of the generator: the backfill check and the batches sent to
//! `POST /v1/traces` (DO5 steps 5 and 6). The key is never logged or printed.

use std::fmt;
use std::time::Duration;

use chrono::{DateTime, Utc};
use reqwest::{Client, StatusCode, header};
use serde::Deserialize;
use serde::de::IgnoredAny;
use tracing::{info, warn};

use crate::error::DemoError;
use crate::otlp::ExportRequest;
use crate::templates::Trace;

/// The most traces in one `POST /v1/traces`.
pub(crate) const MAX_BATCH_TRACES: usize = 500;
/// The wait after a 429 or 503 that has no usable `Retry-After`.
const DEFAULT_RETRY: Duration = Duration::from_secs(5);
/// The wait after a 402 (the units cap) before the same batch is sent again.
const UNITS_CAP_WAIT: Duration = Duration::from_secs(600);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

/// Talks to one cell with one project key.
pub(crate) struct Sink {
    http: Client,
    base: String,
    key: String,
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
    data: Vec<IgnoredAny>,
}

impl Sink {
    pub(crate) fn new(base: &str, key: String) -> Result<Self, DemoError> {
        Ok(Self {
            http: Client::builder().timeout(REQUEST_TIMEOUT).build()?,
            base: base.trim_end_matches('/').to_owned(),
            key,
        })
    }

    /// Whether the project has a trace that starts between `from` and `to`.
    pub(crate) async fn has_trace_between(
        &self,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<bool, DemoError> {
        let (from, to) = (from.to_rfc3339(), to.to_rfc3339());
        let response = self
            .http
            .get(format!("{}/api/v1/projects/current/traces", self.base))
            .bearer_auth(&self.key)
            .query(&[("limit", "1"), ("from", from.as_str()), ("to", to.as_str())])
            .send()
            .await?;
        let status = response.status();
        if !status.is_success() {
            return Err(DemoError::Rejected {
                status: status.as_u16(),
            });
        }
        let page: TracePage = response.json().await?;
        Ok(!page.data.is_empty())
    }

    /// Sends `traces` in requests of at most [`MAX_BATCH_TRACES`] traces each.
    pub(crate) async fn send(&self, traces: &[Trace]) -> Result<(), DemoError> {
        for chunk in traces.chunks(MAX_BATCH_TRACES) {
            self.post(chunk).await?;
        }
        if !traces.is_empty() {
            info!(
                traces = traces.len(),
                requests = traces.len().div_ceil(MAX_BATCH_TRACES),
                "sent traces"
            );
        }
        Ok(())
    }

    /// One batch. A 429 or 503 waits for `Retry-After`, a 402 waits ten minutes, and
    /// both send the same batch again. Any other failure ends the run.
    async fn post(&self, chunk: &[Trace]) -> Result<(), DemoError> {
        let body = ExportRequest {
            resource_spans: chunk.iter().flatten().collect(),
        };
        loop {
            let response = self
                .http
                .post(format!("{}/v1/traces", self.base))
                .bearer_auth(&self.key)
                .json(&body)
                .send()
                .await?;
            let status = response.status();
            if status.is_success() {
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
                other => {
                    return Err(DemoError::Rejected {
                        status: other.as_u16(),
                    });
                }
            }
        }
    }
}

fn retry_after(response: &reqwest::Response) -> Duration {
    response
        .headers()
        .get(header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|text| text.trim().parse::<u64>().ok())
        .map_or(DEFAULT_RETRY, Duration::from_secs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    use axum::extract::{RawQuery, State};
    use axum::http::HeaderMap;
    use axum::response::{IntoResponse, Response};
    use axum::routing::{get, post};
    use axum::{Json, Router};
    use serde_json::{Value, json};

    /// What the fake cell saw, and what it should answer.
    #[derive(Default)]
    struct Cell {
        posts: Mutex<Vec<Seen>>,
        /// How many of the next posts answer 429 with `Retry-After: 0`.
        busy_answers: Mutex<u32>,
        /// Items in the backfill-check page.
        page_items: Mutex<usize>,
        last_query: Mutex<Option<String>>,
    }

    #[derive(Debug, Clone)]
    struct Seen {
        authorization: Option<String>,
        content_type: Option<String>,
        traces: usize,
    }

    async fn ingest(
        State(cell): State<Arc<Cell>>,
        headers: HeaderMap,
        Json(body): Json<Value>,
    ) -> Response {
        let read = |name: header::HeaderName| {
            headers
                .get(name)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned)
        };
        cell.posts.lock().expect("lock").push(Seen {
            authorization: read(header::AUTHORIZATION),
            content_type: read(header::CONTENT_TYPE),
            traces: body["resourceSpans"].as_array().map_or(0, Vec::len),
        });
        let mut busy = cell.busy_answers.lock().expect("lock");
        if *busy > 0 {
            *busy -= 1;
            return (StatusCode::TOO_MANY_REQUESTS, [("retry-after", "0")]).into_response();
        }
        Json(json!({})).into_response()
    }

    async fn trace_page(State(cell): State<Arc<Cell>>, RawQuery(query): RawQuery) -> Json<Value> {
        *cell.last_query.lock().expect("lock") = query;
        let items = *cell.page_items.lock().expect("lock");
        let data: Vec<Value> = (0..items).map(|_| json!({})).collect();
        Json(json!({ "data": data, "next_cursor": null }))
    }

    /// Starts the fake cell on a free loopback port and returns its base URL.
    async fn start(cell: Arc<Cell>) -> String {
        let app = Router::new()
            .route("/v1/traces", post(ingest))
            .route("/api/v1/projects/current/traces", get(trace_page))
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

    fn small_trace() -> Trace {
        serde_json::from_value(json!([{"scopeSpans": [{"spans": [{
            "traceId": "00000000000000000000000000000001",
            "spanId": "0000000000000001",
            "startTimeUnixNano": "1",
            "endTimeUnixNano": "2"
        }]}]}]))
        .expect("a valid trace")
    }

    #[tokio::test]
    async fn one_batch_carries_the_bearer_key_and_json() {
        let cell = Arc::new(Cell::default());
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
        let cell = Arc::new(Cell::default());
        *cell.busy_answers.lock().expect("lock") = 1;
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
    async fn batches_hold_at_most_500_traces() {
        let cell = Arc::new(Cell::default());
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
    async fn the_backfill_check_reads_one_item_of_the_window() {
        let cell = Arc::new(Cell::default());
        let base = start(cell.clone()).await;
        let sink = Sink::new(&base, "sk-cetra-test".to_owned()).expect("client");
        let now = Utc::now();
        let from = now - chrono::Duration::days(30);
        assert!(!sink.has_trace_between(from, now).await.expect("checked"));
        let query = cell
            .last_query
            .lock()
            .expect("lock")
            .clone()
            .unwrap_or_default();
        assert!(query.contains("limit=1"), "query was {query}");
        assert!(query.contains("from="), "query was {query}");
        *cell.page_items.lock().expect("lock") = 1;
        assert!(sink.has_trace_between(from, now).await.expect("checked"));
    }
}
