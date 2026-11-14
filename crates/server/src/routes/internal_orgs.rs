//! `POST /api/internal/v1/orgs`: the control plane creates an org with its id and owner (D-P-29).

use axum::extract::{Extension, State};
use axum::http::StatusCode;
use axum::routing::post;

use crate::api::account::OrgRole;
use crate::api::internal::{CreateInternalOrgRequest, InternalOrg, InternalOwner};
use crate::auth::principal::Principal;
use crate::http::AppState;
use crate::http::access::{Access, RouteDef};
use crate::http::error::{self, ApiError};
use crate::meta::members::add_org_member;
use crate::meta::orgs::{self, OrgRow, org_by_id_tx};
use crate::meta::users::{NewUser, insert_user, normalize_email, user_by_email_tx};
use crate::routes::orgs::{field_error, trimmed_name};
use llmobs_core::ids::OrgId;

/// The internal org route. The control plane holds a `control_plane` token with scope `orgs`.
pub(crate) fn routes() -> Vec<RouteDef> {
    vec![RouteDef {
        method: axum::http::Method::POST,
        path: "/api/internal/v1/orgs",
        access: Access::ControlPlane { scope: "orgs" },
        documented: true,
        handler: post(create_org),
    }]
}

/// What the write transaction did.
enum Outcome {
    /// The org and its owner membership were created.
    Created(OrgRow),
    /// An org with this id already exists. Nothing changed (an idempotent retry).
    Existing(OrgRow),
    /// The email belongs to a disabled user or an instance admin.
    OwnerNotAllowed,
}

/// `POST /api/internal/v1/orgs`. 201 when the org is created, 200 when the id already exists.
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
                    return Ok(Outcome::Existing(org));
                }
                let owner = match user_by_email_tx(conn, &email).await? {
                    Some(user) if user.disabled_at.is_some() || user.is_instance_admin => {
                        return Ok(Outcome::OwnerNotAllowed);
                    }
                    Some(user) => user,
                    None => {
                        insert_user(
                            conn,
                            NewUser {
                                email,
                                name: owner_name,
                                password_hash: None,
                                email_verified_at: Some(now),
                                is_instance_admin: false,
                            },
                            now,
                        )
                        .await?
                    }
                };
                let org = orgs::insert_org_with_derived_slug(conn, id, &name, now).await?;
                add_org_member(conn, org.id, owner.id, OrgRole::Owner, now).await?;
                Ok(Outcome::Created(org))
            })
        })
        .await?;
    match outcome {
        Outcome::Created(org) => {
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
    }
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
    use crate::test_support::{TestApp, TestResponse, capture_logs, control_plane_token};

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
}
