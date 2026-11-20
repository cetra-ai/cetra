//! Issue source API types (spec 09 § Issues, D-IC-1, D-IC-3, D-IC-13).

use std::borrow::Cow;

use serde::{Deserialize, Serialize};
use utoipa::openapi::schema::{ArrayBuilder, ObjectBuilder, Schema, SchemaType, Type};
use utoipa::openapi::{Ref, RefOr};
use utoipa::{IntoParams, PartialSchema, ToSchema};
use uuid::Uuid;

use super::common::{Limit, Page, TimeParam};
use super::online_rules::{BudgetPeriod, present};

/// What an issue source reads. M3 adds kinds (D-IC-1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum IssueSourceKind {
    /// Error spans of completed traces, fingerprinted into issues (D-IC-2).
    Error,
    /// Bad scores of the source's score names, summarized and clustered into issues (D-IC-1).
    Score,
    /// Feedback scores of the source's feedback sources (D-IC-1).
    Feedback,
    /// New production traces, summarized and clustered into issues (D-IC-1).
    Topic,
}

/// An issue source. Spec 09 § Issues.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct IssueSource {
    /// Source id.
    pub id: Uuid,
    /// The project the source belongs to.
    pub project_id: Uuid,
    /// Display name, unique in the project. 1 to 128 characters.
    pub name: String,
    /// What the source reads. Immutable.
    pub kind: IssueSourceKind,
    /// Spans filter for `error` sources. Omitted when there is none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter: Option<String>,
    /// Whether the source runs. A disabled source counts for no plan limit.
    pub enabled: bool,
    /// Channels notified of new and regressed issues. Ordered by id.
    pub channel_ids: Vec<Uuid>,
    /// Why the sweeper skips the source. Omitted when it runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paused_reason: Option<String>,
    /// User who created the source. Omitted when that user is unknown.
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
    /// Issues of this source that are open now.
    pub open_issues: u64,
    /// Share of the matching traces or scores that become candidates, in `(0, 1]`. LLM kinds only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sample_rate: Option<f64>,
    /// Most candidates admitted per UTC day, 1 to 10 000. LLM kinds only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daily_cap: Option<u32>,
    /// Score names read by a `score` or `feedback` source. Omitted for other kinds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub score_names: Option<Vec<String>>,
    /// Feedback sources read by a `feedback` source: `end_user` and/or `human`. Omitted for other
    /// kinds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub feedback_sources: Option<Vec<String>>,
    /// Connection of the chat model that summarizes and names. LLM kinds only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat_connection_id: Option<Uuid>,
    /// Chat model of that connection. LLM kinds only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat_model: Option<String>,
    /// Connection of the embedding model. Fixed after create. LLM kinds only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embed_connection_id: Option<Uuid>,
    /// Embedding model of that connection. Fixed after create. LLM kinds only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embed_model: Option<String>,
    /// Vector dimension of the embedding model, set once the source first embeds. LLM kinds only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embed_dimension: Option<u32>,
    /// Cosine similarity at which a candidate joins an issue, 0.5 to 0.99. LLM kinds only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub floor: Option<f64>,
    /// Smallest group a refit makes into a new issue, 2 to 100. LLM kinds only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_issue_size: Option<u32>,
    /// Spend limit per `budget_period`, in USD. Omitted when there is no budget.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_usd: Option<f64>,
    /// Period of the budget. Present exactly when `budget_usd` is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_period: Option<BudgetPeriod>,
    /// The provider's error text for a connection pause. Omitted when the source is not paused
    /// for a connection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paused_detail: Option<String>,
    /// When the next refit is due. Omitted for other kinds. JSON is RFC 3339.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::api::keys::optional_time"
    )]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub next_refit_at: Option<i64>,
    /// Candidates created in the last 24 hours, by status. LLM kinds only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidates_24h: Option<IssueCandidateCounts>,
    /// Spend of the current UTC day, in USD. LLM kinds only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spent_today_usd: Option<f64>,
}

/// Candidates of an LLM source created in the last 24 hours, by status (D-IC-12).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct IssueCandidateCounts {
    /// Waiting for a worker, or for their retry time.
    pub queued: u64,
    /// Embedded, waiting for the next refit.
    pub pending: u64,
    /// Joined an issue.
    pub assigned: u64,
    /// Not kept: the trace is gone, or the daily cap was reached.
    pub dropped: u64,
    /// Failed after its retries.
    pub failed: u64,
    /// Skipped: the source was deleted, paused too long, or the trace was deleted.
    pub skipped: u64,
}

/// One refit of an LLM source (D-IC-8).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct IssueRefit {
    /// Refit id.
    pub id: Uuid,
    /// `schedule` (every 6 hours) or `manual` ("Refit now").
    pub trigger: String,
    /// When the refit started. JSON is RFC 3339.
    #[serde(with = "cetra_core::time_serde")]
    #[schema(value_type = String, format = DateTime)]
    pub started_at: i64,
    /// When the refit finished. Omitted while it runs. JSON is RFC 3339.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::api::keys::optional_time"
    )]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub finished_at: Option<i64>,
    /// Pending candidates the refit read.
    pub pending: u32,
    /// Clusters found among them.
    pub clusters: u32,
    /// New issues created.
    pub created: u32,
    /// Candidates that joined an existing issue.
    pub joined: u32,
    /// Candidates left as outliers.
    pub outliers: u32,
    /// USD spent on the naming calls.
    pub cost_usd: f64,
    /// Why the refit stopped early, such as `busy`, `budget` or `interrupted`. Omitted when it
    /// finished.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_class: Option<String>,
}

/// `GET …/issue-sources/{id}/refits` answer: the newest 50 refits, newest first.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct IssueRefitPage {
    /// The refits, newest first.
    pub data: Vec<IssueRefit>,
}

/// `GET /api/v1/projects/{project}/issue-sources` answer. It carries the org's enabled-source
/// count (D-IC-13).
///
/// Its schema is written by hand, like `OnlineRulePage`'s, so that `org_active_sources` is required.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IssueSourcePage {
    /// Sources in this page.
    pub data: Vec<IssueSource>,
    /// Opaque cursor for the next page, or null when this is the last page.
    pub next_cursor: Option<String>,
    /// Enabled sources of the whole org, counted for the plan's active-source limit.
    pub org_active_sources: u64,
}

impl PartialSchema for IssueSourcePage {
    fn schema() -> RefOr<Schema> {
        ObjectBuilder::new()
            .description(Some(
                "One page of issue sources. `next_cursor` is null when there is no next page.",
            ))
            .property(
                "data",
                ArrayBuilder::new()
                    .description(Some("Sources in this page."))
                    .items(Ref::from_schema_name(IssueSource::name())),
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
                "org_active_sources",
                ObjectBuilder::new()
                    .schema_type(Type::Integer)
                    .minimum(Some(0))
                    .description(Some(
                        "Enabled sources of the whole org, counted for the plan's active-source limit.",
                    )),
            )
            .required("org_active_sources")
            .into()
    }
}

impl ToSchema for IssueSourcePage {
    fn name() -> Cow<'static, str> {
        Cow::Borrowed("IssueSourcePage")
    }

    fn schemas(schemas: &mut Vec<(String, RefOr<Schema>)>) {
        schemas.push((IssueSource::name().into_owned(), IssueSource::schema()));
        IssueSource::schemas(schemas);
    }
}

/// `POST /api/v1/projects/{project}/issue-sources` body. Absent optional fields take their defaults.
///
/// Optional fields refuse `null` (spec 09 § Conventions).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateIssueSourceRequest {
    /// Display name, 1 to 128 characters after trimming.
    pub name: String,
    /// What the source reads.
    pub kind: IssueSourceKind,
    /// Spans filter. Absent means no filter.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub filter: Option<String>,
    /// Whether the source runs. Default true.
    #[serde(default = "yes")]
    pub enabled: bool,
    /// Channels notified of new and regressed issues, at most 10 channels of this project.
    /// Default none.
    #[serde(default)]
    pub channel_ids: Vec<Uuid>,
    /// Share of the matching traces or scores that become candidates, in `(0, 1]`. LLM kinds
    /// only. Default 1.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub sample_rate: Option<f64>,
    /// Most candidates admitted per UTC day, 1 to 10 000. LLM kinds only. Default 500.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub daily_cap: Option<u32>,
    /// Score names of a `score` or `feedback` source: 1 to 8 names of 1 to 255 bytes. Required
    /// for those kinds; a `topic` source refuses it.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub score_names: Option<Vec<String>>,
    /// Feedback sources of a `feedback` source: `end_user` and/or `human`. Default both.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub feedback_sources: Option<Vec<String>>,
    /// Connection of the chat model. Required for LLM kinds.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub chat_connection_id: Option<Uuid>,
    /// Chat model of that connection. Required for LLM kinds.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub chat_model: Option<String>,
    /// Connection of the embedding model. Required for LLM kinds. Anthropic cannot embed.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub embed_connection_id: Option<Uuid>,
    /// Embedding model of that connection. Required for LLM kinds. Fixed after create.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub embed_model: Option<String>,
    /// Cosine similarity at which a candidate joins an issue, 0.5 to 0.99. Default 0.80.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub floor: Option<f64>,
    /// Smallest group a refit makes into a new issue, 2 to 100. Default 5.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub min_issue_size: Option<u32>,
    /// Spend limit per `budget_period`, in USD. Needs `budget_period`, and the models must be
    /// priced.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub budget_usd: Option<f64>,
    /// Period of the budget. Needed with `budget_usd`.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub budget_period: Option<BudgetPeriod>,
}

/// `PATCH /api/v1/projects/{project}/issue-sources/{id}` body. Absent means unchanged.
///
/// There is no `kind`: sending it is a 400 (unknown field). Optional fields refuse `null`,
/// except `filter`, where `null` clears it.
#[derive(Debug, Clone, PartialEq, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdateIssueSourceRequest {
    /// Display name, 1 to 128 characters after trimming.
    #[serde(default, deserialize_with = "present")]
    pub name: Option<String>,
    /// Spans filter. `null` or `""` clears it.
    #[serde(default, deserialize_with = "present")]
    #[schema(value_type = Option<String>)]
    pub filter: Option<Option<String>>,
    /// Turns the source on or off.
    #[serde(default, deserialize_with = "present")]
    pub enabled: Option<bool>,
    /// New channels, at most 10 of this project. `[]` removes every channel. Absent keeps them.
    #[serde(default, deserialize_with = "present")]
    pub channel_ids: Option<Vec<Uuid>>,
    /// Share of the matching traces or scores that become candidates, in `(0, 1]`. LLM kinds only.
    #[serde(default, deserialize_with = "present")]
    pub sample_rate: Option<f64>,
    /// Most candidates admitted per UTC day, 1 to 10 000. LLM kinds only.
    #[serde(default, deserialize_with = "present")]
    pub daily_cap: Option<u32>,
    /// Replaces the score names of a `score` or `feedback` source.
    #[serde(default, deserialize_with = "present")]
    pub score_names: Option<Vec<String>>,
    /// Replaces the feedback sources of a `feedback` source.
    #[serde(default, deserialize_with = "present")]
    pub feedback_sources: Option<Vec<String>>,
    /// Connection of the chat model. LLM kinds only.
    #[serde(default, deserialize_with = "present")]
    pub chat_connection_id: Option<Uuid>,
    /// Chat model of that connection. LLM kinds only.
    #[serde(default, deserialize_with = "present")]
    pub chat_model: Option<String>,
    /// Refused: the embedding connection is fixed after create (400).
    #[serde(default, deserialize_with = "present")]
    pub embed_connection_id: Option<Uuid>,
    /// Refused: the embedding model is fixed after create (400).
    #[serde(default, deserialize_with = "present")]
    pub embed_model: Option<String>,
    /// Cosine similarity at which a candidate joins an issue, 0.5 to 0.99. LLM kinds only.
    #[serde(default, deserialize_with = "present")]
    pub floor: Option<f64>,
    /// Smallest group a refit makes into a new issue, 2 to 100. LLM kinds only.
    #[serde(default, deserialize_with = "present")]
    pub min_issue_size: Option<u32>,
    /// Spend limit per `budget_period`, in USD. Give it with `budget_period` when the source has
    /// no budget yet.
    #[serde(default, deserialize_with = "present")]
    pub budget_usd: Option<f64>,
    /// Period of the budget.
    #[serde(default, deserialize_with = "present")]
    pub budget_period: Option<BudgetPeriod>,
    /// Removes the budget. Cannot come with `budget_usd` or `budget_period`.
    #[serde(default)]
    pub clear_budget: bool,
}

/// The state of an issue (D-IC-10). `merged` issues point at their target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum IssueState {
    /// Needs attention.
    Open,
    /// Fixed. A recurrence reopens it as regressed.
    Resolved,
    /// Muted. Its occurrences are still written.
    Ignored,
    /// Folded into `merged_into`. Its occurrences count on the target.
    Merged,
}

/// An issue: one distinct error of an error source (spec 09 § Issues, D-IC-10).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct Issue {
    /// Issue id.
    pub id: Uuid,
    /// The project the issue belongs to.
    pub project_id: Uuid,
    /// The source the issue belongs to.
    pub source_id: Uuid,
    /// Name of that source.
    pub source_name: String,
    /// Kind of that source.
    pub kind: IssueSourceKind,
    /// Name of the occurrence scores of this issue: `issue:<id>`.
    pub score_name: String,
    /// Title, from the error.
    pub title: String,
    /// Description written by a member. Omitted when there is none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Lifecycle state.
    pub state: IssueState,
    /// `true` when a resolved issue recurred and was reopened.
    pub regressed: bool,
    /// The issue this one was merged into. Omitted unless `state` is `merged`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merged_into: Option<Uuid>,
    /// Issues merged into this one. Their occurrences count here. Ordered by occurrence count
    /// over the read's period, most first, then by id.
    pub merged_from: Vec<Uuid>,
    /// Start of the earliest occurrence. JSON is RFC 3339.
    #[serde(with = "cetra_core::time_serde")]
    #[schema(value_type = String, format = DateTime)]
    pub first_seen: i64,
    /// Start of the latest occurrence. JSON is RFC 3339.
    #[serde(with = "cetra_core::time_serde")]
    #[schema(value_type = String, format = DateTime)]
    pub last_seen: i64,
    /// When it was resolved. Omitted unless the issue is resolved.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::api::keys::optional_time"
    )]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub resolved_at: Option<i64>,
    /// Who resolved it. Omitted when nobody did or the user is unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_by: Option<Uuid>,
    /// Creation time. JSON is RFC 3339.
    #[serde(with = "cetra_core::time_serde")]
    #[schema(value_type = String, format = DateTime)]
    pub created_at: i64,
    /// Last change time. JSON is RFC 3339.
    #[serde(with = "cetra_core::time_serde")]
    #[schema(value_type = String, format = DateTime)]
    pub updated_at: i64,
    /// Occurrences in the period: the last 24 hours on `get`, or the `from`/`to` period on
    /// `list`. Counts the issue's own occurrences and those of the issues merged into it.
    pub count: u64,
    /// Distinct users in the same period: the largest user count of the issue's names, so an
    /// approximation when merged issues share users.
    pub users: u64,
}

/// `GET /api/v1/projects/{project}/issues` answer. `next_cursor` is null on the last page.
pub type IssuePage = Page<Issue>;

/// Query parameters of `GET /api/v1/projects/{project}/issues`. Spec 09 § Issues.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, IntoParams)]
#[serde(deny_unknown_fields)]
pub struct IssueListParams {
    /// Comma-separated states: `open`, `resolved`, `ignored`, `merged`, or `all` (every state
    /// but `merged`). Default `open`.
    #[serde(default)]
    pub state: Option<String>,
    /// Issues of this source only.
    #[serde(default)]
    pub source_id: Option<Uuid>,
    /// Keeps issues whose title contains this text, ignoring case.
    #[serde(default)]
    pub q: Option<String>,
    /// `last_seen` (default) or `first_seen`, newest first.
    #[serde(default)]
    pub sort: Option<String>,
    /// Start of the period for `count` and `users`, inclusive. RFC 3339 or epoch milliseconds.
    /// Default 24 hours before `to`.
    #[serde(default)]
    pub from: Option<TimeParam>,
    /// End of the period for `count` and `users`, exclusive. RFC 3339 or epoch milliseconds.
    /// Default now.
    #[serde(default)]
    pub to: Option<TimeParam>,
    /// Comma-separated issue ids, at most 100. Only these issues are listed. Blank means no
    /// filter.
    #[serde(default)]
    pub ids: Option<String>,
    /// Maximum number of items to return (1–1000, default 50).
    #[serde(default)]
    #[param(minimum = 1, maximum = 1000, default = 50)]
    pub limit: Limit,
    /// Opaque cursor from a previous page.
    #[serde(default)]
    pub cursor: Option<String>,
}

/// Query parameters of `GET …/issues/{id}/occurrences`. Spec 09 § Issues.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, IntoParams)]
#[serde(deny_unknown_fields)]
pub struct IssueOccurrenceParams {
    /// Start of the days, inclusive. RFC 3339 or epoch milliseconds. Default 14 days before
    /// `to`; the period is at most 90 days.
    #[serde(default)]
    pub from: Option<TimeParam>,
    /// End of the days, exclusive. RFC 3339 or epoch milliseconds. Default now.
    #[serde(default)]
    pub to: Option<TimeParam>,
}

/// Occurrences of an issue (spec 09 § Issues).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct IssueOccurrences {
    /// One entry per UTC day of the period with at least one occurrence, oldest first. `users`
    /// is the largest user count of the issue's names that day, an approximation when merged
    /// issues share users.
    pub days: Vec<IssueDay>,
    /// The newest occurrences of the issue and of its seven merged issues with the most
    /// occurrences in the period (eight names in all), at most 20. Not limited to the period.
    pub examples: Vec<IssueExample>,
}

/// One UTC day of an issue's occurrences.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct IssueDay {
    /// Start of the UTC day. JSON is RFC 3339.
    #[serde(with = "cetra_core::time_serde")]
    #[schema(value_type = String, format = DateTime)]
    pub day: i64,
    /// Occurrences that day.
    pub count: u64,
    /// Largest user count of the issue's names that day.
    pub users: u64,
}

/// One occurrence of an issue, with the trace it is on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct IssueExample {
    /// Trace id, 32 lowercase hex digits.
    pub trace_id: String,
    /// The error span the occurrence came from, 16 lowercase hex digits. Omitted when unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub span_id: Option<String>,
    /// When the occurrence happened (the trace's start). JSON is RFC 3339.
    #[serde(with = "cetra_core::time_serde")]
    #[schema(value_type = String, format = DateTime)]
    pub created_at: i64,
    /// Reason stored with the occurrence. Omitted when there is none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// The issue the occurrence belongs to: the issue itself or one merged into it.
    pub issue_id: Uuid,
}

/// One entry of an issue's history (D-IC-10).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct IssueEvent {
    /// Event id.
    pub id: Uuid,
    /// `created`, `state_changed`, `renamed`, `merged` or `regressed`.
    pub kind: String,
    /// The user who acted. Omitted for the system or an unknown user.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor_user_id: Option<Uuid>,
    /// The value before the change, when there is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_value: Option<String>,
    /// The value after the change, when there is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to_value: Option<String>,
    /// When it happened. JSON is RFC 3339.
    #[serde(with = "cetra_core::time_serde")]
    #[schema(value_type = String, format = DateTime)]
    pub at: i64,
}

/// `GET …/issues/{id}/events` answer: the newest 100 events, newest first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct IssueEventPage {
    /// The events, newest first.
    pub data: Vec<IssueEvent>,
}

/// `PATCH …/issues/{id}` body. Absent means unchanged (spec 09 § Issues).
///
/// Optional fields refuse `null`, except `description`, where `null` clears it.
#[derive(Debug, Clone, PartialEq, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdateIssueRequest {
    /// New title, 1 to 200 characters after trimming. A change records a `renamed` event.
    #[serde(default, deserialize_with = "present")]
    pub title: Option<String>,
    /// New description, at most 2000 characters. `null` or `""` clears it. Records no event.
    #[serde(default, deserialize_with = "present")]
    #[schema(value_type = Option<String>)]
    pub description: Option<Option<String>>,
    /// `open`, `resolved` or `ignored`. `merged` is refused: merge with the merge operation.
    #[serde(default, deserialize_with = "present")]
    pub state: Option<IssueState>,
}

/// `POST …/issues/{id}/merge` body (spec 09 § Issues).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct MergeIssueRequest {
    /// The issue to merge into. It must be an issue of the same source that is not merged.
    pub into: Uuid,
}

fn yes() -> bool {
    true
}
