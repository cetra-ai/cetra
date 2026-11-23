//! Entitlement types in the API (D179, D-P-12, D-P-15).

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::entitlements::{Limits, PlanView, Source, TokenStatus};

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
    /// Per-org OIDC providers, domains and enforcement (M5): Teams add-on or an enterprise licence.
    OrgSso,
    /// SCIM 2.0 provisioning (track EA M5): Enterprise or an enterprise licence.
    Scim,
    /// The audit log: reads and exports (track EA M3): Enterprise or an enterprise licence.
    AuditLog,
}

impl Feature {
    /// Every feature, in declaration order.
    pub const ALL: [Self; 6] = [
        Self::RetentionManagement,
        Self::ProjectRoles,
        Self::SsoEnforcement,
        Self::OrgSso,
        Self::Scim,
        Self::AuditLog,
    ];

    /// The wire name.
    pub fn name(self) -> &'static str {
        match self {
            Self::RetentionManagement => "retention_management",
            Self::ProjectRoles => "project_roles",
            Self::SsoEnforcement => "sso_enforcement",
            Self::OrgSso => "org_sso",
            Self::Scim => "scim",
            Self::AuditLog => "audit_log",
        }
    }

    /// Enterprise features live in `ee` and are never granted by config (D-P-10).
    pub fn is_ee(self) -> bool {
        matches!(
            self,
            Self::ProjectRoles | Self::SsoEnforcement | Self::OrgSso | Self::Scim | Self::AuditLog
        )
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
            Self::OrgSso => {
                "Single sign-on for an organization needs the Teams add-on or an enterprise licence."
            }
            Self::Scim => {
                "SCIM provisioning is part of the Enterprise plan and the self-hosted licence."
            }
            Self::AuditLog => {
                "The audit log is part of the Enterprise plan and the self-hosted licence."
            }
        }
    }
}

/// Where the org's plan comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum PlanSource {
    /// The instance defaults: no valid org token.
    Defaults,
    /// The org's token from the control plane.
    Token,
}

/// An org token's state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum PlanStatus {
    /// Before `expires_at`.
    Active,
    /// After `expires_at`, before `grace_ends_at`: the plan still applies.
    Grace,
}

/// The org's plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct OrgPlan {
    /// Plan id from the token. Omitted with the defaults.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Display name.
    pub name: String,
    /// Add-ons, e.g. `teams`.
    pub add_ons: Vec<String>,
    /// Where the plan comes from.
    pub source: PlanSource,
    /// Omitted with the defaults.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<PlanStatus>,
    /// Omitted with the defaults.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::api::keys::optional_time"
    )]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub expires_at: Option<i64>,
    /// Omitted with the defaults.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::api::keys::optional_time"
    )]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub grace_ends_at: Option<i64>,
}

/// The org's limits (D-P-13). An omitted limit is unlimited, except `audit_retention_days`
/// (see its doc).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct OrgLimits {
    /// Units included in the plan.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub units_included: Option<u64>,
    /// Hard cap on units.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub units_cap: Option<u64>,
    /// Sustained ingest rate, spans per second.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ingest_spans_per_sec: Option<u64>,
    /// Ingest burst, spans.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ingest_burst: Option<u64>,
    /// How many days of data the org may read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_access_days: Option<u64>,
    /// Seats.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_seats: Option<u64>,
    /// Projects.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_projects: Option<u64>,
    /// Alert rules.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_alert_rules: Option<u64>,
    /// Active online evaluation rules.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_active_online_rules: Option<u64>,
    /// Enabled issue sources of active projects.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_active_issue_sources: Option<u64>,
    /// Annotation queues.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_annotation_queues: Option<u64>,
    /// Audit log retention in days. Omitted: the instance's `audit.retention_days`;
    /// 0: kept forever.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audit_retention_days: Option<u64>,
}

impl From<Source> for PlanSource {
    fn from(source: Source) -> Self {
        match source {
            Source::Defaults => Self::Defaults,
            Source::OrgToken => Self::Token,
        }
    }
}

impl From<TokenStatus> for PlanStatus {
    fn from(status: TokenStatus) -> Self {
        match status {
            TokenStatus::Active => Self::Active,
            TokenStatus::Grace => Self::Grace,
        }
    }
}

impl From<PlanView> for OrgPlan {
    fn from(plan: PlanView) -> Self {
        Self {
            id: plan.id,
            name: plan.name,
            add_ons: plan.add_ons,
            source: plan.source.into(),
            status: plan.status.map(PlanStatus::from),
            expires_at: plan.expires_at,
            grace_ends_at: plan.grace_ends_at,
        }
    }
}

impl From<Limits> for OrgLimits {
    fn from(limits: Limits) -> Self {
        Self {
            units_included: limits.units_included,
            units_cap: limits.units_cap,
            ingest_spans_per_sec: limits.ingest_spans_per_sec,
            ingest_burst: limits.ingest_burst,
            data_access_days: limits.data_access_days,
            max_seats: limits.max_seats,
            max_projects: limits.max_projects,
            max_alert_rules: limits.max_alert_rules,
            max_active_online_rules: limits.max_active_online_rules,
            max_active_issue_sources: limits.max_active_issue_sources,
            max_annotation_queues: limits.max_annotation_queues,
            audit_retention_days: limits.audit_retention_days,
        }
    }
}

/// Usage level against `units_included`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum UsageNotice {
    /// Below 80 % of `units_included`, or no included units.
    None,
    /// At least 80 % of `units_included`, below 100 %.
    Warning,
    /// At least `units_included` (D-P-38).
    Reached,
}

/// This UTC month's units (D-P-16, D-P-38).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct OrgMonthUsage {
    /// `YYYY-MM`.
    pub month: String,
    /// Units so far this month, flushed or not.
    pub units: u64,
    /// 00:00 UTC on the 1st of the next month.
    #[serde(with = "cetra_core::time_serde")]
    #[schema(value_type = String, format = DateTime)]
    pub resets_at: i64,
    pub notice: UsageNotice,
}

/// `GET /api/v1/orgs/{org}/entitlements` (D-P-15).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct OrgEntitlements {
    /// Features the org may use now, in a fixed order.
    pub features: Vec<Feature>,
    /// The org's plan (D-P-32).
    pub plan: OrgPlan,
    /// The org's limits (D-P-13).
    pub limits: OrgLimits,
    /// The org's units this month (D-P-16).
    pub usage: OrgMonthUsage,
    /// Where an Upgrade button goes. Omitted when the instance sets none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upgrade_url: Option<String>,
}

/// `PUT /api/internal/v1/orgs/{org_id}/entitlements` (D-P-11). The signature is the credential.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PutOrgTokenRequest {
    /// A signed `kind: org` token. Never logged.
    pub token: String,
}
