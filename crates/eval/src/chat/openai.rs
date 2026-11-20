//! OpenAI-compatible Chat Completions.
//!
//! The base URL includes the version (`https://api.openai.com/v1`). Compatible
//! servers use the same path.

use std::collections::BTreeMap;

use cetra_core::message::{Message, Part, Role};
use reqwest::header::{HeaderName, HeaderValue};
use serde_json::{Map, Value, json};

use super::sse::SseEvent;
use super::stream::{Flow, Sink};
use super::{
    ChatDone, ChatReply, ChatRequest, FinishReason, Prepared, ReplyContent, TokenUsage, ToolCall,
    ToolChoice, ToolDefinition, arguments_text, check_request, endpoint, messages_to_send, number,
    other_finish, request_headers, response_text, send_buffered, stream_arguments, stream_error,
    text_content, texts, validate_model,
};
use crate::egress::EgressClient;
use crate::error::ChatError;
use crate::judge::{Adapter, JudgeConnection, StructuredOutput};
use crate::secret::Secret;

const DEFAULT_BASE: &str = "https://api.openai.com/v1";

/// POSTs `request` to Chat Completions without streaming.
pub(crate) async fn complete(
    client: &EgressClient,
    conn: &JudgeConnection,
    request: &ChatRequest,
) -> Result<ChatReply, ChatError> {
    let prepared = prepare(conn, request, false)?;
    let body = send_buffered(client, conn, request, prepared).await?;
    Ok(decode_reply(&body))
}

/// The URL, headers and body of one Chat Completions call. `stream` adds the streaming
/// fields (D-V-19).
pub(crate) fn prepare(
    conn: &JudgeConnection,
    request: &ChatRequest,
    stream: bool,
) -> Result<Prepared, ChatError> {
    validate_model(&request.model)?;
    check_request(request)?;
    let url = endpoint(conn.base_url.as_ref(), DEFAULT_BASE, "chat/completions")?;
    let headers = request_headers(
        conn,
        vec![
            (
                HeaderName::from_static("authorization"),
                bearer(&conn.api_key)?,
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

fn bearer(key: &Secret) -> Result<HeaderValue, ChatError> {
    let mut bytes = b"Bearer ".to_vec();
    bytes.extend_from_slice(key.expose().as_bytes());
    header_bytes(&bytes)
}

fn header_bytes(bytes: &[u8]) -> Result<HeaderValue, ChatError> {
    HeaderValue::from_bytes(bytes).map_err(|_| ChatError::InvalidHeader)
}

fn request_body(
    mode: StructuredOutput,
    request: &ChatRequest,
    stream: bool,
) -> Result<Vec<u8>, ChatError> {
    let mut body = Map::new();
    body.insert("model".to_owned(), Value::String(request.model.clone()));
    let messages = messages_to_send(mode, request)?;
    body.insert(
        "messages".to_owned(),
        Value::Array(encode_messages(&messages)?),
    );
    body.insert(
        "max_completion_tokens".to_owned(),
        Value::from(request.max_tokens),
    );
    let params = &request.params;
    if let Some(temperature) = params.temperature {
        body.insert("temperature".to_owned(), number(temperature)?);
    }
    if let Some(top_p) = params.top_p {
        body.insert("top_p".to_owned(), number(top_p)?);
    }
    if !params.stop.is_empty() {
        body.insert("stop".to_owned(), json!(params.stop));
    }
    if let Some(penalty) = params.presence_penalty {
        body.insert("presence_penalty".to_owned(), number(penalty)?);
    }
    if let Some(penalty) = params.frequency_penalty {
        body.insert("frequency_penalty".to_owned(), number(penalty)?);
    }
    if let Some(seed) = params.seed {
        body.insert("seed".to_owned(), Value::from(seed));
    }
    if let Some(effort) = &params.reasoning_effort {
        body.insert("reasoning_effort".to_owned(), Value::String(effort.clone()));
    }
    let format = request.response_format.as_ref();
    if let (Some(format), StructuredOutput::Native) = (format, mode) {
        body.insert(
            "response_format".to_owned(),
            json!({
                "type": "json_schema",
                "json_schema": {
                    "name": format.name,
                    "strict": format.strict,
                    "schema": format.schema,
                }
            }),
        );
    }
    let mut tools: Vec<Value> = request.tools.iter().map(tool_json).collect();
    if let (Some(format), StructuredOutput::Tool) = (format, mode) {
        tools.push(json!({
            "type": "function",
            "function": {
                "name": format.tool_name,
                "description": format.tool_description,
                "parameters": format.schema,
                "strict": format.strict,
            }
        }));
    }
    if !tools.is_empty() {
        body.insert("tools".to_owned(), Value::Array(tools));
    }
    let choice = match (format, mode) {
        (Some(format), StructuredOutput::Tool) => Some(json!({
            "type": "function",
            "function": {"name": format.tool_name}
        })),
        // A provider refuses a tool choice when there are no tools.
        _ if !request.tools.is_empty() => request.tool_choice.as_ref().map(choice_json),
        _ => None,
    };
    if let Some(choice) = choice {
        body.insert("tool_choice".to_owned(), choice);
    }
    if stream {
        body.insert("stream".to_owned(), Value::Bool(true));
        body.insert("stream_options".to_owned(), json!({"include_usage": true}));
    }
    serde_json::to_vec(&Value::Object(body)).map_err(|_| ChatError::InvalidRequest)
}

fn tool_json(tool: &ToolDefinition) -> Value {
    let mut function = Map::new();
    function.insert("name".to_owned(), Value::String(tool.name.clone()));
    if let Some(description) = &tool.description {
        function.insert("description".to_owned(), Value::String(description.clone()));
    }
    function.insert("parameters".to_owned(), tool.parameters.clone());
    json!({"type": "function", "function": Value::Object(function)})
}

fn choice_json(choice: &ToolChoice) -> Value {
    match choice {
        ToolChoice::Auto => json!("auto"),
        ToolChoice::None => json!("none"),
        ToolChoice::Required => json!("required"),
        ToolChoice::Tool(name) => json!({"type": "function", "function": {"name": name}}),
    }
}

/// The request's messages as Chat Completions messages.
///
/// The ids of tool-call and tool-result parts are read with `unwrap_or_default`:
/// [`check_request`] refuses a missing or empty id, so the default is never sent.
fn encode_messages(messages: &[Message]) -> Result<Vec<Value>, ChatError> {
    let mut out = Vec::new();
    for message in messages {
        match &message.role {
            Role::System | Role::User => {
                let parts_text = texts(message);
                if !parts_text.is_empty() {
                    out.push(json!({
                        "role": message.role.as_str(),
                        "content": text_content(&parts_text),
                    }));
                }
            }
            Role::Assistant => encode_assistant(message, &mut out)?,
            Role::Tool => {
                for part in &message.parts {
                    if let Part::ToolCallResponse { id, response, .. } = part {
                        out.push(json!({
                            "role": "tool",
                            "tool_call_id": id.as_deref().unwrap_or_default(),
                            "content": response_text(response)?,
                        }));
                    }
                }
            }
            Role::Other(_) => return Err(ChatError::InvalidRequest),
        }
    }
    Ok(out)
}

fn encode_assistant(message: &Message, out: &mut Vec<Value>) -> Result<(), ChatError> {
    let mut calls = Vec::new();
    for part in &message.parts {
        if let Part::ToolCall {
            id,
            name,
            arguments,
            ..
        } = part
        {
            calls.push(json!({
                "id": id.as_deref().unwrap_or_default(),
                "type": "function",
                "function": {
                    "name": name,
                    "arguments": arguments_text(arguments.as_ref())?,
                },
            }));
        }
    }
    let parts_text = texts(message);
    if parts_text.is_empty() && calls.is_empty() {
        return Ok(());
    }
    let content = if parts_text.is_empty() {
        Value::Null
    } else {
        text_content(&parts_text)
    };
    let mut entry = Map::new();
    entry.insert("role".to_owned(), Value::String("assistant".to_owned()));
    entry.insert("content".to_owned(), content);
    if !calls.is_empty() {
        entry.insert("tool_calls".to_owned(), Value::Array(calls));
    }
    out.push(Value::Object(entry));
    Ok(())
}

fn finish_reason(raw: &str) -> Option<FinishReason> {
    match raw {
        "stop" => Some(FinishReason::Stop),
        "length" => Some(FinishReason::Length),
        "tool_calls" | "function_call" => Some(FinishReason::ToolCall),
        "content_filter" => Some(FinishReason::ContentFilter),
        other => other_finish(other),
    }
}

/// Reads a Chat Completions body. A body that is not JSON decodes to an empty reply.
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
    let mut reply = ChatReply {
        finish_reason: None,
        content: None,
        usage,
        response_model,
    };
    let Some(choice) = value
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
    else {
        return reply;
    };
    let Some(message) = choice.get("message") else {
        return reply;
    };
    let refused = message
        .get("refusal")
        .and_then(Value::as_str)
        .is_some_and(|text| !text.is_empty());
    reply.finish_reason = if refused {
        Some(FinishReason::Refusal)
    } else {
        choice
            .get("finish_reason")
            .and_then(Value::as_str)
            .and_then(finish_reason)
    };
    let tool_calls = message
        .get("tool_calls")
        .and_then(Value::as_array)
        .map_or_else(Vec::new, |entries| {
            entries
                .iter()
                .enumerate()
                .map(|(index, entry)| tool_call(index, entry))
                .collect()
        });
    reply.content = Some(ReplyContent {
        text: message
            .get("content")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        tool_calls,
    });
    reply
}

/// One entry of `tool_calls`. Arguments that are not JSON stay a JSON string.
fn tool_call(index: usize, entry: &Value) -> ToolCall {
    let id = entry
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .map_or_else(|| format!("call_{index}"), str::to_owned);
    let name = entry
        .pointer("/function/name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let arguments = match entry.pointer("/function/arguments").and_then(Value::as_str) {
        Some(raw) => serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.to_owned())),
        None => Value::Null,
    };
    ToolCall {
        id,
        name,
        arguments,
    }
}

fn usage_of(usage: Option<&Value>) -> TokenUsage {
    let Some(usage) = usage.and_then(Value::as_object) else {
        return TokenUsage::default();
    };
    let prompt = usage
        .get("prompt_tokens_details")
        .and_then(Value::as_object);
    let completion = usage
        .get("completion_tokens_details")
        .and_then(Value::as_object);
    TokenUsage {
        input: u64_field(usage, "prompt_tokens"),
        output: u64_field(usage, "completion_tokens"),
        cache_read: prompt.map_or(0, |details| u64_field(details, "cached_tokens")),
        cache_write: prompt.map_or(0, |details| u64_field(details, "cache_write_tokens")),
        reasoning: completion.map_or(0, |details| u64_field(details, "reasoning_tokens")),
    }
}

fn u64_field(object: &Map<String, Value>, key: &str) -> u64 {
    object.get(key).and_then(Value::as_u64).unwrap_or(0)
}

/// The state of one streamed Chat Completions answer (D-V-19, D-V-30).
#[derive(Default)]
pub(crate) struct StreamState {
    calls: BTreeMap<u64, PartialCall>,
    finish_reason: Option<FinishReason>,
    saw_finish: bool,
    refused: bool,
    usage: Option<TokenUsage>,
    response_model: Option<String>,
    emitted: usize,
}

/// A tool call whose arguments are still arriving.
#[derive(Default)]
struct PartialCall {
    id: String,
    name: String,
    arguments: String,
}

impl StreamState {
    /// Whether a `finish_reason` arrived: the answer is complete even without `[DONE]`.
    pub(crate) fn ended(&self) -> bool {
        self.saw_finish
    }

    /// Decodes one event. `[DONE]` is the end.
    pub(crate) fn on_event(
        &mut self,
        event: &SseEvent,
        sink: &mut Sink,
    ) -> Result<Flow, ChatError> {
        if event.data == "[DONE]" {
            return Ok(Flow::End);
        }
        let value: Value = serde_json::from_str(&event.data).map_err(|_| ChatError::BadStream)?;
        if value.get("error").is_some_and(Value::is_object) {
            return Err(stream_error(Adapter::OpenAi, &value));
        }
        if let Some(model) = value.get("model").and_then(Value::as_str) {
            self.response_model = Some(model.to_owned());
        }
        if let Some(usage) = value.get("usage").filter(|usage| usage.is_object()) {
            self.usage = Some(usage_of(Some(usage)));
        }
        let Some(choice) = value.pointer("/choices/0") else {
            return Ok(Flow::Continue);
        };
        if let Some(delta) = choice.get("delta") {
            self.on_delta(delta, sink)?;
        }
        if let Some(raw) = choice.get("finish_reason").and_then(Value::as_str) {
            self.saw_finish = true;
            self.finish_reason = finish_reason(raw);
            self.flush(sink)?;
        }
        Ok(Flow::Continue)
    }

    fn on_delta(&mut self, delta: &Value, sink: &mut Sink) -> Result<(), ChatError> {
        if let Some(text) = delta.get("content").and_then(Value::as_str) {
            sink.text(text)?;
        }
        if let Some(refusal) = delta
            .get("refusal")
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
        {
            self.refused = true;
            sink.text(refusal)?;
        }
        let Some(entries) = delta.get("tool_calls").and_then(Value::as_array) else {
            return Ok(());
        };
        for (position, entry) in entries.iter().enumerate() {
            let fallback = u64::try_from(position).unwrap_or(u64::MAX);
            let index = entry
                .get("index")
                .and_then(Value::as_u64)
                .unwrap_or(fallback);
            let call = self.calls.entry(index).or_default();
            if let Some(id) = entry
                .get("id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
            {
                call.id = id.to_owned();
            }
            if let Some(name) = entry
                .pointer("/function/name")
                .and_then(Value::as_str)
                .filter(|name| !name.is_empty())
            {
                call.name = name.to_owned();
            }
            if let Some(arguments) = entry.pointer("/function/arguments").and_then(Value::as_str) {
                call.arguments.push_str(arguments);
            }
        }
        Ok(())
    }

    /// Queues the tool calls collected so far, in index order.
    fn flush(&mut self, sink: &mut Sink) -> Result<(), ChatError> {
        for call in std::mem::take(&mut self.calls).into_values() {
            let id = if call.id.is_empty() {
                format!("call_{}", self.emitted)
            } else {
                call.id
            };
            self.emitted += 1;
            sink.tool_call(ToolCall {
                id,
                name: call.name,
                arguments: stream_arguments(&call.arguments),
            })?;
        }
        Ok(())
    }

    /// Queues the remaining calls and the final event.
    pub(crate) fn finish(&mut self, sink: &mut Sink) -> Result<(), ChatError> {
        self.flush(sink)?;
        let finish_reason = if self.refused {
            Some(FinishReason::Refusal)
        } else {
            self.finish_reason.take()
        };
        sink.done(ChatDone {
            finish_reason,
            usage: self.usage.take(),
            response_model: self.response_model.take(),
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use cetra_core::message::{Message, Part, Role};
    use proptest::prelude::*;
    use reqwest::Url;
    use serde_json::{Map, Value, json};

    use super::{DEFAULT_BASE, StreamState, decode_reply, request_body};
    use crate::chat::sse::SseEvent;
    use crate::chat::stream::Sink;
    use crate::chat::{
        CHAT_IDLE_TIMEOUT, ChatParams, ChatReply, ChatRequest, FinishReason, PROMPT_INSTRUCTION,
        ResponseFormat, TokenUsage, ToolCall, ToolChoice, ToolDefinition, endpoint, validate_model,
    };
    use crate::error::{ChatError, EgressError, JudgeError};
    use crate::hostile_tests::{body, check_reply, config, openai_reply};
    use crate::judge::{StructuredOutput, judge_reply};

    fn chat_completions_url(base: Option<&Url>) -> Result<Url, ChatError> {
        endpoint(base, DEFAULT_BASE, "chat/completions")
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
            model: "gpt-4o-mini".to_owned(),
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

    fn decode(body: &Value) -> ChatReply {
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

    proptest! {
        #![proptest_config(config())]

        #[test]
        fn a_hostile_completion_decodes_or_fails_cleanly(bytes in body(openai_reply())) {
            match judge_reply(decode_reply(&bytes)) {
                Ok(reply) => check_reply(reply),
                Err(
                    JudgeError::EmptyAnswer
                    | JudgeError::Refused
                    | JudgeError::Truncated
                    | JudgeError::ContentFiltered
                    | JudgeError::NotJson,
                ) => {}
                Err(other) => panic!("undocumented decode error {other:?}"),
            }
        }
    }

    /// A stream event the layer refuses with a documented error: a malformed event, a provider
    /// error event, or a cap.
    fn documented_stream_error(outcome: Result<(), ChatError>) {
        match outcome {
            Ok(()) => {}
            Err(
                ChatError::BadStream
                | ChatError::Provider { .. }
                | ChatError::Transport(EgressError::TooLarge { .. }),
            ) => {}
            Err(other) => panic!("undocumented stream error {other:?}"),
        }
    }

    proptest! {
        #![proptest_config(config())]

        #[test]
        fn a_hostile_stream_event_decodes_or_fails_cleanly(bytes in body(openai_reply())) {
            let event = SseEvent {
                event: None,
                data: String::from_utf8_lossy(&bytes).into_owned(),
            };
            let mut state = StreamState::default();
            let mut sink = Sink::default();
            documented_stream_error(match state.on_event(&event, &mut sink) {
                Ok(_) => state.finish(&mut sink),
                Err(error) => Err(error),
            });
        }
    }

    #[test]
    fn joins_the_default_and_a_custom_base() {
        let default = chat_completions_url(None).expect("default base");
        assert_eq!(
            default.as_str(),
            "https://api.openai.com/v1/chat/completions"
        );
        for base in ["https://api.openai.com/v1", "https://api.openai.com/v1/"] {
            let url = chat_completions_url(Some(&Url::parse(base).expect(base))).expect(base);
            assert_eq!(url.as_str(), "https://api.openai.com/v1/chat/completions");
        }
        let custom =
            Url::parse("https://example.com/openai/v1?api-version=2024-10-21").expect("custom");
        let url = chat_completions_url(Some(&custom)).expect("custom");
        assert_eq!(url.path(), "/openai/v1/chat/completions");
        assert_eq!(url.query(), Some("api-version=2024-10-21"));
    }

    #[test]
    fn rejects_model_names_that_cannot_be_sent() {
        assert!(validate_model("gpt-4.1").is_ok());
        assert!(validate_model("gpt-4o_mini.1").is_ok());
        assert!(validate_model(&"a".repeat(256)).is_ok());
        for model in [
            "",
            &"a".repeat(257),
            "gpt/4",
            r"gpt\4",
            "gpt%2f4",
            "a?b",
            "a#b",
            "a b",
            "a\nb",
            "a\u{00a0}b",
            "a\u{0001}b",
        ] {
            assert_eq!(
                validate_model(model),
                Err(ChatError::InvalidModel),
                "{model:?}"
            );
        }
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
                        content: "hmm".to_owned(),
                        extra: Map::new(),
                    },
                    text("21 degrees."),
                ],
            ),
        ];
        let body = body_of(StructuredOutput::Native, &request(messages));
        assert_eq!(
            body["messages"],
            json!([
                {"role": "system", "content": "Be brief."},
                {"role": "user", "content": [
                    {"type": "text", "text": "Hi"},
                    {"type": "text", "text": "there"}
                ]},
                {"role": "assistant", "content": "Checking.", "tool_calls": [
                    {"id": "call_1", "type": "function", "function": {
                        "name": "get_weather",
                        "arguments": "{\"city\":\"Paris\"}"
                    }}
                ]},
                {"role": "tool", "tool_call_id": "call_1", "content": "{\"temp\":21}"},
                {"role": "assistant", "content": "21 degrees."}
            ])
        );
    }

    #[test]
    fn encodes_params_tools_and_choices() {
        let tooled = || ChatRequest {
            tools: vec![
                ToolDefinition {
                    name: "a".to_owned(),
                    description: Some("does a".to_owned()),
                    parameters: json!({}),
                },
                ToolDefinition {
                    name: "b".to_owned(),
                    description: None,
                    parameters: json!({}),
                },
            ],
            ..request(vec![user("Hi")])
        };
        let mut full = tooled();
        full.params = ChatParams {
            temperature: Some(0.5),
            top_p: Some(0.9),
            stop: vec!["END".to_owned()],
            presence_penalty: Some(0.1),
            frequency_penalty: Some(0.2),
            seed: Some(7),
            reasoning_effort: Some("low".to_owned()),
        };
        let body = body_of(StructuredOutput::Native, &full);
        assert_eq!(body["temperature"], json!(0.5));
        assert_eq!(body["top_p"], json!(0.9));
        assert_eq!(body["stop"], json!(["END"]));
        assert_eq!(body["presence_penalty"], json!(0.1));
        assert_eq!(body["frequency_penalty"], json!(0.2));
        assert_eq!(body["seed"], json!(7));
        assert_eq!(body["reasoning_effort"], json!("low"));
        assert_eq!(body["max_completion_tokens"], json!(16));
        assert_eq!(body["tools"][0]["function"]["description"], json!("does a"));
        assert!(body["tools"][1]["function"].get("description").is_none());
        assert!(body.get("tool_choice").is_none());

        for (choice, expected) in [
            (ToolChoice::Auto, json!("auto")),
            (ToolChoice::None, json!("none")),
            (ToolChoice::Required, json!("required")),
            (
                ToolChoice::Tool("b".to_owned()),
                json!({"type": "function", "function": {"name": "b"}}),
            ),
        ] {
            let with_choice = ChatRequest {
                tool_choice: Some(choice),
                ..tooled()
            };
            let body = body_of(StructuredOutput::Native, &with_choice);
            assert_eq!(body["tool_choice"], expected);
        }

        let plain = body_of(StructuredOutput::Native, &request(vec![user("Hi")]));
        assert!(plain.get("stop").is_none());
        assert!(plain.get("temperature").is_none());
        let no_tools = ChatRequest {
            tool_choice: Some(ToolChoice::Auto),
            ..request(vec![user("Hi")])
        };
        assert!(
            body_of(StructuredOutput::Native, &no_tools)
                .get("tool_choice")
                .is_none()
        );
    }

    #[test]
    fn structured_modes_with_user_tools() {
        let with_format = ChatRequest {
            tools: vec![lookup_tool()],
            tool_choice: Some(ToolChoice::Auto),
            response_format: Some(judge_format()),
            ..request(vec![user("Grade.")])
        };

        let native = body_of(StructuredOutput::Native, &with_format);
        assert_eq!(native["response_format"]["type"], json!("json_schema"));
        assert_eq!(
            native["response_format"]["json_schema"]["name"],
            json!("judge_answer")
        );
        assert_eq!(native["tools"].as_array().map(Vec::len), Some(1));
        assert_eq!(native["tools"][0]["function"]["name"], json!("lookup"));
        assert_eq!(native["tool_choice"], json!("auto"));

        let tool = body_of(StructuredOutput::Tool, &with_format);
        assert!(tool.get("response_format").is_none());
        assert_eq!(tool["tools"].as_array().map(Vec::len), Some(2));
        assert_eq!(tool["tools"][0]["function"]["name"], json!("lookup"));
        assert_eq!(
            tool["tools"][1]["function"]["name"],
            json!("submit_judgement")
        );
        assert_eq!(tool["tools"][1]["function"]["strict"], json!(true));
        assert_eq!(
            tool["tool_choice"],
            json!({"type": "function", "function": {"name": "submit_judgement"}})
        );

        let prompt = body_of(StructuredOutput::Prompt, &with_format);
        assert!(prompt.get("response_format").is_none());
        assert_eq!(prompt["tools"].as_array().map(Vec::len), Some(1));
        assert_eq!(prompt["tool_choice"], json!("auto"));
        let content = prompt["messages"][0]["content"].as_str().expect("text");
        assert!(content.starts_with("Grade.\n\n"), "{content}");
        assert!(content.contains(PROMPT_INSTRUCTION), "{content}");
    }

    #[test]
    fn decode_reply_maps_finish_reasons() {
        for (raw, expected) in [
            ("stop", Some(FinishReason::Stop)),
            ("length", Some(FinishReason::Length)),
            ("tool_calls", Some(FinishReason::ToolCall)),
            ("function_call", Some(FinishReason::ToolCall)),
            ("content_filter", Some(FinishReason::ContentFilter)),
            (
                "weird_one",
                Some(FinishReason::Other("weird_one".to_owned())),
            ),
            ("has space", None),
        ] {
            let body = json!({"choices": [{"finish_reason": raw, "message": {"content": "x"}}]});
            assert_eq!(decode(&body).finish_reason, expected, "{raw}");
        }

        let refused = json!({
            "choices": [{"finish_reason": "length", "message": {"refusal": "no", "content": null}}]
        });
        assert_eq!(decode(&refused).finish_reason, Some(FinishReason::Refusal));

        let no_message = decode(&json!({"choices": [{"finish_reason": "length"}]}));
        assert_eq!(no_message.finish_reason, None);
        assert_eq!(no_message.content, None);

        let no_choices = decode(&json!({"model": "m"}));
        assert_eq!(no_choices.finish_reason, None);
        assert_eq!(no_choices.content, None);
        assert_eq!(no_choices.response_model.as_deref(), Some("m"));
    }

    #[test]
    fn decode_reply_reads_tool_calls_and_usage() {
        let body = json!({
            "model": "gpt-x",
            "usage": {
                "prompt_tokens": 3,
                "completion_tokens": 4,
                "prompt_tokens_details": {"cached_tokens": 1},
                "completion_tokens_details": {"reasoning_tokens": 2}
            },
            "choices": [{"message": {"tool_calls": [
                {"type": "function", "function": {"name": "lookup", "arguments": "not json"}},
                {"id": "c2", "function": {"name": "get", "arguments": "{\"a\":1}"}}
            ]}}]
        });
        let reply = decode(&body);
        let content = reply.content.expect("content");
        assert_eq!(
            content.tool_calls,
            vec![
                ToolCall {
                    id: "call_0".to_owned(),
                    name: "lookup".to_owned(),
                    arguments: Value::String("not json".to_owned()),
                },
                ToolCall {
                    id: "c2".to_owned(),
                    name: "get".to_owned(),
                    arguments: json!({"a": 1}),
                },
            ]
        );
        assert_eq!(
            reply.usage,
            Some(TokenUsage {
                input: 3,
                output: 4,
                cache_read: 1,
                cache_write: 0,
                reasoning: 2,
            })
        );
        assert_eq!(reply.response_model.as_deref(), Some("gpt-x"));
        let without_usage = decode(&json!({"choices": [{"message": {"content": "x"}}]}));
        assert_eq!(without_usage.usage, None);
    }
}
