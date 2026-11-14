//! Org entitlements (D179, D-P-15) and the control plane's org-token PUT (D-P-11).

use axum::extract::{Extension, State};
use axum::http::StatusCode;
use axum::routing::{get, put};
use serde::Deserialize;
use sqlx::SqliteConnection;
use uuid::Uuid;

use crate::api::entitlements::{OrgEntitlements, PutOrgTokenRequest};
use crate::auth::roles::OrgPerm;
use crate::entitlements::tokens::{OrgGrant, OrgTokenError, grant_from_token};
use crate::entitlements::{self, Limits};
use crate::http::AppState;
use crate::http::access::{Access, OrgAccess, RouteDef};
use crate::http::client_ip::ClientIp;
use crate::http::error::{self, ApiError};
use crate::meta::MetaError;
use crate::meta::entitlements::{StoredToken, put_token, record_window_end, token_of_tx};
use crate::meta::orgs::org_by_id_tx;
use llmobs_core::ids::OrgId;

/// `GET /api/v1/orgs/{org}/entitlements`; `PUT /api/internal/v1/orgs/{org_id}/entitlements`.
pub(crate) fn routes() -> Vec<RouteDef> {
    vec![
        RouteDef {
            method: axum::http::Method::GET,
            path: "/api/v1/orgs/{org}/entitlements",
            access: Access::Org {
                perm: OrgPerm::View,
                session_only: false,
            },
            documented: true,
            handler: get(get_entitlements),
        },
        RouteDef {
            method: axum::http::Method::PUT,
            path: "/api/internal/v1/orgs/{org_id}/entitlements",
            access: Access::Public,
            documented: true,
            handler: put(put_org_token),
        },
    ]
}

async fn get_entitlements(
    State(state): State<AppState>,
    Extension(access): Extension<OrgAccess>,
) -> Result<error::Json<OrgEntitlements>, ApiError> {
    let org = access.org.id;
    let now = state.clock.now_ns();
    let resolved = state.entitlements.resolve(org, now);
    Ok(error::Json(OrgEntitlements {
        features: resolved.features(),
        plan: state.entitlements.plan_at(org, now).into(),
        limits: resolved.limits.into(),
        upgrade_url: entitlements::upgrade_url(&state, org),
    }))
}

#[derive(Debug, Deserialize)]
struct OrgIdPath {
    org_id: Uuid,
}

/// What the store transaction did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PutOutcome {
    /// The token is stored and applies from now on.
    Stored,
    /// The same token is already stored.
    Same,
    /// The stored token has the same or a newer `issued_at`.
    Stale,
    /// The org does not exist or is not active.
    Missing,
}

/// `PUT /api/internal/v1/orgs/{org_id}/entitlements` (D-P-11). The signature is the
/// credential, so the route is public. Only a failed verification counts against the IP limit.
async fn put_org_token(
    State(state): State<AppState>,
    ClientIp(ip): ClientIp,
    error::Path(OrgIdPath { org_id }): error::Path<OrgIdPath>,
    error::Json(body): error::Json<PutOrgTokenRequest>,
) -> Result<StatusCode, ApiError> {
    let now = state.clock.now_ns();
    let defaults = state.entitlements.defaults();
    let (token_org, grant) = match grant_from_token(
        &body.token,
        state.config.public_url.origin(),
        &defaults.limits,
    ) {
        Ok(verified) => verified,
        Err(error) => {
            state
                .limits
                .ip_forms
                .check(&ip, now)
                .map_err(|retry| ApiError::RateLimited {
                    retry_after_s: retry.0.as_secs().max(1),
                })?;
            return Err(refused(error));
        }
    };
    let org = OrgId::from_uuid(org_id);
    if token_org != org {
        return Err(refused(OrgTokenError::OrgMismatch));
    }
    if now >= grant.grace_ends_at {
        return Err(refused(OrgTokenError::Expired));
    }
    let token_id = grant.token_id;
    let tx_grant = grant.clone();
    let origin = state.config.public_url.origin().to_owned();
    let defaults_limits = defaults.limits;
    let outcome = state
        .meta
        .write(move |conn| {
            Box::pin(async move {
                store_token(
                    conn,
                    org,
                    body.token,
                    &tx_grant,
                    &origin,
                    &defaults_limits,
                    now,
                )
                .await
            })
        })
        .await?;
    match outcome {
        PutOutcome::Stored => {
            state.entitlements.put_grant(org, grant);
            tracing::info!(target: "audit", org_id = %org, token_id = %token_id, "org entitlements updated");
            Ok(StatusCode::NO_CONTENT)
        }
        PutOutcome::Same => Ok(StatusCode::NO_CONTENT),
        PutOutcome::Stale => Err(ApiError::Conflict {
            detail: "a token with the same or a newer issued_at is stored",
        }),
        PutOutcome::Missing => Err(ApiError::NotFound),
    }
}

/// The problem for a refused token. The detail is the error's text; the token is not quoted.
fn refused(error: OrgTokenError) -> ApiError {
    ApiError::BadRequest {
        detail: Some(error.to_string()),
        errors: Vec::new(),
    }
}

/// Stores `token` for `org` in the write transaction and records the data windows it ends
/// (D-P-30). The windows that end are the stored token's, or the instance defaults' once
/// nothing valid is stored; only the largest one matters, so every difference is recorded.
async fn store_token(
    conn: &mut SqliteConnection,
    org: OrgId,
    token: String,
    grant: &OrgGrant,
    origin: &str,
    defaults: &Limits,
    now: i64,
) -> Result<PutOutcome, MetaError> {
    let Some(row) = org_by_id_tx(conn, org).await? else {
        return Ok(PutOutcome::Missing);
    };
    if row.state != "active" {
        return Ok(PutOutcome::Missing);
    }
    let stored = token_of_tx(conn, org).await?;
    if let Some(stored) = &stored {
        if stored.token_id == grant.token_id {
            return Ok(PutOutcome::Same);
        }
        if stored.issued_at >= grant.issued_at {
            return Ok(PutOutcome::Stale);
        }
    }
    // A stored token that no longer verifies for this cell counts as absent.
    let held = stored.as_ref().and_then(|stored| {
        grant_from_token(&stored.token, origin, defaults)
            .ok()
            .filter(|(held_org, _)| *held_org == org)
            .map(|(_, held)| held)
    });
    let new_window = grant.limits.data_access_days;
    let defaults_window = defaults.data_access_days;
    match held {
        Some(held) if now < held.grace_ends_at => {
            if held.limits.data_access_days != new_window {
                record_window_end(conn, org, now, held.limits.data_access_days).await?;
            }
        }
        Some(held) => {
            record_window_end(conn, org, held.grace_ends_at, held.limits.data_access_days).await?;
            if defaults_window != new_window {
                record_window_end(conn, org, now, defaults_window).await?;
            }
        }
        None => {
            if defaults_window != new_window {
                record_window_end(conn, org, now, defaults_window).await?;
            }
        }
    }
    put_token(
        conn,
        &StoredToken {
            org_id: org,
            token,
            token_id: grant.token_id,
            issued_at: grant.issued_at,
            received_at: now,
        },
    )
    .await?;
    Ok(PutOutcome::Stored)
}

#[cfg(test)]
mod tests {
    use axum::http::{Method, header};
    use serde_json::{Value, json};
    use uuid::Uuid;

    use std::net::IpAddr;

    use axum::http::StatusCode;

    use crate::api::account::OrgRole;
    use crate::auth::session::start_session;
    use crate::config::ServerConfig;
    use crate::entitlements::Source;
    use crate::entitlements::tokens::grant_from_token;
    use crate::meta::entitlements::{StoredToken, put_token, token_of_tx, window_ends_since};
    use crate::meta::members::add_org_member;
    use crate::meta::users::{NewUser, insert_user};
    use crate::test_support::{
        TestApp, TestResponse, capture_logs, license_key, org_token, sign_token,
    };
    use llmobs_core::ids::OrgId;

    /// Signs in a new user and returns its id and session cookie.
    async fn signed_in(app: &TestApp, email: &str) -> (Uuid, String) {
        let email = email.to_owned();
        let now = app.state.clock.now_ns();
        let user = app
            .state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    insert_user(
                        conn,
                        NewUser {
                            email,
                            name: None,
                            password_hash: None,
                            email_verified_at: Some(now),
                            is_instance_admin: false,
                        },
                        now,
                    )
                    .await
                })
            })
            .await
            .unwrap();
        let set_cookie = start_session(&app.state, user.id, None, None)
            .await
            .unwrap();
        let pair = set_cookie
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned();
        (user.id, pair)
    }

    async fn add_member(app: &TestApp, org: OrgId, user: Uuid, role: OrgRole) {
        let now = app.state.clock.now_ns();
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move { add_org_member(conn, org, user, role, now).await })
            })
            .await
            .unwrap();
    }

    async fn get_json(app: &TestApp, cookie: &str, path: &str) -> (u16, Value) {
        let response = app
            .request(Method::GET, path)
            .header(header::COOKIE.as_str(), cookie)
            .send()
            .await;
        let status = response.status.as_u16();
        let body = if status == 200 {
            response.json()
        } else {
            Value::Null
        };
        (status, body)
    }

    #[tokio::test]
    async fn members_read_the_licence_features() {
        let app = TestApp::with_license(&["project_roles"]).await;
        let seeded = app.seed().await;
        let (viewer, cookie) = signed_in(&app, "viewer@example.com").await;
        add_member(&app, seeded.org_id, viewer, OrgRole::Viewer).await;

        let (status, body) = get_json(&app, &cookie, "/api/v1/orgs/acme/entitlements").await;
        assert_eq!(status, 200);
        assert_eq!(
            body["features"],
            json!(["retention_management", "project_roles"])
        );
    }

    #[tokio::test]
    async fn no_license_only_core_features() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let (status, body) = get_json(&app, &seeded.cookie, "/api/v1/orgs/acme/entitlements").await;
        assert_eq!(status, 200);
        assert_eq!(body["features"], json!(["retention_management"]));
    }

    #[tokio::test]
    async fn defaults_are_the_plan_without_limits() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let (status, body) = get_json(&app, &seeded.cookie, "/api/v1/orgs/acme/entitlements").await;
        assert_eq!(status, 200);
        assert_eq!(
            body["plan"],
            json!({"name": "Self-hosted", "add_ons": [], "source": "defaults"})
        );
        assert_eq!(body["limits"], json!({}));
    }

    #[tokio::test]
    async fn a_stored_token_sets_the_plan_and_limits() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let org = seeded.org_id;
        let token = org_token(
            org,
            "2025-12-01T00:00:00Z",
            "2099-01-01T00:00:00Z",
            "2099-02-01T00:00:00Z",
            &[],
            &json!({"units_included": 200000, "data_access_days": 1825}),
        );
        assert_eq!(put(&app, org, &token).await.status, StatusCode::NO_CONTENT);

        let (status, body) = get_json(&app, &seeded.cookie, "/api/v1/orgs/acme/entitlements").await;
        assert_eq!(status, 200);
        assert_eq!(body["plan"]["source"], json!("token"));
        assert_eq!(body["plan"]["id"], json!("pro"));
        assert_eq!(body["plan"]["name"], json!("Pro"));
        assert_eq!(body["plan"]["add_ons"], json!(["teams"]));
        assert_eq!(body["plan"]["status"], json!("active"));
        assert!(
            body["plan"]["expires_at"]
                .as_str()
                .is_some_and(|at| at.starts_with("2099-01-01T00:00:00")),
            "{body}"
        );
        assert!(body["plan"]["grace_ends_at"].is_string(), "{body}");
        assert_eq!(
            body["limits"],
            json!({"units_included": 200000, "data_access_days": 1825})
        );
    }

    #[tokio::test]
    async fn upgrade_url_names_the_org() {
        let app = TestApp::with_config(|config: &mut ServerConfig| {
            config.entitlements.upgrade_url = Some("https://up.example/{org}/billing".to_owned());
        })
        .await;
        let seeded = app.seed().await;
        let (status, body) = get_json(&app, &seeded.cookie, "/api/v1/orgs/acme/entitlements").await;
        assert_eq!(status, 200);
        assert_eq!(
            body["upgrade_url"],
            json!(format!("https://up.example/{}/billing", seeded.org_id))
        );
    }

    #[tokio::test]
    async fn outsiders_get_404() {
        let app = TestApp::with_license(&["project_roles"]).await;
        app.seed().await;
        let (_, cookie) = signed_in(&app, "outsider@example.com").await;
        let (status, _) = get_json(&app, &cookie, "/api/v1/orgs/acme/entitlements").await;
        assert_eq!(status, 404);
    }

    /// Sends `token` to the org's entitlements route from `peer`.
    async fn put_as(app: &TestApp, peer: Option<IpAddr>, org: OrgId, token: &str) -> TestResponse {
        let mut request = app
            .request(
                Method::PUT,
                &format!("/api/internal/v1/orgs/{org}/entitlements"),
            )
            .json(&json!({ "token": token }));
        if let Some(peer) = peer {
            request = request.peer(peer);
        }
        request.send().await
    }

    async fn put(app: &TestApp, org: OrgId, token: &str) -> TestResponse {
        put_as(app, None, org, token).await
    }

    /// The org's stored token row, read in a write transaction.
    async fn stored(app: &TestApp, org: OrgId) -> Option<StoredToken> {
        app.state
            .meta
            .write(move |conn| Box::pin(async move { token_of_tx(conn, org).await }))
            .await
            .unwrap()
    }

    /// A signed org token that is valid at the clock (2026-01-01) for `pro` with `limits`.
    fn valid_token(org: OrgId, issued_at: &str, limits: &Value) -> String {
        org_token(
            org,
            issued_at,
            "2099-01-01T00:00:00Z",
            "2099-02-01T00:00:00Z",
            &[],
            limits,
        )
    }

    #[tokio::test]
    async fn stores_a_token_and_applies_it() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let org = seeded.org_id;
        let token = org_token(
            org,
            "2025-12-01T00:00:00Z",
            "2099-01-01T00:00:00Z",
            "2099-02-01T00:00:00Z",
            &["project_roles"],
            &json!({}),
        );

        assert_eq!(put(&app, org, &token).await.status, StatusCode::NO_CONTENT);
        let now = app.state.clock.now_ns();
        assert_eq!(
            app.state.entitlements.plan_at(org, now).source,
            Source::OrgToken
        );
        let first = stored(&app, org).await.expect("stored");
        assert_eq!(first.token, token);

        assert_eq!(put(&app, org, &token).await.status, StatusCode::NO_CONTENT);
        let again = stored(&app, org).await.expect("stored");
        assert_eq!(again.token_id, first.token_id);
        assert_eq!(again.received_at, first.received_at);
    }

    #[tokio::test]
    async fn newer_replaces_and_older_conflicts() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let org = seeded.org_id;
        let ten = valid_token(org, "2025-12-31T10:00:00Z", &json!({}));
        let eleven = valid_token(org, "2025-12-31T11:00:00Z", &json!({}));
        let ten_thirty = valid_token(org, "2025-12-31T10:30:00Z", &json!({}));

        assert_eq!(put(&app, org, &ten).await.status, StatusCode::NO_CONTENT);
        assert_eq!(put(&app, org, &eleven).await.status, StatusCode::NO_CONTENT);
        let conflict = put(&app, org, &ten_thirty).await;
        assert_eq!(conflict.status, StatusCode::CONFLICT);
        assert_eq!(
            conflict.problem().detail.as_deref(),
            Some("a token with the same or a newer issued_at is stored")
        );
        assert_eq!(stored(&app, org).await.expect("stored").token, eleven);
        let held = app.state.entitlements.grant(org).expect("held");
        assert_eq!(
            held.token_id,
            stored(&app, org).await.expect("stored").token_id
        );
    }

    #[tokio::test]
    async fn refusals() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let org = seeded.org_id;
        let detail = |response: TestResponse| {
            assert_eq!(response.status, StatusCode::BAD_REQUEST);
            response.problem().detail.unwrap_or_default()
        };

        assert_eq!(
            detail(put(&app, org, "garbage").await),
            "the token is malformed"
        );
        assert_eq!(
            detail(put(&app, org, &license_key(&[])).await),
            "the token is not of the expected kind"
        );
        let foreign = sign_token(&json!({
            "kind": "org",
            "id": Uuid::now_v7().to_string(),
            "issued_at": "2025-12-01T00:00:00Z",
            "expires_at": "2099-01-01T00:00:00Z",
            "grace_ends_at": "2099-02-01T00:00:00Z",
            "audience": "https://elsewhere.example",
            "org_id": org.to_string(),
            "plan": {"id": "pro", "name": "Pro"},
        }));
        assert_eq!(
            detail(put(&app, org, &foreign).await),
            "the token is for another cell"
        );
        let other = valid_token(OrgId::new_v7(), "2025-12-01T00:00:00Z", &json!({}));
        assert_eq!(
            detail(put(&app, org, &other).await),
            "the token is for another org"
        );
        let expired = org_token(
            org,
            "2025-01-01T00:00:00Z",
            "2025-06-01T00:00:00Z",
            "2025-12-01T00:00:00Z",
            &[],
            &json!({}),
        );
        assert_eq!(
            detail(put(&app, org, &expired).await),
            "the token has expired"
        );

        let unknown = OrgId::new_v7();
        let token = valid_token(unknown, "2025-12-01T00:00:00Z", &json!({}));
        assert_eq!(
            put(&app, unknown, &token).await.status,
            StatusCode::NOT_FOUND
        );

        let token = valid_token(org, "2025-12-01T00:00:00Z", &json!({}));
        let body = json!({ "token": token, "x": 1 });
        let response = app
            .request(
                Method::PUT,
                &format!("/api/internal/v1/orgs/{org}/entitlements"),
            )
            .json(&body)
            .send()
            .await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST);
        assert!(stored(&app, org).await.is_none());
    }

    #[tokio::test]
    async fn failed_tokens_hit_the_limiter_and_good_ones_do_not() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let org = seeded.org_id;
        let peer: IpAddr = "203.0.113.7".parse().unwrap();

        for _ in 0..5 {
            let response = put_as(&app, Some(peer), org, "garbage").await;
            assert_eq!(response.status, StatusCode::BAD_REQUEST);
        }
        let limited = put_as(&app, Some(peer), org, "garbage").await;
        assert_eq!(limited.status, StatusCode::TOO_MANY_REQUESTS);
        assert!(limited.header("retry-after").is_some());

        let first = valid_token(org, "2025-12-01T00:00:00Z", &json!({}));
        assert_eq!(
            put_as(&app, Some(peer), org, &first).await.status,
            StatusCode::NO_CONTENT
        );
        for hour in 0..10 {
            let token = valid_token(org, &format!("2025-12-02T{hour:02}:00:00Z"), &json!({}));
            assert_eq!(
                put_as(&app, Some(peer), org, &token).await.status,
                StatusCode::NO_CONTENT,
                "valid token {hour}"
            );
        }
    }

    #[tokio::test]
    async fn window_shrink_is_recorded() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let org = seeded.org_id;
        let long = valid_token(
            org,
            "2025-12-01T00:00:00Z",
            &json!({"data_access_days": 1825}),
        );
        let short = valid_token(
            org,
            "2025-12-02T00:00:00Z",
            &json!({"data_access_days": 30}),
        );
        let shorter = valid_token(
            org,
            "2025-12-03T00:00:00Z",
            &json!({"data_access_days": 30}),
        );

        assert_eq!(put(&app, org, &long).await.status, StatusCode::NO_CONTENT);
        assert_eq!(put(&app, org, &short).await.status, StatusCode::NO_CONTENT);
        assert_eq!(
            put(&app, org, &shorter).await.status,
            StatusCode::NO_CONTENT
        );

        let now = app.state.clock.now_ns();
        let rows = window_ends_since(app.state.meta.reader(), org, 0)
            .await
            .unwrap();
        assert_eq!(rows, vec![(now, Some(1825))]);
    }

    #[tokio::test]
    async fn audit_log_has_no_token() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let org = seeded.org_id;
        let token = valid_token(org, "2025-12-01T00:00:00Z", &json!({}));

        let (response, logs) = capture_logs(put(&app, org, &token)).await;
        assert_eq!(response.status, StatusCode::NO_CONTENT);
        assert!(logs.contains("org entitlements updated"), "{logs}");
        assert!(logs.contains(&org.to_string()), "{logs}");
        assert!(!logs.contains("llmobs1."), "{logs}");
    }

    #[tokio::test]
    async fn expired_token_window_is_kept_after_replacement() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let org = seeded.org_id;
        let old = org_token(
            org,
            "2025-11-01T00:00:00Z",
            "2025-12-01T00:00:00Z",
            "2025-12-20T00:00:00Z",
            &[],
            &json!({"data_access_days": 1825}),
        );
        let defaults = app.state.entitlements.defaults().limits;
        let (_, old_grant) =
            grant_from_token(&old, app.state.config.public_url.origin(), &defaults).expect("valid");
        let old_grace = old_grant.grace_ends_at;
        let row = StoredToken {
            org_id: org,
            token: old,
            token_id: old_grant.token_id,
            issued_at: old_grant.issued_at,
            received_at: app.state.clock.now_ns(),
        };
        app.state
            .meta
            .write(move |conn| Box::pin(async move { put_token(conn, &row).await }))
            .await
            .unwrap();
        app.state.entitlements.put_grant(org, old_grant);

        let new = valid_token(
            org,
            "2025-12-25T00:00:00Z",
            &json!({"data_access_days": 30}),
        );
        assert_eq!(put(&app, org, &new).await.status, StatusCode::NO_CONTENT);

        let now = app.state.clock.now_ns();
        let rows = window_ends_since(app.state.meta.reader(), org, 0)
            .await
            .unwrap();
        assert_eq!(rows, vec![(now, None), (old_grace, Some(1825))]);
        let window = app
            .state
            .entitlements
            .retention_window_at(&app.state.meta, org, now)
            .await
            .unwrap();
        assert_eq!(window, None);
    }
}
