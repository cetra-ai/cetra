// SPDX-License-Identifier: Elastic-2.0
//! The SCIM Users resource and the discovery documents (RFC 7643 §§ 4.1, 6, 7; RFC 7644
//! §§ 3.2 to 3.6, track EA M5, D-EA-12 to D-EA-19).
//!
//! Every handler starts with [`admit`]. A write runs in one `meta.write`. A refusal found before
//! the first write is returned as `Ok(Err(ScimError))` from inside the transaction, so nothing is
//! written; a `MetaError` (including the seat and last-owner conflicts) rolls the write back.

use axum::body::Bytes;
use axum::extract::{Extension, Path, Query, State};
use axum::http::{Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{MethodRouter, delete, get, patch, post, put};
use cetra_core::ids::OrgId;
use serde::Deserialize;
use serde_json::Value;
use sqlx::SqliteConnection;
use uuid::Uuid;

use super::domains::{in_domains, scim_domains_tx};
use super::error::{ScimError, ScimJson};
use super::filter::parse_user_filter;
use super::patch::{UserChange, map_changes, user_changes};
use super::schema::{
    list_response, resource_types, schemas, service_provider_config, user_resource,
};
use super::store::{self as scim_store, ScimUserFields, ScimUserRow};
use crate::api::entitlements::Feature;
use crate::audit::catalog::ScimAction;
use crate::audit::{self, Event, TargetKind};
use crate::auth::principal::Principal;
use crate::auth::{OrgRole, org_role_str};
use crate::http::AppState;
use crate::http::access::{Access, RouteDef};
use crate::meta::MetaError;
use crate::meta::members;
use crate::meta::users::{self, EmailVerifiedVia, NewUser, normalize_email};

const NOT_ENTITLED: &str = "SCIM provisioning needs the Enterprise plan or licence.";

/// The `/scim/v2` routes. None is documented in the OpenAPI document.
pub(crate) fn routes() -> Vec<RouteDef> {
    vec![
        route(Method::GET, "/scim/v2/ServiceProviderConfig", get(discovery_config)),
        route(Method::GET, "/scim/v2/ResourceTypes", get(resource_types_doc)),
        route(Method::GET, "/scim/v2/Schemas", get(schemas_doc)),
        route(Method::GET, "/scim/v2/Users", get(list_users)),
        route(Method::POST, "/scim/v2/Users", post(create_user)),
        route(Method::GET, "/scim/v2/Users/{id}", get(get_user)),
        route(Method::PUT, "/scim/v2/Users/{id}", put(replace_user)),
        route(Method::PATCH, "/scim/v2/Users/{id}", patch(patch_user)),
        route(Method::DELETE, "/scim/v2/Users/{id}", delete(delete_user_row)),
    ]
}

fn route(method: Method, path: &'static str, handler: MethodRouter<AppState>) -> RouteDef {
    RouteDef {
        method,
        path,
        access: Access::Scim,
        documented: false,
        handler,
    }
}

/// The checks every SCIM handler starts with: the token's rate limit (D-EA-15) and SCIM being on
/// for the org. Returns the org and its default role (P-4). The entitlement is checked per
/// operation, so a lapsed org still deprovisions (D-EA-19).
async fn admit(state: &AppState, principal: &Principal) -> Result<(OrgId, OrgRole), ScimError> {
    let Principal::Scim { org_id, token_id } = principal else {
        return Err(ScimError::forbidden("SCIM is turned off for this organization."));
    };
    let now = state.clock.now_ns();
    state
        .limits
        .scim
        .check(token_id, now)
        .map_err(|retry| ScimError::rate_limited(retry.0.as_secs().max(1)))?;
    let scim = scim_store::org_scim(state.meta.reader(), *org_id).await?;
    if !scim.enabled {
        return Err(ScimError::forbidden("SCIM is turned off for this organization."));
    }
    Ok((*org_id, scim.default_role))
}

/// The per-request values a write needs, read before the transaction.
#[derive(Clone, Copy)]
struct Scope {
    org: OrgId,
    now: i64,
    default_role: OrgRole,
    max_seats: Option<u64>,
    entitled: bool,
}

fn scope_of(state: &AppState, org: OrgId, default_role: OrgRole) -> Scope {
    let now = state.clock.now_ns();
    Scope {
        org,
        now,
        default_role,
        max_seats: state.entitlements.resolve(org, now).limits.max_seats,
        entitled: crate::entitlements::of(state, org).allows(Feature::Scim),
    }
}

/// `<public_url>/scim/v2`, without a trailing slash.
fn base_url(state: &AppState) -> String {
    state.config.public_url.join("/scim/v2")
}

fn parse_id(id: &str) -> Result<Uuid, ScimError> {
    Uuid::parse_str(id).map_err(|_| ScimError::not_found())
}

/// A request body that is a JSON object. Anything else is `invalidSyntax`.
fn json_object(body: &[u8]) -> Result<Value, ScimError> {
    match serde_json::from_slice::<Value>(body) {
        Ok(value @ Value::Object(_)) => Ok(value),
        _ => Err(ScimError::invalid_syntax()),
    }
}

/// A POST or PUT body must name the userName as a string.
fn require_user_name(body: &Value) -> Result<(), ScimError> {
    if body.get("userName").is_some_and(Value::is_string) {
        Ok(())
    } else {
        Err(ScimError::invalid_value("userName is required"))
    }
}

/// Maps a write's `MetaError` to the SCIM error. The seat conflict is a refusal by the plan.
fn write_error(error: MetaError) -> ScimError {
    match error {
        MetaError::Conflict("seat limit") => ScimError::forbidden("The org has no seat left."),
        other => ScimError::from(other),
    }
}

/// Drops the revoked PAT hashes from the cache after the commit.
fn evict(state: &AppState, hashes: &[String]) {
    for hash in hashes {
        state.pat_cache.evict(hash);
    }
}

fn normalized(raw: &str) -> Result<String, ScimError> {
    normalize_email(raw).map_err(|_| ScimError::invalid_value("userName must be an email address"))
}

/// The attributes of one SCIM user as a write needs them. `user_name` is normalized.
#[derive(Clone)]
struct Attrs {
    user_name: String,
    active: bool,
    external_id: Option<String>,
    given_name: Option<String>,
    family_name: Option<String>,
    display_name: Option<String>,
    department: Option<String>,
    employee_number: Option<String>,
}

impl Attrs {
    /// Every attribute absent. A PUT starts here (absent attributes are cleared); a POST starts
    /// here too, with `active` true.
    fn blank() -> Self {
        Self {
            user_name: String::new(),
            active: true,
            external_id: None,
            given_name: None,
            family_name: None,
            display_name: None,
            department: None,
            employee_number: None,
        }
    }

    /// The stored attributes of `row`. A PATCH starts here.
    fn of_row(row: &ScimUserRow) -> Self {
        Self {
            user_name: row.user_name.clone(),
            active: row.active,
            external_id: row.external_id.clone(),
            given_name: row.given_name.clone(),
            family_name: row.family_name.clone(),
            display_name: row.display_name.clone(),
            department: row.department.clone(),
            employee_number: row.employee_number.clone(),
        }
    }

    fn fields(&self) -> ScimUserFields {
        ScimUserFields {
            user_name: self.user_name.clone(),
            external_id: self.external_id.clone(),
            given_name: self.given_name.clone(),
            family_name: self.family_name.clone(),
            display_name: self.display_name.clone(),
            department: self.department.clone(),
            employee_number: self.employee_number.clone(),
        }
    }

    /// The RFC 7643 names of the attributes that differ from `row`. `active` is not one.
    fn changed_from(&self, row: &ScimUserRow) -> Vec<&'static str> {
        let differences = [
            ("userName", self.user_name != row.user_name),
            ("externalId", self.external_id != row.external_id),
            ("name.givenName", self.given_name != row.given_name),
            ("name.familyName", self.family_name != row.family_name),
            ("displayName", self.display_name != row.display_name),
            ("department", self.department != row.department),
            ("employeeNumber", self.employee_number != row.employee_number),
        ];
        differences
            .into_iter()
            .filter(|(_, changed)| *changed)
            .map(|(name, _)| name)
            .collect()
    }
}

/// Applies `changes` to `attrs`. Returns the userName the changes name, if any. A userName and an
/// email that differ after normalization are refused.
fn apply_changes(attrs: &mut Attrs, changes: Vec<UserChange>) -> Result<Option<String>, ScimError> {
    let mut user_name = None;
    let mut email = None;
    for change in changes {
        match change {
            UserChange::Active(active) => attrs.active = active,
            UserChange::UserName(value) => user_name = Some(value),
            UserChange::Email(value) => email = Some(value),
            UserChange::ExternalId(value) => attrs.external_id = value,
            UserChange::GivenName(value) => attrs.given_name = value,
            UserChange::FamilyName(value) => attrs.family_name = value,
            UserChange::DisplayName(value) => attrs.display_name = value,
            UserChange::Department(value) => attrs.department = value,
            UserChange::EmployeeNumber(value) => attrs.employee_number = value,
        }
    }
    match (user_name, email) {
        (Some(name), Some(address)) => {
            if normalized(&name)? != normalized(&address)? {
                return Err(ScimError::invalid_value("emails must match userName"));
            }
            Ok(Some(name))
        }
        (name, address) => Ok(name.or(address)),
    }
}

/// The display name of D-EA-13: `displayName`, else the given and family names, trimmed. `None`
/// when there is neither, which leaves the name unchanged.
fn name_of(attrs: &Attrs) -> Option<String> {
    let display = attrs
        .display_name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty());
    if let Some(display) = display {
        return Some(display.to_owned());
    }
    let joined = [attrs.given_name.as_deref(), attrs.family_name.as_deref()]
        .into_iter()
        .flatten()
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    (!joined.is_empty()).then_some(joined)
}

/// The org memberships of `user` in other orgs.
async fn memberships_elsewhere_tx(
    conn: &mut SqliteConnection,
    org: OrgId,
    user: Uuid,
) -> Result<i64, MetaError> {
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM org_members WHERE user_id = ? AND org_id != ?")
            .bind(user.to_string())
            .bind(org.to_string())
            .fetch_one(&mut *conn)
            .await?;
    Ok(count)
}

/// The name rule (D-EA-13): the user's name follows the SCIM name only when the user belongs to
/// no other org.
async fn sync_name_tx(
    conn: &mut SqliteConnection,
    org: OrgId,
    user: Uuid,
    attrs: &Attrs,
    now: i64,
) -> Result<(), MetaError> {
    let Some(name) = name_of(attrs) else {
        return Ok(());
    };
    if memberships_elsewhere_tx(conn, org, user).await? == 0 {
        users::set_name(conn, user, Some(name.as_str()), now).await?;
    }
    Ok(())
}

/// The deactivation of an active row (step 10, D-EA-16, P-6, P-7). Removes the user's membership
/// of `org` (a last owner is refused), and for a user who is not an instance admin revokes the
/// sessions and PATs, and disables the account when no membership is left. Returns the revoked
/// PAT hashes. Runs in the caller's write transaction.
async fn deactivate_tx(
    conn: &mut SqliteConnection,
    ctx: &audit::Ctx,
    org: OrgId,
    row: &ScimUserRow,
    now: i64,
) -> Result<Vec<String>, MetaError> {
    let user = users::user_by_id_tx(conn, row.user_id)
        .await?
        .ok_or(MetaError::Corrupt {
            table: "scim_users",
            column: "user_id",
        })?;
    let role = members::org_role_tx(conn, org, user.id).await?;
    if role.is_some() {
        members::remove_org_member(conn, org, user.id).await?;
        if role == Some(OrgRole::Owner) && members::count_active_owners(conn, org).await? == 0 {
            return Err(MetaError::Conflict("an org needs an owner"));
        }
    }
    let mut disabled_account = row.disabled_account;
    let hashes = if user.is_instance_admin {
        Vec::new()
    } else {
        let hashes = users::revoke_access_tx(conn, user.id, now).await?;
        let memberships: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM org_members WHERE user_id = ?")
                .bind(user.id.to_string())
                .fetch_one(&mut *conn)
                .await?;
        if memberships == 0 && user.disabled_at.is_none() {
            users::set_disabled(conn, user.id, Some(now), now).await?;
            disabled_account = true;
        }
        hashes
    };
    scim_store::set_user_active(conn, org, row.id, false, disabled_account, now).await?;
    audit::record(
        conn,
        ctx,
        Event::new(ScimAction::UserDeactivated {
            scim_user_id: row.id,
            account_disabled: disabled_account,
        })
        .org(org)
        .target(TargetKind::User, user.id),
        now,
    )
    .await?;
    Ok(hashes)
}

/// POST: provisions one user. The refusals come before any write.
async fn create_tx(
    conn: &mut SqliteConnection,
    state: &AppState,
    ctx: &audit::Ctx,
    scope: Scope,
    attrs: Attrs,
) -> Result<Result<(ScimUserRow, Vec<String>), ScimError>, MetaError> {
    let Scope {
        org,
        now,
        default_role,
        max_seats,
        ..
    } = scope;
    let domains = scim_domains_tx(state, conn, org, now).await?;
    if !in_domains(&domains, &attrs.user_name) {
        return Ok(Err(ScimError::invalid_value(
            "userName is not in a verified domain of this organization",
        )));
    }
    if scim_store::user_by_user_name_tx(conn, org, &attrs.user_name)
        .await?
        .is_some()
    {
        return Err(MetaError::Conflict("uniqueness"));
    }
    if let Some(external_id) = attrs.external_id.as_deref() {
        if scim_store::user_by_external_id_tx(conn, org, external_id)
            .await?
            .is_some()
        {
            return Err(MetaError::Conflict("uniqueness"));
        }
    }

    // Link (D-P-66): an existing account is linked, and an unproven one is reset first. An
    // instance admin is linked without any credential change (P-6).
    let (user_id, linked, mut hashes) = match users::user_by_email_tx(conn, &attrs.user_name).await?
    {
        Some(user) if user.disabled_at.is_some() => {
            return Ok(Err(ScimError::conflict("This account is disabled.")));
        }
        Some(user) => {
            let hashes = if user.is_instance_admin {
                Vec::new()
            } else {
                users::reset_unproven_tx(conn, &user, now).await?
            };
            (user.id, true, hashes)
        }
        None => {
            let user = users::insert_user(
                conn,
                NewUser {
                    email: attrs.user_name.clone(),
                    name: None,
                    password_hash: None,
                    email_verified_at: Some(now),
                    email_verified_via: Some(EmailVerifiedVia::Idp),
                    is_instance_admin: false,
                },
                now,
            )
            .await?;
            (user.id, false, Vec::new())
        }
    };

    if attrs.active {
        users::ensure_member_tx(conn, org, user_id, default_role, max_seats, now).await?;
    }
    sync_name_tx(conn, org, user_id, &attrs, now).await?;
    let row = scim_store::insert_user(conn, org, user_id, &attrs.fields(), now).await?;
    audit::record(
        conn,
        ctx,
        Event::new(ScimAction::UserProvisioned {
            scim_user_id: row.id,
            role: org_role_str(default_role),
            linked,
        })
        .org(org)
        .target(TargetKind::User, user_id),
        now,
    )
    .await?;
    // A user created inactive is deactivated at once: no membership, and the account is
    // disabled when nothing else holds it.
    if !attrs.active {
        hashes.extend(deactivate_tx(conn, ctx, org, &row, now).await?);
    }
    let row = scim_store::user_tx(conn, org, row.id)
        .await?
        .ok_or(MetaError::Corrupt {
            table: "scim_users",
            column: "id",
        })?;
    Ok(Ok((row, hashes)))
}

/// A PUT or PATCH: the attribute changes, the email rule, the lapse rule, reactivation and
/// deactivation, in one transaction. Every refusal is found before the first write.
struct Update {
    id: Uuid,
    /// PUT: the body is the whole resource and absent attributes are cleared. PATCH: `changes`
    /// applies to the stored attributes.
    replace: bool,
    changes: Vec<UserChange>,
}

async fn update_tx(
    conn: &mut SqliteConnection,
    state: &AppState,
    ctx: &audit::Ctx,
    scope: Scope,
    update: Update,
) -> Result<Result<(ScimUserRow, Vec<String>), ScimError>, MetaError> {
    let Scope {
        org,
        now,
        default_role,
        max_seats,
        entitled,
    } = scope;
    let Some(row) = scim_store::user_tx(conn, org, update.id).await? else {
        return Ok(Err(ScimError::not_found()));
    };
    let mut attrs = if update.replace {
        Attrs::blank()
    } else {
        Attrs::of_row(&row)
    };
    let raw = match apply_changes(&mut attrs, update.changes) {
        Ok(raw) => raw,
        Err(error) => return Ok(Err(error)),
    };
    if let Some(raw) = raw {
        attrs.user_name = match normalized(&raw) {
            Ok(name) => name,
            Err(error) => return Ok(Err(error)),
        };
    }
    let changed = attrs.changed_from(&row);
    // Lapse (D-EA-19): only a deactivation gets through.
    if !entitled && (attrs.active || !changed.is_empty()) {
        return Ok(Err(ScimError::forbidden(NOT_ENTITLED)));
    }

    let domains = scim_domains_tx(state, conn, org, now).await?;
    let email_changed = attrs.user_name != row.user_name;
    if email_changed {
        let within = in_domains(&domains, &row.user_name) && in_domains(&domains, &attrs.user_name);
        let taken = users::user_by_email_tx(conn, &attrs.user_name)
            .await?
            .is_some()
            || scim_store::user_by_user_name_tx(conn, org, &attrs.user_name)
                .await?
                .is_some();
        let elsewhere = memberships_elsewhere_tx(conn, org, row.user_id).await? > 0;
        if !within || taken || elsewhere {
            return Ok(Err(ScimError::mutability(
                "userName can only change within the organization's verified domains",
            )));
        }
    }
    let reactivating = !row.active && attrs.active;
    if reactivating && !in_domains(&domains, &attrs.user_name) {
        return Ok(Err(ScimError::invalid_value(
            "userName is not in a verified domain of this organization",
        )));
    }

    if email_changed {
        users::set_email_tx(conn, row.user_id, &attrs.user_name, now).await?;
    }
    if reactivating {
        users::ensure_member_tx(conn, org, row.user_id, default_role, max_seats, now).await?;
        if row.disabled_account {
            users::set_disabled(conn, row.user_id, None, now).await?;
        }
        scim_store::set_user_active(conn, org, row.id, true, false, now).await?;
        audit::record(
            conn,
            ctx,
            Event::new(ScimAction::UserReactivated {
                scim_user_id: row.id,
            })
            .org(org)
            .target(TargetKind::User, row.user_id),
            now,
        )
        .await?;
    }
    let hashes = if row.active && !attrs.active {
        deactivate_tx(conn, ctx, org, &row, now).await?
    } else {
        Vec::new()
    };
    if !changed.is_empty() {
        scim_store::update_user(conn, org, row.id, &attrs.fields(), now).await?;
        sync_name_tx(conn, org, row.user_id, &attrs, now).await?;
        audit::record(
            conn,
            ctx,
            Event::new(ScimAction::UserUpdated {
                scim_user_id: row.id,
                fields: changed,
            })
            .org(org)
            .target(TargetKind::User, row.user_id),
            now,
        )
        .await?;
    }
    let row = scim_store::user_tx(conn, org, row.id)
        .await?
        .ok_or(MetaError::Corrupt {
            table: "scim_users",
            column: "id",
        })?;
    Ok(Ok((row, hashes)))
}

async fn run_update(
    state: &AppState,
    ctx: audit::Ctx,
    scope: Scope,
    update: Update,
) -> Result<Response, ScimError> {
    let state_for_write = state.clone();
    let (row, hashes) = state
        .meta
        .write(move |conn| {
            Box::pin(async move {
                update_tx(conn, &state_for_write, &ctx, scope, update).await
            })
        })
        .await
        .map_err(write_error)??;
    evict(state, &hashes);
    Ok(ScimJson(user_resource(&row, &base_url(state))).into_response())
}

async fn discovery_config(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
) -> Result<Response, ScimError> {
    admit(&state, &principal).await?;
    Ok(ScimJson(service_provider_config(&base_url(&state))).into_response())
}

async fn resource_types_doc(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
) -> Result<Response, ScimError> {
    admit(&state, &principal).await?;
    Ok(ScimJson(resource_types(&base_url(&state))).into_response())
}

async fn schemas_doc(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
) -> Result<Response, ScimError> {
    admit(&state, &principal).await?;
    Ok(ScimJson(schemas()).into_response())
}

/// The `GET /Users` query. Every value is read as text, so a malformed value gets a default
/// rather than a non-SCIM rejection.
#[derive(Deserialize)]
struct ListQuery {
    filter: Option<String>,
    #[serde(rename = "startIndex")]
    start_index: Option<String>,
    count: Option<String>,
}

async fn list_users(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Query(query): Query<ListQuery>,
) -> Result<Response, ScimError> {
    let (org, _) = admit(&state, &principal).await?;
    let start_index = query
        .start_index
        .as_deref()
        .and_then(|value| value.parse::<i64>().ok())
        .unwrap_or(1)
        .max(1);
    let count = query
        .count
        .as_deref()
        .and_then(|value| value.parse::<i64>().ok())
        .unwrap_or(100)
        .clamp(0, 200);
    let filter = query
        .filter
        .as_deref()
        .map(parse_user_filter)
        .transpose()?;
    let start = u32::try_from(start_index).unwrap_or(u32::MAX);
    let count = u32::try_from(count).unwrap_or(0);
    let (total, rows) =
        scim_store::list_users(state.meta.reader(), org, filter.as_ref(), start, count).await?;
    let base = base_url(&state);
    let resources: Vec<Value> = rows.iter().map(|row| user_resource(row, &base)).collect();
    Ok(ScimJson(list_response(total, start, resources)).into_response())
}

/// The org's SCIM user with `id`, or a SCIM 404.
async fn load(state: &AppState, org: OrgId, id: &str) -> Result<ScimUserRow, ScimError> {
    let id = parse_id(id)?;
    scim_store::user(state.meta.reader(), org, id)
        .await?
        .ok_or_else(ScimError::not_found)
}

async fn get_user(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<String>,
) -> Result<Response, ScimError> {
    let (org, _) = admit(&state, &principal).await?;
    let row = load(&state, org, &id).await?;
    Ok(ScimJson(user_resource(&row, &base_url(&state))).into_response())
}

async fn create_user(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    ctx: audit::Ctx,
    body: Bytes,
) -> Result<Response, ScimError> {
    let (org, default_role) = admit(&state, &principal).await?;
    let scope = scope_of(&state, org, default_role);
    if !scope.entitled {
        return Err(ScimError::forbidden(NOT_ENTITLED));
    }
    let body = json_object(&body)?;
    require_user_name(&body)?;
    let mut attrs = Attrs::blank();
    let raw = apply_changes(&mut attrs, map_changes(&body)?)?
        .ok_or_else(|| ScimError::invalid_value("userName is required"))?;
    attrs.user_name = normalized(&raw)?;

    let state_for_write = state.clone();
    let written = state
        .meta
        .write(move |conn| {
            Box::pin(async move {
                create_tx(conn, &state_for_write, &ctx, scope, attrs).await
            })
        })
        .await
        .map_err(write_error)?;
    let (row, hashes) = written?;
    evict(&state, &hashes);

    let base = base_url(&state);
    let location = format!("{base}/Users/{}", row.id);
    Ok((
        StatusCode::CREATED,
        [(header::LOCATION, location)],
        ScimJson(user_resource(&row, &base)),
    )
        .into_response())
}

async fn replace_user(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    ctx: audit::Ctx,
    Path(id): Path<String>,
    body: Bytes,
) -> Result<Response, ScimError> {
    let (org, default_role) = admit(&state, &principal).await?;
    let id = parse_id(&id)?;
    let body = json_object(&body)?;
    require_user_name(&body)?;
    let changes = map_changes(&body)?;
    let update = Update {
        id,
        replace: true,
        changes,
    };
    run_update(&state, ctx, scope_of(&state, org, default_role), update).await
}

async fn patch_user(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    ctx: audit::Ctx,
    Path(id): Path<String>,
    body: Bytes,
) -> Result<Response, ScimError> {
    let (org, default_role) = admit(&state, &principal).await?;
    let id = parse_id(&id)?;
    let body = json_object(&body)?;
    let changes = user_changes(&body)?;
    let update = Update {
        id,
        replace: false,
        changes,
    };
    run_update(&state, ctx, scope_of(&state, org, default_role), update).await
}

/// DELETE: deactivates an active user, then removes the row. Works when lapsed (D-EA-19).
async fn delete_user_row(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    ctx: audit::Ctx,
    Path(id): Path<String>,
) -> Result<Response, ScimError> {
    let (org, _) = admit(&state, &principal).await?;
    let id = parse_id(&id)?;
    let now = state.clock.now_ns();
    let written = state
        .meta
        .write(move |conn| {
            Box::pin(async move {
                let Some(row) = scim_store::user_tx(conn, org, id).await? else {
                    return Ok(Err(ScimError::not_found()));
                };
                let hashes = if row.active {
                    deactivate_tx(conn, &ctx, org, &row, now).await?
                } else {
                    Vec::new()
                };
                scim_store::delete_user(conn, org, id).await?;
                audit::record(
                    conn,
                    &ctx,
                    Event::new(ScimAction::UserDeleted { scim_user_id: id })
                        .org(org)
                        .target(TargetKind::User, row.user_id),
                    now,
                )
                .await?;
                Ok(Ok(hashes))
            })
        })
        .await
        .map_err(write_error)?;
    let hashes = written?;
    evict(&state, &hashes);
    Ok(StatusCode::NO_CONTENT.into_response())
}

#[cfg(test)]
mod tests {
    use axum::http::{Method, StatusCode};
    use serde_json::{Value, json};
    use uuid::Uuid;

    use super::super::error::{CONTENT_TYPE, ERROR_SCHEMA};
    use super::super::schema::USER_SCHEMA;
    use crate::auth::OrgRole;
    use crate::auth::secret::{self, KeyKind};
    use crate::ee::org_sso::store::{DomainRow, insert_domain};
    use crate::ee::scim::store::{insert_token, put_org_scim};
    use crate::test_support::{Seeded, TestApp};

    const PATCH_OP: &str = "urn:ietf:params:scim:api:messages:2.0:PatchOp";

    /// SCIM on for the seeded org, `domain` verified, and a live SCIM token. Returns the token.
    async fn scim_org(app: &TestApp, seeded: &Seeded, domain: &str) -> String {
        let now = app.state.clock.now_ns();
        let key = secret::new_key(KeyKind::Scim).unwrap();
        let (prefix, hash, plaintext) = (key.prefix.clone(), key.hash.clone(), key.plaintext.clone());
        let (org, owner, domain) = (seeded.org_id, seeded.user_id, domain.to_owned());
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    put_org_scim(conn, org, true, OrgRole::Viewer, now).await?;
                    insert_token(conn, org, "test", &prefix, &hash, owner, now).await?;
                    insert_domain(
                        conn,
                        &DomainRow {
                            id: Uuid::now_v7(),
                            org_id: org,
                            domain,
                            token: "token".to_owned(),
                            verified_at: Some(now),
                            last_checked_at: Some(now),
                            created_at: now,
                        },
                    )
                    .await
                })
            })
            .await
            .unwrap();
        plaintext
    }

    fn bearer(token: &str) -> String {
        format!("Bearer {token}")
    }

    async fn create(app: &TestApp, token: &str, user_name: &str) -> crate::test_support::TestResponse {
        app.request(Method::POST, "/scim/v2/Users")
            .header("authorization", &bearer(token))
            .json(&json!({"schemas": [USER_SCHEMA], "userName": user_name}))
            .send()
            .await
    }

    async fn scalar_text(app: &TestApp, sql: &str, value: &str) -> String {
        sqlx::query_scalar::<_, String>(sql)
            .bind(value.to_owned())
            .fetch_one(app.state.meta.reader())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn discovery_documents_are_scim_json() {
        let app = TestApp::with_license(&["scim", "org_sso"]).await;
        let seeded = app.seed().await;
        let token = scim_org(&app, &seeded, "example.com").await;
        let response = app
            .request(Method::GET, "/scim/v2/ServiceProviderConfig")
            .header("authorization", &bearer(&token))
            .send()
            .await;
        assert_eq!(response.status, StatusCode::OK);
        assert_eq!(response.header("content-type"), Some(CONTENT_TYPE));
        let body: Value = response.json();
        assert!(body["schemas"].is_array());
    }

    #[tokio::test]
    async fn create_provisions_a_new_viewer_without_password() {
        let app = TestApp::with_license(&["scim", "org_sso"]).await;
        let seeded = app.seed().await;
        let token = scim_org(&app, &seeded, "example.com").await;
        let response = create(&app, &token, "Grace@Example.com").await;
        assert_eq!(response.status, StatusCode::CREATED);
        assert!(
            response
                .header("location")
                .is_some_and(|location| location.contains("/scim/v2/Users/"))
        );
        let role = scalar_text(
            &app,
            "SELECT om.role FROM org_members om JOIN users u ON u.id = om.user_id \
             WHERE u.email = ?",
            "grace@example.com",
        )
        .await;
        assert_eq!(role, "viewer");
        let verified = scalar_text(
            &app,
            "SELECT email_verified_via FROM users WHERE email = ?",
            "grace@example.com",
        )
        .await;
        assert_eq!(verified, "idp");
    }

    #[tokio::test]
    async fn create_outside_verified_domains_is_invalid_value() {
        let app = TestApp::with_license(&["scim", "org_sso"]).await;
        let seeded = app.seed().await;
        let token = scim_org(&app, &seeded, "example.com").await;
        let response = create(&app, &token, "x@other.org").await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST);
        let body: Value = response.json();
        assert_eq!(body["scimType"], "invalidValue");
        assert_eq!(body["status"], "400");
    }

    #[tokio::test]
    async fn create_duplicate_user_name_is_uniqueness_409() {
        let app = TestApp::with_license(&["scim", "org_sso"]).await;
        let seeded = app.seed().await;
        let token = scim_org(&app, &seeded, "example.com").await;
        assert_eq!(create(&app, &token, "a@example.com").await.status, StatusCode::CREATED);
        let response = create(&app, &token, "a@example.com").await;
        assert_eq!(response.status, StatusCode::CONFLICT);
        let body: Value = response.json();
        assert_eq!(body["scimType"], "uniqueness");
    }

    #[tokio::test]
    async fn create_when_lapsed_is_403_scim_error() {
        let app = TestApp::with_license(&["org_sso"]).await;
        let seeded = app.seed().await;
        let token = scim_org(&app, &seeded, "example.com").await;
        let response = create(&app, &token, "a@example.com").await;
        assert_eq!(response.status, StatusCode::FORBIDDEN);
        let body: Value = response.json();
        assert_eq!(body["schemas"][0], ERROR_SCHEMA);
        assert_eq!(body["status"], "403");
    }

    #[tokio::test]
    async fn list_filters_by_user_name_and_count_zero_returns_only_the_total() {
        let app = TestApp::with_license(&["scim", "org_sso"]).await;
        let seeded = app.seed().await;
        let token = scim_org(&app, &seeded, "example.com").await;
        assert_eq!(create(&app, &token, "a@example.com").await.status, StatusCode::CREATED);
        assert_eq!(create(&app, &token, "b@example.com").await.status, StatusCode::CREATED);

        let filtered = app
            .request(
                Method::GET,
                "/scim/v2/Users?filter=userName%20eq%20%22a%40example.com%22",
            )
            .header("authorization", &bearer(&token))
            .send()
            .await;
        let body: Value = filtered.json();
        assert_eq!(body["totalResults"], 1);

        let counted = app
            .request(Method::GET, "/scim/v2/Users?count=0")
            .header("authorization", &bearer(&token))
            .send()
            .await;
        let body: Value = counted.json();
        assert_eq!(body["totalResults"], 2);
        assert!(body["Resources"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn unsupported_filter_is_invalid_filter() {
        let app = TestApp::with_license(&["scim", "org_sso"]).await;
        let seeded = app.seed().await;
        let token = scim_org(&app, &seeded, "example.com").await;
        let response = app
            .request(Method::GET, "/scim/v2/Users?filter=userName%20co%20%22a%22")
            .header("authorization", &bearer(&token))
            .send()
            .await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST);
        let body: Value = response.json();
        assert_eq!(body["scimType"], "invalidFilter");
    }

    #[tokio::test]
    async fn entra_deactivate_disables_a_single_org_account_and_removes_membership() {
        let app = TestApp::with_license(&["scim", "org_sso"]).await;
        let seeded = app.seed().await;
        let token = scim_org(&app, &seeded, "example.com").await;
        assert_eq!(create(&app, &token, "a@example.com").await.status, StatusCode::CREATED);
        let body = json!({
            "schemas": [PATCH_OP],
            "Operations": [{"op": "replace", "value": {"active": "False"}}]
        });
        let listed: Value = app
            .request(Method::GET, "/scim/v2/Users?filter=userName%20eq%20%22a%40example.com%22")
            .header("authorization", &bearer(&token))
            .send()
            .await
            .json();
        let id = listed["Resources"][0]["id"].as_str().unwrap().to_owned();
        let response = app
            .request(Method::PATCH, &format!("/scim/v2/Users/{id}"))
            .header("authorization", &bearer(&token))
            .json(&body)
            .send()
            .await;
        assert_eq!(response.status, StatusCode::OK);
        let resource: Value = response.json();
        assert_eq!(resource["active"], false);

        let memberships = scalar_text(
            &app,
            "SELECT CAST(COUNT(*) AS TEXT) FROM org_members om JOIN users u ON u.id = om.user_id WHERE u.email = ?",
            "a@example.com",
        )
        .await;
        assert_eq!(memberships, "0");
        let disabled = scalar_text(
            &app,
            "SELECT CASE WHEN disabled_at IS NULL THEN 'no' ELSE 'yes' END FROM users WHERE email = ?",
            "a@example.com",
        )
        .await;
        assert_eq!(disabled, "yes");
    }

    #[tokio::test]
    async fn deactivating_the_last_owner_is_409_and_changes_nothing() {
        let app = TestApp::with_license(&["scim", "org_sso"]).await;
        let seeded = app.seed().await;
        let token = scim_org(&app, &seeded, "example.com").await;
        // The seeded owner is ada@example.com, the only owner of the org.
        assert_eq!(create(&app, &token, "ada@example.com").await.status, StatusCode::CREATED);
        let listed: Value = app
            .request(Method::GET, "/scim/v2/Users")
            .header("authorization", &bearer(&token))
            .send()
            .await
            .json();
        let id = listed["Resources"][0]["id"].as_str().unwrap().to_owned();
        let response = app
            .request(Method::DELETE, &format!("/scim/v2/Users/{id}"))
            .header("authorization", &bearer(&token))
            .send()
            .await;
        assert_eq!(response.status, StatusCode::CONFLICT);
        let body: Value = response.json();
        assert_eq!(body["detail"], "An org needs an owner.");
        let still_member = scalar_text(
            &app,
            "SELECT CAST(COUNT(*) AS TEXT) FROM org_members om JOIN users u ON u.id = om.user_id WHERE u.email = ?",
            "ada@example.com",
        )
        .await;
        assert_eq!(still_member, "1");
    }

    #[tokio::test]
    async fn delete_deactivates_then_removes_the_row() {
        let app = TestApp::with_license(&["scim", "org_sso"]).await;
        let seeded = app.seed().await;
        let token = scim_org(&app, &seeded, "example.com").await;
        assert_eq!(create(&app, &token, "a@example.com").await.status, StatusCode::CREATED);
        let listed: Value = app
            .request(Method::GET, "/scim/v2/Users")
            .header("authorization", &bearer(&token))
            .send()
            .await
            .json();
        let id = listed["Resources"][0]["id"].as_str().unwrap().to_owned();
        let deleted = app
            .request(Method::DELETE, &format!("/scim/v2/Users/{id}"))
            .header("authorization", &bearer(&token))
            .send()
            .await;
        assert_eq!(deleted.status, StatusCode::NO_CONTENT);
        let fetched = app
            .request(Method::GET, &format!("/scim/v2/Users/{id}"))
            .header("authorization", &bearer(&token))
            .send()
            .await;
        assert_eq!(fetched.status, StatusCode::NOT_FOUND);
        let body: Value = fetched.json();
        assert_eq!(body["status"], "404");
        assert_eq!(body["schemas"][0], ERROR_SCHEMA);
    }

    #[tokio::test]
    async fn unauthenticated_request_gets_a_scim_401() {
        let app = TestApp::with_license(&["scim", "org_sso"]).await;
        let response = app.request(Method::GET, "/scim/v2/Users").send().await;
        assert_eq!(response.status, StatusCode::UNAUTHORIZED);
        assert_eq!(response.header("content-type"), Some(CONTENT_TYPE));
        let body: Value = response.json();
        assert_eq!(body["status"], "401");
        assert_eq!(body["schemas"][0], ERROR_SCHEMA);
    }

    #[tokio::test]
    async fn unknown_scim_path_gets_a_scim_404() {
        let app = TestApp::with_license(&["scim", "org_sso"]).await;
        let response = app.request(Method::GET, "/scim/v2/Nope").send().await;
        assert_eq!(response.status, StatusCode::NOT_FOUND);
        assert_eq!(response.header("content-type"), Some(CONTENT_TYPE));
        let body: Value = response.json();
        assert_eq!(body["status"], "404");
        assert_eq!(body["schemas"][0], ERROR_SCHEMA);
    }
}
