//! Scoped span payloads.
//!
//! Spec 02 § Input / output, the scoped row (D-IN-6, D-IN-26 item 9). A fixed
//! scope table fills `input` and `output` only where the convention readers
//! left them empty. The keys are in no static registry, and no convention
//! ranks them, so the driver's `drop_lower_payloads` never drops them.
//!
//! Pipecat (scope starting with `pipecat`): plain `input` and `output` (R-PC-1).
//! LiveKit Agents (scope `livekit-agents`): the `lk.pii.*` content keys, keyed
//! by attribute (R-LK-3).
//! Prompt Flow (scope `promptflow`): `inputs` and `output` (R-PF-3).
//! Langflow (scope `langflow`): `inputs` and `outputs` (R-LF-2).
//! Quarkus LangChain4j (scope `io.quarkus.opentelemetry`): `gen_ai.prompt` and
//! `gen_ai.completion` (D-IN-30).

use cetra_core::enums::SpanKind;
use serde_json::Value as Json;

use crate::convert::openai_messages;
use crate::payload::{Io, Payload, ReadContext, decode_json_text, non_message_payload};
use crate::value::as_str;

/// The scoped input and output of one span. Each field is set only when
/// `filled` lacks it, and a key is consumed only when it fills its field.
pub(crate) fn read(ctx: &mut ReadContext<'_, '_>, filled: &Io) -> Io {
    let mut io = Io::default();
    if ctx.scope.starts_with("pipecat") {
        if filled.input.is_none() {
            io.input = pipecat_input(ctx);
        }
        if filled.output.is_none() {
            io.output = take_value(ctx, "output");
        }
    } else if ctx.scope == "promptflow" {
        if filled.input.is_none() {
            io.input = promptflow_input(ctx);
        }
        if filled.output.is_none() {
            io.output = promptflow_output(ctx);
        }
    } else if ctx.scope == "langflow" {
        if filled.input.is_none() {
            io.input = take_value(ctx, "inputs");
        }
        if filled.output.is_none() {
            io.output = take_value(ctx, "outputs");
        }
    } else if ctx.scope == "io.quarkus.opentelemetry" {
        // D-IN-30 item 4: Quarkus LangChain4j's content keys, plain values.
        if filled.input.is_none() {
            io.input = take_value(ctx, "gen_ai.prompt");
        }
        if filled.output.is_none() {
            io.output = take_value(ctx, "gen_ai.completion");
        }
    } else if ctx.scope == "livekit-agents" {
        if filled.input.is_none() {
            io.input = first_value(
                ctx,
                &[
                    "lk.pii.user_input",
                    "lk.pii.chat_ctx",
                    "lk.pii.function_tool.arguments",
                ],
            );
        }
        if filled.output.is_none() {
            io.output = first_value(
                ctx,
                &["lk.pii.response.text", "lk.pii.function_tool.output"],
            );
        }
    }
    io
}

/// R-PC-1: `input` is an OpenAI-shaped message array when it converts, else
/// the plain value by rule 10. A failed conversion adds no error code.
fn pipecat_input(ctx: &mut ReadContext<'_, '_>) -> Option<Payload> {
    let value = ctx.bag.get("input")?;
    if as_str(value) == Some("") {
        return None;
    }
    ctx.bag.take_payload("input");
    let parsed = decode_json_text(value, ctx.warnings);
    if let Some(json) = parsed.as_ref().filter(|json| json.is_array())
        && let Ok(messages) = openai_messages(json)
    {
        return Some(Payload::Messages(messages));
    }
    match parsed {
        Some(json) if json.is_object() || json.is_array() => Some(Payload::Json(json)),
        _ => Some(non_message_payload(value, ctx.warnings)),
    }
}

/// R-PF-3: on an `llm` span, `inputs` is the OpenAI request object; its
/// `messages` array is messages when it converts. Otherwise the plain value.
fn promptflow_input(ctx: &mut ReadContext<'_, '_>) -> Option<Payload> {
    let value = ctx.bag.get("inputs")?;
    if as_str(value) == Some("") {
        return None;
    }
    ctx.bag.take_payload("inputs");
    if ctx.kind == SpanKind::Llm
        && let Some(json) = decode_json_text(value, ctx.warnings)
        && let Some(messages) = json.get("messages")
        && let Ok(messages) = openai_messages(messages)
    {
        return Some(Payload::Messages(messages));
    }
    Some(non_message_payload(value, ctx.warnings))
}

/// R-PF-3: on an `llm` span, `output` is the OpenAI response object; its
/// `choices[0].message` is one message when it converts. Otherwise the plain value.
fn promptflow_output(ctx: &mut ReadContext<'_, '_>) -> Option<Payload> {
    let value = ctx.bag.get("output")?;
    if as_str(value) == Some("") {
        return None;
    }
    ctx.bag.take_payload("output");
    if ctx.kind == SpanKind::Llm
        && let Some(json) = decode_json_text(value, ctx.warnings)
        && let Some(message) = json.pointer("/choices/0/message")
        && let Ok(messages) = openai_messages(&Json::Array(vec![message.clone()]))
    {
        return Some(Payload::Messages(messages));
    }
    Some(non_message_payload(value, ctx.warnings))
}

/// The first of `keys` that holds a non-empty value. Only that key is consumed.
fn first_value(ctx: &mut ReadContext<'_, '_>, keys: &[&str]) -> Option<Payload> {
    keys.iter().find_map(|&key| take_value(ctx, key))
}

/// A non-empty value, by rule 10. An empty string is absent and not consumed.
fn take_value(ctx: &mut ReadContext<'_, '_>, key: &str) -> Option<Payload> {
    let value = ctx.bag.get(key)?;
    if as_str(value) == Some("") {
        return None;
    }
    ctx.bag.take_payload(key);
    Some(non_message_payload(value, ctx.warnings))
}

#[cfg(test)]
mod tests {
    use cetra_core::enums::{Convention, SpanKind};
    use opentelemetry_proto::tonic::common::v1::any_value::Value;
    use opentelemetry_proto::tonic::common::v1::{AnyValue, KeyValue};
    use serde_json::json;

    use super::read;
    use crate::bag::{AttrBag, Use};
    use crate::conventions::{READERS, REGISTRIES};
    use crate::payload::{Io, Payload, ReadContext, assemble};
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

    fn read_in<'a>(scope: &str, attrs: &'a [KeyValue], filled: &Io) -> (Io, AttrBag<'a>, Warnings) {
        let mut warnings = Warnings::default();
        let mut bag = AttrBag::new(attrs, &mut warnings);
        let io = read(
            &mut ReadContext {
                bag: &mut bag,
                events: &[],
                kind: SpanKind::Span,
                scope,
                conventions: &[],
                warnings: &mut warnings,
            },
            filled,
        );
        (io, bag, warnings)
    }

    #[test]
    fn pipecat_openai_messages_are_the_input() {
        let text =
            json!([{"role": "user", "content": "What is the weather in Paris?"}]).to_string();
        let attrs = [string_attr("input", &text)];
        let (io, bag, warnings) = read_in("pipecat", &attrs, &Io::default());
        assert!(matches!(io.input, Some(Payload::Messages(ref messages)) if messages.len() == 1));
        assert_eq!(bag.state("input"), Some(Use::Payload));
        assert!(io.errors.is_empty());
        assert_eq!(warnings.to_attribute(), None);
    }

    #[test]
    fn pipecat_other_input_is_plain_and_fails_without_an_error_code() {
        // An array that is not OpenAI messages stays JSON.
        let attrs = [string_attr("input", r#"[{"foo":1}]"#)];
        let (io, _, _) = read_in("pipecat.turn", &attrs, &Io::default());
        assert_eq!(io.input, Some(Payload::Json(json!([{"foo": 1}]))));
        assert!(io.errors.is_empty());

        // Invalid JSON is text, with no normalization error.
        let attrs = [string_attr("input", "[oops")];
        let (io, _, warnings) = read_in("pipecat", &attrs, &Io::default());
        assert_eq!(io.input, Some(Payload::Text("[oops".to_owned())));
        assert!(io.errors.is_empty());
        assert_eq!(warnings.to_attribute(), None);
    }

    #[test]
    fn pipecat_output_is_text() {
        let attrs = [string_attr("output", "It is 18C and sunny in Paris.")];
        let (io, bag, _) = read_in("pipecat", &attrs, &Io::default());
        assert_eq!(
            io.output,
            Some(Payload::Text("It is 18C and sunny in Paris.".to_owned()))
        );
        assert_eq!(bag.state("output"), Some(Use::Payload));
    }

    #[test]
    fn plain_input_and_output_stay_unread_on_other_scopes() {
        let attrs = [
            string_attr("input", "hello"),
            string_attr("output", "world"),
        ];
        let (io, bag, _) = read_in("other.scope", &attrs, &Io::default());
        assert_eq!(io, Io::default());
        assert_eq!(bag.state("input"), Some(Use::Unread));
        assert_eq!(bag.state("output"), Some(Use::Unread));
    }

    #[test]
    fn a_filled_input_is_kept_and_its_key_is_not_consumed() {
        let attrs = [string_attr("input", "plain"), string_attr("output", "text")];
        let filled = Io {
            input: Some(Payload::Text("from a convention".to_owned())),
            ..Io::default()
        };
        let (io, bag, _) = read_in("pipecat", &attrs, &filled);
        assert_eq!(io.input, None);
        assert_eq!(bag.state("input"), Some(Use::Unread));
        assert_eq!(io.output, Some(Payload::Text("text".to_owned())));
    }

    #[test]
    fn an_empty_pipecat_value_is_absent_and_unconsumed() {
        let attrs = [string_attr("input", ""), string_attr("output", "")];
        let (io, bag, _) = read_in("pipecat", &attrs, &Io::default());
        assert_eq!(io, Io::default());
        assert_eq!(bag.state("input"), Some(Use::Unread));
        assert_eq!(bag.state("output"), Some(Use::Unread));
    }

    #[test]
    fn livekit_input_follows_the_attribute_order() {
        let attrs = [
            string_attr("lk.pii.user_input", "What is the weather in Paris?"),
            string_attr("lk.pii.chat_ctx", r#"{"items":[]}"#),
        ];
        let (io, bag, _) = read_in("livekit-agents", &attrs, &Io::default());
        assert_eq!(
            io.input,
            Some(Payload::Text("What is the weather in Paris?".to_owned()))
        );
        assert_eq!(bag.state("lk.pii.user_input"), Some(Use::Payload));
        assert_eq!(bag.state("lk.pii.chat_ctx"), Some(Use::Unread));

        // `llm_node`: LiveKit's chat context is JSON.
        let attrs = [string_attr("lk.pii.chat_ctx", r#"{"items":[]}"#)];
        let (io, bag, _) = read_in("livekit-agents", &attrs, &Io::default());
        assert_eq!(io.input, Some(Payload::Json(json!({"items": []}))));
        assert_eq!(bag.state("lk.pii.chat_ctx"), Some(Use::Payload));

        // `function_tool`: the arguments.
        let attrs = [string_attr(
            "lk.pii.function_tool.arguments",
            r#"{"city":"Paris"}"#,
        )];
        let (io, _, _) = read_in("livekit-agents", &attrs, &Io::default());
        assert_eq!(io.input, Some(Payload::Json(json!({"city": "Paris"}))));
    }

    #[test]
    fn livekit_output_follows_the_attribute_order() {
        let attrs = [
            string_attr("lk.pii.response.text", "It is 18C."),
            string_attr("lk.pii.function_tool.output", "{\"temperature_c\":18}"),
        ];
        let (io, bag, _) = read_in("livekit-agents", &attrs, &Io::default());
        assert_eq!(io.output, Some(Payload::Text("It is 18C.".to_owned())));
        assert_eq!(bag.state("lk.pii.function_tool.output"), Some(Use::Unread));

        let attrs = [string_attr(
            "lk.pii.function_tool.output",
            r#"{"temperature_c":18}"#,
        )];
        let (io, _, _) = read_in("livekit-agents", &attrs, &Io::default());
        assert_eq!(io.output, Some(Payload::Json(json!({"temperature_c": 18}))));
    }

    #[test]
    fn livekit_function_tool_keeps_the_genai_tool_call_keys() {
        let attrs = [
            string_attr("gen_ai.operation.name", "execute_tool"),
            string_attr("gen_ai.tool.name", "get_weather"),
            string_attr("gen_ai.tool.call.arguments", r#"{"city":"Paris"}"#),
            string_attr("gen_ai.tool.call.result", r#"{"temperature_c":18}"#),
            string_attr("lk.pii.function_tool.arguments", r#"{"city":"Lyon"}"#),
            string_attr("lk.pii.function_tool.output", r#"{"temperature_c":9}"#),
        ];
        let mut warnings = Warnings::default();
        let mut bag = AttrBag::new(&attrs, &mut warnings);
        let assembled = assemble(
            &mut bag,
            &[],
            SpanKind::Tool,
            "livekit-agents",
            &[Convention::Genai],
            &mut warnings,
            READERS,
            REGISTRIES,
        );
        assert_eq!(
            assembled.io.input,
            Some(Payload::Json(json!({"city": "Paris"})))
        );
        assert_eq!(
            assembled.io.output,
            Some(Payload::Json(json!({"temperature_c": 18})))
        );
        assert_eq!(
            bag.state("lk.pii.function_tool.arguments"),
            Some(Use::Unread)
        );
        assert_eq!(bag.state("lk.pii.function_tool.output"), Some(Use::Unread));
    }

    #[test]
    fn livekit_keys_on_another_scope_stay_unread() {
        let attrs = [
            string_attr("lk.pii.user_input", "hi"),
            string_attr("lk.pii.response.text", "hello"),
        ];
        let (io, bag, _) = read_in("other.scope", &attrs, &Io::default());
        assert_eq!(io, Io::default());
        assert_eq!(bag.state("lk.pii.user_input"), Some(Use::Unread));
        assert_eq!(bag.state("lk.pii.response.text"), Some(Use::Unread));
    }

    fn read_as<'a>(scope: &str, kind: SpanKind, attrs: &'a [KeyValue]) -> (Io, AttrBag<'a>) {
        let mut warnings = Warnings::default();
        let mut bag = AttrBag::new(attrs, &mut warnings);
        let io = read(
            &mut ReadContext {
                bag: &mut bag,
                events: &[],
                kind,
                scope,
                conventions: &[],
                warnings: &mut warnings,
            },
            &Io::default(),
        );
        (io, bag)
    }

    #[test]
    fn promptflow_llm_input_is_messages_when_it_converts() {
        let inputs = json!({
            "model": "gpt-4o-mini",
            "messages": [{"role": "user", "content": "What is the weather in Paris?"}],
        })
        .to_string();
        let attrs = [string_attr("inputs", &inputs)];
        let (io, bag) = read_as("promptflow", SpanKind::Llm, &attrs);
        assert!(matches!(io.input, Some(Payload::Messages(ref messages)) if messages.len() == 1));
        assert_eq!(bag.state("inputs"), Some(Use::Payload));
        assert!(io.errors.is_empty());
    }

    #[test]
    fn promptflow_input_is_plain_json_elsewhere() {
        // No `messages` array: the object.
        let attrs = [string_attr("inputs", r#"{"model":"gpt-4o-mini"}"#)];
        let (io, _) = read_as("promptflow", SpanKind::Llm, &attrs);
        assert_eq!(
            io.input,
            Some(Payload::Json(json!({"model": "gpt-4o-mini"})))
        );

        // A `messages` array that is not OpenAI messages: the object, no error.
        let attrs = [string_attr("inputs", r#"{"messages":[{"foo":1}]}"#)];
        let (io, _) = read_as("promptflow", SpanKind::Llm, &attrs);
        assert_eq!(
            io.input,
            Some(Payload::Json(json!({"messages": [{"foo": 1}]})))
        );
        assert!(io.errors.is_empty());

        // A `Function` span: its inputs are the arguments, never messages.
        let attrs = [string_attr(
            "inputs",
            r#"{"messages":[{"role":"user","content":"hi"}]}"#,
        )];
        let (io, _) = read_as("promptflow", SpanKind::Span, &attrs);
        assert!(matches!(io.input, Some(Payload::Json(_))));
    }

    #[test]
    fn promptflow_llm_output_is_the_first_choice_message() {
        let output = json!({
            "choices": [{"message": {"role": "assistant", "content": "It is 18C."}}],
        })
        .to_string();
        let attrs = [string_attr("output", &output)];
        let (io, bag) = read_as("promptflow", SpanKind::Llm, &attrs);
        assert!(matches!(io.output, Some(Payload::Messages(ref messages)) if messages.len() == 1));
        assert_eq!(bag.state("output"), Some(Use::Payload));

        // No choices: the object.
        let attrs = [string_attr("output", r#"{"id":"chatcmpl-1"}"#)];
        let (io, _) = read_as("promptflow", SpanKind::Llm, &attrs);
        assert_eq!(io.output, Some(Payload::Json(json!({"id": "chatcmpl-1"}))));

        // A non-`llm` span keeps the object even with choices.
        let attrs = [string_attr("output", &output)];
        let (io, _) = read_as("promptflow", SpanKind::Span, &attrs);
        assert!(matches!(io.output, Some(Payload::Json(_))));
    }

    #[test]
    fn langflow_inputs_and_outputs_are_plain_values() {
        let attrs = [
            string_attr(
                "inputs",
                r#"{"input_value":"What is the weather in Paris?"}"#,
            ),
            string_attr("outputs", r#"{"message":"It is 18C."}"#),
        ];
        let (io, bag) = read_as("langflow", SpanKind::Chain, &attrs);
        assert_eq!(
            io.input,
            Some(Payload::Json(
                json!({"input_value": "What is the weather in Paris?"})
            ))
        );
        assert_eq!(
            io.output,
            Some(Payload::Json(json!({"message": "It is 18C."})))
        );
        assert_eq!(bag.state("inputs"), Some(Use::Payload));
        assert_eq!(bag.state("outputs"), Some(Use::Payload));

        // Text is text, not messages.
        let attrs = [string_attr("outputs", "plain text")];
        let (io, _) = read_as("langflow", SpanKind::Chain, &attrs);
        assert_eq!(io.output, Some(Payload::Text("plain text".to_owned())));
    }

    #[test]
    fn promptflow_and_langflow_keys_on_other_scopes_stay_unread() {
        let attrs = [
            string_attr("inputs", r#"{"model":"m"}"#),
            string_attr("output", "x"),
            string_attr("outputs", "y"),
        ];
        for scope in ["other.scope", "promptflow.extra", "langflow.observability"] {
            let (io, bag) = read_as(scope, SpanKind::Llm, &attrs);
            assert_eq!(io, Io::default(), "{scope}");
            assert_eq!(bag.state("inputs"), Some(Use::Unread), "{scope}");
            assert_eq!(bag.state("output"), Some(Use::Unread), "{scope}");
            assert_eq!(bag.state("outputs"), Some(Use::Unread), "{scope}");
        }
    }

    #[test]
    fn quarkus_prompt_and_completion_fill_input_and_output() {
        // D-IN-30 item 4: Quarkus LangChain4j writes its content under
        // `gen_ai.prompt` / `gen_ai.completion` on the
        // `io.quarkus.opentelemetry` scope, as plain values (rule 10).
        let attrs = [
            string_attr("gen_ai.prompt", "What is the weather in Lyon?"),
            string_attr("gen_ai.completion", "It is sunny in Lyon."),
        ];
        let (io, bag, _) = read_in("io.quarkus.opentelemetry", &attrs, &Io::default());
        assert_eq!(
            io.input,
            Some(Payload::Text("What is the weather in Lyon?".to_owned()))
        );
        assert_eq!(
            io.output,
            Some(Payload::Text("It is sunny in Lyon.".to_owned()))
        );
        assert_eq!(bag.state("gen_ai.prompt"), Some(Use::Payload));
        assert_eq!(bag.state("gen_ai.completion"), Some(Use::Payload));
    }

    #[test]
    fn quarkus_keys_on_another_scope_are_untouched() {
        let attrs = [
            string_attr("gen_ai.prompt", "What is the weather in Lyon?"),
            string_attr("gen_ai.completion", "It is sunny in Lyon."),
        ];
        for scope in [
            "other.scope",
            "io.quarkus",
            "io.quarkus.opentelemetry.extra",
        ] {
            let (io, bag, _) = read_in(scope, &attrs, &Io::default());
            assert_eq!(io, Io::default(), "{scope}");
            assert_eq!(bag.state("gen_ai.prompt"), Some(Use::Unread), "{scope}");
            assert_eq!(bag.state("gen_ai.completion"), Some(Use::Unread), "{scope}");
        }
    }

    #[test]
    fn an_openinference_input_wins_and_inputs_stays_unread() {
        // `input.value` is OpenInference's reader; the scoped `inputs` fills only an empty input.
        let attrs = [
            string_attr("input.value", "from openinference"),
            string_attr("inputs", r#"{"model":"m","messages":[]}"#),
        ];
        let mut warnings = Warnings::default();
        let mut bag = AttrBag::new(&attrs, &mut warnings);
        let assembled = assemble(
            &mut bag,
            &[],
            SpanKind::Llm,
            "promptflow",
            &[Convention::Openinference],
            &mut warnings,
            READERS,
            REGISTRIES,
        );
        assert_eq!(
            assembled.io.input,
            Some(Payload::Text("from openinference".to_owned()))
        );
        assert_eq!(bag.state("inputs"), Some(Use::Unread));
    }

    #[test]
    fn with_only_inputs_the_scoped_read_fills_the_input() {
        let attrs = [string_attr("inputs", r#"{"model":"m"}"#)];
        let mut warnings = Warnings::default();
        let mut bag = AttrBag::new(&attrs, &mut warnings);
        let assembled = assemble(
            &mut bag,
            &[],
            SpanKind::Llm,
            "promptflow",
            &[Convention::Openinference],
            &mut warnings,
            READERS,
            REGISTRIES,
        );
        assert_eq!(
            assembled.io.input,
            Some(Payload::Json(json!({"model": "m"})))
        );
        assert_eq!(bag.state("inputs"), Some(Use::Payload));
    }
}
