//! Bedrock Converse content blocks, as Strands writes them on its legacy events.
//!
//! Spec 02 § Input and output, the GenAI legacy events row. A block is a
//! `text`, a `toolUse` or a `toolResult` object. Any other block is an error,
//! and the caller stores the raw events with the `messages:genai:<field>` code.

use cetra_core::message::{Message, Part, Role};
use serde_json::{Map, Value as Json};

use crate::convert::ConvertError;
use crate::payload::parse_tool_arguments;

/// Convert one message from an array of Converse content blocks.
///
/// `text` is a text part, `toolUse` a tool call (`input` is the arguments) and
/// `toolResult` a tool response: the joined text of its text blocks, else the
/// JSON of its `content`; its `status` stays in the part's `extra`.
///
/// # Errors
///
/// [`ConvertError::NotAMessage`] with the block's index when a block is none
/// of `text`, `toolUse` and `toolResult`, or when a modelled field of one is
/// not of the expected JSON type.
pub(crate) fn converse_message(role: Role, blocks: &[Json]) -> Result<Message, ConvertError> {
    let mut parts = Vec::with_capacity(blocks.len());
    for (index, block) in blocks.iter().enumerate() {
        parts.push(part(block, index)?);
    }
    Ok(Message {
        role,
        name: None,
        parts,
        finish_reason: None,
        extra: Map::new(),
    })
}

/// The `text` of the text blocks among `blocks`, joined with `\n`.
///
/// `None` when there is none.
pub(crate) fn converse_text(blocks: &[Json]) -> Option<String> {
    let texts: Vec<&str> = blocks
        .iter()
        .filter_map(|block| block.get("text").and_then(Json::as_str))
        .collect();
    (!texts.is_empty()).then(|| texts.join("\n"))
}

fn part(block: &Json, index: usize) -> Result<Part, ConvertError> {
    let Json::Object(block) = block else {
        return Err(ConvertError::NotAMessage { index });
    };
    if let Some(text) = block.get("text") {
        let content = text
            .as_str()
            .ok_or(ConvertError::NotAMessage { index })?
            .to_owned();
        let mut extra = block.clone();
        extra.shift_remove("text");
        return Ok(Part::Text { content, extra });
    }
    if let Some(tool_use) = block.get("toolUse") {
        return tool_call(block, tool_use, index);
    }
    if let Some(tool_result) = block.get("toolResult") {
        return tool_response(block, tool_result, index);
    }
    Err(ConvertError::NotAMessage { index })
}

fn tool_call(
    block: &Map<String, Json>,
    tool_use: &Json,
    index: usize,
) -> Result<Part, ConvertError> {
    let Json::Object(tool_use) = tool_use else {
        return Err(ConvertError::NotAMessage { index });
    };
    let Some(name) = take_string(tool_use, "name") else {
        return Err(ConvertError::NotAMessage { index });
    };
    let id = take_string(tool_use, "toolUseId");
    let arguments = tool_use.get("input").cloned();
    let extra = extra(block, "toolUse", tool_use, &["toolUseId", "name", "input"]);
    Ok(Part::ToolCall {
        id,
        name,
        arguments,
        extra,
    })
}

fn tool_response(
    block: &Map<String, Json>,
    tool_result: &Json,
    index: usize,
) -> Result<Part, ConvertError> {
    let Json::Object(tool_result) = tool_result else {
        return Err(ConvertError::NotAMessage { index });
    };
    let id = take_string(tool_result, "toolUseId");
    let response = match tool_result.get("content") {
        Some(Json::Array(nested)) => match converse_text(nested) {
            Some(text) => Json::String(text),
            None => Json::Array(nested.clone()),
        },
        Some(other) => other.clone(),
        None => Json::Null,
    };
    let extra = extra(block, "toolResult", tool_result, &["toolUseId", "content"]);
    Ok(Part::ToolCallResponse {
        id,
        response: parse_tool_arguments(response),
        extra,
    })
}

/// The block without `kind`, then the fields of `nested` that `modelled` does
/// not name, so that unmodelled fields round-trip (D65).
fn extra(
    block: &Map<String, Json>,
    kind: &str,
    nested: &Map<String, Json>,
    modelled: &[&str],
) -> Map<String, Json> {
    let mut extra = block.clone();
    extra.shift_remove(kind);
    extra.extend(
        nested
            .iter()
            .filter(|(key, _)| !modelled.contains(&key.as_str()))
            .map(|(key, value)| (key.clone(), value.clone())),
    );
    extra
}

fn take_string(object: &Map<String, Json>, key: &str) -> Option<String> {
    match object.get(key) {
        Some(Json::String(text)) if !text.is_empty() => Some(text.clone()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{converse_message, converse_text};
    use crate::convert::ConvertError;
    use cetra_core::message::Role;
    use cetra_core::message::messages_to_json;
    use serde_json::json;

    fn message(role: Role, blocks: &serde_json::Value) -> cetra_core::message::Message {
        converse_message(role, blocks.as_array().expect("blocks")).expect("message")
    }

    #[test]
    fn text_tool_use_and_tool_result_become_typed_parts() {
        assert_eq!(
            messages_to_json(&[message(
                Role::System,
                &json!([{"text": "You are a weather bot."}])
            )]),
            r#"[{"role":"system","parts":[{"type":"text","content":"You are a weather bot."}]}]"#
        );
        assert_eq!(
            messages_to_json(&[message(
                Role::Assistant,
                &json!([{"toolUse": {"toolUseId": "call_1", "name": "get_weather", "input": {"city": "Paris"}}}])
            )]),
            r#"[{"role":"assistant","parts":[{"type":"tool_call","id":"call_1","name":"get_weather","arguments":{"city":"Paris"}}]}]"#
        );
        assert_eq!(
            messages_to_json(&[message(
                Role::Tool,
                &json!([{"toolResult": {"toolUseId": "call_1", "status": "success", "content": [{"text": "18C sunny in Paris"}]}}])
            )]),
            r#"[{"role":"tool","parts":[{"type":"tool_call_response","id":"call_1","response":"18C sunny in Paris","status":"success"}]}]"#
        );
    }

    #[test]
    fn mixed_blocks_keep_their_order_in_one_message() {
        assert_eq!(
            messages_to_json(&[message(
                Role::Assistant,
                &json!([
                    {"text": "Let me look."},
                    {"toolUse": {"toolUseId": "call_1", "name": "get_weather", "input": {"city": "Paris"}}},
                    {"text": "One moment."}
                ])
            )]),
            concat!(
                r#"[{"role":"assistant","parts":["#,
                r#"{"type":"text","content":"Let me look."},"#,
                r#"{"type":"tool_call","id":"call_1","name":"get_weather","arguments":{"city":"Paris"}},"#,
                r#"{"type":"text","content":"One moment."}]}]"#
            )
        );
    }

    #[test]
    fn a_tool_result_without_text_blocks_is_the_json_of_its_content() {
        assert_eq!(
            messages_to_json(&[message(
                Role::Tool,
                &json!([{"toolResult": {"toolUseId": "call_1", "content": [{"json": {"temp": 18}}]}}])
            )]),
            r#"[{"role":"tool","parts":[{"type":"tool_call_response","id":"call_1","response":[{"json":{"temp":18}}]}]}]"#
        );
        // No `content` at all.
        assert_eq!(
            messages_to_json(&[message(
                Role::Tool,
                &json!([{"toolResult": {"toolUseId": "call_2"}}])
            )]),
            r#"[{"role":"tool","parts":[{"type":"tool_call_response","id":"call_2","response":null}]}]"#
        );
    }

    #[test]
    fn a_tool_result_joins_its_text_blocks_and_ignores_the_others() {
        assert_eq!(
            messages_to_json(&[message(
                Role::Tool,
                &json!([{"toolResult": {"toolUseId": "call_1", "content": [
                    {"text": "18C"},
                    {"image": {"format": "png"}},
                    {"text": "sunny"}
                ]}}])
            )]),
            r#"[{"role":"tool","parts":[{"type":"tool_call_response","id":"call_1","response":"18C\nsunny"}]}]"#
        );
        // A text block that is JSON is parsed (rule 6).
        assert_eq!(
            messages_to_json(&[message(
                Role::Tool,
                &json!([{"toolResult": {"toolUseId": "call_2", "content": [{"text": "{\"temp\":18}"}]}}])
            )]),
            r#"[{"role":"tool","parts":[{"type":"tool_call_response","id":"call_2","response":{"temp":18}}]}]"#
        );
    }

    #[test]
    fn a_tool_use_without_input_or_id_is_still_a_call() {
        assert_eq!(
            messages_to_json(&[message(
                Role::Assistant,
                &json!([{"toolUse": {"name": "get_time"}}])
            )]),
            r#"[{"role":"assistant","parts":[{"type":"tool_call","name":"get_time"}]}]"#
        );
    }

    #[test]
    fn converse_text_is_the_joined_text_blocks_or_none() {
        let mixed = json!([{"text": "a"}, {"toolUse": {}}, {"text": "b"}]);
        assert_eq!(
            converse_text(mixed.as_array().expect("blocks")).as_deref(),
            Some("a\nb")
        );
        let tool_only = json!([{"toolUse": {}}]);
        assert_eq!(converse_text(tool_only.as_array().expect("blocks")), None);
        assert_eq!(converse_text(&[]), None);
    }

    #[test]
    fn an_unknown_or_malformed_block_is_an_error_with_its_index() {
        // (blocks, index of the offending block)
        let cases = [
            (json!([{"text": "ok"}, {"image": {}}]), 1),
            (json!([{"text": "ok"}, {"text": 5}]), 1),
            (json!([{}, {"text": "ok"}]), 0),
            (json!(["text"]), 0),
            (json!([{"toolResult": "done"}]), 0),
            (
                json!([{"toolUse": {"name": "lookup"}}, {"toolUseId": "x"}]),
                1,
            ),
        ];
        for (blocks, index) in cases {
            let error = converse_message(Role::Assistant, blocks.as_array().expect("blocks"))
                .expect_err("not a Converse block");
            assert_eq!(error, ConvertError::NotAMessage { index });
        }
    }
}
