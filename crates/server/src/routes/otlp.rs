//! `POST /v1/traces`: OTLP/HTTP trace export (spec 02 § Endpoints, D-G-2 to D-G-9), and the
//! `POST /v1/logs` and `POST /v1/metrics` routes that acknowledge and drop their data
//! (D-G-40), and the `POST /v1/agent-context` route that stores a coding agent's session context
//! (D-CA-9).
//!
//! The routes are key-only (D-G-15). The three OTLP routes are undocumented (D-G-6), and their
//! bodies are read under their own cap, so the router's JSON limit does not apply.
//! `/v1/agent-context` is documented and has its own body limit.

use std::time::Instant;

use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Extension, State};
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use cetra_core::ids::ProjectId;
use cetra_otel::Rejections;
use tracing::Instrument as _;

use crate::auth::roles::KeyScope;
use crate::entitlements::units::check_cap;
use crate::http::AppState;
use crate::http::access::{Access, ProjectAccess, RouteDef};
use crate::http::error::ApiError;
use crate::ingest::body::{BodyFormat, body_encoding, body_format, read_capped};
use crate::ingest::otlp::export_response;
use crate::ingest::{Accepted, CommitTarget, IngestError, check_rate, commit_in_task};
use crate::meta::projects::ProjectRow;
use crate::observability::spans;
use crate::routes::agent_context::post_agent_context;
use crate::telemetry::{IngestOutcome, Protocol, ingest_outcome};

/// The OTLP/HTTP routes and `POST /v1/agent-context`. The three OTLP routes are undocumented, so
/// they stay out of `openapi.json` (D-G-6). `/v1/agent-context` is documented (D-CA-9).
pub(crate) fn routes() -> Vec<RouteDef> {
    let route = |path, handler| RouteDef {
        method: Method::POST,
        path,
        access: Access::Key {
            scope: KeyScope::Ingest,
        },
        documented: false,
        handler,
    };
    vec![
        route("/v1/traces", post(export)),
        route("/v1/logs", post(export_logs)),
        route("/v1/metrics", post(export_metrics)),
        RouteDef {
            method: Method::POST,
            path: "/v1/agent-context",
            access: Access::Key {
                scope: KeyScope::Ingest,
            },
            documented: true,
            handler: post(post_agent_context).layer(DefaultBodyLimit::max(4096)),
        },
    ]
}

/// `POST /v1/logs` (D-G-40): the request is checked like a trace request, and answered with
/// an OTLP success. The records are not stored; those that look like GenAI content events
/// are counted for the project, so the UI can tell the user their content was dropped.
async fn export_logs(
    State(state): State<AppState>,
    Extension(access): Extension<ProjectAccess>,
    headers: HeaderMap,
    body: Body,
) -> Result<Response, ApiError> {
    let started = Instant::now();
    refuse_over_cap(&state, &access).await?;
    let project = &access.project;
    match drop_logs(&state, project, &headers, body, started).await {
        Ok(response) => Ok(response),
        Err(error) => {
            log_refusal(project.id, &error, started);
            Err(error.into())
        }
    }
}

async fn drop_logs(
    state: &AppState,
    project: &ProjectRow,
    headers: &HeaderMap,
    body: Body,
    started: Instant,
) -> Result<Response, IngestError> {
    let format = body_format(headers).map_err(IngestError::Body)?;
    let encoding = body_encoding(headers).map_err(IngestError::Body)?;
    let reservation = state.ingest.reserve(content_length(headers))?;
    let raw = read_capped(body, headers, state.ingest.max_request_bytes())
        .await
        .map_err(IngestError::Body)?;
    let request_bytes = raw.len();
    let genai = state
        .ingest
        .count_genai_logs(raw, reservation, format, encoding)
        .await?;
    state
        .usage
        .add_log_events(project.org_id, project.id, state.clock.now_ns(), genai);
    tracing::info!(
        target: "ingest",
        project_id = %project.id,
        request_bytes,
        genai_log_records = genai,
        duration_ms = elapsed_ms(started),
        "logs dropped"
    );
    Ok(empty_success(format))
}

/// `POST /v1/metrics` (D-G-40): checked for the credential, the content type and the size,
/// then answered with an OTLP success. The body is not decoded and nothing is counted.
async fn export_metrics(
    State(state): State<AppState>,
    Extension(access): Extension<ProjectAccess>,
    headers: HeaderMap,
    body: Body,
) -> Result<Response, ApiError> {
    let started = Instant::now();
    refuse_over_cap(&state, &access).await?;
    let project = access.project.id;
    let checked: Result<_, IngestError> = async {
        let format = body_format(&headers).map_err(IngestError::Body)?;
        body_encoding(&headers).map_err(IngestError::Body)?;
        let _reservation = state.ingest.reserve(content_length(&headers))?;
        read_capped(body, &headers, state.ingest.max_request_bytes())
            .await
            .map_err(IngestError::Body)?;
        Ok(format)
    }
    .await;
    match checked {
        Ok(format) => Ok(empty_success(format)),
        Err(error) => {
            log_refusal(project, &error, started);
            Err(error.into())
        }
    }
}

/// The org's units cap applies to every OTLP route, so a capped org is refused alike.
pub(crate) async fn refuse_over_cap(
    state: &AppState,
    access: &ProjectAccess,
) -> Result<(), ApiError> {
    check_cap(state, access.project.org_id)
        .await
        .map_err(|cap| {
            tracing::debug!(
                target: "ingest",
                project_id = %access.project.id,
                reason = "quota",
                "request refused"
            );
            cap.into()
        })
}

/// A `200` with an empty `Export*ServiceResponse` in the request's encoding.
fn empty_success(format: BodyFormat) -> Response {
    let (content_type, payload) = export_response(format, &Rejections::default());
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, content_type)],
        payload,
    )
        .into_response()
}

async fn export(
    State(state): State<AppState>,
    Extension(access): Extension<ProjectAccess>,
    headers: HeaderMap,
    body: Body,
) -> Result<Response, ApiError> {
    let started = Instant::now();
    let span = spans::otlp_export(spans::PROTOCOL_HTTP);
    span.record(
        spans::PROJECT_ID,
        tracing::field::display(access.project.id),
    );
    if let Err(cap) = check_cap(&state, access.project.org_id).await {
        state
            .telemetry
            .refuse_ingest(Protocol::otlp_http, IngestOutcome::quota);
        span.record(spans::OUTCOME, tracing::field::debug(IngestOutcome::quota));
        tracing::debug!(
            target: "ingest",
            project_id = %access.project.id,
            reason = "quota",
            "request refused"
        );
        return Err(cap.into());
    }
    match export_body(&state, &access.project, &headers, body, started)
        .instrument(span.clone())
        .await
    {
        Ok(response) => {
            span.record(spans::OUTCOME, tracing::field::debug(IngestOutcome::ok));
            Ok(response)
        }
        Err(error) => {
            let outcome = ingest_outcome(&error);
            state.telemetry.refuse_ingest(Protocol::otlp_http, outcome);
            span.record(spans::OUTCOME, tracing::field::debug(outcome));
            log_refusal(access.project.id, &error, started);
            Err(error.into())
        }
    }
}

/// Steps 2 to 8 of G1.7. Every refusal is returned, and logged by [`export`].
async fn export_body(
    state: &AppState,
    project: &ProjectRow,
    headers: &HeaderMap,
    body: Body,
    started: Instant,
) -> Result<Response, IngestError> {
    let format = body_format(headers).map_err(IngestError::Body)?;
    let encoding = body_encoding(headers).map_err(IngestError::Body)?;
    let reservation = state.ingest.reserve(content_length(headers))?;
    let raw = read_capped(body, headers, state.ingest.max_request_bytes())
        .await
        .map_err(IngestError::Body)?;
    let now_ns = state.clock.now_ns();
    let retention_cap = crate::entitlements::of(state, project.org_id).retention_cap_days();
    let Accepted {
        normalized,
        request_bytes,
        decompressed_bytes,
        rewritten,
        reservation,
    } = state
        .ingest
        .accept(
            raw,
            reservation,
            format,
            encoding,
            project,
            retention_cap,
            now_ns,
        )
        .await?;
    check_rate(state, project.org_id, normalized.spans.len(), now_ns)?;
    let rejections = normalized.rejections.clone();
    let target = CommitTarget {
        org_id: project.org_id,
        project_id: project.id,
        request_bytes: decompressed_bytes,
        now_ns,
    };
    let receipt = commit_in_task(state, target, normalized, reservation).await?;
    if receipt.accepted > 0 {
        state.last_ingest.record(project.id, now_ns);
    }
    state.telemetry.accept_ingest(
        Protocol::otlp_http,
        u64::try_from(receipt.accepted).unwrap_or(u64::MAX),
        request_bytes,
    );
    let span = tracing::Span::current();
    span.record(spans::SPANS, spans::count(receipt.accepted));
    span.record(spans::REJECTED, spans::count(rejections.total()));
    span.record(spans::BYTES, spans::count(request_bytes));
    tracing::info!(
        target: "ingest",
        project_id = %project.id,
        request_bytes,
        decompressed_bytes,
        spans = receipt.accepted,
        rejected = rejections.total(),
        media = receipt.media,
        rewritten,
        duration_ms = elapsed_ms(started),
        "request accepted"
    );
    let (content_type, payload) = export_response(format, &rejections);
    Ok((
        StatusCode::OK,
        [(header::CONTENT_TYPE, content_type)],
        payload,
    )
        .into_response())
}

/// `Content-Length` as a number. Absent or unparsable is `None`.
fn content_length(headers: &HeaderMap) -> Option<u64> {
    headers
        .get(header::CONTENT_LENGTH)?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// One `warn` per refused request; a rate refusal is a `debug`, since a client over its rate
/// is expected. A `500` is logged once by `ApiError::into_response` instead, so the internal
/// variants return early. The reason is a static string: no
/// header, key, body byte or attribute.
fn log_refusal(project: ProjectId, error: &IngestError, started: Instant) {
    let reason = match error {
        IngestError::RateLimited { .. } => {
            tracing::debug!(target: "ingest", project_id = %project, reason = "rate", "request refused");
            return;
        }
        IngestError::Body(body) => match body {
            crate::ingest::body::BodyError::UnsupportedType => "unsupported_type",
            crate::ingest::body::BodyError::UnsupportedEncoding => "unsupported_encoding",
            crate::ingest::body::BodyError::TooLarge => "too_large",
            crate::ingest::body::BodyError::Corrupt => "corrupt",
            crate::ingest::body::BodyError::Read => "read",
        },
        IngestError::TooManySpans => "too_many_spans",
        IngestError::Decode(_) => "decode",
        IngestError::Busy => "budget",
        IngestError::Append(append) => match append {
            cetra_storage::error::AppendError::Backpressure => "backpressure",
            cetra_storage::error::AppendError::WalFailed => "wal_failed",
            cetra_storage::error::AppendError::ShuttingDown => "shutting_down",
            cetra_storage::error::AppendError::DiskFull => "disk_full",
            cetra_storage::error::AppendError::ProjectDropped => "project_dropped",
            cetra_storage::error::AppendError::InvalidBatch { .. } => return,
        },
        IngestError::Media(media) => match media {
            cetra_storage::error::MediaError::Invalid { .. } => "invalid_media",
            cetra_storage::error::MediaError::ProjectDropped => "project_dropped",
            cetra_storage::error::MediaError::DiskFull => "disk_full",
            cetra_storage::error::MediaError::Object(_)
            | cetra_storage::error::MediaError::Catalog(_)
            | cetra_storage::error::MediaError::Query(_) => "storage_unavailable",
            cetra_storage::error::MediaError::HashMismatch
            | cetra_storage::error::MediaError::NotFound
            | cetra_storage::error::MediaError::InvalidRange
            | cetra_storage::error::MediaError::Fault => return,
        },
        IngestError::Batch(_) | IngestError::MediaHash | IngestError::Join(_) => return,
    };
    tracing::warn!(
        target: "ingest",
        project_id = %project,
        duration_ms = elapsed_ms(started),
        reason,
        "request refused"
    );
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::time::Duration;

    use crate::observability::otlp::receiver::{int_attribute, string_attribute};
    use crate::observability::subscriber::capture_spans;
    use crate::test_support::raw_query;
    use axum::http::{Method, StatusCode};
    use bytes::Bytes;
    use bytesize::ByteSize;
    use cetra_otel::{IngestContext, PriceTable, builtin_price_rows, decode_json, normalize};
    use datafusion::prelude::SessionContext;
    use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceResponse;
    use prost::Message;
    use serde_json::Value;

    use super::*;
    use crate::api::common::ProblemCode;
    use crate::ingest::body::{BodyEncoding, BodyFormat};
    use crate::test_support::{Seeded, TestApp, assert_logs_clean, capture_logs, set_limits};

    const MARKER: &str = "SECRET-BODY-MARKER";
    const ATTRIBUTE_VALUE: &str = "attr-secret-value";
    const RESOURCE_TIMES: &str =
        "\"startTimeUnixNano\":\"1767225590000000000\",\"endTimeUnixNano\":\"1767225591000000000\"";

    /// One OTLP/JSON span with valid ids and times. `extra` is appended inside the span
    /// object, so it starts with a comma when it is not empty.
    fn span_json(index: usize, extra: &str) -> String {
        let id = index + 1;
        format!(
            "{{\"traceId\":\"{id:032x}\",\"spanId\":\"{id:016x}\",\"name\":\"span-{index}\",{RESOURCE_TIMES}{extra}}}"
        )
    }

    /// An OTLP/JSON export of `spans` under `resource` (empty, or a `"resource":{..},` fragment).
    fn export_json(resource: &str, spans: &[String]) -> Vec<u8> {
        let mut body = String::from("{\"resourceSpans\":[{");
        body.push_str(resource);
        body.push_str("\"scopeSpans\":[{\"spans\":[");
        body.push_str(&spans.join(","));
        body.push_str("]}]}]}");
        body.into_bytes()
    }

    fn plain_export(count: usize) -> Vec<u8> {
        let spans: Vec<String> = (0..count).map(|index| span_json(index, "")).collect();
        export_json("", &spans)
    }

    /// The same request as protobuf, re-encoded from its JSON form.
    fn protobuf(json: &[u8]) -> Vec<u8> {
        decode_json(json)
            .expect("the test body is valid OTLP/JSON")
            .encode_to_vec()
    }

    fn compress(body: &[u8], encoding: BodyEncoding) -> Vec<u8> {
        match encoding {
            BodyEncoding::Identity => body.to_vec(),
            BodyEncoding::Gzip => {
                let mut encoder =
                    flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
                encoder.write_all(body).expect("gzip write");
                encoder.finish().expect("gzip finish")
            }
            BodyEncoding::Deflate => {
                let mut encoder =
                    flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
                encoder.write_all(body).expect("zlib write");
                encoder.finish().expect("zlib finish")
            }
            BodyEncoding::Zstd => zstd::stream::encode_all(body, 0).expect("zstd encode"),
        }
    }

    /// One span whose string attribute is `bytes` long.
    fn large_attribute_export(bytes: usize) -> Vec<u8> {
        let extra = format!(
            ",\"attributes\":[{{\"key\":\"payload\",\"value\":{{\"stringValue\":\"{}\"}}}}]",
            "a".repeat(bytes)
        );
        export_json("", &[span_json(0, &extra)])
    }

    fn post_traces(
        app: &TestApp,
        key: &str,
        content_type: &str,
        body: impl Into<Bytes>,
    ) -> crate::test_support::TestRequest {
        app.request(Method::POST, "/v1/traces")
            .header("authorization", &format!("Bearer {key}"))
            .header("content-type", content_type)
            .body(body)
    }

    /// Spans in the project, read through the storage query provider.
    async fn stored_spans(app: &TestApp, project: ProjectId) -> usize {
        let query = raw_query(&app.state.storage, project).await;
        let ctx = SessionContext::new();
        ctx.register_table("raw_spans", query.spans)
            .expect("spans register");
        ctx.table("raw_spans")
            .await
            .expect("spans table")
            .count()
            .await
            .expect("count")
    }

    /// All stored span rows, formatted. Used to look at columns the tests need.
    async fn stored_rows(app: &TestApp, project: ProjectId) -> String {
        let query = raw_query(&app.state.storage, project).await;
        let ctx = SessionContext::new();
        ctx.register_table("raw_spans", query.spans)
            .expect("spans register");
        let batches = ctx
            .sql("SELECT * FROM raw_spans")
            .await
            .expect("sql")
            .collect()
            .await
            .expect("collect");
        format!("{batches:?}")
    }

    fn json_body(response: &crate::test_support::TestResponse) -> Value {
        serde_json::from_slice(&response.body).expect("JSON body")
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_export_runs_in_an_otlp_export_span_under_its_request() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let body = protobuf(&plain_export(2));
        let (response, captured) = capture_spans(
            post_traces(&app, &seeded.ingest_key, "application/x-protobuf", body).send(),
        )
        .await;
        assert_eq!(response.status, 200);
        let exports = captured.named("otlp.export");
        assert_eq!(exports.len(), 1, "{exports:?}");
        let export = &exports[0];
        assert_eq!(
            string_attribute(&export.attributes, "protocol").as_deref(),
            Some("http")
        );
        assert_eq!(
            string_attribute(&export.attributes, "project_id"),
            Some(seeded.project.id.to_string())
        );
        assert_eq!(int_attribute(&export.attributes, "spans"), Some(2));
        assert_eq!(int_attribute(&export.attributes, "rejected"), Some(0));
        assert!(int_attribute(&export.attributes, "bytes").is_some_and(|bytes| bytes > 0));
        assert_eq!(
            string_attribute(&export.attributes, "outcome").as_deref(),
            Some("ok")
        );
        let requests: Vec<_> = captured
            .named("http.request")
            .into_iter()
            .filter(|span| {
                string_attribute(&span.attributes, "http.route").as_deref() == Some("/v1/traces")
            })
            .collect();
        assert_eq!(requests.len(), 1, "{requests:?}");
        assert_eq!(export.parent_span_id, requests[0].span_id);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_refused_export_records_its_outcome() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let body = protobuf(&plain_export(1));
        let (response, captured) =
            capture_spans(post_traces(&app, &seeded.ingest_key, "text/plain", body).send()).await;
        assert_eq!(response.status, 415);
        let exports = captured.named("otlp.export");
        assert_eq!(exports.len(), 1, "{exports:?}");
        assert_eq!(
            string_attribute(&exports[0].attributes, "outcome").as_deref(),
            Some("invalid")
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn protobuf_export_is_stored_and_answers_an_empty_body() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let body = protobuf(&plain_export(2));
        let response = post_traces(&app, &seeded.ingest_key, "application/x-protobuf", body)
            .send()
            .await;
        assert_eq!(response.status, StatusCode::OK);
        assert_eq!(
            response.header("content-type"),
            Some("application/x-protobuf")
        );
        assert!(response.body.is_empty());
        assert_eq!(stored_spans(&app, seeded.project.id).await, 2);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_export_with_spans_sets_last_ingest_at() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let body = protobuf(&plain_export(2));
        let response = post_traces(&app, &seeded.ingest_key, "application/x-protobuf", body)
            .send()
            .await;
        assert_eq!(response.status, StatusCode::OK);
        assert_eq!(
            app.state.last_ingest.get(seeded.project.id),
            Some(app.state.clock.now_ns())
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn json_export_answers_an_empty_object() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let response = post_traces(
            &app,
            &seeded.ingest_key,
            "application/json",
            plain_export(2),
        )
        .send()
        .await;
        assert_eq!(response.status, StatusCode::OK);
        assert_eq!(response.header("content-type"), Some("application/json"));
        assert_eq!(response.body.as_ref(), b"{}");
        assert_eq!(stored_spans(&app, seeded.project.id).await, 2);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn every_content_encoding_is_accepted() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let body = plain_export(2);
        for (name, encoding) in [
            ("gzip", BodyEncoding::Gzip),
            ("deflate", BodyEncoding::Deflate),
            ("zstd", BodyEncoding::Zstd),
        ] {
            let (project, key) = app.add_project(&seeded, name, None).await;
            let response = post_traces(&app, &key, "application/json", compress(&body, encoding))
                .header("content-encoding", name)
                .send()
                .await;
            assert_eq!(response.status, StatusCode::OK, "{name}");
            assert_eq!(stored_spans(&app, project.id).await, 2, "{name}");
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn partial_success_stores_the_valid_spans_and_reports_the_rest() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let body = fixture_bytes("edge/rejected-ids/input.json");
        let response = post_traces(&app, &seeded.ingest_key, "application/json", body.clone())
            .send()
            .await;
        assert_eq!(response.status, StatusCode::OK);
        let partial = json_body(&response)["partialSuccess"].clone();
        assert_eq!(partial["rejectedSpans"], "4");
        // The message is the one `normalize` gives for the same request.
        let ctx = IngestContext {
            project_id: seeded.project.id,
            ingest_time: app.state.clock.now_ns(),
            retention_days: seeded.project.retention_days,
            limits: app.state.config.ingest.limits(&app.state.config.media),
        };
        let prices = PriceTable::new(builtin_price_rows()).expect("built-in prices");
        let request = decode_json(&body).expect("fixture decodes");
        let expected = normalize(
            &request,
            &ctx,
            &prices,
            &cetra_otel::AgentContexts::default(),
        )
        .rejections
        .message();
        assert_eq!(partial["errorMessage"].as_str(), expected.as_deref());
        assert_eq!(stored_spans(&app, seeded.project.id).await, 1);
    }

    /// D-P-22: under a 30-day window the age bound is 60 days (D85). A span 61 days old is
    /// rejected; one 59 days old is stored.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_plan_bounds_span_age() {
        const DAY: i64 = 86_400_000_000_000;
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        set_limits(
            &app,
            seeded.org_id,
            "2026-01-01T00:00:00Z",
            &serde_json::json!({"data_access_days": 30}),
        );
        let now = app.state.clock.now_ns();
        let span = |id: usize, start: i64| {
            format!(
                "{{\"traceId\":\"{id:032x}\",\"spanId\":\"{id:016x}\",\"name\":\"span-{id}\",\"startTimeUnixNano\":\"{start}\",\"endTimeUnixNano\":\"{}\"}}",
                start + 1_000_000_000
            )
        };
        let body = export_json("", &[span(1, now - 61 * DAY), span(2, now - 59 * DAY)]);
        let response = post_traces(&app, &seeded.ingest_key, "application/json", body)
            .send()
            .await;
        assert_eq!(response.status, StatusCode::OK);
        assert_eq!(json_body(&response)["partialSuccess"]["rejectedSpans"], "1");
        assert_eq!(stored_spans(&app, seeded.project.id).await, 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn protobuf_partial_success_carries_the_count() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let body = protobuf(&fixture_bytes("edge/rejected-ids/input.json"));
        let response = post_traces(&app, &seeded.ingest_key, "application/x-protobuf", body)
            .send()
            .await;
        assert_eq!(response.status, StatusCode::OK);
        let decoded =
            ExportTraceServiceResponse::decode(response.body.as_ref()).expect("protobuf response");
        let partial = decoded.partial_success.expect("partial success");
        assert_eq!(partial.rejected_spans, 4);
        assert!(!partial.error_message.is_empty());
        assert_eq!(stored_spans(&app, seeded.project.id).await, 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_request_whose_only_span_is_rejected_stores_nothing() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let zero = format!(
            "{{\"traceId\":\"00000000000000000000000000000000\",\"spanId\":\"0000000000000001\",\"name\":\"zero\",{RESOURCE_TIMES}}}"
        );
        let response = post_traces(
            &app,
            &seeded.ingest_key,
            "application/json",
            export_json("", &[zero]),
        )
        .send()
        .await;
        assert_eq!(response.status, StatusCode::OK);
        assert_eq!(json_body(&response)["partialSuccess"]["rejectedSpans"], "1");
        assert_eq!(stored_spans(&app, seeded.project.id).await, 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_export_whose_only_span_is_rejected_leaves_last_ingest_at_unset() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let zero = format!(
            "{{\"traceId\":\"00000000000000000000000000000000\",\"spanId\":\"0000000000000001\",\"name\":\"zero\",{RESOURCE_TIMES}}}"
        );
        let response = post_traces(
            &app,
            &seeded.ingest_key,
            "application/json",
            export_json("", &[zero]),
        )
        .send()
        .await;
        assert_eq!(response.status, StatusCode::OK);
        assert_eq!(app.state.last_ingest.get(seeded.project.id), None);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_empty_request_answers_ok() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let response = post_traces(&app, &seeded.ingest_key, "application/json", "{}")
            .send()
            .await;
        assert_eq!(response.status, StatusCode::OK);
        assert_eq!(response.body.as_ref(), b"{}");
        assert_eq!(stored_spans(&app, seeded.project.id).await, 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_body_above_the_old_one_mib_router_limit_is_read() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let body = protobuf(&large_attribute_export(2 * 1024 * 1024));
        assert!(
            body.len() > 2 * 1024 * 1024,
            "the body is {} bytes",
            body.len()
        );
        let response = post_traces(&app, &seeded.ingest_key, "application/x-protobuf", body)
            .send()
            .await;
        assert_eq!(response.status, StatusCode::OK);
        assert_eq!(stored_spans(&app, seeded.project.id).await, 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_body_above_the_limit_is_refused_with_413() {
        let app = TestApp::with_config(|config| {
            config.ingest.max_request_bytes = ByteSize::kib(64);
        })
        .await;
        let seeded = app.seed().await;
        let body = vec![b' '; 64 * 1024 + 1];
        let response = post_traces(&app, &seeded.ingest_key, "application/json", body)
            .send()
            .await;
        assert_eq!(response.status, StatusCode::PAYLOAD_TOO_LARGE);
        let problem = response.problem();
        assert_eq!(problem.code, ProblemCode::PayloadTooLarge);
        assert_eq!(problem.detail.as_deref(), Some("request body is too large"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_gzip_body_that_decompresses_past_the_limit_is_refused_with_413() {
        let app = TestApp::with_config(|config| {
            config.ingest.max_request_bytes = ByteSize::kib(64);
        })
        .await;
        let seeded = app.seed().await;
        let bomb = compress(&vec![b'a'; 128 * 1024], BodyEncoding::Gzip);
        assert!(bomb.len() < 64 * 1024);
        let response = post_traces(&app, &seeded.ingest_key, "application/json", bomb)
            .header("content-encoding", "gzip")
            .send()
            .await;
        assert_eq!(response.status, StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(
            response.problem().detail.as_deref(),
            Some("request body is too large")
        );
        assert_eq!(stored_spans(&app, seeded.project.id).await, 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn too_many_spans_are_refused_with_413() {
        let app = TestApp::with_config(|config| {
            config.ingest.max_spans_per_request = 2;
        })
        .await;
        let seeded = app.seed().await;
        let response = post_traces(
            &app,
            &seeded.ingest_key,
            "application/json",
            plain_export(3),
        )
        .send()
        .await;
        assert_eq!(response.status, StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(
            response.problem().detail.as_deref(),
            Some("too many spans in one request")
        );
        assert_eq!(stored_spans(&app, seeded.project.id).await, 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn unsupported_types_and_encodings_are_refused_with_415() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let text = post_traces(&app, &seeded.ingest_key, "text/plain", plain_export(1))
            .send()
            .await;
        assert_eq!(text.status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
        assert_eq!(
            text.problem().detail.as_deref(),
            Some("expected Content-Type application/x-protobuf or application/json")
        );
        let brotli = post_traces(
            &app,
            &seeded.ingest_key,
            "application/json",
            plain_export(1),
        )
        .header("content-encoding", "br")
        .send()
        .await;
        assert_eq!(brotli.status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
        assert_eq!(
            brotli.problem().detail.as_deref(),
            Some("unsupported Content-Encoding")
        );
        assert_eq!(stored_spans(&app, seeded.project.id).await, 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn malformed_bodies_are_400_and_their_bytes_stay_out() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let protobuf_garbage = [&[0xff_u8; 11][..], MARKER.as_bytes()].concat();
        let bodies = [
            ("application/x-protobuf", None, protobuf_garbage),
            (
                "application/json",
                None,
                format!("{{\"resourceSpans\": {MARKER}").into_bytes(),
            ),
            (
                "application/json",
                Some("gzip"),
                format!("{MARKER} not gzip").into_bytes(),
            ),
        ];
        for (content_type, encoding, body) in bodies {
            let mut request = post_traces(&app, &seeded.ingest_key, content_type, body);
            if let Some(encoding) = encoding {
                request = request.header("content-encoding", encoding);
            }
            let response =
                assert_logs_clean(&[MARKER, seeded.ingest_key.as_str()], request.send()).await;
            assert_eq!(response.status, StatusCode::BAD_REQUEST, "{content_type}");
            assert_eq!(response.problem().code, ProblemCode::BadRequest);
            let text = String::from_utf8_lossy(&response.body);
            assert!(!text.contains(MARKER), "{text}");
        }
        assert_eq!(stored_spans(&app, seeded.project.id).await, 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn callers_without_an_ingest_key_are_refused_and_store_nothing() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let anonymous = app
            .request(Method::POST, "/v1/traces")
            .header("content-type", "application/json")
            .body(plain_export(1))
            .send()
            .await;
        assert_eq!(anonymous.status, StatusCode::UNAUTHORIZED);

        let unknown = post_traces(
            &app,
            "sk-cetra-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            "application/json",
            plain_export(1),
        )
        .send()
        .await;
        assert_eq!(unknown.status, StatusCode::UNAUTHORIZED);

        for key in [&seeded.read_key, &seeded.public_key] {
            let response = post_traces(&app, key, "application/json", plain_export(1))
                .send()
                .await;
            assert_eq!(response.status, StatusCode::FORBIDDEN);
        }

        let session = app
            .request(Method::POST, "/v1/traces")
            .header("cookie", &seeded.cookie)
            .header("content-type", "application/json")
            .body(plain_export(1))
            .send()
            .await;
        assert_eq!(session.status, StatusCode::FORBIDDEN);
        assert_eq!(stored_spans(&app, seeded.project.id).await, 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_used_key_of_a_deleting_project_is_not_found() {
        let app = TestApp::new().await;
        let seeded: Seeded = app.seed().await;
        let first = post_traces(&app, &seeded.ingest_key, "application/json", "{}")
            .send()
            .await;
        assert_eq!(first.status, StatusCode::OK);
        app.state
            .meta
            .write(|conn| {
                Box::pin(async move {
                    sqlx::query("UPDATE projects SET state = 'deleting'")
                        .execute(&mut *conn)
                        .await?;
                    Ok(())
                })
            })
            .await
            .unwrap();
        let second = post_traces(&app, &seeded.ingest_key, "application/json", "{}")
            .send()
            .await;
        assert_eq!(second.status, StatusCode::NOT_FOUND);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_full_budget_answers_503_until_the_held_request_is_done() {
        let app = TestApp::with_config(|config| {
            config.ingest.max_request_bytes = ByteSize::kib(64);
            config.ingest.max_inflight_bytes = ByteSize::kib(64);
        })
        .await;
        let seeded = app.seed().await;
        let held_body = large_attribute_export(40 * 1024);
        let reservation = app
            .state
            .ingest
            .reserve(Some(held_body.len() as u64))
            .expect("the budget is free");
        let held = app
            .state
            .ingest
            .accept(
                Bytes::from(held_body),
                reservation,
                BodyFormat::Json,
                BodyEncoding::Identity,
                &seeded.project,
                None,
                app.state.clock.now_ns(),
            )
            .await
            .expect("the held request is accepted");

        let body = large_attribute_export(40 * 1024);
        let busy = post_traces(&app, &seeded.ingest_key, "application/json", body.clone())
            .send()
            .await;
        assert_eq!(busy.status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(busy.header("retry-after"), Some("1"));
        assert_eq!(busy.problem().code, ProblemCode::Busy);
        assert_eq!(stored_spans(&app, seeded.project.id).await, 0);

        drop(held);
        let retried = post_traces(&app, &seeded.ingest_key, "application/json", body)
            .send()
            .await;
        assert_eq!(retried.status, StatusCode::OK);
        assert_eq!(stored_spans(&app, seeded.project.id).await, 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_reserved_environment_is_rewritten_and_warned() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let resource = "\"resource\":{\"attributes\":[{\"key\":\"deployment.environment.name\",\"value\":{\"stringValue\":\"cetra-eval\"}}]},";
        let body = export_json(resource, &[span_json(0, "")]);
        let response = post_traces(&app, &seeded.ingest_key, "application/json", body)
            .send()
            .await;
        assert_eq!(response.status, StatusCode::OK);
        let rows = stored_rows(&app, seeded.project.id).await;
        assert!(rows.contains("cetra-eval-external"), "{rows}");
        assert!(rows.contains("reserved_environment"), "{rows}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_accepted_log_names_the_route_and_leaves_out_secrets() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let extra = format!(
            ",\"attributes\":[{{\"key\":\"secret\",\"value\":{{\"stringValue\":\"{ATTRIBUTE_VALUE}\"}}}}]"
        );
        let body = export_json("", &[span_json(0, &extra), span_json(1, "")]);
        let (response, logs) =
            capture_logs(post_traces(&app, &seeded.ingest_key, "application/json", body).send())
                .await;
        assert_eq!(response.status, StatusCode::OK);
        assert!(logs.contains("/v1/traces"), "{logs}");
        assert!(logs.contains(&seeded.project.id.to_string()), "{logs}");
        assert!(logs.contains("spans=2"), "{logs}");
        assert!(!logs.contains(&seeded.ingest_key), "{logs}");
        assert!(!logs.contains(ATTRIBUTE_VALUE), "{logs}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn refusals_log_a_reason_and_leave_out_the_body() {
        let app = TestApp::with_config(|config| {
            config.ingest.max_spans_per_request = 1;
        })
        .await;
        let seeded = app.seed().await;
        let (response, logs) = capture_logs(
            post_traces(
                &app,
                &seeded.ingest_key,
                "application/json",
                export_json(
                    "",
                    &[
                        span_json(0, ""),
                        span_json(1, &format!(",\"attributes\":[{{\"key\":\"k\",\"value\":{{\"stringValue\":\"{MARKER}\"}}}}]")),
                    ],
                ),
            )
            .send(),
        )
        .await;
        assert_eq!(response.status, StatusCode::PAYLOAD_TOO_LARGE);
        assert!(logs.contains("too_many_spans"), "{logs}");
        assert!(!logs.contains(MARKER), "{logs}");
        assert!(!logs.contains(&seeded.ingest_key), "{logs}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_accepted_export_is_published_on_the_live_bus() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let mut events = app.state.live.subscribe();
        let response = post_traces(
            &app,
            &seeded.ingest_key,
            "application/json",
            plain_export(2),
        )
        .send()
        .await;
        assert_eq!(response.status, StatusCode::OK);
        let event = tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .expect("an event arrives")
            .expect("the bus is open");
        assert_eq!(event.project_id, seeded.project.id);
        assert_eq!(
            event.spans.len(),
            stored_spans(&app, seeded.project.id).await
        );
        assert_eq!(event.spans.len(), 2);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn accepted_spans_and_request_bytes_are_counted_for_the_day() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let body = export_json("", &[span_json(0, ""), span_json(1, "")]);
        let len = i64::try_from(body.len()).unwrap();
        let response = post_traces(&app, &seeded.ingest_key, "application/json", body)
            .send()
            .await;
        assert_eq!(response.status, StatusCode::OK);
        app.state
            .usage
            .flush(&app.state.meta)
            .await
            .expect("usage flushes");
        let counted: (i64, i64, i64) = sqlx::query_as(
            "SELECT spans, bytes, scores FROM usage_daily WHERE project_id = ? AND day = '2026-01-01'",
        )
        .bind(seeded.project.id.to_string())
        .fetch_one(app.state.meta.reader())
        .await
        .expect("one usage row for the day");
        assert_eq!(counted, (2, len, 0));
    }

    fn fixture_bytes(relative: &str) -> Vec<u8> {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/otlp")
            .join(relative);
        std::fs::read(&path).expect("fixture is readable")
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn capped_org_gets_402() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        set_limits(
            &app,
            seeded.org_id,
            "2025-12-01T00:00:00Z",
            &serde_json::json!({"units_cap": 10}),
        );
        app.state.usage.add(
            seeded.org_id,
            seeded.project.id,
            app.state.clock.now_ns(),
            10,
            0,
            0,
        );
        let body = export_json("", &[span_json(0, "")]);
        let response = post_traces(&app, &seeded.ingest_key, "application/json", body)
            .send()
            .await;
        assert_eq!(response.status, StatusCode::PAYMENT_REQUIRED);
        assert!(response.header("retry-after").is_some());
        assert_eq!(response.problem().code, ProblemCode::QuotaExceeded);
        assert_eq!(stored_spans(&app, seeded.project.id).await, 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_request_below_the_cap_is_accepted_whole() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        set_limits(
            &app,
            seeded.org_id,
            "2025-12-01T00:00:00Z",
            &serde_json::json!({"units_cap": 10}),
        );
        app.state.usage.add(
            seeded.org_id,
            seeded.project.id,
            app.state.clock.now_ns(),
            9,
            0,
            0,
        );
        let spans: Vec<String> = (0..5).map(|index| span_json(index, "")).collect();
        let body = export_json("", &spans);
        let response = post_traces(&app, &seeded.ingest_key, "application/json", body)
            .send()
            .await;
        assert_eq!(response.status, StatusCode::OK);
        assert_eq!(stored_spans(&app, seeded.project.id).await, 5);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn rate_limited_org_gets_429() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        set_limits(
            &app,
            seeded.org_id,
            "2025-12-01T00:00:00Z",
            &serde_json::json!({"ingest_spans_per_sec": 1, "ingest_burst": 2}),
        );
        let body = export_json("", &[span_json(0, "")]);
        for _ in 0..2 {
            let response = post_traces(&app, &seeded.ingest_key, "application/json", body.clone())
                .send()
                .await;
            assert_eq!(response.status, StatusCode::OK);
        }
        let response = post_traces(&app, &seeded.ingest_key, "application/json", body)
            .send()
            .await;
        assert_eq!(response.status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(response.header("retry-after"), Some("1"));
        assert_eq!(response.problem().code, ProblemCode::RateLimited);
        assert_eq!(stored_spans(&app, seeded.project.id).await, 2);
    }

    fn has_line(text: &str, line: &str) -> bool {
        text.lines().any(|candidate| candidate == line)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn accepted_otlp_request_is_counted() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let body = plain_export(2);
        let len = body.len();
        let response = post_traces(&app, &seeded.ingest_key, "application/json", body)
            .send()
            .await;
        assert_eq!(response.status, StatusCode::OK);
        let text = app
            .state
            .telemetry
            .render(&app.state)
            .await
            .expect("metrics render");
        assert!(
            has_line(
                &text,
                "cetra_ingest_requests_total{protocol=\"otlp_http\",outcome=\"ok\"} 1"
            ),
            "{text}"
        );
        assert!(
            has_line(&text, "cetra_ingest_spans_total{protocol=\"otlp_http\"} 2"),
            "{text}"
        );
        assert!(
            has_line(
                &text,
                &format!("cetra_ingest_bytes_total{{protocol=\"otlp_http\"}} {len}")
            ),
            "{text}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn invalid_otlp_request_is_counted() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let response = post_traces(
            &app,
            &seeded.ingest_key,
            "application/json",
            "{\"resourceSpans\": ",
        )
        .send()
        .await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST);
        let text = app
            .state
            .telemetry
            .render(&app.state)
            .await
            .expect("metrics render");
        assert!(
            has_line(
                &text,
                "cetra_ingest_requests_total{protocol=\"otlp_http\",outcome=\"invalid\"} 1"
            ),
            "{text}"
        );
        assert!(
            !text
                .lines()
                .any(|line| line.starts_with("cetra_ingest_spans_total{")),
            "{text}"
        );
    }

    const LOGS_JSON: &str = r#"{"resourceLogs":[{"scopeLogs":[{"logRecords":[
        {"eventName":"gen_ai.user.message","body":{"stringValue":"SECRET-BODY-MARKER"}},
        {"eventName":"gen_ai.choice"},
        {"body":{"stringValue":"plain log"}}
    ]}]}]}"#;

    fn post_to(
        app: &TestApp,
        path: &str,
        key: &str,
        content_type: &str,
        body: impl Into<Bytes>,
    ) -> crate::test_support::TestRequest {
        app.request(Method::POST, path)
            .header("authorization", &format!("Bearer {key}"))
            .header("content-type", content_type)
            .body(body)
    }

    /// GenAI log records counted for the project, flushed to `usage_daily`.
    async fn counted_log_events(app: &TestApp, project: ProjectId) -> i64 {
        app.state
            .usage
            .flush(&app.state.meta)
            .await
            .expect("usage flushes");
        sqlx::query_scalar(
            "SELECT COALESCE(SUM(genai_log_events), 0) FROM usage_daily WHERE project_id = ?",
        )
        .bind(project.to_string())
        .fetch_one(app.state.meta.reader())
        .await
        .expect("sum")
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn json_logs_are_dropped_and_genai_records_counted() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let response = post_to(
            &app,
            "/v1/logs",
            &seeded.ingest_key,
            "application/json",
            LOGS_JSON,
        )
        .send()
        .await;
        assert_eq!(response.status, StatusCode::OK);
        assert_eq!(response.header("content-type"), Some("application/json"));
        assert_eq!(response.body.as_ref(), b"{}");
        assert_eq!(counted_log_events(&app, seeded.project.id).await, 2);
        assert_eq!(stored_spans(&app, seeded.project.id).await, 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn protobuf_logs_are_dropped_and_genai_records_counted() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let request = cetra_otel::decode_logs_json(LOGS_JSON.as_bytes()).expect("valid logs");
        let body = compress(&request.encode_to_vec(), BodyEncoding::Gzip);
        let response = app
            .request(Method::POST, "/v1/logs")
            .header("authorization", &format!("Bearer {}", seeded.ingest_key))
            .header("content-type", "application/x-protobuf")
            .header("content-encoding", "gzip")
            .body(body)
            .send()
            .await;
        assert_eq!(response.status, StatusCode::OK);
        assert_eq!(
            response.header("content-type"),
            Some("application/x-protobuf")
        );
        assert!(response.body.is_empty());
        assert_eq!(counted_log_events(&app, seeded.project.id).await, 2);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn logs_are_refused_like_traces() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let no_key = app
            .request(Method::POST, "/v1/logs")
            .header("content-type", "application/json")
            .body(LOGS_JSON)
            .send()
            .await;
        assert_eq!(no_key.status, StatusCode::UNAUTHORIZED);
        let wrong_type = post_to(
            &app,
            "/v1/logs",
            &seeded.ingest_key,
            "text/plain",
            LOGS_JSON,
        )
        .send()
        .await;
        assert_eq!(wrong_type.status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
        let malformed = post_to(
            &app,
            "/v1/logs",
            &seeded.ingest_key,
            "application/json",
            "{",
        )
        .send()
        .await;
        assert_eq!(malformed.status, StatusCode::BAD_REQUEST);
        let get = app
            .request(Method::GET, "/v1/logs")
            .header("authorization", &format!("Bearer {}", seeded.ingest_key))
            .send()
            .await;
        assert_eq!(get.status, StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(counted_log_events(&app, seeded.project.id).await, 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn metrics_are_acknowledged_and_dropped() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let json = post_to(
            &app,
            "/v1/metrics",
            &seeded.ingest_key,
            "application/json",
            r#"{"resourceMetrics":[]}"#,
        )
        .send()
        .await;
        assert_eq!(json.status, StatusCode::OK);
        assert_eq!(json.body.as_ref(), b"{}");
        let protobuf = post_to(
            &app,
            "/v1/metrics",
            &seeded.ingest_key,
            "application/x-protobuf",
            Vec::new(),
        )
        .send()
        .await;
        assert_eq!(protobuf.status, StatusCode::OK);
        assert!(protobuf.body.is_empty());
        let wrong_type = post_to(&app, "/v1/metrics", &seeded.ingest_key, "text/plain", "x")
            .send()
            .await;
        assert_eq!(wrong_type.status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
        let no_key = app
            .request(Method::POST, "/v1/metrics")
            .header("content-type", "application/json")
            .body("{}")
            .send()
            .await;
        assert_eq!(no_key.status, StatusCode::UNAUTHORIZED);
    }

    /// The M1 Claude Code export, with its placeholder session id replaced by `session`.
    fn claude_export(session: &str) -> Vec<u8> {
        let text = String::from_utf8(fixture_bytes("claude_code/bash-tool/input.json"))
            .expect("the fixture is UTF-8");
        text.replace("capture-volatile-2", session).into_bytes()
    }

    /// The claude-code context of session `s1`, as `cetra agents hook` posts it.
    fn s1_context() -> Value {
        serde_json::json!({
            "agent": "claude-code",
            "session_id": "s1",
            "repository": "https://github.com/Acme/Widgets.git",
            "branch": "feature/x",
            "user": "Dev@Example.com",
        })
    }

    /// Spans of the project whose stored row matches the SQL `predicate` on `raw_spans`.
    async fn spans_where(app: &TestApp, project: ProjectId, predicate: &str) -> usize {
        let query = raw_query(&app.state.storage, project).await;
        let ctx = SessionContext::new();
        ctx.register_table("raw_spans", query.spans)
            .expect("spans register");
        ctx.sql(&format!("SELECT * FROM raw_spans WHERE {predicate}"))
            .await
            .expect("sql")
            .count()
            .await
            .expect("count")
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn stamped_after_a_context() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let project = seeded.project.id;
        let posted = post_to(
            &app,
            "/v1/agent-context",
            &seeded.ingest_key,
            "application/json",
            s1_context().to_string(),
        )
        .send()
        .await;
        assert_eq!(posted.status, StatusCode::NO_CONTENT);
        let response = post_traces(
            &app,
            &seeded.ingest_key,
            "application/json",
            claude_export("s1"),
        )
        .send()
        .await;
        assert_eq!(response.status, StatusCode::OK);
        let total = stored_spans(&app, project).await;
        assert!(total > 0);
        assert_eq!(
            spans_where(
                &app,
                project,
                "metadata['repository'] = 'github.com/acme/widgets' \
                 AND metadata['branch'] = 'feature/x' AND user_id = 'dev@example.com'"
            )
            .await,
            total
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn not_stamped_for_another_session() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let project = seeded.project.id;
        post_to(
            &app,
            "/v1/agent-context",
            &seeded.ingest_key,
            "application/json",
            s1_context().to_string(),
        )
        .send()
        .await;
        let response = post_traces(
            &app,
            &seeded.ingest_key,
            "application/json",
            claude_export("s2"),
        )
        .send()
        .await;
        assert_eq!(response.status, StatusCode::OK);
        assert!(stored_spans(&app, project).await > 0);
        assert_eq!(
            spans_where(&app, project, "metadata['branch'] IS NOT NULL").await,
            0
        );
        assert!(
            spans_where(
                &app,
                project,
                "metadata['repository'] = 'github.com/acme/widgets'"
            )
            .await
                > 0
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn not_stamped_before_the_context() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let project = seeded.project.id;
        let response = post_traces(
            &app,
            &seeded.ingest_key,
            "application/json",
            claude_export("s1"),
        )
        .send()
        .await;
        assert_eq!(response.status, StatusCode::OK);
        let posted = post_to(
            &app,
            "/v1/agent-context",
            &seeded.ingest_key,
            "application/json",
            s1_context().to_string(),
        )
        .send()
        .await;
        assert_eq!(posted.status, StatusCode::NO_CONTENT);
        assert!(stored_spans(&app, project).await > 0);
        assert_eq!(
            spans_where(&app, project, "metadata['branch'] IS NOT NULL").await,
            0
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn logs_and_metrics_keep_the_body_cap() {
        let app =
            TestApp::with_config(|config| config.ingest.max_request_bytes = ByteSize::b(64)).await;
        let seeded = app.seed().await;
        let big = vec![b' '; 65];
        for path in ["/v1/logs", "/v1/metrics"] {
            let response = post_to(
                &app,
                path,
                &seeded.ingest_key,
                "application/json",
                big.clone(),
            )
            .send()
            .await;
            assert_eq!(response.status, StatusCode::PAYLOAD_TOO_LARGE, "{path}");
        }
    }
}
