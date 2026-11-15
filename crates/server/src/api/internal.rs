//! Types of the internal endpoints the cloud control plane calls (D-P-29).

use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use crate::api::account::OrgRole;

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

/// Body of the `users` routes (D-P-39): the user's subject at the control plane's provider.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct InternalUserRef {
    /// The OIDC subject (`identities.subject`) of the user at `entitlements.control_plane_provider`.
    pub subject: String,
}

/// Answer of `POST /api/internal/v1/users/delete`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct InternalUserDeleted {
    /// Orgs the user was the only owner of. They are kept without an owner; the control
    /// plane resolves them.
    pub sole_owner_of: Vec<Uuid>,
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

/// A drop kind in the feed of `GET /api/internal/v1/deletions` (D-P-49).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum InternalDropKind {
    /// A project drop.
    Project,
    /// An org drop.
    Org,
}

/// One applied drop in the feed (D-P-49).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct InternalDrop {
    /// The deletion id.
    pub id: Uuid,
    pub kind: InternalDropKind,
    pub org_id: Uuid,
    /// Project drops only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<Uuid>,
    /// When the drop was applied.
    #[serde(with = "llmobs_core::time_serde")]
    #[schema(value_type = String, format = DateTime)]
    pub applied_at: i64,
}

/// Query of `GET /api/internal/v1/deletions`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, IntoParams)]
#[serde(deny_unknown_fields)]
pub struct InternalDropsQuery {
    /// `project` or `org`; both when omitted.
    pub kind: Option<InternalDropKind>,
    /// Rows per page, 1–10 000. Default 1 000.
    pub limit: Option<u32>,
    /// Opaque cursor from a previous page.
    pub cursor: Option<String>,
}

/// Body of `POST /api/internal/v1/memberships/lookup` (D-P-40). The subject is tried first;
/// the email only when no identity has the subject.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct MembershipLookupRequest {
    /// The OIDC subject at `entitlements.control_plane_provider`.
    pub subject: String,
    /// Email, normalized like every email (D136), for a user the subject does not find.
    pub email: String,
}

/// One active org the user belongs to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct MembershipOrg {
    /// The org id.
    pub org_id: Uuid,
    /// The org slug.
    pub slug: String,
    /// The user's role in the org.
    pub role: OrgRole,
}

/// Answer of `POST /api/internal/v1/memberships/lookup`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct MembershipLookup {
    /// The user's id. Absent when no user matches.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<Uuid>,
    /// The user's active orgs, sorted by slug.
    pub orgs: Vec<MembershipOrg>,
}

/// One member of an org, as `GET /api/internal/v1/orgs/{org_id}/members` lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct InternalMember {
    /// The user id.
    pub user_id: Uuid,
    /// The user's email. Sent to the control plane only; never logged.
    pub email: String,
    /// The user's display name, when it has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The member's org role.
    pub role: OrgRole,
}

/// Answer of `GET /api/internal/v1/orgs/{org_id}/members`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct InternalMembers {
    /// The members, ordered by email.
    pub data: Vec<InternalMember>,
}

/// Query of `GET /api/internal/v1/orgs/{org_id}/members`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, IntoParams)]
#[serde(deny_unknown_fields)]
pub struct InternalMembersQuery {
    /// Comma-separated roles to keep: `owner`, `admin`, `member`, `viewer`. Absent lists every
    /// member.
    pub role: Option<String>,
}
