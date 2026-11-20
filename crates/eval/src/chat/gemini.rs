//! Gemini `generateContent`.
//!
//! The base URL is the host (`https://generativelanguage.googleapis.com`). The
//! model is a path segment, `v1beta/models/{model}:generateContent`. The API key
//! is the `x-goog-api-key` header and is never added to the query string.

use std::collections::HashMap;

use cetra_core::message::{Message, Part, Role};
use reqwest::Url;
use reqwest::header::{HeaderName, HeaderValue};
use serde_json::{Map, Value, json};

use super::sse::SseEvent;
use super::stream::{Flow, Sink};
use super::{
    ChatDone, ChatReply, ChatRequest, FinishReason, Prepared, ReplyContent, TokenUsage, ToolCall,
    ToolChoice, ToolDefinition, arguments_object, check_request, endpoint, messages_to_send,
    number, other_finish, request_headers, secret_header, send_buffered, stream_error, texts,
    validate_model,
};
use crate::egress::EgressClient;
use crate::error::ChatError;
use crate::judge::{Adapter, JudgeConnection, StructuredOutput};

const DEFAULT_BASE: &str = "https://generativelanguage.googleapis.com";

const FILTERED_FINISH: &[&str] = &[
    "SAFETY",
    "PROHIBITED_CONTENT",
    "BLOCKLIST",
    "SPII",
    "RECITATION",
];

/// POSTs `request` to `generateContent` without streaming.
pub(crate) async fn complete(
    client: &EgressClient,
    conn: &JudgeConnection,
    request: &ChatRequest,
) -> Result<ChatReply, ChatError> {
    let prepared = prepare(conn, request, false)?;
    let body = send_buffered(client, conn, request, prepared).await?;
    Ok(decode_reply(&body))
}

/// The URL, headers and body of one `generateContent` call. `stream` selects
/// `streamGenerateContent?alt=sse` (D-V-19).
pub(crate) fn prepare(
    conn: &JudgeConnection,
    request: &ChatRequest,
    stream: bool,
) -> Result<Prepared, ChatError> {
    validate_model(&request.model)?;
    check_request(request)?;
    let url = generate_content_url(conn.base_url.as_ref(), &request.model, stream)?;
    let headers = request_headers(
        conn,
        vec![
            (
                HeaderName::from_static("x-goog-api-key"),
                secret_header(&conn.api_key)?,
            ),
            (
                HeaderName::from_static("content-type"),
                HeaderValue::from_static("application/json"),
            ),
        ],
    )?;
    let body = request_body(conn.structured_output, request)?;
    Ok(Prepared { url, headers, body })
}

fn generate_content_url(base: Option<&Url>, model: &str, stream: bool) -> Result<Url, ChatError> {
    validate_model(model)?;
    let method = if stream {
        "streamGenerateContent"
    } else {
        "generateContent"
    };
    let path = format!("v1beta/models/{model}:{method}");
    let mut url = endpoint(base, DEFAULT_BASE, &path)?;
    if stream {
        url.query_pairs_mut().append_pair("alt", "sse");
    }
    Ok(url)
}

fn request_body(mode: StructuredOutput, request: &ChatRequest) -> Result<Vec<u8>, ChatError> {
    let mut body = Map::new();
    let messages = messages_to_send(mode, request)?;
    let system: Vec<Value> = messages
        .iter()
        .filter(|message| message.role == Role::System)
        .flat_map(texts)
        .map(|text| json!({"text": text}))
        .collect();
    if !system.is_empty() {
        body.insert("systemInstruction".to_owned(), json!({"parts": system}));
    }
    body.insert(
        "contents".to_owned(),
        Value::Array(encode_messages(&messages)?),
    );
    let mut generation = Map::new();
    generation.insert(
        "maxOutputTokens".to_owned(),
        Value::from(request.max_tokens),
    );
    let params = &request.params;
    if let Some(temperature) = params.temperature {
        generation.insert("temperature".to_owned(), number(temperature)?);
    }
    if let Some(top_p) = params.top_p {
        generation.insert("topP".to_owned(), number(top_p)?);
    }
    if !params.stop.is_empty() {
        generation.insert("stopSequences".to_owned(), json!(params.stop));
    }
    if let Some(penalty) = params.presence_penalty {
        generation.insert("presencePenalty".to_owned(), number(penalty)?);
    }
    if let Some(penalty) = params.frequency_penalty {
        generation.insert("frequencyPenalty".to_owned(), number(penalty)?);
    }
    if let Some(seed) = params.seed {
        generation.insert("seed".to_owned(), Value::from(seed));
    }
    let format = request.response_format.as_ref();
    if let (Some(format), StructuredOutput::Native) = (format, mode) {
        generation.insert(
            "responseMimeType".to_owned(),
            Value::String("application/json".to_owned()),
        );
        generation.insert("responseJsonSchema".to_owned(), format.schema.clone());
    }
    body.insert("generationConfig".to_owned(), Value::Object(generation));
    let mut declarations: Vec<Value> = request.tools.iter().map(declaration).collect();
    if let (Some(format), StructuredOutput::Tool) = (format, mode) {
        declarations.push(json!({
            "name": format.tool_name,
            "description": format.tool_description,
            "parametersJsonSchema": format.schema,
        }));
    }
    if !declarations.is_empty() {
        body.insert(
            "tools".to_owned(),
            json!([{"functionDeclarations": declarations}]),
        );
    }
    let config = match (format, mode) {
        (Some(format), StructuredOutput::Tool) => Some(json!({
            "functionCallingConfig": {
                "mode": "ANY",
                "allowedFunctionNames": [format.tool_name],
            }
        })),
        // A provider refuses a tool config when there are no tools.
        _ if !request.tools.is_empty() => request.tool_choice.as_ref().map(tool_config),
        _ => None,
    };
    if let Some(config) = config {
        body.insert("toolConfig".to_owned(), config);
    }
    serde_json::to_vec(&Value::Object(body)).map_err(|_| ChatError::InvalidRequest)
}

/// One function declaration. `description` is sent only when set.
fn declaration(tool: &ToolDefinition) -> Value {
    let mut entry = Map::new();
    entry.insert("name".to_owned(), Value::String(tool.name.clone()));
    if let Some(description) = &tool.description {
        entry.insert("description".to_owned(), Value::String(description.clone()));
    }
    entry.insert("parametersJsonSchema".to_owned(), tool.parameters.clone());
    Value::Object(entry)
}

fn tool_config(choice: &ToolChoice) -> Value {
    match choice {
        ToolChoice::Auto => json!({"functionCallingConfig": {"mode": "AUTO"}}),
        ToolChoice::None => json!({"functionCallingConfig": {"mode": "NONE"}}),
        ToolChoice::Required => json!({"functionCallingConfig": {"mode": "ANY"}}),
        ToolChoice::Tool(name) => json!({
            "functionCallingConfig": {"mode": "ANY", "allowedFunctionNames": [name]}
        }),
    }
}

/// The request's non-system messages as `contents` entries (D-V-31).
///
/// A tool result names the function of the earlier call with the same id, so it
/// is refused when no earlier call has that id. A message with no parts is skipped,
/// and a message whose role is the same as the one before it is merged into that
/// message. The ids of tool-call and tool-result parts are read with
/// `unwrap_or_default`: [`check_request`] refuses a missing or empty id, so the
/// default is never sent.
fn encode_messages(messages: &[Message]) -> Result<Vec<Value>, ChatError> {
    let mut names: HashMap<&str, &str> = HashMap::new();
    let mut entries: Vec<(&'static str, Vec<Value>)> = Vec::new();
    for (index, message) in messages.iter().enumerate() {
        let (role, parts) = match &message.role {
            Role::System => continue,
            Role::User => (
                "user",
                texts(message)
                    .into_iter()
                    .map(|text| json!({"text": text}))
                    .collect::<Vec<Value>>(),
            ),
            Role::Assistant => ("model", assistant_parts(index, message, &mut names)?),
            Role::Tool => ("user", function_responses(index, message, &names)?),
            Role::Other(_) => return Err(ChatError::InvalidRequest),
        };
        if parts.is_empty() {
            continue;
        }
        match entries.last_mut() {
            Some(last) if last.0 == role => last.1.extend(parts),
            _ => entries.push((role, parts)),
        }
    }
    Ok(entries
        .into_iter()
        .map(|(role, parts)| json!({"role": role, "parts": parts}))
        .collect())
}

/// The text and `functionCall` parts of an assistant message, in part order. Each call's
/// name is recorded for the tool results that follow it. The id is not sent.
fn assistant_parts<'a>(
    index: usize,
    message: &'a Message,
    names: &mut HashMap<&'a str, &'a str>,
) -> Result<Vec<Value>, ChatError> {
    let mut parts = Vec::new();
    for part in &message.parts {
        match part {
            Part::Text { content, .. } => parts.push(json!({"text": content})),
            Part::ToolCall {
                id,
                name,
                arguments,
                ..
            } => {
                names.insert(id.as_deref().unwrap_or_default(), name.as_str());
                let args = arguments_object(index, arguments.as_ref())?;
                parts.push(json!({
                    "functionCall": {"name": name, "args": Value::Object(args)}
                }));
            }
            _ => {}
        }
    }
    Ok(parts)
}

/// The `functionResponse` parts of a tool message. A result that is not an object is
/// wrapped as `{"result": …}`.
fn function_responses(
    index: usize,
    message: &Message,
    names: &HashMap<&str, &str>,
) -> Result<Vec<Value>, ChatError> {
    let mut parts = Vec::new();
    for part in &message.parts {
        if let Part::ToolCallResponse { id, response, .. } = part {
            let name = names
                .get(id.as_deref().unwrap_or_default())
                .copied()
                .ok_or(ChatError::UnsupportedMessage {
                    index,
                    reason: "tool result for an unknown tool call",
                })?;
            let result = if response.is_object() {
                response.clone()
            } else {
                json!({"result": response})
            };
            parts.push(json!({
                "functionResponse": {"name": name, "response": result}
            }));
        }
    }
    Ok(parts)
}

/// The stop reason. Truncation and filtering are checked before the rest.
fn finish_reason(raw: &str) -> Option<FinishReason> {
    match raw {
        "MAX_TOKENS" => Some(FinishReason::Length),
        _ if FILTERED_FINISH.contains(&raw) => Some(FinishReason::ContentFilter),
        "STOP" => Some(FinishReason::Stop),
        other => other_finish(other),
    }
}

/// Reads a `generateContent` body. A body that is not JSON decodes to an empty reply.
pub(crate) fn decode_reply(body: &[u8]) -> ChatReply {
    let value: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    let usage = value
        .get("usageMetadata")
        .filter(|usage| usage.is_object())
        .map(|usage| usage_of(Some(usage)));
    let response_model = value
        .get("modelVersion")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    let mut reply = ChatReply {
        finish_reason: None,
        content: None,
        usage,
        response_model,
    };
    if blocked(&value) {
        reply.finish_reason = Some(FinishReason::ContentFilter);
        return reply;
    }
    let Some(candidate) = value
        .get("candidates")
        .and_then(Value::as_array)
        .and_then(|candidates| candidates.first())
    else {
        return reply;
    };
    reply.finish_reason = candidate
        .get("finishReason")
        .and_then(Value::as_str)
        .and_then(finish_reason);
    let Some(parts) = candidate
        .pointer("/content/parts")
        .and_then(Value::as_array)
    else {
        return reply;
    };
    reply.content = Some(ReplyContent {
        text: text_of(parts),
        tool_calls: tool_calls_of(parts),
    });
    reply
}

/// The `functionCall` parts that carry an `args` key, in order. A missing id becomes
/// `call_<n>`, with `n` the number of calls before it.
fn tool_calls_of(parts: &[Value]) -> Vec<ToolCall> {
    let mut calls = Vec::new();
    for part in parts {
        let Some(call) = part.get("functionCall") else {
            continue;
        };
        let Some(args) = call.get("args") else {
            continue;
        };
        let id = call
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .map_or_else(|| format!("call_{}", calls.len()), str::to_owned);
        calls.push(ToolCall {
            id,
            name: call
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            arguments: args.clone(),
        });
    }
    calls
}

/// The `text` of the parts that are not thoughts, joined with no separator. `None` when
/// there is no text part.
fn text_of(parts: &[Value]) -> Option<String> {
    let mut text = String::new();
    let mut seen = false;
    for part in parts {
        if part.get("thought").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        let Some(piece) = part.get("text").and_then(Value::as_str) else {
            continue;
        };
        text.push_str(piece);
        seen = true;
    }
    seen.then_some(text)
}

fn blocked(value: &Value) -> bool {
    value
        .pointer("/promptFeedback/blockReason")
        .and_then(Value::as_str)
        .is_some_and(|reason| !reason.is_empty())
}

fn usage_of(usage: Option<&Value>) -> TokenUsage {
    let Some(usage) = usage.and_then(Value::as_object) else {
        return TokenUsage::default();
    };
    let reasoning = u64_field(usage, "thoughtsTokenCount");
    TokenUsage {
        input: u64_field(usage, "promptTokenCount"),
        output: u64_field(usage, "candidatesTokenCount").saturating_add(reasoning),
        cache_read: u64_field(usage, "cachedContentTokenCount"),
        cache_write: 0,
        reasoning,
    }
}

fn u64_field(object: &Map<String, Value>, key: &str) -> u64 {
    object.get(key).and_then(Value::as_u64).unwrap_or(0)
}

/// The state of one streamed `streamGenerateContent` answer (D-V-19, D-V-30). The
/// stream ends with the body, so the end is the last event that carries a finish reason
/// or a block.
#[derive(Default)]
pub(crate) struct StreamState {
    finish_reason: Option<FinishReason>,
    saw_finish: bool,
    blocked: bool,
    usage: Option<TokenUsage>,
    response_model: Option<String>,
    emitted: usize,
}

impl StreamState {
    /// Whether a finish reason or a block arrived: the answer is complete.
    pub(crate) fn ended(&self) -> bool {
        self.saw_finish || self.blocked
    }

    /// Decodes one event. Thoughts are skipped, as in a buffered answer.
    pub(crate) fn on_event(
        &mut self,
        event: &SseEvent,
        sink: &mut Sink,
    ) -> Result<Flow, ChatError> {
        let value: Value = serde_json::from_str(&event.data).map_err(|_| ChatError::BadStream)?;
        if value.get("error").is_some_and(Value::is_object) {
            return Err(stream_error(Adapter::Gemini, &value));
        }
        if let Some(model) = value.get("modelVersion").and_then(Value::as_str) {
            self.response_model = Some(model.to_owned());
        }
        if let Some(usage) = value.get("usageMetadata").filter(|usage| usage.is_object()) {
            self.usage = Some(usage_of(Some(usage)));
        }
        if blocked(&value) {
            self.blocked = true;
        }
        let Some(candidate) = value.pointer("/candidates/0") else {
            return Ok(Flow::Continue);
        };
        if let Some(parts) = candidate
            .pointer("/content/parts")
            .and_then(Value::as_array)
        {
            for part in parts {
                if part.get("thought").and_then(Value::as_bool) == Some(true) {
                    continue;
                }
                if let Some(text) = part.get("text").and_then(Value::as_str) {
                    sink.text(text)?;
                }
                if let Some(call) = part.get("functionCall").filter(|call| call.is_object()) {
                    self.function_call(call, sink)?;
                }
            }
        }
        if let Some(raw) = candidate.get("finishReason").and_then(Value::as_str) {
            self.saw_finish = true;
            self.finish_reason = finish_reason(raw);
        }
        Ok(Flow::Continue)
    }

    /// Queues one complete `functionCall`. A missing id becomes `call_<n>`; missing
    /// `args` is an empty object.
    fn function_call(&mut self, call: &Value, sink: &mut Sink) -> Result<(), ChatError> {
        let id = call
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .map_or_else(|| format!("call_{}", self.emitted), str::to_owned);
        self.emitted += 1;
        let name = call
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let arguments = call
            .get("args")
            .cloned()
            .unwrap_or_else(|| Value::Object(Map::new()));
        sink.tool_call(ToolCall {
            id,
            name,
            arguments,
        })
    }

    /// Queues the final event. A block wins over the finish reason.
    pub(crate) fn finish(&mut self, sink: &mut Sink) {
        let finish_reason = if self.blocked {
            Some(FinishReason::ContentFilter)
        } else {
            self.finish_reason.take()
        };
        sink.done(ChatDone {
            finish_reason,
            usage: self.usage.take(),
            response_model: self.response_model.take(),
        });
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use cetra_core::message::{Message, Part, Role};
    use proptest::prelude::*;
    use reqwest::Url;
    use serde_json::{Map, Value, json};

    use super::{StreamState, decode_reply, generate_content_url, request_body};
    use crate::chat::sse::SseEvent;
    use crate::chat::stream::Sink;
    use crate::chat::{
        CHAT_IDLE_TIMEOUT, ChatParams, ChatRequest, FinishReason, ToolCall, ToolChoice,
        ToolDefinition,
    };
    use crate::error::{ChatError, EgressError, JudgeError};
    use crate::hostile_tests::{body, check_reply, config, gemini_reply};
    use crate::judge::{StructuredOutput, judge_reply};

    proptest! {
        #![proptest_config(config())]

        #[test]
        fn a_hostile_generation_decodes_or_fails_cleanly(bytes in body(gemini_reply())) {
            match judge_reply(decode_reply(&bytes)) {
                Ok(reply) => check_reply(reply),
                Err(
                    JudgeError::EmptyAnswer
                    | JudgeError::ContentFiltered
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
            model: "gemini-2.5-flash".to_owned(),
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
        serde_json::from_slice(&request_body(mode, request).expect("body")).expect("json")
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
        fn a_hostile_stream_event_decodes_or_fails_cleanly(bytes in body(gemini_reply())) {
            let event = SseEvent {
                event: None,
                data: String::from_utf8_lossy(&bytes).into_owned(),
            };
            let mut state = StreamState::default();
            let mut sink = Sink::default();
            let outcome = match state.on_event(&event, &mut sink) {
                Ok(_) => {
                    state.finish(&mut sink);
                    Ok(())
                }
                Err(error) => Err(error),
            };
            documented_stream_error(outcome);
        }
    }

    #[test]
    fn puts_the_model_in_the_path_and_keeps_the_key_out_of_the_query() {
        let default = generate_content_url(None, "gemini-2.5-flash", false).expect("default base");
        assert_eq!(
            default.as_str(),
            "https://generativelanguage.googleapis.com/v1beta/models/gemini-2.5-flash:generateContent"
        );
        assert!(default.query().is_none());
        for base in [
            "https://generativelanguage.googleapis.com",
            "https://generativelanguage.googleapis.com/",
        ] {
            let url = generate_content_url(
                Some(&Url::parse(base).expect(base)),
                "gemini-2.5-flash",
                false,
            )
            .expect(base);
            assert_eq!(url.as_str(), default.as_str());
        }
        let custom = Url::parse("https://example.com/gemini?version=1").expect("custom");
        let url = generate_content_url(Some(&custom), "gemini-2.5-flash", false).expect("custom");
        assert_eq!(
            url.path(),
            "/gemini/v1beta/models/gemini-2.5-flash:generateContent"
        );
        assert_eq!(url.query(), Some("version=1"));
        assert!(!url.as_str().contains("key="));
        let streamed =
            generate_content_url(Some(&custom), "gemini-2.5-flash", true).expect("streamed");
        assert_eq!(
            streamed.path(),
            "/gemini/v1beta/models/gemini-2.5-flash:streamGenerateContent"
        );
        assert_eq!(streamed.query(), Some("version=1&alt=sse"));
    }

    #[test]
    fn rejects_a_model_name_before_building_a_url() {
        let err = generate_content_url(None, "gemini/flash", false).expect_err("slash");
        assert_eq!(err, ChatError::InvalidModel);
    }

    #[test]
    fn rejects_a_model_that_would_leave_the_models_path() {
        let custom = Url::parse("https://example.com/gemini/?version=1").expect("custom");
        for model in [r"a\..\..\..\admin", "a%2f..%2f..%2fadmin"] {
            let err = generate_content_url(Some(&custom), model, true).expect_err(model);
            assert_eq!(err, ChatError::InvalidModel, "{model:?}");
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
                        content: "hidden".to_owned(),
                        extra: Map::new(),
                    },
                    text("21 degrees."),
                ],
            ),
        ];
        let body = body_of(StructuredOutput::Native, &request(messages));
        assert_eq!(
            body["systemInstruction"],
            json!({"parts": [{"text": "Be brief."}]})
        );
        assert_eq!(
            body["contents"],
            json!([
                {"role": "user", "parts": [{"text": "Hi"}, {"text": "there"}]},
                {"role": "model", "parts": [
                    {"text": "Checking."},
                    {"functionCall": {"name": "get_weather", "args": {"city": "Paris"}}}
                ]},
                {"role": "user", "parts": [
                    {"functionResponse": {"name": "get_weather", "response": {"temp": 21}}}
                ]},
                {"role": "model", "parts": [{"text": "21 degrees."}]}
            ])
        );
    }

    #[test]
    fn wraps_a_non_object_tool_result() {
        let messages = vec![
            user("q"),
            message(
                Role::Assistant,
                vec![Part::ToolCall {
                    id: Some("c1".to_owned()),
                    name: "t".to_owned(),
                    arguments: None,
                    extra: Map::new(),
                }],
            ),
            message(
                Role::Tool,
                vec![Part::ToolCallResponse {
                    id: Some("c1".to_owned()),
                    response: json!("sunny"),
                    extra: Map::new(),
                }],
            ),
        ];
        let body = body_of(StructuredOutput::Native, &request(messages));
        assert_eq!(
            body["contents"][2]["parts"][0]["functionResponse"]["response"],
            json!({"result": "sunny"})
        );
    }

    #[test]
    fn a_tool_result_needs_an_earlier_call() {
        let messages = vec![
            user("q"),
            message(
                Role::Tool,
                vec![Part::ToolCallResponse {
                    id: Some("x".to_owned()),
                    response: json!({}),
                    extra: Map::new(),
                }],
            ),
        ];
        let err = request_body(StructuredOutput::Native, &request(messages))
            .expect_err("unknown tool call");
        assert_eq!(
            err,
            ChatError::UnsupportedMessage {
                index: 1,
                reason: "tool result for an unknown tool call",
            }
        );
    }

    #[test]
    fn encodes_params_tools_and_choices() {
        let mut full = request(vec![user("Hi")]);
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
        let generation = &body["generationConfig"];
        assert_eq!(generation["maxOutputTokens"], json!(16));
        assert_eq!(generation["temperature"], json!(0.5));
        assert_eq!(generation["topP"], json!(0.9));
        assert_eq!(generation["stopSequences"], json!(["END"]));
        assert_eq!(generation["presencePenalty"], json!(0.1));
        assert_eq!(generation["frequencyPenalty"], json!(0.2));
        assert_eq!(generation["seed"], json!(7));
        assert!(generation.get("reasoning_effort").is_none());
        assert!(generation.get("reasoningEffort").is_none());

        let tooled = || {
            let mut tooled = request(vec![user("Hi")]);
            tooled.tools = vec![
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
            ];
            tooled
        };
        let plain = body_of(StructuredOutput::Native, &tooled());
        assert!(plain.get("toolConfig").is_none());
        assert_eq!(
            plain["tools"],
            json!([{"functionDeclarations": [
                {"name": "a", "description": "does a", "parametersJsonSchema": {}},
                {"name": "b", "parametersJsonSchema": {}}
            ]}])
        );

        for (choice, expected) in [
            (
                ToolChoice::Auto,
                json!({"functionCallingConfig": {"mode": "AUTO"}}),
            ),
            (
                ToolChoice::None,
                json!({"functionCallingConfig": {"mode": "NONE"}}),
            ),
            (
                ToolChoice::Required,
                json!({"functionCallingConfig": {"mode": "ANY"}}),
            ),
            (
                ToolChoice::Tool("t".to_owned()),
                json!({"functionCallingConfig": {"mode": "ANY", "allowedFunctionNames": ["t"]}}),
            ),
        ] {
            let with_choice = ChatRequest {
                tool_choice: Some(choice),
                ..tooled()
            };
            let body = body_of(StructuredOutput::Native, &with_choice);
            assert_eq!(body["toolConfig"], expected);
        }
        let mut no_tools = request(vec![user("Hi")]);
        no_tools.tool_choice = Some(ToolChoice::Auto);
        assert!(
            body_of(StructuredOutput::Native, &no_tools)
                .get("toolConfig")
                .is_none()
        );
    }

    #[test]
    fn decode_reply_reads_blocks_finishes_and_calls() {
        let reply = |body: Value| decode_reply(&serde_json::to_vec(&body).expect("body"));

        let blocked = reply(json!({"promptFeedback": {"blockReason": "SAFETY"}}));
        assert_eq!(blocked.finish_reason, Some(FinishReason::ContentFilter));
        assert_eq!(blocked.content, None);

        let truncated = reply(json!({"candidates": [{"finishReason": "MAX_TOKENS"}]}));
        assert_eq!(truncated.finish_reason, Some(FinishReason::Length));
        assert_eq!(truncated.content, None);

        let filtered = reply(
            json!({"candidates": [{"finishReason": "SAFETY", "content": {"parts": [{"text": "x"}]}}]}),
        );
        assert_eq!(filtered.finish_reason, Some(FinishReason::ContentFilter));

        let stopped =
            reply(json!({"candidates": [{"finishReason": "STOP", "content": {"parts": []}}]}));
        assert_eq!(stopped.finish_reason, Some(FinishReason::Stop));
        assert_eq!(
            reply(json!({"candidates": [{"finishReason": "OTHER"}]})).finish_reason,
            Some(FinishReason::Other("OTHER".to_owned()))
        );

        let calls = reply(json!({
            "modelVersion": "gemini-x",
            "candidates": [{"content": {"parts": [
                {"functionCall": {"name": "a"}},
                {"functionCall": {"name": "b", "args": {"x": 1}}},
                {"text": "hidden", "thought": true},
                {"text": "shown"}
            ]}}]
        }));
        assert_eq!(calls.response_model.as_deref(), Some("gemini-x"));
        let content = calls.content.expect("content");
        assert_eq!(
            content.tool_calls,
            vec![ToolCall {
                id: "call_0".to_owned(),
                name: "b".to_owned(),
                arguments: json!({"x": 1}),
            }]
        );
        assert_eq!(content.text.as_deref(), Some("shown"));
    }
}
