//! `user.*` actions. Instance records; the target is the user.

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(tag = "action", content = "details")]
pub(crate) enum UserAction {
    /// The instance-admin API.
    #[serde(rename = "user.created")]
    #[serde(rename_all = "snake_case")]
    Created { instance_admin: bool },
    /// The requested changes.
    #[serde(rename = "user.updated")]
    #[serde(rename_all = "snake_case")]
    Updated {
        disabled: Option<bool>,
        instance_admin: Option<bool>,
    },
    #[serde(rename = "user.reset_link_issued")]
    ResetLinkIssued,
    /// Control plane.
    #[serde(rename = "user.sessions_revoked")]
    SessionsRevoked,
    /// Control plane.
    #[serde(rename = "user.disabled")]
    Disabled,
    /// Control plane.
    #[serde(rename = "user.deleted")]
    #[serde(rename_all = "snake_case")]
    Deleted { sole_owner_orgs: u32 },
}
