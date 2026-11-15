//! OTLP/gRPC `TraceService/Export` (spec 02 § Endpoints, D-G-29).
//!
//! The credential is the `authorization` metadata. Only a project key with `ingest` is
//! admitted, on its own project, as on the HTTP route (D-G-15). The admission runs in a tower
//! layer ([`Admit`]) before tonic reads the body: the credential, the key's scope, the project,
//! the org's cap, and a reservation of `ingest.max_grpc_message_bytes` (G-1). `export` then
//! shrinks that reservation to the decoded message and accepts it. Refusals use the static
//! messages of design § gRPC status mapping; nothing from the request is logged.

use std::convert::Infallible;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll};
use std::time::Instant;

use llmobs_storage::error::{AppendError, MediaError};
use opentelemetry_proto::tonic::collector::trace::v1::trace_service_server::{
    TraceService, TraceServiceServer,
};
use opentelemetry_proto::tonic::collector::trace::v1::{
    ExportTracePartialSuccess, ExportTraceServiceRequest, ExportTraceServiceResponse,
};
use prost::Message;
use tonic::Code;
use tonic::codec::CompressionEncoding;
use tonic::server::NamedService;
use tower::{Layer, Service};

use super::otlp::partial_success;
use super::{Accepted, CommitTarget, IngestError, Reservation, check_rate, commit_in_task};
use crate::auth::principal::{Principal, bearer_principal};
use crate::auth::roles::KeyScope;
use crate::entitlements::units::check_cap;
use crate::http::AppState;
use crate::http::access::key_only_project;
use crate::http::error::ApiError;
use crate::meta::projects::ProjectRow;
use crate::telemetry::{IngestOutcome, Protocol, ingest_outcome};

/// The status message of a rate refusal. The wrapper logs it at `debug`.
const RATE_LIMITED_MESSAGE: &str = "rate limited";

/// What the admission layer leaves in the request's extensions (G-1): the project the key may
/// write to, and the budget reserved for the largest message. `export` takes the reservation.
#[derive(Clone)]
pub(crate) struct GrpcAdmitted {
    project: ProjectRow,
    reservation: Arc<Mutex<Option<Reservation>>>,
}

/// The tonic service. Cloning shares the state.
#[derive(Clone)]
pub(crate) struct GrpcTraces {
    pub(crate) state: AppState,
}

impl fmt::Debug for GrpcTraces {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("GrpcTraces").finish_non_exhaustive()
    }
}

#[tonic::async_trait]
impl TraceService for GrpcTraces {
    async fn export(
        &self,
        request: tonic::Request<ExportTraceServiceRequest>,
    ) -> Result<tonic::Response<ExportTraceServiceResponse>, tonic::Status> {
        let started = Instant::now();
        match export_request(&self.state, request, started).await {
            Ok(response) => Ok(tonic::Response::new(response)),
            Err(status) => {
                log_refusal(&status, started);
                Err(status)
            }
        }
    }
}

/// The layer that runs [`admit`] on each request before the inner service reads its body.
#[derive(Clone)]
pub(crate) struct AdmitLayer {
    state: AppState,
}

impl<S> Layer<S> for AdmitLayer {
    type Service = Admit<S>;

    fn layer(&self, inner: S) -> Self::Service {
        Admit {
            inner,
            state: self.state.clone(),
        }
    }
}

/// An admitted request goes to `inner` with its [`GrpcAdmitted`] extension. A refusal is
/// answered here with its static status, and the body is never read.
#[derive(Clone)]
pub(crate) struct Admit<S> {
    inner: S,
    state: AppState,
}

impl<S: NamedService> NamedService for Admit<S> {
    const NAME: &'static str = S::NAME;
}

impl<S, B> Service<axum::http::Request<B>> for Admit<S>
where
    S: Service<
            axum::http::Request<B>,
            Response = axum::http::Response<tonic::body::Body>,
            Error = Infallible,
        > + Clone
        + Send
        + 'static,
    S::Future: Send + 'static,
    B: Send + 'static,
{
    type Response = axum::http::Response<tonic::body::Body>;
    type Error = Infallible;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Infallible>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut request: axum::http::Request<B>) -> Self::Future {
        // The clone takes the place of the service that is ready, which this call uses.
        let clone = self.inner.clone();
        let mut inner = std::mem::replace(&mut self.inner, clone);
        let state = self.state.clone();
        Box::pin(async move {
            let started = Instant::now();
            match admit(&state, request.headers()).await {
                Ok(admitted) => {
                    request.extensions_mut().insert(admitted);
                    inner.call(request).await
                }
                Err(status) => {
                    log_refusal(&status, started);
                    state
                        .telemetry
                        .refuse_ingest(Protocol::otlp_grpc, admit_outcome(&status));
                    Ok(status.into_http())
                }
            }
        })
    }
}

/// The service with its limits and compressions, behind the admission layer (G-1). The decoded
/// message cap is `ingest.max_grpc_message_bytes` (validated by `config`).
pub(crate) fn service(state: AppState) -> Admit<TraceServiceServer<GrpcTraces>> {
    let limit =
        usize::try_from(state.config.ingest.max_grpc_message_bytes.as_u64()).unwrap_or(usize::MAX);
    let inner = TraceServiceServer::new(GrpcTraces {
        state: state.clone(),
    })
    .max_decoding_message_size(limit)
    .accept_compressed(CompressionEncoding::Gzip);
    AdmitLayer { state }.layer(inner)
}

/// The admission checks, in order: the credential, the key's scope, the project, the org's cap,
/// then the budget for a message of the largest size. The body is not read.
async fn admit(
    state: &AppState,
    headers: &axum::http::HeaderMap,
) -> Result<GrpcAdmitted, tonic::Status> {
    let principal = bearer_principal(state, headers)
        .await
        .map_err(|error| api_status(&error))?;
    let Principal::ApiKey {
        project_id, scopes, ..
    } = principal
    else {
        return Err(denied());
    };
    if !scopes.contains(KeyScope::Ingest) {
        return Err(denied());
    }
    let project = key_only_project(state, project_id)
        .await
        .map_err(|error| api_status(&error))?;
    if let Err(cap) = check_cap(state, project.org_id).await {
        return Err(tonic::Status::resource_exhausted(cap.detail));
    }
    let reservation = state
        .ingest
        .reserve_exact(state.config.ingest.max_grpc_message_bytes.as_u64())?;
    Ok(GrpcAdmitted {
        project,
        reservation: Arc::new(Mutex::new(Some(reservation))),
    })
}

/// Steps of the `export` contract after admission: take the reservation, fit it to the
/// message, accept, check the rate, commit, answer. The first failing step decides the status.
async fn export_request(
    state: &AppState,
    request: tonic::Request<ExportTraceServiceRequest>,
    started: Instant,
) -> Result<ExportTraceServiceResponse, tonic::Status> {
    let now = state.clock.now_ns();
    let (project, mut reservation) = take_admitted(&request)?;
    let message = request.into_inner();
    // The admission reserved the largest message; give back what the decoded one does not use.
    let size = message.encoded_len();
    state
        .ingest
        .fit(&mut reservation, size)
        .map_err(|error| refused(state, error))?;
    let bytes = u64::try_from(size).unwrap_or(u64::MAX);
    let retention_cap = crate::entitlements::of(state, project.org_id).retention_cap_days();
    let Accepted {
        normalized,
        rewritten,
        reservation,
        ..
    } = state
        .ingest
        .accept_request(
            message,
            reservation,
            bytes,
            bytes,
            &project,
            retention_cap,
            now,
        )
        .await
        .map_err(|error| refused(state, error))?;
    check_rate(state, project.org_id, normalized.spans.len(), now)
        .map_err(|error| refused(state, error))?;
    let rejected = normalized.rejections.total();
    let partial = partial_success(&normalized.rejections).map(|(rejected_spans, error_message)| {
        ExportTracePartialSuccess {
            rejected_spans,
            error_message,
        }
    });
    let target = CommitTarget {
        org_id: project.org_id,
        project_id: project.id,
        request_bytes: bytes,
        now_ns: now,
    };
    let receipt = commit_in_task(state, target, normalized, reservation)
        .await
        .map_err(|error| refused(state, error))?;
    state.telemetry.accept_ingest(
        Protocol::otlp_grpc,
        u64::try_from(receipt.accepted).unwrap_or(u64::MAX),
        bytes,
    );
    tracing::info!(
        target: "ingest",
        transport = "grpc",
        project_id = %project.id,
        request_bytes = bytes,
        spans = receipt.accepted,
        rejected,
        media = receipt.media,
        rewritten,
        duration_ms = elapsed_ms(started),
        "request accepted"
    );
    Ok(ExportTraceServiceResponse {
        partial_success: partial,
    })
}

/// The project and the reservation the layer admitted. Without them `export` was not reached
/// through the layer, which is a defect, not a client error.
fn take_admitted(
    request: &tonic::Request<ExportTraceServiceRequest>,
) -> Result<(ProjectRow, Reservation), tonic::Status> {
    let admitted = request
        .extensions()
        .get::<GrpcAdmitted>()
        .ok_or_else(internal_error)?;
    let reservation = admitted
        .reservation
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take()
        .ok_or_else(internal_error)?;
    Ok((admitted.project.clone(), reservation))
}

/// Logs a refused request: rate and cap refusals at `debug`, the rest at `warn`.
fn log_refusal(status: &tonic::Status, started: Instant) {
    // Rate and cap refusals are expected for a client over its limit: `debug`.
    if status.code() == Code::ResourceExhausted || status.message() == RATE_LIMITED_MESSAGE {
        tracing::debug!(
            target: "ingest",
            transport = "grpc",
            code = ?status.code(),
            duration_ms = elapsed_ms(started),
            reason = status.message(),
            "request refused"
        );
    } else {
        tracing::warn!(
            target: "ingest",
            transport = "grpc",
            code = ?status.code(),
            duration_ms = elapsed_ms(started),
            reason = status.message(),
            "request refused"
        );
    }
}

/// Counts a refused ingest step, then converts it to its status. The outcome comes from the
/// variant, because the status alone cannot tell `disk_full` or a rate refusal from a busy budget.
fn refused(state: &AppState, error: IngestError) -> tonic::Status {
    state
        .telemetry
        .refuse_ingest(Protocol::otlp_grpc, ingest_outcome(&error));
    tonic::Status::from(error)
}

/// The outcome of a refused admission: the credential, the scope, the project or the cap.
fn admit_outcome(status: &tonic::Status) -> IngestOutcome {
    match status.code() {
        Code::Unauthenticated | Code::PermissionDenied => IngestOutcome::unauthorized,
        Code::ResourceExhausted => IngestOutcome::quota,
        _ => IngestOutcome::unavailable,
    }
}

fn denied() -> tonic::Status {
    tonic::Status::permission_denied("credentials do not allow ingest")
}

fn internal_error() -> tonic::Status {
    tonic::Status::internal("internal error")
}

/// The status of an `ApiError` from the credential or the project check.
fn api_status(error: &ApiError) -> tonic::Status {
    match error {
        ApiError::Unauthorized => tonic::Status::unauthenticated("missing or invalid credentials"),
        ApiError::Forbidden { .. } => denied(),
        ApiError::NotFound => tonic::Status::not_found("project not found"),
        _ => internal_error(),
    }
}

/// The status of a failed ingest step (design § gRPC status mapping).
impl From<IngestError> for tonic::Status {
    fn from(error: IngestError) -> Self {
        match error {
            IngestError::TooManySpans => {
                tonic::Status::invalid_argument("too many spans in one request")
            }
            IngestError::Media(MediaError::Invalid { .. }) => {
                tonic::Status::invalid_argument("invalid media in the request")
            }
            IngestError::RateLimited { .. } => tonic::Status::unavailable(RATE_LIMITED_MESSAGE),
            IngestError::Busy
            | IngestError::Append(
                AppendError::Backpressure
                | AppendError::WalFailed
                | AppendError::ShuttingDown
                | AppendError::DiskFull,
            )
            | IngestError::Media(
                MediaError::DiskFull
                | MediaError::Object(_)
                | MediaError::Catalog(_)
                | MediaError::Query(_),
            ) => tonic::Status::unavailable("server is busy, retry later"),
            IngestError::Append(AppendError::ProjectDropped)
            | IngestError::Media(MediaError::ProjectDropped) => {
                tonic::Status::not_found("project not found")
            }
            IngestError::Body(_)
            | IngestError::Decode(_)
            | IngestError::Media(
                MediaError::HashMismatch
                | MediaError::NotFound
                | MediaError::InvalidRange
                | MediaError::Fault,
            )
            | IngestError::Append(AppendError::InvalidBatch { .. })
            | IngestError::Batch(_)
            | IngestError::MediaHash
            | IngestError::Join(_) => internal_error(),
        }
    }
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use crate::test_support::raw_query;
    use bytes::Bytes;
    use datafusion::prelude::SessionContext;
    use http_body_util::{BodyExt, Full};
    use llmobs_otel::decode_json;
    use opentelemetry_proto::tonic::collector::trace::v1::trace_service_server::TraceService;
    use tonic::Code;
    use tower::ServiceExt;

    use super::*;
    use crate::test_support::{Seeded, TestApp, set_limits};

    const TRACES: &str =
        include_str!("../../../../fixtures/otlp/genai/finish-reasons-fill/input.json");

    const EXPORT_PATH: &str = "/opentelemetry.proto.collector.trace.v1.TraceService/Export";

    fn traces() -> ExportTraceServiceRequest {
        decode_json(TRACES.as_bytes()).expect("fixture decodes")
    }

    fn span_count(request: &ExportTraceServiceRequest) -> usize {
        request
            .resource_spans
            .iter()
            .flat_map(|resource| resource.scope_spans.iter())
            .map(|scope| scope.spans.len())
            .sum()
    }

    fn bearer(key: &str) -> (&'static str, String) {
        ("authorization", format!("Bearer {key}"))
    }

    /// Sends `message` through the layered service with `headers`, as tonic's client does: one
    /// uncompressed gRPC frame in the body. Returns the reply, or the status the call ended with.
    async fn send(
        state: &AppState,
        headers: &[(&str, String)],
        message: &ExportTraceServiceRequest,
    ) -> Result<ExportTraceServiceResponse, tonic::Status> {
        let payload = message.encode_to_vec();
        let length = u32::try_from(payload.len()).expect("the fixture is small");
        let mut frame = vec![0_u8];
        frame.extend_from_slice(&length.to_be_bytes());
        frame.extend_from_slice(&payload);
        let mut request = axum::http::Request::post(EXPORT_PATH)
            .header(axum::http::header::CONTENT_TYPE, "application/grpc");
        for (name, value) in headers {
            request = request.header(*name, value.as_str());
        }
        let request = request
            .body(tonic::body::Body::new(Full::new(Bytes::from(frame))))
            .expect("the request builds");
        let response = service(state.clone())
            .oneshot(request)
            .await
            .expect("the service never fails");
        let (parts, body) = response.into_parts();
        let collected = body.collect().await?;
        let trailers = collected.trailers().cloned();
        let status = trailers
            .as_ref()
            .and_then(tonic::Status::from_header_map)
            .or_else(|| tonic::Status::from_header_map(&parts.headers))
            .expect("the answer carries a gRPC status");
        if status.code() != Code::Ok {
            return Err(status);
        }
        let reply = collected.to_bytes();
        Ok(ExportTraceServiceResponse::decode(reply.slice(5..)).expect("the reply decodes"))
    }

    /// Spans in the project, read through the storage query provider.
    async fn stored_spans(app: &TestApp, project: llmobs_core::ids::ProjectId) -> usize {
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

    fn assert_status(status: &tonic::Status, code: Code, message: &str) {
        assert_eq!(status.code(), code, "{status:?}");
        assert_eq!(status.message(), message);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_ingest_key_exports_and_is_counted() {
        let app = TestApp::new().await;
        let seeded: Seeded = app.seed().await;
        let message = traces();
        let spans = span_count(&message);
        let bytes = u64::try_from(message.encoded_len()).unwrap_or(u64::MAX);
        let reply = send(&app.state, &[bearer(&seeded.ingest_key)], &message)
            .await
            .expect("exported");
        assert_eq!(reply.partial_success, None);
        assert_eq!(stored_spans(&app, seeded.project.id).await, spans);

        app.state
            .usage
            .flush(&app.state.meta)
            .await
            .expect("usage flushes");
        let counted: (i64, i64) =
            sqlx::query_as("SELECT spans, bytes FROM usage_daily WHERE project_id = ?")
                .bind(seeded.project.id.to_string())
                .fetch_one(app.state.meta.reader())
                .await
                .expect("one usage row for the day");
        assert_eq!(
            counted,
            (
                i64::try_from(spans).unwrap_or(i64::MAX),
                i64::try_from(bytes).unwrap_or(i64::MAX)
            )
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn refused_credentials_get_their_static_status_and_store_nothing() {
        let app = TestApp::new().await;
        let seeded: Seeded = app.seed().await;

        let anonymous = send(&app.state, &[], &traces())
            .await
            .expect_err("no credentials");
        assert_status(
            &anonymous,
            Code::Unauthenticated,
            "missing or invalid credentials",
        );

        let read = send(&app.state, &[bearer(&seeded.read_key)], &traces())
            .await
            .expect_err("a read key cannot ingest");
        assert_status(
            &read,
            Code::PermissionDenied,
            "credentials do not allow ingest",
        );

        let session = send(
            &app.state,
            &[("cookie", seeded.cookie.to_string())],
            &traces(),
        )
        .await
        .expect_err("a session cookie is not a credential here");
        assert_status(
            &session,
            Code::Unauthenticated,
            "missing or invalid credentials",
        );

        assert_eq!(stored_spans(&app, seeded.project.id).await, 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn export_without_admission_is_an_internal_error() {
        let app = TestApp::new().await;
        let service = GrpcTraces {
            state: app.state.clone(),
        };
        let status = service
            .export(tonic::Request::new(traces()))
            .await
            .expect_err("nothing admitted the request");
        assert_status(&status, Code::Internal, "internal error");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn capped_org_gets_resource_exhausted() {
        let app = TestApp::new().await;
        let seeded: Seeded = app.seed().await;
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
        let status = send(&app.state, &[bearer(&seeded.ingest_key)], &traces())
            .await
            .expect_err("the org is at its cap");
        assert_eq!(status.code(), Code::ResourceExhausted);
        assert!(status.message().contains("10 units"), "{status:?}");
        assert_eq!(stored_spans(&app, seeded.project.id).await, 0);
    }

    #[tokio::test]
    async fn rate_limited_org_gets_unavailable() {
        let app = TestApp::new().await;
        let seeded: Seeded = app.seed().await;
        set_limits(
            &app,
            seeded.org_id,
            "2025-12-01T00:00:00Z",
            &serde_json::json!({"ingest_spans_per_sec": 1, "ingest_burst": 1}),
        );
        // The first export spends the one token: the bucket holds one span's worth.
        send(&app.state, &[bearer(&seeded.ingest_key)], &traces())
            .await
            .expect("the first export is within the rate");
        let status = send(&app.state, &[bearer(&seeded.ingest_key)], &traces())
            .await
            .expect_err("the bucket is empty");
        assert_eq!(status.code(), Code::Unavailable);
        assert_eq!(status.message(), "rate limited");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn accepted_grpc_export_is_counted() {
        let app = TestApp::new().await;
        let seeded: Seeded = app.seed().await;
        let message = traces();
        let spans = span_count(&message);
        let bytes = message.encoded_len();
        send(&app.state, &[bearer(&seeded.ingest_key)], &message)
            .await
            .expect("exported");
        let text = app
            .state
            .telemetry
            .render(&app.state)
            .await
            .expect("metrics render");
        let expected = [
            "llmobs_ingest_requests_total{protocol=\"otlp_grpc\",outcome=\"ok\"} 1".to_owned(),
            format!("llmobs_ingest_spans_total{{protocol=\"otlp_grpc\"}} {spans}"),
            format!("llmobs_ingest_bytes_total{{protocol=\"otlp_grpc\"}} {bytes}"),
        ];
        for line in expected {
            assert!(
                text.lines().any(|candidate| candidate == line),
                "{line}\n{text}"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn refused_grpc_credentials_are_counted() {
        let app = TestApp::new().await;
        send(&app.state, &[], &traces())
            .await
            .expect_err("no credentials");
        let text = app
            .state
            .telemetry
            .render(&app.state)
            .await
            .expect("metrics render");
        let line =
            "llmobs_ingest_requests_total{protocol=\"otlp_grpc\",outcome=\"unauthorized\"} 1";
        assert!(text.lines().any(|candidate| candidate == line), "{text}");
    }
}
