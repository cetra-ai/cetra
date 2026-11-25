//! OpenLLMetry prompts, completions, entity values, and function definitions.
//!
//! Spec 02 § Input and output. Flattened messages win over
//! `traceloop.entity.input` and `traceloop.entity.output`. Those fallbacks are
//! stored as JSON without unwrapping `args`, `kwargs`, or `output`.

use cetra_core::message::{Message, Part};
use serde_json::{Map, Value as Json};

use super::present_json;
use crate::convert::{openai_messages, openllmetry_messages};
use crate::payload::{
    Field, Io, KeyPattern, Payload, ReadContext, non_message_payload, parse_tool_arguments,
};
use crate::value::as_str;

const PROMPT: &str = "gen_ai.prompt.";
const COMPLETION: &str = "gen_ai.completion.";
const ENTITY_INPUT: &str = "traceloop.entity.input";
const ENTITY_OUTPUT: &str = "traceloop.entity.output";
const FUNCTIONS: &str = "llm.request.functions.";

pub(crate) const PAYLOAD_KEYS: &[(Field, KeyPattern)] = &[
    (Field::Input, KeyPattern::IndexedPrefix(PROMPT)),
    (Field::Input, KeyPattern::Exact(ENTITY_INPUT)),
    (Field::Output, KeyPattern::IndexedPrefix(COMPLETION)),
    (Field::Output, KeyPattern::Exact(ENTITY_OUTPUT)),
    (Field::ToolDefinitions, KeyPattern::IndexedPrefix(FUNCTIONS)),
];

pub(crate) fn read(ctx: &mut ReadContext<'_, '_>) -> Io {
    let mut io = Io::default();
    if let Some((payload, failed)) = messages(ctx, PROMPT) {
        io.input = Some(payload);
        if failed {
            io.errors.push("messages:openllmetry:input");
        }
        ctx.bag.drop_payload(ENTITY_INPUT);
    } else if let Some(payload) = entity(ctx, ENTITY_INPUT) {
        io.input = Some(payload);
    }
    if let Some((payload, failed)) = messages(ctx, COMPLETION) {
        io.output = Some(payload);
        if failed {
            io.errors.push("messages:openllmetry:output");
        }
        ctx.bag.drop_payload(ENTITY_OUTPUT);
    } else if let Some(payload) = entity(ctx, ENTITY_OUTPUT) {
        io.output = Some(payload);
    }
    io.tool_definitions = functions(ctx);
    io
}

fn messages(ctx: &mut ReadContext<'_, '_>, prefix: &str) -> Option<(Payload, bool)> {
    let completion = prefix == COMPLETION;
    let mut raw = openllmetry_messages(ctx.bag, prefix)?;
    consume_prefix(ctx, prefix);
    // The value stored on a conversion failure is the client's, so the role
    // default goes on the copy that is converted.
    let client_value = if completion {
        let client_value = Json::Array(raw.clone());
        default_assistant_roles(&mut raw);
        Some(client_value)
    } else {
        None
    };
    let value = Json::Array(raw);
    match openai_messages(&value) {
        Ok(mut converted) => {
            lift_finish_reasons(&mut converted, &value);
            if completion {
                lift_function_calls(&mut converted);
            }
            Some((Payload::Messages(converted), false))
        }
        Err(_) => Some((Payload::Json(client_value.unwrap_or(value)), true)),
    }
}

/// A completion message without a `role` is the model's: LiteLLM's tool turn
/// writes `gen_ai.completion.0.function_call.*` and no role (D-IN-25). A
/// prompt without a role still does not convert.
fn default_assistant_roles(raw: &mut [Json]) {
    for message in raw {
        if let Json::Object(object) = message {
            object
                .entry("role")
                .or_insert_with(|| Json::String("assistant".to_owned()));
        }
    }
}

/// A completion message's `function_call` object becomes one tool-call part,
/// after any text part (LiteLLM, D-IN-25). The value is read before it is
/// removed: a value that is not an object, and an object whose `name` is
/// missing or not a string, stay in the message's `extra`. Nothing fails.
fn lift_function_calls(messages: &mut [Message]) {
    for message in messages {
        let Some(Json::Object(call)) = message.extra.get("function_call") else {
            continue;
        };
        let lifted = call
            .get("name")
            .and_then(Json::as_str)
            .map(|name| (name.to_owned(), call.get("arguments").cloned()));
        let Some((name, arguments)) = lifted else {
            continue;
        };
        message.extra.shift_remove("function_call");
        message.parts.push(Part::ToolCall {
            id: None,
            name,
            arguments: arguments.map(parse_tool_arguments),
            extra: Map::new(),
        });
    }
}

fn lift_finish_reasons(messages: &mut [Message], value: &Json) {
    let Some(items) = value.as_array() else {
        return;
    };
    for (message, item) in messages.iter_mut().zip(items) {
        let Some(reason) = item.get("finish_reason").and_then(Json::as_str) else {
            continue;
        };
        if reason.is_empty() {
            continue;
        }
        message.finish_reason = Some(reason.to_owned());
        message.extra.shift_remove("finish_reason");
    }
}

fn entity(ctx: &mut ReadContext<'_, '_>, key: &str) -> Option<Payload> {
    let payload = {
        let value = ctx.bag.get(key)?;
        if as_str(value) == Some("") {
            return None;
        }
        non_message_payload(value, ctx.warnings)
    };
    ctx.bag.take_payload(key);
    Some(payload)
}

fn functions(ctx: &mut ReadContext<'_, '_>) -> Option<Vec<Json>> {
    let indexes: Vec<usize> = ctx.bag.indexed(FUNCTIONS).into_keys().collect();
    if indexes.is_empty() {
        return None;
    }
    let tools: Vec<Json> = indexes
        .into_iter()
        .filter_map(|index| function_tool(ctx, index))
        .collect();
    consume_prefix(ctx, FUNCTIONS);
    if tools.is_empty() { None } else { Some(tools) }
}

fn function_tool(ctx: &mut ReadContext<'_, '_>, index: usize) -> Option<Json> {
    let base = format!("{FUNCTIONS}{index}.");
    let name = owned_string(ctx, &format!("{base}name"));
    let description = owned_string(ctx, &format!("{base}description"));
    let parameters = parameter(ctx, &format!("{base}parameters"));
    if name.is_none() && description.is_none() && parameters.is_none() {
        return None;
    }
    let mut tool = Map::new();
    tool.insert("type".to_owned(), Json::String("function".to_owned()));
    if let Some(name) = name {
        tool.insert("name".to_owned(), Json::String(name));
    }
    if let Some(description) = description {
        tool.insert("description".to_owned(), Json::String(description));
    }
    if let Some(parameters) = parameters {
        tool.insert("parameters".to_owned(), parameters);
    }
    Some(Json::Object(tool))
}

fn parameter(ctx: &mut ReadContext<'_, '_>, key: &str) -> Option<Json> {
    let value = ctx.bag.get(key)?;
    present_json(value, ctx.warnings).map(parse_tool_arguments)
}

fn owned_string(ctx: &ReadContext<'_, '_>, key: &str) -> Option<String> {
    let text = as_str(ctx.bag.get(key)?)?;
    if text.is_empty() {
        None
    } else {
        Some(text.to_owned())
    }
}

fn consume_prefix(ctx: &mut ReadContext<'_, '_>, prefix: &str) {
    let keys: Vec<String> = ctx
        .bag
        .indexed(prefix)
        .into_iter()
        .flat_map(|(index, fields)| {
            fields
                .into_iter()
                .map(move |(rest, _)| format!("{prefix}{index}.{rest}"))
        })
        .collect();
    for key in keys {
        ctx.bag.take_payload(&key);
    }
}

#[cfg(test)]
mod tests {
    use super::{lift_function_calls, read};
    use crate::bag::{AttrBag, Use};
    use crate::conventions::{READERS, REGISTRIES};
    use crate::payload::{Payload, ReadContext, assemble};
    use crate::warn::Warnings;
    use cetra_core::enums::SpanKind;
    use cetra_core::message::{Message, Role, messages_to_json};
    use opentelemetry_proto::tonic::common::v1::any_value::Value;
    use opentelemetry_proto::tonic::common::v1::{AnyValue, KeyValue};
    use serde_json::{Map, json};

    fn string_attr(key: &str, text: &str) -> KeyValue {
        KeyValue {
            key: key.to_owned(),
            value: Some(AnyValue {
                value: Some(Value::StringValue(text.to_owned())),
            }),
            key_strindex: 0,
        }
    }

    fn read_span(attrs: &[KeyValue]) -> (crate::payload::Io, AttrBag<'_>) {
        let mut warnings = Warnings::default();
        let mut bag = AttrBag::new(attrs, &mut warnings);
        let io = read(&mut ReadContext {
            bag: &mut bag,
            events: &[],
            kind: SpanKind::Llm,
            scope: "",
            conventions: &[],
            warnings: &mut warnings,
        });
        (io, bag)
    }

    #[test]
    fn finish_reason_is_lifted_onto_the_message() {
        let attrs = [
            string_attr("gen_ai.completion.0.role", "assistant"),
            string_attr("gen_ai.completion.0.content", "Paris."),
            string_attr("gen_ai.completion.0.finish_reason", "stop"),
            string_attr("traceloop.entity.output", r#"{"output":"ignored"}"#),
        ];
        let (io, bag) = read_span(&attrs);
        let Payload::Messages(messages) = io.output.expect("messages") else {
            panic!("output is messages");
        };
        assert_eq!(
            messages_to_json(&messages),
            r#"[{"role":"assistant","parts":[{"type":"text","content":"Paris."}],"finish_reason":"stop"}]"#
        );
        assert_eq!(bag.state("traceloop.entity.output"), Some(Use::Payload));
    }

    #[test]
    fn entity_json_is_stored_without_unwrapping() {
        let (io, _) = read_span(&[
            string_attr(
                "traceloop.entity.input",
                r#"{"args":[],"kwargs":{"question":"capital"}}"#,
            ),
            string_attr("traceloop.entity.output", r#"{"output":"Paris"}"#),
        ]);
        assert_eq!(
            io.input,
            Some(Payload::Json(json!({
                "args": [],
                "kwargs": {"question": "capital"}
            })))
        );
        assert_eq!(io.output, Some(Payload::Json(json!({"output": "Paris"}))));
    }

    #[test]
    fn functions_keep_index_order_and_parse_parameters() {
        let attrs = [
            string_attr("llm.request.functions.2.name", "later"),
            string_attr("llm.request.functions.0.name", "lookup"),
            string_attr("llm.request.functions.0.description", "Look up a city"),
            string_attr(
                "llm.request.functions.0.parameters",
                r#"{"type":"object","properties":{"city":{"type":"string"}}}"#,
            ),
            string_attr("llm.request.functions.1.description", ""),
        ];
        let (io, bag) = read_span(&attrs);
        assert_eq!(
            io.tool_definitions,
            Some(vec![
                json!({
                    "type": "function",
                    "name": "lookup",
                    "description": "Look up a city",
                    "parameters": {"type": "object", "properties": {"city": {"type": "string"}}}
                }),
                json!({"type": "function", "name": "later"}),
            ])
        );
        assert_eq!(
            bag.state("llm.request.functions.1.description"),
            Some(Use::Payload)
        );
    }

    #[test]
    fn a_prompt_without_a_role_is_raw_json_and_an_error_code() {
        let (io, _) = read_span(&[string_attr("gen_ai.prompt.0.content", "hello")]);
        assert_eq!(io.input, Some(Payload::Json(json!([{"content": "hello"}]))));
        assert_eq!(io.errors, ["messages:openllmetry:input"]);
    }

    #[test]
    fn a_completion_without_a_role_is_an_assistant_message() {
        let (io, _) = read_span(&[string_attr("gen_ai.completion.0.content", "Paris.")]);
        let Payload::Messages(messages) = io.output.expect("messages") else {
            panic!("output is messages");
        };
        assert_eq!(
            messages_to_json(&messages),
            r#"[{"role":"assistant","parts":[{"type":"text","content":"Paris."}]}]"#
        );
        assert!(io.errors.is_empty());
    }

    #[test]
    fn a_completion_function_call_is_a_tool_call_after_any_text_part() {
        // LiteLLM's tool turn: the call is only in the flattened keys, with no
        // role (D-IN-25).
        let attrs = [
            string_attr("gen_ai.completion.0.content", "Let me check."),
            string_attr("gen_ai.completion.0.function_call.name", "get_weather"),
            string_attr(
                "gen_ai.completion.0.function_call.arguments",
                r#"{"city":"Paris"}"#,
            ),
        ];
        let (io, _) = read_span(&attrs);
        let Payload::Messages(messages) = io.output.expect("messages") else {
            panic!("output is messages");
        };
        assert_eq!(
            messages_to_json(&messages),
            r#"[{"role":"assistant","parts":[{"type":"text","content":"Let me check."},{"type":"tool_call","name":"get_weather","arguments":{"city":"Paris"}}]}]"#
        );
        assert!(io.errors.is_empty());

        // Without the arguments key the call has none.
        let (io, _) = read_span(&[string_attr(
            "gen_ai.completion.0.function_call.name",
            "get_weather",
        )]);
        let Payload::Messages(messages) = io.output.expect("messages") else {
            panic!("output is messages");
        };
        assert_eq!(
            messages_to_json(&messages),
            r#"[{"role":"assistant","parts":[{"type":"tool_call","name":"get_weather"}]}]"#
        );

        // A call without a name is not lifted and stays in `extra`: the
        // message converts and nothing fails.
        let (io, _) = read_span(&[string_attr(
            "gen_ai.completion.0.function_call.arguments",
            r#"{"city":"Paris"}"#,
        )]);
        let Payload::Messages(messages) = io.output.expect("messages") else {
            panic!("output is messages");
        };
        assert_eq!(
            messages_to_json(&messages),
            r#"[{"role":"assistant","parts":[],"function_call":{"arguments":"{\"city\":\"Paris\"}"}}]"#
        );
        assert!(io.errors.is_empty());
    }

    #[test]
    fn a_function_call_value_that_is_not_an_object_stays_in_extra() {
        let mut message = Message {
            role: Role::Assistant,
            name: None,
            parts: Vec::new(),
            finish_reason: None,
            extra: Map::new(),
        };
        message
            .extra
            .insert("function_call".to_owned(), json!("not an object"));
        let mut messages = vec![message.clone()];
        lift_function_calls(&mut messages);
        assert_eq!(messages, vec![message]);
    }

    #[test]
    fn the_litellm_tool_turn_records_no_normalize_error() {
        // Both conventions on one span (D-IN-25): GenAI's empty assistant
        // output wins, and the OpenLLMetry reader converts the flattened
        // completion keys too, so no error code reaches `cetra.normalize_error`.
        let attrs = [
            string_attr(
                "gen_ai.output.messages",
                r#"[{"role":"assistant","parts":[],"finish_reason":"tool_calls"}]"#,
            ),
            string_attr("gen_ai.completion.0.function_call.name", "get_weather"),
            string_attr(
                "gen_ai.completion.0.function_call.arguments",
                r#"{"city":"Paris"}"#,
            ),
        ];
        let mut warnings = Warnings::default();
        let mut bag = AttrBag::new(&attrs, &mut warnings);
        let assembled = assemble(
            &mut bag,
            &[],
            SpanKind::Llm,
            "",
            &[],
            &mut warnings,
            READERS,
            REGISTRIES,
        );
        let Payload::Messages(messages) = assembled.io.output.expect("output") else {
            panic!("output is messages");
        };
        assert_eq!(
            messages_to_json(&messages),
            r#"[{"role":"assistant","parts":[{"type":"tool_call","name":"get_weather","arguments":{"city":"Paris"}}],"finish_reason":"tool_calls"}]"#
        );
        assert!(assembled.io.errors.is_empty());
        assert_eq!(
            bag.state("gen_ai.completion.0.function_call.name"),
            Some(Use::Payload)
        );
        assert_eq!(
            bag.state("gen_ai.completion.0.function_call.arguments"),
            Some(Use::Payload)
        );
    }
}
