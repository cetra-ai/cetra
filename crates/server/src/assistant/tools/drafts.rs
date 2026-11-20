//! The assistant's draft tools (track AS M2, D-AS-6). Each checks a proposal with the validators of the route
//! that would save it, with the permission to read the project (`ProjectPerm::View`), and answers the normalized
//! draft. Nothing is written and nothing leaves the server: the user applies a draft in the panel (M4).

use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::{Map, Value, json};
use uuid::Uuid;

use super::{JsonObject, ToolError, ToolScope};
use crate::api::assistant::AssistantDraftKind;
use crate::api::common::TimeParam;
use crate::api::dashboards::Definition;
use crate::api::evaluators::{EvaluatorKind, OutputScoreBody};
use crate::api::online_rules::{BudgetPeriod, RuleTarget};
use crate::auth::roles::ProjectPerm;
use crate::eval::evaluator;
use crate::http::AppState;
use crate::http::access::{ProjectAccess, user_project};
use crate::http::error::{ApiError, field_error};
use crate::mcp::tools as mcp_tools;
use crate::meta::evaluators::get_evaluator;
use crate::routes::dashboards::{
    check_definition, check_expressions, checked_description, checked_name, dashboard_detail,
    refuse_if_any,
};
use crate::routes::evaluators::{check_against_store, output_of};
use crate::routes::online_rules::{RuleDraft, normalize_filter, validate};
use crate::routes::read::{check_search, open_query, open_query_windowed};
use cetra_storage::{TimeWindow, ViewName};

pub(super) const DRAFT_FILTER: &str = "draft_filter";
pub(super) const DRAFT_SQL: &str = "draft_sql";
pub(super) const DRAFT_DASHBOARD: &str = "draft_dashboard";
pub(super) const DRAFT_EVALUATOR: &str = "draft_evaluator";
pub(super) const DRAFT_ONLINE_RULE: &str = "draft_online_rule";

pub(super) const DRAFT_FILTER_DESCRIPTION: &str = "Propose a filter, search and time range for the traces, spans, sessions or users list. It is validated and shown to the user as an Apply button; nothing is saved.";
pub(super) const DRAFT_SQL_DESCRIPTION: &str = "Propose a SQL query for the SQL console, with its parameters and time range. The statement is planned, not run, and its columns are returned. It is shown to the user as an Apply button; nothing is saved.";
pub(super) const DRAFT_DASHBOARD_DESCRIPTION: &str = "Propose a new dashboard, or an edit of one (give dashboard_id and version), with its full definition: widgets, their queries and layout. It is validated and shown to the user as an Apply button; nothing is saved.";
pub(super) const DRAFT_EVALUATOR_DESCRIPTION: &str = "Propose an LLM judge or a check evaluator with its output score. Webhook evaluators cannot be drafted. It is validated and shown to the user as an Apply button; nothing is saved.";
pub(super) const DRAFT_ONLINE_RULE_DESCRIPTION: &str = "Propose an online rule over an existing evaluator: its target, filter, variable mapping and sampling. It is validated and shown to the user as an Apply button; nothing is saved.";

/// The longest filter a draft holds, as the routes accept it.
const MAX_FILTER_BYTES: usize = 4_096;
/// The longest SQL statement a draft holds.
const MAX_QUERY_BYTES: usize = 65_536;
const NANOS_PER_MINUTE: i64 = 60_000_000_000;

/// The UI's time presets for a draft's window, ending now (D-AS-20).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
enum Range {
    #[serde(rename = "15m")]
    M15,
    #[serde(rename = "1h")]
    H1,
    #[serde(rename = "6h")]
    H6,
    #[serde(rename = "24h")]
    H24,
    #[serde(rename = "7d")]
    D7,
    #[serde(rename = "30d")]
    D30,
}

impl Range {
    /// The preset as the model sent it.
    fn as_str(self) -> &'static str {
        match self {
            Self::M15 => "15m",
            Self::H1 => "1h",
            Self::H6 => "6h",
            Self::H24 => "24h",
            Self::D7 => "7d",
            Self::D30 => "30d",
        }
    }

    /// The preset's length in nanoseconds.
    fn span_ns(self) -> i64 {
        match self {
            Self::M15 => 15 * NANOS_PER_MINUTE,
            Self::H1 => 60 * NANOS_PER_MINUTE,
            Self::H6 => 6 * 60 * NANOS_PER_MINUTE,
            Self::H24 => 24 * 60 * NANOS_PER_MINUTE,
            Self::D7 => 7 * 24 * 60 * NANOS_PER_MINUTE,
            Self::D30 => 30 * 24 * 60 * NANOS_PER_MINUTE,
        }
    }
}

/// A `from` or `to` argument: RFC 3339 or epoch milliseconds, as the routes read them.
fn time_of(field: &str, text: &str) -> Result<TimeParam, ApiError> {
    serde_json::from_value::<TimeParam>(Value::String(text.to_owned()))
        .map_err(|_| field_error(field, "must be an RFC 3339 time or epoch milliseconds"))
}

/// The window a draft names: a preset `range` ending now, or `from` and `to` with `from` before `to`. Neither
/// form, or both, or one bound alone, is refused.
fn window_of(
    now: i64,
    range: Option<Range>,
    from: Option<&str>,
    to: Option<&str>,
) -> Result<Option<TimeWindow>, ApiError> {
    match (range, from, to) {
        (None, None, None) => Ok(None),
        (Some(range), None, None) => Ok(Some(TimeWindow::new(now - range.span_ns(), now)?)),
        (None, Some(from), Some(to)) => {
            let from = time_of("from", from)?.ns();
            let to = time_of("to", to)?.ns();
            Ok(Some(TimeWindow::new(from, to)?))
        }
        _ => Err(field_error("range", "give range, or from and to together")),
    }
}

/// Writes the window the model named into `draft`, as it was sent.
fn window_fields(
    draft: &mut Map<String, Value>,
    range: Option<Range>,
    from: Option<String>,
    to: Option<String>,
) {
    if let Some(range) = range {
        draft.insert("range".to_owned(), json!(range.as_str()));
    }
    if let Some(from) = from {
        draft.insert("from".to_owned(), json!(from));
    }
    if let Some(to) = to {
        draft.insert("to".to_owned(), json!(to));
    }
}

/// Trimmed text, or `None` when it is blank.
fn trimmed(text: Option<String>) -> Option<String> {
    text.map(|text| text.trim().to_owned())
        .filter(|text| !text.is_empty())
}

/// A value as JSON. A serialization failure is an internal error: it reaches the log, never the model.
fn json_of<T: serde::Serialize>(value: &T) -> Result<Value, ApiError> {
    serde_json::to_value(value).map_err(|error| ApiError::Internal(Box::new(error)))
}

/// The answer to the model and the record of a draft (D-AS-6): the normalized draft, accepted.
fn accepted(kind: AssistantDraftKind, draft: Value) -> (Value, (AssistantDraftKind, Value)) {
    let name = match kind {
        AssistantDraftKind::Filter => "filter",
        AssistantDraftKind::Sql => "sql",
        AssistantDraftKind::Dashboard => "dashboard",
        AssistantDraftKind::Evaluator => "evaluator",
        AssistantDraftKind::OnlineRule => "online_rule",
    };
    (
        json!({"accepted": true, "kind": name, "draft": draft.clone()}),
        (kind, draft),
    )
}

/// The scope's project, read by the user with `ProjectPerm::View`: the access every draft needs.
async fn draft_access(state: &AppState, scope: &ToolScope) -> Result<ProjectAccess, ApiError> {
    user_project(
        state,
        &scope.user,
        &scope.project.to_string(),
        ProjectPerm::View,
    )
    .await
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum FilterView {
    Traces,
    Spans,
    Sessions,
    Users,
}

impl FilterView {
    fn as_str(self) -> &'static str {
        match self {
            Self::Traces => "traces",
            Self::Spans => "spans",
            Self::Sessions => "sessions",
            Self::Users => "users",
        }
    }

    /// The storage view the filter is checked against. The users list filters through the traces view.
    fn storage(self) -> ViewName {
        match self {
            Self::Traces | Self::Users => ViewName::Traces,
            Self::Spans => ViewName::Spans,
            Self::Sessions => ViewName::Sessions,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FilterArgs {
    view: FilterView,
    filter: Option<String>,
    search: Option<String>,
    range: Option<Range>,
    from: Option<String>,
    to: Option<String>,
    #[serde(default)]
    include_eval: bool,
    #[serde(default)]
    include_experiments: bool,
}

/// `draft_filter`: a filter, search and window for a list page, checked with the view it filters.
pub(super) async fn draft_filter(
    state: &AppState,
    scope: &ToolScope,
    arguments: JsonObject,
) -> Result<(Value, (AssistantDraftKind, Value)), ToolError> {
    let args: FilterArgs = mcp_tools::arguments(arguments)?;
    if args
        .filter
        .as_ref()
        .is_some_and(|text| text.len() > MAX_FILTER_BYTES)
    {
        return Err(field_error("filter", "filter must be at most 4096 bytes").into());
    }
    check_search(args.search.as_deref())?;
    window_of(
        state.clock.now_ns(),
        args.range,
        args.from.as_deref(),
        args.to.as_deref(),
    )?;
    let access = draft_access(state, scope).await?;
    let filter = trimmed(args.filter);
    if let Some(text) = &filter {
        let ctx = open_query(state, access.project.org_id, access.project.id, None).await?;
        cetra_storage::validate_filter(&ctx, args.view.storage(), text).map_err(|error| {
            ApiError::QueryAt {
                field: "filter".to_owned(),
                error,
            }
        })?;
    }
    let mut draft = Map::new();
    draft.insert("view".to_owned(), json!(args.view.as_str()));
    if let Some(text) = filter {
        draft.insert("filter".to_owned(), json!(text));
    }
    if let Some(text) = trimmed(args.search) {
        draft.insert("search".to_owned(), json!(text));
    }
    window_fields(&mut draft, args.range, args.from, args.to);
    draft.insert("include_eval".to_owned(), json!(args.include_eval));
    draft.insert(
        "include_experiments".to_owned(),
        json!(args.include_experiments),
    );
    Ok(accepted(AssistantDraftKind::Filter, Value::Object(draft)))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SqlArgs {
    query: String,
    #[serde(default)]
    params: Map<String, Value>,
    range: Option<Range>,
    from: Option<String>,
    to: Option<String>,
}

/// `draft_sql`: a SQL statement, planned with its parameters and window (never run). Answers its columns.
pub(super) async fn draft_sql(
    state: &AppState,
    scope: &ToolScope,
    arguments: JsonObject,
) -> Result<(Value, (AssistantDraftKind, Value)), ToolError> {
    let args: SqlArgs = mcp_tools::arguments(arguments)?;
    if args.query.trim().is_empty() || args.query.len() > MAX_QUERY_BYTES {
        return Err(field_error("query", "query must be 1 to 65536 bytes").into());
    }
    let window = window_of(
        state.clock.now_ns(),
        args.range,
        args.from.as_deref(),
        args.to.as_deref(),
    )?;
    let access = draft_access(state, scope).await?;
    let (org, project) = (access.project.org_id, access.project.id);
    let ctx = open_query_windowed(state, org, project, window).await?;
    crate::datasets::sql_views::register(state, &ctx, org, project, &args.query, window).await?;
    let columns = ctx
        .sql_check(&args.query, &args.params)
        .await
        .map_err(ApiError::Sql)?;
    let mut draft = Map::new();
    draft.insert("query".to_owned(), json!(args.query));
    draft.insert("params".to_owned(), Value::Object(args.params));
    window_fields(&mut draft, args.range, args.from, args.to);
    draft.insert(
        "columns".to_owned(),
        Value::Array(
            columns
                .iter()
                .map(|column| json!({"name": column.name, "type": column.data_type}))
                .collect(),
        ),
    );
    Ok(accepted(AssistantDraftKind::Sql, Value::Object(draft)))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DashboardArgs {
    dashboard_id: Option<String>,
    version: Option<i64>,
    name: String,
    description: Option<String>,
    definition: Value,
}

/// `draft_dashboard`: a new dashboard, or an edit of one at the version the model read. Checked as the create
/// route checks it, and the edit as a PATCH would be (the stored version must match).
pub(super) async fn draft_dashboard(
    state: &AppState,
    scope: &ToolScope,
    arguments: JsonObject,
) -> Result<(Value, (AssistantDraftKind, Value)), ToolError> {
    let args: DashboardArgs = mcp_tools::arguments(arguments)?;
    let edit = match (args.dashboard_id.as_deref(), args.version) {
        (None, None) => None,
        (Some(text), Some(version)) => Some((
            Uuid::parse_str(text).map_err(|_| ApiError::NotFound)?,
            version,
        )),
        _ => {
            return Err(field_error("version", "give dashboard_id and version together").into());
        }
    };
    let mut errors = Vec::new();
    let name = checked_name(&args.name, "name", &mut errors);
    // A blank description is no description, as the create route stores it.
    let description = checked_description(args.description.as_deref(), "description", &mut errors)
        .filter(|text| !text.is_empty());
    let definition: Definition = serde_json::from_value(args.definition)
        .map_err(|_| field_error("definition", "must be a dashboard definition with widgets"))?;
    check_definition(&definition, "definition", &mut errors);
    refuse_if_any(errors)?;
    let access = draft_access(state, scope).await?;
    check_expressions(state, &access, &definition, "definition").await?;

    let mut draft = Map::new();
    if let Some((id, version)) = edit {
        let stored = dashboard_detail(state, &access, id).await?;
        if stored.version != version {
            return Err(ApiError::StaleVersion {
                current: stored.version,
            }
            .into());
        }
        draft.insert("dashboard_id".to_owned(), json!(id));
        draft.insert("version".to_owned(), json!(version));
    }
    draft.insert("name".to_owned(), json!(name.unwrap_or_default()));
    if let Some(text) = description {
        draft.insert("description".to_owned(), json!(text));
    }
    draft.insert("definition".to_owned(), json_of(&definition)?);
    Ok(accepted(
        AssistantDraftKind::Dashboard,
        Value::Object(draft),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EvaluatorArgs {
    name: String,
    kind: EvaluatorKind,
    config: Value,
    output_score: OutputScoreBody,
}

/// `draft_evaluator`: an LLM judge or a check, prepared and checked against the store as the create route checks
/// it. A webhook is refused: its URL and secret are not for the model to propose (D-AS-6).
pub(super) async fn draft_evaluator(
    state: &AppState,
    scope: &ToolScope,
    arguments: JsonObject,
) -> Result<(Value, (AssistantDraftKind, Value)), ToolError> {
    let args: EvaluatorArgs = mcp_tools::arguments(arguments)?;
    if args.kind == EvaluatorKind::Webhook {
        return Err(field_error("kind", "webhook evaluators cannot be drafted").into());
    }
    let access = draft_access(state, scope).await?;
    let output = output_of(&args.output_score);
    let prepared = evaluator::prepare(
        Uuid::now_v7(),
        &args.name,
        args.kind,
        &args.config,
        &output,
        None,
        "",
    )
    .map_err(|errors| ApiError::BadRequest {
        detail: None,
        errors,
    })?;
    check_against_store(
        state.meta.reader(),
        access.project.org_id,
        access.project.id,
        &prepared,
    )
    .await?;
    let draft = json!({
        "name": args.name,
        "kind": args.kind.as_str(),
        "config": args.config,
        "output_score": json_of(&args.output_score)?,
    });
    Ok(accepted(AssistantDraftKind::Evaluator, draft))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OnlineRuleArgs {
    name: String,
    evaluator_id: Uuid,
    target: RuleTarget,
    filter: Option<String>,
    variable_mapping: BTreeMap<String, String>,
    #[serde(default = "default_sample_rate")]
    sample_rate: f64,
    budget_usd: Option<f64>,
    budget_period: Option<BudgetPeriod>,
    #[serde(default)]
    enabled: bool,
}

/// A rule samples every matching target unless the model says otherwise.
fn default_sample_rate() -> f64 {
    1.0
}

/// `draft_online_rule`: a rule over an existing evaluator, checked as the create route checks it. The draft starts
/// disabled unless the model asks otherwise.
pub(super) async fn draft_online_rule(
    state: &AppState,
    scope: &ToolScope,
    arguments: JsonObject,
) -> Result<(Value, (AssistantDraftKind, Value)), ToolError> {
    let args: OnlineRuleArgs = mcp_tools::arguments(arguments)?;
    let access = draft_access(state, scope).await?;
    let (org, project) = (access.project.org_id, access.project.id);
    let draft = RuleDraft {
        name: args.name.trim().to_owned(),
        target: args.target,
        filter: normalize_filter(args.filter),
        mapping: args.variable_mapping,
        sample_rate: args.sample_rate,
        budget_usd: args.budget_usd,
        budget_period: args.budget_period,
        enabled: args.enabled,
    };
    let evaluator = get_evaluator(state.meta.reader(), project, args.evaluator_id).await?;
    validate(
        state,
        org,
        project,
        &draft,
        evaluator.as_ref(),
        state.clock.now_ns(),
    )
    .await?;
    let mut body = Map::new();
    body.insert("name".to_owned(), json!(draft.name));
    body.insert("evaluator_id".to_owned(), json!(args.evaluator_id));
    body.insert("target".to_owned(), json!(draft.target.as_str()));
    if let Some(filter) = draft.filter {
        body.insert("filter".to_owned(), json!(filter));
    }
    body.insert("variable_mapping".to_owned(), json_of(&draft.mapping)?);
    body.insert("sample_rate".to_owned(), json!(draft.sample_rate));
    if let Some(budget) = draft.budget_usd {
        body.insert("budget_usd".to_owned(), json!(budget));
    }
    if let Some(period) = draft.budget_period {
        body.insert("budget_period".to_owned(), json!(period.as_str()));
    }
    body.insert("enabled".to_owned(), json!(draft.enabled));
    Ok(accepted(
        AssistantDraftKind::OnlineRule,
        Value::Object(body),
    ))
}

/// The arguments of `draft_filter`, for the model.
pub(super) fn draft_filter_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "view": {"type": "string", "enum": ["traces", "spans", "sessions", "users"], "description": "The list the filter is for."},
            "filter": {"type": "string", "description": "Filter expression over the list's view, at most 4096 bytes."},
            "search": {"type": "string", "description": "Free-text search, at most 1024 bytes."},
            "range": {"type": "string", "enum": ["15m", "1h", "6h", "24h", "7d", "30d"], "description": "Time range ending now. Not with from or to."},
            "from": {"type": "string", "description": "Inclusive start, RFC 3339. Needs to, not range."},
            "to": {"type": "string", "description": "Exclusive end, RFC 3339. Needs from, not range."},
            "include_eval": {"type": "boolean", "description": "Include traces from the evaluation environment. Default false."},
            "include_experiments": {"type": "boolean", "description": "Include experiment traces. Default false."}
        },
        "required": ["view"]
    })
}

/// The arguments of `draft_sql`, for the model.
pub(super) fn draft_sql_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "query": {"type": "string", "description": "One read-only SQL statement, as the SQL console runs it."},
            "params": {"type": "object", "description": "Values of the statement's {name:Type} placeholders."},
            "range": {"type": "string", "enum": ["15m", "1h", "6h", "24h", "7d", "30d"], "description": "Time range ending now. Not with from or to."},
            "from": {"type": "string", "description": "Inclusive start, RFC 3339. Needs to, not range."},
            "to": {"type": "string", "description": "Exclusive end, RFC 3339. Needs from, not range."}
        },
        "required": ["query"]
    })
}

/// The arguments of `draft_dashboard`, for the model.
pub(super) fn draft_dashboard_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "dashboard_id": {"type": "string", "description": "The dashboard to edit (UUID). Omit for a new dashboard."},
            "version": {"type": "integer", "description": "The version of dashboard_id that was read. Needs dashboard_id."},
            "name": {"type": "string", "description": "Display name, 1 to 100 characters."},
            "description": {"type": "string", "description": "Free text, up to 500 characters."},
            "definition": {
                "type": "object",
                "description": "The full definition, as get_dashboard returns it: {\"widgets\": [...]}, up to 50 widgets.",
                "properties": {"widgets": {"type": "array", "maxItems": 50, "items": {"type": "object"}}},
                "required": ["widgets"]
            }
        },
        "required": ["name", "definition"]
    })
}

/// The arguments of `draft_evaluator`, for the model.
pub(super) fn draft_evaluator_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "name": {"type": "string", "description": "Display name, 1 to 128 characters."},
            "kind": {"type": "string", "enum": ["llm_judge", "check"], "description": "llm_judge or check."},
            "config": {"type": "object", "description": "The kind's configuration, as get_evaluator returns it: a judge's prompt, model, connection_id and output; a check's check name and parameters."},
            "output_score": {
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "name": {"type": "string", "description": "Score name, 1 to 255 bytes."},
                    "data_type": {"type": "string", "enum": ["numeric", "boolean", "categorical"]},
                    "categories": {"type": "array", "items": {"type": "string"}, "description": "Category labels of a categorical score."}
                },
                "required": ["name", "data_type"]
            }
        },
        "required": ["name", "kind", "config", "output_score"]
    })
}

/// The arguments of `draft_online_rule`, for the model.
pub(super) fn draft_online_rule_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "name": {"type": "string", "description": "Display name, 1 to 128 characters."},
            "evaluator_id": {"type": "string", "description": "The evaluator the rule runs (UUID), as list_evaluators returns it."},
            "target": {"type": "string", "enum": ["trace", "span"], "description": "What the rule matches. Immutable once created."},
            "filter": {"type": "string", "description": "Filter over the target's view."},
            "variable_mapping": {"type": "object", "additionalProperties": {"type": "string"}, "description": "Prompt variable to mapping expression, 1 to 32 variables."},
            "sample_rate": {"type": "number", "exclusiveMinimum": 0, "maximum": 1, "description": "Share of the matching targets to evaluate. Default 1."},
            "budget_usd": {"type": "number", "exclusiveMinimum": 0, "description": "Budget per period in USD. Judge evaluators only."},
            "budget_period": {"type": "string", "enum": ["day", "month"], "description": "Period of the budget. Required with budget_usd."},
            "enabled": {"type": "boolean", "description": "Whether the rule runs once applied. Default false."}
        },
        "required": ["name", "evaluator_id", "target", "variable_mapping"]
    })
}

#[cfg(test)]
mod tests {
    use axum::http::Method;
    use serde_json::{Map, Value, json};
    use uuid::Uuid;

    use super::super::{JsonObject, ToolResult, ToolScope, UnknownTool, run};
    use crate::api::assistant::AssistantDraftKind;
    use crate::auth::principal::AuthUser;
    use crate::mcp::test_helpers::{FROM, TO};
    use crate::test_support::{Seeded, TestApp};

    fn scope_of(seeded: &Seeded) -> ToolScope {
        ToolScope {
            user: AuthUser {
                id: seeded.user_id,
                is_instance_admin: false,
                email: "ada@example.com".to_owned(),
            },
            project: seeded.project.id,
        }
    }

    fn arguments(value: Value) -> JsonObject {
        match value {
            Value::Object(map) => map,
            _ => Map::new(),
        }
    }

    async fn call(app: &TestApp, seeded: &Seeded, name: &str, value: Value) -> ToolResult {
        run(&app.state, &scope_of(seeded), name, arguments(value))
            .await
            .expect("a known tool")
    }

    async fn seeded_app() -> (TestApp, Seeded) {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        app.load_example_dataset(seeded.project.id).await;
        (app, seeded)
    }

    /// `POST` on a route with the seeded owner's cookie; the body must be created.
    async fn post(app: &TestApp, seeded: &Seeded, path: &str, body: Value) -> Value {
        let response = app
            .request(
                Method::POST,
                &format!("/api/v1/projects/{}{path}", seeded.project.id),
            )
            .header("cookie", &seeded.cookie)
            .json(&body)
            .send()
            .await;
        assert!(
            response.status.is_success(),
            "{}",
            String::from_utf8_lossy(&response.body)
        );
        response.json::<Value>()
    }

    /// `GET` on a route with the seeded owner's cookie.
    async fn get(app: &TestApp, seeded: &Seeded, path: &str) -> Value {
        let response = app
            .request(
                Method::GET,
                &format!("/api/v1/projects/{}{path}", seeded.project.id),
            )
            .header("cookie", &seeded.cookie)
            .send()
            .await;
        assert!(
            response.status.is_success(),
            "{}",
            String::from_utf8_lossy(&response.body)
        );
        response.json::<Value>()
    }

    /// The project's dashboards, evaluators and online rules, as their lists answer them.
    async fn snapshot(app: &TestApp, seeded: &Seeded) -> Value {
        json!({
            "dashboards": get(app, seeded, "/dashboards").await["data"],
            "evaluators": get(app, seeded, "/evaluators").await["data"],
            "online_rules": get(app, seeded, "/online-rules").await["data"],
        })
    }

    fn check_body(name: &str) -> Value {
        json!({
            "name": name,
            "kind": "check",
            "config": {"check": "json_valid"},
            "output_score": {"name": name, "data_type": "boolean"}
        })
    }

    /// A one-widget dashboard definition with a valid chart.
    fn definition() -> Value {
        json!({"widgets": [{
            "id": "w0", "title": "Chart",
            "layout": {"x": 0, "y": 0, "w": 6, "h": 4},
            "kind": "chart",
            "chart": {"type": "line"},
            "query": {"source": "traces", "measures": [{"agg": "count", "label": "Requests"}]}
        }]})
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn draft_filter_validates_each_view() {
        let (app, seeded) = seeded_app().await;
        for (view, filter) in [
            ("traces", "status = 'ok'"),
            ("spans", "status = 'ok'"),
            ("sessions", "trace_count > 1"),
            ("users", "status = 'ok'"),
        ] {
            let result = call(
                &app,
                &seeded,
                "draft_filter",
                json!({"view": view, "filter": filter}),
            )
            .await;
            assert!(result.ok, "{view}: {:?}", result.value);
            assert_eq!(result.value["kind"], "filter");
            assert_eq!(result.value["accepted"], true);
            assert_eq!(
                result.draft.as_ref().map(|(kind, _)| *kind),
                Some(AssistantDraftKind::Filter)
            );
            assert_eq!(result.rows, None);
        }
        let bad = call(
            &app,
            &seeded,
            "draft_filter",
            json!({"view": "traces", "filter": "nope = 1"}),
        )
        .await;
        assert!(!bad.ok);
        assert_eq!(bad.value["error"]["code"], "invalid_filter");
        assert!(bad.value["error"]["position"].is_object());
        assert!(bad.draft.is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn draft_filter_range_rules() {
        let (app, seeded) = seeded_app().await;
        for bad in [
            json!({"view": "traces", "range": "1h", "from": FROM, "to": TO}),
            json!({"view": "traces", "range": "2h"}),
            json!({"view": "traces", "from": FROM}),
        ] {
            let result = call(&app, &seeded, "draft_filter", bad).await;
            assert!(!result.ok);
            assert_eq!(result.value["error"]["code"], "bad_request");
        }
        let ok = call(
            &app,
            &seeded,
            "draft_filter",
            json!({"view": "traces", "range": "1h"}),
        )
        .await;
        assert!(ok.ok, "{:?}", ok.value);
        let (_, draft) = ok.draft.expect("a draft");
        assert_eq!(draft["range"], "1h");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn draft_sql_returns_columns() {
        let (app, seeded) = seeded_app().await;
        let result = call(
            &app,
            &seeded,
            "draft_sql",
            json!({
                "query": "SELECT name, count(*) AS n FROM traces GROUP BY name",
                "from": FROM,
                "to": TO,
            }),
        )
        .await;
        assert!(result.ok, "{:?}", result.value);
        assert_eq!(result.value["kind"], "sql");
        let (kind, draft) = result.draft.expect("a draft");
        assert_eq!(kind, AssistantDraftKind::Sql);
        assert_eq!(draft["columns"].as_array().unwrap().len(), 2);

        let bad = call(
            &app,
            &seeded,
            "draft_sql",
            json!({"query": "DELETE FROM traces", "from": FROM, "to": TO}),
        )
        .await;
        assert!(!bad.ok);
        assert_eq!(bad.value["error"]["code"], "invalid_filter");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn draft_dashboard_new_and_edit() {
        let (app, seeded) = seeded_app().await;
        let new = call(
            &app,
            &seeded,
            "draft_dashboard",
            json!({"name": "Traffic", "definition": definition()}),
        )
        .await;
        assert!(new.ok, "{:?}", new.value);
        let (kind, draft) = new.draft.expect("a draft");
        assert_eq!(kind, AssistantDraftKind::Dashboard);
        assert!(draft.get("dashboard_id").is_none());

        let stored = post(
            &app,
            &seeded,
            "/dashboards",
            json!({"name": "Traffic", "definition": definition()}),
        )
        .await;
        let id = stored["id"].as_str().unwrap().to_owned();
        let stale = call(
            &app,
            &seeded,
            "draft_dashboard",
            json!({"dashboard_id": id, "version": 99, "name": "Traffic", "definition": definition()}),
        )
        .await;
        assert!(!stale.ok);
        assert_eq!(stale.value["error"]["code"], "conflict");

        let edit = call(
            &app,
            &seeded,
            "draft_dashboard",
            json!({"dashboard_id": id, "version": 1, "name": "Traffic 2", "definition": definition()}),
        )
        .await;
        assert!(edit.ok, "{:?}", edit.value);
        let (_, draft) = edit.draft.expect("a draft");
        assert_eq!(draft["dashboard_id"], id);
        assert_eq!(draft["version"], 1);

        let mut wide = definition();
        wide["widgets"][0]["layout"]["w"] = json!(61);
        let refused = call(
            &app,
            &seeded,
            "draft_dashboard",
            json!({"name": "Traffic", "definition": wide}),
        )
        .await;
        assert!(!refused.ok);
        assert_eq!(refused.value["error"]["code"], "bad_request");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn draft_evaluator_refuses_webhooks() {
        let (app, seeded) = seeded_app().await;
        let result = call(
            &app,
            &seeded,
            "draft_evaluator",
            json!({
                "name": "Hook",
                "kind": "webhook",
                "config": {"url": "https://hooks.example.com/x"},
                "output_score": {"name": "hook", "data_type": "numeric"}
            }),
        )
        .await;
        assert!(!result.ok);
        assert_eq!(result.value["error"]["code"], "bad_request");
        assert!(result.draft.is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn draft_evaluator_accepts_a_check_and_checks_the_connection() {
        let (app, seeded) = seeded_app().await;
        let check = call(
            &app,
            &seeded,
            "draft_evaluator",
            json!({
                "name": "json",
                "kind": "check",
                "config": {"check": "json_valid"},
                "output_score": {"name": "json", "data_type": "boolean"}
            }),
        )
        .await;
        assert!(check.ok, "{:?}", check.value);
        let (kind, _) = check.draft.expect("a draft");
        assert_eq!(kind, AssistantDraftKind::Evaluator);

        let judge = call(
            &app,
            &seeded,
            "draft_evaluator",
            json!({
                "name": "relevance",
                "kind": "llm_judge",
                "config": {
                    "connection_id": Uuid::now_v7(),
                    "model": "gpt-5-mini",
                    "prompt": "Question: {{ input }}\nAnswer: {{ output }}",
                    "output": {"type": "boolean"},
                    "include_reason": false,
                    "temperature": null,
                    "max_output_tokens": 1024,
                    "timeout_ms": 60000
                },
                "output_score": {"name": "relevance", "data_type": "boolean"}
            }),
        )
        .await;
        assert!(!judge.ok);
        assert_eq!(judge.value["error"]["code"], "bad_request");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn draft_online_rule_checks_mapping() {
        let (app, seeded) = seeded_app().await;
        let evaluator = post(&app, &seeded, "/evaluators", check_body("json")).await;
        let evaluator_id = evaluator["id"].as_str().unwrap().to_owned();

        let missing_output = call(
            &app,
            &seeded,
            "draft_online_rule",
            json!({
                "name": "JSON check",
                "evaluator_id": evaluator_id,
                "target": "trace",
                "variable_mapping": {"input": "trace.input"}
            }),
        )
        .await;
        assert!(!missing_output.ok);
        assert_eq!(missing_output.value["error"]["code"], "bad_request");

        let ok = call(
            &app,
            &seeded,
            "draft_online_rule",
            json!({
                "name": "JSON check",
                "evaluator_id": evaluator_id,
                "target": "trace",
                "variable_mapping": {"output": "trace.output"}
            }),
        )
        .await;
        assert!(ok.ok, "{:?}", ok.value);
        let (kind, draft) = ok.draft.expect("a draft");
        assert_eq!(kind, AssistantDraftKind::OnlineRule);
        assert_eq!(draft["enabled"], false);
        assert_eq!(draft["target"], "trace");
        assert_eq!(draft["sample_rate"], 1.0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn drafts_write_nothing() {
        let (app, seeded) = seeded_app().await;
        post(
            &app,
            &seeded,
            "/dashboards",
            json!({"name": "Traffic", "definition": definition()}),
        )
        .await;
        let evaluator = post(&app, &seeded, "/evaluators", check_body("json")).await;
        let evaluator_id = evaluator["id"].as_str().unwrap().to_owned();
        let before = snapshot(&app, &seeded).await;

        let filter = call(
            &app,
            &seeded,
            "draft_filter",
            json!({"view": "traces", "filter": "status = 'ok'"}),
        )
        .await;
        assert!(filter.ok, "{:?}", filter.value);
        let sql = call(
            &app,
            &seeded,
            "draft_sql",
            json!({"query": "SELECT 1 AS one", "from": FROM, "to": TO}),
        )
        .await;
        assert!(sql.ok, "{:?}", sql.value);
        let dashboard = call(
            &app,
            &seeded,
            "draft_dashboard",
            json!({"name": "New", "definition": definition()}),
        )
        .await;
        assert!(dashboard.ok, "{:?}", dashboard.value);
        let check = call(&app, &seeded, "draft_evaluator", json!({"name": "other", "kind": "check", "config": {"check": "json_valid"}, "output_score": {"name": "other", "data_type": "boolean"}})).await;
        assert!(check.ok, "{:?}", check.value);
        let rule = call(&app, &seeded, "draft_online_rule", json!({"name": "JSON check", "evaluator_id": evaluator_id, "target": "trace", "variable_mapping": {"output": "trace.output"}})).await;
        assert!(rule.ok, "{:?}", rule.value);

        assert_eq!(snapshot(&app, &seeded).await, before);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn every_tool_is_registered() {
        let (app, seeded) = seeded_app().await;
        for name in [
            "draft_filter",
            "draft_sql",
            "draft_dashboard",
            "draft_evaluator",
            "draft_online_rule",
        ] {
            let outcome = run(&app.state, &scope_of(&seeded), name, JsonObject::new()).await;
            assert!(!matches!(outcome, Err(UnknownTool)), "{name}");
        }
    }
}
