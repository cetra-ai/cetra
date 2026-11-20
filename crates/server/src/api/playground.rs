//! Playground API types: the run request (spec 09 § Playground, D-V-20 to D-V-22, D-V-37).
//!
//! Deserialize and serialize, so the request example round-trips.

use std::collections::BTreeMap;

use cetra_core::ids::{SpanId, TraceId};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use super::common::open_object;

/// `POST /api/v1/projects/{project}/playground/runs` body.
///
/// Unknown fields are refused. Each `messages` entry is a canonical message (spec 01) or a
/// placeholder `{"type": "placeholder", "name": ...}`; a placeholder is replaced, in place, by the
/// messages given under its name in `placeholders`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PlaygroundRunRequest {
    /// LLM connection of the caller's org to call.
    pub connection_id: Uuid,
    /// Provider model id: 1 to 256 bytes, with no whitespace, control character, `/`, `\`, `%`,
    /// `?` or `#`.
    pub model: String,
    /// Canonical messages (spec 01) and placeholder entries, at least one. A text part of a
    /// message written here is rendered with `variables`; a placeholder's messages are not.
    #[schema(value_type = Vec<Object>)]
    pub messages: Vec<serde_json::Value>,
    /// Values of the `{{name}}` variables in the text parts of `messages`. Default: none.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub variables: BTreeMap<String, String>,
    /// Messages that replace the placeholder entries of `messages`, by name. Default: none.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    #[schema(value_type = Object)]
    pub placeholders: BTreeMap<String, Vec<serde_json::Value>>,
    /// Sampling parameters and the completion cap. Default: none.
    #[serde(default, skip_serializing_if = "PlaygroundParams::is_empty")]
    pub params: PlaygroundParams,
    /// Tools the model may call. A tool is never run. Default: none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<PlaygroundTool>,
    /// `auto`, `none`, `required`, or the name of one of `tools`. A tool named `auto`, `none` or
    /// `required` cannot be forced by name. Default: the provider's choice.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<String>,
    /// The schema the answer must match. The answer is streamed as `text` events.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_format: Option<PlaygroundResponseFormat>,
    /// Where the messages came from: a prompt version or a span. Recorded on the run's trace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<PlaygroundSource>,
}

/// Sampling parameters and the completion cap. A provider without a field does not get it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PlaygroundParams {
    /// Sampling temperature.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    /// Nucleus sampling.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    /// Completion cap, including reasoning tokens. From 1 to 65 536. Default 4 096.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    /// Stop sequences. Default: none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stop: Vec<String>,
    /// OpenAI and Gemini.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub presence_penalty: Option<f64>,
    /// OpenAI and Gemini.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frequency_penalty: Option<f64>,
    /// OpenAI and Gemini.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed: Option<i64>,
    /// OpenAI only, sent as given.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
}

impl PlaygroundParams {
    /// Whether no parameter is set, so the field is left out of the JSON.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// A tool the model may call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PlaygroundTool {
    /// `[A-Za-z0-9_-]`, 1 to 64 characters, unique within the request.
    pub name: String,
    /// What the tool does. Default: none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// JSON Schema of the arguments.
    #[schema(schema_with = open_object)]
    pub parameters: serde_json::Value,
}

/// The schema an answer must match. `name` is the schema's name in the provider's native mode.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PlaygroundResponseFormat {
    /// Schema name. `[A-Za-z0-9_-]`, 1 to 64 characters.
    pub name: String,
    /// JSON Schema of the answer.
    #[schema(schema_with = open_object)]
    pub schema: serde_json::Value,
    /// Strict schema adherence where the provider supports it. Default: false.
    #[serde(default)]
    pub strict: bool,
}

/// Where a run's messages came from. Recorded as `prompt:<name>@<version>` or
/// `span:<trace>:<span>` on the run's trace.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum PlaygroundSource {
    /// A prompt version: `{"prompt": {"name": "...", "version": 2}}`.
    Prompt {
        /// Prompt name.
        name: String,
        /// Prompt version number.
        version: i64,
    },
    /// A span: `{"span": {"trace_id": "...", "span_id": "..."}}`.
    Span {
        /// Trace of the span, 32 lowercase hex digits.
        trace_id: TraceId,
        /// The span, 16 lowercase hex digits.
        span_id: SpanId,
    },
}
