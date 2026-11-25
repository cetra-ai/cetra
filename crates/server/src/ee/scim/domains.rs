// SPDX-License-Identifier: Elastic-2.0
//! The domains a SCIM user's email must be in (D-EA-13, D-EA-17, track EA M5 P-1).

use cetra_core::ids::OrgId;
use sqlx::SqliteConnection;

use crate::ee::org_sso::instance_claims;
use crate::ee::org_sso::store as org_sso_store;
use crate::http::AppState;
use crate::meta::MetaError;

/// The org's SCIM domains (P-1): its verified `org_sso_domains` rows minus the domains the
/// instance configuration claims (D-P-54), plus the org token's `sso_domains` (D-EA-17; empty
/// off a control-plane cell). Lowercase, sorted and without duplicates.
///
/// # Errors
///
/// Returns [`MetaError::Db`] when the query fails.
pub(crate) async fn scim_domains_tx(
    state: &AppState,
    conn: &mut SqliteConnection,
    org: OrgId,
    now: i64,
) -> Result<Vec<String>, MetaError> {
    let mut domains: Vec<String> = org_sso_store::domains_tx(conn, org)
        .await?
        .into_iter()
        .filter(|row| row.verified_at.is_some())
        .map(|row| row.domain)
        .filter(|domain| !instance_claims(&state.config.auth, domain))
        .collect();
    domains.extend(state.entitlements.sso_domains(org, now));
    for domain in &mut domains {
        domain.make_ascii_lowercase();
    }
    domains.sort();
    domains.dedup();
    Ok(domains)
}

/// Whether the part of `email` after its last `@` is one of `domains`.
pub(crate) fn in_domains(domains: &[String], email: &str) -> bool {
    email
        .rsplit_once('@')
        .is_some_and(|(_, domain)| domains.iter().any(|listed| listed == domain))
}
