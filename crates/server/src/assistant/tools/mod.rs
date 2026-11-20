//! The assistant's tools (track AS M2, D-AS-5, D-AS-6).
#![cfg_attr(not(test), allow(dead_code))] // used by the turn loop (M3); M3 removes this line

mod shape;

use std::fmt;

use cetra_core::ids::ProjectId;
use serde_json::{Map, Value, json};

use crate::api::assistant::AssistantDraftKind;
use crate::auth::principal::AuthUser;
use crate::http::AppState;
use crate::mcp::output;
use crate::mcp::sql;
use crate::mcp::tools as mcp_tools;
use crate::mcp::traces;

/// A JSON object of tool arguments, as the model sends them.
pub(crate) type JsonObject = Map<String, Value>;

/// One tool as the model sees it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ToolSpec {
    pub(crate) name: &'static str,
    pub(crate) description: &'static str,
    /// A JSON schema object with no `project` property.
    pub(crate) parameters: Value,
}

/// Who calls and where.
#[derive(Debug, Clone)]
pub(crate) struct ToolScope {
    pub(crate) user: AuthUser,
    pub(crate) project: ProjectId,
}

/// A tool's answer.
#[derive(Clone, PartialEq)]
pub(crate) struct ToolResult {
    /// False when the tool failed; `value` is then `{"error": {"code", "message", "position"?}}`.
    pub(crate) ok: bool,
    /// What the model reads.
    pub(crate) value: Value,
    /// Rows or items returned, when the tool returns a list or rows.
    pub(crate) rows: Option<u64>,
    /// A validated draft (draft tools only).
    pub(crate) draft: Option<(AssistantDraftKind, Value)>,
}

/// Hides the value: it is user data (`AGENTS.md` § Privacy).
impl fmt::Debug for ToolResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ToolResult")
            .field("ok", &self.ok)
            .field("rows", &self.rows)
            .field("draft", &self.draft.as_ref().map(|(kind, _)| kind))
            .field("value_len", &output::char_len(&output::render(&self.value)))
            .finish()
    }
}

/// The model named a tool that does not exist.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown tool")]
pub(crate) struct UnknownTool;

/// The argument the model cannot set: every tool result is fitted by the server (D-AS-20).
const MAX_CHARS_ARG: &str = "max_chars";

/// The six MCP tools' description and schema, without the arguments the server sets.
fn spec(name: &'static str, description: &'static str, schema: Value) -> ToolSpec {
    ToolSpec {
        name,
        description,
        parameters: model_schema(schema),
    }
}

/// Removes `project` and `max_chars` from a tool's schema: the server sets both (D-AS-5, D-AS-20).
fn model_schema(mut schema: Value) -> Value {
    if let Some(object) = schema.as_object_mut() {
        if let Some(Value::Object(properties)) = object.get_mut("properties") {
            properties.remove("project");
            properties.remove(MAX_CHARS_ARG);
        }
        let empty = if let Some(Value::Array(required)) = object.get_mut("required") {
            required.retain(|name| !matches!(name.as_str(), Some("project" | "max_chars")));
            required.is_empty()
        } else {
            false
        };
        if empty {
            object.remove("required");
        }
    }
    schema
}

/// Every tool, in a fixed order.
pub(crate) fn specs() -> Vec<ToolSpec> {
    vec![
        spec(
            mcp_tools::GET_SCHEMA,
            mcp_tools::GET_SCHEMA_DESCRIPTION,
            mcp_tools::get_schema_schema(),
        ),
        spec(
            sql::QUERY_SQL,
            mcp_tools::QUERY_SQL_DESCRIPTION,
            mcp_tools::query_sql_schema(),
        ),
        spec(
            sql::LIST_SAVED_VIEWS,
            mcp_tools::LIST_SAVED_VIEWS_DESCRIPTION,
            mcp_tools::list_saved_views_schema(),
        ),
        spec(
            traces::SEARCH_TRACES,
            mcp_tools::SEARCH_TRACES_DESCRIPTION,
            mcp_tools::search_traces_schema(),
        ),
        spec(
            traces::GET_TRACE,
            mcp_tools::GET_TRACE_DESCRIPTION,
            mcp_tools::get_trace_schema(),
        ),
        spec(
            traces::GET_SPAN,
            mcp_tools::GET_SPAN_DESCRIPTION,
            mcp_tools::get_span_schema(),
        ),
    ]
}

/// The number of elements of the array `value[key]`, when it is one.
fn list_len(value: &Value, key: &str) -> Option<u64> {
    value[key]
        .as_array()
        .and_then(|items| u64::try_from(items.len()).ok())
}

/// Runs `name` with `arguments` for `scope`. The scope's project replaces any `project` the model
/// sent; the four tools that return data get `max_chars` set to the server's budget.
pub(crate) async fn run(
    state: &AppState,
    scope: &ToolScope,
    name: &str,
    mut arguments: JsonObject,
) -> Result<ToolResult, UnknownTool> {
    arguments.insert(
        "project".to_owned(),
        Value::String(scope.project.to_string()),
    );
    let user = &scope.user;
    let (outcome, rows) = match name {
        mcp_tools::GET_SCHEMA => (mcp_tools::get_schema(state, user, arguments).await, None),
        sql::QUERY_SQL => {
            arguments.insert(MAX_CHARS_ARG.to_owned(), json!(shape::MAX_CHARS));
            let outcome = sql::query_sql(state, user, arguments).await;
            let rows = outcome
                .as_ref()
                .ok()
                .and_then(|value| value["row_count"].as_u64());
            (outcome, rows)
        }
        sql::LIST_SAVED_VIEWS => {
            let outcome = sql::list_saved_views(state, user, arguments).await;
            let rows = outcome
                .as_ref()
                .ok()
                .and_then(|value| list_len(value, "views"));
            (outcome, rows)
        }
        traces::SEARCH_TRACES => {
            arguments.insert(MAX_CHARS_ARG.to_owned(), json!(shape::MAX_CHARS));
            let outcome = traces::search_traces(state, user, arguments).await;
            let rows = outcome
                .as_ref()
                .ok()
                .and_then(|value| list_len(value, "traces"));
            (outcome, rows)
        }
        traces::GET_TRACE => {
            arguments.insert(MAX_CHARS_ARG.to_owned(), json!(shape::MAX_CHARS));
            let outcome = traces::get_trace(state, user, arguments).await;
            let rows = outcome
                .as_ref()
                .ok()
                .and_then(|value| list_len(value, "spans"));
            (outcome, rows)
        }
        traces::GET_SPAN => {
            arguments.insert(MAX_CHARS_ARG.to_owned(), json!(shape::MAX_CHARS));
            (traces::get_span(state, user, arguments).await, None)
        }
        _ => return Err(UnknownTool),
    };
    Ok(match outcome {
        Ok(value) => ToolResult {
            ok: true,
            value,
            rows,
            draft: None,
        },
        Err(error) => ToolResult {
            ok: false,
            value: output::failure_body(&error),
            rows: None,
            draft: None,
        },
    })
}

#[cfg(test)]
mod tests {
    use serde_json::{Map, Value, json};
    use uuid::Uuid;

    use super::{JsonObject, ToolResult, ToolScope, UnknownTool, run, specs};
    use crate::auth::principal::AuthUser;
    use crate::mcp::output::{char_len, render};
    use crate::mcp::test_helpers::{FROM, TO};
    use crate::test_support::{Seeded, TestApp, second_org};

    /// The scope of the seeded user (an org owner, not an instance admin) on the seeded project.
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

    async fn seeded_app() -> (TestApp, Seeded) {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        app.load_example_dataset(seeded.project.id).await;
        (app, seeded)
    }

    #[test]
    fn specs_have_no_project_argument() {
        let specs = specs();
        let names: Vec<&str> = specs.iter().map(|spec| spec.name).collect();
        assert_eq!(
            names,
            [
                "get_schema",
                "query_sql",
                "list_saved_views",
                "search_traces",
                "get_trace",
                "get_span",
            ]
        );
        for spec in &specs {
            let properties = spec.parameters["properties"].as_object().unwrap();
            assert!(!properties.contains_key("project"), "{}", spec.name);
            assert!(!properties.contains_key("max_chars"), "{}", spec.name);
            let required = spec.parameters.get("required").map(render);
            if let Some(required) = required {
                assert!(!required.contains("\"project\""), "{}", spec.name);
                assert!(!required.contains("\"max_chars\""), "{}", spec.name);
            }
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn query_sql_runs_in_the_scope_project() {
        let (app, seeded) = seeded_app().await;
        let result = run(
            &app.state,
            &scope_of(&seeded),
            "query_sql",
            arguments(json!({
                "query": "SELECT count(*) AS n FROM traces",
                "from": FROM,
                "to": TO,
            })),
        )
        .await
        .unwrap();
        assert!(result.ok);
        assert_eq!(result.rows, Some(1));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_project_argument_from_the_model_is_overwritten() {
        let (app, seeded) = seeded_app().await;
        let result = run(
            &app.state,
            &scope_of(&seeded),
            "query_sql",
            arguments(json!({
                "project": Uuid::now_v7().to_string(),
                "query": "SELECT count(*) AS n FROM traces",
                "from": FROM,
                "to": TO,
            })),
        )
        .await
        .unwrap();
        assert!(result.ok);
        assert_eq!(result.rows, Some(1));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_user_without_access_gets_not_found() {
        let (app, seeded) = seeded_app().await;
        let (_, member) = second_org(&app, "other", "owner@example.com").await;
        let scope = ToolScope {
            user: AuthUser {
                id: member.id,
                is_instance_admin: false,
                email: "owner@example.com".to_owned(),
            },
            project: seeded.project.id,
        };
        let result = run(
            &app.state,
            &scope,
            "query_sql",
            arguments(json!({"query": "SELECT 1 AS one", "from": FROM, "to": TO})),
        )
        .await
        .unwrap();
        assert!(!result.ok);
        assert_eq!(result.value["error"]["code"], "not_found");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn errors_carry_the_problem_code() {
        let (app, seeded) = seeded_app().await;
        let result = run(
            &app.state,
            &scope_of(&seeded),
            "query_sql",
            arguments(json!({"query": "SELECT nope FROM traces", "from": FROM, "to": TO})),
        )
        .await
        .unwrap();
        assert!(!result.ok);
        assert_eq!(result.rows, None);
        assert_eq!(result.value["error"]["code"], "invalid_filter");
        assert!(result.value["error"]["position"].is_object());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_model_cannot_raise_max_chars() {
        let (app, seeded) = seeded_app().await;
        let scope = scope_of(&seeded);
        let found = run(
            &app.state,
            &scope,
            "search_traces",
            arguments(json!({"from": FROM, "to": TO})),
        )
        .await
        .unwrap();
        assert!(found.ok);
        let trace_id = found.value["traces"][0]["trace_id"]
            .as_str()
            .expect("the example holds traces")
            .to_owned();
        let result = run(
            &app.state,
            &scope,
            "get_trace",
            arguments(json!({"trace_id": trace_id, "max_chars": 100_000})),
        )
        .await
        .unwrap();
        assert!(result.ok);
        assert!(char_len(&render(&result.value)) <= 20_000);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn unknown_tool_is_an_error() {
        let (app, seeded) = seeded_app().await;
        let outcome = run(
            &app.state,
            &scope_of(&seeded),
            "drop_everything",
            JsonObject::new(),
        )
        .await;
        assert_eq!(outcome, Err(UnknownTool));
    }

    #[test]
    fn tool_result_debug_hides_the_value() {
        let result = ToolResult {
            ok: true,
            value: json!({"secret": "needle-value"}),
            rows: Some(2),
            draft: None,
        };
        let debug = format!("{result:?}");
        assert!(!debug.contains("needle-value"), "{debug}");
        assert!(debug.contains("value_len"), "{debug}");
    }
}
