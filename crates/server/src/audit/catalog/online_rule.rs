//! `online_rule.*` actions. The target is the rule (`target_kind` `online_rule`).

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(tag = "action", content = "details")]
pub(crate) enum OnlineRuleAction {
    #[serde(rename = "online_rule.created")]
    #[serde(rename_all = "snake_case")]
    Created { evaluator_id: uuid::Uuid },
    #[serde(rename = "online_rule.updated")]
    #[serde(rename_all = "snake_case")]
    Updated {
        fields: Vec<&'static str>,
        budget_resumed: bool,
    },
    #[serde(rename = "online_rule.deleted")]
    Deleted,
    /// `reason` is the `PAUSE_*` constant the code stores.
    #[serde(rename = "online_rule.paused")]
    #[serde(rename_all = "snake_case")]
    Paused { reason: &'static str },
    /// `reason` is `manual` for the resume route and `budget period` for the budget worker.
    #[serde(rename = "online_rule.resumed")]
    #[serde(rename_all = "snake_case")]
    Resumed { reason: &'static str },
    #[serde(rename = "online_rule.backfill_started")]
    #[serde(rename_all = "snake_case")]
    BackfillStarted {
        backfill_id: uuid::Uuid,
        max_targets: u32,
    },
}
