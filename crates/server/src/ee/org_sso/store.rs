// SPDX-License-Identifier: Elastic-2.0
//! Storage of org SSO providers and domains (D-P-52, D-P-54, D-P-59).

use std::collections::{BTreeSet, HashMap};
use std::fmt;

use cetra_core::ids::OrgId;
use sqlx::{SqliteConnection, SqlitePool};
use uuid::Uuid;

use crate::api::org_sso::{JitRole, SsoProtocol};
use crate::crypto::Aad;
use crate::meta::password_resets::delete_user_resets;
use crate::meta::personal_tokens::revoke_user_tokens;
use crate::meta::{MetaError, unique_or};

/// One `org_sso_providers` row. `Debug` omits the sealed secret.
pub(crate) struct ProviderRow {
    pub(crate) id: Uuid,
    pub(crate) org_id: OrgId,
    pub(crate) protocol: SsoProtocol,
    pub(crate) display_name: String,
    /// The OIDC columns; `None` for a SAML row (the table's CHECK keeps them NULL).
    pub(crate) oidc: Option<OidcColumns>,
    pub(crate) enabled: bool,
    pub(crate) enforced: bool,
    pub(crate) jit_role: JitRole,
    pub(crate) last_login_at: Option<i64>,
    pub(crate) created_at: i64,
    pub(crate) updated_at: i64,
}

/// The OIDC columns of a provider row. `Debug` omits the sealed secret.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct OidcColumns {
    pub(crate) issuer: String,
    pub(crate) client_id: String,
    pub(crate) client_secret_enc: Vec<u8>,
}

impl fmt::Debug for OidcColumns {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OidcColumns")
            .field("issuer", &self.issuer)
            .field("client_id", &self.client_id)
            .finish_non_exhaustive()
    }
}

impl fmt::Debug for ProviderRow {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderRow")
            .field("id", &self.id)
            .field("org_id", &self.org_id)
            .field("protocol", &self.protocol)
            .field("display_name", &self.display_name)
            .field("oidc", &self.oidc)
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

/// Attribute names an owner set; `None` uses the crate's defaults for that field (D-SA-6).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct SamlAttributes {
    pub(crate) email: Option<String>,
    pub(crate) display_name: Option<String>,
    pub(crate) first_name: Option<String>,
    pub(crate) last_name: Option<String>,
}

/// One `org_sso_saml` row with its certificates in `position` order. `Debug` prints counts
/// and states only: the metadata URL, entity ID, SSO URL and certificates stay out of logs.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct SamlRow {
    pub(crate) provider_id: Uuid,
    pub(crate) metadata_url: Option<String>,
    pub(crate) idp_entity_id: String,
    pub(crate) sso_url: String,
    pub(crate) certificates: Vec<Vec<u8>>,
    pub(crate) attributes: SamlAttributes,
    pub(crate) refreshed_at: Option<i64>,
    pub(crate) refresh_checked_at: Option<i64>,
    pub(crate) refresh_error: Option<String>,
    pub(crate) certificates_changed_at: Option<i64>,
}

impl fmt::Debug for SamlRow {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SamlRow")
            .field("provider_id", &self.provider_id)
            .field("certificates", &self.certificates.len())
            .field("attributes", &self.attributes)
            .field("refreshed_at", &self.refreshed_at)
            .field("refresh_checked_at", &self.refresh_checked_at)
            .field("refresh_error", &self.refresh_error)
            .field("certificates_changed_at", &self.certificates_changed_at)
            .finish_non_exhaustive()
    }
}

/// What `put_saml_provider` writes. `Debug` prints counts only.
pub(crate) struct SamlWrite {
    pub(crate) display_name: String,
    pub(crate) jit_role: JitRole,
    pub(crate) metadata_url: Option<String>,
    pub(crate) idp_entity_id: String,
    pub(crate) sso_url: String,
    pub(crate) certificates: Vec<Vec<u8>>,
    pub(crate) attributes: SamlAttributes,
    /// `Some(now)` when the metadata was just fetched from `metadata_url`.
    pub(crate) refreshed_at: Option<i64>,
}

impl fmt::Debug for SamlWrite {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SamlWrite")
            .field("jit_role", &self.jit_role)
            .field("metadata_url", &self.metadata_url.is_some())
            .field("certificates", &self.certificates.len())
            .field("attributes", &self.attributes)
            .field("refreshed_at", &self.refreshed_at)
            .finish_non_exhaustive()
    }
}

/// What a scheduled refresh found (SA2.6). Its URL and certificates make `Debug` acceptable
/// only because it is never logged: never pass it to a `tracing` macro.
#[derive(Debug)]
pub(crate) enum RefreshOutcome {
    /// Usable metadata with the stored entity ID.
    Fetched {
        sso_url: String,
        certificates: Vec<Vec<u8>>,
    },
    /// A failure class, stored in `refresh_error` (one of `REFRESH_ERRORS`).
    Failed(&'static str),
}

/// The stored failure classes (`refresh_error`), snake_case. `record_refresh` starts with
/// `debug_assert!(REFRESH_ERRORS.contains(&class))` for `Failed(class)`.
pub(crate) const REFRESH_ERRORS: [&str; 6] = [
    "fetch_failed",
    "status",
    "too_large",
    "denied",
    "unusable",
    "entity_id_changed",
];

/// `AAD` of `client_secret_enc` for provider row `id` (table `org_sso_providers`).
pub(crate) fn secret_aad(id: &Uuid) -> Aad<'_> {
    Aad {
        table: "org_sso_providers",
        column: "client_secret_enc",
        row_id: id,
    }
}

const PROVIDER_BY_ORG: &str = "SELECT id, org_id, protocol, display_name, issuer, client_id, \
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
    protocol: String,
    display_name: String,
    issuer: Option<String>,
    client_id: Option<String>,
    client_secret_enc: Option<Vec<u8>>,
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
    let protocol = SsoProtocol::from_str_opt(&record.protocol)
        .ok_or_else(|| corrupt("org_sso_providers", "protocol"))?;
    let oidc = match protocol {
        SsoProtocol::Oidc => Some(OidcColumns {
            issuer: record
                .issuer
                .ok_or_else(|| corrupt("org_sso_providers", "issuer"))?,
            client_id: record
                .client_id
                .ok_or_else(|| corrupt("org_sso_providers", "client_id"))?,
            client_secret_enc: record
                .client_secret_enc
                .ok_or_else(|| corrupt("org_sso_providers", "client_secret_enc"))?,
        }),
        SsoProtocol::Saml => None,
    };
    Ok(ProviderRow {
        id: parse_uuid(&record.id, "org_sso_providers", "id")?,
        org_id: OrgId::from_uuid(parse_uuid(&record.org_id, "org_sso_providers", "org_id")?),
        protocol,
        display_name: record.display_name,
        oidc,
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
        "SELECT p.id, p.org_id, p.protocol, p.display_name, p.issuer, p.client_id, \
                p.client_secret_enc, p.enabled, p.enforced, p.jit_role, p.last_login_at, \
                p.created_at, p.updated_at \
         FROM org_sso_providers p JOIN orgs o ON o.id = p.org_id \
         WHERE o.state = 'active' ORDER BY p.org_id",
    )
    .fetch_all(pool)
    .await?;
    records.into_iter().map(provider_row).collect()
}

/// Inserts (with `id`, enabled and enforced off) or updates the org's OIDC provider. Over a
/// SAML provider it switches the org to OIDC: the SAML rows are deleted and the flags reset
/// (D-SA-2). On insert
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
    match provider_tx(&mut *conn, org).await? {
        Some(row) if row.protocol == SsoProtocol::Saml => {
            // Switch from SAML: the SAML rows go, the flags and last login reset (D-SA-2).
            let Some(secret) = write.client_secret_enc else {
                return Err(corrupt("org_sso_providers", "client_secret_enc"));
            };
            sqlx::query("DELETE FROM org_sso_saml WHERE provider_id = ?")
                .bind(row.id.to_string())
                .execute(&mut *conn)
                .await?;
            sqlx::query(
                "UPDATE org_sso_providers SET protocol = 'oidc', display_name = ?, issuer = ?, \
                 client_id = ?, client_secret_enc = ?, jit_role = ?, enabled = 0, enforced = 0, \
                 last_login_at = NULL, updated_at = ? WHERE org_id = ?",
            )
            .bind(&write.display_name)
            .bind(&write.issuer)
            .bind(&write.client_id)
            .bind(secret)
            .bind(write.jit_role.as_str())
            .bind(now)
            .bind(org.to_string())
            .execute(&mut *conn)
            .await?;
        }
        Some(_) => {
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
        }
        None => {
            let Some(secret) = write.client_secret_enc else {
                return Err(corrupt("org_sso_providers", "client_secret_enc"));
            };
            sqlx::query(
                "INSERT INTO org_sso_providers \
                 (id, org_id, protocol, display_name, issuer, client_id, client_secret_enc, \
                  jit_role, created_at, updated_at) \
                 VALUES (?, ?, 'oidc', ?, ?, ?, ?, ?, ?, ?)",
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
        }
    }
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

/// `org_sso_saml` with its provider's org, for one provider id.
const SAML_BY_PROVIDER: &str = "SELECT s.provider_id, p.org_id, s.metadata_url, s.idp_entity_id, \
     s.sso_url, s.email_attribute, s.name_attribute, s.first_name_attribute, \
     s.last_name_attribute, s.refreshed_at, s.refresh_checked_at, s.refresh_error, \
     s.certificates_changed_at FROM org_sso_saml s JOIN org_sso_providers p ON p.id = s.provider_id \
     WHERE s.provider_id = ?";

/// Every SAML row of an active org.
const SAML_ACTIVE: &str = "SELECT s.provider_id, p.org_id, s.metadata_url, s.idp_entity_id, \
     s.sso_url, s.email_attribute, s.name_attribute, s.first_name_attribute, \
     s.last_name_attribute, s.refreshed_at, s.refresh_checked_at, s.refresh_error, \
     s.certificates_changed_at FROM org_sso_saml s JOIN org_sso_providers p ON p.id = s.provider_id \
     JOIN orgs o ON o.id = p.org_id WHERE o.state = 'active' ORDER BY s.provider_id";

/// The URL-backed SAML rows of active orgs whose last refresh attempt is before a cutoff.
const SAML_DUE: &str = "SELECT s.provider_id, p.org_id, s.metadata_url, s.idp_entity_id, \
     s.sso_url, s.email_attribute, s.name_attribute, s.first_name_attribute, \
     s.last_name_attribute, s.refreshed_at, s.refresh_checked_at, s.refresh_error, \
     s.certificates_changed_at FROM org_sso_saml s JOIN org_sso_providers p ON p.id = s.provider_id \
     JOIN orgs o ON o.id = p.org_id \
     WHERE s.metadata_url IS NOT NULL AND COALESCE(s.refresh_checked_at, 0) < ? \
       AND o.state = 'active' \
     ORDER BY COALESCE(s.refresh_checked_at, 0), s.provider_id LIMIT ?";

const SAML_CERTIFICATES: &str =
    "SELECT der FROM org_sso_saml_certificates WHERE provider_id = ? ORDER BY position";

/// The certificates of every SAML row of an active org, in position order per provider.
const SAML_CERTIFICATES_ACTIVE: &str = "SELECT c.provider_id, c.der \
     FROM org_sso_saml_certificates c JOIN org_sso_providers p ON p.id = c.provider_id \
     JOIN orgs o ON o.id = p.org_id WHERE o.state = 'active' \
     ORDER BY c.provider_id, c.position";

#[derive(sqlx::FromRow)]
struct SamlRecord {
    provider_id: String,
    org_id: String,
    metadata_url: Option<String>,
    idp_entity_id: String,
    sso_url: String,
    email_attribute: Option<String>,
    name_attribute: Option<String>,
    first_name_attribute: Option<String>,
    last_name_attribute: Option<String>,
    refreshed_at: Option<i64>,
    refresh_checked_at: Option<i64>,
    refresh_error: Option<String>,
    certificates_changed_at: Option<i64>,
}

/// A SAML record with its certificates (none is corrupt) and the provider's org.
fn saml_row(record: SamlRecord, certificates: Vec<Vec<u8>>) -> Result<(OrgId, SamlRow), MetaError> {
    let provider_id = parse_uuid(&record.provider_id, "org_sso_saml", "provider_id")?;
    let org_id = OrgId::from_uuid(parse_uuid(&record.org_id, "org_sso_providers", "org_id")?);
    if certificates.is_empty() {
        return Err(corrupt("org_sso_saml_certificates", "der"));
    }
    Ok((
        org_id,
        SamlRow {
            provider_id,
            metadata_url: record.metadata_url,
            idp_entity_id: record.idp_entity_id,
            sso_url: record.sso_url,
            certificates,
            attributes: SamlAttributes {
                email: record.email_attribute,
                display_name: record.name_attribute,
                first_name: record.first_name_attribute,
                last_name: record.last_name_attribute,
            },
            refreshed_at: record.refreshed_at,
            refresh_checked_at: record.refresh_checked_at,
            refresh_error: record.refresh_error,
            certificates_changed_at: record.certificates_changed_at,
        },
    ))
}

/// The certificates of one SAML row, in position order.
async fn certificate_rows(
    conn: &mut SqliteConnection,
    provider_id: &str,
) -> Result<Vec<Vec<u8>>, MetaError> {
    let rows: Vec<Vec<u8>> = sqlx::query_scalar(SAML_CERTIFICATES)
        .bind(provider_id)
        .fetch_all(&mut *conn)
        .await?;
    Ok(rows)
}

/// The SAML configuration of provider row `provider_id`.
///
/// # Errors
///
/// Returns [`MetaError`] when the query fails or a stored value is corrupt.
pub(crate) async fn saml(
    pool: &SqlitePool,
    provider_id: Uuid,
) -> Result<Option<SamlRow>, MetaError> {
    let mut conn = pool.acquire().await?;
    saml_tx(&mut conn, provider_id).await
}

/// The SAML configuration of provider row `provider_id`, in the write transaction.
///
/// # Errors
///
/// Returns [`MetaError`] when the query fails or a stored value is corrupt.
pub(crate) async fn saml_tx(
    conn: &mut SqliteConnection,
    provider_id: Uuid,
) -> Result<Option<SamlRow>, MetaError> {
    let record: Option<SamlRecord> = sqlx::query_as(SAML_BY_PROVIDER)
        .bind(provider_id.to_string())
        .fetch_optional(&mut *conn)
        .await?;
    let Some(record) = record else {
        return Ok(None);
    };
    let certificates = certificate_rows(conn, &record.provider_id).await?;
    Ok(Some(saml_row(record, certificates)?.1))
}

/// Every SAML configuration of an active org, keyed by provider id (for `OrgSso::reload`).
///
/// # Errors
///
/// Returns [`MetaError`] when a query fails or a stored value is corrupt.
pub(crate) async fn active_saml(pool: &SqlitePool) -> Result<HashMap<Uuid, SamlRow>, MetaError> {
    let mut conn = pool.acquire().await?;
    let records: Vec<SamlRecord> = sqlx::query_as(SAML_ACTIVE).fetch_all(&mut *conn).await?;
    let certificate_records: Vec<(String, Vec<u8>)> = sqlx::query_as(SAML_CERTIFICATES_ACTIVE)
        .fetch_all(&mut *conn)
        .await?;
    let mut certificates: HashMap<String, Vec<Vec<u8>>> = HashMap::new();
    for (provider_id, der) in certificate_records {
        certificates.entry(provider_id).or_default().push(der);
    }
    let mut rows = HashMap::new();
    for record in records {
        let found = certificates.remove(&record.provider_id).unwrap_or_default();
        let (_org, row) = saml_row(record, found)?;
        rows.insert(row.provider_id, row);
    }
    Ok(rows)
}

/// Updates a SAML provider's display name, JIT role and attribute names, keeping its
/// metadata. Returns false when the org's provider is not SAML.
///
/// # Errors
///
/// Returns [`MetaError::Db`] when an update fails.
pub(crate) async fn update_saml_settings(
    conn: &mut SqliteConnection,
    org: OrgId,
    display_name: &str,
    jit_role: JitRole,
    attributes: &SamlAttributes,
    now: i64,
) -> Result<bool, MetaError> {
    let Some(row) = provider_tx(&mut *conn, org).await? else {
        return Ok(false);
    };
    if row.protocol != SsoProtocol::Saml {
        return Ok(false);
    }
    sqlx::query(
        "UPDATE org_sso_providers SET display_name = ?, jit_role = ?, updated_at = ? \
         WHERE org_id = ?",
    )
    .bind(display_name)
    .bind(jit_role.as_str())
    .bind(now)
    .bind(org.to_string())
    .execute(&mut *conn)
    .await?;
    sqlx::query(
        "UPDATE org_sso_saml SET email_attribute = ?, name_attribute = ?, \
         first_name_attribute = ?, last_name_attribute = ? WHERE provider_id = ?",
    )
    .bind(attributes.email.as_deref())
    .bind(attributes.display_name.as_deref())
    .bind(attributes.first_name.as_deref())
    .bind(attributes.last_name.as_deref())
    .bind(row.id.to_string())
    .execute(&mut *conn)
    .await?;
    Ok(true)
}

/// Saves a SAML provider: insert, replace, or switch from OIDC. Returns the provider row id.
/// A switch from OIDC keeps the row id and resets the flags and the last login; a SAML
/// replacement keeps the row id and resets the last login only when the entity ID changes.
/// Identities are not deleted here.
///
/// # Errors
///
/// Returns [`MetaError`] when a write fails or a stored row is corrupt.
pub(crate) async fn put_saml_provider(
    conn: &mut SqliteConnection,
    org: OrgId,
    new_id: Uuid,
    write: SamlWrite,
    now: i64,
) -> Result<Uuid, MetaError> {
    let id = match provider_tx(&mut *conn, org).await? {
        None => {
            sqlx::query(
                "INSERT INTO org_sso_providers \
                 (id, org_id, protocol, display_name, jit_role, created_at, updated_at) \
                 VALUES (?, ?, 'saml', ?, ?, ?, ?)",
            )
            .bind(new_id.to_string())
            .bind(org.to_string())
            .bind(&write.display_name)
            .bind(write.jit_role.as_str())
            .bind(now)
            .bind(now)
            .execute(&mut *conn)
            .await?;
            new_id
        }
        Some(row) if row.protocol == SsoProtocol::Oidc => {
            sqlx::query(
                "UPDATE org_sso_providers SET protocol = 'saml', issuer = NULL, client_id = NULL, \
                 client_secret_enc = NULL, display_name = ?, jit_role = ?, enabled = 0, \
                 enforced = 0, last_login_at = NULL, updated_at = ? WHERE org_id = ?",
            )
            .bind(&write.display_name)
            .bind(write.jit_role.as_str())
            .bind(now)
            .bind(org.to_string())
            .execute(&mut *conn)
            .await?;
            row.id
        }
        Some(row) => {
            let old = saml_tx(&mut *conn, row.id)
                .await?
                .ok_or_else(|| corrupt("org_sso_saml", "provider_id"))?;
            let entity_changed = old.idp_entity_id != write.idp_entity_id;
            // A changed IdP entity ID turns the provider off until a new Test sign-in (D-SA-21).
            sqlx::query(
                "UPDATE org_sso_providers SET display_name = ?, jit_role = ?, updated_at = ?, \
                 enabled = CASE WHEN ? THEN 0 ELSE enabled END, \
                 last_login_at = CASE WHEN ? THEN NULL ELSE last_login_at END \
                 WHERE org_id = ?",
            )
            .bind(&write.display_name)
            .bind(write.jit_role.as_str())
            .bind(now)
            .bind(entity_changed)
            .bind(entity_changed)
            .bind(org.to_string())
            .execute(&mut *conn)
            .await?;
            row.id
        }
    };
    sqlx::query("DELETE FROM org_sso_saml WHERE provider_id = ?")
        .bind(id.to_string())
        .execute(&mut *conn)
        .await?;
    sqlx::query(
        "INSERT INTO org_sso_saml \
         (provider_id, metadata_url, idp_entity_id, sso_url, email_attribute, name_attribute, \
          first_name_attribute, last_name_attribute, refreshed_at, refresh_checked_at, \
          refresh_error, certificates_changed_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NULL, NULL)",
    )
    .bind(id.to_string())
    .bind(write.metadata_url.as_deref())
    .bind(&write.idp_entity_id)
    .bind(&write.sso_url)
    .bind(write.attributes.email.as_deref())
    .bind(write.attributes.display_name.as_deref())
    .bind(write.attributes.first_name.as_deref())
    .bind(write.attributes.last_name.as_deref())
    .bind(write.refreshed_at)
    .bind(write.refreshed_at)
    .execute(&mut *conn)
    .await?;
    for (position, der) in write.certificates.iter().enumerate() {
        sqlx::query(
            "INSERT INTO org_sso_saml_certificates (provider_id, position, der) VALUES (?, ?, ?)",
        )
        .bind(id.to_string())
        .bind(i64::try_from(position).unwrap_or(i64::MAX))
        .bind(der.as_slice())
        .execute(&mut *conn)
        .await?;
    }
    Ok(id)
}

/// SAML providers of active orgs with a metadata URL whose last refresh attempt is older than
/// `cutoff`, oldest attempt first, at most `limit`.
///
/// # Errors
///
/// Returns [`MetaError`] when a query fails or a stored value is corrupt.
pub(crate) async fn due_saml_refreshes(
    pool: &SqlitePool,
    cutoff: i64,
    limit: u32,
) -> Result<Vec<(OrgId, SamlRow)>, MetaError> {
    let mut conn = pool.acquire().await?;
    let records: Vec<SamlRecord> = sqlx::query_as(SAML_DUE)
        .bind(cutoff)
        .bind(i64::from(limit))
        .fetch_all(&mut *conn)
        .await?;
    let mut due = Vec::with_capacity(records.len());
    for record in records {
        let certificates = certificate_rows(&mut conn, &record.provider_id).await?;
        due.push(saml_row(record, certificates)?);
    }
    Ok(due)
}

/// Records a scheduled refresh. Returns whether the certificate set changed.
///
/// # Errors
///
/// Returns [`MetaError`] when a write fails or a stored row is corrupt.
pub(crate) async fn record_refresh(
    conn: &mut SqliteConnection,
    provider_id: Uuid,
    outcome: RefreshOutcome,
    now: i64,
) -> Result<bool, MetaError> {
    let id = provider_id.to_string();
    match outcome {
        RefreshOutcome::Fetched {
            sso_url,
            certificates,
        } => {
            let stored = certificate_rows(conn, &id).await?;
            let stored_set: BTreeSet<&[u8]> = stored.iter().map(Vec::as_slice).collect();
            let fetched_set: BTreeSet<&[u8]> = certificates.iter().map(Vec::as_slice).collect();
            let changed = stored_set != fetched_set;
            if changed {
                sqlx::query("DELETE FROM org_sso_saml_certificates WHERE provider_id = ?")
                    .bind(&id)
                    .execute(&mut *conn)
                    .await?;
                for (position, der) in certificates.iter().enumerate() {
                    sqlx::query(
                        "INSERT INTO org_sso_saml_certificates (provider_id, position, der) \
                         VALUES (?, ?, ?)",
                    )
                    .bind(&id)
                    .bind(i64::try_from(position).unwrap_or(i64::MAX))
                    .bind(der.as_slice())
                    .execute(&mut *conn)
                    .await?;
                }
                sqlx::query(
                    "UPDATE org_sso_saml SET sso_url = ?, refreshed_at = ?, refresh_checked_at = ?, \
                     refresh_error = NULL, certificates_changed_at = ? WHERE provider_id = ?",
                )
                .bind(&sso_url)
                .bind(now)
                .bind(now)
                .bind(now)
                .bind(&id)
                .execute(&mut *conn)
                .await?;
            } else {
                sqlx::query(
                    "UPDATE org_sso_saml SET sso_url = ?, refreshed_at = ?, refresh_checked_at = ?, \
                     refresh_error = NULL WHERE provider_id = ?",
                )
                .bind(&sso_url)
                .bind(now)
                .bind(now)
                .bind(&id)
                .execute(&mut *conn)
                .await?;
            }
            Ok(changed)
        }
        RefreshOutcome::Failed(class) => {
            debug_assert!(REFRESH_ERRORS.contains(&class));
            sqlx::query(
                "UPDATE org_sso_saml SET refresh_error = ?, refresh_checked_at = ? \
                 WHERE provider_id = ?",
            )
            .bind(class)
            .bind(now)
            .bind(&id)
            .execute(&mut *conn)
            .await?;
            Ok(false)
        }
    }
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
    let mut users = BTreeSet::new();
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
        assert_eq!(row.protocol, SsoProtocol::Oidc);
        let oidc = row.oidc.as_ref().expect("oidc columns");
        assert_eq!(oidc.issuer, "https://idp.example");
        assert_eq!(oidc.client_id, "acme-client");
        assert_eq!(oidc.client_secret_enc, vec![0xde, 0xad, 0xbe, 0xef, 0x01]);
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
        assert_eq!(
            row.oidc.as_ref().expect("oidc columns").client_secret_enc,
            vec![0xde, 0xad, 0xbe, 0xef, 0x01]
        );
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
        assert_eq!(
            row.oidc.as_ref().expect("oidc columns").client_secret_enc,
            vec![7, 8]
        );
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
            .open(
                &row[0]
                    .oidc
                    .as_ref()
                    .expect("oidc columns")
                    .client_secret_enc,
                secret_aad(&row[0].id),
            )
            .expect("open");
        assert_eq!(opened, b"client-secret");
        assert!(
            secrets
                .open(
                    &row[0]
                        .oidc
                        .as_ref()
                        .expect("oidc columns")
                        .client_secret_enc,
                    secret_aad(&Uuid::now_v7())
                )
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

    fn saml_write(certificates: Vec<Vec<u8>>, entity: &str) -> SamlWrite {
        SamlWrite {
            display_name: "Acme SAML".to_owned(),
            jit_role: JitRole::Viewer,
            metadata_url: None,
            idp_entity_id: entity.to_owned(),
            sso_url: "https://idp.example/sso".to_owned(),
            certificates,
            attributes: SamlAttributes {
                email: Some("mail".to_owned()),
                ..SamlAttributes::default()
            },
            refreshed_at: Some(10),
        }
    }

    fn with_url(mut payload: SamlWrite, url: &str) -> SamlWrite {
        payload.metadata_url = Some(url.to_owned());
        payload
    }

    async fn put_saml(
        db: &crate::meta::MetaDb,
        org: OrgId,
        certificates: Vec<Vec<u8>>,
        entity: &'static str,
        now: i64,
    ) -> Uuid {
        let payload = saml_write(certificates, entity);
        db.write(move |conn| {
            Box::pin(
                async move { put_saml_provider(conn, org, Uuid::now_v7(), payload, now).await },
            )
        })
        .await
        .unwrap()
    }

    async fn refresh(
        db: &crate::meta::MetaDb,
        id: Uuid,
        outcome: RefreshOutcome,
        now: i64,
    ) -> bool {
        db.write(move |conn| Box::pin(async move { record_refresh(conn, id, outcome, now).await }))
            .await
            .unwrap()
    }

    async fn count(db: &crate::meta::MetaDb, table: &'static str) -> i64 {
        sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT COUNT(*) FROM {table}")))
            .fetch_one(db.reader())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn an_oidc_row_reads_back_with_its_protocol() {
        let (_dir, db) = test_db().await;
        let org = org_named(&db, "acme").await;
        db.write(move |conn| {
            Box::pin(async move {
                put_provider(conn, org, Uuid::now_v7(), write(Some(vec![1])), 1).await?;
                Ok(())
            })
        })
        .await
        .unwrap();
        let row = provider(db.reader(), org).await.unwrap().unwrap();
        assert_eq!(row.protocol, SsoProtocol::Oidc);
        let oidc = row.oidc.as_ref().expect("oidc columns");
        assert_eq!(oidc.issuer, "https://idp.example");
        assert_eq!(oidc.client_id, "acme-client");
        assert_eq!(oidc.client_secret_enc, vec![1]);
        assert!(saml(db.reader(), row.id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn a_saml_provider_round_trips() {
        let (_dir, db) = test_db().await;
        let org = org_named(&db, "acme").await;
        let id = Uuid::now_v7();
        let payload = saml_write(vec![vec![1, 1], vec![2, 2]], "https://idp.example/metadata");
        let written = db
            .write(move |conn| {
                Box::pin(async move { put_saml_provider(conn, org, id, payload, 10).await })
            })
            .await
            .unwrap();
        assert_eq!(written, id);

        let row = provider(db.reader(), org).await.unwrap().unwrap();
        assert_eq!(row.id, id);
        assert_eq!(row.protocol, SsoProtocol::Saml);
        assert!(row.oidc.is_none());
        assert_eq!(row.display_name, "Acme SAML");
        assert!(!row.enabled && !row.enforced);

        let stored = saml(db.reader(), id).await.unwrap().expect("saml row");
        assert_eq!(stored.certificates, vec![vec![1, 1], vec![2, 2]]);
        assert_eq!(stored.idp_entity_id, "https://idp.example/metadata");
        assert_eq!(stored.attributes.email.as_deref(), Some("mail"));
        assert_eq!(stored.attributes.display_name, None);
        assert_eq!(stored.refreshed_at, Some(10));
        assert_eq!(stored.refresh_checked_at, Some(10));
        let debug = format!("{stored:?}");
        assert!(!debug.contains("idp.example"), "{debug}");
    }

    #[tokio::test]
    async fn switching_oidc_to_saml_clears_the_oidc_columns_and_the_flags() {
        let (_dir, db) = test_db().await;
        let org = org_named(&db, "acme").await;
        db.write(move |conn| {
            Box::pin(async move {
                put_provider(conn, org, Uuid::now_v7(), write(Some(vec![1])), 1).await?;
                set_flags(conn, org, true, true, 2).await?;
                set_last_login(conn, org, 3).await?;
                Ok(())
            })
        })
        .await
        .unwrap();
        let before = provider(db.reader(), org).await.unwrap().unwrap();
        assert!(before.enabled && before.enforced);
        assert_eq!(before.last_login_at, Some(3));

        put_saml(&db, org, vec![vec![5]], "https://idp.example/metadata", 4).await;

        let after = provider(db.reader(), org).await.unwrap().unwrap();
        assert_eq!(after.id, before.id);
        assert_eq!(after.protocol, SsoProtocol::Saml);
        assert!(after.oidc.is_none());
        assert!(!after.enabled && !after.enforced);
        assert_eq!(after.last_login_at, None);
    }

    #[tokio::test]
    async fn switching_saml_to_oidc_drops_the_saml_rows() {
        let (_dir, db) = test_db().await;
        let org = org_named(&db, "acme").await;
        put_saml(
            &db,
            org,
            vec![vec![1], vec![2]],
            "https://idp.example/metadata",
            1,
        )
        .await;
        db.write(move |conn| {
            Box::pin(async move {
                put_provider(conn, org, Uuid::now_v7(), write(Some(vec![9])), 5).await?;
                Ok(())
            })
        })
        .await
        .unwrap();

        let row = provider(db.reader(), org).await.unwrap().unwrap();
        assert_eq!(row.protocol, SsoProtocol::Oidc);
        assert_eq!(row.oidc.as_ref().expect("oidc").client_secret_enc, vec![9]);
        assert!(saml(db.reader(), row.id).await.unwrap().is_none());
        assert_eq!(count(&db, "org_sso_saml").await, 0);
        assert_eq!(count(&db, "org_sso_saml_certificates").await, 0);
    }

    #[tokio::test]
    async fn an_oidc_switch_without_a_secret_is_a_caller_bug() {
        let (_dir, db) = test_db().await;
        let org = org_named(&db, "acme").await;
        put_saml(&db, org, vec![vec![1]], "https://idp.example/metadata", 1).await;
        let result = db
            .write(move |conn| {
                Box::pin(async move {
                    put_provider(conn, org, Uuid::now_v7(), write(None), 5).await?;
                    Ok(())
                })
            })
            .await;
        assert!(matches!(
            result,
            Err(MetaError::Corrupt {
                table: "org_sso_providers",
                column: "client_secret_enc",
            })
        ));
    }

    #[tokio::test]
    async fn replacing_saml_with_another_entity_id_resets_last_login() {
        let (_dir, db) = test_db().await;
        let org = org_named(&db, "acme").await;
        put_saml(&db, org, vec![vec![1]], "https://idp.example/a", 1).await;
        db.write(move |conn| {
            Box::pin(async move {
                set_last_login(conn, org, 7).await?;
                Ok(())
            })
        })
        .await
        .unwrap();

        put_saml(&db, org, vec![vec![2]], "https://idp.example/a", 2).await;
        let same = provider(db.reader(), org).await.unwrap().unwrap();
        assert_eq!(same.last_login_at, Some(7));

        put_saml(&db, org, vec![vec![2]], "https://idp.example/b", 3).await;
        let other = provider(db.reader(), org).await.unwrap().unwrap();
        assert_eq!(other.last_login_at, None);
    }

    #[tokio::test]
    async fn deleting_a_saml_provider_cascades() {
        let (_dir, db) = test_db().await;
        let org = org_named(&db, "acme").await;
        put_saml(
            &db,
            org,
            vec![vec![1], vec![2]],
            "https://idp.example/metadata",
            1,
        )
        .await;
        db.write(move |conn| {
            Box::pin(async move {
                delete_provider(conn, org).await?;
                Ok(())
            })
        })
        .await
        .unwrap();
        assert_eq!(count(&db, "org_sso_saml").await, 0);
        assert_eq!(count(&db, "org_sso_saml_certificates").await, 0);
        assert_eq!(count(&db, "org_sso_providers").await, 0);
    }

    #[tokio::test]
    async fn the_check_constraint_refuses_a_mixed_row() {
        let (_dir, db) = test_db().await;
        let org = org_named(&db, "acme").await;
        let result = db
            .write(move |conn| {
                Box::pin(async move {
                    sqlx::query(
                        "INSERT INTO org_sso_providers (id, org_id, protocol, display_name, \
                         issuer, jit_role, created_at, updated_at) \
                         VALUES (?, ?, 'saml', 'x', 'https://idp.example', 'viewer', 1, 1)",
                    )
                    .bind(Uuid::now_v7().to_string())
                    .bind(org.to_string())
                    .execute(&mut *conn)
                    .await?;
                    Ok(())
                })
            })
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn due_refreshes_skip_pasted_metadata_and_recent_checks() {
        let (_dir, db) = test_db().await;
        let pasted = org_named(&db, "pasted").await;
        let stale = org_named(&db, "stale").await;
        let fresh = org_named(&db, "fresh").await;
        put_saml(&db, pasted, vec![vec![1]], "https://idp.example/a", 1).await;
        let stale_payload = with_url(
            saml_write(vec![vec![2]], "https://idp.example/b"),
            "https://idp.example/metadata",
        );
        let fresh_payload = with_url(
            saml_write(vec![vec![3]], "https://idp.example/c"),
            "https://idp.example/metadata",
        );
        db.write(move |conn| {
            Box::pin(async move {
                put_saml_provider(conn, stale, Uuid::now_v7(), stale_payload, 1).await?;
                let fresh_id =
                    put_saml_provider(conn, fresh, Uuid::now_v7(), fresh_payload, 1).await?;
                record_refresh(conn, fresh_id, RefreshOutcome::Failed("fetch_failed"), 100).await?;
                Ok(())
            })
        })
        .await
        .unwrap();

        // Both URL rows were last checked at 10 (the pasted one never is due).
        assert!(
            due_saml_refreshes(db.reader(), 5, 10)
                .await
                .unwrap()
                .is_empty()
        );
        let due = due_saml_refreshes(db.reader(), 50, 10).await.unwrap();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].0, stale);
        assert_eq!(due[0].1.certificates, vec![vec![2]]);
    }

    #[tokio::test]
    async fn a_refresh_with_new_certificates_records_the_change() {
        let (_dir, db) = test_db().await;
        let org = org_named(&db, "acme").await;
        let id = put_saml(&db, org, vec![vec![1], vec![2]], "https://idp.example/m", 1).await;

        let changed = refresh(
            &db,
            id,
            RefreshOutcome::Fetched {
                sso_url: "https://idp.example/new".to_owned(),
                certificates: vec![vec![1], vec![3]],
            },
            20,
        )
        .await;
        assert!(changed);

        let stored = saml(db.reader(), id).await.unwrap().expect("saml row");
        assert_eq!(stored.certificates, vec![vec![1], vec![3]]);
        assert_eq!(stored.certificates_changed_at, Some(20));
        assert_eq!(stored.sso_url, "https://idp.example/new");
        assert_eq!(stored.refreshed_at, Some(20));
        assert_eq!(stored.refresh_checked_at, Some(20));
        assert_eq!(stored.refresh_error, None);
    }

    #[tokio::test]
    async fn a_refresh_with_the_same_set_in_another_order_is_no_change() {
        let (_dir, db) = test_db().await;
        let org = org_named(&db, "acme").await;
        let id = put_saml(&db, org, vec![vec![1], vec![2]], "https://idp.example/m", 1).await;

        let changed = refresh(
            &db,
            id,
            RefreshOutcome::Fetched {
                sso_url: "https://idp.example/sso".to_owned(),
                certificates: vec![vec![2], vec![1]],
            },
            30,
        )
        .await;
        assert!(!changed);

        let stored = saml(db.reader(), id).await.unwrap().expect("saml row");
        assert_eq!(stored.certificates, vec![vec![1], vec![2]]);
        assert_eq!(stored.certificates_changed_at, None);
        assert_eq!(stored.refreshed_at, Some(30));
    }

    #[tokio::test]
    async fn a_failed_refresh_keeps_the_metadata() {
        let (_dir, db) = test_db().await;
        let org = org_named(&db, "acme").await;
        let id = put_saml(&db, org, vec![vec![1]], "https://idp.example/m", 1).await;

        let changed = refresh(&db, id, RefreshOutcome::Failed("fetch_failed"), 40).await;
        assert!(!changed);

        let stored = saml(db.reader(), id).await.unwrap().expect("saml row");
        assert_eq!(stored.certificates, vec![vec![1]]);
        assert_eq!(stored.sso_url, "https://idp.example/sso");
        assert_eq!(stored.refreshed_at, Some(10));
        assert_eq!(stored.refresh_checked_at, Some(40));
        assert_eq!(stored.refresh_error.as_deref(), Some("fetch_failed"));
    }

    #[tokio::test]
    async fn active_saml_ignores_dropped_orgs() {
        let (_dir, db) = test_db().await;
        let kept = org_named(&db, "kept").await;
        let dropped = org_named(&db, "dropped").await;
        let kept_id = put_saml(&db, kept, vec![vec![1]], "https://idp.example/a", 1).await;
        put_saml(&db, dropped, vec![vec![2]], "https://idp.example/b", 1).await;
        db.write(move |conn| {
            Box::pin(async move {
                mark_org_deleting(conn, dropped, 5).await?;
                Ok(())
            })
        })
        .await
        .unwrap();

        let rows = active_saml(db.reader()).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[&kept_id].certificates, vec![vec![1]]);
    }

    #[tokio::test]
    async fn update_saml_settings_keeps_the_metadata() {
        let (_dir, db) = test_db().await;
        let org = org_named(&db, "acme").await;
        let id = put_saml(&db, org, vec![vec![1]], "https://idp.example/m", 1).await;
        let attributes = SamlAttributes {
            email: Some("upn".to_owned()),
            ..SamlAttributes::default()
        };
        let updated = db
            .write(move |conn| {
                Box::pin(async move {
                    update_saml_settings(conn, org, "Renamed", JitRole::Member, &attributes, 9)
                        .await
                })
            })
            .await
            .unwrap();
        assert!(updated);

        let row = provider(db.reader(), org).await.unwrap().unwrap();
        assert_eq!(row.display_name, "Renamed");
        assert_eq!(row.jit_role, JitRole::Member);
        let stored = saml(db.reader(), id).await.unwrap().expect("saml row");
        assert_eq!(stored.attributes.email.as_deref(), Some("upn"));
        assert_eq!(stored.idp_entity_id, "https://idp.example/m");
        assert_eq!(stored.certificates, vec![vec![1]]);
    }

    #[tokio::test]
    async fn update_saml_settings_refuses_an_oidc_provider() {
        let (_dir, db) = test_db().await;
        let org = org_named(&db, "acme").await;
        db.write(move |conn| {
            Box::pin(async move {
                put_provider(conn, org, Uuid::now_v7(), write(Some(vec![1])), 1).await?;
                Ok(())
            })
        })
        .await
        .unwrap();
        let updated = db
            .write(move |conn| {
                Box::pin(async move {
                    update_saml_settings(
                        conn,
                        org,
                        "Renamed",
                        JitRole::Member,
                        &SamlAttributes::default(),
                        9,
                    )
                    .await
                })
            })
            .await
            .unwrap();
        assert!(!updated);
    }
}
