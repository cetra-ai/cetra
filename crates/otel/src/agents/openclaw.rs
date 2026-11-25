//! OpenClaw spans (spec 02 § Coding agents; D-IN-28, spike `m7-openclaw-omnigent.md` § 4.5).
//!
//! OpenClaw's spans come from its diagnostics plugin, detected by the scope `openclaw`.
//! Its runs and harness runs get their kinds, its tool spans take the tool's name, and
//! its per-turn usage and diagnostic spans are dropped.

use cetra_core::enums::SpanKind;
use cetra_core::span::Span;

use super::{Finish, count, rename_tool};
use crate::bag::AttrBag;
use crate::warn::{Warning, Warnings};

/// The model call's time to first byte, in milliseconds (read only by [`ttft`]).
const TTFT_MILLISECONDS: &str = "openclaw.model_call.time_to_first_byte_ms";
/// The tool span, renamed after its tool (D-CA-6).
const TOOL: &str = "openclaw.tool.execution";
/// The prefix of the gateway's RPC spans, which are dropped.
const GATEWAY_RPC_PREFIX: &str = "openclaw.gateway.rpc.";

/// The span kinds of OpenClaw's run spans. Other names use the generic mapping.
pub(crate) fn kind(span_name: &str) -> Option<SpanKind> {
    Some(match span_name {
        "openclaw.run" => SpanKind::Agent,
        "openclaw.harness.run" => SpanKind::Chain,
        _ => return None,
    })
}

/// The per-turn usage span, the diagnostic phase, the gateway RPC spans and the context
/// assembly span are dropped (spike § 4.5).
pub(crate) fn discarded(span_name: &str) -> bool {
    matches!(
        span_name,
        "openclaw.model.usage" | "openclaw.diagnostic.phase" | "openclaw.context.assembled"
    ) || span_name.starts_with(GATEWAY_RPC_PREFIX)
}

/// Time to first token: the milliseconds of `openclaw.model_call.time_to_first_byte_ms`,
/// converted to nanoseconds. The key is read on any OpenClaw span. An overflow stores no
/// value and records `invalid_number`.
pub(crate) fn ttft(bag: &mut AttrBag<'_>, warnings: &mut Warnings) -> Option<i64> {
    let milliseconds = count(bag, TTFT_MILLISECONDS, warnings)?;
    match milliseconds.checked_mul(1_000_000) {
        Some(nanoseconds) => Some(nanoseconds),
        None => {
            warnings.add(Warning::InvalidNumber);
            None
        }
    }
}

/// A tool span takes the tool's name (D-CA-6).
pub(crate) fn finish(span_name: &str, inputs: &Finish, built: &mut Span) {
    if span_name == TOOL
        && let Some(name) = inputs.tool_name.as_deref()
    {
        rename_tool(built, name);
    }
}

#[cfg(test)]
mod tests {
    use cetra_core::enums::SpanKind;
    use cetra_core::names::metadata_key;
    use opentelemetry_proto::tonic::common::v1::AnyValue;
    use opentelemetry_proto::tonic::trace::v1::Span as OtlpSpan;

    use super::*;
    use crate::agents::AgentContexts;
    use crate::agents::test_support::{i, kv, request, run, s, span};

    /// A span with a distinct id, for requests that send several spans.
    fn with_id(mut span: OtlpSpan, id: u8) -> OtlpSpan {
        span.span_id = vec![id; 8];
        span
    }

    #[test]
    fn openclaw_rules() {
        let spans = vec![
            with_id(
                span(
                    "openclaw.model.call",
                    vec![
                        kv("gen_ai.operation.name", s("text_completion")),
                        kv("gen_ai.system", s("anthropic")),
                        kv(TTFT_MILLISECONDS, i(168)),
                    ],
                    vec![],
                ),
                0x30,
            ),
            with_id(span("openclaw.model.usage", vec![], vec![]), 0x31),
            with_id(span("openclaw.run", vec![], vec![]), 0x32),
            with_id(span("openclaw.harness.run", vec![], vec![]), 0x33),
            with_id(
                span(
                    "openclaw.tool.execution",
                    vec![kv("gen_ai.tool.name", s("read"))],
                    vec![],
                ),
                0x34,
            ),
            with_id(span("openclaw.diagnostic.phase", vec![], vec![]), 0x35),
            with_id(span("openclaw.gateway.rpc.dispatch", vec![], vec![]), 0x36),
            with_id(span("openclaw.context.assembled", vec![], vec![]), 0x37),
            with_id(span("openclaw.webhook.processed", vec![], vec![]), 0x38),
        ];
        let normalized = run(
            &request("openclaw", "openclaw", spans),
            &AgentContexts::default(),
        );
        assert_eq!(normalized.discarded, 4);
        let names: Vec<&str> = normalized
            .spans
            .iter()
            .map(|span| span.name.as_str())
            .collect();
        assert_eq!(
            names,
            [
                "openclaw.model.call",
                "openclaw.run",
                "openclaw.harness.run",
                "read",
                "openclaw.webhook.processed",
            ]
        );
        let by_name = |name: &str| {
            normalized
                .spans
                .iter()
                .find(|span| span.name == name)
                .expect("span")
        };
        assert_eq!(by_name("openclaw.model.call").span_kind, SpanKind::Llm);
        assert_eq!(by_name("openclaw.model.call").ttft_ns, Some(168_000_000));
        assert_eq!(by_name("openclaw.run").span_kind, SpanKind::Agent);
        assert_eq!(by_name("openclaw.harness.run").span_kind, SpanKind::Chain);
        let tool = by_name("read");
        assert_eq!(tool.span_kind, SpanKind::Tool);
        assert_eq!(
            tool.attributes.get("cetra.span_name").map(String::as_str),
            Some("openclaw.tool.execution")
        );
        assert_eq!(
            by_name("openclaw.webhook.processed")
                .metadata
                .get("coding_agent")
                .map(String::as_str),
            Some("openclaw")
        );
    }

    #[test]
    fn openclaw_ttft_edge_cases() {
        let model = |value: AnyValue| {
            span(
                "openclaw.model.call",
                vec![kv(TTFT_MILLISECONDS, value)],
                vec![],
            )
        };
        let not_an_integer = run(
            &request("openclaw", "openclaw", vec![model(s("168.5"))]),
            &AgentContexts::default(),
        );
        assert_eq!(not_an_integer.spans.len(), 1);
        assert_eq!(not_an_integer.spans[0].ttft_ns, None);
        assert!(
            !not_an_integer.spans[0]
                .attributes
                .contains_key(metadata_key::NORMALIZE_ERROR)
        );

        let overflow = run(
            &request("openclaw", "openclaw", vec![model(i(i64::MAX))]),
            &AgentContexts::default(),
        );
        assert_eq!(overflow.spans.len(), 1);
        assert_eq!(overflow.spans[0].ttft_ns, None);
    }

    #[test]
    fn openclaw_names_on_another_scope_are_untouched() {
        let spans = vec![
            with_id(span("openclaw.model.usage", vec![], vec![]), 0x31),
            with_id(span("openclaw.gateway.rpc.dispatch", vec![], vec![]), 0x32),
            with_id(
                span(
                    "openclaw.model.call",
                    vec![kv(TTFT_MILLISECONDS, i(168))],
                    vec![],
                ),
                0x33,
            ),
        ];
        let normalized = run(
            &request("app", "openclaw.x", spans),
            &AgentContexts::default(),
        );
        assert_eq!(normalized.discarded, 0);
        assert_eq!(normalized.spans.len(), 3);
        assert_eq!(normalized.spans[2].ttft_ns, None);
        assert!(!normalized.spans[2].metadata.contains_key("coding_agent"));
    }
}
