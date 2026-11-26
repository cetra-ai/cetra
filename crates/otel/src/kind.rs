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
/// `span_name` is the span's name (`""` when absent); the LiveKit and Pipecat
/// rows are gated on it. `scope_name` is the span's instrumentation scope name
/// (`""` when absent); the Agent Framework and Mirascope rows are gated on it.
/// `conventions` is the conventions detected on the span; the VoltAgent row is
/// gated on it.
pub(crate) fn span_kind(
    bag: &mut AttrBag<'_>,
    span_name: &str,
    scope_name: &str,
    conventions: &[Convention],
) -> SpanKind {
    if let Some(kind) = decide(bag, "cetra.span.kind", |text| SpanKind::parse(text).ok()) {
        return kind;
    }
    if let Some(kind) = livekit(span_name, scope_name) {
        return kind;
    }
    if let Some(kind) = decide(bag, "gen_ai.operation.name", genai_operation) {
        return kind;
    }
    if let Some(kind) = voltagent(bag, conventions) {
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
    if let Some(kind) = scope_gated(bag, span_name, scope_name) {
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
///
/// Bifrost (D-IN-25): its server root span has no `gen_ai.operation.name` and
/// repeats its `chat` child's model and content, so under the scope `bifrost`
/// a model key makes such a span `chain`. The keys are peeked, not consumed,
/// and its `chat` child is decided earlier by its operation name.
///
/// Prompt Flow (`span_type`, R-PF-1), Langflow (`trace_type`, `flow_id`, R-LF-1)
/// and n8n (span name, R-N8N-2, R-N8N-3) decide on their own scope only.
///
/// OpenLIT (R-OL-1, D-IN-26 item 11) and Promptfoo (R4, D-IN-26 item 11) also
/// decide on their own scope only: OpenLIT's `gen_ai.operation.type` names the
/// operation when `gen_ai.operation.name` did not (the rows above run first, so
/// reaching this function means it did not decide); a Promptfoo grader span
/// carries `gen_ai.evaluation.name`.
///
/// The vector databases decide here too (Q1 (a), D-IN-26): an OpenLIT
/// `vectordb` span is a `retriever` when it reads (`db.operation.name`
/// `get`, `query` or `search`, lowercased; Chroma writes them in caps) and a
/// plain `span` otherwise, and OpenLLMetry's vector instrumentors name their
/// read operations, so those span names decide. Writes stay `span`.
fn scope_gated(bag: &mut AttrBag<'_>, span_name: &str, scope_name: &str) -> Option<SpanKind> {
    // Pipecat's conversation and turn spans (D-IN-5) name no operation.
    if scope_name == "pipecat.turn" && matches!(span_name, "conversation" | "turn") {
        return Some(SpanKind::Chain);
    }
    if scope_name == "agent_framework"
        && !present(bag, "gen_ai.operation.name")
        && (present(bag, "executor.id")
            || present(bag, "edge_group.id")
            || present(bag, "workflow.id"))
    {
        return Some(SpanKind::Chain);
    }
    if scope_name == "bifrost"
        && !present(bag, "gen_ai.operation.name")
        && (present(bag, "gen_ai.request.model") || present(bag, "gen_ai.response.model"))
    {
        return Some(SpanKind::Chain);
    }
    // Prompt Flow (R-PF-1, D-IN-5): its own `span_type`. `Function` is a plain
    // span: a `@trace` function is anything.
    if scope_name == "promptflow" {
        return decide(bag, "span_type", |text| {
            Some(match text {
                "LLM" => SpanKind::Llm,
                "Embedding" => SpanKind::Embedding,
                "Retrieval" => SpanKind::Retriever,
                "Flow" | "LangChain" => SpanKind::Chain,
                "Function" => SpanKind::Span,
                _ => return None,
            })
        });
    }
    // Langflow (R-LF-1, D-IN-5). Its model call is a separate trace, so
    // `llm` is `chain` here: reading it as `llm` would count the model twice.
    if scope_name == "langflow" {
        if let Some(kind) = decide(bag, "trace_type", |text| {
            Some(match text {
                "agent" => SpanKind::Agent,
                "tool" => SpanKind::Tool,
                "retriever" => SpanKind::Retriever,
                "embedding" => SpanKind::Embedding,
                _ => SpanKind::Chain,
            })
        }) {
            return Some(kind);
        }
        return present(bag, "flow_id").then_some(SpanKind::Chain);
    }
    // n8n (R-N8N-2, R-N8N-3, D-IN-5): every node span is named `node.execute`,
    // and a node with no GenAI operation is a plain span.
    if scope_name == "n8n-workflow" {
        return match span_name {
            "workflow.execute" => Some(SpanKind::Chain),
            "node.execute" if !present(bag, "gen_ai.operation.name") => Some(SpanKind::Span),
            _ => None,
        };
    }
    // OpenLIT (R-OL-1, D-IN-26 item 11): Browser Use 0.x writes its agent and
    // step spans with a non-standard `gen_ai.operation.name` (`run`, `step`) and
    // the operation in `gen_ai.operation.type`. Unit-tested only.
    //
    // OpenLIT's vector spans (Q1 (a), D-IN-26): `gen_ai.operation.name` is
    // `vectordb`, which no row maps, and `db.operation.name` names the
    // operation. Only a read is a `retriever`; writes stay plain spans. The
    // keys are peeked, not consumed: `db.operation.name` stays in
    // `attributes`.
    if scope_name.starts_with("openlit.instrumentation.") {
        if bag.get("gen_ai.operation.name").and_then(as_str) == Some("vectordb") {
            let reads = bag
                .get("db.operation.name")
                .and_then(as_str)
                .is_some_and(|text| {
                    matches!(&*text.to_ascii_lowercase(), "get" | "query" | "search")
                });
            return Some(if reads {
                SpanKind::Retriever
            } else {
                SpanKind::Span
            });
        }
        return decide(bag, "gen_ai.operation.type", |text| {
            Some(match text {
                "invoke_agent" => SpanKind::Agent,
                "execute_task" => SpanKind::Chain,
                _ => return None,
            })
        });
    }
    if let Some(kind) = openllmetry_vector(span_name, scope_name) {
        return Some(kind);
    }
    // Promptfoo (R4, D-IN-26 item 11): the grader span of an assertion. Its
    // `chat` child is decided by its operation name and keeps the tokens. The
    // key is peeked, not consumed.
    if scope_name == "promptfoo.providers" && present(bag, "gen_ai.evaluation.name") {
        return Some(SpanKind::Evaluator);
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
        "embeddings" | "embedding" | "aembedding" => SpanKind::Embedding,
        "execute_tool" => SpanKind::Tool,
        "invoke_agent" | "create_agent" => SpanKind::Agent,
        "retrieval" => SpanKind::Retriever,
        "invoke_workflow" | "invoke_runner" | "plan" | "execute_event_loop_cycle" => {
            SpanKind::Chain
        }
        // Pipecat's speech spans (R-PC-2, D-IN-5): no other library writes them.
        "tts" | "stt" => SpanKind::Span,
        _ => return None,
    })
}

/// The LiveKit Agents rows (R-LK-2, D-IN-5), on scope `livekit-agents`:
/// `llm_node` is `chain`; `llm_request_run` and `start_agent_activity` are
/// `span`. `llm_request` is not here: its `gen_ai.operation.name = chat`
/// decides it `llm`. Consumes nothing: the operation key stays for the
/// operation column.
fn livekit(span_name: &str, scope_name: &str) -> Option<SpanKind> {
    if scope_name != "livekit-agents" {
        return None;
    }
    Some(match span_name {
        "llm_node" => SpanKind::Chain,
        "llm_request_run" | "start_agent_activity" => SpanKind::Span,
        _ => return None,
    })
}

/// The OpenLLMetry vector-database rows (R2, Q1 (a), D-IN-26), on the scopes
/// `opentelemetry.instrumentation.{chromadb,qdrant,milvus,pinecone,weaviate}`:
/// the read operations are `retriever`, recognized by span name because Qdrant
/// and Pinecone write no `db.operation`. Every other name decides nothing, so
/// writes stay plain spans. The Pinecone and Weaviate rows are unit-tested
/// only (D-IN-26 item 11). Consumes nothing.
fn openllmetry_vector(span_name: &str, scope_name: &str) -> Option<SpanKind> {
    let reads = match scope_name {
        "opentelemetry.instrumentation.chromadb" => {
            matches!(span_name, "chroma.query" | "chroma.get")
        }
        "opentelemetry.instrumentation.qdrant" => {
            matches!(
                span_name,
                "qdrant.query_points" | "qdrant.search" | "qdrant.scroll"
            )
        }
        "opentelemetry.instrumentation.milvus" => {
            matches!(span_name, "milvus.search" | "milvus.query" | "milvus.get")
        }
        "opentelemetry.instrumentation.pinecone" => {
            matches!(span_name, "pinecone.query" | "pinecone.fetch")
        }
        "opentelemetry.instrumentation.weaviate" => {
            span_name.starts_with("db.weaviate.collections.query.")
                || span_name.starts_with("db.weaviate.gql.query.")
        }
        _ => return None,
    };
    reads.then_some(SpanKind::Retriever)
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

/// VoltAgent's rows (V2, D-IN-6, D-IN-23 item 9), only on a `voltagent` span:
/// `span.type` `llm` and `tool` decide, and only without a `span.type` does
/// `entity.type` `agent` decide `agent`. The other values the exporter writes
/// (`memory`, `guardrail`, `workflow-step`, …) wait for a captured case.
fn voltagent(bag: &mut AttrBag<'_>, conventions: &[Convention]) -> Option<SpanKind> {
    if !conventions.contains(&Convention::Voltagent) {
        return None;
    }
    if present(bag, "span.type") {
        return decide(bag, "span.type", voltagent_span_type);
    }
    decide(bag, "entity.type", |text| {
        (text == "agent").then_some(SpanKind::Agent)
    })
}

fn voltagent_span_type(text: &str) -> Option<SpanKind> {
    Some(match text {
        "llm" => SpanKind::Llm,
        "tool" => SpanKind::Tool,
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
        span_kind(&mut bag, "", scope, &[])
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
            assert_eq!(span_kind(&mut bag, "", "", &[]), kind, "{text}");
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
        assert_eq!(span_kind(&mut bag, "", "", &[]), SpanKind::Llm);
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
            ("embedding", SpanKind::Embedding),
            ("aembedding", SpanKind::Embedding),
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
        assert_eq!(span_kind(&mut bag, "", "", &[]), SpanKind::Tool);
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
            assert_eq!(span_kind(&mut bag, "", "", &[]), SpanKind::Chain, "{text}");
            assert_eq!(bag.state("mastra.span.type"), Some(Use::Consumed));
        }
        for text in ["generic", "model_chunk", "agent_run"] {
            let attrs = [string_attr("mastra.span.type", text)];
            let mut bag = AttrBag::new(&attrs, &mut Warnings::default());
            assert_eq!(span_kind(&mut bag, "", "", &[]), SpanKind::Span, "{text}");
            assert_eq!(bag.state("mastra.span.type"), Some(Use::Unread));
        }
        // The operation row comes first, so a Mastra span that names one is
        // decided by it.
        let attrs = [
            string_attr("gen_ai.operation.name", "chat"),
            string_attr("mastra.span.type", "model_step"),
        ];
        let mut bag = AttrBag::new(&attrs, &mut Warnings::default());
        assert_eq!(span_kind(&mut bag, "", "", &[]), SpanKind::Llm);
        assert_eq!(bag.state("mastra.span.type"), Some(Use::Unread));
    }

    #[test]
    fn agent_framework_keys_are_chain_only_under_their_scope() {
        for key in ["executor.id", "edge_group.id", "workflow.id"] {
            let attrs = [string_attr(key, "wf-1")];
            let mut bag = AttrBag::new(&attrs, &mut Warnings::default());
            assert_eq!(
                span_kind(&mut bag, "", "agent_framework", &[]),
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
    fn bifrost_root_spans_are_chain_only_under_their_scope() {
        // The server root span: scope `bifrost`, no operation name, a request
        // model. The keys are peeked, never consumed.
        let attrs = [string_attr("gen_ai.request.model", "gpt-4o-mini")];
        let mut bag = AttrBag::new(&attrs, &mut Warnings::default());
        assert_eq!(span_kind(&mut bag, "", "bifrost", &[]), SpanKind::Chain);
        assert_eq!(bag.state("gen_ai.request.model"), Some(Use::Unread));
        // The response model alone decides too.
        let attrs = [string_attr(
            "gen_ai.response.model",
            "gpt-4o-mini-2024-07-18",
        )];
        let mut bag = AttrBag::new(&attrs, &mut Warnings::default());
        assert_eq!(span_kind(&mut bag, "", "bifrost", &[]), SpanKind::Chain);
        assert_eq!(bag.state("gen_ai.response.model"), Some(Use::Unread));
        // The same attributes in another scope stay `llm` (model fallback).
        assert_eq!(
            kind_of(&[string_attr("gen_ai.request.model", "gpt-4o-mini")]),
            SpanKind::Llm
        );
        assert_eq!(
            kind_in(
                "semantic_kernel",
                &[string_attr("gen_ai.request.model", "gpt-4o-mini")]
            ),
            SpanKind::Llm
        );
        // Without a model (the 24 `plugin.*` spans carry no attributes) the
        // scope decides nothing.
        assert_eq!(kind_in("bifrost", &[]), SpanKind::Span);
        // A `chat` span names its operation and stays `llm`.
        let attrs = [
            string_attr("gen_ai.operation.name", "chat"),
            string_attr("gen_ai.request.model", "gpt-4o-mini"),
        ];
        let mut bag = AttrBag::new(&attrs, &mut Warnings::default());
        assert_eq!(span_kind(&mut bag, "", "bifrost", &[]), SpanKind::Llm);
        assert_eq!(bag.state("gen_ai.operation.name"), Some(Use::Consumed));
    }

    #[test]
    fn mirascope_type_is_chain_only_under_its_scope() {
        for text in ["trace", "response_resume"] {
            let attrs = [string_attr("mirascope.type", text)];
            let mut bag = AttrBag::new(&attrs, &mut Warnings::default());
            assert_eq!(
                span_kind(&mut bag, "", "mirascope.ops", &[]),
                SpanKind::Chain,
                "{text}"
            );
            assert_eq!(bag.state("mirascope.type"), Some(Use::Consumed));
        }
        // Without the scope the key decides nothing.
        let attrs = [string_attr("mirascope.type", "trace")];
        let mut bag = AttrBag::new(&attrs, &mut Warnings::default());
        assert_eq!(span_kind(&mut bag, "", "", &[]), SpanKind::Span);
        assert_eq!(bag.state("mirascope.type"), Some(Use::Unread));
        assert_eq!(kind_in("other.framework", &attrs), SpanKind::Span);
        // Another value under the scope does not decide either.
        let attrs = [string_attr("mirascope.type", "other")];
        let mut bag = AttrBag::new(&attrs, &mut Warnings::default());
        assert_eq!(
            span_kind(&mut bag, "", "mirascope.ops", &[]),
            SpanKind::Span
        );
        assert_eq!(bag.state("mirascope.type"), Some(Use::Unread));
    }

    #[test]
    fn openlit_operation_type_decides_only_when_the_name_did_not() {
        // Browser Use 0.x (R-OL-1): `run` and `step` are not standard names.
        for (text, kind) in [
            ("invoke_agent", SpanKind::Agent),
            ("execute_task", SpanKind::Chain),
        ] {
            let attrs = [
                string_attr("gen_ai.operation.name", "run"),
                string_attr("gen_ai.operation.type", text),
            ];
            let mut bag = AttrBag::new(&attrs, &mut Warnings::default());
            assert_eq!(
                span_kind(&mut bag, "", "openlit.instrumentation.browser_use", &[]),
                kind,
                "{text}"
            );
            assert_eq!(bag.state("gen_ai.operation.type"), Some(Use::Consumed));
            // The unmapped operation name decides nothing and stays unread.
            assert_eq!(bag.state("gen_ai.operation.name"), Some(Use::Unread));
        }
        // A standard operation name decides first.
        let attrs = [
            string_attr("gen_ai.operation.name", "chat"),
            string_attr("gen_ai.operation.type", "invoke_agent"),
        ];
        assert_eq!(
            kind_in("openlit.instrumentation.openai", &attrs),
            SpanKind::Llm
        );
        // An unmapped type decides nothing, and falls to the model fallback.
        let attrs = [
            string_attr("gen_ai.operation.name", "run"),
            string_attr("gen_ai.operation.type", "other"),
        ];
        let mut bag = AttrBag::new(&attrs, &mut Warnings::default());
        assert_eq!(
            span_kind(&mut bag, "", "openlit.instrumentation.browser_use", &[]),
            SpanKind::Span
        );
        assert_eq!(bag.state("gen_ai.operation.type"), Some(Use::Unread));
        // Another scope does not read the type.
        let attrs = [
            string_attr("gen_ai.operation.name", "run"),
            string_attr("gen_ai.operation.type", "invoke_agent"),
        ];
        assert_eq!(kind_in("browser_use", &attrs), SpanKind::Span);
    }

    #[test]
    fn openlit_vectordb_spans_are_retrievers_on_reads_only() {
        // Q1 (a), D-IN-26: only a read is a `retriever`; Chroma writes
        // `db.operation.name` in caps (`GET`, `INSERT`, `DELETE`). The keys are
        // peeked, not consumed.
        for (op, kind) in [
            ("get", SpanKind::Retriever),
            ("GET", SpanKind::Retriever),
            ("query", SpanKind::Retriever),
            ("Query", SpanKind::Retriever),
            ("search", SpanKind::Retriever),
            ("SEARCH", SpanKind::Retriever),
            ("insert", SpanKind::Span),
            ("UPSERT", SpanKind::Span),
            ("delete", SpanKind::Span),
            ("create_collection", SpanKind::Span),
        ] {
            let attrs = [
                string_attr("gen_ai.operation.name", "vectordb"),
                string_attr("db.operation.name", op),
            ];
            let mut bag = AttrBag::new(&attrs, &mut Warnings::default());
            assert_eq!(
                span_kind(&mut bag, "GET docs", "openlit.instrumentation.chroma", &[]),
                kind,
                "{op}"
            );
            assert_eq!(bag.state("gen_ai.operation.name"), Some(Use::Unread));
            assert_eq!(bag.state("db.operation.name"), Some(Use::Unread));
        }
        // A span with no `db.operation.name` is a plain span.
        let attrs = [string_attr("gen_ai.operation.name", "vectordb")];
        let mut bag = AttrBag::new(&attrs, &mut Warnings::default());
        assert_eq!(
            span_kind(
                &mut bag,
                "UPSERT unknown",
                "openlit.instrumentation.qdrant",
                &[]
            ),
            SpanKind::Span
        );
        // Another operation name still leaves the `gen_ai.operation.type` row
        // to decide (R-OL-1).
        let attrs = [
            string_attr("gen_ai.operation.name", "run"),
            string_attr("gen_ai.operation.type", "invoke_agent"),
        ];
        assert_eq!(
            kind_in("openlit.instrumentation.browser_use", &attrs),
            SpanKind::Agent
        );
        // Another scope reads neither key.
        let attrs = [
            string_attr("gen_ai.operation.name", "vectordb"),
            string_attr("db.operation.name", "get"),
        ];
        let mut bag = AttrBag::new(&attrs, &mut Warnings::default());
        assert_eq!(
            span_kind(&mut bag, "GET docs", "openlit", &[]),
            SpanKind::Span
        );
        assert_eq!(bag.state("gen_ai.operation.name"), Some(Use::Unread));
        assert_eq!(kind_in("other.scope", &attrs), SpanKind::Span);
    }

    #[test]
    fn openllmetry_vector_read_names_are_retrievers_on_their_scopes_only() {
        // R2, Q1 (a), D-IN-26. The Pinecone and Weaviate rows are unit-tested
        // only (D-IN-26 item 11).
        for (scope, name) in [
            ("opentelemetry.instrumentation.chromadb", "chroma.query"),
            ("opentelemetry.instrumentation.chromadb", "chroma.get"),
            (
                "opentelemetry.instrumentation.qdrant",
                "qdrant.query_points",
            ),
            ("opentelemetry.instrumentation.qdrant", "qdrant.search"),
            ("opentelemetry.instrumentation.qdrant", "qdrant.scroll"),
            ("opentelemetry.instrumentation.milvus", "milvus.search"),
            ("opentelemetry.instrumentation.milvus", "milvus.query"),
            ("opentelemetry.instrumentation.milvus", "milvus.get"),
            ("opentelemetry.instrumentation.pinecone", "pinecone.query"),
            ("opentelemetry.instrumentation.pinecone", "pinecone.fetch"),
            (
                "opentelemetry.instrumentation.weaviate",
                "db.weaviate.collections.query.fetch_objects",
            ),
            (
                "opentelemetry.instrumentation.weaviate",
                "db.weaviate.collections.query.near_text",
            ),
            (
                "opentelemetry.instrumentation.weaviate",
                "db.weaviate.gql.query.get.do",
            ),
        ] {
            assert_eq!(kind_named(name, scope, &[]), SpanKind::Retriever, "{name}");
        }
        // Writes and the other names decide nothing: the fallback gives
        // `span`.
        for (scope, name) in [
            ("opentelemetry.instrumentation.chromadb", "chroma.add"),
            ("opentelemetry.instrumentation.chromadb", "chroma.delete"),
            ("opentelemetry.instrumentation.qdrant", "qdrant.upsert"),
            ("opentelemetry.instrumentation.milvus", "milvus.insert"),
            ("opentelemetry.instrumentation.milvus", "milvus.upsert"),
            ("opentelemetry.instrumentation.milvus", "milvus.delete"),
            (
                "opentelemetry.instrumentation.milvus",
                "milvus.create_collection",
            ),
            ("opentelemetry.instrumentation.pinecone", "pinecone.upsert"),
            ("opentelemetry.instrumentation.pinecone", "pinecone.delete"),
            (
                "opentelemetry.instrumentation.weaviate",
                "db.weaviate.collections.get",
            ),
            (
                "opentelemetry.instrumentation.weaviate",
                "db.weaviate.collections.data.insert",
            ),
        ] {
            assert_eq!(kind_named(name, scope, &[]), SpanKind::Span, "{name}");
        }
        // A read name on another scope, and on another
        // `opentelemetry.instrumentation.*` scope, decides nothing.
        assert_eq!(
            kind_named("chroma.query", "other.scope", &[]),
            SpanKind::Span
        );
        assert_eq!(
            kind_named("chroma.query", "opentelemetry.instrumentation.openai", &[]),
            SpanKind::Span
        );
        // A Weaviate name that only starts like the read prefixes is not one.
        assert_eq!(
            kind_named(
                "db.weaviate.collections.queryx.get",
                "opentelemetry.instrumentation.weaviate",
                &[]
            ),
            SpanKind::Span
        );
    }

    #[test]
    fn promptfoo_grader_spans_are_evaluators_only_on_its_scope() {
        // R4: the grader span carries `gen_ai.evaluation.name`; the key is peeked.
        let attrs = [string_attr("gen_ai.evaluation.name", "contains")];
        let mut bag = AttrBag::new(&attrs, &mut Warnings::default());
        assert_eq!(
            span_kind(&mut bag, "", "promptfoo.providers", &[]),
            SpanKind::Evaluator
        );
        assert_eq!(bag.state("gen_ai.evaluation.name"), Some(Use::Unread));
        // Without the scope the key decides nothing.
        assert_eq!(kind_in("", &attrs), SpanKind::Span);
        // The grader's model call keeps its operation and is `llm`.
        let attrs = [
            string_attr("gen_ai.operation.name", "chat"),
            string_attr("gen_ai.request.model", "gpt-4o-mini"),
        ];
        assert_eq!(kind_in("promptfoo.providers", &attrs), SpanKind::Llm);
    }

    #[test]
    fn voltagent_span_type_and_entity_type_decide_only_on_voltagent_spans() {
        // V2 (D-IN-6): `span.type` first, `entity.type` only without it.
        let llm = [
            string_attr("span.type", "llm"),
            string_attr("entity.type", "agent"),
        ];
        let mut bag = AttrBag::new(&llm, &mut Warnings::default());
        assert_eq!(
            span_kind(&mut bag, "", "@voltagent/core", &[Convention::Voltagent]),
            SpanKind::Llm
        );
        assert_eq!(bag.state("span.type"), Some(Use::Consumed));
        assert_eq!(bag.state("entity.type"), Some(Use::Unread));

        let tool = [string_attr("span.type", "tool")];
        let mut bag = AttrBag::new(&tool, &mut Warnings::default());
        assert_eq!(
            span_kind(&mut bag, "", "@voltagent/core", &[Convention::Voltagent]),
            SpanKind::Tool
        );

        let agent = [string_attr("entity.type", "agent")];
        let mut bag = AttrBag::new(&agent, &mut Warnings::default());
        assert_eq!(
            span_kind(&mut bag, "", "@voltagent/core", &[Convention::Voltagent]),
            SpanKind::Agent
        );
        assert_eq!(bag.state("entity.type"), Some(Use::Consumed));

        // The other values the exporter writes do not decide, and `entity.type`
        // does not rescue a span that carries an unmapped `span.type`.
        for text in ["memory", "guardrail", "workflow-step", "summary"] {
            let attrs = [
                string_attr("span.type", text),
                string_attr("entity.type", "agent"),
            ];
            let mut bag = AttrBag::new(&attrs, &mut Warnings::default());
            assert_eq!(
                span_kind(&mut bag, "", "@voltagent/core", &[Convention::Voltagent]),
                SpanKind::Span,
                "{text}"
            );
            assert_eq!(bag.state("span.type"), Some(Use::Unread), "{text}");
        }

        // Without the `voltagent` convention the row does not apply.
        let attrs = [
            string_attr("span.type", "llm"),
            string_attr("entity.type", "agent"),
        ];
        let mut bag = AttrBag::new(&attrs, &mut Warnings::default());
        assert_eq!(
            span_kind(&mut bag, "", "@voltagent/core", &[]),
            SpanKind::Span
        );
        assert_eq!(bag.state("span.type"), Some(Use::Unread));
        let mut bag = AttrBag::new(&attrs, &mut Warnings::default());
        assert_eq!(
            span_kind(&mut bag, "", "", &[Convention::Openinference]),
            SpanKind::Span
        );
        assert_eq!(bag.state("entity.type"), Some(Use::Unread));

        // The operation row ahead of it still decides first.
        let attrs = [
            string_attr("gen_ai.operation.name", "chat"),
            string_attr("span.type", "tool"),
        ];
        assert_eq!(
            span_kind(
                &mut AttrBag::new(&attrs, &mut Warnings::default()),
                "",
                "@voltagent/core",
                &[Convention::Voltagent]
            ),
            SpanKind::Llm
        );
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
            assert_eq!(span_kind(&mut bag, "", "", &[]), kind, "{text}");
            assert_eq!(bag.state("lmnr.span.type"), Some(Use::Consumed));
        }
        let attrs = [string_attr("lmnr.span.type", "")];
        let mut bag = AttrBag::new(&attrs, &mut Warnings::default());
        assert_eq!(span_kind(&mut bag, "", "", &[]), SpanKind::Span);
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
                assert_eq!(
                    span_kind(&mut bag, "", "", &[]),
                    SpanKind::Llm,
                    "{text} {key}"
                );
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
                assert_eq!(
                    span_kind(&mut bag, "", "", &[]),
                    SpanKind::Llm,
                    "{text} {key}"
                );
                assert_eq!(bag.state(key), Some(Use::Unread));
            }
        }
        // Without a model or tokens: the OpenHands wrapper (D-IN-23 item 8).
        for text in ["LLM", "CACHED"] {
            let attrs = [string_attr("lmnr.span.type", text)];
            let mut bag = AttrBag::new(&attrs, &mut Warnings::default());
            assert_eq!(span_kind(&mut bag, "", "", &[]), SpanKind::Chain, "{text}");
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
        assert_eq!(span_kind(&mut bag, "", "", &[]), SpanKind::Chain);
        assert_eq!(bag.state("gen_ai.request.model"), Some(Use::Unread));
    }

    #[test]
    fn a_tool_key_wins_over_a_model_key_and_neither_is_consumed() {
        let attrs = [
            string_attr("gen_ai.tool.name", ""),
            string_attr("gen_ai.request.model", "gpt"),
        ];
        let mut bag = AttrBag::new(&attrs, &mut Warnings::default());
        assert_eq!(span_kind(&mut bag, "", "", &[]), SpanKind::Tool);
        assert_eq!(bag.state("gen_ai.tool.name"), Some(Use::Unread));
        assert_eq!(bag.state("gen_ai.request.model"), Some(Use::Unread));

        let attrs = [string_attr("gen_ai.tool.call.id", "call-1")];
        assert_eq!(kind_of(&attrs), SpanKind::Tool);

        let attrs = [string_attr("llm.model_name", "gpt")];
        let mut bag = AttrBag::new(&attrs, &mut Warnings::default());
        assert_eq!(span_kind(&mut bag, "", "", &[]), SpanKind::Llm);
        assert_eq!(bag.state("llm.model_name"), Some(Use::Unread));

        assert_eq!(kind_of(&[]), SpanKind::Span);
    }

    fn kind_named(name: &str, scope: &str, attrs: &[KeyValue]) -> SpanKind {
        let mut bag = AttrBag::new(attrs, &mut Warnings::default());
        span_kind(&mut bag, name, scope, &[])
    }

    #[test]
    fn speech_operations_are_spans_and_keep_their_operation() {
        for text in ["tts", "stt"] {
            let attrs = [
                string_attr("gen_ai.operation.name", text),
                string_attr("gen_ai.request.model", "tts-1"),
            ];
            let mut warnings = Warnings::default();
            let mut bag = AttrBag::new(&attrs, &mut warnings);
            assert_eq!(
                span_kind(&mut bag, "", "pipecat", &[]),
                SpanKind::Span,
                "{text}"
            );
            assert_eq!(bag.state("gen_ai.request.model"), Some(Use::Unread));
            // The operation column still reads the consumed key.
            let view = crate::fields::ResourceView::new(&[]);
            let fields = crate::context::fill(&mut bag, &view, "pipecat", &[], &mut warnings);
            assert_eq!(fields.operation.as_deref(), Some(text));
        }
    }

    #[test]
    fn livekit_span_names_decide_on_its_scope_only() {
        let model = [string_attr("gen_ai.request.model", "gpt-4o-mini")];
        assert_eq!(
            kind_named("llm_node", "livekit-agents", &model),
            SpanKind::Chain
        );
        assert_eq!(kind_named("llm_node", "other.scope", &model), SpanKind::Llm);

        assert_eq!(
            kind_named("llm_request_run", "livekit-agents", &model),
            SpanKind::Span
        );
        assert_eq!(
            kind_named("llm_request_run", "other.scope", &model),
            SpanKind::Llm
        );

        // `llm_request` is decided by its `chat` operation, as before.
        let chat = [string_attr("gen_ai.operation.name", "chat")];
        assert_eq!(
            kind_named("llm_request", "livekit-agents", &chat),
            SpanKind::Llm
        );

        // The setup span is a span on LiveKit's scope, and an agent elsewhere.
        let create = [string_attr("gen_ai.operation.name", "create_agent")];
        assert_eq!(
            kind_named("start_agent_activity", "livekit-agents", &create),
            SpanKind::Span
        );
        assert_eq!(
            kind_named("start_agent_activity", "other.scope", &create),
            SpanKind::Agent
        );
        let mut bag = AttrBag::new(&create, &mut Warnings::default());
        assert_eq!(
            span_kind(&mut bag, "start_agent_activity", "livekit-agents", &[]),
            SpanKind::Span
        );
        assert_eq!(bag.state("gen_ai.operation.name"), Some(Use::Unread));
    }

    #[test]
    fn pipecat_turn_and_conversation_are_chains_on_its_turn_scope() {
        for name in ["conversation", "turn"] {
            assert_eq!(
                kind_named(name, "pipecat.turn", &[]),
                SpanKind::Chain,
                "{name}"
            );
            assert_eq!(kind_named(name, "pipecat", &[]), SpanKind::Span, "{name}");
        }
    }

    #[test]
    fn promptflow_span_type_decides_on_its_scope_only() {
        for (text, kind) in [
            ("LLM", SpanKind::Llm),
            ("Embedding", SpanKind::Embedding),
            ("Retrieval", SpanKind::Retriever),
            ("Flow", SpanKind::Chain),
            ("LangChain", SpanKind::Chain),
            ("Function", SpanKind::Span),
        ] {
            let attrs = [string_attr("span_type", text)];
            assert_eq!(kind_named("flow", "promptflow", &attrs), kind, "{text}");
            assert_eq!(kind_named("flow", "other.scope", &attrs), SpanKind::Span);
        }
        // A `Function` span with a model in its inputs stays a plain span.
        let attrs = [
            string_attr("span_type", "Function"),
            string_attr("inputs", r#"{"model":"gpt-4o-mini"}"#),
        ];
        assert_eq!(kind_named("weather", "promptflow", &attrs), SpanKind::Span);

        // An unknown value decides nothing and stays unread.
        let attrs = [
            string_attr("span_type", "Other"),
            string_attr("gen_ai.request.model", "gpt-4o-mini"),
        ];
        let mut bag = AttrBag::new(&attrs, &mut Warnings::default());
        assert_eq!(
            span_kind(&mut bag, "flow", "promptflow", &[]),
            SpanKind::Llm
        );
        assert_eq!(bag.state("span_type"), Some(Use::Unread));

        let attrs = [string_attr("span_type", "LLM")];
        let mut bag = AttrBag::new(&attrs, &mut Warnings::default());
        assert_eq!(span_kind(&mut bag, "llm", "promptflow", &[]), SpanKind::Llm);
        assert_eq!(bag.state("span_type"), Some(Use::Consumed));
    }

    #[test]
    fn langflow_trace_type_and_flow_id_decide_on_its_scope_only() {
        let model = [string_attr("gen_ai.request.model", "gpt-4o-mini")];
        for (text, kind) in [
            ("agent", SpanKind::Agent),
            ("tool", SpanKind::Tool),
            ("retriever", SpanKind::Retriever),
            ("embedding", SpanKind::Embedding),
            ("llm", SpanKind::Chain),
            ("prompt", SpanKind::Chain),
            ("chain", SpanKind::Chain),
        ] {
            let mut attrs = vec![string_attr("trace_type", text)];
            attrs.extend(model.iter().cloned());
            assert_eq!(kind_named("Chat", "langflow", &attrs), kind, "{text}");
            assert_eq!(kind_named("Chat", "other.scope", &attrs), SpanKind::Llm);
        }

        // The flow root has `flow_id` and no `trace_type`.
        let root = [string_attr("flow_id", "7f3c")];
        assert_eq!(kind_named("flow", "langflow", &root), SpanKind::Chain);
        assert_eq!(kind_named("flow", "other.scope", &root), SpanKind::Span);

        // Neither key: no decision, so the model fallback applies.
        assert_eq!(kind_named("x", "langflow", &model), SpanKind::Llm);
        assert_eq!(kind_named("x", "langflow", &[]), SpanKind::Span);
    }

    #[test]
    fn n8n_rows_decide_on_its_scope_only() {
        let model = [string_attr("gen_ai.request.model", "gpt-4o-mini")];
        assert_eq!(
            kind_named("workflow.execute", "n8n-workflow", &[]),
            SpanKind::Chain
        );
        assert_eq!(
            kind_named("workflow.execute", "other.scope", &[]),
            SpanKind::Span
        );

        // A node without a GenAI operation is a plain span, even with a model.
        assert_eq!(
            kind_named("node.execute", "n8n-workflow", &[]),
            SpanKind::Span
        );
        assert_eq!(
            kind_named("node.execute", "n8n-workflow", &model),
            SpanKind::Span
        );
        assert_eq!(
            kind_named("node.execute", "other.scope", &model),
            SpanKind::Llm
        );

        // With an operation, the GenAI rules decide.
        let agent = [string_attr("gen_ai.operation.name", "invoke_agent")];
        assert_eq!(
            kind_named("node.execute", "n8n-workflow", &agent),
            SpanKind::Agent
        );
        let chat = [string_attr("gen_ai.operation.name", "chat")];
        assert_eq!(
            kind_named("node.execute", "n8n-workflow", &chat),
            SpanKind::Llm
        );
        // An operation the GenAI table does not know leaves the rule unused.
        let other = [string_attr("gen_ai.operation.name", "run")];
        assert_eq!(
            kind_named("node.execute", "n8n-workflow", &other),
            SpanKind::Span
        );
        assert_eq!(
            kind_named(
                "node.execute",
                "n8n-workflow",
                &[
                    string_attr("gen_ai.operation.name", "run"),
                    model[0].clone()
                ]
            ),
            SpanKind::Llm
        );
    }
}
