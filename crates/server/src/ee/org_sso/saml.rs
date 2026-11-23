// SPDX-License-Identifier: Elastic-2.0
//! SAML sign-in for an org's provider (track SA M2, D-SA-5, D-SA-10, D-SA-12): the SP-initiated
//! login, the ACS with its unsolicited restart, and the SP metadata.

use std::net::IpAddr;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{HeaderMap, HeaderValue, Method, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum_extra::extract::cookie::Cookie;
use cetra_core::ids::OrgId;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::login::{SignIn, sign_in_core};
use super::saml_pending::{PENDING_TTL_NS, Pending};
use super::{OrgProvider, ProviderKind, SamlProvider};
use crate::api::org_sso::SsoProtocol;
use crate::auth::limits::network_key;
use crate::auth::oidc::flow::{LoginError, display_name};
use crate::auth::secret::{new_opaque_token, random_bytes};
use crate::auth::session::{SessionOrigin, start_session};
use crate::http::AppState;
use crate::http::access::{Access, RouteDef};
use crate::http::client_ip::ClientIp;
use crate::http::error::{self, ApiError};
use crate::http::restore::refuse_outbound;
use crate::meta::users::normalize_email;
use crate::routes::entitlements::OrgIdPath;
use crate::routes::oauth::redirect;

/// The binder cookie of a pending login (D-SA-10, D-SA-20). The `__Host-` prefix forbids
/// `Domain` and requires `Path=/` and `Secure`, so a sibling subdomain cannot set it.
const SAML_COOKIE: &str = "__Host-cetra_saml";
/// The loop-guard cookie of an unsolicited restart (D-SA-5, D-SA-20). Same prefix rules as
/// [`SAML_COOKIE`].
const SAML_RESTART_COOKIE: &str = "__Host-cetra_saml_restart";
/// The fixed RelayState (D-SA-10).
const RELAY_STATE: &str = "cetra";
/// The largest form body the ACS reads (D-SA-12).
const ACS_BODY_LIMIT: usize = 256 * 1024;
/// How long the restart cookie lives, in seconds (D-SA-5).
const RESTART_MAX_AGE_SECS: u64 = 120;
/// How long the binder cookie lives, in seconds (D-SA-10).
const BINDER_MAX_AGE_SECS: u64 = 600;

#[derive(Deserialize)]
struct AcsForm {
    #[serde(rename = "SAMLResponse")]
    saml_response: String,
}

/// The SAML routes: SP metadata, the start of a login and the ACS (D-SA-12).
pub(crate) fn routes() -> Vec<RouteDef> {
    vec![
        RouteDef {
            method: Method::GET,
            path: "/api/auth/saml/{org_id}",
            access: Access::Public,
            documented: true,
            handler: get(sp_metadata),
        },
        RouteDef {
            method: Method::GET,
            path: "/api/auth/saml/{org_id}/start",
            access: Access::Public,
            documented: true,
            handler: get(start),
        },
        RouteDef {
            method: Method::POST,
            path: "/api/auth/saml/{org_id}/acs",
            access: Access::Public,
            documented: true,
            handler: post(acs).layer(DefaultBodyLimit::max(ACS_BODY_LIMIT)),
        },
    ]
}

/// The org's provider, when it is a SAML provider in the index.
fn saml_provider(state: &AppState, org: OrgId) -> Option<Arc<OrgProvider>> {
    state
        .org_sso
        .provider(org)
        .filter(|provider| matches!(provider.kind, ProviderKind::Saml(_)))
}

fn internal(error: impl std::error::Error + Send + Sync + 'static) -> ApiError {
    ApiError::Internal(Box::new(error))
}

fn sso_unavailable() -> Result<Response, ApiError> {
    redirect("/login?error=sso_unavailable", std::iter::empty())
}

/// A `Set-Cookie` for a SAML cookie: `Path=/` (the `__Host-` rule), `Secure` and
/// `SameSite=None`, because the IdP's POST to the ACS is cross-site.
fn cookie(name: &str, value: &str, max_age: u64) -> HeaderValue {
    let text =
        format!("{name}={value}; HttpOnly; Secure; SameSite=None; Path=/; Max-Age={max_age}");
    // The name, the hex and base64url values, and the paths are ASCII.
    #[allow(clippy::expect_used)]
    HeaderValue::from_str(&text).expect("saml cookie is ascii")
}

/// A `Set-Cookie` that clears `name`.
fn clear(name: &str) -> HeaderValue {
    cookie(name, "", 0)
}

/// The value of the cookie `name` in `headers`, if any.
fn named_cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .find_map(|part| {
            let cookie = Cookie::parse_encoded(part.trim().to_owned()).ok()?;
            (cookie.name() == name).then(|| cookie.value().to_owned())
        })
}

/// The binder cookie of the request. Two binder cookies in one request are refused (D-SA-20):
/// `Err(())`, since a sibling subdomain could have set the second one.
fn binder_cookie(headers: &HeaderMap) -> Result<Option<String>, ()> {
    let values: Vec<String> = headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .filter_map(|part| Cookie::parse_encoded(part.trim().to_owned()).ok())
        .filter(|cookie| cookie.name() == SAML_COOKIE)
        .map(|cookie| cookie.value().to_owned())
        .collect();
    match values.as_slice() {
        [] => Ok(None),
        [binder] => Ok(Some(binder.clone())),
        _ => Err(()),
    }
}

/// `GET /api/auth/saml/{org_id}`: the SP's metadata document (D-SA-12).
async fn sp_metadata(
    State(state): State<AppState>,
    error::Path(path): error::Path<OrgIdPath>,
) -> Result<Response, ApiError> {
    let org = OrgId::from_uuid(path.org_id);
    let Some(provider) = saml_provider(&state, org) else {
        return Err(ApiError::NotFound);
    };
    // `saml_provider` keeps only SAML providers, so this arm is not reached.
    let ProviderKind::Saml(_) = &provider.kind else {
        return Err(ApiError::NotFound);
    };
    let sp = super::sp_urls(&state.config.public_url, org);
    let xml = cetra_saml::sp_metadata_xml(&sp.entity_id, &sp.acs_url);
    Ok((
        [(header::CONTENT_TYPE, "application/samlmetadata+xml")],
        xml,
    )
        .into_response())
}

/// `GET /api/auth/saml/{org_id}/start`: starts an SP-initiated login. It is also the IdP tile
/// URL (D-SA-5, D-SA-12).
async fn start(
    State(state): State<AppState>,
    ClientIp(ip): ClientIp,
    error::Path(path): error::Path<OrgIdPath>,
) -> Result<Response, ApiError> {
    refuse_outbound(&state, "external sign-in")?;
    let now = state.clock.now_ns();
    state
        .limits
        .sso_discover
        .check(&network_key(ip), now)
        .map_err(|retry| ApiError::RateLimited {
            retry_after_s: retry.0.as_secs().max(1),
        })?;
    let org = OrgId::from_uuid(path.org_id);
    let Some(provider) = saml_provider(&state, org) else {
        return sso_unavailable();
    };
    // `saml_provider` keeps only SAML providers, so this arm is not reached.
    let ProviderKind::Saml(saml) = &provider.kind else {
        return sso_unavailable();
    };
    if !state.org_sso.flags(org).is_some_and(|flags| flags.enabled) {
        return sso_unavailable();
    }
    begin(&state, org, saml, ip, None, "/".to_owned()).await
}

/// Starts an SP-initiated login: a pending entry, the binder cookie and the redirect to the
/// IdP (D-SA-10). `test_owner` is set for an owner's Test sign-in (D-P-57), and `return_to` is
/// the sanitized redirect the login ends at.
pub(crate) async fn begin(
    state: &AppState,
    org: OrgId,
    provider: &SamlProvider,
    ip: IpAddr,
    test_owner: Option<Uuid>,
    return_to: String,
) -> Result<Response, ApiError> {
    if !state.config.public_url.is_https() {
        return sso_unavailable();
    }
    let sso_allowed = url::Url::parse(provider.metadata.sso_url())
        .is_ok_and(|url| state.org_sso.policy().allows_url(&url));
    if !sso_allowed {
        return sso_unavailable();
    }
    let now = state.clock.now_ns();
    let request_id = format!("_{}", hex::encode(random_bytes(16).map_err(internal)?));
    let binder = new_opaque_token().map_err(internal)?.plaintext;
    let sp = super::sp_urls(&state.config.public_url, org);
    let pending = Pending {
        org,
        test_owner,
        redirect: return_to,
        binder_sha256: hex::encode(Sha256::digest(binder.as_bytes())),
        network: network_key(ip),
        expires_ns: now + PENDING_TTL_NS,
    };
    state
        .org_sso
        .with_pending(|table| table.insert(request_id.clone(), pending, now));
    let location = cetra_saml::authn_request_redirect_url(&cetra_saml::AuthnRequest {
        sp_entity_id: &sp.entity_id,
        acs_url: &sp.acs_url,
        sso_url: provider.metadata.sso_url(),
        request_id: &request_id,
        relay_state: RELAY_STATE,
        issue_instant_ns: now,
    })
    .map_err(internal)?;
    redirect(
        &location,
        [cookie(SAML_COOKIE, &binder, BINDER_MAX_AGE_SECS)],
    )
}

/// A refusal before the pending entry is consumed. No cookie is set, so a POST from another
/// site cannot clear a browser's binder (D-SA-10).
fn refuse(error: &LoginError) -> Result<Response, ApiError> {
    redirect(
        &format!("/login?error={}", error.code().unwrap_or("provider_error")),
        std::iter::empty(),
    )
}

/// A failure after the pending entry is consumed: the binder cookie is cleared. The restart
/// cookie stays, so a loop stops (D-SA-5).
fn fail(error: LoginError) -> Result<Response, ApiError> {
    match error {
        LoginError::Internal(error) => Err(ApiError::Internal(error)),
        error => redirect(
            &format!("/login?error={}", error.code().unwrap_or("provider_error")),
            [clear(SAML_COOKIE)],
        ),
    }
}

/// `POST /api/auth/saml/{org_id}/acs`: validates the IdP's Response and signs the user in
/// (D-SA-5, D-SA-9, D-SA-10, D-SA-12). The steps run in this order, and only a `take` that
/// succeeds lets a failure clear the binder.
async fn acs(
    State(state): State<AppState>,
    ClientIp(ip): ClientIp,
    headers: HeaderMap,
    error::Path(path): error::Path<OrgIdPath>,
    body: Bytes,
) -> Result<Response, ApiError> {
    // 1. Rate limit per client IP.
    let now = state.clock.now_ns();
    state
        .limits
        .saml_acs
        .check(&network_key(ip), now)
        .map_err(|retry| ApiError::RateLimited {
            retry_after_s: retry.0.as_secs().max(1),
        })?;
    // 2. The org's SAML provider.
    let org = OrgId::from_uuid(path.org_id);
    let Some(provider) = saml_provider(&state, org) else {
        return Err(ApiError::NotFound);
    };
    // `saml_provider` keeps only SAML providers, so this arm is not reached.
    let ProviderKind::Saml(saml) = &provider.kind else {
        return Err(ApiError::NotFound);
    };
    // 3. The form.
    let Ok(form) = serde_urlencoded::from_bytes::<AcsForm>(&body) else {
        return refuse(&LoginError::SamlInvalid);
    };
    // 4. The decode.
    let received = match cetra_saml::decode_response(&form.saml_response) {
        Ok(received) => received,
        Err(error) => {
            tracing::info!(target: "audit", org_id = %org, reason = ?error, "saml response refused");
            return refuse(&LoginError::SamlInvalid);
        }
    };
    // 5. An unsolicited Response restarts the login once, through the start URL (D-SA-5).
    let Some(in_response_to) = received.in_response_to().map(str::to_owned) else {
        if named_cookie(&headers, SAML_RESTART_COOKIE).is_some() {
            return refuse(&LoginError::SamlInvalid);
        }
        let sp = super::sp_urls(&state.config.public_url, org);
        return redirect(
            &sp.start_path,
            [cookie(SAML_RESTART_COOKIE, "1", RESTART_MAX_AGE_SECS)],
        );
    };
    // 6. Take the pending entry. Its binder must match the browser's binder cookie (D-SA-10).
    // Two binder cookies are refused before the entry is touched (D-SA-20).
    let Ok(binder) = binder_cookie(&headers) else {
        return refuse(&LoginError::SamlInvalid);
    };
    let Ok(pending) = state
        .org_sso
        .with_pending(|table| table.take(&in_response_to, org, binder.as_deref(), now))
    else {
        return refuse(&LoginError::SamlInvalid);
    };
    // From here the entry is consumed, and failures clear the binder cookie with `fail`.
    let sp = super::sp_urls(&state.config.public_url, org);
    // 7. The provider's flags. A disabled provider serves only an owner's Test sign-in (D-SA-5).
    let enabled = state.org_sso.flags(org).is_some_and(|flags| flags.enabled);
    if !enabled && pending.test_owner.is_none() {
        return fail(LoginError::SsoUnavailable);
    }
    // 8. The Response's validity, signature and content (D-SA-9).
    let expected = cetra_saml::ExpectedResponse {
        sp_entity_id: &sp.entity_id,
        acs_url: &sp.acs_url,
        idp: &saml.metadata,
        request_id: &in_response_to,
        now_ns: now,
        attributes: &saml.attributes,
    };
    let assertion = match cetra_saml::validate_response(&received, &expected) {
        Ok(assertion) => assertion,
        Err(cetra_saml::ResponseError::EncryptedAssertion) => {
            return fail(LoginError::SamlUnsupported);
        }
        Err(error) => {
            tracing::info!(target: "audit", org_id = %org, reason = ?error, "saml response refused");
            return fail(LoginError::SamlInvalid);
        }
    };
    // 9. The email, normalized. Without one the login is refused (D-SA-6).
    let Some(email) = assertion
        .email
        .as_deref()
        .and_then(|email| normalize_email(email).ok())
    else {
        return fail(LoginError::EmailNotVerified);
    };
    // 10. The sign-in core both protocols share (D-SA-12).
    let input = SignIn {
        org,
        provider_id: provider.id,
        protocol: SsoProtocol::Saml,
        jit_role: provider.jit_role,
        subject: assertion.name_id,
        email,
        name: display_name(assertion.display_name),
        test: pending.test_owner.is_some(),
        test_owner: pending.test_owner,
        redirect: pending.redirect,
    };
    let logged_in = match sign_in_core(&state, input).await {
        Ok(logged_in) => logged_in,
        Err(error) => return fail(error),
    };
    // 11. The session, and the audit line.
    let user_agent = headers
        .get(header::USER_AGENT)
        .and_then(|value| value.to_str().ok());
    let session = match start_session(
        &state,
        logged_in.user_id,
        user_agent,
        Some(ip),
        SessionOrigin::OrgProvider(org),
    )
    .await
    {
        Ok(session) => session,
        Err(error) => {
            let mut response = error.into_response();
            response
                .headers_mut()
                .append(header::SET_COOKIE, clear(SAML_COOKIE));
            return Ok(response);
        }
    };
    tracing::info!(
        target: "audit",
        user_id = %logged_in.user_id,
        org_id = %org,
        provider = "saml",
        "login"
    );
    redirect(
        &logged_in.redirect,
        [clear(SAML_COOKIE), clear(SAML_RESTART_COOKIE), session],
    )
}

#[cfg(test)]
mod tests {
    use std::io::Read as _;
    use std::time::Duration;

    use axum::http::{Method, StatusCode};
    use base64::Engine as _;
    use cetra_saml::test_idp::{
        IDP_ENTITY_ID, IDP_SSO_URL, REQUEST_ID, ResponseOptions, Signing, TestKey,
    };

    use super::*;
    use crate::api::common::ProblemCode;
    use crate::api::org_sso::JitRole;
    use crate::auth::roles::{OrgRole, org_role_str};
    use crate::ee::org_sso::saml_pending::PendingTable;
    use crate::ee::org_sso::store;
    use crate::test_support::mock_idp::MockIdp;
    use crate::test_support::oauth::{
        SESSION_NAME, cookie_header, location, one_cookie, set_cookies,
    };
    use crate::test_support::{
        TestApp, TestResponse, begin_saml, capture_logs, configure_org_saml, configure_org_sso,
        license_key, license_key_until, org_sso_app_with, post_acs, saml_app, saml_options,
    };

    const AUTHORIZE: &str = "/api/auth/sso/authorize?domain=acme.test&redirect=%2Facme";

    async fn count(app: &TestApp, sql: String) -> i64 {
        sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
            .fetch_one(app.state.meta.reader())
            .await
            .unwrap()
    }

    /// The Response for `request_id`, signed by the test IdP for this app's SP.
    fn signed(app: &TestApp, org: OrgId, request_id: &str) -> String {
        cetra_saml::test_idp::response_b64(&saml_options(app, org, request_id))
    }

    /// The AuthnRequest XML inside the IdP redirect.
    fn authn_request_xml(response: &TestResponse) -> String {
        let url = url::Url::parse(location(response)).expect("the IdP URL");
        let encoded = url
            .query_pairs()
            .find(|(key, _)| key.as_ref() == "SAMLRequest")
            .map(|(_, value)| value.into_owned())
            .expect("a SAMLRequest parameter");
        let deflated = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .expect("base64 SAMLRequest");
        let mut xml = String::new();
        flate2::read::DeflateDecoder::new(deflated.as_slice())
            .read_to_string(&mut xml)
            .expect("inflated SAMLRequest");
        xml
    }

    fn acs_path(org: OrgId) -> String {
        format!("/api/auth/saml/{org}/acs")
    }

    #[tokio::test]
    async fn a_saml_user_signs_in_and_joins_with_the_jit_role() {
        let (app, seeded, _owner) = saml_app().await;
        let org = seeded.org_id;
        configure_org_saml(&app, org, &[TestKey::Idp], "acme.test", true, false).await;
        let (request_id, binder, _) = begin_saml(&app, None).await;
        let response = post_acs(
            &app,
            org,
            &signed(&app, org, &request_id),
            &[binder.as_str()],
        )
        .await;
        assert_eq!(response.status, StatusCode::FOUND);
        assert_eq!(location(&response), "/acme");
        one_cookie(&response, SESSION_NAME);
        assert!(cookie_header(&response, "__Host-cetra_saml").contains("Max-Age=0"));
        let provider = store::identity_provider(org);
        assert_eq!(
            count(
                &app,
                format!(
                    "SELECT COUNT(*) FROM identities WHERE provider = '{provider}' \
                     AND subject = 'ada@acme.test'"
                ),
            )
            .await,
            1
        );
        assert_eq!(
            count(
                &app,
                format!(
                    "SELECT COUNT(*) FROM org_members m JOIN users u ON u.id = m.user_id \
                     WHERE u.email = 'ada@acme.test' AND m.org_id = '{org}' AND m.role = '{}'",
                    org_role_str(OrgRole::Viewer)
                ),
            )
            .await,
            1
        );
    }

    #[tokio::test]
    async fn the_authorize_redirect_carries_a_valid_authn_request() {
        let (app, seeded, _owner) = saml_app().await;
        let org = seeded.org_id;
        configure_org_saml(&app, org, &[TestKey::Idp], "acme.test", true, false).await;
        let (request_id, _binder, response) = begin_saml(&app, None).await;
        assert!(cetra_saml::is_valid_request_id(&request_id));
        assert!(location(&response).starts_with(IDP_SSO_URL));
        let xml = authn_request_xml(&response);
        assert!(xml.contains(&format!("https://cetra.example/api/auth/saml/{org}/acs")));
        let binder = cookie_header(&response, "__Host-cetra_saml");
        for attribute in [
            "HttpOnly",
            "Secure",
            "SameSite=None",
            "Path=/",
            "Max-Age=600",
        ] {
            assert!(binder.contains(attribute), "{binder}");
        }
    }

    #[tokio::test]
    async fn the_acs_ignores_a_foreign_origin() {
        let (app, seeded, _owner) = saml_app().await;
        let org = seeded.org_id;
        configure_org_saml(&app, org, &[TestKey::Idp], "acme.test", true, false).await;
        let (request_id, binder, _) = begin_saml(&app, None).await;
        let form = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("SAMLResponse", &signed(&app, org, &request_id))
            .append_pair("RelayState", "cetra")
            .finish();
        let cookies = format!("{}; {binder}", seeded.cookie);
        let response = app
            .request(Method::POST, &acs_path(org))
            .header("content-type", "application/x-www-form-urlencoded")
            .origin(Some("https://evil.test"))
            .header("cookie", &cookies)
            .body(form.into_bytes())
            .send()
            .await;
        assert_eq!(location(&response), "/acme");
    }

    #[tokio::test]
    async fn a_response_is_accepted_once() {
        let (app, seeded, _owner) = saml_app().await;
        let org = seeded.org_id;
        configure_org_saml(&app, org, &[TestKey::Idp], "acme.test", true, false).await;
        let (request_id, binder, _) = begin_saml(&app, None).await;
        let b64 = signed(&app, org, &request_id);
        let first = post_acs(&app, org, &b64, &[binder.as_str()]).await;
        assert_eq!(location(&first), "/acme");
        let second = post_acs(&app, org, &b64, &[binder.as_str()]).await;
        assert_eq!(location(&second), "/login?error=saml_invalid");
    }

    #[tokio::test]
    async fn a_missing_binder_is_invalid_and_keeps_the_entry() {
        let (app, seeded, _owner) = saml_app().await;
        let org = seeded.org_id;
        configure_org_saml(&app, org, &[TestKey::Idp], "acme.test", true, false).await;
        let (request_id, binder, _) = begin_saml(&app, None).await;
        let b64 = signed(&app, org, &request_id);
        let refused = post_acs(&app, org, &b64, &[]).await;
        assert_eq!(location(&refused), "/login?error=saml_invalid");
        assert!(set_cookies(&refused).is_empty());
        let response = post_acs(&app, org, &b64, &[binder.as_str()]).await;
        assert_eq!(location(&response), "/acme");
    }

    #[tokio::test]
    async fn junk_posted_cross_site_does_not_clear_the_binder() {
        let (app, seeded, _owner) = saml_app().await;
        let org = seeded.org_id;
        configure_org_saml(&app, org, &[TestKey::Idp], "acme.test", true, false).await;
        let (request_id, binder, _) = begin_saml(&app, None).await;
        let junk = post_acs(&app, org, "!!!", &[binder.as_str()]).await;
        assert_eq!(location(&junk), "/login?error=saml_invalid");
        assert!(set_cookies(&junk).is_empty());
        let response = post_acs(
            &app,
            org,
            &signed(&app, org, &request_id),
            &[binder.as_str()],
        )
        .await;
        assert_eq!(location(&response), "/acme");
    }

    #[tokio::test]
    async fn another_browsers_binder_is_invalid() {
        let (app, seeded, _owner) = saml_app().await;
        let org = seeded.org_id;
        configure_org_saml(&app, org, &[TestKey::Idp], "acme.test", true, false).await;
        let (first_id, first_binder, _) = begin_saml(&app, None).await;
        let (_second_id, second_binder, _) = begin_saml(&app, None).await;
        let b64 = signed(&app, org, &first_id);
        let wrong = post_acs(&app, org, &b64, &[second_binder.as_str()]).await;
        assert_eq!(location(&wrong), "/login?error=saml_invalid");
        let right = post_acs(&app, org, &b64, &[first_binder.as_str()]).await;
        assert_eq!(location(&right), "/acme");
    }

    #[tokio::test]
    async fn a_restart_drops_pending_logins() {
        let (app, seeded, _owner) = saml_app().await;
        let org = seeded.org_id;
        configure_org_saml(&app, org, &[TestKey::Idp], "acme.test", true, false).await;
        let (request_id, binder, _) = begin_saml(&app, None).await;
        app.state
            .org_sso
            .with_pending(|table| *table = PendingTable::default());
        let response = post_acs(
            &app,
            org,
            &signed(&app, org, &request_id),
            &[binder.as_str()],
        )
        .await;
        assert_eq!(location(&response), "/login?error=saml_invalid");
    }

    #[tokio::test]
    async fn an_unsolicited_response_restarts_once() {
        let (app, seeded, _owner) = saml_app().await;
        let org = seeded.org_id;
        configure_org_saml(&app, org, &[TestKey::Idp], "acme.test", true, false).await;
        let options = ResponseOptions {
            response_in_response_to: None,
            confirmation_in_response_to: None,
            ..saml_options(&app, org, REQUEST_ID)
        };
        let b64 = cetra_saml::test_idp::response_b64(&options);
        let start = format!("/api/auth/saml/{org}/start");
        let first = post_acs(&app, org, &b64, &[]).await;
        assert_eq!(location(&first), start.as_str());
        assert!(cookie_header(&first, "__Host-cetra_saml_restart").contains("Max-Age=120"));
        let restart = one_cookie(&first, "__Host-cetra_saml_restart");
        let second = post_acs(&app, org, &b64, &[restart.as_str()]).await;
        assert_eq!(location(&second), "/login?error=saml_invalid");
        let started = app.request(Method::GET, &start).send().await;
        assert!(location(&started).starts_with(IDP_SSO_URL));
        one_cookie(&started, "__Host-cetra_saml");
    }

    #[tokio::test]
    async fn an_unknown_request_id_is_invalid_not_a_restart() {
        let (app, seeded, _owner) = saml_app().await;
        let org = seeded.org_id;
        configure_org_saml(&app, org, &[TestKey::Idp], "acme.test", true, false).await;
        let unknown = "_00000000000000000000000000000000";
        let response = post_acs(&app, org, &signed(&app, org, unknown), &[]).await;
        assert_eq!(location(&response), "/login?error=saml_invalid");
        assert!(set_cookies(&response).is_empty());
    }

    #[tokio::test]
    async fn a_wrong_audience_is_invalid_and_logged_by_class() {
        let (app, seeded, _owner) = saml_app().await;
        let org = seeded.org_id;
        configure_org_saml(&app, org, &[TestKey::Idp], "acme.test", true, false).await;
        let (request_id, binder, _) = begin_saml(&app, None).await;
        let options = ResponseOptions {
            audiences: vec!["https://other.test/sp".to_owned()],
            ..saml_options(&app, org, &request_id)
        };
        let b64 = cetra_saml::test_idp::response_b64(&options);
        let (response, logs) = capture_logs(post_acs(&app, org, &b64, &[binder.as_str()])).await;
        assert_eq!(location(&response), "/login?error=saml_invalid");
        assert!(logs.contains("Audience"), "{logs}");
        assert!(!logs.contains("ada@acme.test"), "{logs}");
        assert!(!logs.contains(IDP_ENTITY_ID), "{logs}");
    }

    #[tokio::test]
    async fn an_encrypted_assertion_is_unsupported() {
        let (app, seeded, _owner) = saml_app().await;
        let org = seeded.org_id;
        configure_org_saml(&app, org, &[TestKey::Idp], "acme.test", true, false).await;
        let (request_id, binder, _) = begin_saml(&app, None).await;
        let mut options = saml_options(&app, org, &request_id);
        options.signing = Signing::None;
        let xml = cetra_saml::test_idp::response_xml(&options);
        let start = xml.find("<saml:Assertion").expect("an assertion");
        let end =
            xml.find("</saml:Assertion>").expect("an assertion end") + "</saml:Assertion>".len();
        let encrypted = format!(
            "{}<saml:EncryptedAssertion><xenc:EncryptedData \
             xmlns:xenc=\"http://www.w3.org/2001/04/xmlenc#\"/></saml:EncryptedAssertion>{}",
            &xml[..start],
            &xml[end..]
        );
        let b64 = base64::engine::general_purpose::STANDARD.encode(encrypted);
        let response = post_acs(&app, org, &b64, &[binder.as_str()]).await;
        assert_eq!(location(&response), "/login?error=saml_unsupported");
    }

    #[tokio::test]
    async fn a_disabled_provider_serves_only_the_owners_test() {
        let (app, seeded, owner) = saml_app().await;
        let org = seeded.org_id;
        configure_org_saml(&app, org, &[TestKey::Idp], "acme.test", false, false).await;
        let anonymous = app.request(Method::GET, AUTHORIZE).send().await;
        assert_eq!(location(&anonymous), "/login?error=sso_unavailable");
        app.state.clock.advance(Duration::from_secs(60));
        let (request_id, binder, _) = begin_saml(&app, Some(owner.cookie.as_str())).await;
        let options = ResponseOptions {
            name_id: "owner@acme.test".to_owned(),
            attributes: vec![("email".to_owned(), vec!["owner@acme.test".to_owned()])],
            ..saml_options(&app, org, &request_id)
        };
        let members = format!("SELECT COUNT(*) FROM org_members WHERE org_id = '{org}'");
        let members_before = count(&app, members.clone()).await;
        let response = post_acs(
            &app,
            org,
            &cetra_saml::test_idp::response_b64(&options),
            &[binder.as_str()],
        )
        .await;
        assert_eq!(location(&response), "/acme");
        one_cookie(&response, SESSION_NAME);
        assert_eq!(count(&app, members.clone()).await, members_before);
        let now = app.state.clock.now_ns();
        assert_eq!(
            count(
                &app,
                format!(
                    "SELECT COUNT(*) FROM org_sso_providers WHERE org_id = '{org}' \
                     AND last_login_at = {now}"
                ),
            )
            .await,
            1
        );
    }

    #[tokio::test]
    async fn a_test_sign_in_signs_in_only_its_owner() {
        let (app, seeded, owner) = saml_app().await;
        let org = seeded.org_id;
        configure_org_saml(&app, org, &[TestKey::Idp], "acme.test", false, false).await;
        let (request_id, binder, _) = begin_saml(&app, Some(owner.cookie.as_str())).await;
        // The default NameID and email are ada@acme.test, who is not the owner.
        let response = post_acs(
            &app,
            org,
            &signed(&app, org, &request_id),
            &[binder.as_str()],
        )
        .await;
        assert_eq!(location(&response), "/login?error=sso_unavailable");
    }

    #[tokio::test]
    async fn the_start_url_needs_an_enabled_provider() {
        let (app, seeded, _owner) = saml_app().await;
        let org = seeded.org_id;
        configure_org_saml(&app, org, &[TestKey::Idp], "acme.test", false, false).await;
        let response = app
            .request(Method::GET, &format!("/api/auth/saml/{org}/start"))
            .send()
            .await;
        assert_eq!(location(&response), "/login?error=sso_unavailable");
    }

    #[tokio::test]
    async fn enforcement_refuses_password_login_for_saml_domains() {
        let (app, seeded, owner) = saml_app().await;
        // Org enforcement refuses a password login after the password verifies (D-P-58), so
        // the covered owner gets a password first.
        let hash = crate::auth::password::Hasher::new()
            .await
            .unwrap()
            .hash("cetra-ok-1")
            .await
            .unwrap();
        let now = app.state.clock.now_ns();
        let owner_id = owner.id;
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    crate::meta::users::set_password_hash(conn, owner_id, Some(&hash), now).await
                })
            })
            .await
            .unwrap();
        configure_org_saml(
            &app,
            seeded.org_id,
            &[TestKey::Idp],
            "acme.test",
            true,
            true,
        )
        .await;
        let response = app
            .request(Method::POST, "/api/auth/login")
            .json(&serde_json::json!({ "email": "owner@acme.test", "password": "cetra-ok-1" }))
            .send()
            .await;
        assert_eq!(response.status, StatusCode::FORBIDDEN);
        assert_eq!(response.problem().code, ProblemCode::SsoRequired);
    }

    #[tokio::test]
    async fn the_lapse_signs_in_without_joining() {
        let (app, seeded, _owner) = org_sso_app_with(|config| {
            config.public_url = serde_json::from_str("\"https://cetra.example\"").expect("url");
            config.license_key = Some(crate::config::Secret::new(license_key_until(
                &["org_sso", "saml"],
                "2025-06-01T00:00:00Z",
                "2025-07-01T00:00:00Z",
            )));
        })
        .await;
        let org = seeded.org_id;
        configure_org_saml(&app, org, &[TestKey::Idp], "acme.test", true, false).await;
        let (request_id, binder, _) = begin_saml(&app, None).await;
        let response = post_acs(
            &app,
            org,
            &signed(&app, org, &request_id),
            &[binder.as_str()],
        )
        .await;
        assert_eq!(location(&response), "/acme");
        assert_eq!(
            count(
                &app,
                "SELECT COUNT(*) FROM org_members m JOIN users u ON u.id = m.user_id \
                 WHERE u.email = 'ada@acme.test'"
                    .to_owned(),
            )
            .await,
            0
        );
    }

    #[tokio::test]
    async fn jit_needs_the_saml_feature() {
        // `org_sso` alone: the org's SAML users sign in but do not join (D-SA-4).
        let (app, seeded, _owner) = org_sso_app_with(|config| {
            config.public_url = serde_json::from_str("\"https://cetra.example\"").expect("url");
        })
        .await;
        let org = seeded.org_id;
        configure_org_saml(&app, org, &[TestKey::Idp], "acme.test", true, false).await;
        let (request_id, binder, _) = begin_saml(&app, None).await;
        let response = post_acs(
            &app,
            org,
            &signed(&app, org, &request_id),
            &[binder.as_str()],
        )
        .await;
        assert_eq!(location(&response), "/acme");
        assert_eq!(
            count(
                &app,
                "SELECT COUNT(*) FROM org_members m JOIN users u ON u.id = m.user_id \
                 WHERE u.email = 'ada@acme.test'"
                    .to_owned(),
            )
            .await,
            0
        );
    }

    #[tokio::test]
    async fn email_must_be_in_a_verified_domain() {
        let (app, seeded, _owner) = saml_app().await;
        let org = seeded.org_id;
        configure_org_saml(&app, org, &[TestKey::Idp], "acme.test", true, false).await;
        let (request_id, binder, _) = begin_saml(&app, None).await;
        let options = ResponseOptions {
            attributes: vec![("email".to_owned(), vec!["ada@other.test".to_owned()])],
            ..saml_options(&app, org, &request_id)
        };
        let response = post_acs(
            &app,
            org,
            &cetra_saml::test_idp::response_b64(&options),
            &[binder.as_str()],
        )
        .await;
        assert_eq!(location(&response), "/login?error=domain_not_allowed");
    }

    #[tokio::test]
    async fn no_email_is_email_not_verified() {
        let (app, seeded, _owner) = saml_app().await;
        let org = seeded.org_id;
        configure_org_saml(&app, org, &[TestKey::Idp], "acme.test", true, false).await;
        let (request_id, binder, _) = begin_saml(&app, None).await;
        let options = ResponseOptions {
            name_id_format: Some("urn:oasis:names:tc:SAML:2.0:nameid-format:persistent".to_owned()),
            attributes: Vec::new(),
            ..saml_options(&app, org, &request_id)
        };
        let response = post_acs(
            &app,
            org,
            &cetra_saml::test_idp::response_b64(&options),
            &[binder.as_str()],
        )
        .await;
        assert_eq!(location(&response), "/login?error=email_not_verified");
    }

    #[tokio::test]
    async fn the_acs_is_rate_limited() {
        let (app, seeded, _owner) = saml_app().await;
        let org = seeded.org_id;
        configure_org_saml(&app, org, &[TestKey::Idp], "acme.test", true, false).await;
        for _ in 0..60 {
            let response = post_acs(&app, org, "!!!", &[]).await;
            assert_eq!(location(&response), "/login?error=saml_invalid");
        }
        let response = post_acs(&app, org, "!!!", &[]).await;
        assert_eq!(response.status, StatusCode::TOO_MANY_REQUESTS);
    }

    #[tokio::test]
    async fn an_oversized_body_is_413() {
        let (app, seeded, _owner) = saml_app().await;
        let response = app
            .request(Method::POST, &acs_path(seeded.org_id))
            .header("content-type", "application/x-www-form-urlencoded")
            .body(vec![b'a'; ACS_BODY_LIMIT + 1])
            .send()
            .await;
        assert_eq!(response.status, StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn the_acs_of_an_org_without_saml_is_404() {
        let (app, seeded, _owner) = saml_app().await;
        let response = post_acs(&app, seeded.org_id, "!!!", &[]).await;
        assert_eq!(response.status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn the_sp_metadata_is_served() {
        let (app, seeded, _owner) = saml_app().await;
        let org = seeded.org_id;
        configure_org_saml(&app, org, &[TestKey::Idp], "acme.test", true, false).await;
        let response = app
            .request(Method::GET, &format!("/api/auth/saml/{org}"))
            .send()
            .await;
        assert_eq!(response.status, StatusCode::OK);
        assert_eq!(
            response.header("content-type"),
            Some("application/samlmetadata+xml")
        );
        let body = String::from_utf8_lossy(&response.body);
        assert!(body.contains(&format!(
            "entityID=\"https://cetra.example/api/auth/saml/{org}\""
        )));
    }

    #[tokio::test]
    async fn sp_metadata_of_an_org_without_saml_is_404() {
        let (app, seeded, _owner) = saml_app().await;
        let response = app
            .request(Method::GET, &format!("/api/auth/saml/{}", seeded.org_id))
            .send()
            .await;
        assert_eq!(response.status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn an_http_public_url_makes_saml_unavailable() {
        let (app, seeded, _owner) = org_sso_app_with(|config| {
            config.license_key = Some(crate::config::Secret::new(license_key(&[
                "org_sso", "saml",
            ])));
        })
        .await;
        configure_org_saml(
            &app,
            seeded.org_id,
            &[TestKey::Idp],
            "acme.test",
            true,
            false,
        )
        .await;
        let response = app.request(Method::GET, AUTHORIZE).send().await;
        assert_eq!(location(&response), "/login?error=sso_unavailable");
    }

    #[tokio::test]
    async fn logs_carry_no_saml_payload() {
        let (app, seeded, _owner) = saml_app().await;
        let org = seeded.org_id;
        configure_org_saml(&app, org, &[TestKey::Idp], "acme.test", true, false).await;
        let (outcome, logs) = capture_logs(async {
            let (request_id, binder, _) = begin_saml(&app, None).await;
            let response = post_acs(
                &app,
                org,
                &signed(&app, org, &request_id),
                &[binder.as_str()],
            )
            .await;
            (request_id, binder, response)
        })
        .await;
        let (request_id, binder, response) = outcome;
        assert_eq!(location(&response), "/acme");
        let binder_value = binder.split_once('=').expect("a cookie pair").1;
        for needle in [
            "ada@acme.test",
            IDP_ENTITY_ID,
            request_id.as_str(),
            binder_value,
        ] {
            assert!(!logs.contains(needle), "log contains a payload: {logs}");
        }
    }

    #[tokio::test]
    async fn start_is_refused_in_restore_mode() {
        let (app, seeded, _owner) = org_sso_app_with(|config| {
            config.restore_mode = true;
        })
        .await;
        let response = app
            .request(
                Method::GET,
                &format!("/api/auth/saml/{}/start", seeded.org_id),
            )
            .send()
            .await;
        crate::http::restore::tests::assert_refused(&response, "external sign-in");
    }

    #[tokio::test]
    async fn two_binder_cookies_are_refused() {
        let (app, seeded, _owner) = saml_app().await;
        let org = seeded.org_id;
        configure_org_saml(&app, org, &[TestKey::Idp], "acme.test", true, false).await;
        let (request_id, binder, _) = begin_saml(&app, None).await;
        let (_other_id, other_binder, _) = begin_saml(&app, None).await;
        let b64 = signed(&app, org, &request_id);
        let refused = post_acs(&app, org, &b64, &[binder.as_str(), other_binder.as_str()]).await;
        assert_eq!(location(&refused), "/login?error=saml_invalid");
        assert!(set_cookies(&refused).is_empty());
        // The refusal leaves the pending entry for the one-cookie request.
        let response = post_acs(&app, org, &b64, &[binder.as_str()]).await;
        assert_eq!(location(&response), "/acme");
    }

    #[tokio::test]
    async fn the_acs_limit_counts_an_ipv6_slash_64() {
        let (app, seeded, _owner) = saml_app().await;
        let org = seeded.org_id;
        configure_org_saml(&app, org, &[TestKey::Idp], "acme.test", true, false).await;
        let host: IpAddr = "2001:db8::1".parse().expect("an IPv6 address");
        let neighbour: IpAddr = "2001:db8::2".parse().expect("an IPv6 address");
        for _ in 0..60 {
            let response = app
                .request(Method::POST, &acs_path(org))
                .header("content-type", "application/x-www-form-urlencoded")
                .origin(Some("https://idp.test"))
                .peer(host)
                .send()
                .await;
            assert_eq!(location(&response), "/login?error=saml_invalid");
        }
        let limited = app
            .request(Method::POST, &acs_path(org))
            .header("content-type", "application/x-www-form-urlencoded")
            .origin(Some("https://idp.test"))
            .peer(neighbour)
            .send()
            .await;
        assert_eq!(limited.status, StatusCode::TOO_MANY_REQUESTS);
    }

    #[tokio::test]
    async fn a_provider_switched_during_sign_in_is_unavailable() {
        let (app, seeded, _owner) = saml_app().await;
        let org = seeded.org_id;
        configure_org_saml(&app, org, &[TestKey::Idp], "acme.test", true, false).await;
        let stored = store::provider(app.state.meta.reader(), org)
            .await
            .expect("the provider query")
            .expect("a provider");
        let sign_in = |provider_id: Uuid, protocol: SsoProtocol| SignIn {
            org,
            provider_id,
            protocol,
            jit_role: JitRole::Viewer,
            subject: "ada-subject".to_owned(),
            email: "ada@acme.test".to_owned(),
            name: None,
            test: false,
            test_owner: None,
            redirect: "/acme".to_owned(),
        };
        // The provider row was deleted and created again since the sign-in was validated.
        let replaced = sign_in_core(&app.state, sign_in(Uuid::now_v7(), SsoProtocol::Saml)).await;
        assert!(matches!(replaced, Err(LoginError::SsoUnavailable)));
        // The provider changed protocol since the sign-in was validated.
        let switched = sign_in_core(&app.state, sign_in(stored.id, SsoProtocol::Oidc)).await;
        assert!(matches!(switched, Err(LoginError::SsoUnavailable)));
        // The validated provider still matches, so the sign-in goes through.
        let matching = sign_in_core(&app.state, sign_in(stored.id, SsoProtocol::Saml)).await;
        assert!(matching.is_ok());
    }

    #[tokio::test]
    async fn sp_metadata_of_an_oidc_org_is_404() {
        let (app, seeded, _owner) = saml_app().await;
        let idp = MockIdp::start().await;
        configure_org_sso(&app, seeded.org_id, &idp, "acme.test", true, false).await;
        let response = app
            .request(Method::GET, &format!("/api/auth/saml/{}", seeded.org_id))
            .send()
            .await;
        assert_eq!(response.status, StatusCode::NOT_FOUND);
    }
}
