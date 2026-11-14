//! Types of the internal endpoints the cloud control plane calls (D-P-29).

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

/// `POST /api/internal/v1/orgs` (D-P-29).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateInternalOrgRequest {
    /// The org id the control plane chose. A UUIDv7.
    pub id: Uuid,
    /// Display name, 1–100 characters after trimming (as `POST /api/v1/orgs`).
    pub name: String,
    /// The first owner.
    pub owner: InternalOwner,
}

/// The owner of a new org.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct InternalOwner {
    /// Email; normalized like every email (D136).
    pub email: String,
    /// Display name for a new user. Ignored when the user exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// The created or existing org.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct InternalOrg {
    /// The org id.
    pub id: Uuid,
    /// The org slug.
    pub slug: String,
}
