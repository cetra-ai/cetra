//! Span kind from the mapping table.
//!
//! Spec 02 § Span kind. The first row that decides wins, and only that
//! attribute is consumed. Fallback keys are peeked and left unread for later
//! phases.

use cetra_core::enums::{Convention, SpanKind};
use opentelemetry_proto::tonic::common::v1::any_value::Value;

use crate::bag::AttrBag;
use crate::value::as_str;

/// Kind of one span. Missing and unrecognized values fall through to `span`.
///
/// `scope_name` is the span's instrumentation scope name (`""` when absent);
/// the Agent Framework and Mirascope rows are gated on it. `_conventions` is
/// the conventions detected on the span; no row reads it yet.
pub(crate) fn span_kind(
    bag: &mut AttrBag<'_>,
    scope_name: &str,
    _conventions: &[Convention],
) -> SpanKind {
    if let Some(kind) = decide(bag, "cetra.span.kind", |text| SpanKind::parse(text).ok()) {
        return kind;
    }
    if let Some(kind) = decide(bag, "gen_ai.operation.name", genai_operation) {
        return kind;
    }
    if let Some(kind) = decide(bag, "mastra.span.type", mastra) {
        return kind;
    }
    if let Some(kind) = decide(bag, "openinference.span.kind", openinference) {
        return kind;
    }
    if let Some(kind) = decide(bag, "ai.operationId", vercel) {
        return kind;
    }
    if let Some(kind) = decide(bag, "traceloop.span.kind", traceloop) {
        return kind;
    }
    if let Some(kind) = decide(bag, "llm.request.type", openllmetry) {
        return kind;
    }
    if let Some(kind) = decide(bag, "langfuse.observation.type", langfuse) {
        return kind;
    }
    if let Some(kind) = laminar(bag) {
        return kind;
    }
    // ADK's `call_llm` span sends no operation name; its `generate_content`
    // child is the model call and repeats the usage (D-B1-8).
    if present(bag, "gcp.vertex.agent.llm_request") && !present(bag, "gen_ai.tool.name") {
        return SpanKind::Chain;
    }
    if present(bag, "gen_ai.tool.name") || present(bag, "gen_ai.tool.call.id") {
        return SpanKind::Tool;
    }
    if let Some(kind) = scope_gated(bag, scope_name) {
        return kind;
    }
    if present(bag, "gen_ai.request.model") || present(bag, "llm.model_name") {
        return SpanKind::Llm;
    }
    SpanKind::Span
}

/// The rows gated on the instrumentation scope name. Their keys name no
/// operation, so the gate keeps them to the framework that writes them.
///
/// Microsoft Agent Framework (D-IN-5): with no `gen_ai.operation.name`, the
/// presence of an executor, edge group or workflow id makes the span `chain`.
/// The keys are peeked, not consumed. Mirascope (D-IN-23 item 6): its
/// `@ops.trace` spans carry only `mirascope.type`.
fn scope_gated(bag: &mut AttrBag<'_>, scope_name: &str) -> Option<SpanKind> {
    if scope_name == "agent_framework"
        && !present(bag, "gen_ai.operation.name")
        && (present(bag, "executor.id")
            || present(bag, "edge_group.id")
            || present(bag, "workflow.id"))
    {
        return Some(SpanKind::Chain);
    }
    if !scope_name.starts_with("mirascope") {
        return None;
    }
    decide(bag, "mirascope.type", |text| {
        matches!(text, "trace" | "response_resume").then_some(SpanKind::Chain)
    })
}

fn decide(
    bag: &mut AttrBag<'_>,
    key: &str,
    map: impl Fn(&str) -> Option<SpanKind>,
) -> Option<SpanKind> {
    let kind = map(bag.get(key).and_then(as_str)?)?;
    bag.take(key);
    Some(kind)
}

fn present(bag: &AttrBag<'_>, key: &str) -> bool {
    bag.get(key).is_some()
}

fn genai_operation(text: &str) -> Option<SpanKind> {
    Some(match text {
        "chat" | "text_completion" | "text_completions" | "generate_content" | "response"
        | "call_llm" => SpanKind::Llm,
        "embeddings" => SpanKind::Embedding,
        "execute_tool" => SpanKind::Tool,
        "invoke_agent" | "create_agent" => SpanKind::Agent,
        "retrieval" => SpanKind::Retriever,
        "invoke_workflow" | "invoke_runner" | "plan" | "execute_event_loop_cycle" => {
            SpanKind::Chain
        }
        _ => return None,
    })
}

/// Mastra's own span-type row (M1, D-IN-5): the steps around a model call.
/// `generic` and `model_chunk` decide nothing; neither do the values no
/// captured case shows (`workflow_conditional`, `rag_*`, `scorer_*`, …).
fn mastra(text: &str) -> Option<SpanKind> {
    Some(match text {
        "model_generation" | "model_step" | "processor_run" | "workflow_step" => SpanKind::Chain,
        _ => return None,
    })
}

fn openinference(text: &str) -> Option<SpanKind> {
    Some(match text {
        "LLM" => SpanKind::Llm,
        "EMBEDDING" => SpanKind::Embedding,
        "TOOL" => SpanKind::Tool,
        "AGENT" => SpanKind::Agent,
        "CHAIN" => SpanKind::Chain,
        "RETRIEVER" => SpanKind::Retriever,
        "RERANKER" => SpanKind::Reranker,
        "GUARDRAIL" => SpanKind::Guardrail,
        "EVALUATOR" => SpanKind::Evaluator,
        _ => return None,
    })
}

fn vercel(text: &str) -> Option<SpanKind> {
    if text.ends_with(".doGenerate") || text.ends_with(".doStream") {
        return Some(SpanKind::Llm);
    }
    if matches!(
        text,
        "ai.generateText" | "ai.streamText" | "ai.generateObject" | "ai.streamObject"
    ) {
        return Some(SpanKind::Chain);
    }
    if text == "ai.toolCall" {
        return Some(SpanKind::Tool);
    }
    if text.ends_with(".doEmbed") {
        return Some(SpanKind::Embedding);
    }
    // The outer call repeats its `doEmbed` child's usage (D-B1-6).
    if matches!(text, "ai.embed" | "ai.embedMany") {
        return Some(SpanKind::Chain);
    }
    None
}

fn traceloop(text: &str) -> Option<SpanKind> {
    Some(match text {
        "workflow" | "task" => SpanKind::Chain,
        "agent" => SpanKind::Agent,
        "tool" => SpanKind::Tool,
        _ => return None,
    })
}

fn openllmetry(text: &str) -> Option<SpanKind> {
    Some(match text {
        "chat" | "completion" => SpanKind::Llm,
        "embedding" => SpanKind::Embedding,
        _ => return None,
    })
}

fn langfuse(text: &str) -> Option<SpanKind> {
    Some(match text {
        "generation" => SpanKind::Llm,
        "embedding" => SpanKind::Embedding,
        "tool" => SpanKind::Tool,
        "retriever" => SpanKind::Retriever,
        "agent" => SpanKind::Agent,
        "chain" => SpanKind::Chain,
        "evaluator" => SpanKind::Evaluator,
        "guardrail" => SpanKind::Guardrail,
        "span" | "event" => SpanKind::Span,
        _ => return None,
    })
}

/// The `lmnr.span.type` row, which reads the bag: `LLM` and `CACHED` are `llm`
/// only when the span names a model or counts tokens. Laminar's OpenHands
/// wrapper names neither (D-IN-23 item 8): it wraps its child's model call and
/// repeats its cost, so it is `chain` and its cost is not added.
///
/// An empty string decides nothing. Any other string does, and unknown
/// spellings are `chain`.
fn laminar(bag: &mut AttrBag<'_>) -> Option<SpanKind> {
    let text = bag.get("lmnr.span.type").and_then(as_str)?;
    if text.is_empty() {
        return None;
    }
    let kind = match text {
        "LLM" | "CACHED" if names_model_or_tokens(bag) => SpanKind::Llm,
        "TOOL" => SpanKind::Tool,
        "EXECUTOR" => SpanKind::Agent,
        "EVALUATOR" | "EVALUATION" | "HUMAN_EVALUATOR" => SpanKind::Evaluator,
        _ => SpanKind::Chain,
    };
    bag.take("lmnr.span.type");
    Some(kind)
}

/// The models a Laminar `LLM` span may name.
const LAMINAR_MODEL_KEYS: &[&str] = &[
    "gen_ai.request.model",
    "gen_ai.response.model",
    "llm.model_name",
];

/// The token counts the usage readers know, input then output.
const LAMINAR_TOKEN_KEYS: &[&str] = &[
    "gen_ai.usage.input_tokens",
    "gen_ai.usage.output_tokens",
    "gen_ai.usage.prompt_tokens",
    "gen_ai.usage.completion_tokens",
    "llm.token_count.prompt",
    "llm.token_count.completion",
];

/// Whether a model key holds a non-empty string or a token key a non-zero
/// integer. Peeked: the model and usage reads still find these keys.
fn names_model_or_tokens(bag: &AttrBag<'_>) -> bool {
    LAMINAR_MODEL_KEYS
        .iter()
        .chain(LAMINAR_TOKEN_KEYS)
        .any(|&key| {
            bag.get(key)
                .and_then(|value| value.value.as_ref())
                .is_some_and(model_or_token)
        })
}

/// A non-empty string or a non-zero integer.
fn model_or_token(value: &Value) -> bool {
    match value {
        Value::StringValue(text) => !text.is_empty(),
        Value::IntValue(number) => *number != 0,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry_proto::tonic::common::v1::{AnyValue, KeyValue};

    use crate::bag::Use;
    use crate::warn::Warnings;

    fn string_attr(key: &str, text: &str) -> KeyValue {
        KeyValue {
            key: key.to_owned(),
            value: Some(AnyValue {
                value: Some(Value::StringValue(text.to_owned())),
            }),
            ..KeyValue::default()
        }
    }

    fn int_attr(key: &str, number: i64) -> KeyValue {
        KeyValue {
            key: key.to_owned(),
            value: Some(AnyValue {
                value: Some(Value::IntValue(number)),
            }),
            ..KeyValue::default()
        }
    }

    fn bool_attr(key: &str) -> KeyValue {
        KeyValue {
            key: key.to_owned(),
            value: Some(AnyValue {
                value: Some(Value::BoolValue(true)),
            }),
            ..KeyValue::default()
        }
    }

    fn kind_of(attrs: &[KeyValue]) -> SpanKind {
        kind_in("", attrs)
    }

    fn kind_in(scope: &str, attrs: &[KeyValue]) -> SpanKind {
        let mut bag = AttrBag::new(attrs, &mut Warnings::default());
        span_kind(&mut bag, scope, &[])
    }

    #[test]
    fn cetra_span_kind_is_the_parsed_kind() {
        for (text, kind) in [
            ("llm", SpanKind::Llm),
            ("embedding", SpanKind::Embedding),
            ("tool", SpanKind::Tool),
            ("agent", SpanKind::Agent),
            ("chain", SpanKind::Chain),
            ("retriever", SpanKind::Retriever),
            ("reranker", SpanKind::Reranker),
            ("guardrail", SpanKind::Guardrail),
            ("evaluator", SpanKind::Evaluator),
            ("span", SpanKind::Span),
        ] {
            let attrs = [
                string_attr("cetra.span.kind", text),
                string_attr("gen_ai.operation.name", "chat"),
            ];
            let mut bag = AttrBag::new(&attrs, &mut Warnings::default());
            assert_eq!(span_kind(&mut bag, "", &[]), kind, "{text}");
            assert_eq!(bag.state("cetra.span.kind"), Some(Use::Consumed));
            assert_eq!(bag.state("gen_ai.operation.name"), Some(Use::Unread));
        }
        assert_eq!(
            kind_of(&[string_attr("cetra.span.kind", "LLM")]),
            SpanKind::Span
        );
    }

    #[test]
    fn an_invalid_cetra_span_kind_falls_through() {
        let attrs = [
            string_attr("cetra.span.kind", "nope"),
            string_attr("gen_ai.operation.name", "chat"),
        ];
        let mut bag = AttrBag::new(&attrs, &mut Warnings::default());
        assert_eq!(span_kind(&mut bag, "", &[]), SpanKind::Llm);
        assert_eq!(bag.state("cetra.span.kind"), Some(Use::Unread));
        assert_eq!(bag.state("gen_ai.operation.name"), Some(Use::Consumed));
    }

    #[test]
    fn gen_ai_operation_name_uses_the_semconv_table() {
        for (text, kind) in [
            ("chat", SpanKind::Llm),
            ("text_completion", SpanKind::Llm),
            ("text_completions", SpanKind::Llm),
            ("generate_content", SpanKind::Llm),
            ("response", SpanKind::Llm),
            ("call_llm", SpanKind::Llm),
            ("embeddings", SpanKind::Embedding),
            ("execute_tool", SpanKind::Tool),
            ("invoke_agent", SpanKind::Agent),
            ("create_agent", SpanKind::Agent),
            ("retrieval", SpanKind::Retriever),
            ("invoke_workflow", SpanKind::Chain),
            ("invoke_runner", SpanKind::Chain),
            ("plan", SpanKind::Chain),
            ("execute_event_loop_cycle", SpanKind::Chain),
        ] {
            assert_eq!(
                kind_of(&[string_attr("gen_ai.operation.name", text)]),
                kind,
                "{text}"
            );
        }
        let attrs = [
            string_attr("gen_ai.operation.name", "invoke"),
            string_attr("openinference.span.kind", "TOOL"),
        ];
        let mut bag = AttrBag::new(&attrs, &mut Warnings::default());
        assert_eq!(span_kind(&mut bag, "", &[]), SpanKind::Tool);
        assert_eq!(bag.state("gen_ai.operation.name"), Some(Use::Unread));
        assert_eq!(bag.state("openinference.span.kind"), Some(Use::Consumed));
    }

    #[test]
    fn mastra_span_type_marks_the_steps_around_a_model_call() {
        for text in [
            "model_generation",
            "model_step",
            "processor_run",
            "workflow_step",
        ] {
            let attrs = [string_attr("mastra.span.type", text)];
            let mut bag = AttrBag::new(&attrs, &mut Warnings::default());
            assert_eq!(span_kind(&mut bag, "", &[]), SpanKind::Chain, "{text}");
            assert_eq!(bag.state("mastra.span.type"), Some(Use::Consumed));
        }
        for text in ["generic", "model_chunk", "agent_run"] {
            let attrs = [string_attr("mastra.span.type", text)];
            let mut bag = AttrBag::new(&attrs, &mut Warnings::default());
            assert_eq!(span_kind(&mut bag, "", &[]), SpanKind::Span, "{text}");
            assert_eq!(bag.state("mastra.span.type"), Some(Use::Unread));
        }
        // The operation row comes first, so a Mastra span that names one is
        // decided by it.
        let attrs = [
            string_attr("gen_ai.operation.name", "chat"),
            string_attr("mastra.span.type", "model_step"),
        ];
        let mut bag = AttrBag::new(&attrs, &mut Warnings::default());
        assert_eq!(span_kind(&mut bag, "", &[]), SpanKind::Llm);
        assert_eq!(bag.state("mastra.span.type"), Some(Use::Unread));
    }

    #[test]
    fn agent_framework_keys_are_chain_only_under_their_scope() {
        for key in ["executor.id", "edge_group.id", "workflow.id"] {
            let attrs = [string_attr(key, "wf-1")];
            let mut bag = AttrBag::new(&attrs, &mut Warnings::default());
            assert_eq!(
                span_kind(&mut bag, "agent_framework", &[]),
                SpanKind::Chain,
                "{key}"
            );
            // Presence fallbacks are peeked, never consumed.
            assert_eq!(bag.state(key), Some(Use::Unread));
        }
        // Without the scope, or with an operation name, the row does not apply.
        assert_eq!(
            kind_of(&[string_attr("executor.id", "upper")]),
            SpanKind::Span
        );
        assert_eq!(
            kind_in("semantic_kernel", &[string_attr("executor.id", "upper")]),
            SpanKind::Span
        );
        let attrs = [
            string_attr("gen_ai.operation.name", "chat"),
            string_attr("executor.id", "upper"),
        ];
        assert_eq!(kind_in("agent_framework", &attrs), SpanKind::Llm);
        // `message.send` carries none of the keys and stays `span`.
        assert_eq!(
            kind_in("agent_framework", &[string_attr("message.type", "str")]),
            SpanKind::Span
        );
    }

    #[test]
    fn mirascope_type_is_chain_only_under_its_scope() {
        for text in ["trace", "response_resume"] {
            let attrs = [string_attr("mirascope.type", text)];
            let mut bag = AttrBag::new(&attrs, &mut Warnings::default());
            assert_eq!(
                span_kind(&mut bag, "mirascope.ops", &[]),
                SpanKind::Chain,
                "{text}"
            );
            assert_eq!(bag.state("mirascope.type"), Some(Use::Consumed));
        }
        // Without the scope the key decides nothing.
        let attrs = [string_attr("mirascope.type", "trace")];
        let mut bag = AttrBag::new(&attrs, &mut Warnings::default());
        assert_eq!(span_kind(&mut bag, "", &[]), SpanKind::Span);
        assert_eq!(bag.state("mirascope.type"), Some(Use::Unread));
        assert_eq!(kind_in("other.framework", &attrs), SpanKind::Span);
        // Another value under the scope does not decide either.
        let attrs = [string_attr("mirascope.type", "other")];
        let mut bag = AttrBag::new(&attrs, &mut Warnings::default());
        assert_eq!(span_kind(&mut bag, "mirascope.ops", &[]), SpanKind::Span);
        assert_eq!(bag.state("mirascope.type"), Some(Use::Unread));
    }

    #[test]
    fn openinference_span_kind_matches_the_uppercase_spellings() {
        for (text, kind) in [
            ("LLM", SpanKind::Llm),
            ("EMBEDDING", SpanKind::Embedding),
            ("TOOL", SpanKind::Tool),
            ("AGENT", SpanKind::Agent),
            ("CHAIN", SpanKind::Chain),
            ("RETRIEVER", SpanKind::Retriever),
            ("RERANKER", SpanKind::Reranker),
            ("GUARDRAIL", SpanKind::Guardrail),
            ("EVALUATOR", SpanKind::Evaluator),
        ] {
            assert_eq!(
                kind_of(&[string_attr("openinference.span.kind", text)]),
                kind,
                "{text}"
            );
        }
        assert_eq!(
            kind_of(&[string_attr("openinference.span.kind", "llm")]),
            SpanKind::Span
        );
    }

    #[test]
    fn ai_operation_id_matches_suffixes_then_exact_names_then_embed() {
        for (text, kind) in [
            ("ai.generateText.doGenerate", SpanKind::Llm),
            ("ai.streamText.doStream", SpanKind::Llm),
            ("custom.doGenerate", SpanKind::Llm),
            ("ai.embed.doGenerate", SpanKind::Llm),
            ("ai.generateText", SpanKind::Chain),
            ("ai.streamText", SpanKind::Chain),
            ("ai.generateObject", SpanKind::Chain),
            ("ai.streamObject", SpanKind::Chain),
            ("ai.toolCall", SpanKind::Tool),
            ("ai.embed", SpanKind::Chain),
            ("ai.embedMany", SpanKind::Chain),
            ("ai.embed.doEmbed", SpanKind::Embedding),
            ("ai.embedMany.doEmbed", SpanKind::Embedding),
        ] {
            assert_eq!(
                kind_of(&[string_attr("ai.operationId", text)]),
                kind,
                "{text}"
            );
        }
        assert_eq!(
            kind_of(&[string_attr("ai.operationId", "doGenerate")]),
            SpanKind::Span
        );
        assert_eq!(
            kind_of(&[string_attr(
                "ai.operationId",
                "ai.generateText.doGenerateExtra"
            )]),
            SpanKind::Span
        );
    }

    #[test]
    fn traceloop_span_kind_maps_workflow_and_task_to_chain() {
        for (text, kind) in [
            ("workflow", SpanKind::Chain),
            ("task", SpanKind::Chain),
            ("agent", SpanKind::Agent),
            ("tool", SpanKind::Tool),
        ] {
            assert_eq!(
                kind_of(&[string_attr("traceloop.span.kind", text)]),
                kind,
                "{text}"
            );
        }
        assert_eq!(
            kind_of(&[string_attr("traceloop.span.kind", "workflow ")]),
            SpanKind::Span
        );
    }

    #[test]
    fn llm_request_type_maps_chat_completion_and_embedding() {
        for (text, kind) in [
            ("chat", SpanKind::Llm),
            ("completion", SpanKind::Llm),
            ("embedding", SpanKind::Embedding),
        ] {
            assert_eq!(
                kind_of(&[string_attr("llm.request.type", text)]),
                kind,
                "{text}"
            );
        }
        assert_eq!(
            kind_of(&[string_attr("llm.request.type", "Chat")]),
            SpanKind::Span
        );
    }

    #[test]
    fn langfuse_observation_type_maps_generation_and_keeps_the_other_kinds() {
        for (text, kind) in [
            ("generation", SpanKind::Llm),
            ("embedding", SpanKind::Embedding),
            ("tool", SpanKind::Tool),
            ("retriever", SpanKind::Retriever),
            ("agent", SpanKind::Agent),
            ("chain", SpanKind::Chain),
            ("evaluator", SpanKind::Evaluator),
            ("guardrail", SpanKind::Guardrail),
            ("span", SpanKind::Span),
            ("event", SpanKind::Span),
        ] {
            assert_eq!(
                kind_of(&[string_attr("langfuse.observation.type", text)]),
                kind,
                "{text}"
            );
        }
        assert_eq!(
            kind_of(&[string_attr("langfuse.observation.type", "GENERATION")]),
            SpanKind::Span
        );
    }

    #[test]
    fn lmnr_span_type_maps_known_spellings_and_other_strings_to_chain() {
        for (text, kind) in [
            ("TOOL", SpanKind::Tool),
            ("EXECUTOR", SpanKind::Agent),
            ("EVALUATOR", SpanKind::Evaluator),
            ("EVALUATION", SpanKind::Evaluator),
            ("HUMAN_EVALUATOR", SpanKind::Evaluator),
            ("SPAN", SpanKind::Chain),
            ("llm", SpanKind::Chain),
        ] {
            let attrs = [string_attr("lmnr.span.type", text)];
            let mut bag = AttrBag::new(&attrs, &mut Warnings::default());
            assert_eq!(span_kind(&mut bag, "", &[]), kind, "{text}");
            assert_eq!(bag.state("lmnr.span.type"), Some(Use::Consumed));
        }
        let attrs = [string_attr("lmnr.span.type", "")];
        let mut bag = AttrBag::new(&attrs, &mut Warnings::default());
        assert_eq!(span_kind(&mut bag, "", &[]), SpanKind::Span);
        assert_eq!(bag.state("lmnr.span.type"), Some(Use::Unread));
        assert_eq!(kind_of(&[bool_attr("lmnr.span.type")]), SpanKind::Span);
    }

    #[test]
    fn lmnr_llm_and_cached_are_llm_only_with_a_model_or_tokens() {
        // A model, as a non-empty string, on either spelling.
        for text in ["LLM", "CACHED"] {
            for key in [
                "gen_ai.request.model",
                "gen_ai.response.model",
                "llm.model_name",
            ] {
                let attrs = [
                    string_attr("lmnr.span.type", text),
                    string_attr(key, "gpt-4o-mini"),
                ];
                let mut bag = AttrBag::new(&attrs, &mut Warnings::default());
                assert_eq!(span_kind(&mut bag, "", &[]), SpanKind::Llm, "{text} {key}");
                assert_eq!(bag.state("lmnr.span.type"), Some(Use::Consumed));
                // Peeked only: the model column still reads it.
                assert_eq!(bag.state(key), Some(Use::Unread));
            }
        }
        // A token count, as a non-zero integer, on either spelling.
        for text in ["LLM", "CACHED"] {
            for key in [
                "gen_ai.usage.input_tokens",
                "gen_ai.usage.output_tokens",
                "gen_ai.usage.prompt_tokens",
                "gen_ai.usage.completion_tokens",
                "llm.token_count.prompt",
                "llm.token_count.completion",
            ] {
                let attrs = [string_attr("lmnr.span.type", text), int_attr(key, 12)];
                let mut bag = AttrBag::new(&attrs, &mut Warnings::default());
                assert_eq!(span_kind(&mut bag, "", &[]), SpanKind::Llm, "{text} {key}");
                assert_eq!(bag.state(key), Some(Use::Unread));
            }
        }
        // Without a model or tokens: the OpenHands wrapper (D-IN-23 item 8).
        for text in ["LLM", "CACHED"] {
            let attrs = [string_attr("lmnr.span.type", text)];
            let mut bag = AttrBag::new(&attrs, &mut Warnings::default());
            assert_eq!(span_kind(&mut bag, "", &[]), SpanKind::Chain, "{text}");
            assert_eq!(bag.state("lmnr.span.type"), Some(Use::Consumed));
        }
        // A zero count and an empty model do not count.
        let attrs = [
            string_attr("lmnr.span.type", "LLM"),
            string_attr("gen_ai.request.model", ""),
            int_attr("gen_ai.usage.input_tokens", 0),
            int_attr("gen_ai.usage.output_tokens", 0),
        ];
        let mut bag = AttrBag::new(&attrs, &mut Warnings::default());
        assert_eq!(span_kind(&mut bag, "", &[]), SpanKind::Chain);
        assert_eq!(bag.state("gen_ai.request.model"), Some(Use::Unread));
    }

    #[test]
    fn a_tool_key_wins_over_a_model_key_and_neither_is_consumed() {
        let attrs = [
            string_attr("gen_ai.tool.name", ""),
            string_attr("gen_ai.request.model", "gpt"),
        ];
        let mut bag = AttrBag::new(&attrs, &mut Warnings::default());
        assert_eq!(span_kind(&mut bag, "", &[]), SpanKind::Tool);
        assert_eq!(bag.state("gen_ai.tool.name"), Some(Use::Unread));
        assert_eq!(bag.state("gen_ai.request.model"), Some(Use::Unread));

        let attrs = [string_attr("gen_ai.tool.call.id", "call-1")];
        assert_eq!(kind_of(&attrs), SpanKind::Tool);

        let attrs = [string_attr("llm.model_name", "gpt")];
        let mut bag = AttrBag::new(&attrs, &mut Warnings::default());
        assert_eq!(span_kind(&mut bag, "", &[]), SpanKind::Llm);
        assert_eq!(bag.state("llm.model_name"), Some(Use::Unread));

        assert_eq!(kind_of(&[]), SpanKind::Span);
    }
}
