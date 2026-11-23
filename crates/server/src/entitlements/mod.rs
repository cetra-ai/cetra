//! One resolved `Entitlements` value per org (D179, D-P-10).
//!
//! M1 has one layer: the instance licence's features, while it is active or in grace.
//! M2 adds the instance defaults and the org's token.

use std::collections::HashMap;
use std::sync::{Arc, PoisonError, RwLock};

use cetra_core::ids::OrgId;

use crate::config::EntitlementDefaults;
use crate::ee::license::{InstanceLicense, LicenseState, LicenseStatus};
use crate::http::AppState;
use crate::http::error::ApiError;
use crate::meta::entitlements::window_ends_since;
use crate::meta::{MetaDb, MetaError};

pub(crate) mod notices;
pub(crate) mod rate;
pub(crate) mod tokens;
pub(crate) mod units;
pub(crate) mod window;

pub(crate) use crate::api::entitlements::Feature;
use tokens::{OrgGrant, OrgTokenError, grant_from_token};
pub(crate) use window::{Visibility, query_options};

/// The window history looks back this far (D-P-30).
const WINDOW_LOOKBACK_NS: i64 = 30 * 86_400 * 1_000_000_000;

/// A set of [`Feature`]s.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct FeatureSet(u32);

impl FeatureSet {
    /// No feature.
    pub(crate) const EMPTY: Self = Self(0);

    fn bit(feature: Feature) -> u32 {
        1 << (feature as u32)
    }

    /// Adds `feature`.
    pub(crate) fn insert(&mut self, feature: Feature) {
        self.0 |= Self::bit(feature);
    }

    /// Whether `feature` is in the set.
    pub(crate) fn contains(self, feature: Feature) -> bool {
        self.0 & Self::bit(feature) != 0
    }

    /// The known features named in `names`; unknown names are skipped (D-P-5).
    pub(crate) fn from_names<'a>(names: impl IntoIterator<Item = &'a str>) -> Self {
        let mut set = Self::EMPTY;
        for feature in names.into_iter().filter_map(Feature::from_name) {
            set.insert(feature);
        }
        set
    }

    /// The features in either set.
    pub(crate) fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// The features in the set, in [`Feature::ALL`] order.
    pub(crate) fn to_vec(self) -> Vec<Feature> {
        Feature::ALL
            .into_iter()
            .filter(|feature| self.contains(*feature))
            .collect()
    }
}

/// The limits of D-P-13 and D-IC-13 for one org. `None` is unlimited.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Limits {
    pub(crate) units_included: Option<u64>,
    pub(crate) units_cap: Option<u64>,
    pub(crate) ingest_spans_per_sec: Option<u64>,
    pub(crate) ingest_burst: Option<u64>,
    pub(crate) data_access_days: Option<u64>,
    pub(crate) max_seats: Option<u64>,
    pub(crate) max_projects: Option<u64>,
    pub(crate) max_alert_rules: Option<u64>,
    pub(crate) max_active_online_rules: Option<u64>,
    pub(crate) max_active_issue_sources: Option<u64>,
    pub(crate) max_annotation_queues: Option<u64>,
    /// Audit log retention in days (D-EA-7). `None`: the instance's `audit.retention_days`;
    /// `Some(0)`: kept forever; otherwise at least 30 days (plan P-4).
    pub(crate) audit_retention_days: Option<u64>,
}

/// The instance defaults, resolved from config once (`[entitlements.defaults]`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Defaults {
    pub(crate) plan_name: String,
    pub(crate) features: FeatureSet,
    pub(crate) limits: Limits,
}

impl Defaults {
    /// From config. `features: None` is every core feature.
    pub(crate) fn from_config(config: &EntitlementDefaults) -> Self {
        let features = match &config.features {
            Some(names) => FeatureSet::from_names(names.iter().map(String::as_str)),
            None => Feature::ALL
                .into_iter()
                .filter(|feature| !feature.is_ee())
                .fold(FeatureSet::EMPTY, |mut set, feature| {
                    set.insert(feature);
                    set
                }),
        };
        Self {
            plan_name: config.plan_name.clone(),
            features,
            limits: Limits {
                units_included: config.units_included,
                units_cap: config.units_cap,
                ingest_spans_per_sec: config.ingest_spans_per_sec,
                ingest_burst: config.ingest_burst,
                data_access_days: config.data_access_days,
                max_seats: config.max_seats,
                max_projects: config.max_projects,
                max_alert_rules: config.max_alert_rules,
                max_active_online_rules: config.max_active_online_rules,
                max_active_issue_sources: config.max_active_issue_sources,
                max_annotation_queues: config.max_annotation_queues,
                // No `[entitlements.defaults]` key: the instance's `audit.retention_days` applies.
                audit_retention_days: None,
            },
        }
    }
}

/// What one org may do now (D179).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Entitlements {
    features: FeatureSet,
    pub(crate) limits: Limits,
}

impl Entitlements {
    /// Entitlements with exactly `features` and `limits`.
    pub(crate) fn new(features: FeatureSet, limits: Limits) -> Self {
        Self { features, limits }
    }

    /// The current window plus 30 days, in days; `None` without a window (D-P-22). Ingest's D85
    /// age check uses it. M's retention uses the window history instead (D-P-45).
    pub(crate) fn retention_cap_days(self) -> Option<u32> {
        retention_cap(self.limits.data_access_days)
    }

    /// Whether the org may use `feature`.
    pub(crate) fn allows(self, feature: Feature) -> bool {
        self.features.contains(feature)
    }

    /// Every allowed feature, in [`Feature::ALL`] order.
    pub(crate) fn features(self) -> Vec<Feature> {
        self.features.to_vec()
    }
}

/// Where an org's plan and limits come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Source {
    /// `[entitlements.defaults]`: no valid org token.
    Defaults,
    /// The org's token, until its grace period ends.
    OrgToken,
}

/// An org token's state at one instant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TokenStatus {
    /// Before the token's `expires_at`.
    Active,
    /// From `expires_at` to before `grace_ends_at`.
    Grace,
}

/// The org's plan as shown to users (D-P-32).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PlanView {
    /// The token's plan id; `None` with the defaults.
    pub(crate) id: Option<String>,
    /// The token's plan name, or `defaults.plan_name`.
    pub(crate) name: String,
    /// The token's add-ons; empty with the defaults.
    pub(crate) add_ons: Vec<String>,
    /// Where the plan comes from.
    pub(crate) source: Source,
    /// Token status at the instant; `None` with the defaults.
    pub(crate) status: Option<TokenStatus>,
    /// The token's `expires_at`, ns.
    pub(crate) expires_at: Option<i64>,
    /// The token's `grace_ends_at`, ns.
    pub(crate) grace_ends_at: Option<i64>,
}

/// Entitlement sources held by the process (D-P-10, D-P-11).
#[derive(Debug)]
pub(crate) struct EntitlementsState {
    /// The instance licence; replaced when an admin installs a key (D-P-75).
    license: RwLock<Arc<InstanceLicense>>,
    /// Serializes licence installs: the stored key and the held licence change together.
    license_writes: tokio::sync::Mutex<()>,
    defaults: Defaults,
    /// The newest accepted token per org, valid or in grace at the time it was read.
    orgs: RwLock<HashMap<OrgId, Arc<OrgGrant>>>,
    /// `entitlements.control_plane`: the org tokens' SSO domains count for SCIM only with it on.
    control_plane: bool,
}

impl EntitlementsState {
    /// Reads the licence, the instance defaults and the stored org tokens.
    ///
    /// A stored token that no longer verifies for this cell (a retired key, a changed
    /// `public_url`) is skipped with a `warn` naming the org id and the reason.
    ///
    /// With `control_plane` off (D-P-50) no stored token is read: the org map stays empty.
    pub(crate) async fn load(
        meta: &MetaDb,
        license: InstanceLicense,
        defaults: Defaults,
        public_url: &str,
        control_plane: bool,
    ) -> Result<Self, MetaError> {
        let mut orgs = HashMap::new();
        let stored_tokens = crate::meta::entitlements::all_tokens(meta.reader()).await?;
        if !control_plane {
            if !stored_tokens.is_empty() {
                tracing::warn!(
                    count = stored_tokens.len(),
                    "stored org tokens ignored: entitlements.control_plane is off"
                );
            }
            return Ok(Self {
                license: RwLock::new(Arc::new(license)),
                license_writes: tokio::sync::Mutex::new(()),
                defaults,
                orgs: RwLock::new(orgs),
                control_plane,
            });
        }
        for stored in stored_tokens {
            let granted = grant_from_token(&stored.token, public_url, &defaults.limits).and_then(
                |(org, grant)| {
                    if org == stored.org_id {
                        Ok(grant)
                    } else {
                        Err(OrgTokenError::OrgMismatch)
                    }
                },
            );
            match granted {
                Ok(grant) => {
                    orgs.insert(stored.org_id, Arc::new(grant));
                }
                Err(error) => tracing::warn!(
                    org_id = %stored.org_id,
                    reason = %error,
                    "stored org token skipped"
                ),
            }
        }
        Ok(Self {
            license: RwLock::new(Arc::new(license)),
            license_writes: tokio::sync::Mutex::new(()),
            defaults,
            orgs: RwLock::new(orgs),
            control_plane,
        })
    }

    /// The instance licence now.
    pub(crate) fn license(&self) -> Arc<InstanceLicense> {
        self.license
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Replaces the instance licence. Every later check sees it (D-P-75).
    pub(crate) fn set_license(&self, license: InstanceLicense) {
        *self.license.write().unwrap_or_else(PoisonError::into_inner) = Arc::new(license);
    }

    /// Held while a licence key is stored and set, so two installs cannot interleave.
    pub(crate) async fn lock_license_writes(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.license_writes.lock().await
    }

    /// The instance defaults.
    pub(crate) fn defaults(&self) -> &Defaults {
        &self.defaults
    }

    /// The org's held grant, valid or not at the current instant.
    pub(crate) fn grant(&self, org: OrgId) -> Option<Arc<OrgGrant>> {
        self.orgs
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&org)
            .cloned()
    }

    /// The org's SSO domains from its org token, which SCIM treats as verified (D-EA-17): the
    /// grant's list while `now < grace_ends_at`; empty otherwise and on a cell without
    /// `entitlements.control_plane`.
    pub(crate) fn sso_domains(&self, org: OrgId, now: i64) -> Vec<String> {
        if !self.control_plane {
            return Vec::new();
        }
        match self.grant(org) {
            Some(grant) if now < grant.grace_ends_at => grant.sso_domains.clone(),
            _ => Vec::new(),
        }
    }

    /// Replaces the org's grant if `grant.issued_at` is newer than the held one.
    pub(crate) fn put_grant(&self, org: OrgId, grant: OrgGrant) {
        let mut orgs = self.orgs.write().unwrap_or_else(PoisonError::into_inner);
        if orgs
            .get(&org)
            .is_some_and(|held| held.issued_at >= grant.issued_at)
        {
            return;
        }
        orgs.insert(org, Arc::new(grant));
    }

    /// What `org` may do at `now`: the org's token while it is in grace, else the defaults,
    /// plus the licence's features while it is active or in grace.
    pub(crate) fn resolve(&self, org: OrgId, now: i64) -> Entitlements {
        let licensed = self.license().state.active_features(now);
        match self.grant(org) {
            Some(grant) if now < grant.grace_ends_at => {
                Entitlements::new(grant.features.union(licensed), grant.limits)
            }
            _ => Entitlements::new(self.defaults.features.union(licensed), self.defaults.limits),
        }
    }

    /// The org's plan at `now` (D-P-32).
    pub(crate) fn plan_at(&self, org: OrgId, now: i64) -> PlanView {
        match self.grant(org) {
            Some(grant) if now < grant.grace_ends_at => PlanView {
                id: Some(grant.plan.id.clone()),
                name: grant.plan.name.clone(),
                add_ons: grant.plan.add_ons.clone(),
                source: Source::OrgToken,
                status: Some(if now < grant.expires_at {
                    TokenStatus::Active
                } else {
                    TokenStatus::Grace
                }),
                expires_at: Some(grant.expires_at),
                grace_ends_at: Some(grant.grace_ends_at),
            },
            _ => PlanView {
                id: None,
                name: self.defaults.plan_name.clone(),
                add_ons: Vec::new(),
                source: Source::Defaults,
                status: None,
                expires_at: None,
                grace_ends_at: None,
            },
        }
    }

    /// The largest data window (days; `None` = unlimited) that applied to `org` during the
    /// last 30 days (D-P-30): the current one, every recorded end since `now − 30 days`, and
    /// the held token's window if it expired since then. `None` anywhere wins.
    pub(crate) async fn retention_window_at(
        &self,
        meta: &MetaDb,
        org: OrgId,
        now: i64,
    ) -> Result<Option<u64>, MetaError> {
        let since = now.saturating_sub(WINDOW_LOOKBACK_NS);
        let mut windows = vec![self.resolve(org, now).limits.data_access_days];
        windows.extend(
            window_ends_since(meta.reader(), org, since)
                .await?
                .into_iter()
                .map(|(_, days)| days),
        );
        if let Some(grant) = self
            .grant(org)
            .filter(|grant| (since..=now).contains(&grant.grace_ends_at))
        {
            windows.push(grant.limits.data_access_days);
        }
        Ok(windows
            .into_iter()
            .try_fold(0_u64, |largest, days| days.map(|days| largest.max(days))))
    }
}

/// The retention cap of a data window of `days` (D-P-22, D-P-45): the window plus 30 days, in
/// days, saturating at `u32::MAX`. `None` (no window) caps nothing.
pub(crate) fn retention_cap(days: Option<u64>) -> Option<u32> {
    days.map(|days| u32::try_from(days.saturating_add(30)).unwrap_or(u32::MAX))
}

/// What `org` may do now. Synchronous and in memory; call it at every check (D-P-10).
pub(crate) fn of(state: &AppState, org: OrgId) -> Entitlements {
    state.entitlements.resolve(org, state.clock.now_ns())
}

/// Ok when `org` may use `feature`; otherwise the `plan_required` error that names it (D-P-14).
pub(crate) fn require(state: &AppState, org: OrgId, feature: Feature) -> Result<(), ApiError> {
    if of(state, org).allows(feature) {
        Ok(())
    } else {
        Err(ApiError::PlanRequired {
            detail: feature.locked_detail(),
        })
    }
}

/// Whether `count + adding` would exceed `limit` (D-P-20). `None` never does.
pub(crate) fn over_limit(limit: Option<u64>, count: u64, adding: u64) -> bool {
    limit.is_some_and(|limit| count.saturating_add(adding) > limit)
}

/// The `plan_limit` error for `limit` (D-P-14). `noun` is (singular, plural).
pub(crate) fn plan_limit(limit: u64, noun: (&str, &str)) -> ApiError {
    ApiError::PlanLimit {
        detail: format!(
            "Your plan allows {limit} {}.",
            if limit == 1 { noun.0 } else { noun.1 }
        ),
    }
}

/// `entitlements.upgrade_url` for `org`, or `None` when unset (D-P-15).
pub(crate) fn upgrade_url(state: &AppState, org: OrgId) -> Option<String> {
    state
        .config
        .entitlements
        .upgrade_url
        .as_deref()
        .map(|template| template.replace("{org}", &org.to_string()))
}

/// Logs the licence status once at startup: status, licence id and key id only (D-P-7).
pub(crate) fn log_license(license: &LicenseState, now_ns: i64) {
    match (license, license.status(now_ns)) {
        (LicenseState::Absent, _) => {
            tracing::info!("no licence key: enterprise features are locked");
        }
        (LicenseState::Invalid(reason), _) => tracing::error!(
            reason = %reason,
            "the licence key was refused: running without a licence"
        ),
        (LicenseState::Valid(license), LicenseStatus::Active) => tracing::info!(
            license_id = %license.id,
            kid = %license.kid,
            "licence active"
        ),
        (LicenseState::Valid(license), LicenseStatus::Grace) => tracing::warn!(
            license_id = %license.id,
            kid = %license.kid,
            "licence expired: enterprise features stop at the end of the grace period"
        ),
        (LicenseState::Valid(license), _) => tracing::warn!(
            license_id = %license.id,
            kid = %license.kid,
            "licence expired: enterprise features are locked"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retention_cap_is_the_window_plus_thirty_days() {
        assert_eq!(retention_cap(None), None);
        assert_eq!(retention_cap(Some(30)), Some(60));
        assert_eq!(retention_cap(Some(u64::MAX)), Some(u32::MAX));
        assert_eq!(retention_cap(Some(u64::from(u32::MAX))), Some(u32::MAX));
        assert_eq!(
            Entitlements::new(
                FeatureSet::EMPTY,
                Limits {
                    data_access_days: Some(30),
                    ..Limits::default()
                }
            )
            .retention_cap_days(),
            Some(60)
        );
    }
    use crate::config::{EntitlementDefaults, PublicUrl, Secret};
    use crate::ee::license::{InvalidLicense, LicenseSource};
    use crate::meta::entitlements::{StoredToken, put_token, record_window_end};
    use crate::meta::orgs::insert_org;
    use crate::test_support::{
        TestApp, capture_logs, license_key, license_key_until, org_token, org_token_with_domains,
        sign_token,
    };
    use cetra_token::TokenError;
    use serde_json::json;
    use tokens::grant_from_token;
    use uuid::Uuid;

    const DAY_NS: i64 = 86_400 * 1_000_000_000;

    /// Nanoseconds of an RFC 3339 instant.
    fn at(rfc3339: &str) -> i64 {
        chrono::DateTime::parse_from_rfc3339(rfc3339)
            .unwrap()
            .timestamp_nanos_opt()
            .unwrap()
    }

    /// A grant issued at `issued_at`, with no features and no limits.
    fn grant_issued_at(issued_at: i64) -> OrgGrant {
        OrgGrant {
            token_id: Uuid::now_v7(),
            issued_at,
            expires_at: i64::MAX,
            grace_ends_at: i64::MAX,
            plan: tokens::PlanInfo {
                id: "pro".to_owned(),
                name: "Pro".to_owned(),
                add_ons: Vec::new(),
            },
            features: FeatureSet::EMPTY,
            limits: Limits::default(),
            sso_domains: Vec::new(),
        }
    }

    /// The grant a dev-signed org token for `org` yields on this cell.
    fn grant_for(
        org: OrgId,
        expires_at: &str,
        grace_ends_at: &str,
        features: &[&str],
        limits: &serde_json::Value,
    ) -> OrgGrant {
        let token = org_token(
            org,
            "2025-12-01T00:00:00Z",
            expires_at,
            grace_ends_at,
            features,
            limits,
        );
        grant_from_token(&token, PublicUrl::default().origin(), &Limits::default())
            .unwrap()
            .1
    }

    /// No licence key.
    fn absent_license() -> InstanceLicense {
        InstanceLicense {
            state: LicenseState::Absent,
            source: LicenseSource::Missing,
        }
    }

    /// Inserts an org in the meta database and returns its id.
    async fn new_org(app: &TestApp, slug: &str) -> OrgId {
        let slug = slug.to_owned();
        let now = app.state.clock.now_ns();
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move { insert_org(conn, OrgId::new_v7(), "Acme", &slug, now).await })
            })
            .await
            .unwrap()
            .id
    }

    #[test]
    fn feature_names_round_trip() {
        for feature in Feature::ALL {
            assert_eq!(Feature::from_name(feature.name()), Some(feature));
        }
        assert_eq!(
            serde_json::to_string(&Feature::ProjectRoles).unwrap(),
            "\"project_roles\""
        );
        assert_eq!(Feature::from_name("org_sso"), Some(Feature::OrgSso));
        assert!(Feature::OrgSso.is_ee());
        assert_eq!(Feature::from_name("scim"), Some(Feature::Scim));
        assert!(Feature::Scim.is_ee());
        assert_eq!(Feature::from_name("audit_log"), Some(Feature::AuditLog));
        assert!(Feature::AuditLog.is_ee());
        assert_eq!(Feature::from_name("audit_logs"), None);
        assert!(Feature::ALL.len() <= 32);
    }

    #[test]
    fn feature_set_ignores_unknown_names() {
        let set = FeatureSet::from_names(["project_roles", "audit_logs", "project_roles"]);
        assert_eq!(set.to_vec(), vec![Feature::ProjectRoles]);
    }

    #[test]
    fn entitlements_allow_listed_features() {
        let entitlements = Entitlements::new(
            FeatureSet::from_names(["sso_enforcement", "project_roles"]),
            Limits::default(),
        );
        assert!(entitlements.allows(Feature::ProjectRoles));
        assert!(entitlements.allows(Feature::SsoEnforcement));
        assert_eq!(
            entitlements.features(),
            vec![Feature::ProjectRoles, Feature::SsoEnforcement]
        );
        assert!(
            !Entitlements::new(FeatureSet::EMPTY, Limits::default()).allows(Feature::ProjectRoles)
        );
    }

    #[tokio::test]
    async fn sso_domains_empty_after_grace() {
        let app = TestApp::with_config(|config| config.entitlements.control_plane = true).await;
        let org = OrgId::new_v7();
        let token = org_token_with_domains(
            org,
            "2025-12-01T00:00:00Z",
            "2026-01-01T00:00:00Z",
            "2026-02-01T00:00:00Z",
            &[],
            &json!({}),
            &["acme.test"],
        );
        let (granted, grant) =
            grant_from_token(&token, PublicUrl::default().origin(), &Limits::default()).unwrap();
        app.state.entitlements.put_grant(granted, grant);
        let domains = vec!["acme.test".to_owned()];

        // Before expiry, and in grace, the token's domains count.
        assert_eq!(
            app.state
                .entitlements
                .sso_domains(org, at("2025-12-15T00:00:00Z")),
            domains
        );
        assert_eq!(
            app.state
                .entitlements
                .sso_domains(org, at("2026-01-15T00:00:00Z")),
            domains
        );
        // From grace_ends_at on, none.
        assert!(
            app.state
                .entitlements
                .sso_domains(org, at("2026-02-01T00:00:00Z"))
                .is_empty()
        );
        // An org without a token has none.
        assert!(
            app.state
                .entitlements
                .sso_domains(OrgId::new_v7(), at("2025-12-15T00:00:00Z"))
                .is_empty()
        );
    }

    #[tokio::test]
    async fn sso_domains_are_empty_on_a_cell_without_control_plane() {
        let app = TestApp::with_config(|config| config.entitlements.control_plane = false).await;
        let org = OrgId::new_v7();
        let token = org_token_with_domains(
            org,
            "2025-12-01T00:00:00Z",
            "2099-01-01T00:00:00Z",
            "2099-02-01T00:00:00Z",
            &[],
            &json!({}),
            &["acme.test"],
        );
        let (granted, grant) =
            grant_from_token(&token, PublicUrl::default().origin(), &Limits::default()).unwrap();
        app.state.entitlements.put_grant(granted, grant);
        assert!(
            app.state
                .entitlements
                .sso_domains(org, at("2025-12-15T00:00:00Z"))
                .is_empty()
        );
    }

    #[tokio::test]
    async fn defaults_grant_every_core_feature() {
        let app = TestApp::new().await;
        let entitlements = of(&app.state, OrgId::new_v7());
        assert!(entitlements.allows(Feature::RetentionManagement));
        assert!(!entitlements.allows(Feature::ProjectRoles));
        assert!(!entitlements.allows(Feature::SsoEnforcement));
        assert_eq!(entitlements.limits, Limits::default());
    }

    #[tokio::test]
    async fn configured_defaults_apply() {
        let app = TestApp::with_config(|config| {
            config.entitlements.defaults = EntitlementDefaults {
                features: Some(Vec::new()),
                units_cap: Some(100_000),
                max_seats: Some(1),
                ..EntitlementDefaults::default()
            };
        })
        .await;
        let entitlements = of(&app.state, OrgId::new_v7());
        assert!(!entitlements.allows(Feature::RetentionManagement));
        assert_eq!(entitlements.limits.units_cap, Some(100_000));
        assert_eq!(entitlements.limits.max_seats, Some(1));
        assert_eq!(entitlements.limits.units_included, None);
    }

    #[tokio::test]
    async fn licence_adds_ee_features_to_defaults() {
        let key = license_key(&["project_roles"]);
        let app = TestApp::with_config(move |config| {
            config.license_key = Some(Secret::new(key));
            config.entitlements.defaults.features = Some(Vec::new());
        })
        .await;
        let entitlements = of(&app.state, OrgId::new_v7());
        assert!(entitlements.allows(Feature::ProjectRoles));
        assert!(!entitlements.allows(Feature::RetentionManagement));
    }

    #[tokio::test]
    async fn no_license_allows_nothing() {
        let app = TestApp::new().await;
        let entitlements = of(&app.state, OrgId::new_v7());
        assert_eq!(entitlements.features(), vec![Feature::RetentionManagement]);
        assert!(!entitlements.allows(Feature::ProjectRoles));
    }

    #[tokio::test]
    async fn license_features_apply_to_every_org() {
        let app = TestApp::with_license(&["project_roles"]).await;
        for org in [OrgId::new_v7(), OrgId::new_v7()] {
            let entitlements = of(&app.state, org);
            assert!(entitlements.allows(Feature::ProjectRoles));
            assert!(!entitlements.allows(Feature::SsoEnforcement));
        }
    }

    #[tokio::test]
    async fn set_license_applies_at_once() {
        let app = TestApp::new().await;
        let org = OrgId::new_v7();
        assert!(!of(&app.state, org).allows(Feature::ProjectRoles));

        app.state.entitlements.set_license(InstanceLicense {
            state: LicenseState::from_key(Some(&license_key(&["project_roles"]))),
            source: LicenseSource::Stored,
        });
        assert!(of(&app.state, org).allows(Feature::ProjectRoles));
        assert_eq!(
            app.state.entitlements.license().source,
            LicenseSource::Stored
        );
    }

    #[tokio::test]
    async fn expired_license_allows_nothing() {
        let key = license_key_until(
            &["project_roles"],
            "2025-12-01T00:00:00Z",
            "2025-12-31T00:00:00Z",
        );
        let app = TestApp::with_config(move |config| {
            config.license_key = Some(Secret::new(key));
        })
        .await;
        let entitlements = of(&app.state, OrgId::new_v7());
        assert_eq!(entitlements.features(), vec![Feature::RetentionManagement]);
        assert!(
            app.state
                .entitlements
                .license()
                .state
                .listed_features()
                .contains(Feature::ProjectRoles)
        );
    }

    #[tokio::test]
    async fn refused_key_runs_without_license() {
        let app = TestApp::with_config(|config| {
            config.license_key = Some(Secret::new("garbage".to_owned()));
        })
        .await;
        assert!(matches!(
            app.state.entitlements.license().state,
            LicenseState::Invalid(InvalidLicense::Token(TokenError::Malformed))
        ));
        assert_eq!(
            of(&app.state, OrgId::new_v7()).features(),
            vec![Feature::RetentionManagement]
        );
    }

    #[tokio::test]
    async fn require_maps_to_plan_required() {
        let app = TestApp::new().await;
        let org = OrgId::new_v7();
        assert!(matches!(
            require(&app.state, org, Feature::ProjectRoles),
            Err(ApiError::PlanRequired { detail }) if detail == Feature::ProjectRoles.locked_detail()
        ));

        let app = TestApp::with_license(&["project_roles"]).await;
        assert!(require(&app.state, org, Feature::ProjectRoles).is_ok());
        assert!(require(&app.state, org, Feature::SsoEnforcement).is_err());
    }

    #[tokio::test]
    async fn org_sso_follows_the_license() {
        let org = OrgId::new_v7();
        let app = TestApp::new().await;
        assert!(!of(&app.state, org).allows(Feature::OrgSso));

        let app = TestApp::with_license(&["org_sso"]).await;
        assert!(of(&app.state, org).allows(Feature::OrgSso));
        assert!(require(&app.state, org, Feature::OrgSso).is_ok());
    }

    #[tokio::test]
    async fn log_license_never_prints_the_customer() {
        let app = TestApp::new().await;
        let now = app.state.clock.now_ns();
        let valid = LicenseState::from_key(Some(&license_key(&["project_roles"])));
        let (_, logs) = capture_logs(async { log_license(&valid, now) }).await;
        assert!(logs.contains("licence active"), "{logs}");
        assert!(
            logs.contains("01900000-0000-7000-8000-000000000510"),
            "{logs}"
        );
        assert!(!logs.contains("Test Customer"), "{logs}");
        assert!(!logs.contains("cetra1."), "{logs}");

        let refused = LicenseState::from_key(Some("garbage"));
        let (_, logs) = capture_logs(async { log_license(&refused, now) }).await;
        assert!(logs.contains("refused"), "{logs}");
        assert!(!logs.contains("garbage"), "{logs}");
    }

    #[tokio::test]
    async fn token_applies_until_grace_ends() {
        let app = TestApp::new().await;
        let org = OrgId::new_v7();
        let grant = grant_for(
            org,
            "2026-02-01T00:00:00Z",
            "2026-03-01T00:00:00Z",
            &["project_roles"],
            &json!({}),
        );
        app.state.entitlements.put_grant(org, grant);

        app.state.clock.set(at("2026-01-01T00:00:00Z"));
        let plan = app
            .state
            .entitlements
            .plan_at(org, app.state.clock.now_ns());
        assert_eq!(plan.source, Source::OrgToken);
        assert_eq!(plan.status, Some(TokenStatus::Active));
        let entitlements = of(&app.state, org);
        assert!(entitlements.allows(Feature::ProjectRoles));
        assert!(!entitlements.allows(Feature::RetentionManagement));

        app.state.clock.set(at("2026-02-15T00:00:00Z"));
        let plan = app
            .state
            .entitlements
            .plan_at(org, app.state.clock.now_ns());
        assert_eq!(plan.source, Source::OrgToken);
        assert_eq!(plan.status, Some(TokenStatus::Grace));
        assert!(of(&app.state, org).allows(Feature::ProjectRoles));

        app.state.clock.set(at("2026-03-01T00:00:00Z"));
        let plan = app
            .state
            .entitlements
            .plan_at(org, app.state.clock.now_ns());
        assert_eq!(plan.source, Source::Defaults);
        assert_eq!(plan.status, None);
        let entitlements = of(&app.state, org);
        assert!(entitlements.allows(Feature::RetentionManagement));
        assert!(!entitlements.allows(Feature::ProjectRoles));
    }

    #[tokio::test]
    async fn older_grant_does_not_replace_newer() {
        let app = TestApp::new().await;
        let org = OrgId::new_v7();
        app.state.entitlements.put_grant(org, grant_issued_at(10));
        app.state.entitlements.put_grant(org, grant_issued_at(5));
        assert_eq!(app.state.entitlements.grant(org).unwrap().issued_at, 10);
    }

    #[tokio::test]
    async fn ee_features_from_a_token_need_no_licence() {
        let app = TestApp::new().await;
        let org = OrgId::new_v7();
        let grant = grant_for(
            org,
            "2099-01-01T00:00:00Z",
            "2099-02-01T00:00:00Z",
            &["project_roles"],
            &json!({}),
        );
        app.state.entitlements.put_grant(org, grant);
        let entitlements = of(&app.state, org);
        assert!(entitlements.allows(Feature::ProjectRoles));
        assert!(!entitlements.allows(Feature::SsoEnforcement));
    }

    #[tokio::test]
    async fn load_skips_tokens_that_no_longer_verify() {
        let app = TestApp::new().await;
        let valid_org = new_org(&app, "valid").await;
        let other_org = new_org(&app, "other").await;
        let received = app.state.clock.now_ns();

        let valid = org_token(
            valid_org,
            "2025-12-01T00:00:00Z",
            "2099-01-01T00:00:00Z",
            "2099-02-01T00:00:00Z",
            &[],
            &json!({}),
        );
        let foreign = sign_token(&json!({
            "kind": "org",
            "id": Uuid::now_v7().to_string(),
            "issued_at": "2025-12-01T00:00:00Z",
            "expires_at": "2099-01-01T00:00:00Z",
            "grace_ends_at": "2099-02-01T00:00:00Z",
            "audience": "https://elsewhere.example",
            "org_id": other_org.to_string(),
            "plan": {"id": "pro", "name": "Pro"},
        }));
        let stored = [(valid_org, valid), (other_org, foreign)];
        for (org_id, token) in stored {
            let record = StoredToken {
                org_id,
                token,
                token_id: Uuid::now_v7(),
                issued_at: received - 1,
                received_at: received,
            };
            app.state
                .meta
                .write(move |conn| Box::pin(async move { put_token(conn, &record).await }))
                .await
                .unwrap();
        }

        let defaults = app.state.entitlements.defaults().clone();
        let (loaded, logs) = capture_logs(EntitlementsState::load(
            &app.state.meta,
            absent_license(),
            defaults,
            PublicUrl::default().origin(),
            true,
        ))
        .await;
        let loaded = loaded.unwrap();
        assert!(loaded.grant(valid_org).is_some());
        assert!(loaded.grant(other_org).is_none());
        assert!(logs.contains("WARN"), "{logs}");
        assert!(logs.contains(&other_org.to_string()), "{logs}");
        assert!(logs.contains("the token is for another cell"), "{logs}");
        assert!(!logs.contains("cetra1."), "{logs}");
    }

    #[tokio::test]
    async fn stored_tokens_are_ignored_when_off() {
        let app = TestApp::new().await;
        let org = new_org(&app, "opt-out").await;
        let received = app.state.clock.now_ns();
        let token = org_token(
            org,
            "2025-12-01T00:00:00Z",
            "2099-01-01T00:00:00Z",
            "2099-02-01T00:00:00Z",
            &[],
            &json!({}),
        );
        let record = StoredToken {
            org_id: org,
            token,
            token_id: Uuid::now_v7(),
            issued_at: received - 1,
            received_at: received,
        };
        app.state
            .meta
            .write(move |conn| Box::pin(async move { put_token(conn, &record).await }))
            .await
            .unwrap();

        let defaults = app.state.entitlements.defaults().clone();
        let (loaded, logs) = capture_logs(EntitlementsState::load(
            &app.state.meta,
            absent_license(),
            defaults,
            PublicUrl::default().origin(),
            false,
        ))
        .await;
        let loaded = loaded.unwrap();
        assert!(loaded.grant(org).is_none());
        assert_eq!(
            loaded.resolve(org, received),
            app.state.entitlements.resolve(org, received)
        );
        assert!(logs.contains("stored org tokens ignored"), "{logs}");
        assert!(logs.contains("count=1"), "{logs}");
        assert!(!logs.contains("cetra1."), "{logs}");
    }

    #[tokio::test]
    async fn retention_window_takes_the_largest_of_30_days() {
        let app = TestApp::new().await;
        let org = new_org(&app, "retention").await;
        let now = app.state.clock.now_ns();
        let grant = grant_for(
            org,
            "2099-01-01T00:00:00Z",
            "2099-02-01T00:00:00Z",
            &[],
            &json!({"data_access_days": 30}),
        );
        app.state.entitlements.put_grant(org, grant);

        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    record_window_end(conn, org, now - 10 * DAY_NS, Some(1825)).await?;
                    record_window_end(conn, org, now - 40 * DAY_NS, None).await
                })
            })
            .await
            .unwrap();
        let window = app
            .state
            .entitlements
            .retention_window_at(&app.state.meta, org, now)
            .await
            .unwrap();
        assert_eq!(window, Some(1825));

        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move { record_window_end(conn, org, now - 5 * DAY_NS, None).await })
            })
            .await
            .unwrap();
        let window = app
            .state
            .entitlements
            .retention_window_at(&app.state.meta, org, now)
            .await
            .unwrap();
        assert_eq!(window, None);
    }
}
