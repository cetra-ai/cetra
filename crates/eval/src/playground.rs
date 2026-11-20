//! Playground runs (D-V-21, D-V-22, D-V-37): rendering the request's messages and building the
//! run's trace for the normalizer, as judge traces are built (D161).

use std::collections::BTreeMap;
use std::fmt;

use cetra_core::enums::SpanKind;
use cetra_core::ids::{SpanId, TraceId};
use cetra_core::message::{Message, Part, messages_to_json};
use cetra_core::names::{attr, environment, metadata_key};
use cetra_otel::{IngestContext, Normalized, PriceTable, RejectReason, normalize};
use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use opentelemetry_proto::tonic::common::v1::{InstrumentationScope, KeyValue};
use opentelemetry_proto::tonic::resource::v1::Resource;
use opentelemetry_proto::tonic::trace::v1::{ResourceSpans, ScopeSpans, Span};
use serde_json::{Map, Value};
use uuid::Uuid;

use crate::chat::{ChatRequest, TokenUsage, ToolDefinition};
use crate::judge::Adapter;
use crate::plain_template;
use crate::trace::{
    double_attr, error_status, int_attr, metadata_attr, otel_span, span_id, string_array_attr,
    string_attr, usage_attrs,
};

/// Most messages a rendered request holds, after placeholders are expanded (D-V-37).
const MAX_MESSAGES: usize = 200;

/// Most bytes of `text` parts a rendered request holds: the request's own parts, once rendered,
/// and the placeholders' parts (D-V-37).
pub const RENDER_MAX_BYTES: usize = 4 * 1024 * 1024;

/// One request entry: a canonical message or a placeholder.
#[derive(Clone, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(untagged)]
pub enum PlaygroundEntry {
    /// Replaced, in place, by the messages given under `name` in `placeholders`.
    Placeholder {
        /// Always `placeholder`.
        r#type: PlaceholderTag,
        /// The placeholder's name.
        name: String,
    },
    /// A canonical message (spec 01).
    Message(Message),
}

impl fmt::Debug for PlaygroundEntry {
    /// Kinds and sizes only: never the message content or the placeholder's name.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Placeholder { .. } => formatter
                .debug_struct("Placeholder")
                .finish_non_exhaustive(),
            Self::Message(message) => formatter
                .debug_struct("Message")
                .field("role_len", &message.role.as_str().len())
                .field("parts", &message.parts.len())
                .finish_non_exhaustive(),
        }
    }
}

/// The `type` of a placeholder entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PlaceholderTag {
    /// The only value.
    Placeholder,
}

/// Why the messages could not be rendered. Holds names and indexes only.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RenderError {
    /// A text part of the request's own message is not a valid template or names a missing variable.
    #[error("message {index}: {source}")]
    Template {
        /// Index of the entry in `entries`.
        index: usize,
        /// Index of the part inside that message.
        part: usize,
        /// The template error.
        source: plain_template::PlainTemplateError,
    },
    /// A placeholder entry names a placeholder that was not given.
    #[error("missing placeholder {name}")]
    MissingPlaceholder {
        /// The placeholder's name.
        name: String,
    },
    /// More than 200 messages after expansion.
    #[error("too many messages")]
    TooMany,
    /// The rendered `text` parts are more than [`RENDER_MAX_BYTES`] in all.
    #[error("the rendered messages are larger than 4 MiB")]
    TooLarge,
}

/// Expands placeholders in place and renders the `text` parts of the request's own messages with
/// `variables` (placeholder messages are copied unrendered). At most 200 messages after expansion,
/// and at most [`RENDER_MAX_BYTES`] of `text` parts in all, checked before each part is appended.
///
/// # Errors
///
/// Returns [`RenderError::MissingPlaceholder`] for a placeholder with no entry in `placeholders`,
/// [`RenderError::Template`] for a text part that does not render, [`RenderError::TooMany`] when
/// the expanded list would exceed 200 messages, and [`RenderError::TooLarge`] when the text parts
/// would exceed [`RENDER_MAX_BYTES`].
pub fn render_messages(
    entries: &[PlaygroundEntry],
    variables: &BTreeMap<String, String>,
    placeholders: &BTreeMap<String, Vec<Message>>,
) -> Result<Vec<Message>, RenderError> {
    let mut messages = Vec::new();
    let mut budget = RENDER_MAX_BYTES;
    for (index, entry) in entries.iter().enumerate() {
        match entry {
            PlaygroundEntry::Placeholder { name, .. } => {
                let Some(found) = placeholders.get(name) else {
                    return Err(RenderError::MissingPlaceholder { name: name.clone() });
                };
                ensure_room(messages.len(), found.len())?;
                for message in found {
                    spend(&mut budget, text_bytes(message))?;
                    messages.push(message.clone());
                }
            }
            PlaygroundEntry::Message(message) => {
                ensure_room(messages.len(), 1)?;
                messages.push(render_message(index, message, variables, &mut budget)?);
            }
        }
    }
    Ok(messages)
}

fn ensure_room(current: usize, added: usize) -> Result<(), RenderError> {
    if current.saturating_add(added) > MAX_MESSAGES {
        return Err(RenderError::TooMany);
    }
    Ok(())
}

/// Takes `bytes` from the render budget, or fails with [`RenderError::TooLarge`].
fn spend(budget: &mut usize, bytes: usize) -> Result<(), RenderError> {
    *budget = budget.checked_sub(bytes).ok_or(RenderError::TooLarge)?;
    Ok(())
}

/// The bytes of a message's `text` parts.
fn text_bytes(message: &Message) -> usize {
    message.parts.iter().fold(0usize, |total, part| match part {
        Part::Text { content, .. } => total.saturating_add(content.len()),
        _ => total,
    })
}

fn render_message(
    index: usize,
    message: &Message,
    variables: &BTreeMap<String, String>,
    budget: &mut usize,
) -> Result<Message, RenderError> {
    let mut rendered = message.clone();
    for (part_index, part) in rendered.parts.iter_mut().enumerate() {
        if let Part::Text { content, .. } = part {
            let text = plain_template::render_up_to(content.as_str(), variables, *budget)
                .map_err(|source| RenderError::Template {
                    index,
                    part: part_index,
                    source,
                })?
                .ok_or(RenderError::TooLarge)?;
            spend(budget, text.len())?;
            *content = text;
        }
    }
    Ok(rendered)
}

/// One finished (or failed, or cancelled) run.
pub struct PlaygroundTrace<'a> {
    /// Random trace id, chosen by the caller.
    pub trace_id: TraceId,
    /// Run start, nanoseconds since the Unix epoch.
    pub started_ns: i64,
    /// Run end, nanoseconds since the Unix epoch.
    pub ended_ns: i64,
    /// Which provider built the request.
    pub adapter: Adapter,
    /// The request as sent: rendered messages, tools, params, model.
    pub request: &'a ChatRequest,
    /// Model the provider says it ran.
    pub response_model: Option<&'a str>,
    /// The assistant message assembled from the stream. `None` when the text and the tool
    /// calls are both empty.
    pub output: Option<&'a Message>,
    /// Token counts, when the provider sent them.
    pub usage: Option<&'a TokenUsage>,
    /// Why generation stopped, when the provider said.
    pub finish_reason: Option<&'a str>,
    /// Time to first token, nanoseconds.
    pub ttft_ns: Option<i64>,
    /// A payload-free error text. `cancelled` for a cancelled run.
    pub error: Option<&'a str>,
    /// The user who ran the run.
    pub run_by: Uuid,
    /// `prompt:<name>@<version>` or `span:<trace hex>:<span hex>`.
    pub source: Option<String>,
    /// The source prompt's name and version.
    pub prompt: Option<(&'a str, Option<i64>)>,
}

impl fmt::Debug for PlaygroundTrace<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PlaygroundTrace")
            .field("trace_id", &self.trace_id.to_hex())
            .field("messages", &self.request.messages.len())
            .field("has_output", &self.output.is_some())
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}

/// Builds the run as OTLP and normalizes it: one `llm` span, environment `cetra-playground`,
/// resource `service.name = cetra-playground`.
///
/// A rejection is a bug in this builder. The rejection counts are logged, and the spans that
/// were accepted are still returned.
pub fn build_playground_trace(
    input: &PlaygroundTrace<'_>,
    ingest: &IngestContext,
    prices: &PriceTable,
) -> Normalized {
    let request = export_request(input);
    let normalized = normalize(&request, ingest, prices);
    if normalized.rejections.total() > 0 {
        tracing::warn!(
            trace_id = %input.trace_id.to_hex(),
            invalid_trace_id = normalized.rejections.count(RejectReason::InvalidTraceId),
            invalid_span_id = normalized.rejections.count(RejectReason::InvalidSpanId),
            timestamp_overflow = normalized.rejections.count(RejectReason::TimestampOverflow),
            start_time_out_of_range = normalized
                .rejections
                .count(RejectReason::StartTimeOutOfRange),
            "playground trace dropped spans"
        );
    }
    normalized
}

fn export_request(input: &PlaygroundTrace<'_>) -> ExportTraceServiceRequest {
    let root_id = span_id(input.trace_id, 0);
    ExportTraceServiceRequest {
        resource_spans: vec![ResourceSpans {
            resource: Some(Resource {
                attributes: vec![string_attr("service.name", environment::PLAYGROUND)],
                ..Resource::default()
            }),
            scope_spans: vec![ScopeSpans {
                scope: Some(InstrumentationScope {
                    name: env!("CARGO_PKG_NAME").to_owned(),
                    version: env!("CARGO_PKG_VERSION").to_owned(),
                    ..InstrumentationScope::default()
                }),
                spans: vec![root_span(input, &root_id)],
                ..ScopeSpans::default()
            }],
            ..ResourceSpans::default()
        }],
    }
}

fn root_span(input: &PlaygroundTrace<'_>, root: &SpanId) -> Span {
    let request = input.request;
    let mut attributes = vec![
        string_attr(attr::SPAN_KIND, SpanKind::Llm.as_str()),
        string_attr(attr::ENVIRONMENT, environment::PLAYGROUND),
        string_attr("gen_ai.operation.name", "chat"),
        string_attr("gen_ai.provider.name", input.adapter.provider_name()),
        string_attr("gen_ai.request.model", &request.model),
        string_attr(attr::INPUT, &messages_to_json(&request.messages)),
        string_attr(attr::INPUT_FORMAT, "messages"),
        metadata_attr(metadata_key::PLAYGROUND_RUN_BY, &input.run_by.to_string()),
    ];
    if let Some(model) = input.response_model {
        attributes.push(string_attr("gen_ai.response.model", model));
    }
    if let Some(source) = &input.source {
        attributes.push(metadata_attr(metadata_key::PLAYGROUND_SOURCE, source));
    }
    if let Some(output) = input.output {
        // The finish reason lives on the output message, not in a span attribute (Precision 9).
        let mut message = output.clone();
        if let Some(reason) = input.finish_reason {
            message.finish_reason = Some(reason.to_owned());
        }
        attributes.push(string_attr(
            attr::OUTPUT,
            &messages_to_json(std::slice::from_ref(&message)),
        ));
        attributes.push(string_attr(attr::OUTPUT_FORMAT, "messages"));
    }
    if !request.tools.is_empty() {
        attributes.push(string_attr(
            "gen_ai.tool.definitions",
            &tool_definitions_json(&request.tools),
        ));
    }
    attributes.extend(request_param_attrs(request));
    if let Some(usage) = input.usage {
        attributes.extend(usage_attrs(usage));
    }
    if let Some(ttft) = input.ttft_ns {
        attributes.push(int_attr(attr::TTFT_NS, ttft));
    }
    if let Some((name, version)) = input.prompt {
        attributes.push(string_attr(attr::PROMPT_NAME, name));
        if let Some(version) = version {
            attributes.push(int_attr(attr::PROMPT_VERSION, version));
        }
    }
    let status = input.error.map(error_status);
    otel_span(
        input.trace_id,
        root,
        None,
        &format!("playground {}", request.model),
        (input.started_ns, input.ended_ns),
        attributes,
        status,
    )
}

/// Tool definitions as a JSON array of `{name, description, parameters}`, the form
/// `gen_ai.tool.definitions` takes (spec 02, `crates/otel/src/conventions/genai.rs`).
fn tool_definitions_json(tools: &[ToolDefinition]) -> String {
    let items: Vec<Value> = tools
        .iter()
        .map(|tool| {
            let mut object = Map::new();
            object.insert("name".to_owned(), Value::String(tool.name.clone()));
            if let Some(description) = &tool.description {
                object.insert("description".to_owned(), Value::String(description.clone()));
            }
            object.insert("parameters".to_owned(), tool.parameters.clone());
            Value::Object(object)
        })
        .collect();
    serde_json::to_string(&items).unwrap_or_else(|_| "[]".to_owned())
}

/// The request's sampling parameters under the `gen_ai.request.*` keys that normalization
/// collects into `request_params`. Only the parameters that are set are written.
fn request_param_attrs(request: &ChatRequest) -> Vec<KeyValue> {
    let params = &request.params;
    let mut attributes = Vec::new();
    if let Some(value) = params.temperature {
        attributes.push(double_attr("gen_ai.request.temperature", value));
    }
    if let Some(value) = params.top_p {
        attributes.push(double_attr("gen_ai.request.top_p", value));
    }
    attributes.push(int_attr(
        "gen_ai.request.max_tokens",
        i64::from(request.max_tokens),
    ));
    if !params.stop.is_empty() {
        attributes.push(string_array_attr("gen_ai.request.stop", &params.stop));
    }
    if let Some(value) = params.presence_penalty {
        attributes.push(double_attr("gen_ai.request.presence_penalty", value));
    }
    if let Some(value) = params.frequency_penalty {
        attributes.push(double_attr("gen_ai.request.frequency_penalty", value));
    }
    if let Some(value) = params.seed {
        attributes.push(int_attr("gen_ai.request.seed", value));
    }
    if let Some(value) = &params.reasoning_effort {
        attributes.push(string_attr("gen_ai.request.reasoning_effort", value));
    }
    attributes
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::OnceLock;

    use cetra_core::enums::{SpanKind, SpanStatus};
    use cetra_core::ids::{ProjectId, TraceId};
    use cetra_core::message::{Message, Part, Role};
    use cetra_core::names::{environment, metadata_key};
    use cetra_otel::{IngestContext, IngestLimits, PriceTable, builtin_price_rows};
    use serde_json::Map;
    use uuid::Uuid;

    use super::{
        PlaceholderTag, PlaygroundEntry, PlaygroundTrace, RenderError, build_playground_trace,
        render_messages,
    };
    use crate::chat::{ChatParams, ChatRequest, TokenUsage};
    use crate::judge::Adapter;
    use crate::plain_template::PlainTemplateError;

    const INGEST: i64 = 1_767_225_600_000_000_000;
    const STARTED: i64 = INGEST - 1_500_000_000;
    const ENDED: i64 = INGEST;
    const TRACE: &str = "4bf92f3577b34da6a3ce929d0e0e4736";
    const PROJECT: &str = "01900000-0000-7000-8000-000000000001";
    const USER: &str = "01900000-0000-7000-8000-000000000040";

    fn prices() -> &'static PriceTable {
        static PRICES: OnceLock<PriceTable> = OnceLock::new();
        PRICES.get_or_init(|| PriceTable::new(builtin_price_rows()).expect("built-in prices"))
    }

    fn ingest() -> IngestContext {
        IngestContext {
            project_id: PROJECT.parse::<ProjectId>().expect("project id"),
            ingest_time: INGEST,
            retention_days: Some(7),
            limits: IngestLimits::default(),
        }
    }

    fn trace_id() -> TraceId {
        TRACE.parse().expect("trace id")
    }

    fn text(role: Role, content: &str) -> Message {
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

    fn vars(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect()
    }

    fn placeholder(name: &str, messages: Vec<Message>) -> BTreeMap<String, Vec<Message>> {
        BTreeMap::from([(name.to_owned(), messages)])
    }

    fn placeholder_entry(name: &str) -> PlaygroundEntry {
        PlaygroundEntry::Placeholder {
            r#type: PlaceholderTag::Placeholder,
            name: name.to_owned(),
        }
    }

    fn first_text(message: &Message) -> &str {
        match message.parts.first() {
            Some(Part::Text { content, .. }) => content.as_str(),
            _ => "",
        }
    }

    fn request(messages: Vec<Message>) -> ChatRequest {
        ChatRequest {
            model: "gpt-4o".to_owned(),
            messages,
            tools: Vec::new(),
            tool_choice: None,
            response_format: None,
            params: ChatParams {
                temperature: Some(0.2),
                ..ChatParams::default()
            },
            max_tokens: 4_096,
            timeout: std::time::Duration::from_secs(60),
            idle_timeout: std::time::Duration::from_secs(60),
        }
    }

    #[test]
    fn renders_variables_and_placeholders() {
        let entries = vec![
            PlaygroundEntry::Message(text(Role::System, "You sell {{product}}.")),
            placeholder_entry("history"),
            PlaygroundEntry::Message(text(Role::User, "Tell me about {{product}}")),
        ];
        let history = vec![
            text(Role::User, "{{not_rendered}}"),
            text(Role::Assistant, "ok {{not_rendered}}"),
        ];
        let out = render_messages(
            &entries,
            &vars(&[("product", "tea")]),
            &placeholder("history", history),
        )
        .expect("renders");
        assert_eq!(out.len(), 4);
        assert_eq!(first_text(&out[0]), "You sell tea.");
        assert_eq!(first_text(&out[1]), "{{not_rendered}}");
        assert_eq!(first_text(&out[2]), "ok {{not_rendered}}");
        assert_eq!(first_text(&out[3]), "Tell me about tea");
    }

    #[test]
    fn missing_variable_names_its_message() {
        let mut message = text(Role::User, "ok");
        message.parts.push(Part::Text {
            content: "{{absent}}".to_owned(),
            extra: Map::new(),
        });
        let entries = vec![
            PlaygroundEntry::Message(text(Role::System, "fine")),
            PlaygroundEntry::Message(message),
        ];
        let error = render_messages(&entries, &vars(&[]), &BTreeMap::new())
            .expect_err("a variable is missing");
        assert!(matches!(
            &error,
            RenderError::Template {
                index: 1,
                part: 1,
                source: PlainTemplateError::MissingVariable { name },
            } if name.as_str() == "absent"
        ));
    }

    #[test]
    fn missing_placeholder() {
        let entries = vec![placeholder_entry("history")];
        let error = render_messages(&entries, &vars(&[]), &BTreeMap::new())
            .expect_err("the placeholder is missing");
        assert_eq!(
            error,
            RenderError::MissingPlaceholder {
                name: "history".to_owned()
            }
        );
    }

    #[test]
    fn too_many_messages() {
        let many = vec![text(Role::User, "x"); 200];
        let entries = vec![
            placeholder_entry("many"),
            PlaygroundEntry::Message(text(Role::User, "last")),
        ];
        assert_eq!(
            render_messages(&entries, &vars(&[]), &placeholder("many", many.clone())),
            Err(RenderError::TooMany)
        );
        assert_eq!(
            render_messages(&entries[..1], &vars(&[]), &placeholder("many", many))
                .map(|messages| messages.len()),
            Ok(200)
        );
    }

    #[test]
    fn rendered_text_is_capped_at_4_mib() {
        let big = "x".repeat(1 << 20);
        let variables = vars(&[("big", big.as_str())]);
        let uses = |count: usize| {
            vec![PlaygroundEntry::Message(text(
                Role::User,
                &"{{big}}".repeat(count),
            ))]
        };
        // Four megabytes of value, in one part: exactly the budget.
        assert!(render_messages(&uses(4), &variables, &BTreeMap::new()).is_ok());
        assert_eq!(
            render_messages(&uses(5), &variables, &BTreeMap::new()),
            Err(RenderError::TooLarge)
        );
        // Placeholder messages count too: three of 2 MiB are over the budget.
        let entries = vec![placeholder_entry("history")];
        let history = vec![text(Role::User, &"y".repeat(2 << 20)); 3];
        assert_eq!(
            render_messages(&entries, &vars(&[]), &placeholder("history", history)),
            Err(RenderError::TooLarge)
        );
    }

    #[test]
    fn trace_has_one_llm_span() {
        let messages = vec![text(Role::System, "Be kind."), text(Role::User, "Hello")];
        let mut request = request(messages);
        request.tools.push(crate::chat::ToolDefinition {
            name: "lookup".to_owned(),
            description: Some("Looks a thing up.".to_owned()),
            parameters: serde_json::json!({"type": "object"}),
        });
        let output = text(Role::Assistant, "Hi there");
        let usage = TokenUsage {
            input: 10,
            output: 3,
            ..TokenUsage::default()
        };
        let run_by = Uuid::parse_str(USER).expect("user id");
        let trace = PlaygroundTrace {
            trace_id: trace_id(),
            started_ns: STARTED,
            ended_ns: ENDED,
            adapter: Adapter::OpenAi,
            request: &request,
            response_model: Some("gpt-4o"),
            output: Some(&output),
            usage: Some(&usage),
            finish_reason: Some("stop"),
            ttft_ns: Some(5_000_000),
            error: None,
            run_by,
            source: Some("prompt:support-reply@2".to_owned()),
            prompt: Some(("support-reply", Some(2))),
        };
        let normalized = build_playground_trace(&trace, &ingest(), prices());
        assert_eq!(normalized.rejections.total(), 0);
        assert_eq!(normalized.spans.len(), 1);
        let span = &normalized.spans[0];
        assert_eq!(span.span_kind, SpanKind::Llm);
        assert_eq!(span.environment.as_deref(), Some(environment::PLAYGROUND));
        assert_eq!(span.prompt_name.as_deref(), Some("support-reply"));
        assert_eq!(span.prompt_version, Some(2));
        assert_eq!(span.ttft_ns, Some(5_000_000));
        assert!(span.tool_definitions.is_some());
        assert!(span.request_params.is_some());
        assert_eq!(span.input_tokens, Some(10));
        assert_eq!(span.output_tokens, Some(3));
        assert!(span.cost_total.is_some(), "gpt-4o is in the built-in table");
        assert_eq!(
            span.metadata
                .get(metadata_key::PLAYGROUND_RUN_BY)
                .map(String::as_str),
            Some(USER)
        );
        assert_eq!(
            span.metadata
                .get(metadata_key::PLAYGROUND_SOURCE)
                .map(String::as_str),
            Some("prompt:support-reply@2")
        );
    }

    #[test]
    fn cancelled_run_is_an_error_span() {
        let request = request(vec![text(Role::User, "Hello")]);
        let run_by = Uuid::parse_str(USER).expect("user id");
        let trace = PlaygroundTrace {
            trace_id: trace_id(),
            started_ns: STARTED,
            ended_ns: ENDED,
            adapter: Adapter::Anthropic,
            request: &request,
            response_model: None,
            output: None,
            usage: None,
            finish_reason: None,
            ttft_ns: None,
            error: Some("cancelled"),
            run_by,
            source: None,
            prompt: None,
        };
        let normalized = build_playground_trace(&trace, &ingest(), prices());
        assert_eq!(normalized.spans.len(), 1);
        let span = &normalized.spans[0];
        assert_eq!(span.status, SpanStatus::Error);
        assert_eq!(span.status_message.as_deref(), Some("cancelled"));
        assert!(span.output.is_none());
    }
}
