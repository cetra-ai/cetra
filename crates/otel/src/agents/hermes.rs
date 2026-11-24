//! Hermes Agent spans (spec 02 § Coding agents; D-IN-29 item 7, spike
//! `m7-coding-agents.md` § Hermes Agent).
//!
//! Hermes's telemetry is the community plugin `hermes-otel`, detected by the scope
//! `hermes-otel-plugin`. `agent` is the turn, `api.*` is the model call, `llm.*` a wrapper
//! without tokens that parents the `api.*` calls and the tool, and `tool.*` is the tool.
//! The wrapper is `chain`, not dropped, so its children keep their parent. No span is
//! dropped.

use cetra_core::enums::SpanKind;
use cetra_core::span::Span;

use super::{Finish, rename_prefixed_tool};

/// The span kinds of Hermes Agent's spans. Other names use the generic mapping.
pub(crate) fn kind(span_name: &str) -> Option<SpanKind> {
    if span_name == "agent" {
        return Some(SpanKind::Agent);
    }
    if span_name.starts_with("llm.") {
        return Some(SpanKind::Chain);
    }
    None
}

/// A `tool.*` span takes the tool's name (D-CA-6).
pub(crate) fn finish(span_name: &str, inputs: &Finish, built: &mut Span) {
    rename_prefixed_tool(span_name, "tool.", inputs, built);
}

#[cfg(test)]
mod tests {
    use cetra_core::enums::SpanKind;
    use cetra_core::ids::SpanId;
    use opentelemetry_proto::tonic::common::v1::KeyValue;
    use opentelemetry_proto::tonic::trace::v1::Span as OtlpSpan;

    use crate::agents::AgentContexts;
    use crate::agents::test_support::{i, kv, request, run, s, span};

    /// A span with a distinct id and an optional parent, for the one trace every test sends.
    fn linked(mut span: OtlpSpan, id: u8, parent: Option<u8>) -> OtlpSpan {
        span.span_id = vec![id; 8];
        span.parent_span_id = parent.map(|id| vec![id; 8]).unwrap_or_default();
        span
    }

    /// The attributes every span of the trace carries.
    fn with_common(mut attributes: Vec<KeyValue>) -> Vec<KeyValue> {
        attributes.extend([
            kv("gen_ai.provider.name", s("openai")),
            kv("gen_ai.request.model", s("gpt-4o-mini")),
        ]);
        attributes
    }

    /// The trace of one Hermes turn: the agent, its `llm.` wrapper, the wrapper's `api.`
    /// model call and its tool.
    fn trace() -> Vec<OtlpSpan> {
        vec![
            linked(span("agent", with_common(vec![]), vec![]), 0x11, None),
            linked(
                span("llm.gpt-4o-mini", with_common(vec![]), vec![]),
                0x22,
                Some(0x11),
            ),
            linked(
                span(
                    "api.gpt-4o-mini",
                    with_common(vec![
                        kv("gen_ai.operation.name", s("chat")),
                        kv("gen_ai.usage.input_tokens", i(82)),
                        kv("gen_ai.usage.output_tokens", i(17)),
                    ]),
                    vec![],
                ),
                0x33,
                Some(0x22),
            ),
            linked(
                span(
                    "tool.read_file",
                    with_common(vec![kv("gen_ai.tool.name", s("read_file"))]),
                    vec![],
                ),
                0x44,
                Some(0x22),
            ),
        ]
    }

    #[test]
    fn hermes_rules() {
        let normalized = run(
            &request("hermes-agent", "hermes-otel-plugin", trace()),
            &AgentContexts::default(),
        );
        assert_eq!(normalized.discarded, 0);
        assert_eq!(normalized.spans.len(), 4);
        let by_name = |name: &str| {
            normalized
                .spans
                .iter()
                .find(|span| span.name == name)
                .expect("span")
        };
        let id = |id: u8| SpanId::from_bytes([id; 8]).expect("span id");
        let agent = by_name("agent");
        assert_eq!(agent.span_kind, SpanKind::Agent);
        assert_eq!(agent.parent_span_id, None);
        let wrapper = by_name("llm.gpt-4o-mini");
        assert_eq!(wrapper.span_kind, SpanKind::Chain);
        assert_eq!(wrapper.parent_span_id, Some(id(0x11)));
        let model = by_name("api.gpt-4o-mini");
        assert_eq!(model.span_kind, SpanKind::Llm);
        assert_eq!(model.input_tokens, Some(82));
        assert_eq!(model.output_tokens, Some(17));
        assert_eq!(model.parent_span_id, Some(id(0x22)));
        let tool = by_name("read_file");
        assert_eq!(tool.span_kind, SpanKind::Tool);
        assert_eq!(
            tool.attributes.get("cetra.span_name").map(String::as_str),
            Some("tool.read_file")
        );
        assert_eq!(tool.parent_span_id, Some(id(0x22)));
        assert_eq!(
            tool.metadata.get("coding_agent").map(String::as_str),
            Some("hermes-agent")
        );
    }

    #[test]
    fn hermes_names_on_another_scope_are_untouched() {
        let normalized = run(
            &request(
                "hermes-agent",
                "x",
                vec![linked(
                    span("llm.gpt-4o-mini", with_common(vec![]), vec![]),
                    0x22,
                    None,
                )],
            ),
            &AgentContexts::default(),
        );
        assert_eq!(normalized.spans.len(), 1);
        assert_eq!(normalized.spans[0].span_kind, SpanKind::Llm);
        assert!(!normalized.spans[0].metadata.contains_key("coding_agent"));
    }
}
