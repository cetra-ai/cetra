// SPDX-License-Identifier: Elastic-2.0
//! Org SSO settings: the identity provider and its domains (track P M5, D-P-51, D-P-61).

use axum::extract::{Extension, State};
use axum::http::{Method, StatusCode};
use axum::routing::{delete, get, patch, post, put};
use cetra_core::ids::OrgId;
use cetra_saml::{IdpCertificate, IdpMetadata, MetadataError, parse_metadata};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use sqlx::{SqliteConnection, SqlitePool};
use uuid::Uuid;

use super::store::{DomainRow, ProviderRow, ProviderWrite, SamlAttributes, SamlRow, SamlWrite};
use super::{MetadataFetchError, dns, instance_claims, normalize_domain, sp_urls, store};
use crate::api::common::FieldError;
use crate::api::common::ProblemCode;
use crate::api::entitlements::Feature;
use crate::api::org_sso::{
    AddOrgSsoDomain, DomainStatus, JitRole, OrgSso as OrgSsoView, OrgSsoCertificate, OrgSsoDomain,
    OrgSsoProvider, OrgSsoSaml, OrgSsoSamlSp, PatchOrgSso, PutOrgSamlProvider, PutOrgSsoProvider,
    SamlAttributeNames, SamlRefreshError, SsoProtocol,
};
use crate::auth::principal::{self, Principal};
use crate::auth::roles::OrgPerm;
use crate::config::{Secret, issuer_problem};
use crate::entitlements::require;
use crate::http::AppState;
use crate::http::access::{Access, OrgAccess, RouteDef};
use crate::http::error::{self, ApiError};
use crate::http::restore::refuse_outbound;
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
            method: Method::PATCH,
            path: "/api/v1/orgs/{org}/sso",
            access: Access::Org {
                perm: OrgPerm::ManageSso,
                session_only: true,
            },
            documented: true,
            handler: patch(patch_org_sso),
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
            method: Method::PUT,
            path: "/api/v1/orgs/{org}/sso/saml",
            access: Access::Org {
                perm: OrgPerm::ManageSso,
                session_only: true,
            },
            documented: true,
            handler: put(put_org_saml_provider),
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

/// The 409 for a protocol switch while the provider is enforced (D-SA-2).
const SWITCH_ENFORCED: &str =
    "Turn off enforcement before switching the identity provider's protocol.";

/// The 409 for an IdP entity ID change without confirmation (D-SA-2). The web UI matches it.
const ENTITY_ID_CHANGE: &str =
    "Changing the IdP entity ID deletes the linked identities; confirm the change.";

/// The 400 field error when a SAML save names no complete source (D-SA-18).
const SAML_SOURCE: &str =
    "give a metadata URL, the metadata XML, or the entity ID, SSO URL and certificates";

#[derive(Debug, Deserialize)]
struct IdPath {
    id: Uuid,
}

/// The settings view: provider (without secret), domains, callback URL and SP values.
async fn view(state: &AppState, org: OrgId) -> Result<OrgSsoView, ApiError> {
    let provider = store::provider(state.meta.reader(), org).await?;
    let domains = store::domains(state.meta.reader(), org).await?;
    let provider = match provider {
        Some(row) => Some(provider_view(state.meta.reader(), row).await?),
        None => None,
    };
    let sp = sp_urls(&state.config.public_url, org);
    Ok(OrgSsoView {
        callback_url: state.config.public_url.join("/api/auth/callback/sso"),
        saml_sp: OrgSsoSamlSp {
            entity_id: sp.entity_id,
            acs_url: sp.acs_url,
            start_url: sp.start_url,
        },
        provider,
        domains: domains.into_iter().map(domain_view).collect(),
    })
}

/// The provider as the settings show it: the OIDC fields for an OIDC row, the SAML settings for
/// a SAML row (D-SA-18).
async fn provider_view(pool: &SqlitePool, row: ProviderRow) -> Result<OrgSsoProvider, ApiError> {
    let (issuer, client_id, saml) = match (row.protocol, row.oidc.as_ref()) {
        (SsoProtocol::Saml, _) => {
            let saml = store::saml(pool, row.id).await?;
            (None, None, saml.as_ref().map(saml_view))
        }
        (SsoProtocol::Oidc, oidc) => (
            oidc.map(|oidc| oidc.issuer.clone()),
            oidc.map(|oidc| oidc.client_id.clone()),
            None,
        ),
    };
    Ok(OrgSsoProvider {
        protocol: row.protocol,
        display_name: row.display_name,
        issuer,
        client_id,
        enabled: row.enabled,
        enforced: row.enforced,
        jit_role: row.jit_role,
        last_login_at: row.last_login_at,
        created_at: row.created_at,
        updated_at: row.updated_at,
        saml,
    })
}

fn saml_view(row: &SamlRow) -> OrgSsoSaml {
    OrgSsoSaml {
        idp_entity_id: row.idp_entity_id.clone(),
        sso_url: row.sso_url.clone(),
        metadata_url: row.metadata_url.clone(),
        certificates: row
            .certificates
            .iter()
            .map(|der| certificate_view(der))
            .collect(),
        attributes: SamlAttributeNames {
            email: row.attributes.email.clone(),
            display_name: row.attributes.display_name.clone(),
            first_name: row.attributes.first_name.clone(),
            last_name: row.attributes.last_name.clone(),
        },
        refreshed_at: row.refreshed_at,
        refresh_error: row
            .refresh_error
            .as_deref()
            .and_then(SamlRefreshError::from_str_opt),
        certificates_changed_at: row.certificates_changed_at,
    }
}

/// A stored certificate's hash and validity. One that no longer reads shows its hash and zero
/// dates.
fn certificate_view(der: &[u8]) -> OrgSsoCertificate {
    match IdpCertificate::from_der(der) {
        Ok(certificate) => OrgSsoCertificate {
            sha256: certificate.sha256_hex(),
            not_before: certificate.not_before_ns(),
            not_after: certificate.not_after_ns(),
        },
        Err(_) => OrgSsoCertificate {
            sha256: hex::encode(Sha256::digest(der)),
            not_before: 0,
            not_after: 0,
        },
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
    refuse_outbound(&state, "identity provider check")?;
    let input = validate(body)?;

    let existing = store::provider(state.meta.reader(), org).await?;
    // A SAML row counts as an identity change: switching to OIDC needs a secret.
    let changes_identity = existing.as_ref().is_some_and(|row| {
        row.oidc
            .as_ref()
            .is_none_or(|oidc| oidc.issuer != input.issuer || oidc.client_id != input.client_id)
    });
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
    // A switch from SAML deletes the identities too (D-SA-2).
    let renames_issuer = existing.as_ref().is_some_and(|row| {
        row.oidc
            .as_ref()
            .is_none_or(|oidc| oidc.issuer != input.issuer)
    });
    let before = existing;
    let outcome = state
        .meta
        .write(move |conn| {
            Box::pin(async move {
                let current = store::provider_tx(conn, org).await?;
                if !unchanged(before.as_ref(), current.as_ref()) {
                    return Ok(Save::Changed);
                }
                // Enforcement turned on meanwhile would be reset by the switch (D-SA-2).
                if current
                    .as_ref()
                    .is_some_and(|row| row.protocol == SsoProtocol::Saml && row.enforced)
                {
                    return Ok(Save::Enforced);
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
    match outcome {
        Save::Saved => {}
        Save::Changed => {
            return Err(ApiError::Conflict {
                detail: "The identity provider changed meanwhile; reload and try again.",
            });
        }
        Save::Enforced => {
            return Err(ApiError::Conflict {
                detail: SWITCH_ENFORCED,
            });
        }
    }
    super::refresh(&state).await;
    tracing::info!(target: "audit", org_id = %org, user_id = %caller, protocol = "oidc", "org sso provider saved");
    Ok(error::Json(view(&state, org).await?))
}

/// `PUT /api/v1/orgs/{org}/sso/saml` (D-SA-18): saves a SAML provider from a metadata URL,
/// pasted metadata or manual fields. With no source it saves only the SAML settings.
async fn put_org_saml_provider(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Extension(access): Extension<OrgAccess>,
    error::Json(body): error::Json<PutOrgSamlProvider>,
) -> Result<error::Json<OrgSsoView>, ApiError> {
    let org = access.org.id;
    let caller = principal::session_user(&principal)?.id;
    require(&state, org, Feature::OrgSso)?;
    require(&state, org, Feature::Saml)?;
    require_https(&state)?;
    let input = validate_saml(body)?;
    let now = state.clock.now_ns();
    match input.source {
        None => save_saml_settings(&state, org, input.settings, now).await?,
        Some(source) => save_saml_provider(&state, org, input.settings, source, now).await?,
    }
    super::refresh(&state).await;
    tracing::info!(target: "audit", org_id = %org, user_id = %caller, protocol = "saml", "org sso provider saved");
    Ok(error::Json(view(&state, org).await?))
}

/// Saves the settings of the org's SAML provider and keeps its metadata (D-SA-18). No
/// enforcement check, no entity-ID check and no identity is deleted.
async fn save_saml_settings(
    state: &AppState,
    org: OrgId,
    settings: SamlSettings,
    now: i64,
) -> Result<(), ApiError> {
    let updated = state
        .meta
        .write(move |conn| {
            Box::pin(async move {
                store::update_saml_settings(
                    conn,
                    org,
                    &settings.display_name,
                    settings.jit_role,
                    &settings.attributes,
                    now,
                )
                .await
            })
        })
        .await?;
    if updated {
        Ok(())
    } else {
        Err(field_error("metadata", SAML_SOURCE))
    }
}

/// Saves a SAML provider from the metadata of `source` (D-SA-2, D-SA-11, D-SA-18). Protocol
/// switches and entity-ID changes are checked inside the write transaction.
async fn save_saml_provider(
    state: &AppState,
    org: OrgId,
    settings: SamlSettings,
    source: SamlSource,
    now: i64,
) -> Result<(), ApiError> {
    let (metadata, metadata_url, refreshed_at) = saml_metadata(state, source, now).await?;
    let sso_allowed = url::Url::parse(metadata.sso_url())
        .is_ok_and(|url| state.org_sso.policy().allows_url(&url));
    if !sso_allowed {
        return Err(field_error(
            "metadata",
            "the identity provider's sign-on URL is not allowed by the egress policy",
        ));
    }
    let confirmed = settings.confirm_entity_id_change;
    let write = SamlWrite {
        display_name: settings.display_name,
        jit_role: settings.jit_role,
        metadata_url,
        idp_entity_id: metadata.entity_id().to_owned(),
        sso_url: metadata.sso_url().to_owned(),
        certificates: metadata
            .certificates()
            .iter()
            .map(|certificate| certificate.der().to_vec())
            .collect(),
        attributes: settings.attributes,
        refreshed_at,
    };
    let id = Uuid::now_v7();
    let outcome = state
        .meta
        .write(move |conn| {
            Box::pin(async move {
                let existing = store::provider_tx(conn, org).await?;
                let delete_identities = match &existing {
                    None => false,
                    // A switch from OIDC deletes the identities too (D-SA-2).
                    Some(row) if row.protocol == SsoProtocol::Oidc => {
                        if row.enforced {
                            return Ok(SamlSave::Enforced);
                        }
                        true
                    }
                    Some(row) => {
                        let old = store::saml_tx(conn, row.id).await?;
                        let entity_changed =
                            old.is_some_and(|old| old.idp_entity_id != write.idp_entity_id);
                        if entity_changed && !confirmed {
                            return Ok(SamlSave::EntityConfirm);
                        }
                        entity_changed
                    }
                };
                store::put_saml_provider(conn, org, id, write, now).await?;
                if delete_identities {
                    store::delete_identities(conn, org).await?;
                }
                Ok(SamlSave::Saved)
            })
        })
        .await?;
    match outcome {
        SamlSave::Saved => Ok(()),
        SamlSave::Enforced => Err(ApiError::Conflict {
            detail: SWITCH_ENFORCED,
        }),
        SamlSave::EntityConfirm => Err(ApiError::Conflict {
            detail: ENTITY_ID_CHANGE,
        }),
    }
}

/// The IdP metadata of a SAML save, with the URL it was fetched from and the fetch time.
/// Pasted metadata and manual fields have neither (D-SA-11).
async fn saml_metadata(
    state: &AppState,
    source: SamlSource,
    now: i64,
) -> Result<(IdpMetadata, Option<String>, Option<i64>), ApiError> {
    match source {
        SamlSource::Url(url) => {
            refuse_outbound(state, "identity provider check")?;
            let bytes = state
                .org_sso
                .fetch_metadata(&url)
                .await
                .map_err(|error| match error {
                    MetadataFetchError::Url => field_error(
                        "metadata_url",
                        "metadata URL must be https, or http on a loopback host",
                    ),
                    _ => field_error("metadata_url", "the metadata could not be loaded"),
                })?;
            let metadata = parse_metadata(&bytes)
                .map_err(|error| field_error("metadata_url", &error.to_string()))?;
            Ok((metadata, Some(url), Some(now)))
        }
        SamlSource::Xml(xml) => {
            let metadata = parse_metadata(xml.as_bytes())
                .map_err(|error| field_error("metadata_xml", &error.to_string()))?;
            Ok((metadata, None, None))
        }
        SamlSource::Manual {
            idp_entity_id,
            sso_url,
            certificates,
        } => {
            let certificates = certificates
                .iter()
                .map(|pem| IdpCertificate::from_pem(pem))
                .collect::<Result<Vec<_>, MetadataError>>()
                .map_err(|error| field_error("certificates", &error.to_string()))?;
            let metadata = IdpMetadata::from_fields(&idp_entity_id, &sso_url, certificates)
                .map_err(|error| {
                    let field = match error {
                        MetadataError::EntityId => "idp_entity_id",
                        MetadataError::SsoUrl => "sso_url",
                        _ => "certificates",
                    };
                    field_error(field, &error.to_string())
                })?;
            Ok((metadata, None, None))
        }
    }
}

/// A SAML save's settings after validation (D-SA-6, D-SA-18).
struct SamlSettings {
    display_name: String,
    jit_role: JitRole,
    attributes: SamlAttributes,
    confirm_entity_id_change: bool,
}

/// Where a SAML save's IdP metadata comes from. Exactly one.
enum SamlSource {
    Url(String),
    Xml(String),
    Manual {
        idp_entity_id: String,
        sso_url: String,
        certificates: Vec<String>,
    },
}

/// A validated `PUT …/sso/saml` body.
struct ValidatedSaml {
    settings: SamlSettings,
    /// `None` saves only the settings.
    source: Option<SamlSource>,
}

fn validate_saml(body: PutOrgSamlProvider) -> Result<ValidatedSaml, ApiError> {
    let display_name = body.display_name.trim().to_owned();
    if !(1..=100).contains(&display_name.chars().count()) {
        return Err(field_error("display_name", "must be 1 to 100 characters"));
    }
    let attributes = SamlAttributes {
        email: attribute_name("attributes.email", body.attributes.email)?,
        display_name: attribute_name("attributes.display_name", body.attributes.display_name)?,
        first_name: attribute_name("attributes.first_name", body.attributes.first_name)?,
        last_name: attribute_name("attributes.last_name", body.attributes.last_name)?,
    };
    // Exactly one complete source, or none. Two sources and a partial manual set both fail.
    let source = match (
        body.metadata_url,
        body.metadata_xml,
        body.idp_entity_id,
        body.sso_url,
        body.certificates,
    ) {
        (None, None, None, None, None) => None,
        (Some(url), None, None, None, None) => Some(SamlSource::Url(url)),
        (None, Some(xml), None, None, None) => Some(SamlSource::Xml(xml)),
        (None, None, Some(idp_entity_id), Some(sso_url), Some(certificates)) => {
            Some(SamlSource::Manual {
                idp_entity_id,
                sso_url,
                certificates,
            })
        }
        _ => {
            return Err(field_error("metadata", SAML_SOURCE));
        }
    };
    Ok(ValidatedSaml {
        settings: SamlSettings {
            display_name,
            jit_role: body.jit_role,
            attributes,
            confirm_entity_id_change: body.confirm_entity_id_change,
        },
        source,
    })
}

/// An attribute name: 1 to 256 characters, no control character (D-SA-6).
fn attribute_name(field: &str, value: Option<String>) -> Result<Option<String>, ApiError> {
    let Some(name) = value else {
        return Ok(None);
    };
    let length = name.chars().count();
    if !(1..=256).contains(&length) || name.chars().any(char::is_control) {
        return Err(field_error(field, "invalid attribute name"));
    }
    Ok(Some(name))
}

/// SAML needs an https public URL: the SP entity ID and the ACS URL are built from it (D-SA-12).
fn require_https(state: &AppState) -> Result<(), ApiError> {
    if state.config.public_url.is_https() {
        Ok(())
    } else {
        Err(ApiError::Conflict {
            detail: "SAML needs an https public URL (public_url).",
        })
    }
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

/// Turns the flags on or off (D-P-57, D-P-58, D-P-61). Turning enforcement on revokes the
/// password sessions and personal access tokens of the org's domains' users, except the
/// sessions the org's provider started and the instance admins' (D-P-58).
async fn patch_org_sso(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Extension(access): Extension<OrgAccess>,
    error::Json(body): error::Json<PatchOrgSso>,
) -> Result<error::Json<OrgSsoView>, ApiError> {
    let org = access.org.id;
    let Principal::Session { user, sso_org, .. } = &principal else {
        return Err(ApiError::Forbidden {
            code: ProblemCode::Forbidden,
        });
    };
    let row = store::provider(state.meta.reader(), org)
        .await?
        .ok_or(ApiError::Conflict {
            detail: "Set up the identity provider first.",
        })?;
    let enabled = body.enabled.unwrap_or(row.enabled);
    let enforced = body.enforced.unwrap_or(row.enforced);
    let was_enforced = row.enforced;
    if (enabled && !row.enabled) || (enforced && !was_enforced) {
        require(&state, org, Feature::OrgSso)?;
        if row.protocol == SsoProtocol::Saml {
            require(&state, org, Feature::Saml)?;
            require_https(&state)?;
        }
    }
    if enforced && !enabled {
        return Err(ApiError::Conflict {
            detail: "Enable the identity provider before enforcing it.",
        });
    }
    let mut domains = Vec::new();
    if enforced && !was_enforced {
        domains = state.org_sso.domains_of(org);
        if domains.is_empty() {
            return Err(ApiError::Conflict {
                detail: "Verify a domain before enforcing single sign-on.",
            });
        }
        if row.last_login_at.is_none() {
            return Err(ApiError::Conflict {
                detail: "Sign in through the identity provider once (Test sign-in) before enforcing it.",
            });
        }
        let caller_domain = user.email.rsplit_once('@').map(|(_, domain)| domain);
        let caller_covered =
            caller_domain.is_some_and(|domain| domains.iter().any(|listed| listed == domain));
        if !user.is_instance_admin && caller_covered && *sso_org != Some(org) {
            return Err(ApiError::Conflict {
                detail: "Sign in through your identity provider before enforcing it.",
            });
        }
    }
    let now = state.clock.now_ns();
    let snapshot = row;
    let outcome = state
        .meta
        .write(move |conn| {
            Box::pin(async move {
                // The provider may have been removed or changed since the read above.
                let Some(current) = store::provider_tx(conn, org).await? else {
                    return Ok(Patch::Missing);
                };
                if current.id != snapshot.id
                    || current.enabled != snapshot.enabled
                    || current.enforced != snapshot.enforced
                    || current.last_login_at != snapshot.last_login_at
                {
                    return Ok(Patch::Changed);
                }
                if !store::set_flags(conn, org, enabled, enforced, now).await? {
                    return Ok(Patch::Missing);
                }
                if enforced && !was_enforced {
                    let (users, hashes) =
                        store::revoke_for_domains(conn, org, &domains, now).await?;
                    return Ok(Patch::Done(users, hashes));
                }
                Ok(Patch::Done(0, Vec::new()))
            })
        })
        .await?;
    let (users, hashes) = match outcome {
        Patch::Missing => return Err(ApiError::NotFound),
        Patch::Changed => {
            return Err(ApiError::Conflict {
                detail: "The identity provider changed meanwhile; reload and try again.",
            });
        }
        Patch::Done(users, hashes) => (users, hashes),
    };
    for hash in &hashes {
        state.pat_cache.evict(hash);
    }
    super::refresh(&state).await;
    tracing::info!(target: "audit", org_id = %org, user_id = %user.id, enabled, enforced, users, "org sso settings changed");
    Ok(error::Json(view(&state, org).await?))
}

/// D-P-58 on a verification under enforcement: the domain's users lose their sessions and
/// tokens in the same transaction. Returns the revoked token hashes.
async fn revoke_if_enforced(
    conn: &mut SqliteConnection,
    org: OrgId,
    domain: &str,
    now: i64,
) -> Result<Vec<String>, MetaError> {
    let enforced = store::provider_tx(conn, org)
        .await?
        .is_some_and(|row| row.enforced);
    if !enforced {
        return Ok(Vec::new());
    }
    let (_users, hashes) = store::revoke_for_domains(conn, org, &[domain.to_owned()], now).await?;
    Ok(hashes)
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
    refuse_outbound(&state, "domain check")?;
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
                        Ok(Check::Verified {
                            taken_from: None,
                            revoked: Vec::new(),
                        })
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
                            let revoked = revoke_if_enforced(conn, org, &domain, now).await?;
                            Ok(Check::Verified {
                                taken_from: Some(holder.org_id),
                                revoked,
                            })
                        }
                    }
                    None => match store::mark_verified(conn, id, now).await {
                        Ok(()) => Ok(Check::Verified {
                            taken_from: None,
                            revoked: revoke_if_enforced(conn, org, &domain, now).await?,
                        }),
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
        Check::Verified {
            taken_from,
            revoked,
        } => {
            for hash in &revoked {
                state.pat_cache.evict(hash);
            }
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
pub(super) fn has(values: &[String], expected: &str) -> bool {
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
    Verified {
        taken_from: Option<OrgId>,
        /// The personal access token hashes the enforcement revoked (D-P-58).
        revoked: Vec<String>,
    },
}

/// What the OIDC write found and did.
#[derive(Debug, PartialEq, Eq)]
enum Save {
    Saved,
    /// The provider changed between the read and the write.
    Changed,
    /// The provider is a SAML one that is enforced: the switch is refused (D-SA-2).
    Enforced,
}

/// What the SAML write found and did.
#[derive(Debug, PartialEq, Eq)]
enum SamlSave {
    Saved,
    /// The provider is an enforced OIDC one: the switch is refused (D-SA-2).
    Enforced,
    /// The IdP entity ID changes and the request did not confirm it (D-SA-2).
    EntityConfirm,
}

/// What a flags update found and did.
enum Patch {
    /// The provider no longer exists.
    Missing,
    /// The provider changed between the read and the write.
    Changed,
    /// The flags are set; the revoked users and token hashes.
    Done(u64, Vec<String>),
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
            before.id == now.id && before.protocol == now.protocol && before.oidc == now.oidc
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
    row.oidc
        .as_ref()
        .and_then(|oidc| {
            state
                .secrets
                .open(&oidc.client_secret_enc, store::secret_aad(&row.id))
                .ok()
        })
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
    use crate::auth::secret::{hash_token, new_opaque_token};
    use crate::ee::org_sso::ProviderKind;
    use crate::meta::identities::insert_identity;
    use crate::test_support::mock_idp::{CLIENT_ID, CLIENT_SECRET, MockIdp};
    use crate::test_support::{
        MetadataServer, TestApp, assert_logs_clean, configure_org_saml, configure_org_sso,
        license_key, make_instance_admin, org_sso_app, org_sso_app_with, reload_org_sso, saml_app,
        second_org, session_cookie_via,
    };
    use cetra_saml::test_idp::{IDP_ENTITY_ID, IDP_SSO_URL, TestKey, metadata_xml};

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
            .open(
                &row.oidc.as_ref().expect("oidc columns").client_secret_enc,
                store::secret_aad(&row.id),
            )
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
            .open(
                &row.oidc.as_ref().expect("oidc columns").client_secret_enc,
                store::secret_aad(&row.id),
            )
            .expect("open");
        assert_eq!(opened, CLIENT_SECRET.as_bytes());
        assert_eq!(row.display_name, "Acme Entra");
    }

    #[tokio::test]
    async fn an_issuer_or_client_change_needs_a_new_test_sign_in() {
        let first = MockIdp::start().await;
        let second = MockIdp::start().await;
        let (app, seeded, owner) = org_sso_app().await;
        configure_org_sso(&app, seeded.org_id, &first, "acme.test", true, false).await;
        let last_login = |app: &TestApp| {
            let reader = app.state.meta.reader().clone();
            async move {
                sqlx::query_scalar::<_, Option<i64>>("SELECT last_login_at FROM org_sso_providers")
                    .fetch_one(&reader)
                    .await
                    .unwrap()
            }
        };
        assert!(last_login(&app).await.is_some());

        let renamed = put(
            &app,
            &owner.cookie,
            &body(first.issuer(), None, "Acme Entra"),
        )
        .await;
        assert_eq!(renamed.status, StatusCode::OK);
        assert!(last_login(&app).await.is_some());

        let changed = put(
            &app,
            &owner.cookie,
            &body(second.issuer(), Some(CLIENT_SECRET), "Acme Entra"),
        )
        .await;
        assert_eq!(changed.status, StatusCode::OK);
        assert!(last_login(&app).await.is_none());
    }

    #[tokio::test]
    async fn patching_a_deleted_provider_revokes_nothing() {
        let idp = MockIdp::start().await;
        let (app, seeded, owner) = org_sso_app().await;
        configure_org_sso(&app, seeded.org_id, &idp, "acme.test", true, false).await;
        let owner_sso = session_cookie_via(&app, owner.id, Some(seeded.org_id)).await;
        run_sql(&app, "DELETE FROM org_sso_providers").await;
        reload_org_sso(&app).await;
        let response = patch_settings(&app, &owner_sso, &json!({"enforced": true})).await;
        assert!(
            response.status == StatusCode::NOT_FOUND || response.status == StatusCode::CONFLICT
        );
        assert_eq!(me_with_cookie(&app, &owner.cookie).await, StatusCode::OK);
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
            protocol: SsoProtocol::Oidc,
            display_name: "Acme".to_owned(),
            oidc: Some(store::OidcColumns {
                issuer: issuer.to_owned(),
                client_id: "c".to_owned(),
                client_secret_enc: secret,
            }),
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

    /// Auth review #15 (D-P-72): the domain proof is re-checked after a day, and a record that
    /// is gone ends the verification. A failed lookup changes nothing.
    #[tokio::test]
    async fn a_lapsed_domain_stops_routing() {
        use super::super::recheck::{Recheck, recheck_once};

        let (app, seeded, owner) = org_sso_app().await;
        seed_provider(&app, seeded.org_id, "https://idp.example").await;
        let added = add_domain(&app, "acme", &owner.cookie, "acme.test").await;
        let row = added.json::<OrgSsoView>().domains[0].clone();
        app.dns_txt(&row.txt_name, &[quoted(&row.txt_value).as_str()]);
        assert_eq!(
            verify(&app, "acme", &owner.cookie, row.id).await.status,
            StatusCode::OK
        );
        let routed = |app: &TestApp| {
            app.state
                .org_sso
                .route("ada@acme.test")
                .map(|route| route.org_id)
        };
        assert_eq!(routed(&app), Some(seeded.org_id));

        // Not due yet.
        let now = app.state.clock.now_ns();
        assert_eq!(
            recheck_once(&app.state, now).await.unwrap(),
            Recheck::default()
        );

        // A day later the record is still there.
        app.state
            .clock
            .advance(std::time::Duration::from_secs(25 * 3_600));
        let now = app.state.clock.now_ns();
        let kept = recheck_once(&app.state, now).await.unwrap();
        assert_eq!((kept.kept, kept.lapsed, kept.failed), (1, 0, 0));
        assert_eq!(routed(&app), Some(seeded.org_id));

        // The next day the resolver fails: nothing changes.
        app.state
            .clock
            .advance(std::time::Duration::from_secs(25 * 3_600));
        let now = app.state.clock.now_ns();
        app.dns_fail(true);
        let failed = recheck_once(&app.state, now).await.unwrap();
        assert_eq!((failed.kept, failed.lapsed, failed.failed), (0, 0, 1));
        assert_eq!(routed(&app), Some(seeded.org_id));

        // The record is gone: the domain is unverified and no longer routes.
        app.dns_fail(false);
        app.dns_txt(&row.txt_name, &["something else"]);
        let lapsed = recheck_once(&app.state, now).await.unwrap();
        assert_eq!((lapsed.kept, lapsed.lapsed, lapsed.failed), (0, 1, 0));
        assert_eq!(routed(&app), None);
        assert_eq!(
            domain_of(&app, "acme", &owner.cookie, "acme.test")
                .await
                .status,
            DomainStatus::Pending
        );
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
        assert_eq!(row.txt_name, "_cetra-verify.acme.test");
        let token = row.txt_value.strip_prefix("cetra-verify=").expect("value");
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
        assert_logs_clean(&["acme.test", "cetra-verify="], async {
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

    const ME: &str = "/api/auth/me";

    async fn me_with_cookie(app: &TestApp, cookie: &str) -> StatusCode {
        app.request(Method::GET, ME)
            .header("cookie", cookie)
            .send()
            .await
            .status
    }

    async fn me_with_token(app: &TestApp, token: &str) -> StatusCode {
        app.request(Method::GET, ME)
            .header("authorization", &format!("Bearer {token}"))
            .send()
            .await
            .status
    }

    async fn patch_settings(
        app: &TestApp,
        cookie: &str,
        body: &Value,
    ) -> crate::test_support::TestResponse {
        app.request(Method::PATCH, PATH)
            .header("cookie", cookie)
            .json(body)
            .send()
            .await
    }

    fn detail(response: &crate::test_support::TestResponse) -> String {
        response.json::<Value>()["detail"]
            .as_str()
            .unwrap_or("")
            .to_owned()
    }

    const RESET_ROWS: &str = "SELECT COUNT(*) FROM password_resets WHERE user_id = ?";
    const SESSION_ROWS: &str = "SELECT COUNT(*) FROM web_sessions WHERE user_id = ?";

    async fn count_rows(app: &TestApp, sql: &'static str, user: Uuid) -> i64 {
        sqlx::query_scalar(sql)
            .bind(user.to_string())
            .fetch_one(app.state.meta.reader())
            .await
            .unwrap()
    }

    async fn run_sql(app: &TestApp, sql: &'static str) {
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    sqlx::query(sql).execute(&mut *conn).await?;
                    Ok(())
                })
            })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn enforcing_revokes_domain_users() {
        let idp = MockIdp::start().await;
        let (app, seeded, owner) = org_sso_app().await;
        let org = seeded.org_id;
        configure_org_sso(&app, org, &idp, "acme.test", true, false).await;
        let owner_sso = session_cookie_via(&app, owner.id, Some(org)).await;
        let ada = app
            .add_member(&seeded, "ada@acme.test", OrgRole::Member)
            .await;
        let ada_pat = app.personal_token(ada.id, true).await;
        let (_beta, bob) = second_org(&app, "beta", "bob@other.test").await;
        let root = app
            .add_member(&seeded, "root@acme.test", OrgRole::Member)
            .await;
        make_instance_admin(&app, root.id).await;

        // Warm the PAT cache, and give Ada a password reset that enforcement must delete.
        assert_eq!(me_with_token(&app, &ada_pat).await, StatusCode::OK);
        let now = app.state.clock.now_ns();
        let user = ada.id;
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    crate::meta::password_resets::insert_reset(
                        conn,
                        &hash_token("reset-token"),
                        user,
                        now,
                        now + 3_600_000_000_000,
                    )
                    .await?;
                    Ok(())
                })
            })
            .await
            .unwrap();
        assert_eq!(count_rows(&app, RESET_ROWS, ada.id).await, 1);

        let enforced = patch_settings(&app, &owner_sso, &json!({"enforced": true})).await;
        assert_eq!(enforced.status, StatusCode::OK);
        assert!(
            enforced
                .json::<OrgSsoView>()
                .provider
                .expect("provider")
                .enforced
        );

        assert_eq!(
            me_with_cookie(&app, &ada.cookie).await,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            me_with_token(&app, &ada_pat).await,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(count_rows(&app, RESET_ROWS, ada.id).await, 0);
        assert_eq!(count_rows(&app, SESSION_ROWS, ada.id).await, 0);
        assert_eq!(me_with_cookie(&app, &bob.cookie).await, StatusCode::OK);
        assert_eq!(me_with_cookie(&app, &root.cookie).await, StatusCode::OK);
        assert_eq!(me_with_cookie(&app, &owner_sso).await, StatusCode::OK);
        assert_eq!(
            me_with_cookie(&app, &owner.cookie).await,
            StatusCode::UNAUTHORIZED
        );
        // Project keys are not touched by enforcement.
        let read = app
            .request(
                Method::GET,
                &format!("/api/v1/projects/{}/traces", seeded.project.id),
            )
            .header("authorization", &format!("Bearer {}", seeded.read_key))
            .send()
            .await;
        assert_eq!(read.status, StatusCode::OK);
    }

    #[tokio::test]
    async fn enforcing_needs_the_callers_sso_session() {
        let idp = MockIdp::start().await;

        // The caller signed in with a password.
        let (app, seeded, owner) = org_sso_app().await;
        configure_org_sso(&app, seeded.org_id, &idp, "acme.test", true, false).await;
        let refused = patch_settings(&app, &owner.cookie, &json!({"enforced": true})).await;
        assert_eq!(refused.status, StatusCode::CONFLICT);
        assert!(detail(&refused).contains("Sign in through your identity provider"));

        // The last sign-in was never recorded.
        let (app, seeded, owner) = org_sso_app().await;
        configure_org_sso(&app, seeded.org_id, &idp, "acme.test", true, false).await;
        let owner_sso = session_cookie_via(&app, owner.id, Some(seeded.org_id)).await;
        run_sql(&app, "UPDATE org_sso_providers SET last_login_at = NULL").await;
        reload_org_sso(&app).await;
        let refused = patch_settings(&app, &owner_sso, &json!({"enforced": true})).await;
        assert_eq!(refused.status, StatusCode::CONFLICT);
        assert!(detail(&refused).contains("Sign in through the identity provider once"));

        // No verified domain.
        let (app, seeded, owner) = org_sso_app().await;
        configure_org_sso(&app, seeded.org_id, &idp, "acme.test", true, false).await;
        let owner_sso = session_cookie_via(&app, owner.id, Some(seeded.org_id)).await;
        run_sql(&app, "UPDATE org_sso_domains SET verified_at = NULL").await;
        reload_org_sso(&app).await;
        let refused = patch_settings(&app, &owner_sso, &json!({"enforced": true})).await;
        assert_eq!(refused.status, StatusCode::CONFLICT);
        assert!(detail(&refused).contains("Verify a domain"));

        // The provider is disabled.
        let (app, seeded, owner) = org_sso_app().await;
        configure_org_sso(&app, seeded.org_id, &idp, "acme.test", false, false).await;
        let owner_sso = session_cookie_via(&app, owner.id, Some(seeded.org_id)).await;
        let refused = patch_settings(&app, &owner_sso, &json!({"enforced": true})).await;
        assert_eq!(refused.status, StatusCode::CONFLICT);
        assert!(detail(&refused).contains("Enable the identity provider"));
    }

    #[tokio::test]
    async fn sessions_and_tokens_created_in_the_race_are_refused() {
        let idp = MockIdp::start().await;
        let (app, seeded, owner) = org_sso_app().await;
        let org = seeded.org_id;
        configure_org_sso(&app, org, &idp, "acme.test", true, false).await;
        let owner_sso = session_cookie_via(&app, owner.id, Some(org)).await;
        let ada = app
            .add_member(&seeded, "ada@acme.test", OrgRole::Member)
            .await;
        let root = app
            .add_member(&seeded, "root@acme.test", OrgRole::Member)
            .await;
        make_instance_admin(&app, root.id).await;
        let enforced = patch_settings(&app, &owner_sso, &json!({"enforced": true})).await;
        assert_eq!(enforced.status, StatusCode::OK);

        // A login in flight: a password session, and a token made after the revocation.
        let token = new_opaque_token().expect("token");
        let in_flight = format!("cetra_session={}", token.plaintext);
        let now = app.state.clock.now_ns();
        let user = ada.id;
        let hash = token.hash.clone();
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    crate::meta::web_sessions::insert_session(
                        conn,
                        crate::meta::web_sessions::NewSession {
                            hash,
                            user_id: user,
                            user_agent: None,
                            ip: None,
                            sso_org_id: None,
                            via_provider: false,
                        },
                        now,
                        now + 3_600_000_000_000,
                    )
                    .await?;
                    Ok(())
                })
            })
            .await
            .unwrap();
        assert_eq!(
            me_with_cookie(&app, &in_flight).await,
            StatusCode::UNAUTHORIZED
        );
        let pat = app.personal_token(ada.id, true).await;
        assert_eq!(me_with_token(&app, &pat).await, StatusCode::UNAUTHORIZED);

        // Through the org's provider, the same user works; so does the instance admin.
        let via_sso = session_cookie_via(&app, ada.id, Some(org)).await;
        assert_eq!(me_with_cookie(&app, &via_sso).await, StatusCode::OK);
        assert_eq!(me_with_cookie(&app, &root.cookie).await, StatusCode::OK);
    }

    #[tokio::test]
    async fn disable_is_refused_while_enforced() {
        let idp = MockIdp::start().await;
        let (app, seeded, owner) = org_sso_app().await;
        configure_org_sso(&app, seeded.org_id, &idp, "acme.test", true, false).await;
        let owner_sso = session_cookie_via(&app, owner.id, Some(seeded.org_id)).await;
        assert_eq!(
            patch_settings(&app, &owner_sso, &json!({"enforced": true}))
                .await
                .status,
            StatusCode::OK
        );
        let refused = patch_settings(&app, &owner_sso, &json!({"enabled": false})).await;
        assert_eq!(refused.status, StatusCode::CONFLICT);
        assert!(detail(&refused).contains("Enable the identity provider"));

        assert_eq!(
            patch_settings(&app, &owner_sso, &json!({"enforced": false}))
                .await
                .status,
            StatusCode::OK
        );
        assert_eq!(
            patch_settings(&app, &owner_sso, &json!({"enabled": false}))
                .await
                .status,
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn lapse_allows_only_turning_off() {
        let idp = MockIdp::start().await;
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let owner = app
            .add_member(&seeded, "owner@acme.test", OrgRole::Owner)
            .await;
        configure_org_sso(&app, seeded.org_id, &idp, "acme.test", true, true).await;
        let owner_sso = session_cookie_via(&app, owner.id, Some(seeded.org_id)).await;

        assert_eq!(
            patch_settings(&app, &owner_sso, &json!({"enforced": false}))
                .await
                .status,
            StatusCode::OK
        );
        let locked = patch_settings(&app, &owner_sso, &json!({"enforced": true})).await;
        assert_eq!(locked.status, StatusCode::FORBIDDEN);
        assert_eq!(
            locked.json::<Value>()["detail"],
            Feature::OrgSso.locked_detail()
        );
        assert_eq!(
            patch_settings(&app, &owner_sso, &json!({"enabled": false}))
                .await
                .status,
            StatusCode::OK
        );
        assert_eq!(
            patch_settings(&app, &owner_sso, &json!({"enabled": true}))
                .await
                .status,
            StatusCode::FORBIDDEN
        );
    }

    #[tokio::test]
    async fn verifying_under_enforcement_revokes() {
        let idp = MockIdp::start().await;
        let (app, seeded, owner) = org_sso_app().await;
        configure_org_sso(&app, seeded.org_id, &idp, "acme.test", true, false).await;
        let owner_sso = session_cookie_via(&app, owner.id, Some(seeded.org_id)).await;
        assert_eq!(
            patch_settings(&app, &owner_sso, &json!({"enforced": true}))
                .await
                .status,
            StatusCode::OK
        );

        let eve = app
            .add_member(&seeded, "eve@acme.io", OrgRole::Member)
            .await;
        let eve_pat = app.personal_token(eve.id, true).await;
        assert_eq!(me_with_token(&app, &eve_pat).await, StatusCode::OK);
        assert_eq!(me_with_cookie(&app, &eve.cookie).await, StatusCode::OK);

        assert_eq!(
            add_domain(&app, "acme", &owner_sso, "acme.io").await.status,
            StatusCode::CREATED
        );
        let row = domain_of(&app, "acme", &owner_sso, "acme.io").await;
        app.dns_txt(&row.txt_name, &[quoted(&row.txt_value).as_str()]);
        assert_eq!(
            verify(&app, "acme", &owner_sso, row.id).await.status,
            StatusCode::OK
        );

        assert_eq!(count_rows(&app, SESSION_ROWS, eve.id).await, 0);
        assert_eq!(
            me_with_cookie(&app, &eve.cookie).await,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            me_with_token(&app, &eve_pat).await,
            StatusCode::UNAUTHORIZED
        );
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

    #[tokio::test]
    async fn put_org_sso_provider_is_refused_in_restore_mode() {
        let (app, _seeded, owner) = org_sso_app_with(|config| config.restore_mode = true).await;
        let response = put(
            &app,
            &owner.cookie,
            &json!({
                "display_name": "Acme Okta",
                "issuer": "https://idp.example",
                "client_id": CLIENT_ID,
                "client_secret": CLIENT_SECRET,
                "jit_role": "viewer",
            }),
        )
        .await;
        crate::http::restore::tests::assert_refused(&response, "identity provider check");
        assert!(identity_providers(&app).await.is_empty());
    }

    #[tokio::test]
    async fn post_org_sso_domain_verify_is_refused_in_restore_mode() {
        let (app, seeded, owner) = org_sso_app_with(|config| config.restore_mode = true).await;
        seed_provider(&app, seeded.org_id, "https://idp.example").await;
        add_domain(&app, "acme", &owner.cookie, "acme.test").await;
        let row = domain_of(&app, "acme", &owner.cookie, "acme.test").await;
        let response = verify(&app, "acme", &owner.cookie, row.id).await;
        crate::http::restore::tests::assert_refused(&response, "domain check");
    }

    const SAML_PATH: &str = "/api/v1/orgs/acme/sso/saml";

    async fn put_saml(
        app: &TestApp,
        cookie: &str,
        payload: &Value,
    ) -> crate::test_support::TestResponse {
        app.request(Method::PUT, SAML_PATH)
            .header("cookie", cookie)
            .json(payload)
            .send()
            .await
    }

    /// A SAML save from the test IdP's pasted metadata.
    fn saml_xml(display_name: &str) -> Value {
        json!({
            "display_name": display_name,
            "jit_role": "member",
            "metadata_xml": metadata_xml(&[TestKey::Idp]),
        })
    }

    /// Writes an identity of the org's provider, as a sign-in through it would.
    async fn insert_org_identity(app: &TestApp, user: Uuid, org: OrgId) {
        let provider = store::identity_provider(org);
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    insert_identity(conn, user, &provider, "sub-1", "owner@acme.test", 1).await?;
                    Ok(())
                })
            })
            .await
            .unwrap();
    }

    fn saml_of(response: &crate::test_support::TestResponse) -> OrgSsoSaml {
        response
            .json::<OrgSsoView>()
            .provider
            .expect("provider")
            .saml
            .expect("saml settings")
    }

    #[tokio::test]
    async fn an_owner_saves_a_saml_provider_from_xml() {
        let (app, seeded, owner) = saml_app().await;
        let saved = put_saml(&app, &owner.cookie, &saml_xml("Acme SAML")).await;
        assert_eq!(saved.status, StatusCode::OK);
        let view: OrgSsoView = saved.json();
        let provider = view.provider.clone().expect("provider");
        assert_eq!(provider.protocol, SsoProtocol::Saml);
        assert!(!provider.enabled);
        assert_eq!(provider.issuer, None);
        assert_eq!(provider.client_id, None);
        let saml = provider.saml.clone().expect("saml settings");
        assert_eq!(saml.idp_entity_id, IDP_ENTITY_ID);
        assert_eq!(saml.certificates.len(), 1);
        assert_eq!(saml.certificates[0].sha256.len(), 64);
        assert!(view.saml_sp.acs_url.ends_with("/acs"));
        assert!(matches!(
            app.state
                .org_sso
                .provider(seeded.org_id)
                .expect("indexed")
                .kind,
            ProviderKind::Saml(_)
        ));
    }

    #[tokio::test]
    async fn manual_fields_save() {
        let (app, _seeded, owner) = saml_app().await;
        let payload = json!({
            "display_name": "Acme SAML",
            "jit_role": "viewer",
            "idp_entity_id": IDP_ENTITY_ID,
            "sso_url": IDP_SSO_URL,
            "certificates": [TestKey::Idp.cert_pem()],
        });
        let saved = put_saml(&app, &owner.cookie, &payload).await;
        assert_eq!(saved.status, StatusCode::OK);
        let saml = saml_of(&saved);
        assert_eq!(saml.idp_entity_id, IDP_ENTITY_ID);
        assert_eq!(saml.sso_url, IDP_SSO_URL);
        assert_eq!(saml.metadata_url, None);
        assert_eq!(saml.refreshed_at, None);
    }

    #[tokio::test]
    async fn a_metadata_url_is_fetched_and_refreshed_at_is_set() {
        let (app, _seeded, owner) = saml_app().await;
        let server = MetadataServer::start(metadata_xml(&[TestKey::Idp]).into_bytes()).await;
        let payload = json!({
            "display_name": "Acme SAML",
            "jit_role": "member",
            "metadata_url": server.url(),
        });
        let saved = put_saml(&app, &owner.cookie, &payload).await;
        assert_eq!(saved.status, StatusCode::OK);
        let saml = saml_of(&saved);
        assert_eq!(saml.metadata_url, Some(server.url()));
        assert!(saml.refreshed_at.is_some());
    }

    #[tokio::test]
    async fn an_http_metadata_url_off_loopback_is_refused() {
        let (app, _seeded, owner) = saml_app().await;
        let payload = json!({
            "display_name": "Acme SAML",
            "jit_role": "member",
            "metadata_url": "http://idp.example/metadata",
        });
        let refused = put_saml(&app, &owner.cookie, &payload).await;
        assert_eq!(refused.status, StatusCode::BAD_REQUEST);
        assert_eq!(
            refused.json::<Value>()["errors"][0]["field"],
            "metadata_url"
        );
    }

    #[tokio::test]
    async fn a_failing_metadata_url_is_400() {
        let (app, _seeded, owner) = saml_app().await;
        let server = MetadataServer::start(Vec::new()).await;
        server.set(StatusCode::INTERNAL_SERVER_ERROR, Vec::new());
        let payload = json!({
            "display_name": "Acme SAML",
            "jit_role": "member",
            "metadata_url": server.url(),
        });
        let refused = put_saml(&app, &owner.cookie, &payload).await;
        assert_eq!(refused.status, StatusCode::BAD_REQUEST);
        assert_eq!(
            refused.json::<Value>()["errors"][0]["field"],
            "metadata_url"
        );
    }

    #[tokio::test]
    async fn two_sources_are_400() {
        let (app, _seeded, owner) = saml_app().await;
        let payload = json!({
            "display_name": "Acme SAML",
            "jit_role": "member",
            "metadata_url": "https://idp.test/metadata",
            "metadata_xml": metadata_xml(&[TestKey::Idp]),
        });
        let refused = put_saml(&app, &owner.cookie, &payload).await;
        assert_eq!(refused.status, StatusCode::BAD_REQUEST);
        assert_eq!(refused.json::<Value>()["errors"][0]["field"], "metadata");
    }

    #[tokio::test]
    async fn no_source_without_a_saml_provider_is_400() {
        let (app, _seeded, owner) = saml_app().await;
        let payload = json!({"display_name": "Acme SAML", "jit_role": "member"});
        let refused = put_saml(&app, &owner.cookie, &payload).await;
        assert_eq!(refused.status, StatusCode::BAD_REQUEST);
        assert_eq!(refused.json::<Value>()["errors"][0]["field"], "metadata");
    }

    #[tokio::test]
    async fn no_source_updates_only_the_settings() {
        let (app, seeded, owner) = saml_app().await;
        let saved = put_saml(&app, &owner.cookie, &saml_xml("Acme SAML")).await;
        assert_eq!(saved.status, StatusCode::OK);
        let before = saml_of(&saved);
        insert_org_identity(&app, owner.id, seeded.org_id).await;

        let payload = json!({
            "display_name": "Acme Entra",
            "jit_role": "viewer",
            "attributes": {"email": "upn"},
        });
        let updated = put_saml(&app, &owner.cookie, &payload).await;
        assert_eq!(updated.status, StatusCode::OK);
        let provider = updated.json::<OrgSsoView>().provider.expect("provider");
        assert_eq!(provider.display_name, "Acme Entra");
        assert_eq!(provider.jit_role, JitRole::Viewer);
        let after = provider.saml.expect("saml settings");
        assert_eq!(after.idp_entity_id, before.idp_entity_id);
        assert_eq!(after.certificates, before.certificates);
        assert_eq!(after.attributes.email.as_deref(), Some("upn"));
        assert_eq!(identity_providers(&app).await.len(), 1);
    }

    #[tokio::test]
    async fn bad_metadata_is_400_with_a_static_message() {
        let (app, _seeded, owner) = saml_app().await;
        let payload = json!({
            "display_name": "Acme SAML",
            "jit_role": "member",
            "metadata_xml": metadata_xml(&[]),
        });
        let refused = put_saml(&app, &owner.cookie, &payload).await;
        assert_eq!(refused.status, StatusCode::BAD_REQUEST);
        let body = refused.json::<Value>();
        assert_eq!(body["errors"][0]["field"], "metadata_xml");
        assert_eq!(
            body["errors"][0]["message"].as_str(),
            Some(
                MetadataError::NoSigningCertificate
                    .to_string()
                    .as_str()
            )
        );
    }

    #[tokio::test]
    async fn saml_needs_both_features() {
        let (app, _seeded, owner) = org_sso_app_with(|config| {
            config.public_url = serde_json::from_str("\"https://cetra.example\"").expect("url");
            config.license_key = Some(Secret::new(license_key(&["org_sso"])));
        })
        .await;
        let refused = put_saml(&app, &owner.cookie, &saml_xml("Acme SAML")).await;
        assert_eq!(refused.status, StatusCode::FORBIDDEN);
        assert_eq!(
            refused.json::<Value>()["detail"],
            Feature::Saml.locked_detail()
        );
    }

    #[tokio::test]
    async fn saml_needs_an_https_public_url() {
        let (app, _seeded, owner) = org_sso_app_with(|config| {
            config.license_key = Some(Secret::new(license_key(&["org_sso", "saml"])));
        })
        .await;
        let refused = put_saml(&app, &owner.cookie, &saml_xml("Acme SAML")).await;
        assert_eq!(refused.status, StatusCode::CONFLICT);
        assert!(detail(&refused).contains("https public URL"));
    }

    #[tokio::test]
    async fn switching_from_oidc_deletes_identities_and_resets_flags() {
        let idp = MockIdp::start().await;
        let (app, seeded, owner) = saml_app().await;
        configure_org_sso(&app, seeded.org_id, &idp, "acme.test", true, false).await;
        insert_org_identity(&app, owner.id, seeded.org_id).await;

        let saved = put_saml(&app, &owner.cookie, &saml_xml("Acme SAML")).await;
        assert_eq!(saved.status, StatusCode::OK);
        assert!(identity_providers(&app).await.is_empty());
        let provider = saved.json::<OrgSsoView>().provider.expect("provider");
        assert!(!provider.enabled);
        assert!(!provider.enforced);
    }

    #[tokio::test]
    async fn switching_is_refused_while_enforced() {
        let idp = MockIdp::start().await;

        // An enforced OIDC provider cannot switch to SAML.
        let (app, seeded, owner) = saml_app().await;
        configure_org_sso(&app, seeded.org_id, &idp, "acme.test", true, true).await;
        let refused = put_saml(&app, &owner.cookie, &saml_xml("Acme SAML")).await;
        assert_eq!(refused.status, StatusCode::CONFLICT);
        assert_eq!(detail(&refused), SWITCH_ENFORCED);

        // An enforced SAML provider cannot switch to OIDC.
        let (app, seeded, owner) = saml_app().await;
        configure_org_saml(
            &app,
            seeded.org_id,
            &[TestKey::Idp],
            "acme.test",
            true,
            true,
        )
        .await;
        let refused = put(
            &app,
            &owner.cookie,
            &body(idp.issuer(), Some(CLIENT_SECRET), "Acme Okta"),
        )
        .await;
        assert_eq!(refused.status, StatusCode::CONFLICT);
        assert_eq!(detail(&refused), SWITCH_ENFORCED);
    }

    #[tokio::test]
    async fn an_entity_id_change_needs_confirmation() {
        let (app, seeded, owner) = saml_app().await;
        configure_org_saml(
            &app,
            seeded.org_id,
            &[TestKey::Idp],
            "acme.test",
            false,
            false,
        )
        .await;
        insert_org_identity(&app, owner.id, seeded.org_id).await;

        let other = metadata_xml(&[TestKey::Idp]).replace(IDP_ENTITY_ID, "https://idp.test/other");
        let mut payload = json!({
            "display_name": "Acme SAML",
            "jit_role": "member",
            "metadata_xml": other,
        });
        let refused = put_saml(&app, &owner.cookie, &payload).await;
        assert_eq!(refused.status, StatusCode::CONFLICT);
        assert_eq!(detail(&refused), ENTITY_ID_CHANGE);
        assert_eq!(identity_providers(&app).await.len(), 1);

        payload["confirm_entity_id_change"] = json!(true);
        let confirmed = put_saml(&app, &owner.cookie, &payload).await;
        assert_eq!(confirmed.status, StatusCode::OK);
        assert!(identity_providers(&app).await.is_empty());
        let provider = confirmed.json::<OrgSsoView>().provider.expect("provider");
        assert_eq!(provider.last_login_at, None);
    }

    #[tokio::test]
    async fn the_same_entity_id_keeps_identities() {
        let (app, seeded, owner) = saml_app().await;
        configure_org_saml(
            &app,
            seeded.org_id,
            &[TestKey::Idp],
            "acme.test",
            false,
            false,
        )
        .await;
        insert_org_identity(&app, owner.id, seeded.org_id).await;
        let saved = put_saml(&app, &owner.cookie, &saml_xml("Acme SAML")).await;
        assert_eq!(saved.status, StatusCode::OK);
        assert_eq!(identity_providers(&app).await.len(), 1);
    }

    #[tokio::test]
    async fn switching_back_to_oidc_needs_a_secret() {
        let idp = MockIdp::start().await;
        let (app, seeded, owner) = saml_app().await;
        configure_org_saml(
            &app,
            seeded.org_id,
            &[TestKey::Idp],
            "acme.test",
            false,
            false,
        )
        .await;
        let refused = put(&app, &owner.cookie, &body(idp.issuer(), None, "Acme Okta")).await;
        assert_eq!(refused.status, StatusCode::BAD_REQUEST);
        assert_eq!(
            refused.json::<Value>()["errors"][0]["field"],
            "client_secret"
        );
    }

    #[tokio::test]
    async fn enabling_saml_needs_the_feature_and_turning_off_does_not() {
        let (app, seeded, owner) = org_sso_app_with(|config| {
            config.public_url = serde_json::from_str("\"https://cetra.example\"").expect("url");
            config.license_key = Some(Secret::new(license_key(&["org_sso"])));
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
        let owner_sso = session_cookie_via(&app, owner.id, Some(seeded.org_id)).await;

        assert_eq!(
            patch_settings(&app, &owner_sso, &json!({"enabled": false}))
                .await
                .status,
            StatusCode::OK
        );
        let locked = patch_settings(&app, &owner_sso, &json!({"enabled": true})).await;
        assert_eq!(locked.status, StatusCode::FORBIDDEN);
        assert_eq!(
            locked.json::<Value>()["detail"],
            Feature::Saml.locked_detail()
        );
    }

    #[tokio::test]
    async fn admins_cannot_save_saml() {
        let (app, seeded, _owner) = saml_app().await;
        let admin = app
            .add_member(&seeded, "admin@acme.test", OrgRole::Admin)
            .await;
        let refused = put_saml(&app, &admin.cookie, &saml_xml("Acme SAML")).await;
        assert_eq!(refused.status, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn the_view_shows_the_sp_values_for_oidc_orgs_too() {
        let idp = MockIdp::start().await;
        let (app, seeded, owner) = org_sso_app().await;
        configure_org_sso(&app, seeded.org_id, &idp, "acme.test", true, false).await;
        let view = view_of(&app, "acme", &owner.cookie).await;
        let provider = view.provider.clone().expect("provider");
        assert_eq!(provider.protocol, SsoProtocol::Oidc);
        assert!(provider.issuer.is_some());
        assert!(provider.saml.is_none());
        let acs_suffix = format!("/api/auth/saml/{}/acs", seeded.org_id);
        let start_suffix = format!("/api/auth/saml/{}/start", seeded.org_id);
        assert!(view.saml_sp.acs_url.ends_with(&acs_suffix));
        assert!(view.saml_sp.start_url.ends_with(&start_suffix));
    }

    #[tokio::test]
    async fn saml_logs_carry_no_metadata() {
        let (app, _seeded, owner) = saml_app().await;
        let saved = assert_logs_clean(&[IDP_ENTITY_ID, IDP_SSO_URL, "BEGIN CERTIFICATE"], async {
            put_saml(&app, &owner.cookie, &saml_xml("Acme SAML")).await
        })
        .await;
        assert_eq!(saved.status, StatusCode::OK);
    }

    #[tokio::test]
    async fn put_org_saml_provider_from_a_url_is_refused_in_restore_mode() {
        let (app, _seeded, owner) = org_sso_app_with(|config| {
            config.public_url = serde_json::from_str("\"https://cetra.example\"").expect("url");
            config.license_key = Some(Secret::new(license_key(&["org_sso", "saml"])));
            config.restore_mode = true;
        })
        .await;
        let payload = json!({
            "display_name": "Acme SAML",
            "jit_role": "member",
            "metadata_url": "https://idp.test/metadata",
        });
        let response = put_saml(&app, &owner.cookie, &payload).await;
        crate::http::restore::tests::assert_refused(&response, "identity provider check");
    }
}
