//! VoltAgent span payloads.
//!
//! Spec 02 § Input and output, the VoltAgent row. Gated on the `voltagent`
//! convention (D-IN-6, D-IN-23 item 9): plain `input` and `output` on any
//! VoltAgent span, and `llm.messages` on its `llm` spans. The keys are not in
//! a static registry, so no other convention's reader drops them (amendment M).
//! The kind (V2), model and session (V3, V6) and token (V4) rules are in
//! `kind.rs`, `context.rs` and `usage.rs`.

use cetra_core::enums::{Convention, SpanKind};

use crate::convert::vercel_messages;
use crate::payload::{Io, Payload, ReadContext, decode_json_text, non_message_payload};
use crate::value::as_str;

const INPUT: &str = "input";
const OUTPUT: &str = "output";
/// A JSON array of AI SDK `{role, content}` messages, on `llm` spans (V5).
const LLM_MESSAGES: &str = "llm.messages";

pub(crate) fn read(ctx: &mut ReadContext<'_, '_>) -> Io {
    if !ctx.conventions.contains(&Convention::Voltagent) {
        return Io::default();
    }
    let mut io = Io::default();
    // `input` is consumed even when `llm.messages` fills the column, so the
    // plain key is not left unread on an `llm` span.
    let plain_input = plain(ctx, INPUT);
    if let Some((payload, failed)) = llm_messages(ctx) {
        io.input = Some(payload);
        if failed {
            io.errors.push("messages:voltagent:input");
        }
    } else {
        io.input = plain_input;
    }
    io.output = plain(ctx, OUTPUT);
    io
}

/// V5: `llm.messages` on an `llm` span, converted with the Vercel converter.
/// `None` when the span is not `llm` or the key is absent or empty. The
/// flag is `true` when the value could not be converted.
fn llm_messages(ctx: &mut ReadContext<'_, '_>) -> Option<(Payload, bool)> {
    if ctx.kind != SpanKind::Llm {
        return None;
    }
    let value = ctx.bag.take_payload(LLM_MESSAGES)?;
    if as_str(value) == Some("") {
        return None;
    }
    let Some(json) = decode_json_text(value, ctx.warnings) else {
        return Some((non_message_payload(value, ctx.warnings), true));
    };
    Some(match vercel_messages(&json) {
        Ok(messages) => (Payload::Messages(messages), false),
        Err(_) => (Payload::Json(json), true),
    })
}

/// V5: a plain `input` or `output`, by rule 10. An empty string is absent.
fn plain(ctx: &mut ReadContext<'_, '_>, key: &str) -> Option<Payload> {
    let value = ctx.bag.take_payload(key)?;
    if as_str(value) == Some("") {
        return None;
    }
    Some(non_message_payload(value, ctx.warnings))
}

#[cfg(test)]
mod tests {
    use cetra_core::enums::{Convention, SpanKind};
    use cetra_core::message::messages_to_json;
    use opentelemetry_proto::tonic::common::v1::any_value::Value;
    use opentelemetry_proto::tonic::common::v1::{AnyValue, KeyValue};
    use serde_json::json;

    use super::read;
    use crate::bag::{AttrBag, Use};
    use crate::payload::{Io, Payload, ReadContext, assemble};
    use crate::warn::Warnings;

    const VOLTAGENT: &[Convention] = &[Convention::Voltagent];
    const SCOPE: &str = "@voltagent/core";

    fn string_attr(key: &str, text: &str) -> KeyValue {
        KeyValue {
            key: key.to_owned(),
            value: Some(AnyValue {
                value: Some(Value::StringValue(text.to_owned())),
            }),
            key_strindex: 0,
        }
    }

    fn read_as<'a>(
        attrs: &'a [KeyValue],
        kind: SpanKind,
        conventions: &[Convention],
    ) -> (Io, AttrBag<'a>, Warnings) {
        let mut warnings = Warnings::default();
        let mut bag = AttrBag::new(attrs, &mut warnings);
        let io = read(&mut ReadContext {
            bag: &mut bag,
            events: &[],
            kind,
            scope: SCOPE,
            conventions,
            warnings: &mut warnings,
        });
        (io, bag, warnings)
    }

    #[test]
    fn plain_input_and_output_are_text_on_an_agent_span() {
        let attrs = [
            string_attr("input", "What's the weather in Paris?"),
            string_attr("output", "It is 18C and sunny in Paris."),
        ];
        let (io, bag, _) = read_as(&attrs, SpanKind::Agent, VOLTAGENT);
        assert_eq!(
            io.input,
            Some(Payload::Text("What's the weather in Paris?".to_owned()))
        );
        assert_eq!(
            io.output,
            Some(Payload::Text("It is 18C and sunny in Paris.".to_owned()))
        );
        assert_eq!(bag.state("input"), Some(Use::Payload));
        assert_eq!(bag.state("output"), Some(Use::Payload));
        assert!(io.errors.is_empty());
    }

    #[test]
    fn tool_spans_read_json_arguments_and_results() {
        let attrs = [
            string_attr("input", r#"{"city":"Paris"}"#),
            string_attr("output", r#"{"temperature_c":18,"city":"Paris"}"#),
        ];
        let (io, _, _) = read_as(&attrs, SpanKind::Tool, VOLTAGENT);
        assert_eq!(io.input, Some(Payload::Json(json!({"city": "Paris"}))));
        assert_eq!(
            io.output,
            Some(Payload::Json(json!({"temperature_c": 18, "city": "Paris"})))
        );
    }

    #[test]
    fn llm_messages_are_the_input_of_an_llm_span_ahead_of_plain_input() {
        let messages = r#"[{"role":"system","content":"You are a weather assistant."},{"role":"user","content":[{"type":"text","text":"What's the weather in Paris?"}]}]"#;
        let attrs = [
            string_attr("llm.messages", messages),
            string_attr("input", "ignored: the messages win"),
        ];
        let (io, bag, _) = read_as(&attrs, SpanKind::Llm, VOLTAGENT);
        let Some(Payload::Messages(converted)) = io.input else {
            panic!("expected messages, got {:?}", io.input);
        };
        assert_eq!(
            messages_to_json(&converted),
            concat!(
                r#"[{"role":"system","parts":[{"type":"text","content":"You are a weather assistant."}]},"#,
                r#"{"role":"user","parts":[{"type":"text","content":"What's the weather in Paris?"}]}]"#
            )
        );
        assert_eq!(bag.state("llm.messages"), Some(Use::Payload));
        // The plain key is marked as payload even when the messages win.
        assert_eq!(bag.state("input"), Some(Use::Payload));
        // An `llm` span has no output of its own.
        assert_eq!(io.output, None);
        assert!(io.errors.is_empty());
    }

    #[test]
    fn llm_messages_are_not_read_on_other_kinds() {
        let attrs = [
            string_attr("llm.messages", r#"[{"role":"user","content":"hi"}]"#),
            string_attr("input", "plain"),
        ];
        let (io, bag, _) = read_as(&attrs, SpanKind::Agent, VOLTAGENT);
        assert_eq!(io.input, Some(Payload::Text("plain".to_owned())));
        assert_eq!(bag.state("llm.messages"), Some(Use::Unread));
    }

    #[test]
    fn an_unconvertible_llm_messages_value_follows_the_failure_path() {
        // Not JSON: text, and the error code.
        let attrs = [string_attr("llm.messages", "not json")];
        let (io, _, _) = read_as(&attrs, SpanKind::Llm, VOLTAGENT);
        assert_eq!(io.input, Some(Payload::Text("not json".to_owned())));
        assert_eq!(io.errors, ["messages:voltagent:input"]);

        // JSON that is not a message array: the raw JSON.
        let attrs = [string_attr("llm.messages", r#"{"role":"user"}"#)];
        let (io, _, _) = read_as(&attrs, SpanKind::Llm, VOLTAGENT);
        assert_eq!(io.input, Some(Payload::Json(json!({"role": "user"}))));
        assert_eq!(io.errors, ["messages:voltagent:input"]);
    }

    #[test]
    fn empty_values_are_absent() {
        let attrs = [
            string_attr("input", ""),
            string_attr("output", ""),
            string_attr("llm.messages", ""),
        ];
        let (io, bag, _) = read_as(&attrs, SpanKind::Llm, VOLTAGENT);
        assert_eq!(io.input, None);
        assert_eq!(io.output, None);
        assert!(io.errors.is_empty());
        assert_eq!(bag.state("input"), Some(Use::Payload));
        assert_eq!(bag.state("llm.messages"), Some(Use::Payload));
    }

    #[test]
    fn nothing_is_read_without_the_voltagent_convention() {
        let attrs = [
            string_attr("input", "plain"),
            string_attr("output", "out"),
            string_attr("llm.messages", r#"[{"role":"user","content":"hi"}]"#),
        ];
        for conventions in [&[][..], &[Convention::Openinference][..]] {
            let (io, bag, _) = read_as(&attrs, SpanKind::Llm, conventions);
            assert_eq!(io, Io::default(), "{conventions:?}");
            assert_eq!(bag.state("input"), Some(Use::Unread));
            assert_eq!(bag.state("output"), Some(Use::Unread));
            assert_eq!(bag.state("llm.messages"), Some(Use::Unread));
        }
    }

    #[test]
    fn genai_input_wins_over_voltagent_plain_input() {
        // The driver keeps the first reader's value. Genai reads before Voltagent.
        let attrs = [
            string_attr(
                "gen_ai.input.messages",
                r#"[{"role":"user","parts":[{"type":"text","content":"from genai"}]}]"#,
            ),
            string_attr("input", "from voltagent"),
        ];
        let mut warnings = Warnings::default();
        let mut bag = AttrBag::new(&attrs, &mut warnings);
        let assembled = assemble(
            &mut bag,
            &[],
            SpanKind::Agent,
            SCOPE,
            &[Convention::Genai, Convention::Voltagent],
            &mut warnings,
            crate::conventions::READERS,
            crate::conventions::REGISTRIES,
        );
        let Some(Payload::Messages(messages)) = assembled.io.input else {
            panic!("expected genai messages");
        };
        assert_eq!(
            messages_to_json(&messages),
            r#"[{"role":"user","parts":[{"type":"text","content":"from genai"}]}]"#
        );
    }
}
