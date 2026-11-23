//! GenAI messages, system instructions, tool definitions, and legacy events.
//!
//! Spec 02 § Input and output, the GenAI and GenAI legacy events rows, and
//! rule 16. Tool-call arguments and results are read only on tool spans.

use cetra_core::enums::SpanKind;
use cetra_core::message::{Message, Role};
use opentelemetry_proto::tonic::common::v1::AnyValue;
use opentelemetry_proto::tonic::trace::v1::span::Event;
use serde_json::{Map, Value as Json};

use super::{messages_from_value, present_json};
use crate::convert::{converse_message, converse_text, openai_messages};
use crate::payload::{
    Field, Io, KeyPattern, Payload, ReadContext, decode_json_text, non_message_payload,
    tool_definitions,
};
use crate::value::as_str;
use crate::warn::Warnings;

const INPUT_MESSAGES: &str = "gen_ai.input.messages";
const OUTPUT_MESSAGES: &str = "gen_ai.output.messages";
const SYSTEM: &str = "gen_ai.system_instructions";
const TOOLS: &str = "gen_ai.tool.definitions";
const TOOL_ARGUMENTS: &str = "gen_ai.tool.call.arguments";
const TOOL_RESULT: &str = "gen_ai.tool.call.result";
const FINISH_REASONS: &str = "gen_ai.response.finish_reasons";
const DETAILS: &str = "gen_ai.client.inference.operation.details";
const OPERATION: &str = "gen_ai.operation.name";
const CHOICE: &str = "gen_ai.choice";

const INPUT_EVENTS: &[(&str, &str)] = &[
    ("gen_ai.system.message", "system"),
    ("gen_ai.user.message", "user"),
    ("gen_ai.assistant.message", "assistant"),
    ("gen_ai.tool.message", "tool"),
];

pub(crate) const PAYLOAD_KEYS: &[(Field, KeyPattern)] = &[
    (Field::Input, KeyPattern::Exact(INPUT_MESSAGES)),
    (Field::Input, KeyPattern::Exact(TOOL_ARGUMENTS)),
    (Field::Input, KeyPattern::Event(DETAILS)),
    (Field::Input, KeyPattern::Event("gen_ai.system.message")),
    (Field::Input, KeyPattern::Event("gen_ai.user.message")),
    (Field::Input, KeyPattern::Event("gen_ai.assistant.message")),
    (Field::Input, KeyPattern::Event("gen_ai.tool.message")),
    (Field::Output, KeyPattern::Exact(OUTPUT_MESSAGES)),
    (Field::Output, KeyPattern::Exact(TOOL_RESULT)),
    (Field::Output, KeyPattern::Event(DETAILS)),
    (Field::Output, KeyPattern::Event("gen_ai.choice")),
    (Field::SystemInstructions, KeyPattern::Exact(SYSTEM)),
    (Field::SystemInstructions, KeyPattern::Event(DETAILS)),
    (Field::ToolDefinitions, KeyPattern::Exact(TOOLS)),
];

pub(crate) fn read(ctx: &mut ReadContext<'_, '_>) -> Io {
    let mut io = Io::default();
    read_messages(ctx, &mut io, INPUT_MESSAGES, "messages:genai:input", true);
    read_messages(
        ctx,
        &mut io,
        OUTPUT_MESSAGES,
        "messages:genai:output",
        false,
    );
    if io.system_instructions.is_none() {
        io.system_instructions = instruction(ctx, SYSTEM);
    }
    io.tool_definitions = definitions(ctx);
    fill_finish_reasons(ctx, &mut io);
    io.events = payload_events(ctx);
    io
}

fn read_messages(
    ctx: &mut ReadContext<'_, '_>,
    io: &mut Io,
    key: &str,
    error: &'static str,
    input: bool,
) {
    let found = if let Some(found) = span_messages(ctx, key) {
        Some(found)
    } else if let Some(found) = details_messages(ctx, key) {
        Some(found)
    } else if let Some(found) = framework_payload(ctx, input) {
        Some(found)
    } else if let Some(found) = legacy_messages(ctx, input) {
        Some(found)
    } else if ctx.kind == SpanKind::Tool {
        tool_payload(ctx, input)
    } else {
        None
    };
    let Some((payload, failed)) = found else {
        return;
    };
    if input {
        io.input = Some(payload);
    } else {
        io.output = Some(payload);
    }
    if failed {
        io.errors.push(error);
    }
}

fn span_messages(ctx: &mut ReadContext<'_, '_>, key: &str) -> Option<(Payload, bool)> {
    if as_str(ctx.bag.get(key)?) == Some("") {
        return None;
    }
    let value = ctx.bag.take_payload(key)?;
    Some(genai_messages(value, ctx.warnings))
}

fn details_messages(ctx: &mut ReadContext<'_, '_>, key: &str) -> Option<(Payload, bool)> {
    let value = details_attr(ctx.events, key)?.clone();
    Some(genai_messages(&value, ctx.warnings))
}

/// Canonical messages, or OpenAI chat messages when no element has `parts`.
///
/// Laminar writes OpenAI request messages and response `choices` under the
/// GenAI keys (D-B1-3). A choice is `{message, finish_reason}`.
fn genai_messages(value: &AnyValue, warnings: &mut Warnings) -> (Payload, bool) {
    if let Some(json) = decode_json_text(value, warnings)
        && let Some(items) = openai_shaped(&json)
    {
        let mut reasons = Vec::with_capacity(items.len());
        let mut raw = Vec::with_capacity(items.len());
        for item in items {
            match item.get("message") {
                Some(Json::Object(message)) => {
                    // A response message lists every optional field, as null.
                    let present = message
                        .iter()
                        .filter(|(_, value)| !value.is_null())
                        .map(|(key, value)| (key.clone(), value.clone()));
                    raw.push(Json::Object(present.collect()));
                    reasons.push(item.get("finish_reason").and_then(Json::as_str));
                }
                _ => {
                    raw.push(item.clone());
                    reasons.push(None);
                }
            }
        }
        if let Ok(mut messages) = openai_messages(&Json::Array(raw)) {
            for (message, reason) in messages.iter_mut().zip(reasons) {
                if let Some(reason) = reason {
                    message.finish_reason = Some(reason.to_owned());
                }
            }
            return (Payload::Messages(messages), false);
        }
    }
    messages_from_value(value, warnings)
}

/// The elements of a non-empty array of objects where none has `parts` and
/// each has a string `role` or an object `message`.
fn openai_shaped(json: &Json) -> Option<&Vec<Json>> {
    let items = json.as_array()?;
    let shaped = !items.is_empty()
        && items.iter().all(|item| {
            item.as_object().is_some_and(|object| {
                !object.contains_key("parts")
                    && (object.get("role").is_some_and(Json::is_string)
                        || object.get("message").is_some_and(Json::is_object))
            })
        });
    shaped.then_some(items)
}

fn instruction(ctx: &mut ReadContext<'_, '_>, key: &str) -> Option<Json> {
    if let Some(value) = ctx.bag.get(key)
        && as_str(value) != Some("")
    {
        let value = value.clone();
        ctx.bag.take_payload(key);
        return present_json(&value, ctx.warnings);
    }
    let value = details_attr(ctx.events, key)?.clone();
    present_json(&value, ctx.warnings)
}

fn definitions(ctx: &mut ReadContext<'_, '_>) -> Option<Vec<Json>> {
    let value = ctx.bag.get(TOOLS)?.clone();
    let Json::Array(items) = present_json(&value, ctx.warnings)? else {
        return None;
    };
    let items = tool_definitions(items);
    if items.is_empty() {
        None
    } else {
        ctx.bag.take_payload(TOOLS);
        Some(items)
    }
}

fn tool_payload(ctx: &mut ReadContext<'_, '_>, input: bool) -> Option<(Payload, bool)> {
    let key = if input { TOOL_ARGUMENTS } else { TOOL_RESULT };
    if as_str(ctx.bag.get(key)?) == Some("") {
        return None;
    }
    let value = ctx.bag.take_payload(key)?;
    Some((non_message_payload(value, ctx.warnings), false))
}

/// R1, R10 and M2: framework-specific input and output attributes, after the
/// GenAI sources and before the legacy events. Each rule is gated, and marks
/// its key consumed only when it fires.
fn framework_payload(ctx: &mut ReadContext<'_, '_>, input: bool) -> Option<(Payload, bool)> {
    // R1: Pydantic AI's agent span reads its whole conversation and its result.
    if ctx.scope == "pydantic-ai" && ctx.bag.get(OPERATION).and_then(as_str) == Some("invoke_agent")
    {
        let key = if input {
            "pydantic_ai.all_messages"
        } else {
            "final_result"
        };
        return attribute_payload(ctx, key, input);
    }
    // R10: Mirascope's `@ops.trace` spans.
    if ctx.scope.starts_with("mirascope") {
        let key = if input {
            "mirascope.trace.arg_values"
        } else {
            "mirascope.trace.output"
        };
        return attribute_payload(ctx, key, false);
    }
    // M2: Mastra's typed spans, `mastra.<T>.input` / `mastra.<T>.output`.
    let span_type = ctx.bag.get("mastra.span.type").and_then(as_str)?;
    if span_type.is_empty()
        || !span_type
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte == b'_')
    {
        return None;
    }
    let key = format!(
        "mastra.{span_type}.{}",
        if input { "input" } else { "output" }
    );
    attribute_payload(ctx, &key, false)
}

/// One attribute as a payload: canonical messages when `messages`, else rule 10.
/// An empty string is absent.
fn attribute_payload(
    ctx: &mut ReadContext<'_, '_>,
    key: &str,
    messages: bool,
) -> Option<(Payload, bool)> {
    if as_str(ctx.bag.get(key)?) == Some("") {
        return None;
    }
    let value = ctx.bag.take_payload(key)?.clone();
    Some(if messages {
        genai_messages(&value, ctx.warnings)
    } else {
        (non_message_payload(&value, ctx.warnings), false)
    })
}

fn legacy_messages(ctx: &mut ReadContext<'_, '_>, input: bool) -> Option<(Payload, bool)> {
    if ctx.scope.starts_with("strands") {
        return strands_messages(ctx, input);
    }
    generic_legacy_messages(ctx.events, ctx.warnings, input)
}

/// Strands' default (legacy) events, which carry Bedrock Converse blocks. The
/// span's operation places each event (amendment K); an event the table does
/// not place on the span is not read, and the generic path does not run.
fn strands_messages(ctx: &mut ReadContext<'_, '_>, input: bool) -> Option<(Payload, bool)> {
    let events = ctx.events;
    match ctx.bag.get(OPERATION).and_then(as_str) {
        Some("chat" | "execute_event_loop_cycle") => {
            strands_conversation(events, ctx.warnings, input)
        }
        Some("execute_tool") => strands_tool(events, ctx.warnings, input),
        Some("invoke_agent") => strands_agent(events, input),
        _ => None,
    }
}

/// Rows 1 to 3: the conversation events of `chat` and cycle spans. `content`
/// (or the choice's `message`) is an array of Converse blocks; a block none of
/// the converter's three keys matches follows the failure path, as does
/// content that is not such an array.
fn strands_conversation(
    events: &[Event],
    warnings: &mut Warnings,
    input: bool,
) -> Option<(Payload, bool)> {
    let relevant: Vec<(&Event, &str)> = events
        .iter()
        .filter_map(|event| {
            let role = if input {
                event_role(event.name.as_str())
            } else if event.name == CHOICE {
                Some("assistant")
            } else {
                None
            };
            role.map(|role| (event, role))
        })
        .collect();
    if relevant.is_empty() {
        return None;
    }
    // The raw value the failure path stores.
    let raw = event_objects(&relevant, warnings);
    let mut messages = Vec::with_capacity(relevant.len());
    for (event, role) in relevant {
        let key = if input { "content" } else { "message" };
        let Some(decoded) =
            event_attr(event, key).and_then(|value| decode_json_text(value, warnings))
        else {
            return Some((Payload::Json(raw), true));
        };
        let Some(blocks) = decoded.as_array() else {
            return Some((Payload::Json(raw), true));
        };
        let Ok(mut message) = converse_message(Role::from(role), blocks) else {
            return Some((Payload::Json(raw), true));
        };
        if !input && let Some(reason) = event_attr(event, "finish_reason").and_then(as_str) {
            message.finish_reason = Some(reason.to_owned());
        }
        messages.push(message);
    }
    Some((Payload::Messages(messages), false))
}

/// Rows 4 and 5: an `execute_tool` span reads its arguments from
/// `gen_ai.tool.message` and the joined text of its choice's blocks.
fn strands_tool(events: &[Event], warnings: &mut Warnings, input: bool) -> Option<(Payload, bool)> {
    if input {
        let value = find_event(events, "gen_ai.tool.message")
            .and_then(|event| event_attr(event, "content"))?;
        return Some((non_message_payload(value, warnings), false));
    }
    let blocks = choice_blocks(events, warnings)?;
    let text = converse_text(&blocks)?;
    Some((Payload::Text(text), false))
}

/// Row 6: an `invoke_agent` span reads its choice's plain-string message as
/// its output, as is.
fn strands_agent(events: &[Event], input: bool) -> Option<(Payload, bool)> {
    if input {
        return None;
    }
    let text = find_event(events, CHOICE)
        .and_then(|event| event_attr(event, "message"))
        .and_then(as_str)?;
    Some((Payload::Text(text.to_owned()), false))
}

/// The `message` blocks of the first `gen_ai.choice` event, decoded.
fn choice_blocks(events: &[Event], warnings: &mut Warnings) -> Option<Vec<Json>> {
    let value = find_event(events, CHOICE).and_then(|event| event_attr(event, "message"))?;
    decode_json_text(value, warnings)?.as_array().cloned()
}

/// The events as JSON objects, the raw value the failure path stores.
fn event_objects(events: &[(&Event, &str)], warnings: &mut Warnings) -> Json {
    Json::Array(
        events
            .iter()
            .map(|(event, role)| event_message(event, role, warnings))
            .collect(),
    )
}

fn find_event<'a>(events: &'a [Event], name: &str) -> Option<&'a Event> {
    events.iter().find(|event| event.name == name)
}

fn event_attr<'a>(event: &'a Event, key: &str) -> Option<&'a AnyValue> {
    event
        .attributes
        .iter()
        .find(|attribute| attribute.key == key)?
        .value
        .as_ref()
}

/// The generic legacy events, for every other emitter (Logfire bridges).
fn generic_legacy_messages(
    events: &[Event],
    warnings: &mut Warnings,
    input: bool,
) -> Option<(Payload, bool)> {
    let mut messages = Vec::new();
    for event in events {
        let Some(role) = legacy_role(event.name.as_str(), input) else {
            continue;
        };
        messages.push(event_message(event, role, warnings));
    }
    if messages.is_empty() {
        return None;
    }
    let raw = Json::Array(messages);
    match openai_messages(&raw) {
        Ok(converted) => Some((Payload::Messages(converted), false)),
        Err(_) => Some((Payload::Json(raw), true)),
    }
}

fn legacy_role(name: &str, input: bool) -> Option<&'static str> {
    if input {
        event_role(name)
    } else if name == CHOICE {
        Some("assistant")
    } else {
        None
    }
}

fn event_role(name: &str) -> Option<&'static str> {
    INPUT_EVENTS
        .iter()
        .find(|(event, _)| *event == name)
        .map(|(_, role)| *role)
}

fn event_message(event: &Event, role: &str, warnings: &mut Warnings) -> Json {
    let mut object = Map::new();
    for attribute in &event.attributes {
        let Some(value) = attribute.value.as_ref() else {
            continue;
        };
        if let Some(json) = present_json(value, warnings) {
            object.insert(attribute.key.clone(), json);
        }
    }
    if object.get("role").and_then(Json::as_str).is_none() {
        object.insert("role".to_owned(), Json::String(role.to_owned()));
    }
    Json::Object(object)
}

fn fill_finish_reasons(ctx: &mut ReadContext<'_, '_>, io: &mut Io) {
    if !matches!(io.output, Some(Payload::Messages(_))) {
        return;
    }
    let Some(value) = ctx.bag.get(FINISH_REASONS).cloned() else {
        return;
    };
    let Some(reasons) = reason_list(&value, ctx.warnings) else {
        return;
    };
    ctx.bag.take(FINISH_REASONS);
    let Some(Payload::Messages(messages)) = io.output.as_mut() else {
        return;
    };
    apply_reasons(messages, &reasons);
}

fn apply_reasons(messages: &mut [Message], reasons: &[Option<String>]) {
    for (message, reason) in messages.iter_mut().zip(reasons) {
        if message.finish_reason.is_none() {
            message.finish_reason.clone_from(reason);
        }
    }
}

fn reason_list(value: &AnyValue, warnings: &mut Warnings) -> Option<Vec<Option<String>>> {
    let Json::Array(items) = present_json(value, warnings)? else {
        return None;
    };
    Some(
        items
            .into_iter()
            .map(|item| match item {
                Json::String(text) if !text.is_empty() => Some(text),
                _ => None,
            })
            .collect(),
    )
}

/// The events consumed as payload sources: the details events and the legacy
/// events. On a Strands span those are the ones the table places on its
/// operation (amendment K); an event it does not place stays in `events`.
fn payload_events(ctx: &ReadContext<'_, '_>) -> Vec<usize> {
    let operation = ctx
        .scope
        .starts_with("strands")
        .then(|| ctx.bag.get(OPERATION).and_then(as_str))
        .flatten();
    ctx.events
        .iter()
        .enumerate()
        .filter(|(_, event)| {
            event.name == DETAILS
                || match operation {
                    Some(operation) => strands_places(operation, &event.name),
                    None => event.name == CHOICE || event_role(event.name.as_str()).is_some(),
                }
        })
        .map(|(index, _)| index)
        .collect()
}

/// Whether the table places the legacy event `name` on a Strands span of this
/// `operation`.
fn strands_places(operation: &str, name: &str) -> bool {
    match operation {
        "chat" | "execute_event_loop_cycle" => event_role(name).is_some() || name == CHOICE,
        "execute_tool" => name == "gen_ai.tool.message" || name == CHOICE,
        "invoke_agent" => name == CHOICE,
        _ => false,
    }
}

/// R5: each key is read from the first `gen_ai.client.inference.operation.details`
/// event that carries it, not from the first event of that name.
fn details_attr<'a>(events: &'a [Event], key: &str) -> Option<&'a AnyValue> {
    events
        .iter()
        .filter(|event| event.name == DETAILS)
        .find_map(|event| {
            event
                .attributes
                .iter()
                .find(|attribute| attribute.key == key)?
                .value
                .as_ref()
        })
}

#[cfg(test)]
mod tests {
    use super::read;
    use crate::bag::AttrBag;
    use crate::payload::{Io, Payload, ReadContext};
    use crate::warn::Warnings;
    use cetra_core::enums::SpanKind;
    use cetra_core::message::messages_to_json;
    use opentelemetry_proto::tonic::common::v1::any_value::Value;
    use opentelemetry_proto::tonic::common::v1::{AnyValue, KeyValue};
    use opentelemetry_proto::tonic::trace::v1::span::Event;
    use serde_json::json;

    fn string_attr(key: &str, text: &str) -> KeyValue {
        KeyValue {
            key: key.to_owned(),
            value: Some(AnyValue {
                value: Some(Value::StringValue(text.to_owned())),
            }),
            key_strindex: 0,
        }
    }

    fn event(name: &str, attrs: &[(&str, &str)]) -> Event {
        Event {
            name: name.to_owned(),
            attributes: attrs
                .iter()
                .map(|(key, value)| string_attr(key, value))
                .collect(),
            ..Event::default()
        }
    }

    fn read_span(attrs: &[KeyValue], events: &[Event], kind: SpanKind) -> Io {
        read_in_scope("", attrs, events, kind)
    }

    fn read_in_scope(scope: &str, attrs: &[KeyValue], events: &[Event], kind: SpanKind) -> Io {
        let mut warnings = Warnings::default();
        let mut bag = AttrBag::new(attrs, &mut warnings);
        read(&mut ReadContext {
            bag: &mut bag,
            events,
            kind,
            scope,
            conventions: &[],
            warnings: &mut warnings,
        })
    }

    const TRACER: &str = "strands.telemetry.tracer";

    #[test]
    fn details_events_read_each_key_from_the_first_event_that_carries_it() {
        let events = [
            event(
                "gen_ai.client.inference.operation.details",
                &[(
                    "gen_ai.system_instructions",
                    r#"[{"type":"text","content":"be brief"}]"#,
                )],
            ),
            event(
                "gen_ai.client.inference.operation.details",
                &[(
                    "gen_ai.input.messages",
                    r#"[{"role":"user","parts":[{"type":"text","content":"second"}]}]"#,
                )],
            ),
            event(
                "gen_ai.client.inference.operation.details",
                &[(
                    "gen_ai.input.messages",
                    r#"[{"role":"user","parts":[{"type":"text","content":"third"}]}]"#,
                )],
            ),
        ];
        let io = read_span(&[], &events, SpanKind::Llm);
        let Payload::Messages(messages) = io.input.expect("input") else {
            panic!("expected messages");
        };
        assert_eq!(
            messages_to_json(&messages),
            r#"[{"role":"user","parts":[{"type":"text","content":"second"}]}]"#
        );
        assert_eq!(
            io.system_instructions,
            Some(json!([{"type": "text", "content": "be brief"}]))
        );
    }

    #[test]
    fn pydantic_ai_agent_spans_read_all_messages_and_final_result() {
        let attrs = [
            string_attr("gen_ai.operation.name", "invoke_agent"),
            string_attr(
                "pydantic_ai.all_messages",
                r#"[{"role":"user","parts":[{"type":"text","content":"hi"}]},{"role":"tool","parts":[{"type":"tool_call_response","id":"call_1","name":"get_weather","result":"18C"}]}]"#,
            ),
            string_attr("final_result", "It is sunny."),
        ];
        let io = read_in_scope("pydantic-ai", &attrs, &[], SpanKind::Agent);
        let Payload::Messages(messages) = io.input.expect("input") else {
            panic!("expected messages");
        };
        assert_eq!(
            messages_to_json(&messages),
            r#"[{"role":"user","parts":[{"type":"text","content":"hi"}]},{"role":"tool","parts":[{"type":"tool_call_response","id":"call_1","response":"18C","name":"get_weather"}]}]"#
        );
        assert_eq!(io.output, Some(Payload::Text("It is sunny.".to_owned())));
        // Another scope, or another operation, leaves both keys unread.
        let io = read_span(&attrs, &[], SpanKind::Agent);
        assert!(io.input.is_none());
        assert!(io.output.is_none());
        let io = read_in_scope(
            "pydantic-ai",
            &[
                string_attr("gen_ai.operation.name", "chat"),
                string_attr("final_result", "It is sunny."),
            ],
            &[],
            SpanKind::Llm,
        );
        assert!(io.output.is_none());
    }

    #[test]
    fn pydantic_ai_agent_input_comes_after_genai_messages() {
        let attrs = [
            string_attr("gen_ai.operation.name", "invoke_agent"),
            string_attr(
                "gen_ai.input.messages",
                r#"[{"role":"user","parts":[{"type":"text","content":"the genai one"}]}]"#,
            ),
            string_attr(
                "pydantic_ai.all_messages",
                r#"[{"role":"user","parts":[{"type":"text","content":"the pydantic one"}]}]"#,
            ),
        ];
        let io = read_in_scope("pydantic-ai", &attrs, &[], SpanKind::Agent);
        let Payload::Messages(messages) = io.input.expect("input") else {
            panic!("expected messages");
        };
        assert_eq!(
            messages_to_json(&messages),
            r#"[{"role":"user","parts":[{"type":"text","content":"the genai one"}]}]"#
        );
    }

    #[test]
    fn mirascope_trace_spans_read_their_args_and_output() {
        let attrs = [
            string_attr(
                "mirascope.trace.arg_values",
                r#"{"q":"What is the weather?"}"#,
            ),
            string_attr("mirascope.trace.output", "It is sunny."),
        ];
        let io = read_in_scope("mirascope.ops", &attrs, &[], SpanKind::Chain);
        assert_eq!(
            io.input,
            Some(Payload::Json(json!({"q": "What is the weather?"})))
        );
        assert_eq!(io.output, Some(Payload::Text("It is sunny.".to_owned())));
        let io = read_span(&attrs, &[], SpanKind::Chain);
        assert!(io.input.is_none());
        assert!(io.output.is_none());
    }

    #[test]
    fn mastra_span_types_read_their_input_and_output() {
        let attrs = [
            string_attr("mastra.span.type", "agent_run"),
            string_attr("mastra.agent_run.input", "What is the weather?"),
            string_attr("mastra.agent_run.output", r#"{"text":"It is sunny."}"#),
        ];
        let io = read_span(&attrs, &[], SpanKind::Agent);
        assert_eq!(
            io.input,
            Some(Payload::Text("What is the weather?".to_owned()))
        );
        assert_eq!(
            io.output,
            Some(Payload::Json(json!({"text": "It is sunny."})))
        );
        let attrs = [
            string_attr("mastra.span.type", "workflow_step"),
            string_attr("mastra.workflow_step.input", r#"{"n":1}"#),
            string_attr("mastra.workflow_step.output", r#"{"n":2}"#),
        ];
        let io = read_span(&attrs, &[], SpanKind::Chain);
        assert_eq!(io.input, Some(Payload::Json(json!({"n": 1}))));
        assert_eq!(io.output, Some(Payload::Json(json!({"n": 2}))));
        // A type outside `[a-z_]+` decides nothing.
        let attrs = [
            string_attr("mastra.span.type", "Model-Step"),
            string_attr("mastra.Model-Step.input", "not read"),
        ];
        let io = read_span(&attrs, &[], SpanKind::Chain);
        assert!(io.input.is_none());
    }

    #[test]
    fn strands_chat_events_become_converse_messages() {
        let attrs = [string_attr("gen_ai.operation.name", "chat")];
        let events = [
            event(
                "gen_ai.system.message",
                &[("content", r#"[{"text": "You are a weather bot."}]"#)],
            ),
            event(
                "gen_ai.user.message",
                &[("content", r#"[{"text": "What is the weather in Paris?"}]"#)],
            ),
            event(
                "gen_ai.assistant.message",
                &[(
                    "content",
                    r#"[{"toolUse": {"toolUseId": "call_1", "name": "get_weather", "input": {"city": "Paris"}}}]"#,
                )],
            ),
            event(
                "gen_ai.tool.message",
                &[(
                    "content",
                    r#"[{"toolResult": {"toolUseId": "call_1", "status": "success", "content": [{"text": "18C sunny in Paris"}]}}]"#,
                )],
            ),
            event(
                "gen_ai.choice",
                &[
                    ("finish_reason", "end_turn"),
                    ("message", r#"[{"text": "It is 18C and sunny in Paris."}]"#),
                ],
            ),
        ];
        let io = read_in_scope(TRACER, &attrs, &events, SpanKind::Llm);
        let Payload::Messages(messages) = io.input.expect("input") else {
            panic!("expected messages");
        };
        assert_eq!(
            messages_to_json(&messages),
            concat!(
                r#"[{"role":"system","parts":[{"type":"text","content":"You are a weather bot."}]},"#,
                r#"{"role":"user","parts":[{"type":"text","content":"What is the weather in Paris?"}]},"#,
                r#"{"role":"assistant","parts":[{"type":"tool_call","id":"call_1","name":"get_weather","arguments":{"city":"Paris"}}]},"#,
                r#"{"role":"tool","parts":[{"type":"tool_call_response","id":"call_1","response":"18C sunny in Paris","status":"success"}]}]"#
            )
        );
        let Payload::Messages(messages) = io.output.expect("output") else {
            panic!("expected messages");
        };
        assert_eq!(
            messages_to_json(&messages),
            r#"[{"role":"assistant","parts":[{"type":"text","content":"It is 18C and sunny in Paris."}],"finish_reason":"end_turn"}]"#
        );
        assert!(io.errors.is_empty());
        assert_eq!(io.events, [0, 1, 2, 3, 4]);
    }

    #[test]
    fn strands_execute_tool_reads_its_arguments_and_result() {
        let attrs = [string_attr("gen_ai.operation.name", "execute_tool")];
        let events = [
            event(
                "gen_ai.tool.message",
                &[
                    ("content", r#"{"city": "Paris"}"#),
                    ("id", "call_1"),
                    ("role", "tool"),
                ],
            ),
            event(
                "gen_ai.choice",
                &[
                    ("id", "call_1"),
                    ("message", r#"[{"text": "18C sunny in Paris"}]"#),
                ],
            ),
        ];
        let io = read_in_scope(TRACER, &attrs, &events, SpanKind::Tool);
        assert_eq!(io.input, Some(Payload::Json(json!({"city": "Paris"}))));
        assert_eq!(
            io.output,
            Some(Payload::Text("18C sunny in Paris".to_owned()))
        );
        assert!(io.errors.is_empty());
    }

    #[test]
    fn strands_invoke_agent_reads_the_choice_text_as_its_output() {
        let attrs = [string_attr("gen_ai.operation.name", "invoke_agent")];
        let events = [
            event(
                "gen_ai.system.message",
                &[("content", r#"[{"text": "You are a weather bot."}]"#)],
            ),
            event(
                "gen_ai.user.message",
                &[("content", r#"[{"text": "What is the weather in Paris?"}]"#)],
            ),
            event(
                "gen_ai.choice",
                &[
                    ("finish_reason", "end_turn"),
                    ("message", "It is 18C and sunny in Paris. "),
                ],
            ),
        ];
        let io = read_in_scope(TRACER, &attrs, &events, SpanKind::Agent);
        // The table places the message events of an agent span nowhere: its
        // input stays to the higher sources, which legacy mode has none of,
        // and those events stay in `events` (only the choice is consumed).
        assert!(io.input.is_none());
        assert_eq!(
            io.output,
            Some(Payload::Text("It is 18C and sunny in Paris. ".to_owned()))
        );
        assert!(io.errors.is_empty());
        assert_eq!(io.events, [2]);
    }

    #[test]
    fn an_unknown_converse_block_follows_the_failure_path() {
        let attrs = [string_attr("gen_ai.operation.name", "chat")];
        let events = [
            event("gen_ai.user.message", &[("content", r#"[{"text": "hi"}]"#)]),
            event("gen_ai.choice", &[("message", r#"[{"image": {}}]"#)]),
        ];
        let io = read_in_scope(TRACER, &attrs, &events, SpanKind::Llm);
        assert_eq!(io.errors, ["messages:genai:output"]);
        assert_eq!(
            io.output,
            Some(Payload::Json(json!([{
                "message": [{"image": {}}],
                "role": "assistant"
            }])))
        );
    }

    #[test]
    fn a_non_strands_scope_keeps_the_generic_legacy_path() {
        let attrs = [string_attr("gen_ai.operation.name", "chat")];
        let events = [event(
            "gen_ai.user.message",
            &[("content", r#"[{"text": "hi"}]"#)],
        )];
        let io = read_span(&attrs, &events, SpanKind::Llm);
        let Payload::Messages(messages) = io.input.expect("input") else {
            panic!("expected messages");
        };
        assert_eq!(
            messages_to_json(&messages),
            r#"[{"role":"user","parts":[{"text":"hi"}]}]"#
        );
    }

    #[test]
    fn finish_reasons_fill_only_missing_ones() {
        let messages = r#"[{"role":"assistant","parts":[{"type":"text","content":"one"}],"finish_reason":"stop"},{"role":"assistant","parts":[{"type":"text","content":"two"}]}]"#;
        let io = read_span(
            &[
                string_attr("gen_ai.output.messages", messages),
                string_attr(
                    "gen_ai.response.finish_reasons",
                    r#"["length","tool_calls"]"#,
                ),
            ],
            &[],
            SpanKind::Llm,
        );
        let Payload::Messages(messages) = io.output.expect("output") else {
            panic!("expected messages");
        };
        assert_eq!(
            messages_to_json(&messages),
            concat!(
                r#"[{"role":"assistant","parts":[{"type":"text","content":"one"}],"finish_reason":"stop"},"#,
                r#"{"role":"assistant","parts":[{"type":"text","content":"two"}],"finish_reason":"tool_calls"}]"#
            )
        );
    }

    #[test]
    fn tool_definitions_parse_json_strings() {
        let io = read_span(
            &[string_attr(
                "gen_ai.tool.definitions",
                r#"["{\"name\":\"lookup\"}",{"name":"search"}]"#,
            )],
            &[],
            SpanKind::Llm,
        );
        assert_eq!(
            io.tool_definitions,
            Some(vec![json!({"name": "lookup"}), json!({"name": "search"})])
        );
    }
}
