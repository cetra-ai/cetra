// SPDX-License-Identifier: Elastic-2.0
//! SCIM errors and responses (RFC 7644 § 3.12, track EA M5, D-EA-12). Every `/scim/v2` answer is
//! `application/scim+json`. The error layer rewrites the problem answers of that path, so no
//! problem body (and no internal detail) leaves it.

use axum::extract::Request;
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use serde_json::{Value, json};

use crate::http::error::ApiError;
use crate::meta::MetaError;

pub(crate) const ERROR_SCHEMA: &str = "urn:ietf:params:scim:api:messages:2.0:Error";
pub(crate) const CONTENT_TYPE: &str = "application/scim+json";

/// One SCIM error answer. `detail` is static text, never a request value.
#[derive(Debug)]
pub(crate) struct ScimError {
    pub(crate) status: StatusCode,
    pub(crate) scim_type: Option<&'static str>,
    pub(crate) detail: &'static str,
    pub(crate) retry_after_s: Option<u64>,
}

impl ScimError {
    fn new(status: StatusCode, scim_type: Option<&'static str>, detail: &'static str) -> Self {
        Self {
            status,
            scim_type,
            detail,
            retry_after_s: None,
        }
    }

    pub(crate) fn invalid_filter() -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            Some("invalidFilter"),
            "Only eq filters on userName, externalId and emails are supported.",
        )
    }

    /// A `/Groups` filter other than one `eq` on `displayName` or `externalId`.
    pub(crate) fn invalid_group_filter() -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            Some("invalidFilter"),
            "Only eq filters on displayName and externalId are supported.",
        )
    }

    pub(crate) fn invalid_value(detail: &'static str) -> Self {
        Self::new(StatusCode::BAD_REQUEST, Some("invalidValue"), detail)
    }

    pub(crate) fn invalid_syntax() -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            Some("invalidSyntax"),
            "The request body is not a valid SCIM resource.",
        )
    }

    pub(crate) fn uniqueness() -> Self {
        Self::new(
            StatusCode::CONFLICT,
            Some("uniqueness"),
            "A user with this userName or externalId already exists.",
        )
    }

    /// RFC 7644 § 3.12 `mutability`: the attribute cannot be changed.
    pub(crate) fn mutability(detail: &'static str) -> Self {
        Self::new(StatusCode::BAD_REQUEST, Some("mutability"), detail)
    }

    /// A group with the same displayName or externalId exists.
    pub(crate) fn group_uniqueness() -> Self {
        Self::new(
            StatusCode::CONFLICT,
            Some("uniqueness"),
            "A group with this displayName or externalId already exists.",
        )
    }

    pub(crate) fn not_found() -> Self {
        Self::new(StatusCode::NOT_FOUND, None, "Resource not found.")
    }

    pub(crate) fn conflict(detail: &'static str) -> Self {
        Self::new(StatusCode::CONFLICT, None, detail)
    }

    pub(crate) fn forbidden(detail: &'static str) -> Self {
        Self::new(StatusCode::FORBIDDEN, None, detail)
    }

    pub(crate) fn internal() -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, None, "Internal error.")
    }

    pub(crate) fn rate_limited(retry_after_s: u64) -> Self {
        Self {
            retry_after_s: Some(retry_after_s),
            ..Self::new(StatusCode::TOO_MANY_REQUESTS, None, "Too many requests.")
        }
    }

    /// The SCIM error body. `status` is a string, as RFC 7644 § 3.12 requires.
    fn body(&self) -> Value {
        let mut body = json!({
            "schemas": [ERROR_SCHEMA],
            "status": self.status.as_u16().to_string(),
            "detail": self.detail,
        });
        if let Some(scim_type) = self.scim_type {
            body["scimType"] = json!(scim_type);
        }
        body
    }
}

impl IntoResponse for ScimError {
    fn into_response(self) -> Response {
        let bytes = serde_json::to_vec(&self.body()).unwrap_or_default();
        let mut response =
            (self.status, [(header::CONTENT_TYPE, CONTENT_TYPE)], bytes).into_response();
        if let Some(seconds) = self.retry_after_s
            && let Ok(value) = HeaderValue::from_str(&seconds.to_string())
        {
            response.headers_mut().insert(header::RETRY_AFTER, value);
        }
        response
    }
}

impl From<MetaError> for ScimError {
    fn from(error: MetaError) -> Self {
        match error {
            MetaError::Conflict("uniqueness") => Self::uniqueness(),
            MetaError::Conflict("an org needs an owner") => {
                Self::conflict("An org needs an owner.")
            }
            MetaError::Conflict(_) => Self::conflict("Conflict."),
            other => {
                tracing::error!(error = %other, "scim request failed");
                Self::internal()
            }
        }
    }
}

impl From<ApiError> for ScimError {
    fn from(error: ApiError) -> Self {
        error.log();
        match error {
            ApiError::PlanRequired { .. } => {
                Self::forbidden("SCIM provisioning needs the Enterprise plan or licence.")
            }
            ApiError::PlanLimit { .. } => Self::forbidden("The org has no seat left."),
            ApiError::RateLimited { retry_after_s } => Self::rate_limited(retry_after_s),
            ApiError::NotFound => Self::not_found(),
            other => {
                let status = StatusCode::from_u16(other.problem().status)
                    .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
                if status == StatusCode::INTERNAL_SERVER_ERROR {
                    Self::internal()
                } else {
                    Self::new(status, None, "Request refused.")
                }
            }
        }
    }
}

/// A SCIM success body: serialized with `serde_json`, sent as `application/scim+json`.
pub(crate) struct ScimJson<T>(pub(crate) T);

impl<T: Serialize> IntoResponse for ScimJson<T> {
    fn into_response(self) -> Response {
        match serde_json::to_vec(&self.0) {
            Ok(bytes) => ([(header::CONTENT_TYPE, CONTENT_TYPE)], bytes).into_response(),
            Err(_) => ScimError::internal().into_response(),
        }
    }
}

/// The error layer for `/scim/v2`. Every non-2xx answer on that path that is not already
/// `application/scim+json` (a problem, a plain-text 413, a bare 405) becomes a SCIM error with
/// the same status and a fixed detail for the status class. Its other headers (such as
/// `Retry-After` or `x-test-access`) are copied across, except the content type and length.
/// Other paths and other answers pass through.
pub(crate) async fn scim_errors(request: Request, next: Next) -> Response {
    let scim = is_scim_path(request.uri().path());
    let response = next.run(request).await;
    if !scim || response.status().is_success() || is_scim_json(&response) {
        return response;
    }
    let (parts, _body) = response.into_parts();
    let mut rewritten = ScimError {
        status: parts.status,
        scim_type: None,
        detail: layer_detail(parts.status),
        retry_after_s: None,
    }
    .into_response();
    for (name, value) in &parts.headers {
        if *name != header::CONTENT_TYPE && *name != header::CONTENT_LENGTH {
            rewritten.headers_mut().append(name.clone(), value.clone());
        }
    }
    rewritten
}

fn is_scim_path(path: &str) -> bool {
    path == "/scim/v2" || path.starts_with("/scim/v2/")
}

fn is_scim_json(response: &Response) -> bool {
    response
        .headers()
        .get(header::CONTENT_TYPE)
        .is_some_and(|value| value.as_bytes() == CONTENT_TYPE.as_bytes())
}

/// The fixed detail for a rewritten problem. The problem's own detail can name internals, so
/// it is never copied.
fn layer_detail(status: StatusCode) -> &'static str {
    match status.as_u16() {
        401 => "Invalid or missing bearer token.",
        403 => "Request refused.",
        404 => "Resource not found.",
        405 => "Method not allowed.",
        413 => "Request body too large.",
        415 => "Use Content-Type application/scim+json.",
        429 => "Too many requests.",
        _ => "Request failed.",
    }
}

#[cfg(test)]
mod tests {
    use axum::http::Method;

    use super::*;

    #[test]
    fn the_body_has_a_string_status_and_no_scim_type_when_none() {
        let body = ScimError::not_found().body();
        assert_eq!(body["status"], "404");
        assert_eq!(body["schemas"][0], ERROR_SCHEMA);
        assert!(body.get("scimType").is_none());
    }

    #[test]
    fn uniqueness_carries_its_scim_type_and_status() {
        let error = ScimError::uniqueness();
        assert_eq!(error.status, StatusCode::CONFLICT);
        assert_eq!(error.body()["scimType"], "uniqueness");
    }

    #[test]
    fn the_group_errors_carry_their_scim_types_and_fixed_details() {
        let filter = ScimError::invalid_group_filter();
        assert_eq!(filter.status, StatusCode::BAD_REQUEST);
        assert_eq!(filter.body()["scimType"], "invalidFilter");
        assert_eq!(
            filter.detail,
            "Only eq filters on displayName and externalId are supported."
        );
        let uniqueness = ScimError::group_uniqueness();
        assert_eq!(uniqueness.status, StatusCode::CONFLICT);
        assert_eq!(uniqueness.body()["scimType"], "uniqueness");
    }

    #[test]
    fn a_meta_uniqueness_conflict_is_a_scim_uniqueness_error() {
        let error = ScimError::from(MetaError::Conflict("uniqueness"));
        assert_eq!(error.status, StatusCode::CONFLICT);
        assert_eq!(error.scim_type, Some("uniqueness"));
    }

    #[test]
    fn an_unknown_meta_conflict_is_a_generic_conflict() {
        let error = ScimError::from(MetaError::Conflict("something else"));
        assert_eq!(error.status, StatusCode::CONFLICT);
        assert_eq!(error.scim_type, None);
        assert_eq!(error.detail, "Conflict.");
    }

    #[test]
    fn a_plan_refusal_is_a_forbidden_error_with_a_fixed_detail() {
        let error = ScimError::from(ApiError::PlanRequired { detail: "Scim" });
        assert_eq!(error.status, StatusCode::FORBIDDEN);
        assert_eq!(
            error.detail,
            "SCIM provisioning needs the Enterprise plan or licence."
        );
    }

    #[test]
    fn a_rate_limit_keeps_its_retry_delay() {
        let error = ScimError::from(ApiError::RateLimited { retry_after_s: 9 });
        assert_eq!(error.status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(error.retry_after_s, Some(9));
    }

    #[test]
    fn an_internal_api_error_gives_the_generic_internal_detail() {
        let error = ScimError::from(ApiError::Internal(Box::new(std::io::Error::other(
            "a value that must not leave",
        ))));
        assert_eq!(error.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(error.detail, "Internal error.");
    }

    /// A router with one route on `/scim/v2/item` (GET only), behind the layer.
    fn layered(handler: axum::routing::MethodRouter) -> axum::Router {
        axum::Router::new()
            .route("/scim/v2/item", handler)
            .route("/other", axum::routing::get(|| async { "plain" }))
            .layer(axum::middleware::from_fn(scim_errors))
    }

    async fn send(app: axum::Router, request: axum::http::Request<axum::body::Body>) -> Response {
        use tower::ServiceExt;
        app.oneshot(request).await.unwrap()
    }

    #[tokio::test]
    async fn a_plain_text_413_becomes_a_scim_error_and_keeps_its_headers() {
        let handler = axum::routing::post(|| async {
            (
                StatusCode::PAYLOAD_TOO_LARGE,
                [("x-test-access", "denied-401")],
                "body too large",
            )
        });
        let request = axum::http::Request::builder()
            .method(Method::POST)
            .uri("/scim/v2/item")
            .body(axum::body::Body::empty())
            .unwrap();
        let response = send(layered(handler), request).await;
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(response.headers()[header::CONTENT_TYPE], CONTENT_TYPE);
        assert_eq!(response.headers()["x-test-access"], "denied-401");
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["status"], "413");
        assert_eq!(body["detail"], "Request body too large.");
        assert_eq!(body["schemas"][0], ERROR_SCHEMA);
    }

    #[tokio::test]
    async fn a_bare_405_becomes_a_scim_error() {
        let request = axum::http::Request::builder()
            .method(Method::POST)
            .uri("/scim/v2/item")
            .body(axum::body::Body::empty())
            .unwrap();
        let response = send(layered(axum::routing::get(|| async { "ok" })), request).await;
        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(response.headers()[header::CONTENT_TYPE], CONTENT_TYPE);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["status"], "405");
        assert_eq!(body["detail"], "Method not allowed.");
    }

    #[tokio::test]
    async fn other_paths_and_successes_pass_through() {
        let request = axum::http::Request::builder()
            .uri("/other")
            .body(axum::body::Body::empty())
            .unwrap();
        let response = send(layered(axum::routing::get(|| async { "ok" })), request).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_ne!(
            response.headers()[header::CONTENT_TYPE],
            CONTENT_TYPE,
            "a path off /scim/v2 keeps its own answer"
        );
    }

    #[test]
    fn the_layer_details_never_echo_a_problem() {
        assert_eq!(
            layer_detail(StatusCode::UNAUTHORIZED),
            "Invalid or missing bearer token."
        );
        assert_eq!(layer_detail(StatusCode::IM_A_TEAPOT), "Request failed.");
    }
}
