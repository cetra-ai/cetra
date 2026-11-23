//! axum router, shared state, and the request log (D150).
//!
//! `cetra serve` binds [`router`] with
//! `into_make_service_with_connect_info::<SocketAddr>()` so [`ClientIp`] can
//! read the socket peer. That binding is P5.
#![cfg_attr(not(test), allow(dead_code))] // `cetra serve` binds this in P5

pub(crate) mod access;
pub(crate) mod client_ip;
mod csrf;
pub(crate) mod error;
pub(crate) mod health;
pub(crate) mod restore;

use std::sync::Arc;
use std::time::Instant;

use tokio::sync::OnceCell;

use axum::Router;
use axum::extract::{DefaultBodyLimit, MatchedPath, Request};
use axum::middleware::{self, Next};
use axum::response::Response;
use cetra_core::ids::ProjectId;
use cetra_storage::Storage;
use tower::ServiceExt;
use tracing::Instrument as _;

use crate::auth::Limits;
use crate::auth::last_used::LastUsed;
use crate::auth::principal::{ApiKeyCache, PatCache, authenticate};
use crate::clock::Clock;
use crate::config::ServerConfig;
use crate::meta::MetaDb;
use crate::observability::spans;
use crate::routes::dashboards::WidgetCache;

/// Bodies on this router. Later tracks raise it on a single route.
const BODY_LIMIT: usize = 1024 * 1024;

/// Shared server state. Cloning copies the `Arc`.
#[derive(Clone)]
pub(crate) struct AppState(Arc<AppInner>);

pub(crate) struct AppInner {
    pub(crate) config: ServerConfig,
    pub(crate) meta: MetaDb,
    pub(crate) storage: Arc<Storage>,
    /// The last object-store probe behind `/readyz`, cached 10 s (D-N-36).
    pub(crate) ready: health::ReadyCache,
    pub(crate) clock: Clock,
    pub(crate) limits: Limits,
    /// Device-flow codes and their grants, in memory only (D-AO-3, D-AO-5).
    pub(crate) device: crate::auth::device::DeviceStore,
    /// Successful API-key lookups, cached 60 s by hash.
    pub(crate) key_cache: ApiKeyCache,
    /// Successful personal-token lookups, cached 60 s by hash. The user is not cached.
    pub(crate) pat_cache: PatCache,
    /// Coalesced `api_keys.last_used_at` updates.
    pub(crate) last_used: LastUsed,
    /// Coalesced `personal_tokens.last_used_at` updates.
    pub(crate) token_last_used: LastUsed,
    pub(crate) setup: crate::bootstrap::SetupState,
    /// Built on the first password check, so tests that never log in skip argon2.
    pub(crate) password_hasher: OnceCell<crate::auth::password::Hasher>,
    /// `None` when `smtp_url` is unset. Shared so a send can outlive the request.
    pub(crate) mailer: Option<Arc<crate::email::Mailer>>,
    /// OAuth and OIDC providers from `[auth]`. Discovery has not run yet.
    pub(crate) providers: crate::auth::oidc::Providers,
    /// HMAC key for the OAuth state cookie. A new process mints a new key.
    pub(crate) oauth_key: [u8; 32],
    /// OTLP ingest core: budget, prices, `accept` and `commit` (D-G-1).
    pub(crate) ingest: crate::ingest::Ingest,
    /// When each project last received OTLP spans (D-C-20), in memory.
    pub(crate) last_ingest: crate::ingest::last_ingest::LastIngest,
    /// Live bus and stream slots (D-G-12).
    pub(crate) live: crate::live::LiveBus,
    /// `POST /dashboards/query` answers cached 10 s (spec 05 § Dashboard queries).
    pub(crate) widget_cache: WidgetCache,
    /// Usage counters, flushed every 10 s (D-G-11).
    pub(crate) usage: crate::usage::Usage,
    /// Alert emails sent per org on the current UTC day (D-K-19).
    pub(crate) alert_email_count: crate::alerts::quota::DailyEmailCount,
    /// Deletion worker state: wake-up, guard, attempts (D-M-3).
    pub(crate) deletions: crate::deletion::Deletions,
    /// Seals and opens secret columns (D-I-2).
    pub(crate) secrets: crate::crypto::SecretBox,
    /// One egress client for judges, webhooks and connection tests (D-I-29).
    pub(crate) eval_runner: Arc<cetra_eval::EvalRunner>,
    /// Decrypted connections, keyed by id and `updated_at`.
    pub(crate) connections: crate::eval::connection::ConnectionCache,
    /// Trace-load permits (D-I-23).
    pub(crate) trace_loads: crate::eval::trace_load::TraceLoads,
    /// Live score definitions (D-I-26).
    pub(crate) definitions: crate::eval::definitions::DefinitionCache,
    /// Licence and (from M2) org entitlements (D179, D-P-10).
    pub(crate) entitlements: crate::entitlements::EntitlementsState,
    /// The single backup slot: a second backup answers `busy` (N M2, D-N-19).
    pub(crate) backup_slot: tokio::sync::Mutex<()>,
    /// The eval queue's in-memory state (D-I-6).
    pub(crate) eval: crate::eval::EvalQueue,
    /// Traces the online-rule trigger waits on (D-I-8).
    pub(crate) pending: crate::eval::pending::PendingTraces,
    /// Per-org span rate (D-P-19, D-P-35).
    pub(crate) ingest_rate: crate::entitlements::rate::IngestRate,
    /// Per-org SSO routing (track P M5).
    pub(crate) org_sso: crate::ee::org_sso::OrgSso,
    /// The Prometheus registry served on `metrics_addr` (N M3, D-N-28).
    pub(crate) telemetry: crate::telemetry::Telemetry,
    /// The `score` and `feedback` issue sources' rules, read by each score write (D-IC-4).
    pub(crate) issue_score_rules: crate::issues::triggers::ScoreRules,
    /// The IC candidate worker slots (`issues.workers`, D-IC-12).
    pub(crate) issue_workers: Arc<tokio::sync::Semaphore>,
    /// Wakes the IC candidate loop: a candidate was queued, or a worker finished.
    pub(crate) issue_wake: tokio::sync::Notify,
    /// Product analytics: feature counters and the PostHog sink (track TL M6).
    pub(crate) analytics: crate::analytics::Analytics,
}

impl AppState {
    pub(crate) fn new(inner: AppInner) -> Self {
        Self(Arc::new(inner))
    }

    /// Wakes the IC candidate loop (D-IC-12).
    pub(crate) fn issues_wake(&self) {
        self.issue_wake.notify_one();
    }

    /// Drops the cached answers of `project` (D-M-11).
    pub(crate) fn clear_answers(&self, project: ProjectId) {
        self.widget_cache.clear_project(project);
    }

    /// The state when this is the last clone. The router holds the other one
    /// until its tasks finish.
    pub(crate) fn into_inner(self) -> Result<AppInner, Self> {
        match Arc::try_unwrap(self.0) {
            Ok(inner) => Ok(inner),
            Err(arc) => Err(Self(arc)),
        }
    }
}

impl std::ops::Deref for AppState {
    type Target = AppInner;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl AppState {
    /// The process hasher, created on the first password check.
    ///
    /// # Errors
    ///
    /// Returns [`error::ApiError::Internal`] when the dummy hash cannot be computed.
    pub(crate) async fn passwords(
        &self,
    ) -> Result<&crate::auth::password::Hasher, error::ApiError> {
        self.password_hasher
            .get_or_try_init(crate::auth::password::Hasher::new)
            .await
            .map_err(|error| error::ApiError::Internal(Box::new(error)))
    }
}

/// Health probes and the shared layers. `ui` answers every path that is not an API path.
pub(crate) fn router(state: AppState, ui: Option<Router>) -> Router {
    let routed = access::build_router(&state, access::routes());
    finish(state, routed, ui)
}

/// [`router`] plus routes that tests add before the body limit and the log.
pub(crate) fn router_with(state: AppState, extra: Router<AppState>) -> Router {
    let routed = access::build_router(&state, access::routes()).merge(extra);
    finish(state, routed, None)
}

/// The route table, then the shared layers. Tests pass their own table.
pub(crate) fn serve_router(state: AppState, routes: Vec<access::RouteDef>) -> Router {
    let routed = access::build_router(&state, routes);
    finish(state, routed, None)
}

/// The OTLP/HTTP listener's router: the OTLP routes `/v1/traces`, `/v1/logs` and `/v1/metrics`,
/// `/v1/agent-context` (D-CA-9), and nothing else (D-G-21, D-G-40).
pub(crate) fn otlp_router(state: AppState) -> Router {
    serve_router(state, crate::routes::otlp::routes())
}

fn finish(state: AppState, routed: Router<AppState>, ui: Option<Router>) -> Router {
    // serde_json's default recursion limit is 128, which is spec 12's JSON depth limit.
    // Logging is the outer layer. Authentication runs next, then the origin check (D141).
    routed
        .fallback(move |request: Request| fallback(ui.clone(), request))
        .method_not_allowed_fallback(method_not_allowed)
        .layer(DefaultBodyLimit::max(BODY_LIMIT))
        .layer(middleware::from_fn_with_state(state.clone(), csrf::csrf))
        .layer(middleware::from_fn_with_state(state.clone(), authenticate))
        // DNT and Sec-GPC for the length of the request (D-TL-15).
        .layer(middleware::from_fn(crate::analytics::actor::privacy_signal))
        .layer(middleware::from_fn(log_request))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            crate::telemetry::track_http,
        ))
        .with_state(state)
}

/// Unknown API paths are `404` problems; any other path goes to the UI when there is one.
async fn fallback(ui: Option<Router>, request: Request) -> Response {
    let path = request.uri().path();
    let api = matches!(path, "/api" | "/v1" | "/mcp" | "/healthz" | "/readyz")
        || path.starts_with("/api/")
        || path.starts_with("/v1/")
        || path.starts_with("/mcp/");
    match ui {
        Some(ui) if !api => match ui.oneshot(request).await {
            Ok(response) => response,
            Err(never) => match never {},
        },
        _ => not_found().await,
    }
}

async fn not_found() -> Response {
    error::problem_response(&error::not_found_problem())
}

async fn method_not_allowed() -> Response {
    error::problem_response(&error::method_not_allowed_problem())
}

/// One `info` event per request, and the `http.request` span around the handler and that
/// event, so events inside nest under it (track TL M2). The route is the matched template (D150); the URL,
/// its query and the body are never recorded. The health probes get no span, nor does the
/// UI's analytics beacon (track TL M6): they are polled every few seconds. A 5xx marks the
/// span failed.
async fn log_request(request: Request, next: Next) -> Response {
    let method = request.method().clone();
    let route = request
        .extensions()
        .get::<MatchedPath>()
        .map_or("<unmatched>", MatchedPath::as_str)
        .to_owned();
    let span = if matches!(
        route.as_str(),
        "/healthz" | "/readyz" | "/api/v1/analytics/events"
    ) {
        tracing::Span::none()
    } else {
        spans::http_request(&method, &route)
    };
    let started = Instant::now();
    let response = next.run(request).instrument(span.clone()).await;
    let status = response.status().as_u16();
    span.record(spans::STATUS_CODE, i64::from(status));
    if response.status().is_server_error() {
        span.record(spans::OTEL_STATUS, "error");
    }
    let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    // Inside the span, so `self_monitoring.level = "info"` records it on `http.request`.
    // The stdout layer sees no span, so the stdout line is byte-identical.
    span.in_scope(|| {
        tracing::info!(
            target: "http",
            %method,
            %route,
            status,
            duration_ms,
            "request"
        );
    });
    response
}

#[cfg(test)]
mod tests {
    use axum::Router;
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use axum::routing::{get, post};
    use opentelemetry_proto::tonic::trace::v1::Span as ProtoSpan;
    use opentelemetry_proto::tonic::trace::v1::span::SpanKind;
    use opentelemetry_proto::tonic::trace::v1::status::StatusCode as ProtoStatusCode;
    use serde::Deserialize;

    use super::client_ip::ClientIp;
    use super::error::{ApiError, Json, Path, Query};
    use crate::api::common::ProblemCode;
    use crate::observability::otlp::receiver::{Captured, int_attribute, string_attribute};
    use crate::observability::subscriber::capture_spans;
    use crate::test_support::{TestApp, assert_logs_clean, capture_logs};

    #[tokio::test(flavor = "current_thread")]
    async fn health_probes() {
        let app = TestApp::new().await;
        let live = app
            .request(axum::http::Method::GET, "/healthz")
            .send()
            .await;
        assert_eq!(live.status, 200);
        let body: serde_json::Value = live.json();
        assert_eq!(body["status"], "ok");

        let ready = app.request(axum::http::Method::GET, "/readyz").send().await;
        assert_eq!(ready.status, 200);
        let body: serde_json::Value = ready.json();
        assert_eq!(body["status"], "ready");

        app.state.meta.reader().close().await;
        let down = app.request(axum::http::Method::GET, "/readyz").send().await;
        assert_eq!(down.status, 503);
        assert_eq!(
            down.headers.get("content-type").unwrap(),
            "application/problem+json"
        );
        let problem = down.problem();
        assert_eq!(problem.code, ProblemCode::Busy);
        assert_eq!(problem.detail.as_deref(), Some("meta.db"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn unknown_path_and_method_are_problems() {
        let app = TestApp::new().await;
        let missing = app
            .request(axum::http::Method::GET, "/missing")
            .send()
            .await;
        assert_eq!(missing.status, 404);
        assert_eq!(
            missing.headers.get("content-type").unwrap(),
            "application/problem+json"
        );
        assert_eq!(missing.problem().code, ProblemCode::NotFound);

        let wrong = app
            .request(axum::http::Method::POST, "/healthz")
            .send()
            .await;
        assert_eq!(wrong.status, 405);
        assert_eq!(
            wrong.headers.get("content-type").unwrap(),
            "application/problem+json"
        );
        assert_eq!(wrong.problem().code, ProblemCode::MethodNotAllowed);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn json_rejections_hide_the_input() {
        let app = TestApp::with_routes(Router::new().route("/json", post(take_json))).await;

        let typed = app
            .request(axum::http::Method::POST, "/json")
            .header("content-type", "text/plain")
            .body("{\"secret\": tru")
            .send()
            .await;
        assert_eq!(typed.status, 415);
        assert_eq!(typed.problem().code, ProblemCode::UnsupportedMediaType);
        assert!(!typed_body_contains(&typed, "tru"));

        let invalid = app
            .request(axum::http::Method::POST, "/json")
            .header("content-type", "application/json")
            .body("{\"secret\": tru")
            .send()
            .await;
        assert_eq!(invalid.status, 400);
        assert_eq!(invalid.problem().code, ProblemCode::BadRequest);
        let detail = invalid.problem().detail.unwrap();
        assert!(detail.contains("invalid JSON body"), "{detail}");
        assert!(!detail.contains("tru"), "{detail}");

        let big = vec![b'{'; 1024 * 1024 + 1];
        let limited = app
            .request(axum::http::Method::POST, "/json")
            .header("content-type", "application/json")
            .body(big)
            .send()
            .await;
        assert_eq!(limited.status, 413);
        assert_eq!(limited.problem().code, ProblemCode::PayloadTooLarge);

        let accepted = app
            .request(axum::http::Method::POST, "/json")
            .json(&serde_json::json!({"ok": true}))
            .origin(None)
            .send()
            .await;
        assert_eq!(accepted.status, 200);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn path_and_query_details_omit_the_value() {
        let app = TestApp::with_routes(Router::new().route("/item/{id}", get(take_path))).await;
        let path = app
            .request(axum::http::Method::GET, "/item/secret-value")
            .send()
            .await;
        assert_eq!(path.status, 400);
        let detail = path.problem().detail.unwrap();
        assert!(!detail.contains("secret-value"), "{detail}");

        let query = app
            .request(axum::http::Method::GET, "/item/1?q=secret-value")
            .send()
            .await;
        assert_eq!(query.status, 400);
        let detail = query.problem().detail.unwrap();
        assert!(detail.contains("invalid query"), "{detail}");
        assert!(!detail.contains("secret-value"), "{detail}");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn request_log_uses_the_route_template() {
        let app = TestApp::with_routes(Router::new().route("/echo/{id}", get(echo))).await;
        let response = assert_logs_clean(&["super-secret-token", "secret.example"], {
            app.request(axum::http::Method::GET, "/echo/super-secret-token")
                .origin(Some("https://secret.example"))
                .send()
        })
        .await;
        assert_eq!(response.status, 200);
        let (_body, logs) = capture_logs(
            app.request(axum::http::Method::GET, "/echo/super-secret-token")
                .send(),
        )
        .await;
        assert!(logs.contains("/echo/{id}"), "{logs}");
        assert!(!logs.contains("super-secret-token"), "{logs}");
    }

    async fn inside() -> &'static str {
        tracing::info!(n = 1_i64, "inside");
        "ok"
    }

    async fn boom() -> StatusCode {
        tracing::error!(error = "boom", "handler failed");
        StatusCode::INTERNAL_SERVER_ERROR
    }

    async fn quiet_500() -> StatusCode {
        StatusCode::INTERNAL_SERVER_ERROR
    }

    async fn take_body(_body: axum::body::Bytes) -> StatusCode {
        StatusCode::NO_CONTENT
    }

    /// The `http.request` span of the request whose matched route is `route`.
    fn request_span(captured: &Captured, route: &str) -> ProtoSpan {
        captured
            .named("http.request")
            .into_iter()
            .find(|span| string_attribute(&span.attributes, "http.route").as_deref() == Some(route))
            .unwrap_or_else(|| panic!("no http.request span for {route}"))
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_request_runs_in_an_http_request_span() {
        let app = TestApp::with_routes(Router::new().route("/spans/{id}", get(inside))).await;
        let (response, captured) =
            capture_spans(app.request(axum::http::Method::GET, "/spans/42").send()).await;
        assert_eq!(response.status, 200);
        let span = request_span(&captured, "/spans/{id}");
        assert_eq!(
            string_attribute(&span.attributes, "http.request.method").as_deref(),
            Some("GET")
        );
        assert_eq!(
            int_attribute(&span.attributes, "http.response.status_code"),
            Some(200)
        );
        assert_eq!(span.kind, SpanKind::Server as i32);
        assert_ne!(
            span.status.as_ref().map_or(0, |status| status.code),
            ProtoStatusCode::Error as i32
        );
        let events: Vec<&str> = span
            .events
            .iter()
            .map(|event| event.name.as_str())
            .collect();
        assert!(events.contains(&"inside"), "{events:?}");
        assert!(events.contains(&"request"), "{events:?}");
        assert!(!captured.text().contains("/spans/42"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_server_error_marks_the_request_span_as_error() {
        let app = TestApp::with_routes(
            Router::new()
                .route("/boom", get(boom))
                .route("/quiet-500", get(quiet_500)),
        )
        .await;
        let ((), captured) = capture_spans(async {
            let boom = app.request(axum::http::Method::GET, "/boom").send().await;
            assert_eq!(boom.status, 500);
            let quiet = app
                .request(axum::http::Method::GET, "/quiet-500")
                .send()
                .await;
            assert_eq!(quiet.status, 500);
        })
        .await;
        let failed = request_span(&captured, "/boom");
        assert_eq!(
            failed.status.as_ref().map(|status| status.code),
            Some(ProtoStatusCode::Error as i32)
        );
        assert!(
            failed
                .events
                .iter()
                .any(|event| event.name == "handler failed"),
            "{failed:?}"
        );
        let quiet = request_span(&captured, "/quiet-500");
        assert_eq!(
            quiet.status.as_ref().map(|status| status.code),
            Some(ProtoStatusCode::Error as i32)
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_client_error_leaves_the_request_span_unset() {
        let app = TestApp::new().await;
        let (response, captured) =
            capture_spans(app.request(axum::http::Method::GET, "/missing").send()).await;
        assert_eq!(response.status, 404);
        let span = request_span(&captured, "<unmatched>");
        assert_eq!(
            int_attribute(&span.attributes, "http.response.status_code"),
            Some(404)
        );
        assert_ne!(
            span.status.as_ref().map_or(0, |status| status.code),
            ProtoStatusCode::Error as i32
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn request_spans_never_carry_the_url_query_or_body() {
        let app = TestApp::with_routes(Router::new().route("/spans/{id}", post(take_body))).await;
        let (response, captured) = capture_spans(
            app.request(
                axum::http::Method::POST,
                "/spans/sk-cetra-planted00000000000000000000000000?token=sk-cetra-planted11111111111111111111111111&email=user@example.com",
            )
            .body(r#"{"prompt":"secret prompt text","key":"sk-cetra-planted22222222222222222222222222"}"#)
            .origin(None)
            .send(),
        )
        .await;
        assert_eq!(response.status, 204);
        let text = captured.text();
        for secret in ["sk-cetra-planted", "user@example.com", "secret prompt text"] {
            assert!(!text.contains(secret), "{text}");
        }
        request_span(&captured, "/spans/{id}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn health_probes_have_no_span() {
        // The real analytics route (M6) is in the router already, so a test route at its path
        // would be a duplicate: the beacon is posted with a session, as the UI posts it.
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let ((), captured) = capture_spans(async {
            let live = app
                .request(axum::http::Method::GET, "/healthz")
                .send()
                .await;
            assert_eq!(live.status, 200);
            let ready = app.request(axum::http::Method::GET, "/readyz").send().await;
            assert_eq!(ready.status, 200);
            let beacon = app
                .request(axum::http::Method::POST, "/api/v1/analytics/events")
                .header("cookie", &seeded.cookie)
                .json(&serde_json::json!({"events": []}))
                .send()
                .await;
            assert_eq!(beacon.status, 204);
            // A request that does get a span, so the empty list above means something.
            let missing = app
                .request(axum::http::Method::GET, "/missing")
                .send()
                .await;
            assert_eq!(missing.status, 404);
        })
        .await;
        let spans = captured.named("http.request");
        assert_eq!(spans.len(), 1, "{spans:?}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_incoming_traceparent_is_ignored() {
        let app = TestApp::with_routes(Router::new().route("/spans/{id}", get(inside))).await;
        let ((), captured) = capture_spans(async {
            let response = app
                .request(axum::http::Method::GET, "/spans/1")
                .header(
                    "traceparent",
                    "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01",
                )
                .send()
                .await;
            assert_eq!(response.status, 200);
        })
        .await;
        let span = request_span(&captured, "/spans/{id}");
        let planted: [u8; 16] = [
            0x0a, 0xf7, 0x65, 0x19, 0x16, 0xcd, 0x43, 0xdd, 0x84, 0x48, 0xeb, 0x21, 0x1c, 0x80,
            0x31, 0x9c,
        ];
        assert_ne!(span.trace_id, planted);
        assert!(span.parent_span_id.is_empty());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn client_ip_extractor_reads_the_peer() {
        let app = TestApp::with(
            |config| {
                config.trusted_proxies = vec!["10.0.0.0/8".parse().unwrap()];
            },
            Router::new().route("/who", get(who)),
        )
        .await;
        let response = app
            .request(axum::http::Method::GET, "/who")
            .peer("10.0.0.8".parse().unwrap())
            .header("x-forwarded-for", "203.0.113.9, 10.0.0.2")
            .send()
            .await;
        assert_eq!(response.status, 200);
        assert_eq!(response.body.as_ref(), b"203.0.113.9");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn manual_clock_starts_at_the_epoch() {
        let app = TestApp::with_config(|config| {
            assert!(config.trusted_proxies.is_empty());
        })
        .await;
        assert_eq!(app.state.clock.now_ns(), 1_767_225_600_000_000_000);
    }

    #[test]
    fn error_statuses() {
        assert_eq!(status_of(ApiError::Unauthorized), 401);
        assert_eq!(
            status_of(ApiError::Forbidden {
                code: ProblemCode::Forbidden,
            }),
            403
        );
        assert_eq!(
            status_of(ApiError::Forbidden {
                code: ProblemCode::SsoRequired,
            }),
            403
        );
        assert_eq!(
            status_of(ApiError::Forbidden {
                code: ProblemCode::EmailNotVerified,
            }),
            403
        );
        assert_eq!(status_of(ApiError::PlanRequired { detail: "x" }), 403);
        let quota = ApiError::QuotaExceeded {
            detail: "over the cap".to_owned(),
            retry_after_s: 90,
        }
        .into_response();
        assert_eq!(quota.status(), 402);
        assert_eq!(quota.headers().get("retry-after").unwrap(), "90");
        assert_eq!(status_of(ApiError::NotFound), 404);
        assert_eq!(
            status_of(ApiError::Conflict {
                detail: "already exists",
            }),
            409
        );
        assert_eq!(
            status_of(ApiError::PayloadTooLarge {
                detail: "request body is too large",
            }),
            413
        );
        assert_eq!(
            status_of(ApiError::UnsupportedMediaType {
                detail: "unsupported Content-Encoding",
            }),
            415
        );

        let unavailable = ApiError::Unavailable {
            detail: "server is busy, retry later",
        }
        .into_response();
        assert_eq!(unavailable.status(), 503);
        assert_eq!(unavailable.headers().get("retry-after").unwrap(), "1");
        assert!(unavailable.headers().get("ratelimit-limit").is_none());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn ingest_problems_carry_their_detail() {
        let cases = [
            (
                ApiError::PayloadTooLarge {
                    detail: "too many spans in one request",
                },
                "payload_too_large",
                "too many spans in one request",
            ),
            (
                ApiError::UnsupportedMediaType {
                    detail: "unsupported Content-Encoding",
                },
                "unsupported_media_type",
                "unsupported Content-Encoding",
            ),
            (
                ApiError::Unavailable {
                    detail: "server is busy, retry later",
                },
                "busy",
                "server is busy, retry later",
            ),
        ];
        for (error, code, detail) in cases {
            let response = error.into_response();
            let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            let problem: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(problem["code"], code);
            assert_eq!(problem["detail"], detail);
        }
    }

    fn status_of(error: ApiError) -> u16 {
        error.into_response().status().as_u16()
    }

    #[test]
    fn rate_limited_sets_the_headers() {
        let response = ApiError::RateLimited { retry_after_s: 12 }.into_response();
        assert_eq!(response.status(), 429);
        let headers = response.headers();
        assert_eq!(headers.get("retry-after").unwrap(), "12");
        assert_eq!(headers.get("ratelimit-limit").unwrap(), "12");
        assert_eq!(headers.get("ratelimit-remaining").unwrap(), "0");
        assert_eq!(headers.get("ratelimit-reset").unwrap(), "12");
    }

    async fn take_json(Json(_value): Json<serde_json::Value>) -> Json<serde_json::Value> {
        Json(serde_json::Value::Null)
    }

    async fn take_path(Path(_id): Path<u32>, Query(_query): Query<ItemQuery>) -> &'static str {
        "ok"
    }

    async fn echo(Path(id): Path<String>) -> String {
        id
    }

    async fn who(ClientIp(ip): ClientIp) -> String {
        ip.to_string()
    }

    #[derive(Deserialize)]
    struct ItemQuery {
        #[allow(dead_code)]
        q: u32,
    }

    fn typed_body_contains(response: &crate::test_support::TestResponse, needle: &str) -> bool {
        String::from_utf8_lossy(&response.body).contains(needle)
    }
}
