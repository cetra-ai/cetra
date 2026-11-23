// SPDX-License-Identifier: Elastic-2.0
//! The Provisioning API's group role mappings (track EA M6, D-EA-14, D-EA-19): the org's SCIM
//! groups with their mappings, and setting or removing one mapping. Owners only, session only.
//! A change that only lowers roles needs no feature, so a lapsed org can still take roles away.

use std::collections::HashSet;

use axum::extract::{Extension, State};
use axum::http::{Method, StatusCode};
use axum::routing::{delete, get, put};
use cetra_core::ids::{OrgId, ProjectId};
use serde::Deserialize;
use sqlx::SqliteConnection;
use uuid::Uuid;

use super::recompute::{self, RecomputeScope};
use super::store::{self, GroupMapping, ScimGroupRow};
use crate::api::account::OrgRole;
use crate::api::common::FieldError;
use crate::api::entitlements::Feature;
use crate::api::members::ProjectRoleOverride;
use crate::api::scim::{
    PutScimGroupMapping, ScimGroup, ScimGroupList, ScimGroupMapping, ScimProjectRole,
};
use crate::audit::catalog::ScimAction;
use crate::audit::{self, Event, TargetKind};
use crate::auth::roles::{self, OrgPerm, ProjectOverride};
use crate::entitlements;
use crate::http::AppState;
use crate::http::access::{Access, OrgAccess, RouteDef};
use crate::http::error::{self, ApiError};
use crate::meta::MetaError;

/// The most projects one mapping may hold.
const MAX_PROJECTS: usize = 200;

/// The group routes of the Provisioning API.
pub(crate) fn routes() -> Vec<RouteDef> {
    vec![
        RouteDef {
            method: Method::GET,
            path: "/api/v1/orgs/{org}/scim/groups",
            access: Access::Org {
                perm: OrgPerm::ManageProvisioning,
                session_only: true,
            },
            documented: true,
            handler: get(list_groups),
        },
        RouteDef {
            method: Method::PUT,
            path: "/api/v1/orgs/{org}/scim/groups/{id}/mapping",
            access: Access::Org {
                perm: OrgPerm::ManageProvisioning,
                session_only: true,
            },
            documented: true,
            handler: put(set_mapping),
        },
        RouteDef {
            method: Method::DELETE,
            path: "/api/v1/orgs/{org}/scim/groups/{id}/mapping",
            access: Access::Org {
                perm: OrgPerm::ManageProvisioning,
                session_only: true,
            },
            documented: true,
            handler: delete(remove_mapping),
        },
    ]
}

#[derive(Debug, Deserialize)]
struct GroupPath {
    id: Uuid,
}

/// What a mapping PUT did inside its transaction.
enum Set {
    Done(ScimGroupRow),
    NotFound,
    /// A project that is not a project of the org.
    UnknownProject,
    /// The change raises roles and the org lacks the feature it needs.
    Locked(Feature),
}

async fn list_groups(
    State(state): State<AppState>,
    Extension(access): Extension<OrgAccess>,
) -> Result<error::Json<ScimGroupList>, ApiError> {
    let org = access.org.id;
    let reader = state.meta.reader();
    let rows = store::all_groups(reader, org).await?;
    let counts = store::member_counts(reader, org).await?;
    let mappings = store::mappings(reader, org).await?;
    let groups = rows
        .iter()
        .map(|row| {
            let count = counts.get(&row.id).copied().unwrap_or(0);
            api_group(row, count, mappings.get(&row.id))
        })
        .collect();
    Ok(error::Json(ScimGroupList { groups }))
}

async fn set_mapping(
    State(state): State<AppState>,
    Extension(access): Extension<OrgAccess>,
    ctx: audit::Ctx,
    error::Path(path): error::Path<GroupPath>,
    error::Json(body): error::Json<PutScimGroupMapping>,
) -> Result<error::Json<ScimGroup>, ApiError> {
    let org = access.org.id;
    let group = path.id;
    let mapping = checked_mapping(&body)?;
    // The feature checks read the entitlements in memory, before the write (D-P-10).
    let scim_allowed = entitlements::of(&state, org).allows(Feature::Scim);
    let projects_allowed = entitlements::of(&state, org).allows(Feature::ProjectRoles);
    let now = state.clock.now_ns();
    let write_state = state.clone();
    let outcome = state
        .meta
        .write(move |conn| {
            Box::pin(async move {
                let Some(row) = store::group_tx(conn, org, group).await? else {
                    return Ok(Set::NotFound);
                };
                if !projects_in_org_tx(conn, org, &mapping.projects).await? {
                    return Ok(Set::UnknownProject);
                }
                let old = store::mappings_tx(conn, org)
                    .await?
                    .remove(&group)
                    .unwrap_or(GroupMapping {
                        org_role: None,
                        projects: Vec::new(),
                    });
                if !scim_allowed && !only_lowers(&old, &mapping) {
                    return Ok(Set::Locked(Feature::Scim));
                }
                if !projects_allowed && writes_project_roles(&old, &mapping) {
                    return Ok(Set::Locked(Feature::ProjectRoles));
                }
                store::put_mapping(conn, group, &mapping, now).await?;
                recompute_group_tx(conn, &ctx, &write_state, org, group).await?;
                let event = Event::new(ScimAction::GroupMappingChanged {
                    scim_group_id: group,
                    org_role: mapping.org_role.map(roles::org_role_str),
                    projects: u32::try_from(mapping.projects.len()).unwrap_or(u32::MAX),
                })
                .org(org)
                .target(TargetKind::ScimGroup, group);
                audit::record(conn, &ctx, event, now).await?;
                Ok(Set::Done(row))
            })
        })
        .await?;
    match outcome {
        Set::Done(row) => Ok(error::Json(group_view(&state, org, &row).await?)),
        Set::NotFound => Err(ApiError::NotFound),
        Set::UnknownProject => Err(field_error("projects", "unknown project")),
        Set::Locked(feature) => Err(ApiError::PlanRequired {
            detail: feature.locked_detail(),
        }),
    }
}

async fn remove_mapping(
    State(state): State<AppState>,
    Extension(access): Extension<OrgAccess>,
    ctx: audit::Ctx,
    error::Path(path): error::Path<GroupPath>,
) -> Result<StatusCode, ApiError> {
    let org = access.org.id;
    let group = path.id;
    let now = state.clock.now_ns();
    let write_state = state.clone();
    // `false` means the group is not in the org. A group with no mapping is a no-op (204).
    let found = state
        .meta
        .write(move |conn| {
            Box::pin(async move {
                if store::group_tx(conn, org, group).await?.is_none() {
                    return Ok(false);
                }
                if !store::delete_mapping(conn, group).await? {
                    return Ok(true);
                }
                recompute_group_tx(conn, &ctx, &write_state, org, group).await?;
                let event = Event::new(ScimAction::GroupMappingChanged {
                    scim_group_id: group,
                    org_role: None,
                    projects: 0,
                })
                .org(org)
                .target(TargetKind::ScimGroup, group);
                audit::record(conn, &ctx, event, now).await?;
                Ok(true)
            })
        })
        .await?;
    if found {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::NotFound)
    }
}

/// Checks the body without the database: the owner and project rules and an empty mapping.
fn checked_mapping(body: &PutScimGroupMapping) -> Result<GroupMapping, ApiError> {
    if body.org_role == Some(OrgRole::Owner) {
        return Err(field_error("org_role", "owner cannot be mapped"));
    }
    if body.projects.len() > MAX_PROJECTS {
        return Err(field_error("projects", "at most 200 projects"));
    }
    let mut seen = HashSet::new();
    if !body
        .projects
        .iter()
        .all(|project| seen.insert(project.project_id))
    {
        return Err(field_error("projects", "duplicate project"));
    }
    if body.org_role.is_none() && body.projects.is_empty() {
        return Err(ApiError::BadRequest {
            detail: Some("Map an org role or a project, or delete the mapping.".to_owned()),
            errors: Vec::new(),
        });
    }
    Ok(GroupMapping {
        org_role: body.org_role,
        projects: body
            .projects
            .iter()
            .map(|project| {
                (
                    ProjectId::from_uuid(project.project_id),
                    stored_override(project.role),
                )
            })
            .collect(),
    })
}

/// Whether every project of `projects` is a project of `org`.
async fn projects_in_org_tx(
    conn: &mut SqliteConnection,
    org: OrgId,
    projects: &[(ProjectId, ProjectOverride)],
) -> Result<bool, MetaError> {
    for (project, _) in projects {
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM projects WHERE org_id = ? AND id = ?")
            .bind(org.to_string())
            .bind(project.to_string())
            .fetch_one(&mut *conn)
            .await?;
        if count == 0 {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Whether `new` only lowers access compared with `old`: the org role is no higher, every
/// project row is kept at or below its old rank, and no project row is added. Removed rows
/// lower access (D-EA-19).
fn only_lowers(old: &GroupMapping, new: &GroupMapping) -> bool {
    org_level(new.org_role) <= org_level(old.org_role)
        && new.projects.iter().all(|(project, role)| {
            project_rank_in(old, *project)
                .is_some_and(|rank| override_rank(*role) <= rank)
        })
}

/// Whether `new` writes a project row that is new or higher than the old one. Writing one
/// needs `project_roles`.
fn writes_project_roles(old: &GroupMapping, new: &GroupMapping) -> bool {
    new.projects
        .iter()
        .any(|(project, role)| match project_rank_in(old, *project) {
            None => true,
            Some(rank) => override_rank(*role) > rank,
        })
}

/// The rank of a group's org role: no mapping lowest, then viewer, member and admin.
fn org_level(role: Option<OrgRole>) -> u8 {
    role.map_or(0, |role| roles::org_rank(role) + 1)
}

/// The rank of a stored project override: none < viewer < member < admin (D-EA-14).
fn override_rank(role: ProjectOverride) -> u8 {
    match role {
        ProjectOverride::None => 0,
        ProjectOverride::Viewer => 1,
        ProjectOverride::Member => 2,
        ProjectOverride::Admin => 3,
    }
}

fn project_rank_in(mapping: &GroupMapping, project: ProjectId) -> Option<u8> {
    mapping
        .projects
        .iter()
        .find(|(id, _)| *id == project)
        .map(|(_, role)| override_rank(*role))
}

/// P-6: when SCIM is on after the write, recomputes the roles of the group's members in the
/// same transaction (D-EA-14). Nothing happens while SCIM is off.
async fn recompute_group_tx(
    conn: &mut SqliteConnection,
    ctx: &audit::Ctx,
    state: &AppState,
    org: OrgId,
    group: Uuid,
) -> Result<(), MetaError> {
    let scim = store::org_scim_tx(conn, org).await?;
    if !scim.enabled {
        return Ok(());
    }
    let scope = RecomputeScope::of(state, org, scim.default_role);
    let members = store::group_member_ids_tx(conn, group).await?;
    recompute::recompute_tx(conn, ctx, &scope, &members).await
}

/// The group as the API shows it, read after the write.
async fn group_view(state: &AppState, org: OrgId, row: &ScimGroupRow) -> Result<ScimGroup, ApiError> {
    let reader = state.meta.reader();
    let members = store::group_member_ids(reader, row.id).await?;
    let mappings = store::mappings(reader, org).await?;
    Ok(api_group(
        row,
        u32::try_from(members.len()).unwrap_or(u32::MAX),
        mappings.get(&row.id),
    ))
}

fn api_group(row: &ScimGroupRow, member_count: u32, mapping: Option<&GroupMapping>) -> ScimGroup {
    ScimGroup {
        id: row.id,
        display_name: row.display_name.clone(),
        external_id: row.external_id.clone(),
        member_count,
        mapping: mapping.map(|mapping| ScimGroupMapping {
            org_role: mapping.org_role,
            projects: mapping
                .projects
                .iter()
                .map(|(project, role)| ScimProjectRole {
                    project_id: project.as_uuid(),
                    role: api_override(*role),
                })
                .collect(),
        }),
    }
}

fn stored_override(role: ProjectRoleOverride) -> ProjectOverride {
    match role {
        ProjectRoleOverride::Admin => ProjectOverride::Admin,
        ProjectRoleOverride::Member => ProjectOverride::Member,
        ProjectRoleOverride::Viewer => ProjectOverride::Viewer,
        ProjectRoleOverride::None => ProjectOverride::None,
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
    use axum::http::{Method, StatusCode};
    use serde_json::{Value, json};
    use uuid::Uuid;

    use super::super::store::{self as scim_store, GroupMapping};
    use super::super::test_helpers::{create_group, create_user, scim_org};
    use crate::api::account::OrgRole;
    use crate::api::common::ProblemCode;
    use crate::api::scim::ScimGroup;
    use crate::auth::roles::ProjectOverride;
    use crate::meta::members;
    use crate::test_support::{Seeded, TestApp, TestResponse, second_org};

    const DOMAIN: &str = "acme.test";

    /// The mapping route of `group` in `acme`.
    fn mapping_path(group: Uuid) -> String {
        format!("/api/v1/orgs/acme/scim/groups/{group}/mapping")
    }

    /// An app licensed with `features`, `seed()`, and SCIM on with a live token whose
    /// default role is `default_role`.
    async fn app_with(features: &[&str], default_role: OrgRole) -> (TestApp, Seeded, String) {
        let app = TestApp::with_license(features).await;
        let seeded = app.seed().await;
        let token = scim_org(&app, &seeded, DOMAIN, default_role).await;
        (app, seeded, token)
    }

    /// Provisions `email` through `POST /scim/v2/Users`. Returns the SCIM user id and the
    /// account's user id.
    async fn provision(app: &TestApp, seeded: &Seeded, token: &str, email: &str) -> (Uuid, Uuid) {
        let response = create_user(app, token, email).await;
        assert_eq!(response.status, StatusCode::CREATED);
        let scim_id: Uuid = response.json::<Value>()["id"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();
        let row = scim_store::user(app.state.meta.reader(), seeded.org_id, scim_id)
            .await
            .unwrap()
            .unwrap();
        (scim_id, row.user_id)
    }

    /// Creates a SCIM group with `members` through `POST /scim/v2/Groups`. Returns its id.
    async fn group_with(app: &TestApp, token: &str, name: &str, members: &[Uuid]) -> Uuid {
        let members: Vec<String> = members.iter().map(Uuid::to_string).collect();
        let refs: Vec<&str> = members.iter().map(String::as_str).collect();
        let response = create_group(app, token, name, &refs).await;
        assert_eq!(response.status, StatusCode::CREATED);
        response.json::<Value>()["id"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap()
    }

    /// Makes `email` a SCIM user of the org in the database: an account, a membership with
    /// `role` and the SCIM row. Returns the SCIM user id and the account's user id.
    async fn provision_direct(
        app: &TestApp,
        seeded: &Seeded,
        email: &str,
        role: OrgRole,
    ) -> (Uuid, Uuid) {
        let member = app.add_member(seeded, email, role).await;
        let now = app.state.clock.now_ns();
        let org = seeded.org_id;
        let fields = scim_store::ScimUserFields {
            user_name: email.to_owned(),
            external_id: None,
            given_name: None,
            family_name: None,
            display_name: None,
            department: None,
            employee_number: None,
        };
        let row = app
            .state
            .meta
            .write(move |conn| {
                Box::pin(
                    async move { scim_store::insert_user(conn, org, member.id, &fields, now).await },
                )
            })
            .await
            .unwrap();
        (row.id, row.user_id)
    }

    /// Makes a SCIM group of `members` in the database. Returns its id.
    async fn group_direct(app: &TestApp, seeded: &Seeded, name: &str, members: &[Uuid]) -> Uuid {
        let now = app.state.clock.now_ns();
        let org = seeded.org_id;
        let name = name.to_owned();
        let members = members.to_vec();
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    let group = scim_store::insert_group(conn, org, &name, None, now).await?;
                    scim_store::add_group_members_tx(conn, org, group.id, &members).await?;
                    Ok(group.id)
                })
            })
            .await
            .unwrap()
    }

    /// Sends a mapping PUT for `group` as the session `cookie`.
    async fn put(app: &TestApp, cookie: &str, group: Uuid, body: &Value) -> TestResponse {
        app.request(Method::PUT, &mapping_path(group))
            .header("cookie", cookie)
            .json(body)
            .send()
            .await
    }

    /// Sends a mapping DELETE for `group` as the session `cookie`.
    async fn delete(app: &TestApp, cookie: &str, group: Uuid) -> TestResponse {
        app.request(Method::DELETE, &mapping_path(group))
            .header("cookie", cookie)
            .send()
            .await
    }

    /// Writes a mapping straight to the database, for a state the route cannot reach
    /// (a lapsed org that already had a mapping).
    async fn write_mapping(app: &TestApp, group: Uuid, mapping: GroupMapping) {
        let now = app.state.clock.now_ns();
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move { scim_store::put_mapping(conn, group, &mapping, now).await })
            })
            .await
            .unwrap();
    }

    async fn role_of(app: &TestApp, seeded: &Seeded, user: Uuid) -> Option<OrgRole> {
        members::org_role(app.state.meta.reader(), seeded.org_id, user)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn owner_maps_a_group_and_roles_follow() {
        let (app, seeded, token) =
            app_with(&["scim", "project_roles"], OrgRole::Viewer).await;
        let (scim_id, user) = provision(&app, &seeded, &token, "grace@acme.test").await;
        let group = group_with(&app, &token, "Engineers", &[scim_id]).await;
        assert_eq!(role_of(&app, &seeded, user).await, Some(OrgRole::Viewer));

        let project = seeded.project.id;
        let body = json!({
            "org_role": "member",
            "projects": [{"project_id": project.as_uuid(), "role": "admin"}],
        });
        let response = put(&app, &seeded.cookie, group, &body).await;
        assert_eq!(response.status, StatusCode::OK);
        let shown: ScimGroup = response.json();
        assert_eq!(shown.member_count, 1);
        assert_eq!(shown.mapping.unwrap().org_role, Some(OrgRole::Member));

        assert_eq!(role_of(&app, &seeded, user).await, Some(OrgRole::Member));
        let reader = app.state.meta.reader();
        assert_eq!(
            members::project_override(reader, project, user).await.unwrap(),
            Some(ProjectOverride::Admin)
        );
    }

    #[tokio::test]
    async fn owner_cannot_be_mapped() {
        let (app, seeded, token) = app_with(&["scim"], OrgRole::Viewer).await;
        let group = group_with(&app, &token, "Owners", &[]).await;
        let response = put(&app, &seeded.cookie, group, &json!({"org_role": "owner"})).await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn projects_of_another_org_are_refused() {
        // A random id is no project of `acme`, which is the same refusal as another org's.
        let (app, seeded, token) = app_with(&["scim", "project_roles"], OrgRole::Viewer).await;
        let group = group_with(&app, &token, "Engineers", &[]).await;
        let body = json!({"projects": [{"project_id": Uuid::now_v7(), "role": "viewer"}]});
        let response = put(&app, &seeded.cookie, group, &body).await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn lowering_works_when_lapsed() {
        // No `scim` feature: the org has lapsed. A lapse refuses SCIM creates, so the member
        // and the group are written to the database.
        let (app, seeded, _token) = app_with(&["project_roles"], OrgRole::Member).await;
        let (scim_id, user) =
            provision_direct(&app, &seeded, "grace@acme.test", OrgRole::Member).await;
        let group = group_direct(&app, &seeded, "Engineers", &[scim_id]).await;
        write_mapping(
            &app,
            group,
            GroupMapping {
                org_role: Some(OrgRole::Member),
                projects: Vec::new(),
            },
        )
        .await;
        assert_eq!(role_of(&app, &seeded, user).await, Some(OrgRole::Member));

        let lowered = put(&app, &seeded.cookie, group, &json!({"org_role": "viewer"})).await;
        assert_eq!(lowered.status, StatusCode::OK);
        assert_eq!(role_of(&app, &seeded, user).await, Some(OrgRole::Viewer));
    }

    #[tokio::test]
    async fn raising_needs_the_feature() {
        let (app, seeded, _token) = app_with(&["project_roles"], OrgRole::Viewer).await;
        let group = group_direct(&app, &seeded, "Engineers", &[]).await;
        let response = put(&app, &seeded.cookie, group, &json!({"org_role": "member"})).await;
        assert_eq!(response.status, StatusCode::FORBIDDEN);
        assert_eq!(response.problem().code, ProblemCode::PlanRequired);
    }

    #[tokio::test]
    async fn project_roles_need_the_project_roles_feature() {
        let (app, seeded, token) = app_with(&["scim"], OrgRole::Viewer).await;
        let group = group_with(&app, &token, "Engineers", &[]).await;
        let body = json!({"projects": [{"project_id": seeded.project.id.as_uuid(), "role": "admin"}]});
        let response = put(&app, &seeded.cookie, group, &body).await;
        assert_eq!(response.status, StatusCode::FORBIDDEN);
        assert_eq!(response.problem().code, ProblemCode::PlanRequired);
    }

    #[tokio::test]
    async fn delete_mapping_returns_members_to_the_default() {
        let (app, seeded, token) =
            app_with(&["scim", "project_roles"], OrgRole::Viewer).await;
        let (scim_id, user) = provision(&app, &seeded, &token, "grace@acme.test").await;
        let group = group_with(&app, &token, "Engineers", &[scim_id]).await;
        let set = put(&app, &seeded.cookie, group, &json!({"org_role": "member"})).await;
        assert_eq!(set.status, StatusCode::OK);
        assert_eq!(role_of(&app, &seeded, user).await, Some(OrgRole::Member));

        let removed = delete(&app, &seeded.cookie, group).await;
        assert_eq!(removed.status, StatusCode::NO_CONTENT);
        assert_eq!(role_of(&app, &seeded, user).await, Some(OrgRole::Viewer));
    }

    #[tokio::test]
    async fn admins_cannot_manage_mappings() {
        let (app, seeded, token) = app_with(&["scim", "project_roles"], OrgRole::Viewer).await;
        let group = group_with(&app, &token, "Engineers", &[]).await;
        let admin = app
            .add_member(&seeded, "admin@acme.test", OrgRole::Admin)
            .await;
        let response = put(&app, &admin.cookie, group, &json!({"org_role": "member"})).await;
        assert_eq!(response.status, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn another_orgs_group_is_404() {
        let (app, seeded, _token) = app_with(&["scim", "project_roles"], OrgRole::Viewer).await;
        let (other, _owner) = second_org(&app, "other", "ops@other.test").await;
        let now = app.state.clock.now_ns();
        let foreign = app
            .state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    scim_store::insert_group(conn, other, "Ops", None, now).await
                })
            })
            .await
            .unwrap();
        let response = put(
            &app,
            &seeded.cookie,
            foreign.id,
            &json!({"org_role": "member"}),
        )
        .await;
        assert_eq!(response.status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn duplicate_projects_are_refused() {
        let (app, seeded, token) = app_with(&["scim", "project_roles"], OrgRole::Viewer).await;
        let group = group_with(&app, &token, "Engineers", &[]).await;
        let project = seeded.project.id.as_uuid();
        let body = json!({"projects": [
            {"project_id": project, "role": "admin"},
            {"project_id": project, "role": "viewer"},
        ]});
        let response = put(&app, &seeded.cookie, group, &body).await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn mapping_changes_with_scim_off_recompute_nothing() {
        let (app, seeded, token) =
            app_with(&["scim", "project_roles"], OrgRole::Viewer).await;
        let (scim_id, user) = provision(&app, &seeded, &token, "grace@acme.test").await;
        let group = group_with(&app, &token, "Engineers", &[scim_id]).await;
        let org = seeded.org_id;
        let now = app.state.clock.now_ns();
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    scim_store::put_org_scim(conn, org, false, OrgRole::Viewer, now).await
                })
            })
            .await
            .unwrap();

        let response = put(&app, &seeded.cookie, group, &json!({"org_role": "member"})).await;
        assert_eq!(response.status, StatusCode::OK);
        assert_eq!(role_of(&app, &seeded, user).await, Some(OrgRole::Viewer));
    }
}
