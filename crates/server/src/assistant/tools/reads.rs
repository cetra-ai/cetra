//! The assistant's read tools over sessions, scores, the project and alert rules (track AS M2, D-AS-5, D-AS-19,
//! D-AS-20). Each tool reads the scope's project with `ProjectPerm::View` and answers through the function its
//! route serves, so a tool shows exactly what the route shows.

use cetra_core::enums::{ScoreSource, ScoreTargetKind};
use cetra_core::ids::{SpanId, TraceId};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use super::shape::{self, VALUE_CUT};
use super::{JsonObject, ToolError, ToolScope};
use crate::api::common::{ExclusionParams, Limit, SortOrder, TimeParam};
use crate::api::score_definitions::ScoreDefinitionListParams;
use crate::api::scores::ScoreListParams;
use crate::api::sessions::{SessionListParams, SessionSort};
use crate::auth::roles::ProjectPerm;
use crate::http::AppState;
use crate::http::access::{ProjectAccess, user_project};
use crate::http::error::ApiError;
use crate::mcp::tools as mcp_tools;
use crate::routes::alert_rules::alert_rule_page;
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

/// `limit` when the model omits it (D-AS-20).
const DEFAULT_LIMIT: u32 = 20;
/// The message for a `limit` outside 1 to 100 (D-AS-20).
const LIMIT_MESSAGE: &str = "limit must be between 1 and 100";

/// The model's `limit`, as the routes' `Limit`: 1 to 100, 20 by default (D-AS-20).
fn list_limit(given: Option<u32>) -> Result<Limit, ApiError> {
    let value = given.unwrap_or(DEFAULT_LIMIT);
    if !(1..=100).contains(&value) {
        return Err(bad_limit());
    }
    serde_json::from_value(json!(value)).map_err(|_| bad_limit())
}

fn bad_limit() -> ApiError {
    ApiError::BadRequest {
        detail: Some(LIMIT_MESSAGE.to_owned()),
        errors: Vec::new(),
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

/// The item count of a list answer.
fn count(len: usize) -> Option<u64> {
    u64::try_from(len).ok()
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
    let rows = count(page.data.len());
    let value = shape::list_body("sessions", items(&page.data)?, page.next_cursor, VALUE_CUT);
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
        from: args.from,
        to: args.to,
        limit: list_limit(args.limit)?,
        cursor: args.cursor,
    };
    let access = project_access(state, scope).await?;
    let page = score_page(state, &access, params).await?;
    let rows = count(page.data.len());
    let value = shape::list_body("scores", items(&page.data)?, page.next_cursor, VALUE_CUT);
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
    let rows = count(page.data.len());
    let value = shape::list_body(
        "definitions",
        items(&page.data)?,
        page.next_cursor,
        VALUE_CUT,
    );
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
    let rows = count(page.data.len());
    let value = shape::list_body("rules", items(&page.data)?, page.next_cursor, VALUE_CUT);
    Ok((value, rows))
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
}
