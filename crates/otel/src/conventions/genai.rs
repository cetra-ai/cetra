//! GenAI messages, system instructions, tool definitions, and legacy events.
//!
//! Spec 02 § Input and output, the GenAI and GenAI legacy events rows, and
//! rule 16. Tool-call arguments and results are read only on tool spans.

use cetra_core::enums::SpanKind;
use cetra_core::message::{Message, Part, Role};
use opentelemetry_proto::tonic::common::v1::AnyValue;
use opentelemetry_proto::tonic::trace::v1::span::Event;
use serde_json::{Map, Value as Json};

use super::{messages_from_value, present_json};
use crate::convert::{converse_message, converse_text, openai_messages};
use crate::payload::{
    Field, Io, KeyPattern, Payload, ReadContext, decode_json_text, non_message_payload,
    parse_tool_arguments, tool_definitions,
};
use crate::value::{as_str, to_json};
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
const LITELLM_CALL_NAME: &str = "gen_ai.completion.0.function_call.name";
const LITELLM_CALL_ARGUMENTS: &str = "gen_ai.completion.0.function_call.arguments";

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
    let mut legacy = Vec::new();
    read_messages(
        ctx,
        &mut io,
        INPUT_MESSAGES,
        "messages:genai:input",
        true,
        &mut legacy,
    );
    read_messages(
        ctx,
        &mut io,
        OUTPUT_MESSAGES,
        "messages:genai:output",
        false,
        &mut legacy,
    );
    litellm_tool_call(ctx, &mut io);
    if io.system_instructions.is_none() {
        io.system_instructions = instruction(ctx, SYSTEM);
    }
    io.tool_definitions = definitions(ctx);
    fill_finish_reasons(ctx, &mut io);
    io.events = payload_events(ctx, &legacy);
    io
}

/// LiteLLM writes a tool-calling turn's output as one assistant message with
/// no parts, and the call only as `gen_ai.completion.0.function_call.{name,
/// arguments}` (D-IN-25). When the output is exactly that message and the
/// name is a non-empty string, the call becomes one `tool_call` part of it.
///
/// The keys fall under OpenLLMetry's `gen_ai.completion.` prefix, but every
/// reader runs before `drop_lower_payloads`, so they are still in the bag
/// here. This function marks them itself (`take_payload` ignores a missing
/// key), so they never reach `attributes` whatever the detected conventions
/// are; that is why GenAI's `PAYLOAD_KEYS` needs no entry for them.
fn litellm_tool_call(ctx: &mut ReadContext<'_, '_>, io: &mut Io) {
    let empty_assistant = matches!(
        io.output.as_ref(),
        Some(Payload::Messages(messages))
            if messages.len() == 1
                && messages[0].role == Role::Assistant
                && messages[0].parts.is_empty()
    );
    if !empty_assistant {
        return;
    }
    let Some(name) = ctx
        .bag
        .get(LITELLM_CALL_NAME)
        .and_then(as_str)
        .filter(|name| !name.is_empty())
    else {
        return;
    };
    let name = name.to_owned();
    let arguments = ctx
        .bag
        .get(LITELLM_CALL_ARGUMENTS)
        .and_then(as_str)
        .map(|text| parse_tool_arguments(Json::String(text.to_owned())));
    ctx.bag.take_payload(LITELLM_CALL_NAME);
    ctx.bag.take_payload(LITELLM_CALL_ARGUMENTS);
    let Some(Payload::Messages(messages)) = io.output.as_mut() else {
        return;
    };
    messages[0].parts.push(Part::ToolCall {
        id: None,
        name,
        arguments,
        extra: Map::new(),
    });
}

/// `legacy` collects the indices of the legacy events a reader read (Strands
/// spans only; see [`payload_events`]).
fn read_messages(
    ctx: &mut ReadContext<'_, '_>,
    io: &mut Io,
    key: &str,
    error: &'static str,
    input: bool,
    legacy: &mut Vec<usize>,
) {
    let found = if let Some(found) = span_messages(ctx, key) {
        Some(found)
    } else if let Some(found) = details_messages(ctx, key) {
        Some(found)
    } else if let Some(found) = framework_payload(ctx, input) {
        Some(found)
    } else if let Some(found) = legacy_messages(ctx, input, legacy) {
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
    // D-IN-25 item 11: Bifrost's server root span repeats its `chat` child's
    // content as a bare string under the message keys: the last user text, a
    // tool result or the answer. That scope decodes the value once, in
    // [`bifrost_payload`].
    if ctx.scope != "bifrost" {
        return Some(genai_messages(value, ctx.warnings));
    }
    Some(bifrost_payload(value, ctx.warnings))
}

/// D-IN-25 item 11: on scope `bifrost`, only a JSON array whose every element
/// is an object is messages; any other value is the field's plain value
/// (rule 10), and nothing failed. The value is decoded once, and the decoded
/// value is what the plain branch stores.
fn bifrost_payload(value: &AnyValue, warnings: &mut Warnings) -> (Payload, bool) {
    let decoded = decode_json_text(value, warnings);
    let messages = decoded.as_ref().is_some_and(|json| {
        json.as_array()
            .is_some_and(|items| items.iter().all(Json::is_object))
    });
    if messages {
        return genai_messages(value, warnings);
    }
    (plain_payload(value, decoded, warnings), false)
}

/// Rule 10 from a value decoded once: an object or array is `json`; a JSON
/// scalar came from a string, which is the text it was sent as; a value that
/// does not decode is that text, or its rule 1 encoding when it is not a
/// string at all.
fn plain_payload(value: &AnyValue, decoded: Option<Json>, warnings: &mut Warnings) -> Payload {
    match decoded {
        Some(json) if json.is_object() || json.is_array() => Payload::Json(json),
        _ => match as_str(value) {
            Some(text) => Payload::Text(text.to_owned()),
            None => Payload::Json(to_json(value, warnings)),
        },
    }
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

fn legacy_messages(
    ctx: &mut ReadContext<'_, '_>,
    input: bool,
    legacy: &mut Vec<usize>,
) -> Option<(Payload, bool)> {
    if ctx.scope.starts_with("strands") {
        return strands_messages(ctx, input, legacy);
    }
    generic_legacy_messages(ctx.events, ctx.warnings, input)
}

/// Strands' default (legacy) events, which carry Bedrock Converse blocks. The
/// span's operation picks the reader (amendment K). Each reader pushes the
/// indices of the events it read onto `legacy`; an event no reader reads stays
/// in the span's events.
fn strands_messages(
    ctx: &mut ReadContext<'_, '_>,
    input: bool,
    legacy: &mut Vec<usize>,
) -> Option<(Payload, bool)> {
    let events = ctx.events;
    match ctx.bag.get(OPERATION).and_then(as_str) {
        Some("chat" | "execute_event_loop_cycle") => {
            strands_conversation(events, ctx.warnings, input, legacy)
        }
        // An agent span's input is its system and user messages, read as on a
        // `chat` span. Its output stays the plain-string choice (row 6).
        Some("invoke_agent") if input => strands_conversation(events, ctx.warnings, input, legacy),
        Some("invoke_agent") => strands_agent(events, legacy),
        Some("execute_tool") => strands_tool(events, ctx.warnings, input, legacy),
        _ => None,
    }
}

/// Rows 1 to 3: the conversation events of `chat` and cycle spans. `content`
/// (or the choice's `message`) is an array of Converse blocks; a block none of
/// the converter's three keys matches follows the failure path, as does
/// content that is not such an array. Every event it places is read, so each
/// one is pushed onto `legacy`.
fn strands_conversation(
    events: &[Event],
    warnings: &mut Warnings,
    input: bool,
    legacy: &mut Vec<usize>,
) -> Option<(Payload, bool)> {
    let mut relevant: Vec<(&Event, &str)> = Vec::new();
    for (index, event) in events.iter().enumerate() {
        let role = if input {
            event_role(event.name.as_str())
        } else if event.name == CHOICE {
            Some("assistant")
        } else {
            None
        };
        if let Some(role) = role {
            relevant.push((event, role));
            legacy.push(index);
        }
    }
    if relevant.is_empty() {
        return None;
    }
    let mut messages = Vec::with_capacity(relevant.len());
    for &(event, role) in &relevant {
        let key = if input { "content" } else { "message" };
        let Some(decoded) =
            event_attr(event, key).and_then(|value| decode_json_text(value, warnings))
        else {
            return Some((Payload::Json(event_objects(&relevant, warnings)), true));
        };
        let Some(blocks) = decoded.as_array() else {
            return Some((Payload::Json(event_objects(&relevant, warnings)), true));
        };
        let Ok(mut message) = converse_message(Role::from(role), blocks) else {
            return Some((Payload::Json(event_objects(&relevant, warnings)), true));
        };
        if !input && let Some(reason) = event_attr(event, "finish_reason").and_then(as_str) {
            message.finish_reason = Some(reason.to_owned());
        }
        messages.push(message);
    }
    Some((Payload::Messages(messages), false))
}

/// Rows 4 and 5: an `execute_tool` span reads its arguments from the first
/// `gen_ai.tool.message` event that has `content`, and the joined text of its
/// first choice's blocks. Only the event it reads is pushed onto `legacy`.
fn strands_tool(
    events: &[Event],
    warnings: &mut Warnings,
    input: bool,
    legacy: &mut Vec<usize>,
) -> Option<(Payload, bool)> {
    if input {
        let index = position_of(events, "gen_ai.tool.message")?;
        let value = event_attr(&events[index], "content")?;
        legacy.push(index);
        return Some((non_message_payload(value, warnings), false));
    }
    let index = position_of(events, CHOICE)?;
    let blocks = choice_blocks(&events[index], warnings)?;
    let text = converse_text(&blocks)?;
    legacy.push(index);
    Some((Payload::Text(text), false))
}

/// Row 6: an `invoke_agent` span reads its first choice's plain-string message
/// as its output, as is.
fn strands_agent(events: &[Event], legacy: &mut Vec<usize>) -> Option<(Payload, bool)> {
    let index = position_of(events, CHOICE)?;
    let text = event_attr(&events[index], "message").and_then(as_str)?;
    legacy.push(index);
    Some((Payload::Text(text.to_owned()), false))
}

/// The `message` blocks of a `gen_ai.choice` event, decoded.
fn choice_blocks(event: &Event, warnings: &mut Warnings) -> Option<Vec<Json>> {
    let value = event_attr(event, "message")?;
    decode_json_text(value, warnings)?.as_array().cloned()
}

/// The events as JSON objects, the raw value the failure path stores. Called
/// only on the failure branches.
fn event_objects(events: &[(&Event, &str)], warnings: &mut Warnings) -> Json {
    Json::Array(
        events
            .iter()
            .map(|(event, role)| event_message(event, role, warnings))
            .collect(),
    )
}

fn position_of(events: &[Event], name: &str) -> Option<usize> {
    events.iter().position(|event| event.name == name)
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
        // A finish reason is text: a message's `finish_reason` must be a
        // string, so a JSON-looking value stays the text it was sent as.
        if attribute.key == "finish_reason" {
            if let Some(text) = as_str(value).filter(|text| !text.is_empty()) {
                object.insert(attribute.key.clone(), Json::String(text.to_owned()));
            }
            continue;
        }
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
/// events. On a Strands span only the legacy events a reader read (`legacy`)
/// are consumed; an event no reader read stays in `events`. Other spans keep
/// every legacy event.
fn payload_events(ctx: &ReadContext<'_, '_>, legacy: &[usize]) -> Vec<usize> {
    let strands = ctx.scope.starts_with("strands");
    ctx.events
        .iter()
        .enumerate()
        .filter(|(index, event)| {
            event.name == DETAILS
                || if strands {
                    legacy.contains(index)
                } else {
                    event.name == CHOICE || event_role(event.name.as_str()).is_some()
                }
        })
        .map(|(index, _)| index)
        .collect()
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
    use crate::bag::{AttrBag, Use};
    use crate::payload::{Io, Payload, ReadContext};
    use crate::warn::Warnings;
    use cetra_core::enums::SpanKind;
    use cetra_core::message::messages_to_json;
    use opentelemetry_proto::tonic::common::v1::any_value::Value;
    use opentelemetry_proto::tonic::common::v1::{AnyValue, ArrayValue, KeyValue};
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

    /// Reads the span and keeps the bag, so a test can check how each key was used.
    fn read_span_with_bag<'a>(scope: &str, attrs: &'a [KeyValue]) -> (Io, AttrBag<'a>) {
        let mut warnings = Warnings::default();
        let mut bag = AttrBag::new(attrs, &mut warnings);
        let io = read(&mut ReadContext {
            bag: &mut bag,
            events: &[],
            kind: SpanKind::Llm,
            scope,
            conventions: &[],
            warnings: &mut warnings,
        });
        (io, bag)
    }

    const LITELLM_CALL_NAME: &str = "gen_ai.completion.0.function_call.name";
    const LITELLM_CALL_ARGUMENTS: &str = "gen_ai.completion.0.function_call.arguments";

    #[test]
    fn litellm_function_call_keys_become_the_output_tool_call() {
        // A span carrying both conventions (D-IN-25): the output is one
        // assistant message with no parts, and the call is only in the
        // OpenLLMetry-shaped keys.
        let attrs = [
            string_attr(
                "gen_ai.output.messages",
                r#"[{"role":"assistant","parts":[],"finish_reason":"tool_calls"}]"#,
            ),
            string_attr(LITELLM_CALL_NAME, "get_weather"),
            string_attr(LITELLM_CALL_ARGUMENTS, r#"{"city":"Paris"}"#),
        ];
        let (io, bag) = read_span_with_bag("", &attrs);
        let Payload::Messages(messages) = io.output.expect("output") else {
            panic!("output is messages");
        };
        assert_eq!(messages.len(), 1);
        assert_eq!(
            messages_to_json(&messages),
            r#"[{"role":"assistant","parts":[{"type":"tool_call","name":"get_weather","arguments":{"city":"Paris"}}],"finish_reason":"tool_calls"}]"#
        );
        assert_eq!(bag.state(LITELLM_CALL_NAME), Some(Use::Payload));
        assert_eq!(bag.state(LITELLM_CALL_ARGUMENTS), Some(Use::Payload));

        // Without the arguments key the call has no arguments; a missing key is
        // no problem for the marking.
        let attrs = [
            string_attr(
                "gen_ai.output.messages",
                r#"[{"role":"assistant","parts":[],"finish_reason":"tool_calls"}]"#,
            ),
            string_attr(LITELLM_CALL_NAME, "get_weather"),
        ];
        let (io, bag) = read_span_with_bag("", &attrs);
        let Payload::Messages(messages) = io.output.expect("output") else {
            panic!("output is messages");
        };
        assert_eq!(
            messages_to_json(&messages),
            r#"[{"role":"assistant","parts":[{"type":"tool_call","name":"get_weather"}],"finish_reason":"tool_calls"}]"#
        );
        assert_eq!(bag.state(LITELLM_CALL_NAME), Some(Use::Payload));
        assert_eq!(bag.state(LITELLM_CALL_ARGUMENTS), None);

        // An output whose message has parts is left alone, and the keys stay
        // unread for the OpenLLMetry reader.
        let attrs = [
            string_attr(
                "gen_ai.output.messages",
                r#"[{"role":"assistant","parts":[{"type":"text","content":"Paris."}]}]"#,
            ),
            string_attr(LITELLM_CALL_NAME, "get_weather"),
            string_attr(LITELLM_CALL_ARGUMENTS, r#"{"city":"Paris"}"#),
        ];
        let (io, bag) = read_span_with_bag("", &attrs);
        let Payload::Messages(messages) = io.output.expect("output") else {
            panic!("output is messages");
        };
        assert_eq!(
            messages_to_json(&messages),
            r#"[{"role":"assistant","parts":[{"type":"text","content":"Paris."}]}]"#
        );
        assert_eq!(bag.state(LITELLM_CALL_NAME), Some(Use::Unread));
        assert_eq!(bag.state(LITELLM_CALL_ARGUMENTS), Some(Use::Unread));

        // An empty name is no call: nothing is added and nothing is marked.
        let attrs = [
            string_attr(
                "gen_ai.output.messages",
                r#"[{"role":"assistant","parts":[]}]"#,
            ),
            string_attr(LITELLM_CALL_NAME, ""),
            string_attr(LITELLM_CALL_ARGUMENTS, r#"{"city":"Paris"}"#),
        ];
        let (io, bag) = read_span_with_bag("", &attrs);
        let Payload::Messages(messages) = io.output.expect("output") else {
            panic!("output is messages");
        };
        assert_eq!(
            messages_to_json(&messages),
            r#"[{"role":"assistant","parts":[]}]"#
        );
        assert_eq!(bag.state(LITELLM_CALL_NAME), Some(Use::Unread));
        assert_eq!(bag.state(LITELLM_CALL_ARGUMENTS), Some(Use::Unread));

        // Two messages, or a message of another role, are not LiteLLM's shape.
        for output in [
            r#"[{"role":"assistant","parts":[]},{"role":"assistant","parts":[]}]"#,
            r#"[{"role":"user","parts":[]}]"#,
        ] {
            let attrs = [
                string_attr("gen_ai.output.messages", output),
                string_attr(LITELLM_CALL_NAME, "get_weather"),
            ];
            let (io, bag) = read_span_with_bag("", &attrs);
            let Payload::Messages(messages) = io.output.expect("output") else {
                panic!("output is messages");
            };
            assert_eq!(messages_to_json(&messages), output);
            assert_eq!(bag.state(LITELLM_CALL_NAME), Some(Use::Unread));
        }
    }

    const TRACER: &str = "strands.telemetry.tracer";

    #[test]
    fn bifrost_root_span_text_is_the_fields_plain_value() {
        // D-IN-25 item 11: on scope `bifrost`, a message key that is not a
        // JSON array is the field's plain value, and nothing failed.
        let attrs = [
            string_attr("gen_ai.input.messages", "What is the weather in Paris?"),
            string_attr(
                "gen_ai.output.messages",
                r#"{"temperature_c":18,"conditions":"sunny"}"#,
            ),
        ];
        let (io, bag) = read_span_with_bag("bifrost", &attrs);
        assert_eq!(
            io.input,
            Some(Payload::Text("What is the weather in Paris?".to_owned()))
        );
        assert_eq!(
            io.output,
            Some(Payload::Json(
                json!({"temperature_c": 18, "conditions": "sunny"})
            ))
        );
        assert!(io.errors.is_empty());
        assert_eq!(bag.state("gen_ai.input.messages"), Some(Use::Payload));
        assert_eq!(bag.state("gen_ai.output.messages"), Some(Use::Payload));

        // A JSON array that is not all objects is not messages either: it is
        // a plain JSON value, and nothing failed.
        let attrs = [string_attr("gen_ai.output.messages", "[1,2]")];
        let (io, _) = read_span_with_bag("bifrost", &attrs);
        assert_eq!(io.output, Some(Payload::Json(json!([1, 2]))));
        assert!(io.errors.is_empty());

        // A message array still goes through the converter.
        let attrs = [string_attr(
            "gen_ai.output.messages",
            r#"[{"role":"assistant","parts":[{"type":"text","content":"It is 18C and sunny."}]}]"#,
        )];
        let (io, _) = read_span_with_bag("bifrost", &attrs);
        let Payload::Messages(messages) = io.output.expect("output") else {
            panic!("output is messages");
        };
        assert_eq!(
            messages_to_json(&messages),
            r#"[{"role":"assistant","parts":[{"type":"text","content":"It is 18C and sunny."}]}]"#
        );
        assert!(io.errors.is_empty());

        // The same plain text in another scope is still a conversion failure.
        let attrs = [string_attr(
            "gen_ai.input.messages",
            "What is the weather in Paris?",
        )];
        let (io, _) = read_span_with_bag("", &attrs);
        assert_eq!(
            io.input,
            Some(Payload::Text("What is the weather in Paris?".to_owned()))
        );
        assert_eq!(io.errors, ["messages:genai:input"]);
    }

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
    fn strands_invoke_agent_reads_its_messages_as_input_and_the_choice_text_as_output() {
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
        let Payload::Messages(messages) = io.input.expect("input") else {
            panic!("expected messages");
        };
        assert_eq!(
            messages_to_json(&messages),
            concat!(
                r#"[{"role":"system","parts":[{"type":"text","content":"You are a weather bot."}]},"#,
                r#"{"role":"user","parts":[{"type":"text","content":"What is the weather in Paris?"}]}]"#
            )
        );
        assert_eq!(
            io.output,
            Some(Payload::Text("It is 18C and sunny in Paris. ".to_owned()))
        );
        assert!(io.errors.is_empty());
        // The message events and the choice are all payload sources.
        assert_eq!(io.events, [0, 1, 2]);
    }

    #[test]
    fn strands_invoke_agent_input_is_the_message_events_and_not_the_choice() {
        // Amendment K's table: an agent span's message events are its input,
        // and its choice is the output only, never a message.
        let attrs = [string_attr("gen_ai.operation.name", "invoke_agent")];
        let events = [
            event(
                "gen_ai.user.message",
                &[("content", r#"[{"text": "What is the weather in Paris?"}]"#)],
            ),
            event(
                "gen_ai.choice",
                &[("finish_reason", "end_turn"), ("message", "18C")],
            ),
        ];
        let io = read_in_scope(TRACER, &attrs, &events, SpanKind::Agent);
        let Payload::Messages(messages) = io.input.expect("input") else {
            panic!("expected messages");
        };
        assert_eq!(messages.len(), 1);
        assert_eq!(io.output, Some(Payload::Text("18C".to_owned())));
        assert_eq!(io.events, [0, 1]);
    }

    #[test]
    fn strands_invoke_agent_with_a_non_converse_message_follows_the_failure_path() {
        let attrs = [string_attr("gen_ai.operation.name", "invoke_agent")];
        let events = [event(
            "gen_ai.user.message",
            &[("content", "plain text, not blocks")],
        )];
        let io = read_in_scope(TRACER, &attrs, &events, SpanKind::Agent);
        assert_eq!(io.errors, ["messages:genai:input"]);
        assert!(matches!(io.input, Some(Payload::Json(_))));
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
    fn a_strands_span_without_an_operation_reads_and_consumes_nothing() {
        let events = [
            event("gen_ai.user.message", &[("content", r#"[{"text": "hi"}]"#)]),
            event("gen_ai.choice", &[("message", r#"[{"text": "hello"}]"#)]),
        ];
        let io = read_in_scope(TRACER, &[], &events, SpanKind::Llm);
        assert_eq!(io.input, None);
        assert_eq!(io.output, None);
        assert!(io.events.is_empty());
    }

    #[test]
    fn an_execute_tool_choice_without_a_text_block_stays_in_the_events() {
        let attrs = [string_attr("gen_ai.operation.name", "execute_tool")];
        let events = [event(
            "gen_ai.choice",
            &[("message", r#"[{"json": {"temp": 18}}]"#)],
        )];
        let io = read_in_scope(TRACER, &attrs, &events, SpanKind::Tool);
        assert_eq!(io.output, None);
        assert!(io.events.is_empty());
    }

    #[test]
    fn an_execute_tool_choice_that_is_not_an_array_stays_in_the_events() {
        let attrs = [string_attr("gen_ai.operation.name", "execute_tool")];
        let events = [event("gen_ai.choice", &[("message", "18C sunny")])];
        let io = read_in_scope(TRACER, &attrs, &events, SpanKind::Tool);
        assert_eq!(io.output, None);
        assert!(io.events.is_empty());
    }

    #[test]
    fn an_invoke_agent_choice_that_is_not_a_string_stays_in_the_events() {
        // The choice's `message` is an OTLP array of blocks, not a string.
        let attrs = [string_attr("gen_ai.operation.name", "invoke_agent")];
        let blocks = AnyValue {
            value: Some(Value::ArrayValue(ArrayValue {
                values: vec![AnyValue {
                    value: Some(Value::StringValue("18C".to_owned())),
                }],
            })),
        };
        let events = [Event {
            name: "gen_ai.choice".to_owned(),
            attributes: vec![KeyValue {
                key: "message".to_owned(),
                value: Some(blocks),
                key_strindex: 0,
            }],
            ..Event::default()
        }];
        let io = read_in_scope(TRACER, &attrs, &events, SpanKind::Agent);
        assert_eq!(io.output, None);
        assert!(io.events.is_empty());
    }

    #[test]
    fn an_execute_tool_message_without_content_stays_in_the_events() {
        let attrs = [string_attr("gen_ai.operation.name", "execute_tool")];
        let events = [event("gen_ai.tool.message", &[("id", "call_1")])];
        let io = read_in_scope(TRACER, &attrs, &events, SpanKind::Tool);
        assert_eq!(io.input, None);
        assert!(io.events.is_empty());
    }

    #[test]
    fn a_legacy_event_finish_reason_stays_text_even_when_it_looks_like_json() {
        // The generic path (no Strands scope): the value is text, not an object.
        let events = [event(
            "gen_ai.choice",
            &[("message", "hi"), ("finish_reason", r#"{"a":1}"#)],
        )];
        let io = read_span(&[], &events, SpanKind::Llm);
        let Payload::Messages(messages) = io.output.expect("output") else {
            panic!("expected messages");
        };
        let value: serde_json::Value =
            serde_json::from_str(&messages_to_json(&messages)).expect("json");
        assert_eq!(value[0]["finish_reason"], r#"{"a":1}"#);
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
