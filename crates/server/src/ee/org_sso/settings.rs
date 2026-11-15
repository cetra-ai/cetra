// SPDX-License-Identifier: Elastic-2.0
//! Org SSO settings: the identity provider and its domains (track P M5, D-P-51, D-P-61).
#![cfg_attr(not(test), allow(dead_code))]

use axum::extract::{Extension, State};
use axum::http::{Method, StatusCode};
use axum::routing::{delete, get, post, put};
use llmobs_core::ids::OrgId;
use serde::Deserialize;
use uuid::Uuid;

use super::store::{DomainRow, ProviderRow, ProviderWrite};
use super::{dns, instance_claims, normalize_domain, store};
use crate::api::common::FieldError;
use crate::api::entitlements::Feature;
use crate::api::org_sso::{
    AddOrgSsoDomain, DomainStatus, JitRole, OrgSso as OrgSsoView, OrgSsoDomain, OrgSsoProvider,
    PutOrgSsoProvider,
};
use crate::auth::principal::{self, Principal};
use crate::auth::roles::OrgPerm;
use crate::config::{Secret, issuer_problem};
use crate::entitlements::require;
use crate::http::AppState;
use crate::http::access::{Access, OrgAccess, RouteDef};
use crate::http::error::{self, ApiError};
use crate::meta::MetaError;

/// The org SSO settings routes. Owners only, session only (D-P-51).
pub(crate) fn routes() -> Vec<RouteDef> {
    vec![
        RouteDef {
            method: Method::GET,
            path: "/api/v1/orgs/{org}/sso",
            access: Access::Org {
                perm: OrgPerm::ManageSso,
                session_only: true,
            },
            documented: true,
            handler: get(get_org_sso),
        },
        RouteDef {
            method: Method::PUT,
            path: "/api/v1/orgs/{org}/sso/provider",
            access: Access::Org {
                perm: OrgPerm::ManageSso,
                session_only: true,
            },
            documented: true,
            handler: put(put_org_sso_provider),
        },
        RouteDef {
            method: Method::DELETE,
            path: "/api/v1/orgs/{org}/sso/provider",
            access: Access::Org {
                perm: OrgPerm::ManageSso,
                session_only: true,
            },
            documented: true,
            handler: delete(delete_org_sso_provider),
        },
        RouteDef {
            method: Method::POST,
            path: "/api/v1/orgs/{org}/sso/domains",
            access: Access::Org {
                perm: OrgPerm::ManageSso,
                session_only: true,
            },
            documented: true,
            handler: post(post_org_sso_domain),
        },
        RouteDef {
            method: Method::POST,
            path: "/api/v1/orgs/{org}/sso/domains/{id}/verify",
            access: Access::Org {
                perm: OrgPerm::ManageSso,
                session_only: true,
            },
            documented: true,
            handler: post(post_org_sso_domain_verify),
        },
        RouteDef {
            method: Method::DELETE,
            path: "/api/v1/orgs/{org}/sso/domains/{id}",
            access: Access::Org {
                perm: OrgPerm::ManageSso,
                session_only: true,
            },
            documented: true,
            handler: delete(delete_org_sso_domain),
        },
    ]
}

/// At most this many domains per org (D-P-54).
const MAX_DOMAINS: usize = 20;

#[derive(Debug, Deserialize)]
struct IdPath {
    id: Uuid,
}

/// The settings view: provider (without secret), domains, callback URL.
async fn view(state: &AppState, org: OrgId) -> Result<OrgSsoView, ApiError> {
    let provider = store::provider(state.meta.reader(), org).await?;
    let domains = store::domains(state.meta.reader(), org).await?;
    Ok(OrgSsoView {
        callback_url: state.config.public_url.join("/api/auth/callback/sso"),
        provider: provider.map(provider_view),
        domains: domains.into_iter().map(domain_view).collect(),
    })
}

fn provider_view(row: ProviderRow) -> OrgSsoProvider {
    OrgSsoProvider {
        display_name: row.display_name,
        issuer: row.issuer,
        client_id: row.client_id,
        enabled: row.enabled,
        enforced: row.enforced,
        jit_role: row.jit_role,
        last_login_at: row.last_login_at,
        created_at: row.created_at,
        updated_at: row.updated_at,
    }
}

fn domain_view(row: DomainRow) -> OrgSsoDomain {
    let verified = row.verified_at.is_some();
    OrgSsoDomain {
        id: row.id,
        txt_name: dns::record_name(&row.domain),
        txt_value: dns::record_value(&row.token),
        domain: row.domain,
        status: if verified {
            DomainStatus::Verified
        } else {
            DomainStatus::Pending
        },
        verified_at: row.verified_at,
        last_checked_at: row.last_checked_at,
        created_at: row.created_at,
    }
}

async fn get_org_sso(
    State(state): State<AppState>,
    Extension(access): Extension<OrgAccess>,
) -> Result<error::Json<OrgSsoView>, ApiError> {
    Ok(error::Json(view(&state, access.org.id).await?))
}

async fn put_org_sso_provider(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Extension(access): Extension<OrgAccess>,
    error::Json(body): error::Json<PutOrgSsoProvider>,
) -> Result<error::Json<OrgSsoView>, ApiError> {
    let org = access.org.id;
    let caller = principal::session_user(&principal)?.id;
    require(&state, org, Feature::OrgSso)?;
    let input = validate(body)?;

    let existing = store::provider(state.meta.reader(), org).await?;
    let changes_identity = existing
        .as_ref()
        .is_some_and(|row| row.issuer != input.issuer || row.client_id != input.client_id);
    let secret = match (&input.secret, &existing) {
        (Some(secret), _) => secret.clone(),
        (None, Some(row)) if !changes_identity => open_stored(&state, row)?,
        (None, Some(_)) => {
            return Err(field_error(
                "client_secret",
                "required when the issuer or client id changes",
            ));
        }
        (None, None) => return Err(field_error("client_secret", "required")),
    };

    // Discovery, before the write lock: a bad issuer is refused before anything is stored.
    let oidc = state
        .org_sso
        .build_provider(
            org,
            &input.display_name,
            &input.issuer,
            &input.client_id,
            Secret::new(secret),
        )
        .map_err(|_| field_error("issuer", "invalid issuer"))?;
    let now = state.clock.now_ns();
    oidc.metadata(now).await.map_err(|_| {
        field_error(
            "issuer",
            "the issuer's discovery document could not be loaded",
        )
    })?;

    // The row id is fixed before the write: a new secret is sealed to it.
    let id = existing.as_ref().map_or_else(Uuid::now_v7, |row| row.id);
    let sealed = match &input.secret {
        Some(secret) => Some(
            state
                .secrets
                .seal(secret.as_bytes(), store::secret_aad(&id))
                .map_err(|error| ApiError::Internal(Box::new(error)))?,
        ),
        None => None,
    };
    let renames_issuer = existing
        .as_ref()
        .is_some_and(|row| row.issuer != input.issuer);
    let before = existing;
    let outcome = state
        .meta
        .write(move |conn| {
            Box::pin(async move {
                let current = store::provider_tx(conn, org).await?;
                if !unchanged(before.as_ref(), current.as_ref()) {
                    return Ok(Save::Changed);
                }
                store::put_provider(
                    conn,
                    org,
                    id,
                    ProviderWrite {
                        display_name: input.display_name,
                        issuer: input.issuer,
                        client_id: input.client_id,
                        client_secret_enc: sealed,
                        jit_role: input.jit_role,
                    },
                    now,
                )
                .await?;
                if renames_issuer {
                    store::delete_identities(conn, org).await?;
                }
                Ok(Save::Saved)
            })
        })
        .await?;
    if outcome == Save::Changed {
        return Err(ApiError::Conflict {
            detail: "The identity provider changed meanwhile; reload and try again.",
        });
    }
    super::refresh(&state).await;
    tracing::info!(target: "audit", org_id = %org, user_id = %caller, "org sso provider saved");
    Ok(error::Json(view(&state, org).await?))
}

async fn delete_org_sso_provider(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Extension(access): Extension<OrgAccess>,
) -> Result<StatusCode, ApiError> {
    let org = access.org.id;
    let caller = principal::session_user(&principal)?.id;
    let outcome = state
        .meta
        .write(move |conn| {
            Box::pin(async move {
                let Some(row) = store::provider_tx(conn, org).await? else {
                    return Ok(Remove::Missing);
                };
                if row.enforced {
                    return Ok(Remove::Enforced);
                }
                store::delete_provider(conn, org).await?;
                Ok(Remove::Removed)
            })
        })
        .await?;
    match outcome {
        Remove::Missing => Err(ApiError::NotFound),
        Remove::Enforced => Err(ApiError::Conflict {
            detail: "Turn off enforcement before removing the identity provider.",
        }),
        Remove::Removed => {
            super::refresh(&state).await;
            tracing::info!(target: "audit", org_id = %org, user_id = %caller, "org sso provider removed");
            Ok(StatusCode::NO_CONTENT)
        }
    }
}

async fn post_org_sso_domain(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Extension(access): Extension<OrgAccess>,
    error::Json(body): error::Json<AddOrgSsoDomain>,
) -> Result<(StatusCode, error::Json<OrgSsoView>), ApiError> {
    let org = access.org.id;
    let caller = principal::session_user(&principal)?.id;
    require(&state, org, Feature::OrgSso)?;
    let domain =
        normalize_domain(&body.domain).ok_or_else(|| field_error("domain", "invalid domain"))?;
    if instance_claims(&state.config.auth, &domain) {
        return Err(field_error(
            "domain",
            "this domain is managed by the server configuration",
        ));
    }
    let id = Uuid::now_v7();
    let token = dns::new_token().map_err(|error| ApiError::Internal(Box::new(error)))?;
    let now = state.clock.now_ns();
    let row = DomainRow {
        id,
        org_id: org,
        domain,
        token,
        verified_at: None,
        last_checked_at: None,
        created_at: now,
    };
    let outcome = state
        .meta
        .write(move |conn| {
            Box::pin(async move {
                if store::domains_tx(conn, org).await?.len() >= MAX_DOMAINS {
                    return Ok(Add::TooMany);
                }
                match store::insert_domain(conn, &row).await {
                    Ok(()) => Ok(Add::Added),
                    Err(MetaError::Conflict(_)) => Ok(Add::Listed),
                    Err(error) => Err(error),
                }
            })
        })
        .await?;
    match outcome {
        Add::TooMany => Err(field_error(
            "domain",
            "an organization can have at most 20 domains",
        )),
        Add::Listed => Err(ApiError::Conflict {
            detail: "This domain is already listed.",
        }),
        Add::Added => {
            tracing::info!(target: "audit", org_id = %org, user_id = %caller, domain_id = %id, "org sso domain added");
            Ok((StatusCode::CREATED, error::Json(view(&state, org).await?)))
        }
    }
}

async fn post_org_sso_domain_verify(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Extension(access): Extension<OrgAccess>,
    error::Path(IdPath { id }): error::Path<IdPath>,
) -> Result<error::Json<OrgSsoView>, ApiError> {
    let org = access.org.id;
    let caller = principal::session_user(&principal)?.id;
    require(&state, org, Feature::OrgSso)?;
    let row = store::domain(state.meta.reader(), org, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if row.verified_at.is_some() {
        return Ok(error::Json(view(&state, org).await?));
    }
    let now = state.clock.now_ns();
    state
        .limits
        .sso_dns_checks
        .check(&org, now)
        .map_err(|retry| ApiError::RateLimited {
            retry_after_s: retry.0.as_secs().max(1),
        })?;
    let values = match state.org_sso.dns.txt(&dns::record_name(&row.domain)).await {
        Ok(values) => values,
        Err(_) => {
            state
                .meta
                .write(move |conn| {
                    Box::pin(async move { store::mark_checked(conn, id, now).await })
                })
                .await?;
            return Err(ApiError::Unavailable {
                detail: "The DNS lookup failed. Try again later.",
            });
        }
    };
    let found = has(&values, &dns::record_value(&row.token));
    let domain = row.domain.clone();
    let outcome = state
        .meta
        .write(move |conn| {
            Box::pin(async move {
                if !found {
                    store::mark_checked(conn, id, now).await?;
                    return Ok(Check::Missing);
                }
                match store::verified_row_tx(conn, &domain).await? {
                    Some(holder) if holder.org_id == org => {
                        store::mark_checked(conn, id, now).await?;
                        Ok(Check::Verified { taken_from: None })
                    }
                    Some(holder) => {
                        // D-P-54: the holder's record is gone from the same answer, so the
                        // domain moves. An answer that still holds the holder's record keeps it.
                        if has(&values, &dns::record_value(&holder.token)) {
                            store::mark_checked(conn, id, now).await?;
                            Ok(Check::Elsewhere)
                        } else {
                            store::mark_unverified(conn, holder.id).await?;
                            store::mark_verified(conn, id, now).await?;
                            Ok(Check::Verified {
                                taken_from: Some(holder.org_id),
                            })
                        }
                    }
                    None => match store::mark_verified(conn, id, now).await {
                        Ok(()) => Ok(Check::Verified { taken_from: None }),
                        Err(MetaError::Conflict(_)) => {
                            store::mark_checked(conn, id, now).await?;
                            Ok(Check::Elsewhere)
                        }
                        Err(error) => Err(error),
                    },
                }
            })
        })
        .await?;
    match outcome {
        Check::Missing => Err(ApiError::Conflict {
            detail: "The TXT record was not found. DNS changes can take a while to appear; try again later.",
        }),
        Check::Elsewhere => Err(ApiError::Conflict {
            detail: "This domain is verified by another organization.",
        }),
        Check::Verified { taken_from } => {
            super::refresh(&state).await;
            tracing::info!(target: "audit", org_id = %org, user_id = %caller, domain_id = %id, "org sso domain verified");
            if let Some(from) = taken_from {
                tracing::info!(target: "audit", org_id = %org, from_org_id = %from, domain_id = %id, "org sso domain taken over");
            }
            Ok(error::Json(view(&state, org).await?))
        }
    }
}

async fn delete_org_sso_domain(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Extension(access): Extension<OrgAccess>,
    error::Path(IdPath { id }): error::Path<IdPath>,
) -> Result<StatusCode, ApiError> {
    let org = access.org.id;
    let caller = principal::session_user(&principal)?.id;
    let removed = state
        .meta
        .write(move |conn| Box::pin(async move { store::delete_domain(conn, org, id).await }))
        .await?;
    if !removed {
        return Err(ApiError::NotFound);
    }
    super::refresh(&state).await;
    tracing::info!(target: "audit", org_id = %org, user_id = %caller, domain_id = %id, "org sso domain removed");
    Ok(StatusCode::NO_CONTENT)
}

/// Whether one of the TXT strings is `expected`, ignoring surrounding whitespace and one
/// pair of quotes.
fn has(values: &[String], expected: &str) -> bool {
    values.iter().any(|value| {
        let value = value.trim();
        let value = value
            .strip_prefix('"')
            .and_then(|rest| rest.strip_suffix('"'))
            .unwrap_or(value);
        value == expected
    })
}

/// What the add found and did.
#[derive(Debug, PartialEq, Eq)]
enum Add {
    Added,
    /// The org already lists the domain.
    Listed,
    TooMany,
}

/// What the verification found and did.
#[derive(Debug, PartialEq, Eq)]
enum Check {
    /// The TXT record is not in the answer.
    Missing,
    /// Another org holds the domain and its record is still in the answer.
    Elsewhere,
    /// The domain is this org's now. `taken_from` is the org it moved from.
    Verified { taken_from: Option<OrgId> },
}

/// What the write found and did.
#[derive(Debug, PartialEq, Eq)]
enum Save {
    Saved,
    /// The provider changed between the read and the write.
    Changed,
}

/// What the delete found and did.
#[derive(Debug, PartialEq, Eq)]
enum Remove {
    Missing,
    Enforced,
    Removed,
}

/// Whether the provider the write saw is the one the request read: the same row, issuer,
/// client id and sealed secret (both absent counts as the same).
fn unchanged(before: Option<&ProviderRow>, now: Option<&ProviderRow>) -> bool {
    match (before, now) {
        (None, None) => true,
        (Some(before), Some(now)) => {
            before.id == now.id
                && before.issuer == now.issuer
                && before.client_id == now.client_id
                && before.client_secret_enc == now.client_secret_enc
        }
        _ => false,
    }
}

/// A request body after validation.
struct Validated {
    display_name: String,
    issuer: String,
    client_id: String,
    /// `None` keeps the stored secret.
    secret: Option<String>,
    jit_role: JitRole,
}

fn validate(body: PutOrgSsoProvider) -> Result<Validated, ApiError> {
    let display_name = body.display_name.trim().to_owned();
    if !(1..=100).contains(&display_name.chars().count()) {
        return Err(field_error("display_name", "must be 1 to 100 characters"));
    }
    let issuer = body.issuer.trim().to_owned();
    if issuer.len() > 512 {
        return Err(field_error("issuer", "must be at most 512 bytes"));
    }
    if let Some(reason) = issuer_problem(&issuer) {
        return Err(field_error("issuer", reason));
    }
    let client_id = body.client_id.trim().to_owned();
    if !(1..=255).contains(&client_id.chars().count()) {
        return Err(field_error("client_id", "must be 1 to 255 characters"));
    }
    let secret = match body.client_secret {
        None => None,
        Some(secret) => {
            let secret = secret.trim().to_owned();
            if !(1..=1024).contains(&secret.chars().count()) {
                return Err(field_error("client_secret", "must be 1 to 1024 characters"));
            }
            Some(secret)
        }
    };
    Ok(Validated {
        display_name,
        issuer,
        client_id,
        secret,
        jit_role: body.jit_role,
    })
}

/// The stored secret, opened. A secret that does not open is asked for again.
fn open_stored(state: &AppState, row: &ProviderRow) -> Result<String, ApiError> {
    state
        .secrets
        .open(&row.client_secret_enc, store::secret_aad(&row.id))
        .ok()
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .ok_or_else(|| field_error("client_secret", "required"))
}

fn field_error(field: &str, message: &str) -> ApiError {
    ApiError::BadRequest {
        detail: None,
        errors: vec![FieldError {
            field: field.to_owned(),
            message: message.to_owned(),
            position: None,
        }],
    }
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;
    use serde_json::{Value, json};

    use super::*;
    use crate::api::account::OrgRole;
    use crate::meta::identities::insert_identity;
    use crate::test_support::mock_idp::{CLIENT_ID, CLIENT_SECRET, MockIdp};
    use crate::test_support::{
        TestApp, assert_logs_clean, org_sso_app, org_sso_app_with, second_org,
    };

    const PATH: &str = "/api/v1/orgs/acme/sso";
    const PROVIDER_PATH: &str = "/api/v1/orgs/acme/sso/provider";

    fn body(issuer: &str, secret: Option<&str>, display_name: &str) -> Value {
        let mut body = json!({
            "display_name": display_name,
            "issuer": issuer,
            "client_id": CLIENT_ID,
            "jit_role": "member",
        });
        if let Some(secret) = secret {
            body["client_secret"] = json!(secret);
        }
        body
    }

    /// Writes a provider through `store`, sealed to CLIENT_SECRET, with no HTTP.
    async fn seed_provider(app: &TestApp, org: OrgId, issuer: &str) -> Uuid {
        let id = Uuid::now_v7();
        let sealed = app
            .state
            .secrets
            .seal(CLIENT_SECRET.as_bytes(), store::secret_aad(&id))
            .expect("seal");
        let now = app.state.clock.now_ns();
        let issuer = issuer.to_owned();
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    let write = ProviderWrite {
                        display_name: "Acme Okta".to_owned(),
                        issuer,
                        client_id: CLIENT_ID.to_owned(),
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

    async fn put(app: &TestApp, cookie: &str, value: &Value) -> crate::test_support::TestResponse {
        app.request(Method::PUT, PROVIDER_PATH)
            .header("cookie", cookie)
            .json(value)
            .send()
            .await
    }

    async fn get(app: &TestApp, cookie: &str) -> crate::test_support::TestResponse {
        app.request(Method::GET, PATH)
            .header("cookie", cookie)
            .send()
            .await
    }

    async fn identity_providers(app: &TestApp) -> Vec<String> {
        sqlx::query_scalar("SELECT provider FROM identities ORDER BY provider")
            .fetch_all(app.state.meta.reader())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn owner_saves_and_reads_the_provider() {
        let idp = MockIdp::start().await;
        let (app, seeded, owner) = org_sso_app().await;
        let saved = put(
            &app,
            &owner.cookie,
            &body(idp.issuer(), Some(CLIENT_SECRET), "Acme Okta"),
        )
        .await;
        assert_eq!(saved.status, StatusCode::OK);
        let view: OrgSsoView = saved.json();
        let provider = view.provider.clone().expect("provider");
        assert!(!provider.enabled);
        assert!(!provider.enforced);
        assert_eq!(provider.jit_role, JitRole::Member);
        assert!(!String::from_utf8_lossy(&saved.body).contains(CLIENT_SECRET));

        let read = get(&app, &owner.cookie).await;
        assert_eq!(read.status, StatusCode::OK);
        assert_eq!(read.json::<OrgSsoView>(), view);

        let row = store::provider(app.state.meta.reader(), seeded.org_id)
            .await
            .unwrap()
            .expect("row");
        let opened = app
            .state
            .secrets
            .open(&row.client_secret_enc, store::secret_aad(&row.id))
            .expect("open");
        assert_eq!(opened, CLIENT_SECRET.as_bytes());
    }

    #[tokio::test]
    async fn admins_and_members_cannot() {
        let (app, seeded, owner) = org_sso_app().await;
        let admin = app
            .add_member(&seeded, "admin@acme.test", OrgRole::Admin)
            .await;
        let viewer = app
            .add_member(&seeded, "viewer@acme.test", OrgRole::Viewer)
            .await;
        assert_eq!(get(&app, &admin.cookie).await.status, StatusCode::FORBIDDEN);
        assert_eq!(
            get(&app, &viewer.cookie).await.status,
            StatusCode::FORBIDDEN
        );

        let token = app.personal_token(owner.id, true).await;
        let via_token = app
            .request(Method::GET, PATH)
            .header("authorization", &format!("Bearer {token}"))
            .send()
            .await;
        assert_eq!(via_token.status, StatusCode::FORBIDDEN);

        let (_other, other_owner) = second_org(&app, "beta", "b@beta.test").await;
        assert_eq!(
            get(&app, &other_owner.cookie).await.status,
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn create_needs_a_secret_and_an_issuer_change_needs_one_too() {
        let first = MockIdp::start().await;
        let second = MockIdp::start().await;
        let (app, seeded, owner) = org_sso_app().await;

        let created = put(
            &app,
            &owner.cookie,
            &body(first.issuer(), None, "Acme Okta"),
        )
        .await;
        assert_eq!(created.status, StatusCode::BAD_REQUEST);
        assert_eq!(
            created.json::<Value>()["errors"][0]["field"],
            "client_secret"
        );

        let saved = put(
            &app,
            &owner.cookie,
            &body(first.issuer(), Some(CLIENT_SECRET), "Acme Okta"),
        )
        .await;
        assert_eq!(saved.status, StatusCode::OK);

        let user = owner.id;
        let org_provider = format!("org:{}", seeded.org_id);
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    insert_identity(conn, user, &org_provider, "sub-1", "owner@acme.test", 1)
                        .await?;
                    insert_identity(conn, user, "oidc:other", "sub-2", "owner@acme.test", 1)
                        .await?;
                    Ok(())
                })
            })
            .await
            .unwrap();

        let no_secret = put(
            &app,
            &owner.cookie,
            &body(second.issuer(), None, "Acme Okta"),
        )
        .await;
        assert_eq!(no_secret.status, StatusCode::BAD_REQUEST);
        assert_eq!(identity_providers(&app).await.len(), 2);

        let changed = put(
            &app,
            &owner.cookie,
            &body(second.issuer(), Some(CLIENT_SECRET), "Acme Okta"),
        )
        .await;
        assert_eq!(changed.status, StatusCode::OK);
        assert_eq!(
            identity_providers(&app).await,
            vec!["oidc:other".to_owned()]
        );

        let renamed = put(
            &app,
            &owner.cookie,
            &body(second.issuer(), None, "Acme Entra"),
        )
        .await;
        assert_eq!(renamed.status, StatusCode::OK);
        let row = store::provider(app.state.meta.reader(), seeded.org_id)
            .await
            .unwrap()
            .expect("row");
        let opened = app
            .state
            .secrets
            .open(&row.client_secret_enc, store::secret_aad(&row.id))
            .expect("open");
        assert_eq!(opened, CLIENT_SECRET.as_bytes());
        assert_eq!(row.display_name, "Acme Entra");
    }

    #[tokio::test]
    async fn bad_issuers_are_400() {
        let idp = MockIdp::start().await;
        let (app, _seeded, owner) = org_sso_app().await;
        for issuer in ["ftp://x", "https://login.microsoftonline.com/common/v2.0"] {
            let refused = put(
                &app,
                &owner.cookie,
                &body(issuer, Some(CLIENT_SECRET), "Acme"),
            )
            .await;
            assert_eq!(refused.status, StatusCode::BAD_REQUEST, "{issuer}");
            assert_eq!(
                refused.json::<Value>()["errors"][0]["field"],
                "issuer",
                "{issuer}"
            );
        }

        idp.fail_discovery();
        let unavailable = put(
            &app,
            &owner.cookie,
            &body(idp.issuer(), Some(CLIENT_SECRET), "Acme"),
        )
        .await;
        assert_eq!(unavailable.status, StatusCode::BAD_REQUEST);
        assert_eq!(unavailable.json::<Value>()["errors"][0]["field"], "issuer");

        let (strict, _seeded, strict_owner) =
            org_sso_app_with(|config| config.security.allow_private_network_egress = false).await;
        let idp = MockIdp::start().await;
        let guarded = put(
            &strict,
            &strict_owner.cookie,
            &body(idp.issuer(), Some(CLIENT_SECRET), "Acme"),
        )
        .await;
        assert_eq!(guarded.status, StatusCode::BAD_REQUEST);
        assert_eq!(guarded.json::<Value>()["errors"][0]["field"], "issuer");
    }

    #[tokio::test]
    async fn writes_need_the_feature_and_removal_does_not() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let owner = app
            .add_member(&seeded, "owner@acme.test", OrgRole::Owner)
            .await;
        let locked = put(
            &app,
            &owner.cookie,
            &body("https://idp.example", Some(CLIENT_SECRET), "Acme"),
        )
        .await;
        assert_eq!(locked.status, StatusCode::FORBIDDEN);
        assert_eq!(
            locked.json::<Value>()["detail"],
            Feature::OrgSso.locked_detail()
        );

        seed_provider(&app, seeded.org_id, "https://idp.example").await;
        assert_eq!(get(&app, &owner.cookie).await.status, StatusCode::OK);
        let removed = app
            .request(Method::DELETE, PROVIDER_PATH)
            .header("cookie", &owner.cookie)
            .send()
            .await;
        assert_eq!(removed.status, StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn an_enforced_provider_cannot_be_removed() {
        let (app, seeded, owner) = org_sso_app().await;
        seed_provider(&app, seeded.org_id, "https://idp.example").await;
        let org = seeded.org_id;
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    store::set_flags(conn, org, true, true, 1).await?;
                    Ok(())
                })
            })
            .await
            .unwrap();
        let refused = app
            .request(Method::DELETE, PROVIDER_PATH)
            .header("cookie", &owner.cookie)
            .send()
            .await;
        assert_eq!(refused.status, StatusCode::CONFLICT);
    }

    #[test]
    fn a_changed_provider_is_detected_at_write_time() {
        let row = |secret: Vec<u8>, issuer: &str| ProviderRow {
            id: Uuid::nil(),
            org_id: OrgId::new_v7(),
            display_name: "Acme".to_owned(),
            issuer: issuer.to_owned(),
            client_id: "c".to_owned(),
            client_secret_enc: secret,
            enabled: false,
            enforced: false,
            jit_role: JitRole::Viewer,
            last_login_at: None,
            created_at: 1,
            updated_at: 1,
        };
        let before = row(vec![1, 2], "https://idp.example");
        assert!(unchanged(None, None));
        assert!(unchanged(
            Some(&before),
            Some(&row(vec![1, 2], "https://idp.example"))
        ));
        assert!(!unchanged(
            Some(&before),
            Some(&row(vec![9, 9], "https://idp.example"))
        ));
        assert!(!unchanged(
            Some(&before),
            Some(&row(vec![1, 2], "https://other.example"))
        ));
        assert!(!unchanged(Some(&before), None));
        assert!(!unchanged(None, Some(&before)));
    }

    fn settings_path(slug: &str) -> String {
        format!("/api/v1/orgs/{slug}/sso")
    }

    async fn add_domain(
        app: &TestApp,
        slug: &str,
        cookie: &str,
        domain: &str,
    ) -> crate::test_support::TestResponse {
        app.request(Method::POST, &format!("{}/domains", settings_path(slug)))
            .header("cookie", cookie)
            .json(&json!({ "domain": domain }))
            .send()
            .await
    }

    async fn verify(
        app: &TestApp,
        slug: &str,
        cookie: &str,
        id: Uuid,
    ) -> crate::test_support::TestResponse {
        app.request(
            Method::POST,
            &format!("{}/domains/{id}/verify", settings_path(slug)),
        )
        .header("cookie", cookie)
        .send()
        .await
    }

    async fn view_of(app: &TestApp, slug: &str, cookie: &str) -> OrgSsoView {
        app.request(Method::GET, &settings_path(slug))
            .header("cookie", cookie)
            .send()
            .await
            .json()
    }

    async fn domain_of(app: &TestApp, slug: &str, cookie: &str, domain: &str) -> OrgSsoDomain {
        view_of(app, slug, cookie)
            .await
            .domains
            .into_iter()
            .find(|row| row.domain == domain)
            .expect("domain listed")
    }

    /// The TXT answer for one record value, quoted as DNS publishes it.
    fn quoted(value: &str) -> String {
        format!("\"{value}\"")
    }

    #[tokio::test]
    async fn add_verify_and_route() {
        let (app, seeded, owner) = org_sso_app().await;
        seed_provider(&app, seeded.org_id, "https://idp.example").await;

        let added = add_domain(&app, "acme", &owner.cookie, "Acme.test.").await;
        assert_eq!(added.status, StatusCode::CREATED);
        let row = added.json::<OrgSsoView>().domains[0].clone();
        assert_eq!(row.domain, "acme.test");
        assert_eq!(row.status, DomainStatus::Pending);
        assert_eq!(row.txt_name, "_llmobs-verify.acme.test");
        let token = row.txt_value.strip_prefix("llmobs-verify=").expect("value");
        assert_eq!(token.len(), 32);
        assert!(token.bytes().all(|byte| byte.is_ascii_hexdigit()));

        let missing = verify(&app, "acme", &owner.cookie, row.id).await;
        assert_eq!(missing.status, StatusCode::CONFLICT);
        assert!(
            missing.json::<Value>()["detail"]
                .as_str()
                .expect("detail")
                .contains("not found")
        );
        assert!(
            domain_of(&app, "acme", &owner.cookie, "acme.test")
                .await
                .last_checked_at
                .is_some()
        );

        app.dns_txt(&row.txt_name, &[quoted(&row.txt_value).as_str()]);
        let verified = verify(&app, "acme", &owner.cookie, row.id).await;
        assert_eq!(verified.status, StatusCode::OK);
        assert_eq!(
            verified.json::<OrgSsoView>().domains[0].status,
            DomainStatus::Verified
        );
        assert_eq!(
            app.state
                .org_sso
                .route("ada@acme.test")
                .map(|route| route.org_id),
            Some(seeded.org_id)
        );

        // Verified: no further lookup, so a changed answer changes nothing.
        app.dns_txt(&row.txt_name, &["something else"]);
        assert_eq!(
            verify(&app, "acme", &owner.cookie, row.id).await.status,
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn one_org_per_domain_and_a_stale_domain_moves() {
        let (app, seeded, owner) = org_sso_app().await;
        seed_provider(&app, seeded.org_id, "https://idp.example").await;
        add_domain(&app, "acme", &owner.cookie, "acme.test").await;
        let acme = domain_of(&app, "acme", &owner.cookie, "acme.test").await;
        app.dns_txt(&acme.txt_name, &[quoted(&acme.txt_value).as_str()]);
        assert_eq!(
            verify(&app, "acme", &owner.cookie, acme.id).await.status,
            StatusCode::OK
        );

        let (beta_org, beta) = second_org(&app, "beta", "b@beta.test").await;
        seed_provider(&app, beta_org, "https://idp.example").await;
        assert_eq!(
            add_domain(&app, "beta", &beta.cookie, "acme.test")
                .await
                .status,
            StatusCode::CREATED
        );
        let beta_row = domain_of(&app, "beta", &beta.cookie, "acme.test").await;
        // Both records in one answer: acme keeps the domain.
        app.dns_txt(
            &acme.txt_name,
            &[
                quoted(&acme.txt_value).as_str(),
                quoted(&beta_row.txt_value).as_str(),
            ],
        );
        let refused = verify(&app, "beta", &beta.cookie, beta_row.id).await;
        assert_eq!(refused.status, StatusCode::CONFLICT);
        assert!(
            refused.json::<Value>()["detail"]
                .as_str()
                .expect("detail")
                .contains("another organization")
        );

        // acme's record is gone: the domain moves to beta.
        app.dns_txt(&acme.txt_name, &[quoted(&beta_row.txt_value).as_str()]);
        let moved = verify(&app, "beta", &beta.cookie, beta_row.id).await;
        assert_eq!(moved.status, StatusCode::OK);
        assert_eq!(
            domain_of(&app, "acme", &owner.cookie, "acme.test")
                .await
                .status,
            DomainStatus::Pending
        );
        assert_eq!(
            app.state
                .org_sso
                .route("x@acme.test")
                .map(|route| route.org_id),
            Some(beta_org)
        );
    }

    #[tokio::test]
    async fn instance_config_owns_some_domains() {
        let (app, _seeded, owner) = org_sso_app_with(|config| {
            config.auth.enforce_sso_domains = vec!["corp.test".to_owned()];
        })
        .await;
        let refused = add_domain(&app, "acme", &owner.cookie, "corp.test").await;
        assert_eq!(refused.status, StatusCode::BAD_REQUEST);
        assert_eq!(refused.json::<Value>()["errors"][0]["field"], "domain");
    }

    #[tokio::test]
    async fn domain_limits_hold() {
        let (app, _seeded, owner) = org_sso_app().await;
        for index in 0..20 {
            let added = add_domain(&app, "acme", &owner.cookie, &format!("d{index}.test")).await;
            assert_eq!(added.status, StatusCode::CREATED, "{index}");
        }
        assert_eq!(
            add_domain(&app, "acme", &owner.cookie, "d20.test")
                .await
                .status,
            StatusCode::BAD_REQUEST
        );

        let (app, _seeded, owner) = org_sso_app().await;
        add_domain(&app, "acme", &owner.cookie, "acme.test").await;
        let row = domain_of(&app, "acme", &owner.cookie, "acme.test").await;
        for _ in 0..10 {
            assert_eq!(
                verify(&app, "acme", &owner.cookie, row.id).await.status,
                StatusCode::CONFLICT
            );
        }
        assert_eq!(
            verify(&app, "acme", &owner.cookie, row.id).await.status,
            StatusCode::TOO_MANY_REQUESTS
        );
    }

    #[tokio::test]
    async fn dns_failure_is_503() {
        let (app, _seeded, owner) = org_sso_app().await;
        add_domain(&app, "acme", &owner.cookie, "acme.test").await;
        let row = domain_of(&app, "acme", &owner.cookie, "acme.test").await;
        app.dns_txt(&row.txt_name, &[quoted(&row.txt_value).as_str()]);

        app.dns_fail(true);
        assert_eq!(
            verify(&app, "acme", &owner.cookie, row.id).await.status,
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert!(
            domain_of(&app, "acme", &owner.cookie, "acme.test")
                .await
                .last_checked_at
                .is_some()
        );

        app.dns_fail(false);
        assert_eq!(
            verify(&app, "acme", &owner.cookie, row.id).await.status,
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn remove_works_without_the_feature() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let owner = app
            .add_member(&seeded, "owner@acme.test", OrgRole::Owner)
            .await;
        let id = Uuid::now_v7();
        let org = seeded.org_id;
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    let row = DomainRow {
                        id,
                        org_id: org,
                        domain: "acme.test".to_owned(),
                        token: "0123456789abcdef0123456789abcdef".to_owned(),
                        verified_at: None,
                        last_checked_at: None,
                        created_at: 1,
                    };
                    store::insert_domain(conn, &row).await?;
                    Ok(())
                })
            })
            .await
            .unwrap();

        let added = add_domain(&app, "acme", &owner.cookie, "other.test").await;
        assert_eq!(added.status, StatusCode::FORBIDDEN);
        assert_eq!(
            added.json::<Value>()["detail"],
            Feature::OrgSso.locked_detail()
        );

        let removed = app
            .request(
                Method::DELETE,
                &format!("{}/domains/{id}", settings_path("acme")),
            )
            .header("cookie", &owner.cookie)
            .send()
            .await;
        assert_eq!(removed.status, StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn logs_carry_no_domain() {
        let (app, seeded, owner) = org_sso_app().await;
        seed_provider(&app, seeded.org_id, "https://idp.example").await;
        assert_logs_clean(&["acme.test", "llmobs-verify="], async {
            add_domain(&app, "acme", &owner.cookie, "acme.test").await;
            let row = domain_of(&app, "acme", &owner.cookie, "acme.test").await;
            app.dns_txt(&row.txt_name, &[quoted(&row.txt_value).as_str()]);
            verify(&app, "acme", &owner.cookie, row.id).await;
            app.request(
                Method::DELETE,
                &format!("{}/domains/{}", settings_path("acme"), row.id),
            )
            .header("cookie", &owner.cookie)
            .send()
            .await;
        })
        .await;
    }

    #[tokio::test]
    async fn logs_carry_no_secret() {
        let idp = MockIdp::start().await;
        let (app, _seeded, owner) = org_sso_app().await;
        let saved = assert_logs_clean(&[CLIENT_SECRET], async {
            put(
                &app,
                &owner.cookie,
                &body(idp.issuer(), Some(CLIENT_SECRET), "Acme Okta"),
            )
            .await
        })
        .await;
        assert_eq!(saved.status, StatusCode::OK);
    }
}
