//! Org members and per-project overrides (spec 08, D139).
#![cfg_attr(not(test), allow(dead_code))]

use std::collections::HashMap;

use axum::extract::{Extension, State};
use axum::http::StatusCode;
use axum::routing::{delete, get, patch};
use serde::Deserialize;
use uuid::Uuid;

use crate::api::account::OrgRole;
use crate::api::common::{Page, PageParams, ProblemCode};
use crate::audit::catalog::MemberAction;
use crate::audit::{self, Event, TargetKind};
use crate::api::members::{
    MemberUser, OrgMember, ProjectMember, ProjectRoleOverride, UpdateOrgMemberRequest,
};
use crate::auth::principal::{self, Principal};
use crate::auth::roles::{self, OrgPerm, ProjectOverride, ProjectPerm, Role};
use crate::entitlements::{over_limit, plan_limit};
use crate::http::AppState;
use crate::http::access::{Access, OrgAccess, ProjectAccess, RouteDef};
use crate::http::error::{self, ApiError};
use crate::meta::MetaError;
use crate::meta::invites;
use crate::meta::members::{self, MemberRow};

use super::orgs::page_named;

/// Member and override routes.
pub(crate) fn routes() -> Vec<RouteDef> {
    vec![
        RouteDef {
            method: axum::http::Method::GET,
            path: "/api/v1/orgs/{org}/members",
            access: Access::Org {
                perm: OrgPerm::View,
                session_only: false,
            },
            documented: true,
            handler: get(list_org_members),
        },
        RouteDef {
            method: axum::http::Method::PATCH,
            path: "/api/v1/orgs/{org}/members/{user}",
            access: Access::Org {
                perm: OrgPerm::ManageMembers,
                session_only: true,
            },
            documented: true,
            handler: patch(update_org_member),
        },
        RouteDef {
            method: axum::http::Method::DELETE,
            path: "/api/v1/orgs/{org}/members/{user}",
            access: Access::Org {
                perm: OrgPerm::View,
                session_only: true,
            },
            documented: true,
            handler: delete(remove_org_member),
        },
        RouteDef {
            method: axum::http::Method::GET,
            path: "/api/v1/projects/{project}/members",
            access: Access::Project {
                perm: ProjectPerm::ViewMembers,
                session_only: false,
            },
            documented: true,
            handler: get(list_project_members),
        },
    ]
}

#[derive(Debug, Deserialize)]
pub(crate) struct UserPath {
    pub(crate) user: Uuid,
}

pub(crate) enum MemberChange {
    Done,
    Missing,
    Forbidden,
}

enum RoleChange {
    Done,
    Missing,
    Forbidden,
    /// The org's seat limit, which the new role would exceed.
    OverLimit(u64),
}

async fn list_org_members(
    State(state): State<AppState>,
    Extension(access): Extension<OrgAccess>,
    error::Query(params): error::Query<PageParams>,
) -> Result<error::Json<Page<OrgMember>>, ApiError> {
    let members = members::org_members(state.meta.reader(), access.org.id).await?;
    let rows = members
        .into_iter()
        .map(|member| {
            (
                member.user.email.clone(),
                member.user.id.to_string(),
                org_member(&member),
            )
        })
        .collect();
    Ok(error::Json(page_named(
        rows,
        params.cursor.as_deref(),
        params.limit.get(),
    )?))
}

async fn update_org_member(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Extension(access): Extension<OrgAccess>,
    ctx: audit::Ctx,
    error::Path(UserPath { user: target }): error::Path<UserPath>,
    error::Json(body): error::Json<UpdateOrgMemberRequest>,
) -> Result<error::Json<OrgMember>, ApiError> {
    let user = principal::session_user(&principal)?;
    let actor = user.id;
    let org_id = access.org.id;
    let role = body.role;
    let now = state.clock.now_ns();
    let limits_state = state.clone();
    let outcome = state
        .meta
        .write(move |conn| {
            Box::pin(async move {
                let Some(actor_role) = members::org_role_tx(conn, org_id, actor).await? else {
                    return Ok(RoleChange::Forbidden);
                };
                if roles::org_rank(actor_role) < roles::org_rank(OrgRole::Admin) {
                    return Ok(RoleChange::Forbidden);
                }
                let Some(target_role) = members::org_role_tx(conn, org_id, target).await? else {
                    return Ok(RoleChange::Missing);
                };
                if !may_set_role(actor_role, target_role, role) {
                    return Ok(RoleChange::Forbidden);
                }
                if target_role == OrgRole::Viewer && role != OrgRole::Viewer {
                    let max_seats = limits_state
                        .entitlements
                        .resolve(org_id, now)
                        .limits
                        .max_seats;
                    if let Some(limit) = max_seats {
                        let seats = members::seat_members_tx(conn, org_id).await?
                            + invites::seat_invites_tx(conn, org_id, now, None).await?;
                        if over_limit(Some(limit), seats, 1) {
                            return Ok(RoleChange::OverLimit(limit));
                        }
                    }
                }
                members::set_org_role(conn, org_id, target, role).await?;
                if matches!(role, OrgRole::Owner | OrgRole::Admin) {
                    members::clear_project_overrides(conn, org_id, target).await?;
                }
                if target_role == OrgRole::Owner
                    && members::count_active_owners(conn, org_id).await? == 0
                {
                    return Err(MetaError::Conflict("an org needs an owner"));
                }
                audit::record(
                    conn,
                    &ctx,
                    Event::new(MemberAction::RoleChanged {
                        old_role: roles::org_role_str(target_role),
                        new_role: roles::org_role_str(role),
                    })
                    .org(org_id)
                    .target(TargetKind::User, target),
                    now,
                )
                .await?;
                Ok(RoleChange::Done)
            })
        })
        .await?;
    match outcome {
        RoleChange::Done => {}
        RoleChange::Missing => return Err(ApiError::NotFound),
        RoleChange::Forbidden => return Err(forbidden()),
        RoleChange::OverLimit(limit) => return Err(plan_limit(limit, ("seat", "seats"))),
    }
    let member = find_member(&state, org_id, target).await?;
    Ok(error::Json(org_member(&member)))
}

async fn remove_org_member(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Extension(access): Extension<OrgAccess>,
    ctx: audit::Ctx,
    error::Path(UserPath { user: target }): error::Path<UserPath>,
) -> Result<StatusCode, ApiError> {
    let user = principal::session_user(&principal)?;
    let actor = user.id;
    let org_id = access.org.id;
    let now = state.clock.now_ns();
    let outcome = state
        .meta
        .write(move |conn| {
            Box::pin(async move {
                let Some(actor_role) = members::org_role_tx(conn, org_id, actor).await? else {
                    return Ok(MemberChange::Forbidden);
                };
                let Some(target_role) = members::org_role_tx(conn, org_id, target).await? else {
                    return Ok(MemberChange::Missing);
                };
                if actor != target && !may_remove_other(actor_role, target_role) {
                    return Ok(MemberChange::Forbidden);
                }
                members::remove_org_member(conn, org_id, target).await?;
                // A disabled owner does not count: the org must keep one who can act.
                if target_role == OrgRole::Owner
                    && members::count_active_owners(conn, org_id).await? == 0
                {
                    return Err(MetaError::Conflict("an org needs an owner"));
                }
                audit::record(
                    conn,
                    &ctx,
                    Event::new(MemberAction::Removed {
                        role: roles::org_role_str(target_role),
                    })
                    .org(org_id)
                    .target(TargetKind::User, target),
                    now,
                )
                .await?;
                Ok(MemberChange::Done)
            })
        })
        .await?;
    match outcome {
        MemberChange::Done => {}
        MemberChange::Missing => return Err(ApiError::NotFound),
        MemberChange::Forbidden => return Err(forbidden()),
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn list_project_members(
    State(state): State<AppState>,
    Extension(access): Extension<ProjectAccess>,
    error::Query(params): error::Query<PageParams>,
) -> Result<error::Json<Page<ProjectMember>>, ApiError> {
    let org_id = access.project.org_id;
    let project_id = access.project.id;
    let members = members::org_members(state.meta.reader(), org_id).await?;
    let overrides = members::project_overrides(state.meta.reader(), project_id).await?;
    let overrides: HashMap<Uuid, ProjectOverride> = overrides.into_iter().collect();
    let rows = members
        .iter()
        .map(|member| {
            (
                member.user.email.clone(),
                member.user.id.to_string(),
                project_member(member, overrides.get(&member.user.id).copied()),
            )
        })
        .collect();
    Ok(error::Json(page_named(
        rows,
        params.cursor.as_deref(),
        params.limit.get(),
    )?))
}

/// Nobody sets a role above their own, and only an owner changes an owner or grants `owner`.
fn may_set_role(actor: OrgRole, target: OrgRole, new_role: OrgRole) -> bool {
    if target == OrgRole::Owner && actor != OrgRole::Owner {
        return false;
    }
    if new_role == OrgRole::Owner && actor != OrgRole::Owner {
        return false;
    }
    roles::org_rank(new_role) <= roles::org_rank(actor)
}

/// Removing someone else needs `ManageMembers`. Only an owner removes an owner.
fn may_remove_other(actor: OrgRole, target: OrgRole) -> bool {
    if roles::org_rank(actor) < roles::org_rank(OrgRole::Admin) {
        return false;
    }
    target != OrgRole::Owner || actor == OrgRole::Owner
}

async fn find_member(
    state: &AppState,
    org_id: cetra_core::ids::OrgId,
    user: Uuid,
) -> Result<MemberRow, ApiError> {
    let members = members::org_members(state.meta.reader(), org_id).await?;
    members
        .into_iter()
        .find(|member| member.user.id == user)
        .ok_or(ApiError::NotFound)
}

fn org_member(member: &MemberRow) -> OrgMember {
    OrgMember {
        user: member_user(member),
        role: member.role,
        joined_at: member.joined_at,
    }
}

pub(crate) fn project_member(member: &MemberRow, stored: Option<ProjectOverride>) -> ProjectMember {
    let effective = roles::effective_project_role(member.role, stored).map(api_role);
    ProjectMember {
        user: member_user(member),
        org_role: member.role,
        role_override: stored.map(api_override),
        effective_role: effective,
    }
}

fn member_user(member: &MemberRow) -> MemberUser {
    MemberUser {
        id: member.user.id,
        email: member.user.email.clone(),
        name: member.user.name.clone(),
    }
}

fn api_role(role: Role) -> OrgRole {
    match role {
        Role::Admin => OrgRole::Admin,
        Role::Member => OrgRole::Member,
        Role::Viewer => OrgRole::Viewer,
    }
}

fn api_override(role: ProjectOverride) -> ProjectRoleOverride {
    match role {
        ProjectOverride::Admin => ProjectRoleOverride::Admin,
        ProjectOverride::Member => ProjectRoleOverride::Member,
        ProjectOverride::Viewer => ProjectRoleOverride::Viewer,
        ProjectOverride::None => ProjectRoleOverride::None,
    }
}

fn forbidden() -> ApiError {
    ApiError::Forbidden {
        code: ProblemCode::Forbidden,
    }
}

#[cfg(test)]
mod tests {
    use axum::http::{Method, header};
    use cetra_core::ids::OrgId;
    use serde_json::Value;

    use super::*;
    use crate::api::account::{OrgMembership, Project};
    use crate::api::common::ProblemCode;
    use crate::auth::session::start_session;
    use crate::meta::users::{NewUser, insert_user};
    use crate::test_support::{TestApp, capture_logs, set_limits};

    fn cookie_pair(set_cookie: &axum::http::HeaderValue) -> String {
        set_cookie
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned()
    }

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
                            email_verified_via: Some(
                                crate::meta::users::EmailVerifiedVia::MailToken,
                            ),
                            is_instance_admin: true,
                        },
                        now,
                    )
                    .await
                })
            })
            .await
            .unwrap();
        let cookie = start_session(
            &app.state,
            user.id,
            None,
            None,
            crate::auth::session::SessionOrigin::Password,
        )
        .await
        .unwrap();
        (user.id, cookie_pair(&cookie))
    }

    async fn add_member(app: &TestApp, org: OrgId, user: Uuid, role: OrgRole) {
        let now = app.state.clock.now_ns();
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move { members::add_org_member(conn, org, user, role, now).await })
            })
            .await
            .unwrap();
    }

    /// `(target_id, details)` of every audit row with `action`, oldest first.
    async fn audit_rows(app: &TestApp, action: &str) -> Vec<(Option<String>, serde_json::Value)> {
        let rows: Vec<(Option<String>, String)> =
            sqlx::query_as("SELECT target_id, details FROM audit_events WHERE action = ? ORDER BY seq")
                .bind(action)
                .fetch_all(app.state.meta.reader())
                .await
                .unwrap();
        rows.into_iter()
            .map(|(target, details)| (target, serde_json::from_str(&details).unwrap()))
            .collect()
    }

    async fn create_org(app: &TestApp, cookie: &str, name: &str) -> OrgMembership {
        let response = app
            .request(Method::POST, "/api/v1/orgs")
            .header(header::COOKIE.as_str(), cookie)
            .json(&serde_json::json!({ "name": name }))
            .send()
            .await;
        assert_eq!(response.status, 201);
        response.json()
    }

    async fn create_project(app: &TestApp, cookie: &str, org: &str, name: &str) -> Project {
        let response = app
            .request(Method::POST, &format!("/api/v1/orgs/{org}/projects"))
            .header(header::COOKIE.as_str(), cookie)
            .json(&serde_json::json!({ "name": name }))
            .send()
            .await;
        assert_eq!(response.status, 201);
        response.json()
    }

    #[test]
    fn member_writes_are_session_only() {
        for route in routes() {
            let session_only = match route.access {
                Access::Org { session_only, .. } | Access::Project { session_only, .. } => {
                    session_only
                }
                other => panic!("unexpected access {other:?} on {}", route.path),
            };
            let writes = matches!(route.method, Method::PATCH | Method::PUT | Method::DELETE);
            assert_eq!(session_only, writes, "{} {}", route.method, route.path);
        }
        assert!(routes().iter().any(|route| {
            route.path == "/api/v1/orgs/{org}/members/{user}"
                && route.method == Method::PATCH
                && matches!(
                    route.access,
                    Access::Org {
                        perm: OrgPerm::ManageMembers,
                        session_only: true,
                    }
                )
        }));
        assert!(routes().iter().any(|route| {
            route.path == "/api/v1/orgs/{org}/members/{user}"
                && route.method == Method::DELETE
                && matches!(
                    route.access,
                    Access::Org {
                        perm: OrgPerm::View,
                        session_only: true,
                    }
                )
        }));
    }

    #[tokio::test]
    async fn promoting_a_viewer_needs_a_seat() {
        let app = TestApp::new().await;
        let (_, ada) = signed_in(&app, "ada@example.com").await;
        let (viewer_id, _) = signed_in(&app, "viewer@example.com").await;
        let org = create_org(&app, &ada, "Acme").await;
        add_member(&app, org.id, viewer_id, OrgRole::Viewer).await;
        set_limits(
            &app,
            org.id,
            "2026-01-01T00:00:00Z",
            &serde_json::json!({"max_seats": 1}),
        );
        let path = format!("/api/v1/orgs/acme/members/{viewer_id}");

        let refused = app
            .request(Method::PATCH, &path)
            .header(header::COOKIE.as_str(), &ada)
            .json(&serde_json::json!({ "role": "member" }))
            .send()
            .await;
        assert_eq!(refused.status, 403);
        assert_eq!(refused.problem().code, ProblemCode::PlanLimit);
        assert_eq!(
            refused.problem().detail.as_deref(),
            Some("Your plan allows 1 seat.")
        );
        let still: OrgMember = app
            .request(Method::GET, "/api/v1/orgs/acme/members")
            .header(header::COOKIE.as_str(), &ada)
            .send()
            .await
            .json::<Page<OrgMember>>()
            .data
            .into_iter()
            .find(|member| member.user.id == viewer_id)
            .unwrap();
        assert_eq!(still.role, OrgRole::Viewer);

        set_limits(
            &app,
            org.id,
            "2026-01-01T00:01:00Z",
            &serde_json::json!({"max_seats": 2}),
        );
        let promoted = app
            .request(Method::PATCH, &path)
            .header(header::COOKIE.as_str(), &ada)
            .json(&serde_json::json!({ "role": "member" }))
            .send()
            .await;
        assert_eq!(
            promoted.status,
            200,
            "{}",
            String::from_utf8_lossy(&promoted.body)
        );
    }

    #[tokio::test]
    async fn roles_follow_the_ceiling_and_keep_an_owner() {
        let app = TestApp::new().await;
        let (ada_id, ada) = signed_in(&app, "ada@example.com").await;
        let (admin_id, admin) = signed_in(&app, "admin@example.com").await;
        let (member_id, _) = signed_in(&app, "member@example.com").await;
        let (second_id, _) = signed_in(&app, "second@example.com").await;
        let org = create_org(&app, &ada, "Acme").await;
        add_member(&app, org.id, admin_id, OrgRole::Admin).await;
        add_member(&app, org.id, member_id, OrgRole::Member).await;
        add_member(&app, org.id, second_id, OrgRole::Member).await;

        let (promoted, logs) = capture_logs(async {
            app.request(
                Method::PATCH,
                &format!("/api/v1/orgs/acme/members/{member_id}"),
            )
            .header(header::COOKIE.as_str(), &admin)
            .json(&serde_json::json!({ "role": "admin" }))
            .send()
            .await
        })
        .await;
        assert_eq!(promoted.status, 200);
        let promoted: OrgMember = promoted.json();
        assert_eq!(promoted.role, OrgRole::Admin);
        assert_eq!(promoted.user.email, "member@example.com");
        assert_eq!(
            audit_rows(&app, "member.role_changed").await,
            vec![(
                Some(member_id.to_string()),
                serde_json::json!({ "old_role": "member", "new_role": "admin" })
            )]
        );
        assert!(!logs.contains("member@example.com"));
        assert!(!logs.contains("admin@example.com"));
        let body: Value = serde_json::to_value(&promoted).unwrap();
        assert!(body["user"].get("is_instance_admin").is_none());

        let too_high = app
            .request(
                Method::PATCH,
                &format!("/api/v1/orgs/acme/members/{second_id}"),
            )
            .header(header::COOKIE.as_str(), &admin)
            .json(&serde_json::json!({ "role": "owner" }))
            .send()
            .await;
        assert_eq!(too_high.status, 403);
        assert!(too_high.problem().detail.is_none());

        let demote_owner = app
            .request(
                Method::PATCH,
                &format!("/api/v1/orgs/acme/members/{ada_id}"),
            )
            .header(header::COOKIE.as_str(), &admin)
            .json(&serde_json::json!({ "role": "member" }))
            .send()
            .await;
        assert_eq!(demote_owner.status, 403);

        let sole = app
            .request(
                Method::PATCH,
                &format!("/api/v1/orgs/acme/members/{ada_id}"),
            )
            .header(header::COOKIE.as_str(), &ada)
            .json(&serde_json::json!({ "role": "admin" }))
            .send()
            .await;
        assert_eq!(sole.status, 409);
        assert_eq!(
            sole.problem().detail.as_deref(),
            Some("an org needs an owner")
        );

        let granted = app
            .request(
                Method::PATCH,
                &format!("/api/v1/orgs/acme/members/{second_id}"),
            )
            .header(header::COOKIE.as_str(), &ada)
            .json(&serde_json::json!({ "role": "owner" }))
            .send()
            .await;
        assert_eq!(granted.status, 200);
        let granted: OrgMember = granted.json();
        assert_eq!(granted.role, OrgRole::Owner);

        let stepped_down = app
            .request(
                Method::PATCH,
                &format!("/api/v1/orgs/acme/members/{ada_id}"),
            )
            .header(header::COOKIE.as_str(), &ada)
            .json(&serde_json::json!({ "role": "admin" }))
            .send()
            .await;
        assert_eq!(stepped_down.status, 200);
        let stepped_down: OrgMember = stepped_down.json();
        assert_eq!(stepped_down.role, OrgRole::Admin);
    }

    #[tokio::test]
    async fn removal_allows_self_and_keeps_the_last_owner() {
        let app = TestApp::new().await;
        let (ada_id, ada) = signed_in(&app, "ada@example.com").await;
        let (member_id, member) = signed_in(&app, "member@example.com").await;
        let (viewer_id, viewer) = signed_in(&app, "viewer@example.com").await;
        let (other_id, other) = signed_in(&app, "other@example.com").await;
        let org = create_org(&app, &ada, "Acme").await;
        add_member(&app, org.id, member_id, OrgRole::Member).await;
        add_member(&app, org.id, viewer_id, OrgRole::Viewer).await;
        add_member(&app, org.id, other_id, OrgRole::Owner).await;

        let gone = app
            .request(
                Method::DELETE,
                &format!("/api/v1/orgs/acme/members/{viewer_id}"),
            )
            .header(header::COOKIE.as_str(), &viewer)
            .send()
            .await;
        assert_eq!(gone.status, 204);

        let denied = app
            .request(
                Method::DELETE,
                &format!("/api/v1/orgs/acme/members/{ada_id}"),
            )
            .header(header::COOKIE.as_str(), &member)
            .send()
            .await;
        assert_eq!(denied.status, 403);

        let last = app
            .request(
                Method::DELETE,
                &format!("/api/v1/orgs/acme/members/{ada_id}"),
            )
            .header(header::COOKIE.as_str(), &ada)
            .send()
            .await;
        // Another owner remains, so this succeeds. Then the remaining owner cannot leave.
        assert_eq!(last.status, 204);
        let stuck = app
            .request(
                Method::DELETE,
                &format!("/api/v1/orgs/acme/members/{other_id}"),
            )
            .header(header::COOKIE.as_str(), &other)
            .send()
            .await;
        assert_eq!(stuck.status, 409);
        assert_eq!(
            stuck.problem().detail.as_deref(),
            Some("an org needs an owner")
        );
    }

    #[tokio::test]
    async fn removal_is_recorded_with_the_role() {
        let app = TestApp::new().await;
        let (_, ada) = signed_in(&app, "ada@example.com").await;
        let (member_id, _) = signed_in(&app, "member@example.com").await;
        let org = create_org(&app, &ada, "Acme").await;
        add_member(&app, org.id, member_id, OrgRole::Member).await;

        let removed = app
            .request(
                Method::DELETE,
                &format!("/api/v1/orgs/acme/members/{member_id}"),
            )
            .header(header::COOKIE.as_str(), &ada)
            .send()
            .await;
        assert_eq!(removed.status, 204);
        assert_eq!(
            audit_rows(&app, "member.removed").await,
            vec![(
                Some(member_id.to_string()),
                serde_json::json!({ "role": "member" })
            )]
        );
    }

    #[tokio::test]
    async fn a_refused_last_owner_removal_records_nothing() {
        let app = TestApp::new().await;
        let (ada_id, ada) = signed_in(&app, "ada@example.com").await;
        create_org(&app, &ada, "Acme").await;

        let refused = app
            .request(
                Method::DELETE,
                &format!("/api/v1/orgs/acme/members/{ada_id}"),
            )
            .header(header::COOKIE.as_str(), &ada)
            .send()
            .await;
        assert_eq!(refused.status, 409);
        assert!(audit_rows(&app, "member.removed").await.is_empty());
    }

    /// Review 2026-11-15 #5: a disabled owner counted toward the last-owner rule, so the only
    /// active owner could leave or step down and no one could act for the org.
    #[tokio::test]
    async fn a_disabled_owner_does_not_keep_the_org_owned() {
        let app = TestApp::new().await;
        let (ada_id, ada) = signed_in(&app, "ada@example.com").await;
        let (other_id, _) = signed_in(&app, "other@example.com").await;
        let org = create_org(&app, &ada, "Acme").await;
        add_member(&app, org.id, other_id, OrgRole::Owner).await;
        let now = app.state.clock.now_ns();
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    crate::meta::users::set_disabled(conn, other_id, Some(now), now).await
                })
            })
            .await
            .unwrap();

        let demoted = app
            .request(
                Method::PATCH,
                &format!("/api/v1/orgs/acme/members/{ada_id}"),
            )
            .header(header::COOKIE.as_str(), &ada)
            .json(&serde_json::json!({ "role": "admin" }))
            .send()
            .await;
        assert_eq!(demoted.status, 409);
        let left = app
            .request(
                Method::DELETE,
                &format!("/api/v1/orgs/acme/members/{ada_id}"),
            )
            .header(header::COOKIE.as_str(), &ada)
            .send()
            .await;
        assert_eq!(left.status, 409);
        // The disabled owner can still be removed while an active one stays.
        let removed = app
            .request(
                Method::DELETE,
                &format!("/api/v1/orgs/acme/members/{other_id}"),
            )
            .header(header::COOKIE.as_str(), &ada)
            .send()
            .await;
        assert_eq!(removed.status, 204);
    }

    #[tokio::test]
    async fn promotion_to_admin_clears_overrides() {
        let app = TestApp::new().await;
        let (_, ada) = signed_in(&app, "ada@example.com").await;
        let (member_id, _) = signed_in(&app, "member@example.com").await;
        let org = create_org(&app, &ada, "Acme").await;
        add_member(&app, org.id, member_id, OrgRole::Member).await;
        let project = create_project(&app, &ada, "acme", "Support").await;
        let project_id = project.id;
        let user_id = member_id;
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    members::set_project_override(conn, project_id, user_id, ProjectOverride::None)
                        .await
                })
            })
            .await
            .unwrap();

        let promoted = app
            .request(
                Method::PATCH,
                &format!("/api/v1/orgs/acme/members/{member_id}"),
            )
            .header(header::COOKIE.as_str(), &ada)
            .json(&serde_json::json!({ "role": "admin" }))
            .send()
            .await;
        assert_eq!(promoted.status, 200);
        let stored = members::project_override(app.state.meta.reader(), project.id, member_id)
            .await
            .unwrap();
        assert!(stored.is_none());
    }

    /// Auth review #10: a read key, often a CI secret, never lists the org's people.
    #[tokio::test]
    async fn an_api_key_cannot_list_project_members() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let path = format!("/api/v1/projects/{}/members", seeded.project.id);
        for key in [&seeded.read_key, &seeded.ingest_key] {
            let response = app
                .request(Method::GET, &path)
                .header(header::AUTHORIZATION.as_str(), &format!("Bearer {key}"))
                .send()
                .await;
            assert_eq!(response.status, 403);
        }
        let current = app
            .request(Method::GET, "/api/v1/projects/current/members")
            .header(
                header::AUTHORIZATION.as_str(),
                &format!("Bearer {}", seeded.read_key),
            )
            .send()
            .await;
        assert_eq!(current.status, 403);
        let session = app
            .request(Method::GET, &path)
            .header(header::COOKIE.as_str(), &seeded.cookie)
            .send()
            .await;
        assert_eq!(session.status, 200);
    }
}
