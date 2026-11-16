// SPDX-License-Identifier: Elastic-2.0
//! Storage of org SSO providers and domains (D-P-52, D-P-54, D-P-59).

use std::fmt;

use cetra_core::ids::OrgId;
use sqlx::{SqliteConnection, SqlitePool};
use uuid::Uuid;

use crate::api::org_sso::JitRole;
use crate::crypto::Aad;
use crate::meta::password_resets::delete_user_resets;
use crate::meta::personal_tokens::revoke_user_tokens;
use crate::meta::{MetaError, unique_or};

/// One `org_sso_providers` row. `Debug` omits the sealed secret.
pub(crate) struct ProviderRow {
    pub(crate) id: Uuid,
    pub(crate) org_id: OrgId,
    pub(crate) display_name: String,
    pub(crate) issuer: String,
    pub(crate) client_id: String,
    pub(crate) client_secret_enc: Vec<u8>,
    pub(crate) enabled: bool,
    pub(crate) enforced: bool,
    pub(crate) jit_role: JitRole,
    pub(crate) last_login_at: Option<i64>,
    pub(crate) created_at: i64,
    pub(crate) updated_at: i64,
}

impl fmt::Debug for ProviderRow {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderRow")
            .field("id", &self.id)
            .field("org_id", &self.org_id)
            .field("display_name", &self.display_name)
            .field("issuer", &self.issuer)
            .field("client_id", &self.client_id)
            .field("enabled", &self.enabled)
            .field("enforced", &self.enforced)
            .field("jit_role", &self.jit_role)
            .field("last_login_at", &self.last_login_at)
            .field("created_at", &self.created_at)
            .field("updated_at", &self.updated_at)
            .finish_non_exhaustive()
    }
}

/// One `org_sso_domains` row.
#[derive(Debug, Clone)]
pub(crate) struct DomainRow {
    pub(crate) id: Uuid,
    pub(crate) org_id: OrgId,
    pub(crate) domain: String,
    pub(crate) token: String,
    pub(crate) verified_at: Option<i64>,
    pub(crate) last_checked_at: Option<i64>,
    pub(crate) created_at: i64,
}

/// What `put_provider` writes. `client_secret_enc: None` keeps the stored one.
pub(crate) struct ProviderWrite {
    pub(crate) display_name: String,
    pub(crate) issuer: String,
    pub(crate) client_id: String,
    pub(crate) client_secret_enc: Option<Vec<u8>>,
    pub(crate) jit_role: JitRole,
}

/// `AAD` of `client_secret_enc` for provider row `id` (table `org_sso_providers`).
pub(crate) fn secret_aad(id: &Uuid) -> Aad<'_> {
    Aad {
        table: "org_sso_providers",
        column: "client_secret_enc",
        row_id: id,
    }
}

const PROVIDER_BY_ORG: &str = "SELECT id, org_id, display_name, issuer, client_id, \
     client_secret_enc, enabled, enforced, jit_role, last_login_at, created_at, updated_at \
     FROM org_sso_providers WHERE org_id = ?";

const DOMAINS_BY_ORG: &str = "SELECT id, org_id, domain, token, verified_at, last_checked_at, \
     created_at FROM org_sso_domains WHERE org_id = ? ORDER BY domain";

const DOMAIN_BY_ID: &str = "SELECT id, org_id, domain, token, verified_at, last_checked_at, \
     created_at FROM org_sso_domains WHERE org_id = ? AND id = ?";

const VERIFIED_DOMAIN: &str = "SELECT id, org_id, domain, token, verified_at, last_checked_at, \
     created_at FROM org_sso_domains WHERE domain = ? AND verified_at IS NOT NULL";

#[derive(Debug, sqlx::FromRow)]
struct ProviderRecord {
    id: String,
    org_id: String,
    display_name: String,
    issuer: String,
    client_id: String,
    client_secret_enc: Vec<u8>,
    enabled: i64,
    enforced: i64,
    jit_role: String,
    last_login_at: Option<i64>,
    created_at: i64,
    updated_at: i64,
}

#[derive(Debug, sqlx::FromRow)]
struct DomainRecord {
    id: String,
    org_id: String,
    domain: String,
    token: String,
    verified_at: Option<i64>,
    last_checked_at: Option<i64>,
    created_at: i64,
}

fn corrupt(table: &'static str, column: &'static str) -> MetaError {
    MetaError::Corrupt { table, column }
}

fn parse_uuid(value: &str, table: &'static str, column: &'static str) -> Result<Uuid, MetaError> {
    Uuid::parse_str(value).map_err(|_| corrupt(table, column))
}

fn parse_flag(value: i64, column: &'static str) -> Result<bool, MetaError> {
    match value {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(corrupt("org_sso_providers", column)),
    }
}

fn provider_row(record: ProviderRecord) -> Result<ProviderRow, MetaError> {
    Ok(ProviderRow {
        id: parse_uuid(&record.id, "org_sso_providers", "id")?,
        org_id: OrgId::from_uuid(parse_uuid(&record.org_id, "org_sso_providers", "org_id")?),
        display_name: record.display_name,
        issuer: record.issuer,
        client_id: record.client_id,
        client_secret_enc: record.client_secret_enc,
        enabled: parse_flag(record.enabled, "enabled")?,
        enforced: parse_flag(record.enforced, "enforced")?,
        jit_role: JitRole::from_str_opt(&record.jit_role)
            .ok_or_else(|| corrupt("org_sso_providers", "jit_role"))?,
        last_login_at: record.last_login_at,
        created_at: record.created_at,
        updated_at: record.updated_at,
    })
}

fn domain_row(record: DomainRecord) -> Result<DomainRow, MetaError> {
    Ok(DomainRow {
        id: parse_uuid(&record.id, "org_sso_domains", "id")?,
        org_id: OrgId::from_uuid(parse_uuid(&record.org_id, "org_sso_domains", "org_id")?),
        domain: record.domain,
        token: record.token,
        verified_at: record.verified_at,
        last_checked_at: record.last_checked_at,
        created_at: record.created_at,
    })
}

/// The org's provider, if it has one.
///
/// # Errors
///
/// Returns [`MetaError`] when the query fails or a stored value is corrupt.
pub(crate) async fn provider(
    pool: &SqlitePool,
    org: OrgId,
) -> Result<Option<ProviderRow>, MetaError> {
    let record: Option<ProviderRecord> = sqlx::query_as(PROVIDER_BY_ORG)
        .bind(org.to_string())
        .fetch_optional(pool)
        .await?;
    record.map(provider_row).transpose()
}

/// The org's provider in the write transaction.
///
/// # Errors
///
/// Returns [`MetaError`] when the query fails or a stored value is corrupt.
pub(crate) async fn provider_tx(
    conn: &mut SqliteConnection,
    org: OrgId,
) -> Result<Option<ProviderRow>, MetaError> {
    let record: Option<ProviderRecord> = sqlx::query_as(PROVIDER_BY_ORG)
        .bind(org.to_string())
        .fetch_optional(&mut *conn)
        .await?;
    record.map(provider_row).transpose()
}

/// Providers of active orgs, ordered by org id.
///
/// # Errors
///
/// Returns [`MetaError`] when the query fails or a stored value is corrupt.
pub(crate) async fn active_providers(pool: &SqlitePool) -> Result<Vec<ProviderRow>, MetaError> {
    let records: Vec<ProviderRecord> = sqlx::query_as(
        "SELECT p.id, p.org_id, p.display_name, p.issuer, p.client_id, p.client_secret_enc, \
                p.enabled, p.enforced, p.jit_role, p.last_login_at, p.created_at, p.updated_at \
         FROM org_sso_providers p JOIN orgs o ON o.id = p.org_id \
         WHERE o.state = 'active' ORDER BY p.org_id",
    )
    .fetch_all(pool)
    .await?;
    records.into_iter().map(provider_row).collect()
}

/// Inserts (with `id`, enabled and enforced off) or updates the org's provider. On insert
/// `client_secret_enc` must be `Some` (else `MetaError::Corrupt`, a caller bug). An update keeps
/// the stored row id, so a new sealed secret must be sealed with [`secret_aad`] of the id that
/// [`provider_tx`] returns.
///
/// # Errors
///
/// Returns [`MetaError`] when the write fails or an insert has no secret.
pub(crate) async fn put_provider(
    conn: &mut SqliteConnection,
    org: OrgId,
    id: Uuid,
    write: ProviderWrite,
    now: i64,
) -> Result<(), MetaError> {
    if provider_tx(&mut *conn, org).await?.is_some() {
        sqlx::query(
            "UPDATE org_sso_providers SET display_name = ?, issuer = ?, client_id = ?, \
             client_secret_enc = COALESCE(?, client_secret_enc), jit_role = ?, updated_at = ?, \
             last_login_at = CASE WHEN issuer != ? OR client_id != ? THEN NULL \
                                  ELSE last_login_at END \
             WHERE org_id = ?",
        )
        .bind(&write.display_name)
        .bind(&write.issuer)
        .bind(&write.client_id)
        .bind(write.client_secret_enc)
        .bind(write.jit_role.as_str())
        .bind(now)
        .bind(&write.issuer)
        .bind(&write.client_id)
        .bind(org.to_string())
        .execute(&mut *conn)
        .await?;
        return Ok(());
    }
    let Some(secret) = write.client_secret_enc else {
        return Err(corrupt("org_sso_providers", "client_secret_enc"));
    };
    sqlx::query(
        "INSERT INTO org_sso_providers \
         (id, org_id, display_name, issuer, client_id, client_secret_enc, jit_role, \
          created_at, updated_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(id.to_string())
    .bind(org.to_string())
    .bind(&write.display_name)
    .bind(&write.issuer)
    .bind(&write.client_id)
    .bind(secret)
    .bind(write.jit_role.as_str())
    .bind(now)
    .bind(now)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Sets the two flags and `updated_at`. Returns whether the org has a provider.
///
/// # Errors
///
/// Returns [`MetaError::Db`] when the update fails.
pub(crate) async fn set_flags(
    conn: &mut SqliteConnection,
    org: OrgId,
    enabled: bool,
    enforced: bool,
    now: i64,
) -> Result<bool, MetaError> {
    let changed = sqlx::query(
        "UPDATE org_sso_providers SET enabled = ?, enforced = ?, updated_at = ? WHERE org_id = ?",
    )
    .bind(i64::from(enabled))
    .bind(i64::from(enforced))
    .bind(now)
    .bind(org.to_string())
    .execute(&mut *conn)
    .await?
    .rows_affected();
    Ok(changed == 1)
}

/// Records a successful sign-in through the org's provider.
///
/// # Errors
///
/// Returns [`MetaError::Db`] when the update fails.
pub(crate) async fn set_last_login(
    conn: &mut SqliteConnection,
    org: OrgId,
    now: i64,
) -> Result<(), MetaError> {
    sqlx::query("UPDATE org_sso_providers SET last_login_at = ? WHERE org_id = ?")
        .bind(now)
        .bind(org.to_string())
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// Deletes the provider row and the org's identities (`identities.provider = 'org:<org>'`).
/// Returns whether a provider row existed.
///
/// # Errors
///
/// Returns [`MetaError::Db`] when a delete fails.
pub(crate) async fn delete_provider(
    conn: &mut SqliteConnection,
    org: OrgId,
) -> Result<bool, MetaError> {
    delete_identities(&mut *conn, org).await?;
    let changed = sqlx::query("DELETE FROM org_sso_providers WHERE org_id = ?")
        .bind(org.to_string())
        .execute(&mut *conn)
        .await?
        .rows_affected();
    Ok(changed == 1)
}

/// Deletes the org's identities only (issuer change, D-P-52).
///
/// # Errors
///
/// Returns [`MetaError::Db`] when the delete fails.
pub(crate) async fn delete_identities(
    conn: &mut SqliteConnection,
    org: OrgId,
) -> Result<u64, MetaError> {
    let changed = sqlx::query("DELETE FROM identities WHERE provider = ?")
        .bind(identity_provider(org))
        .execute(&mut *conn)
        .await?
        .rows_affected();
    Ok(changed)
}

/// `org:<org>`, the `identities.provider` of the org's provider.
pub(crate) fn identity_provider(org: OrgId) -> String {
    format!("org:{org}")
}

/// The org's domains, ordered by domain.
///
/// # Errors
///
/// Returns [`MetaError`] when the query fails or a stored value is corrupt.
pub(crate) async fn domains(pool: &SqlitePool, org: OrgId) -> Result<Vec<DomainRow>, MetaError> {
    let records: Vec<DomainRecord> = sqlx::query_as(DOMAINS_BY_ORG)
        .bind(org.to_string())
        .fetch_all(pool)
        .await?;
    records.into_iter().map(domain_row).collect()
}

/// Verified domains of active orgs whose proof was last checked before `cutoff` (or never
/// since it was made), oldest check first, at most `limit`.
///
/// # Errors
///
/// Returns [`MetaError`] when the query fails or a stored value is corrupt.
pub(crate) async fn stale_verified_domains(
    pool: &SqlitePool,
    cutoff: i64,
    limit: u32,
) -> Result<Vec<DomainRow>, MetaError> {
    let records: Vec<DomainRecord> = sqlx::query_as(
        "SELECT d.id, d.org_id, d.domain, d.token, d.verified_at, d.last_checked_at, \
         d.created_at FROM org_sso_domains d JOIN orgs o ON o.id = d.org_id \
         WHERE d.verified_at IS NOT NULL AND o.state = 'active' \
           AND MAX(COALESCE(d.last_checked_at, 0), d.verified_at) < ? \
         ORDER BY MAX(COALESCE(d.last_checked_at, 0), d.verified_at) LIMIT ?",
    )
    .bind(cutoff)
    .bind(i64::from(limit))
    .fetch_all(pool)
    .await?;
    records.into_iter().map(domain_row).collect()
}

/// The org's domains in the write transaction, ordered by domain.
///
/// # Errors
///
/// Returns [`MetaError`] when the query fails or a stored value is corrupt.
pub(crate) async fn domains_tx(
    conn: &mut SqliteConnection,
    org: OrgId,
) -> Result<Vec<DomainRow>, MetaError> {
    let records: Vec<DomainRecord> = sqlx::query_as(DOMAINS_BY_ORG)
        .bind(org.to_string())
        .fetch_all(&mut *conn)
        .await?;
    records.into_iter().map(domain_row).collect()
}

/// The same row on the read pool (P8's verify reads before the DNS lookup).
///
/// # Errors
///
/// Returns [`MetaError`] when the query fails or a stored value is corrupt.
pub(crate) async fn domain(
    pool: &SqlitePool,
    org: OrgId,
    id: Uuid,
) -> Result<Option<DomainRow>, MetaError> {
    let record: Option<DomainRecord> = sqlx::query_as(DOMAIN_BY_ID)
        .bind(org.to_string())
        .bind(id.to_string())
        .fetch_optional(pool)
        .await?;
    record.map(domain_row).transpose()
}

/// The verified row of `domain` in any org (P8's takeover).
///
/// # Errors
///
/// Returns [`MetaError`] when the query fails or a stored value is corrupt.
pub(crate) async fn verified_row_tx(
    conn: &mut SqliteConnection,
    domain: &str,
) -> Result<Option<DomainRow>, MetaError> {
    let record: Option<DomainRecord> = sqlx::query_as(VERIFIED_DOMAIN)
        .bind(domain)
        .fetch_optional(&mut *conn)
        .await?;
    record.map(domain_row).transpose()
}

/// `verified_at = NULL` (the domain moved to another org, D-P-54).
///
/// # Errors
///
/// Returns [`MetaError::Db`] when the update fails.
pub(crate) async fn mark_unverified(
    conn: &mut SqliteConnection,
    id: Uuid,
) -> Result<(), MetaError> {
    sqlx::query("UPDATE org_sso_domains SET verified_at = NULL WHERE id = ?")
        .bind(id.to_string())
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// Inserts a pending domain. `MetaError::Conflict` when the org already lists it.
///
/// # Errors
///
/// Returns [`MetaError::Conflict`] for a duplicate domain, and [`MetaError::Db`] otherwise.
pub(crate) async fn insert_domain(
    conn: &mut SqliteConnection,
    row: &DomainRow,
) -> Result<(), MetaError> {
    sqlx::query(
        "INSERT INTO org_sso_domains \
         (id, org_id, domain, token, verified_at, last_checked_at, created_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(row.id.to_string())
    .bind(row.org_id.to_string())
    .bind(&row.domain)
    .bind(&row.token)
    .bind(row.verified_at)
    .bind(row.last_checked_at)
    .bind(row.created_at)
    .execute(&mut *conn)
    .await
    .map_err(|error| unique_or(error, "domain already listed"))?;
    Ok(())
}

/// `verified_at = now, last_checked_at = now`. `MetaError::Conflict` when another org verified
/// it first (the partial unique index).
///
/// # Errors
///
/// Returns [`MetaError::Conflict`] when another org holds the domain, and [`MetaError::Db`]
/// otherwise.
pub(crate) async fn mark_verified(
    conn: &mut SqliteConnection,
    id: Uuid,
    now: i64,
) -> Result<(), MetaError> {
    sqlx::query("UPDATE org_sso_domains SET verified_at = ?, last_checked_at = ? WHERE id = ?")
        .bind(now)
        .bind(now)
        .bind(id.to_string())
        .execute(&mut *conn)
        .await
        .map_err(|error| unique_or(error, "domain verified by another org"))?;
    Ok(())
}

/// Sets `last_checked_at`.
///
/// # Errors
///
/// Returns [`MetaError::Db`] when the update fails.
pub(crate) async fn mark_checked(
    conn: &mut SqliteConnection,
    id: Uuid,
    now: i64,
) -> Result<(), MetaError> {
    sqlx::query("UPDATE org_sso_domains SET last_checked_at = ? WHERE id = ?")
        .bind(now)
        .bind(id.to_string())
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// Deletes one of the org's domains. Returns whether it existed.
///
/// # Errors
///
/// Returns [`MetaError::Db`] when the delete fails.
pub(crate) async fn delete_domain(
    conn: &mut SqliteConnection,
    org: OrgId,
    id: Uuid,
) -> Result<bool, MetaError> {
    let changed = sqlx::query("DELETE FROM org_sso_domains WHERE org_id = ? AND id = ?")
        .bind(org.to_string())
        .bind(id.to_string())
        .execute(&mut *conn)
        .await?
        .rows_affected();
    Ok(changed == 1)
}

/// Enforcement's revocation (D-P-58) for every user of `domains` who is not an instance
/// admin: deletes their web sessions not started through `org`'s provider and their password
/// resets, and revokes all their personal access tokens. Returns the number of users and the
/// revoked token hashes (to evict from the PAT cache after commit).
///
/// # Errors
///
/// Returns [`MetaError`] when a query fails or a stored id is corrupt.
pub(crate) async fn revoke_for_domains(
    conn: &mut SqliteConnection,
    org: OrgId,
    domains: &[String],
    now: i64,
) -> Result<(u64, Vec<String>), MetaError> {
    let mut users = std::collections::BTreeSet::new();
    for domain in domains {
        let ids: Vec<String> = sqlx::query_scalar(
            "SELECT id FROM users WHERE is_instance_admin = 0 \
             AND substr(email, instr(email, '@') + 1) = ?",
        )
        .bind(domain)
        .fetch_all(&mut *conn)
        .await?;
        for id in ids {
            users.insert(parse_uuid(&id, "users", "id")?);
        }
    }
    let mut hashes = Vec::new();
    for user in &users {
        sqlx::query(
            "DELETE FROM web_sessions WHERE user_id = ? \
             AND (sso_org_id IS NULL OR sso_org_id != ?)",
        )
        .bind(user.to_string())
        .bind(org.to_string())
        .execute(&mut *conn)
        .await?;
        hashes.extend(revoke_user_tokens(&mut *conn, *user, now).await?);
        delete_user_resets(&mut *conn, *user).await?;
    }
    Ok((u64::try_from(users.len()).unwrap_or(u64::MAX), hashes))
}

/// Verified domains of active orgs that have a provider, with the provider's flags (P6's index).
///
/// # Errors
///
/// Returns [`MetaError`] when the query fails or a stored value is corrupt.
pub(crate) async fn routes(
    pool: &SqlitePool,
) -> Result<Vec<(String, OrgId, bool, bool)>, MetaError> {
    let rows: Vec<(String, String, i64, i64)> = sqlx::query_as(
        "SELECT d.domain, d.org_id, p.enabled, p.enforced \
         FROM org_sso_domains d \
         JOIN org_sso_providers p ON p.org_id = d.org_id \
         JOIN orgs o ON o.id = d.org_id \
         WHERE d.verified_at IS NOT NULL AND o.state = 'active' \
         ORDER BY d.domain",
    )
    .fetch_all(pool)
    .await?;
    rows.into_iter()
        .map(|(domain, org_id, enabled, enforced)| {
            Ok((
                domain,
                OrgId::from_uuid(parse_uuid(&org_id, "org_sso_domains", "org_id")?),
                parse_flag(enabled, "enabled")?,
                parse_flag(enforced, "enforced")?,
            ))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use cetra_core::ids::OrgId;
    use uuid::Uuid;

    use super::*;
    use crate::meta::identities::insert_identity;
    use crate::meta::orgs::{delete_org, insert_org, mark_org_deleting};
    use crate::meta::test_db;
    use crate::meta::users::{NewUser, insert_user};

    async fn domain_tx(
        conn: &mut SqliteConnection,
        org: OrgId,
        id: Uuid,
    ) -> Result<Option<DomainRow>, MetaError> {
        let record: Option<DomainRecord> = sqlx::query_as(DOMAIN_BY_ID)
            .bind(org.to_string())
            .bind(id.to_string())
            .fetch_optional(&mut *conn)
            .await?;
        record.map(domain_row).transpose()
    }

    async fn verified_elsewhere_tx(
        conn: &mut SqliteConnection,
        org: OrgId,
        domain: &str,
    ) -> Result<bool, MetaError> {
        let found: i64 = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM org_sso_domains \
             WHERE domain = ? AND verified_at IS NOT NULL AND org_id != ?)",
        )
        .bind(domain)
        .bind(org.to_string())
        .fetch_one(&mut *conn)
        .await?;
        Ok(found == 1)
    }

    fn write(enc: Option<Vec<u8>>) -> ProviderWrite {
        ProviderWrite {
            display_name: "Acme SSO".to_owned(),
            issuer: "https://idp.example".to_owned(),
            client_id: "acme-client".to_owned(),
            client_secret_enc: enc,
            jit_role: JitRole::Viewer,
        }
    }

    fn new_domain(org: OrgId, domain: &str, now: i64) -> DomainRow {
        DomainRow {
            id: Uuid::now_v7(),
            org_id: org,
            domain: domain.to_owned(),
            token: "0123456789abcdef0123456789abcdef".to_owned(),
            verified_at: None,
            last_checked_at: None,
            created_at: now,
        }
    }

    async fn org_named(db: &crate::meta::MetaDb, slug: &'static str) -> OrgId {
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

    #[tokio::test]
    async fn provider_round_trip() {
        let (_dir, db) = test_db().await;
        let org = org_named(&db, "acme").await;
        let id = Uuid::now_v7();
        let sealed = vec![0xde, 0xad, 0xbe, 0xef, 0x01];
        db.write(move |conn| {
            Box::pin(async move {
                put_provider(conn, org, id, write(Some(sealed)), 10).await?;
                Ok(())
            })
        })
        .await
        .unwrap();

        let row = provider(db.reader(), org).await.unwrap().unwrap();
        assert_eq!(row.id, id);
        assert_eq!(row.org_id, org);
        assert_eq!(row.display_name, "Acme SSO");
        assert_eq!(row.issuer, "https://idp.example");
        assert_eq!(row.client_id, "acme-client");
        assert_eq!(row.client_secret_enc, vec![0xde, 0xad, 0xbe, 0xef, 0x01]);
        assert!(!row.enabled && !row.enforced);
        assert_eq!(row.jit_role, JitRole::Viewer);
        assert_eq!((row.created_at, row.updated_at), (10, 10));
        let debug = format!("{row:?}");
        assert!(!debug.contains("deadbeef01"), "{debug}");
        assert!(!debug.contains("de, ad"), "{debug}");

        // Update without a secret keeps the sealed bytes; the id stays.
        db.write(move |conn| {
            Box::pin(async move {
                let mut update = write(None);
                update.display_name = "Renamed".to_owned();
                update.jit_role = JitRole::Member;
                put_provider(conn, org, Uuid::now_v7(), update, 20).await?;
                Ok(())
            })
        })
        .await
        .unwrap();
        let row = provider(db.reader(), org).await.unwrap().unwrap();
        assert_eq!(row.id, id);
        assert_eq!(row.display_name, "Renamed");
        assert_eq!(row.jit_role, JitRole::Member);
        assert_eq!(row.client_secret_enc, vec![0xde, 0xad, 0xbe, 0xef, 0x01]);
        assert_eq!(row.updated_at, 20);

        // Update with a secret replaces it.
        db.write(move |conn| {
            Box::pin(async move {
                put_provider(conn, org, id, write(Some(vec![7, 8])), 30).await?;
                Ok(())
            })
        })
        .await
        .unwrap();
        let row = provider(db.reader(), org).await.unwrap().unwrap();
        assert_eq!(row.client_secret_enc, vec![7, 8]);
    }

    #[tokio::test]
    async fn secret_is_sealed_to_its_row_and_login_is_recorded() {
        let (_dir, db) = test_db().await;
        let org = org_named(&db, "acme").await;
        let id = Uuid::now_v7();
        let secrets = crate::crypto::SecretBox::new(&[9; 32]);
        let sealed = secrets
            .seal(b"client-secret", secret_aad(&id))
            .expect("seal");
        db.write(move |conn| {
            Box::pin(async move {
                put_provider(conn, org, id, write(Some(sealed)), 1).await?;
                set_last_login(conn, org, 42).await?;
                Ok(())
            })
        })
        .await
        .unwrap();

        let row = active_providers(db.reader()).await.unwrap();
        assert_eq!(row.len(), 1);
        assert_eq!(row[0].last_login_at, Some(42));
        let opened = secrets
            .open(&row[0].client_secret_enc, secret_aad(&row[0].id))
            .expect("open");
        assert_eq!(opened, b"client-secret");
        assert!(
            secrets
                .open(&row[0].client_secret_enc, secret_aad(&Uuid::now_v7()))
                .is_err(),
            "a sealed secret opens only for its own row"
        );
    }

    #[tokio::test]
    async fn domain_reads_and_state_changes() {
        let (_dir, db) = test_db().await;
        let org = org_named(&db, "acme").await;
        let row = new_domain(org, "acme.test", 1);
        let id = row.id;
        db.write(move |conn| {
            Box::pin(async move {
                insert_domain(conn, &row).await?;
                let found = domain_tx(conn, org, id).await?.expect("row");
                assert_eq!(found.domain, "acme.test");
                assert_eq!(domains_tx(conn, org).await?.len(), 1);
                assert!(verified_row_tx(conn, "acme.test").await?.is_none());
                mark_verified(conn, id, 5).await?;
                mark_checked(conn, id, 6).await?;
                let verified = verified_row_tx(conn, "acme.test").await?.expect("verified");
                assert_eq!(verified.id, id);
                mark_unverified(conn, id).await?;
                assert!(verified_row_tx(conn, "acme.test").await?.is_none());
                Ok(())
            })
        })
        .await
        .unwrap();

        let stored = domain(db.reader(), org, id).await.unwrap().expect("row");
        assert_eq!(stored.verified_at, None);
        assert_eq!(stored.last_checked_at, Some(6));
        assert!(
            domain(db.reader(), org, Uuid::now_v7())
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn insert_without_a_secret_is_a_caller_bug() {
        let (_dir, db) = test_db().await;
        let org = org_named(&db, "acme").await;
        let error = db
            .write(move |conn| {
                Box::pin(async move {
                    put_provider(conn, org, Uuid::now_v7(), write(None), 1).await?;
                    Ok(())
                })
            })
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            MetaError::Corrupt {
                table: "org_sso_providers",
                column: "client_secret_enc"
            }
        ));
    }

    #[tokio::test]
    async fn domain_verified_by_one_org_only() {
        let (_dir, db) = test_db().await;
        let a = org_named(&db, "org-a").await;
        let b = org_named(&db, "org-b").await;
        let row_a = new_domain(a, "acme.test", 1);
        let row_b = new_domain(b, "acme.test", 1);
        let (id_a, id_b) = (row_a.id, row_b.id);
        db.write(move |conn| {
            Box::pin(async move {
                insert_domain(conn, &row_a).await?;
                insert_domain(conn, &row_b).await?;
                mark_verified(conn, id_a, 5).await?;
                Ok(())
            })
        })
        .await
        .unwrap();

        db.write(move |conn| {
            Box::pin(async move {
                let refused = mark_verified(conn, id_b, 6).await;
                assert!(matches!(
                    refused,
                    Err(MetaError::Conflict("domain verified by another org"))
                ));
                assert!(verified_elsewhere_tx(conn, b, "acme.test").await?);
                assert!(!verified_elsewhere_tx(conn, a, "acme.test").await?);
                Ok(())
            })
        })
        .await
        .unwrap();

        db.write(move |conn| {
            Box::pin(async move {
                assert!(delete_domain(conn, a, id_a).await?);
                mark_verified(conn, id_b, 7).await?;
                Ok(())
            })
        })
        .await
        .unwrap();
        let verified = domains(db.reader(), b).await.unwrap();
        assert_eq!(verified[0].verified_at, Some(7));
    }

    #[tokio::test]
    async fn duplicate_domain_in_one_org_conflicts() {
        let (_dir, db) = test_db().await;
        let a = org_named(&db, "org-a").await;
        let first = new_domain(a, "acme.test", 1);
        let second = new_domain(a, "acme.test", 1);
        db.write(move |conn| {
            Box::pin(async move {
                insert_domain(conn, &first).await?;
                let error = insert_domain(conn, &second).await.unwrap_err();
                assert!(matches!(
                    error,
                    MetaError::Conflict("domain already listed")
                ));
                Ok(())
            })
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn routes_skip_unverified_inactive_and_providerless() {
        let (_dir, db) = test_db().await;
        let with = org_named(&db, "with").await;
        let without = org_named(&db, "without").await;
        let gone = org_named(&db, "gone").await;
        let verified = new_domain(with, "with.test", 1);
        let pending = new_domain(with, "pending.test", 1);
        let providerless = new_domain(without, "bare.test", 1);
        let deleting = new_domain(gone, "gone.test", 1);
        let (id_verified, id_providerless, id_deleting) =
            (verified.id, providerless.id, deleting.id);
        db.write(move |conn| {
            Box::pin(async move {
                put_provider(conn, with, Uuid::now_v7(), write(Some(vec![1])), 1).await?;
                put_provider(conn, gone, Uuid::now_v7(), write(Some(vec![1])), 1).await?;
                set_flags(conn, with, true, false, 2).await?;
                insert_domain(conn, &verified).await?;
                insert_domain(conn, &pending).await?;
                insert_domain(conn, &providerless).await?;
                insert_domain(conn, &deleting).await?;
                mark_verified(conn, id_verified, 3).await?;
                mark_verified(conn, id_providerless, 3).await?;
                mark_verified(conn, id_deleting, 3).await?;
                assert!(mark_org_deleting(conn, gone, 4).await?);
                Ok(())
            })
        })
        .await
        .unwrap();

        let routed = routes(db.reader()).await.unwrap();
        assert_eq!(routed, vec![("with.test".to_owned(), with, true, false)]);
        let active = active_providers(db.reader()).await.unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].org_id, with);
        assert!(active[0].enabled);
    }

    #[tokio::test]
    async fn delete_provider_removes_only_its_identities() {
        let (_dir, db) = test_db().await;
        let org = org_named(&db, "acme").await;
        let user = db
            .write(|conn| {
                Box::pin(async move {
                    let user = insert_user(
                        conn,
                        NewUser {
                            email: "ada@example.com".to_owned(),
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
            .unwrap();
        let provider_name = identity_provider(org);
        db.write(move |conn| {
            Box::pin(async move {
                put_provider(conn, org, Uuid::now_v7(), write(Some(vec![1])), 1).await?;
                insert_identity(conn, user, &provider_name, "sub-1", "ada@example.com", 1).await?;
                insert_identity(conn, user, "oidc:x", "sub-2", "ada@example.com", 1).await?;
                assert!(delete_provider(conn, org).await?);
                Ok(())
            })
        })
        .await
        .unwrap();

        let left: Vec<String> = sqlx::query_scalar("SELECT provider FROM identities")
            .fetch_all(db.reader())
            .await
            .unwrap();
        assert_eq!(left, vec!["oidc:x".to_owned()]);
        assert!(provider(db.reader(), org).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn org_drop_cascades_and_keeps_other_identities() {
        let (_dir, db) = test_db().await;
        let org = org_named(&db, "acme").await;
        let user = db
            .write(|conn| {
                Box::pin(async move {
                    let user = insert_user(
                        conn,
                        NewUser {
                            email: "ada@example.com".to_owned(),
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
            .unwrap();
        let provider_name = identity_provider(org);
        db.write(move |conn| {
            Box::pin(async move {
                put_provider(conn, org, Uuid::now_v7(), write(Some(vec![1])), 1).await?;
                let domain = new_domain(org, "acme.test", 1);
                insert_domain(conn, &domain).await?;
                insert_identity(conn, user, &provider_name, "sub-1", "ada@example.com", 1).await?;
                insert_identity(conn, user, "oidc:x", "sub-2", "ada@example.com", 1).await?;
                sqlx::query(
                    "INSERT INTO web_sessions \
                     (id, user_id, created_at, last_seen_at, expires_at, sso_org_id) \
                     VALUES ('sess-1', ?, 1, 1, 9999999999, ?)",
                )
                .bind(user.to_string())
                .bind(org.to_string())
                .execute(&mut *conn)
                .await?;
                delete_org(conn, org).await?;
                Ok(())
            })
        })
        .await
        .unwrap();

        let providers: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM org_sso_providers")
            .fetch_one(db.reader())
            .await
            .unwrap();
        let domains: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM org_sso_domains")
            .fetch_one(db.reader())
            .await
            .unwrap();
        assert_eq!((providers, domains), (0, 0));
        let left: Vec<String> = sqlx::query_scalar("SELECT provider FROM identities")
            .fetch_all(db.reader())
            .await
            .unwrap();
        assert_eq!(left, vec!["oidc:x".to_owned()]);
        let session_org: Option<String> =
            sqlx::query_scalar("SELECT sso_org_id FROM web_sessions WHERE id = 'sess-1'")
                .fetch_one(db.reader())
                .await
                .unwrap();
        assert_eq!(session_org, None);
    }
}
