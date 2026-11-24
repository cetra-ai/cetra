//! `member.*` actions. The target is the user (`target_kind` `user`).

/// How a member was added.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum JoinVia {
    SsoJit,
    AutoProvision,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(tag = "action", content = "details")]
pub(crate) enum MemberAction {
    #[serde(rename = "member.added")]
    #[serde(rename_all = "snake_case")]
    Added { role: &'static str, via: JoinVia },
    #[serde(rename = "member.role_changed")]
    #[serde(rename_all = "snake_case")]
    RoleChanged {
        old_role: &'static str,
        new_role: &'static str,
    },
    /// The removed member's role. A self-removal shows as actor = target.
    #[serde(rename = "member.removed")]
    #[serde(rename_all = "snake_case")]
    Removed { role: &'static str },
    #[serde(rename = "member.override_set")]
    #[serde(rename_all = "snake_case")]
    OverrideSet {
        project_id: uuid::Uuid,
        old_role: Option<&'static str>,
        new_role: &'static str,
    },
    #[serde(rename = "member.override_removed")]
    #[serde(rename_all = "snake_case")]
    OverrideRemoved {
        project_id: uuid::Uuid,
        old_role: Option<&'static str>,
    },
}
