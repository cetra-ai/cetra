// SPDX-License-Identifier: Elastic-2.0
//! Daily refresh of SAML metadata fetched from a URL (D-SA-11). A rotated IdP certificate
//! reaches the routing on its own; a changed entity ID is refused and the last good metadata
//! is kept. Pasted metadata and manual fields have no URL and are never refreshed.

use cetra_saml::parse_metadata;

use super::MetadataFetchError;
use super::store::{self, RefreshOutcome};
use crate::http::AppState;
use crate::meta::MetaError;

const DAY_NS: i64 = 86_400_000_000_000;
/// SAML providers refreshed per pass. The rest wait for the next pass.
const BATCH: u32 = 20;

/// What one pass did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Refresh {
    /// Providers whose metadata was fetched and stored.
    pub(crate) refreshed: usize,
    /// Refreshed providers whose certificate set changed.
    pub(crate) rotated: usize,
    /// Providers whose refresh failed; they keep their last good metadata.
    pub(crate) failed: usize,
}

/// Refreshes up to `BATCH` SAML providers whose metadata URL was last tried a day ago or more.
///
/// # Errors
///
/// Returns [`MetaError`] when a query or a write fails.
pub(crate) async fn refresh_once(state: &AppState, now: i64) -> Result<Refresh, MetaError> {
    let due = store::due_saml_refreshes(state.meta.reader(), now - DAY_NS, BATCH).await?;
    let mut run = Refresh::default();
    let mut reload = false;
    for (org, row) in due {
        let Some(url) = row.metadata_url.clone() else {
            continue;
        };
        let outcome = fetch(state, &url, &row.idp_entity_id).await;
        let failure = match &outcome {
            RefreshOutcome::Failed(class) => Some(*class),
            RefreshOutcome::Fetched { .. } => None,
        };
        let sso_changed = match &outcome {
            RefreshOutcome::Fetched { sso_url, .. } => *sso_url != row.sso_url,
            RefreshOutcome::Failed(_) => false,
        };
        let provider_id = row.provider_id;
        let used_entity_id = row.idp_entity_id.clone();
        let used_url = url;
        let written = state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    // The owner may have changed the provider while the URL was fetched. Then
                    // this outcome describes a provider that no longer exists: write nothing.
                    let Some(current) = store::saml_tx(conn, provider_id).await? else {
                        return Ok(None);
                    };
                    if current.idp_entity_id != used_entity_id
                        || current.metadata_url.as_deref() != Some(used_url.as_str())
                    {
                        return Ok(None);
                    }
                    let changed = store::record_refresh(conn, provider_id, outcome, now).await?;
                    Ok(Some(changed))
                })
            })
            .await?;
        let Some(changed) = written else {
            continue;
        };
        if let Some(class) = failure {
            run.failed += 1;
            tracing::info!(
                org_id = %org,
                reason = class,
                "org sso saml metadata refresh failed"
            );
            continue;
        }
        run.refreshed += 1;
        if changed {
            run.rotated += 1;
            tracing::warn!(
                target: "audit",
                org_id = %org,
                "org sso saml certificates changed"
            );
        }
        reload |= sso_changed;
    }
    if run.rotated > 0 || reload {
        super::refresh(state).await;
    }
    Ok(run)
}

/// Fetches and checks the metadata at `url` against the stored entity ID. A failure is its
/// class only (one of `store::REFRESH_ERRORS`).
async fn fetch(state: &AppState, url: &str, entity_id: &str) -> RefreshOutcome {
    let bytes = match state.org_sso.fetch_metadata(url).await {
        Ok(bytes) => bytes,
        Err(MetadataFetchError::Denied) => return RefreshOutcome::Failed("denied"),
        Err(MetadataFetchError::TooLarge) => return RefreshOutcome::Failed("too_large"),
        Err(MetadataFetchError::Status) => return RefreshOutcome::Failed("status"),
        Err(MetadataFetchError::Url | MetadataFetchError::Unavailable) => {
            return RefreshOutcome::Failed("fetch_failed");
        }
    };
    let Ok(metadata) = parse_metadata(&bytes) else {
        return RefreshOutcome::Failed("unusable");
    };
    if metadata.entity_id() != entity_id {
        return RefreshOutcome::Failed("entity_id_changed");
    }
    RefreshOutcome::Fetched {
        sso_url: metadata.sso_url().to_owned(),
        certificates: metadata
            .certificates()
            .iter()
            .map(|certificate| certificate.der().to_vec())
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use axum::http::{Method, StatusCode};
    use cetra_core::ids::OrgId;
    use cetra_saml::test_idp::{IDP_ENTITY_ID, ResponseOptions, TestKey, metadata_xml};
    use serde_json::json;
    use uuid::Uuid;

    use super::*;
    use crate::ee::org_sso::{dns, store};
    use crate::test_support::oauth::{SESSION_NAME, location, one_cookie};
    use crate::test_support::{
        Member, MetadataServer, TestApp, assert_logs_clean, begin_saml, post_acs, reload_org_sso,
        saml_app, saml_options,
    };

    const SAML_PATH: &str = "/api/v1/orgs/acme/sso/saml";
    const DAY: Duration = Duration::from_secs(24 * 60 * 60);

    /// Saves the org's SAML provider from `url` as its owner, as the settings page does.
    async fn save_from_url(app: &TestApp, owner: &Member, url: &str) {
        let payload = json!({
            "display_name": "Acme SAML",
            "jit_role": "member",
            "metadata_url": url,
        });
        let saved = app
            .request(Method::PUT, SAML_PATH)
            .header("cookie", &owner.cookie)
            .json(&payload)
            .send()
            .await;
        assert_eq!(saved.status, StatusCode::OK);
    }

    /// Makes the org's provider usable for a sign-in: enabled, `acme.test` verified, and a test
    /// sign-in on record, then reloads the routing.
    async fn enable_acme(app: &TestApp, org: OrgId) {
        let token = dns::new_token().expect("token");
        let now = app.state.clock.now_ns();
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    store::set_flags(conn, org, true, false, now).await?;
                    store::set_last_login(conn, org, now).await?;
                    let row = store::DomainRow {
                        id: Uuid::now_v7(),
                        org_id: org,
                        domain: "acme.test".to_owned(),
                        token,
                        verified_at: None,
                        last_checked_at: None,
                        created_at: now,
                    };
                    let domain_id = row.id;
                    store::insert_domain(conn, &row).await?;
                    store::mark_verified(conn, domain_id, now).await?;
                    Ok(())
                })
            })
            .await
            .expect("enable the provider");
        reload_org_sso(app).await;
    }

    /// The org's SAML configuration as stored.
    async fn stored_saml(app: &TestApp, org: OrgId) -> store::SamlRow {
        let provider = store::provider(app.state.meta.reader(), org)
            .await
            .expect("query")
            .expect("a provider");
        store::saml(app.state.meta.reader(), provider.id)
            .await
            .expect("query")
            .expect("a SAML row")
    }

    #[tokio::test]
    async fn a_rotated_certificate_is_picked_up() {
        let (app, seeded, owner) = saml_app().await;
        let org = seeded.org_id;
        let server = MetadataServer::start(metadata_xml(&[TestKey::Idp]).into_bytes()).await;
        save_from_url(&app, &owner, &server.url()).await;
        server.set(
            StatusCode::OK,
            metadata_xml(&[TestKey::Idp, TestKey::IdpNext]).into_bytes(),
        );
        app.state.clock.advance(DAY + Duration::from_secs(1));
        let now = app.state.clock.now_ns();
        let run = refresh_once(&app.state, now).await.expect("a refresh pass");
        assert_eq!(
            run,
            Refresh {
                refreshed: 1,
                rotated: 1,
                failed: 0
            }
        );
        let saml = stored_saml(&app, org).await;
        assert_eq!(saml.certificates_changed_at, Some(now));
        assert_eq!(saml.refresh_error, None);
        enable_acme(&app, org).await;
        let (request_id, binder, _) = begin_saml(&app, None).await;
        let options = ResponseOptions {
            key: TestKey::IdpNext,
            ..saml_options(&app, org, &request_id)
        };
        let response = post_acs(
            &app,
            org,
            &cetra_saml::test_idp::response_b64(&options),
            &[binder.as_str()],
        )
        .await;
        assert_eq!(location(&response), "/acme");
        one_cookie(&response, SESSION_NAME);
    }

    #[tokio::test]
    async fn a_changed_entity_id_is_refused_and_kept() {
        let (app, seeded, owner) = saml_app().await;
        let org = seeded.org_id;
        let server = MetadataServer::start(metadata_xml(&[TestKey::Idp]).into_bytes()).await;
        save_from_url(&app, &owner, &server.url()).await;
        let before = stored_saml(&app, org).await;
        server.set(
            StatusCode::OK,
            metadata_xml(&[TestKey::Idp])
                .replace(IDP_ENTITY_ID, "https://idp.test/other")
                .into_bytes(),
        );
        app.state.clock.advance(DAY + Duration::from_secs(1));
        let now = app.state.clock.now_ns();
        let run = refresh_once(&app.state, now).await.expect("a refresh pass");
        assert_eq!(
            run,
            Refresh {
                refreshed: 0,
                rotated: 0,
                failed: 1
            }
        );
        let after = stored_saml(&app, org).await;
        assert_eq!(after.refresh_error.as_deref(), Some("entity_id_changed"));
        assert_eq!(after.idp_entity_id, before.idp_entity_id);
        assert_eq!(after.certificates, before.certificates);
        assert_eq!(after.certificates_changed_at, None);
    }

    #[tokio::test]
    async fn a_failing_url_keeps_the_last_good_metadata() {
        let (app, seeded, owner) = saml_app().await;
        let org = seeded.org_id;
        let server = MetadataServer::start(metadata_xml(&[TestKey::Idp]).into_bytes()).await;
        save_from_url(&app, &owner, &server.url()).await;
        let before = stored_saml(&app, org).await;
        server.set(StatusCode::INTERNAL_SERVER_ERROR, Vec::new());
        app.state.clock.advance(DAY + Duration::from_secs(1));
        let now = app.state.clock.now_ns();
        let run = refresh_once(&app.state, now).await.expect("a refresh pass");
        assert_eq!(
            run,
            Refresh {
                refreshed: 0,
                rotated: 0,
                failed: 1
            }
        );
        let after = stored_saml(&app, org).await;
        assert_eq!(after.refresh_error.as_deref(), Some("status"));
        assert_eq!(after.refresh_checked_at, Some(now));
        assert_eq!(after.refreshed_at, before.refreshed_at);
        assert_eq!(after.sso_url, before.sso_url);
        assert_eq!(after.certificates, before.certificates);
    }

    #[tokio::test]
    async fn a_recent_check_is_not_due() {
        let (app, _seeded, owner) = saml_app().await;
        let server = MetadataServer::start(metadata_xml(&[TestKey::Idp]).into_bytes()).await;
        save_from_url(&app, &owner, &server.url()).await;
        app.state.clock.advance(DAY + Duration::from_secs(1));
        let now = app.state.clock.now_ns();
        let first = refresh_once(&app.state, now).await.expect("a refresh pass");
        assert_eq!(first.refreshed, 1);
        let later = now + 3_600_000_000_000;
        let second = refresh_once(&app.state, later)
            .await
            .expect("a refresh pass");
        assert_eq!(second, Refresh::default());
    }

    #[tokio::test]
    async fn pasted_metadata_is_never_refreshed() {
        let (app, seeded, owner) = saml_app().await;
        let org = seeded.org_id;
        let payload = json!({
            "display_name": "Acme SAML",
            "jit_role": "member",
            "metadata_xml": metadata_xml(&[TestKey::Idp]),
        });
        let saved = app
            .request(Method::PUT, SAML_PATH)
            .header("cookie", &owner.cookie)
            .json(&payload)
            .send()
            .await;
        assert_eq!(saved.status, StatusCode::OK);
        app.state.clock.advance(DAY + Duration::from_secs(1));
        let now = app.state.clock.now_ns();
        let run = refresh_once(&app.state, now).await.expect("a refresh pass");
        assert_eq!(run, Refresh::default());
        let saml = stored_saml(&app, org).await;
        assert_eq!(saml.metadata_url, None);
        assert_eq!(saml.refresh_checked_at, None);
    }

    #[tokio::test]
    async fn logs_carry_no_url() {
        let (app, _seeded, owner) = saml_app().await;
        let server = MetadataServer::start(metadata_xml(&[TestKey::Idp]).into_bytes()).await;
        let url = server.url();
        save_from_url(&app, &owner, &url).await;
        server.set(StatusCode::INTERNAL_SERVER_ERROR, Vec::new());
        app.state.clock.advance(DAY + Duration::from_secs(1));
        let now = app.state.clock.now_ns();
        let failed = assert_logs_clean(
            &[
                url.as_str(),
                "127.0.0.1",
                IDP_ENTITY_ID,
                "BEGIN CERTIFICATE",
            ],
            refresh_once(&app.state, now),
        )
        .await
        .expect("a refresh pass");
        assert_eq!(failed.failed, 1);
        server.set(
            StatusCode::OK,
            metadata_xml(&[TestKey::Idp, TestKey::IdpNext]).into_bytes(),
        );
        app.state.clock.advance(DAY + Duration::from_secs(1));
        let now = app.state.clock.now_ns();
        let rotated = assert_logs_clean(
            &[
                url.as_str(),
                "127.0.0.1",
                IDP_ENTITY_ID,
                "BEGIN CERTIFICATE",
            ],
            refresh_once(&app.state, now),
        )
        .await
        .expect("a refresh pass");
        assert_eq!(rotated.rotated, 1);
    }
}
