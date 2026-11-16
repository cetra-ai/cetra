//! OAuth and OIDC authorization-code login (spec 08, D148).
//!
//! Both routes are public GETs. The state cookie is the CSRF defence. Failures
//! redirect to `/login?error=<code>` and clear the cookie. An unknown provider
//! on authorize is 404.

use std::io;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde::Deserialize;

use crate::auth::oidc::flow::{self, LoginError, StartError};
use crate::auth::oidc::state_cookie;
use crate::auth::session::{SessionOrigin, start_session};
use crate::http::AppState;
use crate::http::access::{Access, RouteDef};
use crate::http::client_ip::ClientIp;
use crate::http::error::{self, ApiError};
use crate::http::restore::refuse_outbound;

/// Authorize and callback.
pub(crate) fn routes() -> Vec<RouteDef> {
    vec![
        RouteDef {
            method: axum::http::Method::GET,
            path: "/api/auth/authorize/{provider}",
            access: Access::Public,
            documented: true,
            handler: get(authorize),
        },
        RouteDef {
            method: axum::http::Method::GET,
            path: "/api/auth/callback/{provider}",
            access: Access::Public,
            documented: true,
            handler: get(callback),
        },
    ]
}

#[derive(Deserialize)]
struct AuthorizeQuery {
    #[serde(default)]
    redirect: Option<String>,
    #[serde(default)]
    setup_token: Option<String>,
}

#[derive(Deserialize)]
struct CallbackQuery {
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    error: Option<String>,
}

async fn authorize(
    State(state): State<AppState>,
    error::Path(provider): error::Path<String>,
    error::Query(query): error::Query<AuthorizeQuery>,
) -> Result<Response, ApiError> {
    refuse_outbound(&state, "external sign-in")?;
    match flow::start(
        &state,
        &provider,
        query.redirect.as_deref(),
        query.setup_token.as_deref(),
    )
    .await
    {
        Ok((location, cookie)) => redirect(&location, [cookie]),
        Err(StartError::NotFound) => Err(ApiError::NotFound),
        Err(StartError::Unavailable) => {
            redirect("/login?error=provider_unavailable", std::iter::empty())
        }
        Err(StartError::Internal(error)) => Err(ApiError::Internal(error)),
    }
}

async fn callback(
    State(state): State<AppState>,
    ClientIp(ip): ClientIp,
    headers: HeaderMap,
    error::Path(provider): error::Path<String>,
    error::Query(query): error::Query<CallbackQuery>,
) -> Result<Response, ApiError> {
    refuse_outbound(&state, "external sign-in")?;
    let https = state.config.public_url.is_https();
    let clear = state_cookie::clear(https);
    let finished = if provider == crate::ee::org_sso::login::PROVIDER {
        crate::ee::org_sso::login::finish(
            &state,
            &headers,
            query.state.as_deref(),
            query.code.as_deref(),
            query.error.is_some(),
        )
        .await
    } else {
        flow::finish(
            &state,
            &headers,
            &provider,
            query.state.as_deref(),
            query.code.as_deref(),
            query.error.is_some(),
        )
        .await
    };
    match finished {
        Ok(logged_in) => {
            let session = match start_session(
                &state,
                logged_in.user_id,
                headers
                    .get(header::USER_AGENT)
                    .and_then(|value| value.to_str().ok()),
                Some(ip),
                logged_in
                    .sso_org
                    .map_or(SessionOrigin::Provider, SessionOrigin::OrgProvider),
            )
            .await
            {
                Ok(cookie) => cookie,
                Err(error) => {
                    let mut response = error.into_response();
                    response.headers_mut().append(header::SET_COOKIE, clear);
                    return Ok(response);
                }
            };
            match logged_in.sso_org {
                Some(org_id) => tracing::info!(
                    target: "audit",
                    user_id = %logged_in.user_id,
                    org_id = %org_id,
                    provider = %provider,
                    "login"
                ),
                None => tracing::info!(
                    target: "audit",
                    user_id = %logged_in.user_id,
                    provider = %provider,
                    "login"
                ),
            }
            redirect(&logged_in.redirect, [clear, session])
        }
        Err(LoginError::Internal(error)) => {
            let mut response = ApiError::Internal(error).into_response();
            response.headers_mut().append(header::SET_COOKIE, clear);
            Ok(response)
        }
        Err(error) => {
            let code = error.code().unwrap_or("provider_error");
            redirect(&format!("/login?error={code}"), [clear])
        }
    }
}

pub(crate) fn redirect(
    location: &str,
    cookies: impl IntoIterator<Item = axum::http::HeaderValue>,
) -> Result<Response, ApiError> {
    let location = axum::http::HeaderValue::from_str(location).map_err(|_| {
        ApiError::Internal(Box::new(io::Error::new(
            io::ErrorKind::InvalidData,
            "redirect",
        )))
    })?;
    let mut response = Response::builder()
        .status(StatusCode::FOUND)
        .header(header::LOCATION, location)
        .body(axum::body::Body::empty())
        .map_err(|error| ApiError::Internal(Box::new(error)))?;
    for cookie in cookies {
        response.headers_mut().append(header::SET_COOKIE, cookie);
    }
    Ok(response)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use axum::http::{Method, StatusCode, header};
    use llmobs_core::ids::OrgId;
    use serde_json::json;
    use uuid::Uuid;

    use crate::api::account::SignupPolicy;
    use crate::auth::OrgRole;
    use crate::auth::secret::new_opaque_token;
    use crate::bootstrap;
    use crate::config::{AutoProvision, GithubAuth, GoogleAuth, OidcAuth, Secret};
    use crate::meta::identities;
    use crate::meta::invites::{self, INVITE_TTL_NS};
    use crate::meta::users;
    use crate::test_support::mock_idp::{
        CLIENT_ID, CLIENT_SECRET, GithubEmail, GithubUser, MockIdp,
    };
    use crate::test_support::{TestApp, assert_logs_clean, capture_logs, set_limits};

    use crate::test_support::oauth::*;

    const PASSWORD: &str = "llmobs-ok1";

    fn secret(value: &str) -> Secret<String> {
        serde_json::from_str(&format!("\"{value}\"")).expect("secret")
    }

    fn oidc(issuer: &str) -> OidcAuth {
        OidcAuth {
            name: "mock".to_owned(),
            display_name: "Mock".to_owned(),
            issuer: issuer.to_owned(),
            client_id: CLIENT_ID.to_owned(),
            client_secret: secret(CLIENT_SECRET),
            scopes: vec![
                "openid".to_owned(),
                "email".to_owned(),
                "profile".to_owned(),
            ],
            allowed_domains: Vec::new(),
            auto_provision: None,
            link_existing_by_email: false,
        }
    }

    async fn oidc_app(idp: &MockIdp, signup: SignupPolicy) -> TestApp {
        let issuer = idp.issuer().to_owned();
        TestApp::with_config(move |config| {
            config.auth.signup = signup;
            config.auth.oidc = vec![oidc(&issuer)];
        })
        .await
    }

    async fn setup_admin(app: &TestApp) -> String {
        bootstrap::prepare(&app.state).await.expect("prepare");
        let token = app.state.setup.token().unwrap().expect("setup token");
        let response = app
            .request(Method::POST, "/api/auth/setup")
            .json(&json!({
                "setup_token": token,
                "email": "ada@example.com",
                "password": PASSWORD,
                "name": "Ada",
                "org_name": "Acme",
                "project_name": "Bot",
            }))
            .send()
            .await;
        assert_eq!(response.status, StatusCode::CREATED);
        one_cookie(&response, SESSION_NAME)
    }

    async fn signup(app: &TestApp, email: &str) -> String {
        let response = app
            .request(Method::POST, "/api/auth/signup")
            .json(&json!({
                "email": email,
                "password": PASSWORD,
                "name": "Grace",
            }))
            .send()
            .await;
        assert_eq!(response.status, StatusCode::CREATED);
        one_cookie(&response, SESSION_NAME)
    }

    async fn password_status(app: &TestApp, email: &str) -> StatusCode {
        app.request(Method::POST, "/api/auth/login")
            .json(&json!({ "email": email, "password": PASSWORD }))
            .send()
            .await
            .status
    }

    fn google(domains: &[&str], link: bool) -> GoogleAuth {
        GoogleAuth {
            client_id: CLIENT_ID.to_owned(),
            client_secret: secret(CLIENT_SECRET),
            allowed_domains: domains.iter().map(|domain| (*domain).to_owned()).collect(),
            link_existing_by_email: link,
        }
    }

    /// Returns the invitation's plaintext token.
    async fn invite(app: &TestApp, email: &str) -> String {
        let org_id: String = sqlx::query_scalar("SELECT id FROM orgs LIMIT 1")
            .fetch_one(app.state.meta.reader())
            .await
            .expect("org");
        let user_id: String = sqlx::query_scalar("SELECT id FROM users LIMIT 1")
            .fetch_one(app.state.meta.reader())
            .await
            .expect("user");
        let org = org_id.parse::<OrgId>().expect("org id");
        let user = Uuid::parse_str(&user_id).expect("user id");
        let now = app.state.clock.now_ns();
        let token = new_opaque_token().expect("token");
        let plaintext = token.plaintext.clone();
        let email = email.to_owned();
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    invites::replace_invite(
                        conn,
                        org,
                        &email,
                        OrgRole::Member,
                        &token.hash,
                        user,
                        now,
                        now + INVITE_TTL_NS,
                    )
                    .await
                    .map(|_| ())
                })
            })
            .await
            .expect("invite");
        plaintext
    }

    #[tokio::test]
    async fn oidc_login_creates_a_user_and_signs_in_again() {
        let idp = MockIdp::start().await;
        let app = oidc_app(&idp, SignupPolicy::Open).await;
        setup_admin(&app).await;

        let pending = begin(&app, "oidc:mock", "/traces").await;
        let cookie = set_cookies(
            &app.request(
                Method::GET,
                "/api/auth/authorize/oidc:mock?redirect=%2Ftraces",
            )
            .send()
            .await,
        );
        assert!(cookie[0].contains("HttpOnly"));
        assert!(cookie[0].contains("SameSite=Lax"));
        assert!(cookie[0].contains("Path=/api/auth/callback"));
        assert!(cookie[0].contains("Max-Age=600"));
        assert!(!cookie[0].contains("Secure"));

        let code = idp.issue_code(grant("grace", &pending, "grace@example.com", Some(true)));
        let response = assert_logs_clean(
            &[code.as_str(), CLIENT_SECRET, "grace@example.com", "eyJ"],
            finish(
                &app,
                "oidc:mock",
                &pending,
                &format!("code={code}&state={}", pending.state),
            ),
        )
        .await;
        assert_eq!(response.status, StatusCode::FOUND);
        assert_eq!(location(&response), "/traces");
        let session = one_cookie(&response, SESSION_NAME);
        assert!(cookie_header(&response, OAUTH_NAME).contains("Max-Age=0"));
        let user = me(&app, &session).await;
        assert_eq!(user.email, "grace@example.com");
        assert_eq!(user.name.as_deref(), Some("Grace"));
        assert!(!user.id.is_nil());
        let providers = identities::user_identities(app.state.meta.reader(), user.id)
            .await
            .expect("identities");
        assert_eq!(providers, vec!["oidc:mock".to_owned()]);
        let hash: Option<String> =
            sqlx::query_scalar("SELECT password_hash FROM users WHERE id = ?")
                .bind(user.id.to_string())
                .fetch_one(app.state.meta.reader())
                .await
                .expect("user");
        assert!(hash.is_none());

        let again = begin(&app, "oidc:mock", "/traces").await;
        let mut second = grant("grace", &again, "grace@example.com", Some(false));
        second.claims.sub = "grace".to_owned();
        let code = idp.issue_code(second);
        let response = finish(
            &app,
            "oidc:mock",
            &again,
            &format!("code={code}&state={}", again.state),
        )
        .await;
        assert_eq!(location(&response), "/traces");
        let signed_in = me(&app, &one_cookie(&response, SESSION_NAME)).await;
        assert_eq!(signed_in.id, user.id);
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM identities")
            .fetch_one(app.state.meta.reader())
            .await
            .expect("count");
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn empty_database_requires_setup() {
        let idp = MockIdp::start().await;
        let app = oidc_app(&idp, SignupPolicy::Open).await;
        let pending = begin(&app, "oidc:mock", "/").await;
        let code = idp.issue_code(grant("grace", &pending, "grace@example.com", Some(true)));
        let response = finish(
            &app,
            "oidc:mock",
            &pending,
            &format!("code={code}&state={}", pending.state),
        )
        .await;
        assert!(location(&response).contains("error=setup_required"));
        assert!(
            set_cookies(&response)
                .iter()
                .all(|cookie| cookie.starts_with(OAUTH_NAME))
        );
    }

    #[tokio::test]
    async fn bad_state_is_invalid_state() {
        let idp = MockIdp::start().await;
        let app = oidc_app(&idp, SignupPolicy::Open).await;
        setup_admin(&app).await;
        let pending = begin(&app, "oidc:mock", "/traces").await;

        let missing = app
            .request(Method::GET, "/api/auth/callback/oidc:mock")
            .send()
            .await;
        assert!(location(&missing).contains("error=invalid_state"));

        let wrong = finish(&app, "oidc:mock", &pending, "state=nope&code=abc").await;
        assert!(location(&wrong).contains("error=invalid_state"));

        let other = app
            .request(
                Method::GET,
                &format!("/api/auth/callback/github?state={}&code=abc", pending.state),
            )
            .header(header::COOKIE.as_str(), &pending.cookie)
            .send()
            .await;
        assert!(location(&other).contains("error=invalid_state"));

        let denied = app
            .request(
                Method::GET,
                "/api/auth/callback/oidc:mock?error=access_denied",
            )
            .send()
            .await;
        assert!(location(&denied).contains("error=invalid_state"));

        let mut tampered = pending.cookie.clone();
        let end = tampered.len() - 1;
        let flipped = if tampered.as_bytes()[end] == b'A' {
            "B"
        } else {
            "A"
        };
        tampered.replace_range(end.., flipped);
        let bad = app
            .request(
                Method::GET,
                &format!("/api/auth/callback/oidc:mock?state={}", pending.state),
            )
            .header(header::COOKIE.as_str(), &tampered)
            .send()
            .await;
        assert!(location(&bad).contains("error=invalid_state"));

        app.state.clock.advance(Duration::from_secs(601));
        let stale = finish(
            &app,
            "oidc:mock",
            &pending,
            &format!("state={}", pending.state),
        )
        .await;
        assert!(location(&stale).contains("error=invalid_state"));
        assert!(cookie_header(&stale, OAUTH_NAME).contains("Max-Age=0"));
    }

    #[tokio::test]
    async fn rejected_tokens_are_provider_error() {
        let idp = MockIdp::start().await;
        let app = oidc_app(&idp, SignupPolicy::Open).await;
        setup_admin(&app).await;

        enum Bad {
            Nonce,
            Audience,
            Issuer,
            Expired,
            ForeignKey,
        }
        for bad in [
            Bad::Nonce,
            Bad::Audience,
            Bad::Issuer,
            Bad::Expired,
            Bad::ForeignKey,
        ] {
            let pending = begin(&app, "oidc:mock", "/traces").await;
            let mut issued = grant("grace", &pending, "grace@example.com", Some(true));
            match bad {
                Bad::Nonce => issued.claims.nonce = Some("other-nonce".to_owned()),
                Bad::Audience => issued.claims.aud = Some("other-client".to_owned()),
                Bad::Issuer => issued.claims.iss = Some("https://evil.example".to_owned()),
                Bad::Expired => issued.claims.exp = Some(1),
                Bad::ForeignKey => issued.foreign_key = true,
            }
            let code = idp.issue_code(issued);
            let response = finish(
                &app,
                "oidc:mock",
                &pending,
                &format!("code={code}&state={}", pending.state),
            )
            .await;
            assert!(
                location(&response).contains("error=provider_error"),
                "{}",
                location(&response)
            );
            assert!(
                set_cookies(&response)
                    .iter()
                    .all(|cookie| !cookie.starts_with(SESSION_NAME))
            );
        }

        let pending = begin(&app, "oidc:mock", "/traces").await;
        let missing = finish(
            &app,
            "oidc:mock",
            &pending,
            &format!("state={}", pending.state),
        )
        .await;
        assert!(location(&missing).contains("error=provider_error"));
        let denied = finish(
            &app,
            "oidc:mock",
            &pending,
            &format!("state={}&error=access_denied", pending.state),
        )
        .await;
        assert!(location(&denied).contains("error=provider_error"));
    }

    #[tokio::test]
    async fn a_rotated_signing_key_is_fetched_once() {
        let idp = MockIdp::start().await;
        let app = oidc_app(&idp, SignupPolicy::Open).await;
        setup_admin(&app).await;
        let pending = begin(&app, "oidc:mock", "/traces").await;
        let before = idp.discovery_hits();
        idp.rotate();
        let code = idp.issue_code(grant("grace", &pending, "grace@example.com", Some(true)));
        let response = finish(
            &app,
            "oidc:mock",
            &pending,
            &format!("code={code}&state={}", pending.state),
        )
        .await;
        assert_eq!(location(&response), "/traces");
        assert!(idp.discovery_hits() > before);
    }

    #[tokio::test]
    async fn unverified_email_does_not_create_a_user() {
        let idp = MockIdp::start().await;
        let app = oidc_app(&idp, SignupPolicy::Open).await;
        setup_admin(&app).await;
        for verified in [Some(false), None] {
            let pending = begin(&app, "oidc:mock", "/").await;
            let code = idp.issue_code(grant("grace", &pending, "grace@example.com", verified));
            let response = finish(
                &app,
                "oidc:mock",
                &pending,
                &format!("code={code}&state={}", pending.state),
            )
            .await;
            assert!(location(&response).contains("error=email_not_verified"));
        }
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users")
            .fetch_one(app.state.meta.reader())
            .await
            .expect("count");
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn an_existing_email_is_not_linked() {
        let idp = MockIdp::start().await;
        let app = oidc_app(&idp, SignupPolicy::Open).await;
        setup_admin(&app).await;
        let pending = begin(&app, "oidc:mock", "/").await;
        let mut grant = grant("grace", &pending, "ada@example.com", Some(true));
        grant.claims.sub = "someone-else".to_owned();
        let code = idp.issue_code(grant);
        let response = finish(
            &app,
            "oidc:mock",
            &pending,
            &format!("code={code}&state={}", pending.state),
        )
        .await;
        assert!(location(&response).contains("error=link_refused"));
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM identities")
            .fetch_one(app.state.meta.reader())
            .await
            .expect("count");
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn a_callback_signs_in_the_code_not_the_existing_session() {
        let idp = MockIdp::start().await;
        let app = oidc_app(&idp, SignupPolicy::Open).await;
        let ada = setup_admin(&app).await;
        let ada_user = me(&app, &ada).await;
        let pending = begin(&app, "oidc:mock", "/traces").await;
        let code = idp.issue_code(grant("grace", &pending, "grace@example.com", Some(true)));
        let response = app
            .request(
                Method::GET,
                &format!(
                    "/api/auth/callback/oidc:mock?code={code}&state={}",
                    pending.state
                ),
            )
            .header(
                header::COOKIE.as_str(),
                &format!("{}; {}", pending.cookie, ada),
            )
            .send()
            .await;
        assert_eq!(location(&response), "/traces");
        let signed_in = me(&app, &one_cookie(&response, SESSION_NAME)).await;
        assert_eq!(signed_in.email, "grace@example.com");
        assert_ne!(signed_in.id, ada_user.id);
        let ada_providers = identities::user_identities(app.state.meta.reader(), ada_user.id)
            .await
            .expect("ada");
        assert!(ada_providers.is_empty());
    }

    #[tokio::test]
    async fn signup_policy_gates_new_users() {
        let idp = MockIdp::start().await;
        let app = oidc_app(&idp, SignupPolicy::InviteOnly).await;
        setup_admin(&app).await;
        let pending = begin(&app, "oidc:mock", "/").await;
        let code = idp.issue_code(grant("grace", &pending, "grace@example.com", Some(true)));
        let closed = finish(
            &app,
            "oidc:mock",
            &pending,
            &format!("code={code}&state={}", pending.state),
        )
        .await;
        assert!(location(&closed).contains("error=signup_closed"));

        invite(&app, "grace@example.com").await;
        let pending = begin(&app, "oidc:mock", "/").await;
        let code = idp.issue_code(grant("grace", &pending, "grace@example.com", Some(true)));
        let created = finish(
            &app,
            "oidc:mock",
            &pending,
            &format!("code={code}&state={}", pending.state),
        )
        .await;
        assert_eq!(location(&created), "/");
        let accepted: Option<i64> =
            sqlx::query_scalar("SELECT accepted_at FROM invites WHERE email = ?")
                .bind("grace@example.com")
                .fetch_one(app.state.meta.reader())
                .await
                .expect("invite");
        assert!(accepted.is_none());

        let idp = MockIdp::start().await;
        let app = oidc_app(&idp, SignupPolicy::Disabled).await;
        setup_admin(&app).await;
        let pending = begin(&app, "oidc:mock", "/").await;
        let code = idp.issue_code(grant("grace", &pending, "grace@example.com", Some(true)));
        let disabled = finish(
            &app,
            "oidc:mock",
            &pending,
            &format!("code={code}&state={}", pending.state),
        )
        .await;
        assert!(location(&disabled).contains("error=signup_closed"));
    }

    #[tokio::test]
    async fn redirects_that_leave_the_origin_become_root() {
        let idp = MockIdp::start().await;
        let app = oidc_app(&idp, SignupPolicy::Open).await;
        for value in ["//evil.example", "/\\evil", "https://evil.example"] {
            let response = app
                .request(
                    Method::GET,
                    &format!(
                        "/api/auth/authorize/oidc:mock?{}",
                        serde_urlencoded::to_string([("redirect", value)]).expect("query")
                    ),
                )
                .send()
                .await;
            assert_eq!(saved_redirect(&response), "/", "{value}");
        }
        setup_admin(&app).await;
        let pending = begin(&app, "oidc:mock", "/traces").await;
        assert_eq!(
            saved_redirect(
                &app.request(
                    Method::GET,
                    "/api/auth/authorize/oidc:mock?redirect=%2Ftraces"
                )
                .send()
                .await
            ),
            "/traces"
        );
        let code = idp.issue_code(grant("grace", &pending, "grace@example.com", Some(true)));
        let response = finish(
            &app,
            "oidc:mock",
            &pending,
            &format!("code={code}&state={}", pending.state),
        )
        .await;
        assert_eq!(location(&response), "/traces");
    }

    #[tokio::test]
    async fn a_disabled_identity_cannot_sign_in() {
        let idp = MockIdp::start().await;
        let app = oidc_app(&idp, SignupPolicy::Open).await;
        setup_admin(&app).await;
        let pending = begin(&app, "oidc:mock", "/").await;
        let code = idp.issue_code(grant("grace", &pending, "grace@example.com", Some(true)));
        let created = finish(
            &app,
            "oidc:mock",
            &pending,
            &format!("code={code}&state={}", pending.state),
        )
        .await;
        let user = me(&app, &one_cookie(&created, SESSION_NAME)).await;
        let now = app.state.clock.now_ns();
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move { users::set_disabled(conn, user.id, Some(now), now).await })
            })
            .await
            .expect("disable");
        let pending = begin(&app, "oidc:mock", "/").await;
        let code = idp.issue_code(grant("grace", &pending, "grace@example.com", Some(true)));
        let response = finish(
            &app,
            "oidc:mock",
            &pending,
            &format!("code={code}&state={}", pending.state),
        )
        .await;
        assert!(location(&response).contains("error=account_disabled"));
        assert!(
            set_cookies(&response)
                .iter()
                .all(|cookie| !cookie.starts_with(SESSION_NAME))
        );
    }

    #[tokio::test]
    async fn github_uses_the_numeric_id_and_the_verified_primary() {
        let idp = MockIdp::start().await;
        let base = idp.issuer().to_owned();
        let app = TestApp::with_github(&base, |config| {
            config.auth.signup = SignupPolicy::Open;
            config.auth.github = Some(GithubAuth {
                client_id: CLIENT_ID.to_owned(),
                client_secret: secret(CLIENT_SECRET),
                link_existing_by_email: true,
            });
        })
        .await;
        setup_admin(&app).await;
        let pending = begin(&app, "github", "/traces").await;
        assert!(pending.nonce.is_none());
        let code = idp.issue_github_code(GithubUser {
            id: 42,
            login: "not-the-subject".to_owned(),
            name: Some(" Grace ".to_owned()),
            emails: vec![GithubEmail {
                email: "grace@example.com".to_owned(),
                primary: true,
                verified: true,
            }],
        });
        let response = assert_logs_clean(
            &[code.as_str(), CLIENT_SECRET, "grace@example.com"],
            finish(
                &app,
                "github",
                &pending,
                &format!("code={code}&state={}", pending.state),
            ),
        )
        .await;
        assert_eq!(location(&response), "/traces");
        let user = me(&app, &one_cookie(&response, SESSION_NAME)).await;
        assert_eq!(user.name.as_deref(), Some("Grace"));
        let subject: String =
            sqlx::query_scalar("SELECT subject FROM identities WHERE provider = 'github'")
                .fetch_one(app.state.meta.reader())
                .await
                .expect("subject");
        assert_eq!(subject, "42");

        let pending = begin(&app, "github", "/").await;
        let code = idp.issue_github_code(GithubUser {
            id: 43,
            login: "ada".to_owned(),
            name: None,
            emails: vec![
                GithubEmail {
                    email: "primary@example.com".to_owned(),
                    primary: true,
                    verified: false,
                },
                GithubEmail {
                    email: "other@example.com".to_owned(),
                    primary: false,
                    verified: true,
                },
            ],
        });
        let response = finish(
            &app,
            "github",
            &pending,
            &format!("code={code}&state={}", pending.state),
        )
        .await;
        assert!(location(&response).contains("error=email_not_verified"));
    }

    #[tokio::test]
    async fn discovery_failure_does_not_set_a_cookie() {
        let idp = MockIdp::start().await;
        idp.fail_discovery();
        let app = oidc_app(&idp, SignupPolicy::Open).await;
        let response = app
            .request(Method::GET, "/api/auth/authorize/oidc:mock")
            .send()
            .await;
        assert_eq!(response.status, StatusCode::FOUND);
        assert!(location(&response).contains("error=provider_unavailable"));
        assert!(set_cookies(&response).is_empty());

        let missing = app
            .request(Method::GET, "/api/auth/authorize/no-such")
            .send()
            .await;
        assert_eq!(missing.status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn google_allowed_domains_require_the_email_and_hd() {
        let idp = MockIdp::start().await;
        let base = idp.issuer().to_owned();
        let app = TestApp::with_google(&base, |config| {
            config.auth.signup = SignupPolicy::Open;
            config.auth.google = Some(google(&["acme.com"], true));
        })
        .await;
        setup_admin(&app).await;

        let pending = begin(&app, "google", "/").await;
        let mut issued = grant("grace", &pending, "ada@acme.com", Some(true));
        issued.claims.hd = Some("acme.com".to_owned());
        let code = idp.issue_code(issued);
        let response = finish(
            &app,
            "google",
            &pending,
            &format!("code={code}&state={}", pending.state),
        )
        .await;
        assert_eq!(location(&response), "/");

        for hd in [None, Some("other.com".to_owned())] {
            let pending = begin(&app, "google", "/").await;
            let mut issued = grant("grace", &pending, "ada@acme.com", Some(true));
            issued.claims.sub = format!("other-{}", hd.as_deref().unwrap_or("missing"));
            issued.claims.hd = hd;
            let code = idp.issue_code(issued);
            let response = finish(
                &app,
                "google",
                &pending,
                &format!("code={code}&state={}", pending.state),
            )
            .await;
            assert!(location(&response).contains("error=domain_not_allowed"));
        }
    }

    #[tokio::test]
    async fn an_oidc_domain_list_rejects_an_outside_email() {
        let idp = MockIdp::start().await;
        let issuer = idp.issuer().to_owned();
        let app = TestApp::with_config(move |config| {
            config.auth.signup = SignupPolicy::Open;
            let mut provider = oidc(&issuer);
            provider.allowed_domains = vec!["acme.com".to_owned()];
            config.auth.oidc = vec![provider];
        })
        .await;
        setup_admin(&app).await;
        let pending = begin(&app, "oidc:mock", "/").await;
        let code = idp.issue_code(grant("grace", &pending, "grace@example.com", Some(true)));
        let response = finish(
            &app,
            "oidc:mock",
            &pending,
            &format!("code={code}&state={}", pending.state),
        )
        .await;
        assert!(location(&response).contains("error=domain_not_allowed"));
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users")
            .fetch_one(app.state.meta.reader())
            .await
            .expect("count");
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn github_links_a_verified_user_and_revokes_sessions_and_tokens() {
        let idp = MockIdp::start().await;
        let base = idp.issuer().to_owned();
        let app = TestApp::with_github(&base, |config| {
            config.auth.signup = SignupPolicy::Open;
            config.auth.github = Some(GithubAuth {
                client_id: CLIENT_ID.to_owned(),
                client_secret: secret(CLIENT_SECRET),
                link_existing_by_email: true,
            });
        })
        .await;
        setup_admin(&app).await;
        let session = signup(&app, "grace@example.com").await;
        let user = me(&app, &session).await;
        let now = app.state.clock.now_ns();
        let user_id = user.id;
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    users::set_email_verified(
                        conn,
                        user_id,
                        now,
                        users::EmailVerifiedVia::MailToken,
                    )
                    .await
                })
            })
            .await
            .expect("verify");
        let minted = app
            .request(Method::POST, "/api/v1/me/tokens")
            .header(header::COOKIE.as_str(), &session)
            .json(&json!({ "name": "cli", "scopes": ["read"] }))
            .send()
            .await;
        assert_eq!(minted.status, StatusCode::CREATED);
        let secret: String = minted.json::<serde_json::Value>()["secret"]
            .as_str()
            .expect("secret")
            .to_owned();
        let before = app
            .request(Method::GET, "/api/auth/me")
            .header(header::AUTHORIZATION.as_str(), &format!("Bearer {secret}"))
            .send()
            .await;
        assert_eq!(before.status, StatusCode::OK);

        let pending = begin(&app, "github", "/traces").await;
        let code = idp.issue_github_code(GithubUser {
            id: 42,
            login: "grace".to_owned(),
            name: Some("Grace".to_owned()),
            emails: vec![GithubEmail {
                email: "grace@example.com".to_owned(),
                primary: true,
                verified: true,
            }],
        });
        let (response, logs) = capture_logs(finish(
            &app,
            "github",
            &pending,
            &format!("code={code}&state={}", pending.state),
        ))
        .await;
        assert_eq!(location(&response), "/traces");
        assert!(logs.contains("identity linked"));
        assert!(!logs.contains("grace@example.com"));
        let old = app
            .request(Method::GET, "/api/auth/me")
            .header(header::COOKIE.as_str(), &session)
            .send()
            .await;
        assert_eq!(old.status, StatusCode::UNAUTHORIZED);
        let pat = app
            .request(Method::GET, "/api/auth/me")
            .header(header::AUTHORIZATION.as_str(), &format!("Bearer {secret}"))
            .send()
            .await;
        assert_eq!(pat.status, StatusCode::UNAUTHORIZED);
        assert_eq!(
            password_status(&app, "grace@example.com").await,
            StatusCode::OK
        );
        let hash: Option<String> =
            sqlx::query_scalar("SELECT password_hash FROM users WHERE email = ?")
                .bind("grace@example.com")
                .fetch_one(app.state.meta.reader())
                .await
                .expect("hash");
        assert!(hash.is_some());
        let providers = identities::user_identities(app.state.meta.reader(), user.id)
            .await
            .expect("identities");
        assert_eq!(providers, vec!["github".to_owned()]);
    }

    #[tokio::test]
    async fn generic_oidc_does_not_link_and_an_admin_is_never_linked() {
        let idp = MockIdp::start().await;
        let app = oidc_app(&idp, SignupPolicy::Open).await;
        setup_admin(&app).await;
        signup(&app, "grace@example.com").await;
        let pending = begin(&app, "oidc:mock", "/").await;
        let code = idp.issue_code(grant("grace", &pending, "grace@example.com", Some(true)));
        let response = finish(
            &app,
            "oidc:mock",
            &pending,
            &format!("code={code}&state={}", pending.state),
        )
        .await;
        assert!(location(&response).contains("error=link_refused"));

        let idp = MockIdp::start().await;
        let base = idp.issuer().to_owned();
        let app = TestApp::with_github(&base, |config| {
            config.auth.signup = SignupPolicy::Open;
            config.auth.github = Some(GithubAuth {
                client_id: CLIENT_ID.to_owned(),
                client_secret: secret(CLIENT_SECRET),
                link_existing_by_email: true,
            });
        })
        .await;
        setup_admin(&app).await;
        let pending = begin(&app, "github", "/").await;
        let code = idp.issue_github_code(GithubUser {
            id: 7,
            login: "ada".to_owned(),
            name: None,
            emails: vec![GithubEmail {
                email: "ada@example.com".to_owned(),
                primary: true,
                verified: true,
            }],
        });
        let response = finish(
            &app,
            "github",
            &pending,
            &format!("code={code}&state={}", pending.state),
        )
        .await;
        assert!(location(&response).contains("error=link_refused"));
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM identities")
            .fetch_one(app.state.meta.reader())
            .await
            .expect("count");
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn an_unverified_password_loses_that_password_when_google_links_it() {
        let idp = MockIdp::start().await;
        let base = idp.issuer().to_owned();
        let app = TestApp::with_google(&base, |config| {
            config.auth.signup = SignupPolicy::Open;
            config.auth.google = Some(google(&[], true));
        })
        .await;
        setup_admin(&app).await;
        signup(&app, "grace@example.com").await;
        let pending = begin(&app, "google", "/").await;
        let code = idp.issue_code(grant("grace", &pending, "grace@example.com", Some(true)));
        let response = finish(
            &app,
            "google",
            &pending,
            &format!("code={code}&state={}", pending.state),
        )
        .await;
        assert_eq!(location(&response), "/");
        let hash: Option<String> =
            sqlx::query_scalar("SELECT password_hash FROM users WHERE email = ?")
                .bind("grace@example.com")
                .fetch_one(app.state.meta.reader())
                .await
                .expect("hash");
        assert!(hash.is_none());
        let verified: Option<i64> =
            sqlx::query_scalar("SELECT email_verified_at FROM users WHERE email = ?")
                .bind("grace@example.com")
                .fetch_one(app.state.meta.reader())
                .await
                .expect("verified");
        assert!(verified.is_some());
        assert_eq!(
            password_status(&app, "grace@example.com").await,
            StatusCode::UNAUTHORIZED
        );
    }

    /// Auth review #9: an invitation token proves nothing about the mailbox, so an account made
    /// with it loses its password, sessions and tokens when the identity provider links it.
    #[tokio::test]
    async fn an_invite_verified_password_loses_that_password_when_google_links_it() {
        let idp = MockIdp::start().await;
        let base = idp.issuer().to_owned();
        let app = TestApp::with_google(&base, |config| {
            config.auth.signup = SignupPolicy::InviteOnly;
            config.auth.google = Some(google(&[], true));
        })
        .await;
        setup_admin(&app).await;
        // The inviter holds the token, so they register the victim's address with their own
        // password.
        let token = invite(&app, "victim@example.com").await;
        let created = app
            .request(Method::POST, "/api/auth/signup")
            .json(&json!({
                "email": "victim@example.com",
                "password": PASSWORD,
                "invite_token": token,
            }))
            .send()
            .await;
        assert_eq!(created.status, StatusCode::CREATED);
        let session = one_cookie(&created, SESSION_NAME);
        let minted = app
            .request(Method::POST, "/api/v1/me/tokens")
            .header(header::COOKIE.as_str(), &session)
            .json(&json!({ "name": "cli", "scopes": ["read"] }))
            .send()
            .await;
        assert_eq!(minted.status, StatusCode::CREATED);
        let pat: String = minted.json::<serde_json::Value>()["secret"]
            .as_str()
            .expect("secret")
            .to_owned();
        let via: Option<String> =
            sqlx::query_scalar("SELECT email_verified_via FROM users WHERE email = ?")
                .bind("victim@example.com")
                .fetch_one(app.state.meta.reader())
                .await
                .expect("via");
        assert_eq!(via.as_deref(), Some("invite"));
        assert_eq!(
            password_status(&app, "victim@example.com").await,
            StatusCode::OK
        );

        let pending = begin(&app, "google", "/").await;
        let code = idp.issue_code(grant("victim", &pending, "victim@example.com", Some(true)));
        let response = finish(
            &app,
            "google",
            &pending,
            &format!("code={code}&state={}", pending.state),
        )
        .await;
        assert_eq!(location(&response), "/");
        assert_eq!(
            password_status(&app, "victim@example.com").await,
            StatusCode::UNAUTHORIZED
        );
        let old_session = app
            .request(Method::GET, "/api/auth/me")
            .header(header::COOKIE.as_str(), &session)
            .send()
            .await;
        assert_eq!(old_session.status, StatusCode::UNAUTHORIZED);
        let old_pat = app
            .request(Method::GET, "/api/auth/me")
            .header(header::AUTHORIZATION.as_str(), &format!("Bearer {pat}"))
            .send()
            .await;
        assert_eq!(old_pat.status, StatusCode::UNAUTHORIZED);
        let via: Option<String> =
            sqlx::query_scalar("SELECT email_verified_via FROM users WHERE email = ?")
                .bind("victim@example.com")
                .fetch_one(app.state.meta.reader())
                .await
                .expect("via");
        assert_eq!(via.as_deref(), Some("idp"));
    }

    #[tokio::test]
    async fn a_disabled_account_is_not_linked() {
        let idp = MockIdp::start().await;
        let base = idp.issuer().to_owned();
        let app = TestApp::with_github(&base, |config| {
            config.auth.signup = SignupPolicy::Open;
            config.auth.github = Some(GithubAuth {
                client_id: CLIENT_ID.to_owned(),
                client_secret: secret(CLIENT_SECRET),
                link_existing_by_email: true,
            });
        })
        .await;
        setup_admin(&app).await;
        let session = signup(&app, "grace@example.com").await;
        let user = me(&app, &session).await;
        let now = app.state.clock.now_ns();
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move { users::set_disabled(conn, user.id, Some(now), now).await })
            })
            .await
            .expect("disable");
        let pending = begin(&app, "github", "/").await;
        let code = idp.issue_github_code(GithubUser {
            id: 42,
            login: "grace".to_owned(),
            name: None,
            emails: vec![GithubEmail {
                email: "grace@example.com".to_owned(),
                primary: true,
                verified: true,
            }],
        });
        let response = finish(
            &app,
            "github",
            &pending,
            &format!("code={code}&state={}", pending.state),
        )
        .await;
        assert!(location(&response).contains("error=account_disabled"));
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM identities")
            .fetch_one(app.state.meta.reader())
            .await
            .expect("count");
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn setup_through_sso_clears_the_token() {
        let idp = MockIdp::start().await;
        let app = oidc_app(&idp, SignupPolicy::Open).await;
        bootstrap::prepare(&app.state).await.expect("prepare");
        let token = app.state.setup.token().unwrap().expect("setup token");

        let pending = begin(&app, "oidc:mock", "/").await;
        let code = idp.issue_code(grant("grace", &pending, "ada@example.com", Some(true)));
        let missing = finish(
            &app,
            "oidc:mock",
            &pending,
            &format!("code={code}&state={}", pending.state),
        )
        .await;
        assert!(location(&missing).contains("error=setup_required"));

        let pending = begin_with(
            &app,
            "oidc:mock",
            &[("redirect", "/traces"), ("setup_token", &token)],
        )
        .await;
        let mut issued = grant("grace", &pending, "ada@example.com", Some(true));
        issued.claims.sub = "ada".to_owned();
        let code = idp.issue_code(issued);
        let (response, logs) = capture_logs(finish(
            &app,
            "oidc:mock",
            &pending,
            &format!("code={code}&state={}", pending.state),
        ))
        .await;
        assert_eq!(location(&response), "/traces");
        for needle in [
            token.as_str(),
            code.as_str(),
            CLIENT_SECRET,
            "ada@example.com",
            "eyJ",
        ] {
            assert!(!logs.contains(needle), "log contains {needle}: {logs}");
        }
        assert!(logs.contains("setup completed"), "{logs}");
        assert!(app.state.setup.token().unwrap().is_none());
        let admin: i64 = sqlx::query_scalar("SELECT is_instance_admin FROM users WHERE email = ?")
            .bind("ada@example.com")
            .fetch_one(app.state.meta.reader())
            .await
            .expect("admin");
        assert_eq!(admin, 1);

        let pending = begin_with(
            &app,
            "oidc:mock",
            &[("redirect", "/"), ("setup_token", &token)],
        )
        .await;
        let mut grant = grant("grace", &pending, "grace@example.com", Some(true));
        grant.claims.sub = "grace".to_owned();
        let code = idp.issue_code(grant);
        let later = finish(
            &app,
            "oidc:mock",
            &pending,
            &format!("code={code}&state={}", pending.state),
        )
        .await;
        assert_eq!(location(&later), "/");
        let admin: i64 = sqlx::query_scalar("SELECT is_instance_admin FROM users WHERE email = ?")
            .bind("grace@example.com")
            .fetch_one(app.state.meta.reader())
            .await
            .expect("admin");
        assert_eq!(admin, 0);
        assert!(app.state.setup.token().unwrap().is_none());
    }

    #[tokio::test]
    async fn auto_provision_joins_the_org_and_invites_under_invite_only() {
        let idp = MockIdp::start().await;
        let issuer = idp.issuer().to_owned();
        let app = TestApp::with_config(move |config| {
            config.auth.signup = SignupPolicy::InviteOnly;
            let mut provider = oidc(&issuer);
            provider.auto_provision = Some(AutoProvision {
                org: "acme".to_owned(),
                role: "member".to_owned(),
            });
            config.auth.oidc = vec![provider];
        })
        .await;
        setup_admin(&app).await;
        let pending = begin(&app, "oidc:mock", "/").await;
        let code = idp.issue_code(grant("grace", &pending, "grace@example.com", Some(true)));
        let response = finish(
            &app,
            "oidc:mock",
            &pending,
            &format!("code={code}&state={}", pending.state),
        )
        .await;
        assert_eq!(location(&response), "/");
        let user = me(&app, &one_cookie(&response, SESSION_NAME)).await;
        let role: String = sqlx::query_scalar("SELECT role FROM org_members WHERE user_id = ?")
            .bind(user.id.to_string())
            .fetch_one(app.state.meta.reader())
            .await
            .expect("role");
        assert_eq!(role, "member");

        let pending = begin(&app, "oidc:mock", "/").await;
        let code = idp.issue_code(grant("grace", &pending, "grace@example.com", Some(true)));
        let again = finish(
            &app,
            "oidc:mock",
            &pending,
            &format!("code={code}&state={}", pending.state),
        )
        .await;
        assert_eq!(location(&again), "/");
        let memberships: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM org_members WHERE user_id = ?")
                .bind(user.id.to_string())
                .fetch_one(app.state.meta.reader())
                .await
                .expect("count");
        assert_eq!(memberships, 1);
    }

    #[tokio::test]
    async fn auto_provision_at_the_seat_limit_signs_in_without_a_membership() {
        let idp = MockIdp::start().await;
        let issuer = idp.issuer().to_owned();
        let app = TestApp::with_config(move |config| {
            config.auth.signup = SignupPolicy::InviteOnly;
            let mut provider = oidc(&issuer);
            provider.auto_provision = Some(AutoProvision {
                org: "acme".to_owned(),
                role: "member".to_owned(),
            });
            config.auth.oidc = vec![provider];
        })
        .await;
        setup_admin(&app).await;
        let org = crate::meta::orgs::org_by_slug(app.state.meta.reader(), "acme")
            .await
            .unwrap()
            .unwrap();
        // The owner from setup fills the only seat.
        set_limits(
            &app,
            org.id,
            "2026-01-01T00:00:00Z",
            &json!({"max_seats": 1}),
        );
        let pending = begin(&app, "oidc:mock", "/").await;
        let code = idp.issue_code(grant("grace", &pending, "grace@example.com", Some(true)));
        let response = finish(
            &app,
            "oidc:mock",
            &pending,
            &format!("code={code}&state={}", pending.state),
        )
        .await;
        assert_eq!(location(&response), "/");
        let user = me(&app, &one_cookie(&response, SESSION_NAME)).await;
        let memberships: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM org_members WHERE user_id = ?")
                .bind(user.id.to_string())
                .fetch_one(app.state.meta.reader())
                .await
                .expect("count");
        assert_eq!(memberships, 0);
    }

    #[tokio::test]
    async fn a_missing_provision_org_logs_the_slug_and_still_signs_in() {
        let idp = MockIdp::start().await;
        let issuer = idp.issuer().to_owned();
        let app = TestApp::with_config(move |config| {
            config.auth.signup = SignupPolicy::Open;
            let mut provider = oidc(&issuer);
            provider.auto_provision = Some(AutoProvision {
                org: "absent-org".to_owned(),
                role: "viewer".to_owned(),
            });
            config.auth.oidc = vec![provider];
        })
        .await;
        setup_admin(&app).await;
        let pending = begin(&app, "oidc:mock", "/").await;
        let code = idp.issue_code(grant("grace", &pending, "grace@example.com", Some(true)));
        let (response, logs) = capture_logs(finish(
            &app,
            "oidc:mock",
            &pending,
            &format!("code={code}&state={}", pending.state),
        ))
        .await;
        assert_eq!(location(&response), "/");
        assert!(logs.contains("absent-org"), "{logs}");
        assert!(!logs.contains("grace@example.com"), "{logs}");
        let memberships: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM org_members WHERE user_id != \
             (SELECT id FROM users WHERE email = 'ada@example.com')",
        )
        .fetch_one(app.state.meta.reader())
        .await
        .expect("count");
        assert_eq!(memberships, 0);
    }

    #[tokio::test]
    async fn signup_domains_apply_to_provider_signups() {
        let idp = MockIdp::start().await;
        let issuer = idp.issuer().to_owned();
        let app = TestApp::with_config(move |config| {
            config.auth.signup = SignupPolicy::Open;
            config.auth.signup_domains = vec!["acme.test".to_owned()];
            config.auth.oidc = vec![oidc(&issuer)];
        })
        .await;
        setup_admin(&app).await;
        let pending = begin(&app, "oidc:mock", "/").await;
        let code = idp.issue_code(grant("grace", &pending, "grace@example.com", Some(true)));
        let closed = finish(
            &app,
            "oidc:mock",
            &pending,
            &format!("code={code}&state={}", pending.state),
        )
        .await;
        assert!(location(&closed).contains("error=signup_closed"));

        let pending = begin(&app, "oidc:mock", "/").await;
        let mut grant = grant("grace", &pending, "grace@acme.test", Some(true));
        grant.claims.sub = "inside".to_owned();
        let code = idp.issue_code(grant);
        let opened = finish(
            &app,
            "oidc:mock",
            &pending,
            &format!("code={code}&state={}", pending.state),
        )
        .await;
        assert_eq!(location(&opened), "/");
    }

    #[tokio::test]
    async fn instance_providers_are_refused_under_org_enforcement() {
        let idp = MockIdp::start().await;
        let issuer = idp.issuer().to_owned();
        let app = TestApp::with_config(move |config| {
            config.security.allow_private_network_egress = true;
            config.auth.signup = SignupPolicy::Open;
            config.auth.oidc = vec![oidc(&issuer)];
        })
        .await;
        let seeded = app.seed().await;
        crate::test_support::configure_org_sso(&app, seeded.org_id, &idp, "acme.test", true, true)
            .await;

        // A new person of the enforced domain is refused and nothing is written.
        let pending = begin(&app, "oidc:mock", "/").await;
        let code = idp.issue_code(grant("ada-sub", &pending, "ada@acme.test", Some(true)));
        let refused = finish(
            &app,
            "oidc:mock",
            &pending,
            &format!("code={code}&state={}", pending.state),
        )
        .await;
        assert_eq!(location(&refused), "/login?error=sso_required");
        let written: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM users WHERE email = 'ada@acme.test'")
                .fetch_one(app.state.meta.reader())
                .await
                .unwrap();
        assert_eq!(written, 0);

        // An existing non-admin of the domain is refused too.
        let now = app.state.clock.now_ns();
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    users::insert_user(
                        conn,
                        users::NewUser {
                            email: "grace@acme.test".to_owned(),
                            name: None,
                            password_hash: None,
                            email_verified_at: Some(now),
                            email_verified_via: Some(users::EmailVerifiedVia::MailToken),
                            is_instance_admin: false,
                        },
                        now,
                    )
                    .await?;
                    Ok(())
                })
            })
            .await
            .unwrap();
        let pending = begin(&app, "oidc:mock", "/").await;
        let code = idp.issue_code(grant("grace-sub", &pending, "grace@acme.test", Some(true)));
        let refused = finish(
            &app,
            "oidc:mock",
            &pending,
            &format!("code={code}&state={}", pending.state),
        )
        .await;
        assert_eq!(location(&refused), "/login?error=sso_required");

        // An instance admin of the domain passes the check; linking refuses admins anyway.
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    users::insert_user(
                        conn,
                        users::NewUser {
                            email: "root@acme.test".to_owned(),
                            name: None,
                            password_hash: None,
                            email_verified_at: Some(now),
                            email_verified_via: Some(users::EmailVerifiedVia::MailToken),
                            is_instance_admin: true,
                        },
                        now,
                    )
                    .await?;
                    Ok(())
                })
            })
            .await
            .unwrap();
        let pending = begin(&app, "oidc:mock", "/").await;
        let code = idp.issue_code(grant("root-sub", &pending, "root@acme.test", Some(true)));
        let admitted = finish(
            &app,
            "oidc:mock",
            &pending,
            &format!("code={code}&state={}", pending.state),
        )
        .await;
        assert_eq!(location(&admitted), "/login?error=link_refused");
    }

    #[tokio::test]
    async fn authorize_is_refused_in_restore_mode() {
        let app = TestApp::with_config(|config| config.restore_mode = true).await;
        let response = app
            .request(Method::GET, "/api/auth/authorize/github")
            .send()
            .await;
        crate::http::restore::tests::assert_refused(&response, "external sign-in");
    }

    #[tokio::test]
    async fn callback_is_refused_in_restore_mode() {
        let app = TestApp::with_config(|config| config.restore_mode = true).await;
        let response = app
            .request(Method::GET, "/api/auth/callback/github?state=s&code=c")
            .send()
            .await;
        crate::http::restore::tests::assert_refused(&response, "external sign-in");
    }
}
