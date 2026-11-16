//! `POST /api/internal/v1/memberships/lookup` and `GET /api/internal/v1/orgs/{org_id}/members`:
//! the control plane reads which orgs a user belongs to, and who belongs to an org (D-P-40).
//! Emails go to the control plane only; they are never logged.

use axum::extract::{Extension, State};
use axum::routing::{get, post};

use crate::api::account::OrgRole;
use crate::api::internal::{
    InternalMember, InternalMembers, InternalMembersQuery, MembershipLookup,
    MembershipLookupRequest, MembershipOrg,
};
use crate::auth::principal::Principal;
use crate::auth::roles::parse_org_role;
use crate::http::AppState;
use crate::http::access::{Access, RouteDef};
use crate::http::error::{self, ApiError, field_error};
use crate::meta::identities::identity_user;
use crate::meta::members::org_members;
use crate::meta::orgs::{org_by_id, orgs_of_user};
use crate::meta::users::{normalize_email, user_by_email};
use crate::routes::entitlements::OrgIdPath;
use cetra_core::ids::OrgId;

/// The internal membership routes. The control plane holds a `control_plane` token with
/// scope `members`.
pub(crate) fn routes() -> Vec<RouteDef> {
    vec![
        RouteDef {
            method: axum::http::Method::POST,
            path: "/api/internal/v1/memberships/lookup",
            access: Access::ControlPlane { scope: "members" },
            documented: true,
            handler: post(lookup),
        },
        RouteDef {
            method: axum::http::Method::GET,
            path: "/api/internal/v1/orgs/{org_id}/members",
            access: Access::ControlPlane { scope: "members" },
            documented: true,
            handler: get(list_members),
        },
    ]
}

/// `POST /api/internal/v1/memberships/lookup`. An unknown user answers 200 with no orgs.
async fn lookup(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    error::Json(body): error::Json<MembershipLookupRequest>,
) -> Result<error::Json<MembershipLookup>, ApiError> {
    let Principal::ControlPlane { .. } = principal else {
        return Err(ApiError::Unauthorized);
    };
    let reader = state.meta.reader();
    let provider = &state.config.entitlements.control_plane_provider;
    let user = match identity_user(reader, provider, &body.subject).await? {
        Some(user) => Some(user),
        None => {
            let email =
                normalize_email(&body.email).map_err(|_| field_error("email", "invalid email"))?;
            user_by_email(reader, &email).await?.map(|user| user.id)
        }
    };
    let Some(user_id) = user else {
        return Ok(error::Json(MembershipLookup {
            user_id: None,
            orgs: Vec::new(),
        }));
    };
    // `orgs_of_user` lists active orgs only.
    let mut orgs: Vec<MembershipOrg> = orgs_of_user(reader, user_id)
        .await?
        .into_iter()
        .map(|(org, role)| MembershipOrg {
            org_id: org.id.as_uuid(),
            slug: org.slug,
            role,
        })
        .collect();
    orgs.sort_by(|a, b| a.slug.cmp(&b.slug));
    Ok(error::Json(MembershipLookup {
        user_id: Some(user_id),
        orgs,
    }))
}

/// The roles of `?role=owner,admin`; `None` when the parameter is absent.
fn role_filter(raw: Option<&str>) -> Result<Option<Vec<OrgRole>>, ApiError> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    raw.split(',')
        .map(|part| parse_org_role(part.trim()).ok_or_else(|| field_error("role", "unknown role")))
        .collect::<Result<Vec<_>, _>>()
        .map(Some)
}

/// `GET /api/internal/v1/orgs/{org_id}/members`. An unknown or inactive org answers 404.
async fn list_members(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    error::Path(OrgIdPath { org_id }): error::Path<OrgIdPath>,
    error::Query(query): error::Query<InternalMembersQuery>,
) -> Result<error::Json<InternalMembers>, ApiError> {
    let Principal::ControlPlane { .. } = principal else {
        return Err(ApiError::Unauthorized);
    };
    let roles = role_filter(query.role.as_deref())?;
    let reader = state.meta.reader();
    let org = OrgId::from_uuid(org_id);
    match org_by_id(reader, org).await? {
        Some(row) if row.state == "active" => {}
        _ => return Err(ApiError::NotFound),
    }
    let data = org_members(reader, org)
        .await?
        .into_iter()
        .filter(|member| {
            roles
                .as_ref()
                .is_none_or(|roles| roles.contains(&member.role))
        })
        .map(|member| InternalMember {
            user_id: member.user.id,
            email: member.user.email,
            name: member.user.name,
            role: member.role,
        })
        .collect();
    Ok(error::Json(InternalMembers { data }))
}

#[cfg(test)]
mod tests {
    use axum::http::Method;
    use serde_json::{Value, json};
    use uuid::Uuid;

    use super::*;
    use crate::meta::identities::insert_identity;
    use crate::meta::members::add_org_member;
    use crate::meta::orgs::{insert_org, mark_org_deleting};
    use crate::meta::users::{NewUser, insert_user};
    use crate::test_support::{TestApp, TestResponse, capture_logs};

    const PROVIDER: &str = "oidc:cloud";

    async fn lookup_with(app: &TestApp, token: &str, subject: &str, email: &str) -> TestResponse {
        app.request(Method::POST, "/api/internal/v1/memberships/lookup")
            .control_plane(token)
            .json(&json!({ "subject": subject, "email": email }))
            .send()
            .await
    }

    async fn members_with(app: &TestApp, token: &str, org: OrgId, query: &str) -> TestResponse {
        app.request(
            Method::GET,
            &format!("/api/internal/v1/orgs/{org}/members{query}"),
        )
        .control_plane(token)
        .send()
        .await
    }

    /// A user with an OIDC identity at the control plane's provider.
    async fn linked_user(app: &TestApp, email: &str, subject: &str) -> Uuid {
        let email = email.to_owned();
        let subject = subject.to_owned();
        let now = app.state.clock.now_ns();
        app.state
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
                            email_verified_via: Some(
                                crate::meta::users::EmailVerifiedVia::MailToken,
                            ),
                            is_instance_admin: false,
                        },
                        now,
                    )
                    .await?;
                    insert_identity(conn, user.id, PROVIDER, &subject, &email, now).await?;
                    Ok(user.id)
                })
            })
            .await
            .unwrap()
    }

    /// A user with no identity, a member of `org` with `role`.
    async fn member(app: &TestApp, org: OrgId, email: &str, role: OrgRole) -> Uuid {
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
                            name: Some("Named".to_owned()),
                            password_hash: None,
                            email_verified_at: Some(now),
                            email_verified_via: Some(
                                crate::meta::users::EmailVerifiedVia::MailToken,
                            ),
                            is_instance_admin: false,
                        },
                        now,
                    )
                    .await?;
                    add_org_member(conn, org, user.id, role, now).await?;
                    Ok(user.id)
                })
            })
            .await
            .unwrap()
    }

    async fn org(app: &TestApp, name: &str, slug: &str) -> OrgId {
        let (name, slug) = (name.to_owned(), slug.to_owned());
        let now = app.state.clock.now_ns();
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    let row = insert_org(conn, OrgId::new_v7(), &name, &slug, now).await?;
                    Ok(row.id)
                })
            })
            .await
            .unwrap()
    }

    async fn join(app: &TestApp, org: OrgId, user: Uuid, role: OrgRole) {
        let now = app.state.clock.now_ns();
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move { add_org_member(conn, org, user, role, now).await })
            })
            .await
            .unwrap();
    }

    async fn retire(app: &TestApp, org: OrgId) {
        let now = app.state.clock.now_ns();
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move { mark_org_deleting(conn, org, now).await.map(|_| ()) })
            })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn lookup_by_subject_lists_active_orgs_by_slug() {
        let app = TestApp::new().await;
        let user = linked_user(&app, "by-sub@example.com", "sub-lookup").await;
        let zeta = org(&app, "Zeta", "zeta").await;
        let alpha = org(&app, "Alpha", "alpha").await;
        let gone = org(&app, "Gone", "gone").await;
        join(&app, zeta, user, OrgRole::Viewer).await;
        join(&app, alpha, user, OrgRole::Owner).await;
        join(&app, gone, user, OrgRole::Member).await;
        retire(&app, gone).await;

        let token = "members";
        let found = lookup_with(&app, token, "sub-lookup", "ignored@example.com").await;
        assert_eq!(found.status, 200);
        assert_eq!(
            found.json::<Value>(),
            json!({
                "user_id": user.to_string(),
                "orgs": [
                    { "org_id": alpha.to_string(), "slug": "alpha", "role": "owner" },
                    { "org_id": zeta.to_string(), "slug": "zeta", "role": "viewer" },
                ],
            })
        );
    }

    #[tokio::test]
    async fn lookup_falls_back_to_the_normalized_email() {
        let app = TestApp::new().await;
        let user = member(
            &app,
            org(&app, "Mail", "mail").await,
            "mail@example.com",
            OrgRole::Admin,
        )
        .await;

        let token = "members";
        let found = lookup_with(&app, token, "unknown-subject", "  Mail@Example.COM ").await;
        assert_eq!(found.status, 200);
        let body: Value = found.json();
        assert_eq!(body["user_id"], json!(user.to_string()));
        assert_eq!(body["orgs"][0]["slug"], json!("mail"));
        assert_eq!(body["orgs"][0]["role"], json!("admin"));
    }

    #[tokio::test]
    async fn lookup_of_an_unknown_user_is_empty() {
        let app = TestApp::new().await;
        let token = "members";
        let none = lookup_with(&app, token, "nobody", "nobody@example.com").await;
        assert_eq!(none.status, 200);
        assert_eq!(none.json::<Value>(), json!({ "orgs": [] }));
    }

    #[tokio::test]
    async fn lookup_refuses_an_invalid_email() {
        let app = TestApp::new().await;
        let token = "members";
        let refused = lookup_with(&app, token, "nobody", "not-an-email").await;
        assert_eq!(refused.status, 400);
    }

    #[tokio::test]
    async fn members_are_listed_by_email_and_filtered_by_role() {
        let app = TestApp::new().await;
        let home = org(&app, "Home", "home").await;
        member(&app, home, "zoe@example.com", OrgRole::Viewer).await;
        member(&app, home, "bea@example.com", OrgRole::Admin).await;
        member(&app, home, "ann@example.com", OrgRole::Owner).await;
        member(&app, home, "cy@example.com", OrgRole::Member).await;
        let token = "members";

        let (all, logs) = capture_logs(async { members_with(&app, token, home, "").await }).await;
        assert_eq!(all.status, 200);
        assert!(!logs.contains("example.com"));
        let emails: Vec<String> = all.json::<Value>()["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|member| member["email"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(
            emails,
            [
                "ann@example.com",
                "bea@example.com",
                "cy@example.com",
                "zoe@example.com"
            ]
        );

        let some = members_with(&app, token, home, "?role=owner,admin").await;
        assert_eq!(some.status, 200);
        let body: Value = some.json();
        let roles: Vec<(&str, &str)> = body["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|member| {
                (
                    member["email"].as_str().unwrap(),
                    member["role"].as_str().unwrap(),
                )
            })
            .collect();
        assert_eq!(
            roles,
            [("ann@example.com", "owner"), ("bea@example.com", "admin")]
        );
        assert_eq!(body["data"][0]["name"], json!("Named"));
    }

    #[tokio::test]
    async fn members_refuse_an_unknown_role() {
        let app = TestApp::new().await;
        let home = org(&app, "Roles", "roles").await;
        let token = "members";
        let refused = members_with(&app, token, home, "?role=owner,boss").await;
        assert_eq!(refused.status, 400);
    }

    #[tokio::test]
    async fn members_of_an_unknown_or_inactive_org_are_not_found() {
        let app = TestApp::new().await;
        let token = "members";
        let unknown = members_with(&app, token, OrgId::new_v7(), "").await;
        assert_eq!(unknown.status, 404);

        let gone = org(&app, "Retired", "retired").await;
        retire(&app, gone).await;
        let inactive = members_with(&app, token, gone, "").await;
        assert_eq!(inactive.status, 404);
    }

    #[tokio::test]
    async fn needs_the_members_scope() {
        let app = TestApp::new().await;
        let home = org(&app, "Scoped", "scoped").await;
        let users = "users";
        assert_eq!(
            lookup_with(&app, users, "sub", "x@example.com")
                .await
                .status,
            403
        );
        assert_eq!(members_with(&app, users, home, "").await.status, 403);
        let anonymous = app
            .request(Method::POST, "/api/internal/v1/memberships/lookup")
            .json(&json!({ "subject": "sub", "email": "x@example.com" }))
            .send()
            .await;
        assert_eq!(anonymous.status, 401);
    }

    #[tokio::test]
    async fn the_subject_wins_over_the_email() {
        let app = TestApp::new().await;
        let linked = linked_user(&app, "linked@example.com", "sub-wins").await;
        let other = member(
            &app,
            org(&app, "Other", "other").await,
            "other@example.com",
            OrgRole::Member,
        )
        .await;
        let token = "members";
        let found = lookup_with(&app, token, "sub-wins", "other@example.com").await;
        let body: Value = found.json();
        assert_eq!(body["user_id"], json!(linked.to_string()));
        assert_ne!(body["user_id"], json!(other.to_string()));
    }
}
