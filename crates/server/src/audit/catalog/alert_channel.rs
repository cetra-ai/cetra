//! `alert_channel.*` actions. The target is the channel (`target_kind` `alert_channel`).
//! Never a URL or a recipient.

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(tag = "action", content = "details")]
pub(crate) enum AlertChannelAction {
    #[serde(rename = "alert_channel.created")]
    #[serde(rename_all = "snake_case")]
    Created { kind: &'static str },
    #[serde(rename = "alert_channel.updated")]
    #[serde(rename_all = "snake_case")]
    Updated { fields: Vec<&'static str> },
    #[serde(rename = "alert_channel.deleted")]
    Deleted,
}
