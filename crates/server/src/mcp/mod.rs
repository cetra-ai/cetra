//! The MCP server at `POST /mcp` (spec 09 § MCP server, D-H-2, D-H-3, D-H-10, D-H-11).
//!
//! Streamable HTTP through `rmcp`, stateless, with JSON responses: every POST is served on its
//! own by a fresh [`McpServer`] that holds the caller. Only personal access tokens reach it.

mod issues;
pub(crate) mod output;
mod prompts;
pub(crate) mod sql;
pub(crate) mod tools;
pub(crate) mod traces;

use std::sync::Arc;
use std::time::Instant;

use axum::body::Body;
use axum::extract::{Extension, Request, State};
use axum::http::Method;
use axum::response::Response;
use axum::routing::post;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, Implementation, JsonObject, ListToolsResult,
    PaginatedRequestParams, ServerCapabilities, ServerConfig,
};
use rmcp::service::RequestContext;
use rmcp::transport::streamable_http_server::session::never::NeverSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{ErrorData as McpError, RoleServer, ServerHandler};
use serde_json::Value;
use tracing::Instrument as _;

use crate::analytics::actor::{Actor, Via};
use crate::analytics::catalog::Event;
use crate::analytics::record;
use crate::api::common::ProblemCode;
use crate::auth::principal::{AuthUser, Principal};
use crate::http::AppState;
use crate::http::access::{Access, RouteDef};
use crate::http::error::ApiError;
use crate::observability::spans;

/// Largest JSON-RPC body rmcp reads (it answers 413 above this).
const MCP_BODY_LIMIT: usize = 1024 * 1024;

/// Instructions sent with `initialize`.
const INSTRUCTIONS: &str = "Read-only access to LLM traces, spans and prompts, and to the issues found in them. Call list_projects first: the other tools take a `project`. Each tool's description says what it needs. Results are compact JSON; when a result is cut, `truncated` is true and `notes` says what was left out.";

/// `POST /mcp`. Not in the OpenAPI document.
pub(crate) fn routes() -> Vec<RouteDef> {
    vec![RouteDef {
        method: Method::POST,
        path: "/mcp",
        access: Access::Pat,
        documented: false,
        handler: post(mcp),
    }]
}

/// Admits the token (D-H-10), then hands the request to rmcp with a server bound to the caller.
async fn mcp(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    request: Request,
) -> Result<Response, ApiError> {
    // The access layer admits only a PAT here (`Access::Pat`).
    let Principal::Pat { user, token_id, .. } = principal else {
        return Err(ApiError::Forbidden {
            code: ProblemCode::Forbidden,
        });
    };
    let _guard = state
        .limits
        .mcp
        .admit(token_id, state.clock.now_ns())
        .map_err(|retry| ApiError::RateLimited {
            retry_after_s: retry.0.as_secs().max(1),
        })?;
    let server = McpServer {
        state,
        user,
        request_span: tracing::Span::current(),
        opted_out: crate::analytics::actor::request_opted_out(),
    };
    let service = StreamableHttpService::new(
        move || Ok(server.clone()),
        Arc::new(NeverSessionManager::default()),
        service_config(),
    );
    Ok(service.handle(request).await.map(Body::new))
}

fn service_config() -> StreamableHttpServerConfig {
    StreamableHttpServerConfig::default()
        .with_legacy_session_mode(false)
        .with_json_response(true)
        .with_sse_keep_alive(None)
        .with_sse_retry(None)
        .disable_allowed_hosts()
        .disable_allowed_origins()
        .with_max_request_body_bytes(MCP_BODY_LIMIT)
}

/// One request's MCP server: the app and the token's user.
#[derive(Clone)]
pub(crate) struct McpServer {
    state: AppState,
    user: AuthUser,
    /// The request's `http.request` span: rmcp runs a tool call in a spawned task, so the call's span names it as its parent explicitly.
    request_span: tracing::Span,
    /// The request opted out of per-user analytics (captured here: rmcp runs tools in spawned tasks, outside the request's scope).
    opted_out: bool,
}

impl ServerHandler for McpServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                cetra_core::names::PRODUCT,
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(INSTRUCTIONS)
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        Ok(ListToolsResult::with_all_items(tools::tool_list()))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        let started = Instant::now();
        let span = spans::query(&self.request_span, spans::QUERY_MCP);
        let arguments = request.arguments.unwrap_or_default();
        // rmcp runs this in a spawned task and cancels `context.ct` when the client disconnects,
        // while the concurrency guard in the axum handler is already
        // released. Racing the tool against the token drops its query instead of letting it run
        // to completion for a caller that is gone (DataFusion cancels on drop). `biased` with
        // the tool first: a tool that already finished is never thrown away. In stateless mode
        // rmcp 3.10 ignores `notifications/cancelled`, so a cancel without a disconnect runs
        // until the tool's own time limit.
        let (tool, outcome) = tokio::select! {
            biased;
            result = self.run_tool(&request.name, arguments).instrument(span.clone()) => match result {
                Ok(pair) => pair,
                Err(error) => {
                    span.record(spans::OUTCOME, "unknown_tool");
                    return Err(error);
                }
            },
            () = context.ct.cancelled() => {
                span.record(spans::OUTCOME, "cancelled");
                let duration_ms =
                    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
                tracing::info!(target: "mcp", tool = %request.name, user_id = %self.user.id,
                               cancelled = true, duration_ms, "tool call");
                return Err(McpError::internal_error(
                    "tool call cancelled: the client went away",
                    None,
                ));
            }
        };
        span.record(spans::TOOL, tool);
        let ok = outcome.is_ok();
        let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let result = match outcome {
            Ok(body) => {
                span.record(spans::OUTCOME, "ok");
                tracing::info!(target: "mcp", tool, user_id = %self.user.id, status = 200u16, duration_ms, "tool call");
                output::success(&body)
            }
            Err(error) => {
                span.record(spans::OUTCOME, "error");
                let status = error.problem().status;
                tracing::info!(target: "mcp", tool, user_id = %self.user.id, status, duration_ms, "tool call");
                output::failure(&error)
            }
        };
        let actor = Actor {
            opted_out: self.opted_out,
            ..Actor::user(self.user.id, Via::Pat)
        };
        for event in tool_events(tool, ok) {
            record(&self.state, &event, &actor);
        }
        Ok(result.into())
    }
}

impl McpServer {
    /// The tool named `name` run with `arguments`: its name and outcome. `Err` only for an
    /// unknown tool (a protocol error, not a tool error).
    async fn run_tool(
        &self,
        name: &str,
        arguments: JsonObject,
    ) -> Result<(&'static str, Result<Value, ApiError>), McpError> {
        let pair = match name {
            tools::LIST_PROJECTS => (
                tools::LIST_PROJECTS,
                tools::list_projects(&self.state, &self.user, arguments).await,
            ),
            tools::GET_SCHEMA => (
                tools::GET_SCHEMA,
                tools::get_schema(&self.state, &self.user, arguments).await,
            ),
            sql::QUERY_SQL => (
                sql::QUERY_SQL,
                sql::query_sql(&self.state, &self.user, arguments).await,
            ),
            sql::LIST_SAVED_VIEWS => (
                sql::LIST_SAVED_VIEWS,
                sql::list_saved_views(&self.state, &self.user, arguments).await,
            ),
            prompts::LIST_PROMPTS => (
                prompts::LIST_PROMPTS,
                prompts::list_prompts(&self.state, &self.user, arguments).await,
            ),
            prompts::GET_PROMPT => (
                prompts::GET_PROMPT,
                prompts::get_prompt(&self.state, &self.user, arguments).await,
            ),
            traces::SEARCH_TRACES => (
                traces::SEARCH_TRACES,
                traces::search_traces(&self.state, &self.user, arguments).await,
            ),
            traces::GET_TRACE => (
                traces::GET_TRACE,
                traces::get_trace(&self.state, &self.user, arguments).await,
            ),
            traces::GET_SPAN => (
                traces::GET_SPAN,
                traces::get_span(&self.state, &self.user, arguments).await,
            ),
            issues::LIST_ISSUES => (
                issues::LIST_ISSUES,
                issues::list_issues(&self.state, &self.user, arguments).await,
            ),
            _ => return Err(McpError::invalid_params("unknown tool", None)),
        };
        Ok(pair)
    }
}

/// The catalog events of one finished tool call: `mcp.tool_called`, plus the event of the
/// same action in the UI or API for the tools that have one (`query_sql` is a SQL console
/// run, `get_trace` a trace read), so those count whichever client ran them.
fn tool_events(tool: &'static str, ok: bool) -> Vec<Event> {
    let mut events = vec![Event::McpToolCalled { tool, ok }];
    if ok && tool == sql::QUERY_SQL {
        events.push(Event::SqlConsoleRun);
    }
    if ok && tool == traces::GET_TRACE {
        events.push(Event::TraceViewed);
    }
    events
}

#[cfg(test)]
pub(crate) mod test_helpers {
    use std::future::Future;

    use axum::http::{Method, StatusCode};
    use serde_json::{Value, json};

    use crate::test_support::{Seeded, TestApp, TestResponse};

    pub(crate) const V2025: &str = "2025-11-25";
    pub(crate) const V2026: &str = "2026-07-28";

    /// A window covering the example dataset: all of 2026-01-01, UTC.
    pub(crate) const FROM: &str = "2026-01-01T00:00:00Z";
    pub(crate) const TO: &str = "2026-01-02T00:00:00Z";

    /// `POST /mcp` with `host` and `accept` set as every MCP client sends them, the optional
    /// bearer, an optional `mcp-protocol-version`, and `extra` headers.
    pub(crate) async fn post_mcp(
        app: &TestApp,
        bearer: Option<&str>,
        version: Option<&str>,
        extra: &[(&str, &str)],
        body: &Value,
    ) -> TestResponse {
        let mut request = app
            .request(Method::POST, "/mcp")
            .header("host", "127.0.0.1:8080")
            .header("accept", "application/json, text/event-stream")
            .json(body);
        if let Some(version) = version {
            request = request.header("mcp-protocol-version", version);
        }
        if let Some(bearer) = bearer {
            request = request.header("authorization", &format!("Bearer {bearer}"));
        }
        for (name, value) in extra {
            request = request.header(name, value);
        }
        request.send().await
    }

    /// [`post_mcp`] with protocol 2025-11-25 and no extra header.
    pub(crate) async fn rpc(app: &TestApp, bearer: Option<&str>, body: &Value) -> TestResponse {
        post_mcp(app, bearer, Some(V2025), &[], body).await
    }

    /// `tools/call` of `name`; returns `(is_error, parsed text content)`.
    /// Panics on an HTTP status other than 200 or a JSON-RPC error.
    pub(crate) async fn call(
        app: &TestApp,
        token: &str,
        name: &str,
        arguments: &Value,
    ) -> (bool, Value) {
        let response = rpc(
            app,
            Some(token),
            &json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                    "params": {"name": name, "arguments": arguments}}),
        )
        .await;
        assert_eq!(response.status, StatusCode::OK);
        let body: Value = response.json();
        let result = &body["result"];
        let text = result["content"][0]["text"].as_str().expect("text content");
        (
            result["isError"] == json!(true),
            serde_json::from_str(text).unwrap(),
        )
    }

    /// Runs `future` under its own subscriber that keeps what production keeps: `info` and
    /// above, `rmcp` at `warn` (the directive `cetra serve` adds, step 11). Returns the
    /// formatted output. Use it, not `test_support::capture_logs` (which captures every level,
    /// rmcp's debug dumps included), for MCP log tests, on a current-thread runtime.
    pub(crate) async fn capture_info_logs<T>(future: impl Future<Output = T>) -> (T, String) {
        use tracing_subscriber::Layer;
        use tracing_subscriber::layer::SubscriberExt;
        crate::test_support::ensure_global_subscriber();
        let buffer = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
        let writer = std::sync::Arc::clone(&buffer);
        let filter = tracing_subscriber::filter::Targets::new()
            .with_default(tracing::Level::INFO)
            .with_target("rmcp", tracing::Level::WARN);
        let layer = tracing_subscriber::fmt::layer()
            .with_writer(move || Buffer(std::sync::Arc::clone(&writer)))
            .with_ansi(false)
            .with_target(true)
            .with_filter(filter);
        let dispatch = tracing::Dispatch::new(tracing_subscriber::registry().with(layer));
        let value = {
            let _guard = tracing::dispatcher::set_default(&dispatch);
            future.await
        };
        drop(dispatch);
        let bytes = buffer.lock().expect("log buffer").clone();
        (value, String::from_utf8_lossy(&bytes).into_owned())
    }

    /// `io::Write` over the shared buffer of [`capture_info_logs`].
    struct Buffer(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for Buffer {
        fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("log buffer").write(data)
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// A seeded app and a read-only PAT of ada (org owner, so project admin).
    pub(crate) async fn app_with_token() -> (TestApp, Seeded, String) {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let token = app.personal_token(seeded.user_id, false).await;
        (app, seeded, token)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use axum::http::{Method, StatusCode};
    use serde_json::{Value, json};
    use uuid::Uuid;

    use super::test_helpers::*;
    use super::{INSTRUCTIONS, output};
    use super::{sql, tool_events, tools, traces};
    use crate::api::convert;
    use crate::auth::roles::{KeyScope, OrgRole, ProjectOverride};
    use crate::auth::secret::{self, KeyKind};
    use crate::meta::members::{add_org_member, set_project_override};
    use crate::meta::orgs::insert_org;
    use crate::meta::personal_tokens::{self, NewToken};
    use crate::meta::projects::insert_project;
    use crate::meta::prompts::{self as store, NewPrompt, NewVersion, PromptType};
    use crate::meta::users::{self, NewUser, insert_user};
    use crate::observability::otlp::receiver::{Captured, string_attribute};
    use crate::observability::subscriber::capture_spans;
    use crate::test_support::TestApp;
    use cetra_core::ids::{OrgId, ProjectId};
    use opentelemetry_proto::tonic::trace::v1::Span as ProtoSpan;

    const TOOLS: [&str; 10] = [
        "list_projects",
        "get_schema",
        "query_sql",
        "list_saved_views",
        "list_prompts",
        "get_prompt",
        "search_traces",
        "get_trace",
        "get_span",
        "list_issues",
    ];

    fn init_body(version: &str) -> Value {
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
               "params": {"protocolVersion": version, "capabilities": {},
                          "clientInfo": {"name": "test", "version": "1"}}})
    }

    fn tools_list() -> Value {
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"})
    }

    fn meta_2026() -> Value {
        json!({"io.modelcontextprotocol/protocolVersion": V2026,
               "io.modelcontextprotocol/clientCapabilities": {}})
    }

    /// The token's id, as `personal_tokens` stores it.
    async fn token_id(app: &TestApp, token: &str) -> Uuid {
        personal_tokens::token_by_hash(app.state.meta.reader(), &secret::hash_token(token))
            .await
            .unwrap()
            .unwrap()
            .id
    }

    fn text_json(body: &Value) -> Value {
        let text = body["result"]["content"][0]["text"]
            .as_str()
            .expect("text content");
        serde_json::from_str(text).unwrap()
    }

    // Protocol

    #[tokio::test(flavor = "multi_thread")]
    async fn initialize_negotiates_2025_11_25() {
        let (app, _, tok) = app_with_token().await;
        let response = rpc(&app, Some(tok.as_str()), &init_body("2025-11-25")).await;
        assert_eq!(response.status, StatusCode::OK);
        assert_eq!(response.header("content-type"), Some("application/json"));
        assert!(response.header("mcp-session-id").is_none());
        let body: Value = response.json();
        assert_eq!(body["result"]["protocolVersion"], json!("2025-11-25"));
        assert_eq!(
            body["result"]["serverInfo"]["name"],
            json!(cetra_core::names::PRODUCT)
        );
        assert_eq!(
            body["result"]["serverInfo"]["version"],
            json!(env!("CARGO_PKG_VERSION"))
        );
        assert_eq!(body["result"]["capabilities"]["tools"], json!({}));
        assert_eq!(body["result"]["instructions"], json!(INSTRUCTIONS));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn initialize_negotiates_older_and_unknown_versions() {
        let (app, _, tok) = app_with_token().await;
        for version in ["2024-11-05", "2025-03-26", "2025-06-18"] {
            let response = post_mcp(
                &app,
                Some(tok.as_str()),
                Some(version),
                &[],
                &init_body(version),
            )
            .await;
            assert_eq!(response.status, StatusCode::OK, "{version}");
            let body: Value = response.json();
            assert_eq!(body["result"]["protocolVersion"], json!(version));
        }
        let response = post_mcp(
            &app,
            Some(tok.as_str()),
            None,
            &[],
            &init_body("2099-01-01"),
        )
        .await;
        assert_eq!(response.status, StatusCode::OK);
        let body: Value = response.json();
        assert_eq!(body["result"]["protocolVersion"], json!("2025-11-25"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn initialized_notification_is_accepted() {
        let (app, _, tok) = app_with_token().await;
        let response = rpc(
            &app,
            Some(tok.as_str()),
            &json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        )
        .await;
        assert_eq!(response.status, StatusCode::ACCEPTED);
        assert!(response.body.is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn tools_list_has_every_tool_read_only() {
        let (app, _, tok) = app_with_token().await;
        let body: Value = rpc(&app, Some(tok.as_str()), &tools_list()).await.json();
        let tools = body["result"]["tools"].as_array().unwrap();
        let names: Vec<&str> = tools
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, TOOLS);
        for tool in tools {
            assert_eq!(tool["annotations"]["readOnlyHint"], json!(true));
            assert_eq!(tool["inputSchema"]["type"], json!("object"));
        }
        assert!(tools[0]["inputSchema"].get("required").is_none());
        assert_eq!(tools[1]["inputSchema"]["required"], json!(["project"]));
        assert_eq!(
            tools[2]["inputSchema"]["required"],
            json!(["project", "query", "from", "to"])
        );
        assert_eq!(tools[3]["inputSchema"]["required"], json!(["project"]));
        assert_eq!(tools[4]["inputSchema"]["required"], json!(["project"]));
        assert_eq!(
            tools[5]["inputSchema"]["required"],
            json!(["project", "name"])
        );
        assert_eq!(
            tools[6]["inputSchema"]["required"],
            json!(["project", "from", "to"])
        );
        assert_eq!(
            tools[7]["inputSchema"]["required"],
            json!(["project", "trace_id"])
        );
        assert_eq!(
            tools[8]["inputSchema"]["required"],
            json!(["project", "trace_id", "span_id"])
        );
        assert_eq!(tools[9]["inputSchema"]["required"], json!(["project"]));
        assert!(body["result"].get("resultType").is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn unknown_tool_is_a_protocol_error() {
        let (app, _, tok) = app_with_token().await;
        let body: Value = rpc(
            &app,
            Some(tok.as_str()),
            &json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call",
                    "params": {"name": "nope"}}),
        )
        .await
        .json();
        assert_eq!(body["error"]["code"], json!(-32602));
        assert_eq!(body["error"]["message"], json!("unknown tool"));
        assert!(body.get("result").is_none());
        assert_eq!(app.state.analytics.counters.get("mcp.tool_called"), 0);
    }

    #[test]
    fn tool_events_add_sql_and_trace_reads() {
        use crate::analytics::catalog::Event;
        assert_eq!(
            tool_events(sql::QUERY_SQL, true),
            vec![
                Event::McpToolCalled {
                    tool: "query_sql",
                    ok: true,
                },
                Event::SqlConsoleRun,
            ]
        );
        let trace = tool_events(traces::GET_TRACE, true);
        assert_eq!(trace.last(), Some(&Event::TraceViewed));
        assert_eq!(
            tool_events(sql::QUERY_SQL, false),
            vec![Event::McpToolCalled {
                tool: "query_sql",
                ok: false,
            }]
        );
        assert_eq!(
            tool_events(tools::LIST_PROJECTS, true),
            vec![Event::McpToolCalled {
                tool: "list_projects",
                ok: true,
            }]
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn stateless_2026_07_28_requests() {
        let (app, _, tok) = app_with_token().await;

        let call_body = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {"name": "list_projects", "arguments": {}, "_meta": meta_2026()}});
        let response = post_mcp(
            &app,
            Some(tok.as_str()),
            Some(V2026),
            &[("mcp-method", "tools/call"), ("mcp-name", "list_projects")],
            &call_body,
        )
        .await;
        assert_eq!(response.status, StatusCode::OK);
        let body: Value = response.json();
        assert_eq!(body["result"]["resultType"], json!("complete"));
        assert_eq!(body["result"]["isError"], json!(false));
        assert_eq!(
            text_json(&body)["projects"][0]["slug"],
            json!("support-bot")
        );

        let list_body = json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list",
            "params": {"_meta": meta_2026()}});
        let response = post_mcp(
            &app,
            Some(tok.as_str()),
            Some(V2026),
            &[("mcp-method", "tools/list")],
            &list_body,
        )
        .await;
        assert_eq!(response.status, StatusCode::OK);
        let body: Value = response.json();
        assert_eq!(body["result"]["resultType"], json!("complete"));
        let names: Vec<&str> = body["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, TOOLS);

        let discover_body = json!({"jsonrpc": "2.0", "id": 3, "method": "server/discover",
            "params": {"_meta": meta_2026()}});
        let response = post_mcp(
            &app,
            Some(tok.as_str()),
            Some(V2026),
            &[("mcp-method", "server/discover")],
            &discover_body,
        )
        .await;
        assert_eq!(response.status, StatusCode::OK);
        let body: Value = response.json();
        assert_eq!(
            body["result"]["supportedVersions"],
            json!([
                "2024-11-05",
                "2025-03-26",
                "2025-06-18",
                "2025-11-25",
                "2026-07-28"
            ])
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn stateless_2026_07_28_rejects_incomplete_requests() {
        let (app, _, tok) = app_with_token().await;
        let bare = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"});
        let with_meta = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list",
            "params": {"_meta": meta_2026()}});

        // (a) the method header is there, the `_meta` is not.
        let response = post_mcp(
            &app,
            Some(tok.as_str()),
            Some(V2026),
            &[("mcp-method", "tools/list")],
            &bare,
        )
        .await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST);
        let body: Value = response.json();
        assert_eq!(body["error"]["code"], json!(-32602));

        // (b) `_meta` is there, `Mcp-Method` is not.
        let response = post_mcp(&app, Some(tok.as_str()), Some(V2026), &[], &with_meta).await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST);
        let body: Value = response.json();
        assert_eq!(body["error"]["code"], json!(-32020));

        // (c) an unknown protocol version and no `_meta`.
        let response = post_mcp(
            &app,
            Some(tok.as_str()),
            Some("1999-01-01"),
            &[("mcp-method", "tools/list")],
            &bare,
        )
        .await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn client_headers_are_required() {
        let (app, _, tok) = app_with_token().await;
        let bearer = format!("Bearer {tok}");
        let body = tools_list();

        // (a) no `accept`.
        let response = app
            .request(Method::POST, "/mcp")
            .header("host", "127.0.0.1:8080")
            .header("authorization", &bearer)
            .json(&body)
            .send()
            .await;
        assert_eq!(response.status, StatusCode::NOT_ACCEPTABLE);

        // (b) no `host`.
        let response = app
            .request(Method::POST, "/mcp")
            .header("accept", "application/json, text/event-stream")
            .header("authorization", &bearer)
            .json(&body)
            .send()
            .await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST);

        // (c) not JSON.
        let response = app
            .request(Method::POST, "/mcp")
            .header("host", "127.0.0.1:8080")
            .header("accept", "application/json, text/event-stream")
            .header("authorization", &bearer)
            .header("content-type", "text/plain")
            .body(body.to_string())
            .send()
            .await;
        assert_eq!(response.status, StatusCode::UNSUPPORTED_MEDIA_TYPE);

        // (d) JSON content type, invalid JSON.
        let response = app
            .request(Method::POST, "/mcp")
            .header("host", "127.0.0.1:8080")
            .header("accept", "application/json, text/event-stream")
            .header("authorization", &bearer)
            .header("content-type", "application/json")
            .body("{not json")
            .send()
            .await;
        assert_eq!(response.status, StatusCode::UNSUPPORTED_MEDIA_TYPE);

        // (e) a body over the limit.
        let big = json!({"jsonrpc": "2.0", "id": 4, "method": "tools/list",
            "params": {"x": "a".repeat(1_100_000)}});
        let response = app
            .request(Method::POST, "/mcp")
            .header("host", "127.0.0.1:8080")
            .header("accept", "application/json, text/event-stream")
            .header("authorization", &bearer)
            .json(&big)
            .send()
            .await;
        assert_eq!(response.status, StatusCode::PAYLOAD_TOO_LARGE);

        // (f) a method the route does not take.
        let response = app
            .request(Method::DELETE, "/mcp")
            .header("host", "127.0.0.1:8080")
            .header("authorization", &bearer)
            .send()
            .await;
        assert_eq!(response.status, StatusCode::METHOD_NOT_ALLOWED);
        let body: Value = response.json();
        assert_eq!(body["code"], json!("method_not_allowed"));

        // Every rejected request released its in-flight slot.
        assert_eq!(
            app.state.limits.mcp.in_flight(token_id(&app, &tok).await),
            0
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn any_host_and_origin_are_accepted() {
        let (app, _, tok) = app_with_token().await;
        let response = post_mcp(
            &app,
            Some(tok.as_str()),
            Some(V2025),
            &[
                ("host", "mcp.example.com"),
                ("origin", "https://evil.example"),
            ],
            &tools_list(),
        )
        .await;
        assert_eq!(response.status, StatusCode::OK);
        let body: Value = response.json();
        assert!(body["result"]["tools"].is_array());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn get_is_method_not_allowed() {
        let (app, _, tok) = app_with_token().await;
        let response = app
            .request(Method::GET, "/mcp")
            .header("host", "127.0.0.1:8080")
            .header("authorization", &format!("Bearer {tok}"))
            .send()
            .await;
        assert_eq!(response.status, StatusCode::METHOD_NOT_ALLOWED);
        let body: Value = response.json();
        assert_eq!(body["code"], json!("method_not_allowed"));
    }

    // Auth

    #[tokio::test(flavor = "multi_thread")]
    async fn only_personal_tokens_reach_mcp() {
        let (app, seeded, _) = app_with_token().await;
        let read_pat = app.personal_token(seeded.user_id, false).await;
        let write_pat = app.personal_token(seeded.user_id, true).await;
        let list = tools_list();

        let response = rpc(&app, None, &list).await;
        assert_eq!(response.status, StatusCode::UNAUTHORIZED);
        assert_eq!(response.json::<Value>()["code"], json!("unauthorized"));

        for key in [&seeded.read_key, &seeded.ingest_key, &seeded.public_key] {
            let response = rpc(&app, Some(key.as_str()), &list).await;
            assert_eq!(response.status, StatusCode::FORBIDDEN);
            assert_eq!(response.json::<Value>()["code"], json!("forbidden"));
        }

        for pat in [&read_pat, &write_pat] {
            let response = rpc(&app, Some(pat.as_str()), &list).await;
            assert_eq!(response.status, StatusCode::OK);
            let body: Value = response.json();
            assert!(body["result"]["tools"].is_array());
        }

        let response = app
            .request(Method::POST, "/mcp")
            .header("host", "127.0.0.1:8080")
            .header("accept", "application/json, text/event-stream")
            .header("cookie", &seeded.cookie)
            .json(&list)
            .send()
            .await;
        assert_eq!(response.status, StatusCode::FORBIDDEN);
        assert_eq!(response.json::<Value>()["code"], json!("forbidden"));

        let response = app
            .request(Method::POST, "/mcp")
            .header("host", "127.0.0.1:8080")
            .header("accept", "application/json, text/event-stream")
            .header("cookie", &seeded.cookie)
            .header("authorization", &format!("Bearer {read_pat}"))
            .json(&list)
            .send()
            .await;
        assert_eq!(response.status, StatusCode::OK);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn dead_tokens_are_unauthorized() {
        let (app, seeded, _) = app_with_token().await;
        let list = tools_list();
        let now = app.state.clock.now_ns();

        // (a) a token that was never issued.
        let bogus = format!("pat-cetra-{}", "A".repeat(32));
        let response = rpc(&app, Some(bogus.as_str()), &list).await;
        assert_eq!(response.status, StatusCode::UNAUTHORIZED, "never issued");

        // (b) a token revoked before its first use.
        let revoked = app.personal_token(seeded.user_id, false).await;
        let id = token_id(&app, &revoked).await;
        let user_id = seeded.user_id;
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    personal_tokens::revoke_token(conn, user_id, id, now).await?;
                    Ok(())
                })
            })
            .await
            .unwrap();
        let response = rpc(&app, Some(revoked.as_str()), &list).await;
        assert_eq!(response.status, StatusCode::UNAUTHORIZED, "revoked");

        // (c) an expired token.
        let minted = secret::new_key(KeyKind::Personal).unwrap();
        let (user_id, prefix, hash) = (seeded.user_id, minted.prefix.clone(), minted.hash.clone());
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    personal_tokens::insert_token(
                        conn,
                        NewToken {
                            user_id,
                            name: "old".to_owned(),
                            prefix,
                            hash,
                            scopes: vec![KeyScope::Read],
                            expires_at: Some(now - 1),
                        },
                        now,
                    )
                    .await?;
                    Ok(())
                })
            })
            .await
            .unwrap();
        let response = rpc(&app, Some(minted.plaintext.as_str()), &list).await;
        assert_eq!(response.status, StatusCode::UNAUTHORIZED, "expired");

        // (d) a token of a user who is then disabled.
        let member = app
            .add_member(&seeded, "gone@example.com", OrgRole::Member)
            .await;
        let disabled_token = app.personal_token(member.id, false).await;
        let member_id = member.id;
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    users::set_disabled(conn, member_id, Some(now), now).await?;
                    Ok(())
                })
            })
            .await
            .unwrap();
        let response = rpc(&app, Some(disabled_token.as_str()), &list).await;
        assert_eq!(response.status, StatusCode::UNAUTHORIZED, "disabled user");
    }

    // Limits

    #[tokio::test(flavor = "multi_thread")]
    async fn rate_limit_is_sixty_per_minute_per_token() {
        let (app, seeded, tok) = app_with_token().await;
        let list = tools_list();
        for _ in 0..60 {
            assert_eq!(
                rpc(&app, Some(tok.as_str()), &list).await.status,
                StatusCode::OK
            );
        }
        let refused = rpc(&app, Some(tok.as_str()), &list).await;
        assert_eq!(refused.status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(refused.header("retry-after"), Some("1"));
        assert_eq!(refused.json::<Value>()["code"], json!("rate_limited"));

        let other = app.personal_token(seeded.user_id, false).await;
        assert_eq!(
            rpc(&app, Some(other.as_str()), &list).await.status,
            StatusCode::OK
        );

        app.state.clock.advance(Duration::from_secs(1));
        assert_eq!(
            rpc(&app, Some(tok.as_str()), &list).await.status,
            StatusCode::OK
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn concurrency_is_four_per_token() {
        let (app, _, tok) = app_with_token().await;
        let id = token_id(&app, &tok).await;
        let now = app.state.clock.now_ns();
        let guards: Vec<_> = (0..4)
            .map(|_| app.state.limits.mcp.admit(id, now).expect("under the cap"))
            .collect();
        let list = tools_list();

        let held = rpc(&app, Some(tok.as_str()), &list).await;
        assert_eq!(held.status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(held.header("retry-after"), Some("1"));
        assert_eq!(held.json::<Value>()["code"], json!("rate_limited"));

        drop(guards);
        assert_eq!(
            rpc(&app, Some(tok.as_str()), &list).await.status,
            StatusCode::OK
        );
        assert_eq!(app.state.limits.mcp.in_flight(id), 0);

        let (is_error, _) = call(&app, &tok, "get_schema", &json!({"project": "acme/nope"})).await;
        assert!(is_error);
        assert_eq!(app.state.limits.mcp.in_flight(id), 0);
    }

    /// A call whose client disconnects mid-tool drops its query instead of running it to
    /// completion.
    ///
    /// Deterministic in three parts: the slow call's cross join over the 14 example spans
    /// (14⁹ ≈ 20 billion rows) runs for minutes, so whether it is still running when the
    /// probes run never depends on timing precision; the first probe is retried until the
    /// slow call holds its admission (a probe admitted earlier just answers and frees it);
    /// and after the disconnect the free probe
    /// itself waits out the 200 ms admission window, so the cancellation's propagation (all
    /// in-memory, µs–ms) is absorbed rather than raced.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_disconnected_call_drops_its_query() {
        use bytesize::ByteSize;
        use tokio::io::AsyncWriteExt;

        let app = TestApp::with_config(|config| {
            config.storage.query.max_memory_total = Some(ByteSize::mib(64));
            config.storage.query.max_memory = ByteSize::mib(64);
            config.storage.query.initial_memory = ByteSize::mib(64);
            config.storage.query.admission_wait = Duration::from_millis(200);
        })
        .await;
        let seeded = app.seed().await;
        app.load_example_dataset(seeded.project.id).await;
        let token = app.personal_token(seeded.user_id, false).await;
        let addr = app.serve_loopback().await;
        let project = seeded.project.id.to_string();
        let select_one =
            json!({"project": project, "query": "SELECT 1 AS one", "from": FROM, "to": TO});

        // The slow call, sent over a socket we hold: its response only exists once the tool
        // finishes, so the connection stays open in the meantime.
        let slow = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {"name": "query_sql", "arguments": {
                "project": project,
                "query": "SELECT count(*) FROM spans a CROSS JOIN spans b CROSS JOIN spans c \
                          CROSS JOIN spans d CROSS JOIN spans e CROSS JOIN spans f \
                          CROSS JOIN spans g CROSS JOIN spans h CROSS JOIN spans i",
                "from": FROM, "to": TO}}});
        let body = serde_json::to_string(&slow).unwrap();
        let mut socket = tokio::net::TcpStream::connect(addr).await.unwrap();
        let head = format!(
            "POST /mcp HTTP/1.1\r\nhost: {addr}\r\nauthorization: Bearer {token}\r\n\
             accept: application/json, text/event-stream\r\ncontent-type: application/json\r\n\
             mcp-protocol-version: {V2025}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            body.len(),
        );
        socket.write_all(head.as_bytes()).await.unwrap();
        socket.write_all(body.as_bytes()).await.unwrap();
        socket.flush().await.unwrap();

        // While it runs it holds the only query memory: another call is busy. Retry until the
        // slow call has been admitted.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let (is_error, body) = call(&app, &token, "query_sql", &select_one).await;
            if is_error && body["error"]["code"] == json!("busy") {
                break;
            }
            assert!(!is_error, "{body}");
            assert!(
                std::time::Instant::now() < deadline,
                "the slow call was never admitted"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        // The client goes away. rmcp cancels the call's token, `call_tool` drops the tool
        // future, the query context (and its memory) is dropped with it, and a following
        // call is admitted again.
        drop(socket);
        let (is_error, body) = call(&app, &token, "query_sql", &select_one).await;
        assert!(!is_error, "{body}");
        assert_eq!(body["rows"], json!([{"one": 1}]));
        assert!(
            app.state.analytics.counters.get("sql_console.run") >= 1,
            "sql_console.run not counted"
        );
    }

    // Tools

    #[tokio::test(flavor = "multi_thread")]
    async fn list_projects_lists_visible_projects() {
        let (app, seeded, tok) = app_with_token().await;
        let (playground, _) = app.add_project(&seeded, "playground", None).await;
        let (is_error, body) = call(&app, &tok, "list_projects", &json!({})).await;
        assert!(!is_error);
        assert_eq!(
            body,
            json!({
                "projects": [
                    {"id": seeded.project.id.to_string(), "org": "acme", "slug": "support-bot",
                     "name": "Support", "role": "admin"},
                    {"id": playground.id.to_string(), "org": "acme", "slug": "playground",
                     "name": "playground", "role": "admin"},
                ],
                "truncated": false,
                "notes": [],
            })
        );
        assert!(
            app.state.analytics.counters.get("mcp.tool_called") >= 1,
            "mcp.tool_called not counted"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn list_projects_follows_roles_and_overrides() {
        let (app, seeded, _) = app_with_token().await;
        let viewer = app
            .add_member(&seeded, "viewer@example.com", OrgRole::Viewer)
            .await;
        let (playground, _) = app.add_project(&seeded, "playground", None).await;
        let (playground_id, viewer_id) = (playground.id, viewer.id);
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    set_project_override(conn, playground_id, viewer_id, ProjectOverride::None)
                        .await?;
                    Ok(())
                })
            })
            .await
            .unwrap();
        let viewer_tok = app.personal_token(viewer.id, false).await;
        let (_, body) = call(&app, &viewer_tok, "list_projects", &json!({})).await;
        assert_eq!(
            body["projects"],
            json!([{"id": seeded.project.id.to_string(), "org": "acme", "slug": "support-bot",
                    "name": "Support", "role": "viewer"}])
        );

        let stranger_id = app
            .state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    let user = insert_user(
                        conn,
                        NewUser {
                            email: "stranger@example.com".to_owned(),
                            name: None,
                            password_hash: None,
                            email_verified_at: None,
                            email_verified_via: None,
                            is_instance_admin: false,
                        },
                        0,
                    )
                    .await?;
                    Ok(user.id)
                })
            })
            .await
            .unwrap();
        let stranger_tok = app.personal_token(stranger_id, false).await;
        let (_, body) = call(&app, &stranger_tok, "list_projects", &json!({})).await;
        assert_eq!(body["projects"], json!([]));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn list_projects_skips_inactive_projects_and_orgs() {
        let (app, seeded, tok) = app_with_token().await;
        let (gone, _) = app.add_project(&seeded, "gone", None).await;
        let gone_id = gone.id;
        let user_id = seeded.user_id;
        let now = app.state.clock.now_ns();
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    sqlx::query("UPDATE projects SET state = 'deleting' WHERE id = ?")
                        .bind(gone_id.to_string())
                        .execute(&mut *conn)
                        .await?;
                    let org = insert_org(conn, OrgId::new_v7(), "Beta", "beta", now).await?;
                    add_org_member(conn, org.id, user_id, OrgRole::Owner, now).await?;
                    insert_project(conn, org.id, "Beta app", "beta-app", None, now).await?;
                    sqlx::query("UPDATE orgs SET state = 'deleting' WHERE id = ?")
                        .bind(org.id.to_string())
                        .execute(&mut *conn)
                        .await?;
                    Ok(())
                })
            })
            .await
            .unwrap();
        let (_, body) = call(&app, &tok, "list_projects", &json!({})).await;
        let slugs: Vec<&str> = body["projects"]
            .as_array()
            .unwrap()
            .iter()
            .map(|project| project["slug"].as_str().unwrap())
            .collect();
        assert_eq!(slugs, ["support-bot"]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn list_projects_rejects_arguments() {
        let (app, _, tok) = app_with_token().await;
        let (is_error, body) = call(&app, &tok, "list_projects", &json!({"x": 1})).await;
        assert!(is_error);
        assert_eq!(body["error"]["code"], json!("bad_request"));
        assert!(
            body["error"]["message"]
                .as_str()
                .unwrap()
                .starts_with("invalid arguments: unknown field `x`")
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn list_projects_fits_its_budget() {
        let (app, seeded, tok) = app_with_token().await;
        for i in 0..200 {
            app.add_project(&seeded, &format!("p{i:03}"), None).await;
        }
        let (is_error, body) = call(&app, &tok, "list_projects", &json!({})).await;
        assert!(!is_error);
        assert_eq!(body["truncated"], json!(true));
        assert_eq!(
            body["notes"],
            json!(["result cut at 20000 characters; later projects left out"])
        );
        assert!(output::char_len(&output::render(&body)) <= 20_000);
        let projects = body["projects"].as_array().unwrap();
        assert!(projects.len() < 201);
        assert_eq!(projects[0]["slug"], json!("support-bot"));
        assert_eq!(projects[1]["slug"], json!("p000"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn get_schema_is_the_schema_endpoint_body() {
        let (app, seeded, tok) = app_with_token().await;
        let expected = serde_json::to_value(convert::schema_response()).unwrap();
        for project in [seeded.project.id.to_string(), "acme/support-bot".to_owned()] {
            let (is_error, body) =
                call(&app, &tok, "get_schema", &json!({"project": project})).await;
            assert!(!is_error);
            assert_eq!(body, expected);
            assert!(output::char_len(&output::render(&body)) < 50_000);
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn get_schema_errors_are_tool_errors() {
        let (app, seeded, tok) = app_with_token().await;

        let (is_error, body) =
            call(&app, &tok, "get_schema", &json!({"project": "acme/nope"})).await;
        assert!(is_error);
        assert_eq!(
            body,
            json!({"error": {"code": "not_found", "message": "Not Found"}})
        );

        let (is_error, body) = call(
            &app,
            &tok,
            "get_schema",
            &json!({"project": seeded.project.id.to_string(), "x": 1}),
        )
        .await;
        assert!(is_error);
        assert_eq!(body["error"]["code"], json!("bad_request"));
        assert!(
            body["error"]["message"]
                .as_str()
                .unwrap()
                .starts_with("invalid arguments: unknown field `x`")
        );

        let (is_error, body) = call(&app, &tok, "get_schema", &json!({})).await;
        assert!(is_error);
        assert_eq!(body["error"]["code"], json!("bad_request"));
        assert!(
            body["error"]["message"]
                .as_str()
                .unwrap()
                .starts_with("invalid arguments: missing field `project`")
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn get_schema_needs_a_visible_active_project() {
        let (app, seeded, tok) = app_with_token().await;
        let not_found = json!({"error": {"code": "not_found", "message": "Not Found"}});
        let gone = "acme/gone";
        let user_id = seeded.user_id;
        let now = app.state.clock.now_ns();
        let (gone_project, _) = app.add_project(&seeded, "gone", None).await;
        let gone_id = gone_project.id;
        let viewer = app
            .add_member(&seeded, "viewer@example.com", OrgRole::Viewer)
            .await;
        let (hidden, _) = app.add_project(&seeded, "hidden", None).await;
        let (viewer_id, hidden_id) = (viewer.id, hidden.id);
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    set_project_override(conn, hidden_id, viewer_id, ProjectOverride::None).await?;
                    sqlx::query("UPDATE projects SET state = 'deleting' WHERE id = ?")
                        .bind(gone_id.to_string())
                        .execute(&mut *conn)
                        .await?;
                    let org = insert_org(conn, OrgId::new_v7(), "Beta", "beta", now).await?;
                    add_org_member(conn, org.id, user_id, OrgRole::Owner, now).await?;
                    insert_project(conn, org.id, "Beta app", "beta-app", None, now).await?;
                    sqlx::query("UPDATE orgs SET state = 'deleting' WHERE id = ?")
                        .bind(org.id.to_string())
                        .execute(&mut *conn)
                        .await?;
                    Ok(())
                })
            })
            .await
            .unwrap();

        // (a) a project hidden from the caller by an override.
        let viewer_tok = app.personal_token(viewer_id, false).await;
        let (is_error, body) = call(
            &app,
            &viewer_tok,
            "get_schema",
            &json!({"project": "acme/hidden"}),
        )
        .await;
        assert!(is_error);
        assert_eq!(body, not_found);

        // (b) an inactive project, (c) a project of an inactive org.
        for project in [gone, "beta/beta-app"] {
            let (is_error, body) =
                call(&app, &tok, "get_schema", &json!({"project": project})).await;
            assert!(is_error, "{project}");
            assert_eq!(body, not_found, "{project}");
        }

        // (d) a user who is not a member of the org.
        let stranger_id = app
            .state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    let user = insert_user(
                        conn,
                        NewUser {
                            email: "stranger@example.com".to_owned(),
                            name: None,
                            password_hash: None,
                            email_verified_at: None,
                            email_verified_via: None,
                            is_instance_admin: false,
                        },
                        0,
                    )
                    .await?;
                    Ok(user.id)
                })
            })
            .await
            .unwrap();
        let stranger_tok = app.personal_token(stranger_id, false).await;
        let (is_error, body) = call(
            &app,
            &stranger_tok,
            "get_schema",
            &json!({"project": "acme/support-bot"}),
        )
        .await;
        assert!(is_error);
        assert_eq!(body, not_found);

        // (e) an org viewer's read-only PAT reads the schema.
        let (is_error, body) = call(
            &app,
            &viewer_tok,
            "get_schema",
            &json!({"project": "acme/support-bot"}),
        )
        .await;
        assert!(!is_error);
        let expected = serde_json::to_value(convert::schema_response()).unwrap();
        assert_eq!(body, expected);
    }

    /// Creates prompt `name` (text prompts) in `project`: one version per entry of `versions`,
    /// each labelled with the labels in its entry.
    async fn create_prompt(
        app: &TestApp,
        project: ProjectId,
        name: &str,
        versions: Vec<Vec<&str>>,
    ) {
        let now = app.state.clock.now_ns();
        let name = name.to_owned();
        let versions: Vec<NewVersion> = versions
            .into_iter()
            .map(|labels| NewVersion {
                template: "\"hello\"".to_owned(),
                config: "{}".to_owned(),
                commit_message: None,
                labels: labels.into_iter().map(str::to_owned).collect(),
                actor: None,
            })
            .collect();
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    let mut versions = versions.into_iter();
                    let first = versions.next().expect("at least one version");
                    let prompt = NewPrompt {
                        id: Uuid::now_v7(),
                        name,
                        prompt_type: PromptType::Text,
                        description: None,
                        tags: Vec::new(),
                    };
                    let (row, _) = store::create_prompt(conn, project, prompt, first, now).await?;
                    for version in versions {
                        store::add_version(conn, row.id, version, now).await?;
                    }
                    Ok(())
                })
            })
            .await
            .unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn list_prompts_orders_filters_and_limits() {
        let (app, seeded, tok) = app_with_token().await;
        let project = seeded.project.id;
        create_prompt(&app, project, "beta", vec![vec!["production"]]).await;
        create_prompt(&app, project, "Alpha", vec![vec!["staging"]]).await;
        let names = |body: &Value| -> Vec<String> {
            body["prompts"]
                .as_array()
                .unwrap()
                .iter()
                .map(|prompt| prompt["name"].as_str().unwrap().to_owned())
                .collect()
        };

        let (is_error, body) = call(
            &app,
            &tok,
            "list_prompts",
            &json!({"project": project.to_string()}),
        )
        .await;
        assert!(!is_error, "{body}");
        assert_eq!(names(&body), ["Alpha", "beta"]);
        assert_eq!(body["truncated"], json!(false));
        assert_eq!(body["notes"], json!([]));
        assert_eq!(
            body["prompts"][1],
            json!({"name": "beta", "type": "text", "tags": [], "latest_version": 1,
                   "labels": {"production": 1}})
        );

        let (_, body) = call(
            &app,
            &tok,
            "list_prompts",
            &json!({"project": project.to_string(), "q": "ALP"}),
        )
        .await;
        assert_eq!(names(&body), ["Alpha"]);

        let (_, body) = call(
            &app,
            &tok,
            "list_prompts",
            &json!({"project": project.to_string(), "label": "production"}),
        )
        .await;
        assert_eq!(names(&body), ["beta"]);

        let (_, body) = call(
            &app,
            &tok,
            "list_prompts",
            &json!({"project": project.to_string(), "limit": 1}),
        )
        .await;
        assert_eq!(names(&body), ["Alpha"]);
        assert_eq!(body["truncated"], json!(true));
        assert_eq!(
            body["notes"],
            json!(["more prompts than limit; narrow with q or label"])
        );

        for limit in [0, 201] {
            let (is_error, body) = call(
                &app,
                &tok,
                "list_prompts",
                &json!({"project": project.to_string(), "limit": limit}),
            )
            .await;
            assert!(is_error, "{limit}");
            assert_eq!(body["error"]["code"], json!("bad_request"));
            assert_eq!(body["error"]["message"], json!("limit must be 1 to 200"));
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn get_prompt_resolves_labels_and_versions() {
        let (app, seeded, tok) = app_with_token().await;
        let project = seeded.project.id.to_string();
        create_prompt(
            &app,
            seeded.project.id,
            "support",
            vec![vec!["production"], vec![]],
        )
        .await;

        // The default is the version labelled production, here version 1.
        let (is_error, body) = call(
            &app,
            &tok,
            "get_prompt",
            &json!({"project": project, "name": "support"}),
        )
        .await;
        assert!(!is_error, "{body}");
        assert_eq!(body["name"], json!("support"));
        assert_eq!(body["version"], json!(1));
        assert_eq!(body["labels"], json!(["production"]));
        assert_eq!(body["template"], json!("hello"));

        // `version: 2` and `label: "latest"` are the latest version, which carries `latest`.
        for arguments in [
            json!({"project": project, "name": "support", "version": 2}),
            json!({"project": project, "name": "support", "label": "latest"}),
        ] {
            let (is_error, body) = call(&app, &tok, "get_prompt", &arguments).await;
            assert!(!is_error, "{body}");
            assert_eq!(body["version"], json!(2));
            assert_eq!(body["labels"], json!(["latest"]));
        }

        let (is_error, body) = call(
            &app,
            &tok,
            "get_prompt",
            &json!({"project": project, "name": "support", "label": "production", "version": 1}),
        )
        .await;
        assert!(is_error);
        assert_eq!(
            body,
            json!({"error": {"code": "bad_request",
                             "message": "give label or version, not both"}})
        );

        let not_found = json!({"error": {"code": "not_found", "message": "Not Found"}});
        for arguments in [
            json!({"project": project, "name": "nope"}),
            json!({"project": project, "name": "support", "label": "staging"}),
            json!({"project": project, "name": "support", "version": 9}),
        ] {
            let (is_error, body) = call(&app, &tok, "get_prompt", &arguments).await;
            assert!(is_error, "{arguments}");
            assert_eq!(body, not_found, "{arguments}");
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn prompt_tools_need_view_access() {
        let (app, seeded, _) = app_with_token().await;
        create_prompt(&app, seeded.project.id, "support", vec![vec!["production"]]).await;
        let stranger_id = app
            .state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    let user = insert_user(
                        conn,
                        NewUser {
                            email: "stranger@example.com".to_owned(),
                            name: None,
                            password_hash: None,
                            email_verified_at: None,
                            email_verified_via: None,
                            is_instance_admin: false,
                        },
                        0,
                    )
                    .await?;
                    Ok(user.id)
                })
            })
            .await
            .unwrap();
        let stranger_tok = app.personal_token(stranger_id, false).await;
        let project = seeded.project.id.to_string();
        let not_found = json!({"error": {"code": "not_found", "message": "Not Found"}});
        for (tool, arguments) in [
            ("list_prompts", json!({"project": project})),
            ("get_prompt", json!({"project": project, "name": "support"})),
        ] {
            let (is_error, body) = call(&app, &stranger_tok, tool, &arguments).await;
            assert!(is_error, "{tool}");
            assert_eq!(body, not_found, "{tool}");
        }
    }

    // Logs

    #[tokio::test]
    async fn mcp_logs_tool_calls_without_secrets() {
        let (app, _, tok) = app_with_token().await;
        let (_, logs) = capture_info_logs(async {
            call(&app, &tok, "list_projects", &json!({})).await;
            call(
                &app,
                &tok,
                "get_schema",
                &json!({"project": "acme/canary-7f3a9"}),
            )
            .await;
            rpc(&app, None, &tools_list()).await;
        })
        .await;
        assert!(logs.contains("tool call"), "{logs}");
        assert!(logs.contains("list_projects"), "{logs}");
        assert!(logs.contains("get_schema"), "{logs}");
        assert!(!logs.contains(&tok), "{logs}");
        assert!(!logs.contains("canary-7f3a9"), "{logs}");
    }

    #[tokio::test]
    async fn malformed_bodies_are_not_logged() {
        let (app, _, tok) = app_with_token().await;
        let body = r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"get_schema","arguments":{"project":"CANARY-51b2""#;
        let (response, logs) = capture_info_logs(async {
            app.request(Method::POST, "/mcp")
                .header("host", "127.0.0.1:8080")
                .header("accept", "application/json, text/event-stream")
                .header("content-type", "application/json")
                .header("authorization", &format!("Bearer {tok}"))
                .body(body)
                .send()
                .await
        })
        .await;
        assert_eq!(response.status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
        assert!(!logs.contains("CANARY-51b2"), "{logs}");
    }

    /// The one span named `name` that a test captured.
    fn only(captured: &Captured, name: &str) -> ProtoSpan {
        let mut spans = captured.named(name);
        assert_eq!(spans.len(), 1, "{name}: {spans:?}");
        spans.remove(0)
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_tool_call_runs_in_a_query_span_under_its_request() {
        let (app, _, tok) = app_with_token().await;
        let ((is_error, _), captured) =
            capture_spans(call(&app, &tok, "list_projects", &json!({}))).await;
        assert!(!is_error);

        let request = only(&captured, "http.request");
        let query = only(&captured, "query");
        assert_eq!(
            string_attribute(&query.attributes, "kind"),
            Some("mcp".to_owned())
        );
        assert_eq!(
            string_attribute(&query.attributes, "tool"),
            Some("list_projects".to_owned())
        );
        assert_eq!(
            string_attribute(&query.attributes, "outcome"),
            Some("ok".to_owned())
        );
        assert_eq!(
            string_attribute(&request.attributes, "http.route"),
            Some("/mcp".to_owned())
        );
        assert_eq!(query.parent_span_id, request.span_id);
        assert!(captured.spans().iter().all(|span| !span.name.starts_with("rmcp")));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn an_unknown_tool_records_no_tool_name() {
        let (app, _, tok) = app_with_token().await;
        let ((), captured) = capture_spans(async {
            let body: Value = rpc(
                &app,
                Some(tok.as_str()),
                &json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call",
                        "params": {"name": "sk-cetra-planted-tool"}}),
            )
            .await
            .json();
            assert_eq!(body["error"]["code"], json!(-32602));
        })
        .await;

        let query = only(&captured, "query");
        assert_eq!(
            string_attribute(&query.attributes, "outcome"),
            Some("unknown_tool".to_owned())
        );
        assert_eq!(string_attribute(&query.attributes, "tool"), None);
        assert!(!captured.text().contains("sk-cetra-planted-tool"));
    }
}
