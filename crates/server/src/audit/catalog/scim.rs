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
    /// Target `scim_group`. Ids and counts only, never the display name.
    #[serde(rename = "scim.group_created")]
    #[serde(rename_all = "snake_case")]
    GroupCreated {
        scim_group_id: uuid::Uuid,
        members: u32,
    },
    /// Target `scim_group`. `fields` names the attributes changed: `displayName`, `externalId`.
    #[serde(rename = "scim.group_updated")]
    #[serde(rename_all = "snake_case")]
    GroupUpdated {
        scim_group_id: uuid::Uuid,
        fields: Vec<&'static str>,
    },
    /// Target `scim_group`.
    #[serde(rename = "scim.group_deleted")]
    #[serde(rename_all = "snake_case")]
    GroupDeleted { scim_group_id: uuid::Uuid },
    /// Target `scim_group`. One record per request, with the counts (a bulk change is one record).
    #[serde(rename = "scim.group_members_changed")]
    #[serde(rename_all = "snake_case")]
    GroupMembersChanged {
        scim_group_id: uuid::Uuid,
        added: u32,
        removed: u32,
    },
    /// Target `scim_group`. `org_role` is `None` when the group maps no org role.
    #[serde(rename = "scim.group_mapping_changed")]
    #[serde(rename_all = "snake_case")]
    GroupMappingChanged {
        scim_group_id: uuid::Uuid,
        org_role: Option<&'static str>,
        projects: u32,
    },
    /// No target. `users` is the managed members checked, `changed` the ones whose roles changed.
    #[serde(rename = "scim.roles_recomputed")]
    #[serde(rename_all = "snake_case")]
    RolesRecomputed { users: u32, changed: u32 },
    /// No target. `count` is every refused user; `scim_user_ids` holds at most 20 of them.
    #[serde(rename = "scim.role_raise_refused")]
    #[serde(rename_all = "snake_case")]
    RoleRaiseRefused {
        count: u32,
        scim_user_ids: Vec<uuid::Uuid>,
    },
}
