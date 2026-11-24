//! `project.*` actions.

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(tag = "action", content = "details")]
pub(crate) enum ProjectAction {
    #[serde(rename = "project.created")]
    Created,
    /// Names are never recorded.
    #[serde(rename = "project.renamed")]
    Renamed,
    #[serde(rename = "project.retention_changed")]
    #[serde(rename_all = "snake_case")]
    RetentionChanged {
        old_days: Option<u32>,
        new_days: Option<u32>,
    },
    #[serde(rename = "project.completion_timeout_changed")]
    #[serde(rename_all = "snake_case")]
    CompletionTimeoutChanged {
        old_seconds: Option<u32>,
        new_seconds: Option<u32>,
    },
    #[serde(rename = "project.dropped")]
    Dropped,
}
