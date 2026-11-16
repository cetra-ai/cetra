//! Helpers for the OAuth and OIDC login flows, shared by `routes::oauth` and the org SSO
//! login tests (track P M5). A test drives the mock IdP through these.

use axum::http::{Method, StatusCode, header};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::Deserialize;
use uuid::Uuid;

use crate::test_support::mock_idp::Grant;
use crate::test_support::{TestApp, TestResponse};

pub(crate) const OAUTH_NAME: &str = cetra_core::cetra_name!("_oauth");
pub(crate) const SESSION_NAME: &str = cetra_core::cetra_name!("_session");

#[derive(Deserialize)]
pub(crate) struct MeBody {
    pub(crate) user: MeUser,
}

#[derive(Deserialize)]
pub(crate) struct MeUser {
    pub(crate) id: Uuid,
    pub(crate) email: String,
    pub(crate) name: Option<String>,
}

/// One authorize round trip: the state cookie and the values the callback must echo.
pub(crate) struct Pending {
    pub(crate) cookie: String,
    pub(crate) state: String,
    pub(crate) nonce: Option<String>,
    pub(crate) challenge: String,
    pub(crate) redirect_uri: String,
}

pub(crate) fn set_cookies(response: &TestResponse) -> Vec<String> {
    response
        .headers
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok().map(str::to_owned))
        .collect()
}

pub(crate) fn cookie_header(response: &TestResponse, name: &str) -> String {
    let prefix = format!("{name}=");
    set_cookies(response)
        .into_iter()
        .find(|value| value.starts_with(&prefix))
        .unwrap_or_else(|| panic!("missing {name}"))
}

pub(crate) fn one_cookie(response: &TestResponse, name: &str) -> String {
    cookie_header(response, name)
        .split(';')
        .next()
        .expect("pair")
        .to_owned()
}

pub(crate) fn location(response: &TestResponse) -> &str {
    response.header("location").expect("location")
}

/// The sanitized `redirect` the state cookie carries.
pub(crate) fn saved_redirect(response: &TestResponse) -> String {
    let pair = one_cookie(response, OAUTH_NAME);
    let value = pair.split_once('=').expect("cookie").1;
    let payload = value.split_once('.').expect("signature").0;
    let bytes = URL_SAFE_NO_PAD.decode(payload).expect("payload");
    let state: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
    state["redirect"].as_str().expect("redirect").to_owned()
}

pub(crate) async fn begin(app: &TestApp, provider: &str, redirect: &str) -> Pending {
    begin_with(app, provider, &[("redirect", redirect)]).await
}

/// `GET /api/auth/authorize/{provider}?query`, with the instance providers' query.
pub(crate) async fn begin_with(app: &TestApp, provider: &str, query: &[(&str, &str)]) -> Pending {
    begin_at(
        app,
        &format!(
            "/api/auth/authorize/{provider}?{}",
            serde_urlencoded::to_string(query).expect("query")
        ),
        None,
    )
    .await
}

/// Starts a login at `path_and_query` (any authorize route, such as the org's), optionally
/// with a `cookie` header (an owner's session for a Test sign-in).
pub(crate) async fn begin_at(app: &TestApp, path_and_query: &str, cookie: Option<&str>) -> Pending {
    let mut request = app.request(Method::GET, path_and_query);
    if let Some(cookie) = cookie {
        request = request.header(header::COOKIE.as_str(), cookie);
    }
    let response = request.send().await;
    assert_eq!(response.status, StatusCode::FOUND);
    let url = url::Url::parse(location(&response)).expect("authorize url");
    let pairs: std::collections::HashMap<String, String> = url.query_pairs().into_owned().collect();
    Pending {
        cookie: one_cookie(&response, OAUTH_NAME),
        state: pairs["state"].clone(),
        nonce: pairs.get("nonce").cloned(),
        challenge: pairs["code_challenge"].clone(),
        redirect_uri: pairs["redirect_uri"].clone(),
    }
}

pub(crate) async fn finish(
    app: &TestApp,
    provider: &str,
    pending: &Pending,
    query: &str,
) -> TestResponse {
    app.request(
        Method::GET,
        &format!("/api/auth/callback/{provider}?{query}"),
    )
    .header(header::COOKIE.as_str(), &pending.cookie)
    .send()
    .await
}

/// A mock IdP grant for `subject` and `email`, bound to `pending`.
pub(crate) fn grant(
    subject: &str,
    pending: &Pending,
    email: &str,
    verified: Option<bool>,
) -> Grant {
    let mut grant = Grant::new(subject, &pending.challenge, &pending.redirect_uri);
    grant.claims.email = Some(email.to_owned());
    grant.claims.email_verified = verified;
    grant.claims.nonce.clone_from(&pending.nonce);
    grant.claims.name = Some(" Grace ".to_owned());
    grant.claims.hd = Some("acme.test".to_owned());
    grant
}

pub(crate) async fn me(app: &TestApp, cookie: &str) -> MeUser {
    let response = app
        .request(Method::GET, "/api/auth/me")
        .header(header::COOKIE.as_str(), cookie)
        .send()
        .await;
    assert_eq!(response.status, StatusCode::OK);
    response.json::<MeBody>().user
}
