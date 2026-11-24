//! `dataset.*` actions. The target is the dataset (`target_kind` `dataset`). Items, imports
//! and version restores are data work and are not recorded.

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(tag = "action", content = "details")]
pub(crate) enum DatasetAction {
    #[serde(rename = "dataset.created")]
    Created,
    #[serde(rename = "dataset.updated")]
    #[serde(rename_all = "snake_case")]
    Updated { fields: Vec<&'static str> },
    #[serde(rename = "dataset.deleted")]
    Deleted,
    #[serde(rename = "dataset.export_started")]
    #[serde(rename_all = "snake_case")]
    ExportStarted { version: u32, format: &'static str },
}
