//! The org's assistant setting (track AS, D-AS-3) and its tool-calling test (D-AS-24).

use std::time::{Duration, Instant};

use cetra_core::ids::OrgId;
use cetra_core::message::{Message, Part, Role};
use cetra_eval::{
    ChatError, ChatEvent, ChatParams, ChatRequest, ErrorClass, JudgeConnection, ToolChoice,
    ToolDefinition, stream_chat,
};
use serde_json::{Map, json};

use crate::api::connections::{LlmConnectionTestError, LlmConnectionTestResult, TestErrorClass};
use crate::http::AppState;
use crate::http::error::ApiError;
use crate::meta::assistant as store;
use crate::meta::connections::{self, ConnectionRow};

/// The tool the test asks the model to call (D-AS-24).
const PING: &str = "ping";
/// The completion cap of the test call (D-AS-24).
const TEST_MAX_TOKENS: u32 = 256;
/// The whole-call and idle timeout of the test call (D-AS-24).
const TEST_TIMEOUT: Duration = Duration::from_secs(30);

/// What the org's setting allows, resolved from its row and its connection.
pub(crate) enum Resolved {
    /// The assistant may run: its connection and model.
    Usable {
        connection: ConnectionRow,
        model: String,
    },
    /// The setting is off, or there is no row.
    Disabled,
    /// The setting is on, but its connection or model is missing.
    NoConnection,
}

/// Resolves the setting of `org`, in order: no row or not enabled gives `Disabled`; no connection,
/// no model, or a connection that is no longer in the org gives `NoConnection`; otherwise `Usable`.
///
/// # Errors
///
/// Returns [`ApiError`] when a query fails or a stored value does not decode.
pub(crate) async fn resolve(state: &AppState, org: OrgId) -> Result<Resolved, ApiError> {
    let Some(row) = store::get_settings(state.meta.reader(), org).await? else {
        return Ok(Resolved::Disabled);
    };
    if !row.enabled {
        return Ok(Resolved::Disabled);
    }
    let (Some(connection_id), Some(model)) = (row.connection_id, row.model) else {
        return Ok(Resolved::NoConnection);
    };
    match connections::get_connection(state.meta.reader(), org, connection_id).await? {
        Some(connection) => Ok(Resolved::Usable { connection, model }),
        None => Ok(Resolved::NoConnection),
    }
}

/// Runs D-AS-24's test call on `connection` with `model`. Answers the outcome: a failed call is
/// `ok: false` with the error, not an `Err`. Changes nothing on the connection.
///
/// # Errors
///
/// Returns [`ApiError`] (`500`) when the connection's sealed key does not open.
pub(crate) async fn test_call(
    state: &AppState,
    connection: &ConnectionRow,
    model: &str,
) -> Result<LlmConnectionTestResult, ApiError> {
    let decrypted = state
        .connections
        .get(&state.secrets, connection)
        .map_err(internal)?;
    let request = test_request(model);
    let started = Instant::now();
    let outcome = run_test(state, &decrypted, &request).await;
    let latency_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    // The messages quote no payload: `ChatError` displays a fixed text, and the class is a token.
    let error = match outcome {
        Ok(true) => None,
        Ok(false) => Some(LlmConnectionTestError {
            class: TestErrorClass::Permanent,
            message: "the model did not call the tool".to_owned(),
        }),
        Err(error) => Some(LlmConnectionTestError {
            class: test_class(error.class()),
            message: error.to_string(),
        }),
    };
    Ok(LlmConnectionTestResult {
        ok: error.is_none(),
        model: model.to_owned(),
        latency_ms,
        error,
    })
}

/// Reads the whole stream. True when a `ping` tool call arrived.
async fn run_test(
    state: &AppState,
    connection: &JudgeConnection,
    request: &ChatRequest,
) -> Result<bool, ChatError> {
    let mut stream = stream_chat(state.eval_runner.egress(), connection, request).await?;
    let mut called = false;
    while let Some(event) = stream.next().await? {
        if let ChatEvent::ToolCall(call) = event
            && call.name == PING
        {
            called = true;
        }
    }
    Ok(called)
}

/// The test request of D-AS-24: one system and one user message, the tool `ping` forced.
fn test_request(model: &str) -> ChatRequest {
    ChatRequest {
        model: model.to_owned(),
        messages: vec![
            text_message(Role::System, "You are checking that tool calling works."),
            text_message(Role::User, "Call the tool ping with ok set to true."),
        ],
        tools: vec![ToolDefinition {
            name: PING.to_owned(),
            description: Some("Answer the check.".to_owned()),
            parameters: json!({
                "type": "object",
                "properties": {"ok": {"type": "boolean"}},
                "required": ["ok"],
                "additionalProperties": false
            }),
        }],
        tool_choice: Some(ToolChoice::Tool(PING.to_owned())),
        response_format: None,
        params: ChatParams::default(),
        max_tokens: TEST_MAX_TOKENS,
        timeout: TEST_TIMEOUT,
        idle_timeout: TEST_TIMEOUT,
    }
}

fn text_message(role: Role, content: &str) -> Message {
    Message {
        role,
        name: None,
        parts: vec![Part::Text {
            content: content.to_owned(),
            extra: Map::new(),
        }],
        finish_reason: None,
        extra: Map::new(),
    }
}

/// The class the connection test answers, mapped as `routes/connections.rs` maps it.
fn test_class(class: ErrorClass) -> TestErrorClass {
    match class {
        ErrorClass::Retryable => TestErrorClass::Retryable,
        ErrorClass::Pause => TestErrorClass::Pause,
        ErrorClass::Permanent => TestErrorClass::Permanent,
    }
}

fn internal(error: impl std::error::Error + Send + Sync + 'static) -> ApiError {
    ApiError::Internal(Box::new(error))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_test_forces_the_ping_tool() {
        let request = test_request("gpt-test");
        assert_eq!(request.model, "gpt-test");
        assert_eq!(
            request.tool_choice,
            Some(ToolChoice::Tool("ping".to_owned()))
        );
        assert_eq!(request.tools.len(), 1);
        assert_eq!(request.tools[0].name, "ping");
        assert_eq!(request.max_tokens, 256);
        assert_eq!(request.messages.len(), 2);
    }
}
