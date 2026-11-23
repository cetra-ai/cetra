//! `deletion.*` actions (target `deletion`, org records). `kind` comes from
//! `crate::meta::deletions::DeletionKind::as_str()`. Reasons are never recorded.

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(tag = "action", content = "details")]
pub(crate) enum DeletionAction {
    #[serde(rename = "deletion.requested")]
    #[serde(rename_all = "snake_case")]
    Requested {
        kind: &'static str,
        follow_up_of: Option<uuid::Uuid>,
    },
    /// Trace, filter and user deletions. Drops record `project.dropped` or `org.dropped` instead.
    #[serde(rename = "deletion.applied")]
    #[serde(rename_all = "snake_case")]
    Applied { kind: &'static str },
    /// Never the reason text.
    #[serde(rename = "deletion.failed")]
    #[serde(rename_all = "snake_case")]
    Failed { kind: &'static str },
}
