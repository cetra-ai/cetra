// SPDX-License-Identifier: Elastic-2.0
//! Per-org SSO (track P M5, D-P-25, D-P-41): org OIDC providers, verified email domains,
//! enforcement and login discovery. Self-hosted instances; works for any org with `org_sso`.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex, PoisonError, RwLock};

use cetra_core::ids::OrgId;

use crate::api::org_sso::{JitRole, SsoProtocol};
use crate::auth::oidc::{IdpHttp, IdpHttpError, OidcProvider};
use crate::config::{AuthSettings, Secret};
use crate::crypto::SecretBox;
use crate::http::AppState;
use crate::meta::{MetaDb, MetaError};

pub(crate) mod dns;
pub(crate) mod login;
pub(crate) mod recheck;
pub(crate) mod saml_pending;
pub(crate) mod settings;
pub(crate) mod store;

/// A verified domain's org and its provider's flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DomainRoute {
    pub(crate) org_id: OrgId,
    pub(crate) enabled: bool,
    pub(crate) enforced: bool,
}

/// An org's provider flags (P10 refuses a disabled provider outside a test sign-in).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ProviderFlags {
    pub(crate) enabled: bool,
    pub(crate) enforced: bool,
}

/// What a built provider depends on (D-P-62, D-SA-2). `updated_at` is not used: tests run on a
/// fixed manual clock, and flags changes must not rebuild the provider. A SAML provider keeps
/// its IdP entity ID, SSO URL, certificates, attribute names, display name and JIT role here.
#[derive(Clone, PartialEq, Eq)]
enum ProviderConfig {
    Oidc {
        issuer: String,
        client_id: String,
        client_secret_enc: Vec<u8>,
        display_name: String,
        jit_role: JitRole,
    },
    Saml {
        idp_entity_id: String,
        sso_url: String,
        certificates: Vec<Vec<u8>>,
        attributes: store::SamlAttributes,
        display_name: String,
        jit_role: JitRole,
    },
}

impl fmt::Debug for ProviderConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ProviderConfig { .. }")
    }
}

/// An org's built provider. `Debug` omits the client secret (OidcProvider's own `Debug` does).
#[derive(Debug)]
pub(crate) struct OrgProvider {
    pub(crate) kind: ProviderKind,
    pub(crate) jit_role: JitRole,
    config: ProviderConfig,
}

/// The protocol-specific part of a built provider.
#[derive(Debug)]
pub(crate) enum ProviderKind {
    Oidc(OidcProvider),
    Saml(SamlProvider),
}

/// A built SAML provider: the trusted metadata and the attribute names to read.
pub(crate) struct SamlProvider {
    pub(crate) metadata: cetra_saml::IdpMetadata,
    pub(crate) attributes: cetra_saml::AttributeNames,
}

/// `Debug` prints nothing of the metadata: its entity ID, SSO URL and certificates stay out of
/// logs.
impl fmt::Debug for SamlProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SamlProvider { .. }")
    }
}

/// cetra's SAML endpoints for `org` (D-SA-12). Paths are absolute; URLs join `public_url`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SpUrls {
    pub(crate) entity_id: String,
    pub(crate) acs_url: String,
    pub(crate) start_url: String,
    pub(crate) acs_path: String,
    pub(crate) start_path: String,
    pub(crate) saml_path: String,
}

/// The SP entity ID, ACS and start URLs of `org`, from the instance's public URL (D-SA-12).
pub(crate) fn sp_urls(public_url: &crate::config::PublicUrl, org: OrgId) -> SpUrls {
    let saml_path = format!("/api/auth/saml/{org}");
    let acs_path = format!("{saml_path}/acs");
    let start_path = format!("{saml_path}/start");
    SpUrls {
        entity_id: public_url.join(&saml_path),
        acs_url: public_url.join(&acs_path),
        start_url: public_url.join(&start_path),
        acs_path,
        start_path,
        saml_path,
    }
}

/// The attribute names to read: the crate's defaults, with each field the owner set replaced
/// by that one name (D-SA-6).
pub(crate) fn attribute_names(attributes: &store::SamlAttributes) -> cetra_saml::AttributeNames {
    let mut names = cetra_saml::AttributeNames::default();
    if let Some(name) = &attributes.email {
        names.email = vec![name.clone()];
    }
    if let Some(name) = &attributes.display_name {
        names.display_name = vec![name.clone()];
    }
    if let Some(name) = &attributes.first_name {
        names.first_name = vec![name.clone()];
    }
    if let Some(name) = &attributes.last_name {
        names.last_name = vec![name.clone()];
    }
    names
}

#[derive(Debug, Default)]
struct Index {
    domains: HashMap<String, DomainRoute>,
    flags: HashMap<OrgId, ProviderFlags>,
    providers: HashMap<OrgId, Arc<OrgProvider>>,
}

/// Org SSO state in `AppInner` (D-P-62).
#[derive(Debug)]
pub(crate) struct OrgSso {
    index: RwLock<Arc<Index>>,
    /// Held across a reload's reads and its store, so a stale reload cannot overwrite a newer one.
    reload_lock: tokio::sync::Mutex<()>,
    /// `IdpHttp::guarded(identity_http_client(policy), policy)` (P2).
    http: IdpHttp,
    /// The egress policy `http` enforces.
    policy: cetra_eval::EgressPolicy,
    /// Pending SAML AuthnRequests (D-SA-10).
    saml_pending: Mutex<saml_pending::PendingTable>,
    pub(crate) dns: dns::TxtResolver,
}

/// Why `OrgSso::new` failed.
#[derive(Debug, thiserror::Error)]
pub(crate) enum OrgSsoError {
    /// The identity provider client could not be built.
    #[error("could not build the identity provider client")]
    Client,
}

/// What `reload` found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Reloaded {
    /// Verified domains ignored because instance config lists them (D-P-54).
    pub(crate) claimed: usize,
    /// Providers whose secret could not be opened (skipped).
    pub(crate) unreadable: usize,
}

impl OrgSso {
    /// An empty index. Call [`Self::reload`] before serving.
    ///
    /// # Errors
    ///
    /// Returns [`OrgSsoError::Client`] when the HTTP client cannot be built.
    pub(crate) fn new(
        policy: cetra_eval::EgressPolicy,
        dns: dns::TxtResolver,
    ) -> Result<Self, OrgSsoError> {
        let client = cetra_eval::identity_http_client(policy).map_err(|_| OrgSsoError::Client)?;
        Ok(Self {
            index: RwLock::new(Arc::new(Index::default())),
            reload_lock: tokio::sync::Mutex::new(()),
            http: IdpHttp::guarded(client, policy),
            policy,
            saml_pending: Mutex::new(saml_pending::PendingTable::default()),
            dns,
        })
    }

    /// Rebuilds the index from `meta.db`. A provider whose `ProviderConfig` did not change
    /// keeps its `Arc` (and so its cached metadata).
    ///
    /// # Errors
    ///
    /// Returns [`MetaError`] when the stored rows cannot be read.
    pub(crate) async fn reload(
        &self,
        meta: &MetaDb,
        secrets: &SecretBox,
        auth: &AuthSettings,
    ) -> Result<Reloaded, MetaError> {
        let _serialized = self.reload_lock.lock().await;
        let mut claimed = 0;
        let mut domains = HashMap::new();
        for (domain, org_id, enabled, enforced) in store::routes(meta.reader()).await? {
            if instance_claims(auth, &domain) {
                claimed += 1;
                continue;
            }
            domains.insert(
                domain,
                DomainRoute {
                    org_id,
                    enabled,
                    enforced,
                },
            );
        }

        let current = self.snapshot();
        let saml = store::active_saml(meta.reader()).await?;
        let mut flags = HashMap::new();
        let mut providers = HashMap::new();
        let mut unreadable = 0;
        for row in store::active_providers(meta.reader()).await? {
            flags.insert(
                row.org_id,
                ProviderFlags {
                    enabled: row.enabled,
                    enforced: row.enforced,
                },
            );
            let config = match row.protocol {
                SsoProtocol::Oidc => row.oidc.as_ref().map(|oidc| ProviderConfig::Oidc {
                    issuer: oidc.issuer.clone(),
                    client_id: oidc.client_id.clone(),
                    client_secret_enc: oidc.client_secret_enc.clone(),
                    display_name: row.display_name.clone(),
                    jit_role: row.jit_role,
                }),
                SsoProtocol::Saml => saml.get(&row.id).map(|saml| ProviderConfig::Saml {
                    idp_entity_id: saml.idp_entity_id.clone(),
                    sso_url: saml.sso_url.clone(),
                    certificates: saml.certificates.clone(),
                    attributes: saml.attributes.clone(),
                    display_name: row.display_name.clone(),
                    jit_role: row.jit_role,
                }),
            };
            let Some(config) = config else {
                unreadable += 1;
                tracing::warn!(org_id = %row.org_id, "org identity provider skipped");
                continue;
            };
            if let Some(existing) = current.providers.get(&row.org_id)
                && existing.config == config
            {
                providers.insert(row.org_id, Arc::clone(existing));
                continue;
            }
            let kind = self.build_kind(&row, &config, secrets);
            match kind {
                Some(kind) => {
                    providers.insert(
                        row.org_id,
                        Arc::new(OrgProvider {
                            kind,
                            jit_role: row.jit_role,
                            config,
                        }),
                    );
                }
                None => {
                    unreadable += 1;
                    tracing::warn!(org_id = %row.org_id, "org identity provider skipped");
                }
            }
        }

        let index = Index {
            domains,
            flags,
            providers,
        };
        *self.index.write().unwrap_or_else(PoisonError::into_inner) = Arc::new(index);
        Ok(Reloaded {
            claimed,
            unreadable,
        })
    }

    /// The route of `email`'s domain (the part after the last `@`), if any.
    pub(crate) fn route(&self, email: &str) -> Option<DomainRoute> {
        email
            .rsplit_once('@')
            .and_then(|(_, domain)| self.route_domain(domain))
    }

    /// The route of `domain`, if it is verified and routed.
    pub(crate) fn route_domain(&self, domain: &str) -> Option<DomainRoute> {
        self.snapshot().domains.get(domain).copied()
    }

    /// The org's built provider, when it has one in the index.
    pub(crate) fn provider(&self, org: OrgId) -> Option<Arc<OrgProvider>> {
        self.snapshot().providers.get(&org).cloned()
    }

    /// The org's provider flags, when it has a provider in the index.
    pub(crate) fn flags(&self, org: OrgId) -> Option<ProviderFlags> {
        self.snapshot().flags.get(&org).copied()
    }

    /// The org's verified, routed domains, sorted.
    pub(crate) fn domains_of(&self, org: OrgId) -> Vec<String> {
        let index = self.snapshot();
        let mut domains: Vec<String> = index
            .domains
            .iter()
            .filter(|(_, route)| route.org_id == org)
            .map(|(domain, _)| domain.clone())
            .collect();
        domains.sort();
        domains
    }

    /// Whether any routed domain has an enabled provider (`AuthConfig.sso_discovery`).
    pub(crate) fn discovery_enabled(&self) -> bool {
        self.snapshot().domains.values().any(|route| route.enabled)
    }

    /// Builds the protocol part of a provider from its stored configuration. `None` when a
    /// secret cannot be opened, an issuer is refused, or the metadata is unusable.
    fn build_kind(
        &self,
        row: &store::ProviderRow,
        config: &ProviderConfig,
        secrets: &SecretBox,
    ) -> Option<ProviderKind> {
        match config {
            ProviderConfig::Oidc {
                issuer,
                client_id,
                client_secret_enc,
                display_name,
                ..
            } => {
                let secret = secrets
                    .open(client_secret_enc, store::secret_aad(&row.id))
                    .ok()
                    .and_then(|bytes| String::from_utf8(bytes).ok())?;
                self.build_provider(
                    row.org_id,
                    display_name,
                    issuer,
                    client_id,
                    Secret::new(secret),
                )
                .ok()
                .map(ProviderKind::Oidc)
            }
            ProviderConfig::Saml {
                idp_entity_id,
                sso_url,
                certificates,
                attributes,
                ..
            } => {
                let certificates = certificates
                    .iter()
                    .map(|der| cetra_saml::IdpCertificate::from_der(der))
                    .collect::<Result<Vec<_>, _>>()
                    .ok()?;
                let metadata =
                    cetra_saml::IdpMetadata::from_fields(idp_entity_id, sso_url, certificates)
                        .ok()?;
                Some(ProviderKind::Saml(SamlProvider {
                    metadata,
                    attributes: attribute_names(attributes),
                }))
            }
        }
    }

    /// The egress policy of the instance's identity provider client.
    pub(crate) fn policy(&self) -> cetra_eval::EgressPolicy {
        self.policy
    }

    /// Runs `f` on the pending SAML table. A poisoned lock is recovered, as `index` is.
    pub(crate) fn with_pending<T>(
        &self,
        f: impl FnOnce(&mut saml_pending::PendingTable) -> T,
    ) -> T {
        let mut table = self
            .saml_pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        f(&mut table)
    }

    /// A provider built from settings (the save-time discovery check of P7 and `reload`).
    ///
    /// # Errors
    ///
    /// Returns [`crate::auth::oidc::OidcError`] when the issuer is not usable.
    pub(crate) fn build_provider(
        &self,
        org: OrgId,
        display_name: &str,
        issuer: &str,
        client_id: &str,
        secret: Secret<String>,
    ) -> Result<OidcProvider, crate::auth::oidc::OidcError> {
        OidcProvider::for_org(
            org,
            display_name,
            issuer,
            client_id,
            secret,
            self.http.clone(),
        )
    }

    /// Fetches IdP metadata from `url` through the guarded client (D-SA-11, D-SA-19).
    ///
    /// # Errors
    ///
    /// Returns [`MetadataFetchError`] when the URL is refused, the fetch fails or the answer
    /// is not `200`.
    pub(crate) async fn fetch_metadata(&self, url: &str) -> Result<Vec<u8>, MetadataFetchError> {
        if crate::config::issuer_problem(url).is_some() {
            return Err(MetadataFetchError::Url);
        }
        url::Url::parse(url).map_err(|_| MetadataFetchError::Url)?;
        let request = oauth2::http::Request::builder()
            .method("GET")
            .uri(url)
            .header(
                "accept",
                "application/samlmetadata+xml, application/xml, text/xml",
            )
            .body(Vec::new())
            .map_err(|_| MetadataFetchError::Url)?;
        let response = self.http.send(request).await.map_err(|error| match error {
            IdpHttpError::Denied => MetadataFetchError::Denied,
            IdpHttpError::TooLarge => MetadataFetchError::TooLarge,
            IdpHttpError::Request => MetadataFetchError::Unavailable,
        })?;
        if response.status() != oauth2::http::StatusCode::OK {
            return Err(MetadataFetchError::Status);
        }
        Ok(response.into_body())
    }

    fn snapshot(&self) -> Arc<Index> {
        Arc::clone(&self.index.read().unwrap_or_else(PoisonError::into_inner))
    }
}

/// Why [`OrgSso::fetch_metadata`] failed. No variant holds the URL or a body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MetadataFetchError {
    /// The URL is not usable as an IdP metadata address.
    Url,
    /// The egress policy refuses the address.
    Denied,
    /// The request could not be sent or read.
    Unavailable,
    /// The answer is not `200`.
    Status,
    /// The body passed the cap.
    TooLarge,
}

/// A domain as stored: trimmed, lowercase ASCII, one trailing dot removed; at most 253
/// characters; at least two labels; each label 1–63 of `[a-z0-9-]`, not starting or ending
/// with `-`; the last label not all digits (refuses IPv4). `None` otherwise.
pub(crate) fn normalize_domain(raw: &str) -> Option<String> {
    let lower = raw.trim().to_ascii_lowercase();
    let domain = lower.strip_suffix('.').unwrap_or(&lower);
    if domain.is_empty() || domain.len() > 253 || !domain.is_ascii() {
        return None;
    }
    let labels: Vec<&str> = domain.split('.').collect();
    if labels.len() < 2 {
        return None;
    }
    for label in &labels {
        if label.is_empty()
            || label.len() > 63
            || label.starts_with('-')
            || label.ends_with('-')
            || !label
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        {
            return None;
        }
    }
    let last = labels[labels.len() - 1];
    if last.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    Some(domain.to_owned())
}

/// Whether instance config owns `domain` (D-P-54): `enforce_sso_domains`, Google's
/// `allowed_domains` or any `[[auth.oidc]].allowed_domains`.
pub(crate) fn instance_claims(auth: &AuthSettings, domain: &str) -> bool {
    auth.enforce_sso_domains
        .iter()
        .any(|listed| listed == domain)
        || auth
            .google
            .as_ref()
            .is_some_and(|google| google.allowed_domains.iter().any(|listed| listed == domain))
        || auth.oidc.iter().any(|provider| {
            provider
                .allowed_domains
                .iter()
                .any(|listed| listed == domain)
        })
}

/// Whether `email` must sign in through its org's provider (D-P-58). The caller exempts
/// instance admins.
pub(crate) fn enforces(state: &AppState, email: &str) -> bool {
    state
        .org_sso
        .route(email)
        .is_some_and(|route| route.enforced)
}

/// Reloads and logs failures (`warn`, error class only). Called after every org SSO write.
pub(crate) async fn refresh(state: &AppState) {
    if let Err(error) = state
        .org_sso
        .reload(&state.meta, &state.secrets, &state.config.auth)
        .await
    {
        tracing::warn!(error = %error, "org SSO reload failed");
    }
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use cetra_saml::test_idp::TestKey;

    use super::*;
    use crate::config::OidcAuth;
    use crate::meta::orgs::insert_org;
    use crate::test_support::{TestApp, capture_logs, configure_org_saml};

    const ISSUER: &str = "https://idp.example";

    async fn add_org(app: &TestApp, slug: &'static str) -> OrgId {
        let org = OrgId::new_v7();
        let now = app.state.clock.now_ns();
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    insert_org(conn, org, slug, slug, now).await?;
                    Ok(())
                })
            })
            .await
            .unwrap();
        org
    }

    /// A provider with a sealed secret of `test-secret`, disabled and not enforced.
    async fn create_provider(app: &TestApp, org: OrgId, display_name: &str) -> Uuid {
        let id = Uuid::now_v7();
        let sealed = app
            .state
            .secrets
            .seal(b"test-secret", store::secret_aad(&id))
            .expect("seal");
        let now = app.state.clock.now_ns();
        let display_name = display_name.to_owned();
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    let write = store::ProviderWrite {
                        display_name,
                        issuer: ISSUER.to_owned(),
                        client_id: "acme-client".to_owned(),
                        client_secret_enc: Some(sealed),
                        jit_role: JitRole::Viewer,
                    };
                    store::put_provider(conn, org, id, write, now).await?;
                    Ok(())
                })
            })
            .await
            .unwrap();
        id
    }

    /// Changes the display name only: the secret stays as it is.
    async fn rename_provider(app: &TestApp, org: OrgId, display_name: &str) {
        let now = app.state.clock.now_ns();
        let display_name = display_name.to_owned();
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    let write = store::ProviderWrite {
                        display_name,
                        issuer: ISSUER.to_owned(),
                        client_id: "acme-client".to_owned(),
                        client_secret_enc: None,
                        jit_role: JitRole::Viewer,
                    };
                    store::put_provider(conn, org, Uuid::now_v7(), write, now).await?;
                    Ok(())
                })
            })
            .await
            .unwrap();
    }

    async fn set_flags(app: &TestApp, org: OrgId, enabled: bool, enforced: bool) {
        let now = app.state.clock.now_ns();
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    store::set_flags(conn, org, enabled, enforced, now).await?;
                    Ok(())
                })
            })
            .await
            .unwrap();
    }

    /// A domain row, verified now when `verified`.
    async fn add_domain(app: &TestApp, org: OrgId, domain: &'static str, verified: bool) {
        let now = app.state.clock.now_ns();
        let row = store::DomainRow {
            id: Uuid::now_v7(),
            org_id: org,
            domain: domain.to_owned(),
            token: dns::new_token().expect("token"),
            verified_at: None,
            last_checked_at: None,
            created_at: now,
        };
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    let id = row.id;
                    store::insert_domain(conn, &row).await?;
                    if verified {
                        store::mark_verified(conn, id, now).await?;
                    }
                    Ok(())
                })
            })
            .await
            .unwrap();
    }

    async fn reload(app: &TestApp) -> Reloaded {
        app.state
            .org_sso
            .reload(&app.state.meta, &app.state.secrets, &app.state.config.auth)
            .await
            .expect("reload")
    }

    #[tokio::test]
    async fn reload_routes_verified_domains() {
        let app = TestApp::new().await;
        let org = add_org(&app, "acme").await;
        create_provider(&app, org, "Acme SSO").await;
        add_domain(&app, org, "acme.test", true).await;
        add_domain(&app, org, "beta.test", false).await;
        reload(&app).await;

        assert_eq!(
            app.state.org_sso.route("ada@acme.test"),
            Some(DomainRoute {
                org_id: org,
                enabled: false,
                enforced: false,
            })
        );
        assert_eq!(app.state.org_sso.route("x@beta.test"), None);
        assert!(!app.state.org_sso.discovery_enabled());
        assert_eq!(
            app.state.org_sso.domains_of(org),
            vec!["acme.test".to_owned()]
        );

        set_flags(&app, org, true, false).await;
        reload(&app).await;
        assert!(app.state.org_sso.discovery_enabled());
        assert_eq!(
            app.state
                .org_sso
                .route("ada@acme.test")
                .map(|route| route.enabled),
            Some(true)
        );
    }

    #[tokio::test]
    async fn claimed_domains_are_ignored() {
        let app = TestApp::with_config(|config| {
            config.auth.enforce_sso_domains = vec!["acme.test".to_owned()];
        })
        .await;
        let org = add_org(&app, "acme").await;
        create_provider(&app, org, "Acme SSO").await;
        add_domain(&app, org, "acme.test", true).await;
        assert_eq!(reload(&app).await.claimed, 1);
        assert_eq!(app.state.org_sso.route("ada@acme.test"), None);

        let app = TestApp::with_config(|config| {
            config.auth.oidc.push(OidcAuth {
                name: "okta".to_owned(),
                display_name: "Okta".to_owned(),
                issuer: "https://acme.okta.com".to_owned(),
                client_id: "o".to_owned(),
                client_secret: Secret::new("o-secret".to_owned()),
                scopes: vec!["openid".to_owned()],
                allowed_domains: vec!["acme.test".to_owned()],
                auto_provision: None,
                link_existing_by_email: false,
            });
        })
        .await;
        let org = add_org(&app, "acme").await;
        create_provider(&app, org, "Acme SSO").await;
        add_domain(&app, org, "acme.test", true).await;
        assert_eq!(reload(&app).await.claimed, 1);
        assert_eq!(app.state.org_sso.route("ada@acme.test"), None);
    }

    #[tokio::test]
    async fn unchanged_providers_keep_their_metadata() {
        let app = TestApp::new().await;
        let org = add_org(&app, "acme").await;
        create_provider(&app, org, "Acme SSO").await;
        reload(&app).await;
        let first = app.state.org_sso.provider(org).expect("provider");

        reload(&app).await;
        let second = app.state.org_sso.provider(org).expect("provider");
        assert!(Arc::ptr_eq(&first, &second));

        set_flags(&app, org, true, true).await;
        reload(&app).await;
        let third = app.state.org_sso.provider(org).expect("provider");
        assert!(Arc::ptr_eq(&first, &third), "flags are not configuration");
        assert_eq!(
            app.state.org_sso.flags(org),
            Some(ProviderFlags {
                enabled: true,
                enforced: true,
            })
        );

        rename_provider(&app, org, "Renamed SSO").await;
        reload(&app).await;
        let renamed = app.state.org_sso.provider(org).expect("provider");
        assert!(!Arc::ptr_eq(&first, &renamed));
        match &renamed.kind {
            ProviderKind::Oidc(oidc) => assert_eq!(oidc.display_name(), "Renamed SSO"),
            ProviderKind::Saml(_) => panic!("an OIDC provider was rebuilt as SAML"),
        }
        assert_eq!(renamed.jit_role, JitRole::Viewer);
    }

    #[tokio::test]
    async fn unchanged_saml_providers_keep_their_arc() {
        let app = TestApp::new().await;
        let org = add_org(&app, "acme").await;
        configure_org_saml(&app, org, &[TestKey::Idp], "acme.test", true, false).await;
        let first = app.state.org_sso.provider(org).expect("provider");
        assert!(matches!(first.kind, ProviderKind::Saml(_)));

        reload(&app).await;
        let second = app.state.org_sso.provider(org).expect("provider");
        assert!(Arc::ptr_eq(&first, &second));

        set_flags(&app, org, false, false).await;
        reload(&app).await;
        let third = app.state.org_sso.provider(org).expect("provider");
        assert!(Arc::ptr_eq(&first, &third), "flags are not configuration");
    }

    #[tokio::test]
    async fn a_saml_provider_with_an_unreadable_certificate_is_skipped() {
        let app = TestApp::new().await;
        let org = add_org(&app, "acme").await;
        configure_org_saml(&app, org, &[TestKey::Idp], "acme.test", true, false).await;
        assert!(app.state.org_sso.provider(org).is_some());

        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    sqlx::query("UPDATE org_sso_saml_certificates SET der = X'00'")
                        .execute(&mut *conn)
                        .await?;
                    Ok(())
                })
            })
            .await
            .unwrap();
        let (result, logs) = capture_logs(app.state.org_sso.reload(
            &app.state.meta,
            &app.state.secrets,
            &app.state.config.auth,
        ))
        .await;
        assert_eq!(result.expect("reload").unreadable, 1);
        assert!(app.state.org_sso.provider(org).is_none());
        assert!(logs.contains(&org.to_string()), "{logs}");
    }

    #[test]
    fn sp_urls_follow_the_public_url() {
        let public_url: crate::config::PublicUrl =
            serde_json::from_str("\"https://cetra.example\"").expect("url");
        let org = OrgId::new_v7();
        let urls = sp_urls(&public_url, org);
        assert_eq!(urls.saml_path, format!("/api/auth/saml/{org}"));
        assert_eq!(urls.acs_path, format!("{}/acs", urls.saml_path));
        assert_eq!(urls.start_path, format!("{}/start", urls.saml_path));
        assert_eq!(
            urls.entity_id,
            format!("https://cetra.example/api/auth/saml/{org}")
        );
        assert_eq!(urls.acs_url, format!("{}/acs", urls.entity_id));
        assert_eq!(urls.start_url, format!("{}/start", urls.entity_id));
    }

    #[test]
    fn domains_are_normalized_and_refused_when_malformed() {
        assert_eq!(normalize_domain("Acme.COM.").as_deref(), Some("acme.com"));
        assert_eq!(
            normalize_domain("xn--bcher-kva.example").as_deref(),
            Some("xn--bcher-kva.example")
        );
        for refused in [
            "",
            "acme",
            "-acme.com",
            "acme-.com",
            "a..com",
            "1.2.3.4",
            "acme.com/x",
            "ädme.com",
        ] {
            assert_eq!(normalize_domain(refused), None, "{refused}");
        }
        let long_label = format!("{}.com", "a".repeat(64));
        assert_eq!(normalize_domain(&long_label), None);
        let long_domain = format!("{}.com", vec!["a".repeat(63); 3].join("."));
        assert!(normalize_domain(&long_domain).is_some());
        let too_long = format!("{}.{}", "a".repeat(63), vec!["b".repeat(63); 3].join("."));
        assert!(too_long.len() > 253);
        assert_eq!(normalize_domain(&too_long), None);
    }

    #[tokio::test]
    async fn the_fixed_dns_answers_through_the_test_app() {
        let app = TestApp::new().await;
        app.dns_txt("acme.test", &["cetra-verify=0123"]);
        assert_eq!(
            app.state.org_sso.dns.txt("acme.test").await.expect("txt"),
            vec!["cetra-verify=0123".to_owned()]
        );
        app.dns_fail(true);
        assert!(app.state.org_sso.dns.txt("acme.test").await.is_err());
        app.dns_fail(false);
        assert_eq!(
            app.state
                .org_sso
                .dns
                .txt("acme.test")
                .await
                .expect("txt")
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn an_unreadable_secret_is_skipped() {
        let app = TestApp::new().await;
        let org = add_org(&app, "acme").await;
        create_provider(&app, org, "Acme SSO").await;
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    sqlx::query(
                        "UPDATE org_sso_providers SET client_secret_enc = X'00' WHERE org_id = ?",
                    )
                    .bind(org.to_string())
                    .execute(&mut *conn)
                    .await?;
                    Ok(())
                })
            })
            .await
            .unwrap();

        let (result, logs) = capture_logs(app.state.org_sso.reload(
            &app.state.meta,
            &app.state.secrets,
            &app.state.config.auth,
        ))
        .await;
        assert_eq!(result.expect("reload").unreadable, 1);
        assert!(app.state.org_sso.provider(org).is_none());
        assert!(logs.contains(&org.to_string()), "{logs}");
        assert!(!logs.contains("test-secret"), "{logs}");
    }

    #[tokio::test]
    async fn enforces_follows_the_flag() {
        let app = TestApp::new().await;
        let org = add_org(&app, "acme").await;
        create_provider(&app, org, "Acme SSO").await;
        add_domain(&app, org, "acme.test", true).await;
        reload(&app).await;
        assert!(!enforces(&app.state, "ada@acme.test"));

        set_flags(&app, org, true, true).await;
        reload(&app).await;
        assert!(enforces(&app.state, "ada@acme.test"));
        assert!(!enforces(&app.state, "ada@other.test"));
    }
}
