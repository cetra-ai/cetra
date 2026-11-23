//! `scim.*` actions: SCIM provisioning (track EA M5). Written in the org's log. Names and
//! emails are never recorded: the details hold ids, role names and attribute names only.

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(tag = "action", content = "details")]
pub(crate) enum ScimAction {
    #[serde(rename = "scim.enabled")]
    Enabled,
    #[serde(rename = "scim.disabled")]
    Disabled,
    #[serde(rename = "scim.default_role_changed")]
    #[serde(rename_all = "snake_case")]
    DefaultRoleChanged {
        old_role: &'static str,
        new_role: &'static str,
    },
    /// Target `scim_token`.
    #[serde(rename = "scim.token_created")]
    TokenCreated,
    /// Target `scim_token`.
    #[serde(rename = "scim.token_revoked")]
    TokenRevoked,
    /// Target `user`. `linked` is true when the account already existed.
    #[serde(rename = "scim.user_provisioned")]
    #[serde(rename_all = "snake_case")]
    UserProvisioned {
        scim_user_id: uuid::Uuid,
        role: &'static str,
        linked: bool,
    },
    /// Attribute names only.
    #[serde(rename = "scim.user_updated")]
    #[serde(rename_all = "snake_case")]
    UserUpdated {
        scim_user_id: uuid::Uuid,
        fields: Vec<&'static str>,
    },
    #[serde(rename = "scim.user_deactivated")]
    #[serde(rename_all = "snake_case")]
    UserDeactivated {
        scim_user_id: uuid::Uuid,
        account_disabled: bool,
    },
    #[serde(rename = "scim.user_reactivated")]
    #[serde(rename_all = "snake_case")]
    UserReactivated { scim_user_id: uuid::Uuid },
    #[serde(rename = "scim.user_deleted")]
    #[serde(rename_all = "snake_case")]
    UserDeleted { scim_user_id: uuid::Uuid },
}
