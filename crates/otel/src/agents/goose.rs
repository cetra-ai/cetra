//! Goose spans (spec 02 § Coding agents; D-IN-29 item 5, spike `m7-coding-agents.md` § Goose).
//!
//! Goose is detected by the scope `goose`. Its `reply` is the turn, but its turn-total
//! tokens are not read: usage is read on `llm` and `embedding` spans only. `chat` is a
//! wrapper whose child `stream_response_from_provider` repeats its tokens, so the child is
//! the `llm` span and the wrapper is `chain`, which keeps the tokens counted once. Its tool
//! span is renamed after its tool. No span is dropped.

use cetra_core::enums::SpanKind;
use cetra_core::span::Span;

use super::{Finish, rename_tool};

/// The span kinds of Goose's spans. Other names use the generic mapping.
pub(crate) fn kind(span_name: &str) -> Option<SpanKind> {
    Some(match span_name {
        "reply" => SpanKind::Agent,
        "chat" => SpanKind::Chain,
        "stream_response_from_provider" => SpanKind::Llm,
        _ => return None,
    })
}

/// An `execute_tool` span takes the tool's name (D-CA-6).
pub(crate) fn finish(span_name: &str, inputs: &Finish, built: &mut Span) {
    if span_name == "execute_tool"
        && let Some(name) = inputs.tool_name.as_deref()
    {
        rename_tool(built, name);
    }
}

#[cfg(test)]
mod tests {
    use cetra_core::enums::{CostSource, SpanKind};
    use opentelemetry_proto::tonic::common::v1::KeyValue;
    use opentelemetry_proto::tonic::trace::v1::Span as OtlpSpan;

    use crate::agents::AgentContexts;
    use crate::agents::test_support::{i, kv, request, run, run_priced, s, span};
    use crate::cost::{PriceRow, PriceScope};

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

    /// The trace of one Goose turn: a reply, its chat wrapper, the wrapper's model call
    /// and the turn's tool.
    fn trace() -> Vec<OtlpSpan> {
        vec![
            linked(
                span(
                    "reply",
                    with_common(vec![
                        kv("gen_ai.operation.name", s("invoke_agent")),
                        kv("gen_ai.usage.input_tokens", i(213)),
                        kv("gen_ai.usage.output_tokens", i(29)),
                    ]),
                    vec![],
                ),
                0x11,
                None,
            ),
            linked(
                span(
                    "chat",
                    with_common(vec![
                        kv("gen_ai.operation.name", s("chat")),
                        kv("gen_ai.usage.input_tokens", i(82)),
                        kv("gen_ai.usage.output_tokens", i(17)),
                    ]),
                    vec![],
                ),
                0x22,
                Some(0x11),
            ),
            linked(
                span(
                    "stream_response_from_provider",
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
                    "execute_tool",
                    with_common(vec![
                        kv("gen_ai.operation.name", s("execute_tool")),
                        kv("gen_ai.tool.name", s("shell")),
                    ]),
                    vec![],
                ),
                0x44,
                Some(0x11),
            ),
        ]
    }

    #[test]
    fn goose_rules() {
        let row = PriceRow {
            scope: PriceScope::Builtin,
            model_name: "gpt-4o-mini".to_owned(),
            match_pattern: "gpt-4o-mini".to_owned(),
            provider: None,
            valid_from: 0,
            price_input: Some(0.15),
            price_output: Some(0.6),
            price_cache_read: None,
            price_cache_write: None,
            price_reasoning: None,
            tiers: None,
        };
        let normalized = run_priced(&request("goose", "goose", trace()), vec![row]);
        assert_eq!(normalized.discarded, 0);
        assert_eq!(normalized.spans.len(), 4);
        let by_name = |name: &str| {
            normalized
                .spans
                .iter()
                .find(|span| span.name == name)
                .expect("span")
        };
        let reply = by_name("reply");
        assert_eq!(reply.span_kind, SpanKind::Agent);
        assert_eq!(reply.input_tokens, None);
        assert_eq!(reply.output_tokens, None);
        let chat = by_name("chat");
        assert_eq!(chat.span_kind, SpanKind::Chain);
        assert_eq!(chat.input_tokens, None);
        assert_eq!(chat.output_tokens, None);
        let model = by_name("stream_response_from_provider");
        assert_eq!(model.span_kind, SpanKind::Llm);
        assert_eq!(model.input_tokens, Some(82));
        assert_eq!(model.output_tokens, Some(17));
        assert_eq!(model.cost_source, Some(CostSource::Computed));
        let tool = by_name("shell");
        assert_eq!(tool.span_kind, SpanKind::Tool);
        assert_eq!(
            tool.attributes.get("cetra.span_name").map(String::as_str),
            Some("execute_tool")
        );
        assert_eq!(
            tool.metadata.get("coding_agent").map(String::as_str),
            Some("goose")
        );
        let input_sum: i64 = normalized
            .spans
            .iter()
            .map(|span| span.input_tokens.unwrap_or(0))
            .sum();
        assert_eq!(input_sum, 82);
    }

    #[test]
    fn goose_names_on_another_scope_are_untouched() {
        let normalized = run(
            &request(
                "goose",
                "x",
                vec![linked(
                    span(
                        "chat",
                        with_common(vec![kv("gen_ai.operation.name", s("chat"))]),
                        vec![],
                    ),
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
