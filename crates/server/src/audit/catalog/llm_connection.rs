//! `llm_connection.*` actions. The target is the connection (`target_kind` `llm_connection`).

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(tag = "action", content = "details")]
pub(crate) enum LlmConnectionAction {
    #[serde(rename = "llm_connection.created")]
    #[serde(rename_all = "snake_case")]
    Created { adapter: &'static str },
    #[serde(rename = "llm_connection.updated")]
    #[serde(rename_all = "snake_case")]
    Updated {
        fields: Vec<&'static str>,
        key_changed: bool,
    },
    #[serde(rename = "llm_connection.deleted")]
    Deleted,
}
