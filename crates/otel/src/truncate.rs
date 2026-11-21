//! Truncation of payloads and span strings.
//!
//! Spec 02 § Truncation. Runs after media extraction. A string whose byte
//! length equals the limit is kept. A longer string is cut on a UTF-8 boundary
//! and the marker is appended, so the stored value is longer than the limit.

use std::collections::{BTreeMap, BTreeSet};

use cetra_core::message::{Message, Part, Role, messages_to_json};
use cetra_core::names::marker::TRUNCATED_MESSAGES_PREFIX;
use cetra_core::names::marker::TRUNCATED_STRING_SUFFIX_PREFIX;
use cetra_core::span::Span;
use serde_json::{Map, Value as Json};

use crate::payload::{Io, Payload};

/// Step 2 does not cut a string leaf below this many bytes.
const LEAF_FLOOR: usize = 1024;

/// Truncate `input` and `output`. A messages list that still does not fit
/// becomes text and records `truncate:<field>`.
pub(crate) fn truncate_payloads(io: &mut Io, limit: usize, names: &mut Vec<String>) {
    truncate_column(
        &mut io.input,
        limit,
        "input",
        "truncate:input",
        &mut io.errors,
        names,
    );
    truncate_column(
        &mut io.output,
        limit,
        "output",
        "truncate:output",
        &mut io.errors,
        names,
    );
}

fn truncate_column(
    slot: &mut Option<Payload>,
    limit: usize,
    name: &str,
    error: &'static str,
    errors: &mut Vec<&'static str>,
    names: &mut Vec<String>,
) {
    let Some(payload) = slot.take() else {
        return;
    };
    let (next, change) = apply_payload(payload, limit);
    *slot = Some(next);
    match change {
        Change::Same => {}
        Change::Cut => names.push(name.to_owned()),
        Change::Text => {
            names.push(name.to_owned());
            errors.push(error);
        }
    }
}

enum Change {
    Same,
    Cut,
    Text,
}

fn apply_payload(payload: Payload, limit: usize) -> (Payload, Change) {
    match payload {
        Payload::Messages(messages) => match fit_messages(messages, limit) {
            Fitted::Same(messages) => (Payload::Messages(messages), Change::Same),
            Fitted::Messages(messages) => (Payload::Messages(messages), Change::Cut),
            Fitted::Text(text) => (Payload::Text(text), Change::Text),
        },
        Payload::Json(value) => {
            let text = serde_json::to_string(&value).unwrap_or_else(|_| "null".to_owned());
            if text.len() <= limit {
                (Payload::Json(value), Change::Same)
            } else {
                (Payload::Text(cut_string(&text, limit)), Change::Cut)
            }
        }
        Payload::Text(text) => {
            if text.len() <= limit {
                (Payload::Text(text), Change::Same)
            } else {
                (Payload::Text(cut_string(&text, limit)), Change::Cut)
            }
        }
    }
}

enum Fitted {
    Same(Vec<Message>),
    Messages(Vec<Message>),
    Text(String),
}

fn fit_messages(messages: Vec<Message>, limit: usize) -> Fitted {
    if messages_to_json(&messages).len() <= limit {
        return Fitted::Same(messages);
    }
    let mut kept = keep_prefix_and_suffix(&messages, limit);
    if messages_to_json(&kept).len() <= limit {
        return Fitted::Messages(kept);
    }
    shrink_leaves(&mut kept, limit);
    if messages_to_json(&kept).len() <= limit {
        return Fitted::Messages(kept);
    }
    Fitted::Text(cut_string(&messages_to_json(&kept), limit))
}

/// Protected messages plus the longest suffix whose serialized form fits.
fn keep_prefix_and_suffix(messages: &[Message], limit: usize) -> Vec<Message> {
    let protected = protected_indices(messages);
    let fallback = assemble(messages, &protected, messages.len());
    for keep_from in 1..=messages.len() {
        let candidate = assemble(messages, &protected, keep_from);
        if messages_to_json(&candidate).len() <= limit {
            return candidate;
        }
    }
    fallback
}

fn protected_indices(messages: &[Message]) -> Vec<bool> {
    let mut flags = vec![false; messages.len()];
    let mut saw_user = false;
    for (index, message) in messages.iter().enumerate() {
        if message.role == Role::System {
            flags[index] = true;
        }
        if !saw_user && message.role == Role::User {
            flags[index] = true;
            saw_user = true;
        }
    }
    flags
}

/// `keep_from` is the first index of the kept suffix. Protected messages stay
/// wherever they are. Each removed run becomes one placeholder.
fn assemble(messages: &[Message], protected: &[bool], keep_from: usize) -> Vec<Message> {
    let mut out = Vec::new();
    let mut removed = 0usize;
    for (index, message) in messages.iter().enumerate() {
        let keep = protected[index] || index >= keep_from;
        if keep {
            if removed > 0 {
                out.push(placeholder(removed));
                removed = 0;
            }
            out.push(message.clone());
        } else {
            removed += 1;
        }
    }
    if removed > 0 {
        out.push(placeholder(removed));
    }
    out
}

fn placeholder(removed: usize) -> Message {
    Message {
        role: Role::System,
        name: None,
        parts: vec![Part::Text {
            content: format!("{TRUNCATED_MESSAGES_PREFIX}{removed} messages truncated]"),
            extra: Map::new(),
        }],
        finish_reason: None,
        extra: Map::new(),
    }
}

fn shrink_leaves(messages: &mut [Message], limit: usize) {
    loop {
        if messages_to_json(messages).len() <= limit {
            return;
        }
        let Some(pos) = longest_leaf(messages) else {
            return;
        };
        let original = leaf_text(messages, pos).to_owned();
        let marker = string_marker(original.len());
        let mut best = None;
        let mut low = LEAF_FLOOR;
        let mut high = original.len();
        while low <= high {
            let mid = low + (high - low) / 2;
            set_leaf(messages, pos, marked(&original, mid, &marker));
            if messages_to_json(messages).len() <= limit {
                best = Some(mid);
                if mid == original.len() {
                    break;
                }
                low = mid + 1;
            } else if mid == 0 {
                break;
            } else {
                high = mid - 1;
            }
        }
        let cut_at = best.unwrap_or(LEAF_FLOOR);
        set_leaf(messages, pos, marked(&original, cut_at, &marker));
        if best.is_some() {
            return;
        }
    }
}

#[derive(Clone, Copy)]
enum LeafPos {
    Text { message: usize, part: usize },
    Reasoning { message: usize, part: usize },
    Arguments { message: usize, part: usize },
    Response { message: usize, part: usize },
}

fn longest_leaf(messages: &[Message]) -> Option<LeafPos> {
    let mut best: Option<(usize, LeafPos)> = None;
    for (message_index, message) in messages.iter().enumerate() {
        for (part_index, part) in message.parts.iter().enumerate() {
            let found = match part {
                Part::Text { content, .. } if leaf_open(content) => Some(LeafPos::Text {
                    message: message_index,
                    part: part_index,
                }),
                Part::Reasoning { content, .. } if leaf_open(content) => Some(LeafPos::Reasoning {
                    message: message_index,
                    part: part_index,
                }),
                Part::ToolCall {
                    arguments: Some(Json::String(text)),
                    ..
                } if leaf_open(text) => Some(LeafPos::Arguments {
                    message: message_index,
                    part: part_index,
                }),
                Part::ToolCallResponse {
                    response: Json::String(text),
                    ..
                } if leaf_open(text) => Some(LeafPos::Response {
                    message: message_index,
                    part: part_index,
                }),
                _ => None,
            };
            if let Some(pos) = found {
                let len = leaf_text(messages, pos).len();
                if best.is_none_or(|(best_len, _)| len > best_len) {
                    best = Some((len, pos));
                }
            }
        }
    }
    best.map(|(_, pos)| pos)
}

fn leaf_open(text: &str) -> bool {
    text.len() > LEAF_FLOOR && !text.contains(TRUNCATED_STRING_SUFFIX_PREFIX)
}

fn leaf_text(messages: &[Message], pos: LeafPos) -> &str {
    let missing = "";
    let Some(message) = messages.get(message_index(pos)) else {
        return missing;
    };
    let Some(part) = message.parts.get(part_index(pos)) else {
        return missing;
    };
    match (pos, part) {
        (LeafPos::Text { .. }, Part::Text { content, .. })
        | (LeafPos::Reasoning { .. }, Part::Reasoning { content, .. }) => content,
        (
            LeafPos::Arguments { .. },
            Part::ToolCall {
                arguments: Some(Json::String(text)),
                ..
            },
        )
        | (
            LeafPos::Response { .. },
            Part::ToolCallResponse {
                response: Json::String(text),
                ..
            },
        ) => text,
        _ => missing,
    }
}

fn set_leaf(messages: &mut [Message], pos: LeafPos, value: String) {
    let Some(message) = messages.get_mut(message_index(pos)) else {
        return;
    };
    let Some(part) = message.parts.get_mut(part_index(pos)) else {
        return;
    };
    match (pos, part) {
        (LeafPos::Text { .. }, Part::Text { content, .. })
        | (LeafPos::Reasoning { .. }, Part::Reasoning { content, .. }) => *content = value,
        (
            LeafPos::Arguments { .. },
            Part::ToolCall {
                arguments: Some(Json::String(text)),
                ..
            },
        )
        | (
            LeafPos::Response { .. },
            Part::ToolCallResponse {
                response: Json::String(text),
                ..
            },
        ) => *text = value,
        _ => {}
    }
}

fn message_index(pos: LeafPos) -> usize {
    match pos {
        LeafPos::Text { message, .. }
        | LeafPos::Reasoning { message, .. }
        | LeafPos::Arguments { message, .. }
        | LeafPos::Response { message, .. } => message,
    }
}

fn part_index(pos: LeafPos) -> usize {
    match pos {
        LeafPos::Text { part, .. }
        | LeafPos::Reasoning { part, .. }
        | LeafPos::Arguments { part, .. }
        | LeafPos::Response { part, .. } => part,
    }
}

fn marked(original: &str, cut_at: usize, marker: &str) -> String {
    let end = original.floor_char_boundary(cut_at.min(original.len()));
    let mut out = String::with_capacity(end + marker.len());
    out.push_str(&original[..end]);
    out.push_str(marker);
    out
}

/// Cut every string column of `span` that is longer than `limit` bytes, and
/// record its column name in `truncated`. Spec 02 § Truncation.
///
/// `input` and `output` are cut by `truncate_payloads` before the span is built.
/// `input_preview` and `output_preview` come from those cut payloads, so they
/// are already within the limit. Map keys are not cut, only map values.
/// Sort `truncated` and drop duplicates.
pub(crate) fn truncate_columns(span: &mut Span, limit: usize) {
    let names = &mut span.truncated;
    cut_field(&mut span.name, "name", limit, names);
    let optional: [(&str, &mut Option<String>); 16] = [
        ("status_message", &mut span.status_message),
        ("service_name", &mut span.service_name),
        ("session_id", &mut span.session_id),
        ("user_id", &mut span.user_id),
        ("environment", &mut span.environment),
        ("release", &mut span.release),
        ("provider", &mut span.provider),
        ("request_model", &mut span.request_model),
        ("response_model", &mut span.response_model),
        ("prompt_name", &mut span.prompt_name),
        ("operation", &mut span.operation),
        ("request_params", &mut span.request_params),
        ("scope_name", &mut span.scope_name),
        ("scope_version", &mut span.scope_version),
        ("system_instructions", &mut span.system_instructions),
        ("tool_definitions", &mut span.tool_definitions),
    ];
    for (column, text) in optional {
        cut_optional(text, column, limit, names);
    }
    truncate_map(&mut span.attributes, "", names, limit);
    truncate_map(&mut span.resource_attributes, "", names, limit);
    truncate_map(&mut span.metadata, "metadata.", names, limit);
    let mut tags_cut = false;
    for tag in &mut span.tags {
        if let Some(cut) = cut_if_over(tag, limit) {
            *tag = cut;
            tags_cut = true;
            names.push("tags".to_owned());
        }
    }
    if tags_cut {
        // Two different long tags can share a cut prefix. Keep the first of each.
        let mut seen = BTreeSet::new();
        span.tags.retain(|tag| seen.insert(tag.clone()));
    }
    for (index, event) in span.events.iter_mut().enumerate() {
        cut_field(
            &mut event.name,
            &format!("events[{index}].name"),
            limit,
            names,
        );
        cut_optional(
            &mut event.attributes,
            &format!("events[{index}].attributes"),
            limit,
            names,
        );
    }
    for (index, link) in span.links.iter_mut().enumerate() {
        cut_optional(
            &mut link.attributes,
            &format!("links[{index}].attributes"),
            limit,
            names,
        );
    }
    span.truncated.sort();
    span.truncated.dedup();
}

fn cut_field(text: &mut String, column: &str, limit: usize, names: &mut Vec<String>) {
    if let Some(cut) = cut_if_over(text, limit) {
        *text = cut;
        names.push(column.to_owned());
    }
}

fn cut_optional(text: &mut Option<String>, column: &str, limit: usize, names: &mut Vec<String>) {
    if let Some(text) = text.as_mut() {
        cut_field(text, column, limit, names);
    }
}

/// Cut each value. The column name is `prefix` followed by the key.
fn truncate_map(
    map: &mut BTreeMap<String, String>,
    prefix: &str,
    names: &mut Vec<String>,
    limit: usize,
) {
    for (key, value) in map.iter_mut() {
        if let Some(cut) = cut_if_over(value, limit) {
            *value = cut;
            names.push(format!("{prefix}{key}"));
        }
    }
}

fn cut_if_over(text: &str, limit: usize) -> Option<String> {
    if text.len() <= limit {
        None
    } else {
        Some(cut_string(text, limit))
    }
}

fn cut_string(text: &str, limit: usize) -> String {
    let end = text.floor_char_boundary(limit.min(text.len()));
    let marker = string_marker(text.len());
    let mut out = String::with_capacity(end + marker.len());
    out.push_str(&text[..end]);
    out.push_str(&marker);
    out
}

fn string_marker(original_bytes: usize) -> String {
    format!("{TRUNCATED_STRING_SUFFIX_PREFIX}{original_bytes} bytes]")
}

#[cfg(test)]
mod tests {
    use super::*;
    use cetra_core::message::Role;

    fn user(content: &str) -> Message {
        Message {
            role: Role::User,
            name: None,
            parts: vec![Part::Text {
                content: content.to_owned(),
                extra: Map::new(),
            }],
            finish_reason: None,
            extra: Map::new(),
        }
    }

    fn system(content: &str) -> Message {
        Message {
            role: Role::System,
            name: None,
            parts: vec![Part::Text {
                content: content.to_owned(),
                extra: Map::new(),
            }],
            finish_reason: None,
            extra: Map::new(),
        }
    }

    fn assistant(content: &str) -> Message {
        Message {
            role: Role::Assistant,
            name: None,
            parts: vec![Part::Text {
                content: content.to_owned(),
                extra: Map::new(),
            }],
            finish_reason: None,
            extra: Map::new(),
        }
    }

    #[test]
    fn a_four_byte_character_is_not_split() {
        let text = "𝄞𝄞";
        assert_eq!(text.len(), 8);
        let cut = cut_string(text, 6);
        assert!(cut.starts_with('𝄞'));
        assert_eq!(cut.chars().filter(|c| *c == '𝄞').count(), 1);
        assert!(cut.contains(TRUNCATED_STRING_SUFFIX_PREFIX));
        assert!(cut.ends_with("8 bytes]"));
    }

    #[test]
    fn the_limit_is_inclusive() {
        assert_eq!(cut_if_over("abcd", 4), None);
        let cut = cut_if_over("abcde", 4).expect("over");
        assert!(cut.starts_with("abcd"));
        assert!(cut.ends_with("5 bytes]"));
    }

    #[test]
    fn the_middle_of_a_message_list_is_replaced() {
        let messages = vec![
            system("rules"),
            user("hello"),
            assistant("one"),
            assistant("two"),
            assistant("three"),
            assistant("end"),
        ];
        let full = messages_to_json(&messages).len();
        let protected = protected_indices(&messages);
        let suffix = assemble(&messages, &protected, 5);
        let limit = messages_to_json(&suffix).len();
        assert!(limit < full);
        let fitted = fit_messages(messages, limit);
        let Fitted::Messages(kept) = fitted else {
            panic!("expected a message list");
        };
        let json = messages_to_json(&kept);
        assert!(json.len() <= limit, "{json}");
        assert!(json.contains("[cetra: 3 messages truncated]"), "{json}");
        assert!(json.contains("\"content\":\"rules\""), "{json}");
        assert!(json.contains("\"content\":\"hello\""), "{json}");
        assert!(json.contains("\"content\":\"end\""), "{json}");
        assert!(!json.contains("\"content\":\"one\""), "{json}");
        let fixture = vec![
            system("rules"),
            user("hello"),
            assistant("one"),
            assistant("two"),
            assistant("end"),
        ];
        let kept = assemble(&fixture, &protected_indices(&fixture), 4);
        assert_eq!(
            messages_to_json(&kept).len(),
            272,
            "{}",
            messages_to_json(&kept)
        );
    }

    #[test]
    fn a_system_message_between_gaps_makes_two_placeholders() {
        let messages = vec![
            system("rules"),
            user("hello"),
            assistant(&"one".repeat(40)),
            system("mid"),
            assistant(&"two".repeat(40)),
            assistant("end"),
        ];
        let protected = protected_indices(&messages);
        let kept = assemble(&messages, &protected, 5);
        let json = messages_to_json(&kept);
        assert_eq!(
            json.matches("[cetra: 1 messages truncated]").count(),
            2,
            "{json}"
        );
        let limit = json.len();
        let Fitted::Messages(fitted) = fit_messages(messages, limit) else {
            panic!("step 1 keeps messages");
        };
        assert_eq!(messages_to_json(&fitted), json);
    }

    #[test]
    fn json_over_the_limit_becomes_text_without_an_error() {
        let mut names = Vec::new();
        let mut io = Io {
            input: Some(Payload::Json(
                serde_json::json!({"query": "abcdefghijklmnopqrstuvwxyz0123456789"}),
            )),
            ..Io::default()
        };
        truncate_payloads(&mut io, 32, &mut names);
        assert_eq!(names, ["input"]);
        assert!(io.errors.is_empty());
        let Some(Payload::Text(text)) = io.input else {
            panic!("text");
        };
        assert_eq!(
            text,
            "{\"query\":\"abcdefghijklmnopqrstuv…[cetra: truncated, original 48 bytes]"
        );
    }

    #[test]
    fn a_huge_message_is_cut_at_its_leaf() {
        let content = "x".repeat(4_000);
        let messages = vec![user(&content)];
        let fitted = fit_messages(messages, 1_500);
        let Fitted::Messages(kept) = fitted else {
            panic!("step 2 keeps messages");
        };
        let json = messages_to_json(&kept);
        assert!(json.len() <= 1_500, "{}", json.len());
        assert!(json.contains(TRUNCATED_STRING_SUFFIX_PREFIX), "{json}");
        assert!(json.contains("4000 bytes]"), "{json}");
    }

    #[test]
    fn a_list_that_cannot_fit_becomes_text() {
        let mut names = Vec::new();
        let mut io = Io {
            input: Some(Payload::Messages(vec![user(&"y".repeat(4_000))])),
            ..Io::default()
        };
        truncate_payloads(&mut io, 64, &mut names);
        assert_eq!(names, ["input"]);
        assert_eq!(io.errors, ["truncate:input"]);
        let Some(Payload::Text(text)) = io.input else {
            panic!("text fallback");
        };
        assert!(text.len() > 64, "{}", text.len());
        assert!(text.contains(TRUNCATED_STRING_SUFFIX_PREFIX), "{text}");
    }

    fn blank_span() -> Span {
        serde_json::from_str(
            r#"{
                "project_id": "01900000-0000-7000-8000-000000000001",
                "trace_id": "f80a0000000000000000000000000001",
                "span_id": "f80a000000000001",
                "name": "span",
                "span_kind": "span",
                "start_time": "2026-01-01T00:00:00.000000000Z",
                "end_time": "2026-01-01T00:00:00.000000000Z",
                "status": "unset",
                "ingest_time": "2026-01-01T00:00:00.000000000Z",
                "schema_version": 1
            }"#,
        )
        .unwrap_or_else(|_| panic!("blank span json"))
    }

    /// 40 bytes, over the 32-byte limit used below.
    fn long(letter: char) -> String {
        letter.to_string().repeat(40)
    }

    #[test]
    fn every_promoted_string_column_is_cut() {
        let mut span = blank_span();
        let text = long('p');
        span.service_name = Some(text.clone());
        span.session_id = Some(text.clone());
        span.user_id = Some(text.clone());
        span.environment = Some(text.clone());
        span.release = Some(text.clone());
        span.provider = Some(text.clone());
        span.request_model = Some(text.clone());
        span.response_model = Some(text.clone());
        span.prompt_name = Some(text.clone());
        span.operation = Some(text.clone());
        span.request_params = Some(text.clone());
        span.scope_name = Some(text.clone());
        span.scope_version = Some(text);
        truncate_columns(&mut span, 32);
        for value in [
            &span.service_name,
            &span.session_id,
            &span.user_id,
            &span.environment,
            &span.release,
            &span.provider,
            &span.request_model,
            &span.response_model,
            &span.prompt_name,
            &span.operation,
            &span.request_params,
            &span.scope_name,
            &span.scope_version,
        ] {
            let value = value.as_deref().expect("set");
            assert!(value.starts_with(&"p".repeat(32)), "{value}");
            assert!(value.ends_with("40 bytes]"), "{value}");
        }
        assert_eq!(
            span.truncated,
            [
                "environment",
                "operation",
                "prompt_name",
                "provider",
                "release",
                "request_model",
                "request_params",
                "response_model",
                "scope_name",
                "scope_version",
                "service_name",
                "session_id",
                "user_id",
            ]
        );
    }

    #[test]
    fn a_value_of_exactly_the_limit_is_kept() {
        let mut span = blank_span();
        span.session_id = Some("s".repeat(32));
        truncate_columns(&mut span, 32);
        assert_eq!(span.session_id, Some("s".repeat(32)));
        assert!(span.truncated.is_empty());
    }

    #[test]
    fn tags_and_metadata_are_cut_and_tags_stay_unique() {
        let mut span = blank_span();
        // Two tags that differ only after the limit have the same cut prefix.
        span.tags = vec![
            format!("{}a", "t".repeat(40)),
            format!("{}b", "t".repeat(40)),
            "short".to_owned(),
        ];
        span.metadata.insert("note".to_owned(), long('m'));
        span.metadata.insert("ok".to_owned(), "fine".to_owned());
        truncate_columns(&mut span, 32);
        assert_eq!(span.tags.len(), 2, "{:?}", span.tags);
        assert!(span.tags[0].starts_with(&"t".repeat(32)));
        assert_eq!(span.tags[1], "short");
        assert!(span.metadata["note"].starts_with(&"m".repeat(32)));
        assert_eq!(span.metadata["ok"], "fine");
        assert_eq!(span.truncated, ["metadata.note", "tags"]);
    }

    #[test]
    fn event_names_and_link_attributes_are_cut() {
        let mut span = blank_span();
        span.events = vec![
            cetra_core::span::SpanEvent {
                time: 0,
                name: long('e'),
                attributes: Some(format!("{{\"k\":\"{}\"}}", long('v'))),
            },
            cetra_core::span::SpanEvent {
                time: 0,
                name: "short".to_owned(),
                attributes: None,
            },
        ];
        span.links = vec![cetra_core::span::SpanLink {
            trace_id: serde_json::from_str("\"f80a0000000000000000000000000002\"")
                .expect("trace id"),
            span_id: serde_json::from_str("\"f80a000000000002\"").expect("span id"),
            attributes: Some(long('l')),
        }];
        truncate_columns(&mut span, 32);
        assert!(span.events[0].name.starts_with(&"e".repeat(32)));
        assert_eq!(span.events[1].name, "short");
        assert!(
            span.links[0]
                .attributes
                .as_deref()
                .is_some_and(|text| text.ends_with("40 bytes]"))
        );
        assert_eq!(
            span.truncated,
            [
                "events[0].attributes",
                "events[0].name",
                "links[0].attributes"
            ]
        );
    }
}
