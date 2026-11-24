//! `sso.*` actions: org SSO. Written in the org's log. Domain text is never recorded.

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(tag = "action", content = "details")]
pub(crate) enum SsoAction {
    /// Target `sso_provider`. `protocol` is `oidc` or `saml`. `issuer_changed` marks a save
    /// that changed the asserted identity (the issuer or the IdP entity ID, or a protocol
    /// switch), which deleted the org's identities. The SAML metadata refresh writes it with
    /// actor `system` when the IdP's certificates rotate.
    #[serde(rename = "sso.provider_saved")]
    #[serde(rename_all = "snake_case")]
    ProviderSaved {
        protocol: &'static str,
        issuer_changed: bool,
    },
    /// Target `sso_provider`. `protocol` is `oidc` or `saml`.
    #[serde(rename = "sso.provider_removed")]
    #[serde(rename_all = "snake_case")]
    ProviderRemoved { protocol: &'static str },
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
