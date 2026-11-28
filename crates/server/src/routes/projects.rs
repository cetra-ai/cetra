//! Project list, create, read, and update (spec 09).
#![cfg_attr(not(test), allow(dead_code))]

use axum::extract::{Extension, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, patch, post};
use cetra_core::ids::OrgId;
use uuid::Uuid;

use crate::analytics::actor::Actor;
use crate::analytics::catalog::Event as CatalogEvent;
use crate::analytics::record;
use crate::api::account::{CreateProjectRequest, OrgRole, Project, UpdateProjectRequest};
use crate::api::common::{FieldError, Page, PageParams};
use crate::audit::catalog::ProjectAction;
use crate::audit::{self, Event, TargetKind};
use crate::auth::principal::{self, Principal};
use crate::auth::roles::{self, OrgPerm, ProjectPerm, Role};
use crate::entitlements::{over_limit, plan_limit};
use crate::http::AppState;
use crate::http::access::{Access, OrgAccess, ProjectAccess, RouteDef};
use crate::http::error::{self, ApiError};
use crate::meta::members::project_override;
use crate::meta::orgs::{self, SlugError};
use crate::meta::projects::{
    self, COMPLETION_TIMEOUT_MAX_S, COMPLETION_TIMEOUT_MIN_S, ProjectPatch, ProjectRow,
};
use crate::meta::usage::genai_log_events;

use super::orgs::page_named;

/// Project routes. List and get were already in the OpenAPI contract.
pub(crate) fn routes() -> Vec<RouteDef> {
    vec![
        RouteDef {
            method: axum::http::Method::GET,
            path: "/api/v1/orgs/{org}/projects",
            access: Access::Org {
                perm: OrgPerm::View,
                session_only: false,
            },
            documented: true,
            handler: get(list_projects),
        },
        RouteDef {
            method: axum::http::Method::POST,
            path: "/api/v1/orgs/{org}/projects",
            access: Access::Org {
                perm: OrgPerm::CreateProject,
                session_only: false,
            },
            documented: true,
            handler: post(create_project),
        },
        RouteDef {
            method: axum::http::Method::GET,
            path: "/api/v1/projects/{project}",
            access: Access::Project {
                perm: ProjectPerm::View,
                session_only: false,
            },
            documented: true,
            handler: get(get_project),
        },
        RouteDef {
            method: axum::http::Method::PATCH,
            path: "/api/v1/projects/{project}",
            access: Access::Project {
                perm: ProjectPerm::ManageSettings,
                session_only: false,
            },
            documented: true,
            handler: patch(update_project),
        },
    ]
}

/// The org's active projects the caller can see, in `projects_of_org` order, each with the
/// caller's effective project role (OH-5). `GET /orgs/{org}/projects` and the activity endpoint
/// share it, so they see the same projects.
pub(crate) async fn visible_projects(
    state: &AppState,
    access: &OrgAccess,
    user_id: Uuid,
) -> Result<Vec<(ProjectRow, Role)>, ApiError> {
    let projects = projects::projects_of_org(state.meta.reader(), access.org.id).await?;
    let mut visible = Vec::new();
    for project in projects {
        if project.state != "active" {
            continue;
        }
        let ov = project_override(state.meta.reader(), project.id, user_id).await?;
        let Some(role) = roles::effective_project_role(access.role, ov) else {
            continue;
        };
        visible.push((project, role));
    }
    Ok(visible)
}

async fn list_projects(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Extension(access): Extension<OrgAccess>,
    error::Query(params): error::Query<PageParams>,
) -> Result<error::Json<Page<Project>>, ApiError> {
    let user = principal::session_user(&principal)?;
    let visible: Vec<_> = visible_projects(&state, &access, user.id)
        .await?
        .into_iter()
        .map(|(project, role)| {
            (
                project.name.clone(),
                project.id.to_string(),
                project_json(&project, Some(role)),
            )
        })
        .collect();
    Ok(error::Json(page_named(
        visible,
        params.cursor.as_deref(),
        params.limit.get(),
    )?))
}

enum ProjectCreate {
    Created(ProjectRow),
    /// The org's project limit, which is what the caller hit.
    OverLimit(u64),
}

async fn create_project(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Extension(access): Extension<OrgAccess>,
    ctx: audit::Ctx,
    error::Json(body): error::Json<CreateProjectRequest>,
) -> Result<Response, ApiError> {
    let name = trimmed_name(&body.name)?;
    if let Some(slug) = &body.slug {
        orgs::validate_project_slug(slug).map_err(slug_error)?;
    }
    let retention = body.retention_days.map(checked_retention).transpose()?;
    if retention.is_some() && !may_manage_retention(&state, access.org.id) {
        return Err(ApiError::PlanRequired {
            detail: RETENTION_PLAN,
        });
    }
    let org_id = access.org.id;
    let slug = body.slug;
    let now = state.clock.now_ns();
    let max_projects = state.entitlements.resolve(org_id, now).limits.max_projects;
    let created = state
        .meta
        .write(move |conn| {
            Box::pin(async move {
                if let Some(limit) = max_projects {
                    let active = projects::active_projects_tx(conn, org_id).await?;
                    if over_limit(Some(limit), active, 1) {
                        return Ok(ProjectCreate::OverLimit(limit));
                    }
                }
                let project = match &slug {
                    Some(slug) => {
                        projects::insert_project(conn, org_id, &name, slug, retention, now).await
                    }
                    None => {
                        projects::insert_project_with_derived_slug(
                            conn, org_id, &name, retention, now,
                        )
                        .await
                    }
                }?;
                audit::record(
                    conn,
                    &ctx,
                    Event::new(ProjectAction::Created)
                        .org(org_id)
                        .project(project.id)
                        .target(TargetKind::Project, project.id),
                    now,
                )
                .await?;
                Ok(ProjectCreate::Created(project))
            })
        })
        .await?;
    let project = match created {
        ProjectCreate::Created(project) => project,
        ProjectCreate::OverLimit(limit) => {
            return Err(plan_limit(limit, ("project", "projects")));
        }
    };
    // CreateProject requires an org admin, whose effective project role is admin.
    record(
        &state,
        &CatalogEvent::ProjectCreated,
        &Actor::org(&principal, &access),
    );
    Ok((
        StatusCode::CREATED,
        error::Json(project_json(&project, Some(Role::Admin))),
    )
        .into_response())
}

async fn get_project(
    State(state): State<AppState>,
    Extension(access): Extension<ProjectAccess>,
) -> Result<error::Json<Project>, ApiError> {
    Ok(error::Json(project_detail(&state, &access).await?))
}

/// The project's settings: the body of `GET /projects/{project}`. The assistant's `get_project_settings`
/// tool shares it (AS M2).
pub(crate) async fn project_detail(
    state: &AppState,
    access: &ProjectAccess,
) -> Result<Project, ApiError> {
    let mut project = project_json(&access.project, access.role);
    project.genai_log_events =
        Some(genai_log_events(state.meta.reader(), access.project.id).await?);
    project.last_ingest_at = state.last_ingest.get(access.project.id);
    Ok(project)
}

async fn update_project(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Extension(access): Extension<ProjectAccess>,
    ctx: audit::Ctx,
    error::Json(body): error::Json<UpdateProjectRequest>,
) -> Result<error::Json<Project>, ApiError> {
    principal::session_user(&principal)?;
    let name = match body.name {
        None => None,
        Some(name) => Some(trimmed_name(&name)?),
    };
    let retention_days = match body.retention_days {
        None => None,
        Some(None) => Some(None),
        Some(Some(days)) => Some(Some(checked_retention(days)?)),
    };
    if retention_needs_entitlement(retention_days, access.project.retention_days)
        && !may_manage_retention(&state, access.project.org_id)
    {
        return Err(ApiError::PlanRequired {
            detail: RETENTION_PLAN,
        });
    }
    let timeout = body
        .trace_completion_timeout_s
        .map(checked_timeout)
        .transpose()?;
    let org_id = access.project.org_id;
    let id = access.project.id;
    let role = access.role;
    let now = state.clock.now_ns();
    state
        .meta
        .write(move |conn| {
            Box::pin(async move {
                // The old values are read in this write transaction, so each record states the
                // change this request made. The timeout is stored in `settings`, read as
                // `project_by_id` reads it.
                let old: Option<(String, Option<i64>, Option<i64>)> = sqlx::query_as(
                    "SELECT name, retention_days, \
                     CAST(CASE WHEN json_valid(settings) AND json_type(settings) = 'object' \
                     THEN json_extract(settings, '$.trace_completion_timeout_s') END AS INTEGER) \
                     FROM projects WHERE id = ?",
                )
                .bind(id.to_string())
                .fetch_optional(&mut *conn)
                .await
                .map_err(crate::meta::MetaError::from)?;
                // No row: the update would change nothing, so there is nothing to record.
                let Some((old_name, old_retention, old_timeout)) = old else {
                    return Ok(());
                };
                let old_retention = old_retention.and_then(|days| u32::try_from(days).ok());
                let old_timeout = old_timeout
                    .and_then(|seconds| u32::try_from(seconds).ok())
                    .filter(|seconds| {
                        (COMPLETION_TIMEOUT_MIN_S..=COMPLETION_TIMEOUT_MAX_S).contains(seconds)
                    });
                // The old and new value of each setting the request changes (retention is
                // `Option<Option<u32>>`, outer `None` = unchanged).
                let name_changed = name.as_ref().is_some_and(|new| *new != old_name);
                let retention_change = match retention_days {
                    Some(new) if new != old_retention => Some((old_retention, new)),
                    _ => None,
                };
                let timeout_change = timeout
                    .filter(|new| Some(*new) != old_timeout)
                    .map(|new| (old_timeout, Some(new)));
                projects::update_project(
                    conn,
                    id,
                    ProjectPatch {
                        name,
                        retention_days,
                        trace_completion_timeout_s: timeout,
                    },
                    now,
                )
                .await?;
                if name_changed {
                    audit::record(
                        conn,
                        &ctx,
                        Event::new(ProjectAction::Renamed)
                            .org(org_id)
                            .project(id)
                            .target(TargetKind::Project, id),
                        now,
                    )
                    .await?;
                }
                if let Some((old_days, new_days)) = retention_change {
                    audit::record(
                        conn,
                        &ctx,
                        Event::new(ProjectAction::RetentionChanged { old_days, new_days })
                            .org(org_id)
                            .project(id)
                            .target(TargetKind::Project, id),
                        now,
                    )
                    .await?;
                }
                if let Some((old_seconds, new_seconds)) = timeout_change {
                    audit::record(
                        conn,
                        &ctx,
                        Event::new(ProjectAction::CompletionTimeoutChanged {
                            old_seconds,
                            new_seconds,
                        })
                        .org(org_id)
                        .project(id)
                        .target(TargetKind::Project, id),
                        now,
                    )
                    .await?;
                }
                Ok(())
            })
        })
        .await?;
    if let Some(Some(days)) = retention_days {
        crate::retention::apply_now(&state, id, days).await;
    }
    let Some(project) = projects::project_by_id(state.meta.reader(), id).await? else {
        return Err(ApiError::NotFound);
    };
    let mut json = project_json(&project, role);
    json.genai_log_events = Some(genai_log_events(state.meta.reader(), id).await?);
    Ok(error::Json(json))
}

fn project_json(project: &ProjectRow, role: Option<Role>) -> Project {
    Project {
        id: project.id,
        org_id: project.org_id,
        slug: project.slug.clone(),
        name: project.name.clone(),
        retention_days: project.retention_days,
        trace_completion_timeout_s: project.completion_timeout_s(),
        genai_log_events: None,
        last_ingest_at: None,
        role: role.map(api_role),
        created_at: project.created_at,
    }
}

fn api_role(role: Role) -> OrgRole {
    match role {
        Role::Admin => OrgRole::Admin,
        Role::Member => OrgRole::Member,
        Role::Viewer => OrgRole::Viewer,
    }
}

/// Whether the org may set project retention (D177, D-P-12). Free when self-hosted; on a
/// cloud org it follows the `retention_management` feature of its plan.
fn may_manage_retention(state: &AppState, org: OrgId) -> bool {
    crate::entitlements::of(state, org).allows(crate::entitlements::Feature::RetentionManagement)
}

const RETENTION_PLAN: &str = "data retention management is part of the Pro plan";

/// Whether a `PATCH` changes retention to a set value, which needs the entitlement (D177).
/// Clearing it (`Some(None)`), leaving it alone (`None`) and sending the stored value again
/// do not: a rename that echoes the form's value must not be refused.
fn retention_needs_entitlement(value: Option<Option<u32>>, stored: Option<u32>) -> bool {
    matches!(value, Some(Some(days)) if Some(days) != stored)
}

fn checked_retention(days: u32) -> Result<u32, ApiError> {
    if (1..=36_500).contains(&days) {
        Ok(days)
    } else {
        Err(field_error("retention_days", "must be 1 to 36500"))
    }
}

fn checked_timeout(seconds: u32) -> Result<u32, ApiError> {
    if (COMPLETION_TIMEOUT_MIN_S..=COMPLETION_TIMEOUT_MAX_S).contains(&seconds) {
        Ok(seconds)
    } else {
        Err(field_error(
            "trace_completion_timeout_s",
            "must be 10 to 3600",
        ))
    }
}

fn slug_error(error: SlugError) -> ApiError {
    field_error("slug", &error.to_string())
}

fn trimmed_name(value: &str) -> Result<String, ApiError> {
    let name = value.trim();
    let length = name.chars().count();
    if !(1..=100).contains(&length) {
        return Err(field_error("name", "must be 1 to 100 characters"));
    }
    Ok(name.to_owned())
}

fn field_error(field: &str, message: &str) -> ApiError {
    ApiError::BadRequest {
        detail: None,
        errors: vec![FieldError {
            field: field.to_owned(),
            message: message.to_owned(),
            position: None,
        }],
    }
}

#[cfg(test)]
mod tests {
    use axum::http::{Method, header};
    use uuid::Uuid;

    use super::*;
    use crate::api::account::OrgMembership;
    use crate::api::common::ProblemCode;
    use crate::auth::roles::ProjectOverride;
    use crate::auth::session::start_session;
    use crate::meta::members::{add_org_member, set_project_override};
    use crate::meta::users::{NewUser, insert_user};
    use crate::test_support::{Seeded, TestApp, dataset_span, set_limits};

    /// `(target_id, details)` of every audit row with `action`, oldest first.
    async fn audit_rows(app: &TestApp, action: &str) -> Vec<(Option<String>, serde_json::Value)> {
        let rows: Vec<(Option<String>, String)> = sqlx::query_as(
            "SELECT target_id, details FROM audit_events WHERE action = ? ORDER BY seq",
        )
        .bind(action)
        .fetch_all(app.state.meta.reader())
        .await
        .unwrap();
        rows.into_iter()
            .map(|(target, details)| (target, serde_json::from_str(&details).unwrap()))
            .collect()
    }

    #[test]
    fn only_a_set_retention_needs_the_entitlement() {
        assert!(retention_needs_entitlement(Some(Some(7)), None));
        assert!(retention_needs_entitlement(Some(Some(7)), Some(30)));
        assert!(!retention_needs_entitlement(Some(None), Some(30)));
        assert!(!retention_needs_entitlement(None, Some(30)));
    }

    #[test]
    fn the_stored_retention_needs_no_entitlement() {
        assert!(!retention_needs_entitlement(Some(Some(7)), Some(7)));
    }

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
                            is_instance_admin: false,
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
                Box::pin(async move { add_org_member(conn, org, user, role, now).await })
            })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn create_reserves_settings_and_takes_null_retention() {
        let app = TestApp::new().await;
        let (_, ada) = signed_in(&app, "ada@example.com").await;
        let org = app
            .request(Method::POST, "/api/v1/orgs")
            .header(header::COOKIE.as_str(), &ada)
            .json(&serde_json::json!({ "name": "Acme", "slug": "acme" }))
            .send()
            .await;
        assert_eq!(org.status, 201);
        let create = |body: serde_json::Value| {
            app.request(Method::POST, "/api/v1/orgs/acme/projects")
                .header(header::COOKIE.as_str(), &ada)
                .json(&body)
                .send()
        };

        let reserved = create(serde_json::json!({ "name": "Settings", "slug": "settings" })).await;
        assert_eq!(reserved.status, 400);
        let problem = reserved.problem();
        assert_eq!(problem.errors[0].field, "slug");

        let derived = create(serde_json::json!({ "name": "Settings" })).await;
        assert_eq!(derived.status, 201);
        let derived: Project = derived.json();
        assert_eq!(derived.slug, "settings-1");

        let forever = create(serde_json::json!({
            "name": "Keeper",
            "slug": "keeper",
            "retention_days": null
        }))
        .await;
        assert_eq!(forever.status, 201);
        let forever: Project = forever.json();
        assert!(forever.retention_days.is_none());

        let zero = create(serde_json::json!({
            "name": "Zero",
            "slug": "zero",
            "retention_days": 0
        }))
        .await;
        assert_eq!(zero.status, 400);
    }

    #[tokio::test]
    async fn projects_are_limited() {
        let app = TestApp::new().await;
        let (_, ada) = signed_in(&app, "ada@example.com").await;
        let org: OrgMembership = app
            .request(Method::POST, "/api/v1/orgs")
            .header(header::COOKIE.as_str(), &ada)
            .json(&serde_json::json!({ "name": "Acme", "slug": "acme" }))
            .send()
            .await
            .json();
        let create = |name: &'static str| {
            app.request(Method::POST, "/api/v1/orgs/acme/projects")
                .header(header::COOKIE.as_str(), &ada)
                .json(&serde_json::json!({ "name": name }))
                .send()
        };
        assert_eq!(create("one").await.status, 201);

        set_limits(
            &app,
            org.id,
            "2026-01-01T00:00:00Z",
            &serde_json::json!({"max_projects": 1}),
        );
        let refused = create("two").await;
        assert_eq!(refused.status, 403);
        assert_eq!(refused.problem().code, ProblemCode::PlanLimit);
        assert_eq!(
            refused.problem().detail.as_deref(),
            Some("Your plan allows 1 project.")
        );

        set_limits(
            &app,
            org.id,
            "2026-01-01T00:01:00Z",
            &serde_json::json!({"max_projects": 2}),
        );
        assert_eq!(create("two").await.status, 201);
    }

    #[tokio::test]
    async fn projects_hide_none_and_report_effective_roles() {
        let app = TestApp::new().await;
        let (_, ada) = signed_in(&app, "ada@example.com").await;
        let (admin_id, admin) = signed_in(&app, "admin@example.com").await;
        let (member_id, member) = signed_in(&app, "member@example.com").await;
        let (viewer_id, viewer) = signed_in(&app, "viewer@example.com").await;
        let (_, bob) = signed_in(&app, "bob@example.com").await;

        let org = app
            .request(Method::POST, "/api/v1/orgs")
            .header(header::COOKIE.as_str(), &ada)
            .json(&serde_json::json!({ "name": "Acme", "slug": "acme" }))
            .send()
            .await;
        assert_eq!(org.status, 201);
        let org: OrgMembership = org.json();
        add_member(&app, org.id, admin_id, OrgRole::Admin).await;
        add_member(&app, org.id, member_id, OrgRole::Member).await;
        add_member(&app, org.id, viewer_id, OrgRole::Viewer).await;

        let denied = app
            .request(Method::POST, "/api/v1/orgs/acme/projects")
            .header(header::COOKIE.as_str(), &viewer)
            .json(&serde_json::json!({ "name": "Nope" }))
            .send()
            .await;
        assert_eq!(denied.status, 403);
        assert_eq!(denied.problem().code, ProblemCode::Forbidden);
        assert!(denied.problem().detail.is_none());

        let created = app
            .request(Method::POST, "/api/v1/orgs/acme/projects")
            .header(header::COOKIE.as_str(), &admin)
            .json(&serde_json::json!({ "name": "Support bot", "slug": "support-bot" }))
            .send()
            .await;
        assert_eq!(created.status, 201);
        let created: Project = created.json();
        assert_eq!(created.slug, "support-bot");
        assert_eq!(created.role, Some(OrgRole::Admin));
        assert!(created.retention_days.is_none());

        let hidden = app
            .request(Method::POST, "/api/v1/orgs/acme/projects")
            .header(header::COOKIE.as_str(), &ada)
            .json(&serde_json::json!({ "name": "Hidden" }))
            .send()
            .await;
        let hidden: Project = hidden.json();
        let hidden_id = hidden.id;
        let member_id_override = member_id;
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    set_project_override(conn, hidden_id, member_id_override, ProjectOverride::None)
                        .await
                })
            })
            .await
            .unwrap();

        let members = app
            .request(Method::GET, "/api/v1/orgs/acme/projects")
            .header(header::COOKIE.as_str(), &member)
            .send()
            .await;
        let members: Page<Project> = members.json();
        assert_eq!(members.data.len(), 1);
        assert_eq!(members.data[0].slug, "support-bot");
        assert_eq!(members.data[0].role, Some(OrgRole::Member));

        let viewers = app
            .request(Method::GET, "/api/v1/orgs/acme/projects")
            .header(header::COOKIE.as_str(), &viewer)
            .send()
            .await;
        let viewers: Page<Project> = viewers.json();
        assert_eq!(viewers.data.len(), 2);
        assert!(
            viewers
                .data
                .iter()
                .all(|project| project.role == Some(OrgRole::Viewer))
        );

        let owners = app
            .request(Method::GET, "/api/v1/orgs/acme/projects")
            .header(header::COOKIE.as_str(), &ada)
            .send()
            .await;
        let owners: Page<Project> = owners.json();
        assert_eq!(owners.data.len(), 2);
        assert!(
            owners
                .data
                .iter()
                .all(|project| project.role == Some(OrgRole::Admin))
        );

        let by_id = app
            .request(Method::GET, &format!("/api/v1/projects/{}", created.id))
            .header(header::COOKIE.as_str(), &ada)
            .send()
            .await;
        assert_eq!(by_id.status, 200);
        let by_id: Project = by_id.json();
        assert_eq!(by_id.slug, "support-bot");

        let by_slug = app
            .request(Method::GET, "/api/v1/projects/acme%2Fsupport-bot")
            .header(header::COOKIE.as_str(), &ada)
            .send()
            .await;
        assert_eq!(by_slug.status, 200);
        let by_slug: Project = by_slug.json();
        assert_eq!(by_slug.id, created.id);

        let hidden_get = app
            .request(Method::GET, &format!("/api/v1/projects/{hidden_id}"))
            .header(header::COOKIE.as_str(), &member)
            .send()
            .await;
        assert_eq!(hidden_get.status, 404);

        let other = app
            .request(Method::POST, "/api/v1/orgs")
            .header(header::COOKIE.as_str(), &bob)
            .json(&serde_json::json!({ "name": "Other" }))
            .send()
            .await;
        let other: OrgMembership = other.json();
        let foreign = app
            .request(
                Method::POST,
                &format!("/api/v1/orgs/{}/projects", other.slug),
            )
            .header(header::COOKIE.as_str(), &bob)
            .json(&serde_json::json!({ "name": "Foreign" }))
            .send()
            .await;
        let foreign: Project = foreign.json();
        let missing = app
            .request(Method::GET, &format!("/api/v1/projects/{}", foreign.id))
            .header(header::COOKIE.as_str(), &ada)
            .send()
            .await;
        assert_eq!(missing.status, 404);
    }

    #[tokio::test]
    async fn patch_retention_sets_clears_and_rejects_the_range() {
        let app = TestApp::new().await;
        let (_, ada) = signed_in(&app, "ada@example.com").await;
        let org = app
            .request(Method::POST, "/api/v1/orgs")
            .header(header::COOKIE.as_str(), &ada)
            .json(&serde_json::json!({ "name": "Acme", "slug": "acme" }))
            .send()
            .await;
        assert_eq!(org.status, 201);
        let created = app
            .request(Method::POST, "/api/v1/orgs/acme/projects")
            .header(header::COOKIE.as_str(), &ada)
            .json(&serde_json::json!({ "name": "Support bot", "slug": "support-bot" }))
            .send()
            .await;
        let created: Project = created.json();
        assert!(created.retention_days.is_none());
        let path = format!("/api/v1/projects/{}", created.id);

        let set = app
            .request(Method::PATCH, &path)
            .header(header::COOKIE.as_str(), &ada)
            .json(&serde_json::json!({ "retention_days": 30 }))
            .send()
            .await;
        assert_eq!(set.status, 200);
        let set: Project = set.json();
        assert_eq!(set.retention_days, Some(30));

        let absent = app
            .request(Method::PATCH, &path)
            .header(header::COOKIE.as_str(), &ada)
            .json(&serde_json::json!({}))
            .send()
            .await;
        assert_eq!(absent.status, 200);
        let absent: Project = absent.json();
        assert_eq!(absent.retention_days, Some(30));

        let cleared = app
            .request(Method::PATCH, &path)
            .header(header::COOKIE.as_str(), &ada)
            .json(&serde_json::json!({ "retention_days": null }))
            .send()
            .await;
        assert_eq!(cleared.status, 200);
        let cleared: Project = cleared.json();
        assert!(cleared.retention_days.is_none());

        for days in [0, 36_501] {
            let rejected = app
                .request(Method::PATCH, &path)
                .header(header::COOKIE.as_str(), &ada)
                .json(&serde_json::json!({ "retention_days": days }))
                .send()
                .await;
            assert_eq!(rejected.status, 400);
            assert_eq!(rejected.problem().errors[0].field, "retention_days");
            assert_eq!(rejected.problem().errors[0].message, "must be 1 to 36500");
        }
        let still = app
            .request(Method::GET, &path)
            .header(header::COOKIE.as_str(), &ada)
            .send()
            .await;
        let still: Project = still.json();
        assert!(still.retention_days.is_none());
    }

    /// Creates org `acme` and project `support-bot` as the caller, and returns the project.
    async fn support_bot(app: &TestApp, cookie: &str) -> Project {
        let org = app
            .request(Method::POST, "/api/v1/orgs")
            .header(header::COOKIE.as_str(), cookie)
            .json(&serde_json::json!({ "name": "Acme", "slug": "acme" }))
            .send()
            .await;
        assert_eq!(org.status, 201);
        let created = app
            .request(Method::POST, "/api/v1/orgs/acme/projects")
            .header(header::COOKIE.as_str(), cookie)
            .json(&serde_json::json!({ "name": "Support bot", "slug": "support-bot" }))
            .send()
            .await;
        assert_eq!(created.status, 201);
        created.json()
    }

    /// Writes `projects.settings` of a project by SQL.
    async fn set_settings(app: &TestApp, project: &Project, settings: &'static str) {
        let id = project.id.to_string();
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    sqlx::query("UPDATE projects SET settings = ? WHERE id = ?")
                        .bind(settings)
                        .bind(id)
                        .execute(&mut *conn)
                        .await?;
                    Ok(())
                })
            })
            .await
            .unwrap();
    }

    /// `projects.settings` of a project, read by SQL.
    async fn stored_settings(app: &TestApp, project: &Project) -> serde_json::Value {
        let text: String = sqlx::query_scalar("SELECT settings FROM projects WHERE id = ?")
            .bind(project.id.to_string())
            .fetch_one(app.state.meta.reader())
            .await
            .unwrap();
        serde_json::from_str(&text).unwrap()
    }

    #[tokio::test]
    async fn a_new_project_has_the_default_timeout() {
        let app = TestApp::new().await;
        let (_, ada) = signed_in(&app, "ada@example.com").await;
        let created = support_bot(&app, &ada).await;
        assert_eq!(created.trace_completion_timeout_s, 60);
        let path = format!("/api/v1/projects/{}", created.id);
        let fetched = app
            .request(Method::GET, &path)
            .header(header::COOKIE.as_str(), &ada)
            .send()
            .await;
        let fetched: Project = fetched.json();
        assert_eq!(fetched.trace_completion_timeout_s, 60);
    }

    #[tokio::test]
    async fn a_project_reports_the_genai_log_events_it_dropped() {
        let app = TestApp::new().await;
        let (_, ada) = signed_in(&app, "ada@example.com").await;
        let created = support_bot(&app, &ada).await;
        assert_eq!(created.genai_log_events, None, "not in a create answer");
        let path = format!("/api/v1/projects/{}", created.id);
        let read = |method| {
            app.request(method, &path)
                .header(header::COOKIE.as_str(), &ada)
        };
        let fetched: Project = read(Method::GET).send().await.json();
        assert_eq!(fetched.genai_log_events, Some(0));

        let org = created.org_id;
        app.state
            .usage
            .add_log_events(org, created.id, app.state.clock.now_ns(), 5);
        app.state.usage.flush(&app.state.meta).await.unwrap();
        let fetched: Project = read(Method::GET).send().await.json();
        assert_eq!(fetched.genai_log_events, Some(5));
        let patched: Project = read(Method::PATCH)
            .json(&serde_json::json!({ "name": "Renamed" }))
            .send()
            .await
            .json();
        assert_eq!(patched.genai_log_events, Some(5));
    }

    #[tokio::test]
    async fn patch_sets_the_timeout() {
        let app = TestApp::new().await;
        let (_, ada) = signed_in(&app, "ada@example.com").await;
        let created = support_bot(&app, &ada).await;
        let path = format!("/api/v1/projects/{}", created.id);
        let patched = app
            .request(Method::PATCH, &path)
            .header(header::COOKIE.as_str(), &ada)
            .json(&serde_json::json!({ "trace_completion_timeout_s": 120 }))
            .send()
            .await;
        assert_eq!(patched.status, 200);
        let patched: Project = patched.json();
        assert_eq!(patched.trace_completion_timeout_s, 120);
        assert_eq!(patched.name, "Support bot");
        assert!(patched.retention_days.is_none());

        let fetched = app
            .request(Method::GET, &path)
            .header(header::COOKIE.as_str(), &ada)
            .send()
            .await;
        let fetched: Project = fetched.json();
        assert_eq!(fetched.trace_completion_timeout_s, 120);
        assert_eq!(
            stored_settings(&app, &created).await,
            serde_json::json!({ "trace_completion_timeout_s": 120 })
        );
    }

    #[tokio::test]
    async fn patch_refuses_an_out_of_range_timeout() {
        let app = TestApp::new().await;
        let (_, ada) = signed_in(&app, "ada@example.com").await;
        let created = support_bot(&app, &ada).await;
        let path = format!("/api/v1/projects/{}", created.id);
        for seconds in [9, 3601] {
            let rejected = app
                .request(Method::PATCH, &path)
                .header(header::COOKIE.as_str(), &ada)
                .json(&serde_json::json!({ "trace_completion_timeout_s": seconds }))
                .send()
                .await;
            assert_eq!(rejected.status, 400);
            assert_eq!(
                rejected.problem().errors[0].field,
                "trace_completion_timeout_s"
            );
            assert_eq!(rejected.problem().errors[0].message, "must be 10 to 3600");
        }
        assert_eq!(stored_settings(&app, &created).await, serde_json::json!({}));
    }

    #[tokio::test]
    async fn project_create_is_recorded() {
        let app = TestApp::new().await;
        let (_, ada) = signed_in(&app, "ada@example.com").await;
        app.request(Method::POST, "/api/v1/orgs")
            .header(header::COOKIE.as_str(), &ada)
            .json(&serde_json::json!({ "name": "Acme", "slug": "acme" }))
            .send()
            .await;
        let created: Project = app
            .request(Method::POST, "/api/v1/orgs/acme/projects")
            .header(header::COOKIE.as_str(), &ada)
            .json(&serde_json::json!({ "name": "Support bot" }))
            .send()
            .await
            .json();

        let rows = audit_rows(&app, "project.created").await;
        assert_eq!(
            rows,
            vec![(Some(created.id.to_string()), serde_json::json!({}))]
        );
    }

    #[tokio::test]
    async fn an_over_limit_create_records_nothing() {
        let app = TestApp::new().await;
        let (_, ada) = signed_in(&app, "ada@example.com").await;
        let org: OrgMembership = app
            .request(Method::POST, "/api/v1/orgs")
            .header(header::COOKIE.as_str(), &ada)
            .json(&serde_json::json!({ "name": "Acme", "slug": "acme" }))
            .send()
            .await
            .json();
        let create = |name: &'static str| {
            app.request(Method::POST, "/api/v1/orgs/acme/projects")
                .header(header::COOKIE.as_str(), &ada)
                .json(&serde_json::json!({ "name": name }))
                .send()
        };
        assert_eq!(create("one").await.status, 201);
        set_limits(
            &app,
            org.id,
            "2026-01-01T00:00:00Z",
            &serde_json::json!({"max_projects": 1}),
        );
        assert_eq!(create("two").await.status, 403);

        assert_eq!(audit_rows(&app, "project.created").await.len(), 1);
    }

    #[tokio::test]
    async fn a_timeout_change_is_audited() {
        let app = TestApp::new().await;
        let (_, ada) = signed_in(&app, "ada@example.com").await;
        let created = support_bot(&app, &ada).await;
        let path = format!("/api/v1/projects/{}", created.id);

        let status = app
            .request(Method::PATCH, &path)
            .header(header::COOKIE.as_str(), &ada)
            .json(&serde_json::json!({ "trace_completion_timeout_s": 120 }))
            .send()
            .await
            .status;
        assert_eq!(status, 200);
        assert_eq!(
            audit_rows(&app, "project.completion_timeout_changed").await,
            vec![(
                Some(created.id.to_string()),
                serde_json::json!({ "old_seconds": null, "new_seconds": 120 })
            )]
        );

        let status = app
            .request(Method::PATCH, &path)
            .header(header::COOKIE.as_str(), &ada)
            .json(&serde_json::json!({ "name": "Renamed" }))
            .send()
            .await
            .status;
        assert_eq!(status, 200);
        assert_eq!(audit_rows(&app, "project.renamed").await.len(), 1);
        assert_eq!(
            audit_rows(&app, "project.completion_timeout_changed")
                .await
                .len(),
            1
        );
        assert!(
            audit_rows(&app, "project.retention_changed")
                .await
                .is_empty()
        );
    }

    #[tokio::test]
    async fn patch_keeps_other_settings_keys() {
        let app = TestApp::new().await;
        let (_, ada) = signed_in(&app, "ada@example.com").await;
        let created = support_bot(&app, &ada).await;
        set_settings(&app, &created, r#"{"other": 1}"#).await;
        let path = format!("/api/v1/projects/{}", created.id);
        let patched = app
            .request(Method::PATCH, &path)
            .header(header::COOKIE.as_str(), &ada)
            .json(&serde_json::json!({ "trace_completion_timeout_s": 30 }))
            .send()
            .await;
        assert_eq!(patched.status, 200);
        assert_eq!(
            stored_settings(&app, &created).await,
            serde_json::json!({ "other": 1, "trace_completion_timeout_s": 30 })
        );
    }

    #[tokio::test]
    async fn a_garbage_settings_value_reads_as_the_default() {
        let app = TestApp::new().await;
        let (_, ada) = signed_in(&app, "ada@example.com").await;
        let created = support_bot(&app, &ada).await;
        set_settings(&app, &created, r#"{"trace_completion_timeout_s": 99999}"#).await;
        let path = format!("/api/v1/projects/{}", created.id);
        let fetched = app
            .request(Method::GET, &path)
            .header(header::COOKIE.as_str(), &ada)
            .send()
            .await;
        let fetched: Project = fetched.json();
        assert_eq!(fetched.trace_completion_timeout_s, 60);
    }

    /// 2026-01-01T00:00:00Z, the manual clock's start.
    const NOW: i64 = 1_767_225_600_000_000_000;
    const DAY: i64 = 86_400 * 1_000_000_000;
    const HOUR: i64 = 3_600 * 1_000_000_000;
    const TRACE_A: &str = "a4000000000000000000000000000001";

    /// Commits one span of `trace` to `project`, starting at `start_time`.
    async fn commit_at(app: &TestApp, project: &ProjectRow, trace: &str, start_time: i64) {
        let mut row = dataset_span(0);
        row.trace_id = trace.parse::<cetra_core::ids::TraceId>().unwrap();
        row.span_id = "b4000000000000aa"
            .parse::<cetra_core::ids::SpanId>()
            .unwrap();
        row.parent_span_id = None;
        row.start_time = start_time;
        row.end_time = start_time + 1_000;
        row.ingest_time = app.state.clock.now_ns();
        app.commit_spans(project, vec![row]).await;
    }

    async fn trace_status(app: &TestApp, seeded: &Seeded, trace: &str) -> StatusCode {
        app.request(
            Method::GET,
            &format!("/api/v1/projects/{}/traces/{trace}", seeded.project.id),
        )
        .header(header::COOKIE.as_str(), &seeded.cookie)
        .send()
        .await
        .status
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn patch_applies_retention_at_once() {
        let app = TestApp::new().await;
        app.state.clock.set(NOW);
        let seeded = app.seed().await;
        commit_at(&app, &seeded.project, TRACE_A, NOW - 3 * DAY + 12 * HOUR).await;
        assert_eq!(trace_status(&app, &seeded, TRACE_A).await, StatusCode::OK);

        let path = format!("/api/v1/projects/{}", seeded.project.id);
        let patched = app
            .request(Method::PATCH, &path)
            .header(header::COOKIE.as_str(), &seeded.cookie)
            .json(&serde_json::json!({ "retention_days": 1 }))
            .send()
            .await;
        assert_eq!(patched.status, 200);
        assert_eq!(
            trace_status(&app, &seeded, TRACE_A).await,
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn patch_to_forever_keeps_dropped_data_dropped() {
        let app = TestApp::new().await;
        app.state.clock.set(NOW);
        let seeded = app.seed().await;
        commit_at(&app, &seeded.project, TRACE_A, NOW - 3 * DAY + 12 * HOUR).await;
        let path = format!("/api/v1/projects/{}", seeded.project.id);
        let set = app
            .request(Method::PATCH, &path)
            .header(header::COOKIE.as_str(), &seeded.cookie)
            .json(&serde_json::json!({ "retention_days": 1 }))
            .send()
            .await;
        assert_eq!(set.status, 200);
        assert_eq!(
            trace_status(&app, &seeded, TRACE_A).await,
            StatusCode::NOT_FOUND
        );

        let forever = app
            .request(Method::PATCH, &path)
            .header(header::COOKIE.as_str(), &seeded.cookie)
            .json(&serde_json::json!({ "retention_days": null }))
            .send()
            .await;
        assert_eq!(forever.status, 200);
        let forever: Project = forever.json();
        assert!(forever.retention_days.is_none());
        assert_eq!(
            trace_status(&app, &seeded, TRACE_A).await,
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn patch_records_the_change() {
        let app = TestApp::new().await;
        app.state.clock.set(NOW);
        let seeded = app.seed().await;
        let path = format!("/api/v1/projects/{}", seeded.project.id);

        let status = app
            .request(Method::PATCH, &path)
            .header(header::COOKIE.as_str(), &seeded.cookie)
            .json(&serde_json::json!({ "retention_days": 7 }))
            .send()
            .await
            .status;
        assert_eq!(status, 200);
        assert_eq!(
            audit_rows(&app, "project.retention_changed").await,
            vec![(
                Some(seeded.project.id.to_string()),
                serde_json::json!({ "old_days": null, "new_days": 7 })
            )]
        );

        let status = app
            .request(Method::PATCH, &path)
            .header(header::COOKIE.as_str(), &seeded.cookie)
            .json(&serde_json::json!({ "name": "x" }))
            .send()
            .await
            .status;
        assert_eq!(status, 200);
        assert_eq!(audit_rows(&app, "project.renamed").await.len(), 1);
        assert_eq!(audit_rows(&app, "project.retention_changed").await.len(), 1);
    }

    /// Sends `PATCH retention_days: 30` to a project of org `acme`, after the org holds a
    /// token with `features` (`None`: no token, so the instance defaults apply).
    async fn patch_retention_with(features: Option<&[&str]>) -> crate::test_support::TestResponse {
        patch_with(features, None, serde_json::json!({ "retention_days": 30 })).await
    }

    /// Sends `body` as a `PATCH` to a project of org `acme` whose stored retention is `stored`
    /// (set before the org holds a token with `features`).
    async fn patch_with(
        features: Option<&[&str]>,
        stored: Option<u32>,
        body: serde_json::Value,
    ) -> crate::test_support::TestResponse {
        let app = TestApp::new().await;
        let (_, ada) = signed_in(&app, "ada@example.com").await;
        let org = app
            .request(Method::POST, "/api/v1/orgs")
            .header(header::COOKIE.as_str(), &ada)
            .json(&serde_json::json!({ "name": "Acme", "slug": "acme" }))
            .send()
            .await;
        assert_eq!(org.status, 201);
        let created = app
            .request(Method::POST, "/api/v1/orgs/acme/projects")
            .header(header::COOKIE.as_str(), &ada)
            .json(&serde_json::json!({ "name": "Support bot", "slug": "support-bot" }))
            .send()
            .await;
        let created: Project = created.json();
        if let Some(days) = stored {
            let set = app
                .request(Method::PATCH, &format!("/api/v1/projects/{}", created.id))
                .header(header::COOKIE.as_str(), &ada)
                .json(&serde_json::json!({ "retention_days": days }))
                .send()
                .await;
            assert_eq!(set.status, 200);
        }
        if let Some(features) = features {
            let org_id = orgs::org_by_slug(app.state.meta.reader(), "acme")
                .await
                .unwrap()
                .expect("org")
                .id;
            let token = crate::test_support::org_token(
                org_id,
                "2025-12-01T00:00:00Z",
                "2099-01-01T00:00:00Z",
                "2099-02-01T00:00:00Z",
                features,
                &serde_json::json!({}),
            );
            let defaults = app.state.entitlements.defaults().limits;
            let (_, grant) = crate::entitlements::tokens::grant_from_token(
                &token,
                app.state.config.public_url.origin(),
                &defaults,
            )
            .expect("valid token");
            app.state.entitlements.put_grant(org_id, grant);
        }
        app.request(Method::PATCH, &format!("/api/v1/projects/{}", created.id))
            .header(header::COOKIE.as_str(), &ada)
            .json(&body)
            .send()
            .await
    }

    #[tokio::test]
    async fn retention_management_follows_the_plan_features() {
        let locked = patch_retention_with(Some(&[])).await;
        assert_eq!(locked.status, 403);
        assert_eq!(locked.problem().code, ProblemCode::PlanRequired);

        let allowed = patch_retention_with(Some(&["retention_management"])).await;
        assert_eq!(allowed.status, 200);

        let defaults = patch_retention_with(None).await;
        assert_eq!(defaults.status, 200);
    }

    #[tokio::test]
    async fn a_locked_org_may_rename_and_echo_the_stored_retention() {
        let body = serde_json::json!({ "name": "Renamed", "retention_days": 30 });
        let echoed = patch_with(Some(&[]), Some(30), body.clone()).await;
        assert_eq!(echoed.status, 200);
        let changed = patch_with(Some(&[]), Some(7), body).await;
        assert_eq!(changed.status, 403);
        assert_eq!(changed.problem().code, ProblemCode::PlanRequired);
    }

    #[tokio::test]
    async fn get_project_has_no_last_ingest_at_before_spans_arrive() {
        let app = TestApp::new().await;
        let (_, ada) = signed_in(&app, "ada@example.com").await;
        let created = support_bot(&app, &ada).await;
        let path = format!("/api/v1/projects/{}", created.id);
        let response = app
            .request(Method::GET, &path)
            .header(header::COOKIE.as_str(), &ada)
            .send()
            .await;
        let body: serde_json::Value = response.json();
        assert!(body.get("last_ingest_at").is_none());
    }

    #[tokio::test]
    async fn get_project_shows_last_ingest_at_after_a_batch() {
        const T: i64 = 1_767_225_600_123_456_789; // 2026-01-01T00:00:00.123456789Z
        let app = TestApp::new().await;
        let (_, ada) = signed_in(&app, "ada@example.com").await;
        let created = support_bot(&app, &ada).await;
        app.state.last_ingest.record(created.id, T);
        let path = format!("/api/v1/projects/{}", created.id);
        let response = app
            .request(Method::GET, &path)
            .header(header::COOKIE.as_str(), &ada)
            .send()
            .await;
        let body: serde_json::Value = response.json();
        assert_eq!(body["last_ingest_at"], "2026-01-01T00:00:00.123456789Z");
        let fetched: Project = response.json();
        assert_eq!(fetched.last_ingest_at, Some(T));
    }

    #[tokio::test]
    async fn lists_never_show_last_ingest_at() {
        const T: i64 = 1_767_225_600_123_456_789;
        let app = TestApp::new().await;
        let (_, ada) = signed_in(&app, "ada@example.com").await;
        let created = support_bot(&app, &ada).await;
        app.state.last_ingest.record(created.id, T);
        let response = app
            .request(Method::GET, "/api/v1/orgs/acme/projects")
            .header(header::COOKIE.as_str(), &ada)
            .send()
            .await;
        assert_eq!(response.status, 200);
        let body: serde_json::Value = response.json();
        assert_eq!(body["data"].as_array().map(Vec::len), Some(1));
        assert!(body["data"][0].get("last_ingest_at").is_none());
    }
}
