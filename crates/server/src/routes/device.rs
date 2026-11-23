//! The device authorization routes (RFC 8628, D-AO-3): the CLI asks for a code and polls for
//! its grant; the signed-in user reads, approves or denies the code in the web UI. Spec 08
//! § Device authorization.

use axum::extract::{Extension, State};
use axum::http::{Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use cetra_core::ids::ProjectId;
use uuid::Uuid;

use crate::analytics::actor::Actor;
use crate::analytics::catalog::Event;
use crate::analytics::record;
use crate::api::common::ProblemCode;
use crate::api::device::{
    DeviceApproveRequest, DeviceCodeRequest, DeviceCodeResponse, DeviceGrant, DeviceGrantProject,
    DevicePurpose, DeviceRequest, DeviceTokenRequest, InternalDeviceCode,
};
use crate::auth::device::{
    DeviceError, Grant, GrantProject, POLL_INTERVAL_S, Pending, Poll, Purpose,
};
use crate::auth::limits::network_key;
use crate::auth::principal::{AuthUser, Principal, session_user};
use crate::auth::roles::{KeyScope, ProjectPerm};
use crate::auth::secret::{self, KeyKind, NewKey};
use crate::http::AppState;
use crate::http::access::{Access, RouteDef, user_project};
use crate::http::client_ip::ClientIp;
use crate::http::error::{self, ApiError, field_error};
use crate::meta::MetaError;
use crate::meta::api_keys::{self, NewApiKey};
use crate::meta::orgs::org_by_id;
use crate::meta::personal_tokens::{self, NewToken};

/// `expires_in` of a code: the store's `CODE_TTL_NS`, 10 minutes, in seconds.
const EXPIRES_IN_S: u64 = 600;
/// A grant's token lives 90 days from its approval.
const NINETY_DAYS_NS: i64 = 90 * 24 * 60 * 60 * 1_000_000_000;

/// Device flow routes. The two CLI calls are public, with a per-network limit on the first.
/// The three session calls are session-only: a PAT or a key may not approve a code (D-AO-3).
pub(crate) fn routes() -> Vec<RouteDef> {
    vec![
        RouteDef {
            method: Method::POST,
            path: "/api/auth/device/code",
            access: Access::Public,
            documented: true,
            handler: post(issue_code),
        },
        RouteDef {
            method: Method::POST,
            path: "/api/auth/device/token",
            access: Access::Public,
            documented: true,
            handler: post(poll_token),
        },
        RouteDef {
            method: Method::GET,
            path: "/api/auth/device/{user_code}",
            access: Access::User { session_only: true },
            documented: true,
            handler: get(get_request),
        },
        RouteDef {
            method: Method::POST,
            path: "/api/auth/device/{user_code}/approve",
            access: Access::User { session_only: true },
            documented: true,
            handler: post(approve),
        },
        RouteDef {
            method: Method::POST,
            path: "/api/auth/device/{user_code}/deny",
            access: Access::User { session_only: true },
            documented: true,
            handler: post(deny),
        },
        RouteDef {
            method: Method::POST,
            path: "/api/internal/v1/device/codes",
            access: Access::ControlPlane { scope: "device" },
            documented: true,
            handler: post(register_code),
        },
    ]
}

/// `POST /api/auth/device/code`. Public, so it sends no cookie and no `Origin` (CSRF exempts
/// it, D-AO-3). Ten codes a minute per client network.
async fn issue_code(
    State(state): State<AppState>,
    ClientIp(ip): ClientIp,
    error::Json(body): error::Json<DeviceCodeRequest>,
) -> Result<error::Json<DeviceCodeResponse>, ApiError> {
    let now = state.clock.now_ns();
    state
        .limits
        .device_code
        .check(&network_key(ip), now)
        .map_err(|retry| ApiError::RateLimited {
            retry_after_s: retry.0.as_secs().max(1),
        })?;
    let host = body.host.as_deref().and_then(sanitise_host);
    let issued = state
        .device
        .issue(purpose_in(body.purpose), host, now)
        .map_err(|error| match error {
            DeviceError::Full => ApiError::Unavailable {
                detail: "too many pending device codes",
            },
            other => internal(other),
        })?;
    let public_url = &state.config.public_url;
    let verification_uri = public_url.join("/device");
    let verification_uri_complete = public_url.join(&format!("/device?code={}", issued.user_code));
    Ok(error::Json(DeviceCodeResponse {
        device_code: issued.device_code,
        user_code: issued.user_code,
        verification_uri,
        verification_uri_complete,
        expires_in: EXPIRES_IN_S,
        interval: POLL_INTERVAL_S,
    }))
}

/// `POST /api/auth/device/token`. Public and not limited: the interval and `slow_down` are the
/// limit. A grant is answered once, with `Cache-Control: no-store`.
async fn poll_token(
    State(state): State<AppState>,
    error::Json(body): error::Json<DeviceTokenRequest>,
) -> Result<Response, ApiError> {
    let now = state.clock.now_ns();
    let grant = match state.device.poll(&body.device_code, now) {
        Poll::Pending => {
            return Err(ApiError::DeviceGrant {
                code: ProblemCode::AuthorizationPending,
            });
        }
        Poll::SlowDown => {
            return Err(ApiError::DeviceGrant {
                code: ProblemCode::SlowDown,
            });
        }
        Poll::Denied => {
            return Err(ApiError::DeviceGrant {
                code: ProblemCode::AccessDenied,
            });
        }
        Poll::Expired => {
            return Err(ApiError::DeviceGrant {
                code: ProblemCode::ExpiredToken,
            });
        }
        Poll::Granted(grant) => grant,
    };
    let answer = DeviceGrant {
        server: state.config.public_url.origin().to_owned(),
        token: grant.token,
        project: grant.project.map(project_out),
        ingest_key: grant.ingest_key,
    };
    Ok((
        [
            (header::CACHE_CONTROL, "no-store"),
            (header::PRAGMA, "no-cache"),
        ],
        error::Json(answer),
    )
        .into_response())
}

/// `GET /api/auth/device/{user_code}`: what the approval page shows. 404 for an unknown, expired
/// or already decided code.
async fn get_request(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    error::Path(user_code): error::Path<String>,
) -> Result<error::Json<DeviceRequest>, ApiError> {
    let user = session_user(&principal)?;
    let now = state.clock.now_ns();
    check_lookup(&state, user.id, now)?;
    let pending = state
        .device
        .pending(&user_code, now)
        .ok_or(ApiError::NotFound)?;
    Ok(error::Json(DeviceRequest {
        purpose: purpose_out(pending.purpose),
        host: pending.host,
        created_at: pending.created_at,
    }))
}

/// `POST /api/auth/device/{user_code}/approve`. The code is claimed first and released when the
/// minting fails, so a failed approval leaves the code pending.
async fn approve(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    error::Path(user_code): error::Path<String>,
    error::Json(body): error::Json<DeviceApproveRequest>,
) -> Result<StatusCode, ApiError> {
    let user = session_user(&principal)?;
    let now = state.clock.now_ns();
    check_lookup(&state, user.id, now)?;
    let (pending, ticket) = state
        .device
        .claim(&user_code, now)
        .map_err(|_| ApiError::NotFound)?;
    let (grant, token_id, key) = match mint(&state, user, &pending, body, now).await {
        Ok(minted) => minted,
        Err(failure) => {
            state.device.release(&user_code, ticket);
            return Err(failure);
        }
    };
    // NotFound: the approval went stale while minting and the code moved on. The minted
    // secrets are not handed out.
    state
        .device
        .complete(&user_code, ticket, grant)
        .map_err(|error| match error {
            DeviceError::NotFound => ApiError::NotFound,
            other => internal(other),
        })?;
    match key {
        Some((key_id, project_id)) => {
            tracing::info!(target: "audit", %token_id, %key_id, %project_id, user_id = %user.id, purpose = ?pending.purpose, "device code approved");
        }
        None => {
            tracing::info!(target: "audit", %token_id, user_id = %user.id, purpose = ?pending.purpose, "device code approved");
        }
    }
    record(
        &state,
        &Event::DeviceApproved,
        &Actor::principal(&principal),
    );
    Ok(StatusCode::NO_CONTENT)
}

/// Mints what an approval grants: a `read, write` token for `login`; for `init`, a `read` token
/// and an `ingest` key for the chosen project, in one transaction. Returns the grant, the token's
/// id, and for `init` the key's id and project.
async fn mint(
    state: &AppState,
    user: &AuthUser,
    pending: &Pending,
    body: DeviceApproveRequest,
    now: i64,
) -> Result<(Grant, Uuid, Option<(Uuid, ProjectId)>), ApiError> {
    let host = pending.host.as_deref();
    match pending.purpose {
        Purpose::Login => {
            if body.project_id.is_some() {
                return Err(field_error("project_id", "only for init"));
            }
            let NewKey {
                plaintext,
                hash,
                prefix,
            } = secret::new_key(KeyKind::Personal).map_err(internal)?;
            let token = NewToken {
                user_id: user.id,
                name: token_name("CLI", host),
                prefix,
                hash,
                scopes: vec![KeyScope::Read, KeyScope::Write],
                expires_at: Some(now.saturating_add(NINETY_DAYS_NS)),
            };
            let row = state
                .meta
                .write(move |conn| {
                    Box::pin(async move { personal_tokens::insert_token(conn, token, now).await })
                })
                .await?;
            Ok((
                Grant {
                    token: plaintext,
                    project: None,
                    ingest_key: None,
                },
                row.id,
                None,
            ))
        }
        Purpose::Init => {
            let project_id = body
                .project_id
                .ok_or_else(|| field_error("project_id", "required for init"))?;
            let access = user_project(
                state,
                user,
                &project_id.to_string(),
                ProjectPerm::ManageSettings,
            )
            .await?;
            let project = access.project;
            let org = org_by_id(state.meta.reader(), project.org_id)
                .await?
                .ok_or(ApiError::NotFound)?;
            let name = token_name("CLI init", host);
            let NewKey {
                plaintext: token_plain,
                hash: token_hash,
                prefix: token_prefix,
            } = secret::new_key(KeyKind::Personal).map_err(internal)?;
            let NewKey {
                plaintext: key_plain,
                hash: key_hash,
                prefix: key_prefix,
            } = secret::new_key(KeyKind::Secret).map_err(internal)?;
            let token = NewToken {
                user_id: user.id,
                name: name.clone(),
                prefix: token_prefix,
                hash: token_hash,
                scopes: vec![KeyScope::Read],
                expires_at: Some(now.saturating_add(NINETY_DAYS_NS)),
            };
            let key = NewApiKey {
                project_id: project.id,
                kind: "secret",
                scopes: vec![KeyScope::Ingest],
                name: Some(name),
                prefix: key_prefix,
                hash: key_hash,
                created_by: user.id,
                expires_at: None,
            };
            let (token_id, key_id) = state
                .meta
                .write(move |conn| {
                    Box::pin(async move {
                        let token_row = personal_tokens::insert_token(conn, token, now).await?;
                        let key_row = api_keys::insert_api_key(conn, key, now).await?;
                        Ok::<(Uuid, Uuid), MetaError>((token_row.id, key_row.id))
                    })
                })
                .await?;
            let grant_project = GrantProject {
                id: project.id,
                org_slug: org.slug,
                slug: project.slug,
                name: project.name,
            };
            Ok((
                Grant {
                    token: token_plain,
                    project: Some(grant_project),
                    ingest_key: Some(key_plain),
                },
                token_id,
                Some((key_id, project_id)),
            ))
        }
    }
}

/// `POST /api/auth/device/{user_code}/deny`. 404 for a code that is not pending.
async fn deny(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    error::Path(user_code): error::Path<String>,
) -> Result<StatusCode, ApiError> {
    let user = session_user(&principal)?;
    let now = state.clock.now_ns();
    check_lookup(&state, user.id, now)?;
    state
        .device
        .deny(&user_code, now)
        .map_err(|_| ApiError::NotFound)?;
    tracing::info!(target: "audit", user_id = %user.id, "device code denied");
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /api/internal/v1/device/codes`: the control plane registers a code it issued (D-AO-14),
/// with a control-plane token of scope `device`. An identical retry answers 204 again.
async fn register_code(
    State(state): State<AppState>,
    error::Json(body): error::Json<InternalDeviceCode>,
) -> Result<StatusCode, ApiError> {
    let now = state.clock.now_ns();
    let host = body.host.as_deref().and_then(sanitise_host);
    let purpose = purpose_in(body.purpose);
    state
        .device
        .register(
            &body.device_code_sha256,
            &body.user_code,
            purpose,
            host,
            body.expires_at,
            now,
        )
        .map_err(|error| match error {
            DeviceError::Invalid(field) => field_error(field, "invalid"),
            DeviceError::Taken => ApiError::Conflict {
                detail: "device code already registered",
            },
            DeviceError::Full => ApiError::Unavailable {
                detail: "too many pending device codes",
            },
            other => internal(other),
        })?;
    tracing::info!(target: "audit", purpose = ?purpose, "device code registered by the control plane");
    Ok(StatusCode::NO_CONTENT)
}

/// Thirty lookups, approvals and denials a minute per user (D-AO-3).
fn check_lookup(state: &AppState, user_id: Uuid, now: i64) -> Result<(), ApiError> {
    state
        .limits
        .device_lookup
        .check(&user_id, now)
        .map_err(|retry| ApiError::RateLimited {
            retry_after_s: retry.0.as_secs().max(1),
        })
}

/// The machine name the CLI reports, kept to `[A-Za-z0-9.-]` and at most 64 characters. `None`
/// when nothing usable remains.
fn sanitise_host(raw: &str) -> Option<String> {
    let kept: String = raw
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '.' || *c == '-')
        .take(64)
        .collect();
    (!kept.is_empty()).then_some(kept)
}

/// The name of a grant's token: `<prefix> on <host>`, or the prefix alone without a host.
fn token_name(prefix: &str, host: Option<&str>) -> String {
    match host {
        Some(host) => format!("{prefix} on {host}"),
        None => prefix.to_owned(),
    }
}

fn internal(error: impl std::error::Error + Send + Sync + 'static) -> ApiError {
    ApiError::Internal(Box::new(error))
}

fn purpose_in(purpose: DevicePurpose) -> Purpose {
    match purpose {
        DevicePurpose::Login => Purpose::Login,
        DevicePurpose::Init => Purpose::Init,
    }
}

fn purpose_out(purpose: Purpose) -> DevicePurpose {
    match purpose {
        Purpose::Login => DevicePurpose::Login,
        Purpose::Init => DevicePurpose::Init,
    }
}

fn project_out(project: GrantProject) -> DeviceGrantProject {
    DeviceGrantProject {
        id: project.id,
        org_slug: project.org_slug,
        slug: project.slug,
        name: project.name,
    }
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    use axum::http::{Method, header};
    use chrono::{DateTime, SecondsFormat, Utc};
    use serde_json::{Value, json};

    use super::EXPIRES_IN_S;
    use crate::api::common::Page;
    use crate::api::device::{DeviceCodeResponse, DeviceGrant, DevicePurpose, DeviceRequest};
    use crate::api::keys::PersonalToken;
    use crate::auth::roles::OrgRole;
    use crate::auth::secret::hash_token;
    use crate::meta::projects::insert_project;
    use crate::test_support::{TestApp, TestResponse, capture_logs, second_org};

    const CLIENT: IpAddr = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 10));
    const DAY_NS: i64 = 24 * 60 * 60 * 1_000_000_000;

    async fn post_code(app: &TestApp, peer: IpAddr, purpose: &str, host: &str) -> TestResponse {
        app.request(Method::POST, "/api/auth/device/code")
            .origin(None)
            .peer(peer)
            .json(&json!({ "purpose": purpose, "host": host }))
            .send()
            .await
    }

    async fn code_for(app: &TestApp, purpose: &str, host: &str) -> DeviceCodeResponse {
        let response = post_code(app, CLIENT, purpose, host).await;
        assert_eq!(
            response.status,
            200,
            "{}",
            String::from_utf8_lossy(&response.body)
        );
        response.json()
    }

    async fn poll(app: &TestApp, device_code: &str) -> TestResponse {
        app.request(Method::POST, "/api/auth/device/token")
            .origin(None)
            .json(&json!({ "device_code": device_code }))
            .send()
            .await
    }

    async fn lookup(app: &TestApp, cookie: &str, user_code: &str) -> TestResponse {
        app.request(Method::GET, &format!("/api/auth/device/{user_code}"))
            .header(header::COOKIE.as_str(), cookie)
            .send()
            .await
    }

    async fn approve(app: &TestApp, cookie: &str, user_code: &str, body: Value) -> TestResponse {
        app.request(
            Method::POST,
            &format!("/api/auth/device/{user_code}/approve"),
        )
        .header(header::COOKIE.as_str(), cookie)
        .json(&body)
        .send()
        .await
    }

    async fn deny(app: &TestApp, cookie: &str, user_code: &str) -> TestResponse {
        app.request(Method::POST, &format!("/api/auth/device/{user_code}/deny"))
            .header(header::COOKIE.as_str(), cookie)
            .send()
            .await
    }

    fn error_code(response: &TestResponse) -> String {
        let body: Value = response.json();
        body["code"].as_str().unwrap_or("").to_owned()
    }

    #[tokio::test]
    async fn a_code_request_needs_no_origin_and_names_the_public_urls() {
        let app = TestApp::new().await;
        let answer = code_for(&app, "login", "dev-laptop").await;
        let origin = app.state.config.public_url.origin().to_owned();
        assert_eq!(answer.verification_uri, format!("{origin}/device"));
        assert_eq!(
            answer.verification_uri_complete,
            format!("{origin}/device?code={}", answer.user_code)
        );
        assert_eq!(answer.expires_in, EXPIRES_IN_S);
        assert_eq!(answer.interval, 5);
        assert_eq!(answer.device_code.len(), 43);
    }

    #[tokio::test]
    async fn the_eleventh_code_a_minute_from_one_network_is_refused() {
        let app = TestApp::new().await;
        for _ in 0..10 {
            assert_eq!(post_code(&app, CLIENT, "login", "h").await.status, 200);
        }
        assert_eq!(post_code(&app, CLIENT, "login", "h").await.status, 429);
        let other = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 11));
        assert_eq!(post_code(&app, other, "login", "h").await.status, 200);

        // Two addresses of one /64 share one bucket.
        let first = IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 1, 2, 0, 0, 0, 1));
        let second = IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 1, 2, 0xffff, 0, 0, 2));
        for _ in 0..10 {
            assert_eq!(post_code(&app, first, "login", "h").await.status, 200);
        }
        assert_eq!(post_code(&app, second, "login", "h").await.status, 429);
    }

    #[tokio::test]
    async fn a_user_gets_thirty_lookups_a_minute() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        for _ in 0..30 {
            let response = lookup(&app, &seeded.cookie, "BCDF-GHJK").await;
            assert_eq!(response.status, 404);
        }
        assert_eq!(lookup(&app, &seeded.cookie, "BCDF-GHJK").await.status, 429);
    }

    #[tokio::test]
    async fn polling_is_pending_then_slow_down_and_an_unknown_code_is_expired() {
        let app = TestApp::new().await;
        let code = code_for(&app, "login", "dev-laptop").await;

        let first = poll(&app, &code.device_code).await;
        assert_eq!(first.status, 400);
        assert_eq!(error_code(&first), "authorization_pending");

        let second = poll(&app, &code.device_code).await;
        assert_eq!(second.status, 400);
        assert_eq!(error_code(&second), "slow_down");

        let unknown = poll(&app, &"A".repeat(43)).await;
        assert_eq!(unknown.status, 400);
        assert_eq!(error_code(&unknown), "expired_token");
    }

    #[tokio::test]
    async fn the_login_flow_mints_a_read_write_token_and_the_grant_is_collected_once() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let code = code_for(&app, "login", "dev-laptop").await;

        let shown = lookup(&app, &seeded.cookie, &code.user_code).await;
        assert_eq!(shown.status, 200);
        let shown: DeviceRequest = shown.json();
        assert_eq!(shown.purpose, DevicePurpose::Login);
        assert_eq!(shown.host.as_deref(), Some("dev-laptop"));

        let approved = approve(&app, &seeded.cookie, &code.user_code, json!({})).await;
        assert_eq!(approved.status, 204);

        let polled = poll(&app, &code.device_code).await;
        assert_eq!(polled.status, 200);
        assert_eq!(polled.header("cache-control"), Some("no-store"));
        let grant: DeviceGrant = polled.json();
        assert_eq!(grant.server, app.state.config.public_url.origin());
        assert!(grant.token.starts_with("pat-cetra-"));
        assert!(grant.project.is_none());
        assert!(grant.ingest_key.is_none());

        let me = app
            .request(Method::GET, "/api/auth/me")
            .header(
                header::AUTHORIZATION.as_str(),
                &format!("Bearer {}", grant.token),
            )
            .send()
            .await;
        assert_eq!(me.status, 200);

        let listed = app
            .request(Method::GET, "/api/v1/me/tokens")
            .header(header::COOKIE.as_str(), &seeded.cookie)
            .send()
            .await;
        let tokens: Page<PersonalToken> = listed.json();
        let token = tokens
            .data
            .iter()
            .find(|token| token.name == "CLI on dev-laptop")
            .expect("the login token");
        assert_eq!(
            serde_json::to_value(&token.scopes).expect("scopes"),
            json!(["read", "write"])
        );
        assert_eq!(
            token.expires_at,
            Some(app.state.clock.now_ns() + 90 * DAY_NS)
        );

        let again = poll(&app, &code.device_code).await;
        assert_eq!(error_code(&again), "expired_token");
        assert!(
            app.state.analytics.counters.get("device.approved") >= 1,
            "device.approved not counted"
        );
    }

    #[tokio::test]
    async fn the_init_flow_mints_a_read_token_and_an_ingest_key_for_the_project() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let code = code_for(&app, "init", "dev-laptop").await;

        let approved = approve(
            &app,
            &seeded.cookie,
            &code.user_code,
            json!({ "project_id": seeded.project.id }),
        )
        .await;
        assert_eq!(approved.status, 204);

        let polled = poll(&app, &code.device_code).await;
        assert_eq!(polled.status, 200);
        let grant: DeviceGrant = polled.json();
        let project = grant.project.expect("the project");
        assert_eq!(project.org_slug, "acme");
        assert_eq!(project.slug, "support-bot");
        assert!(project.id == seeded.project.id);
        let key = grant.ingest_key.as_deref().expect("the ingest key");
        assert!(key.starts_with("sk-cetra-"));

        let listed = app
            .request(
                Method::GET,
                &format!("/api/v1/projects/{}/api-keys", seeded.project.id),
            )
            .header(header::COOKIE.as_str(), &seeded.cookie)
            .send()
            .await;
        let keys: Value = listed.json();
        let row = keys["data"]
            .as_array()
            .and_then(|rows| {
                rows.iter()
                    .find(|row| row["name"] == "CLI init on dev-laptop")
            })
            .expect("the init key");
        assert_eq!(row["scopes"], json!(["ingest"]));

        let listed = app
            .request(Method::GET, "/api/v1/me/tokens")
            .header(header::COOKIE.as_str(), &seeded.cookie)
            .send()
            .await;
        let tokens: Page<PersonalToken> = listed.json();
        let token = tokens
            .data
            .iter()
            .find(|token| token.name == "CLI init on dev-laptop")
            .expect("the init token");
        assert_eq!(
            serde_json::to_value(&token.scopes).expect("scopes"),
            json!(["read"])
        );
    }

    #[tokio::test]
    async fn init_needs_a_project_and_login_refuses_one_and_both_stay_pending() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let init = code_for(&app, "init", "dev-laptop").await;
        let login = code_for(&app, "login", "dev-laptop").await;

        let missing = approve(&app, &seeded.cookie, &init.user_code, json!({})).await;
        assert_eq!(missing.status, 400);
        let body: Value = missing.json();
        assert_eq!(body["errors"][0]["field"], "project_id");

        let refused = approve(
            &app,
            &seeded.cookie,
            &login.user_code,
            json!({ "project_id": seeded.project.id }),
        )
        .await;
        assert_eq!(refused.status, 400);

        assert_eq!(
            lookup(&app, &seeded.cookie, &init.user_code).await.status,
            200
        );
        assert_eq!(
            lookup(&app, &seeded.cookie, &login.user_code).await.status,
            200
        );
    }

    #[tokio::test]
    async fn a_member_is_forbidden_and_another_orgs_project_is_not_found() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let member = app
            .add_member(&seeded, "member@example.com", OrgRole::Member)
            .await;
        let code = code_for(&app, "init", "dev-laptop").await;

        let refused = approve(
            &app,
            &member.cookie,
            &code.user_code,
            json!({ "project_id": seeded.project.id }),
        )
        .await;
        assert_eq!(refused.status, 403);

        let (org, _) = second_org(&app, "other", "owner@other.test").await;
        let now = app.state.clock.now_ns();
        let other = app
            .state
            .meta
            .write(move |conn| {
                Box::pin(
                    async move { insert_project(conn, org, "Other", "other", None, now).await },
                )
            })
            .await
            .expect("the other project");
        let hidden = approve(
            &app,
            &seeded.cookie,
            &code.user_code,
            json!({ "project_id": other.id }),
        )
        .await;
        assert_eq!(hidden.status, 404);

        assert_eq!(
            lookup(&app, &seeded.cookie, &code.user_code).await.status,
            200
        );
    }

    #[tokio::test]
    async fn deny_is_final_and_an_unknown_code_is_not_found() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let code = code_for(&app, "login", "dev-laptop").await;

        let denied = deny(&app, &seeded.cookie, &code.user_code).await;
        assert_eq!(denied.status, 204);

        let polled = poll(&app, &code.device_code).await;
        assert_eq!(polled.status, 400);
        assert_eq!(error_code(&polled), "access_denied");

        assert_eq!(deny(&app, &seeded.cookie, "BCDF-GHJK").await.status, 404);
    }

    #[tokio::test]
    async fn a_bearer_token_cannot_reach_the_session_routes() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let token = app.personal_token(seeded.user_id, true).await;
        let auth = format!("Bearer {token}");
        let code = code_for(&app, "login", "dev-laptop").await;

        let shown = app
            .request(Method::GET, &format!("/api/auth/device/{}", code.user_code))
            .header(header::AUTHORIZATION.as_str(), &auth)
            .send()
            .await;
        assert_eq!(shown.status, 403);

        let approved = app
            .request(
                Method::POST,
                &format!("/api/auth/device/{}/approve", code.user_code),
            )
            .header(header::AUTHORIZATION.as_str(), &auth)
            .json(&json!({}))
            .send()
            .await;
        assert_eq!(approved.status, 403);

        let denied = app
            .request(
                Method::POST,
                &format!("/api/auth/device/{}/deny", code.user_code),
            )
            .header(header::AUTHORIZATION.as_str(), &auth)
            .send()
            .await;
        assert_eq!(denied.status, 403);
    }

    #[tokio::test]
    async fn a_session_approval_without_an_origin_is_refused() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let code = code_for(&app, "login", "dev-laptop").await;

        let response = app
            .request(
                Method::POST,
                &format!("/api/auth/device/{}/approve", code.user_code),
            )
            .origin(None)
            .header(header::COOKIE.as_str(), &seeded.cookie)
            .json(&json!({}))
            .send()
            .await;
        assert_eq!(response.status, 403);
    }

    #[tokio::test]
    async fn the_logs_hold_neither_the_token_nor_the_key() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let code = code_for(&app, "init", "dev-laptop").await;

        let (grant, logs) = capture_logs(async {
            let approved = approve(
                &app,
                &seeded.cookie,
                &code.user_code,
                json!({ "project_id": seeded.project.id }),
            )
            .await;
            assert_eq!(approved.status, 204);
            assert!(approved.body.is_empty());
            let polled = poll(&app, &code.device_code).await;
            assert_eq!(polled.status, 200);
            polled.json::<DeviceGrant>()
        })
        .await;

        let key = grant.ingest_key.as_deref().expect("the ingest key");
        assert!(!logs.contains(&grant.token));
        assert!(!logs.contains(key));
        assert!(!logs.contains(&code.device_code));
    }

    const SECOND_NS: i64 = 1_000_000_000;
    const INTERNAL_PATH: &str = "/api/internal/v1/device/codes";

    /// A registration body with a `login` purpose.
    fn registration(device_code: &str, user_code: &str, host: &str, expires_at: i64) -> Value {
        json!({
            "device_code_sha256": hash_token(device_code),
            "user_code": user_code,
            "purpose": "login",
            "host": host,
            "expires_at": rfc3339(expires_at),
        })
    }

    fn rfc3339(ns: i64) -> String {
        let nanos = u32::try_from(ns.rem_euclid(1_000_000_000)).unwrap_or(0);
        DateTime::<Utc>::from_timestamp(ns.div_euclid(1_000_000_000), nanos)
            .expect("the instant is in range")
            .to_rfc3339_opts(SecondsFormat::Nanos, true)
    }

    /// `POST /api/internal/v1/device/codes` with a control-plane token of `scope`.
    async fn register(app: &TestApp, scope: &str, body: &Value) -> TestResponse {
        app.request(Method::POST, INTERNAL_PATH)
            .control_plane(scope)
            .json(body)
            .send()
            .await
    }

    #[tokio::test]
    async fn a_registered_code_shows_on_the_approval_page() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let now = app.state.clock.now_ns();
        let body = registration(
            "device-code-a",
            "BCDF-GHJK",
            "dev-laptop",
            now + 600 * SECOND_NS,
        );
        assert_eq!(register(&app, "device", &body).await.status, 204);

        let shown = lookup(&app, &seeded.cookie, "BCDF-GHJK").await;
        assert_eq!(shown.status, 200);
        let shown: DeviceRequest = shown.json();
        assert_eq!(shown.purpose, DevicePurpose::Login);
        assert_eq!(shown.host.as_deref(), Some("dev-laptop"));
        assert_eq!(shown.created_at, now);
    }

    #[tokio::test]
    async fn only_a_device_token_may_register_and_a_session_may_not() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let now = app.state.clock.now_ns();
        let body = registration(
            "device-code-b",
            "BCDF-GHJK",
            "dev-laptop",
            now + 600 * SECOND_NS,
        );

        assert_eq!(register(&app, "orgs", &body).await.status, 403);
        let session = app
            .request(Method::POST, INTERNAL_PATH)
            .header(header::COOKIE.as_str(), &seeded.cookie)
            .json(&body)
            .send()
            .await;
        assert_eq!(session.status, 403);
        assert_eq!(register(&app, "device", &body).await.status, 204);
    }

    #[tokio::test]
    async fn a_clash_gets_409_and_an_identical_retry_gets_204() {
        let app = TestApp::new().await;
        let now = app.state.clock.now_ns();
        let expires = now + 600 * SECOND_NS;
        let body = registration("device-code-c", "BCDF-GHJK", "dev-laptop", expires);
        assert_eq!(register(&app, "device", &body).await.status, 204);
        assert_eq!(register(&app, "device", &body).await.status, 204);

        let other_host = registration("device-code-c", "BCDF-GHJK", "other-host", expires);
        assert_eq!(register(&app, "device", &other_host).await.status, 409);

        let other_hash = registration("device-code-d", "BCDF-GHJK", "dev-laptop", expires);
        assert_eq!(register(&app, "device", &other_hash).await.status, 409);
    }

    #[tokio::test]
    async fn bad_input_gets_400_and_a_long_host_is_sanitised() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let now = app.state.clock.now_ns();
        let expires = now + 600 * SECOND_NS;

        let mut bad_hash = registration("device-code-e", "BCDF-GHJK", "dev-laptop", expires);
        bad_hash["device_code_sha256"] = json!("xyz");
        let refused = register(&app, "device", &bad_hash).await;
        assert_eq!(refused.status, 400);
        let problem: Value = refused.json();
        assert_eq!(problem["errors"][0]["field"], "device_code_sha256");

        let bad_user = registration("device-code-e", "BCDA-GHJK", "dev-laptop", expires);
        assert_eq!(register(&app, "device", &bad_user).await.status, 400);

        let mut extra = registration("device-code-e", "BCDF-GHJK", "dev-laptop", expires);
        extra["extra"] = json!(1);
        assert_eq!(register(&app, "device", &extra).await.status, 400);

        let long = "h".repeat(100);
        let body = registration("device-code-f", "BCDF-GHJK", &long, expires);
        assert_eq!(register(&app, "device", &body).await.status, 204);
        let shown: DeviceRequest = lookup(&app, &seeded.cookie, "BCDF-GHJK").await.json();
        let kept = "h".repeat(64);
        assert_eq!(shown.host.as_deref(), Some(kept.as_str()));
    }

    #[tokio::test]
    async fn a_registered_code_is_approved_and_granted_like_an_issued_one() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let now = app.state.clock.now_ns();
        let device_code = "device-code-end-to-end";
        let body = registration(
            device_code,
            "BCDF-GHJK",
            "dev-laptop",
            now + 600 * SECOND_NS,
        );
        assert_eq!(register(&app, "device", &body).await.status, 204);

        let approved = approve(&app, &seeded.cookie, "BCDF-GHJK", json!({})).await;
        assert_eq!(approved.status, 204);

        let polled = poll(&app, device_code).await;
        assert_eq!(polled.status, 200);
        let grant: DeviceGrant = polled.json();
        assert_eq!(grant.server, app.state.config.public_url.origin());
        assert!(grant.token.starts_with("pat-cetra-"));
    }
}
