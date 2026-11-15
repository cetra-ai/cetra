// SPDX-License-Identifier: Elastic-2.0
//! Sign-in through an org's provider (track P M5, D-P-55, D-P-56, D-P-57, D-P-65).
#![cfg_attr(not(test), allow(dead_code))]

use std::io;

use axum::extract::State;
use axum::http::{HeaderMap, Method};
use axum::response::Response;
use axum::routing::get;
use llmobs_core::ids::OrgId;
use serde::Deserialize;
use sqlx::SqliteConnection;
use uuid::Uuid;

use super::{normalize_domain, store};
use crate::api::account::SignupPolicy;
use crate::api::entitlements::Feature;
use crate::auth::OrgRole;
use crate::auth::oidc::ExchangeError;
use crate::auth::oidc::flow::{self, LoggedIn, LoginError, display_name};
use crate::auth::oidc::state_cookie::{self, OAuthState};
use crate::auth::roles::org_role_str;
use crate::auth::secret::new_opaque_token;
use crate::auth::session::resolve_cookie_header;
use crate::entitlements::{of, over_limit};
use crate::http::AppState;
use crate::http::access::{Access, RouteDef};
use crate::http::client_ip::ClientIp;
use crate::http::error::{self, ApiError};
use crate::meta::users::{NewUser, UserRow, normalize_email};
use crate::meta::{
    MetaError, identities, invites, members, orgs, personal_tokens, users, web_sessions,
};
use crate::routes::oauth::redirect;

/// The callback's provider segment and the state cookie's provider.
pub(crate) const PROVIDER: &str = "sso";

/// The org sign-in routes. P11 adds discovery.
pub(crate) fn routes() -> Vec<RouteDef> {
    vec![RouteDef {
        method: Method::GET,
        path: "/api/auth/sso/authorize",
        access: Access::Public,
        documented: true,
        handler: get(authorize),
    }]
}

#[derive(Deserialize)]
struct SsoAuthorizeQuery {
    #[serde(default)]
    domain: Option<String>,
    #[serde(default)]
    redirect: Option<String>,
}

fn internal(error: impl std::error::Error + Send + Sync + 'static) -> ApiError {
    ApiError::Internal(Box::new(error))
}

fn unavailable() -> Result<Response, ApiError> {
    redirect("/login?error=sso_unavailable", std::iter::empty())
}

/// `GET /api/auth/sso/authorize?domain=&redirect=` (D-P-60, D-P-57).
async fn authorize(
    State(state): State<AppState>,
    ClientIp(ip): ClientIp,
    headers: HeaderMap,
    error::Query(query): error::Query<SsoAuthorizeQuery>,
) -> Result<Response, ApiError> {
    let now = state.clock.now_ns();
    state
        .limits
        .sso_discover
        .check(&ip, now)
        .map_err(|retry| ApiError::RateLimited {
            retry_after_s: retry.0.as_secs().max(1),
        })?;
    let Some(domain) = query.domain.as_deref().and_then(normalize_domain) else {
        return unavailable();
    };
    let Some(route) = state.org_sso.route_domain(&domain) else {
        return unavailable();
    };
    let Some(provider) = state.org_sso.provider(route.org_id) else {
        return unavailable();
    };
    // A disabled provider signs in only an owner's Test sign-in (D-P-57). The answer is the
    // same as for an unknown domain, so it does not reveal the provider.
    let test = if route.enabled {
        false
    } else {
        let owner = match resolve_cookie_header(&state, &headers, now).await? {
            Some((row, _)) => {
                members::org_role(state.meta.reader(), route.org_id, row.user.id).await?
                    == Some(OrgRole::Owner)
            }
            None => false,
        };
        if !owner {
            return unavailable();
        }
        true
    };
    let redirect_uri = state.config.public_url.join("/api/auth/callback/sso");
    let csrf = new_opaque_token().map_err(internal)?.plaintext;
    let nonce = new_opaque_token().map_err(internal)?.plaintext;
    let (location, verifier) = match provider
        .oidc
        .authorization_url(&redirect_uri, &csrf, &nonce, now)
        .await
    {
        Ok(pair) => pair,
        Err(ExchangeError::Unavailable) => {
            return redirect("/login?error=provider_unavailable", std::iter::empty());
        }
        Err(ExchangeError::Rejected) => {
            return Err(internal(io::Error::new(
                io::ErrorKind::InvalidData,
                "oauth authorization url",
            )));
        }
    };
    let cookie = state_cookie::seal(
        &state.oauth_key,
        &OAuthState {
            provider: PROVIDER.to_owned(),
            state: csrf,
            nonce,
            pkce_verifier: verifier,
            redirect: state_cookie::safe_redirect(query.redirect.as_deref()),
            setup_token: None,
            created_ns: now,
            org_id: Some(route.org_id.as_uuid()),
            test,
        },
        state.config.public_url.is_https(),
    )
    .map_err(|()| {
        internal(io::Error::new(
            io::ErrorKind::InvalidData,
            "oauth state cookie",
        ))
    })?;
    redirect(&location, [cookie])
}

/// The callback's work for an org provider (D-P-65). The route starts the session.
///
/// # Errors
///
/// Returns a [`LoginError`]; the codes are the same closed set as the instance providers.
pub(crate) async fn finish(
    state: &AppState,
    headers: &HeaderMap,
    query_state: Option<&str>,
    code: Option<&str>,
    provider_error: bool,
) -> Result<LoggedIn, LoginError> {
    let now = state.clock.now_ns();
    let saved = state_cookie::open(
        &state.oauth_key,
        headers,
        PROVIDER,
        query_state.unwrap_or(""),
        now,
    )
    .map_err(|()| LoginError::InvalidState)?;
    let org = OrgId::from_uuid(saved.org_id.ok_or(LoginError::InvalidState)?);
    if provider_error {
        return Err(LoginError::ProviderError);
    }
    let Some(code) = code else {
        return Err(LoginError::ProviderError);
    };
    let Some(provider) = state.org_sso.provider(org) else {
        return Err(LoginError::SsoUnavailable);
    };
    let enabled = state.org_sso.flags(org).is_some_and(|flags| flags.enabled);
    if !enabled && !saved.test {
        return Err(LoginError::SsoUnavailable);
    }
    let redirect_uri = state.config.public_url.join("/api/auth/callback/sso");
    let claims = provider
        .oidc
        .exchange_code(&redirect_uri, code, &saved.pkce_verifier, &saved.nonce, now)
        .await
        .map_err(flow::login_exchange)?;
    if claims.subject.is_empty() {
        return Err(LoginError::ProviderError);
    }
    // An explicit `false` is refused; an absent claim is trusted (Entra omits it), since the
    // org proved the domain and runs the provider (D-P-55).
    if claims.email_verified == Some(false) {
        return Err(LoginError::EmailNotVerified);
    }
    let email = claims
        .email
        .as_deref()
        .and_then(|email| normalize_email(email).ok())
        .ok_or(LoginError::EmailNotVerified)?;
    let domains = state.org_sso.domains_of(org);
    let listed = email
        .rsplit_once('@')
        .is_some_and(|(_, domain)| domains.iter().any(|item| item == domain));
    if !listed {
        return Err(LoginError::DomainNotAllowed);
    }
    let input = Admit {
        org,
        identity: store::identity_provider(org),
        subject: claims.subject,
        email,
        name: display_name(claims.name),
        domains,
        jit: of(state, org)
            .allows(Feature::OrgSso)
            .then(|| provider.jit_role.org_role()),
        max_seats: state.entitlements.resolve(org, now).limits.max_seats,
        signup: state.config.auth.signup,
        test: saved.test,
        now,
    };
    let admitted = state
        .meta
        .write(move |conn| Box::pin(async move { admit(conn, &input).await }))
        .await;
    let admitted = match admitted {
        Ok(admitted) => admitted,
        Err(MetaError::Conflict(_)) => Admitted::LinkRefused,
        Err(error) => return Err(flow::internal(error)),
    };
    let (user_id, revoked, linked, joined, no_seat) = match admitted {
        Admitted::User {
            user_id,
            revoked,
            linked,
            joined,
            no_seat,
        } => (user_id, revoked, linked, joined, no_seat),
        Admitted::AccountDisabled => return Err(LoginError::AccountDisabled),
        Admitted::LinkRefused => return Err(LoginError::LinkRefused),
        Admitted::SignupClosed => return Err(LoginError::SignupClosed),
        Admitted::SeatLimit => return Err(LoginError::SeatLimit),
        Admitted::DomainNotAllowed => return Err(LoginError::DomainNotAllowed),
        Admitted::SsoUnavailable => return Err(LoginError::SsoUnavailable),
    };
    for hash in &revoked {
        state.pat_cache.evict(hash);
    }
    if linked {
        tracing::info!(target: "audit", user_id = %user_id, org_id = %org, "identity linked");
    }
    if let Some(role) = joined {
        tracing::info!(target: "audit", user_id = %user_id, org_id = %org, role = org_role_str(role), "org sso joined");
    }
    let redirect = if no_seat {
        tracing::info!(target: "audit", user_id = %user_id, org_id = %org, "org sso join skipped: seat limit");
        let slug = orgs::org_by_id(state.meta.reader(), org)
            .await
            .map_err(flow::internal)?
            .map_or_else(String::new, |row| row.slug);
        format!("/sso/no-seat?org={slug}")
    } else {
        saved.redirect
    };
    Ok(LoggedIn {
        user_id,
        redirect,
        sso_org: Some(org),
    })
}

/// What one org login brings to `admit`. All values are owned, so the write can take them.
struct Admit {
    org: OrgId,
    /// `org:<org>`, the identity's provider.
    identity: String,
    subject: String,
    email: String,
    name: Option<String>,
    /// The org's verified domains (lowercase).
    domains: Vec<String>,
    /// The role a joining user gets, when the org has `org_sso` (D-P-56).
    jit: Option<OrgRole>,
    max_seats: Option<u64>,
    signup: SignupPolicy,
    test: bool,
    now: i64,
}

/// What `admit` decided.
#[derive(Debug, PartialEq, Eq)]
enum Admitted {
    User {
        user_id: Uuid,
        /// Personal access token hashes the link revoked.
        revoked: Vec<String>,
        linked: bool,
        /// The role of a new membership.
        joined: Option<OrgRole>,
        /// JIT was refused for lack of a seat, and the person keeps their other orgs (D-P-56).
        no_seat: bool,
    },
    AccountDisabled,
    LinkRefused,
    SignupClosed,
    SeatLimit,
    DomainNotAllowed,
    /// A Test sign-in by someone who is not an owner, or an unknown person (D-P-57).
    SsoUnavailable,
}

/// Who the login is, decided by reads.
enum Target {
    /// The identity exists.
    Known(Uuid),
    /// The email belongs to an account that the identity links to.
    Link(UserRow),
    /// A new user.
    New,
}

/// Decides the login. All reads come first; a refusal writes nothing (D-P-55, D-P-56).
async fn admit(conn: &mut SqliteConnection, input: &Admit) -> Result<Admitted, MetaError> {
    let org = input.org;
    let now = input.now;
    let known = identities::identity_user_tx(conn, &input.identity, &input.subject).await?;
    let target = match known {
        Some(user_id) => {
            let user = users::user_by_id_tx(conn, user_id)
                .await?
                .ok_or(MetaError::Corrupt {
                    table: "identities",
                    column: "user_id",
                })?;
            if !listed(&input.domains, &user.email) {
                return Ok(Admitted::DomainNotAllowed);
            }
            if user.disabled_at.is_some() {
                return Ok(Admitted::AccountDisabled);
            }
            if input.test && members::org_role_tx(conn, org, user.id).await? != Some(OrgRole::Owner)
            {
                return Ok(Admitted::SsoUnavailable);
            }
            Target::Known(user.id)
        }
        None => match users::user_by_email_tx(conn, &input.email).await? {
            Some(user) => {
                if user.disabled_at.is_some() {
                    return Ok(Admitted::AccountDisabled);
                }
                if input.test {
                    if members::org_role_tx(conn, org, user.id).await? != Some(OrgRole::Owner) {
                        return Ok(Admitted::SsoUnavailable);
                    }
                    if user.is_instance_admin {
                        // D-P-57: the operator who owns the org tests without an identity, so
                        // enforcement becomes reachable. Only the last sign-in is recorded.
                        store::set_last_login(conn, org, now).await?;
                        return Ok(Admitted::User {
                            user_id: user.id,
                            revoked: Vec::new(),
                            linked: false,
                            joined: None,
                            no_seat: false,
                        });
                    }
                } else if user.is_instance_admin {
                    return Ok(Admitted::LinkRefused);
                }
                Target::Link(user)
            }
            None => {
                if input.test {
                    return Ok(Admitted::SsoUnavailable);
                }
                if matches!(input.signup, SignupPolicy::Disabled) {
                    return Ok(Admitted::SignupClosed);
                }
                Target::New
            }
        },
    };

    // Membership: a non-member joins with the role of a pending invitation or the org's JIT
    // role, within the seats. Test sign-in never joins (D-P-56, D-P-57).
    let user_id = match &target {
        Target::Known(id) => Some(*id),
        Target::Link(user) => Some(user.id),
        Target::New => None,
    };
    let existing = match user_id {
        Some(id) => members::org_role_tx(conn, org, id).await?,
        None => None,
    };
    let mut join = None;
    let mut no_seat = false;
    if !input.test
        && existing.is_none()
        && let Some(jit_role) = input.jit
    {
        let invite = invites::pending_invite_for_tx(conn, org, &input.email, now).await?;
        let role = invite.as_ref().map_or(jit_role, |invite| invite.role);
        let over = if role == OrgRole::Viewer {
            false
        } else if let Some(limit) = input.max_seats {
            let seats = members::seat_members_tx(conn, org).await?
                + if invite.is_some() {
                    0
                } else {
                    invites::seat_invites_tx(conn, org, now, None).await?
                };
            over_limit(Some(limit), seats, 1)
        } else {
            false
        };
        if over {
            if !invites::has_membership_or_invite_elsewhere_tx(
                conn,
                user_id,
                &input.email,
                org,
                now,
            )
            .await?
            {
                return Ok(Admitted::SeatLimit);
            }
            no_seat = true;
        } else {
            join = Some((role, invite.map(|invite| invite.id)));
        }
    }

    // Writes. A conflict (a concurrent first login) rolls the whole write back.
    let mut revoked = Vec::new();
    let mut linked = false;
    let user_id = match target {
        Target::Known(id) => id,
        Target::Link(user) => {
            identities::insert_identity(
                conn,
                user.id,
                &input.identity,
                &input.subject,
                &input.email,
                now,
            )
            .await?;
            if user.email_verified_at.is_none() {
                // D148's defence against a pre-registered account: the unverified password and
                // its sessions and tokens go when the provider links it.
                web_sessions::delete_user_sessions(conn, user.id, None).await?;
                revoked = personal_tokens::revoke_user_tokens(conn, user.id, now).await?;
                users::set_password_hash(conn, user.id, None, now).await?;
                users::set_email_verified(conn, user.id, now).await?;
            }
            linked = true;
            user.id
        }
        Target::New => {
            let user = users::insert_user(
                conn,
                NewUser {
                    email: input.email.clone(),
                    name: input.name.clone(),
                    password_hash: None,
                    email_verified_at: Some(now),
                    is_instance_admin: false,
                },
                now,
            )
            .await?;
            identities::insert_identity(
                conn,
                user.id,
                &input.identity,
                &input.subject,
                &input.email,
                now,
            )
            .await?;
            user.id
        }
    };
    let mut joined = None;
    if let Some((role, invite_id)) = join {
        match members::add_org_member(conn, org, user_id, role, now).await {
            Ok(()) | Err(MetaError::Conflict(_)) => {}
            Err(error) => return Err(error),
        }
        if let Some(invite_id) = invite_id {
            invites::mark_accepted(conn, invite_id, now).await?;
        }
        joined = Some(role);
    }
    store::set_last_login(conn, org, now).await?;
    Ok(Admitted::User {
        user_id,
        revoked,
        linked,
        joined,
        no_seat,
    })
}

fn listed(domains: &[String], email: &str) -> bool {
    email
        .rsplit_once('@')
        .is_some_and(|(_, domain)| domains.iter().any(|item| item == domain))
}

#[cfg(test)]
mod tests {
    use axum::http::{Method, StatusCode, header};
    use uuid::Uuid;

    use super::*;
    use crate::api::account::SignupPolicy;
    use crate::auth::oidc::state_cookie::{self, OAuthState};
    use crate::auth::secret::new_opaque_token;
    use crate::meta::invites::{self, INVITE_TTL_NS};
    use crate::meta::members;
    use crate::meta::users::{NewUser, insert_user, set_disabled};
    use crate::test_support::mock_idp::MockIdp;
    use crate::test_support::oauth::{
        SESSION_NAME, begin_at, finish, grant, location, me, one_cookie,
    };
    use crate::test_support::{
        TestApp, TestResponse, assert_logs_clean, configure_org_sso, make_instance_admin,
        org_sso_app, org_sso_app_with, reload_org_sso, second_org, session_cookie_via,
    };

    const AUTHORIZE: &str = "/api/auth/sso/authorize?domain=acme.test&redirect=%2Facme";
    const CALLBACK_SSO: &str = "/api/auth/callback/sso";

    /// One login through the org's provider: authorize, the mock's grant, the callback.
    async fn sign_in(
        app: &TestApp,
        idp: &MockIdp,
        cookie: Option<&str>,
        subject: &str,
        email: &str,
        verified: Option<bool>,
    ) -> TestResponse {
        let pending = begin_at(app, AUTHORIZE, cookie).await;
        let code = idp.issue_code(grant(subject, &pending, email, verified));
        finish(
            app,
            PROVIDER,
            &pending,
            &format!("code={code}&state={}", pending.state),
        )
        .await
    }

    async fn count(app: &TestApp, sql: &'static str) -> i64 {
        sqlx::query_scalar(sql)
            .fetch_one(app.state.meta.reader())
            .await
            .unwrap()
    }

    async fn count_with(app: &TestApp, sql: &'static str, bind: &str) -> i64 {
        sqlx::query_scalar(sql)
            .bind(bind.to_owned())
            .fetch_one(app.state.meta.reader())
            .await
            .unwrap()
    }

    async fn exec(app: &TestApp, sql: &'static str) {
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    sqlx::query(sql).execute(&mut *conn).await?;
                    Ok(())
                })
            })
            .await
            .unwrap();
    }

    async fn reload(app: &TestApp) {
        reload_org_sso(app).await;
    }

    async fn add_user(app: &TestApp, email: &str, verified: bool, admin: bool) -> Uuid {
        let now = app.state.clock.now_ns();
        let email = email.to_owned();
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    let user = insert_user(
                        conn,
                        NewUser {
                            email,
                            name: None,
                            password_hash: Some("unused-hash".to_owned()),
                            email_verified_at: verified.then_some(now),
                            is_instance_admin: admin,
                        },
                        now,
                    )
                    .await?;
                    Ok(user.id)
                })
            })
            .await
            .unwrap()
    }

    async fn me_status(app: &TestApp, cookie: &str) -> StatusCode {
        app.request(Method::GET, "/api/auth/me")
            .header(header::COOKIE.as_str(), cookie)
            .send()
            .await
            .status
    }

    async fn token_status(app: &TestApp, token: &str) -> StatusCode {
        app.request(Method::GET, "/api/auth/me")
            .header(header::AUTHORIZATION.as_str(), &format!("Bearer {token}"))
            .send()
            .await
            .status
    }

    #[tokio::test]
    async fn a_new_domain_user_joins_with_the_jit_role() {
        let idp = MockIdp::start().await;
        let (app, seeded, _owner) = org_sso_app().await;
        configure_org_sso(&app, seeded.org_id, &idp, "acme.test", true, false).await;
        let org = seeded.org_id.to_string();

        let response = sign_in(&app, &idp, None, "ada-sub", "ada@acme.test", None).await;
        assert_eq!(response.status, StatusCode::FOUND);
        assert_eq!(location(&response), "/acme");
        let session = one_cookie(&response, SESSION_NAME);
        assert_eq!(me(&app, &session).await.email, "ada@acme.test");
        assert_eq!(
            count_with(
                &app,
                "SELECT COUNT(*) FROM org_members m JOIN users u ON u.id = m.user_id \
                 WHERE u.email = 'ada@acme.test' AND m.role = 'viewer' AND m.org_id = ?",
                &org,
            )
            .await,
            1
        );
        assert_eq!(
            count(
                &app,
                "SELECT COUNT(*) FROM identities WHERE provider LIKE 'org:%' AND subject = 'ada-sub'",
            )
            .await,
            1
        );
        assert_eq!(
            count_with(
                &app,
                "SELECT COUNT(*) FROM web_sessions WHERE sso_org_id = ?",
                &org
            )
            .await,
            1
        );
        let row = store::provider(app.state.meta.reader(), seeded.org_id)
            .await
            .unwrap()
            .expect("provider");
        assert!(row.last_login_at.is_some());
    }

    #[tokio::test]
    async fn email_verified_false_is_refused() {
        let idp = MockIdp::start().await;
        let (app, seeded, _owner) = org_sso_app().await;
        configure_org_sso(&app, seeded.org_id, &idp, "acme.test", true, false).await;
        let response = sign_in(&app, &idp, None, "ada-sub", "ada@acme.test", Some(false)).await;
        assert_eq!(location(&response), "/login?error=email_not_verified");
        assert_eq!(
            count(
                &app,
                "SELECT COUNT(*) FROM users WHERE email = 'ada@acme.test'"
            )
            .await,
            0
        );
    }

    #[tokio::test]
    async fn a_pending_invitation_sets_the_role() {
        let idp = MockIdp::start().await;
        let (app, seeded, _owner) = org_sso_app().await;
        configure_org_sso(&app, seeded.org_id, &idp, "acme.test", true, false).await;
        let org = seeded.org_id;
        let inviter = seeded.user_id;
        let token = new_opaque_token().expect("token");
        let now = app.state.clock.now_ns();
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    invites::replace_invite(
                        conn,
                        org,
                        "ada@acme.test",
                        OrgRole::Member,
                        &token.hash,
                        inviter,
                        now,
                        now + INVITE_TTL_NS,
                    )
                    .await?;
                    Ok(())
                })
            })
            .await
            .unwrap();

        let response = sign_in(&app, &idp, None, "ada-sub", "ada@acme.test", None).await;
        assert_eq!(location(&response), "/acme");
        assert_eq!(
            count(
                &app,
                "SELECT COUNT(*) FROM org_members m JOIN users u ON u.id = m.user_id \
                 WHERE u.email = 'ada@acme.test' AND m.role = 'member'",
            )
            .await,
            1
        );
        assert_eq!(
            count(
                &app,
                "SELECT COUNT(*) FROM invites WHERE email = 'ada@acme.test' AND accepted_at IS NOT NULL",
            )
            .await,
            1
        );
    }

    #[tokio::test]
    async fn seats_refuse_a_newcomer_and_keep_other_orgs() {
        let idp = MockIdp::start().await;
        let (app, seeded, _owner) = org_sso_app_with(|config| {
            config.entitlements.defaults.max_seats = Some(1);
        })
        .await;
        configure_org_sso(&app, seeded.org_id, &idp, "acme.test", true, false).await;
        exec(&app, "UPDATE org_sso_providers SET jit_role = 'member'").await;
        reload(&app).await;

        // Nobody else in the org: a new person is refused with no user row.
        let refused = sign_in(&app, &idp, None, "ada-sub", "ada@acme.test", None).await;
        assert_eq!(location(&refused), "/login?error=seat_limit");
        assert_eq!(
            count(
                &app,
                "SELECT COUNT(*) FROM users WHERE email = 'ada@acme.test'"
            )
            .await,
            0
        );

        // Bob belongs to another org: he signs in without a seat.
        let (beta, _beta_owner) = second_org(&app, "beta", "b@beta.test").await;
        let bob = add_user(&app, "bob@acme.test", true, false).await;
        let now = app.state.clock.now_ns();
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    members::add_org_member(conn, beta, bob, OrgRole::Member, now).await?;
                    Ok(())
                })
            })
            .await
            .unwrap();
        let no_seat = sign_in(&app, &idp, None, "bob-sub", "bob@acme.test", None).await;
        assert_eq!(location(&no_seat), "/sso/no-seat?org=acme");
        assert!(!one_cookie(&no_seat, SESSION_NAME).is_empty());
        assert_eq!(
            count(
                &app,
                "SELECT COUNT(*) FROM org_members m JOIN users u ON u.id = m.user_id \
                 WHERE u.email = 'bob@acme.test' AND m.org_id = (SELECT id FROM orgs WHERE slug = 'acme')",
            )
            .await,
            0
        );
        assert_eq!(
            count(
                &app,
                "SELECT COUNT(*) FROM identities WHERE subject = 'bob-sub'"
            )
            .await,
            1
        );

        // Cara has an invitation to another org: the same redirect, her user row exists.
        let token = new_opaque_token().expect("token");
        let inviter = seeded.user_id;
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    invites::replace_invite(
                        conn,
                        beta,
                        "cara@acme.test",
                        OrgRole::Member,
                        &token.hash,
                        inviter,
                        now,
                        now + INVITE_TTL_NS,
                    )
                    .await?;
                    Ok(())
                })
            })
            .await
            .unwrap();
        let cara = sign_in(&app, &idp, None, "cara-sub", "cara@acme.test", None).await;
        assert_eq!(location(&cara), "/sso/no-seat?org=acme");
        assert_eq!(
            count(
                &app,
                "SELECT COUNT(*) FROM users WHERE email = 'cara@acme.test'"
            )
            .await,
            1
        );
        assert_eq!(
            count(
                &app,
                "SELECT COUNT(*) FROM invites WHERE email = 'cara@acme.test' AND accepted_at IS NULL",
            )
            .await,
            1
        );
    }

    #[tokio::test]
    async fn a_viewer_join_uses_no_seat() {
        let idp = MockIdp::start().await;
        let (app, seeded, _owner) = org_sso_app_with(|config| {
            config.entitlements.defaults.max_seats = Some(1);
        })
        .await;
        configure_org_sso(&app, seeded.org_id, &idp, "acme.test", true, false).await;
        let response = sign_in(&app, &idp, None, "ada-sub", "ada@acme.test", None).await;
        assert_eq!(location(&response), "/acme");
    }

    #[tokio::test]
    async fn another_domain_is_refused() {
        let idp = MockIdp::start().await;
        let (app, seeded, _owner) = org_sso_app().await;
        configure_org_sso(&app, seeded.org_id, &idp, "acme.test", true, false).await;
        let response = sign_in(&app, &idp, None, "eve-sub", "eve@other.test", None).await;
        assert_eq!(location(&response), "/login?error=domain_not_allowed");
        assert_eq!(
            count(
                &app,
                "SELECT COUNT(*) FROM users WHERE email = 'eve@other.test'"
            )
            .await,
            0
        );
    }

    #[tokio::test]
    async fn linking_revokes_an_unverified_account_and_refuses_admins() {
        let idp = MockIdp::start().await;
        let (app, seeded, _owner) = org_sso_app().await;
        configure_org_sso(&app, seeded.org_id, &idp, "acme.test", true, false).await;

        let dan = add_user(&app, "dan@acme.test", false, false).await;
        let dan_session = session_cookie_via(&app, dan, None).await;
        let dan_token = app.personal_token(dan, true).await;
        assert_eq!(me_status(&app, &dan_session).await, StatusCode::OK);
        let linked = sign_in(&app, &idp, None, "dan-sub", "dan@acme.test", None).await;
        assert_eq!(location(&linked), "/acme");
        assert_eq!(
            me_status(&app, &dan_session).await,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            token_status(&app, &dan_token).await,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            count(
                &app,
                "SELECT COUNT(*) FROM users WHERE email = 'dan@acme.test' \
                 AND password_hash IS NULL AND email_verified_at IS NOT NULL",
            )
            .await,
            1
        );

        let carl = add_user(&app, "carl@acme.test", true, false).await;
        let carl_session = session_cookie_via(&app, carl, None).await;
        let carl_login = sign_in(&app, &idp, None, "carl-sub", "carl@acme.test", None).await;
        assert_eq!(location(&carl_login), "/acme");
        assert_eq!(me_status(&app, &carl_session).await, StatusCode::OK);

        add_user(&app, "root@acme.test", true, true).await;
        let root = sign_in(&app, &idp, None, "root-sub", "root@acme.test", None).await;
        assert_eq!(location(&root), "/login?error=link_refused");

        let disabled = add_user(&app, "dis@acme.test", true, false).await;
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    set_disabled(conn, disabled, Some(1), 1).await?;
                    Ok(())
                })
            })
            .await
            .unwrap();
        let blocked = sign_in(&app, &idp, None, "dis-sub", "dis@acme.test", None).await;
        assert_eq!(location(&blocked), "/login?error=account_disabled");
    }

    #[tokio::test]
    async fn a_known_identity_signs_in_again_and_loses_a_removed_domain() {
        let idp = MockIdp::start().await;
        let (app, seeded, _owner) = org_sso_app().await;
        configure_org_sso(&app, seeded.org_id, &idp, "acme.test", true, false).await;
        assert_eq!(
            location(&sign_in(&app, &idp, None, "ada-sub", "ada@acme.test", None).await),
            "/acme"
        );
        assert_eq!(
            location(&sign_in(&app, &idp, None, "ada-sub", "ada@acme.test", None).await),
            "/acme"
        );
        assert_eq!(
            count(
                &app,
                "SELECT COUNT(*) FROM org_members m JOIN users u ON u.id = m.user_id \
                 WHERE u.email = 'ada@acme.test'",
            )
            .await,
            1
        );

        // The callback of a login begun before the domain was removed.
        let pending = begin_at(&app, AUTHORIZE, None).await;
        exec(&app, "UPDATE org_sso_domains SET verified_at = NULL").await;
        reload(&app).await;
        let code = idp.issue_code(grant("ada-sub", &pending, "ada@acme.test", None));
        let response = finish(
            &app,
            PROVIDER,
            &pending,
            &format!("code={code}&state={}", pending.state),
        )
        .await;
        assert_eq!(location(&response), "/login?error=domain_not_allowed");
    }

    #[tokio::test]
    async fn signup_policy_and_domains_gate_new_users() {
        let idp = MockIdp::start().await;
        let (app, seeded, _owner) = org_sso_app_with(|config| {
            config.auth.signup = SignupPolicy::Disabled;
        })
        .await;
        configure_org_sso(&app, seeded.org_id, &idp, "acme.test", true, false).await;
        assert_eq!(
            location(&sign_in(&app, &idp, None, "ada-sub", "ada@acme.test", None).await),
            "/login?error=signup_closed"
        );

        let (app, seeded, _owner) = org_sso_app_with(|config| {
            config.auth.signup = SignupPolicy::Open;
            config.auth.signup_domains = vec!["x.test".to_owned()];
        })
        .await;
        configure_org_sso(&app, seeded.org_id, &idp, "acme.test", true, false).await;
        assert_eq!(
            location(&sign_in(&app, &idp, None, "ada-sub", "ada@acme.test", None).await),
            "/acme"
        );
    }

    #[tokio::test]
    async fn a_lapse_signs_in_without_joining() {
        let idp = MockIdp::start().await;
        let app = TestApp::with_config(|config| {
            config.security.allow_private_network_egress = true;
        })
        .await;
        let seeded = app.seed().await;
        configure_org_sso(&app, seeded.org_id, &idp, "acme.test", true, false).await;
        let response = sign_in(&app, &idp, None, "ada-sub", "ada@acme.test", None).await;
        assert_eq!(location(&response), "/acme");
        assert_eq!(
            count(
                &app,
                "SELECT COUNT(*) FROM org_members m JOIN users u ON u.id = m.user_id \
                 WHERE u.email = 'ada@acme.test'",
            )
            .await,
            0
        );
    }

    #[tokio::test]
    async fn authorize_without_a_route_is_sso_unavailable_and_rate_limited() {
        let idp = MockIdp::start().await;
        let (app, seeded, owner) = org_sso_app().await;
        configure_org_sso(&app, seeded.org_id, &idp, "acme.test", true, false).await;
        let admin = app
            .add_member(&seeded, "admin@acme.test", OrgRole::Admin)
            .await;

        let unknown = app
            .request(Method::GET, "/api/auth/sso/authorize?domain=nope.test")
            .send()
            .await;
        assert_eq!(location(&unknown), "/login?error=sso_unavailable");
        assert!(unknown.header(header::SET_COOKIE.as_str()).is_none());

        exec(&app, "UPDATE org_sso_domains SET verified_at = NULL").await;
        reload(&app).await;
        let pending = app.request(Method::GET, AUTHORIZE).send().await;
        assert_eq!(location(&pending), "/login?error=sso_unavailable");

        exec(&app, "UPDATE org_sso_domains SET verified_at = 1").await;
        exec(&app, "UPDATE org_sso_providers SET enabled = 0").await;
        reload(&app).await;
        assert_eq!(
            location(&app.request(Method::GET, AUTHORIZE).send().await),
            "/login?error=sso_unavailable"
        );
        let by_admin = app
            .request(Method::GET, AUTHORIZE)
            .header(header::COOKIE.as_str(), &admin.cookie)
            .send()
            .await;
        assert_eq!(location(&by_admin), "/login?error=sso_unavailable");
        let _ = owner;

        let limited_app = TestApp::new().await;
        for _ in 0..300 {
            let response = limited_app
                .request(Method::GET, "/api/auth/sso/authorize?domain=nope.test")
                .send()
                .await;
            assert_eq!(location(&response), "/login?error=sso_unavailable");
        }
        let over = limited_app
            .request(Method::GET, "/api/auth/sso/authorize?domain=nope.test")
            .send()
            .await;
        assert_eq!(over.status, StatusCode::TOO_MANY_REQUESTS);
    }

    #[tokio::test]
    async fn an_owner_tests_a_disabled_provider_and_nobody_else_does() {
        let idp = MockIdp::start().await;

        let (app, seeded, owner) = org_sso_app().await;
        configure_org_sso(&app, seeded.org_id, &idp, "acme.test", false, false).await;
        let tested = sign_in(
            &app,
            &idp,
            Some(&owner.cookie),
            "owner-sub",
            "owner@acme.test",
            None,
        )
        .await;
        assert_eq!(location(&tested), "/acme");
        let session = one_cookie(&tested, SESSION_NAME);
        assert_eq!(me(&app, &session).await.email, "owner@acme.test");
        assert_eq!(
            count_with(
                &app,
                "SELECT COUNT(*) FROM web_sessions WHERE sso_org_id = ?",
                &seeded.org_id.to_string(),
            )
            .await,
            1
        );

        let (app, seeded, owner) = org_sso_app().await;
        configure_org_sso(&app, seeded.org_id, &idp, "acme.test", false, false).await;
        let member = app
            .add_member(&seeded, "ada@acme.test", OrgRole::Member)
            .await;
        let refused = app
            .request(Method::GET, AUTHORIZE)
            .header(header::COOKIE.as_str(), &member.cookie)
            .send()
            .await;
        assert_eq!(location(&refused), "/login?error=sso_unavailable");
        let by_owner_for_a_stranger = sign_in(
            &app,
            &idp,
            Some(&owner.cookie),
            "new-sub",
            "new@acme.test",
            None,
        )
        .await;
        assert_eq!(
            location(&by_owner_for_a_stranger),
            "/login?error=sso_unavailable"
        );
        assert_eq!(
            count(
                &app,
                "SELECT COUNT(*) FROM users WHERE email = 'new@acme.test'"
            )
            .await,
            0
        );

        let (app, seeded, _owner) = org_sso_app().await;
        configure_org_sso(&app, seeded.org_id, &idp, "acme.test", true, false).await;
        let pending = begin_at(&app, AUTHORIZE, None).await;
        exec(&app, "UPDATE org_sso_providers SET enabled = 0").await;
        reload(&app).await;
        let code = idp.issue_code(grant("ada-sub", &pending, "ada@acme.test", None));
        let response = finish(
            &app,
            PROVIDER,
            &pending,
            &format!("code={code}&state={}", pending.state),
        )
        .await;
        assert_eq!(location(&response), "/login?error=sso_unavailable");
    }

    #[tokio::test]
    async fn an_instance_admin_owner_can_test_then_enforce() {
        let idp = MockIdp::start().await;
        let (app, seeded, _owner) = org_sso_app().await;
        let root = app
            .add_member(&seeded, "root@acme.test", OrgRole::Owner)
            .await;
        make_instance_admin(&app, root.id).await;
        configure_org_sso(&app, seeded.org_id, &idp, "acme.test", false, false).await;
        exec(&app, "UPDATE org_sso_providers SET last_login_at = NULL").await;
        reload(&app).await;

        let tested = sign_in(
            &app,
            &idp,
            Some(&root.cookie),
            "root-sub",
            "root@acme.test",
            None,
        )
        .await;
        assert_eq!(location(&tested), "/acme");
        assert_eq!(
            count(
                &app,
                "SELECT COUNT(*) FROM identities WHERE subject = 'root-sub'"
            )
            .await,
            0
        );
        let session = one_cookie(&tested, SESSION_NAME);
        let enabled = app
            .request(Method::PATCH, "/api/v1/orgs/acme/sso")
            .header(header::COOKIE.as_str(), &session)
            .json(&serde_json::json!({ "enabled": true }))
            .send()
            .await;
        assert_eq!(enabled.status, StatusCode::OK);
        let enforced = app
            .request(Method::PATCH, "/api/v1/orgs/acme/sso")
            .header(header::COOKIE.as_str(), &session)
            .json(&serde_json::json!({ "enforced": true }))
            .send()
            .await;
        assert_eq!(enforced.status, StatusCode::OK);

        // The same person, a normal sign-in: an instance admin is never linked.
        let normal = sign_in(&app, &idp, None, "root-sub-2", "root@acme.test", None).await;
        assert_eq!(location(&normal), "/login?error=link_refused");
    }

    #[tokio::test]
    async fn state_is_bound_to_the_org_flow() {
        let idp = MockIdp::start().await;
        let (app, seeded, _owner) = org_sso_app().await;
        configure_org_sso(&app, seeded.org_id, &idp, "acme.test", true, false).await;
        let now = app.state.clock.now_ns();
        let instance = state_cookie::seal(
            &app.state.oauth_key,
            &OAuthState {
                provider: "oidc:mock".to_owned(),
                state: "csrf".to_owned(),
                nonce: "nonce".to_owned(),
                pkce_verifier: "verifier".to_owned(),
                redirect: "/".to_owned(),
                setup_token: None,
                created_ns: now,
                org_id: None,
                test: false,
            },
            false,
        )
        .expect("cookie");
        let pair = instance
            .to_str()
            .expect("text")
            .split(';')
            .next()
            .expect("pair")
            .to_owned();
        let response = app
            .request(Method::GET, &format!("{CALLBACK_SSO}?state=csrf&code=abc"))
            .header(header::COOKIE.as_str(), &pair)
            .send()
            .await;
        assert_eq!(location(&response), "/login?error=invalid_state");

        let pending = begin_at(&app, AUTHORIZE, None).await;
        let response = app
            .request(
                Method::GET,
                &format!(
                    "/api/auth/callback/oidc:mock?state={}&code=abc",
                    pending.state
                ),
            )
            .header(header::COOKIE.as_str(), &pending.cookie)
            .send()
            .await;
        assert_eq!(location(&response), "/login?error=invalid_state");
    }

    #[tokio::test]
    async fn logs_carry_no_email_or_domain() {
        let idp = MockIdp::start().await;
        let (app, seeded, _owner) = org_sso_app().await;
        configure_org_sso(&app, seeded.org_id, &idp, "acme.test", true, false).await;
        let response = assert_logs_clean(&["ada@acme.test", "acme.test"], async {
            sign_in(&app, &idp, None, "ada-sub", "ada@acme.test", None).await
        })
        .await;
        assert_eq!(location(&response), "/acme");
    }
}
