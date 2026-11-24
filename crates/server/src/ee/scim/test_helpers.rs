// SPDX-License-Identifier: Elastic-2.0
//! Shared setup for the SCIM tests of users, groups, mappings and replayed IdP requests
//! (track EA M6). Compiled for tests only, through `mod.rs`.

use axum::http::Method;
use cetra_core::ids::OrgId;
use serde_json::{Value, json};
use uuid::Uuid;

use super::error::CONTENT_TYPE;
use super::schema::USER_SCHEMA;
use super::store::{insert_token, put_org_scim};
use crate::auth::OrgRole;
use crate::auth::secret::{self, KeyKind};
use crate::ee::org_sso::store::{DomainRow, insert_domain};
use crate::test_support::{Seeded, TestApp, TestResponse};

const GROUP_SCHEMA: &str = "urn:ietf:params:scim:schemas:core:2.0:Group";

/// SCIM on for the seeded org with `default_role`, `domain` verified, and a live SCIM token.
/// Returns the token.
pub(crate) async fn scim_org(
    app: &TestApp,
    seeded: &Seeded,
    domain: &str,
    default_role: OrgRole,
) -> String {
    scim_org_in(app, seeded.org_id, seeded.user_id, domain, default_role).await
}

/// SCIM on for `org` with `default_role`, `domain` verified, and a live SCIM token created
/// by `owner`. Returns the token.
pub(crate) async fn scim_org_in(
    app: &TestApp,
    org: OrgId,
    owner: Uuid,
    domain: &str,
    default_role: OrgRole,
) -> String {
    let now = app.state.clock.now_ns();
    let key = secret::new_key(KeyKind::Scim).unwrap();
    let (prefix, hash, plaintext) = (key.prefix.clone(), key.hash.clone(), key.plaintext.clone());
    let domain = domain.to_owned();
    app.state
        .meta
        .write(move |conn| {
            Box::pin(async move {
                put_org_scim(conn, org, true, default_role, now).await?;
                insert_token(conn, org, "test", &prefix, &hash, owner, now).await?;
                insert_domain(
                    conn,
                    &DomainRow {
                        id: Uuid::now_v7(),
                        org_id: org,
                        domain,
                        token: "token".to_owned(),
                        verified_at: Some(now),
                        last_checked_at: Some(now),
                        created_at: now,
                    },
                )
                .await
            })
        })
        .await
        .unwrap();
    plaintext
}

pub(crate) fn bearer(token: &str) -> String {
    format!("Bearer {token}")
}

/// POST `/scim/v2/Users` with a User body that carries only the `userName`.
pub(crate) async fn create_user(app: &TestApp, token: &str, user_name: &str) -> TestResponse {
    scim_request(
        app,
        token,
        Method::POST,
        "/scim/v2/Users",
        &json!({"schemas": [USER_SCHEMA], "userName": user_name}),
    )
    .await
}

/// POST `/scim/v2/Groups` with `displayName` and one `members` entry per SCIM user id.
pub(crate) async fn create_group(
    app: &TestApp,
    token: &str,
    display_name: &str,
    members: &[&str],
) -> TestResponse {
    let members: Vec<Value> = members.iter().map(|id| json!({"value": id})).collect();
    scim_request(
        app,
        token,
        Method::POST,
        "/scim/v2/Groups",
        &json!({"schemas": [GROUP_SCHEMA], "displayName": display_name, "members": members}),
    )
    .await
}

/// Sends `body` as `application/scim+json` with the bearer `token`.
pub(crate) async fn scim_request(
    app: &TestApp,
    token: &str,
    method: Method,
    path: &str,
    body: &Value,
) -> TestResponse {
    app.request(method, path)
        .header("authorization", &bearer(token))
        .header("content-type", CONTENT_TYPE)
        .body(serde_json::to_vec(body).unwrap())
        .send()
        .await
}
