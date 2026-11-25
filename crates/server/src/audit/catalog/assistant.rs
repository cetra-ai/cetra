//! `assistant.*` actions: the assistant setting. Never the model name.

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(tag = "action", content = "details")]
pub(crate) enum AssistantAction {
    #[serde(rename = "assistant.setting_changed")]
    #[serde(rename_all = "snake_case")]
    SettingChanged {
        enabled: bool,
        connection_id: Option<uuid::Uuid>,
    },
}
