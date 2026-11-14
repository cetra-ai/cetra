//! Entitlement types in the API (D179, D-P-12, D-P-15).

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// A feature an org may be entitled to. Names are append-only (D-P-12).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Feature {
    /// Data retention management: a core feature, granted by the instance defaults.
    RetentionManagement,
    /// Per-project role overrides (D139): Teams add-on or an enterprise licence.
    ProjectRoles,
    /// Instance-wide `auth.enforce_sso_domains`: an enterprise licence.
    SsoEnforcement,
}

impl Feature {
    /// Every feature, in declaration order.
    pub const ALL: [Self; 3] = [
        Self::RetentionManagement,
        Self::ProjectRoles,
        Self::SsoEnforcement,
    ];

    /// The wire name.
    pub fn name(self) -> &'static str {
        match self {
            Self::RetentionManagement => "retention_management",
            Self::ProjectRoles => "project_roles",
            Self::SsoEnforcement => "sso_enforcement",
        }
    }

    /// Enterprise features live in `ee` and are never granted by config (D-P-10).
    pub fn is_ee(self) -> bool {
        matches!(self, Self::ProjectRoles | Self::SsoEnforcement)
    }

    /// The feature named `name`; `None` for a name this build does not know.
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|feature| feature.name() == name)
    }

    /// The problem `detail` when an org lacks the feature (D-P-14). No payload.
    pub fn locked_detail(self) -> &'static str {
        match self {
            Self::RetentionManagement => "Data retention management is part of the Pro plan.",
            Self::ProjectRoles => {
                "Per-project roles need the Teams add-on or an enterprise licence."
            }
            Self::SsoEnforcement => "SSO enforcement needs an enterprise licence.",
        }
    }
}

/// `GET /api/v1/orgs/{org}/entitlements` (D-P-15). M2 adds the plan and limits; M3 the usage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct OrgEntitlements {
    /// Features the org may use now, in a fixed order.
    pub features: Vec<Feature>,
    /// Where an Upgrade button goes. Omitted when the instance sets none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upgrade_url: Option<String>,
}
