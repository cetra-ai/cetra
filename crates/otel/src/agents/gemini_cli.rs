//! Gemini CLI spans (spec 02 § Coding agents; D-CA-5, D-CA-6).
//!
//! The generic rules have already run when these apply. They decide the kind of each
//! span, the messages of an LLM call and of a tool call, and the tool's name.

use cetra_core::enums::SpanKind;
use cetra_core::span::Span;
use opentelemetry_proto::tonic::common::v1::AnyValue;
use serde_json::Value as Json;

use super::{AgentContexts, Finish, apply_session_context, rename_tool, set_input, set_output};
use crate::bag::AttrBag;
use crate::context::ContextFields;
use crate::convert::{gemini_contents, gemini_system_instruction};
use crate::payload::{Assembled, Payload, decode_json_text, non_message_payload};
use crate::warn::Warnings;

const WIRE_NAME: &str = "gemini-cli";

/// Process attributes that the resource carries and that are not kept (D-CA-6).
const PROCESS_ATTRIBUTES: [&str; 3] = ["process.command_args", "process.command", "process.owner"];

/// The span kinds of Gemini CLI's spans. Other names have no rule.
pub(crate) fn kind(span_name: &str) -> Option<SpanKind> {
    Some(match span_name {
        "llm_call" => SpanKind::Llm,
        "tool_call" => SpanKind::Tool,
        "schedule_tool_calls" => SpanKind::Chain,
        "user_prompt" | "agent_call" => SpanKind::Agent,
        _ => return None,
    })
}

/// The session context of the span's session id (D-CA-5 rank 2, D-CA-9).
pub(crate) fn context(fields: &mut ContextFields, agents: &AgentContexts) {
    apply_session_context(agents, WIRE_NAME, fields);
}

/// Messages, tool definitions and system instructions of an LLM call, and the messages of a
/// tool call. A conversion that fails leaves the generic result (D-CA-6).
pub(crate) fn payload(
    span_name: &str,
    bag: &mut AttrBag<'_>,
    assembled: &mut Assembled,
    warnings: &mut Warnings,
) {
    match span_name {
        "llm_call" => llm_call(bag, assembled, warnings),
        "tool_call" | "schedule_tool_calls" => tool_messages(bag, assembled, warnings),
        _ => {}
    }
}

fn llm_call(bag: &mut AttrBag<'_>, assembled: &mut Assembled, warnings: &mut Warnings) {
    let input = bag
        .take_payload("gen_ai.input.messages")
        .and_then(|value| decode_json_text(value, warnings));
    if let Some(messages) = input
        .as_ref()
        .and_then(|json| gemini_contents(json).ok())
        .filter(|messages| !messages.is_empty())
    {
        set_input(assembled, Payload::Messages(messages));
    }

    let output = bag
        .take_payload("gen_ai.output.messages")
        .and_then(|value| decode_json_text(value, warnings))
        .map(|json| {
            if matches!(json, Json::Object(_)) {
                Json::Array(vec![json])
            } else {
                json
            }
        });
    if let Some(messages) = output
        .as_ref()
        .and_then(|json| gemini_contents(json).ok())
        .filter(|messages| !messages.is_empty())
    {
        set_output(assembled, Payload::Messages(messages));
    }

    let definitions = bag
        .take_payload("gen_ai.tool.definitions")
        .and_then(|value| decode_json_text(value, warnings));
    if let Some(Json::Array(items)) = definitions {
        let flattened: Vec<Json> = items
            .iter()
            .filter_map(|item| item.get("functionDeclarations")?.as_array())
            .flatten()
            .cloned()
            .collect();
        if !flattened.is_empty() {
            assembled.io.tool_definitions = Some(flattened);
        }
    }

    let instructions = bag
        .take_payload("gen_ai.system_instructions")
        .and_then(|value| decode_json_text(value, warnings));
    if let Some(value) = instructions {
        assembled.io.system_instructions = Some(gemini_system_instruction(&value));
    }
}

/// A tool call's or a batch's messages are JSON when they parse, and text otherwise (rule 10).
fn tool_messages(bag: &mut AttrBag<'_>, assembled: &mut Assembled, warnings: &mut Warnings) {
    if let Some(value) = bag.take_payload("gen_ai.input.messages") {
        set_input(assembled, json_payload(value, warnings));
    }
    if let Some(value) = bag.take_payload("gen_ai.output.messages") {
        set_output(assembled, json_payload(value, warnings));
    }
}

fn json_payload(value: &AnyValue, warnings: &mut Warnings) -> Payload {
    match decode_json_text(value, warnings) {
        Some(json) => Payload::Json(json),
        None => non_message_payload(value, warnings),
    }
}

/// A tool call takes the tool's name. Every Gemini span loses its process attributes from the
/// resource (D-CA-6).
pub(crate) fn finish(span_name: &str, inputs: &Finish, built: &mut Span) {
    for name in PROCESS_ATTRIBUTES {
        built.resource_attributes.remove(name);
    }
    if span_name == "tool_call"
        && let Some(name) = inputs.tool_name.as_deref()
    {
        rename_tool(built, name);
    }
}

#[cfg(test)]
mod tests {
    use cetra_core::enums::PayloadFormat;
    use cetra_core::names::metadata_key;

    use super::*;
    use crate::agents::test_support::{kv, request, run, s, span};

    fn gemini(
        spans: Vec<opentelemetry_proto::tonic::trace::v1::Span>,
    ) -> opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest {
        request("gemini-cli", "gemini-cli", spans)
    }

    #[test]
    fn gemini_llm_call_messages_are_converted() {
        let llm = span(
            "llm_call",
            vec![
                kv(
                    "gen_ai.input.messages",
                    s(r#"[{"role":"user","parts":[{"text":"list files"}]}]"#),
                ),
                kv(
                    "gen_ai.output.messages",
                    s(
                        r#"{"role":"model","parts":[{"functionCall":{"name":"run_shell_command","args":{"command":"echo hello"}}}]}"#,
                    ),
                ),
            ],
            vec![],
        );
        let normalized = run(&gemini(vec![llm]), &AgentContexts::default());
        let span = &normalized.spans[0];
        assert_eq!(span.span_kind, SpanKind::Llm);
        assert_eq!(span.input_format, Some(PayloadFormat::Messages));
        assert_eq!(span.output_format, Some(PayloadFormat::Messages));
        let output = span.output.as_deref().unwrap_or_default();
        assert!(output.contains("tool_call"), "{output}");
        assert!(output.contains("run_shell_command"), "{output}");
        assert!(!span.attributes.contains_key(metadata_key::NORMALIZE_ERROR));
        assert!(!span.attributes.contains_key("gen_ai.input.messages"));
    }

    #[test]
    fn gemini_tool_definitions_are_flattened() {
        let llm = span(
            "llm_call",
            vec![kv(
                "gen_ai.tool.definitions",
                s(r#"[{"functionDeclarations":[{"name":"a"},{"name":"b"}]}]"#),
            )],
            vec![],
        );
        let normalized = run(&gemini(vec![llm]), &AgentContexts::default());
        assert_eq!(
            normalized.spans[0].tool_definitions.as_deref(),
            Some(r#"[{"name":"a"},{"name":"b"}]"#)
        );
    }

    #[test]
    fn gemini_tool_call_is_named_and_json() {
        let tool = span(
            "tool_call",
            vec![
                kv("gen_ai.tool.name", s("run_shell_command")),
                kv("gen_ai.input.messages", s(r#"{"command":"echo hello"}"#)),
                kv("gen_ai.output.messages", s(r#"{"output":"hello"}"#)),
            ],
            vec![],
        );
        let normalized = run(&gemini(vec![tool]), &AgentContexts::default());
        let span = &normalized.spans[0];
        assert_eq!(span.name, "run_shell_command");
        assert_eq!(span.span_kind, SpanKind::Tool);
        assert_eq!(
            span.attributes.get("cetra.span_name").map(String::as_str),
            Some("tool_call")
        );
        assert_eq!(span.input.as_deref(), Some(r#"{"command":"echo hello"}"#));
        assert_eq!(span.input_format, Some(PayloadFormat::Json));
        assert_eq!(span.output.as_deref(), Some(r#"{"output":"hello"}"#));
        assert_eq!(span.output_format, Some(PayloadFormat::Json));
    }

    #[test]
    fn gemini_conversion_failure_keeps_the_generic_result() {
        let llm_attributes = || {
            vec![kv(
                "gen_ai.input.messages",
                s(r#"{"role":"user","parts":[{"text":"not a list"}]}"#),
            )]
        };
        let agent = run(
            &gemini(vec![span("llm_call", llm_attributes(), vec![])]),
            &AgentContexts::default(),
        );
        let generic = run(
            &request("app", "", vec![span("llm_call", llm_attributes(), vec![])]),
            &AgentContexts::default(),
        );
        assert_eq!(agent.spans[0].input, generic.spans[0].input);
        assert_eq!(agent.spans[0].input_format, generic.spans[0].input_format);
    }

    #[test]
    fn gemini_tool_call_non_json_text_is_text() {
        let tool = span(
            "tool_call",
            vec![
                kv("gen_ai.tool.name", s("grep")),
                kv("gen_ai.input.messages", s("list files")),
            ],
            vec![],
        );
        let normalized = run(&gemini(vec![tool]), &AgentContexts::default());
        let span = &normalized.spans[0];
        assert_eq!(span.input.as_deref(), Some("list files"));
        assert_eq!(span.input_format, Some(PayloadFormat::Text));
    }

    #[test]
    fn gemini_process_attributes_are_dropped() {
        let mut request = gemini(vec![span("llm_call", vec![], vec![])]);
        let resource = request.resource_spans[0]
            .resource
            .as_mut()
            .expect("resource");
        resource.attributes.extend([
            kv("process.command", s("gemini")),
            kv("process.command_args", s("[]")),
            kv("process.owner", s("someone")),
            kv("host.name", s("laptop")),
        ]);
        let normalized = run(&request, &AgentContexts::default());
        let resource = &normalized.spans[0].resource_attributes;
        assert!(!resource.contains_key("process.command"));
        assert!(!resource.contains_key("process.command_args"));
        assert!(!resource.contains_key("process.owner"));
        assert!(resource.contains_key("host.name"));
    }
}
