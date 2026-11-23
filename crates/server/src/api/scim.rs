// SPDX-License-Identifier: Elastic-2.0
//! The Provisioning API DTOs: an org's SCIM settings, bearer tokens (track EA M5,
//! D-EA-12, D-EA-15) and group role mappings (track EA M6, D-EA-14). The `/scim/v2`
//! resources themselves are in `ee::scim`.

use cetra_core::time_serde;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use super::account::OrgRole;
use super::keys::optional_time;
use super::members::ProjectRoleOverride;

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

/// One SCIM group of the org, with its role mapping (D-EA-14). The display name is shown to
/// owners on their settings page only; it is never logged or recorded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ScimGroup {
    /// Group id.
    pub id: Uuid,
    /// The name the identity provider gave the group.
    pub display_name: String,
    /// The identity provider's external id, when it sent one.
    pub external_id: Option<String>,
    /// How many SCIM users are in the group.
    pub member_count: u32,
    /// The group's role mapping. `null` when the group maps nothing.
    pub mapping: Option<ScimGroupMapping>,
}

/// A group's role mapping: an org role and one project role per project. Members get the
/// highest of their groups' roles (D-EA-14).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ScimGroupMapping {
    /// The org role the group gives: `admin`, `member` or `viewer`. `null` when it gives none.
    pub org_role: Option<OrgRole>,
    /// One project role per project. Empty when the group maps no project.
    pub projects: Vec<ScimProjectRole>,
}

/// A project role of a group mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ScimProjectRole {
    /// Project id.
    pub project_id: Uuid,
    /// The project role: `admin`, `member`, `viewer` or `none` (hidden).
    pub role: ProjectRoleOverride,
}

/// `GET /api/v1/orgs/{org}/scim/groups`: every SCIM group of the org, ordered by display name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ScimGroupList {
    /// The groups.
    pub groups: Vec<ScimGroup>,
}

/// `PUT /api/v1/orgs/{org}/scim/groups/{id}/mapping` body. The new mapping replaces the old
/// one; a mapping with neither an org role nor a project is refused (use DELETE).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PutScimGroupMapping {
    /// The org role the group gives: `admin`, `member` or `viewer`. Omit it to give none.
    #[serde(default)]
    pub org_role: Option<OrgRole>,
    /// One project role per project, at most 200. Omit the list to map no project.
    #[serde(default)]
    pub projects: Vec<ScimProjectRole>,
}
