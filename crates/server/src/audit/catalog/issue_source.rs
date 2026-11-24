//! `issue_source.*` actions. The target is the issue source (`target_kind` `issue_source`).

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(tag = "action", content = "details")]
pub(crate) enum IssueSourceAction {
    #[serde(rename = "issue_source.created")]
    #[serde(rename_all = "snake_case")]
    Created { kind: &'static str },
    #[serde(rename = "issue_source.updated")]
    #[serde(rename_all = "snake_case")]
    Updated { fields: Vec<&'static str> },
    #[serde(rename = "issue_source.deleted")]
    Deleted,
}
