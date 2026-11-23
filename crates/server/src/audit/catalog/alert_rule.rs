//! `alert_rule.*` actions. The target is the alert rule (`target_kind` `alert_rule`).

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(tag = "action", content = "details")]
pub(crate) enum AlertRuleAction {
    #[serde(rename = "alert_rule.created")]
    #[serde(rename_all = "snake_case")]
    Created { metric: &'static str, enabled: bool },
    #[serde(rename = "alert_rule.updated")]
    #[serde(rename_all = "snake_case")]
    Updated { fields: Vec<&'static str> },
    #[serde(rename = "alert_rule.deleted")]
    Deleted,
}
