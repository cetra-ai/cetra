// SPDX-License-Identifier: Elastic-2.0
//! Storage of SCIM provisioning: the per-org settings, the bearer tokens and the provisioned
//! users (track EA M5, D-EA-12 to D-EA-19). The `scim_users` row is per org; the `users` row
//! is instance-wide. Names and emails are never logged or recorded from here.

use std::collections::{HashMap, HashSet};
use std::str::FromStr;

use cetra_core::ids::{OrgId, ProjectId};
use sqlx::{SqliteConnection, SqlitePool};
use uuid::Uuid;

use crate::auth::{
    OrgRole, ProjectOverride, org_role_str, parse_org_role, parse_project_override,
    project_override_str,
};
use crate::meta::{MetaError, unique_or};

/// Max live tokens per org (D-EA-15).
pub(crate) const MAX_LIVE_TOKENS: u64 = 2;

/// One `org_scim` row. A missing row is disabled and `viewer`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OrgScim {
    pub(crate) enabled: bool,
    /// `viewer` or `member` (D-EA-14, P-4).
    pub(crate) default_role: OrgRole,
}

/// One `scim_tokens` row. The plaintext and the hash are not here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ScimTokenRow {
    pub(crate) id: Uuid,
    pub(crate) org_id: OrgId,
    pub(crate) name: String,
    pub(crate) prefix: String,
    pub(crate) created_by: Option<Uuid>,
    pub(crate) created_at: i64,
    pub(crate) last_used_at: Option<i64>,
    pub(crate) revoked_at: Option<i64>,
}

/// One `scim_users` row. It holds names and emails, so it has no `Debug`.
#[derive(Clone)]
pub(crate) struct ScimUserRow {
    pub(crate) id: Uuid,
    pub(crate) user_id: Uuid,
    /// The normalized email.
    pub(crate) user_name: String,
    pub(crate) external_id: Option<String>,
    pub(crate) given_name: Option<String>,
    pub(crate) family_name: Option<String>,
    pub(crate) display_name: Option<String>,
    pub(crate) department: Option<String>,
    pub(crate) employee_number: Option<String>,
    pub(crate) active: bool,
    pub(crate) created_at: i64,
    pub(crate) updated_at: i64,
}

/// The columns a SCIM user carries besides its links and flags. It holds names and emails,
/// so it has no `Debug`.
#[derive(Clone)]
pub(crate) struct ScimUserFields {
    /// The normalized email.
    pub(crate) user_name: String,
    pub(crate) external_id: Option<String>,
    pub(crate) given_name: Option<String>,
    pub(crate) family_name: Option<String>,
    pub(crate) display_name: Option<String>,
    pub(crate) department: Option<String>,
    pub(crate) employee_number: Option<String>,
}

/// What a list filter matches (C2 parses it). It holds an email or an external id, so it has
/// no `Debug`.
#[derive(Clone)]
pub(crate) enum UserFilter {
    UserName(String),
    ExternalId(String),
    /// Matches `user_name`, which is the email.
    Email(String),
}

/// The `org_scim` row, or the defaults when there is none.
///
/// # Errors
///
/// Returns [`MetaError::Corrupt`] when `default_role` is not `viewer` or `member`.
pub(crate) async fn org_scim(pool: &SqlitePool, org: OrgId) -> Result<OrgScim, MetaError> {
    let record: Option<OrgScimRecord> =
        sqlx::query_as("SELECT enabled, default_role FROM org_scim WHERE org_id = ?")
            .bind(org.to_string())
            .fetch_optional(pool)
            .await?;
    org_scim_from(record)
}

/// [`org_scim`] inside the caller's transaction.
///
/// # Errors
///
/// Returns [`MetaError`] as [`org_scim`] does.
pub(crate) async fn org_scim_tx(
    conn: &mut SqliteConnection,
    org: OrgId,
) -> Result<OrgScim, MetaError> {
    let record: Option<OrgScimRecord> =
        sqlx::query_as("SELECT enabled, default_role FROM org_scim WHERE org_id = ?")
            .bind(org.to_string())
            .fetch_optional(&mut *conn)
            .await?;
    org_scim_from(record)
}

/// Inserts or replaces the `org_scim` row. The caller passes `viewer` or `member` only.
///
/// # Errors
///
/// Returns [`MetaError::Db`] when the upsert fails.
pub(crate) async fn put_org_scim(
    conn: &mut SqliteConnection,
    org: OrgId,
    enabled: bool,
    default_role: OrgRole,
    now: i64,
) -> Result<(), MetaError> {
    sqlx::query(
        "INSERT INTO org_scim (org_id, enabled, default_role, updated_at) VALUES (?, ?, ?, ?) \
         ON CONFLICT (org_id) DO UPDATE SET enabled = excluded.enabled, \
         default_role = excluded.default_role, updated_at = excluded.updated_at",
    )
    .bind(org.to_string())
    .bind(i64::from(enabled))
    .bind(org_role_str(default_role))
    .bind(now)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Inserts a live token. The count of live tokens and the insert are one write transaction.
///
/// `hash` is the SHA-256 of the plaintext, lowercase hex. A duplicate hash is
/// [`MetaError::Conflict`] `"duplicate token"`.
///
/// # Errors
///
/// Returns [`MetaError::Conflict`] `"at most two live SCIM tokens"` when the org already
/// has [`MAX_LIVE_TOKENS`] live tokens, and [`MetaError`] when a query fails.
pub(crate) async fn insert_token(
    conn: &mut SqliteConnection,
    org: OrgId,
    name: &str,
    prefix: &str,
    hash: &str,
    created_by: Uuid,
    now: i64,
) -> Result<ScimTokenRow, MetaError> {
    let live: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM scim_tokens WHERE org_id = ? AND revoked_at IS NULL",
    )
    .bind(org.to_string())
    .fetch_one(&mut *conn)
    .await?;
    if u64::try_from(live).unwrap_or(u64::MAX) >= MAX_LIVE_TOKENS {
        return Err(MetaError::Conflict("at most two live SCIM tokens"));
    }
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO scim_tokens (id, org_id, name, prefix, token_hash, created_by, created_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(id.to_string())
    .bind(org.to_string())
    .bind(name)
    .bind(prefix)
    .bind(hash)
    .bind(created_by.to_string())
    .bind(now)
    .execute(&mut *conn)
    .await
    .map_err(|error| unique_or(error, "duplicate token"))?;
    Ok(ScimTokenRow {
        id,
        org_id: org,
        name: name.to_owned(),
        prefix: prefix.to_owned(),
        created_by: Some(created_by),
        created_at: now,
        last_used_at: None,
        revoked_at: None,
    })
}

/// Live tokens of `org`, newest first.
///
/// # Errors
///
/// Returns [`MetaError`] when the query fails or a stored value does not decode.
pub(crate) async fn list_tokens(
    pool: &SqlitePool,
    org: OrgId,
) -> Result<Vec<ScimTokenRow>, MetaError> {
    let records: Vec<TokenRecord> = sqlx::query_as(
        "SELECT id, org_id, name, prefix, created_by, created_at, last_used_at, revoked_at \
         FROM scim_tokens WHERE org_id = ? AND revoked_at IS NULL \
         ORDER BY created_at DESC, id DESC",
    )
    .bind(org.to_string())
    .fetch_all(pool)
    .await?;
    records.into_iter().map(token_row).collect()
}

/// Revokes one live token of `org`. `None` means it is missing or already revoked.
///
/// The string is the token hash, so the caller can drop the cache entry after commit.
///
/// # Errors
///
/// Returns [`MetaError::Db`] when the update fails.
pub(crate) async fn revoke_token(
    conn: &mut SqliteConnection,
    org: OrgId,
    id: Uuid,
    now: i64,
) -> Result<Option<String>, MetaError> {
    let row: Option<HashRecord> = sqlx::query_as(
        "UPDATE scim_tokens SET revoked_at = ? \
         WHERE org_id = ? AND id = ? AND revoked_at IS NULL \
         RETURNING token_hash",
    )
    .bind(now)
    .bind(org.to_string())
    .bind(id.to_string())
    .fetch_optional(&mut *conn)
    .await?;
    Ok(row.map(|row| row.token_hash))
}

/// The token with this hash, revoked or not, and whether its org is active. The caller
/// checks `revoked_at`.
///
/// # Errors
///
/// Returns [`MetaError`] when a query fails or a stored value does not decode.
pub(crate) async fn token_by_hash(
    pool: &SqlitePool,
    hash: &str,
) -> Result<Option<(ScimTokenRow, bool)>, MetaError> {
    let record: Option<TokenRecord> = sqlx::query_as(
        "SELECT id, org_id, name, prefix, created_by, created_at, last_used_at, revoked_at \
         FROM scim_tokens WHERE token_hash = ?",
    )
    .bind(hash)
    .fetch_optional(pool)
    .await?;
    let Some(record) = record else {
        return Ok(None);
    };
    let row = token_row(record)?;
    let state: Option<String> = sqlx::query_scalar("SELECT state FROM orgs WHERE id = ?")
        .bind(row.org_id.to_string())
        .fetch_optional(pool)
        .await?;
    Ok(Some((row, state.as_deref() == Some("active"))))
}

/// Sets `last_used_at` when the stored value is absent or earlier. One statement per row.
///
/// The caller holds the transaction.
///
/// # Errors
///
/// Returns [`MetaError`] when an update fails.
pub(crate) async fn set_tokens_last_used(
    conn: &mut SqliteConnection,
    rows: &[(Uuid, i64)],
) -> Result<(), MetaError> {
    for (id, at) in rows {
        sqlx::query(
            "UPDATE scim_tokens SET last_used_at = ? \
             WHERE id = ? AND (last_used_at IS NULL OR last_used_at < ?)",
        )
        .bind(at)
        .bind(id.to_string())
        .bind(at)
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

/// Inserts a provisioned user, active and with its account enabled.
///
/// A duplicate `user_name`, `external_id` or `user_id` in the org is
/// [`MetaError::Conflict`] `"uniqueness"`.
///
/// # Errors
///
/// Returns [`MetaError`] when the insert fails.
pub(crate) async fn insert_user(
    conn: &mut SqliteConnection,
    org: OrgId,
    user_id: Uuid,
    fields: &ScimUserFields,
    now: i64,
) -> Result<ScimUserRow, MetaError> {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO scim_users (id, org_id, user_id, user_name, external_id, given_name, \
         family_name, display_name, department, employee_number, active, created_at, updated_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 1, ?, ?)",
    )
    .bind(id.to_string())
    .bind(org.to_string())
    .bind(user_id.to_string())
    .bind(fields.user_name.as_str())
    .bind(fields.external_id.as_deref())
    .bind(fields.given_name.as_deref())
    .bind(fields.family_name.as_deref())
    .bind(fields.display_name.as_deref())
    .bind(fields.department.as_deref())
    .bind(fields.employee_number.as_deref())
    .bind(now)
    .bind(now)
    .execute(&mut *conn)
    .await
    .map_err(|error| unique_or(error, "uniqueness"))?;
    Ok(ScimUserRow {
        id,
        user_id,
        user_name: fields.user_name.clone(),
        external_id: fields.external_id.clone(),
        given_name: fields.given_name.clone(),
        family_name: fields.family_name.clone(),
        display_name: fields.display_name.clone(),
        department: fields.department.clone(),
        employee_number: fields.employee_number.clone(),
        active: true,
        created_at: now,
        updated_at: now,
    })
}

/// The SCIM user with this id in `org`, inside the caller's transaction.
///
/// # Errors
///
/// Returns [`MetaError`] when the query fails or a stored value does not decode.
pub(crate) async fn user_tx(
    conn: &mut SqliteConnection,
    org: OrgId,
    id: Uuid,
) -> Result<Option<ScimUserRow>, MetaError> {
    let sql = format!("SELECT {USER_COLUMNS} FROM scim_users WHERE org_id = ? AND id = ?");
    let record: Option<UserRecord> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .bind(org.to_string())
        .bind(id.to_string())
        .fetch_optional(&mut *conn)
        .await?;
    record.map(user_row).transpose()
}

/// The SCIM user with this id in `org`.
///
/// # Errors
///
/// Returns [`MetaError`] when the query fails or a stored value does not decode.
pub(crate) async fn user(
    pool: &SqlitePool,
    org: OrgId,
    id: Uuid,
) -> Result<Option<ScimUserRow>, MetaError> {
    let sql = format!("SELECT {USER_COLUMNS} FROM scim_users WHERE org_id = ? AND id = ?");
    let record: Option<UserRecord> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .bind(org.to_string())
        .bind(id.to_string())
        .fetch_optional(pool)
        .await?;
    record.map(user_row).transpose()
}

/// The SCIM user of `org` with this normalized `user_name`, in the caller's transaction.
///
/// # Errors
///
/// Returns [`MetaError`] when the query fails or a stored value does not decode.
pub(crate) async fn user_by_user_name_tx(
    conn: &mut SqliteConnection,
    org: OrgId,
    user_name: &str,
) -> Result<Option<ScimUserRow>, MetaError> {
    let sql = format!("SELECT {USER_COLUMNS} FROM scim_users WHERE org_id = ? AND user_name = ?");
    let record: Option<UserRecord> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .bind(org.to_string())
        .bind(user_name)
        .fetch_optional(&mut *conn)
        .await?;
    record.map(user_row).transpose()
}

/// The SCIM user of `org` with this `externalId`, in the caller's transaction.
///
/// # Errors
///
/// Returns [`MetaError`] when the query fails or a stored value does not decode.
pub(crate) async fn user_by_external_id_tx(
    conn: &mut SqliteConnection,
    org: OrgId,
    external_id: &str,
) -> Result<Option<ScimUserRow>, MetaError> {
    let sql = format!("SELECT {USER_COLUMNS} FROM scim_users WHERE org_id = ? AND external_id = ?");
    let record: Option<UserRecord> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .bind(org.to_string())
        .bind(external_id)
        .fetch_optional(&mut *conn)
        .await?;
    record.map(user_row).transpose()
}

/// One page of the SCIM users of `org`, oldest first, and the total that matches `filter`.
///
/// `start_index` is 1-based (RFC 7644 § 3.4.2.4); a value of 0 starts at the first row. The
/// caller clamps `count`.
///
/// # Errors
///
/// Returns [`MetaError`] when a query fails or a stored value does not decode.
pub(crate) async fn list_users(
    pool: &SqlitePool,
    org: OrgId,
    filter: Option<&UserFilter>,
    start_index: u32,
    count: u32,
) -> Result<(u64, Vec<ScimUserRow>), MetaError> {
    let (clause, value) = filter_clause(filter);

    let count_sql = format!("SELECT COUNT(*) FROM scim_users WHERE org_id = ?{clause}");
    let mut total =
        sqlx::query_scalar::<_, i64>(sqlx::AssertSqlSafe(count_sql)).bind(org.to_string());
    if let Some(value) = value {
        total = total.bind(value);
    }
    let total = total.fetch_one(pool).await?;

    let page_sql = format!(
        "SELECT {USER_COLUMNS} FROM scim_users WHERE org_id = ?{clause} \
         ORDER BY created_at, id LIMIT ? OFFSET ?"
    );
    let mut page =
        sqlx::query_as::<_, UserRecord>(sqlx::AssertSqlSafe(page_sql)).bind(org.to_string());
    if let Some(value) = value {
        page = page.bind(value);
    }
    let records = page
        .bind(i64::from(count))
        .bind(i64::from(start_index.saturating_sub(1)))
        .fetch_all(pool)
        .await?;
    let rows = records
        .into_iter()
        .map(user_row)
        .collect::<Result<Vec<_>, _>>()?;
    Ok((u64::try_from(total).unwrap_or(0), rows))
}

/// Replaces the attributes of a SCIM user. The caller has checked that the row exists.
///
/// # Errors
///
/// Returns [`MetaError::Conflict`] `"uniqueness"` when another row of the org has the same
/// `user_name` or `external_id`, and [`MetaError::Db`] when the update fails.
pub(crate) async fn update_user(
    conn: &mut SqliteConnection,
    org: OrgId,
    id: Uuid,
    fields: &ScimUserFields,
    now: i64,
) -> Result<(), MetaError> {
    sqlx::query(
        "UPDATE scim_users SET user_name = ?, external_id = ?, given_name = ?, family_name = ?, \
         display_name = ?, department = ?, employee_number = ?, updated_at = ? \
         WHERE org_id = ? AND id = ?",
    )
    .bind(fields.user_name.as_str())
    .bind(fields.external_id.as_deref())
    .bind(fields.given_name.as_deref())
    .bind(fields.family_name.as_deref())
    .bind(fields.display_name.as_deref())
    .bind(fields.department.as_deref())
    .bind(fields.employee_number.as_deref())
    .bind(now)
    .bind(org.to_string())
    .bind(id.to_string())
    .execute(&mut *conn)
    .await
    .map_err(|error| unique_or(error, "uniqueness"))?;
    Ok(())
}

/// Sets the SCIM row's `active` flag.
///
/// # Errors
///
/// Returns [`MetaError::Db`] when the update fails.
pub(crate) async fn set_user_active(
    conn: &mut SqliteConnection,
    org: OrgId,
    id: Uuid,
    active: bool,
    now: i64,
) -> Result<(), MetaError> {
    sqlx::query(
        "UPDATE scim_users SET active = ?, updated_at = ? \
         WHERE org_id = ? AND id = ?",
    )
    .bind(i64::from(active))
    .bind(now)
    .bind(org.to_string())
    .bind(id.to_string())
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Deletes the SCIM row. `false` means it was missing.
///
/// # Errors
///
/// Returns [`MetaError::Db`] when the delete fails.
pub(crate) async fn delete_user(
    conn: &mut SqliteConnection,
    org: OrgId,
    id: Uuid,
) -> Result<bool, MetaError> {
    let changed = sqlx::query("DELETE FROM scim_users WHERE org_id = ? AND id = ?")
        .bind(org.to_string())
        .bind(id.to_string())
        .execute(&mut *conn)
        .await?
        .rows_affected();
    Ok(changed == 1)
}

/// Records that SCIM disabled the account `user` (D-EA-16), for `org`. A later create or
/// reactivation re-enables it. Replaces an earlier row.
///
/// # Errors
///
/// Returns [`MetaError::Db`] when the upsert fails.
pub(crate) async fn mark_scim_disabled(
    conn: &mut SqliteConnection,
    org: OrgId,
    user: Uuid,
    now: i64,
) -> Result<(), MetaError> {
    sqlx::query(
        "INSERT INTO scim_disabled_accounts (user_id, org_id, disabled_at) VALUES (?, ?, ?) \
         ON CONFLICT (user_id) DO UPDATE SET org_id = excluded.org_id, \
         disabled_at = excluded.disabled_at",
    )
    .bind(user.to_string())
    .bind(org.to_string())
    .bind(now)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Whether SCIM disabled the account `user`. An account disabled any other way has no row.
///
/// # Errors
///
/// Returns [`MetaError::Db`] when the query fails.
pub(crate) async fn is_scim_disabled_tx(
    conn: &mut SqliteConnection,
    user: Uuid,
) -> Result<bool, MetaError> {
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM scim_disabled_accounts WHERE user_id = ?")
            .bind(user.to_string())
            .fetch_one(&mut *conn)
            .await?;
    Ok(count > 0)
}

/// Forgets that SCIM disabled the account `user`, once it is enabled again.
///
/// # Errors
///
/// Returns [`MetaError::Db`] when the delete fails.
pub(crate) async fn clear_scim_disabled(
    conn: &mut SqliteConnection,
    user: Uuid,
) -> Result<(), MetaError> {
    sqlx::query("DELETE FROM scim_disabled_accounts WHERE user_id = ?")
        .bind(user.to_string())
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// Whether an active SCIM row of `org` links `user_id` (D-EA-18).
///
/// # Errors
///
/// Returns [`MetaError::Db`] when the query fails.
pub(crate) async fn is_managed_tx(
    conn: &mut SqliteConnection,
    org: OrgId,
    user_id: Uuid,
) -> Result<bool, MetaError> {
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM scim_users WHERE org_id = ? AND user_id = ? AND active = 1",
    )
    .bind(org.to_string())
    .bind(user_id.to_string())
    .fetch_one(&mut *conn)
    .await?;
    Ok(count > 0)
}

/// The user ids of the active SCIM rows of `org`.
///
/// # Errors
///
/// Returns [`MetaError::Corrupt`] when a stored id does not parse, and [`MetaError::Db`]
/// when the query fails.
pub(crate) async fn managed_user_ids(
    pool: &SqlitePool,
    org: OrgId,
) -> Result<HashSet<Uuid>, MetaError> {
    let ids: Vec<String> =
        sqlx::query_scalar("SELECT user_id FROM scim_users WHERE org_id = ? AND active = 1")
            .bind(org.to_string())
            .fetch_all(pool)
            .await?;
    ids.iter()
        .map(|id| parse_id::<Uuid>(id, "scim_users", "user_id"))
        .collect()
}

/// One `scim_groups` row (track EA M6). It holds a display name, so it has no `Debug`.
#[derive(Clone)]
pub(crate) struct ScimGroupRow {
    pub(crate) id: Uuid,
    pub(crate) display_name: String,
    pub(crate) external_id: Option<String>,
    /// Times feed the resource's `meta`.
    pub(crate) created_at: i64,
    pub(crate) updated_at: i64,
}

/// What a group list filter matches. It holds a name or an external id, so it has no `Debug`.
#[derive(Clone)]
pub(crate) enum GroupFilter {
    /// Matches `display_name` with SQLite `=`, which is case-sensitive.
    DisplayName(String),
    ExternalId(String),
}

/// One group's role mapping (D-EA-14). `org_role` is `None` when the group maps no org role.
/// `projects` holds one entry per project; a project set to `none` hides it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GroupMapping {
    pub(crate) org_role: Option<OrgRole>,
    pub(crate) projects: Vec<(ProjectId, ProjectOverride)>,
}

/// Inserts a group of `org`, with no members yet.
///
/// A `display_name` or `external_id` already used in the org is [`MetaError::Conflict`]
/// `"uniqueness"`.
///
/// # Errors
///
/// Returns [`MetaError`] when the insert fails.
pub(crate) async fn insert_group(
    conn: &mut SqliteConnection,
    org: OrgId,
    display_name: &str,
    external_id: Option<&str>,
    now: i64,
) -> Result<ScimGroupRow, MetaError> {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO scim_groups (id, org_id, display_name, external_id, created_at, updated_at) \
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(id.to_string())
    .bind(org.to_string())
    .bind(display_name)
    .bind(external_id)
    .bind(now)
    .bind(now)
    .execute(&mut *conn)
    .await
    .map_err(|error| unique_or(error, "uniqueness"))?;
    Ok(ScimGroupRow {
        id,
        display_name: display_name.to_owned(),
        external_id: external_id.map(str::to_owned),
        created_at: now,
        updated_at: now,
    })
}

/// The group with this id in `org`, inside the caller's transaction.
///
/// # Errors
///
/// Returns [`MetaError`] when the query fails or a stored value does not decode.
pub(crate) async fn group_tx(
    conn: &mut SqliteConnection,
    org: OrgId,
    id: Uuid,
) -> Result<Option<ScimGroupRow>, MetaError> {
    let sql = format!("SELECT {GROUP_COLUMNS} FROM scim_groups WHERE org_id = ? AND id = ?");
    let record: Option<GroupRecord> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .bind(org.to_string())
        .bind(id.to_string())
        .fetch_optional(&mut *conn)
        .await?;
    record.map(group_row).transpose()
}

/// One page of the groups of `org`, oldest first, and the total that matches `filter`.
///
/// `start_index` is 1-based; a value of 0 starts at the first row. The caller clamps `count`.
///
/// # Errors
///
/// Returns [`MetaError`] when a query fails or a stored value does not decode.
pub(crate) async fn list_groups(
    pool: &SqlitePool,
    org: OrgId,
    filter: Option<&GroupFilter>,
    start_index: u32,
    count: u32,
) -> Result<(u64, Vec<ScimGroupRow>), MetaError> {
    let (clause, value) = group_filter_clause(filter);

    let count_sql = format!("SELECT COUNT(*) FROM scim_groups WHERE org_id = ?{clause}");
    let mut total =
        sqlx::query_scalar::<_, i64>(sqlx::AssertSqlSafe(count_sql)).bind(org.to_string());
    if let Some(value) = value {
        total = total.bind(value);
    }
    let total = total.fetch_one(pool).await?;

    let page_sql = format!(
        "SELECT {GROUP_COLUMNS} FROM scim_groups WHERE org_id = ?{clause} \
         ORDER BY created_at, id LIMIT ? OFFSET ?"
    );
    let mut page =
        sqlx::query_as::<_, GroupRecord>(sqlx::AssertSqlSafe(page_sql)).bind(org.to_string());
    if let Some(value) = value {
        page = page.bind(value);
    }
    let records = page
        .bind(i64::from(count))
        .bind(i64::from(start_index.saturating_sub(1)))
        .fetch_all(pool)
        .await?;
    let rows = records
        .into_iter()
        .map(group_row)
        .collect::<Result<Vec<_>, _>>()?;
    Ok((u64::try_from(total).unwrap_or(0), rows))
}

/// Every group of `org`, ordered by display name (the settings page).
///
/// # Errors
///
/// Returns [`MetaError`] when the query fails or a stored value does not decode.
pub(crate) async fn all_groups(
    pool: &SqlitePool,
    org: OrgId,
) -> Result<Vec<ScimGroupRow>, MetaError> {
    let sql = format!(
        "SELECT {GROUP_COLUMNS} FROM scim_groups WHERE org_id = ? ORDER BY display_name, id"
    );
    let records: Vec<GroupRecord> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .bind(org.to_string())
        .fetch_all(pool)
        .await?;
    records.into_iter().map(group_row).collect()
}

/// Replaces the name and external id of a group. The caller has checked that the row exists.
///
/// # Errors
///
/// Returns [`MetaError::Conflict`] `"uniqueness"` when another group of the org has the same
/// `display_name` or `external_id`, and [`MetaError::Db`] when the update fails.
pub(crate) async fn update_group(
    conn: &mut SqliteConnection,
    org: OrgId,
    id: Uuid,
    display_name: &str,
    external_id: Option<&str>,
    now: i64,
) -> Result<(), MetaError> {
    sqlx::query(
        "UPDATE scim_groups SET display_name = ?, external_id = ?, updated_at = ? \
         WHERE org_id = ? AND id = ?",
    )
    .bind(display_name)
    .bind(external_id)
    .bind(now)
    .bind(org.to_string())
    .bind(id.to_string())
    .execute(&mut *conn)
    .await
    .map_err(|error| unique_or(error, "uniqueness"))?;
    Ok(())
}

/// Deletes a group. Its members and its mapping go with it (cascade). `false` means it was
/// missing.
///
/// # Errors
///
/// Returns [`MetaError::Db`] when the delete fails.
pub(crate) async fn delete_group(
    conn: &mut SqliteConnection,
    org: OrgId,
    id: Uuid,
) -> Result<bool, MetaError> {
    let changed = sqlx::query("DELETE FROM scim_groups WHERE org_id = ? AND id = ?")
        .bind(org.to_string())
        .bind(id.to_string())
        .execute(&mut *conn)
        .await?
        .rows_affected();
    Ok(changed == 1)
}

/// The SCIM user ids that are members of `group`, in the caller's transaction.
///
/// # Errors
///
/// Returns [`MetaError::Corrupt`] when a stored id does not parse, and [`MetaError::Db`] when
/// the query fails.
pub(crate) async fn group_member_ids_tx(
    conn: &mut SqliteConnection,
    group: Uuid,
) -> Result<Vec<Uuid>, MetaError> {
    let ids: Vec<String> = sqlx::query_scalar(
        "SELECT scim_user_id FROM scim_group_members WHERE group_id = ? ORDER BY scim_user_id",
    )
    .bind(group.to_string())
    .fetch_all(&mut *conn)
    .await?;
    ids.iter()
        .map(|id| parse_id::<Uuid>(id, "scim_group_members", "scim_user_id"))
        .collect()
}

/// [`group_member_ids_tx`] on a pool connection.
///
/// # Errors
///
/// Returns [`MetaError`] as [`group_member_ids_tx`] does.
pub(crate) async fn group_member_ids(
    pool: &SqlitePool,
    group: Uuid,
) -> Result<Vec<Uuid>, MetaError> {
    let mut conn = pool.acquire().await?;
    group_member_ids_tx(&mut conn, group).await
}

/// The member count of every group of `org`. A group with no members counts 0.
///
/// # Errors
///
/// Returns [`MetaError::Corrupt`] when a stored id does not parse, and [`MetaError::Db`] when
/// the query fails.
pub(crate) async fn member_counts(
    pool: &SqlitePool,
    org: OrgId,
) -> Result<HashMap<Uuid, u32>, MetaError> {
    let rows: Vec<(String, i64)> = sqlx::query_as(
        "SELECT g.id, COUNT(m.scim_user_id) FROM scim_groups g \
         LEFT JOIN scim_group_members m ON m.group_id = g.id \
         WHERE g.org_id = ? GROUP BY g.id",
    )
    .bind(org.to_string())
    .fetch_all(pool)
    .await?;
    rows.into_iter()
        .map(|(id, count)| -> Result<(Uuid, u32), MetaError> {
            let id = parse_id::<Uuid>(&id, "scim_groups", "id")?;
            Ok((id, u32::try_from(count).unwrap_or(u32::MAX)))
        })
        .collect()
}

/// Adds the given SCIM users to `group`. Ids that are not SCIM users of `org` are ignored, so
/// another org's user can never join. The caller has checked that `group` belongs to `org`.
/// Returns the ids added; a user already in the group is not in the result.
///
/// # Errors
///
/// Returns [`MetaError::Db`] when an insert fails.
pub(crate) async fn add_group_members_tx(
    conn: &mut SqliteConnection,
    org: OrgId,
    group: Uuid,
    users: &[Uuid],
) -> Result<Vec<Uuid>, MetaError> {
    let mut added = Vec::new();
    for user in users {
        let changed = sqlx::query(
            "INSERT OR IGNORE INTO scim_group_members (group_id, scim_user_id) \
             SELECT ?, id FROM scim_users WHERE org_id = ? AND id = ?",
        )
        .bind(group.to_string())
        .bind(org.to_string())
        .bind(user.to_string())
        .execute(&mut *conn)
        .await?
        .rows_affected();
        if changed == 1 {
            added.push(*user);
        }
    }
    Ok(added)
}

/// Removes the given SCIM users from `group`. Returns the ids removed; a user not in the group
/// is not in the result.
///
/// # Errors
///
/// Returns [`MetaError::Db`] when a delete fails.
pub(crate) async fn remove_group_members_tx(
    conn: &mut SqliteConnection,
    group: Uuid,
    users: &[Uuid],
) -> Result<Vec<Uuid>, MetaError> {
    let mut removed = Vec::new();
    for user in users {
        let changed =
            sqlx::query("DELETE FROM scim_group_members WHERE group_id = ? AND scim_user_id = ?")
                .bind(group.to_string())
                .bind(user.to_string())
                .execute(&mut *conn)
                .await?
                .rows_affected();
        if changed == 1 {
            removed.push(*user);
        }
    }
    Ok(removed)
}

/// The group ids that the SCIM user `scim_user` belongs to.
///
/// # Errors
///
/// Returns [`MetaError::Corrupt`] when a stored id does not parse, and [`MetaError::Db`] when
/// the query fails.
pub(crate) async fn groups_of_user_tx(
    conn: &mut SqliteConnection,
    scim_user: Uuid,
) -> Result<Vec<Uuid>, MetaError> {
    let ids: Vec<String> = sqlx::query_scalar(
        "SELECT group_id FROM scim_group_members WHERE scim_user_id = ? ORDER BY group_id",
    )
    .bind(scim_user.to_string())
    .fetch_all(&mut *conn)
    .await?;
    ids.iter()
        .map(|id| parse_id::<Uuid>(id, "scim_group_members", "group_id"))
        .collect()
}

/// The role mappings of the groups of `org`, keyed by group id. A group with no mapping is
/// absent.
///
/// # Errors
///
/// Returns [`MetaError::Corrupt`] when a stored value does not decode, and [`MetaError::Db`]
/// when a query fails.
pub(crate) async fn mappings(
    pool: &SqlitePool,
    org: OrgId,
) -> Result<HashMap<Uuid, GroupMapping>, MetaError> {
    let mut conn = pool.acquire().await?;
    load_mappings(&mut conn, org).await
}

/// [`mappings`] inside the caller's transaction.
///
/// # Errors
///
/// Returns [`MetaError`] as [`mappings`] does.
pub(crate) async fn mappings_tx(
    conn: &mut SqliteConnection,
    org: OrgId,
) -> Result<HashMap<Uuid, GroupMapping>, MetaError> {
    load_mappings(conn, org).await
}

/// Replaces the mapping of `group`: its org role and its project rows (D-EA-14). The caller
/// has validated the roles and checked that the group belongs to the path's org.
///
/// A group with neither an org role nor project rows keeps no mapping row.
///
/// # Errors
///
/// Returns [`MetaError::Conflict`] `"owner is not mappable"` when `org_role` is owner, and
/// `"duplicate project"` when a project appears twice; both are refused before any write.
/// Returns [`MetaError::Db`] when a write fails.
pub(crate) async fn put_mapping(
    conn: &mut SqliteConnection,
    group: Uuid,
    mapping: &GroupMapping,
    now: i64,
) -> Result<(), MetaError> {
    if mapping.org_role == Some(OrgRole::Owner) {
        return Err(MetaError::Conflict("owner is not mappable"));
    }
    let mut seen = HashSet::new();
    if !mapping
        .projects
        .iter()
        .all(|(project, _)| seen.insert(*project))
    {
        return Err(MetaError::Conflict("duplicate project"));
    }

    sqlx::query("DELETE FROM scim_group_project_roles WHERE group_id = ?")
        .bind(group.to_string())
        .execute(&mut *conn)
        .await?;
    for (project, role) in &mapping.projects {
        sqlx::query(
            "INSERT INTO scim_group_project_roles (group_id, project_id, role) VALUES (?, ?, ?)",
        )
        .bind(group.to_string())
        .bind(project.to_string())
        .bind(project_override_str(*role))
        .execute(&mut *conn)
        .await?;
    }

    if mapping.org_role.is_none() && mapping.projects.is_empty() {
        sqlx::query("DELETE FROM scim_group_mappings WHERE group_id = ?")
            .bind(group.to_string())
            .execute(&mut *conn)
            .await?;
    } else {
        sqlx::query(
            "INSERT INTO scim_group_mappings (group_id, org_role, updated_at) VALUES (?, ?, ?) \
             ON CONFLICT (group_id) DO UPDATE SET org_role = excluded.org_role, \
             updated_at = excluded.updated_at",
        )
        .bind(group.to_string())
        .bind(mapping.org_role.map(org_role_str))
        .bind(now)
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

/// Removes the org role and every project row of `group`. `false` means it had neither.
///
/// # Errors
///
/// Returns [`MetaError::Db`] when a delete fails.
pub(crate) async fn delete_mapping(
    conn: &mut SqliteConnection,
    group: Uuid,
) -> Result<bool, MetaError> {
    let projects = sqlx::query("DELETE FROM scim_group_project_roles WHERE group_id = ?")
        .bind(group.to_string())
        .execute(&mut *conn)
        .await?
        .rows_affected();
    let org = sqlx::query("DELETE FROM scim_group_mappings WHERE group_id = ?")
        .bind(group.to_string())
        .execute(&mut *conn)
        .await?
        .rows_affected();
    Ok(projects + org > 0)
}

/// The ids of the active SCIM users of `org`, oldest first (`created_at, id`).
///
/// # Errors
///
/// Returns [`MetaError::Corrupt`] when a stored id does not parse, and [`MetaError::Db`] when
/// the query fails.
pub(crate) async fn active_user_ids_tx(
    conn: &mut SqliteConnection,
    org: OrgId,
) -> Result<Vec<Uuid>, MetaError> {
    let ids: Vec<String> = sqlx::query_scalar(
        "SELECT id FROM scim_users WHERE org_id = ? AND active = 1 ORDER BY created_at, id",
    )
    .bind(org.to_string())
    .fetch_all(&mut *conn)
    .await?;
    ids.iter()
        .map(|id| parse_id::<Uuid>(id, "scim_users", "id"))
        .collect()
}

/// Every SCIM user of `org` with its group ids, in one query: the map is keyed by the SCIM
/// user id. A user in no group has no entry.
///
/// # Errors
///
/// Returns [`MetaError::Corrupt`] when a stored id does not parse, and [`MetaError::Db`] when
/// the query fails.
pub(crate) async fn groups_by_user_tx(
    conn: &mut SqliteConnection,
    org: OrgId,
) -> Result<HashMap<Uuid, Vec<Uuid>>, MetaError> {
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT m.scim_user_id, m.group_id FROM scim_group_members m \
         JOIN scim_groups g ON g.id = m.group_id WHERE g.org_id = ? \
         ORDER BY m.scim_user_id, m.group_id",
    )
    .bind(org.to_string())
    .fetch_all(&mut *conn)
    .await?;
    let mut by_user: HashMap<Uuid, Vec<Uuid>> = HashMap::new();
    for (user, group) in rows {
        let user = parse_id::<Uuid>(&user, "scim_group_members", "scim_user_id")?;
        let group = parse_id::<Uuid>(&group, "scim_group_members", "group_id")?;
        by_user.entry(user).or_default().push(group);
    }
    Ok(by_user)
}

#[derive(Debug, sqlx::FromRow)]
struct OrgScimRecord {
    enabled: i64,
    default_role: String,
}

#[derive(Debug, sqlx::FromRow)]
struct TokenRecord {
    id: String,
    org_id: String,
    name: String,
    prefix: String,
    created_by: Option<String>,
    created_at: i64,
    last_used_at: Option<i64>,
    revoked_at: Option<i64>,
}

#[derive(Debug, sqlx::FromRow)]
struct HashRecord {
    token_hash: String,
}

/// Every column of a SCIM user, in the order [`UserRecord`] reads them.
const USER_COLUMNS: &str = "id, org_id, user_id, user_name, external_id, given_name, \
     family_name, display_name, department, employee_number, active, created_at, updated_at";

#[derive(sqlx::FromRow)]
struct UserRecord {
    id: String,
    user_id: String,
    user_name: String,
    external_id: Option<String>,
    given_name: Option<String>,
    family_name: Option<String>,
    display_name: Option<String>,
    department: Option<String>,
    employee_number: Option<String>,
    active: i64,
    created_at: i64,
    updated_at: i64,
}

fn org_scim_from(record: Option<OrgScimRecord>) -> Result<OrgScim, MetaError> {
    let Some(record) = record else {
        return Ok(OrgScim {
            enabled: false,
            default_role: OrgRole::Viewer,
        });
    };
    let default_role = match parse_org_role(&record.default_role) {
        Some(role @ (OrgRole::Viewer | OrgRole::Member)) => role,
        _ => {
            return Err(MetaError::Corrupt {
                table: "org_scim",
                column: "default_role",
            });
        }
    };
    Ok(OrgScim {
        enabled: record.enabled != 0,
        default_role,
    })
}

fn token_row(record: TokenRecord) -> Result<ScimTokenRow, MetaError> {
    let created_by = record
        .created_by
        .as_deref()
        .map(|id| parse_id::<Uuid>(id, "scim_tokens", "created_by"))
        .transpose()?;
    Ok(ScimTokenRow {
        id: parse_id(&record.id, "scim_tokens", "id")?,
        org_id: parse_id(&record.org_id, "scim_tokens", "org_id")?,
        name: record.name,
        prefix: record.prefix,
        created_by,
        created_at: record.created_at,
        last_used_at: record.last_used_at,
        revoked_at: record.revoked_at,
    })
}

fn user_row(record: UserRecord) -> Result<ScimUserRow, MetaError> {
    Ok(ScimUserRow {
        id: parse_id(&record.id, "scim_users", "id")?,
        user_id: parse_id(&record.user_id, "scim_users", "user_id")?,
        user_name: record.user_name,
        external_id: record.external_id,
        given_name: record.given_name,
        family_name: record.family_name,
        display_name: record.display_name,
        department: record.department,
        employee_number: record.employee_number,
        active: record.active != 0,
        created_at: record.created_at,
        updated_at: record.updated_at,
    })
}

/// The SQL condition a filter adds, and its bound value. The value is never in the SQL text.
fn filter_clause(filter: Option<&UserFilter>) -> (&'static str, Option<&str>) {
    match filter {
        None => ("", None),
        Some(UserFilter::UserName(value) | UserFilter::Email(value)) => {
            (" AND user_name = ?", Some(value.as_str()))
        }
        Some(UserFilter::ExternalId(value)) => (" AND external_id = ?", Some(value.as_str())),
    }
}

/// Every column of a SCIM group, in the order [`GroupRecord`] reads them.
const GROUP_COLUMNS: &str = "id, display_name, external_id, created_at, updated_at";

#[derive(sqlx::FromRow)]
struct GroupRecord {
    id: String,
    display_name: String,
    external_id: Option<String>,
    created_at: i64,
    updated_at: i64,
}

fn group_row(record: GroupRecord) -> Result<ScimGroupRow, MetaError> {
    Ok(ScimGroupRow {
        id: parse_id(&record.id, "scim_groups", "id")?,
        display_name: record.display_name,
        external_id: record.external_id,
        created_at: record.created_at,
        updated_at: record.updated_at,
    })
}

/// The SQL condition a group filter adds, and its bound value. The value is never in the SQL
/// text.
fn group_filter_clause(filter: Option<&GroupFilter>) -> (&'static str, Option<&str>) {
    match filter {
        None => ("", None),
        Some(GroupFilter::DisplayName(value)) => (" AND display_name = ?", Some(value.as_str())),
        Some(GroupFilter::ExternalId(value)) => (" AND external_id = ?", Some(value.as_str())),
    }
}

/// A stored org role of a mapping. Only `admin`, `member` and `viewer` are mappable.
fn mapped_org_role(value: &str) -> Result<OrgRole, MetaError> {
    match parse_org_role(value) {
        Some(role @ (OrgRole::Admin | OrgRole::Member | OrgRole::Viewer)) => Ok(role),
        _ => Err(MetaError::Corrupt {
            table: "scim_group_mappings",
            column: "org_role",
        }),
    }
}

/// [`mappings`] on an open connection.
async fn load_mappings(
    conn: &mut SqliteConnection,
    org: OrgId,
) -> Result<HashMap<Uuid, GroupMapping>, MetaError> {
    let org_rows: Vec<(String, Option<String>)> = sqlx::query_as(
        "SELECT m.group_id, m.org_role FROM scim_group_mappings m \
         JOIN scim_groups g ON g.id = m.group_id WHERE g.org_id = ?",
    )
    .bind(org.to_string())
    .fetch_all(&mut *conn)
    .await?;
    let project_rows: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT r.group_id, r.project_id, r.role FROM scim_group_project_roles r \
         JOIN scim_groups g ON g.id = r.group_id WHERE g.org_id = ? \
         ORDER BY r.group_id, r.project_id",
    )
    .bind(org.to_string())
    .fetch_all(&mut *conn)
    .await?;

    let empty = || GroupMapping {
        org_role: None,
        projects: Vec::new(),
    };
    let mut mappings: HashMap<Uuid, GroupMapping> = HashMap::new();
    for (group, org_role) in org_rows {
        let group = parse_id::<Uuid>(&group, "scim_group_mappings", "group_id")?;
        let org_role = org_role.as_deref().map(mapped_org_role).transpose()?;
        mappings.entry(group).or_insert_with(empty).org_role = org_role;
    }
    for (group, project, role) in project_rows {
        let group = parse_id::<Uuid>(&group, "scim_group_project_roles", "group_id")?;
        let project = parse_id::<ProjectId>(&project, "scim_group_project_roles", "project_id")?;
        let role = parse_project_override(&role).ok_or(MetaError::Corrupt {
            table: "scim_group_project_roles",
            column: "role",
        })?;
        mappings
            .entry(group)
            .or_insert_with(empty)
            .projects
            .push((project, role));
    }
    // A mapping left with neither an org role nor a project (after a project was deleted)
    // reads as no mapping.
    mappings.retain(|_, mapping| mapping.org_role.is_some() || !mapping.projects.is_empty());
    Ok(mappings)
}

/// Parses a stored id. The error names the column, never the value.
fn parse_id<T: FromStr>(
    value: &str,
    table: &'static str,
    column: &'static str,
) -> Result<T, MetaError> {
    value
        .parse()
        .map_err(|_| MetaError::Corrupt { table, column })
}

#[cfg(test)]
mod tests {
    use cetra_core::ids::OrgId;
    use uuid::Uuid;

    use super::*;
    use crate::meta::MetaDb;
    use crate::meta::orgs::{delete_org, insert_org, mark_org_deleting};
    use crate::meta::projects::insert_project;
    use crate::meta::test_db;
    use crate::meta::users::{NewUser, insert_user as insert_account};

    async fn org_named(db: &MetaDb, slug: &'static str) -> OrgId {
        db.write(move |conn| {
            Box::pin(async move {
                let id = OrgId::new_v7();
                insert_org(conn, id, slug, slug, 1).await?;
                Ok(id)
            })
        })
        .await
        .unwrap()
    }

    async fn account_named(db: &MetaDb, email: &'static str) -> Uuid {
        db.write(move |conn| {
            Box::pin(async move {
                let user = insert_account(
                    conn,
                    NewUser {
                        email: email.to_owned(),
                        name: None,
                        password_hash: None,
                        email_verified_at: None,
                        email_verified_via: None,
                        is_instance_admin: false,
                    },
                    1,
                )
                .await?;
                Ok(user.id)
            })
        })
        .await
        .unwrap()
    }

    fn fields(user_name: &str, external_id: Option<&str>) -> ScimUserFields {
        ScimUserFields {
            user_name: user_name.to_owned(),
            external_id: external_id.map(str::to_owned),
            given_name: None,
            family_name: None,
            display_name: None,
            department: None,
            employee_number: None,
        }
    }

    /// A project of `org` with this slug.
    async fn project_named(db: &MetaDb, org: OrgId, slug: &'static str) -> ProjectId {
        db.write(move |conn| {
            Box::pin(async move {
                let project = insert_project(conn, org, slug, slug, None, 1).await?;
                Ok(project.id)
            })
        })
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn group_names_are_unique_per_org() {
        let (_dir, db) = test_db().await;
        let acme = org_named(&db, "acme").await;
        let other = org_named(&db, "other").await;
        db.write(move |conn| {
            Box::pin(async move {
                insert_group(conn, acme, "Admins", Some("ext-1"), 1).await?;
                let same_name = insert_group(conn, acme, "Admins", None, 1).await;
                assert!(matches!(same_name, Err(MetaError::Conflict("uniqueness"))));
                let same_external = insert_group(conn, acme, "Viewers", Some("ext-1"), 1).await;
                assert!(matches!(
                    same_external,
                    Err(MetaError::Conflict("uniqueness"))
                ));
                insert_group(conn, other, "Admins", Some("ext-1"), 1).await?;
                Ok(())
            })
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn members_of_another_org_never_join() {
        let (_dir, db) = test_db().await;
        let acme = org_named(&db, "acme").await;
        let other = org_named(&db, "other").await;
        let ada = account_named(&db, "ada@acme.test").await;
        let bob = account_named(&db, "bob@other.test").await;
        let (group, ada_scim, bob_scim) = db
            .write(move |conn| {
                Box::pin(async move {
                    let group = insert_group(conn, acme, "Engineering", None, 1).await?.id;
                    let ada_scim = insert_user(conn, acme, ada, &fields("ada@acme.test", None), 1)
                        .await?
                        .id;
                    let bob_scim =
                        insert_user(conn, other, bob, &fields("bob@other.test", None), 1)
                            .await?
                            .id;
                    let added =
                        add_group_members_tx(conn, acme, group, &[ada_scim, bob_scim]).await?;
                    assert_eq!(added, vec![ada_scim]);
                    Ok((group, ada_scim, bob_scim))
                })
            })
            .await
            .unwrap();
        let members = group_member_ids(db.reader(), group).await.unwrap();
        assert_eq!(members, vec![ada_scim]);
        assert!(!members.contains(&bob_scim));
    }

    #[tokio::test]
    async fn put_mapping_replaces_project_rows() {
        let (_dir, db) = test_db().await;
        let acme = org_named(&db, "acme").await;
        let first = project_named(&db, acme, "first").await;
        let second = project_named(&db, acme, "second").await;
        let group = db
            .write(move |conn| {
                Box::pin(
                    async move { Ok(insert_group(conn, acme, "Engineering", None, 1).await?.id) },
                )
            })
            .await
            .unwrap();
        db.write(move |conn| {
            Box::pin(async move {
                let first_mapping = GroupMapping {
                    org_role: Some(OrgRole::Member),
                    projects: vec![(first, ProjectOverride::Admin)],
                };
                put_mapping(conn, group, &first_mapping, 2).await?;

                let duplicate = GroupMapping {
                    org_role: Some(OrgRole::Admin),
                    projects: vec![
                        (second, ProjectOverride::Viewer),
                        (second, ProjectOverride::Member),
                    ],
                };
                let refused = put_mapping(conn, group, &duplicate, 3).await;
                assert!(matches!(
                    refused,
                    Err(MetaError::Conflict("duplicate project"))
                ));

                let owner = GroupMapping {
                    org_role: Some(OrgRole::Owner),
                    projects: Vec::new(),
                };
                let refused = put_mapping(conn, group, &owner, 3).await;
                assert!(matches!(
                    refused,
                    Err(MetaError::Conflict("owner is not mappable"))
                ));

                let replacement = GroupMapping {
                    org_role: Some(OrgRole::Viewer),
                    projects: vec![(second, ProjectOverride::None)],
                };
                put_mapping(conn, group, &replacement, 4).await?;
                Ok(())
            })
        })
        .await
        .unwrap();

        let loaded = mappings(db.reader(), acme).await.unwrap();
        assert_eq!(
            loaded[&group],
            GroupMapping {
                org_role: Some(OrgRole::Viewer),
                projects: vec![(second, ProjectOverride::None)],
            }
        );

        db.write(move |conn| {
            Box::pin(async move {
                let nothing = GroupMapping {
                    org_role: None,
                    projects: Vec::new(),
                };
                put_mapping(conn, group, &nothing, 5).await?;
                Ok(())
            })
        })
        .await
        .unwrap();
        assert!(
            !mappings(db.reader(), acme)
                .await
                .unwrap()
                .contains_key(&group)
        );
    }

    #[tokio::test]
    async fn deleting_a_group_cascades_members_and_mappings() {
        let (_dir, db) = test_db().await;
        let acme = org_named(&db, "acme").await;
        let project = project_named(&db, acme, "web").await;
        let ada = account_named(&db, "ada@acme.test").await;
        let group = db
            .write(move |conn| {
                Box::pin(async move {
                    let group = insert_group(conn, acme, "Engineering", None, 1).await?.id;
                    let ada_scim = insert_user(conn, acme, ada, &fields("ada@acme.test", None), 1)
                        .await?
                        .id;
                    add_group_members_tx(conn, acme, group, &[ada_scim]).await?;
                    let mapping = GroupMapping {
                        org_role: Some(OrgRole::Member),
                        projects: vec![(project, ProjectOverride::Admin)],
                    };
                    put_mapping(conn, group, &mapping, 1).await?;
                    Ok(group)
                })
            })
            .await
            .unwrap();

        db.write(move |conn| {
            Box::pin(async move {
                assert!(delete_group(conn, acme, group).await?);
                assert!(!delete_group(conn, acme, group).await?);
                Ok(())
            })
        })
        .await
        .unwrap();

        for table in [
            "scim_group_members",
            "scim_group_mappings",
            "scim_group_project_roles",
        ] {
            let count = sqlx::query_scalar::<_, i64>(sqlx::AssertSqlSafe(format!(
                "SELECT COUNT(*) FROM {table}"
            )))
            .fetch_one(db.reader())
            .await
            .unwrap();
            assert_eq!(count, 0, "{table} keeps the rows of a deleted group");
        }
        let users = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM scim_users")
            .fetch_one(db.reader())
            .await
            .unwrap();
        assert_eq!(users, 1, "deleting a group keeps its members");
    }

    #[tokio::test]
    async fn deleting_a_project_drops_its_mapping_rows() {
        let (_dir, db) = test_db().await;
        let acme = org_named(&db, "acme").await;
        let project = project_named(&db, acme, "web").await;
        let group = db
            .write(move |conn| {
                Box::pin(async move {
                    let group = insert_group(conn, acme, "Engineering", None, 1).await?.id;
                    let mapping = GroupMapping {
                        org_role: Some(OrgRole::Member),
                        projects: vec![(project, ProjectOverride::Admin)],
                    };
                    put_mapping(conn, group, &mapping, 1).await?;
                    Ok(group)
                })
            })
            .await
            .unwrap();

        db.write(move |conn| {
            Box::pin(async move {
                sqlx::query("DELETE FROM projects WHERE id = ?")
                    .bind(project.to_string())
                    .execute(&mut *conn)
                    .await?;
                Ok(())
            })
        })
        .await
        .unwrap();

        let loaded = mappings(db.reader(), acme).await.unwrap();
        assert_eq!(
            loaded[&group],
            GroupMapping {
                org_role: Some(OrgRole::Member),
                projects: Vec::new(),
            }
        );
        let rows = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM scim_group_project_roles")
            .fetch_one(db.reader())
            .await
            .unwrap();
        assert_eq!(rows, 0);
    }

    #[tokio::test]
    async fn groups_of_user_lists_memberships() {
        let (_dir, db) = test_db().await;
        let acme = org_named(&db, "acme").await;
        let ada = account_named(&db, "ada@acme.test").await;
        let bob = account_named(&db, "bob@acme.test").await;
        db.write(move |conn| {
            Box::pin(async move {
                let engineering = insert_group(conn, acme, "Engineering", None, 1).await?.id;
                let admins = insert_group(conn, acme, "Admins", None, 1).await?.id;
                let viewers = insert_group(conn, acme, "Viewers", None, 1).await?.id;
                let ada_scim = insert_user(conn, acme, ada, &fields("ada@acme.test", None), 1)
                    .await?
                    .id;
                let bob_scim = insert_user(conn, acme, bob, &fields("bob@acme.test", None), 1)
                    .await?
                    .id;
                add_group_members_tx(conn, acme, engineering, &[ada_scim]).await?;
                add_group_members_tx(conn, acme, admins, &[ada_scim]).await?;
                add_group_members_tx(conn, acme, viewers, &[bob_scim]).await?;

                let mut groups = groups_of_user_tx(conn, ada_scim).await?;
                groups.sort();
                let mut expected = vec![engineering, admins];
                expected.sort();
                assert_eq!(groups, expected);

                let by_user = groups_by_user_tx(conn, acme).await?;
                assert_eq!(by_user[&ada_scim].len(), 2);
                assert_eq!(by_user[&bob_scim], vec![viewers]);
                Ok(())
            })
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn org_scim_defaults_when_missing() {
        let (_dir, db) = test_db().await;
        let org = org_named(&db, "acme").await;
        let scim = org_scim(db.reader(), org).await.unwrap();
        assert!(!scim.enabled);
        assert_eq!(scim.default_role, OrgRole::Viewer);

        db.write(move |conn| {
            Box::pin(async move { put_org_scim(conn, org, true, OrgRole::Member, 5).await })
        })
        .await
        .unwrap();
        let scim = org_scim(db.reader(), org).await.unwrap();
        assert!(scim.enabled);
        assert_eq!(scim.default_role, OrgRole::Member);
    }

    #[tokio::test]
    async fn a_third_live_token_is_refused() {
        let (_dir, db) = test_db().await;
        let org = org_named(&db, "acme").await;
        let owner = account_named(&db, "owner@example.com").await;
        let first = db
            .write(move |conn| {
                Box::pin(async move {
                    let first =
                        insert_token(conn, org, "okta", "scim-cetra-aaaaaa", "hash-1", owner, 1)
                            .await?;
                    insert_token(conn, org, "entra", "scim-cetra-bbbbbb", "hash-2", owner, 2)
                        .await?;
                    Ok(first.id)
                })
            })
            .await
            .unwrap();

        let refused = db
            .write(move |conn| {
                Box::pin(async move {
                    insert_token(conn, org, "third", "scim-cetra-cccccc", "hash-3", owner, 3)
                        .await
                        .map(|_| ())
                })
            })
            .await;
        assert!(matches!(
            refused,
            Err(MetaError::Conflict("at most two live SCIM tokens"))
        ));

        db.write(move |conn| {
            Box::pin(async move {
                assert!(revoke_token(conn, org, first, 4).await?.is_some());
                Ok(())
            })
        })
        .await
        .unwrap();
        db.write(move |conn| {
            Box::pin(async move {
                insert_token(conn, org, "third", "scim-cetra-cccccc", "hash-3", owner, 5).await?;
                Ok(())
            })
        })
        .await
        .unwrap();
        assert_eq!(list_tokens(db.reader(), org).await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn revoking_returns_the_hash_once() {
        let (_dir, db) = test_db().await;
        let org = org_named(&db, "acme").await;
        let owner = account_named(&db, "owner@example.com").await;
        let id = db
            .write(move |conn| {
                Box::pin(async move {
                    let row =
                        insert_token(conn, org, "okta", "scim-cetra-aaaaaa", "hash-1", owner, 1)
                            .await?;
                    Ok(row.id)
                })
            })
            .await
            .unwrap();

        let first = db
            .write(move |conn| Box::pin(async move { revoke_token(conn, org, id, 2).await }))
            .await
            .unwrap();
        assert_eq!(first.as_deref(), Some("hash-1"));
        let second = db
            .write(move |conn| Box::pin(async move { revoke_token(conn, org, id, 3).await }))
            .await
            .unwrap();
        assert_eq!(second, None);
        assert!(list_tokens(db.reader(), org).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn token_lookup_needs_an_active_org() {
        let (_dir, db) = test_db().await;
        let org = org_named(&db, "acme").await;
        let owner = account_named(&db, "owner@example.com").await;
        db.write(move |conn| {
            Box::pin(async move {
                insert_token(conn, org, "okta", "scim-cetra-aaaaaa", "hash-1", owner, 1).await?;
                Ok(())
            })
        })
        .await
        .unwrap();

        let (row, active) = token_by_hash(db.reader(), "hash-1").await.unwrap().unwrap();
        assert_eq!(row.org_id, org);
        assert!(active);
        assert!(
            token_by_hash(db.reader(), "unknown")
                .await
                .unwrap()
                .is_none()
        );

        let marked = db
            .write(move |conn| Box::pin(async move { mark_org_deleting(conn, org, 9).await }))
            .await
            .unwrap();
        assert!(marked);
        let (_, active) = token_by_hash(db.reader(), "hash-1").await.unwrap().unwrap();
        assert!(!active);
    }

    #[tokio::test]
    async fn user_name_and_external_id_are_unique_per_org() {
        let (_dir, db) = test_db().await;
        let acme = org_named(&db, "acme").await;
        let globex = org_named(&db, "globex").await;
        let ada = account_named(&db, "ada@example.com").await;
        let bob = account_named(&db, "bob@example.com").await;

        db.write(move |conn| {
            Box::pin(async move {
                insert_user(
                    conn,
                    acme,
                    ada,
                    &fields("ada@example.com", Some("ext-1")),
                    1,
                )
                .await?;
                Ok(())
            })
        })
        .await
        .unwrap();

        let same_name = db
            .write(move |conn| {
                Box::pin(async move {
                    insert_user(conn, acme, bob, &fields("ada@example.com", None), 2)
                        .await
                        .map(|_| ())
                })
            })
            .await;
        assert!(matches!(same_name, Err(MetaError::Conflict("uniqueness"))));

        let same_external = db
            .write(move |conn| {
                Box::pin(async move {
                    insert_user(
                        conn,
                        acme,
                        bob,
                        &fields("bob@example.com", Some("ext-1")),
                        3,
                    )
                    .await
                    .map(|_| ())
                })
            })
            .await;
        assert!(matches!(
            same_external,
            Err(MetaError::Conflict("uniqueness"))
        ));

        // Another org may use the same names.
        db.write(move |conn| {
            Box::pin(async move {
                insert_user(
                    conn,
                    globex,
                    bob,
                    &fields("ada@example.com", Some("ext-1")),
                    4,
                )
                .await?;
                Ok(())
            })
        })
        .await
        .unwrap();
        let found = db
            .write(move |conn| {
                Box::pin(async move {
                    Ok(user_by_user_name_tx(conn, globex, "ada@example.com")
                        .await?
                        .is_some())
                })
            })
            .await
            .unwrap();
        assert!(found);
    }

    #[tokio::test]
    async fn list_users_pages_and_filters() {
        let (_dir, db) = test_db().await;
        let org = org_named(&db, "acme").await;
        let a = account_named(&db, "a@example.com").await;
        let b = account_named(&db, "b@example.com").await;
        let c = account_named(&db, "c@example.com").await;
        db.write(move |conn| {
            Box::pin(async move {
                insert_user(conn, org, a, &fields("a@example.com", Some("e1")), 1).await?;
                insert_user(conn, org, b, &fields("b@example.com", Some("e2")), 2).await?;
                insert_user(conn, org, c, &fields("c@example.com", None), 3).await?;
                Ok(())
            })
        })
        .await
        .unwrap();

        let (total, rows) = list_users(db.reader(), org, None, 1, 2).await.unwrap();
        assert_eq!(total, 3);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].user_name, "a@example.com");
        assert_eq!(rows[1].user_name, "b@example.com");

        let (total, rows) = list_users(db.reader(), org, None, 3, 2).await.unwrap();
        assert_eq!(total, 3);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].user_name, "c@example.com");

        let by_name = UserFilter::UserName("b@example.com".to_owned());
        let (total, rows) = list_users(db.reader(), org, Some(&by_name), 1, 10)
            .await
            .unwrap();
        assert_eq!(total, 1);
        assert_eq!(rows[0].user_name, "b@example.com");

        let by_email = UserFilter::Email("b@example.com".to_owned());
        let (total, _) = list_users(db.reader(), org, Some(&by_email), 1, 10)
            .await
            .unwrap();
        assert_eq!(total, 1);

        let by_external = UserFilter::ExternalId("e1".to_owned());
        let (total, rows) = list_users(db.reader(), org, Some(&by_external), 1, 10)
            .await
            .unwrap();
        assert_eq!(total, 1);
        assert_eq!(rows[0].user_id, a);

        let missing = UserFilter::Email("zz@example.com".to_owned());
        let (total, rows) = list_users(db.reader(), org, Some(&missing), 1, 10)
            .await
            .unwrap();
        assert_eq!(total, 0);
        assert!(rows.is_empty());
    }

    #[tokio::test]
    async fn is_managed_only_for_active_rows() {
        let (_dir, db) = test_db().await;
        let org = org_named(&db, "acme").await;
        let ada = account_named(&db, "ada@example.com").await;
        let row = db
            .write(move |conn| {
                Box::pin(async move {
                    insert_user(conn, org, ada, &fields("ada@example.com", None), 1).await
                })
            })
            .await
            .unwrap();

        let managed = db
            .write(move |conn| Box::pin(async move { is_managed_tx(conn, org, ada).await }))
            .await
            .unwrap();
        assert!(managed);
        let ids = managed_user_ids(db.reader(), org).await.unwrap();
        assert!(ids.contains(&ada));

        let id = row.id;
        db.write(move |conn| {
            Box::pin(async move { set_user_active(conn, org, id, false, 2).await })
        })
        .await
        .unwrap();
        let managed = db
            .write(move |conn| Box::pin(async move { is_managed_tx(conn, org, ada).await }))
            .await
            .unwrap();
        assert!(!managed);
        assert!(managed_user_ids(db.reader(), org).await.unwrap().is_empty());

        let linked = db
            .write(move |conn| {
                Box::pin(async move { user_by_user_name_tx(conn, org, "ada@example.com").await })
            })
            .await
            .unwrap();
        assert!(linked.is_some_and(|linked| !linked.active));
    }

    #[tokio::test]
    async fn org_delete_cascades() {
        let (_dir, db) = test_db().await;
        let org = org_named(&db, "acme").await;
        let owner = account_named(&db, "owner@example.com").await;
        let ada = account_named(&db, "ada@example.com").await;
        db.write(move |conn| {
            Box::pin(async move {
                put_org_scim(conn, org, true, OrgRole::Viewer, 1).await?;
                insert_token(conn, org, "okta", "scim-cetra-aaaaaa", "hash-1", owner, 1).await?;
                insert_user(conn, org, ada, &fields("ada@example.com", None), 1).await?;
                Ok(())
            })
        })
        .await
        .unwrap();

        db.write(move |conn| Box::pin(async move { delete_org(conn, org).await }))
            .await
            .unwrap();

        for table in ["org_scim", "scim_tokens", "scim_users"] {
            let count: i64 =
                sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT COUNT(*) FROM {table}")))
                    .fetch_one(db.reader())
                    .await
                    .unwrap();
            assert_eq!(count, 0, "{table} kept rows of a deleted org");
        }
    }
}
