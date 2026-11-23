// SPDX-License-Identifier: Elastic-2.0
//! The Provisioning API DTOs: an org's SCIM settings and bearer tokens (track EA M5,
//! D-EA-12, D-EA-15). The `/scim/v2` resources themselves are in `ee::scim`.

use cetra_core::time_serde;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use super::account::OrgRole;
use super::keys::optional_time;

/// `GET` and `PATCH /api/v1/orgs/{org}/scim`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ScimSettings {
    /// Whether SCIM provisioning is on. Readable without the `scim` feature.
    pub enabled: bool,
    /// The org role a newly provisioned member gets: `viewer` or `member`.
    pub default_role: OrgRole,
    /// The SCIM base URL to give the identity provider: `<public_url>/scim/v2`.
    pub base_url: String,
    /// The org's verified SCIM domains.
    pub verified_domains: u32,
    /// The live tokens, newest first.
    pub tokens: Vec<ScimToken>,
}

/// A SCIM bearer token. Its secret is not a field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ScimToken {
    /// Token id.
    pub id: Uuid,
    /// Display name.
    pub name: String,
    /// Type prefix plus the first 6 random characters.
    pub prefix: String,
    /// Creation time. JSON is RFC 3339.
    #[serde(with = "time_serde")]
    #[schema(value_type = String, format = DateTime)]
    pub created_at: i64,
    /// User who created the token. Omitted when that user is unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_by: Option<Uuid>,
    /// Last successful authentication. Omitted until one has happened.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "optional_time"
    )]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub last_used_at: Option<i64>,
}

/// `PATCH /api/v1/orgs/{org}/scim` body. Absent fields keep their value.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdateScimSettings {
    /// Turns SCIM on or off.
    #[serde(default)]
    pub enabled: Option<bool>,
    /// The role of newly provisioned members: `viewer` or `member`.
    #[serde(default)]
    pub default_role: Option<OrgRole>,
}

/// `POST /api/v1/orgs/{org}/scim/tokens` body.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateScimTokenRequest {
    /// Display name, 1 to 100 characters.
    pub name: String,
}

/// The answer to a token create. The secret is shown only here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct CreatedScimToken {
    /// The stored token, without the secret.
    pub token: ScimToken,
    /// The bearer value (`scim-cetra-…`). Shown only in this response.
    pub secret: String,
}
