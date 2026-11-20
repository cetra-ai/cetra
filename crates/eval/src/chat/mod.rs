//! Provider chat calls: one request shape for the OpenAI-compatible, Anthropic and
//! Gemini APIs (D21, D-V-19). Judges send it without streaming (D-V-29); the
//! playground streams it.

mod anthropic;
mod gemini;
mod http;
mod openai;
mod sse;
mod stream;

use std::borrow::Cow;
use std::fmt;
use std::time::Duration;

use bytes::Bytes;
use cetra_core::message::{Message, Part, Role};
use reqwest::Url;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde_json::{Map, Value, json};

pub(crate) use http::{provider_error, stream_error};
pub use stream::ChatStream;
use stream::Decoder;

use crate::egress::{EgressClient, EgressRequest, EgressStreamRequest};
use crate::error::ChatError;
use crate::judge::{Adapter, JudgeConnection, StructuredOutput};
use crate::secret::Secret;

/// Largest provider response body, in bytes (D159).
pub(crate) const RESPONSE_MAX_BYTES: usize = 4 * 1024 * 1024;

/// Largest answer a stream may assemble, text plus tool-call arguments, and largest
/// single SSE event (D-V-30, D159).
pub(crate) const ANSWER_MAX_BYTES: usize = RESPONSE_MAX_BYTES;
/// Largest raw stream (D-V-30).
pub(crate) const STREAM_MAX_BYTES: usize = 32 * 1024 * 1024;

const MODEL_MAX_BYTES: usize = 256;

/// Longest whole-call timeout (D-V-24, D159).
pub const CHAT_TIMEOUT_MAX: Duration = Duration::from_secs(300);
/// A stream that sends nothing for this long fails (D-V-24).
pub const CHAT_IDLE_TIMEOUT: Duration = Duration::from_secs(60);
/// `max_tokens` when a playground request has none (D-V-24). Callers apply it.
pub const CHAT_MAX_TOKENS_DEFAULT: u32 = 4_096;
/// Largest `max_tokens` (D-V-24).
pub const CHAT_MAX_TOKENS_MAX: u32 = 65_536;

/// Longest tool name, in bytes.
const TOOL_NAME_MAX_BYTES: usize = 64;
/// Longest provider finish value kept as [`FinishReason::Other`], in characters.
const FINISH_MAX_CHARS: usize = 64;

/// Instruction appended in [`StructuredOutput::Prompt`], before the schema.
pub(crate) const PROMPT_INSTRUCTION: &str =
    "Reply with one JSON object and nothing else. It must match this JSON Schema:";

/// Token counts in OTel semantics. Input includes cached prompt tokens. Output includes reasoning tokens.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TokenUsage {
    /// Prompt tokens, including cached tokens.
    pub input: u64,
    /// Completion tokens, including reasoning tokens.
    pub output: u64,
    /// Cached prompt tokens.
    pub cache_read: u64,
    /// Prompt tokens written to the cache.
    pub cache_write: u64,
    /// Reasoning tokens. They are also included in `output`.
    pub reasoning: u64,
}

/// One chat call. The caller owns every field; [`stream_chat`] and the judges check the bounds.
#[derive(Clone, PartialEq)]
pub struct ChatRequest {
    /// Provider model id.
    pub model: String,
    /// Canonical messages (spec 01), at least one.
    pub messages: Vec<Message>,
    /// Tools the model may call.
    pub tools: Vec<ToolDefinition>,
    /// How the model may use `tools`. `None` leaves the provider default.
    pub tool_choice: Option<ToolChoice>,
    /// Schema the answer must match, sent through the connection's structured-output mode.
    pub response_format: Option<ResponseFormat>,
    /// Sampling parameters.
    pub params: ChatParams,
    /// Completion cap, including reasoning tokens. 1..=[`CHAT_MAX_TOKENS_MAX`].
    pub max_tokens: u32,
    /// Whole-call timeout. Above zero and at most [`CHAT_TIMEOUT_MAX`].
    pub timeout: Duration,
    /// Longest silence of a stream. Above zero. Not used by buffered calls.
    pub idle_timeout: Duration,
}

impl fmt::Debug for ChatRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let tools: Vec<&str> = self.tools.iter().map(|tool| tool.name.as_str()).collect();
        formatter
            .debug_struct("ChatRequest")
            .field("model", &self.model)
            .field("messages", &self.messages.len())
            .field("tools", &tools)
            .field("tool_choice", &self.tool_choice)
            .field(
                "response_format",
                &self
                    .response_format
                    .as_ref()
                    .map(|format| format.name.as_str()),
            )
            .field("params", &self.params)
            .field("max_tokens", &self.max_tokens)
            .field("timeout", &self.timeout)
            .field("idle_timeout", &self.idle_timeout)
            .finish()
    }
}

/// One tool the model may call.
#[derive(Clone, PartialEq)]
pub struct ToolDefinition {
    /// `[A-Za-z0-9_-]{1,64}`.
    pub name: String,
    /// What the tool does.
    pub description: Option<String>,
    /// JSON Schema of the arguments.
    pub parameters: Value,
}

impl fmt::Debug for ToolDefinition {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ToolDefinition")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

/// How the model may use the request's tools.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolChoice {
    /// The model decides.
    Auto,
    /// No tool call.
    None,
    /// At least one tool call.
    Required,
    /// This tool. It must be one of the request's tools.
    Tool(String),
}

/// The schema an answer must match (D-V-25).
#[derive(Clone, PartialEq)]
pub struct ResponseFormat {
    /// Schema name in native mode (OpenAI `json_schema.name`).
    pub name: String,
    /// JSON Schema of the answer.
    pub schema: Value,
    /// OpenAI `strict`, in native and tool mode.
    pub strict: bool,
    /// The tool forced in tool mode. `[A-Za-z0-9_-]{1,64}`.
    pub tool_name: String,
    /// The forced tool's description.
    pub tool_description: String,
}

impl fmt::Debug for ResponseFormat {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResponseFormat")
            .field("name", &self.name)
            .field("strict", &self.strict)
            .field("tool_name", &self.tool_name)
            .finish_non_exhaustive()
    }
}

/// Sampling parameters. A provider without the field does not get it (D-V-31).
#[derive(Clone, PartialEq, Default)]
pub struct ChatParams {
    /// Sampling temperature.
    pub temperature: Option<f64>,
    /// Nucleus sampling.
    pub top_p: Option<f64>,
    /// Stop sequences. Empty sends none.
    pub stop: Vec<String>,
    /// OpenAI and Gemini.
    pub presence_penalty: Option<f64>,
    /// OpenAI and Gemini.
    pub frequency_penalty: Option<f64>,
    /// OpenAI and Gemini.
    pub seed: Option<i64>,
    /// OpenAI only, sent as given.
    pub reasoning_effort: Option<String>,
}

impl fmt::Debug for ChatParams {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ChatParams")
            .field("temperature", &self.temperature)
            .field("top_p", &self.top_p)
            .field("stop_count", &self.stop.len())
            .field("presence_penalty", &self.presence_penalty)
            .field("frequency_penalty", &self.frequency_penalty)
            .field("seed", &self.seed)
            .field("reasoning_effort_set", &self.reasoning_effort.is_some())
            .finish()
    }
}

/// One complete tool call from the model.
#[derive(Clone, PartialEq)]
pub struct ToolCall {
    /// Provider call id; `call_<n>` when the provider gave none.
    pub id: String,
    /// Tool name.
    pub name: String,
    /// Arguments. Text that is not JSON is kept as a JSON string (spec 01).
    pub arguments: Value,
}

impl fmt::Debug for ToolCall {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ToolCall")
            .field("id", &self.id)
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

/// Why generation stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FinishReason {
    /// The model finished.
    Stop,
    /// The token cap was reached.
    Length,
    /// The model called a tool.
    ToolCall,
    /// A content filter stopped the answer.
    ContentFilter,
    /// The model refused.
    Refusal,
    /// Another provider value, kept when it is a short token.
    Other(String),
}

impl FinishReason {
    /// `stop`, `length`, `tool_call`, `content_filter`, `refusal`, or the provider's token.
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::Stop => "stop",
            Self::Length => "length",
            Self::ToolCall => "tool_call",
            Self::ContentFilter => "content_filter",
            Self::Refusal => "refusal",
            Self::Other(reason) => reason.as_str(),
        }
    }
}

/// One event of a streamed call.
#[derive(Clone, PartialEq)]
pub enum ChatEvent {
    /// A piece of the answer text. Never empty.
    Text(String),
    /// A complete tool call. In tool mode, the forced tool's call arrives here (D-V-25).
    ToolCall(ToolCall),
    /// The last event.
    Done(ChatDone),
}

impl fmt::Debug for ChatEvent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Text(text) => formatter
                .debug_struct("Text")
                .field("bytes", &text.len())
                .finish(),
            Self::ToolCall(call) => formatter.debug_tuple("ToolCall").field(call).finish(),
            Self::Done(done) => formatter.debug_tuple("Done").field(done).finish(),
        }
    }
}

/// How a streamed call ended.
#[derive(Debug, Clone, PartialEq)]
pub struct ChatDone {
    /// Why generation stopped, when the provider said.
    pub finish_reason: Option<FinishReason>,
    /// Token counts; `None` when the stream carried none.
    pub usage: Option<TokenUsage>,
    /// Model the provider says it ran.
    pub response_model: Option<String>,
}

/// Rejects a model name that would be placed in a URL or a JSON body.
pub(crate) fn validate_model(model: &str) -> Result<(), ChatError> {
    if model.is_empty() || model.len() > MODEL_MAX_BYTES || model.chars().any(forbidden_in_model) {
        return Err(ChatError::InvalidModel);
    }
    Ok(())
}

/// The URL parser reads `\` as `/` in http(s) paths, and `%` starts an escape.
fn forbidden_in_model(ch: char) -> bool {
    ch.is_whitespace() || ch.is_control() || matches!(ch, '/' | '\\' | '%' | '?' | '#')
}

/// Joins `path` onto a provider base, preserving a query string.
///
/// A base without a trailing slash is treated as a directory. `set_path` drops
/// the query, so it is written back for bases such as `?api-version=`.
pub(crate) fn endpoint(
    base: Option<&Url>,
    default_base: &str,
    path: &str,
) -> Result<Url, ChatError> {
    let parsed;
    let base = match base {
        Some(base) => base,
        None => {
            parsed = Url::parse(default_base).map_err(|_| ChatError::InvalidRequest)?;
            &parsed
        }
    };
    let query = base.query().map(ToOwned::to_owned);
    let mut directory = base.clone();
    if !directory.path().ends_with('/') {
        let mut with_slash = directory.path().to_owned();
        with_slash.push('/');
        directory.set_path(&with_slash);
    }
    let mut url = directory
        .join(path)
        .map_err(|_| ChatError::InvalidRequest)?;
    if let Some(query) = query {
        url.set_query(Some(&query));
    }
    Ok(url)
}

/// Request headers: the connection's extra headers first, then `own`, which replace an
/// extra header of the same name (D-I-46).
pub(crate) fn request_headers(
    conn: &JudgeConnection,
    own: Vec<(HeaderName, HeaderValue)>,
) -> Result<Vec<(HeaderName, HeaderValue)>, ChatError> {
    let mut map = HeaderMap::new();
    for (name, value) in &conn.extra_headers {
        let name = HeaderName::try_from(name.as_str()).map_err(|_| ChatError::InvalidHeader)?;
        map.insert(name, secret_header(value)?);
    }
    for (name, value) in own {
        map.insert(name, value);
    }
    Ok(map
        .into_iter()
        .filter_map(|(name, value)| name.map(|name| (name, value)))
        .collect())
}

/// A secret as a header value.
pub(crate) fn secret_header(value: &Secret) -> Result<HeaderValue, ChatError> {
    HeaderValue::from_bytes(value.expose().as_bytes()).map_err(|_| ChatError::InvalidHeader)
}

/// The bounds of D-V-24, tool names, and the parts D-V-25 allows.
pub fn check_request(request: &ChatRequest) -> Result<(), ChatError> {
    if !(1..=CHAT_MAX_TOKENS_MAX).contains(&request.max_tokens) {
        return Err(ChatError::InvalidRequest);
    }
    if request.timeout.is_zero()
        || request.timeout > CHAT_TIMEOUT_MAX
        || request.idle_timeout.is_zero()
    {
        return Err(ChatError::InvalidRequest);
    }
    if request.messages.is_empty() {
        return Err(ChatError::InvalidRequest);
    }
    if request
        .tools
        .iter()
        .any(|tool| !valid_tool_name(&tool.name))
    {
        return Err(ChatError::InvalidRequest);
    }
    if let Some(format) = &request.response_format
        && !valid_tool_name(&format.tool_name)
    {
        return Err(ChatError::InvalidRequest);
    }
    // Tool mode sends the forced tool next to the request's tools: two tools of one name.
    if let Some(format) = &request.response_format
        && request
            .tools
            .iter()
            .any(|tool| tool.name == format.tool_name)
    {
        return Err(ChatError::InvalidRequest);
    }
    if let Some(ToolChoice::Tool(name)) = &request.tool_choice
        && !request.tools.iter().any(|tool| tool.name == *name)
    {
        return Err(ChatError::InvalidRequest);
    }
    check_messages(&request.messages)
}

/// A tool name is 1 to 64 bytes of `[A-Za-z0-9_-]`.
fn valid_tool_name(name: &str) -> bool {
    (1..=TOOL_NAME_MAX_BYTES).contains(&name.len())
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

/// Refuses what D-V-25 and D-V-31 do not send.
pub(crate) fn check_messages(messages: &[Message]) -> Result<(), ChatError> {
    for (index, message) in messages.iter().enumerate() {
        let unsupported = |reason: &'static str| ChatError::UnsupportedMessage { index, reason };
        if matches!(message.role, Role::Other(_)) {
            return Err(unsupported("unknown role"));
        }
        for part in &message.parts {
            match part {
                Part::Text { .. } if message.role == Role::Tool => {
                    return Err(unsupported("text in a tool message"));
                }
                Part::Text { .. } | Part::Reasoning { .. } => {}
                Part::ToolCall { id, .. } => {
                    if message.role != Role::Assistant {
                        return Err(unsupported("tool call outside an assistant message"));
                    }
                    if id.as_deref().is_none_or(str::is_empty) {
                        return Err(unsupported("tool call without an id"));
                    }
                }
                Part::ToolCallResponse { id, .. } => {
                    if message.role != Role::Tool {
                        return Err(unsupported("tool result outside a tool message"));
                    }
                    if id.as_deref().is_none_or(str::is_empty) {
                        return Err(unsupported("tool result without an id"));
                    }
                }
                Part::Uri { .. } | Part::Blob { .. } => {
                    return Err(unsupported("media part"));
                }
                Part::Unknown(_) => return Err(unsupported("unknown part")),
            }
        }
    }
    Ok(())
}

/// The text parts of `message`, in order.
pub(crate) fn texts(message: &Message) -> Vec<&str> {
    message
        .parts
        .iter()
        .filter_map(|part| match part {
            Part::Text { content, .. } => Some(content.as_str()),
            _ => None,
        })
        .collect()
}

/// One text: a JSON string. Several: `[{"type":"text","text":…}, …]`.
pub(crate) fn text_content(texts: &[&str]) -> Value {
    match texts {
        [one] => Value::String((*one).to_owned()),
        _ => Value::Array(
            texts
                .iter()
                .map(|text| json!({"type": "text", "text": text}))
                .collect(),
        ),
    }
}

/// Tool-call arguments as a JSON object: an object as is; `None` and `null` as `{}`; a
/// JSON string that parses to an object as that object. Anything else is
/// `UnsupportedMessage { index, reason: "tool call arguments are not a JSON object" }`.
pub(crate) fn arguments_object(
    index: usize,
    arguments: Option<&Value>,
) -> Result<Map<String, Value>, ChatError> {
    let parsed = match arguments {
        None | Some(Value::Null) => return Ok(Map::new()),
        Some(Value::String(text)) => serde_json::from_str::<Value>(text).ok(),
        Some(value) => Some(value.clone()),
    };
    match parsed {
        Some(Value::Object(object)) => Ok(object),
        _ => Err(ChatError::UnsupportedMessage {
            index,
            reason: "tool call arguments are not a JSON object",
        }),
    }
}

/// Tool-call arguments as JSON text: a JSON string as is, `None` and `null` as `{}`, any
/// other value as compact JSON.
pub(crate) fn arguments_text(arguments: Option<&Value>) -> Result<String, ChatError> {
    match arguments {
        Some(Value::String(text)) => Ok(text.clone()),
        Some(Value::Null) | None => Ok("{}".to_owned()),
        Some(value) => serde_json::to_string(value).map_err(|_| ChatError::InvalidRequest),
    }
}

/// Arguments assembled from a stream: blank text is `{}`, JSON text is its value, other
/// text is kept as a JSON string.
pub(crate) fn stream_arguments(raw: &str) -> Value {
    if raw.trim().is_empty() {
        return Value::Object(Map::new());
    }
    serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.to_owned()))
}

/// A tool result as text: a JSON string as is, any other value as compact JSON.
pub(crate) fn response_text(response: &Value) -> Result<String, ChatError> {
    match response {
        Value::String(text) => Ok(text.clone()),
        value => serde_json::to_string(value).map_err(|_| ChatError::InvalidRequest),
    }
}

/// A finite number. NaN and infinities are `InvalidRequest`.
pub(crate) fn number(value: f64) -> Result<Value, ChatError> {
    serde_json::Number::from_f64(value)
        .map(Value::Number)
        .ok_or(ChatError::InvalidRequest)
}

/// A provider finish value outside the known ones: kept when it is 1–64 characters of
/// `[A-Za-z0-9_.-]`, else `None`.
pub(crate) fn other_finish(raw: &str) -> Option<FinishReason> {
    let fits = (1..=FINISH_MAX_CHARS).contains(&raw.chars().count());
    (fits && http::is_token(raw)).then(|| FinishReason::Other(raw.to_owned()))
}

/// The messages to send in prompt mode: [`PROMPT_INSTRUCTION`] and the pretty-printed
/// schema are appended, after a blank line, to the last text part of the last user
/// message that has one. When no user message has text, a user message
/// `"{PROMPT_INSTRUCTION}\n{schema}"` is appended.
pub(crate) fn apply_prompt_mode(
    messages: &[Message],
    schema: &Value,
) -> Result<Vec<Message>, ChatError> {
    let pretty = serde_json::to_string_pretty(schema).map_err(|_| ChatError::InvalidRequest)?;
    let mut out = messages.to_vec();
    let slot = out
        .iter_mut()
        .rev()
        .filter(|message| message.role == Role::User)
        .find_map(|message| last_text(&mut message.parts));
    if let Some(content) = slot {
        content.push_str("\n\n");
        content.push_str(PROMPT_INSTRUCTION);
        content.push('\n');
        content.push_str(&pretty);
        return Ok(out);
    }
    out.push(Message {
        role: Role::User,
        name: None,
        parts: vec![Part::Text {
            content: format!("{PROMPT_INSTRUCTION}\n{pretty}"),
            extra: Map::new(),
        }],
        finish_reason: None,
        extra: Map::new(),
    });
    Ok(out)
}

/// The last text part of `parts`, for editing in place.
fn last_text(parts: &mut [Part]) -> Option<&mut String> {
    parts.iter_mut().rev().find_map(|part| match part {
        Part::Text { content, .. } => Some(content),
        _ => None,
    })
}

/// The messages to encode: prompt mode applied when a response format is set.
pub(crate) fn messages_to_send<'a>(
    mode: StructuredOutput,
    request: &'a ChatRequest,
) -> Result<Cow<'a, [Message]>, ChatError> {
    match (&request.response_format, mode) {
        (Some(format), StructuredOutput::Prompt) => Ok(Cow::Owned(apply_prompt_mode(
            &request.messages,
            &format.schema,
        )?)),
        _ => Ok(Cow::Borrowed(request.messages.as_slice())),
    }
}

/// The mode the error hint is computed for: the connection's mode when a response
/// format was sent, else prompt mode (which gives no hint).
pub(crate) fn hint_mode(conn: &JudgeConnection, request: &ChatRequest) -> StructuredOutput {
    if request.response_format.is_some() {
        conn.structured_output
    } else {
        StructuredOutput::Prompt
    }
}

/// A request ready to send.
pub(crate) struct Prepared {
    pub(crate) url: Url,
    pub(crate) headers: Vec<(HeaderName, HeaderValue)>,
    pub(crate) body: Vec<u8>,
}

/// POSTs `prepared` without streaming and returns the 200 body (at most 4 MiB, D159).
/// Another status is [`provider_error`] with [`hint_mode`].
pub(crate) async fn send_buffered(
    client: &EgressClient,
    conn: &JudgeConnection,
    request: &ChatRequest,
    prepared: Prepared,
) -> Result<Bytes, ChatError> {
    let response = client
        .post(EgressRequest {
            url: &prepared.url,
            headers: prepared.headers,
            body: prepared.body,
            timeout: request.timeout,
            max_response_bytes: RESPONSE_MAX_BYTES,
        })
        .await?;
    let status = response.status.as_u16();
    if status != 200 {
        return Err(provider_error(
            conn.adapter,
            hint_mode(conn, request),
            status,
            &response.body,
        ));
    }
    Ok(response.body)
}

/// Sends `request` without streaming (judges, D-V-29).
pub(crate) async fn complete_chat(
    client: &EgressClient,
    conn: &JudgeConnection,
    request: &ChatRequest,
) -> Result<ChatReply, ChatError> {
    match conn.adapter {
        Adapter::OpenAi => openai::complete(client, conn, request).await,
        Adapter::Anthropic => anthropic::complete(client, conn, request).await,
        Adapter::Gemini => gemini::complete(client, conn, request).await,
    }
}

/// Sends `request` as a streamed call (D-V-19, D-V-24, D-V-30).
///
/// # Errors
///
/// The request errors of [`ChatError`] before anything is sent. A non-200 answer is
/// [`ChatError::Provider`], its body read up to 4 MiB. Transport failures are
/// [`ChatError::Transport`].
pub async fn stream_chat(
    client: &EgressClient,
    conn: &JudgeConnection,
    request: &ChatRequest,
) -> Result<ChatStream, ChatError> {
    let (prepared, decoder) = match conn.adapter {
        Adapter::OpenAi => (
            openai::prepare(conn, request, true)?,
            Decoder::OpenAi(openai::StreamState::default()),
        ),
        Adapter::Anthropic => (
            anthropic::prepare(conn, request, true)?,
            Decoder::Anthropic(anthropic::StreamState::default()),
        ),
        Adapter::Gemini => (
            gemini::prepare(conn, request, true)?,
            Decoder::Gemini(gemini::StreamState::default()),
        ),
    };
    let body = client
        .post_stream(EgressStreamRequest {
            url: &prepared.url,
            headers: prepared.headers,
            body: prepared.body,
            timeout: request.timeout,
            idle_timeout: request.idle_timeout,
            max_response_bytes: STREAM_MAX_BYTES,
        })
        .await?;
    let status = body.status().as_u16();
    if status != 200 {
        let bytes = body.read_to_end(RESPONSE_MAX_BYTES).await?;
        return Err(provider_error(
            conn.adapter,
            hint_mode(conn, request),
            status,
            &bytes,
        ));
    }
    Ok(ChatStream::new(body, decoder))
}

/// A decoded non-streamed answer. Decoders never fail: every shape maps to a field.
// Holds the answer text and tool arguments. Nothing logs it, so `Debug` is derived.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ChatReply {
    /// The provider's stop reason. Each decoder applies its provider's priority
    /// (refusal, truncation, filter), and leaves it `None` when the answer container is missing.
    pub(crate) finish_reason: Option<FinishReason>,
    /// `None` when the provider returned no answer container (no choice or message,
    /// no content array, no candidate or parts).
    pub(crate) content: Option<ReplyContent>,
    /// Token counts; `None` when the body had no usage object.
    pub(crate) usage: Option<TokenUsage>,
    /// Model the provider says it ran.
    pub(crate) response_model: Option<String>,
}

/// The answer's text and tool calls.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ReplyContent {
    /// The text parts joined; `None` when there was no text part.
    pub(crate) text: Option<String>,
    /// Tool calls in order.
    pub(crate) tool_calls: Vec<ToolCall>,
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use cetra_core::message::{Message, Part, Role};
    use serde_json::{Map, Value, json};

    use super::{
        CHAT_IDLE_TIMEOUT, CHAT_MAX_TOKENS_MAX, ChatEvent, ChatParams, ChatRequest, FinishReason,
        PROMPT_INSTRUCTION, ResponseFormat, ToolCall, ToolChoice, ToolDefinition,
        apply_prompt_mode, arguments_object, arguments_text, check_messages, check_request, number,
        other_finish, request_headers, response_text, stream_arguments,
    };
    use crate::error::{ChatError, JudgeError};
    use crate::judge::{Adapter, JudgeConnection, StructuredOutput};
    use crate::secret::Secret;
    use reqwest::header::{HeaderName, HeaderValue};

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

    fn tool_def(name: &str) -> ToolDefinition {
        ToolDefinition {
            name: name.to_owned(),
            description: None,
            parameters: json!({"type": "object"}),
        }
    }

    fn schema_format(tool_name: &str) -> ResponseFormat {
        ResponseFormat {
            name: "judge_answer".to_owned(),
            schema: json!({"type": "object"}),
            strict: true,
            tool_name: tool_name.to_owned(),
            tool_description: "Submit.".to_owned(),
        }
    }

    fn request() -> ChatRequest {
        ChatRequest {
            model: "gpt-4o-mini".to_owned(),
            messages: vec![user("Hi")],
            tools: Vec::new(),
            tool_choice: None,
            response_format: None,
            params: ChatParams::default(),
            max_tokens: 16,
            timeout: Duration::from_secs(2),
            idle_timeout: CHAT_IDLE_TIMEOUT,
        }
    }

    fn boolean_schema() -> Value {
        json!({
            "type": "object",
            "properties": {"value": {"type": "boolean"}},
            "required": ["value"],
            "additionalProperties": false
        })
    }

    fn only_text(message: &Message) -> &str {
        match message.parts.first() {
            Some(Part::Text { content, .. }) => content.as_str(),
            _ => panic!("not a text message"),
        }
    }

    #[test]
    fn stream_arguments_keeps_what_it_cannot_parse() {
        assert_eq!(stream_arguments(""), json!({}));
        assert_eq!(stream_arguments("  "), json!({}));
        assert_eq!(stream_arguments("{\"a\":1}"), json!({"a": 1}));
        assert_eq!(stream_arguments("{\"a\":"), json!("{\"a\":"));
    }

    #[test]
    fn chat_event_debug_hides_payload() {
        let text = ChatEvent::Text("SECRET-PAYLOAD".to_owned());
        assert_eq!(format!("{text:?}"), "Text { bytes: 14 }");
        let call = ChatEvent::ToolCall(ToolCall {
            id: "call_0".to_owned(),
            name: "lookup".to_owned(),
            arguments: json!({"q": "SECRET-PAYLOAD"}),
        });
        assert!(!format!("{call:?}").contains("SECRET-PAYLOAD"));
    }

    #[test]
    fn request_headers_put_own_headers_last() {
        let conn = JudgeConnection {
            adapter: Adapter::OpenAi,
            base_url: None,
            api_key: Secret::new("sk-test".to_owned()),
            extra_headers: vec![
                ("X-A".to_owned(), Secret::new("1".to_owned())),
                (
                    "Content-Type".to_owned(),
                    Secret::new("text/plain".to_owned()),
                ),
            ],
            structured_output: StructuredOutput::Native,
        };
        let own = vec![(
            HeaderName::from_static("content-type"),
            HeaderValue::from_static("application/json"),
        )];
        let headers = request_headers(&conn, own).expect("headers");
        let values = |name: &str| -> Vec<String> {
            headers
                .iter()
                .filter(|(header, _)| header.as_str() == name)
                .map(|(_, value)| value.to_str().expect("ascii").to_owned())
                .collect()
        };
        assert_eq!(values("x-a"), vec!["1".to_owned()]);
        assert_eq!(values("content-type"), vec!["application/json".to_owned()]);

        let bad = JudgeConnection {
            extra_headers: vec![("bad name".to_owned(), Secret::new("v".to_owned()))],
            ..conn
        };
        assert_eq!(
            request_headers(&bad, Vec::new()),
            Err(ChatError::InvalidHeader)
        );
    }

    #[test]
    fn check_request_enforces_the_bounds() {
        assert_eq!(check_request(&request()), Ok(()));
        for max_tokens in [0, CHAT_MAX_TOKENS_MAX + 1] {
            let bad = ChatRequest {
                max_tokens,
                ..request()
            };
            assert_eq!(
                check_request(&bad),
                Err(ChatError::InvalidRequest),
                "{max_tokens}"
            );
        }
        let at_max = ChatRequest {
            max_tokens: CHAT_MAX_TOKENS_MAX,
            ..request()
        };
        assert_eq!(check_request(&at_max), Ok(()));
        for timeout in [Duration::ZERO, Duration::from_secs(301)] {
            let bad = ChatRequest {
                timeout,
                ..request()
            };
            assert_eq!(check_request(&bad), Err(ChatError::InvalidRequest));
        }
        let at_limit = ChatRequest {
            timeout: Duration::from_secs(300),
            ..request()
        };
        assert_eq!(check_request(&at_limit), Ok(()));
        let no_idle = ChatRequest {
            idle_timeout: Duration::ZERO,
            ..request()
        };
        assert_eq!(check_request(&no_idle), Err(ChatError::InvalidRequest));
        let empty = ChatRequest {
            messages: Vec::new(),
            ..request()
        };
        assert_eq!(check_request(&empty), Err(ChatError::InvalidRequest));
    }

    #[test]
    fn check_request_checks_tool_names_and_choice() {
        let long = "a".repeat(65);
        for name in ["", "a b", "é", long.as_str()] {
            let bad = ChatRequest {
                tools: vec![tool_def(name)],
                ..request()
            };
            assert_eq!(
                check_request(&bad),
                Err(ChatError::InvalidRequest),
                "{name:?}"
            );
        }
        let exact = "a".repeat(64);
        for name in [exact.as_str(), "get_weather-2"] {
            let ok = ChatRequest {
                tools: vec![tool_def(name)],
                ..request()
            };
            assert_eq!(check_request(&ok), Ok(()), "{name:?}");
        }

        let missing = ChatRequest {
            tools: vec![tool_def("get_weather")],
            tool_choice: Some(ToolChoice::Tool("missing".to_owned())),
            ..request()
        };
        assert_eq!(check_request(&missing), Err(ChatError::InvalidRequest));
        let present = ChatRequest {
            tool_choice: Some(ToolChoice::Tool("get_weather".to_owned())),
            ..missing
        };
        assert_eq!(check_request(&present), Ok(()));

        let bad_format = ChatRequest {
            response_format: Some(schema_format("bad name")),
            ..request()
        };
        assert_eq!(check_request(&bad_format), Err(ChatError::InvalidRequest));

        let clash = ChatRequest {
            tools: vec![tool_def("submit")],
            response_format: Some(schema_format("submit")),
            ..request()
        };
        assert_eq!(check_request(&clash), Err(ChatError::InvalidRequest));
        let distinct = ChatRequest {
            tools: vec![tool_def("lookup")],
            response_format: Some(schema_format("submit")),
            ..request()
        };
        assert_eq!(check_request(&distinct), Ok(()));
    }

    #[test]
    fn check_messages_names_the_index_and_reason() {
        let extra = Map::new;
        let cases: Vec<(Message, &'static str)> = vec![
            (
                message(Role::Other("critic".to_owned()), vec![]),
                "unknown role",
            ),
            (
                message(Role::Tool, vec![text("x")]),
                "text in a tool message",
            ),
            (
                message(
                    Role::User,
                    vec![Part::ToolCall {
                        id: Some("c1".to_owned()),
                        name: "t".to_owned(),
                        arguments: None,
                        extra: extra(),
                    }],
                ),
                "tool call outside an assistant message",
            ),
            (
                message(
                    Role::Assistant,
                    vec![Part::ToolCall {
                        id: None,
                        name: "t".to_owned(),
                        arguments: None,
                        extra: extra(),
                    }],
                ),
                "tool call without an id",
            ),
            (
                message(
                    Role::Assistant,
                    vec![Part::ToolCall {
                        id: Some(String::new()),
                        name: "t".to_owned(),
                        arguments: None,
                        extra: extra(),
                    }],
                ),
                "tool call without an id",
            ),
            (
                message(
                    Role::User,
                    vec![Part::ToolCallResponse {
                        id: Some("c1".to_owned()),
                        response: json!({}),
                        extra: extra(),
                    }],
                ),
                "tool result outside a tool message",
            ),
            (
                message(
                    Role::Tool,
                    vec![Part::ToolCallResponse {
                        id: None,
                        response: json!({}),
                        extra: extra(),
                    }],
                ),
                "tool result without an id",
            ),
            (
                message(
                    Role::User,
                    vec![Part::Uri {
                        modality: None,
                        mime_type: None,
                        uri: "media://a".to_owned(),
                        extra: extra(),
                    }],
                ),
                "media part",
            ),
            (
                message(
                    Role::User,
                    vec![Part::Blob {
                        modality: None,
                        mime_type: None,
                        content: "AA==".to_owned(),
                        extra: extra(),
                    }],
                ),
                "media part",
            ),
            (
                message(Role::User, vec![Part::Unknown(json!({"type": "x"}))]),
                "unknown part",
            ),
        ];
        for (bad, reason) in cases {
            let messages = vec![user("ok"), bad];
            assert_eq!(
                check_messages(&messages),
                Err(ChatError::UnsupportedMessage { index: 1, reason }),
                "{reason}"
            );
        }
        let reasoning = message(
            Role::User,
            vec![Part::Reasoning {
                content: "r".to_owned(),
                extra: extra(),
            }],
        );
        assert_eq!(check_messages(&[user("ok"), reasoning]), Ok(()));

        let err = ChatError::UnsupportedMessage {
            index: 1,
            reason: "media part",
        };
        assert_eq!(err.to_string(), "message 1: media part");
        assert_eq!(JudgeError::from(err), JudgeError::InvalidRequest);
    }

    #[test]
    fn prompt_mode_appends_the_schema_after_a_blank_line() {
        let out = apply_prompt_mode(&[user("Grade.")], &boolean_schema()).expect("prompt");
        assert_eq!(out.len(), 1);
        assert_eq!(
            only_text(&out[0]),
            "Grade.\n\nReply with one JSON object and nothing else. It must match this JSON Schema:\n{\n  \"type\": \"object\",\n  \"properties\": {\n    \"value\": {\n      \"type\": \"boolean\"\n    }\n  },\n  \"required\": [\n    \"value\"\n  ],\n  \"additionalProperties\": false\n}"
        );
    }

    #[test]
    fn prompt_mode_targets_the_last_user_text() {
        let pretty = serde_json::to_string_pretty(&boolean_schema()).expect("pretty");
        let messages = vec![
            user("a"),
            message(Role::Assistant, vec![text("b")]),
            message(Role::User, vec![text("c"), text("d")]),
        ];
        let out = apply_prompt_mode(&messages, &boolean_schema()).expect("prompt");
        assert_eq!(out.len(), 3);
        assert_eq!(out[0], messages[0]);
        assert_eq!(out[1], messages[1]);
        assert_eq!(out[2].parts[0], text("c"));
        assert_eq!(
            out[2].parts[1],
            text(&format!("d\n\n{PROMPT_INSTRUCTION}\n{pretty}"))
        );

        let out = apply_prompt_mode(&[message(Role::System, vec![text("s")])], &boolean_schema())
            .expect("prompt");
        assert_eq!(out.len(), 2);
        assert_eq!(
            out[1],
            message(
                Role::User,
                vec![text(&format!("{PROMPT_INSTRUCTION}\n{pretty}"))]
            )
        );
    }

    #[test]
    fn arguments_object_wants_an_object() {
        let object = json!({"a": 1});
        let expected = object.as_object().cloned().expect("an object");
        assert_eq!(arguments_object(2, Some(&object)), Ok(expected.clone()));
        assert_eq!(arguments_object(2, None), Ok(Map::new()));
        assert_eq!(arguments_object(2, Some(&Value::Null)), Ok(Map::new()));
        assert_eq!(arguments_object(2, Some(&json!("{\"a\":1}"))), Ok(expected));
        let refused = ChatError::UnsupportedMessage {
            index: 2,
            reason: "tool call arguments are not a JSON object",
        };
        for bad in [json!("[1]"), json!(3), json!("x")] {
            assert_eq!(
                arguments_object(2, Some(&bad)),
                Err(refused.clone()),
                "{bad}"
            );
        }
    }

    #[test]
    fn argument_and_result_helpers() {
        assert_eq!(
            arguments_text(Some(&json!("{\"a\":1}"))).expect("text"),
            "{\"a\":1}"
        );
        assert_eq!(
            arguments_text(Some(&json!({"a": 1}))).expect("text"),
            "{\"a\":1}"
        );
        assert_eq!(arguments_text(None).expect("text"), "{}");
        assert_eq!(arguments_text(Some(&Value::Null)).expect("text"), "{}");
        assert_eq!(response_text(&json!("ok")).expect("text"), "ok");
        assert_eq!(response_text(&json!({"t": 1})).expect("text"), "{\"t\":1}");
    }

    #[test]
    fn number_refuses_non_finite() {
        for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert_eq!(number(value), Err(ChatError::InvalidRequest));
        }
        assert_eq!(number(0.5), Ok(json!(0.5)));
    }

    #[test]
    fn other_finish_keeps_tokens_only() {
        assert_eq!(
            other_finish("pause_turn"),
            Some(FinishReason::Other("pause_turn".to_owned()))
        );
        let long = "a".repeat(65);
        for raw in ["", "a b", long.as_str()] {
            assert_eq!(other_finish(raw), None, "{raw:?}");
        }
    }

    #[test]
    fn debug_hides_payload() {
        let secret = "SECRET-PAYLOAD";
        let request = ChatRequest {
            model: "m-1".to_owned(),
            messages: vec![user(&format!("say {secret}"))],
            tools: vec![ToolDefinition {
                name: "get_weather".to_owned(),
                description: Some(secret.to_owned()),
                parameters: json!({"x": secret}),
            }],
            tool_choice: Some(ToolChoice::Tool("get_weather".to_owned())),
            response_format: Some(ResponseFormat {
                name: "judge_answer".to_owned(),
                schema: json!({"s": secret}),
                strict: true,
                tool_name: "submit".to_owned(),
                tool_description: secret.to_owned(),
            }),
            params: ChatParams {
                stop: vec![secret.to_owned()],
                reasoning_effort: Some(secret.to_owned()),
                ..ChatParams::default()
            },
            max_tokens: 16,
            timeout: Duration::from_secs(2),
            idle_timeout: CHAT_IDLE_TIMEOUT,
        };
        let call = ToolCall {
            id: "call_0".to_owned(),
            name: "get_weather".to_owned(),
            arguments: json!({"city": secret}),
        };
        let debug = format!("{request:?} {call:?}");
        crate::error::assert_no_payload(&debug, secret);
        assert!(debug.contains("m-1"), "{debug}");
        assert!(debug.contains("get_weather"), "{debug}");
    }
}
