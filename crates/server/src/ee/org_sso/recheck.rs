// SPDX-License-Identifier: Elastic-2.0
//! Daily re-proof of verified domains (D-P-72, auth review #15). A domain whose TXT record is
//! gone stops being verified, so a lapsed domain cannot keep signing people in or linking
//! accounts for its old owner.

use super::{dns, settings, store};
use crate::audit::catalog::SsoAction;
use crate::audit::{self, Event, TargetKind};
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
            let org = row.org_id;
            state
                .meta
                .write(move |conn| {
                    Box::pin(async move {
                        store::mark_unverified(conn, id).await?;
                        let event = Event::new(SsoAction::DomainLapsed)
                            .org(org)
                            .target(TargetKind::SsoDomain, id);
                        let ctx = audit::Ctx::system();
                        audit::record(conn, &ctx, event, now).await
                    })
                })
                .await?;
            run.lapsed += 1;
            tracing::warn!(
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

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::test_support::org_sso_app;

    /// A verified domain whose record is gone is recorded by the system, in its org's log.
    #[tokio::test]
    async fn lapse_is_recorded_by_system() {
        let (app, seeded, _owner) = org_sso_app().await;
        let id = uuid::Uuid::now_v7();
        let now = app.state.clock.now_ns();
        let row = store::DomainRow {
            id,
            org_id: seeded.org_id,
            domain: "acme.test".to_owned(),
            token: "token-1".to_owned(),
            verified_at: Some(now),
            last_checked_at: Some(now),
            created_at: now,
        };
        app.state
            .meta
            .write(move |conn| Box::pin(async move { store::insert_domain(conn, &row).await }))
            .await
            .unwrap();
        app.dns_txt(&dns::record_name("acme.test"), &["something else"]);
        app.state.clock.advance(Duration::from_secs(25 * 3_600));
        let later = app.state.clock.now_ns();

        let run = recheck_once(&app.state, later).await.unwrap();
        assert_eq!(run.lapsed, 1);

        let rows: Vec<(String, Option<String>, String, Option<String>, String, String)> = sqlx::query_as(
            "SELECT action, org_id, actor_kind, actor_id, target_kind, target_id \
             FROM audit_events WHERE action = 'sso.domain_lapsed'",
        )
        .fetch_all(app.state.meta.reader())
        .await
        .unwrap();
        assert_eq!(rows.len(), 1);
        let (action, org, actor_kind, actor_id, target_kind, target_id) = &rows[0];
        assert_eq!(action, "sso.domain_lapsed");
        assert_eq!(org.as_deref(), Some(seeded.org_id.to_string().as_str()));
        assert_eq!(actor_kind, "system");
        assert!(actor_id.is_none(), "the system has no actor id");
        assert_eq!(target_kind, "sso_domain");
        assert_eq!(target_id, &id.to_string());
    }
}
