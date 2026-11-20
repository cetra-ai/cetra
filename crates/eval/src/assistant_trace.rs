//! The in-app assistant's turn trace (D-AS-11, D-AS-12): an `agent` root, one `llm` span per model
//! call and one `tool` span per tool call, in the reserved environment `cetra-assistant`. It holds
//! no message text, tool arguments, tool results or drafts: names, times, sizes, usage, models and
//! error codes only. Built as OTLP and normalized like judge traces (D161).

use std::fmt;

use cetra_core::enums::SpanKind;
use cetra_core::ids::TraceId;
use cetra_core::names::{attr, environment, metadata_key};
use cetra_otel::{IngestContext, Normalized, PriceTable, RejectReason, normalize};
use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use opentelemetry_proto::tonic::common::v1::InstrumentationScope;
use opentelemetry_proto::tonic::resource::v1::Resource;
use opentelemetry_proto::tonic::trace::v1::{ResourceSpans, ScopeSpans, Span};
use uuid::Uuid;

use crate::trace::{
    error_status, int_attr, metadata_attr, otel_span, span_id, string_array_attr, string_attr,
    usage_attrs,
};
use crate::{Adapter, TokenUsage};

/// The largest span index. Span ids take the index as one byte (`span_id`).
const LAST_INDEX: usize = u8::MAX as usize;

/// One model call of a turn.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelCallRecord {
    pub started_ns: i64,
    pub ended_ns: i64,
    pub response_model: Option<String>,
    pub usage: Option<TokenUsage>,
    /// `FinishReason::as_str()` of the call, when it finished.
    pub finish_reason: Option<String>,
    pub ttft_ns: Option<i64>,
    /// A payload-free error text (`ChatError`'s `Display`, `cancelled`, `the LLM connection is busy`).
    pub error: Option<String>,
}

/// One tool call of a turn.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolCallRecord {
    pub name: String,
    pub started_ns: i64,
    pub ended_ns: i64,
    /// Length of the compact JSON result the model received.
    pub size_bytes: u64,
    pub rows: Option<u64>,
    /// The problem code of a failed tool (`not_found`, `invalid_filter`, …), else `None`.
    pub error_code: Option<String>,
    /// The index into `AssistantTurnTrace::calls` of the model call that asked for this tool.
    pub call: usize,
}

/// One turn.
#[derive(Clone)]
pub struct AssistantTurnTrace<'a> {
    pub trace_id: TraceId,
    pub started_ns: i64,
    pub ended_ns: i64,
    pub adapter: Adapter,
    pub model: &'a str,
    pub calls: &'a [ModelCallRecord],
    /// In run order; `ToolCallRecord::call` is the index into `calls` of the model call that asked for it.
    pub tools: &'a [ToolCallRecord],
    /// A payload-free reason when the turn failed or was cancelled.
    pub error: Option<&'a str>,
    pub user: Uuid,
    pub conversation: Uuid,
    pub prompt_version: &'a str,
}

/// Ids, counts and times only: no model, tool names or error texts.
impl fmt::Debug for AssistantTurnTrace<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AssistantTurnTrace")
            .field("trace_id", &self.trace_id.to_hex())
            .field("started_ns", &self.started_ns)
            .field("ended_ns", &self.ended_ns)
            .field("calls", &self.calls.len())
            .field("tools", &self.tools.len())
            .field("has_error", &self.error.is_some())
            .finish_non_exhaustive()
    }
}

/// Builds the turn as OTLP and normalizes it.
///
/// A rejection is a bug in this builder. The rejection counts are logged, and the spans that
/// were accepted are still returned.
pub fn build_assistant_trace(
    input: &AssistantTurnTrace<'_>,
    ingest: &IngestContext,
    prices: &PriceTable,
) -> Normalized {
    let request = export_request(input);
    let normalized = normalize(
        &request,
        ingest,
        prices,
        &cetra_otel::AgentContexts::default(),
    );
    if normalized.rejections.total() > 0 {
        tracing::warn!(
            trace_id = %input.trace_id.to_hex(),
            invalid_trace_id = normalized.rejections.count(RejectReason::InvalidTraceId),
            invalid_span_id = normalized.rejections.count(RejectReason::InvalidSpanId),
            timestamp_overflow = normalized.rejections.count(RejectReason::TimestampOverflow),
            start_time_out_of_range = normalized
                .rejections
                .count(RejectReason::StartTimeOutOfRange),
            "assistant trace dropped spans"
        );
    }
    normalized
}

fn export_request(input: &AssistantTurnTrace<'_>) -> ExportTraceServiceRequest {
    // Model calls past index 255 are dropped; a tool needs its call's span, so a tool whose call
    // was dropped is dropped too. Neither can happen within the turn limits (D-AS-15).
    let kept_calls = input.calls.len().min(LAST_INDEX);
    let mut dropped_calls = input.calls.len() - kept_calls;
    let mut dropped_tools = 0usize;
    let mut spans = vec![root_span(input)];
    for (position, call) in input.calls.iter().take(kept_calls).enumerate() {
        let Ok(index) = u8::try_from(1 + position) else {
            dropped_calls += 1;
            continue;
        };
        spans.push(llm_span(input, index, call));
    }
    for (position, tool) in input.tools.iter().enumerate() {
        let index = u8::try_from(1 + kept_calls + position).ok();
        let parent = u8::try_from(1 + tool.call).ok();
        match (index, parent) {
            (Some(index), Some(parent)) if tool.call < kept_calls => {
                spans.push(tool_span(input, index, parent, tool));
            }
            _ => dropped_tools += 1,
        }
    }
    if dropped_calls + dropped_tools > 0 {
        tracing::warn!(
            dropped_model_calls = dropped_calls,
            dropped_tool_calls = dropped_tools,
            "assistant trace dropped spans past the index limit"
        );
    }
    ExportTraceServiceRequest {
        resource_spans: vec![ResourceSpans {
            resource: Some(Resource {
                attributes: vec![string_attr("service.name", environment::ASSISTANT)],
                ..Resource::default()
            }),
            scope_spans: vec![ScopeSpans {
                scope: Some(InstrumentationScope {
                    name: env!("CARGO_PKG_NAME").to_owned(),
                    version: env!("CARGO_PKG_VERSION").to_owned(),
                    ..InstrumentationScope::default()
                }),
                spans,
                ..ScopeSpans::default()
            }],
            ..ResourceSpans::default()
        }],
    }
}

fn root_span(input: &AssistantTurnTrace<'_>) -> Span {
    let attributes = vec![
        string_attr(attr::SPAN_KIND, SpanKind::Agent.as_str()),
        string_attr(attr::ENVIRONMENT, environment::ASSISTANT),
        metadata_attr(metadata_key::ASSISTANT_USER, &input.user.to_string()),
        metadata_attr(
            metadata_key::ASSISTANT_CONVERSATION_ID,
            &input.conversation.to_string(),
        ),
        metadata_attr(metadata_key::ASSISTANT_PROMPT_VERSION, input.prompt_version),
    ];
    otel_span(
        input.trace_id,
        &span_id(input.trace_id, 0),
        None,
        "assistant turn",
        (input.started_ns, input.ended_ns),
        attributes,
        input.error.map(error_status),
    )
}

fn llm_span(input: &AssistantTurnTrace<'_>, index: u8, call: &ModelCallRecord) -> Span {
    let mut attributes = vec![
        string_attr(attr::SPAN_KIND, SpanKind::Llm.as_str()),
        string_attr(attr::ENVIRONMENT, environment::ASSISTANT),
        string_attr("gen_ai.operation.name", "chat"),
        string_attr("gen_ai.provider.name", input.adapter.provider_name()),
        string_attr("gen_ai.request.model", input.model),
    ];
    if let Some(model) = &call.response_model {
        attributes.push(string_attr("gen_ai.response.model", model));
    }
    if let Some(usage) = &call.usage {
        attributes.extend(usage_attrs(usage));
    }
    if let Some(reason) = &call.finish_reason {
        attributes.push(string_array_attr(
            "gen_ai.response.finish_reasons",
            std::slice::from_ref(reason),
        ));
    }
    if let Some(ttft) = call.ttft_ns {
        attributes.push(int_attr(attr::TTFT_NS, ttft));
    }
    otel_span(
        input.trace_id,
        &span_id(input.trace_id, index),
        Some(&span_id(input.trace_id, 0)),
        &format!("chat {}", input.model),
        (call.started_ns, call.ended_ns),
        attributes,
        call.error.as_deref().map(error_status),
    )
}

fn tool_span(input: &AssistantTurnTrace<'_>, index: u8, parent: u8, tool: &ToolCallRecord) -> Span {
    let mut attributes = vec![
        string_attr(attr::SPAN_KIND, SpanKind::Tool.as_str()),
        string_attr(attr::ENVIRONMENT, environment::ASSISTANT),
        string_attr("gen_ai.tool.name", &tool.name),
        metadata_attr("cetra.assistant.size_bytes", &tool.size_bytes.to_string()),
    ];
    if let Some(rows) = tool.rows {
        attributes.push(metadata_attr("cetra.assistant.rows", &rows.to_string()));
    }
    otel_span(
        input.trace_id,
        &span_id(input.trace_id, index),
        Some(&span_id(input.trace_id, parent)),
        &format!("tool {}", tool.name),
        (tool.started_ns, tool.ended_ns),
        attributes,
        tool.error_code.as_deref().map(error_status),
    )
}

#[cfg(test)]
mod tests {
    use std::sync::OnceLock;

    use cetra_core::enums::{SpanKind, SpanStatus};
    use cetra_core::ids::{ProjectId, TraceId};
    use cetra_core::names::{environment, metadata_key};
    use cetra_otel::{IngestContext, IngestLimits, PriceTable, builtin_price_rows};
    use uuid::Uuid;

    use super::{AssistantTurnTrace, ModelCallRecord, ToolCallRecord, build_assistant_trace};
    use crate::{Adapter, TokenUsage};

    const INGEST: i64 = 1_767_225_600_000_000_000;
    const STARTED: i64 = INGEST - 1_500_000_000;
    const ENDED: i64 = INGEST;
    const TRACE: &str = "4bf92f3577b34da6a3ce929d0e0e4736";
    const PROJECT: &str = "01900000-0000-7000-8000-000000000001";
    const USER: &str = "01900000-0000-7000-8000-000000000040";
    const CONVERSATION: &str = "01900000-0000-7000-8000-000000000050";
    const PROMPT_VERSION: &str = "1";
    const TOOL: &str = "list_traces";

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

    fn model_call(
        usage: Option<TokenUsage>,
        finish_reason: Option<&str>,
        ttft_ns: Option<i64>,
        error: Option<&str>,
    ) -> ModelCallRecord {
        ModelCallRecord {
            started_ns: STARTED,
            ended_ns: ENDED,
            response_model: Some("gpt-4o".to_owned()),
            usage,
            finish_reason: finish_reason.map(str::to_owned),
            ttft_ns,
            error: error.map(str::to_owned),
        }
    }

    fn tool(call: usize) -> ToolCallRecord {
        ToolCallRecord {
            name: TOOL.to_owned(),
            started_ns: STARTED,
            ended_ns: ENDED,
            size_bytes: 512,
            rows: Some(3),
            error_code: None,
            call,
        }
    }

    fn turn<'a>(
        calls: &'a [ModelCallRecord],
        tools: &'a [ToolCallRecord],
        error: Option<&'a str>,
    ) -> AssistantTurnTrace<'a> {
        AssistantTurnTrace {
            trace_id: trace_id(),
            started_ns: STARTED,
            ended_ns: ENDED,
            adapter: Adapter::OpenAi,
            model: "gpt-4o",
            calls,
            tools,
            error,
            user: Uuid::parse_str(USER).expect("user id"),
            conversation: Uuid::parse_str(CONVERSATION).expect("conversation id"),
            prompt_version: PROMPT_VERSION,
        }
    }

    fn two_calls() -> Vec<ModelCallRecord> {
        vec![
            model_call(
                Some(TokenUsage {
                    input: 10,
                    output: 3,
                    ..TokenUsage::default()
                }),
                Some("tool_call"),
                Some(5_000_000),
                None,
            ),
            model_call(
                Some(TokenUsage {
                    input: 20,
                    output: 4,
                    ..TokenUsage::default()
                }),
                Some("stop"),
                None,
                None,
            ),
        ]
    }

    #[test]
    fn builds_root_llm_and_tool_spans() {
        let calls = two_calls();
        let tools = vec![tool(0)];
        let turn = turn(&calls, &tools, None);
        let normalized = build_assistant_trace(&turn, &ingest(), prices());
        assert_eq!(normalized.rejections.total(), 0);
        assert_eq!(normalized.spans.len(), 4);

        let root = &normalized.spans[0];
        assert_eq!(root.span_kind, SpanKind::Agent);
        assert_eq!(root.environment.as_deref(), Some(environment::ASSISTANT));
        assert!(root.parent_span_id.is_none());
        assert_eq!(
            root.metadata
                .get(metadata_key::ASSISTANT_USER)
                .map(String::as_str),
            Some(USER)
        );
        assert_eq!(
            root.metadata
                .get(metadata_key::ASSISTANT_CONVERSATION_ID)
                .map(String::as_str),
            Some(CONVERSATION)
        );
        assert_eq!(
            root.metadata
                .get(metadata_key::ASSISTANT_PROMPT_VERSION)
                .map(String::as_str),
            Some(PROMPT_VERSION)
        );

        for llm in &normalized.spans[1..3] {
            assert_eq!(llm.span_kind, SpanKind::Llm);
            assert_eq!(llm.environment.as_deref(), Some(environment::ASSISTANT));
            assert_eq!(llm.parent_span_id, Some(root.span_id));
            assert!(llm.cost_total.is_some(), "gpt-4o is in the built-in table");
        }
        assert_eq!(normalized.spans[1].input_tokens, Some(10));
        assert_eq!(normalized.spans[2].output_tokens, Some(4));

        let tool_span = &normalized.spans[3];
        assert_eq!(tool_span.span_kind, SpanKind::Tool);
        assert_eq!(
            tool_span.environment.as_deref(),
            Some(environment::ASSISTANT)
        );
        assert_eq!(
            tool_span.parent_span_id,
            Some(normalized.spans[1].span_id),
            "the tool was asked by the first model call"
        );
        assert_eq!(
            tool_span
                .attributes
                .get("gen_ai.tool.name")
                .map(String::as_str),
            Some(TOOL)
        );
        assert_eq!(
            tool_span
                .metadata
                .get("cetra.assistant.size_bytes")
                .map(String::as_str),
            Some("512")
        );
        assert_eq!(
            tool_span
                .metadata
                .get("cetra.assistant.rows")
                .map(String::as_str),
            Some("3")
        );
    }

    #[test]
    fn holds_no_payload() {
        let calls = two_calls();
        let tools = vec![tool(0)];
        let turn = turn(&calls, &tools, None);
        let normalized = build_assistant_trace(&turn, &ingest(), prices());
        for span in &normalized.spans {
            assert!(span.input.is_none(), "{} has an input", span.name);
            assert!(span.output.is_none(), "{} has an output", span.name);
            for value in span.attributes.values().chain(span.metadata.values()) {
                assert!(value.len() <= 64, "{} holds a long value", span.name);
            }
        }
    }

    #[test]
    fn error_turn_is_an_error_root() {
        let calls = vec![model_call(None, None, None, Some("cancelled"))];
        let turn = turn(&calls, &[], Some("cancelled"));
        let normalized = build_assistant_trace(&turn, &ingest(), prices());
        assert_eq!(normalized.rejections.total(), 0);
        assert_eq!(normalized.spans.len(), 2);
        let root = &normalized.spans[0];
        assert_eq!(root.status, SpanStatus::Error);
        assert_eq!(root.status_message.as_deref(), Some("cancelled"));
        assert_eq!(normalized.spans[1].status, SpanStatus::Error);
    }

    #[test]
    fn debug_holds_no_names() {
        let calls = two_calls();
        let tools = vec![tool(0)];
        let turn = turn(&calls, &tools, Some("cancelled"));
        let text = format!("{turn:?}");
        assert!(!text.contains(TOOL), "debug output names a tool");
        assert!(!text.contains("gpt-4o"), "debug output names a model");
    }

    #[test]
    fn drops_model_calls_past_the_index_limit() {
        let calls = vec![model_call(None, None, None, None); 300];
        let turn = turn(&calls, &[], None);
        let normalized = build_assistant_trace(&turn, &ingest(), prices());
        assert_eq!(normalized.spans.len(), 256, "root plus 255 model calls");
    }
}
