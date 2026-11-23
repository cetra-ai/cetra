//! `sso.*` actions: org SSO. Written in the org's log. Domain text is never recorded.

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(tag = "action", content = "details")]
pub(crate) enum SsoAction {
    /// Target `sso_provider`.
    #[serde(rename = "sso.provider_saved")]
    #[serde(rename_all = "snake_case")]
    ProviderSaved { issuer_changed: bool },
    /// Target `sso_provider`.
    #[serde(rename = "sso.provider_removed")]
    ProviderRemoved,
    #[serde(rename = "sso.settings_changed")]
    #[serde(rename_all = "snake_case")]
    SettingsChanged {
        enabled_before: bool,
        enabled: bool,
        enforced_before: bool,
        enforced: bool,
        revoked_users: u64,
    },
    /// Target `sso_domain`, id only.
    #[serde(rename = "sso.domain_added")]
    DomainAdded,
    #[serde(rename = "sso.domain_verified")]
    #[serde(rename_all = "snake_case")]
    DomainVerified { taken_from: Option<uuid::Uuid> },
    /// Written in the losing org's log.
    #[serde(rename = "sso.domain_lost")]
    #[serde(rename_all = "snake_case")]
    DomainLost { to_org_id: uuid::Uuid },
    #[serde(rename = "sso.domain_removed")]
    DomainRemoved,
    /// Actor `system`.
    #[serde(rename = "sso.domain_lapsed")]
    DomainLapsed,
    /// The break-glass command; actor `cli`.
    #[serde(rename = "sso.disabled")]
    Disabled,
}
