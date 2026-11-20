//! Anthropic Messages.
//!
//! The base URL is the host (`https://api.anthropic.com`). The request path is
//! `v1/messages`. Structured outputs are sent as `output_config.format` and do
//! not use an `anthropic-beta` header. The version header is the one in the
//! current Messages reference.

use std::collections::BTreeMap;

use cetra_core::message::{Message, Part, Role};
use reqwest::header::{HeaderName, HeaderValue};
use serde_json::{Map, Value, json};

use super::sse::SseEvent;
use super::stream::{Flow, Sink};
use super::{
    ChatDone, ChatReply, ChatRequest, FinishReason, Prepared, ReplyContent, TokenUsage, ToolCall,
    ToolChoice, ToolDefinition, arguments_object, check_request, endpoint, messages_to_send,
    number, other_finish, request_headers, response_text, secret_header, send_buffered,
    stream_arguments, stream_error, texts, validate_model,
};
use crate::egress::EgressClient;
use crate::error::ChatError;
use crate::judge::{Adapter, JudgeConnection, StructuredOutput};

const DEFAULT_BASE: &str = "https://api.anthropic.com";
const ANTHROPIC_VERSION: &str = "2026-10-21";

/// POSTs `request` to Messages without streaming.
pub(crate) async fn complete(
    client: &EgressClient,
    conn: &JudgeConnection,
    request: &ChatRequest,
) -> Result<ChatReply, ChatError> {
    let prepared = prepare(conn, request, false)?;
    let body = send_buffered(client, conn, request, prepared).await?;
    Ok(decode_reply(&body))
}

/// The URL, headers and body of one Messages call. `stream` adds `"stream": true` (D-V-19).
pub(crate) fn prepare(
    conn: &JudgeConnection,
    request: &ChatRequest,
    stream: bool,
) -> Result<Prepared, ChatError> {
    validate_model(&request.model)?;
    check_request(request)?;
    let url = endpoint(conn.base_url.as_ref(), DEFAULT_BASE, "v1/messages")?;
    let headers = request_headers(
        conn,
        vec![
            (
                HeaderName::from_static("x-api-key"),
                secret_header(&conn.api_key)?,
            ),
            (
                HeaderName::from_static("anthropic-version"),
                HeaderValue::from_static(ANTHROPIC_VERSION),
            ),
            (
                HeaderName::from_static("content-type"),
                HeaderValue::from_static("application/json"),
            ),
        ],
    )?;
    let body = request_body(conn.structured_output, request, stream)?;
    Ok(Prepared { url, headers, body })
}

fn request_body(
    mode: StructuredOutput,
    request: &ChatRequest,
    stream: bool,
) -> Result<Vec<u8>, ChatError> {
    let mut body = Map::new();
    body.insert("model".to_owned(), Value::String(request.model.clone()));
    body.insert("max_tokens".to_owned(), Value::from(request.max_tokens));
    let messages = messages_to_send(mode, request)?;
    let system: Vec<Value> = messages
        .iter()
        .filter(|message| message.role == Role::System)
        .flat_map(texts)
        .map(text_block)
        .collect();
    if !system.is_empty() {
        body.insert("system".to_owned(), Value::Array(system));
    }
    body.insert(
        "messages".to_owned(),
        Value::Array(encode_messages(&messages)?),
    );
    let params = &request.params;
    if let Some(temperature) = params.temperature {
        body.insert("temperature".to_owned(), number(temperature)?);
    }
    if let Some(top_p) = params.top_p {
        body.insert("top_p".to_owned(), number(top_p)?);
    }
    if !params.stop.is_empty() {
        body.insert("stop_sequences".to_owned(), json!(params.stop));
    }
    let format = request.response_format.as_ref();
    if let (Some(format), StructuredOutput::Native) = (format, mode) {
        body.insert(
            "output_config".to_owned(),
            json!({
                "format": {
                    "type": "json_schema",
                    "schema": format.schema,
                }
            }),
        );
    }
    let mut tools: Vec<Value> = request.tools.iter().map(tool_json).collect();
    if let (Some(format), StructuredOutput::Tool) = (format, mode) {
        tools.push(json!({
            "name": format.tool_name,
            "description": format.tool_description,
            "input_schema": format.schema,
        }));
    }
    if !tools.is_empty() {
        body.insert("tools".to_owned(), Value::Array(tools));
    }
    let choice = match (format, mode) {
        (Some(format), StructuredOutput::Tool) => {
            Some(json!({"type": "tool", "name": format.tool_name}))
        }
        // A provider refuses a tool choice when there are no tools.
        _ if !request.tools.is_empty() => request.tool_choice.as_ref().map(choice_json),
        _ => None,
    };
    if let Some(choice) = choice {
        body.insert("tool_choice".to_owned(), choice);
    }
    if stream {
        body.insert("stream".to_owned(), Value::Bool(true));
    }
    serde_json::to_vec(&Value::Object(body)).map_err(|_| ChatError::InvalidRequest)
}

/// One text content block.
fn text_block(text: &str) -> Value {
    json!({"type": "text", "text": text})
}

fn tool_json(tool: &ToolDefinition) -> Value {
    let mut entry = Map::new();
    entry.insert("name".to_owned(), Value::String(tool.name.clone()));
    if let Some(description) = &tool.description {
        entry.insert("description".to_owned(), Value::String(description.clone()));
    }
    entry.insert("input_schema".to_owned(), tool.parameters.clone());
    Value::Object(entry)
}

fn choice_json(choice: &ToolChoice) -> Value {
    match choice {
        ToolChoice::Auto => json!({"type": "auto"}),
        ToolChoice::None => json!({"type": "none"}),
        ToolChoice::Required => json!({"type": "any"}),
        ToolChoice::Tool(name) => json!({"type": "tool", "name": name}),
    }
}

/// The request's non-system messages as Messages messages (D-V-31).
///
/// A message with no blocks is skipped. A message whose role is the same as the one
/// before it is merged into that message. The ids of tool-call and tool-result parts
/// are read with `unwrap_or_default`: [`check_request`] refuses a missing or empty id,
/// so the default is never sent.
fn encode_messages(messages: &[Message]) -> Result<Vec<Value>, ChatError> {
    let mut entries: Vec<(&'static str, Vec<Value>)> = Vec::new();
    for (index, message) in messages.iter().enumerate() {
        let (role, blocks) = match &message.role {
            Role::System => continue,
            Role::User => (
                "user",
                texts(message)
                    .into_iter()
                    .map(text_block)
                    .collect::<Vec<Value>>(),
            ),
            Role::Assistant => ("assistant", assistant_blocks(index, message)?),
            Role::Tool => ("user", tool_result_blocks(message)?),
            Role::Other(_) => return Err(ChatError::InvalidRequest),
        };
        if blocks.is_empty() {
            continue;
        }
        match entries.last_mut() {
            Some(last) if last.0 == role => last.1.extend(blocks),
            _ => entries.push((role, blocks)),
        }
    }
    Ok(entries
        .into_iter()
        .map(|(role, blocks)| {
            let blocks = if role == "user" {
                tool_results_first(blocks)
            } else {
                blocks
            };
            json!({"role": role, "content": content_of(blocks)})
        })
        .collect())
}

/// The `tool_result` blocks of a user turn come before the other blocks, as Anthropic
/// requires. A stable partition: each group keeps its order.
fn tool_results_first(blocks: Vec<Value>) -> Vec<Value> {
    let (results, others): (Vec<Value>, Vec<Value>) = blocks
        .into_iter()
        .partition(|block| block.get("type").and_then(Value::as_str) == Some("tool_result"));
    results.into_iter().chain(others).collect()
}

/// The text and `tool_use` blocks of an assistant message, in part order.
fn assistant_blocks(index: usize, message: &Message) -> Result<Vec<Value>, ChatError> {
    let mut blocks = Vec::new();
    for part in &message.parts {
        match part {
            Part::Text { content, .. } => blocks.push(text_block(content)),
            Part::ToolCall {
                id,
                name,
                arguments,
                ..
            } => {
                let input = arguments_object(index, arguments.as_ref())?;
                blocks.push(json!({
                    "type": "tool_use",
                    "id": id.as_deref().unwrap_or_default(),
                    "name": name,
                    "input": Value::Object(input),
                }));
            }
            _ => {}
        }
    }
    Ok(blocks)
}

/// The `tool_result` blocks of a tool message.
fn tool_result_blocks(message: &Message) -> Result<Vec<Value>, ChatError> {
    let mut blocks = Vec::new();
    for part in &message.parts {
        if let Part::ToolCallResponse { id, response, .. } = part {
            blocks.push(json!({
                "type": "tool_result",
                "tool_use_id": id.as_deref().unwrap_or_default(),
                "content": response_text(response)?,
            }));
        }
    }
    Ok(blocks)
}

/// One text block is sent as a JSON string; any other blocks as an array.
fn content_of(blocks: Vec<Value>) -> Value {
    if let [block] = blocks.as_slice()
        && block.get("type").and_then(Value::as_str) == Some("text")
        && let Some(text) = block.get("text").and_then(Value::as_str)
    {
        return Value::String(text.to_owned());
    }
    Value::Array(blocks)
}

/// The stop reason, with Anthropic's priority: truncation, then refusal.
fn finish_reason(stop_reason: Option<&str>, refusal_details: bool) -> Option<FinishReason> {
    match stop_reason {
        Some("max_tokens") => Some(FinishReason::Length),
        _ if stop_reason == Some("refusal") || refusal_details => Some(FinishReason::Refusal),
        Some("end_turn" | "stop_sequence") => Some(FinishReason::Stop),
        Some("tool_use") => Some(FinishReason::ToolCall),
        Some(other) => other_finish(other),
        None => None,
    }
}

/// Reads a Messages body. A body that is not JSON decodes to an empty reply.
pub(crate) fn decode_reply(body: &[u8]) -> ChatReply {
    let value: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    let usage = value
        .get("usage")
        .filter(|usage| usage.is_object())
        .map(|usage| usage_of(Some(usage)));
    let response_model = value
        .get("model")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    let finish = finish_reason(
        value.get("stop_reason").and_then(Value::as_str),
        stop_details_refusal(&value),
    );
    let content = value
        .get("content")
        .and_then(Value::as_array)
        .map(|blocks| ReplyContent {
            text: text_of(blocks),
            tool_calls: tool_calls_of(blocks),
        });
    ChatReply {
        finish_reason: finish,
        content,
        usage,
        response_model,
    }
}

/// The `tool_use` blocks that carry an `input` key, in order. A missing id becomes
/// `call_<n>`, with `n` the number of calls before it.
fn tool_calls_of(blocks: &[Value]) -> Vec<ToolCall> {
    let mut calls = Vec::new();
    for block in blocks {
        if block.get("type").and_then(Value::as_str) != Some("tool_use") {
            continue;
        }
        let Some(input) = block.get("input") else {
            continue;
        };
        let id = block
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .map_or_else(|| format!("call_{}", calls.len()), str::to_owned);
        calls.push(ToolCall {
            id,
            name: block
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            arguments: input.clone(),
        });
    }
    calls
}

/// The `text` of the `text` blocks, joined with no separator. `None` when there is none.
fn text_of(blocks: &[Value]) -> Option<String> {
    let mut text = String::new();
    let mut seen = false;
    for block in blocks {
        if block.get("type").and_then(Value::as_str) != Some("text") {
            continue;
        }
        let Some(piece) = block.get("text").and_then(Value::as_str) else {
            continue;
        };
        text.push_str(piece);
        seen = true;
    }
    seen.then_some(text)
}

fn stop_details_refusal(value: &Value) -> bool {
    value
        .get("stop_details")
        .and_then(|details| details.get("type"))
        .and_then(Value::as_str)
        == Some("refusal")
}

fn usage_of(usage: Option<&Value>) -> TokenUsage {
    let Some(usage) = usage.and_then(Value::as_object) else {
        return TokenUsage::default();
    };
    let details = usage
        .get("output_tokens_details")
        .and_then(Value::as_object);
    TokenUsage {
        input: u64_field(usage, "input_tokens")
            .saturating_add(u64_field(usage, "cache_read_input_tokens"))
            .saturating_add(u64_field(usage, "cache_creation_input_tokens")),
        output: u64_field(usage, "output_tokens"),
        cache_read: u64_field(usage, "cache_read_input_tokens"),
        cache_write: u64_field(usage, "cache_creation_input_tokens"),
        reasoning: details.map_or(0, |details| u64_field(details, "thinking_tokens")),
    }
}

fn u64_field(object: &Map<String, Value>, key: &str) -> u64 {
    object.get(key).and_then(Value::as_u64).unwrap_or(0)
}

/// The state of one streamed Messages answer (D-V-19, D-V-30). Blocks are keyed by
/// their `index`.
#[derive(Default)]
pub(crate) struct StreamState {
    blocks: BTreeMap<u64, Block>,
    usage: Map<String, Value>,
    stop_reason: Option<String>,
    refusal_details: bool,
    response_model: Option<String>,
    emitted: usize,
}

/// One content block of the answer while it streams.
enum Block {
    Text,
    ToolUse {
        id: String,
        name: String,
        json: String,
    },
    Other,
}

impl StreamState {
    /// Decodes one event. `message_stop` is the end; the event type is read from the
    /// data's `type`, else from the `event:` line.
    pub(crate) fn on_event(
        &mut self,
        event: &SseEvent,
        sink: &mut Sink,
    ) -> Result<Flow, ChatError> {
        let value: Value = serde_json::from_str(&event.data).map_err(|_| ChatError::BadStream)?;
        let kind = value
            .get("type")
            .and_then(Value::as_str)
            .or(event.event.as_deref())
            .unwrap_or_default();
        let index = value.get("index").and_then(Value::as_u64).unwrap_or(0);
        match kind {
            "message_start" => {
                if let Some(message) = value.get("message") {
                    if let Some(model) = message.get("model").and_then(Value::as_str) {
                        self.response_model = Some(model.to_owned());
                    }
                    self.merge_usage(message.get("usage"));
                }
            }
            "content_block_start" => self.block_start(index, &value, sink)?,
            "content_block_delta" => self.block_delta(index, &value, sink)?,
            "content_block_stop" => {
                if let Some(Block::ToolUse { id, name, json }) = self.blocks.remove(&index) {
                    self.emit_call(id, name, &json, sink)?;
                }
            }
            "message_delta" => {
                if let Some(reason) = value.pointer("/delta/stop_reason").and_then(Value::as_str) {
                    self.stop_reason = Some(reason.to_owned());
                }
                if value
                    .pointer("/delta/stop_details/type")
                    .and_then(Value::as_str)
                    == Some("refusal")
                {
                    self.refusal_details = true;
                }
                self.merge_usage(value.get("usage"));
            }
            "message_stop" => return Ok(Flow::End),
            "error" => return Err(stream_error(Adapter::Anthropic, &value)),
            _ => {}
        }
        Ok(Flow::Continue)
    }

    fn block_start(&mut self, index: u64, value: &Value, sink: &mut Sink) -> Result<(), ChatError> {
        let block = value.get("content_block");
        let block_type = block
            .and_then(|block| block.get("type"))
            .and_then(Value::as_str);
        let state = match block_type {
            Some("text") => {
                let text = block
                    .and_then(|block| block.get("text"))
                    .and_then(Value::as_str);
                sink.text(text.unwrap_or_default())?;
                Block::Text
            }
            Some("tool_use") => Block::ToolUse {
                id: string_of(block, "id"),
                name: string_of(block, "name"),
                json: String::new(),
            },
            _ => Block::Other,
        };
        self.blocks.insert(index, state);
        Ok(())
    }

    fn block_delta(&mut self, index: u64, value: &Value, sink: &mut Sink) -> Result<(), ChatError> {
        let delta = value.get("delta");
        match delta
            .and_then(|delta| delta.get("type"))
            .and_then(Value::as_str)
        {
            Some("text_delta") => {
                let text = delta
                    .and_then(|delta| delta.get("text"))
                    .and_then(Value::as_str);
                sink.text(text.unwrap_or_default())?;
            }
            Some("input_json_delta") => {
                let part = delta
                    .and_then(|delta| delta.get("partial_json"))
                    .and_then(Value::as_str);
                if let (Some(Block::ToolUse { json, .. }), Some(part)) =
                    (self.blocks.get_mut(&index), part)
                {
                    json.push_str(part);
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// Merges a usage object: a later value replaces an earlier one, a null is skipped.
    fn merge_usage(&mut self, usage: Option<&Value>) {
        let Some(object) = usage.and_then(Value::as_object) else {
            return;
        };
        for (key, value) in object {
            if !value.is_null() {
                self.usage.insert(key.clone(), value.clone());
            }
        }
    }

    fn emit_call(
        &mut self,
        id: String,
        name: String,
        json: &str,
        sink: &mut Sink,
    ) -> Result<(), ChatError> {
        let id = if id.is_empty() {
            format!("call_{}", self.emitted)
        } else {
            id
        };
        self.emitted += 1;
        sink.tool_call(ToolCall {
            id,
            name,
            arguments: stream_arguments(json),
        })
    }

    /// Queues the tool calls still open, in index order, and the final event.
    pub(crate) fn finish(&mut self, sink: &mut Sink) -> Result<(), ChatError> {
        for (_, block) in std::mem::take(&mut self.blocks) {
            if let Block::ToolUse { id, name, json } = block {
                self.emit_call(id, name, &json, sink)?;
            }
        }
        let usage = (!self.usage.is_empty())
            .then(|| usage_of(Some(&Value::Object(std::mem::take(&mut self.usage)))));
        sink.done(ChatDone {
            finish_reason: finish_reason(self.stop_reason.as_deref(), self.refusal_details),
            usage,
            response_model: self.response_model.take(),
        });
        Ok(())
    }
}

/// A string field of an optional object, or `""`.
fn string_of(object: Option<&Value>, key: &str) -> String {
    object
        .and_then(|object| object.get(key))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use cetra_core::message::{Message, Part, Role};
    use proptest::prelude::*;
    use reqwest::Url;
    use serde_json::{Map, Value, json};

    use super::{DEFAULT_BASE, decode_reply, request_body};
    use crate::chat::{
        CHAT_IDLE_TIMEOUT, ChatParams, ChatRequest, FinishReason, PROMPT_INSTRUCTION,
        ResponseFormat, ToolChoice, ToolDefinition, endpoint, validate_model,
    };
    use crate::error::{ChatError, JudgeError};
    use crate::hostile_tests::{anthropic_reply, body, check_reply, config};
    use crate::judge::{StructuredOutput, judge_reply};

    proptest! {
        #![proptest_config(config())]

        #[test]
        fn a_hostile_message_decodes_or_fails_cleanly(bytes in body(anthropic_reply())) {
            match judge_reply(decode_reply(&bytes)) {
                Ok(reply) => check_reply(reply),
                Err(
                    JudgeError::EmptyAnswer
                    | JudgeError::Refused
                    | JudgeError::Truncated
                    | JudgeError::NotJson,
                ) => {}
                Err(other) => panic!("undocumented decode error {other:?}"),
            }
        }
    }

    fn text(content: &str) -> Part {
        Part::Text {
            content: content.to_owned(),
            extra: Map::new(),
        }
    }

    fn message(role: Role, parts: Vec<Part>) -> Message {
        Message {
            role,
            name: None,
            parts,
            finish_reason: None,
            extra: Map::new(),
        }
    }

    fn user(content: &str) -> Message {
        message(Role::User, vec![text(content)])
    }

    fn request(messages: Vec<Message>) -> ChatRequest {
        ChatRequest {
            model: "claude-sonnet-4-5".to_owned(),
            messages,
            tools: Vec::new(),
            tool_choice: None,
            response_format: None,
            params: ChatParams::default(),
            max_tokens: 16,
            timeout: Duration::from_secs(2),
            idle_timeout: CHAT_IDLE_TIMEOUT,
        }
    }

    fn body_of(mode: StructuredOutput, request: &ChatRequest) -> Value {
        serde_json::from_slice(&request_body(mode, request, false).expect("body")).expect("json")
    }

    fn decode(body: &Value) -> crate::chat::ChatReply {
        decode_reply(&serde_json::to_vec(body).expect("body"))
    }

    fn lookup_tool() -> ToolDefinition {
        ToolDefinition {
            name: "lookup".to_owned(),
            description: Some("Looks a thing up.".to_owned()),
            parameters: json!({"type": "object"}),
        }
    }

    fn judge_format() -> ResponseFormat {
        ResponseFormat {
            name: "judge_answer".to_owned(),
            schema: json!({
                "type": "object",
                "properties": {"value": {"type": "boolean"}},
                "required": ["value"],
                "additionalProperties": false
            }),
            strict: true,
            tool_name: "submit_judgement".to_owned(),
            tool_description: "Submit your judgement.".to_owned(),
        }
    }

    #[test]
    fn joins_the_default_and_a_custom_base() {
        let default = endpoint(None, DEFAULT_BASE, "v1/messages").expect("default base");
        assert_eq!(default.as_str(), "https://api.anthropic.com/v1/messages");
        for base in ["https://api.anthropic.com", "https://api.anthropic.com/"] {
            let url = endpoint(
                Some(&Url::parse(base).expect(base)),
                DEFAULT_BASE,
                "v1/messages",
            )
            .expect(base);
            assert_eq!(url.as_str(), "https://api.anthropic.com/v1/messages");
        }
        let custom = Url::parse("https://example.com/anthropic?version=1").expect("custom");
        let url = endpoint(Some(&custom), DEFAULT_BASE, "v1/messages").expect("custom");
        assert_eq!(url.path(), "/anthropic/v1/messages");
        assert_eq!(url.query(), Some("version=1"));
    }

    #[test]
    fn rejects_a_model_name_with_a_slash() {
        assert!(validate_model("claude-sonnet-4-5").is_ok());
        assert_eq!(
            validate_model("claude/sonnet"),
            Err(ChatError::InvalidModel)
        );
    }

    #[test]
    fn encodes_a_tool_conversation() {
        let messages = vec![
            message(Role::System, vec![text("Be brief.")]),
            message(Role::User, vec![text("Hi"), text("there")]),
            message(
                Role::Assistant,
                vec![
                    text("Checking."),
                    Part::ToolCall {
                        id: Some("call_1".to_owned()),
                        name: "get_weather".to_owned(),
                        arguments: Some(json!({"city": "Paris"})),
                        extra: Map::new(),
                    },
                ],
            ),
            message(
                Role::Tool,
                vec![Part::ToolCallResponse {
                    id: Some("call_1".to_owned()),
                    response: json!({"temp": 21}),
                    extra: Map::new(),
                }],
            ),
            message(
                Role::Assistant,
                vec![
                    Part::Reasoning {
                        content: "hidden".to_owned(),
                        extra: Map::new(),
                    },
                    text("21 degrees."),
                ],
            ),
        ];
        let body = body_of(StructuredOutput::Native, &request(messages));
        assert_eq!(
            body["system"],
            json!([{"type": "text", "text": "Be brief."}])
        );
        assert_eq!(
            body["messages"],
            json!([
                {"role": "user", "content": [
                    {"type": "text", "text": "Hi"},
                    {"type": "text", "text": "there"}
                ]},
                {"role": "assistant", "content": [
                    {"type": "text", "text": "Checking."},
                    {"type": "tool_use", "id": "call_1", "name": "get_weather", "input": {"city": "Paris"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "call_1", "content": "{\"temp\":21}"}
                ]},
                {"role": "assistant", "content": "21 degrees."}
            ])
        );
    }

    #[test]
    fn merges_a_tool_result_with_the_next_user_message() {
        let messages = vec![
            user("q"),
            message(
                Role::Assistant,
                vec![Part::ToolCall {
                    id: Some("call_1".to_owned()),
                    name: "get_weather".to_owned(),
                    arguments: None,
                    extra: Map::new(),
                }],
            ),
            message(
                Role::Tool,
                vec![Part::ToolCallResponse {
                    id: Some("call_1".to_owned()),
                    response: json!("sunny"),
                    extra: Map::new(),
                }],
            ),
            user("and?"),
        ];
        let body = body_of(StructuredOutput::Native, &request(messages));
        let entries = body["messages"].as_array().expect("messages");
        assert_eq!(entries.len(), 3, "{entries:?}");
        assert_eq!(
            entries[2],
            json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "call_1", "content": "sunny"},
                {"type": "text", "text": "and?"}
            ]})
        );
    }

    #[test]
    fn tool_results_lead_a_merged_user_turn() {
        let messages = vec![
            user("q"),
            message(
                Role::Assistant,
                vec![Part::ToolCall {
                    id: Some("c1".to_owned()),
                    name: "t".to_owned(),
                    arguments: Some(json!({})),
                    extra: Map::new(),
                }],
            ),
            user("x"),
            message(
                Role::Tool,
                vec![Part::ToolCallResponse {
                    id: Some("c1".to_owned()),
                    response: json!("ok"),
                    extra: Map::new(),
                }],
            ),
        ];
        let body = body_of(StructuredOutput::Native, &request(messages));
        let entries = body["messages"].as_array().expect("messages");
        assert_eq!(
            entries.last(),
            Some(&json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "c1", "content": "ok"},
                {"type": "text", "text": "x"}
            ]}))
        );
    }

    #[test]
    fn tool_call_arguments_must_be_an_object() {
        let messages = vec![
            user("q"),
            message(
                Role::Assistant,
                vec![Part::ToolCall {
                    id: Some("call_1".to_owned()),
                    name: "get_weather".to_owned(),
                    arguments: Some(json!("[1]")),
                    extra: Map::new(),
                }],
            ),
        ];
        let err = request_body(StructuredOutput::Native, &request(messages), false)
            .expect_err("arguments are not an object");
        assert_eq!(
            err,
            ChatError::UnsupportedMessage {
                index: 1,
                reason: "tool call arguments are not a JSON object",
            }
        );
    }

    #[test]
    fn encodes_params_tools_and_choices() {
        let mut with_params = request(vec![user("Hi")]);
        with_params.params = ChatParams {
            temperature: Some(0.5),
            top_p: Some(0.9),
            stop: vec!["END".to_owned()],
            presence_penalty: Some(0.1),
            frequency_penalty: Some(0.2),
            seed: Some(7),
            reasoning_effort: Some("low".to_owned()),
        };
        let body = body_of(StructuredOutput::Prompt, &with_params);
        assert_eq!(body["temperature"], json!(0.5));
        assert_eq!(body["top_p"], json!(0.9));
        assert_eq!(body["stop_sequences"], json!(["END"]));
        for key in [
            "presence_penalty",
            "frequency_penalty",
            "seed",
            "reasoning_effort",
            "stop",
        ] {
            assert!(body.get(key).is_none(), "{key} is not sent");
        }

        let mut tools = request(vec![user("Hi")]);
        tools.tools = vec![
            lookup_tool(),
            ToolDefinition {
                name: "bare".to_owned(),
                description: None,
                parameters: json!({"type": "object"}),
            },
        ];
        let choices = [
            (ToolChoice::Auto, json!({"type": "auto"})),
            (ToolChoice::None, json!({"type": "none"})),
            (ToolChoice::Required, json!({"type": "any"})),
            (
                ToolChoice::Tool("lookup".to_owned()),
                json!({"type": "tool", "name": "lookup"}),
            ),
        ];
        for (choice, expected) in choices {
            tools.tool_choice = Some(choice);
            let body = body_of(StructuredOutput::Native, &tools);
            assert_eq!(body["tool_choice"], expected);
        }
        let body = body_of(StructuredOutput::Native, &tools);
        assert_eq!(
            body["tools"],
            json!([
                {"name": "lookup", "description": "Looks a thing up.", "input_schema": {"type": "object"}},
                {"name": "bare", "input_schema": {"type": "object"}}
            ])
        );
        let mut no_tools = request(vec![user("Hi")]);
        no_tools.tool_choice = Some(ToolChoice::Auto);
        assert!(
            body_of(StructuredOutput::Native, &no_tools)
                .get("tool_choice")
                .is_none()
        );
    }

    #[test]
    fn structured_modes_with_user_tools() {
        let mut native = request(vec![user("Grade.")]);
        native.tools = vec![lookup_tool()];
        native.response_format = Some(judge_format());
        let body = body_of(StructuredOutput::Native, &native);
        assert_eq!(
            body["output_config"],
            json!({"format": {"type": "json_schema", "schema": judge_format().schema}})
        );
        assert_eq!(body["tools"].as_array().map(Vec::len), Some(1));

        let mut tool = native;
        tool.tool_choice = Some(ToolChoice::Auto);
        let body = body_of(StructuredOutput::Tool, &tool);
        assert_eq!(
            body["tools"],
            json!([
                {"name": "lookup", "description": "Looks a thing up.", "input_schema": {"type": "object"}},
                {"name": "submit_judgement", "description": "Submit your judgement.", "input_schema": judge_format().schema}
            ])
        );
        assert_eq!(
            body["tool_choice"],
            json!({"type": "tool", "name": "submit_judgement"})
        );
        assert!(body.get("output_config").is_none());

        let mut prompt = request(vec![user("Grade.")]);
        prompt.response_format = Some(judge_format());
        let body = body_of(StructuredOutput::Prompt, &prompt);
        assert!(body.get("output_config").is_none());
        assert!(body.get("tools").is_none());
        let sent = body["messages"][0]["content"].as_str().expect("text");
        assert!(sent.starts_with("Grade.\n\n"), "{sent}");
        assert!(sent.contains(PROMPT_INSTRUCTION), "{sent}");
    }

    #[test]
    fn decode_reply_maps_stop_reasons() {
        let reason = |body: Value| decode(&body).finish_reason;
        assert_eq!(
            reason(
                json!({"stop_reason": "max_tokens", "stop_details": {"type": "refusal"}, "content": []})
            ),
            Some(FinishReason::Length)
        );
        assert_eq!(
            reason(json!({"stop_reason": "refusal", "content": []})),
            Some(FinishReason::Refusal)
        );
        assert_eq!(
            reason(
                json!({"stop_reason": "end_turn", "stop_details": {"type": "refusal"}, "content": []})
            ),
            Some(FinishReason::Refusal)
        );
        assert_eq!(
            reason(json!({"stop_reason": "end_turn", "content": []})),
            Some(FinishReason::Stop)
        );
        assert_eq!(
            reason(json!({"stop_reason": "stop_sequence", "content": []})),
            Some(FinishReason::Stop)
        );
        assert_eq!(
            reason(json!({"stop_reason": "tool_use", "content": []})),
            Some(FinishReason::ToolCall)
        );
        assert_eq!(
            reason(json!({"stop_reason": "pause_turn", "content": []})),
            Some(FinishReason::Other("pause_turn".to_owned()))
        );
        let without_content = decode(&json!({"stop_reason": "end_turn"}));
        assert_eq!(without_content.content, None);
        assert_eq!(without_content.finish_reason, Some(FinishReason::Stop));
    }

    #[test]
    fn decode_reply_skips_tool_use_without_input() {
        let reply = decode(&json!({"content": [
            {"type": "tool_use", "id": "t1", "name": "x"},
            {"type": "tool_use", "id": "t2", "name": "x", "input": {"a": 1}}
        ]}));
        let calls = reply.content.expect("content").tool_calls;
        assert_eq!(calls.len(), 1, "{calls:?}");
        assert_eq!(calls[0].id, "t2");
        assert_eq!(calls[0].arguments, json!({"a": 1}));
    }
}
