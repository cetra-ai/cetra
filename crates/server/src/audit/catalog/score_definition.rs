//! `score_definition.*` actions. The target is the score definition (`target_kind` `score_definition`).

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(tag = "action", content = "details")]
pub(crate) enum ScoreDefinitionAction {
    #[serde(rename = "score_definition.created")]
    #[serde(rename_all = "snake_case")]
    Created { data_type: &'static str },
    #[serde(rename = "score_definition.updated")]
    #[serde(rename_all = "snake_case")]
    Updated { fields: Vec<&'static str> },
    #[serde(rename = "score_definition.archived")]
    Archived,
}
