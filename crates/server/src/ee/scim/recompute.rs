// SPDX-License-Identifier: Elastic-2.0
//! Recomputes the roles of SCIM members from their groups (track EA M6, D-EA-14, D-EA-19).
//!
//! [`recompute_tx`] is the only code that sets a managed member's org role or project overrides
//! from SCIM (P-2). Every change that can affect them calls it in the same transaction.

use std::collections::{HashMap, HashSet};

use cetra_core::ids::{OrgId, ProjectId};
use sqlx::SqliteConnection;
use uuid::Uuid;

use super::store::{self as scim_store, GroupMapping, ScimUserRow};
use crate::api::entitlements::Feature;
use crate::audit::catalog::ScimAction;
use crate::audit::{self, Event};
use crate::auth::OrgRole;
use crate::auth::roles::{ProjectOverride, Role, effective_project_role, org_rank, override_rank};
use crate::entitlements::over_limit;
use crate::http::AppState;
use crate::meta::{MetaError, invites, members, users};

/// How many refused user ids one `scim.role_raise_refused` record holds (P-4).
const REFUSED_IDS_MAX: usize = 20;

/// The org roles and project overrides of every member of an org, read once for a recompute of
/// several users (see `recompute_tx`).
struct Preloaded {
    org_roles: HashMap<Uuid, OrgRole>,
    overrides: HashMap<Uuid, Vec<(ProjectId, ProjectOverride)>>,
}

/// What the caller knows outside the write. The entitlements are read in memory, before the
/// transaction starts (P-5).
#[derive(Debug, Clone, Copy)]
pub(crate) struct RecomputeScope {
    pub(crate) org: OrgId,
    pub(crate) now: i64,
    pub(crate) default_role: OrgRole,
    pub(crate) max_seats: Option<u64>,
    /// `scim` is entitled: raises are applied (P-5).
    pub(crate) scim: bool,
    /// `project_roles` is entitled: new overrides may be written (P-5).
    pub(crate) project_roles: bool,
}

impl RecomputeScope {
    /// The scope of a request on `org` that uses `default_role`. `max_seats` comes from the
    /// resolved limits, `scim` and `project_roles` from the entitlements.
    pub(crate) fn of(state: &AppState, org: OrgId, default_role: OrgRole) -> Self {
        let now = state.clock.now_ns();
        Self {
            org,
            now,
            default_role,
            max_seats: state.entitlements.resolve(org, now).limits.max_seats,
            scim: crate::entitlements::of(state, org).allows(Feature::Scim),
            project_roles: crate::entitlements::of(state, org).allows(Feature::ProjectRoles),
        }
    }
}

/// D-EA-14: recomputes the org role and project overrides of each listed SCIM user of the org
/// from its groups' mappings, in the caller's transaction, and writes the records of P-4.
///
/// Rows that are missing, inactive or not members of the org are skipped, and so is an owner.
/// Rows are processed once each, ordered by `created_at, id` (P-8). `ctx` is the caller's.
/// A user whose org-role raise is refused gets no raising override either: a seat refusal
/// covers the user's overrides too. Lowerings and neutral changes still apply.
///
/// # Errors
///
/// Returns [`MetaError`] when a read or write fails, or a stored value does not decode. A seat
/// refusal is not an error: the raise is refused and recorded.
pub(crate) async fn recompute_tx(
    conn: &mut SqliteConnection,
    ctx: &audit::Ctx,
    scope: &RecomputeScope,
    scim_users: &[Uuid],
) -> Result<(), MetaError> {
    let org = scope.org;
    let mappings = scim_store::mappings_tx(conn, org).await?;
    let groups_by_user = scim_store::groups_by_user_tx(conn, org).await?;
    let rows = ordered_rows_tx(conn, org, scim_users).await?;
    // Several users are read once, not once each (N+1). A user's org role and overrides are
    // changed only when its own row is reached, so the copy is current for it. One user reads
    // its own rows, as before.
    let preloaded = if rows.len() > 1 {
        Some(Preloaded {
            org_roles: members::org_roles_of_org_tx(conn, org).await?,
            overrides: members::project_overrides_of_org_tx(conn, org).await?,
        })
    } else {
        None
    };

    let mut users_checked: u32 = 0;
    let mut changed: u32 = 0;
    let mut refused: Vec<Uuid> = Vec::new();
    for row in &rows {
        let current = match &preloaded {
            Some(loaded) => loaded.org_roles.get(&row.user_id).copied(),
            None => members::org_role_tx(conn, org, row.user_id).await?,
        };
        let Some(current) = current else {
            continue;
        };
        // D-EA-14 and D139: owners are never changed.
        if current == OrgRole::Owner {
            continue;
        }
        users_checked = users_checked.saturating_add(1);

        let groups = groups_by_user.get(&row.id).map_or(&[][..], Vec::as_slice);
        let target = target_org_role(groups, &mappings, scope.default_role);
        // Lowerings always apply. A raise is skipped without `scim` (P-5, lapse), and refused
        // for a Viewer whose raise would pass the seat limit (D-EA-14).
        let mut role = target;
        let mut seat_email = None;
        let mut seat_refused = false;
        if org_rank(target) > org_rank(current) {
            if !scope.scim {
                role = current;
            } else if current == OrgRole::Viewer {
                match seat_for_raise_tx(conn, scope, row.user_id).await? {
                    Some(email) => seat_email = Some(email),
                    None => {
                        role = current;
                        seat_refused = true;
                        refused.push(row.id);
                    }
                }
            }
        }
        if role != current {
            members::set_org_role(conn, org, row.user_id, role).await?;
            changed = changed.saturating_add(1);
            // The raise takes the account's seat, so its pending invitation is removed (as
            // `ensure_member_tx` does).
            if let Some(email) = seat_email
                && let Some(invite) =
                    invites::pending_invite_for_tx(conn, org, &email, scope.now).await?
            {
                invites::delete_invite(conn, org, invite.id).await?;
            }
        }

        let existing: HashMap<ProjectId, ProjectOverride> = match &preloaded {
            Some(loaded) => loaded
                .overrides
                .get(&row.user_id)
                .into_iter()
                .flatten()
                .copied()
                .collect(),
            None => members::project_overrides_of_user_tx(conn, org, row.user_id)
                .await?
                .into_iter()
                .collect(),
        };
        if role == OrgRole::Admin {
            // D139: org admins have no overrides. Their effective access is the same either way.
            for project in existing.keys() {
                if members::remove_project_override(conn, *project, row.user_id).await? {
                    changed = changed.saturating_add(1);
                }
            }
            continue;
        }

        let desired = desired_overrides(groups, &mappings);
        let mut projects: Vec<ProjectId> = existing.keys().chain(desired.keys()).copied().collect();
        projects.sort();
        projects.dedup();
        for project in projects {
            let before_override = existing.get(&project).copied();
            let after_override = desired.get(&project).copied();
            // Both sides use the final org role, so only the override change is judged (P-5).
            let before = effective_project_role(role, before_override);
            let after = effective_project_role(role, after_override);
            if before_override == after_override {
                continue;
            }
            // A change that leaves effective access the same is neither a raise nor a lowering,
            // so no entitlement gates it: it removes or rewrites a stale override. A raise is
            // skipped without `scim` and `project_roles`, and when the user's org-role raise was
            // refused for the seat limit.
            let raises = effective_rank(after) > effective_rank(before);
            if raises && (seat_refused || !(scope.scim && scope.project_roles)) {
                continue;
            }
            match after_override {
                Some(override_role) => {
                    members::set_project_override(conn, project, row.user_id, override_role)
                        .await?;
                }
                None => {
                    members::remove_project_override(conn, project, row.user_id).await?;
                }
            }
            changed = changed.saturating_add(1);
        }
    }

    if changed > 0 {
        let event = ScimAction::RolesRecomputed {
            users: users_checked,
            changed,
        };
        audit::record(conn, ctx, Event::new(event).org(org), scope.now).await?;
    }
    if !refused.is_empty() {
        let event = ScimAction::RoleRaiseRefused {
            count: u32::try_from(refused.len()).unwrap_or(u32::MAX),
            scim_user_ids: refused.iter().take(REFUSED_IDS_MAX).copied().collect(),
        };
        audit::record(conn, ctx, Event::new(event).org(org), scope.now).await?;
    }
    Ok(())
}

/// The org role the groups of a SCIM user give: the highest role that a group maps, else
/// `default_role` (D-EA-14). A group that maps no org role does not count.
pub(crate) fn target_org_role(
    groups: &[Uuid],
    mappings: &HashMap<Uuid, GroupMapping>,
    default_role: OrgRole,
) -> OrgRole {
    groups
        .iter()
        .filter_map(|group| mappings.get(group)?.org_role)
        .max_by_key(|role| org_rank(*role))
        .unwrap_or(default_role)
}

/// The SCIM users of `org` among `scim_users`, each once, active ones only, ordered by
/// `created_at, id` (P-8). A missing row is skipped.
async fn ordered_rows_tx(
    conn: &mut SqliteConnection,
    org: OrgId,
    scim_users: &[Uuid],
) -> Result<Vec<ScimUserRow>, MetaError> {
    let mut seen = HashSet::new();
    let mut rows = Vec::new();
    for id in scim_users {
        if !seen.insert(*id) {
            continue;
        }
        if let Some(row) = scim_store::user_tx(conn, org, *id).await?
            && row.active
        {
            rows.push(row);
        }
    }
    rows.sort_by_key(|row| (row.created_at, row.id));
    Ok(rows)
}

/// Whether a Viewer may be raised to a seat-taking role, with `ensure_member_tx`'s rule: the
/// org's seats and the pending seat invitations of the account's email, plus one, must fit the
/// limit. Returns the account's email when it fits.
async fn seat_for_raise_tx(
    conn: &mut SqliteConnection,
    scope: &RecomputeScope,
    user: Uuid,
) -> Result<Option<String>, MetaError> {
    let account = users::user_by_id_tx(conn, user)
        .await?
        .ok_or(MetaError::Corrupt {
            table: "scim_users",
            column: "user_id",
        })?;
    // The account's own invitation is removed when it is raised, so it does not count.
    let seats = members::seat_members_tx(conn, scope.org).await?
        + invites::seat_invites_tx(conn, scope.org, scope.now, Some(account.email.as_str()))
            .await?;
    Ok((!over_limit(scope.max_seats, seats, 1)).then_some(account.email))
}

/// The project overrides the groups give: per project, the highest override among the groups'
/// rows (D-EA-14).
fn desired_overrides(
    groups: &[Uuid],
    mappings: &HashMap<Uuid, GroupMapping>,
) -> HashMap<ProjectId, ProjectOverride> {
    let mut desired: HashMap<ProjectId, ProjectOverride> = HashMap::new();
    for group in groups {
        let Some(mapping) = mappings.get(group) else {
            continue;
        };
        for (project, role) in &mapping.projects {
            let entry = desired.entry(*project).or_insert(*role);
            if override_rank(*role) > override_rank(*entry) {
                *entry = *role;
            }
        }
    }
    desired
}

/// Effective access ranked none < viewer < member < admin (P-5). `None` is no access.
fn effective_rank(role: Option<Role>) -> u8 {
    match role {
        None => 0,
        Some(Role::Viewer) => 1,
        Some(Role::Member) => 2,
        Some(Role::Admin) => 3,
    }
}

#[cfg(test)]
mod tests {
    use axum::http::{Method, StatusCode};
    use cetra_core::ids::{OrgId, ProjectId};
    use serde_json::json;
    use uuid::Uuid;

    use super::super::store::GroupMapping;
    use super::super::test_helpers::{create_user, scim_org, scim_request};
    use super::*;
    use crate::auth::ProjectOverride;
    use crate::meta::invites;
    use crate::meta::members::add_org_member;
    use crate::meta::users::{EmailVerifiedVia, NewUser, insert_user as insert_account};
    use crate::test_support::{Seeded, TestApp};

    const PATCH_OP: &str = "urn:ietf:params:scim:api:messages:2.0:PatchOp";
    const PATH: &str = "/api/v1/orgs/acme/scim";
    const SCIM_USERS: &str = "/scim/v2/Users";

    /// An app with `features` licensed and the seeded org (`ada@example.com`, an owner).
    async fn app_with(features: &[&str]) -> (TestApp, Seeded) {
        let app = TestApp::with_license(features).await;
        let seeded = app.seed().await;
        (app, seeded)
    }

    /// A managed member: an account of `email` with `role` in `org` and an active SCIM row.
    /// Returns the SCIM id and the account id.
    async fn managed(app: &TestApp, org: OrgId, email: &str, role: OrgRole) -> (Uuid, Uuid) {
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
                    add_org_member(conn, org, user.id, role, now).await?;
                    let fields = scim_store::ScimUserFields {
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

    /// A group of `org` with `members` (SCIM ids) and `mapping`. Returns the group id.
    async fn group_with(
        app: &TestApp,
        org: OrgId,
        name: &str,
        members: Vec<Uuid>,
        mapping: GroupMapping,
    ) -> Uuid {
        let now = app.state.clock.now_ns();
        let name = name.to_owned();
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    let group = scim_store::insert_group(conn, org, &name, None, now).await?;
                    scim_store::add_group_members_tx(conn, org, group.id, &members).await?;
                    scim_store::put_mapping(conn, group.id, &mapping, now).await?;
                    Ok(group.id)
                })
            })
            .await
            .unwrap()
    }

    /// Stores a project override of `user` on `project`, as a manual change would.
    async fn set_override(app: &TestApp, project: ProjectId, user: Uuid, role: ProjectOverride) {
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    members::set_project_override(conn, project, user, role).await?;
                    Ok(())
                })
            })
            .await
            .unwrap();
    }

    /// The scope of a direct recompute on `org`: the limits and entitlements are given.
    fn scope(
        app: &TestApp,
        org: OrgId,
        default_role: OrgRole,
        max_seats: Option<u64>,
        scim: bool,
        project_roles: bool,
    ) -> RecomputeScope {
        RecomputeScope {
            org,
            now: app.state.clock.now_ns(),
            default_role,
            max_seats,
            scim,
            project_roles,
        }
    }

    /// Runs `recompute_tx` over `users` in one write, as a SCIM request would.
    async fn recompute(app: &TestApp, scope: RecomputeScope, users: Vec<Uuid>) {
        let ctx = audit::Ctx {
            actor: audit::Actor::Scim {
                token_id: Uuid::now_v7(),
            },
            ip: None,
            user_agent: None,
        };
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move { recompute_tx(conn, &ctx, &scope, &users).await })
            })
            .await
            .unwrap();
    }

    async fn role_of(app: &TestApp, org: OrgId, user: Uuid) -> Option<OrgRole> {
        members::org_role(app.state.meta.reader(), org, user)
            .await
            .unwrap()
    }

    async fn override_of(app: &TestApp, project: ProjectId, user: Uuid) -> Option<ProjectOverride> {
        members::project_override(app.state.meta.reader(), project, user)
            .await
            .unwrap()
    }

    /// The `details` of every audit row with `action`.
    async fn details_of(app: &TestApp, action: &str) -> Vec<String> {
        sqlx::query_scalar("SELECT details FROM audit_events WHERE action = ? ORDER BY seq")
            .bind(action.to_owned())
            .fetch_all(app.state.meta.reader())
            .await
            .unwrap()
    }

    fn mapped(org_role: Option<OrgRole>) -> GroupMapping {
        GroupMapping {
            org_role,
            projects: Vec::new(),
        }
    }

    #[tokio::test]
    async fn highest_mapped_role_wins() {
        let (app, seeded) = app_with(&["scim"]).await;
        let org = seeded.org_id;
        let (scim_id, user) = managed(&app, org, "x@acme.test", OrgRole::Viewer).await;
        group_with(
            &app,
            org,
            "viewers",
            vec![scim_id],
            mapped(Some(OrgRole::Viewer)),
        )
        .await;
        group_with(
            &app,
            org,
            "members",
            vec![scim_id],
            mapped(Some(OrgRole::Member)),
        )
        .await;
        recompute(
            &app,
            scope(&app, org, OrgRole::Viewer, None, true, true),
            vec![scim_id],
        )
        .await;
        assert_eq!(role_of(&app, org, user).await, Some(OrgRole::Member));
    }

    #[tokio::test]
    async fn unmapped_users_get_the_default_role() {
        let (app, seeded) = app_with(&["scim"]).await;
        let org = seeded.org_id;
        let (scim_id, user) = managed(&app, org, "x@acme.test", OrgRole::Viewer).await;
        recompute(
            &app,
            scope(&app, org, OrgRole::Member, None, true, true),
            vec![scim_id],
        )
        .await;
        assert_eq!(role_of(&app, org, user).await, Some(OrgRole::Member));
    }

    #[tokio::test]
    async fn owners_are_never_changed() {
        let (app, seeded) = app_with(&["scim"]).await;
        let org = seeded.org_id;
        let (scim_id, user) = managed(&app, org, "x@acme.test", OrgRole::Owner).await;
        group_with(
            &app,
            org,
            "viewers",
            vec![scim_id],
            mapped(Some(OrgRole::Viewer)),
        )
        .await;
        recompute(
            &app,
            scope(&app, org, OrgRole::Viewer, None, true, true),
            vec![scim_id],
        )
        .await;
        assert_eq!(role_of(&app, org, user).await, Some(OrgRole::Owner));
    }

    #[tokio::test]
    async fn admins_get_no_overrides() {
        let (app, seeded) = app_with(&["scim", "project_roles"]).await;
        let org = seeded.org_id;
        let project = seeded.project.id;
        let (scim_id, user) = managed(&app, org, "x@acme.test", OrgRole::Member).await;
        set_override(&app, project, user, ProjectOverride::Viewer).await;
        group_with(
            &app,
            org,
            "admins",
            vec![scim_id],
            GroupMapping {
                org_role: Some(OrgRole::Admin),
                projects: vec![(project, ProjectOverride::Viewer)],
            },
        )
        .await;
        recompute(
            &app,
            scope(&app, org, OrgRole::Viewer, None, true, true),
            vec![scim_id],
        )
        .await;
        assert_eq!(role_of(&app, org, user).await, Some(OrgRole::Admin));
        assert_eq!(override_of(&app, project, user).await, None);
    }

    #[tokio::test]
    async fn unmapped_projects_lose_their_overrides() {
        let (app, seeded) = app_with(&["scim", "project_roles"]).await;
        let org = seeded.org_id;
        let project = seeded.project.id;
        let (scim_id, user) = managed(&app, org, "x@acme.test", OrgRole::Member).await;
        set_override(&app, project, user, ProjectOverride::Admin).await;
        recompute(
            &app,
            scope(&app, org, OrgRole::Member, None, true, true),
            vec![scim_id],
        )
        .await;
        // Admin lowers to the Member fallback, so the removal is a lowering.
        assert_eq!(override_of(&app, project, user).await, None);
    }

    #[tokio::test]
    async fn raise_past_the_seat_limit_is_refused_and_recorded() {
        let (app, seeded) = app_with(&["scim"]).await;
        let org = seeded.org_id;
        let (scim_id, user) = managed(&app, org, "x@acme.test", OrgRole::Viewer).await;
        group_with(
            &app,
            org,
            "members",
            vec![scim_id],
            mapped(Some(OrgRole::Member)),
        )
        .await;
        // The owner takes the only seat.
        let limited = scope(&app, org, OrgRole::Viewer, Some(1), true, true);
        recompute(&app, limited, vec![scim_id]).await;
        assert_eq!(role_of(&app, org, user).await, Some(OrgRole::Viewer));
        let refusals = details_of(&app, "scim.role_raise_refused").await;
        assert_eq!(refusals.len(), 1);
        assert!(refusals[0].contains(&scim_id.to_string()));
    }

    #[tokio::test]
    async fn member_to_admin_needs_no_seat() {
        let (app, seeded) = app_with(&["scim"]).await;
        let org = seeded.org_id;
        let (scim_id, user) = managed(&app, org, "x@acme.test", OrgRole::Member).await;
        group_with(
            &app,
            org,
            "admins",
            vec![scim_id],
            mapped(Some(OrgRole::Admin)),
        )
        .await;
        // Over the limit already: a Member raised to Admin takes no seat, so it still applies.
        let limited = scope(&app, org, OrgRole::Viewer, Some(1), true, true);
        recompute(&app, limited, vec![scim_id]).await;
        assert_eq!(role_of(&app, org, user).await, Some(OrgRole::Admin));
        assert!(details_of(&app, "scim.role_raise_refused").await.is_empty());
    }

    #[tokio::test]
    async fn lapsed_org_lowers_but_never_raises() {
        let (app, seeded) = app_with(&["scim"]).await;
        let org = seeded.org_id;
        let (raise_id, raised) = managed(&app, org, "a@acme.test", OrgRole::Viewer).await;
        group_with(
            &app,
            org,
            "members",
            vec![raise_id],
            mapped(Some(OrgRole::Member)),
        )
        .await;
        let (lower_id, lowered) = managed(&app, org, "b@acme.test", OrgRole::Admin).await;
        let lapsed = scope(&app, org, OrgRole::Viewer, None, false, false);
        recompute(&app, lapsed, vec![raise_id, lower_id]).await;
        assert_eq!(role_of(&app, org, raised).await, Some(OrgRole::Viewer));
        assert_eq!(role_of(&app, org, lowered).await, Some(OrgRole::Viewer));
        assert!(details_of(&app, "scim.role_raise_refused").await.is_empty());
    }

    #[tokio::test]
    async fn without_project_roles_no_new_override_is_written() {
        let (app, seeded) = app_with(&["scim"]).await;
        let org = seeded.org_id;
        let (first, second) = (
            seeded.project.id,
            app.add_project(&seeded, "second", None).await.0.id,
        );
        let (scim_id, user) = managed(&app, org, "x@acme.test", OrgRole::Member).await;
        group_with(
            &app,
            org,
            "mixed",
            vec![scim_id],
            GroupMapping {
                org_role: None,
                projects: vec![
                    // A raise on the Member fallback: not written without project_roles.
                    (second, ProjectOverride::Admin),
                    // A lowering: written.
                    (first, ProjectOverride::Viewer),
                ],
            },
        )
        .await;
        let no_roles = scope(&app, org, OrgRole::Member, None, true, false);
        recompute(&app, no_roles, vec![scim_id]).await;
        assert_eq!(override_of(&app, second, user).await, None);
        assert_eq!(
            override_of(&app, first, user).await,
            Some(ProjectOverride::Viewer)
        );
    }

    #[tokio::test]
    async fn removing_a_none_override_is_a_raise_and_is_skipped_when_lapsed() {
        let (app, seeded) = app_with(&["scim", "project_roles"]).await;
        let org = seeded.org_id;
        let project = seeded.project.id;
        let (scim_id, user) = managed(&app, org, "x@acme.test", OrgRole::Member).await;
        set_override(&app, project, user, ProjectOverride::None).await;
        // Lapsed: the removal would raise access, so it is skipped.
        recompute(
            &app,
            scope(&app, org, OrgRole::Member, None, false, true),
            vec![scim_id],
        )
        .await;
        assert_eq!(
            override_of(&app, project, user).await,
            Some(ProjectOverride::None)
        );
        // Entitled: the removal applies.
        recompute(
            &app,
            scope(&app, org, OrgRole::Member, None, true, true),
            vec![scim_id],
        )
        .await;
        assert_eq!(override_of(&app, project, user).await, None);
    }

    #[tokio::test]
    async fn a_neutral_stale_override_is_removed_even_when_lapsed() {
        let (app, seeded) = app_with(&["scim", "project_roles"]).await;
        let org = seeded.org_id;
        let project = seeded.project.id;
        let (scim_id, user) = managed(&app, org, "x@acme.test", OrgRole::Member).await;
        // A Member override on a Member: the effective access is Member with or without it.
        set_override(&app, project, user, ProjectOverride::Member).await;
        // Lapsed, and no group maps the project: the removal is neither a raise nor a lowering.
        recompute(
            &app,
            scope(&app, org, OrgRole::Member, None, false, false),
            vec![scim_id],
        )
        .await;
        assert_eq!(override_of(&app, project, user).await, None);
        assert_eq!(role_of(&app, org, user).await, Some(OrgRole::Member));
    }

    #[tokio::test]
    async fn one_roles_recomputed_record_per_request() {
        let (app, seeded) = app_with(&["scim"]).await;
        let org = seeded.org_id;
        let mut ids = Vec::new();
        for email in ["a@acme.test", "b@acme.test", "c@acme.test"] {
            ids.push(managed(&app, org, email, OrgRole::Viewer).await.0);
        }
        group_with(
            &app,
            org,
            "members",
            ids.clone(),
            mapped(Some(OrgRole::Member)),
        )
        .await;
        recompute(
            &app,
            scope(&app, org, OrgRole::Viewer, None, true, true),
            ids,
        )
        .await;
        let records = details_of(&app, "scim.roles_recomputed").await;
        assert_eq!(records.len(), 1);
        assert!(records[0].contains("\"changed\":3"));
    }

    #[tokio::test]
    async fn refused_raises_write_one_record_with_a_count() {
        let (app, seeded) = app_with(&["scim"]).await;
        let org = seeded.org_id;
        let mut ids = Vec::new();
        for email in ["a@acme.test", "b@acme.test"] {
            ids.push(managed(&app, org, email, OrgRole::Viewer).await.0);
        }
        group_with(
            &app,
            org,
            "members",
            ids.clone(),
            mapped(Some(OrgRole::Member)),
        )
        .await;
        recompute(
            &app,
            scope(&app, org, OrgRole::Viewer, Some(1), true, true),
            ids,
        )
        .await;
        let records = details_of(&app, "scim.role_raise_refused").await;
        assert_eq!(records.len(), 1);
        assert!(records[0].contains("\"count\":2"));
    }

    #[tokio::test]
    async fn linking_an_existing_member_recomputes_at_once() {
        let (app, seeded) = app_with(&["scim", "org_sso"]).await;
        let existing = app
            .add_member(&seeded, "x@acme.test", OrgRole::Viewer)
            .await;
        let token = scim_org(&app, &seeded, "acme.test", OrgRole::Member).await;
        let response = create_user(&app, &token, "x@acme.test").await;
        assert_eq!(response.status, StatusCode::CREATED);
        // The link is a raise to the default role, applied in the same request.
        assert_eq!(
            role_of(&app, seeded.org_id, existing.id).await,
            Some(OrgRole::Member)
        );
    }

    #[tokio::test]
    async fn changing_the_default_role_recomputes_unmapped_members() {
        let (app, seeded) = app_with(&["scim", "org_sso"]).await;
        scim_org(&app, &seeded, "acme.test", OrgRole::Viewer).await;
        let (_, user) = managed(&app, seeded.org_id, "x@acme.test", OrgRole::Viewer).await;
        let response = app
            .request(Method::PATCH, PATH)
            .header("cookie", seeded.cookie.as_str())
            .json(&json!({"default_role": "member"}))
            .send()
            .await;
        assert_eq!(response.status, StatusCode::OK);
        assert_eq!(
            role_of(&app, seeded.org_id, user).await,
            Some(OrgRole::Member)
        );
    }

    #[tokio::test]
    async fn a_default_role_change_with_scim_off_recomputes_nothing() {
        let (app, seeded) = app_with(&["scim", "org_sso"]).await;
        let org = seeded.org_id;
        let (_, user) = managed(&app, org, "x@acme.test", OrgRole::Viewer).await;
        let now = app.state.clock.now_ns();
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    scim_store::put_org_scim(conn, org, false, OrgRole::Viewer, now).await?;
                    Ok(())
                })
            })
            .await
            .unwrap();
        let response = app
            .request(Method::PATCH, PATH)
            .header("cookie", seeded.cookie.as_str())
            .json(&json!({"default_role": "member"}))
            .send()
            .await;
        assert_eq!(response.status, StatusCode::OK);
        assert_eq!(role_of(&app, org, user).await, Some(OrgRole::Viewer));
        assert!(details_of(&app, "scim.roles_recomputed").await.is_empty());
    }

    #[tokio::test]
    async fn reactivation_uses_the_groups_role() {
        let (app, seeded) = app_with(&["scim", "org_sso"]).await;
        let org = seeded.org_id;
        let token = scim_org(&app, &seeded, "acme.test", OrgRole::Viewer).await;
        let (scim_id, user) = managed(&app, org, "x@acme.test", OrgRole::Member).await;
        group_with(
            &app,
            org,
            "members",
            vec![scim_id],
            mapped(Some(OrgRole::Member)),
        )
        .await;
        let path = format!("{SCIM_USERS}/{scim_id}");
        let off = json!({"schemas": [PATCH_OP], "Operations": [{"op": "replace", "value": {"active": false}}]});
        assert_eq!(
            scim_request(&app, &token, Method::PATCH, &path, &off)
                .await
                .status,
            StatusCode::OK
        );
        assert_eq!(role_of(&app, org, user).await, None);
        let on = json!({"schemas": [PATCH_OP], "Operations": [{"op": "replace", "value": {"active": true}}]});
        assert_eq!(
            scim_request(&app, &token, Method::PATCH, &path, &on)
                .await
                .status,
            StatusCode::OK
        );
        assert_eq!(role_of(&app, org, user).await, Some(OrgRole::Member));
    }

    /// A pending invitation of `email` to `org`, written as an invite of the owner would be.
    async fn pending_invite(app: &TestApp, org: OrgId, email: &str, role: OrgRole) {
        let now = app.state.clock.now_ns();
        let email = email.to_owned();
        let owner = seeded_owner(app, org).await;
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    invites::replace_invite(
                        conn,
                        org,
                        &email,
                        role,
                        "scim-recompute-test-hash",
                        owner,
                        now,
                        now + invites::INVITE_TTL_NS,
                    )
                    .await?;
                    Ok(())
                })
            })
            .await
            .unwrap();
    }

    /// The owner of `org` (the seeded account), for the inviter of a test invitation.
    async fn seeded_owner(app: &TestApp, org: OrgId) -> Uuid {
        let owner: String = sqlx::query_scalar(
            "SELECT user_id FROM org_members WHERE org_id = ? AND role = 'owner' LIMIT 1",
        )
        .bind(org.to_string())
        .fetch_one(app.state.meta.reader())
        .await
        .unwrap();
        Uuid::parse_str(&owner).unwrap()
    }

    #[tokio::test]
    async fn a_seat_refused_user_gets_no_raised_override() {
        let (app, seeded) = app_with(&["scim"]).await;
        let org = seeded.org_id;
        let project = seeded.project.id;
        let (scim_id, user) = managed(&app, org, "x@acme.test", OrgRole::Viewer).await;
        group_with(
            &app,
            org,
            "admins",
            vec![scim_id],
            GroupMapping {
                org_role: Some(OrgRole::Member),
                projects: vec![(project, ProjectOverride::Admin)],
            },
        )
        .await;
        // The owner takes the only seat, so the Viewer's raise to Member is refused, and its
        // raising project override is not written either.
        let limited = scope(&app, org, OrgRole::Viewer, Some(1), true, true);
        recompute(&app, limited, vec![scim_id]).await;
        assert_eq!(role_of(&app, org, user).await, Some(OrgRole::Viewer));
        assert_eq!(override_of(&app, project, user).await, None);
        assert_eq!(details_of(&app, "scim.role_raise_refused").await.len(), 1);
    }

    #[tokio::test]
    async fn a_raise_deletes_the_pending_invite() {
        let (app, seeded) = app_with(&["scim"]).await;
        let org = seeded.org_id;
        let (scim_id, user) = managed(&app, org, "x@acme.test", OrgRole::Viewer).await;
        group_with(
            &app,
            org,
            "members",
            vec![scim_id],
            mapped(Some(OrgRole::Member)),
        )
        .await;
        pending_invite(&app, org, "x@acme.test", OrgRole::Member).await;
        recompute(
            &app,
            scope(&app, org, OrgRole::Viewer, None, true, true),
            vec![scim_id],
        )
        .await;
        assert_eq!(role_of(&app, org, user).await, Some(OrgRole::Member));
        let left: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM invites WHERE org_id = ? AND email = 'x@acme.test' \
             AND accepted_at IS NULL",
        )
        .bind(org.to_string())
        .fetch_one(app.state.meta.reader())
        .await
        .unwrap();
        assert_eq!(left, 0);
    }

    #[tokio::test]
    async fn the_seat_check_excludes_the_users_own_invite() {
        let (app, seeded) = app_with(&["scim"]).await;
        let org = seeded.org_id;
        let (scim_id, user) = managed(&app, org, "x@acme.test", OrgRole::Viewer).await;
        group_with(
            &app,
            org,
            "members",
            vec![scim_id],
            mapped(Some(OrgRole::Member)),
        )
        .await;
        // Seats: the owner (1) plus the user's own invitation. The invitation is removed by the
        // raise, so it must not count against the limit of 2: the owner and the raised user fit.
        pending_invite(&app, org, "x@acme.test", OrgRole::Member).await;
        let limited = scope(&app, org, OrgRole::Viewer, Some(2), true, true);
        recompute(&app, limited, vec![scim_id]).await;
        assert_eq!(role_of(&app, org, user).await, Some(OrgRole::Member));
        assert!(details_of(&app, "scim.role_raise_refused").await.is_empty());
    }

    #[tokio::test]
    async fn inactive_and_non_member_users_are_skipped() {
        let (app, seeded) = app_with(&["scim"]).await;
        let org = seeded.org_id;
        let (inactive_id, inactive) = managed(&app, org, "i@acme.test", OrgRole::Viewer).await;
        let (removed_id, removed) = managed(&app, org, "n@acme.test", OrgRole::Viewer).await;
        group_with(
            &app,
            org,
            "members",
            vec![inactive_id, removed_id],
            mapped(Some(OrgRole::Member)),
        )
        .await;
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    scim_store::set_user_active(conn, org, inactive_id, false, 0).await?;
                    sqlx::query("DELETE FROM org_members WHERE org_id = ? AND user_id = ?")
                        .bind(org.to_string())
                        .bind(removed.to_string())
                        .execute(&mut *conn)
                        .await?;
                    Ok(())
                })
            })
            .await
            .unwrap();
        recompute(
            &app,
            scope(&app, org, OrgRole::Viewer, None, true, true),
            vec![inactive_id, removed_id],
        )
        .await;
        assert_eq!(role_of(&app, org, inactive).await, Some(OrgRole::Viewer));
        assert_eq!(role_of(&app, org, removed).await, None);
        assert!(details_of(&app, "scim.roles_recomputed").await.is_empty());
    }
}
