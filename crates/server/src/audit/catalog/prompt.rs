//! `prompt.*` actions. The target is the prompt (`target_kind` `prompt`). Label names are
//! configuration identifiers (`production`), not payload (D-EA-21).

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(tag = "action", content = "details")]
pub(crate) enum PromptAction {
    #[serde(rename = "prompt.created")]
    Created,
    #[serde(rename = "prompt.updated")]
    #[serde(rename_all = "snake_case")]
    Updated { fields: Vec<&'static str> },
    #[serde(rename = "prompt.deleted")]
    Deleted,
    #[serde(rename = "prompt.version_created")]
    #[serde(rename_all = "snake_case")]
    VersionCreated { version: u32 },
    #[serde(rename = "prompt.label_set")]
    #[serde(rename_all = "snake_case")]
    LabelSet { label: String, version: u32 },
    #[serde(rename = "prompt.label_removed")]
    #[serde(rename_all = "snake_case")]
    LabelRemoved { label: String },
    #[serde(rename = "prompt.protected_labels_changed")]
    #[serde(rename_all = "snake_case")]
    ProtectedLabelsChanged { count: u32 },
}
