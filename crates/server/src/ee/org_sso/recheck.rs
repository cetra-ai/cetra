// SPDX-License-Identifier: Elastic-2.0
//! Daily re-proof of verified domains (D-P-72, auth review #15). A domain whose TXT record is
//! gone stops being verified, so a lapsed domain cannot keep signing people in or linking
//! accounts for its old owner.

use super::{dns, settings, store};
use crate::http::AppState;
use crate::meta::MetaError;

const DAY_NS: i64 = 24 * 60 * 60 * 1_000_000_000;
/// Domains checked per pass. The rest wait for the next pass.
const BATCH: u32 = 50;

/// What one pass did.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Recheck {
    /// Domains whose record is still there.
    pub(crate) kept: u32,
    /// Domains whose record is gone, now unverified.
    pub(crate) lapsed: u32,
    /// Lookups that failed; the domain keeps its state until the next pass.
    pub(crate) failed: u32,
}

/// Re-checks the verified domains last proven more than a day ago. A failed lookup is not a
/// lapse (a resolver outage must not lock every org out), only an answer without the record is.
///
/// # Errors
///
/// Returns [`MetaError`] when a query fails.
pub(crate) async fn recheck_once(state: &AppState, now: i64) -> Result<Recheck, MetaError> {
    let due = store::stale_verified_domains(state.meta.reader(), now - DAY_NS, BATCH).await?;
    let mut run = Recheck::default();
    for row in due {
        let id = row.id;
        let values = match state.org_sso.dns.txt(&dns::record_name(&row.domain)).await {
            Ok(values) => values,
            Err(_) => {
                run.failed += 1;
                continue;
            }
        };
        if settings::has(&values, &dns::record_value(&row.token)) {
            state
                .meta
                .write(move |conn| {
                    Box::pin(async move { store::mark_checked(conn, id, now).await })
                })
                .await?;
            run.kept += 1;
        } else {
            state
                .meta
                .write(move |conn| Box::pin(async move { store::mark_unverified(conn, id).await }))
                .await?;
            run.lapsed += 1;
            tracing::warn!(
                target: "audit",
                org_id = %row.org_id,
                domain_id = %id,
                "org sso domain lapsed: the TXT record is gone"
            );
        }
    }
    if run.lapsed > 0 {
        super::refresh(state).await;
    }
    Ok(run)
}
