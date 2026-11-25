//! `evaluator.*` actions. The target is the evaluator (`target_kind` `evaluator`).

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(tag = "action", content = "details")]
pub(crate) enum EvaluatorAction {
    /// `url_host` is set for webhooks only: the host, never the URL.
    #[serde(rename = "evaluator.created")]
    #[serde(rename_all = "snake_case")]
    Created {
        kind: &'static str,
        url_host: Option<String>,
    },
    #[serde(rename = "evaluator.updated")]
    #[serde(rename_all = "snake_case")]
    Updated {
        fields: Vec<&'static str>,
        url_host: Option<String>,
    },
    #[serde(rename = "evaluator.deleted")]
    Deleted,
}
