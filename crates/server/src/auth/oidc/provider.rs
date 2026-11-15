//! Google and generic OIDC providers. Metadata is fetched lazily and cached.
//!
//! Google and a generic issuer share one code exchange. The issuer is not
//! rewritten. `hd` is read from the id_token payload after the signature check.
#![cfg_attr(not(test), allow(dead_code))]

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use llmobs_core::ids::OrgId;
use oauth2::{
    AuthorizationCode, ClientId, ClientSecret, EndpointMaybeSet, EndpointNotSet, EndpointSet,
    PkceCodeChallenge, PkceCodeVerifier, RedirectUrl, RequestTokenError, Scope,
};
use openidconnect::core::{CoreAuthenticationFlow, CoreClient, CoreIdToken, CoreProviderMetadata};
use openidconnect::{
    ClaimsVerificationError, CsrfToken, IssuerUrl, Nonce, SignatureVerificationError, TokenResponse,
};
use tokio::sync::RwLock;

use super::{ExchangeError, LoginClaims, OidcError};
use crate::config::{AutoProvision as ConfigProvision, GoogleAuth, OidcAuth, Secret};

/// Response bodies from an identity provider are capped here (D-P-53).
const MAX_IDP_BODY: usize = 1024 * 1024;

/// The HTTP client of an identity provider (D-P-53). `policy: Some` (an org's provider)
/// refuses a URL that fails `allows_url` before sending: reqwest does not call the guarded
/// resolver for IP-literal hosts. `policy: None` (operator providers from `[auth]`) sends as
/// before. Bodies are capped at 1 MiB.
#[derive(Debug, Clone)]
pub(crate) struct IdpHttp {
    client: reqwest::Client,
    policy: Option<llmobs_eval::EgressPolicy>,
}

/// Why an identity-provider request failed. No variant holds the URL or a body.
#[derive(Debug, thiserror::Error)]
pub(crate) enum IdpHttpError {
    /// The address is not allowed by the org's egress policy (D166, D-P-53).
    #[error("identity provider address refused")]
    Denied,
    /// The response body passed the cap.
    #[error("identity provider response too large")]
    TooLarge,
    /// The request could not be sent or read.
    #[error("identity provider request failed")]
    Request,
}

impl IdpHttp {
    /// The client of an operator provider (`[[auth.oidc]]`, Google): no address check.
    pub(crate) fn operator(client: reqwest::Client) -> Self {
        Self {
            client,
            policy: None,
        }
    }

    /// The client of an org's provider: every request is checked against `policy` first.
    /// `client` must come from [`llmobs_eval::identity_http_client`] with the same policy.
    pub(crate) fn guarded(client: reqwest::Client, policy: llmobs_eval::EgressPolicy) -> Self {
        Self {
            client,
            policy: Some(policy),
        }
    }

    /// One request: the policy check, then the client, then the capped body.
    ///
    /// # Errors
    ///
    /// Returns [`IdpHttpError::Denied`] before sending when the policy refuses the URL,
    /// [`IdpHttpError::TooLarge`] past the body cap, and [`IdpHttpError::Request`] for any
    /// other failure.
    pub(crate) async fn send(
        &self,
        request: oauth2::HttpRequest,
    ) -> Result<oauth2::HttpResponse, IdpHttpError> {
        let (parts, body) = request.into_parts();
        let url = reqwest::Url::parse(&parts.uri.to_string()).map_err(|_| IdpHttpError::Request)?;
        if self.policy.is_some_and(|policy| !policy.allows_url(&url)) {
            return Err(IdpHttpError::Denied);
        }
        let mut response = self
            .client
            .request(parts.method, url)
            .headers(parts.headers)
            .body(body)
            .send()
            .await
            .map_err(|_| IdpHttpError::Request)?;
        let status = response.status();
        let headers = response.headers().clone();
        let mut collected = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| IdpHttpError::Request)? {
            if collected.len().saturating_add(chunk.len()) > MAX_IDP_BODY {
                return Err(IdpHttpError::TooLarge);
            }
            collected.extend_from_slice(&chunk);
        }
        let mut out = oauth2::http::Response::new(collected);
        *out.status_mut() = status;
        *out.headers_mut() = headers;
        Ok(out)
    }
}

/// How the issuer and the hosted-domain claim are interpreted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OidcKind {
    /// Issuer is fixed to `https://accounts.google.com`.
    Google,
    /// Issuer comes from `[[auth.oidc]]`.
    Generic,
}

/// Org and role created for a new user when the provider says so.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AutoProvision {
    /// Org slug. Existence is checked at login, not at startup.
    pub(crate) org: String,
    /// Never [`ProvisionRole`]'s owner. The config check rejects `owner`.
    pub(crate) role: ProvisionRole,
}

/// `auto_provision.role`. Owner is not a value here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProvisionRole {
    /// Org admin.
    Admin,
    /// Org member.
    Member,
    /// Org viewer.
    Viewer,
}

/// One OIDC client, Google or generic.
#[derive(Debug)]
pub(crate) struct OidcProvider {
    id: String,
    kind: OidcKind,
    display_name: String,
    issuer: IssuerUrl,
    client_id: ClientId,
    client_secret: Secret<String>,
    scopes: Vec<String>,
    allowed_domains: Vec<String>,
    auto_provision: Option<AutoProvision>,
    link_existing_by_email: bool,
    http: IdpHttp,
    metadata: RwLock<Option<(CoreProviderMetadata, i64)>>,
}

impl OidcProvider {
    pub(crate) fn from_google(
        settings: &GoogleAuth,
        http: reqwest::Client,
    ) -> Result<Self, OidcError> {
        Self::new(
            OidcKind::Google,
            "google".to_owned(),
            "Google".to_owned(),
            "https://accounts.google.com",
            &settings.client_id,
            settings.client_secret.clone(),
            default_scopes(),
            settings.allowed_domains.clone(),
            None,
            settings.link_existing_by_email,
            IdpHttp::operator(http),
        )
    }

    pub(crate) fn from_generic(
        settings: &OidcAuth,
        http: reqwest::Client,
    ) -> Result<Self, OidcError> {
        Self::new(
            OidcKind::Generic,
            format!("oidc:{}", settings.name),
            settings.display_name.trim().to_owned(),
            &settings.issuer,
            &settings.client_id,
            settings.client_secret.clone(),
            settings.scopes.clone(),
            settings.allowed_domains.clone(),
            settings
                .auto_provision
                .as_ref()
                .map(provision)
                .transpose()?,
            settings.link_existing_by_email,
            IdpHttp::operator(http),
        )
    }

    /// An org's provider (D-P-52, D-P-53): id `org:<org_id>`, default scopes, no domain list,
    /// no auto-provision, no linking flag (the org flow decides). `http` must be
    /// `IdpHttp::guarded(llmobs_eval::identity_http_client(policy)?, policy)`.
    pub(crate) fn for_org(
        org_id: OrgId,
        display_name: &str,
        issuer: &str,
        client_id: &str,
        client_secret: Secret<String>,
        http: IdpHttp,
    ) -> Result<Self, OidcError> {
        Self::new(
            OidcKind::Generic,
            format!("org:{org_id}"),
            display_name.trim().to_owned(),
            issuer,
            client_id,
            client_secret,
            default_scopes(),
            Vec::new(),
            None,
            false,
            http,
        )
    }

    /// A generic provider aimed at the mock IdP.
    #[cfg(test)]
    pub(crate) fn for_test(issuer: &str, http: reqwest::Client) -> Result<Self, OidcError> {
        let secret = serde_json::from_str("\"test-secret\"").expect("secret");
        Self::new(
            OidcKind::Generic,
            "oidc:mock".to_owned(),
            "Mock".to_owned(),
            issuer,
            "test-client",
            secret,
            default_scopes(),
            Vec::new(),
            None,
            false,
            IdpHttp::operator(http),
        )
    }

    pub(crate) fn id(&self) -> &str {
        &self.id
    }

    pub(crate) fn display_name(&self) -> &str {
        &self.display_name
    }

    /// Verified-email domains. Empty means the provider does not restrict them.
    pub(crate) fn allowed_domains(&self) -> &[String] {
        &self.allowed_domains
    }

    /// Google checks `hd` as well as the email domain.
    pub(crate) fn is_google(&self) -> bool {
        self.kind == OidcKind::Google
    }

    /// Org membership added after a successful login.
    pub(crate) fn auto_provision(&self) -> Option<&AutoProvision> {
        self.auto_provision.as_ref()
    }

    /// Link a verified email onto an existing non-admin account.
    pub(crate) fn link_existing_by_email(&self) -> bool {
        self.link_existing_by_email
    }

    /// Points discovery at a mock issuer. The cache is still empty.
    #[cfg(test)]
    pub(crate) fn point_at(&mut self, issuer: &str) {
        // The mock IdP's issuer is a URL this process just bound.
        #[allow(clippy::expect_used)]
        let issuer = IssuerUrl::new(issuer.to_owned()).expect("mock issuer");
        self.issuer = issuer;
    }

    /// Discovery document, fetched on first use and when older than an hour.
    ///
    /// A failed refresh keeps the previous document. With nothing cached, the
    /// error is [`OidcError::Unavailable`] and the issuer is not included.
    ///
    /// `now` is nanoseconds since the Unix epoch. The provider does not read
    /// the clock, so tests choose the timestamp.
    pub(crate) async fn metadata(&self, now: i64) -> Result<CoreProviderMetadata, OidcError> {
        if let Some((cached, fetched)) = self.metadata.read().await.as_ref()
            && now.saturating_sub(*fetched) < super::METADATA_TTL_NS
        {
            return Ok(cached.clone());
        }
        let http = self.http.clone();
        let call = move |request: oauth2::HttpRequest| {
            let http = http.clone();
            async move { http.send(request).await }
        };
        match CoreProviderMetadata::discover_async(self.issuer.clone(), &call).await {
            Ok(fresh) => {
                *self.metadata.write().await = Some((fresh.clone(), now));
                Ok(fresh)
            }
            Err(_error) => {
                // The discovery error names the issuer and can quote the body.
                // Neither is logged or returned.
                tracing::warn!("identity provider discovery failed");
                self.metadata
                    .read()
                    .await
                    .as_ref()
                    .map(|(cached, _fetched)| cached.clone())
                    .ok_or(OidcError::Unavailable)
            }
        }
    }

    /// Fetches metadata and replaces the cache. A failure does not keep the
    /// previous document: this is the key-rotation retry.
    ///
    /// # Errors
    ///
    /// Returns [`OidcError::Unavailable`] when discovery fails. The issuer is
    /// not included.
    pub(crate) async fn refresh_metadata(
        &self,
        now: i64,
    ) -> Result<CoreProviderMetadata, OidcError> {
        let http = self.http.clone();
        let call = move |request: oauth2::HttpRequest| {
            let http = http.clone();
            async move { http.send(request).await }
        };
        match CoreProviderMetadata::discover_async(self.issuer.clone(), &call).await {
            Ok(fresh) => {
                *self.metadata.write().await = Some((fresh.clone(), now));
                Ok(fresh)
            }
            Err(_error) => {
                tracing::warn!("identity provider discovery failed");
                Err(OidcError::Unavailable)
            }
        }
    }

    /// Authorization URL and the PKCE verifier to store in the state cookie.
    ///
    /// # Errors
    ///
    /// Returns [`ExchangeError::Unavailable`] when metadata cannot be loaded.
    pub(crate) async fn authorization_url(
        &self,
        redirect_uri: &str,
        state: &str,
        nonce: &str,
        now: i64,
    ) -> Result<(String, String), ExchangeError> {
        let metadata = self.metadata(now).await.map_err(exchange_unavailable)?;
        let client = self.client(metadata, redirect_uri)?;
        let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
        let verifier = verifier.secret().clone();
        let state = state.to_owned();
        let nonce = nonce.to_owned();
        let mut request = client.authorize_url(
            CoreAuthenticationFlow::AuthorizationCode,
            move || CsrfToken::new(state),
            move || Nonce::new(nonce),
        );
        for scope in &self.scopes {
            if scope != "openid" {
                request = request.add_scope(Scope::new(scope.clone()));
            }
        }
        let (url, _csrf, _nonce) = request.set_pkce_challenge(challenge).url();
        Ok((url.to_string(), verifier))
    }

    /// Exchanges `code` and checks the id_token.
    ///
    /// On a missing signing key, metadata is fetched once and the same token is
    /// checked again. The code is not exchanged twice.
    ///
    /// # Errors
    ///
    /// Returns [`ExchangeError::Unavailable`] when the provider cannot be
    /// reached, and [`ExchangeError::Rejected`] when the token is refused or
    /// the claims do not verify.
    pub(crate) async fn exchange_code(
        &self,
        redirect_uri: &str,
        code: &str,
        verifier: &str,
        nonce: &str,
        now: i64,
    ) -> Result<LoginClaims, ExchangeError> {
        let metadata = self.metadata(now).await.map_err(exchange_unavailable)?;
        let client = self.client(metadata, redirect_uri)?;
        let http = self.http.clone();
        let call = move |request: oauth2::HttpRequest| {
            let http = http.clone();
            async move { http.send(request).await }
        };
        let token = client
            .exchange_code(AuthorizationCode::new(code.to_owned()))
            .map_err(|_| ExchangeError::Rejected)?
            .set_pkce_verifier(PkceCodeVerifier::new(verifier.to_owned()))
            .request_async(&call)
            .await
            .map_err(|error| token_error(&error))?;
        let Some(id_token) = token.id_token() else {
            return Err(ExchangeError::Rejected);
        };
        let nonce = Nonce::new(nonce.to_owned());
        let claims = match id_token.claims(&client.id_token_verifier(), &nonce) {
            Ok(claims) => claims,
            Err(error) if missing_key(&error) => {
                let metadata = self
                    .refresh_metadata(now)
                    .await
                    .map_err(exchange_unavailable)?;
                let client = self.client(metadata, redirect_uri)?;
                id_token
                    .claims(&client.id_token_verifier(), &nonce)
                    .map_err(|_| ExchangeError::Rejected)?
            }
            Err(_error) => return Err(ExchangeError::Rejected),
        };
        let subject = claims.subject().to_string();
        if subject.is_empty() {
            return Err(ExchangeError::Rejected);
        }
        Ok(LoginClaims {
            subject,
            email: claims.email().map(|email| email.to_string()),
            email_verified: claims.email_verified(),
            hd: hosted_domain(id_token),
            name: claims
                .name()
                .and_then(|localized| localized.get(None))
                .map(|name| name.to_string()),
        })
    }

    fn client(
        &self,
        metadata: CoreProviderMetadata,
        redirect_uri: &str,
    ) -> Result<ReadyClient, ExchangeError> {
        let redirect =
            RedirectUrl::new(redirect_uri.to_owned()).map_err(|_| ExchangeError::Rejected)?;
        Ok(CoreClient::from_provider_metadata(
            metadata,
            self.client_id.clone(),
            Some(ClientSecret::new(self.client_secret.expose().clone())),
        )
        .set_redirect_uri(redirect))
    }

    #[allow(clippy::too_many_arguments)]
    fn new(
        kind: OidcKind,
        id: String,
        display_name: String,
        issuer: &str,
        client_id: &str,
        client_secret: Secret<String>,
        scopes: Vec<String>,
        allowed_domains: Vec<String>,
        auto_provision: Option<AutoProvision>,
        link_existing_by_email: bool,
        http: IdpHttp,
    ) -> Result<Self, OidcError> {
        let issuer = IssuerUrl::new(issuer.to_owned()).map_err(|_| OidcError::Invalid)?;
        Ok(Self {
            id,
            kind,
            display_name,
            issuer,
            client_id: ClientId::new(client_id.to_owned()),
            client_secret,
            scopes,
            allowed_domains,
            auto_provision,
            link_existing_by_email,
            http,
            metadata: RwLock::new(None),
        })
    }
}

type ReadyClient = CoreClient<
    EndpointSet,
    EndpointNotSet,
    EndpointNotSet,
    EndpointNotSet,
    EndpointMaybeSet,
    EndpointMaybeSet,
>;

fn exchange_unavailable(error: OidcError) -> ExchangeError {
    match error {
        OidcError::Unavailable => ExchangeError::Unavailable,
        OidcError::Client | OidcError::Invalid => ExchangeError::Rejected,
    }
}

fn missing_key(error: &ClaimsVerificationError) -> bool {
    matches!(
        error,
        ClaimsVerificationError::SignatureVerification(SignatureVerificationError::NoMatchingKey)
    )
}

fn token_error<E, T>(error: &RequestTokenError<E, T>) -> ExchangeError
where
    E: std::error::Error + 'static,
    T: oauth2::ErrorResponse + 'static,
{
    match error {
        RequestTokenError::Request(_) => ExchangeError::Unavailable,
        RequestTokenError::ServerResponse(_)
        | RequestTokenError::Parse(_, _)
        | RequestTokenError::Other(_) => ExchangeError::Rejected,
    }
}

/// `hd` is not a standard claim, so it is read from the verified compact JWT.
///
/// The string is the id_token. It is not logged.
fn hosted_domain(id_token: &CoreIdToken) -> Option<String> {
    let compact = id_token.to_string();
    let payload = compact.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD.decode(payload).ok()?;
    let claims: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    claims
        .get("hd")
        .and_then(serde_json::Value::as_str)
        .filter(|domain| !domain.is_empty())
        .map(str::to_owned)
}

fn default_scopes() -> Vec<String> {
    ["openid", "email", "profile"]
        .into_iter()
        .map(str::to_owned)
        .collect()
}

fn provision(value: &ConfigProvision) -> Result<AutoProvision, OidcError> {
    let role = match value.role.as_str() {
        "admin" => ProvisionRole::Admin,
        "member" => ProvisionRole::Member,
        "viewer" => ProvisionRole::Viewer,
        _ => return Err(OidcError::Invalid),
    };
    Ok(AutoProvision {
        org: value.org.clone(),
        role,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::oidc::{METADATA_TTL_NS, identity_client};
    use crate::config::{AutoProvision as ConfigProvision, GoogleAuth, OidcAuth};
    use crate::test_support::mock_idp::MockIdp;

    fn secret(value: &str) -> Secret<String> {
        serde_json::from_str(&format!("\"{value}\"")).expect("secret")
    }

    #[test]
    fn google_and_generic_keep_their_settings() {
        let http = identity_client().expect("client");
        let google = OidcProvider::from_google(
            &GoogleAuth {
                client_id: "g".to_owned(),
                client_secret: secret("g-secret"),
                allowed_domains: vec!["acme.test".to_owned()],
                link_existing_by_email: true,
            },
            http.clone(),
        )
        .expect("google");
        assert_eq!(google.id, "google");
        assert_eq!(google.kind, OidcKind::Google);
        assert_eq!(google.display_name, "Google");
        assert_eq!(google.issuer.as_str(), "https://accounts.google.com");
        assert_eq!(google.client_id.as_str(), "g");
        assert_eq!(google.client_secret.expose(), "g-secret");
        assert_eq!(google.scopes, default_scopes());
        assert_eq!(google.allowed_domains, vec!["acme.test".to_owned()]);
        assert!(google.auto_provision.is_none());
        assert!(google.link_existing_by_email);
        assert!(!format!("{google:?}").contains("g-secret"));

        let generic = OidcProvider::from_generic(
            &OidcAuth {
                name: "okta".to_owned(),
                display_name: " Okta ".to_owned(),
                issuer: "https://acme.okta.com".to_owned(),
                client_id: "o".to_owned(),
                client_secret: secret("o-secret"),
                scopes: vec!["openid".to_owned()],
                allowed_domains: vec!["okta.test".to_owned()],
                auto_provision: Some(ConfigProvision {
                    org: "acme".to_owned(),
                    role: "viewer".to_owned(),
                }),
                link_existing_by_email: false,
            },
            http,
        )
        .expect("generic");
        assert_eq!(generic.id, "oidc:okta");
        assert_eq!(generic.kind, OidcKind::Generic);
        assert_eq!(generic.display_name, "Okta");
        assert_eq!(generic.client_secret.expose(), "o-secret");
        assert_eq!(
            generic.auto_provision,
            Some(AutoProvision {
                org: "acme".to_owned(),
                role: ProvisionRole::Viewer,
            })
        );
        assert!(!generic.link_existing_by_email);
        assert!(!format!("{generic:?}").contains("o-secret"));
    }

    #[tokio::test]
    async fn metadata_is_cached_then_refreshed() {
        let idp = MockIdp::start().await;
        let provider = OidcProvider::for_test(idp.issuer(), identity_client().expect("client"))
            .expect("provider");
        assert_eq!(idp.discovery_hits(), 0);
        let first = provider.metadata(0).await.expect("first");
        assert_eq!(idp.discovery_hits(), 1);
        assert_eq!(first.issuer().as_str(), idp.issuer());
        let cached = provider
            .metadata(METADATA_TTL_NS - 1)
            .await
            .expect("cached");
        assert_eq!(idp.discovery_hits(), 1);
        assert_eq!(cached.issuer().as_str(), first.issuer().as_str());
        let refreshed = provider.metadata(METADATA_TTL_NS).await.expect("refresh");
        assert_eq!(idp.discovery_hits(), 2);
        assert_eq!(refreshed.issuer().as_str(), idp.issuer());
    }

    #[tokio::test]
    async fn stale_metadata_is_kept_when_discovery_fails() {
        let idp = MockIdp::start().await;
        let provider = OidcProvider::for_test(idp.issuer(), identity_client().expect("client"))
            .expect("provider");
        let first = provider.metadata(0).await.expect("first");
        idp.fail_discovery();
        let stale = provider.metadata(METADATA_TTL_NS).await.expect("stale");
        assert_eq!(stale.issuer().as_str(), first.issuer().as_str());
        assert!(idp.discovery_hits() >= 2);

        let fresh = OidcProvider::for_test(idp.issuer(), identity_client().expect("client"))
            .expect("fresh");
        let error = fresh.metadata(0).await.expect_err("unavailable");
        assert!(matches!(error, OidcError::Unavailable));
        let text = error.to_string();
        assert_eq!(text, "identity provider metadata is unavailable");
        assert!(!text.contains("127.0.0.1"), "{text}");
    }

    fn guarded(allow_private_network: bool) -> IdpHttp {
        let policy = llmobs_eval::EgressPolicy {
            allow_private_network,
        };
        IdpHttp::guarded(
            llmobs_eval::identity_http_client(policy).expect("client"),
            policy,
        )
    }

    fn org_provider(issuer: &str, http: IdpHttp) -> OidcProvider {
        OidcProvider::for_org(
            OrgId::new_v7(),
            " Acme SSO ",
            issuer,
            "acme-client",
            secret("acme-secret"),
            http,
        )
        .expect("org provider")
    }

    #[test]
    fn for_org_names_the_provider_by_org_and_keeps_the_secret_out_of_debug() {
        let org = OrgId::new_v7();
        let provider = OidcProvider::for_org(
            org,
            " Acme SSO ",
            "https://idp.example",
            "acme-client",
            secret("acme-secret"),
            guarded(false),
        )
        .expect("org provider");
        assert_eq!(provider.id(), format!("org:{org}"));
        assert_eq!(provider.display_name(), "Acme SSO");
        assert_eq!(provider.kind, OidcKind::Generic);
        assert!(provider.allowed_domains().is_empty());
        assert!(provider.auto_provision().is_none());
        assert!(!provider.link_existing_by_email());
        assert!(!format!("{provider:?}").contains("acme-secret"));
    }

    #[tokio::test]
    async fn guarded_metadata_loads_when_private_egress_is_allowed() {
        let idp = MockIdp::start().await;
        let provider = org_provider(idp.issuer(), guarded(true));
        let metadata = provider.metadata(0).await.expect("metadata");
        assert_eq!(metadata.issuer().as_str(), idp.issuer());
        assert_eq!(idp.discovery_hits(), 1);
    }

    #[tokio::test]
    async fn guarded_literal_ip_is_refused_before_sending() {
        let idp = MockIdp::start().await;
        let provider = org_provider(idp.issuer(), guarded(false));
        let error = provider.metadata(0).await.expect_err("literal loopback");
        assert!(matches!(error, OidcError::Unavailable));
        assert_eq!(idp.discovery_hits(), 0);
    }

    #[tokio::test]
    async fn guarded_host_name_is_refused_by_the_resolver() {
        let idp = MockIdp::start().await;
        let mut provider = org_provider(idp.issuer(), guarded(false));
        let localhost = idp.issuer().replace("127.0.0.1", "localhost");
        provider.point_at(&localhost);
        let error = provider.metadata(0).await.expect_err("localhost");
        assert!(matches!(error, OidcError::Unavailable));
        assert_eq!(idp.discovery_hits(), 0);
    }

    #[tokio::test]
    async fn send_refuses_a_metadata_address() {
        let request = oauth2::http::Request::builder()
            .method(oauth2::http::Method::GET)
            .uri("http://169.254.169.254/latest")
            .body(Vec::new())
            .expect("request");
        let error = guarded(false)
            .send(request)
            .await
            .expect_err("metadata address");
        assert!(matches!(error, IdpHttpError::Denied));
        assert_eq!(error.to_string(), "identity provider address refused");
    }

    #[tokio::test]
    async fn send_caps_the_body() {
        use axum::Router;
        use axum::routing::get;

        let app = Router::new().route("/big", get(|| async { vec![0_u8; 2 * 1024 * 1024] }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let port = listener.local_addr().expect("addr").port();
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve");
        });

        let request = oauth2::http::Request::builder()
            .method(oauth2::http::Method::GET)
            .uri(format!("http://127.0.0.1:{port}/big"))
            .body(Vec::new())
            .expect("request");
        let error = guarded(true)
            .send(request)
            .await
            .expect_err("body over the cap");
        assert!(matches!(error, IdpHttpError::TooLarge));
        assert_eq!(error.to_string(), "identity provider response too large");
    }
}
