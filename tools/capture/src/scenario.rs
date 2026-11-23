//! Scenario files under `tools/capture/{py,ts}/scenarios/<group>/<case>/`.
//!
//! The JSON shape is `docs/plans/b2-capture/README.md` § The scenario format.
//! Usage numbers use OTel semantics: `input` includes cache tokens and `output`
//! includes reasoning tokens.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::fs;
use std::path::Path;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::error::CaptureError;

/// Fixture groups a capture may write.
///
/// `fixtures/otlp/README.md` § Layout, without `cetra` (D121).
pub const GROUPS: &[&str] = &[
    "genai",
    "openinference",
    "openllmetry",
    "vercel",
    "langfuse",
    "laminar",
    "google_adk",
    "claude_code",
    "codex",
    "gemini_cli",
    "edge",
];

const SETTING_KEYS: &[&str] = &["max_attribute_bytes", "media_min_bytes", "media_max_bytes"];
const CONTEXT_KEYS: &[&str] = &["project_id", "ingest_time"];
const ENV_PREFIXES: &[&str] = &[
    "OTEL_",
    "OPENINFERENCE_",
    "TRACELOOP_",
    "LANGFUSE_",
    "LMNR_",
    "ANTHROPIC_",
    "ADK_",
];
const ENV_FORBIDDEN: &[&str] = &["sk-", "pk-", "Bearer ", "@"];

/// `group/case` path of a captured fixture.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CasePath {
    /// Fixture group. One of [`GROUPS`].
    pub group: String,
    /// Kebab-case case name.
    pub case: String,
}

impl FromStr for CasePath {
    type Err = CaptureError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let Some((group, case)) = text.split_once('/') else {
            return Err(path_invalid(text, "case path must be group/case"));
        };
        if case.contains('/') || group.is_empty() || case.is_empty() {
            return Err(path_invalid(text, "case path must be group/case"));
        }
        if !GROUPS.contains(&group) {
            return Err(path_invalid(text, format!("unknown group \"{group}\"")));
        }
        if !is_kebab_case(case) {
            return Err(path_invalid(
                text,
                format!("case \"{case}\" must be kebab-case"),
            ));
        }
        Ok(Self {
            group: group.to_owned(),
            case: case.to_owned(),
        })
    }
}

impl fmt::Display for CasePath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}/{}", self.group, self.case)
    }
}

fn path_invalid(text: &str, reason: impl Into<String>) -> CaptureError {
    CaptureError::ScenarioInvalid {
        case: text.to_owned(),
        reason: reason.into(),
    }
}

fn is_kebab_case(text: &str) -> bool {
    let mut parts = text.split('-');
    match parts.next() {
        Some(part) if is_kebab_part(part) => parts.all(is_kebab_part),
        _ => false,
    }
}

fn is_kebab_part(text: &str) -> bool {
    !text.is_empty()
        && text
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
}

/// One capture: the program, its scripted turns, and the `case.json` fields copied through.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scenario {
    /// What the case exercises. Copied into `case.json`.
    pub description: String,
    /// Spec sections this case pins. At least one.
    pub spec_refs: Vec<String>,
    /// Free text copied into `case.json`.
    #[serde(default)]
    pub notes: Option<String>,
    /// Per-case ingest overrides. Keys are the `case.json` settings keys; values are integers.
    #[serde(default)]
    pub settings: Option<Map<String, Value>>,
    /// `project_id` and `ingest_time` overrides copied into `case.json`.
    #[serde(default)]
    pub context: Option<Map<String, Value>>,
    /// Package that produces the convention under test.
    pub instrumentation: String,
    /// Packages whose versions are recorded. Must contain [`Self::instrumentation`].
    pub libraries: Vec<String>,
    /// Extra environment for instrumentation switches only.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Attributes whose values are timestamps and must be shifted with the spans.
    #[serde(default)]
    pub timestamp_attributes: Vec<TimestampAttribute>,
    /// Measured durations or rates. Kept as captured; `just check-capture` ignores their values.
    #[serde(default)]
    pub timing_attributes: Vec<String>,
    /// Span names to keep. Empty keeps every span. Applied before the determinism steps.
    #[serde(default)]
    pub span_names: Vec<String>,
    /// Attribute keys whose string values are random per run (UUIDs, timestamped ids).
    /// Their values are replaced by `capture-volatile-<n>` everywhere they appear.
    #[serde(default)]
    pub volatile_attributes: Vec<String>,
    /// Scripted provider turns, served in order. At least one, all for the same provider.
    pub turns: Vec<Turn>,
    /// Fixture path, taken from the scenario directory by [`Scenario::load`].
    #[serde(skip, default)]
    pub case: CasePath,
}

/// An attribute whose value is a timestamp in one of three encodings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TimestampAttribute {
    /// Attribute key.
    pub key: String,
    /// How the value is encoded.
    pub format: TimestampFormat,
}

/// Encoding of a timestamp attribute.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TimestampFormat {
    /// RFC 3339 text. One pair of surrounding double quotes is the same instant:
    /// Langfuse stores `json.dumps` of a datetime, and the quotes stay.
    Rfc3339,
    /// Nanoseconds since the Unix epoch, as a number or a decimal string.
    UnixNano,
    /// Milliseconds since the Unix epoch, as a number or a decimal string.
    UnixMilli,
}

/// One scripted response from the mock provider.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Turn {
    /// Provider endpoint this turn answers.
    pub api: Api,
    /// Model id sent back on the response.
    pub model: String,
    /// The real host the program calls for this turn (D-IN-3); the mock checks the
    /// request's `Host` header against it. Absent: no check.
    #[serde(default)]
    pub host: Option<String>,
    /// Assistant message. Required for chat turns and absent for embeddings.
    #[serde(default)]
    pub message: Option<Message>,
    /// Finish reason. Required for chat turns and absent for embeddings.
    #[serde(default)]
    pub finish: Option<Finish>,
    /// Token counts in OTel semantics. Missing fields are 0.
    #[serde(default)]
    pub usage: Usage,
    /// Embedding width. Required for embedding turns, from 1 to 64, and absent otherwise.
    #[serde(default)]
    pub dims: Option<u32>,
    /// Delay before the first streamed chunk, in milliseconds.
    #[serde(default = "default_ttft_ms")]
    pub ttft_ms: u64,
    /// Delay between streamed chunks, in milliseconds.
    #[serde(default = "default_chunk_delay_ms")]
    pub chunk_delay_ms: u64,
    /// How many pieces streamed text is split into. At least 1.
    #[serde(default = "default_chunks")]
    pub chunks: u32,
}

fn default_ttft_ms() -> u64 {
    30
}

fn default_chunk_delay_ms() -> u64 {
    5
}

fn default_chunks() -> u32 {
    3
}

/// Provider endpoint a turn's request must hit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Api {
    /// OpenAI Chat Completions.
    #[serde(rename = "openai.chat")]
    OpenaiChat,
    /// OpenAI Responses.
    #[serde(rename = "openai.responses")]
    OpenaiResponses,
    /// OpenAI Embeddings.
    #[serde(rename = "openai.embeddings")]
    OpenaiEmbeddings,
    /// Anthropic Messages.
    #[serde(rename = "anthropic.messages")]
    AnthropicMessages,
    /// Gemini `generateContent` and `streamGenerateContent`.
    #[serde(rename = "gemini.generate_content")]
    GeminiGenerateContent,
    /// Gemini `embedContent` and `batchEmbedContents`.
    #[serde(rename = "gemini.embed_content")]
    GeminiEmbedContent,
    /// Cohere Chat (v2).
    #[serde(rename = "cohere.chat")]
    CohereChat,
    /// Ollama Chat.
    #[serde(rename = "ollama.chat")]
    OllamaChat,
}

impl Api {
    /// Provider that serves this endpoint.
    pub fn provider(self) -> Provider {
        match self {
            Self::OpenaiChat | Self::OpenaiResponses | Self::OpenaiEmbeddings => Provider::OpenAi,
            Self::AnthropicMessages => Provider::Anthropic,
            Self::GeminiGenerateContent | Self::GeminiEmbedContent => Provider::Gemini,
            Self::CohereChat => Provider::Cohere,
            Self::OllamaChat => Provider::Ollama,
        }
    }

    /// The serde name, which is how a scenario's `api` field spells the endpoint.
    pub fn name(self) -> &'static str {
        match self {
            Self::OpenaiChat => "openai.chat",
            Self::OpenaiResponses => "openai.responses",
            Self::OpenaiEmbeddings => "openai.embeddings",
            Self::AnthropicMessages => "anthropic.messages",
            Self::GeminiGenerateContent => "gemini.generate_content",
            Self::GeminiEmbedContent => "gemini.embed_content",
            Self::CohereChat => "cohere.chat",
            Self::OllamaChat => "ollama.chat",
        }
    }

    fn is_embedding(self) -> bool {
        matches!(self, Self::OpenaiEmbeddings | Self::GeminiEmbedContent)
    }
}

/// Provider family. A scenario uses only one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    /// `api.openai.com`.
    OpenAi,
    /// `api.anthropic.com`.
    Anthropic,
    /// `generativelanguage.googleapis.com`.
    Gemini,
    /// `api.cohere.com`.
    Cohere,
    /// `ollama.com`.
    Ollama,
}

impl Provider {
    /// Real host written back over the mock address.
    pub fn host(self) -> &'static str {
        match self {
            Self::OpenAi => "api.openai.com",
            Self::Anthropic => "api.anthropic.com",
            Self::Gemini => "generativelanguage.googleapis.com",
            Self::Cohere => "api.cohere.com",
            Self::Ollama => "ollama.com",
        }
    }
}

/// Assistant message returned for a chat turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Message {
    /// Visible reply text.
    #[serde(default)]
    pub text: Option<String>,
    /// Reasoning or thinking text.
    #[serde(default)]
    pub reasoning: Option<String>,
    /// Tool calls. Empty when the model did not call a tool.
    #[serde(default)]
    pub tool_calls: Vec<ToolCall>,
}

/// One tool call on an assistant message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolCall {
    /// Provider tool-call id. Unique within the scenario.
    pub id: String,
    /// Tool name. Non-empty.
    pub name: String,
    /// Tool arguments. A JSON object.
    pub arguments: Value,
}

/// Why the model stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Finish {
    /// The model finished the reply.
    Stop,
    /// The model called one or more tools.
    ToolCalls,
    /// The model hit the output limit.
    Length,
}

/// Token counts in OTel semantics.
///
/// `input` includes `cache_read + cache_write`. `output` includes `reasoning`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct Usage {
    /// Input tokens, including cache tokens.
    pub input: u64,
    /// Output tokens, including reasoning tokens.
    pub output: u64,
    /// Cache tokens read, already included in `input`.
    pub cache_read: u64,
    /// Cache tokens written, already included in `input`.
    pub cache_write: u64,
    /// Reasoning tokens, already included in `output`.
    pub reasoning: u64,
}

impl Scenario {
    /// Reads `dir/scenario.json` and checks it.
    ///
    /// `dir`'s last two components are the case path, for example `genai/chat-tool`.
    pub fn load(dir: &Path) -> Result<Self, CaptureError> {
        let path = dir.join("scenario.json");
        let text = fs::read_to_string(&path).map_err(|source| CaptureError::ScenarioIo {
            path: path.clone(),
            source,
        })?;
        let mut scenario: Self = serde_json::from_str(&text)
            .map_err(|source| CaptureError::ScenarioJson { path, source })?;
        scenario.case = case_path_from_dir(dir)?;
        scenario.validate()?;
        Ok(scenario)
    }

    /// Checks the rules in `docs/plans/b2-capture/01-harness.md` § K2.
    ///
    /// [`Self::load`] calls this. The error's case is [`Self::case`].
    pub fn validate(&self) -> Result<(), CaptureError> {
        self.check()
            .map_err(|reason| CaptureError::ScenarioInvalid {
                case: self.case.to_string(),
                reason,
            })
    }

    fn check(&self) -> Result<(), String> {
        if self.spec_refs.is_empty() {
            return Err("spec_refs must be non-empty".to_owned());
        }
        if self.libraries.is_empty() {
            return Err("libraries must be non-empty".to_owned());
        }
        if !self
            .libraries
            .iter()
            .any(|library| library == &self.instrumentation)
        {
            return Err("libraries must contain instrumentation".to_owned());
        }
        if self.turns.is_empty() {
            return Err("turns must be non-empty".to_owned());
        }
        reject_duplicates(&self.libraries, "libraries")?;
        reject_duplicates(
            &self
                .timestamp_attributes
                .iter()
                .map(|attribute| attribute.key.as_str())
                .collect::<Vec<_>>(),
            "timestamp_attributes",
        )?;
        reject_duplicates(&self.timing_attributes, "timing_attributes")?;
        for (field, values) in [
            ("span_names", &self.span_names),
            ("volatile_attributes", &self.volatile_attributes),
        ] {
            if values.iter().any(String::is_empty) {
                return Err(format!("{field} entries must be non-empty"));
            }
            reject_duplicates(values, field)?;
        }
        self.check_env()?;
        check_string_map(self.settings.as_ref(), SETTING_KEYS, "settings", true)?;
        check_string_map(self.context.as_ref(), CONTEXT_KEYS, "context", false)?;
        self.check_turns()?;
        self.check_tool_calls()?;
        Ok(())
    }

    fn check_env(&self) -> Result<(), String> {
        for (key, value) in &self.env {
            if !ENV_PREFIXES.iter().any(|prefix| key.starts_with(prefix)) {
                return Err(format!(
                    "env key \"{key}\" must start with OTEL_, OPENINFERENCE_, TRACELOOP_, LANGFUSE_, LMNR_, ANTHROPIC_, or ADK_"
                ));
            }
            if let Some(marker) = ENV_FORBIDDEN.iter().find(|marker| value.contains(**marker)) {
                return Err(format!("env key \"{key}\" must not contain \"{marker}\""));
            }
        }
        Ok(())
    }

    fn check_turns(&self) -> Result<(), String> {
        let provider = self.turns[0].api.provider();
        for (index, turn) in self.turns.iter().enumerate() {
            if turn.api.provider() != provider {
                return Err(format!(
                    "turns[{index}].api provider differs from turns[0].api"
                ));
            }
            if turn.chunks < 1 {
                return Err(format!("turns[{index}].chunks must be at least 1"));
            }
            if let Some(host) = &turn.host
                && !is_dns_name(host)
            {
                return Err(format!("turns[{index}].host must be a lowercase DNS name"));
            }
            if turn.api.is_embedding() {
                check_embedding(index, turn)?;
            } else {
                check_chat(index, turn)?;
            }
            check_usage(index, turn)?;
        }
        Ok(())
    }

    fn check_tool_calls(&self) -> Result<(), String> {
        let mut seen: HashMap<&str, (usize, usize)> = HashMap::new();
        for (index, turn) in self.turns.iter().enumerate() {
            let Some(message) = &turn.message else {
                continue;
            };
            for (tool, call) in message.tool_calls.iter().enumerate() {
                if call.name.is_empty() {
                    return Err(format!(
                        "turns[{index}].message.tool_calls[{tool}].name must be non-empty"
                    ));
                }
                if !call.arguments.is_object() {
                    return Err(format!(
                        "turns[{index}].message.tool_calls[{tool}].arguments must be a JSON object"
                    ));
                }
                if let Some(&(first_index, first_tool)) = seen.get(call.id.as_str()) {
                    return Err(format!(
                        "turns[{index}].message.tool_calls[{tool}].id duplicates turns[{first_index}].message.tool_calls[{first_tool}].id"
                    ));
                }
                seen.insert(call.id.as_str(), (index, tool));
            }
        }
        Ok(())
    }
}

fn check_chat(index: usize, turn: &Turn) -> Result<(), String> {
    let Some(message) = &turn.message else {
        return Err(format!("turns[{index}].message is required"));
    };
    let Some(finish) = turn.finish else {
        return Err(format!("turns[{index}].finish is required"));
    };
    if turn.dims.is_some() {
        return Err(format!("turns[{index}].dims must be absent"));
    }
    if turn.api == Api::OpenaiChat && present_text(message.reasoning.as_deref()) {
        return Err(format!("turns[{index}].message.reasoning must be absent"));
    }
    if !present_text(message.text.as_deref())
        && !present_text(message.reasoning.as_deref())
        && message.tool_calls.is_empty()
    {
        return Err(format!(
            "turns[{index}].message needs text, reasoning, or a tool call"
        ));
    }
    if matches!(turn.api, Api::CohereChat | Api::OllamaChat) {
        if !message.tool_calls.is_empty() {
            return Err(format!(
                "turns[{index}].message.tool_calls are not supported by {}",
                turn.api.name()
            ));
        }
        if present_text(message.reasoning.as_deref()) {
            return Err(format!(
                "turns[{index}].message.reasoning is not supported by {}",
                turn.api.name()
            ));
        }
        if !matches!(finish, Finish::Stop | Finish::Length) {
            return Err(format!(
                "turns[{index}].finish must be stop or length for {}",
                turn.api.name()
            ));
        }
    }
    let has_tools = !message.tool_calls.is_empty();
    if has_tools != matches!(finish, Finish::ToolCalls) {
        return Err(format!(
            "turns[{index}].finish must be tool_calls exactly when tool calls are present"
        ));
    }
    Ok(())
}

fn check_embedding(index: usize, turn: &Turn) -> Result<(), String> {
    match turn.dims {
        Some(dims) if (1..=64).contains(&dims) => {}
        Some(_) => return Err(format!("turns[{index}].dims must be from 1 to 64")),
        None => return Err(format!("turns[{index}].dims is required")),
    }
    if turn.message.is_some() {
        return Err(format!("turns[{index}].message must be absent"));
    }
    if turn.finish.is_some() {
        return Err(format!("turns[{index}].finish must be absent"));
    }
    Ok(())
}

fn check_usage(index: usize, turn: &Turn) -> Result<(), String> {
    let usage = &turn.usage;
    let Some(cache) = usage.cache_read.checked_add(usage.cache_write) else {
        return Err(format!(
            "turns[{index}].usage.input must be at least cache_read + cache_write"
        ));
    };
    if usage.input < cache {
        return Err(format!(
            "turns[{index}].usage.input must be at least cache_read + cache_write"
        ));
    }
    if usage.output < usage.reasoning {
        return Err(format!(
            "turns[{index}].usage.output must be at least reasoning"
        ));
    }
    match turn.api.provider() {
        Provider::OpenAi | Provider::Gemini if usage.cache_write != 0 => {
            return Err(format!("turns[{index}].usage.cache_write must be 0"));
        }
        Provider::Anthropic if usage.reasoning != 0 => {
            return Err(format!("turns[{index}].usage.reasoning must be 0"));
        }
        Provider::Cohere | Provider::Ollama => {
            for (field, value) in [
                ("cache_read", usage.cache_read),
                ("cache_write", usage.cache_write),
                ("reasoning", usage.reasoning),
            ] {
                if value != 0 {
                    return Err(format!(
                        "turns[{index}].usage.{field} is not reported by {}",
                        turn.api.name()
                    ));
                }
            }
        }
        Provider::OpenAi | Provider::Anthropic | Provider::Gemini => {}
    }
    if turn.api.is_embedding()
        && (usage.output != 0
            || usage.cache_read != 0
            || usage.cache_write != 0
            || usage.reasoning != 0)
    {
        return Err(format!("turns[{index}].usage must contain only input"));
    }
    if turn.api == Api::GeminiEmbedContent && usage.input != 0 {
        return Err(format!("turns[{index}].usage must be all zero"));
    }
    Ok(())
}

/// A present string counts when it is non-empty. `null` and `""` do not.
/// A lowercase DNS name: 1 to 253 bytes of `a-z`, `0-9`, `-` and `.`, with at least one `.`,
/// and neither end is `.` or `-`.
fn is_dns_name(host: &str) -> bool {
    let bytes = host.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 253
        && bytes.contains(&b'.')
        && bytes.iter().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'.')
        })
        && !matches!(bytes.first(), Some(b'.' | b'-'))
        && !matches!(bytes.last(), Some(b'.' | b'-'))
}

fn present_text(text: Option<&str>) -> bool {
    text.is_some_and(|text| !text.is_empty())
}

fn reject_duplicates(values: &[impl AsRef<str>], field: &str) -> Result<(), String> {
    let mut seen = HashSet::new();
    for value in values {
        let value = value.as_ref();
        if !seen.insert(value.to_owned()) {
            return Err(format!("{field} value \"{value}\" is duplicated"));
        }
    }
    Ok(())
}

fn check_string_map(
    map: Option<&Map<String, Value>>,
    allowed: &[&str],
    field: &str,
    integers: bool,
) -> Result<(), String> {
    let Some(map) = map else {
        return Ok(());
    };
    for (key, value) in map {
        if !allowed.contains(&key.as_str()) {
            return Err(format!("{field} key \"{key}\" is not allowed"));
        }
        if integers && !value.is_i64() && !value.is_u64() {
            return Err(format!("{field}.{key} must be an integer"));
        }
    }
    Ok(())
}

fn case_path_from_dir(dir: &Path) -> Result<CasePath, CaptureError> {
    let case_name = dir.file_name().and_then(|name| name.to_str());
    let group = dir
        .parent()
        .and_then(|parent| parent.file_name())
        .and_then(|name| name.to_str());
    match (group, case_name) {
        (Some(group), Some(case_name)) => format!("{group}/{case_name}").parse(),
        _ => Err(CaptureError::ScenarioInvalid {
            case: dir.display().to_string(),
            reason: "scenario directory must be <group>/<case>".to_owned(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    use serde_json::json;

    use super::*;

    const FULL: &str = r#"{
      "description": "OpenAI chat completion with one tool call, traced by OpenInference.",
      "spec_refs": ["02 § Mapping tables / Input / output"],
      "notes": "Optional free text, copied into case.json.",
      "settings": { "media_min_bytes": 64 },
      "context": { "ingest_time": "2026-01-01T00:00:00.000000000Z" },
      "instrumentation": "openinference-instrumentation-openai",
      "libraries": ["openai", "openinference-instrumentation-openai", "opentelemetry-sdk"],
      "env": { "OTEL_INSTRUMENTATION_GENAI_CAPTURE_MESSAGE_CONTENT": "true" },
      "timestamp_attributes": [{ "key": "langfuse.observation.completion_start_time", "format": "rfc3339" }],
      "timing_attributes": ["ai.response.msToFirstChunk"],
      "turns": [
        {
          "api": "openai.chat",
          "model": "gpt-4o-mini-2024-07-18",
          "message": {
            "text": null,
            "reasoning": null,
            "tool_calls": [{ "id": "call_weather_1", "name": "get_weather", "arguments": { "city": "Paris" } }]
          },
          "finish": "tool_calls",
          "usage": { "input": 82, "output": 17, "cache_read": 0, "cache_write": 0, "reasoning": 0 },
          "ttft_ms": 30,
          "chunk_delay_ms": 5,
          "chunks": 3
        }
      ]
    }"#;

    fn chat() -> Value {
        json!({
            "description": "A chat turn.",
            "spec_refs": ["02 § Mapping tables / Input / output"],
            "instrumentation": "openinference-instrumentation-openai",
            "libraries": ["openai", "openinference-instrumentation-openai"],
            "turns": [{
                "api": "openai.chat",
                "model": "gpt-4o-mini-2024-07-18",
                "message": { "text": "hello" },
                "finish": "stop",
                "usage": { "input": 10, "output": 2 }
            }]
        })
    }

    fn embedding(api: &str) -> Value {
        json!({
            "description": "An embedding turn.",
            "spec_refs": ["02 § Mapping tables / Input / output"],
            "instrumentation": "openai",
            "libraries": ["openai"],
            "turns": [{
                "api": api,
                "model": "text-embedding-3-small",
                "dims": 8,
                "usage": { "input": 6 }
            }]
        })
    }

    fn load_value(value: Value) -> Result<Scenario, CaptureError> {
        let mut scenario: Scenario =
            serde_json::from_value(value).expect("test scenario JSON should parse");
        scenario.case = "genai/chat-tool".parse().expect("fixture case path");
        scenario.validate()?;
        Ok(scenario)
    }

    fn reason(value: Value) -> String {
        match load_value(value) {
            Err(CaptureError::ScenarioInvalid { case, reason }) => {
                assert_eq!(case, "genai/chat-tool");
                reason
            }
            Ok(_) => panic!("expected an invalid scenario"),
            Err(error) => panic!("expected ScenarioInvalid, got {error}"),
        }
    }

    fn assert_reason(value: Value, expected: &str) {
        let reason = reason(value);
        assert!(
            reason.contains(expected),
            "reason {reason:?} does not contain {expected:?}"
        );
    }

    fn temp_case(group: &str, case_name: &str) -> PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir()
            .join(format!("cetra-capture-{}-{n}", std::process::id()))
            .join(group)
            .join(case_name);
        fs::create_dir_all(&dir).expect("create scenario dir");
        dir
    }

    #[test]
    fn full_scenario_round_trips() {
        let value: Value = serde_json::from_str(FULL).expect("full scenario JSON");
        let dir = temp_case("genai", "chat-tool");
        fs::write(
            dir.join("scenario.json"),
            serde_json::to_vec(&value).expect("json"),
        )
        .expect("write");
        let scenario = Scenario::load(&dir).expect("load");
        assert_eq!(scenario.case.to_string(), "genai/chat-tool");
        assert_eq!(scenario.turns[0].api, Api::OpenaiChat);
        assert_eq!(scenario.turns[0].api.provider(), Provider::OpenAi);
        assert_eq!(scenario.turns[0].api.provider().host(), "api.openai.com");
        assert_eq!(Provider::Anthropic.host(), "api.anthropic.com");
        assert_eq!(Provider::Gemini.host(), "generativelanguage.googleapis.com");
        assert_eq!(Provider::Cohere.host(), "api.cohere.com");
        assert_eq!(Provider::Ollama.host(), "ollama.com");
        assert_eq!(scenario.turns[0].finish, Some(Finish::ToolCalls));
        assert_eq!(
            scenario.timestamp_attributes[0].format,
            TimestampFormat::Rfc3339
        );

        let encoded = serde_json::to_string(&scenario).expect("serialize");
        assert!(encoded.contains("\"openai.chat\""));
        assert!(encoded.contains("\"tool_calls\""));
        assert!(encoded.contains("\"rfc3339\""));
        assert!(!encoded.contains("\"case\""));
        let mut again: Scenario = serde_json::from_str(&encoded).expect("deserialize");
        again.case = scenario.case.clone();
        assert_eq!(again, scenario);
    }

    #[test]
    fn turn_host_must_be_a_lowercase_dns_name() {
        for host in ["openrouter.ai", "api.novita.ai", "a-b.c1.io"] {
            let mut value = chat();
            value["turns"][0]["host"] = json!(host);
            assert!(load_value(value).is_ok(), "{host} should be accepted");
        }
        for host in [
            "OpenRouter.ai",
            "openrouter.ai:443",
            "https://openrouter.ai",
            "localhost",
            ".openrouter.ai",
            "openrouter.ai.",
            "-openrouter.ai",
            "",
        ] {
            let mut value = chat();
            value["turns"][0]["host"] = json!(host);
            assert_reason(value, "turns[0].host must be a lowercase DNS name");
        }
        let long = format!("{}.com", "a".repeat(250));
        let mut value = chat();
        value["turns"][0]["host"] = json!(long);
        assert_reason(value, "turns[0].host must be a lowercase DNS name");
    }

    #[test]
    fn defaults_apply() {
        let scenario = load_value(embedding("openai.embeddings")).expect("defaults");
        let turn = &scenario.turns[0];
        assert!(turn.host.is_none());
        assert_eq!(turn.ttft_ms, 30);
        assert_eq!(turn.chunk_delay_ms, 5);
        assert_eq!(turn.chunks, 3);
        assert_eq!(turn.usage.output, 0);
        assert_eq!(turn.usage.cache_read, 0);
        assert_eq!(turn.usage.cache_write, 0);
        assert_eq!(turn.usage.reasoning, 0);
        assert!(turn.message.is_none());
        assert!(turn.finish.is_none());
        assert!(scenario.notes.is_none());
        assert!(scenario.settings.is_none());
        assert!(scenario.context.is_none());
        assert!(scenario.env.is_empty());
        assert!(scenario.timestamp_attributes.is_empty());
        assert!(scenario.timing_attributes.is_empty());
        assert!(scenario.span_names.is_empty());
        assert!(scenario.volatile_attributes.is_empty());
    }

    #[test]
    fn responses_reasoning_and_anthropic_thinking_text_are_allowed() {
        let mut responses = chat();
        responses["turns"][0]["api"] = json!("openai.responses");
        responses["turns"][0]["message"]["reasoning"] = json!("thinking");
        responses["turns"][0]["usage"]["reasoning"] = json!(4);
        responses["turns"][0]["usage"]["output"] = json!(4);
        load_value(responses).expect("responses may return reasoning text");

        let mut anthropic = chat();
        anthropic["instrumentation"] = json!("anthropic");
        anthropic["libraries"] = json!(["anthropic"]);
        anthropic["turns"][0]["api"] = json!("anthropic.messages");
        anthropic["turns"][0]["message"]["reasoning"] = json!("thinking");
        load_value(anthropic)
            .expect("anthropic thinking text is allowed when usage.reasoning is 0");
    }

    #[test]
    fn empty_reasoning_is_not_openai_chat_reasoning() {
        let mut value = chat();
        value["turns"][0]["message"]["reasoning"] = json!("");
        load_value(value).expect("empty reasoning is absent");
    }

    #[test]
    fn one_provider_per_scenario() {
        let mut value = chat();
        value["turns"].as_array_mut().expect("turns")[0] = chat()["turns"][0].clone();
        let mut second = chat()["turns"][0].clone();
        second["api"] = json!("anthropic.messages");
        value["turns"].as_array_mut().expect("turns").push(second);
        assert_reason(value, "turns[1].api provider differs from turns[0].api");
    }

    #[test]
    fn chat_message_and_finish_are_required() {
        let mut value = chat();
        value["turns"][0]["message"] = Value::Null;
        assert_reason(value, "turns[0].message is required");

        let mut value = chat();
        value["turns"][0]["finish"] = Value::Null;
        assert_reason(value, "turns[0].finish is required");
    }

    #[test]
    fn chat_message_needs_content() {
        let mut value = chat();
        value["turns"][0]["message"] = json!({});
        assert_reason(
            value,
            "turns[0].message needs text, reasoning, or a tool call",
        );
    }

    #[test]
    fn chat_dims_must_be_absent() {
        let mut value = chat();
        value["turns"][0]["dims"] = json!(8);
        assert_reason(value, "turns[0].dims must be absent");
    }

    #[test]
    fn finish_matches_tool_calls() {
        let mut value = chat();
        value["turns"][0]["finish"] = json!("tool_calls");
        assert_reason(
            value,
            "turns[0].finish must be tool_calls exactly when tool calls are present",
        );

        let mut value = chat();
        value["turns"][0]["message"]["tool_calls"] =
            json!([{ "id": "call_1", "name": "get_weather", "arguments": {} }]);
        assert_reason(
            value,
            "turns[0].finish must be tool_calls exactly when tool calls are present",
        );
    }

    #[test]
    fn embedding_shape_is_dims_only() {
        let mut value = embedding("openai.embeddings");
        value["turns"][0]["dims"] = Value::Null;
        assert_reason(value, "turns[0].dims is required");

        let mut value = embedding("openai.embeddings");
        value["turns"][0]["dims"] = json!(0);
        assert_reason(value, "turns[0].dims must be from 1 to 64");

        let mut value = embedding("openai.embeddings");
        value["turns"][0]["dims"] = json!(65);
        assert_reason(value, "turns[0].dims must be from 1 to 64");

        let mut value = embedding("openai.embeddings");
        value["turns"][0]["message"] = json!({ "text": "hello" });
        assert_reason(value, "turns[0].message must be absent");

        let mut value = embedding("openai.embeddings");
        value["turns"][0]["finish"] = json!("stop");
        assert_reason(value, "turns[0].finish must be absent");

        let mut value = embedding("openai.embeddings");
        value["turns"][0]["usage"]["output"] = json!(1);
        assert_reason(value, "turns[0].usage must contain only input");
    }

    #[test]
    fn usage_is_consistent() {
        let mut value = chat();
        value["turns"][0]["usage"]["cache_read"] = json!(11);
        assert_reason(
            value,
            "turns[0].usage.input must be at least cache_read + cache_write",
        );

        let mut value = chat();
        value["turns"][0]["api"] = json!("openai.responses");
        value["turns"][0]["usage"]["reasoning"] = json!(3);
        assert_reason(value, "turns[0].usage.output must be at least reasoning");
    }

    #[test]
    fn providers_report_only_what_their_api_has() {
        let mut value = chat();
        value["turns"][0]["usage"]["cache_write"] = json!(1);
        value["turns"][0]["usage"]["input"] = json!(1);
        assert_reason(value, "turns[0].usage.cache_write must be 0");

        let mut value = chat();
        value["turns"][0]["message"]["reasoning"] = json!("thinking");
        assert_reason(value, "turns[0].message.reasoning must be absent");

        let mut value = chat();
        value["turns"][0]["api"] = json!("anthropic.messages");
        value["turns"][0]["usage"]["reasoning"] = json!(1);
        value["turns"][0]["usage"]["output"] = json!(1);
        assert_reason(value, "turns[0].usage.reasoning must be 0");

        let mut value = chat();
        value["turns"][0]["api"] = json!("gemini.generate_content");
        value["turns"][0]["usage"]["cache_write"] = json!(1);
        value["turns"][0]["usage"]["input"] = json!(1);
        assert_reason(value, "turns[0].usage.cache_write must be 0");

        let mut value = embedding("gemini.embed_content");
        value["turns"][0]["usage"]["input"] = json!(6);
        assert_reason(value, "turns[0].usage must be all zero");

        let mut value = chat();
        value["turns"][0]["api"] = json!("cohere.chat");
        value["turns"][0]["usage"]["cache_read"] = json!(1);
        assert_reason(
            value,
            "turns[0].usage.cache_read is not reported by cohere.chat",
        );

        let mut value = chat();
        value["turns"][0]["api"] = json!("ollama.chat");
        value["turns"][0]["message"]["tool_calls"] =
            json!([{ "id": "call_1", "name": "get_weather", "arguments": {} }]);
        value["turns"][0]["finish"] = json!("tool_calls");
        assert_reason(
            value,
            "turns[0].message.tool_calls are not supported by ollama.chat",
        );
    }

    #[test]
    fn cohere_and_ollama_chat_have_no_reasoning_and_only_stop_or_length() {
        let mut value = chat();
        value["turns"][0]["api"] = json!("cohere.chat");
        value["turns"][0]["message"]["reasoning"] = json!("thinking");
        assert_reason(
            value,
            "turns[0].message.reasoning is not supported by cohere.chat",
        );

        let mut value = chat();
        value["turns"][0]["api"] = json!("ollama.chat");
        value["turns"][0]["finish"] = json!("tool_calls");
        assert_reason(
            value,
            "turns[0].finish must be stop or length for ollama.chat",
        );

        let mut value = chat();
        value["turns"][0]["api"] = json!("ollama.chat");
        value["turns"][0]["finish"] = json!("length");
        load_value(value).expect("ollama.chat may stop at the length limit");
    }

    #[test]
    fn api_name_is_the_serde_name() {
        let apis = [
            Api::OpenaiChat,
            Api::OpenaiResponses,
            Api::OpenaiEmbeddings,
            Api::AnthropicMessages,
            Api::GeminiGenerateContent,
            Api::GeminiEmbedContent,
            Api::CohereChat,
            Api::OllamaChat,
        ];
        for api in apis {
            assert_eq!(
                serde_json::to_value(api).expect("serialize"),
                json!(api.name())
            );
        }
    }

    #[test]
    fn tool_call_ids_are_unique_and_names_are_non_empty() {
        let mut value = chat();
        value["turns"][0]["finish"] = json!("tool_calls");
        value["turns"][0]["message"]["tool_calls"] = json!([
            { "id": "call_1", "name": "", "arguments": {} }
        ]);
        assert_reason(
            value,
            "turns[0].message.tool_calls[0].name must be non-empty",
        );

        let mut value = chat();
        value["turns"][0]["finish"] = json!("tool_calls");
        value["turns"][0]["message"]["tool_calls"] = json!([
            { "id": "call_1", "name": "get_weather", "arguments": [] }
        ]);
        assert_reason(
            value,
            "turns[0].message.tool_calls[0].arguments must be a JSON object",
        );

        let mut value = chat();
        let call = json!({ "id": "call_1", "name": "get_weather", "arguments": {} });
        value["turns"][0]["finish"] = json!("tool_calls");
        value["turns"][0]["message"]["tool_calls"] = json!([call]);
        let mut second = value["turns"][0].clone();
        second["message"]["tool_calls"] = json!([call]);
        value["turns"].as_array_mut().expect("turns").push(second);
        assert_reason(
            value,
            "turns[1].message.tool_calls[0].id duplicates turns[0].message.tool_calls[0].id",
        );
    }

    #[test]
    fn env_keys_are_prefixes_and_values_are_not_secrets() {
        let mut value = chat();
        value["env"] = json!({ "SECRET": "1" });
        assert_reason(
            value,
            "env key \"SECRET\" must start with OTEL_, OPENINFERENCE_, TRACELOOP_, LANGFUSE_, LMNR_, ANTHROPIC_, or ADK_",
        );
        let mut allowed = chat();
        allowed["env"] = json!({ "ANTHROPIC_OPEN_TELEMETRY": "false" });
        load_value(allowed).expect("ANTHROPIC_ prefix");
        let mut adk = chat();
        adk["env"] = json!({ "ADK_CAPTURE_MESSAGE_CONTENT_IN_SPANS": "true" });
        load_value(adk).expect("ADK_ prefix");

        for marker in ["sk-", "pk-", "Bearer ", "@"] {
            let mut value = chat();
            value["env"] = json!({ "OTEL_TEST": format!("x{marker}y") });
            assert_reason(
                value,
                &format!("env key \"OTEL_TEST\" must not contain \"{marker}\""),
            );
        }
    }

    #[test]
    fn lists_have_no_duplicates() {
        let mut value = chat();
        value["libraries"] = json!([
            "openinference-instrumentation-openai",
            "openinference-instrumentation-openai"
        ]);
        assert_reason(
            value,
            "libraries value \"openinference-instrumentation-openai\" is duplicated",
        );

        let mut value = chat();
        value["timestamp_attributes"] = json!([
            { "key": "a", "format": "unix_nano" },
            { "key": "a", "format": "unix_milli" }
        ]);
        assert_reason(value, "timestamp_attributes value \"a\" is duplicated");

        let mut value = chat();
        value["timing_attributes"] =
            json!(["ai.response.msToFirstChunk", "ai.response.msToFirstChunk"]);
        assert_reason(
            value,
            "timing_attributes value \"ai.response.msToFirstChunk\" is duplicated",
        );
    }

    #[test]
    fn span_names_and_volatile_attributes_reject_empty_and_duplicate_entries() {
        let mut value = chat();
        value["span_names"] = json!(["ok", ""]);
        assert_reason(value, "span_names entries must be non-empty");

        let mut value = chat();
        value["volatile_attributes"] = json!([""]);
        assert_reason(value, "volatile_attributes entries must be non-empty");

        let mut value = chat();
        value["span_names"] = json!(["a", "a"]);
        assert_reason(value, "span_names value \"a\" is duplicated");

        let mut value = chat();
        value["volatile_attributes"] = json!(["session.id", "session.id"]);
        assert_reason(
            value,
            "volatile_attributes value \"session.id\" is duplicated",
        );

        let mut value = chat();
        value["span_names"] = json!(["chat"]);
        value["volatile_attributes"] = json!(["session.id"]);
        load_value(value).expect("valid span_names and volatile_attributes");
    }

    #[test]
    fn required_lists_and_library_membership() {
        let mut value = chat();
        value["spec_refs"] = json!([]);
        assert_reason(value, "spec_refs must be non-empty");

        let mut value = chat();
        value["libraries"] = json!([]);
        assert_reason(value, "libraries must be non-empty");

        let mut value = chat();
        value["libraries"] = json!(["openai"]);
        assert_reason(value, "libraries must contain instrumentation");

        let mut value = chat();
        value["turns"] = json!([]);
        assert_reason(value, "turns must be non-empty");

        let mut value = chat();
        value["turns"][0]["chunks"] = json!(0);
        assert_reason(value, "turns[0].chunks must be at least 1");
    }

    #[test]
    fn settings_and_context_keys_are_limited() {
        let mut value = chat();
        value["settings"] = json!({ "nope": 1 });
        assert_reason(value, "settings key \"nope\" is not allowed");

        let mut value = chat();
        value["settings"] = json!({ "media_min_bytes": 1.5 });
        assert_reason(value, "settings.media_min_bytes must be an integer");

        let mut value = chat();
        value["context"] = json!({ "user": "x" });
        assert_reason(value, "context key \"user\" is not allowed");

        let mut value = chat();
        value["settings"] = json!({ "media_min_bytes": 64, "media_max_bytes": 128 });
        value["context"] = json!({
            "project_id": "01900000-0000-7000-8000-000000000001",
            "ingest_time": "2026-01-01T00:00:00.000000000Z"
        });
        load_value(value).expect("allowed settings and context keys");
    }

    #[test]
    fn unknown_fields_are_rejected() {
        let mut value = chat();
        value["extra"] = json!(true);
        let error = serde_json::from_value::<Scenario>(value).expect_err("unknown field");
        assert!(error.to_string().contains("extra"), "{error}");

        let mut value = chat();
        value["turns"][0]["extra"] = json!(true);
        let error = serde_json::from_value::<Scenario>(value).expect_err("unknown turn field");
        assert!(error.to_string().contains("extra"), "{error}");

        let mut value = chat();
        value["turns"][0]["usage"]["extra"] = json!(1);
        let error = serde_json::from_value::<Scenario>(value).expect_err("unknown usage field");
        assert!(error.to_string().contains("extra"), "{error}");

        let mut value = chat();
        value["turns"][0]["message"]["extra"] = json!(true);
        let error = serde_json::from_value::<Scenario>(value).expect_err("unknown message field");
        assert!(error.to_string().contains("extra"), "{error}");
    }

    #[test]
    fn case_path_accepts_the_coding_agent_groups() {
        for group in ["claude_code", "codex", "gemini_cli"] {
            let text = format!("{group}/x");
            let path: CasePath = text.parse().expect(&text);
            assert_eq!(path.to_string(), text);
        }
    }

    #[test]
    fn case_path_parsing() {
        let path: CasePath = "google_adk/agent-tool"
            .parse()
            .expect("group with underscore");
        assert_eq!(path.to_string(), "google_adk/agent-tool");
        assert_eq!(
            "edge/a1"
                .parse::<CasePath>()
                .expect("short case")
                .to_string(),
            "edge/a1"
        );

        let error = "cetra/chat-tool".parse::<CasePath>().expect_err("cetra");
        let CaptureError::ScenarioInvalid { case, reason } = error else {
            panic!("expected ScenarioInvalid");
        };
        assert_eq!(case, "cetra/chat-tool");
        assert!(reason.contains("unknown group \"cetra\""), "{reason}");

        let error = "nope/chat-tool".parse::<CasePath>().expect_err("bad group");
        let CaptureError::ScenarioInvalid { reason, .. } = error else {
            panic!("expected ScenarioInvalid");
        };
        assert!(reason.contains("unknown group \"nope\""), "{reason}");

        for bad in [
            "genai/Chat",
            "genai/a--b",
            "genai/a-",
            "genai/-a",
            "genai/a_b",
        ] {
            let error = bad.parse::<CasePath>().expect_err(bad);
            let CaptureError::ScenarioInvalid { reason, .. } = error else {
                panic!("expected ScenarioInvalid");
            };
            assert!(reason.contains("kebab-case"), "{bad}: {reason}");
        }

        let error = "genai".parse::<CasePath>().expect_err("one component");
        let CaptureError::ScenarioInvalid { reason, .. } = error else {
            panic!("expected ScenarioInvalid");
        };
        assert!(reason.contains("group/case"), "{reason}");
    }

    #[test]
    fn load_reports_io_json_and_validation_errors() {
        let missing = temp_case("genai", "missing");
        let absent = missing.parent().expect("group").join("absent");
        let error = Scenario::load(&absent).expect_err("missing file");
        assert!(matches!(error, CaptureError::ScenarioIo { .. }), "{error}");

        let dir = temp_case("genai", "bad-json");
        fs::write(dir.join("scenario.json"), b"not json").expect("write");
        let error = Scenario::load(&dir).expect_err("bad json");
        assert!(
            matches!(error, CaptureError::ScenarioJson { .. }),
            "{error}"
        );

        let dir = temp_case("genai", "empty-turns");
        let mut value = chat();
        value["turns"] = json!([]);
        fs::write(
            dir.join("scenario.json"),
            serde_json::to_vec(&value).expect("json"),
        )
        .expect("write");
        let error = Scenario::load(&dir).expect_err("invalid");
        let CaptureError::ScenarioInvalid { case, reason } = error else {
            panic!("expected ScenarioInvalid");
        };
        assert_eq!(case, "genai/empty-turns");
        assert!(reason.contains("turns must be non-empty"), "{reason}");
    }
}
