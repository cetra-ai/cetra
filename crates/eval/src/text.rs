//! Text extracted from a check variable, and the prompt rendering of a JSON value.
//!
//! Spec 06 § `check` defaults (D164) and § Rendering (D163). Extraction keeps a
//! string, the text parts of the last assistant message, or compact JSON.
//! Rendering keeps a string as itself, writes one line per message, and uses
//! compact JSON for every other value.

use cetra_core::message::{Message, Modality, Part, Role};
use serde_json::Value;

/// Parses `value` as a messages array.
///
/// Returns [`None`] when any element is not a [`Message`]. Later rendering uses
/// the same parse.
pub(crate) fn is_messages(value: &Value) -> Option<Vec<Message>> {
    serde_json::from_value(value.clone()).ok()
}

/// The text a check compares.
///
/// A JSON string is returned as itself, not as a quoted JSON string. A messages
/// array yields the text parts of the last `assistant` message, joined with
/// newlines. When that message has no text part it yields its media transcripts
/// instead (non-empty, joined the same way), or an empty string when it has
/// none. Every other value is compact JSON.
pub(crate) fn extract_text(value: &Value) -> String {
    if let Value::String(text) = value {
        return text.clone();
    }
    if let Some(messages) = is_messages(value) {
        return last_assistant_text(&messages);
    }
    // `Value` contains only JSON, so compact serialization cannot fail.
    serde_json::to_string(value).unwrap_or_default()
}

/// Renders a value the way a judge prompt sees it.
///
/// A string is copied. Null is empty. A messages array is one `role: …` line
/// per message: text and reasoning are their content, media is `[image]`,
/// `[audio]`, or `[file]` (`[audio: <transcript>]` and so on when the part has
/// a non-empty transcript), a tool call is `tool_call name(arguments)`, and a
/// tool result is `tool_result: ` plus the rendered response. Every other
/// value, including an unknown message part, is compact JSON.
pub(crate) fn render_value(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(text) => text.clone(),
        other => {
            if let Some(messages) = is_messages(other) {
                render_messages(&messages)
            } else {
                compact_json(other)
            }
        }
    }
}

fn render_messages(messages: &[Message]) -> String {
    messages
        .iter()
        .map(render_message)
        .collect::<Vec<_>>()
        .join("\n")
}

fn render_message(message: &Message) -> String {
    let body = message
        .parts
        .iter()
        .map(render_part)
        .collect::<Vec<_>>()
        .join(" ");
    format!("{}: {body}", message.role.as_str())
}

fn render_part(part: &Part) -> String {
    match part {
        Part::Text { content, .. } | Part::Reasoning { content, .. } => content.clone(),
        Part::ToolCall {
            name, arguments, ..
        } => match arguments {
            Some(arguments) => format!("tool_call {name}({})", compact_json(arguments)),
            None => format!("tool_call {name}()"),
        },
        Part::ToolCallResponse { response, .. } => {
            format!("tool_result: {}", render_value(response))
        }
        Part::Uri {
            modality,
            transcript,
            ..
        }
        | Part::Blob {
            modality,
            transcript,
            ..
        } => media_placeholder(modality, transcript.as_deref()),
        Part::Unknown(value) => compact_json(value),
    }
}

/// Image and audio keep their labels. Video and any other modality are `[file]`.
/// A non-empty transcript is appended after `: `: `[audio: <transcript>]`.
/// A dropped part has no modality, so a dropped transcript renders as `[file: …]`.
fn media_placeholder(modality: &Option<Modality>, transcript: Option<&str>) -> String {
    let kind = match modality {
        Some(Modality::Image) => "image",
        Some(Modality::Audio) => "audio",
        _ => "file",
    };
    match transcript {
        Some(text) if !text.is_empty() => format!("[{kind}: {text}]"),
        _ => format!("[{kind}]"),
    }
}

fn compact_json(value: &Value) -> String {
    // `Value` contains only JSON, so compact serialization cannot fail.
    serde_json::to_string(value).unwrap_or_default()
}

fn last_assistant_text(messages: &[Message]) -> String {
    let Some(message) = messages
        .iter()
        .rev()
        .find(|message| message.role == Role::Assistant)
    else {
        return String::new();
    };
    let texts: Vec<&str> = message
        .parts
        .iter()
        .filter_map(|part| match part {
            Part::Text { content, .. } => Some(content.as_str()),
            Part::Reasoning { .. }
            | Part::ToolCall { .. }
            | Part::ToolCallResponse { .. }
            | Part::Uri { .. }
            | Part::Blob { .. }
            | Part::Unknown(_) => None,
        })
        .collect();
    if !texts.is_empty() {
        return texts.join("\n");
    }
    message
        .parts
        .iter()
        .filter_map(|part| match part {
            Part::Uri { transcript, .. } | Part::Blob { transcript, .. } => transcript.as_deref(),
            _ => None,
        })
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::extract_text;

    #[test]
    fn extracts_text_by_shape() {
        assert_eq!(extract_text(&json!("hello")), "hello");
        assert_eq!(
            extract_text(&json!(
                r#"[{"role":"assistant","parts":[{"type":"text","content":"hidden"}]}]"#
            )),
            r#"[{"role":"assistant","parts":[{"type":"text","content":"hidden"}]}]"#
        );
        assert_eq!(extract_text(&json!(12)), "12");
        assert_eq!(extract_text(&json!({"b": 1, "a": 2})), r#"{"b":1,"a":2}"#);
        assert_eq!(extract_text(&json!([1, 2])), "[1,2]");

        let two_assistants = json!([
            {"role": "assistant", "parts": [{"type": "text", "content": "first"}]},
            {"role": "user", "parts": [{"type": "text", "content": "q"}]},
            {
                "role": "assistant",
                "parts": [
                    {"type": "text", "content": "second"},
                    {"type": "reasoning", "content": "scratch"},
                    {"type": "text", "content": "more"}
                ]
            },
            {"role": "user", "parts": [{"type": "text", "content": "tail"}]}
        ]);
        assert_eq!(extract_text(&two_assistants), "second\nmore");

        let no_assistant = json!([
            {"role": "user", "parts": [{"type": "text", "content": "hi"}]}
        ]);
        assert_eq!(extract_text(&no_assistant), "");

        let tool_only = json!([
            {"role": "assistant", "parts": [{"type": "tool_call", "name": "lookup"}]}
        ]);
        assert_eq!(extract_text(&tool_only), "");
    }

    #[test]
    fn media_renders_its_transcript() {
        assert_eq!(
            super::render_value(&json!([
                {"role": "assistant", "parts": [
                    {"type": "blob", "modality": "audio", "content": "AAAA", "transcript": "hello"}
                ]}
            ])),
            "assistant: [audio: hello]"
        );
    }

    #[test]
    fn an_empty_transcript_renders_the_plain_placeholder() {
        assert_eq!(
            super::render_value(&json!([
                {"role": "assistant", "parts": [
                    {"type": "uri", "modality": "audio", "uri": "media://x", "transcript": ""}
                ]}
            ])),
            "assistant: [audio]"
        );
    }

    #[test]
    fn a_voice_only_reply_extracts_its_transcript() {
        let reply = json!([
            {"role": "user", "parts": [{"type": "text", "content": "hi"}]},
            {"role": "assistant", "parts": [
                {"type": "blob", "modality": "audio", "content": "AAAA", "transcript": "first"},
                {"type": "uri", "modality": "audio", "uri": "media://x"},
                {"type": "uri", "modality": "audio", "uri": "media://y", "transcript": "second"}
            ]}
        ]);
        assert_eq!(extract_text(&reply), "first\nsecond");
    }

    #[test]
    fn text_wins_over_transcripts_in_extraction() {
        let reply = json!([
            {"role": "assistant", "parts": [
                {"type": "blob", "modality": "audio", "content": "AAAA", "transcript": "spoken"},
                {"type": "text", "content": "written"}
            ]}
        ]);
        assert_eq!(extract_text(&reply), "written");
    }

    #[test]
    fn renders_every_part_and_other_json() {
        let planted = "SECRET-PAYLOAD";
        let messages = json!([
            {
                "role": "user",
                "parts": [
                    {"type": "text", "content": "see"},
                    {"type": "uri", "modality": "image", "uri": format!("media://{planted}")},
                    {"type": "blob", "modality": "audio", "content": planted},
                    {"type": "uri", "modality": "video", "uri": format!("https://example.test/{planted}")},
                    {"type": "blob", "modality": "file", "content": planted},
                    {"type": "uri", "uri": format!("media://{planted}")}
                ]
            },
            {
                "role": "assistant",
                "parts": [
                    {"type": "reasoning", "content": "thinking"},
                    {"type": "tool_call", "name": "get_weather", "arguments": {"city": "Paris"}},
                    {"type": "tool_call", "name": "bare"},
                    {"type": "tool_call", "name": "empty", "arguments": null}
                ]
            },
            {
                "role": "tool",
                "parts": [
                    {"type": "tool_call_response", "response": "sunny"},
                    {"type": "tool_call_response", "response": {"ok": true}},
                    {"type": "tool_call_response", "response": null}
                ]
            },
            {
                "role": "custom",
                "parts": [{"type": "widget", "value": 1}]
            },
            {"role": "user"}
        ]);
        assert_eq!(
            super::render_value(&messages),
            "\
user: see [image] [audio] [file] [file] [file]
assistant: thinking tool_call get_weather({\"city\":\"Paris\"}) tool_call bare() tool_call empty(null)
tool: tool_result: sunny tool_result: {\"ok\":true} tool_result: 
custom: {\"type\":\"widget\",\"value\":1}
user: "
        );
        assert!(!super::render_value(&messages).contains(planted));

        assert_eq!(super::render_value(&json!("hello\nthere")), "hello\nthere");
        assert_eq!(super::render_value(&Value::Null), "");
        assert_eq!(super::render_value(&json!(12)), "12");
        assert_eq!(
            super::render_value(&json!({"b": 1, "a": 2})),
            r#"{"b":1,"a":2}"#
        );
        assert_eq!(super::render_value(&json!([1, 2])), "[1,2]");
        assert_eq!(
            super::render_value(&json!([{"not": "a message"}])),
            r#"[{"not":"a message"}]"#
        );
    }
}
