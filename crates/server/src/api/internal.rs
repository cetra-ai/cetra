//! Types of the internal endpoints the cloud control plane calls (D-P-29).

use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
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

/// One org's usage on one UTC day, summed over its projects (D-P-24).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct InternalUsageRow {
    pub org_id: Uuid,
    /// `YYYY-MM-DD`, UTC.
    pub day: String,
    pub spans: u64,
    pub scores: u64,
    /// `spans + scores`.
    pub units: u64,
    pub bytes: u64,
    pub judge_input_tokens: u64,
    pub judge_output_tokens: u64,
    pub judge_cost_usd: f64,
}

/// Query of `GET /api/internal/v1/usage`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, IntoParams)]
#[serde(deny_unknown_fields)]
pub struct InternalUsageQuery {
    /// First day, inclusive, `YYYY-MM-DD`. Required.
    pub from: String,
    /// Last day, inclusive, `YYYY-MM-DD`. Required. `to - from` is at most 92 days.
    pub to: String,
    /// Rows per page, 1–10 000. Default 1 000.
    pub limit: Option<u32>,
    /// Opaque cursor from a previous page.
    pub cursor: Option<String>,
}
