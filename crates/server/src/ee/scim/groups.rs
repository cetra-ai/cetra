// SPDX-License-Identifier: Elastic-2.0
//! The SCIM Groups resource (RFC 7643 § 4.2, RFC 7644 §§ 3.2 to 3.6, track EA M6, D-EA-12,
//! D-EA-14, D-EA-19).
//!
//! Members are the org's SCIM user ids. Every write that changes membership recomputes the
//! affected members' roles in the same transaction (P-2). A write runs in one `meta.write`; a
//! refusal found before its first write is returned as `Ok(Err(ScimError))`, so nothing is
//! written. A lapsed org (no `scim` entitlement) may remove members and delete groups, but not
//! add members, rename, or create (D-EA-19, P-5).

use axum::body::Bytes;
use axum::extract::{Extension, Path, Query, State};
use axum::http::{Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, patch, post, put};
use cetra_core::ids::OrgId;
use serde::Deserialize;
use serde_json::Value;
use sqlx::{SqliteConnection, SqlitePool};
use uuid::Uuid;

use super::error::{ScimError, ScimJson};
use super::filter::parse_group_filter;
use super::patch::{GroupChange, external_id, group_changes, member_values};
use super::recompute::{self, RecomputeScope};
use super::schema::{group_resource, list_response};
use super::store::{self as scim_store, ScimGroupRow};
use super::users::{self, ListQuery};
use crate::audit::catalog::ScimAction;
use crate::audit::{self, Event, TargetKind};
use crate::auth::principal::Principal;
use crate::http::AppState;
use crate::http::access::RouteDef;
use crate::meta::MetaError;

const NOT_ENTITLED: &str = "SCIM provisioning needs the Enterprise plan or licence.";

/// The `/scim/v2/Groups` routes. None is documented in the OpenAPI document.
pub(crate) fn routes() -> Vec<RouteDef> {
    vec![
        users::route(Method::GET, "/scim/v2/Groups", get(list_groups)),
        users::route(Method::POST, "/scim/v2/Groups", post(create_group)),
        users::route(Method::GET, "/scim/v2/Groups/{id}", get(get_group)),
        users::route(Method::PUT, "/scim/v2/Groups/{id}", put(replace_group)),
        users::route(Method::PATCH, "/scim/v2/Groups/{id}", patch(patch_group)),
        users::route(
            Method::DELETE,
            "/scim/v2/Groups/{id}",
            delete(delete_group_row),
        ),
    ]
}

/// The `GET /Groups` query: the list parameters the Users route reads, plus
/// `excludedAttributes`.
#[derive(Deserialize)]
struct GroupListQuery {
    #[serde(flatten)]
    list: ListQuery,
    #[serde(rename = "excludedAttributes")]
    excluded_attributes: Option<String>,
}

/// The group with this id in `pool`'s org, for the reads. The write paths use `group_tx`
/// inside their transaction.
async fn group_of(
    pool: &SqlitePool,
    org: OrgId,
    id: Uuid,
) -> Result<Option<ScimGroupRow>, MetaError> {
    let mut conn = pool.acquire().await?;
    scim_store::group_tx(&mut conn, org, id).await
}

/// The group and its member ids as they are now, inside the caller's transaction.
async fn loaded_tx(
    conn: &mut SqliteConnection,
    org: OrgId,
    id: Uuid,
) -> Result<(ScimGroupRow, Vec<Uuid>), MetaError> {
    let row = scim_store::group_tx(conn, org, id)
        .await?
        .ok_or(MetaError::Corrupt {
            table: "scim_groups",
            column: "id",
        })?;
    let members = scim_store::group_member_ids_tx(conn, id).await?;
    Ok((row, members))
}

/// A group write's error: a displayName or externalId another group holds is the group
/// uniqueness error. Other errors are the Users route's.
fn group_error(error: MetaError) -> ScimError {
    match error {
        MetaError::Conflict("uniqueness") => ScimError::group_uniqueness(),
        other => users::write_error(other),
    }
}

/// The `displayName` of a request body: a non-blank string.
fn display_name_of(body: &Value) -> Result<String, ScimError> {
    body.get("displayName")
        .and_then(Value::as_str)
        .filter(|name| !name.trim().is_empty())
        .map(str::to_owned)
        .ok_or_else(|| ScimError::invalid_value("displayName is required"))
}

/// The ids of `values` that are SCIM users of `org`, in order and without repeats. A value that
/// is not a UUID, or that names no user of the org, is skipped, so another org's user never
/// joins (P-7, D-EA-12). By design an unknown id in an Add or a create is ignored: the request
/// still answers 200 (or 201) and that id changes nothing.
async fn known_members(
    conn: &mut SqliteConnection,
    org: OrgId,
    values: &[String],
) -> Result<Vec<Uuid>, MetaError> {
    let mut known = Vec::new();
    for value in values {
        let Ok(id) = Uuid::parse_str(value) else {
            continue;
        };
        if !known.contains(&id) && scim_store::user_tx(conn, org, id).await?.is_some() {
            known.push(id);
        }
    }
    Ok(known)
}

/// The values that parse as UUIDs. Used for removals, which need no org check: an id that is
/// not a member simply matches nothing.
fn member_uuids(values: &[String]) -> Vec<Uuid> {
    values
        .iter()
        .filter_map(|value| Uuid::parse_str(value).ok())
        .collect()
}

/// A count for an audit record. A bulk change is one record with its count (README).
fn count(ids: &[Uuid]) -> u32 {
    u32::try_from(ids.len()).unwrap_or(u32::MAX)
}

/// Whether `excludedAttributes` names `members`. The names are compared case-insensitively.
fn excludes_members(excluded: Option<&str>) -> bool {
    excluded.is_some_and(|list| {
        list.split(',')
            .any(|name| name.trim().eq_ignore_ascii_case("members"))
    })
}

/// A group as a PUT or PATCH leaves it: the name, the external id and the member ids.
struct Target {
    display_name: String,
    external_id: Option<String>,
    members: Vec<Uuid>,
}

/// Applies `changes` in order to the group `row` whose members are `current`. Member values
/// are resolved against the org's users here. The refusals (a `replace` that names no user)
/// are returned as `Ok(Err(..))` before any write.
async fn fold_changes(
    conn: &mut SqliteConnection,
    org: OrgId,
    row: &ScimGroupRow,
    current: &[Uuid],
    changes: &[GroupChange],
) -> Result<Result<Target, ScimError>, MetaError> {
    let mut target = Target {
        display_name: row.display_name.clone(),
        external_id: row.external_id.clone(),
        members: current.to_vec(),
    };
    for change in changes {
        match change {
            GroupChange::DisplayName(name) => target.display_name.clone_from(name),
            GroupChange::ExternalId(value) => target.external_id.clone_from(value),
            GroupChange::AddMembers(values) => {
                for id in known_members(conn, org, values).await? {
                    if !target.members.contains(&id) {
                        target.members.push(id);
                    }
                }
            }
            GroupChange::RemoveMembers(values) => {
                let ids = member_uuids(values);
                target.members.retain(|member| !ids.contains(member));
            }
            GroupChange::ReplaceMembers(values) => {
                let ids = known_members(conn, org, values).await?;
                // A malformed list never empties a group; an explicit empty list does.
                if !values.is_empty() && ids.is_empty() {
                    return Ok(Err(ScimError::invalid_value(
                        "No listed member is a user of this organization.",
                    )));
                }
                target.members = ids;
            }
        }
    }
    Ok(Ok(target))
}

/// Writes `target` over the group `row` (currently with members `current`) and records what
/// changed: one `group_updated` for the attributes, one `group_members_changed` with the counts
/// for the members, and the recompute of every member whose groups changed (P-2, P-4, P-5).
/// A lapsed org (no `scim`) is refused a rename or an added member before the first write.
async fn apply_tx(
    conn: &mut SqliteConnection,
    ctx: &audit::Ctx,
    scope: &RecomputeScope,
    org: OrgId,
    row: &ScimGroupRow,
    current: &[Uuid],
    target: Target,
) -> Result<Result<(), ScimError>, MetaError> {
    let mut fields: Vec<&'static str> = Vec::new();
    if target.display_name != row.display_name {
        fields.push("displayName");
    }
    if target.external_id != row.external_id {
        fields.push("externalId");
    }
    let added: Vec<Uuid> = target
        .members
        .iter()
        .copied()
        .filter(|member| !current.contains(member))
        .collect();
    let removed: Vec<Uuid> = current
        .iter()
        .copied()
        .filter(|member| !target.members.contains(member))
        .collect();
    if !scope.scim && (!fields.is_empty() || !added.is_empty()) {
        return Ok(Err(ScimError::forbidden(NOT_ENTITLED)));
    }

    let now = scope.now;
    if !fields.is_empty() {
        scim_store::update_group(
            conn,
            org,
            row.id,
            &target.display_name,
            target.external_id.as_deref(),
            now,
        )
        .await?;
        audit::record(
            conn,
            ctx,
            Event::new(ScimAction::GroupUpdated {
                scim_group_id: row.id,
                fields,
            })
            .org(org)
            .target(TargetKind::ScimGroup, row.id),
            now,
        )
        .await?;
    }

    let removed_ids = scim_store::remove_group_members_tx(conn, row.id, &removed).await?;
    let added_ids = scim_store::add_group_members_tx(conn, org, row.id, &added).await?;
    if !added_ids.is_empty() || !removed_ids.is_empty() {
        audit::record(
            conn,
            ctx,
            Event::new(ScimAction::GroupMembersChanged {
                scim_group_id: row.id,
                added: count(&added_ids),
                removed: count(&removed_ids),
            })
            .org(org)
            .target(TargetKind::ScimGroup, row.id),
            now,
        )
        .await?;
        let affected: Vec<Uuid> = added_ids.iter().chain(&removed_ids).copied().collect();
        recompute::recompute_tx(conn, ctx, scope, &affected).await?;
    }
    Ok(Ok(()))
}

/// A PUT or PATCH inside its transaction: loads the group (404 otherwise, P-7), folds `changes`
/// over it, applies the result, and returns the group with its members.
async fn edit_tx(
    conn: &mut SqliteConnection,
    ctx: &audit::Ctx,
    scope: &RecomputeScope,
    org: OrgId,
    id: Uuid,
    changes: &[GroupChange],
) -> Result<Result<(ScimGroupRow, Vec<Uuid>), ScimError>, MetaError> {
    let Some(row) = scim_store::group_tx(conn, org, id).await? else {
        return Ok(Err(ScimError::not_found()));
    };
    let current = scim_store::group_member_ids_tx(conn, id).await?;
    let target = match fold_changes(conn, org, &row, &current, changes).await? {
        Ok(target) => target,
        Err(error) => return Ok(Err(error)),
    };
    match apply_tx(conn, ctx, scope, org, &row, &current, target).await? {
        Ok(()) => {}
        Err(error) => return Ok(Err(error)),
    }
    Ok(Ok(loaded_tx(conn, org, id).await?))
}

/// Runs a PUT or PATCH's changes in one write and answers with the group.
async fn write_edit(
    state: &AppState,
    ctx: audit::Ctx,
    scope: RecomputeScope,
    org: OrgId,
    id: Uuid,
    changes: Vec<GroupChange>,
) -> Result<Response, ScimError> {
    let (row, members) = state
        .meta
        .write(move |conn| {
            Box::pin(async move { edit_tx(conn, &ctx, &scope, org, id, &changes).await })
        })
        .await
        .map_err(group_error)??;
    Ok(ScimJson(group_resource(
        &row,
        Some(&members),
        &users::base_url(state),
    ))
    .into_response())
}

/// GET /Groups: a page of the org's groups, filtered by `displayName eq` or `externalId eq`,
/// with members unless `excludedAttributes` names them.
async fn list_groups(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Query(query): Query<GroupListQuery>,
) -> Result<Response, ScimError> {
    let (org, _) = users::admit(&state, &principal).await?;
    let filter = query
        .list
        .filter
        .as_deref()
        .map(parse_group_filter)
        .transpose()?;
    let (start, count) = users::page(&query.list);
    let (total, rows) =
        scim_store::list_groups(state.meta.reader(), org, filter.as_ref(), start, count).await?;
    let base = users::base_url(&state);
    let with_members = !excludes_members(query.excluded_attributes.as_deref());
    let mut resources = Vec::with_capacity(rows.len());
    for row in &rows {
        let members = if with_members {
            Some(scim_store::group_member_ids(state.meta.reader(), row.id).await?)
        } else {
            None
        };
        resources.push(group_resource(row, members.as_deref(), &base));
    }
    Ok(ScimJson(list_response(total, start, &resources)).into_response())
}

/// GET /Groups/{id}: the group with its members, or 404.
async fn get_group(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<String>,
) -> Result<Response, ScimError> {
    let (org, _) = users::admit(&state, &principal).await?;
    let id = users::parse_id(&id)?;
    let row = group_of(state.meta.reader(), org, id)
        .await?
        .ok_or_else(ScimError::not_found)?;
    let members = scim_store::group_member_ids(state.meta.reader(), id).await?;
    Ok(ScimJson(group_resource(
        &row,
        Some(&members),
        &users::base_url(&state),
    ))
    .into_response())
}

/// POST /Groups: creates a group, with the listed members, and recomputes the roles of the
/// members added. Refused when the org is lapsed (P-5, D-EA-19). A name another group holds is
/// a 409 `uniqueness`.
async fn create_group(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    ctx: audit::Ctx,
    body: Bytes,
) -> Result<Response, ScimError> {
    let (org, default_role) = users::admit(&state, &principal).await?;
    let scope = RecomputeScope::of(&state, org, default_role);
    if !scope.scim {
        return Err(ScimError::forbidden(NOT_ENTITLED));
    }
    let body = users::json_object(&body)?;
    let display_name = display_name_of(&body)?;
    let external_id_value = external_id(body.get("externalId").unwrap_or(&Value::Null))?;
    let members = match body.get("members") {
        Some(value) => member_values(value)?,
        None => Vec::new(),
    };

    let (row, member_ids) = state
        .meta
        .write(move |conn| {
            Box::pin(async move {
                let created = scim_store::insert_group(
                    conn,
                    org,
                    &display_name,
                    external_id_value.as_deref(),
                    scope.now,
                )
                .await?;
                let known = known_members(conn, org, &members).await?;
                let added = scim_store::add_group_members_tx(conn, org, created.id, &known).await?;
                if !added.is_empty() {
                    recompute::recompute_tx(conn, &ctx, &scope, &added).await?;
                }
                audit::record(
                    conn,
                    &ctx,
                    Event::new(ScimAction::GroupCreated {
                        scim_group_id: created.id,
                        members: count(&added),
                    })
                    .org(org)
                    .target(TargetKind::ScimGroup, created.id),
                    scope.now,
                )
                .await?;
                loaded_tx(conn, org, created.id).await
            })
        })
        .await
        .map_err(group_error)?;

    let base = users::base_url(&state);
    let location = format!("{base}/Groups/{}", row.id);
    Ok((
        StatusCode::CREATED,
        [(header::LOCATION, location)],
        ScimJson(group_resource(&row, Some(&member_ids), &base)),
    )
        .into_response())
}

/// PUT /Groups/{id}: replaces the name and the external id (absent is cleared) and, when
/// `members` is present, the members. An absent `members` keeps the current ones.
async fn replace_group(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    ctx: audit::Ctx,
    Path(id): Path<String>,
    body: Bytes,
) -> Result<Response, ScimError> {
    let (org, default_role) = users::admit(&state, &principal).await?;
    let id = users::parse_id(&id)?;
    let body = users::json_object(&body)?;
    let mut changes = vec![
        GroupChange::DisplayName(display_name_of(&body)?),
        GroupChange::ExternalId(external_id(body.get("externalId").unwrap_or(&Value::Null))?),
    ];
    if let Some(value) = body.get("members") {
        changes.push(GroupChange::ReplaceMembers(member_values(value)?));
    }
    let scope = RecomputeScope::of(&state, org, default_role);
    write_edit(&state, ctx, scope, org, id, changes).await
}

/// PATCH /Groups/{id}: applies the PATCH operations in order (RFC 7644 § 3.5.2).
async fn patch_group(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    ctx: audit::Ctx,
    Path(id): Path<String>,
    body: Bytes,
) -> Result<Response, ScimError> {
    let (org, default_role) = users::admit(&state, &principal).await?;
    let id = users::parse_id(&id)?;
    let body = users::json_object(&body)?;
    let changes = group_changes(&body)?;
    let scope = RecomputeScope::of(&state, org, default_role);
    write_edit(&state, ctx, scope, org, id, changes).await
}

/// DELETE /Groups/{id}: removes the group, then recomputes the roles of its former members
/// (they lose the group's mapping). Allowed when lapsed: the recompute only lowers (D-EA-19).
async fn delete_group_row(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    ctx: audit::Ctx,
    Path(id): Path<String>,
) -> Result<Response, ScimError> {
    let (org, default_role) = users::admit(&state, &principal).await?;
    let id = users::parse_id(&id)?;
    let scope = RecomputeScope::of(&state, org, default_role);
    state
        .meta
        .write(move |conn| {
            Box::pin(async move {
                if scim_store::group_tx(conn, org, id).await?.is_none() {
                    return Ok(Err(ScimError::not_found()));
                }
                let members = scim_store::group_member_ids_tx(conn, id).await?;
                scim_store::delete_group(conn, org, id).await?;
                if !members.is_empty() {
                    recompute::recompute_tx(conn, &ctx, &scope, &members).await?;
                }
                audit::record(
                    conn,
                    &ctx,
                    Event::new(ScimAction::GroupDeleted { scim_group_id: id })
                        .org(org)
                        .target(TargetKind::ScimGroup, id),
                    scope.now,
                )
                .await?;
                Ok(Ok(()))
            })
        })
        .await
        .map_err(group_error)??;
    Ok(StatusCode::NO_CONTENT.into_response())
}

#[cfg(test)]
mod tests {
    use axum::http::{Method, StatusCode};
    use cetra_core::ids::OrgId;
    use serde_json::{Value, json};
    use uuid::Uuid;

    use super::super::patch::PATCH_SCHEMA;
    use super::super::schema::GROUP_SCHEMA;
    use super::super::store::{self as scim_store, GroupMapping, ScimUserFields};
    use super::super::test_helpers::{bearer, create_user, scim_org, scim_request};
    use crate::auth::OrgRole;
    use crate::meta::members;
    use crate::meta::members::add_org_member;
    use crate::meta::users::{EmailVerifiedVia, NewUser, insert_user as insert_account};
    use crate::test_support::{TestApp, TestResponse, second_org};

    fn id_of(response: &TestResponse) -> String {
        response.json::<Value>()["id"].as_str().unwrap().to_owned()
    }

    async fn account_of(app: &TestApp, scim_id: &str) -> Uuid {
        let user: String = sqlx::query_scalar("SELECT user_id FROM scim_users WHERE id = ?")
            .bind(scim_id.to_owned())
            .fetch_one(app.state.meta.reader())
            .await
            .unwrap();
        Uuid::parse_str(&user).unwrap()
    }

    async fn role_of(app: &TestApp, org: OrgId, account: Uuid) -> Option<OrgRole> {
        members::org_role(app.state.meta.reader(), org, account)
            .await
            .unwrap()
    }

    async fn audit_count(app: &TestApp, action: &str) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM audit_events WHERE action = ?")
            .bind(action.to_owned())
            .fetch_one(app.state.meta.reader())
            .await
            .unwrap()
    }

    /// POST a group with `display_name` and the given SCIM member ids.
    async fn create(
        app: &TestApp,
        token: &str,
        display_name: &str,
        members: &[&str],
    ) -> TestResponse {
        let members: Vec<Value> = members.iter().map(|id| json!({"value": id})).collect();
        scim_request(
            app,
            token,
            Method::POST,
            "/scim/v2/Groups",
            &json!({"schemas": [GROUP_SCHEMA], "displayName": display_name, "members": members}),
        )
        .await
    }

    /// Sends PATCH `operations` to the group `id`.
    async fn patch(app: &TestApp, token: &str, id: &str, operations: Value) -> TestResponse {
        scim_request(
            app,
            token,
            Method::PATCH,
            &format!("/scim/v2/Groups/{id}"),
            &json!({"schemas": [PATCH_SCHEMA], "Operations": operations}),
        )
        .await
    }

    async fn get(app: &TestApp, token: &str, path: &str) -> TestResponse {
        app.request(Method::GET, path)
            .header("authorization", &bearer(token))
            .send()
            .await
    }

    /// Sets the org role that the group `group` maps to.
    async fn map_group(app: &TestApp, group: &str, role: OrgRole) {
        let group = Uuid::parse_str(group).unwrap();
        let now = app.state.clock.now_ns();
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    scim_store::put_mapping(
                        conn,
                        group,
                        &GroupMapping {
                            org_role: Some(role),
                            projects: Vec::new(),
                        },
                        now,
                    )
                    .await
                })
            })
            .await
            .unwrap();
    }

    /// A SCIM user with an account and a viewer membership, written directly, for a lapsed org
    /// (which cannot POST /Users). Returns the SCIM id and the account id.
    async fn provision(app: &TestApp, org: OrgId, email: &str) -> (Uuid, Uuid) {
        let now = app.state.clock.now_ns();
        let email = email.to_owned();
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    let user = insert_account(
                        conn,
                        NewUser {
                            email: email.clone(),
                            name: None,
                            password_hash: None,
                            email_verified_at: Some(now),
                            email_verified_via: Some(EmailVerifiedVia::Idp),
                            is_instance_admin: false,
                        },
                        now,
                    )
                    .await?;
                    add_org_member(conn, org, user.id, OrgRole::Viewer, now).await?;
                    let fields = ScimUserFields {
                        user_name: email,
                        external_id: None,
                        given_name: None,
                        family_name: None,
                        display_name: None,
                        department: None,
                        employee_number: None,
                    };
                    let row = scim_store::insert_user(conn, org, user.id, &fields, now).await?;
                    Ok((row.id, user.id))
                })
            })
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn create_with_members_maps_roles() {
        let app = TestApp::with_license(&["scim", "org_sso"]).await;
        let seeded = app.seed().await;
        let token = scim_org(&app, &seeded, "example.com", OrgRole::Viewer).await;
        let ada = create_user(&app, &token, "grace@example.com").await;
        assert_eq!(ada.status, StatusCode::CREATED);
        let ada_id = id_of(&ada);
        let ada_account = account_of(&app, &ada_id).await;

        let admins = create(&app, &token, "Admins", &[]).await;
        assert_eq!(admins.status, StatusCode::CREATED);
        let admins_id = id_of(&admins);
        map_group(&app, &admins_id, OrgRole::Admin).await;
        let added = patch(
            &app,
            &token,
            &admins_id,
            json!([{"op": "add", "path": "members", "value": [{"value": ada_id}]}]),
        )
        .await;
        assert_eq!(added.status, StatusCode::OK);
        assert_eq!(
            role_of(&app, seeded.org_id, ada_account).await,
            Some(OrgRole::Admin)
        );

        // A second group with the same member: its new membership keeps the admin role that the
        // first group's mapping gives.
        let staff = create(&app, &token, "Staff", &[&ada_id]).await;
        assert_eq!(staff.status, StatusCode::CREATED);
        assert!(
            staff
                .header("location")
                .is_some_and(|location| location.contains("/scim/v2/Groups/"))
        );
        let body: Value = staff.json();
        assert_eq!(body["members"][0]["value"], ada_id);
        assert_eq!(
            role_of(&app, seeded.org_id, ada_account).await,
            Some(OrgRole::Admin)
        );
    }

    #[tokio::test]
    async fn list_filters_by_display_name() {
        let app = TestApp::with_license(&["scim", "org_sso"]).await;
        let seeded = app.seed().await;
        let token = scim_org(&app, &seeded, "example.com", OrgRole::Viewer).await;
        assert_eq!(
            create(&app, &token, "Admins", &[]).await.status,
            StatusCode::CREATED
        );
        assert_eq!(
            create(&app, &token, "Staff", &[]).await.status,
            StatusCode::CREATED
        );
        let listed: Value = get(
            &app,
            &token,
            "/scim/v2/Groups?filter=displayName%20eq%20%22Admins%22",
        )
        .await
        .json();
        assert_eq!(listed["totalResults"], 1);
        assert_eq!(listed["Resources"][0]["displayName"], "Admins");
    }

    #[tokio::test]
    async fn excluded_members_are_omitted() {
        let app = TestApp::with_license(&["scim", "org_sso"]).await;
        let seeded = app.seed().await;
        let token = scim_org(&app, &seeded, "example.com", OrgRole::Viewer).await;
        let ada = create_user(&app, &token, "ada@example.com").await;
        let ada_id = id_of(&ada);
        assert_eq!(
            create(&app, &token, "Admins", &[&ada_id]).await.status,
            StatusCode::CREATED
        );

        let excluded: Value = get(&app, &token, "/scim/v2/Groups?excludedAttributes=MEMBERS")
            .await
            .json();
        assert!(excluded["Resources"][0].get("members").is_none());

        let included: Value = get(&app, &token, "/scim/v2/Groups").await.json();
        assert_eq!(included["Resources"][0]["members"][0]["value"], ada_id);
    }

    #[tokio::test]
    async fn duplicate_display_name_is_409() {
        let app = TestApp::with_license(&["scim", "org_sso"]).await;
        let seeded = app.seed().await;
        let token = scim_org(&app, &seeded, "example.com", OrgRole::Viewer).await;
        assert_eq!(
            create(&app, &token, "Admins", &[]).await.status,
            StatusCode::CREATED
        );
        let response = create(&app, &token, "Admins", &[]).await;
        assert_eq!(response.status, StatusCode::CONFLICT);
        let body: Value = response.json();
        assert_eq!(body["scimType"], "uniqueness");
    }

    #[tokio::test]
    async fn another_orgs_group_is_404() {
        let app = TestApp::with_license(&["scim", "org_sso"]).await;
        let seeded = app.seed().await;
        let token = scim_org(&app, &seeded, "example.com", OrgRole::Viewer).await;
        let (other_org, _) = second_org(&app, "other", "owner@other.test").await;
        let now = app.state.clock.now_ns();
        let other_group = app
            .state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    let row = scim_store::insert_group(conn, other_org, "Other", None, now).await?;
                    Ok(row.id)
                })
            })
            .await
            .unwrap();
        let path = format!("/scim/v2/Groups/{other_group}");
        assert_eq!(get(&app, &token, &path).await.status, StatusCode::NOT_FOUND);
        let deleted = app
            .request(Method::DELETE, &path)
            .header("authorization", &bearer(&token))
            .send()
            .await;
        assert_eq!(deleted.status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn patch_add_and_remove_recompute_roles() {
        let app = TestApp::with_license(&["scim", "org_sso"]).await;
        let seeded = app.seed().await;
        let token = scim_org(&app, &seeded, "example.com", OrgRole::Viewer).await;
        let ada = create_user(&app, &token, "grace@example.com").await;
        let ada_id = id_of(&ada);
        let ada_account = account_of(&app, &ada_id).await;
        let admins = create(&app, &token, "Admins", &[]).await;
        let admins_id = id_of(&admins);
        map_group(&app, &admins_id, OrgRole::Admin).await;

        let added = patch(
            &app,
            &token,
            &admins_id,
            json!([{"op": "add", "path": "members", "value": [{"value": ada_id}]}]),
        )
        .await;
        assert_eq!(added.status, StatusCode::OK);
        assert_eq!(
            role_of(&app, seeded.org_id, ada_account).await,
            Some(OrgRole::Admin)
        );

        // Entra's removal: the path `members` with the values to remove.
        let removed = patch(
            &app,
            &token,
            &admins_id,
            json!([{"op": "remove", "path": "members", "value": [{"value": ada_id}]}]),
        )
        .await;
        assert_eq!(removed.status, StatusCode::OK);
        let body: Value = removed.json();
        assert_eq!(body["members"].as_array().unwrap().len(), 0);
        assert_eq!(
            role_of(&app, seeded.org_id, ada_account).await,
            Some(OrgRole::Viewer)
        );
    }

    #[tokio::test]
    async fn put_without_members_keeps_them() {
        let app = TestApp::with_license(&["scim", "org_sso"]).await;
        let seeded = app.seed().await;
        let token = scim_org(&app, &seeded, "example.com", OrgRole::Viewer).await;
        let ada = create_user(&app, &token, "ada@example.com").await;
        let ada_id = id_of(&ada);
        let old = create(&app, &token, "Old", &[&ada_id]).await;
        let old_id = id_of(&old);

        let response = scim_request(
            &app,
            &token,
            Method::PUT,
            &format!("/scim/v2/Groups/{old_id}"),
            &json!({"schemas": [GROUP_SCHEMA], "displayName": "New"}),
        )
        .await;
        assert_eq!(response.status, StatusCode::OK);
        let body: Value = response.json();
        assert_eq!(body["displayName"], "New");
        assert_eq!(body["members"][0]["value"], ada_id);
    }

    #[tokio::test]
    async fn delete_lowers_members_to_their_remaining_groups() {
        let app = TestApp::with_license(&["scim", "org_sso"]).await;
        let seeded = app.seed().await;
        let token = scim_org(&app, &seeded, "example.com", OrgRole::Viewer).await;
        let ada = create_user(&app, &token, "grace@example.com").await;
        let ada_id = id_of(&ada);
        let ada_account = account_of(&app, &ada_id).await;
        let admins_id = id_of(&create(&app, &token, "Admins", &[]).await);
        let staff_id = id_of(&create(&app, &token, "Staff", &[]).await);
        map_group(&app, &admins_id, OrgRole::Admin).await;
        map_group(&app, &staff_id, OrgRole::Member).await;

        for group in [&admins_id, &staff_id] {
            let added = patch(
                &app,
                &token,
                group,
                json!([{"op": "add", "path": "members", "value": [{"value": ada_id}]}]),
            )
            .await;
            assert_eq!(added.status, StatusCode::OK);
        }
        assert_eq!(
            role_of(&app, seeded.org_id, ada_account).await,
            Some(OrgRole::Admin)
        );

        let deleted = app
            .request(Method::DELETE, &format!("/scim/v2/Groups/{admins_id}"))
            .header("authorization", &bearer(&token))
            .send()
            .await;
        assert_eq!(deleted.status, StatusCode::NO_CONTENT);
        assert_eq!(
            role_of(&app, seeded.org_id, ada_account).await,
            Some(OrgRole::Member)
        );
    }

    #[tokio::test]
    async fn lapsed_org_removes_members_but_cannot_add() {
        let app = TestApp::with_license(&["org_sso"]).await;
        let seeded = app.seed().await;
        let token = scim_org(&app, &seeded, "example.com", OrgRole::Viewer).await;
        let (ada, _) = provision(&app, seeded.org_id, "grace@example.com").await;
        let (bob, _) = provision(&app, seeded.org_id, "bob@example.com").await;
        let now = app.state.clock.now_ns();
        let team = app
            .state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    let row =
                        scim_store::insert_group(conn, seeded.org_id, "Team", None, now).await?;
                    scim_store::add_group_members_tx(conn, seeded.org_id, row.id, &[ada]).await?;
                    Ok(row.id)
                })
            })
            .await
            .unwrap();
        let team_id = team.to_string();

        let added = patch(
            &app,
            &token,
            &team_id,
            json!([{"op": "add", "path": "members", "value": [{"value": bob.to_string()}]}]),
        )
        .await;
        assert_eq!(added.status, StatusCode::FORBIDDEN);
        assert_eq!(
            create(&app, &token, "New", &[]).await.status,
            StatusCode::FORBIDDEN
        );

        let removed = patch(
            &app,
            &token,
            &team_id,
            json!([{"op": "remove", "path": "members", "value": [{"value": ada.to_string()}]}]),
        )
        .await;
        assert_eq!(removed.status, StatusCode::OK);
        let body: Value = removed.json();
        assert_eq!(body["members"].as_array().unwrap().len(), 0);

        let deleted = app
            .request(Method::DELETE, &format!("/scim/v2/Groups/{team_id}"))
            .header("authorization", &bearer(&token))
            .send()
            .await;
        assert_eq!(deleted.status, StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn every_group_operation_is_recorded() {
        let app = TestApp::with_license(&["scim", "org_sso"]).await;
        let seeded = app.seed().await;
        let token = scim_org(&app, &seeded, "example.com", OrgRole::Viewer).await;
        let ada = create_user(&app, &token, "ada@example.com").await;
        let ada_id = id_of(&ada);
        let team_id = id_of(&create(&app, &token, "Team", &[]).await);
        let added = patch(
            &app,
            &token,
            &team_id,
            json!([{"op": "add", "path": "members", "value": [{"value": ada_id}]}]),
        )
        .await;
        assert_eq!(added.status, StatusCode::OK);
        let renamed = scim_request(
            &app,
            &token,
            Method::PUT,
            &format!("/scim/v2/Groups/{team_id}"),
            &json!({"schemas": [GROUP_SCHEMA], "displayName": "Renamed"}),
        )
        .await;
        assert_eq!(renamed.status, StatusCode::OK);
        let deleted = app
            .request(Method::DELETE, &format!("/scim/v2/Groups/{team_id}"))
            .header("authorization", &bearer(&token))
            .send()
            .await;
        assert_eq!(deleted.status, StatusCode::NO_CONTENT);

        assert_eq!(audit_count(&app, "scim.group_created").await, 1);
        assert_eq!(audit_count(&app, "scim.group_members_changed").await, 1);
        assert_eq!(audit_count(&app, "scim.group_updated").await, 1);
        assert_eq!(audit_count(&app, "scim.group_deleted").await, 1);
    }
}
