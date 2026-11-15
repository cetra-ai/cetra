//! `POST /api/internal/v1/users/{revoke,disable,delete}`: the control plane acts on a user by
//! its OIDC subject at `entitlements.control_plane_provider` (D-P-39), or by email for a user
//! with no identity there (D-S-9).

use axum::extract::{Extension, State};
use axum::http::StatusCode;
use axum::routing::post;
use uuid::Uuid;

use crate::api::common::ProblemCode;
use crate::api::internal::{InternalUserDeleted, InternalUserRef};
use crate::auth::principal::Principal;
use crate::http::AppState;
use crate::http::access::{Access, RouteDef};
use crate::http::error::{self, ApiError, field_error};
use crate::meta::MetaError;
use crate::meta::identities::{identity_user, user_identities};
use crate::meta::members::count_owners;
use crate::meta::personal_tokens::revoke_user_tokens;
use crate::meta::users::{self, normalize_email, user_by_email, user_by_id};
use crate::meta::web_sessions::delete_user_sessions;
use llmobs_core::ids::OrgId;

/// The internal user routes. The control plane holds a `control_plane` token with scope `users`.
pub(crate) fn routes() -> Vec<RouteDef> {
    vec![
        RouteDef {
            method: axum::http::Method::POST,
            path: "/api/internal/v1/users/revoke",
            access: Access::ControlPlane { scope: "users" },
            documented: true,
            handler: post(revoke),
        },
        RouteDef {
            method: axum::http::Method::POST,
            path: "/api/internal/v1/users/disable",
            access: Access::ControlPlane { scope: "users" },
            documented: true,
            handler: post(disable),
        },
        RouteDef {
            method: axum::http::Method::POST,
            path: "/api/internal/v1/users/delete",
            access: Access::ControlPlane { scope: "users" },
            documented: true,
            handler: post(delete),
        },
    ]
}

/// How the control plane's call found the user (logged, D-S-9).
#[derive(Debug, Clone, Copy)]
enum Matched {
    Subject,
    Email,
}

impl Matched {
    fn as_str(self) -> &'static str {
        match self {
            Self::Subject => "subject",
            Self::Email => "email",
        }
    }
}

/// The user the call names, or `None` when there is none. The subject is looked up first,
/// at `entitlements.control_plane_provider`. Without such an identity, the normalized email
/// names a user only when that user has no identity at that provider: one linked under
/// another subject is another control-plane account (D-S-9). The email is validated only
/// when it is used, as `memberships/lookup` does. The control plane never manages an
/// instance admin, so one is refused.
async fn managed_user(
    state: &AppState,
    body: &InternalUserRef,
) -> Result<Option<(Uuid, Matched)>, ApiError> {
    let reader = state.meta.reader();
    let provider = &state.config.entitlements.control_plane_provider;
    let found = match identity_user(reader, provider, &body.subject).await? {
        Some(user_id) => Some((user_id, Matched::Subject)),
        None => {
            let email =
                normalize_email(&body.email).map_err(|_| field_error("email", "invalid email"))?;
            match user_by_email(reader, &email).await? {
                Some(user)
                    if !user_identities(reader, user.id)
                        .await?
                        .iter()
                        .any(|linked| linked == provider) =>
                {
                    Some((user.id, Matched::Email))
                }
                _ => None,
            }
        }
    };
    let Some((user_id, matched)) = found else {
        return Ok(None);
    };
    let Some(user) = user_by_id(reader, user_id).await? else {
        return Ok(None);
    };
    if user.is_instance_admin {
        return Err(ApiError::Forbidden {
            code: ProblemCode::Forbidden,
        });
    }
    Ok(Some((user_id, matched)))
}

/// Ends the user's web sessions and personal tokens. Returns the revoked token hashes.
async fn revoke_access(
    state: &AppState,
    user: Uuid,
    disable: bool,
) -> Result<Vec<String>, ApiError> {
    let now = state.clock.now_ns();
    let hashes = state
        .meta
        .write(move |conn| {
            Box::pin(async move {
                delete_user_sessions(conn, user, None).await?;
                let hashes = revoke_user_tokens(conn, user, now).await?;
                if disable {
                    // An already disabled user keeps the instant it was first disabled.
                    let disabled_at: Option<i64> =
                        sqlx::query_scalar("SELECT disabled_at FROM users WHERE id = ?")
                            .bind(user.to_string())
                            .fetch_one(&mut *conn)
                            .await
                            .map_err(MetaError::from)?;
                    if disabled_at.is_none() {
                        users::set_disabled(conn, user, Some(now), now).await?;
                    }
                }
                Ok(hashes)
            })
        })
        .await?;
    Ok(hashes)
}

/// `POST /api/internal/v1/users/revoke`: ends every session and personal token of the user.
async fn revoke(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    error::Json(body): error::Json<InternalUserRef>,
) -> Result<StatusCode, ApiError> {
    let Principal::ControlPlane { token_id, .. } = principal else {
        return Err(ApiError::Unauthorized);
    };
    let (user, matched) = managed_user(&state, &body)
        .await?
        .ok_or(ApiError::NotFound)?;
    let hashes = revoke_access(&state, user, false).await?;
    evict(&state, &hashes);
    tracing::info!(
        target: "audit",
        user_id = %user,
        actor = "control_plane",
        token_id = %token_id,
        matched = matched.as_str(),
        "user sessions revoked"
    );
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /api/internal/v1/users/disable`: revokes access and sets `disabled_at`. Unlike the
/// instance admin's disable, the last owner of an org may be disabled: the org stays.
async fn disable(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    error::Json(body): error::Json<InternalUserRef>,
) -> Result<StatusCode, ApiError> {
    let Principal::ControlPlane { token_id, .. } = principal else {
        return Err(ApiError::Unauthorized);
    };
    let (user, matched) = managed_user(&state, &body)
        .await?
        .ok_or(ApiError::NotFound)?;
    let hashes = revoke_access(&state, user, true).await?;
    evict(&state, &hashes);
    tracing::info!(
        target: "audit",
        user_id = %user,
        actor = "control_plane",
        token_id = %token_id,
        matched = matched.as_str(),
        "user disabled"
    );
    Ok(StatusCode::NO_CONTENT)
}

/// What the delete transaction did.
struct Deleted {
    /// Orgs the user was the only owner of. They stay, without an owner.
    sole_owner_of: Vec<Uuid>,
    /// The user's personal token hashes, for the cache.
    hashes: Vec<String>,
}

/// `POST /api/internal/v1/users/delete`. An unknown subject answers 200 with no orgs, so a
/// retry after a lost response succeeds.
async fn delete(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    error::Json(body): error::Json<InternalUserRef>,
) -> Result<error::Json<InternalUserDeleted>, ApiError> {
    let Principal::ControlPlane { token_id, .. } = principal else {
        return Err(ApiError::Unauthorized);
    };
    let Some((user, matched)) = managed_user(&state, &body).await? else {
        return Ok(error::Json(InternalUserDeleted {
            sole_owner_of: Vec::new(),
        }));
    };
    let deleted = state
        .meta
        .write(move |conn| {
            Box::pin(async move {
                let owned: Vec<String> = sqlx::query_scalar(
                    "SELECT org_id FROM org_members WHERE user_id = ? AND role = 'owner' \
                     ORDER BY org_id",
                )
                .bind(user.to_string())
                .fetch_all(&mut *conn)
                .await
                .map_err(MetaError::from)?;
                let mut sole_owner_of = Vec::new();
                for org in owned {
                    let org_id = Uuid::parse_str(&org).map_err(|_| MetaError::Corrupt {
                        table: "org_members",
                        column: "org_id",
                    })?;
                    let org_id = OrgId::from_uuid(org_id);
                    if count_owners(conn, org_id).await? == 1 {
                        sole_owner_of.push(org_id.as_uuid());
                    }
                }
                let hashes: Vec<String> =
                    sqlx::query_scalar("SELECT token_hash FROM personal_tokens WHERE user_id = ?")
                        .bind(user.to_string())
                        .fetch_all(&mut *conn)
                        .await
                        .map_err(MetaError::from)?;
                // Rows that name the user without cascading keep the row and lose the name.
                sqlx::query("UPDATE invites SET invited_by = NULL WHERE invited_by = ?")
                    .bind(user.to_string())
                    .execute(&mut *conn)
                    .await
                    .map_err(MetaError::from)?;
                sqlx::query("UPDATE api_keys SET created_by = NULL WHERE created_by = ?")
                    .bind(user.to_string())
                    .execute(&mut *conn)
                    .await
                    .map_err(MetaError::from)?;
                sqlx::query("DELETE FROM users WHERE id = ?")
                    .bind(user.to_string())
                    .execute(&mut *conn)
                    .await
                    .map_err(MetaError::from)?;
                Ok(Deleted {
                    sole_owner_of,
                    hashes,
                })
            })
        })
        .await?;
    evict(&state, &deleted.hashes);
    tracing::info!(
        target: "audit",
        user_id = %user,
        actor = "control_plane",
        token_id = %token_id,
        sole_owner_orgs = deleted.sole_owner_of.len(),
        matched = matched.as_str(),
        "user deleted"
    );
    Ok(error::Json(InternalUserDeleted {
        sole_owner_of: deleted.sole_owner_of,
    }))
}

fn evict(state: &AppState, hashes: &[String]) {
    for hash in hashes {
        state.pat_cache.evict(hash);
    }
}

#[cfg(test)]
mod tests {
    use axum::http::{Method, header};
    use serde_json::{Value, json};

    use super::*;
    use crate::auth::roles::OrgRole;
    use crate::auth::session::start_session;
    use crate::meta::api_keys::{NewApiKey, insert_api_key};
    use crate::meta::identities::insert_identity;
    use crate::meta::members::add_org_member;
    use crate::meta::orgs::insert_org;
    use crate::meta::projects::insert_project;
    use crate::meta::users::{NewUser, insert_user};
    use crate::test_support::{TestApp, TestResponse, capture_logs, make_instance_admin};

    const PROVIDER: &str = "oidc:cloud";

    async fn post_user(
        app: &TestApp,
        route: &str,
        token: &str,
        subject: &str,
        email: &str,
    ) -> TestResponse {
        app.request(Method::POST, &format!("/api/internal/v1/users/{route}"))
            .control_plane(token)
            .json(&json!({ "subject": subject, "email": email }))
            .send()
            .await
    }

    /// A user with an OIDC identity at the control plane's provider. Returns the id and a
    /// session cookie.
    struct Linked {
        id: Uuid,
        cookie: String,
    }

    async fn linked_user(app: &TestApp, email: &str, subject: &str, admin: bool) -> Linked {
        let email = email.to_owned();
        let subject = subject.to_owned();
        let now = app.state.clock.now_ns();
        let id = app
            .state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    let user = insert_user(
                        conn,
                        NewUser {
                            email: email.clone(),
                            name: None,
                            password_hash: None,
                            email_verified_at: Some(now),
                            email_verified_via: Some(users::EmailVerifiedVia::ControlPlane),
                            is_instance_admin: admin,
                        },
                        now,
                    )
                    .await?;
                    insert_identity(conn, user.id, PROVIDER, &subject, &email, now).await?;
                    Ok(user.id)
                })
            })
            .await
            .unwrap();
        let header = start_session(
            &app.state,
            id,
            None,
            None,
            crate::auth::session::SessionOrigin::Password,
        )
        .await
        .unwrap();
        let cookie = header
            .to_str()
            .unwrap()
            .split_once(';')
            .unwrap()
            .0
            .to_owned();
        Linked { id, cookie }
    }

    /// Makes `user` an owner of a new org, returning the org id.
    async fn own_org(app: &TestApp, user: Uuid, name: &str, slug: &str) -> OrgId {
        let (name, slug) = (name.to_owned(), slug.to_owned());
        let now = app.state.clock.now_ns();
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    let org = insert_org(conn, OrgId::new_v7(), &name, &slug, now).await?;
                    add_org_member(conn, org.id, user, OrgRole::Owner, now).await?;
                    Ok(org.id)
                })
            })
            .await
            .unwrap()
    }

    async fn add_owner(app: &TestApp, org: OrgId, user: Uuid) {
        let now = app.state.clock.now_ns();
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move { add_org_member(conn, org, user, OrgRole::Owner, now).await })
            })
            .await
            .unwrap();
    }

    async fn session_me(app: &TestApp, cookie: &str) -> TestResponse {
        app.request(Method::GET, "/api/auth/me")
            .header(header::COOKIE.as_str(), cookie)
            .send()
            .await
    }

    async fn orgs_with(app: &TestApp, token: &str) -> TestResponse {
        app.request(Method::GET, "/api/v1/orgs")
            .header("authorization", &format!("Bearer {token}"))
            .send()
            .await
    }

    async fn user_row(app: &TestApp, id: Uuid) -> Option<users::UserRow> {
        user_by_id(app.state.meta.reader(), id).await.unwrap()
    }

    /// A user with no identity, as Q's `POST /api/internal/v1/orgs` creates one.
    async fn email_only_user(app: &TestApp, email: &str) -> Uuid {
        let email = email.to_owned();
        let now = app.state.clock.now_ns();
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    let user = insert_user(
                        conn,
                        NewUser {
                            email,
                            name: None,
                            password_hash: None,
                            email_verified_at: Some(now),
                            email_verified_via: Some(users::EmailVerifiedVia::MailToken),
                            is_instance_admin: false,
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

    #[tokio::test]
    async fn revoke_ends_sessions_and_tokens_and_keeps_the_user() {
        let app = TestApp::new().await;
        let user = linked_user(&app, "rev@example.com", "sub-rev", false).await;
        own_org(&app, user.id, "Rev", "rev").await;
        let pat = app.personal_token(user.id, false).await;
        assert_eq!(orgs_with(&app, &pat).await.status, 200);
        assert_eq!(session_me(&app, &user.cookie).await.status, 200);

        let token = "users";
        let revoked = post_user(&app, "revoke", token, "sub-rev", "rev@example.com").await;
        assert_eq!(revoked.status, 204);

        assert_eq!(session_me(&app, &user.cookie).await.status, 401);
        assert_eq!(orgs_with(&app, &pat).await.status, 401);
        let row = user_row(&app, user.id).await.unwrap();
        assert!(row.disabled_at.is_none());
    }

    #[tokio::test]
    async fn disable_revokes_and_sets_disabled_at_even_for_the_last_owner() {
        let app = TestApp::new().await;
        let user = linked_user(&app, "dis@example.com", "sub-dis", false).await;
        let org = own_org(&app, user.id, "Dis", "dis").await;
        let pat = app.personal_token(user.id, false).await;
        assert_eq!(orgs_with(&app, &pat).await.status, 200);

        let token = "users";
        let disabled = post_user(&app, "disable", token, "sub-dis", "dis@example.com").await;
        assert_eq!(disabled.status, 204);

        assert_eq!(session_me(&app, &user.cookie).await.status, 401);
        assert_eq!(orgs_with(&app, &pat).await.status, 401);
        let row = user_row(&app, user.id).await.unwrap();
        assert!(row.disabled_at.is_some());
        let owner_still_there: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM org_members WHERE org_id = ? AND user_id = ? AND role = 'owner'",
        )
        .bind(org.to_string())
        .bind(user.id.to_string())
        .fetch_one(app.state.meta.reader())
        .await
        .unwrap();
        assert_eq!(owner_still_there, 1);
    }

    #[tokio::test]
    async fn disabling_twice_keeps_the_first_disabled_at() {
        let app = TestApp::new().await;
        let user = linked_user(&app, "twice@example.com", "sub-twice", false).await;
        let token = "users";

        assert_eq!(
            post_user(&app, "disable", token, "sub-twice", "twice@example.com")
                .await
                .status,
            204
        );
        let first = user_row(&app, user.id).await.unwrap().disabled_at;
        assert!(first.is_some());

        app.state.clock.advance(std::time::Duration::from_secs(60));
        assert_eq!(
            post_user(&app, "disable", token, "sub-twice", "twice@example.com")
                .await
                .status,
            204
        );
        assert_eq!(user_row(&app, user.id).await.unwrap().disabled_at, first);
        assert_eq!(session_me(&app, &user.cookie).await.status, 401);
    }

    #[tokio::test]
    async fn delete_reports_sole_owner_orgs_and_clears_references() {
        let app = TestApp::new().await;
        let user = linked_user(&app, "del@example.com", "sub-del", false).await;
        let other = linked_user(&app, "other@example.com", "sub-other", false).await;
        let sole = own_org(&app, user.id, "Sole", "sole").await;
        let shared = own_org(&app, user.id, "Shared", "shared").await;
        add_owner(&app, shared, other.id).await;
        let pat = app.personal_token(user.id, false).await;
        assert_eq!(orgs_with(&app, &pat).await.status, 200);

        // An invite the user sent and an API key the user created must survive, unlinked.
        let invite_id = Uuid::now_v7().to_string();
        let invite_org = sole.to_string();
        let invited_by = user.id.to_string();
        let now = app.state.clock.now_ns();
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    sqlx::query(
                        "INSERT INTO invites (id, org_id, email, role, token_hash, invited_by, \
                         created_at, expires_at) VALUES (?, ?, ?, 'member', ?, ?, ?, ?)",
                    )
                    .bind(invite_id)
                    .bind(invite_org)
                    .bind("invitee@example.com")
                    .bind("invite-hash")
                    .bind(invited_by)
                    .bind(now)
                    .bind(now + 1_000_000_000)
                    .execute(&mut *conn)
                    .await
                    .map_err(MetaError::from)?;
                    let project = insert_project(conn, sole, "Keys", "keys", None, now).await?;
                    insert_api_key(
                        conn,
                        NewApiKey {
                            project_id: project.id,
                            kind: "secret",
                            scopes: vec![crate::auth::roles::KeyScope::Read],
                            name: Some("made by the user".to_owned()),
                            prefix: "sk-llmobs-keep".to_owned(),
                            hash: "key-hash-keep".to_owned(),
                            created_by: user.id,
                            expires_at: None,
                        },
                        now,
                    )
                    .await?;
                    Ok(())
                })
            })
            .await
            .unwrap();

        let token = "users";
        let deleted = post_user(&app, "delete", token, "sub-del", "del@example.com").await;
        assert_eq!(deleted.status, 200);
        let body: Value = deleted.json();
        assert_eq!(body, json!({ "sole_owner_of": [sole.to_string()] }));

        assert!(user_row(&app, user.id).await.is_none());
        assert_eq!(orgs_with(&app, &pat).await.status, 401);
        assert_eq!(session_me(&app, &user.cookie).await.status, 401);
        let rows: Vec<Option<String>> = sqlx::query_scalar(
            "SELECT invited_by FROM invites WHERE email = 'invitee@example.com'",
        )
        .fetch_all(app.state.meta.reader())
        .await
        .unwrap();
        assert_eq!(rows, vec![None]);
        let created: Vec<Option<String>> =
            sqlx::query_scalar("SELECT created_by FROM api_keys WHERE prefix = 'sk-llmobs-keep'")
                .fetch_all(app.state.meta.reader())
                .await
                .unwrap();
        assert_eq!(created, vec![None]);
        let broken = sqlx::query("PRAGMA foreign_key_check")
            .fetch_all(app.state.meta.reader())
            .await
            .unwrap();
        assert!(broken.is_empty(), "{} dangling references", broken.len());

        let again = post_user(&app, "delete", token, "sub-del", "del@example.com").await;
        assert_eq!(again.status, 200);
        assert_eq!(again.json::<Value>(), json!({ "sole_owner_of": [] }));
    }

    #[tokio::test]
    async fn delete_logs_no_identity_details() {
        let app = TestApp::new().await;
        let user = linked_user(&app, "quiet@example.com", "sub-quiet", false).await;
        let (response, logs) = capture_logs(async {
            post_user(&app, "delete", "users", "sub-quiet", "quiet@example.com").await
        })
        .await;
        assert_eq!(response.status, 200);
        assert!(logs.contains("user deleted"));
        assert!(logs.contains("control_plane"));
        assert!(logs.contains("matched=\"subject\""));
        assert!(!logs.contains("quiet@example.com"));
        assert!(!logs.contains("sub-quiet"));
        assert!(user_row(&app, user.id).await.is_none());
    }

    #[tokio::test]
    async fn instance_admins_are_refused() {
        let app = TestApp::new().await;
        linked_user(&app, "admin@example.com", "sub-admin", true).await;
        let token = "users";
        for route in ["revoke", "disable", "delete"] {
            let refused = post_user(&app, route, token, "sub-admin", "admin@example.com").await;
            assert_eq!(refused.status, 403, "{route}");
            assert_eq!(refused.problem().code, ProblemCode::Forbidden, "{route}");
        }
        let still = user_by_email_for(&app, "admin@example.com").await;
        assert!(still.disabled_at.is_none());
    }

    async fn user_by_email_for(app: &TestApp, email: &str) -> users::UserRow {
        user_by_email(app.state.meta.reader(), email)
            .await
            .unwrap()
            .unwrap()
    }

    #[tokio::test]
    async fn unknown_subjects() {
        let app = TestApp::new().await;
        let token = "users";
        assert_eq!(
            post_user(&app, "revoke", token, "nobody", "nobody@example.com")
                .await
                .status,
            404
        );
        assert_eq!(
            post_user(&app, "disable", token, "nobody", "nobody@example.com")
                .await
                .status,
            404
        );
        let deleted = post_user(&app, "delete", token, "nobody", "nobody@example.com").await;
        assert_eq!(deleted.status, 200);
        assert_eq!(deleted.json::<Value>(), json!({ "sole_owner_of": [] }));
    }

    #[tokio::test]
    async fn needs_the_users_scope() {
        let app = TestApp::new().await;
        let user = linked_user(&app, "scope@example.com", "sub-scope", false).await;
        for route in ["revoke", "disable", "delete"] {
            let usage = post_user(&app, route, "usage", "sub-scope", "scope@example.com").await;
            assert_eq!(usage.status, 403, "{route}");
        }
        let session = app
            .request(Method::POST, "/api/internal/v1/users/revoke")
            .header(header::COOKIE.as_str(), &user.cookie)
            .json(&json!({ "subject": "sub-scope", "email": "scope@example.com" }))
            .send()
            .await;
        assert_eq!(session.status, 403);
        let anonymous = app
            .request(Method::POST, "/api/internal/v1/users/revoke")
            .json(&json!({ "subject": "sub-scope", "email": "scope@example.com" }))
            .send()
            .await;
        assert_eq!(anonymous.status, 401);
        assert!(user_row(&app, user.id).await.is_some());
    }

    #[tokio::test]
    async fn only_the_control_plane_provider_is_matched() {
        let app = TestApp::new().await;
        let user = linked_user(&app, "prov@example.com", "sub-prov", false).await;
        let id = user.id;
        let now = app.state.clock.now_ns();
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    insert_identity(conn, id, "oidc:other", "sub-other", "prov@example.com", now)
                        .await
                })
            })
            .await
            .unwrap();
        let token = "users";
        let unknown = post_user(&app, "revoke", token, "sub-other", "prov@example.com").await;
        assert_eq!(unknown.status, 404);
        assert_eq!(session_me(&app, &user.cookie).await.status, 200);
    }

    #[tokio::test]
    async fn delete_falls_back_to_the_normalized_email() {
        let app = TestApp::new().await;
        let user = email_only_user(&app, "owner@example.com").await;
        let sole = own_org(&app, user, "Solo", "solo").await;
        let deleted = post_user(
            &app,
            "delete",
            "users",
            "sub-never-seen",
            "  Owner@Example.COM ",
        )
        .await;
        assert_eq!(deleted.status, 200);
        assert_eq!(
            deleted.json::<Value>(),
            json!({ "sole_owner_of": [sole.to_string()] })
        );
        assert!(user_row(&app, user).await.is_none());
    }

    #[tokio::test]
    async fn revoke_and_disable_fall_back_to_the_email() {
        let app = TestApp::new().await;
        let token = "users";
        let rev = email_only_user(&app, "rev-mail@example.com").await;
        own_org(&app, rev, "RevMail", "rev-mail").await;
        let rev_pat = app.personal_token(rev, false).await;
        assert_eq!(orgs_with(&app, &rev_pat).await.status, 200);
        let dis = email_only_user(&app, "dis-mail@example.com").await;
        own_org(&app, dis, "DisMail", "dis-mail").await;
        let dis_pat = app.personal_token(dis, false).await;
        assert_eq!(orgs_with(&app, &dis_pat).await.status, 200);

        let revoked = post_user(&app, "revoke", token, "sub-unknown", "rev-mail@example.com").await;
        assert_eq!(revoked.status, 204);
        assert_eq!(orgs_with(&app, &rev_pat).await.status, 401);
        assert!(user_row(&app, rev).await.unwrap().disabled_at.is_none());

        let disabled = post_user(
            &app,
            "disable",
            token,
            "sub-unknown",
            "dis-mail@example.com",
        )
        .await;
        assert_eq!(disabled.status, 204);
        assert_eq!(orgs_with(&app, &dis_pat).await.status, 401);
        assert!(user_row(&app, dis).await.unwrap().disabled_at.is_some());
    }

    #[tokio::test]
    async fn the_subject_wins_over_the_email() {
        let app = TestApp::new().await;
        let a = linked_user(&app, "a@example.com", "sub-a", false).await;
        let b = email_only_user(&app, "b@example.com").await;
        let deleted = post_user(&app, "delete", "users", "sub-a", "b@example.com").await;
        assert_eq!(deleted.status, 200);
        assert!(user_row(&app, a.id).await.is_none());
        assert!(user_row(&app, b).await.is_some());
    }

    #[tokio::test]
    async fn an_email_linked_under_another_subject_is_unknown() {
        let app = TestApp::new().await;
        let token = "users";
        let linked = linked_user(&app, "linked@example.com", "sub-real", false).await;
        assert_eq!(
            post_user(&app, "revoke", token, "sub-wrong", "linked@example.com")
                .await
                .status,
            404
        );
        assert_eq!(
            post_user(&app, "disable", token, "sub-wrong", "linked@example.com")
                .await
                .status,
            404
        );
        let deleted = post_user(&app, "delete", token, "sub-wrong", "linked@example.com").await;
        assert_eq!(deleted.status, 200);
        assert_eq!(deleted.json::<Value>(), json!({ "sole_owner_of": [] }));
        assert!(user_row(&app, linked.id).await.is_some());
        assert_eq!(session_me(&app, &linked.cookie).await.status, 200);
    }

    #[tokio::test]
    async fn an_email_linked_only_to_another_provider_is_found() {
        let app = TestApp::new().await;
        let user = email_only_user(&app, "other-prov@example.com").await;
        let now = app.state.clock.now_ns();
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    insert_identity(
                        conn,
                        user,
                        "oidc:other",
                        "sub-other-prov",
                        "other-prov@example.com",
                        now,
                    )
                    .await
                })
            })
            .await
            .unwrap();
        let revoked = post_user(
            &app,
            "revoke",
            "users",
            "sub-unknown",
            "other-prov@example.com",
        )
        .await;
        assert_eq!(revoked.status, 204);
    }

    #[tokio::test]
    async fn instance_admins_are_refused_by_email() {
        let app = TestApp::new().await;
        let admin = email_only_user(&app, "root@example.com").await;
        make_instance_admin(&app, admin).await;
        for route in ["revoke", "disable", "delete"] {
            let refused = post_user(&app, route, "users", "sub-unknown", "root@example.com").await;
            assert_eq!(refused.status, 403, "{route}");
            assert_eq!(refused.problem().code, ProblemCode::Forbidden, "{route}");
        }
        assert!(user_row(&app, admin).await.unwrap().disabled_at.is_none());
    }

    #[tokio::test]
    async fn an_invalid_email_is_refused_only_when_used() {
        let app = TestApp::new().await;
        let token = "users";
        for route in ["revoke", "disable", "delete"] {
            let refused = post_user(&app, route, token, "sub-unknown", "not-an-email").await;
            assert_eq!(refused.status, 400, "{route}");
            assert_eq!(refused.problem().errors[0].field, "email", "{route}");
        }
        linked_user(&app, "ok@example.com", "sub-ok", false).await;
        let known = post_user(&app, "revoke", token, "sub-ok", "not-an-email").await;
        assert_eq!(known.status, 204);
    }

    #[tokio::test]
    async fn the_email_is_required() {
        let app = TestApp::new().await;
        let response = app
            .request(Method::POST, "/api/internal/v1/users/revoke")
            .control_plane("users")
            .json(&json!({ "subject": "sub-x" }))
            .send()
            .await;
        assert_eq!(response.status, 400);
    }

    #[tokio::test]
    async fn delete_by_email_logs_no_email() {
        let app = TestApp::new().await;
        let user = email_only_user(&app, "quiet2@example.com").await;
        let (response, logs) = capture_logs(async {
            post_user(&app, "delete", "users", "sub-unknown", "quiet2@example.com").await
        })
        .await;
        assert_eq!(response.status, 200);
        assert!(logs.contains("user deleted"));
        assert!(logs.contains("matched=\"email\""));
        assert!(!logs.contains("quiet2@example.com"));
        assert!(!logs.contains("sub-unknown"));
        assert!(user_row(&app, user).await.is_none());
    }
}
