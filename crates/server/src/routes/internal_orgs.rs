//! The control plane's org routes: `POST /api/internal/v1/orgs` creates an org with its id and
//! owner (D-P-29, D-P-49); `DELETE /api/internal/v1/orgs/{org_id}` starts its drop (D-P-49).

use axum::extract::{Extension, State};
use axum::http::StatusCode;
use axum::routing::{delete, post};
use llmobs_core::ids::OrgId;

use crate::api::account::OrgRole;
use crate::api::deletions::Deletion;
use crate::api::internal::{CreateInternalOrgRequest, InternalOrg, InternalOwner};
use crate::auth::principal::Principal;
use crate::deletion::request_org_drop;
use crate::http::AppState;
use crate::http::access::{Access, RouteDef};
use crate::http::error::{self, ApiError};
use crate::meta::deletions::{ActorKind, org_drop_exists_tx, org_drop_of};
use crate::meta::members::add_org_member;
use crate::meta::orgs::{self, OrgRow, org_by_id, org_by_id_tx};
use crate::meta::personal_tokens::revoke_user_tokens;
use crate::meta::users::EmailVerifiedVia;
use crate::meta::users::{
    NewUser, insert_user, normalize_email, set_email_verified, set_password_hash, user_by_email_tx,
};
use crate::meta::web_sessions::delete_user_sessions;
use crate::routes::deletions::deletion_json;
use crate::routes::entitlements::OrgIdPath;
use crate::routes::orgs::{field_error, trimmed_name};

/// The internal org routes. The control plane holds a `control_plane` token with scope `orgs`.
pub(crate) fn routes() -> Vec<RouteDef> {
    vec![
        RouteDef {
            method: axum::http::Method::POST,
            path: "/api/internal/v1/orgs",
            access: Access::ControlPlane { scope: "orgs" },
            documented: true,
            handler: post(create_org),
        },
        RouteDef {
            method: axum::http::Method::DELETE,
            path: "/api/internal/v1/orgs/{org_id}",
            access: Access::ControlPlane { scope: "orgs" },
            documented: true,
            handler: delete(drop_org),
        },
    ]
}

/// What the write transaction did.
enum Outcome {
    /// The org and its owner membership were created, with the hashes of the PATs revoked
    /// from an unverified owner account.
    Created(OrgRow, Vec<String>),
    /// An org with this id already exists. Nothing changed (an idempotent retry).
    Existing(OrgRow),
    /// The email belongs to a disabled user or an instance admin.
    OwnerNotAllowed,
    /// The org with this id is being deleted (D-P-49).
    Dropping,
    /// The org with this id was deleted; its id is never reused (D-P-49).
    Dropped,
}

/// `POST /api/internal/v1/orgs`. 201 when the org is created, 200 when the id already exists,
/// 409 when the id is being or was deleted.
async fn create_org(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    error::Json(body): error::Json<CreateInternalOrgRequest>,
) -> Result<(StatusCode, error::Json<InternalOrg>), ApiError> {
    let Principal::ControlPlane { token_id, .. } = principal else {
        return Err(ApiError::Unauthorized);
    };
    if body.id.get_version_num() != 7 {
        return Err(field_error("id", "must be a UUIDv7"));
    }
    let name = trimmed_name(&body.name)?;
    let InternalOwner {
        email,
        name: owner_name,
    } = body.owner;
    let owner_name = owner_name_of(owner_name.as_deref())?;
    let email = normalize_email(&email).map_err(|_| field_error("owner.email", "invalid email"))?;
    let id = OrgId::from_uuid(body.id);
    let now = state.clock.now_ns();
    let outcome = state
        .meta
        .write(move |conn| {
            Box::pin(async move {
                if let Some(org) = org_by_id_tx(conn, id).await? {
                    if org.state == "deleting" {
                        return Ok(Outcome::Dropping);
                    }
                    return Ok(Outcome::Existing(org));
                }
                if org_drop_exists_tx(conn, id).await? {
                    return Ok(Outcome::Dropped);
                }
                let mut revoked = Vec::new();
                let owner = match user_by_email_tx(conn, &email).await? {
                    Some(user) if user.disabled_at.is_some() || user.is_instance_admin => {
                        return Ok(Outcome::OwnerNotAllowed);
                    }
                    Some(user) => {
                        if !user.email_proven() {
                            // D148's defence against a pre-registered account: whoever signed
                            // up with this email never proved it, so their password, sessions
                            // and tokens go before the account owns the org.
                            delete_user_sessions(conn, user.id, None).await?;
                            revoked = revoke_user_tokens(conn, user.id, now).await?;
                            set_password_hash(conn, user.id, None, now).await?;
                            set_email_verified(conn, user.id, now, EmailVerifiedVia::ControlPlane)
                                .await?;
                        }
                        user
                    }
                    None => {
                        insert_user(
                            conn,
                            NewUser {
                                email,
                                name: owner_name,
                                password_hash: None,
                                email_verified_at: Some(now),
                                email_verified_via: Some(EmailVerifiedVia::ControlPlane),
                                is_instance_admin: false,
                            },
                            now,
                        )
                        .await?
                    }
                };
                let org = orgs::insert_org_with_derived_slug(conn, id, &name, now).await?;
                add_org_member(conn, org.id, owner.id, OrgRole::Owner, now).await?;
                Ok(Outcome::Created(org, revoked))
            })
        })
        .await?;
    match outcome {
        Outcome::Created(org, revoked) => {
            for hash in revoked {
                state.pat_cache.evict(&hash);
            }
            tracing::info!(
                target: "audit",
                org_id = %org.id,
                actor = "control_plane",
                token_id = %token_id,
                "org created"
            );
            Ok((StatusCode::CREATED, error::Json(internal_org(&org))))
        }
        Outcome::Existing(org) => Ok((StatusCode::OK, error::Json(internal_org(&org)))),
        Outcome::OwnerNotAllowed => Err(ApiError::OwnerNotAllowed),
        Outcome::Dropping => Err(conflict("the organization is being deleted")),
        Outcome::Dropped => Err(conflict("the organization was deleted; use a new id")),
    }
}

fn conflict(detail: &'static str) -> ApiError {
    crate::meta::MetaError::Conflict(detail).into()
}

/// `DELETE /api/internal/v1/orgs/{org_id}`: starts the org's drop (D-M-7) with no confirm. 202
/// with the deletion while it runs, 200 once it is applied. Repeats answer for the org's
/// newest org deletion, so a retry is safe whatever happened in between. An org that was
/// never dropped answers 404.
async fn drop_org(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    error::Path(OrgIdPath { org_id }): error::Path<OrgIdPath>,
) -> Result<(StatusCode, error::Json<Deletion>), ApiError> {
    let Principal::ControlPlane { token_id, .. } = principal else {
        return Err(ApiError::Unauthorized);
    };
    let org = OrgId::from_uuid(org_id);
    if let Some(row) = org_by_id(state.meta.reader(), org).await?
        && row.state == "active"
        && let Some(deletion) =
            request_org_drop(&state, &row, (ActorKind::ControlPlane, token_id)).await?
    {
        return Ok((StatusCode::ACCEPTED, error::Json(deletion_json(&deletion))));
    }
    let Some(deletion) = org_drop_of(state.meta.reader(), org).await? else {
        return Err(ApiError::NotFound);
    };
    let status = match deletion.state {
        crate::meta::deletions::DeletionState::Applied => StatusCode::OK,
        _ => StatusCode::ACCEPTED,
    };
    Ok((status, error::Json(deletion_json(&deletion))))
}

/// The owner's display name: trimmed, absent when empty, at most 100 characters.
fn owner_name_of(value: Option<&str>) -> Result<Option<String>, ApiError> {
    let Some(name) = value.map(str::trim).filter(|name| !name.is_empty()) else {
        return Ok(None);
    };
    if name.chars().count() > 100 {
        return Err(field_error("owner.name", "must be 1 to 100 characters"));
    }
    Ok(Some(name.to_owned()))
}

fn internal_org(org: &OrgRow) -> InternalOrg {
    InternalOrg {
        id: org.id.as_uuid(),
        slug: org.slug.clone(),
    }
}

#[cfg(test)]
mod tests {
    use axum::http::{Method, header};
    use serde_json::{Value, json};
    use uuid::Uuid;

    use super::*;
    use crate::api::common::ProblemCode;
    use crate::meta::users::{NewUser, insert_user, set_disabled, user_by_email};
    use crate::test_support::{
        TestApp, TestResponse, capture_logs, control_plane_token, sign_token,
    };

    fn bearer(token: &str) -> String {
        format!("Bearer {token}")
    }

    fn body(id: Uuid, name: &str, email: &str, owner_name: Option<&str>) -> Value {
        let mut owner = json!({ "email": email });
        if let Some(owner_name) = owner_name {
            owner["name"] = json!(owner_name);
        }
        json!({ "id": id, "name": name, "owner": owner })
    }

    async fn create(app: &TestApp, token: &str, body: &Value) -> TestResponse {
        app.request(Method::POST, "/api/internal/v1/orgs")
            .header(header::AUTHORIZATION.as_str(), &bearer(token))
            .json(body)
            .send()
            .await
    }

    async fn org_count_for(app: &TestApp, id: Uuid) -> i64 {
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    sqlx::query_scalar("SELECT COUNT(*) FROM orgs WHERE id = ?")
                        .bind(id.to_string())
                        .fetch_one(&mut *conn)
                        .await
                        .map_err(crate::meta::MetaError::from)
                })
            })
            .await
            .unwrap()
    }

    async fn project_count_for(app: &TestApp, id: Uuid) -> i64 {
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    sqlx::query_scalar("SELECT COUNT(*) FROM projects WHERE org_id = ?")
                        .bind(id.to_string())
                        .fetch_one(&mut *conn)
                        .await
                        .map_err(crate::meta::MetaError::from)
                })
            })
            .await
            .unwrap()
    }

    async fn insert_plain_user(app: &TestApp, email: &str, name: Option<&str>) -> Uuid {
        let email = email.to_owned();
        let name = name.map(str::to_owned);
        let now = app.state.clock.now_ns();
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    insert_user(
                        conn,
                        NewUser {
                            email,
                            name,
                            password_hash: None,
                            email_verified_at: None,
                            email_verified_via: None,
                            is_instance_admin: false,
                        },
                        now,
                    )
                    .await
                })
            })
            .await
            .unwrap()
            .id
    }

    #[tokio::test]
    async fn creates_org_and_new_owner() {
        let app = TestApp::new().await;
        let id = Uuid::now_v7();
        let (created, logs) = capture_logs(async {
            create(
                &app,
                &control_plane_token("orgs"),
                &body(id, "Acme Cloud", "new@example.com", Some("New Owner")),
            )
            .await
        })
        .await;
        assert_eq!(created.status, 201);
        let org: InternalOrg = created.json();
        assert_eq!(org.id, id);
        assert_eq!(org.slug, "acme-cloud");
        assert!(logs.contains("org created"));
        assert!(logs.contains("control_plane"));
        assert!(!logs.contains("new@example.com"));
        assert!(!logs.contains("New Owner"));
        assert!(!logs.contains("Acme Cloud"));

        let user = user_by_email(app.state.meta.reader(), "new@example.com")
            .await
            .unwrap()
            .unwrap();
        assert!(user.email_verified_at.is_some());
        assert!(user.password_hash.is_none());
        assert_eq!(user.name.as_deref(), Some("New Owner"));
        assert_eq!(project_count_for(&app, id).await, 0);
        let owner: String =
            sqlx::query_scalar("SELECT role FROM org_members WHERE org_id = ? AND user_id = ?")
                .bind(id.to_string())
                .bind(user.id.to_string())
                .fetch_one(app.state.meta.reader())
                .await
                .unwrap();
        assert_eq!(owner, "owner");
    }

    /// Review 2026-11-15 #4: an account signed up with the owner's email but never verified kept
    /// its password and sessions, so whoever registered it first owned the new org.
    #[tokio::test]
    async fn an_unverified_owner_loses_the_password_and_sessions() {
        let app = TestApp::new().await;
        let now = app.state.clock.now_ns();
        let user_id = app
            .state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    insert_user(
                        conn,
                        NewUser {
                            email: "squatted@example.com".to_owned(),
                            name: None,
                            password_hash: Some("$argon2id$attacker".to_owned()),
                            email_verified_at: None,
                            email_verified_via: None,
                            is_instance_admin: false,
                        },
                        now,
                    )
                    .await
                })
            })
            .await
            .unwrap()
            .id;
        crate::auth::session::start_session(&app.state, user_id, None, None, None)
            .await
            .unwrap();
        let id = Uuid::now_v7();
        let created = create(
            &app,
            &control_plane_token("orgs"),
            &body(id, "Squat", "squatted@example.com", None),
        )
        .await;
        assert_eq!(created.status, 201);
        let user = user_by_email(app.state.meta.reader(), "squatted@example.com")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(user.id, user_id);
        assert_eq!(user.password_hash, None);
        assert!(user.email_verified_at.is_some());
        let sessions: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM web_sessions WHERE user_id = ?")
                .bind(user_id.to_string())
                .fetch_one(app.state.meta.reader())
                .await
                .unwrap();
        assert_eq!(sessions, 0);
    }

    #[tokio::test]
    async fn reuses_an_existing_user() {
        let app = TestApp::new().await;
        let user_id = insert_plain_user(&app, "existing@example.com", Some("Existing")).await;
        let id = Uuid::now_v7();
        let created = create(
            &app,
            &control_plane_token("orgs"),
            &body(id, "Reuse", "existing@example.com", Some("Other Name")),
        )
        .await;
        assert_eq!(created.status, 201);
        let user = user_by_email(app.state.meta.reader(), "existing@example.com")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(user.id, user_id);
        assert_eq!(user.name.as_deref(), Some("Existing"));
        let members: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM org_members WHERE org_id = ? AND user_id = ? AND role = 'owner'",
        )
        .bind(id.to_string())
        .bind(user_id.to_string())
        .fetch_one(app.state.meta.reader())
        .await
        .unwrap();
        assert_eq!(members, 1);
    }

    #[tokio::test]
    async fn same_id_is_idempotent() {
        let app = TestApp::new().await;
        let token = control_plane_token("orgs");
        let id = Uuid::now_v7();
        let request = body(id, "Retry", "retry@example.com", None);
        let first = create(&app, &token, &request).await;
        assert_eq!(first.status, 201);
        let second = create(&app, &token, &request).await;
        assert_eq!(second.status, 200);
        let first: InternalOrg = first.json();
        let second: InternalOrg = second.json();
        assert_eq!(first, second);

        // Ownership changes after creation do not matter: the id is the key.
        let org = id.to_string();
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    sqlx::query("DELETE FROM org_members WHERE org_id = ?")
                        .bind(org)
                        .execute(&mut *conn)
                        .await
                        .map_err(crate::meta::MetaError::from)
                })
            })
            .await
            .unwrap();
        let third = create(&app, &token, &request).await;
        assert_eq!(third.status, 200);
        assert_eq!(third.json::<InternalOrg>(), first);
        assert_eq!(org_count_for(&app, id).await, 1);
    }

    #[tokio::test]
    async fn refuses_disabled_and_admin_owners() {
        let app = TestApp::new().await;
        app.seed().await;
        let token = control_plane_token("orgs");

        let disabled = insert_plain_user(&app, "disabled@example.com", None).await;
        let now = app.state.clock.now_ns();
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move { set_disabled(conn, disabled, Some(now), now).await })
            })
            .await
            .unwrap();
        let disabled_id = Uuid::now_v7();
        let refused = create(
            &app,
            &token,
            &body(disabled_id, "Blocked", "disabled@example.com", None),
        )
        .await;
        assert_eq!(refused.status, 422);
        assert_eq!(refused.problem().code, ProblemCode::OwnerNotAllowed);
        assert_eq!(org_count_for(&app, disabled_id).await, 0);

        let admin_id = Uuid::now_v7();
        let refused = create(
            &app,
            &token,
            &body(admin_id, "Admin Owned", "ada@example.com", None),
        )
        .await;
        assert_eq!(refused.status, 422);
        assert_eq!(refused.problem().code, ProblemCode::OwnerNotAllowed);
        assert_eq!(org_count_for(&app, admin_id).await, 0);
    }

    #[tokio::test]
    async fn validation_refuses_bad_input() {
        let app = TestApp::new().await;
        let token = control_plane_token("orgs");

        let v4 = create(
            &app,
            &token,
            &body(Uuid::new_v4(), "Acme", "v4@example.com", None),
        )
        .await;
        assert_eq!(v4.status, 400);
        assert_eq!(v4.problem().errors[0].field, "id");

        let blank = create(
            &app,
            &token,
            &body(Uuid::now_v7(), "   ", "blank@example.com", None),
        )
        .await;
        assert_eq!(blank.status, 400);
        assert_eq!(blank.problem().errors[0].field, "name");

        let bad_email = create(
            &app,
            &token,
            &body(Uuid::now_v7(), "Acme", "not-an-email", None),
        )
        .await;
        assert_eq!(bad_email.status, 400);
        assert_eq!(bad_email.problem().errors[0].field, "owner.email");

        let unknown = create(
            &app,
            &token,
            &json!({
                "id": Uuid::now_v7(),
                "name": "Acme",
                "owner": { "email": "unknown@example.com" },
                "plan": "pro",
            }),
        )
        .await;
        assert_eq!(unknown.status, 400);
    }

    #[tokio::test]
    async fn needs_the_orgs_scope() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let id = Uuid::now_v7();
        let request = body(id, "Scoped", "scoped@example.com", None);

        let usage = create(&app, &control_plane_token("usage"), &request).await;
        assert_eq!(usage.status, 403);

        let session = app
            .request(Method::POST, "/api/internal/v1/orgs")
            .header(header::COOKIE.as_str(), &seeded.cookie)
            .json(&request)
            .send()
            .await;
        assert_eq!(session.status, 403);

        let anonymous = app
            .request(Method::POST, "/api/internal/v1/orgs")
            .json(&request)
            .send()
            .await;
        assert_eq!(anonymous.status, 401);
        assert_eq!(org_count_for(&app, id).await, 0);
    }

    #[tokio::test]
    async fn slug_collision_gets_a_suffix() {
        let app = TestApp::new().await;
        let token = control_plane_token("orgs");
        let first = Uuid::now_v7();
        let created = create(
            &app,
            &token,
            &body(first, "Acme Cloud", "first@example.com", None),
        )
        .await;
        assert_eq!(created.status, 201);
        assert_eq!(created.json::<InternalOrg>().slug, "acme-cloud");

        let second = Uuid::now_v7();
        let created = create(
            &app,
            &token,
            &body(second, "Acme Cloud", "second@example.com", None),
        )
        .await;
        assert_eq!(created.status, 201);
        assert_eq!(created.json::<InternalOrg>().slug, "acme-cloud-2");
    }

    /// A `DELETE` of `org_id` with `token`.
    async fn drop_org_request(app: &TestApp, token: &str, org_id: Uuid) -> TestResponse {
        app.request(Method::DELETE, &format!("/api/internal/v1/orgs/{org_id}"))
            .header(header::AUTHORIZATION.as_str(), &bearer(token))
            .send()
            .await
    }

    #[tokio::test]
    async fn the_control_plane_drops_an_org() {
        let app = TestApp::new().await;
        let token_id = Uuid::now_v7();
        let token = sign_token(&json!({
            "kind": "control_plane",
            "id": token_id.to_string(),
            "issued_at": "2026-01-01T00:00:00Z",
            "expires_at": "2026-01-01T00:05:00Z",
            "audience": app.state.config.public_url.origin(),
            "scope": "orgs",
        }));
        let id = Uuid::now_v7();
        let created = create(
            &app,
            &token,
            &body(id, "Doomed", "doomed@example.com", None),
        )
        .await;
        assert_eq!(created.status, 201);

        let (first, logs) = capture_logs(async { drop_org_request(&app, &token, id).await }).await;
        assert_eq!(first.status, 202);
        let deletion: Value = first.json();
        assert_eq!(deletion["requested_by"]["kind"], "control_plane");
        assert_eq!(deletion["requested_by"]["id"], json!(token_id));
        assert!(logs.contains("deletion requested"));
        assert!(logs.contains("control_plane"));
        assert!(!logs.contains("doomed@example.com"));
        let deletion_id = deletion["id"].clone();

        let again = drop_org_request(&app, &token, id).await;
        assert_eq!(again.status, 202);
        assert_eq!(again.json::<Value>()["id"], deletion_id);

        crate::deletion::run_once(&app.state).await.unwrap();
        let applied = drop_org_request(&app, &token, id).await;
        assert_eq!(applied.status, 200);
        let applied: Value = applied.json();
        assert_eq!(applied["status"], "applied");
        assert_eq!(applied["id"], deletion_id);
        assert_eq!(org_count_for(&app, id).await, 0);
    }

    #[tokio::test]
    async fn drop_refuses_unknown_orgs_and_other_callers() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let id = Uuid::now_v7();

        let unknown = drop_org_request(&app, &control_plane_token("orgs"), id).await;
        assert_eq!(unknown.status, 404);

        let usage =
            drop_org_request(&app, &control_plane_token("usage"), seeded.org_id.as_uuid()).await;
        assert_eq!(usage.status, 403);

        let session = app
            .request(
                Method::DELETE,
                &format!("/api/internal/v1/orgs/{}", seeded.org_id),
            )
            .header(header::COOKIE.as_str(), &seeded.cookie)
            .send()
            .await;
        assert_eq!(session.status, 403);
        assert!(
            org_by_id(app.state.meta.reader(), seeded.org_id)
                .await
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn create_refuses_a_dropping_or_dropped_id() {
        let app = TestApp::new().await;
        let token = control_plane_token("orgs");
        let id = Uuid::now_v7();
        assert_eq!(
            create(&app, &token, &body(id, "Once", "once@example.com", None))
                .await
                .status,
            201
        );
        assert_eq!(drop_org_request(&app, &token, id).await.status, 202);

        let dropping = create(&app, &token, &body(id, "Once", "once@example.com", None)).await;
        assert_eq!(dropping.status, 409);
        assert_eq!(dropping.problem().code, ProblemCode::Conflict);
        assert_eq!(
            dropping.problem().detail.as_deref(),
            Some("the organization is being deleted")
        );

        crate::deletion::run_once(&app.state).await.unwrap();
        let dropped = create(&app, &token, &body(id, "Once", "once@example.com", None)).await;
        assert_eq!(dropped.status, 409);
        assert_eq!(
            dropped.problem().detail.as_deref(),
            Some("the organization was deleted; use a new id")
        );

        let fresh = create(
            &app,
            &token,
            &body(Uuid::now_v7(), "Fresh", "fresh@example.com", None),
        )
        .await;
        assert_eq!(fresh.status, 201);
    }
}
