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
use super::recompute::{self, RecomputeScope};
use super::schema::{
    list_response, resource_types, schemas, service_provider_config, user_resource,
};
use super::store::{self as scim_store, ScimUserFields, ScimUserRow, UserFilter};
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
        route(
            Method::GET,
            "/scim/v2/ServiceProviderConfig",
            get(discovery_config),
        ),
        route(
            Method::GET,
            "/scim/v2/ResourceTypes",
            get(resource_types_doc),
        ),
        route(Method::GET, "/scim/v2/Schemas", get(schemas_doc)),
        route(Method::GET, "/scim/v2/Users", get(list_users)),
        route(Method::POST, "/scim/v2/Users", post(create_user)),
        route(Method::GET, "/scim/v2/Users/{id}", get(get_user)),
        route(Method::PUT, "/scim/v2/Users/{id}", put(replace_user)),
        route(Method::PATCH, "/scim/v2/Users/{id}", patch(patch_user)),
        route(
            Method::DELETE,
            "/scim/v2/Users/{id}",
            delete(delete_user_row),
        ),
    ]
}

pub(crate) fn route(
    method: Method,
    path: &'static str,
    handler: MethodRouter<AppState>,
) -> RouteDef {
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
pub(crate) async fn admit(
    state: &AppState,
    principal: &Principal,
) -> Result<(OrgId, OrgRole), ScimError> {
    let Principal::Scim { org_id, token_id } = principal else {
        return Err(ScimError::forbidden(
            "SCIM is turned off for this organization.",
        ));
    };
    let now = state.clock.now_ns();
    state
        .limits
        .scim
        .check(token_id, now)
        .map_err(|retry| ScimError::rate_limited(retry.0.as_secs().max(1)))?;
    let scim = scim_store::org_scim(state.meta.reader(), *org_id).await?;
    if !scim.enabled {
        return Err(ScimError::forbidden(
            "SCIM is turned off for this organization.",
        ));
    }
    Ok((*org_id, scim.default_role))
}

/// `<public_url>/scim/v2`, without a trailing slash.
pub(crate) fn base_url(state: &AppState) -> String {
    state.config.public_url.join("/scim/v2")
}

pub(crate) fn parse_id(id: &str) -> Result<Uuid, ScimError> {
    Uuid::parse_str(id).map_err(|_| ScimError::not_found())
}

/// A request body that is a JSON object. Anything else is `invalidSyntax`.
pub(crate) fn json_object(body: &[u8]) -> Result<Value, ScimError> {
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
pub(crate) fn write_error(error: MetaError) -> ScimError {
    match error {
        MetaError::Conflict("seat limit") => ScimError::forbidden("The org has no seat left."),
        other => ScimError::from(other),
    }
}

/// Drops the revoked PAT hashes from the cache after the commit.
pub(crate) fn evict(state: &AppState, hashes: &[String]) {
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
            (
                "employeeNumber",
                self.employee_number != row.employee_number,
            ),
        ];
        differences
            .into_iter()
            .filter(|(_, changed)| *changed)
            .map(|(name, _)| name)
            .collect()
    }
}

/// Applies `changes` to `attrs` and returns the userName they name, if any. An `Email` change
/// (`emails`) is ignored: a cetra account's email is its userName (D-EA-13), so `emails` on a
/// POST, PUT or PATCH never renames the user and is never refused for differing from it.
fn apply_changes(attrs: &mut Attrs, changes: Vec<UserChange>) -> Option<String> {
    let mut user_name = None;
    for change in changes {
        match change {
            UserChange::Active(active) => attrs.active = active,
            UserChange::UserName(value) => user_name = Some(value),
            UserChange::Email(_) => {}
            UserChange::ExternalId(value) => attrs.external_id = value,
            UserChange::GivenName(value) => attrs.given_name = value,
            UserChange::FamilyName(value) => attrs.family_name = value,
            UserChange::DisplayName(value) => attrs.display_name = value,
            UserChange::Department(value) => attrs.department = value,
            UserChange::EmployeeNumber(value) => attrs.employee_number = value,
        }
    }
    user_name
}

/// The list filter's values as stored: a userName or an email is normalized like the stored
/// addresses (D-EA-13). A value that is not an email is only lowercased, so it matches nothing.
fn normalize_filter(filter: UserFilter) -> UserFilter {
    match filter {
        UserFilter::UserName(value) => UserFilter::UserName(normalize_or_lowercase(&value)),
        UserFilter::Email(value) => UserFilter::Email(normalize_or_lowercase(&value)),
        UserFilter::ExternalId(value) => UserFilter::ExternalId(value),
    }
}

fn normalize_or_lowercase(raw: &str) -> String {
    normalize_email(raw).unwrap_or_else(|_| raw.to_ascii_lowercase())
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
/// of `org` (a last owner is refused). For a user who is not an instance admin it revokes the
/// sessions and PATs, and disables the account when no membership is left, recording that SCIM
/// disabled it (the re-provision rule). Sets the row inactive. Returns the revoked PAT hashes.
/// Runs in the caller's write transaction.
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
    let mut disabled_account = false;
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
            scim_store::mark_scim_disabled(conn, org, user.id, now).await?;
            disabled_account = true;
        }
        hashes
    };
    scim_store::set_user_active(conn, org, row.id, false, now).await?;
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

/// POST: provisions one user (D-EA-13, D-EA-18, P-5, P-6, P-7). The refusals come before any
/// write. An inactive user gets no membership.
async fn create_tx(
    conn: &mut SqliteConnection,
    state: &AppState,
    ctx: &audit::Ctx,
    scope: RecomputeScope,
    attrs: Attrs,
) -> Result<Result<(ScimUserRow, Vec<String>), ScimError>, MetaError> {
    let RecomputeScope {
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
    if let Some(external_id) = attrs.external_id.as_deref()
        && scim_store::user_by_external_id_tx(conn, org, external_id)
            .await?
            .is_some()
    {
        return Err(MetaError::Conflict("uniqueness"));
    }

    // Link or create. An existing account is linked, and an unproven one is reset first (D-P-66).
    // An account that is disabled is refused, unless SCIM disabled it: then it is enabled again
    // (D-EA-16). An instance admin is linked without any credential change (P-6).
    let (user_id, linked, mut hashes) =
        match users::user_by_email_tx(conn, &attrs.user_name).await? {
            Some(user) => {
                let scim_disabled = scim_store::is_scim_disabled_tx(conn, user.id).await?;
                if user.disabled_at.is_some() && !scim_disabled {
                    return Ok(Err(ScimError::conflict("This account is disabled.")));
                }
                if scim_disabled {
                    scim_store::clear_scim_disabled(conn, user.id).await?;
                    users::set_disabled(conn, user.id, None, now).await?;
                }
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

    // Membership (P-7). An active user new to the org gets the default role. An active user who
    // already is a member is recomputed from its groups once its SCIM row exists (P-2). An
    // inactive user gets no membership.
    let was_member = members::org_role_tx(conn, org, user_id).await?.is_some();
    let (role, already_member) = if attrs.active {
        match users::ensure_member_tx(conn, org, user_id, default_role, max_seats, now).await? {
            Some(role) => (Some(role), false),
            None => (members::org_role_tx(conn, org, user_id).await?, true),
        }
    } else {
        (None, false)
    };
    sync_name_tx(conn, org, user_id, &attrs, now).await?;
    let row = scim_store::insert_user(conn, org, user_id, &attrs.fields(), now).await?;
    let role = if already_member {
        recompute::recompute_tx(conn, ctx, &scope, &[row.id]).await?;
        members::org_role_tx(conn, org, user_id).await?
    } else {
        role
    };
    audit::record(
        conn,
        ctx,
        Event::new(ScimAction::UserProvisioned {
            scim_user_id: row.id,
            role: role.map_or("none", org_role_str),
            linked,
        })
        .org(org)
        .target(TargetKind::User, user_id),
        now,
    )
    .await?;
    if !attrs.active {
        if was_member {
            // The deactivation removes the membership and revokes the access (D-EA-16).
            hashes.extend(deactivate_tx(conn, ctx, org, &row, now).await?);
        } else {
            // A linked non-member keeps its sessions; the row is only marked inactive.
            scim_store::set_user_active(conn, org, row.id, false, now).await?;
        }
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
    scope: RecomputeScope,
    update: Update,
) -> Result<Result<(ScimUserRow, Vec<String>), ScimError>, MetaError> {
    let RecomputeScope {
        org,
        now,
        default_role,
        max_seats,
        scim,
        ..
    } = scope;
    let Some(row) = scim_store::user_tx(conn, org, update.id).await? else {
        return Ok(Err(ScimError::not_found()));
    };
    // A PUT replaces the attributes, but an absent `active` keeps the row's state (G).
    let mut attrs = if update.replace {
        Attrs {
            active: row.active,
            ..Attrs::blank()
        }
    } else {
        Attrs::of_row(&row)
    };
    if let Some(raw) = apply_changes(&mut attrs, update.changes) {
        attrs.user_name = match normalized(&raw) {
            Ok(name) => name,
            Err(error) => return Ok(Err(error)),
        };
    }
    let deactivating = row.active && !attrs.active;
    let reactivating = !row.active && attrs.active;
    // Lapse (D-EA-19): a deactivation gets through and drops the other changes. A reactivation
    // and an attribute change are refused. A no-op is accepted.
    if !scim {
        if deactivating {
            attrs = Attrs {
                active: false,
                ..Attrs::of_row(&row)
            };
        } else if reactivating || !attrs.changed_from(&row).is_empty() {
            return Ok(Err(ScimError::forbidden(NOT_ENTITLED)));
        }
    }
    let changed = attrs.changed_from(&row);
    let account = users::user_by_id_tx(conn, row.user_id)
        .await?
        .ok_or(MetaError::Corrupt {
            table: "scim_users",
            column: "user_id",
        })?;
    let domains = scim_domains_tx(state, conn, org, now).await?;

    let email_changed = attrs.user_name != row.user_name;
    if email_changed {
        // A userName another account or SCIM row holds is a conflict (H).
        let taken = users::user_by_email_tx(conn, &attrs.user_name)
            .await?
            .is_some()
            || scim_store::user_by_user_name_tx(conn, org, &attrs.user_name)
                .await?
                .is_some();
        if taken {
            return Ok(Err(ScimError::uniqueness()));
        }
        if account.is_instance_admin {
            return Ok(Err(ScimError::mutability(
                "An instance admin's userName cannot change here.",
            )));
        }
        let within = in_domains(&domains, &row.user_name) && in_domains(&domains, &attrs.user_name);
        let elsewhere = memberships_elsewhere_tx(conn, org, row.user_id).await? > 0;
        if !within || elsewhere {
            return Ok(Err(ScimError::mutability(
                "userName can only change within the organization's verified domains",
            )));
        }
    }
    if reactivating {
        if !in_domains(&domains, &attrs.user_name) {
            return Ok(Err(ScimError::invalid_value(
                "userName is not in a verified domain of this organization",
            )));
        }
        // An account that an instance admin or the control plane disabled stays disabled
        // (D-EA-16).
        if account.disabled_at.is_some()
            && !scim_store::is_scim_disabled_tx(conn, row.user_id).await?
        {
            return Ok(Err(ScimError::conflict("This account is disabled.")));
        }
    }

    if email_changed {
        users::set_email_tx(conn, row.user_id, &attrs.user_name, now).await?;
    }
    if reactivating {
        // The row is active first, so the recompute sees it. The membership takes the role of the
        // user's groups (D-EA-14), so a user mapped to viewer is not added at a higher default.
        scim_store::set_user_active(conn, org, row.id, true, now).await?;
        let mappings = scim_store::mappings_tx(conn, org).await?;
        let groups = scim_store::groups_of_user_tx(conn, row.id).await?;
        let target = recompute::target_org_role(&groups, &mappings, default_role);
        users::ensure_member_tx(conn, org, row.user_id, target, max_seats, now).await?;
        recompute::recompute_tx(conn, ctx, &scope, &[row.id]).await?;
        if scim_store::is_scim_disabled_tx(conn, row.user_id).await? {
            scim_store::clear_scim_disabled(conn, row.user_id).await?;
            users::set_disabled(conn, row.user_id, None, now).await?;
        }
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
    let hashes = if deactivating {
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
    scope: RecomputeScope,
    update: Update,
) -> Result<Response, ScimError> {
    let state_for_write = state.clone();
    let (row, hashes) = state
        .meta
        .write(move |conn| {
            Box::pin(async move { update_tx(conn, &state_for_write, &ctx, scope, update).await })
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
pub(crate) struct ListQuery {
    pub(crate) filter: Option<String>,
    #[serde(rename = "startIndex")]
    pub(crate) start_index: Option<String>,
    pub(crate) count: Option<String>,
}

/// The `startIndex` and `count` of a list query, as `(start, count)` (RFC 7644 § 3.4.2.4).
/// `startIndex` is at least 1 and defaults to 1; `count` is clamped to 0 to 200 and defaults to
/// 100. A malformed value gets the default.
pub(crate) fn page(query: &ListQuery) -> (u32, u32) {
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
    (
        u32::try_from(start_index).unwrap_or(u32::MAX),
        u32::try_from(count).unwrap_or(0),
    )
}

async fn list_users(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Query(query): Query<ListQuery>,
) -> Result<Response, ScimError> {
    let (org, _) = admit(&state, &principal).await?;
    let (start, count) = page(&query);
    let filter = query
        .filter
        .as_deref()
        .map(parse_user_filter)
        .transpose()?
        .map(normalize_filter);
    let (total, rows) =
        scim_store::list_users(state.meta.reader(), org, filter.as_ref(), start, count).await?;
    let base = base_url(&state);
    let resources: Vec<Value> = rows.iter().map(|row| user_resource(row, &base)).collect();
    Ok(ScimJson(list_response(total, start, &resources)).into_response())
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
    let scope = RecomputeScope::of(&state, org, default_role);
    if !scope.scim {
        return Err(ScimError::forbidden(NOT_ENTITLED));
    }
    let body = json_object(&body)?;
    require_user_name(&body)?;
    let mut attrs = Attrs::blank();
    let raw = apply_changes(&mut attrs, map_changes(&body)?)
        .ok_or_else(|| ScimError::invalid_value("userName is required"))?;
    attrs.user_name = normalized(&raw)?;

    let state_for_write = state.clone();
    let written = state
        .meta
        .write(move |conn| {
            Box::pin(async move { create_tx(conn, &state_for_write, &ctx, scope, attrs).await })
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
    run_update(
        &state,
        ctx,
        RecomputeScope::of(&state, org, default_role),
        update,
    )
    .await
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
    run_update(
        &state,
        ctx,
        RecomputeScope::of(&state, org, default_role),
        update,
    )
    .await
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
    use cetra_core::ids::OrgId;
    use serde_json::{Value, json};
    use uuid::Uuid;

    use super::super::error::{CONTENT_TYPE, ERROR_SCHEMA};
    use super::super::schema::USER_SCHEMA;
    use crate::auth::OrgRole;
    use crate::ee::scim::store::{self as scim_store, ScimUserFields};
    use crate::ee::scim::test_helpers::{bearer, create_user, scim_org, scim_org_in};
    use crate::meta::members::add_org_member;
    use crate::meta::users::{
        EmailVerifiedVia, NewUser, insert_user as insert_account, set_disabled,
    };
    use crate::test_support::{TestApp, make_instance_admin, second_org, set_limits};

    const PATCH_OP: &str = "urn:ietf:params:scim:api:messages:2.0:PatchOp";

    async fn scalar_text(app: &TestApp, sql: &'static str, value: &str) -> String {
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
        let token = scim_org(&app, &seeded, "example.com", OrgRole::Viewer).await;
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
        let token = scim_org(&app, &seeded, "example.com", OrgRole::Viewer).await;
        let response = create_user(&app, &token, "Grace@Example.com").await;
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
        let token = scim_org(&app, &seeded, "example.com", OrgRole::Viewer).await;
        let response = create_user(&app, &token, "x@other.org").await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST);
        let body: Value = response.json();
        assert_eq!(body["scimType"], "invalidValue");
        assert_eq!(body["status"], "400");
    }

    #[tokio::test]
    async fn create_duplicate_user_name_is_uniqueness_409() {
        let app = TestApp::with_license(&["scim", "org_sso"]).await;
        let seeded = app.seed().await;
        let token = scim_org(&app, &seeded, "example.com", OrgRole::Viewer).await;
        assert_eq!(
            create_user(&app, &token, "a@example.com").await.status,
            StatusCode::CREATED
        );
        let response = create_user(&app, &token, "a@example.com").await;
        assert_eq!(response.status, StatusCode::CONFLICT);
        let body: Value = response.json();
        assert_eq!(body["scimType"], "uniqueness");
    }

    #[tokio::test]
    async fn create_when_lapsed_is_403_scim_error() {
        let app = TestApp::with_license(&["org_sso"]).await;
        let seeded = app.seed().await;
        let token = scim_org(&app, &seeded, "example.com", OrgRole::Viewer).await;
        let response = create_user(&app, &token, "a@example.com").await;
        assert_eq!(response.status, StatusCode::FORBIDDEN);
        let body: Value = response.json();
        assert_eq!(body["schemas"][0], ERROR_SCHEMA);
        assert_eq!(body["status"], "403");
    }

    #[tokio::test]
    async fn list_filters_by_user_name_and_count_zero_returns_only_the_total() {
        let app = TestApp::with_license(&["scim", "org_sso"]).await;
        let seeded = app.seed().await;
        let token = scim_org(&app, &seeded, "example.com", OrgRole::Viewer).await;
        assert_eq!(
            create_user(&app, &token, "a@example.com").await.status,
            StatusCode::CREATED
        );
        assert_eq!(
            create_user(&app, &token, "b@example.com").await.status,
            StatusCode::CREATED
        );

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
        let token = scim_org(&app, &seeded, "example.com", OrgRole::Viewer).await;
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
        let token = scim_org(&app, &seeded, "example.com", OrgRole::Viewer).await;
        assert_eq!(
            create_user(&app, &token, "a@example.com").await.status,
            StatusCode::CREATED
        );
        let body = json!({
            "schemas": [PATCH_OP],
            "Operations": [{"op": "replace", "value": {"active": "False"}}]
        });
        let listed: Value = app
            .request(
                Method::GET,
                "/scim/v2/Users?filter=userName%20eq%20%22a%40example.com%22",
            )
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
        let token = scim_org(&app, &seeded, "example.com", OrgRole::Viewer).await;
        // The seeded owner is ada@example.com, the only owner of the org.
        assert_eq!(
            create_user(&app, &token, "ada@example.com").await.status,
            StatusCode::CREATED
        );
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
        let token = scim_org(&app, &seeded, "example.com", OrgRole::Viewer).await;
        assert_eq!(
            create_user(&app, &token, "a@example.com").await.status,
            StatusCode::CREATED
        );
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

    /// A provisioned member written straight to the store, so a lapsed licence can be tested:
    /// an account, its `member` membership and an active SCIM row. Returns the SCIM id and the
    /// account id.
    async fn provisioned(app: &TestApp, org: OrgId, email: &'static str) -> (Uuid, Uuid) {
        let now = app.state.clock.now_ns();
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    let user = insert_account(
                        conn,
                        NewUser {
                            email: email.to_owned(),
                            name: None,
                            password_hash: None,
                            email_verified_at: Some(now),
                            email_verified_via: Some(EmailVerifiedVia::Idp),
                            is_instance_admin: false,
                        },
                        now,
                    )
                    .await?;
                    add_org_member(conn, org, user.id, OrgRole::Member, now).await?;
                    let fields = ScimUserFields {
                        user_name: email.to_owned(),
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

    /// The SCIM id of the user with this userName, from the list.
    async fn scim_id_of(app: &TestApp, token: &str, user_name: &str) -> String {
        let listed: Value = app
            .request(
                Method::GET,
                &format!(
                    "/scim/v2/Users?filter=userName%20eq%20%22{}%22",
                    user_name.replace('@', "%40")
                ),
            )
            .header("authorization", &bearer(token))
            .send()
            .await
            .json();
        listed["Resources"][0]["id"].as_str().unwrap().to_owned()
    }

    async fn patch(
        app: &TestApp,
        token: &str,
        id: &str,
        body: &Value,
    ) -> crate::test_support::TestResponse {
        app.request(Method::PATCH, &format!("/scim/v2/Users/{id}"))
            .header("authorization", &bearer(token))
            .json(body)
            .send()
            .await
    }

    async fn audit_rows_by_scim_tokens(app: &TestApp) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM audit_events WHERE actor_kind = 'scim_token'")
            .fetch_one(app.state.meta.reader())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn another_orgs_user_is_404() {
        let app = TestApp::with_license(&["scim", "org_sso"]).await;
        let seeded = app.seed().await;
        let token = scim_org(&app, &seeded, "example.com", OrgRole::Viewer).await;
        let (globex, globex_owner) = second_org(&app, "globex", "owner@globex.test").await;
        let other = scim_org_in(
            &app,
            globex,
            globex_owner.id,
            "globex.test",
            OrgRole::Viewer,
        )
        .await;
        assert_eq!(
            create_user(&app, &token, "a@example.com").await.status,
            StatusCode::CREATED
        );
        let id = scim_id_of(&app, &token, "a@example.com").await;
        let fetched = app
            .request(Method::GET, &format!("/scim/v2/Users/{id}"))
            .header("authorization", &bearer(&other))
            .send()
            .await;
        assert_eq!(fetched.status, StatusCode::NOT_FOUND);
        let deleted = app
            .request(Method::DELETE, &format!("/scim/v2/Users/{id}"))
            .header("authorization", &bearer(&other))
            .send()
            .await;
        assert_eq!(deleted.status, StatusCode::NOT_FOUND);
        assert_eq!(scim_id_of(&app, &token, "a@example.com").await, id);
    }

    #[tokio::test]
    async fn a_scim_disabled_org_is_403() {
        let app = TestApp::with_license(&["scim", "org_sso"]).await;
        let seeded = app.seed().await;
        let token = scim_org(&app, &seeded, "example.com", OrgRole::Viewer).await;
        let org = seeded.org_id;
        let now = app.state.clock.now_ns();
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    crate::ee::scim::store::put_org_scim(conn, org, false, OrgRole::Viewer, now)
                        .await
                })
            })
            .await
            .unwrap();
        let response = app
            .request(Method::GET, "/scim/v2/Users")
            .header("authorization", &bearer(&token))
            .send()
            .await;
        assert_eq!(response.status, StatusCode::FORBIDDEN);
        assert_eq!(response.header("content-type"), Some(CONTENT_TYPE));
        let body: Value = response.json();
        assert_eq!(body["detail"], "SCIM is turned off for this organization.");
    }

    #[tokio::test]
    async fn the_limiter_answers_429_with_retry_after() {
        let app = TestApp::with_license(&["scim", "org_sso"]).await;
        let seeded = app.seed().await;
        let token = scim_org(&app, &seeded, "example.com", OrgRole::Viewer).await;
        for _ in 0..600 {
            let response = app
                .request(Method::GET, "/scim/v2/Users?count=0")
                .header("authorization", &bearer(&token))
                .send()
                .await;
            assert_eq!(response.status, StatusCode::OK);
        }
        let limited = app
            .request(Method::GET, "/scim/v2/Users?count=0")
            .header("authorization", &bearer(&token))
            .send()
            .await;
        assert_eq!(limited.status, StatusCode::TOO_MANY_REQUESTS);
        assert!(limited.header("retry-after").is_some());
        let body: Value = limited.json();
        assert_eq!(body["schemas"][0], ERROR_SCHEMA);
    }

    #[tokio::test]
    async fn create_links_an_unproven_account_and_resets_it() {
        let app = TestApp::with_license(&["scim", "org_sso"]).await;
        let seeded = app.seed().await;
        let token = scim_org(&app, &seeded, "example.com", OrgRole::Viewer).await;
        let now = app.state.clock.now_ns();
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    insert_account(
                        conn,
                        NewUser {
                            email: "grace@example.com".to_owned(),
                            name: None,
                            password_hash: Some("hash".to_owned()),
                            email_verified_at: None,
                            email_verified_via: None,
                            is_instance_admin: false,
                        },
                        now,
                    )
                    .await?;
                    Ok(())
                })
            })
            .await
            .unwrap();
        assert_eq!(
            create_user(&app, &token, "grace@example.com").await.status,
            StatusCode::CREATED
        );
        let password = scalar_text(
            &app,
            "SELECT CASE WHEN password_hash IS NULL THEN 'none' ELSE 'set' END FROM users WHERE email = ?",
            "grace@example.com",
        )
        .await;
        assert_eq!(password, "none");
        let via = scalar_text(
            &app,
            "SELECT email_verified_via FROM users WHERE email = ?",
            "grace@example.com",
        )
        .await;
        assert_eq!(via, "idp");
    }

    #[tokio::test]
    async fn create_for_an_instance_admin_leaves_credentials() {
        let app = TestApp::with_license(&["scim", "org_sso"]).await;
        let seeded = app.seed().await;
        let token = scim_org(&app, &seeded, "example.com", OrgRole::Viewer).await;
        let now = app.state.clock.now_ns();
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    insert_account(
                        conn,
                        NewUser {
                            email: "root@example.com".to_owned(),
                            name: None,
                            password_hash: Some("hash".to_owned()),
                            email_verified_at: Some(now),
                            email_verified_via: Some(EmailVerifiedVia::MailToken),
                            is_instance_admin: true,
                        },
                        now,
                    )
                    .await?;
                    Ok(())
                })
            })
            .await
            .unwrap();
        assert_eq!(
            create_user(&app, &token, "root@example.com").await.status,
            StatusCode::CREATED
        );
        let password = scalar_text(
            &app,
            "SELECT CASE WHEN password_hash IS NULL THEN 'none' ELSE 'set' END FROM users WHERE email = ?",
            "root@example.com",
        )
        .await;
        assert_eq!(password, "set");
        let via = scalar_text(
            &app,
            "SELECT email_verified_via FROM users WHERE email = ?",
            "root@example.com",
        )
        .await;
        assert_eq!(via, "mail_token");
    }

    #[tokio::test]
    async fn an_instance_claimed_domain_is_refused() {
        let key = crate::test_support::license_key(&["scim", "org_sso"]);
        let app = TestApp::with_config(move |config| {
            config.license_key = Some(crate::config::Secret::new(key));
            config.auth.enforce_sso_domains = vec!["claimed.test".to_owned()];
        })
        .await;
        let seeded = app.seed().await;
        let token = scim_org(&app, &seeded, "claimed.test", OrgRole::Viewer).await;
        let response = create_user(&app, &token, "x@claimed.test").await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST);
        let body: Value = response.json();
        assert_eq!(body["scimType"], "invalidValue");
    }

    #[tokio::test]
    async fn create_at_the_seat_limit_with_member_default_is_refused() {
        let app = TestApp::with_license(&["scim", "org_sso"]).await;
        let seeded = app.seed().await;
        set_limits(
            &app,
            seeded.org_id,
            "2026-01-01T00:00:00Z",
            &json!({"max_seats": 1}),
        );
        let member_token = scim_org_in(
            &app,
            seeded.org_id,
            seeded.user_id,
            "example.com",
            OrgRole::Member,
        )
        .await;
        let refused = create_user(&app, &member_token, "a@example.com").await;
        assert_eq!(refused.status, StatusCode::FORBIDDEN);
        let body: Value = refused.json();
        assert_eq!(body["schemas"][0], ERROR_SCHEMA);
    }

    #[tokio::test]
    async fn viewer_default_ignores_the_seat_limit() {
        let app = TestApp::with_license(&["scim", "org_sso"]).await;
        let seeded = app.seed().await;
        set_limits(
            &app,
            seeded.org_id,
            "2026-01-01T00:00:00Z",
            &json!({"max_seats": 1}),
        );
        let token = scim_org(&app, &seeded, "example.com", OrgRole::Viewer).await;
        assert_eq!(
            create_user(&app, &token, "a@example.com").await.status,
            StatusCode::CREATED
        );
    }

    #[tokio::test]
    async fn every_operation_writes_an_audit_row_with_the_scim_token_actor() {
        let app = TestApp::with_license(&["scim", "org_sso"]).await;
        let seeded = app.seed().await;
        let token = scim_org(&app, &seeded, "example.com", OrgRole::Viewer).await;
        assert_eq!(
            create_user(&app, &token, "a@example.com").await.status,
            StatusCode::CREATED
        );
        let id = scim_id_of(&app, &token, "a@example.com").await;
        let deactivated = patch(
            &app,
            &token,
            &id,
            &json!({"schemas": [PATCH_OP], "Operations":[{"op": "replace", "value": {"active": false}}]}),
        )
        .await;
        assert_eq!(deactivated.status, StatusCode::OK);
        let deleted = app
            .request(Method::DELETE, &format!("/scim/v2/Users/{id}"))
            .header("authorization", &bearer(&token))
            .send()
            .await;
        assert_eq!(deleted.status, StatusCode::NO_CONTENT);
        // provisioned, deactivated and deleted: one record each.
        assert_eq!(audit_rows_by_scim_tokens(&app).await, 3);
    }

    #[tokio::test]
    async fn an_upn_and_a_mail_that_differ_are_accepted() {
        let app = TestApp::with_license(&["scim", "org_sso"]).await;
        let seeded = app.seed().await;
        let token = scim_org(&app, &seeded, "example.com", OrgRole::Viewer).await;
        let response = app
            .request(Method::POST, "/scim/v2/Users")
            .header("authorization", &bearer(&token))
            .json(&json!({
                "schemas": [USER_SCHEMA],
                "userName": "ada.lovelace@example.com",
                "emails": [{"value": "mail@corp.example.org", "type": "work", "primary": true}]
            }))
            .send()
            .await;
        assert_eq!(response.status, StatusCode::CREATED);
        let body: Value = response.json();
        assert_eq!(body["userName"], "ada.lovelace@example.com");
        assert_eq!(body["emails"][0]["value"], "ada.lovelace@example.com");
    }

    #[tokio::test]
    async fn emails_never_rename_a_user_on_patch() {
        let app = TestApp::with_license(&["scim", "org_sso"]).await;
        let seeded = app.seed().await;
        let token = scim_org(&app, &seeded, "example.com", OrgRole::Viewer).await;
        assert_eq!(
            create_user(&app, &token, "grace@example.com").await.status,
            StatusCode::CREATED
        );
        let id = scim_id_of(&app, &token, "grace@example.com").await;
        let response = patch(
            &app,
            &token,
            &id,
            &json!({"schemas": [PATCH_OP], "Operations":[{
                "op": "replace",
                "path": "emails[type eq \"work\"].value",
                "value": "other@example.com"
            }]}),
        )
        .await;
        assert_eq!(response.status, StatusCode::OK);
        let body: Value = response.json();
        assert_eq!(body["userName"], "grace@example.com");
    }

    #[tokio::test]
    async fn the_user_name_filter_is_normalized() {
        let app = TestApp::with_license(&["scim", "org_sso"]).await;
        let seeded = app.seed().await;
        let token = scim_org(&app, &seeded, "example.com", OrgRole::Viewer).await;
        assert_eq!(
            create_user(&app, &token, "ada@example.com").await.status,
            StatusCode::CREATED
        );
        let listed: Value = app
            .request(
                Method::GET,
                "/scim/v2/Users?filter=userName%20eq%20%22ADA%40Example.COM%22",
            )
            .header("authorization", &bearer(&token))
            .send()
            .await
            .json();
        assert_eq!(listed["totalResults"], 1);
    }

    #[tokio::test]
    async fn a_taken_user_name_on_update_is_uniqueness() {
        let app = TestApp::with_license(&["scim", "org_sso"]).await;
        let seeded = app.seed().await;
        let token = scim_org(&app, &seeded, "example.com", OrgRole::Viewer).await;
        assert_eq!(
            create_user(&app, &token, "grace@example.com").await.status,
            StatusCode::CREATED
        );
        assert_eq!(
            create_user(&app, &token, "lin@example.com").await.status,
            StatusCode::CREATED
        );
        let id = scim_id_of(&app, &token, "grace@example.com").await;
        let response = patch(
            &app,
            &token,
            &id,
            &json!({"schemas": [PATCH_OP], "Operations":[{"op": "replace", "value": {"userName": "lin@example.com"}}]}),
        )
        .await;
        assert_eq!(response.status, StatusCode::CONFLICT);
        let body: Value = response.json();
        assert_eq!(body["scimType"], "uniqueness");
    }

    #[tokio::test]
    async fn an_instance_admin_user_name_cannot_change() {
        let app = TestApp::with_license(&["scim", "org_sso"]).await;
        let seeded = app.seed().await;
        let token = scim_org(&app, &seeded, "example.com", OrgRole::Viewer).await;
        let (_, user) = provisioned(&app, seeded.org_id, "grace@example.com").await;
        make_instance_admin(&app, user).await;
        let id = scim_id_of(&app, &token, "grace@example.com").await;
        let response = patch(
            &app,
            &token,
            &id,
            &json!({"schemas": [PATCH_OP], "Operations":[{"op": "replace", "value": {"userName": "grace2@example.com"}}]}),
        )
        .await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST);
        let body: Value = response.json();
        assert_eq!(body["scimType"], "mutability");
    }

    #[tokio::test]
    async fn put_without_active_keeps_the_state() {
        let app = TestApp::with_license(&["scim", "org_sso"]).await;
        let seeded = app.seed().await;
        let token = scim_org(&app, &seeded, "example.com", OrgRole::Viewer).await;
        assert_eq!(
            create_user(&app, &token, "grace@example.com").await.status,
            StatusCode::CREATED
        );
        let id = scim_id_of(&app, &token, "grace@example.com").await;
        let response = app
            .request(Method::PUT, &format!("/scim/v2/Users/{id}"))
            .header("authorization", &bearer(&token))
            .json(&json!({"schemas": [USER_SCHEMA], "userName": "grace@example.com", "name": {"givenName": "Grace"}}))
            .send()
            .await;
        assert_eq!(response.status, StatusCode::OK);
        let body: Value = response.json();
        assert_eq!(body["active"], true);
    }

    #[tokio::test]
    async fn lapsed_deactivate_via_put_works() {
        let app = TestApp::with_license(&["org_sso"]).await;
        let seeded = app.seed().await;
        let token = scim_org(&app, &seeded, "example.com", OrgRole::Viewer).await;
        let (id, _) = provisioned(&app, seeded.org_id, "grace@example.com").await;
        let response = app
            .request(Method::PUT, &format!("/scim/v2/Users/{id}"))
            .header("authorization", &bearer(&token))
            .json(&json!({
                "schemas": [USER_SCHEMA],
                "userName": "grace@example.com",
                "active": false,
                "name": {"givenName": "Changed"}
            }))
            .send()
            .await;
        assert_eq!(response.status, StatusCode::OK);
        let body: Value = response.json();
        assert_eq!(body["active"], false);
        assert!(body["name"].get("givenName").is_none());
        let memberships = scalar_text(
            &app,
            "SELECT CAST(COUNT(*) AS TEXT) FROM org_members om JOIN users u ON u.id = om.user_id WHERE u.email = ?",
            "grace@example.com",
        )
        .await;
        assert_eq!(memberships, "0");
    }

    #[tokio::test]
    async fn lapsed_no_op_active_true_works_and_reactivation_is_refused() {
        let app = TestApp::with_license(&["org_sso"]).await;
        let seeded = app.seed().await;
        let token = scim_org(&app, &seeded, "example.com", OrgRole::Viewer).await;
        let (id, _) = provisioned(&app, seeded.org_id, "grace@example.com").await;
        let noop = patch(
            &app,
            &token,
            &id.to_string(),
            &json!({"schemas": [PATCH_OP], "Operations":[{"op": "replace", "value": {"active": true}}]}),
        )
        .await;
        assert_eq!(noop.status, StatusCode::OK);
        let off = patch(
            &app,
            &token,
            &id.to_string(),
            &json!({"schemas": [PATCH_OP], "Operations":[{"op": "replace", "value": {"active": false}}]}),
        )
        .await;
        assert_eq!(off.status, StatusCode::OK);
        let back = patch(
            &app,
            &token,
            &id.to_string(),
            &json!({"schemas": [PATCH_OP], "Operations":[{"op": "replace", "value": {"active": true}}]}),
        )
        .await;
        assert_eq!(back.status, StatusCode::FORBIDDEN);
        let body: Value = back.json();
        assert_eq!(body["schemas"][0], ERROR_SCHEMA);
    }

    #[tokio::test]
    async fn post_inactive_for_an_existing_member_removes_the_membership() {
        let app = TestApp::with_license(&["scim", "org_sso"]).await;
        let seeded = app.seed().await;
        let token = scim_org(&app, &seeded, "example.com", OrgRole::Viewer).await;
        let _grace = app
            .add_member(&seeded, "grace@example.com", OrgRole::Member)
            .await;
        let response = app
            .request(Method::POST, "/scim/v2/Users")
            .header("authorization", &bearer(&token))
            .json(&json!({"schemas": [USER_SCHEMA], "userName": "grace@example.com", "active": false}))
            .send()
            .await;
        assert_eq!(response.status, StatusCode::CREATED);
        let body: Value = response.json();
        assert_eq!(body["active"], false);
        let memberships = scalar_text(
            &app,
            "SELECT CAST(COUNT(*) AS TEXT) FROM org_members om JOIN users u ON u.id = om.user_id WHERE u.email = ?",
            "grace@example.com",
        )
        .await;
        assert_eq!(memberships, "0");
    }

    #[tokio::test]
    async fn post_inactive_for_a_new_user_adds_no_membership() {
        let app = TestApp::with_license(&["scim", "org_sso"]).await;
        let seeded = app.seed().await;
        let token = scim_org(&app, &seeded, "example.com", OrgRole::Viewer).await;
        let response = app
            .request(Method::POST, "/scim/v2/Users")
            .header("authorization", &bearer(&token))
            .json(
                &json!({"schemas": [USER_SCHEMA], "userName": "new@example.com", "active": false}),
            )
            .send()
            .await;
        assert_eq!(response.status, StatusCode::CREATED);
        let body: Value = response.json();
        assert_eq!(body["active"], false);
        let memberships = scalar_text(
            &app,
            "SELECT CAST(COUNT(*) AS TEXT) FROM org_members om JOIN users u ON u.id = om.user_id WHERE u.email = ?",
            "new@example.com",
        )
        .await;
        assert_eq!(memberships, "0");
        let disabled = scalar_text(
            &app,
            "SELECT CASE WHEN disabled_at IS NULL THEN 'no' ELSE 'yes' END FROM users WHERE email = ?",
            "new@example.com",
        )
        .await;
        assert_eq!(disabled, "no");
    }

    #[tokio::test]
    async fn deleted_then_reprovisioned_account_is_reenabled() {
        let app = TestApp::with_license(&["scim", "org_sso"]).await;
        let seeded = app.seed().await;
        let token = scim_org(&app, &seeded, "example.com", OrgRole::Viewer).await;
        assert_eq!(
            create_user(&app, &token, "grace@example.com").await.status,
            StatusCode::CREATED
        );
        let id = scim_id_of(&app, &token, "grace@example.com").await;
        let deleted = app
            .request(Method::DELETE, &format!("/scim/v2/Users/{id}"))
            .header("authorization", &bearer(&token))
            .send()
            .await;
        assert_eq!(deleted.status, StatusCode::NO_CONTENT);
        let disabled = scalar_text(
            &app,
            "SELECT CASE WHEN disabled_at IS NULL THEN 'no' ELSE 'yes' END FROM users WHERE email = ?",
            "grace@example.com",
        )
        .await;
        assert_eq!(disabled, "yes");
        assert_eq!(
            create_user(&app, &token, "grace@example.com").await.status,
            StatusCode::CREATED
        );
        let enabled = scalar_text(
            &app,
            "SELECT CASE WHEN disabled_at IS NULL THEN 'no' ELSE 'yes' END FROM users WHERE email = ?",
            "grace@example.com",
        )
        .await;
        assert_eq!(enabled, "no");
    }

    #[tokio::test]
    async fn admin_disabled_account_is_refused() {
        let app = TestApp::with_license(&["scim", "org_sso"]).await;
        let seeded = app.seed().await;
        let token = scim_org(&app, &seeded, "example.com", OrgRole::Viewer).await;
        let now = app.state.clock.now_ns();
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    let user = insert_account(
                        conn,
                        NewUser {
                            email: "grace@example.com".to_owned(),
                            name: None,
                            password_hash: None,
                            email_verified_at: Some(now),
                            email_verified_via: Some(EmailVerifiedVia::Idp),
                            is_instance_admin: false,
                        },
                        now,
                    )
                    .await?;
                    set_disabled(conn, user.id, Some(now), now).await?;
                    Ok(())
                })
            })
            .await
            .unwrap();
        let response = create_user(&app, &token, "grace@example.com").await;
        assert_eq!(response.status, StatusCode::CONFLICT);
        let body: Value = response.json();
        assert_eq!(body["detail"], "This account is disabled.");
    }
}
