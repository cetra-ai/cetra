//! Origin check for cookie-authenticated writes (D141).
//!
//! Bearer requests skip the check. Authentication runs first, and only a
//! session principal triggers the origin check.

use axum::extract::{MatchedPath, Request, State};
use axum::http::Method;
use axum::middleware::Next;
use axum::response::Response;

use crate::api::common::{Problem, ProblemCode};
use crate::auth::principal::Principal;
use crate::http::AppState;
use crate::http::error;

/// Requires `Origin` to equal `public_url`'s origin on state-changing requests
/// that use a session, and on every state-changing `/api/auth/` request.
pub(crate) async fn csrf(State(state): State<AppState>, request: Request, next: Next) -> Response {
    if skips_origin(request.method()) || !must_check(&request) {
        return next.run(request).await;
    }
    let origin = request
        .headers()
        .get(axum::http::header::ORIGIN)
        .and_then(|value| value.to_str().ok());
    if origin == Some(state.config.public_url.origin()) {
        return next.run(request).await;
    }
    error::problem_response(&Problem::new(
        ProblemCode::Forbidden,
        Some("origin check failed".to_owned()),
    ))
}

fn skips_origin(method: &Method) -> bool {
    matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS)
}

fn must_check(request: &Request) -> bool {
    // The IdP's cross-site POST carries the IdP's Origin; the binder cookie and the signature protect the ACS (D-SA-10, D-SA-12).
    if request
        .extensions()
        .get::<MatchedPath>()
        .is_some_and(|path| path.as_str() == "/api/auth/saml/{org_id}/acs")
    {
        return false;
    }
    let session = request
        .extensions()
        .get::<Option<Principal>>()
        .and_then(Option::as_ref)
        .is_some_and(|principal| matches!(principal, Principal::Session { .. }));
    // The device flow's CLI calls carry no cookie and no Origin (D-AO-3). A session request to
    // them is still checked, because `session` is true.
    let path = request.uri().path();
    let device_cli = path == "/api/auth/device/code" || path == "/api/auth/device/token";
    session || (path.starts_with("/api/auth/") && !device_cli)
}

#[cfg(test)]
mod tests {
    use axum::http::{Method, StatusCode};

    use crate::test_support::saml_app;

    #[tokio::test]
    async fn the_saml_acs_skips_the_origin_check() {
        let (app, seeded, _owner) = saml_app().await;
        // No SAML provider is configured, so the ACS answers 404: the origin check let it through.
        let response = app
            .request(
                Method::POST,
                &format!("/api/auth/saml/{}/acs", seeded.org_id),
            )
            .origin(Some("https://evil.test"))
            .header("cookie", &seeded.cookie)
            .body(Vec::<u8>::new())
            .send()
            .await;
        assert_eq!(response.status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn other_saml_paths_keep_the_origin_check() {
        let (app, seeded, _owner) = saml_app().await;
        for path in [
            format!("/api/auth/saml/{}/start", seeded.org_id),
            format!("/api/auth/saml/{}/acs/x", seeded.org_id),
        ] {
            let response = app
                .request(Method::POST, &path)
                .origin(Some("https://evil.test"))
                .header("cookie", &seeded.cookie)
                .body(Vec::<u8>::new())
                .send()
                .await;
            assert!(
                !response.status.is_success() && !response.status.is_redirection(),
                "{path} was accepted: {}",
                response.status
            );
        }
    }
}
