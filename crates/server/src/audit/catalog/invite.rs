//! `invite.*` actions. The target is the invite. The email is never recorded.

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(tag = "action", content = "details")]
pub(crate) enum InviteAction {
    #[serde(rename = "invite.created")]
    #[serde(rename_all = "snake_case")]
    Created { role: &'static str },
    #[serde(rename = "invite.revoked")]
    Revoked,
    #[serde(rename = "invite.accepted")]
    #[serde(rename_all = "snake_case")]
    Accepted { role: &'static str },
}
