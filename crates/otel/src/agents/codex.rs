//! Codex spans (spec 02 § Coding agents; D-CA-5, D-CA-6, D110).
//!
//! Codex's internal spans are discarded before these rules run (see `kept`). The turn
//! is the LLM root of a session, and a kept tool dispatch hangs under its turn. Both
//! are re-linked to ids made from the turn id (`relink`).

use cetra_core::enums::SpanKind;
use cetra_core::ids::SpanId;
use cetra_core::span::Span;
use opentelemetry_proto::tonic::common::v1::KeyValue;
use sha2::{Digest, Sha256};

use super::{
    AgentContexts, Finish, apply_session_context, count, peek_text, rename_tool, take_text,
};
use crate::bag::AttrBag;
use crate::context::ContextFields;
use crate::usage::Usage;
use crate::value::as_str;
use crate::warn::Warnings;

const WIRE_NAME: &str = "codex";
/// The turn span, the LLM root of a Codex session.
const TURN: &str = "session_task.turn";
/// The prefix of the turn's token counts.
const TOKENS: &str = "codex.turn.token_usage.";

/// D-CA-6: a kept span is the turn, or a tool dispatch with a name. Every other Codex span is
/// internal and is discarded.
pub(crate) fn kept(span_name: &str, attributes: &[KeyValue]) -> bool {
    if span_name == TURN {
        return true;
    }
    let dispatch =
        super::attribute(attributes, "codex.tool.phase").and_then(as_str) == Some("dispatch");
    let named = super::attribute(attributes, "tool_name")
        .and_then(as_str)
        .is_some_and(|name| !name.is_empty());
    dispatch && named
}

/// The turn is the LLM; a kept dispatch is a tool.
pub(crate) fn kind(span_name: &str) -> Option<SpanKind> {
    Some(if span_name == TURN {
        SpanKind::Llm
    } else {
        SpanKind::Tool
    })
}

/// The turn id of a kept span: `turn.id`, else `turn_id`. Peeked, not taken.
pub(crate) fn turn_id(bag: &AttrBag<'_>) -> Option<String> {
    peek_text(bag, "turn.id").or_else(|| peek_text(bag, "turn_id"))
}

/// D-CA-6 session id, then the session context (D-CA-5 rank 2, D-CA-9).
///
/// The turn takes its first non-empty string `thread.id`, because the bag keeps only the
/// last duplicate. An integer `thread.id` is the OS thread and is ignored. The other spans
/// take `conversation.id`. Each falls back to the other id when its own is absent.
/// `model` becomes the request model when no other source set it.
pub(crate) fn context(
    span_name: &str,
    attributes: &[KeyValue],
    bag: &mut AttrBag<'_>,
    fields: &mut ContextFields,
    agents: &AgentContexts,
) {
    let session = if span_name == TURN {
        thread_id(bag, attributes).or_else(|| take_text(bag, "conversation.id"))
    } else {
        take_text(bag, "conversation.id").or_else(|| thread_id(bag, attributes))
    };
    if session.is_some() {
        fields.session_id = session;
    }
    if fields.request_model.is_none() {
        fields.request_model = take_text(bag, "model");
    }
    apply_session_context(agents, WIRE_NAME, fields);
}

/// The first non-empty string `thread.id` of the span, taken from the bag when found.
fn thread_id(bag: &mut AttrBag<'_>, attributes: &[KeyValue]) -> Option<String> {
    let id = attributes
        .iter()
        .filter(|attr| attr.key == "thread.id")
        .filter_map(|attr| attr.value.as_ref())
        .filter_map(as_str)
        .find(|text| !text.is_empty())?
        .to_owned();
    bag.take("thread.id");
    Some(id)
}

/// Tokens under `codex.turn.token_usage.` on the turn. `input_tokens` already includes the
/// cache, so nothing is added back (D110). `None` unless the turn has an input or an output count.
pub(crate) fn usage(
    span_name: &str,
    kind: SpanKind,
    bag: &mut AttrBag<'_>,
    warnings: &mut Warnings,
) -> Option<Usage> {
    if kind != SpanKind::Llm || span_name != TURN {
        return None;
    }
    let input = tokens(bag, "input_tokens", warnings);
    let output = tokens(bag, "output_tokens", warnings);
    if input.is_none() && output.is_none() {
        return None;
    }
    let cache_read = tokens(bag, "cached_input_tokens", warnings);
    let cache_write = tokens(bag, "cache_write_input_tokens", warnings);
    let reasoning = tokens(bag, "reasoning_output_tokens", warnings);
    let total = tokens(bag, "total_tokens", warnings).or_else(|| {
        input
            .zip(output)
            .map(|(input, output)| input.saturating_add(output))
    });
    Some(Usage {
        input_tokens: input,
        output_tokens: output,
        cache_read_tokens: cache_read,
        cache_write_tokens: cache_write,
        reasoning_tokens: reasoning,
        total_tokens: total,
    })
}

fn tokens(bag: &mut AttrBag<'_>, name: &str, warnings: &mut Warnings) -> Option<i64> {
    count(bag, &format!("{TOKENS}{name}"), warnings)
}

/// D-CA-6 re-link. The turn gets the turn's id and no parent. A kept dispatch hangs under its
/// turn, and takes the tool's name when a turn id is known.
pub(crate) fn finish(span_name: &str, inputs: &Finish, built: &mut Span) {
    let Some(turn) = inputs.turn_id.as_deref() else {
        return;
    };
    if span_name == TURN {
        built.span_id = relink(turn);
        built.parent_span_id = None;
        return;
    }
    built.parent_span_id = Some(relink(turn));
    if let Some(name) = inputs.tool_name.as_deref() {
        rename_tool(built, name);
    }
}

/// D-CA-6: the first 8 bytes of SHA-256("cetra.codex.turn" ‖ turn id), never all zero.
pub(crate) fn relink(turn_id: &str) -> SpanId {
    let mut hasher = Sha256::new();
    hasher.update(b"cetra.codex.turn");
    hasher.update(turn_id.as_bytes());
    let digest = hasher.finalize();
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&digest[..8]);
    if bytes == [0; 8] {
        bytes[7] = 1;
    }
    // Eight bytes, and not all zeros: `from_bytes` only rejects the all-zero id.
    #[allow(clippy::expect_used)]
    {
        SpanId::from_bytes(bytes).expect("span id is eight non-zero bytes")
    }
}

#[cfg(test)]
mod tests {
    use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
    use opentelemetry_proto::tonic::trace::v1::Span as OtlpSpan;

    use super::*;
    use crate::agents::AgentContext;
    use crate::agents::test_support::{i, kv, request, run, s, span};

    fn codex(spans: Vec<OtlpSpan>) -> ExportTraceServiceRequest {
        request("codex_exec", "codex_exec", spans)
    }

    #[test]
    fn codex_internal_spans_are_discarded_and_counted() {
        let request = codex(vec![
            span("fs.get_metadata", vec![], vec![]),
            span("turn/start", vec![], vec![]),
            span("handle_responses", vec![], vec![]),
        ]);
        let normalized = run(&request, &AgentContexts::default());
        assert!(normalized.spans.is_empty());
        assert_eq!(normalized.discarded, 3);
        assert_eq!(normalized.rejections.total(), 0);
    }

    #[test]
    fn codex_discard_before_validation() {
        let mut internal = span("auth", vec![], vec![]);
        internal.start_time_unix_nano = 1;
        internal.end_time_unix_nano = 2;
        let normalized = run(&codex(vec![internal]), &AgentContexts::default());
        assert!(normalized.spans.is_empty());
        assert_eq!(normalized.discarded, 1);
        assert_eq!(normalized.rejections.total(), 0);
    }

    #[test]
    fn codex_turn_is_the_llm_root() {
        let turn = span(
            "session_task.turn",
            vec![
                kv("turn.id", s("t1")),
                kv("thread.id", i(32)),
                kv("thread.id", s("conv-1")),
                kv("model", s("gpt-5.5-codex")),
                kv("codex.turn.token_usage.input_tokens", i(800)),
                kv("codex.turn.token_usage.cached_input_tokens", i(600)),
                kv("codex.turn.token_usage.cache_write_input_tokens", i(0)),
                kv("codex.turn.token_usage.output_tokens", i(60)),
                kv("codex.turn.token_usage.reasoning_output_tokens", i(20)),
                kv("codex.turn.token_usage.total_tokens", i(860)),
            ],
            vec![],
        );
        let normalized = run(&codex(vec![turn]), &AgentContexts::default());
        let span = &normalized.spans[0];
        assert_eq!(span.span_kind, SpanKind::Llm);
        assert_eq!(span.parent_span_id, None);
        assert_eq!(span.span_id, relink("t1"));
        assert_eq!(span.session_id.as_deref(), Some("conv-1"));
        assert_eq!(span.request_model.as_deref(), Some("gpt-5.5-codex"));
        assert_eq!(span.input_tokens, Some(800));
        assert_eq!(span.cache_read_tokens, Some(600));
        assert_eq!(span.cache_write_tokens, Some(0));
        assert_eq!(span.output_tokens, Some(60));
        assert_eq!(span.reasoning_tokens, Some(20));
        assert_eq!(span.total_tokens, Some(860));
        assert_eq!(
            span.metadata.get("coding_agent").map(String::as_str),
            Some("codex")
        );
    }

    #[test]
    fn codex_turn_takes_the_first_string_thread_id() {
        let turn = span(
            "session_task.turn",
            vec![
                kv("turn.id", s("t1")),
                kv("thread.id", s("first")),
                kv("thread.id", s("second")),
            ],
            vec![],
        );
        let normalized = run(&codex(vec![turn]), &AgentContexts::default());
        assert_eq!(normalized.spans[0].session_id.as_deref(), Some("first"));
    }

    #[test]
    fn codex_tool_hangs_under_its_turn() {
        let mut tool = span(
            "exec_command",
            vec![
                kv("tool_name", s("exec_command")),
                kv("codex.tool.phase", s("dispatch")),
                kv("turn.id", s("t1")),
                kv("conversation.id", s("conv-1")),
            ],
            vec![],
        );
        tool.span_id = vec![0x33; 8];
        let normalized = run(&codex(vec![tool]), &AgentContexts::default());
        let span = &normalized.spans[0];
        assert_eq!(span.parent_span_id, Some(relink("t1")));
        assert_eq!(span.span_kind, SpanKind::Tool);
        assert_eq!(span.name, "exec_command");
        assert!(!span.attributes.contains_key("cetra.span_name"));
        assert_eq!(span.session_id.as_deref(), Some("conv-1"));
    }

    #[test]
    fn codex_session_context_fills_the_person_repository_and_branch() {
        let mut agents = AgentContexts::default();
        agents.insert(
            "codex",
            "conv-1",
            AgentContext {
                repository: Some("gitlab.com/x/y".to_owned()),
                branch: Some("main".to_owned()),
                user: Some(" Dev@Example.com ".to_owned()),
                updated_at: 1,
            },
        );
        let tool = span(
            "exec_command",
            vec![
                kv("tool_name", s("exec_command")),
                kv("codex.tool.phase", s("dispatch")),
                kv("conversation.id", s("conv-1")),
                kv("user.id", s("anon")),
            ],
            vec![],
        );
        let normalized = run(&codex(vec![tool]), &agents);
        let span = &normalized.spans[0];
        assert_eq!(span.user_id.as_deref(), Some("dev@example.com"));
        assert_eq!(
            span.metadata.get("repository").map(String::as_str),
            Some("gitlab.com/x/y")
        );
        assert_eq!(
            span.metadata.get("branch").map(String::as_str),
            Some("main")
        );
    }

    #[test]
    fn relink_is_stable() {
        assert_eq!(
            relink("t1").as_bytes(),
            &[0x89, 0x53, 0x85, 0x1c, 0xad, 0x03, 0x7c, 0xe1]
        );
    }
}
