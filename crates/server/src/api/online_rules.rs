//! Online rule API types (spec 09 § Evaluation & alerts, D-I-15, D-I-30).

use std::borrow::Cow;
use std::collections::BTreeMap;

use cetra_core::enums::ScoreDataType;
use serde::{Deserialize, Deserializer, Serialize};
use utoipa::openapi::schema::{ArrayBuilder, ObjectBuilder, Schema, SchemaType, Type};
use utoipa::openapi::{Ref, RefOr};
use utoipa::{PartialSchema, ToSchema};
use uuid::Uuid;

use super::common::TimeParam;
use super::evaluators::{CategoryCount, StatsWindow};
use super::metrics::MetricPoint;

/// The period of a rule budget, in UTC.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum BudgetPeriod {
    /// The budget applies to each UTC day.
    Day,
    /// The budget applies to each UTC calendar month.
    Month,
}

impl BudgetPeriod {
    /// The stored budget period.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Day => "day",
            Self::Month => "month",
        }
    }

    /// Parses the stored budget period. Any other text is `None`.
    pub fn from_name(text: &str) -> Option<Self> {
        match text {
            "day" => Some(Self::Day),
            "month" => Some(Self::Month),
            _ => None,
        }
    }
}

/// What an online rule matches: a trace, or a span of a trace (D-I-9).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum RuleTarget {
    /// Each trace that matches the filter.
    Trace,
    /// Each span that matches the filter.
    Span,
}

impl RuleTarget {
    /// The stored rule target.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Trace => "trace",
            Self::Span => "span",
        }
    }

    /// Parses the stored rule target. Any other text is `None`.
    pub fn from_name(text: &str) -> Option<Self> {
        match text {
            "trace" => Some(Self::Trace),
            "span" => Some(Self::Span),
            _ => None,
        }
    }
}

/// Why a rule is paused (D-I-14, D-I-15).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum PauseReason {
    /// The rule's budget for the period is used up.
    Budget,
    /// The breaker tripped, or the rule's webhook failed.
    Errors,
    /// The rule's connection failed.
    Connection,
}

impl PauseReason {
    /// The stored pause reason.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Budget => "budget",
            Self::Errors => "errors",
            Self::Connection => "connection",
        }
    }

    /// Parses the stored pause reason. Any other text is `None`.
    pub fn from_name(text: &str) -> Option<Self> {
        match text {
            "budget" => Some(Self::Budget),
            "errors" => Some(Self::Errors),
            "connection" => Some(Self::Connection),
            _ => None,
        }
    }
}

/// Spend of a rule budget in the current period.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct RuleSpend {
    /// Start of the current UTC day or month.
    #[serde(with = "cetra_core::time_serde")]
    #[schema(value_type = String, format = DateTime)]
    pub period_start: i64,
    /// Cost of the finished jobs in the period, in USD.
    pub spent_usd: f64,
    /// Estimated cost of the running jobs, in USD.
    pub reserved_usd: f64,
}

/// Job counts of a rule, by status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct JobCounts {
    /// Jobs waiting to run.
    pub queued: u64,
    /// Jobs running now.
    pub running: u64,
    /// Finished jobs with a score.
    pub done: u64,
    /// Jobs that failed for good.
    pub failed: u64,
    /// Jobs left out before they ran.
    pub skipped: u64,
}

/// An online rule. Spec 09 § Evaluation & alerts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct OnlineRule {
    /// Rule id.
    pub id: Uuid,
    /// Display name, 1 to 128 characters.
    pub name: String,
    /// The evaluator the rule runs. Immutable.
    pub evaluator_id: Uuid,
    /// What the rule matches. Immutable.
    pub target: RuleTarget,
    /// Filter over the target's view. Omitted when the rule has none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter: Option<String>,
    /// Prompt variable to mapping expression.
    pub variable_mapping: BTreeMap<String, String>,
    /// Share of the matching targets that are evaluated, in `(0, 1]`.
    pub sample_rate: f64,
    /// Budget per period, in USD. Omitted when the rule has none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_usd: Option<f64>,
    /// Period of the budget. Present exactly when `budget_usd` is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_period: Option<BudgetPeriod>,
    /// Whether the rule runs.
    pub enabled: bool,
    /// Why the rule is paused. Omitted when it is not paused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paused_reason: Option<PauseReason>,
    /// When the rule was paused. Omitted when it is not paused.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::api::keys::optional_time"
    )]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub paused_at: Option<i64>,
    /// Detail of the pause: an error text or a fixed text. Never payload content.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paused_detail: Option<String>,
    /// Budget spend of the current period. Omitted when the rule has no budget.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spend: Option<RuleSpend>,
    /// Jobs of the rule created in the last 24 hours, by status.
    pub jobs_last_24h: JobCounts,
    /// Live matches dropped by the hourly cap in the current UTC hour. Counted in memory, so 0 after a restart.
    pub dropped_last_hour: u64,
    /// User who created the rule. Omitted when that user is unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_by: Option<Uuid>,
    /// Creation time. JSON is RFC 3339.
    #[serde(with = "cetra_core::time_serde")]
    #[schema(value_type = String, format = DateTime)]
    pub created_at: i64,
    /// Last change time. JSON is RFC 3339.
    #[serde(with = "cetra_core::time_serde")]
    #[schema(value_type = String, format = DateTime)]
    pub updated_at: i64,
}

/// `GET /api/v1/projects/{project}/online-rules` answer. It carries the org's enabled-rule count (D-I-32).
///
/// Its schema is written by hand, like `Page<T>`'s, so that `org_enabled_rules` is required.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OnlineRulePage {
    /// Rules in this page.
    pub data: Vec<OnlineRule>,
    /// Opaque cursor for the next page, or null when this is the last page.
    pub next_cursor: Option<String>,
    /// Enabled rules of the whole org, counted for the plan's active-rule limit.
    pub org_enabled_rules: u64,
}

impl PartialSchema for OnlineRulePage {
    fn schema() -> RefOr<Schema> {
        ObjectBuilder::new()
            .description(Some(
                "One page of online rules. `next_cursor` is null when there is no next page.",
            ))
            .property(
                "data",
                ArrayBuilder::new()
                    .description(Some("Rules in this page."))
                    .items(Ref::from_schema_name(OnlineRule::name())),
            )
            .required("data")
            .property(
                "next_cursor",
                ObjectBuilder::new()
                    .schema_type(SchemaType::from_iter([Type::String, Type::Null]))
                    .description(Some(
                        "Opaque cursor for the next page, or null when this is the last page.",
                    )),
            )
            .required("next_cursor")
            .property(
                "org_enabled_rules",
                ObjectBuilder::new()
                    .schema_type(Type::Integer)
                    .minimum(Some(0))
                    .description(Some(
                        "Enabled rules of the whole org, counted for the plan's active-rule limit.",
                    )),
            )
            .required("org_enabled_rules")
            .into()
    }
}

impl ToSchema for OnlineRulePage {
    fn name() -> Cow<'static, str> {
        Cow::Borrowed("OnlineRulePage")
    }

    fn schemas(schemas: &mut Vec<(String, RefOr<Schema>)>) {
        schemas.push((OnlineRule::name().into_owned(), OnlineRule::schema()));
        OnlineRule::schemas(schemas);
    }
}

/// `POST /api/v1/projects/{project}/online-rules` body. Absent optional fields take their defaults.
///
/// Optional fields refuse `null` (spec 09 § Conventions). Its `Debug` is derived: a rule holds
/// configuration only, no payload or secret.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateOnlineRuleRequest {
    /// Display name, 1 to 128 characters after trimming.
    pub name: String,
    /// The evaluator the rule runs.
    pub evaluator_id: Uuid,
    /// What the rule matches. Immutable once created.
    pub target: RuleTarget,
    /// Filter over the target's view. Absent means no filter.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub filter: Option<String>,
    /// Prompt variable to mapping expression, 1 to 32 variables.
    pub variable_mapping: BTreeMap<String, String>,
    /// Share of the matching targets to evaluate, in `(0, 1]`. Default 1.
    #[serde(default = "default_sample_rate")]
    pub sample_rate: f64,
    /// Budget per period, in USD. Judge evaluators only.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub budget_usd: Option<f64>,
    /// Period of the budget. Required with `budget_usd`.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub budget_period: Option<BudgetPeriod>,
    /// Whether the rule runs. Default true.
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

/// `PATCH /api/v1/projects/{project}/online-rules/{id}` body. Absent means unchanged.
///
/// There is no `evaluator_id` or `target`: sending either is a 400 (unknown field). Optional
/// fields refuse `null`; removing the budget is `clear_budget: true`.
#[derive(Debug, Clone, PartialEq, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdateOnlineRuleRequest {
    /// Display name, 1 to 128 characters after trimming.
    #[serde(default, deserialize_with = "present")]
    pub name: Option<String>,
    /// Filter over the target's view. `""` clears it.
    #[serde(default, deserialize_with = "present")]
    pub filter: Option<String>,
    /// Replaces the whole mapping, 1 to 32 variables.
    #[serde(default, deserialize_with = "present")]
    pub variable_mapping: Option<BTreeMap<String, String>>,
    /// Share of the matching targets to evaluate, in `(0, 1]`.
    #[serde(default, deserialize_with = "present")]
    pub sample_rate: Option<f64>,
    /// Budget per period, in USD. Judge evaluators only.
    #[serde(default, deserialize_with = "present")]
    pub budget_usd: Option<f64>,
    /// Period of the budget. Needed with `budget_usd` when the rule has none yet.
    #[serde(default, deserialize_with = "present")]
    pub budget_period: Option<BudgetPeriod>,
    /// Removes the budget. Cannot come with `budget_usd` or `budget_period`.
    #[serde(default)]
    pub clear_budget: bool,
    /// Turns the rule on or off.
    #[serde(default, deserialize_with = "present")]
    pub enabled: Option<bool>,
}

/// `POST /api/v1/projects/{project}/online-rules/preview` body (D-I-9). Absent fields take their
/// defaults.
///
/// Deserialize only, because `TimeParam` has no `Serialize`. Its `Debug` is derived: the filter
/// is a query over stored data, and nothing logs it.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct OnlineRulePreviewRequest {
    /// The rule's target: the view the filter is read on.
    pub target: RuleTarget,
    /// Filter DSL over the target's view. Absent or blank matches everything.
    #[serde(default)]
    pub filter: Option<String>,
    /// Window start. Default: `to` minus 24 hours.
    #[serde(default)]
    pub from: Option<TimeParam>,
    /// Window end. Default: now.
    #[serde(default)]
    pub to: Option<TimeParam>,
}

/// The preview's count (D-I-9).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct OnlineRulePreview {
    /// Matching traces or spans, at most 10 000.
    pub matched: u64,
    /// More than 10 000 matched.
    pub capped: bool,
    /// The window that was counted. RFC 3339.
    #[serde(with = "cetra_core::time_serde")]
    #[schema(value_type = String, format = DateTime)]
    pub from: i64,
    /// The end of the window that was counted. RFC 3339.
    #[serde(with = "cetra_core::time_serde")]
    #[schema(value_type = String, format = DateTime)]
    pub to: i64,
}

/// `GET /api/v1/projects/{project}/online-rules/stats` response (D-I-50).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct OnlineRuleStatsResponse {
    /// The 24 hours the figures cover.
    pub window: StatsWindow,
    /// One entry per online rule, ordered like the rule list (by name, then id). Rules without
    /// jobs or scores are included with zeros.
    pub data: Vec<OnlineRuleStats>,
}

/// The stats of one online rule (D-I-50).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct OnlineRuleStats {
    /// The rule.
    pub rule_id: Uuid,
    /// Its finished jobs in the window, per hour, 24 points, oldest first.
    pub jobs_hourly: Vec<RuleJobsHour>,
    /// Its scores in the window.
    pub scores: RuleScoreStats,
}

/// The finished jobs of one online rule in one hour.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct RuleJobsHour {
    /// Start of the hour. JSON is RFC 3339.
    #[serde(with = "cetra_core::time_serde")]
    #[schema(value_type = String, format = DateTime)]
    pub time: i64,
    /// Done jobs.
    pub done: u64,
    /// Failed jobs.
    pub failed: u64,
    /// Skipped jobs.
    pub skipped: u64,
}

/// The scores of one online rule in the window. Read with the rule's evaluator's current output
/// type (D-I-48).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct RuleScoreStats {
    /// The output type of the rule's evaluator.
    pub data_type: ScoreDataType,
    /// Live scores in the window, of any type.
    pub count: u64,
    /// Mean of the numeric values. Numeric outputs only. Omitted when there is no numeric value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub average: Option<f64>,
    /// Share of `true` values, 0 to 1. Boolean outputs only. Omitted when there is no value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pass_rate: Option<f64>,
    /// The most frequent label. Categorical outputs only. Omitted when there is none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_category: Option<CategoryCount>,
    /// Per hour: the average (numeric) or pass rate (boolean), 24 points, oldest first. An hour
    /// without a value has no `value`.
    pub hourly: Vec<MetricPoint>,
}

/// Deserializes a field that is present, and refuses `null` through `T`'s own deserializer.
pub(super) fn present<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}

fn default_sample_rate() -> f64 {
    1.0
}

fn default_enabled() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rule_round_trips_the_fixture() {
        let text = include_str!("../../../../fixtures/api/static/online-rule.json");
        let rule: OnlineRule = serde_json::from_str(text).unwrap();
        let back: serde_json::Value = serde_json::to_value(&rule).unwrap();
        let fixture: serde_json::Value = serde_json::from_str(text).unwrap();
        assert_eq!(back, fixture);
    }

    #[test]
    fn paused_rule_omits_its_absent_options() {
        let text = include_str!("../../../../fixtures/api/static/online-rules.json");
        let page: OnlineRulePage = serde_json::from_str(text).unwrap();
        let paused = page
            .data
            .iter()
            .find(|rule| rule.paused_reason == Some(PauseReason::Errors))
            .unwrap();
        assert!(paused.budget_usd.is_none() && paused.spend.is_none());
        let value = serde_json::to_value(paused).unwrap();
        assert!(value.get("budget_usd").is_none());
        assert!(value.get("budget_period").is_none());
        assert!(value.get("spend").is_none());
        assert!(value.get("paused_reason").is_some());
    }

    #[test]
    fn update_request_refuses_immutable_and_null_fields() {
        let target = r#"{"target":"span"}"#;
        let evaluator = r#"{"evaluator_id":"01900000-0000-7000-8000-000000000500"}"#;
        let null_name = r#"{"name":null}"#;
        let null_clear = r#"{"clear_budget":null}"#;
        assert!(serde_json::from_str::<UpdateOnlineRuleRequest>(target).is_err());
        assert!(serde_json::from_str::<UpdateOnlineRuleRequest>(evaluator).is_err());
        assert!(serde_json::from_str::<UpdateOnlineRuleRequest>(null_name).is_err());
        assert!(serde_json::from_str::<UpdateOnlineRuleRequest>(null_clear).is_err());
        let empty: UpdateOnlineRuleRequest = serde_json::from_str("{}").unwrap();
        assert_eq!(empty.name, None);
        assert!(!empty.clear_budget);
    }

    #[test]
    fn create_request_defaults_the_sample_rate_and_enabled() {
        let text = r#"{"name":"r","evaluator_id":"01900000-0000-7000-8000-000000000500","target":"trace","variable_mapping":{"input":"trace.input"}}"#;
        let request: CreateOnlineRuleRequest = serde_json::from_str(text).unwrap();
        assert!((request.sample_rate - 1.0).abs() < f64::EPSILON);
        assert!(request.enabled);
        assert!(request.filter.is_none());
    }
}
