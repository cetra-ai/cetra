//! LLM judge calls.
//!
//! E11 is the OpenAI-compatible adapter, E12 is Anthropic, and E13 is Gemini.

mod answer;

use std::time::Duration;

#[cfg(test)]
pub(crate) use answer::Answer;
pub(crate) use answer::{RawAnswer, answer_schema, find_object, parse_answer};
pub(crate) use answer::{cut_chars, normalize_label};
use reqwest::Url;
use serde_json::Value;

use cetra_core::message::{Message, Part, Role};
use serde_json::Map;

use crate::chat::{
    CHAT_IDLE_TIMEOUT, ChatParams, ChatReply, ChatRequest, FinishReason, ResponseFormat, TokenUsage,
};
use crate::egress::EgressClient;
use crate::error::JudgeError;
use crate::secret::Secret;

/// Which provider API a connection calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Adapter {
    /// OpenAI Chat Completions, including compatible servers.
    OpenAi,
    /// Anthropic Messages.
    Anthropic,
    /// Gemini `generateContent`.
    Gemini,
}

impl Adapter {
    /// The stored connection value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::OpenAi => "openai",
            Self::Anthropic => "anthropic",
            Self::Gemini => "gemini",
        }
    }

    /// The `gen_ai.provider.name` the judge span records, and the provider prices are looked up with.
    #[must_use]
    pub const fn provider_name(self) -> &'static str {
        match self {
            Self::OpenAi => "openai",
            Self::Anthropic => "anthropic",
            Self::Gemini => "gcp.gemini",
        }
    }

    /// Parses the stored connection value. Unknown text is [`UnknownName`].
    pub fn from_name(text: &str) -> Result<Self, UnknownName> {
        match text {
            "openai" => Ok(Self::OpenAi),
            "anthropic" => Ok(Self::Anthropic),
            "gemini" => Ok(Self::Gemini),
            _ => Err(UnknownName),
        }
    }
}

/// How the provider is asked to fill the answer schema.
///
/// The default is native JSON schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StructuredOutput {
    /// Provider-native structured output.
    #[default]
    Native,
    /// A forced `submit_judgement` tool call.
    Tool,
    /// The schema is appended to the prompt.
    Prompt,
}

/// Name of the tool forced in [`StructuredOutput::Tool`].
pub(crate) const JUDGEMENT_TOOL: &str = "submit_judgement";

/// Description sent with [`JUDGEMENT_TOOL`].
pub(crate) const JUDGEMENT_DESCRIPTION: &str = "Submit your judgement.";

impl StructuredOutput {
    /// The stored connection value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Native => "native",
            Self::Tool => "tool",
            Self::Prompt => "prompt",
        }
    }

    /// Parses the stored connection value. Unknown text is [`UnknownName`].
    pub fn from_name(text: &str) -> Result<Self, UnknownName> {
        match text {
            "native" => Ok(Self::Native),
            "tool" => Ok(Self::Tool),
            "prompt" => Ok(Self::Prompt),
            _ => Err(UnknownName),
        }
    }
}

/// [`Adapter::from_name`] or [`StructuredOutput::from_name`] rejected the text.
///
/// The rejected text is not kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnknownName;

impl std::fmt::Display for UnknownName {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("unknown name")
    }
}

impl std::error::Error for UnknownName {}

/// A decrypted provider connection. The server loads and decrypts it.
#[derive(Clone, PartialEq, Eq)]
pub struct JudgeConnection {
    /// Provider API.
    pub adapter: Adapter,
    /// Override for the provider base. The OpenAI default is `https://api.openai.com/v1`.
    ///
    /// [`Debug`](std::fmt::Debug) prints its host only.
    pub base_url: Option<Url>,
    /// Provider credential. [`Debug`](std::fmt::Debug) redacts it.
    pub api_key: Secret,
    /// Extra request headers, applied after the adapter's own headers.
    ///
    /// The same header name replaces the earlier value. Values are redacted in
    /// [`Debug`](std::fmt::Debug).
    pub extra_headers: Vec<(String, Secret)>,
    /// Structured-output mode.
    pub structured_output: StructuredOutput,
}

impl std::fmt::Debug for JudgeConnection {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("JudgeConnection")
            .field("adapter", &self.adapter)
            .field(
                "base_url_host",
                &self.base_url.as_ref().and_then(Url::host_str),
            )
            .field("api_key", &self.api_key)
            .field("extra_headers", &self.extra_headers)
            .field("structured_output", &self.structured_output)
            .finish()
    }
}

/// One completion call. The caller owns the model, prompt, and answer schema.
pub(crate) struct CompletionCall<'a> {
    /// Provider model id.
    pub(crate) model: &'a str,
    /// Rendered prompt. Sent as one user message.
    pub(crate) prompt: &'a str,
    /// JSON Schema of the answer object.
    pub(crate) schema: &'a Value,
    /// OpenAI `json_schema.name` in native mode. `^[a-zA-Z0-9_-]{1,64}$`.
    pub(crate) schema_name: &'a str,
    /// Function name forced in [`StructuredOutput::Tool`].
    pub(crate) tool_name: &'a str,
    /// Function description sent in [`StructuredOutput::Tool`].
    pub(crate) tool_description: &'a str,
    /// Sampling temperature. Omitted from the request when unset.
    pub(crate) temperature: Option<f64>,
    /// Completion cap, including reasoning tokens.
    pub(crate) max_output_tokens: u32,
    /// Whole-call timeout. The client also has a 10 s connect timeout.
    pub(crate) timeout: Duration,
}

impl std::fmt::Debug for CompletionCall<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CompletionCall")
            .field("model", &self.model)
            .field("prompt_bytes", &self.prompt.len())
            .field("tool_name", &self.tool_name)
            .field("temperature", &self.temperature)
            .field("max_output_tokens", &self.max_output_tokens)
            .field("timeout", &self.timeout)
            .finish()
    }
}

/// What the provider returned, before the answer parser runs.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct JudgeReply {
    /// Tool-call arguments, or the message text.
    pub(crate) answer: RawAnswer,
    /// Text the model returned. Tool-call arguments are compact JSON.
    pub(crate) output_text: String,
    /// Token counts in OTel semantics.
    pub(crate) usage: TokenUsage,
    /// Model the provider says it ran.
    pub(crate) response_model: Option<String>,
}

/// The chat request of one completion call: the prompt as one user message, the answer
/// schema as the strict response format, and the call's tool as the forced tool in tool mode.
fn chat_request(call: &CompletionCall<'_>) -> ChatRequest {
    ChatRequest {
        model: call.model.to_owned(),
        messages: vec![Message {
            role: Role::User,
            name: None,
            parts: vec![Part::Text {
                content: call.prompt.to_owned(),
                extra: Map::new(),
            }],
            finish_reason: None,
            extra: Map::new(),
        }],
        tools: Vec::new(),
        tool_choice: None,
        response_format: Some(ResponseFormat {
            name: call.schema_name.to_owned(),
            schema: call.schema.clone(),
            strict: true,
            tool_name: call.tool_name.to_owned(),
            tool_description: call.tool_description.to_owned(),
        }),
        params: ChatParams {
            temperature: call.temperature,
            ..ChatParams::default()
        },
        max_tokens: call.max_output_tokens,
        timeout: call.timeout,
        idle_timeout: CHAT_IDLE_TIMEOUT,
    }
}

/// The judge's reading of a reply. Refusal, truncation, and filtering fail first, then a
/// missing answer, then the first tool call (its arguments must be a JSON object), then the text.
pub(crate) fn judge_reply(reply: ChatReply) -> Result<JudgeReply, JudgeError> {
    match reply.finish_reason {
        Some(FinishReason::Refusal) => return Err(JudgeError::Refused),
        Some(FinishReason::Length) => return Err(JudgeError::Truncated),
        Some(FinishReason::ContentFilter) => return Err(JudgeError::ContentFiltered),
        _ => {}
    }
    let content = reply.content.ok_or(JudgeError::EmptyAnswer)?;
    let usage = reply.usage.unwrap_or_default();
    if let Some(call) = content.tool_calls.first() {
        let object = call
            .arguments
            .as_object()
            .cloned()
            .ok_or(JudgeError::NotJson)?;
        let output_text =
            serde_json::to_string(&call.arguments).map_err(|_| JudgeError::NotJson)?;
        return Ok(JudgeReply {
            answer: RawAnswer::Object(object),
            output_text,
            usage,
            response_model: reply.response_model,
        });
    }
    let text = content.text.ok_or(JudgeError::EmptyAnswer)?;
    Ok(JudgeReply {
        answer: RawAnswer::Text(text.clone()),
        output_text: text,
        usage,
        response_model: reply.response_model,
    })
}

/// Sends one judge call and returns the model's answer without parsing it.
///
/// # Errors
///
/// Returns [`JudgeError::InvalidModel`] for a model name that cannot be sent,
/// [`JudgeError::InvalidHeader`] for an extra header that is not valid HTTP, and
/// [`JudgeError::Transport`] when the client fails. A non-200 status becomes
/// [`JudgeError::Provider`]. Refusal, truncation, and content filtering are
/// their own variants and do not include the provider's text.
pub(crate) async fn call(
    client: &EgressClient,
    conn: &JudgeConnection,
    call: &CompletionCall<'_>,
) -> Result<JudgeReply, JudgeError> {
    judge_reply(crate::chat::complete_chat(client, conn, &chat_request(call)).await?)
}

#[cfg(test)]
mod tests {
    use super::{Adapter, JudgeConnection, StructuredOutput, UnknownName};
    use crate::secret::Secret;

    #[test]
    fn connection_debug_shows_the_host_only() {
        let conn = JudgeConnection {
            adapter: Adapter::OpenAi,
            base_url: Some(
                reqwest::Url::parse("https://judge.example/SECRET-PAYLOAD/v1?key=SECRET-PAYLOAD")
                    .expect("url"),
            ),
            api_key: Secret::new("sk-SECRET-PAYLOAD".to_owned()),
            extra_headers: vec![(
                "X-Tenant".to_owned(),
                Secret::new("SECRET-PAYLOAD".to_owned()),
            )],
            structured_output: StructuredOutput::Native,
        };
        let debug = format!("{conn:?}");
        crate::error::assert_no_payload(&debug, "SECRET-PAYLOAD");
        assert!(debug.contains("judge.example"), "{debug}");
        assert!(debug.contains("X-Tenant"), "{debug}");
    }

    #[test]
    fn from_name_rejects_other_text() {
        for text in ["OPENAI", "", "open ai"] {
            assert_eq!(Adapter::from_name(text), Err(UnknownName), "{text:?}");
        }
        assert_eq!(StructuredOutput::from_name("NATIVE"), Err(UnknownName));
    }

    #[test]
    fn parses_adapter_and_mode_names() {
        assert_eq!(Adapter::from_name("openai"), Ok(Adapter::OpenAi));
        assert_eq!(Adapter::OpenAi.as_str(), "openai");
        assert_eq!(Adapter::from_name("anthropic"), Ok(Adapter::Anthropic));
        assert_eq!(Adapter::Anthropic.as_str(), "anthropic");
        assert_eq!(Adapter::from_name("gemini"), Ok(Adapter::Gemini));
        assert_eq!(Adapter::Gemini.as_str(), "gemini");
        let err = Adapter::from_name("vertex/SECRET-PAYLOAD").expect_err("unknown adapter");
        assert_eq!(err, UnknownName);
        assert_eq!(err.to_string(), "unknown name");
        assert!(!err.to_string().contains("SECRET-PAYLOAD"));
        assert!(Adapter::from_name("OpenAI").is_err());

        assert_eq!(
            StructuredOutput::from_name("native"),
            Ok(StructuredOutput::Native)
        );
        assert_eq!(StructuredOutput::default(), StructuredOutput::Native);
        assert_eq!(StructuredOutput::Native.as_str(), "native");
        assert_eq!(
            StructuredOutput::from_name("tool"),
            Ok(StructuredOutput::Tool)
        );
        assert_eq!(StructuredOutput::Tool.as_str(), "tool");
        assert_eq!(
            StructuredOutput::from_name("prompt"),
            Ok(StructuredOutput::Prompt)
        );
        assert_eq!(StructuredOutput::Prompt.as_str(), "prompt");
        assert!(StructuredOutput::from_name("Tool").is_err());
    }

    #[test]
    fn judge_reply_keeps_the_old_priority() {
        use super::{RawAnswer, judge_reply};
        use crate::chat::{ChatReply, FinishReason, ReplyContent, TokenUsage, ToolCall};
        use crate::error::JudgeError;
        use serde_json::{Value, json};

        let text = |text: &str| ReplyContent {
            text: Some(text.to_owned()),
            tool_calls: Vec::new(),
        };
        let tool = |arguments: Value| ReplyContent {
            text: Some("{}".to_owned()),
            tool_calls: vec![ToolCall {
                id: "call_0".to_owned(),
                name: "submit_judgement".to_owned(),
                arguments,
            }],
        };
        let reply =
            |finish_reason: Option<FinishReason>, content: Option<ReplyContent>| ChatReply {
                finish_reason,
                content,
                usage: None,
                response_model: None,
            };

        assert_eq!(
            judge_reply(reply(Some(FinishReason::Refusal), Some(text("x")))),
            Err(JudgeError::Refused)
        );
        assert_eq!(
            judge_reply(reply(Some(FinishReason::Length), None)),
            Err(JudgeError::Truncated)
        );
        assert_eq!(
            judge_reply(reply(Some(FinishReason::ContentFilter), Some(text("x")))),
            Err(JudgeError::ContentFiltered)
        );
        assert_eq!(
            judge_reply(reply(Some(FinishReason::Stop), None)),
            Err(JudgeError::EmptyAnswer)
        );
        assert_eq!(
            judge_reply(reply(None, Some(tool(json!([1]))))),
            Err(JudgeError::NotJson)
        );

        let object =
            judge_reply(reply(None, Some(tool(json!({"value": true}))))).expect("object answer");
        assert_eq!(
            object.answer,
            RawAnswer::Object(json!({"value": true}).as_object().cloned().expect("object"))
        );
        assert_eq!(object.output_text, "{\"value\":true}");
        assert_eq!(object.usage, TokenUsage::default());
        assert_eq!(object.response_model, None);

        let empty = ReplyContent {
            text: None,
            tool_calls: Vec::new(),
        };
        assert_eq!(
            judge_reply(reply(None, Some(empty))),
            Err(JudgeError::EmptyAnswer)
        );
        let answer = judge_reply(reply(None, Some(text("x")))).expect("text answer");
        assert_eq!(answer.answer, RawAnswer::Text("x".to_owned()));
        assert_eq!(answer.output_text, "x");
    }
}
