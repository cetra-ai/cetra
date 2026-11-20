//! The assistant's read tools over sessions, scores, the project, alert rules, evaluators, online rules,
//! dashboards and the widget query (track AS M2, D-AS-5, D-AS-19, D-AS-20). Each tool reads the scope's project
//! with the permission of its route (`ProjectPerm::View`; `query_metrics` takes `ProjectPerm::Query`) and
//! answers through the function its route serves, so a tool shows exactly what the route shows.

use std::collections::HashMap;

use cetra_core::enums::{ScoreSource, ScoreTargetKind};
use cetra_core::ids::{SpanId, TraceId};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use super::shape::{self, CONFIG_CUT, VALUE_CUT};
use super::{JsonObject, ToolError, ToolScope};
use crate::api::common::{ExclusionParams, Limit, PageParams, SortOrder, TimeParam};
use crate::api::dashboards::{DashboardListParams, WidgetQueryRequest, WidgetShape};
use crate::api::score_definitions::ScoreDefinitionListParams;
use crate::api::scores::ScoreListParams;
use crate::api::sessions::{SessionListParams, SessionSort};
use crate::auth::roles::ProjectPerm;
use crate::http::AppState;
use crate::http::access::{ProjectAccess, user_project};
use crate::http::error::ApiError;
use crate::mcp::sql::required_window;
use crate::mcp::tools as mcp_tools;
use crate::routes::alert_rules::alert_rule_page;
use crate::routes::dashboards::{dashboard_detail, dashboard_page, widget_data};
use crate::routes::evaluators::{evaluator_detail, evaluator_page, evaluator_stats};
use crate::routes::online_rules::{rule_detail, rule_page, rule_stats};
use crate::routes::projects::project_detail;
use crate::routes::score_definitions::definition_page;
use crate::routes::scores::score_page;
use crate::routes::sessions::{session_detail, session_page};

pub(super) const LIST_SESSIONS: &str = "list_sessions";
pub(super) const GET_SESSION: &str = "get_session";
pub(super) const LIST_SCORES: &str = "list_scores";
pub(super) const LIST_SCORE_DEFINITIONS: &str = "list_score_definitions";
pub(super) const GET_PROJECT_SETTINGS: &str = "get_project_settings";
pub(super) const LIST_ALERT_RULES: &str = "list_alert_rules";

pub(super) const LIST_SESSIONS_DESCRIPTION: &str = "Sessions (conversations grouped by session_id) in a time window, newest first, with trace counts, cost, errors and scores. Evaluation and experiment traces are left out unless asked.";
pub(super) const GET_SESSION_DESCRIPTION: &str = "One session's turns in order: each trace's summary and its root input and output, cut to 200 characters. Use get_trace for a full turn.";
pub(super) const LIST_SCORES_DESCRIPTION: &str = "Scores (human, API and evaluator scores), newest first, filtered by name, source, target or evaluator.";
pub(super) const LIST_SCORE_DEFINITIONS_DESCRIPTION: &str =
    "The project's score definitions: name, type, range or categories, direction.";
pub(super) const GET_PROJECT_SETTINGS_DESCRIPTION: &str =
    "The project's settings: name, retention, trace completion timeout, last ingest time.";
pub(super) const LIST_ALERT_RULES_DESCRIPTION: &str =
    "The project's alert rules with their condition, state, last value and last error.";
pub(super) const LIST_EVALUATORS_DESCRIPTION: &str = "Evaluators (LLM judges, checks, webhooks) with their output score and last-7-days job and score summary.";
pub(super) const GET_EVALUATOR_DESCRIPTION: &str = "One evaluator's full configuration (judge template, model, connection id, check parameters) and its daily stats. Webhook secrets and URLs are never shown.";
pub(super) const LIST_ONLINE_RULES_DESCRIPTION: &str = "Online evaluation rules: evaluator, filter, sampling, budget, state and the last 24 hours of jobs and scores.";
pub(super) const GET_ONLINE_RULE_DESCRIPTION: &str =
    "One online rule's configuration and its hourly jobs and scores over the last 24 hours.";
pub(super) const LIST_DASHBOARDS_DESCRIPTION: &str =
    "The project's custom dashboards (name, description).";
pub(super) const GET_DASHBOARD_DESCRIPTION: &str =
    "One dashboard's definition: its widgets, their queries and layout, and its version.";
pub(super) const QUERY_METRICS_DESCRIPTION: &str = "Aggregate traces, spans or scores over time or by group, as a dashboard widget does: counts, sums, averages and percentiles of a field, optionally broken down. from and to are required, at most 30 days apart.";

pub(super) const LIST_EVALUATORS: &str = "list_evaluators";
pub(super) const GET_EVALUATOR: &str = "get_evaluator";
pub(super) const LIST_ONLINE_RULES: &str = "list_online_rules";
pub(super) const GET_ONLINE_RULE: &str = "get_online_rule";
pub(super) const LIST_DASHBOARDS: &str = "list_dashboards";
pub(super) const GET_DASHBOARD: &str = "get_dashboard";
pub(super) const QUERY_METRICS: &str = "query_metrics";

/// `limit` when the model omits it (D-AS-20).
const DEFAULT_LIMIT: u32 = 20;
/// The most rows of a list tool (D-AS-20).
const LIST_MAX: u32 = 100;
/// The most rows of `list_evaluators`: the stats read takes at most 50 ids (D-AS-20).
const EVALUATOR_LIST_MAX: u32 = 50;

/// The model's `limit`, as the routes' `Limit`: 1 to `max`, 20 by default (D-AS-20).
fn bounded_limit(given: Option<u32>, max: u32) -> Result<Limit, ApiError> {
    let value = given.unwrap_or(DEFAULT_LIMIT);
    if !(1..=max).contains(&value) {
        return Err(bad_limit(max));
    }
    serde_json::from_value(json!(value)).map_err(|_| bad_limit(max))
}

/// The model's `limit` for a list tool: 1 to 100 (D-AS-20).
fn list_limit(given: Option<u32>) -> Result<Limit, ApiError> {
    bounded_limit(given, LIST_MAX)
}

fn bad_limit(max: u32) -> ApiError {
    ApiError::BadRequest {
        detail: Some(format!("limit must be between 1 and {max}")),
        errors: Vec::new(),
    }
}

/// A UUID argument. Text that is not a UUID is `not_found`, as the routes answer an id they cannot find.
fn id_arg(text: &str) -> Result<Uuid, ApiError> {
    Uuid::parse_str(text).map_err(|_| ApiError::NotFound)
}

/// A `from` or `to` argument as a time: RFC 3339 or epoch milliseconds, as `TimeParam` reads them.
/// Text that is neither is `None`, so the window check refuses it as missing.
fn time_arg(text: &str) -> Option<TimeParam> {
    serde_json::from_value(Value::String(text.to_owned())).ok()
}

/// Removes `key` from the object `parent` of `value`, or from `value` itself when `parent` is `None`. A
/// value that is not an object is left as it is.
fn without(value: &mut Value, parent: Option<&str>, key: &str) {
    let target = match parent {
        Some(parent) => value.get_mut(parent),
        None => Some(value),
    };
    if let Some(object) = target.and_then(Value::as_object_mut) {
        object.remove(key);
    }
}

/// `value` as JSON. A serialization failure is an internal error: it reaches the log, never the model.
fn json_of<T: Serialize>(value: &T) -> Result<Value, ApiError> {
    serde_json::to_value(value).map_err(|error| ApiError::Internal(Box::new(error)))
}

/// The items of a list as JSON values.
fn items<T: Serialize>(rows: &[T]) -> Result<Vec<Value>, ApiError> {
    rows.iter().map(json_of).collect()
}

/// The items a shaped list answer holds: the array `list` of `value` after the budget fit, so `rows` counts what
/// the model receives.
fn shaped_rows(value: &Value, list: &str) -> Option<u64> {
    u64::try_from(value.get(list)?.as_array()?.len()).ok()
}

/// The scope's project, read by the user with `ProjectPerm::View`: the access every read tool needs.
async fn project_access(state: &AppState, scope: &ToolScope) -> Result<ProjectAccess, ApiError> {
    user_project(
        state,
        &scope.user,
        &scope.project.to_string(),
        ProjectPerm::View,
    )
    .await
}

/// A tool that takes no arguments: any argument, a `project` included, is refused.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NoArguments {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SessionListArgs {
    filter: Option<String>,
    from: Option<TimeParam>,
    to: Option<TimeParam>,
    #[serde(default)]
    sort: SessionSort,
    #[serde(default)]
    order: SortOrder,
    limit: Option<u32>,
    cursor: Option<String>,
    #[serde(default)]
    include_eval: bool,
    #[serde(default)]
    include_experiments: bool,
}

/// `list_sessions`: the sessions of `GET /sessions` in a window.
pub(super) async fn list_sessions(
    state: &AppState,
    scope: &ToolScope,
    arguments: JsonObject,
) -> Result<(Value, Option<u64>), ToolError> {
    let args: SessionListArgs = mcp_tools::arguments(arguments)?;
    let params = SessionListParams {
        filter: args.filter,
        from: args.from,
        to: args.to,
        sort: args.sort,
        order: args.order,
        limit: list_limit(args.limit)?,
        cursor: args.cursor,
        include_eval: args.include_eval,
        include_experiments: args.include_experiments,
    };
    let access = project_access(state, scope).await?;
    let page = session_page(state, &access, params).await?;
    let value = shape::list_body("sessions", items(&page.data)?, page.next_cursor, VALUE_CUT);
    let rows = shaped_rows(&value, "sessions");
    Ok((value, rows))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SessionDetailArgs {
    session_id: String,
    #[serde(default)]
    include_eval: bool,
    #[serde(default)]
    include_experiments: bool,
}

/// `get_session`: one session with its turns, as `GET /sessions/{session_id}`.
pub(super) async fn get_session(
    state: &AppState,
    scope: &ToolScope,
    arguments: JsonObject,
) -> Result<(Value, Option<u64>), ToolError> {
    let args: SessionDetailArgs = mcp_tools::arguments(arguments)?;
    let access = project_access(state, scope).await?;
    let exclusions = ExclusionParams {
        include_eval: args.include_eval,
        include_experiments: args.include_experiments,
    };
    let detail = session_detail(state, &access, &args.session_id, exclusions).await?;
    let value = shape::object_body(json_of(&detail)?, VALUE_CUT, Some("turns"));
    Ok((value, None))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ScoreListArgs {
    target: Option<ScoreTargetKind>,
    trace_id: Option<TraceId>,
    span_id: Option<SpanId>,
    session_id: Option<String>,
    name: Option<String>,
    source: Option<ScoreSource>,
    evaluator_id: Option<Uuid>,
    rule_id: Option<Uuid>,
    from: Option<TimeParam>,
    to: Option<TimeParam>,
    limit: Option<u32>,
    cursor: Option<String>,
}

/// `list_scores`: the scores of `GET /scores`.
pub(super) async fn list_scores(
    state: &AppState,
    scope: &ToolScope,
    arguments: JsonObject,
) -> Result<(Value, Option<u64>), ToolError> {
    let args: ScoreListArgs = mcp_tools::arguments(arguments)?;
    let params = ScoreListParams {
        target: args.target,
        trace_id: args.trace_id,
        span_id: args.span_id,
        session_id: args.session_id,
        name: args.name,
        source: args.source,
        evaluator_id: args.evaluator_id,
        rule_id: args.rule_id,
        queue_id: None,
        from: args.from,
        to: args.to,
        limit: list_limit(args.limit)?,
        cursor: args.cursor,
    };
    let access = project_access(state, scope).await?;
    let page = score_page(state, &access, params).await?;
    let value = shape::list_body("scores", items(&page.data)?, page.next_cursor, VALUE_CUT);
    let rows = shaped_rows(&value, "scores");
    Ok((value, rows))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DefinitionListArgs {
    #[serde(default)]
    include_archived: bool,
    limit: Option<u32>,
    cursor: Option<String>,
}

/// `list_score_definitions`: the definitions of `GET /score-definitions`.
pub(super) async fn list_score_definitions(
    state: &AppState,
    scope: &ToolScope,
    arguments: JsonObject,
) -> Result<(Value, Option<u64>), ToolError> {
    let args: DefinitionListArgs = mcp_tools::arguments(arguments)?;
    let params = ScoreDefinitionListParams {
        include_archived: args.include_archived,
        limit: list_limit(args.limit)?,
        cursor: args.cursor,
    };
    let access = project_access(state, scope).await?;
    let page = definition_page(state, &access, params).await?;
    let value = shape::list_body(
        "definitions",
        items(&page.data)?,
        page.next_cursor,
        VALUE_CUT,
    );
    let rows = shaped_rows(&value, "definitions");
    Ok((value, rows))
}

/// `get_project_settings`: the project, as `GET /projects/{project}`.
pub(super) async fn get_project_settings(
    state: &AppState,
    scope: &ToolScope,
    arguments: JsonObject,
) -> Result<(Value, Option<u64>), ToolError> {
    let _: NoArguments = mcp_tools::arguments(arguments)?;
    let access = project_access(state, scope).await?;
    let project = project_detail(state, &access).await?;
    let value = shape::object_body(json_of(&project)?, VALUE_CUT, None);
    Ok((value, None))
}

/// `list_alert_rules`: the rules of `GET /alert-rules`. Channels appear by id only, never their config.
pub(super) async fn list_alert_rules(
    state: &AppState,
    scope: &ToolScope,
    arguments: JsonObject,
) -> Result<(Value, Option<u64>), ToolError> {
    let _: NoArguments = mcp_tools::arguments(arguments)?;
    let access = project_access(state, scope).await?;
    let page = alert_rule_page(state, &access).await?;
    let value = shape::list_body("rules", items(&page.data)?, page.next_cursor, VALUE_CUT);
    let rows = shaped_rows(&value, "rules");
    Ok((value, rows))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PageArgs {
    limit: Option<u32>,
    cursor: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EvaluatorArgs {
    evaluator_id: String,
}

/// `list_evaluators`: the evaluators of `GET /evaluators`, each with its 7-day stats. The daily series are
/// left out (the model asks for one evaluator to see them).
pub(super) async fn list_evaluators(
    state: &AppState,
    scope: &ToolScope,
    arguments: JsonObject,
) -> Result<(Value, Option<u64>), ToolError> {
    let args: PageArgs = mcp_tools::arguments(arguments)?;
    let params = PageParams {
        limit: bounded_limit(args.limit, EVALUATOR_LIST_MAX)?,
        cursor: args.cursor,
    };
    let access = project_access(state, scope).await?;
    let page = evaluator_page(state, &access, params).await?;
    let mut stats = HashMap::new();
    if !page.data.is_empty() {
        let ids = page
            .data
            .iter()
            .map(|evaluator| evaluator.id.to_string())
            .collect::<Vec<_>>()
            .join(",");
        for entry in evaluator_stats(state, &access, Some(ids.as_str()))
            .await?
            .data
        {
            stats.insert(entry.evaluator_id, entry);
        }
    }
    let mut items = Vec::with_capacity(page.data.len());
    for evaluator in &page.data {
        let mut item = json_of(evaluator)?;
        item["stats"] = match stats.get(&evaluator.id) {
            Some(entry) => {
                let mut value = json_of(entry)?;
                without(&mut value, Some("jobs"), "runs_daily");
                without(&mut value, Some("scores"), "daily");
                value
            }
            None => Value::Null,
        };
        items.push(item);
    }
    let value = shape::list_body("evaluators", items, page.next_cursor, VALUE_CUT);
    let rows = shaped_rows(&value, "evaluators");
    Ok((value, rows))
}

/// `get_evaluator`: one evaluator with its configuration and its 7-day stats, daily series included.
pub(super) async fn get_evaluator(
    state: &AppState,
    scope: &ToolScope,
    arguments: JsonObject,
) -> Result<(Value, Option<u64>), ToolError> {
    let args: EvaluatorArgs = mcp_tools::arguments(arguments)?;
    let id = id_arg(&args.evaluator_id)?;
    let access = project_access(state, scope).await?;
    let evaluator = evaluator_detail(state, &access, id).await?;
    let id_text = id.to_string();
    let stats = evaluator_stats(state, &access, Some(id_text.as_str())).await?;
    let entry = stats.data.iter().find(|entry| entry.evaluator_id == id);
    let value = shape::object_body(
        json!({"evaluator": json_of(&evaluator)?, "stats": json_of(&entry)?}),
        CONFIG_CUT,
        None,
    );
    Ok((value, None))
}

/// `list_online_rules`: the rules of `GET /online-rules`, each with its 24-hour stats. The hourly series are
/// left out (the model asks for one rule to see them).
pub(super) async fn list_online_rules(
    state: &AppState,
    scope: &ToolScope,
    arguments: JsonObject,
) -> Result<(Value, Option<u64>), ToolError> {
    let args: PageArgs = mcp_tools::arguments(arguments)?;
    let params = PageParams {
        limit: list_limit(args.limit)?,
        cursor: args.cursor,
    };
    let access = project_access(state, scope).await?;
    let page = rule_page(state, &access, params).await?;
    let mut stats = HashMap::new();
    for entry in rule_stats(state, &access).await?.data {
        stats.insert(entry.rule_id, entry);
    }
    let mut items = Vec::with_capacity(page.data.len());
    for rule in &page.data {
        let mut item = json_of(rule)?;
        item["stats"] = match stats.get(&rule.id) {
            Some(entry) => {
                let mut value = json_of(entry)?;
                without(&mut value, None, "jobs_hourly");
                without(&mut value, Some("scores"), "hourly");
                value
            }
            None => Value::Null,
        };
        items.push(item);
    }
    let value = shape::list_body("rules", items, page.next_cursor, VALUE_CUT);
    let rows = shaped_rows(&value, "rules");
    Ok((value, rows))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RuleArgs {
    rule_id: String,
}

/// `get_online_rule`: one rule with its configuration and its hourly stats, as the rule route and its stats
/// route answer them.
pub(super) async fn get_online_rule(
    state: &AppState,
    scope: &ToolScope,
    arguments: JsonObject,
) -> Result<(Value, Option<u64>), ToolError> {
    let args: RuleArgs = mcp_tools::arguments(arguments)?;
    let id = id_arg(&args.rule_id)?;
    let access = project_access(state, scope).await?;
    let rule = rule_detail(state, &access, id).await?;
    let stats = rule_stats(state, &access).await?;
    let entry = stats.data.iter().find(|entry| entry.rule_id == id);
    let value = shape::object_body(
        json!({"rule": json_of(&rule)?, "stats": json_of(&entry)?}),
        CONFIG_CUT,
        None,
    );
    Ok((value, None))
}

/// `list_dashboards`: the dashboards of `GET /dashboards`, without their definitions.
pub(super) async fn list_dashboards(
    state: &AppState,
    scope: &ToolScope,
    arguments: JsonObject,
) -> Result<(Value, Option<u64>), ToolError> {
    let args: PageArgs = mcp_tools::arguments(arguments)?;
    let params = DashboardListParams {
        limit: list_limit(args.limit)?,
        cursor: args.cursor,
    };
    let access = project_access(state, scope).await?;
    let page = dashboard_page(state, &access, params).await?;
    let value = shape::list_body(
        "dashboards",
        items(&page.data)?,
        page.next_cursor,
        VALUE_CUT,
    );
    let rows = shaped_rows(&value, "dashboards");
    Ok((value, rows))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DashboardArgs {
    dashboard_id: String,
}

/// `get_dashboard`: one dashboard with its definition and version.
pub(super) async fn get_dashboard(
    state: &AppState,
    scope: &ToolScope,
    arguments: JsonObject,
) -> Result<(Value, Option<u64>), ToolError> {
    let args: DashboardArgs = mcp_tools::arguments(arguments)?;
    let id = id_arg(&args.dashboard_id)?;
    let access = project_access(state, scope).await?;
    let dashboard = dashboard_detail(state, &access, id).await?;
    let value = shape::object_body(json_of(&dashboard)?, CONFIG_CUT, None);
    Ok((value, None))
}

/// `query_metrics`: one widget's query, as `POST /dashboards/query` answers it. The window is checked here
/// (RFC 3339 or epoch milliseconds, at most 30 days); the route refuses epoch milliseconds itself.
pub(super) async fn query_metrics(
    state: &AppState,
    scope: &ToolScope,
    arguments: JsonObject,
) -> Result<(Value, Option<u64>), ToolError> {
    let args: WidgetQueryRequest = mcp_tools::arguments(arguments)?;
    required_window(time_arg(&args.from), time_arg(&args.to))?;
    let access = user_project(
        state,
        &scope.user,
        &scope.project.to_string(),
        ProjectPerm::Query,
    )
    .await?;
    let list = match args.shape {
        WidgetShape::Series => "series",
        WidgetShape::Total => "rows",
    };
    let data = widget_data(state, &access, args)
        .await
        .map_err(ToolError::Shared)?;
    let value = shape::object_body(json_of(&*data)?, VALUE_CUT, Some(list));
    Ok((value, None))
}

/// The arguments of `list_evaluators`, for the model.
pub(super) fn list_evaluators_schema() -> Value {
    page_schema(50)
}

/// The arguments of `get_evaluator`, for the model.
pub(super) fn get_evaluator_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "evaluator_id": {"type": "string", "description": "The evaluator id (UUID), as list_evaluators returns it."}
        },
        "required": ["evaluator_id"]
    })
}

/// The arguments of `list_online_rules`, for the model.
pub(super) fn list_online_rules_schema() -> Value {
    page_schema(100)
}

/// The arguments of `get_online_rule`, for the model.
pub(super) fn get_online_rule_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "rule_id": {"type": "string", "description": "The online rule id (UUID), as list_online_rules returns it."}
        },
        "required": ["rule_id"]
    })
}

/// The arguments of `list_dashboards`, for the model.
pub(super) fn list_dashboards_schema() -> Value {
    page_schema(100)
}

/// The arguments of `get_dashboard`, for the model.
pub(super) fn get_dashboard_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "dashboard_id": {"type": "string", "description": "The dashboard id (UUID), as list_dashboards returns it."}
        },
        "required": ["dashboard_id"]
    })
}

/// The arguments of `query_metrics`, for the model: the `WidgetQueryRequest` fields (D-AS-20).
pub(super) fn query_metrics_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "from": {"type": "string", "description": "Inclusive window start, RFC 3339 (for example 2026-01-01T00:00:00Z)."},
            "to": {"type": "string", "description": "Exclusive window end, RFC 3339. from and to are at most 30 days apart."},
            "bucket": {"type": "string", "enum": ["auto", "1m", "5m", "10m", "15m", "30m", "1h", "6h", "12h", "1d"], "description": "Bucket width of a series. Default auto. Ignored for total."},
            "filter": {"type": "string", "description": "Filter over the traces view, applied to every measure of the query."},
            "group_by": {"type": "string", "enum": ["model", "environment", "release"], "description": "Split the series by this field, unless the query has its own breakdown."},
            "include_eval": {"type": "boolean", "description": "Include traces from the evaluation environment. Default false."},
            "include_experiments": {"type": "boolean", "description": "Include experiment traces. Default false."},
            "query": {
                "type": "object",
                "additionalProperties": false,
                "description": "The source view and its measures, with an optional breakdown.",
                "properties": {
                    "source": {"type": "string", "enum": ["traces", "spans", "scores"], "description": "The view the measures read."},
                    "filter": {"type": "string", "description": "Filter over the source view."},
                    "measures": {
                        "type": "array",
                        "minItems": 1,
                        "maxItems": 6,
                        "description": "One to six measures.",
                        "items": {
                            "type": "object",
                            "additionalProperties": false,
                            "properties": {
                                "agg": {"type": "string", "enum": ["count", "count_distinct", "sum", "avg", "min", "max", "p50", "p75", "p90", "p95", "p99", "rate"], "description": "How the rows aggregate."},
                                "field": {"type": "string", "description": "The field aggregated. Absent for count and rate."},
                                "where": {"type": "string", "description": "A filter over the source view. Required for rate."},
                                "label": {"type": "string", "description": "Name shown for the measure, up to 60 characters."}
                            },
                            "required": ["agg"]
                        }
                    },
                    "breakdown": {
                        "type": "object",
                        "additionalProperties": false,
                        "description": "The split of the query by one field.",
                        "properties": {
                            "field": {"type": "string", "description": "The field the rows are grouped by."},
                            "limit": {"type": "integer", "minimum": 1, "maximum": 20, "description": "Groups kept. Default 5."},
                            "other": {"type": "boolean", "description": "Whether the other values form one more group. Default true."}
                        },
                        "required": ["field"]
                    }
                },
                "required": ["source", "measures"]
            },
            "shape": {"type": "string", "enum": ["series", "total"], "description": "series: one value per bucket. total: one value over the window."},
            "compare": {"type": "boolean", "description": "total only: also the value over the previous range of the same length. Default false."}
        },
        "required": ["from", "to", "query", "shape"]
    })
}

/// The schema of a list tool with `limit` (1 to `max`, default 20) and `cursor`.
fn page_schema(max: u32) -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "limit": {"type": "integer", "minimum": 1, "maximum": max, "description": "Items to return. Default 20."},
            "cursor": {"type": "string", "description": "next_cursor of the previous answer, to read the next page."}
        }
    })
}

/// The arguments of `list_sessions`, for the model.
pub(super) fn list_sessions_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "filter": {"type": "string", "description": "Filter expression over the sessions view (the same filter language as the UI's)."},
            "from": {"type": "string", "description": "Inclusive start of the window: RFC 3339 or epoch milliseconds."},
            "to": {"type": "string", "description": "Exclusive end of the window: RFC 3339 or epoch milliseconds."},
            "sort": {"type": "string", "enum": ["start_time", "end_time", "trace_count", "cost_total"], "description": "Sort key. Default start_time."},
            "order": {"type": "string", "enum": ["asc", "desc"], "description": "Sort direction. Default desc."},
            "limit": {"type": "integer", "minimum": 1, "maximum": 100, "description": "Items to return. Default 20."},
            "cursor": {"type": "string", "description": "next_cursor of the previous answer, to read the next page."},
            "include_eval": {"type": "boolean", "description": "Include traces from the evaluation environment. Default false."},
            "include_experiments": {"type": "boolean", "description": "Include experiment traces. Default false."}
        }
    })
}

/// The arguments of `get_session`, for the model.
pub(super) fn get_session_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "session_id": {"type": "string", "description": "The session id, as list_sessions returns it."},
            "include_eval": {"type": "boolean", "description": "Include traces from the evaluation environment. Default false."},
            "include_experiments": {"type": "boolean", "description": "Include experiment traces. Default false."}
        },
        "required": ["session_id"]
    })
}

/// The arguments of `list_scores`, for the model.
pub(super) fn list_scores_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "target": {"type": "string", "enum": ["span", "trace", "session"], "description": "Kind of target to return."},
            "trace_id": {"type": "string", "description": "Scores attached to this trace (32 hex characters)."},
            "span_id": {"type": "string", "description": "Scores attached to this span (16 hex characters)."},
            "session_id": {"type": "string", "description": "Scores attached to this session."},
            "name": {"type": "string", "description": "Score name."},
            "source": {"type": "string", "enum": ["api", "sdk", "human", "end_user", "llm_judge", "check", "webhook"], "description": "Who wrote the score."},
            "evaluator_id": {"type": "string", "description": "Scores written by this evaluator (UUID)."},
            "rule_id": {"type": "string", "description": "Scores written by this online rule (UUID)."},
            "from": {"type": "string", "description": "Inclusive created_at start: RFC 3339 or epoch milliseconds."},
            "to": {"type": "string", "description": "Exclusive created_at end: RFC 3339 or epoch milliseconds."},
            "limit": {"type": "integer", "minimum": 1, "maximum": 100, "description": "Items to return. Default 20."},
            "cursor": {"type": "string", "description": "next_cursor of the previous answer, to read the next page."}
        }
    })
}

/// The arguments of `list_score_definitions`, for the model.
pub(super) fn list_score_definitions_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "include_archived": {"type": "boolean", "description": "Include archived definitions. Default false."},
            "limit": {"type": "integer", "minimum": 1, "maximum": 100, "description": "Items to return. Default 20."},
            "cursor": {"type": "string", "description": "next_cursor of the previous answer, to read the next page."}
        }
    })
}

/// The arguments of `get_project_settings`, for the model: none.
pub(super) fn get_project_settings_schema() -> Value {
    json!({"type": "object", "additionalProperties": false, "properties": {}})
}

/// The arguments of `list_alert_rules`, for the model: none.
pub(super) fn list_alert_rules_schema() -> Value {
    json!({"type": "object", "additionalProperties": false, "properties": {}})
}

#[cfg(test)]
mod tests {
    use axum::http::{Method, StatusCode};
    use serde_json::{Map, Value, json};
    use uuid::Uuid;

    use super::super::{JsonObject, ToolResult, ToolScope, run};
    use crate::auth::principal::AuthUser;
    use crate::auth::roles::OrgRole;
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

    async fn call(app: &TestApp, scope: &ToolScope, name: &str, value: Value) -> ToolResult {
        run(&app.state, scope, name, arguments(value))
            .await
            .expect("a known tool")
    }

    async fn seeded_app() -> (TestApp, Seeded) {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        app.load_example_dataset(seeded.project.id).await;
        (app, seeded)
    }

    /// `GET` on a route with the seeded owner's cookie, as JSON.
    async fn get_route(app: &TestApp, seeded: &Seeded, path: &str) -> (StatusCode, Value) {
        let response = app
            .request(
                Method::GET,
                &format!("/api/v1/projects/{}{path}", seeded.project.id),
            )
            .header("cookie", &seeded.cookie)
            .send()
            .await;
        (response.status, response.json::<Value>())
    }

    /// `POST` on a route with the seeded owner's cookie.
    async fn post_route(app: &TestApp, seeded: &Seeded, path: &str, body: Value) -> Value {
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

    /// The largest string length anywhere in `value`.
    fn longest_string(value: &Value) -> usize {
        match value {
            Value::String(text) => text.chars().count(),
            Value::Array(items) => items.iter().map(longest_string).max().unwrap_or(0),
            Value::Object(map) => map.values().map(longest_string).max().unwrap_or(0),
            _ => 0,
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn list_sessions_returns_the_example_sessions() {
        let (app, seeded) = seeded_app().await;
        let result = call(
            &app,
            &scope_of(&seeded),
            "list_sessions",
            json!({"from": FROM, "to": TO}),
        )
        .await;
        assert!(result.ok, "{:?}", result.value);
        assert_eq!(result.rows, Some(2));
        let tool_ids: Vec<&str> = result.value["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|session| session["session_id"].as_str().unwrap())
            .collect();

        let (status, route) =
            get_route(&app, &seeded, &format!("/sessions?from={FROM}&to={TO}")).await;
        assert_eq!(status, StatusCode::OK, "{route}");
        let route_ids: Vec<&str> = route["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|session| session["session_id"].as_str().unwrap())
            .collect();
        assert_eq!(tool_ids, route_ids);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn list_sessions_rejects_a_bad_filter() {
        let (app, seeded) = seeded_app().await;
        let result = call(
            &app,
            &scope_of(&seeded),
            "list_sessions",
            json!({"filter": "nope = 1"}),
        )
        .await;
        assert!(!result.ok);
        assert_eq!(result.value["error"]["code"], "invalid_filter");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn get_session_keeps_the_route_truncated_flag() {
        let (app, seeded) = seeded_app().await;
        let result = call(
            &app,
            &scope_of(&seeded),
            "get_session",
            json!({"session_id": "sess-001"}),
        )
        .await;
        assert!(result.ok, "{:?}", result.value);
        let (status, route) = get_route(&app, &seeded, "/sessions/sess-001").await;
        assert_eq!(status, StatusCode::OK, "{route}");
        assert_eq!(result.value["truncated"], route["truncated"]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn get_session_cuts_payloads() {
        let (app, seeded) = seeded_app().await;
        let result = call(
            &app,
            &scope_of(&seeded),
            "get_session",
            json!({"session_id": "sess-001"}),
        )
        .await;
        assert!(result.ok, "{:?}", result.value);
        assert!(longest_string(&result.value) <= 200);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn get_session_unknown_is_not_found() {
        let (app, seeded) = seeded_app().await;
        let result = call(
            &app,
            &scope_of(&seeded),
            "get_session",
            json!({"session_id": "no-such-session"}),
        )
        .await;
        assert!(!result.ok);
        assert_eq!(result.value["error"]["code"], "not_found");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn list_scores_filters_by_name() {
        let (app, seeded) = seeded_app().await;
        let result = call(
            &app,
            &scope_of(&seeded),
            "list_scores",
            json!({"name": "resolved"}),
        )
        .await;
        assert!(result.ok, "{:?}", result.value);
        let scores = result.value["scores"].as_array().unwrap();
        assert!(!scores.is_empty());
        assert!(scores.iter().all(|score| score["name"] == "resolved"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn list_score_definitions_lists_live_ones() {
        let (app, seeded) = seeded_app().await;
        post_route(
            &app,
            &seeded,
            "/score-definitions",
            json!({"name": "relevance", "data_type": "numeric", "min_value": 0.0, "max_value": 1.0}),
        )
        .await;
        let tone = post_route(
            &app,
            &seeded,
            "/score-definitions",
            json!({
                "name": "tone",
                "data_type": "categorical",
                "categories": [{"label": "good", "value": 1.0}, {"label": "bad"}]
            }),
        )
        .await;
        let archived = response_id(&tone);
        let deleted = app
            .request(
                Method::DELETE,
                &format!(
                    "/api/v1/projects/{}/score-definitions/{archived}",
                    seeded.project.id
                ),
            )
            .header("cookie", &seeded.cookie)
            .send()
            .await;
        assert!(deleted.status.is_success());

        let live = call(
            &app,
            &scope_of(&seeded),
            "list_score_definitions",
            json!({}),
        )
        .await;
        assert!(live.ok, "{:?}", live.value);
        assert_eq!(live.value["definitions"].as_array().unwrap().len(), 1);
        assert_eq!(live.value["definitions"][0]["name"], "relevance");

        let all = call(
            &app,
            &scope_of(&seeded),
            "list_score_definitions",
            json!({"include_archived": true}),
        )
        .await;
        assert!(all.ok, "{:?}", all.value);
        assert_eq!(all.value["definitions"].as_array().unwrap().len(), 2);
    }

    /// The `id` of a created resource.
    fn response_id(body: &Value) -> String {
        body["id"].as_str().expect("a created id").to_owned()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn get_project_settings_returns_the_settings() {
        let (app, seeded) = seeded_app().await;
        let result = call(&app, &scope_of(&seeded), "get_project_settings", json!({})).await;
        assert!(result.ok, "{:?}", result.value);
        assert_eq!(result.value["name"], seeded.project.name);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn list_alert_rules_holds_no_channel_config() {
        const WEBHOOK_URL: &str = "https://hooks.example.com/x";
        const SECRET: &str = "0123456789abcdef";
        let (app, seeded) = seeded_app().await;
        let channel = post_route(
            &app,
            &seeded,
            "/channels",
            json!({
                "name": "Pager",
                "kind": "webhook",
                "url": WEBHOOK_URL,
                "secret": SECRET,
            }),
        )
        .await;
        let channel_id = response_id(&channel);
        post_route(
            &app,
            &seeded,
            "/alert-rules",
            json!({
                "name": "Error rate",
                "condition": {"metric": "error_rate", "window": "5m", "comparator": "gt", "threshold": 0.05},
                "channel_ids": [channel_id],
            }),
        )
        .await;

        let result = call(&app, &scope_of(&seeded), "list_alert_rules", json!({})).await;
        assert!(result.ok, "{:?}", result.value);
        let rules = result.value["rules"].as_array().unwrap();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0]["channel_ids"], json!([channel_id]));
        let text = result.value.to_string();
        assert!(!text.contains(WEBHOOK_URL), "{text}");
        assert!(!text.contains(SECRET), "{text}");
    }

    #[test]
    fn rows_count_the_items_the_model_receives() {
        let items: Vec<Value> = (0..1_000).map(|_| json!("a".repeat(100))).collect();
        let body = super::super::shape::list_body("rows", items, None, 200);
        let kept = body["rows"].as_array().unwrap().len();
        assert!(kept > 0 && kept < 1_000, "{kept}");
        assert_eq!(super::shaped_rows(&body, "rows"), Some(kept as u64));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn limit_is_bounded() {
        let (app, seeded) = seeded_app().await;
        for limit in [0, 101] {
            let result = call(
                &app,
                &scope_of(&seeded),
                "list_sessions",
                json!({"limit": limit}),
            )
            .await;
            assert!(!result.ok, "limit {limit}");
            assert_eq!(
                result.value["error"]["code"], "bad_request",
                "limit {limit}"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_project_argument_is_refused() {
        let (app, seeded) = seeded_app().await;
        let result = call(
            &app,
            &scope_of(&seeded),
            "list_sessions",
            json!({"project": "x"}),
        )
        .await;
        assert!(!result.ok);
        assert_eq!(result.value["error"]["code"], "bad_request");
        let result = call(
            &app,
            &scope_of(&seeded),
            "get_project_settings",
            json!({"project": "x"}),
        )
        .await;
        assert!(!result.ok);
        assert_eq!(result.value["error"]["code"], "bad_request");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn viewer_reads_the_same() {
        let (app, seeded) = seeded_app().await;
        let viewer = app
            .add_member(&seeded, "viewer@example.com", OrgRole::Viewer)
            .await;
        let scope = ToolScope {
            user: AuthUser {
                id: viewer.id,
                is_instance_admin: false,
                email: "viewer@example.com".to_owned(),
            },
            project: seeded.project.id,
        };
        for (name, value) in [
            ("list_sessions", json!({})),
            ("get_session", json!({"session_id": "sess-001"})),
            ("list_scores", json!({})),
            ("list_score_definitions", json!({})),
            ("get_project_settings", json!({})),
            ("list_alert_rules", json!({})),
        ] {
            let result = call(&app, &scope, name, value).await;
            assert!(result.ok, "{name}: {:?}", result.value);
        }
    }

    /// A check evaluator body, as `routes/evaluators.rs`'s tests create one.
    fn check_body(name: &str, needle: &str) -> Value {
        json!({
            "name": name,
            "kind": "check",
            "config": {"check": "contains", "value": needle},
            "output_score": {"name": "mentions_refund", "data_type": "boolean"}
        })
    }

    /// An online rule body over `evaluator`, as `routes/online_rules.rs`'s tests create one.
    fn rule_body(evaluator: &str) -> Value {
        json!({
            "name": "Relevance on answers",
            "evaluator_id": evaluator,
            "target": "trace",
            "filter": "status = 'ok'",
            "variable_mapping": {"input": "trace.input", "output": "trace.output"},
            "sample_rate": 0.25,
            "budget_usd": 5.0,
            "budget_period": "day",
            "enabled": true
        })
    }

    /// A dashboard body with one chart, as `routes/dashboards.rs`'s tests create one.
    fn dashboard_body(name: &str) -> Value {
        json!({
            "name": name,
            "definition": {"widgets": [{
                "id": "requests",
                "title": "Requests",
                "layout": {"x": 0, "y": 0, "w": 6, "h": 4},
                "kind": "chart",
                "chart": {"type": "line"},
                "query": {"source": "traces", "measures": [{"agg": "count", "label": "Requests"}]}
            }]}
        })
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn list_evaluators_summarizes_stats() {
        let (app, seeded) = seeded_app().await;
        let empty = call(&app, &scope_of(&seeded), "list_evaluators", json!({})).await;
        assert!(empty.ok, "{:?}", empty.value);
        assert_eq!(empty.value["evaluators"], json!([]));
        assert_eq!(empty.rows, Some(0));

        post_route(
            &app,
            &seeded,
            "/evaluators",
            check_body("Refund mention", "refund"),
        )
        .await;
        let result = call(&app, &scope_of(&seeded), "list_evaluators", json!({})).await;
        assert!(result.ok, "{:?}", result.value);
        assert_eq!(result.rows, Some(1));
        let evaluators = result.value["evaluators"].as_array().unwrap();
        assert_eq!(evaluators[0]["name"], "Refund mention");
        assert!(evaluators[0]["stats"].is_object());
        assert!(evaluators[0]["stats"]["jobs"].get("runs_daily").is_none());
        assert!(evaluators[0]["stats"]["scores"].get("daily").is_none());

        let big = call(
            &app,
            &scope_of(&seeded),
            "list_evaluators",
            json!({"limit": 51}),
        )
        .await;
        assert!(!big.ok);
        assert_eq!(big.value["error"]["code"], "bad_request");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn get_evaluator_keeps_long_config() {
        let (app, seeded) = seeded_app().await;
        let needle = "x".repeat(1_000);
        let created = post_route(
            &app,
            &seeded,
            "/evaluators",
            check_body("Long needle", &needle),
        )
        .await;
        let result = call(
            &app,
            &scope_of(&seeded),
            "get_evaluator",
            json!({"evaluator_id": response_id(&created)}),
        )
        .await;
        assert!(result.ok, "{:?}", result.value);
        assert_eq!(result.value["evaluator"]["id"], created["id"]);
        assert!(result.value["stats"].is_object());
        assert!(result.value.to_string().contains(&needle));

        let missing = call(
            &app,
            &scope_of(&seeded),
            "get_evaluator",
            json!({"evaluator_id": "not-a-uuid"}),
        )
        .await;
        assert!(!missing.ok);
        assert_eq!(missing.value["error"]["code"], "not_found");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn get_online_rule_and_list() {
        let (app, seeded) = seeded_app().await;
        let evaluator = post_route(
            &app,
            &seeded,
            "/evaluators",
            check_body("Refund mention", "refund"),
        )
        .await;
        // The evaluator is a check, which takes no budget: the rule goes without one.
        let mut body = rule_body(&response_id(&evaluator));
        body.as_object_mut().unwrap().remove("budget_usd");
        body.as_object_mut().unwrap().remove("budget_period");
        let rule = post_route(&app, &seeded, "/online-rules", body).await;
        let rule_id = response_id(&rule);

        let listed = call(&app, &scope_of(&seeded), "list_online_rules", json!({})).await;
        assert!(listed.ok, "{:?}", listed.value);
        let rules = listed.value["rules"].as_array().unwrap();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0]["id"], rule_id);
        assert!(rules[0]["stats"].is_object());
        assert!(rules[0]["stats"].get("jobs_hourly").is_none());
        assert!(rules[0]["stats"]["scores"].get("hourly").is_none());

        let result = call(
            &app,
            &scope_of(&seeded),
            "get_online_rule",
            json!({"rule_id": rule_id}),
        )
        .await;
        assert!(result.ok, "{:?}", result.value);
        assert_eq!(result.value["rule"]["id"], rule_id);
        assert_eq!(result.value["stats"]["rule_id"], rule_id);
        assert!(result.value["stats"]["jobs_hourly"].is_array());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn get_dashboard_returns_the_version() {
        let (app, seeded) = seeded_app().await;
        let created = post_route(&app, &seeded, "/dashboards", dashboard_body("Traffic")).await;
        let listed = call(&app, &scope_of(&seeded), "list_dashboards", json!({})).await;
        assert!(listed.ok, "{:?}", listed.value);
        assert_eq!(listed.value["dashboards"].as_array().unwrap().len(), 1);

        let result = call(
            &app,
            &scope_of(&seeded),
            "get_dashboard",
            json!({"dashboard_id": response_id(&created)}),
        )
        .await;
        assert!(result.ok, "{:?}", result.value);
        assert_eq!(result.value["id"], created["id"]);
        assert_eq!(result.value["version"], created["version"]);
        assert!(result.value["definition"]["widgets"].is_array());

        let unknown = call(
            &app,
            &scope_of(&seeded),
            "get_dashboard",
            json!({"dashboard_id": Uuid::now_v7().to_string()}),
        )
        .await;
        assert!(!unknown.ok);
        assert_eq!(unknown.value["error"]["code"], "not_found");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn query_metrics_counts_traces() {
        let (app, seeded) = seeded_app().await;
        let body = json!({
            "from": FROM,
            "to": TO,
            "bucket": "auto",
            "query": {"source": "traces", "measures": [{"agg": "count"}]},
            "shape": "total"
        });
        let result = call(&app, &scope_of(&seeded), "query_metrics", body.clone()).await;
        assert!(result.ok, "{:?}", result.value);
        let rows = result.value["rows"].as_array().unwrap();
        assert_eq!(rows.len(), 1);
        let count = rows[0]["values"][0].as_f64().expect("a count");
        // The example holds five traces in the window (the dashboard route's own test).
        assert!(count >= 5.0, "{count}");

        // The dashboard query route answers the same body with the same value.
        let response = app
            .request(
                Method::POST,
                &format!("/api/v1/projects/{}/dashboards/query", seeded.project.id),
            )
            .header("cookie", &seeded.cookie)
            .json(&body)
            .send()
            .await;
        assert_eq!(response.status, StatusCode::OK);
        let route = response.json::<Value>();
        assert_eq!(route["rows"][0]["values"][0], rows[0]["values"][0]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn query_metrics_requires_a_window() {
        let (app, seeded) = seeded_app().await;
        let query = json!({"source": "traces", "measures": [{"agg": "count"}]});
        let cases = [
            json!({"from": FROM, "query": query, "shape": "total"}),
            json!({"from": FROM, "to": "2026-02-01T00:00:00Z", "query": query, "shape": "total"}),
            json!({"from": "1767225600000", "to": "1767312000000", "query": query, "shape": "total"}),
        ];
        for case in cases {
            let result = call(&app, &scope_of(&seeded), "query_metrics", case.clone()).await;
            assert!(!result.ok, "{case}");
            assert_eq!(result.value["error"]["code"], "bad_request", "{case}");
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn query_metrics_bad_filter_is_invalid_filter() {
        let (app, seeded) = seeded_app().await;
        let result = call(
            &app,
            &scope_of(&seeded),
            "query_metrics",
            json!({
                "from": FROM,
                "to": TO,
                "filter": "nope = 1",
                "query": {"source": "traces", "measures": [{"agg": "count"}]},
                "shape": "total"
            }),
        )
        .await;
        assert!(!result.ok);
        assert_eq!(result.value["error"]["code"], "invalid_filter");
    }
}
