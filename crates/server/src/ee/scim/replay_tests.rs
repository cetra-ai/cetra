// SPDX-License-Identifier: Elastic-2.0
//! Replays the Okta and Entra ID SCIM requests of `fixtures/scim/` (track EA M6, G5). Each
//! scenario file is an ordered list of steps; `fixtures/scim/README.md` describes the format.

use std::collections::BTreeMap;

use axum::http::Method;
use serde::Deserialize;
use serde_json::Value;

use super::error::CONTENT_TYPE;
use super::test_helpers::{bearer, scim_org};
use crate::auth::OrgRole;
use crate::test_support::TestApp;

/// One request of a scenario, after the `{name}` substitution.
#[derive(Deserialize)]
struct Step {
    method: String,
    path: String,
    body: Option<Value>,
    expect: Expect,
}

/// What a step's response must show: its status, the values to save, and the JSON checks.
#[derive(Deserialize)]
struct Expect {
    status: u16,
    #[serde(default)]
    save: BTreeMap<String, String>,
    #[serde(default)]
    json: BTreeMap<String, Value>,
    /// JSON pointers that must not exist in the response body.
    #[serde(default)]
    absent: Vec<String>,
}

/// Percent-encodes the value of every query parameter of `path`. Unreserved characters
/// (RFC 3986) are kept; the `?`, `&` and `=` separators are kept too.
fn encode_query(path: &str) -> String {
    let Some((base, query)) = path.split_once('?') else {
        return path.to_owned();
    };
    let pairs: Vec<String> = query
        .split('&')
        .map(|pair| match pair.split_once('=') {
            Some((key, value)) => format!("{key}={}", percent_encode(value)),
            None => pair.to_owned(),
        })
        .collect();
    format!("{base}?{}", pairs.join("&"))
}

fn percent_encode(text: &str) -> String {
    text.bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                char::from(byte).to_string()
            }
            _ => format!("%{byte:02X}"),
        })
        .collect()
}

/// Runs the steps of `scenario` in order against a licensed org with SCIM on and `acme.test`
/// verified. Each step's `{name}` placeholders are replaced by the values saved so far, on the
/// serialized step, before the step is parsed.
async fn replay(scenario: &str) {
    let app = TestApp::with_license(&["scim", "org_sso"]).await;
    let seeded = app.seed().await;
    let token = scim_org(&app, &seeded, "acme.test", OrgRole::Viewer).await;
    let steps: Vec<Value> = serde_json::from_str(scenario).expect("a scenario is a JSON array");
    let mut saved: BTreeMap<String, String> = BTreeMap::new();

    for (index, raw) in steps.iter().enumerate() {
        let mut text = serde_json::to_string(raw).unwrap();
        for (name, value) in &saved {
            text = text.replace(&format!("{{{name}}}"), value);
        }
        let step: Step = serde_json::from_str(&text).expect("a step has method, path and expect");
        let label = format!("step {index}: {} {}", step.method, step.path);

        let method = Method::from_bytes(step.method.as_bytes()).expect("a valid HTTP method");
        let path = encode_query(&step.path);
        let mut request = app
            .request(method, &path)
            .header("authorization", &bearer(&token))
            .header("content-type", CONTENT_TYPE);
        if let Some(body) = &step.body {
            request = request.body(serde_json::to_vec(body).unwrap());
        }
        let response = request.send().await;
        assert_eq!(response.status.as_u16(), step.expect.status, "{label}");

        if step.expect.save.is_empty()
            && step.expect.json.is_empty()
            && step.expect.absent.is_empty()
        {
            continue;
        }
        let body: Value = response.json();
        for (name, pointer) in &step.expect.save {
            let value = body
                .pointer(pointer)
                .and_then(Value::as_str)
                .unwrap_or_else(|| panic!("{label}: no string at {pointer}"));
            saved.insert(name.clone(), value.to_owned());
        }
        for (pointer, expected) in &step.expect.json {
            assert_eq!(body.pointer(pointer), Some(expected), "{label}: {pointer}");
        }
        for pointer in &step.expect.absent {
            assert!(
                body.pointer(pointer).is_none(),
                "{label}: {pointer} is present"
            );
        }
    }
}

#[tokio::test]
async fn okta_connection_replays() {
    replay(include_str!(
        "../../../../../fixtures/scim/okta/connection.json"
    ))
    .await;
}

#[tokio::test]
async fn okta_user_lifecycle_replays() {
    replay(include_str!(
        "../../../../../fixtures/scim/okta/user_lifecycle.json"
    ))
    .await;
}

#[tokio::test]
async fn okta_groups_replays() {
    replay(include_str!(
        "../../../../../fixtures/scim/okta/groups.json"
    ))
    .await;
}

#[tokio::test]
async fn entra_connection_replays() {
    replay(include_str!(
        "../../../../../fixtures/scim/entra/connection.json"
    ))
    .await;
}

#[tokio::test]
async fn entra_user_lifecycle_replays() {
    replay(include_str!(
        "../../../../../fixtures/scim/entra/user_lifecycle.json"
    ))
    .await;
}

#[tokio::test]
async fn entra_groups_replays() {
    replay(include_str!(
        "../../../../../fixtures/scim/entra/groups.json"
    ))
    .await;
}
