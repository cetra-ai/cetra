//! The stored history of a conversation as the provider sees it (track AS M3, D-AS-15, D-AS-21,
//! D-AS-22).

use cetra_core::message::{Message, Part, Role};
use serde_json::{Map, Value, json};

use crate::api::assistant::AssistantPart;
use crate::meta::assistant::{MessageRole, MessageRow};

/// History budget in characters: 100,000 estimated tokens at 4 characters each (D-AS-15).
pub(crate) const HISTORY_CHARS: usize = 400_000;

/// The result a call gets when the turn ended before it ran (D-AS-22).
const NOT_RUN: &str = "not run: the turn ended before this call";
/// The note a stub carries (D-AS-22).
const STUB_NOTE: &str = "result removed after 24 hours; call the tool again if you need it";

#[derive(Debug, thiserror::Error)]
pub(crate) enum HistoryError {
    #[error("a stored message does not parse")]
    Corrupt,
}

/// A stored tool result that may still be replaced by its stub, when the history is too large.
struct Candidate {
    /// Index of the provider message in the output.
    message: usize,
    /// Index of the response part in that message.
    part: usize,
    /// What the response becomes when stubbed.
    stub: Value,
}

/// The provider messages for `rows` (oldest first, ending with the current turn's messages so far),
/// after the system message, fitted to [`HISTORY_CHARS`]. The tool definitions are not counted.
///
/// # Errors
///
/// Returns [`HistoryError::Corrupt`] when a stored message's `parts` does not parse.
pub(crate) fn provider_messages(
    system: &str,
    rows: &[MessageRow],
) -> Result<Vec<Message>, HistoryError> {
    let parsed = rows
        .iter()
        .map(|row| serde_json::from_str::<Vec<AssistantPart>>(&row.parts))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| HistoryError::Corrupt)?;

    let mut out = vec![message(Role::System, vec![text(system.to_owned())])];
    let mut candidates = Vec::new();
    // The call ids of the latest assistant message, until the tool message that answers them.
    let mut pending: Vec<String> = Vec::new();
    for (index, (row, parts)) in rows.iter().zip(&parsed).enumerate() {
        match row.role {
            MessageRole::User => {
                let parts: Vec<Part> = parts
                    .iter()
                    .filter_map(|part| match part {
                        AssistantPart::Text { content } => Some(text(content.clone())),
                        _ => None,
                    })
                    .collect();
                push_user(&mut out, parts);
            }
            MessageRole::Assistant => {
                pending.clear();
                let mut out_parts = Vec::new();
                for part in parts {
                    match part {
                        AssistantPart::Text { content } => out_parts.push(text(content.clone())),
                        AssistantPart::ToolCall {
                            id,
                            name,
                            arguments,
                        } => {
                            pending.push(id.clone());
                            out_parts.push(Part::ToolCall {
                                id: Some(id.clone()),
                                name: name.clone(),
                                arguments: call_arguments(arguments),
                                extra: Map::new(),
                            });
                        }
                        AssistantPart::ToolResult { .. } | AssistantPart::Draft { .. } => {}
                    }
                }
                if out_parts.is_empty() {
                    pending.clear();
                    continue;
                }
                out.push(message(Role::Assistant, out_parts));
                let answered_next = rows
                    .get(index + 1)
                    .is_some_and(|next| next.role == MessageRole::Tool);
                if !answered_next {
                    let calls = std::mem::take(&mut pending);
                    push_answers(&mut out, &mut candidates, &calls, &[]);
                }
            }
            MessageRole::Tool => {
                let calls = std::mem::take(&mut pending);
                push_answers(&mut out, &mut candidates, &calls, parts);
            }
        }
    }
    fit(&mut out, candidates);
    Ok(out)
}

fn text(content: String) -> Part {
    Part::Text {
        content,
        extra: Map::new(),
    }
}

fn message(role: Role, parts: Vec<Part>) -> Message {
    Message {
        role,
        name: None,
        parts,
        finish_reason: None,
        extra: Map::new(),
    }
}

/// Appends a user message, or merges its parts into the previous message when that is also a user
/// message (a failed turn leaves two in a row, and not every provider accepts that).
fn push_user(out: &mut Vec<Message>, parts: Vec<Part>) {
    if parts.is_empty() {
        return;
    }
    match out.last_mut() {
        Some(last) if last.role == Role::User => last.parts.extend(parts),
        _ => out.push(message(Role::User, parts)),
    }
}

/// The arguments sent to the provider: the stored value when it is an object, or a string that
/// parses to one. Anything else is sent without arguments, so one malformed call does not make
/// every later turn fail.
fn call_arguments(arguments: &Value) -> Option<Value> {
    match arguments {
        Value::Object(_) => Some(arguments.clone()),
        Value::String(encoded) => match serde_json::from_str::<Value>(encoded) {
            Ok(value @ Value::Object(_)) => Some(value),
            _ => None,
        },
        _ => None,
    }
}

fn stub_object(tool: &str, size_bytes: u64, rows: Option<u64>) -> Value {
    json!({
        "stubbed": true,
        "tool": tool,
        "size_bytes": size_bytes,
        "rows": rows,
        "note": STUB_NOTE,
    })
}

/// Appends the tool message that answers `calls`, in call order. Each call takes its result from
/// `stored` (the tool message's parts, or nothing), a stub when the result was stubbed, or the
/// synthesized "not run" result when there is none. Nothing is appended when there are no calls.
fn push_answers(
    out: &mut Vec<Message>,
    candidates: &mut Vec<Candidate>,
    calls: &[String],
    stored: &[AssistantPart],
) {
    if calls.is_empty() {
        return;
    }
    let message_index = out.len();
    let mut parts = Vec::with_capacity(calls.len());
    for call in calls {
        let result = stored.iter().find(|part| match part {
            AssistantPart::ToolResult { id, .. } => id == call,
            _ => false,
        });
        let response = match result {
            Some(AssistantPart::ToolResult {
                name,
                result: Some(value),
                size_bytes,
                rows,
                stubbed: false,
                ..
            }) => {
                candidates.push(Candidate {
                    message: message_index,
                    part: parts.len(),
                    stub: stub_object(name, *size_bytes, *rows),
                });
                value.clone()
            }
            Some(AssistantPart::ToolResult {
                name,
                size_bytes,
                rows,
                ..
            }) => stub_object(name, *size_bytes, *rows),
            _ => json!({ "error": NOT_RUN }),
        };
        parts.push(Part::ToolCallResponse {
            id: Some(call.clone()),
            response,
            extra: Map::new(),
        });
    }
    out.push(message(Role::Tool, parts));
}

/// Characters of a message: its text, and the compact JSON of its arguments and responses.
fn message_chars(message: &Message) -> usize {
    message.parts.iter().map(part_chars).sum()
}

fn part_chars(part: &Part) -> usize {
    match part {
        Part::Text { content, .. } => content.chars().count(),
        Part::ToolCall { arguments, .. } => arguments.as_ref().map_or(0, json_chars),
        Part::ToolCallResponse { response, .. } => json_chars(response),
        _ => 0,
    }
}

fn json_chars(value: &Value) -> usize {
    serde_json::to_string(value).map_or(0, |encoded| encoded.chars().count())
}

/// Stubs the oldest tool results, then drops the oldest whole turns, until the history fits
/// [`HISTORY_CHARS`]. The system message and the last turn stay; a last turn that alone is too
/// large is sent as it is.
fn fit(out: &mut Vec<Message>, candidates: Vec<Candidate>) {
    let mut total: usize = out.iter().map(message_chars).sum();
    let mut candidates = candidates.into_iter();
    while total > HISTORY_CHARS {
        let Some(Candidate {
            message,
            part,
            stub,
        }) = candidates.next()
        else {
            break;
        };
        let before = message_chars(&out[message]);
        if let Part::ToolCallResponse { response, .. } = &mut out[message].parts[part] {
            *response = stub;
        }
        total = total.saturating_sub(before) + message_chars(&out[message]);
    }
    while total > HISTORY_CHARS {
        // The second user message, when there is one, starts the turn after the oldest one.
        let users: Vec<usize> = out
            .iter()
            .enumerate()
            .skip(1)
            .filter(|(_, message)| message.role == Role::User)
            .map(|(index, _)| index)
            .collect();
        let Some(&second) = users.get(1) else {
            break;
        };
        let dropped: usize = out[1..second].iter().map(message_chars).sum();
        out.drain(1..second);
        total = total.saturating_sub(dropped);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::meta::assistant::MessageStatus;

    fn row(seq: i64, role: MessageRole, parts: &Value) -> MessageRow {
        MessageRow {
            seq,
            role,
            parts: parts.to_string(),
            context: None,
            status: MessageStatus::Complete,
            feedback: None,
            turn_trace_id: None,
            created_at: 0,
        }
    }

    fn call(id: &str, name: &str, arguments: Value) -> Value {
        json!({"type": "tool_call", "id": id, "name": name, "arguments": arguments})
    }

    fn result(id: &str, name: &str, value: Value, size: u64, rows: Option<u64>) -> Value {
        json!({
            "type": "tool_result",
            "id": id,
            "name": name,
            "ok": true,
            "result": value,
            "size_bytes": size,
            "rows": rows,
            "stubbed": false,
        })
    }

    fn response_of(message: &Message, index: usize) -> Value {
        match &message.parts[index] {
            Part::ToolCallResponse { response, .. } => response.clone(),
            other => panic!("not a tool response: {other:?}"),
        }
    }

    fn roles(out: &[Message]) -> Vec<Role> {
        out.iter().map(|message| message.role.clone()).collect()
    }

    #[test]
    fn maps_roles_and_parts() {
        let rows = vec![
            row(
                1,
                MessageRole::User,
                &json!([{"type": "text", "content": "Which errors?"}]),
            ),
            row(
                2,
                MessageRole::Assistant,
                &json!([
                    {"type": "text", "content": "Checking."},
                    call("call_1", "query_sql", json!({"query": "SELECT 1"})),
                    call("call_2", "lookup", json!({"id": 7})),
                ]),
            ),
            row(
                3,
                MessageRole::Tool,
                &json!([
                    result("call_1", "query_sql", json!({"rows": [1]}), 12, Some(1)),
                    result("call_2", "lookup", json!({"rows": []}), 9, Some(0)),
                ]),
            ),
        ];
        let out = provider_messages("system", &rows).unwrap();
        assert_eq!(
            roles(&out),
            vec![Role::System, Role::User, Role::Assistant, Role::Tool]
        );
        assert!(
            matches!(&out[1].parts[0], Part::Text { content, .. } if content == "Which errors?")
        );
        assert!(matches!(&out[2].parts[0], Part::Text { content, .. } if content == "Checking."));
        let calls: Vec<Option<&str>> = out[2]
            .parts
            .iter()
            .filter_map(|part| match part {
                Part::ToolCall { id, .. } => Some(id.as_deref()),
                _ => None,
            })
            .collect();
        assert_eq!(calls, vec![Some("call_1"), Some("call_2")]);
        let answers: Vec<Option<&str>> = out[3]
            .parts
            .iter()
            .map(|part| match part {
                Part::ToolCallResponse { id, .. } => id.as_deref(),
                other => panic!("not a tool response: {other:?}"),
            })
            .collect();
        assert_eq!(answers, vec![Some("call_1"), Some("call_2")]);
        assert_eq!(response_of(&out[3], 0), json!({"rows": [1]}));
    }

    #[test]
    fn stubbed_results_are_sent_as_stubs() {
        let rows = vec![
            row(
                1,
                MessageRole::User,
                &json!([{"type": "text", "content": "Again"}]),
            ),
            row(
                2,
                MessageRole::Assistant,
                &json!([call("call_1", "query_sql", json!({}))]),
            ),
            row(
                3,
                MessageRole::Tool,
                &json!([{
                    "type": "tool_result",
                    "id": "call_1",
                    "name": "query_sql",
                    "ok": true,
                    "result": null,
                    "size_bytes": 40,
                    "rows": 3,
                    "stubbed": true,
                }]),
            ),
        ];
        let out = provider_messages("system", &rows).unwrap();
        assert_eq!(
            response_of(&out[3], 0),
            json!({
                "stubbed": true,
                "tool": "query_sql",
                "size_bytes": 40,
                "rows": 3,
                "note": STUB_NOTE,
            })
        );
    }

    #[test]
    fn drafts_are_not_sent() {
        let rows = vec![
            row(
                1,
                MessageRole::User,
                &json!([{"type": "text", "content": "Make a filter"}]),
            ),
            row(
                2,
                MessageRole::Assistant,
                &json!([
                    {"type": "text", "content": "Here."},
                    call("call_1", "lookup", json!({})),
                    {"type": "draft", "id": "draft_call_1", "kind": "filter", "draft": {"filter": "x"}},
                ]),
            ),
            row(
                3,
                MessageRole::Tool,
                &json!([
                    result("call_1", "lookup", json!({"rows": []}), 9, Some(0)),
                    {"type": "draft", "id": "draft_call_1", "kind": "filter", "draft": {"filter": "x"}},
                ]),
            ),
        ];
        let out = provider_messages("system", &rows).unwrap();
        assert_eq!(out[2].parts.len(), 2, "text and call only");
        assert_eq!(out[3].parts.len(), 1, "the response only");
    }

    #[test]
    fn missing_results_are_synthesized() {
        let rows = vec![
            row(
                1,
                MessageRole::User,
                &json!([{"type": "text", "content": "Look"}]),
            ),
            row(
                2,
                MessageRole::Assistant,
                &json!([call("call_1", "query_sql", json!({}))]),
            ),
        ];
        let out = provider_messages("system", &rows).unwrap();
        assert_eq!(
            roles(&out),
            vec![Role::System, Role::User, Role::Assistant, Role::Tool]
        );
        assert_eq!(
            response_of(&out[3], 0),
            json!({"error": NOT_RUN}),
            "a call with no stored result is answered"
        );
    }

    #[test]
    fn ids_pair_per_step() {
        let rows = vec![
            row(
                1,
                MessageRole::User,
                &json!([{"type": "text", "content": "Twice"}]),
            ),
            row(
                2,
                MessageRole::Assistant,
                &json!([call("call_1", "query_sql", json!({}))]),
            ),
            row(
                3,
                MessageRole::Assistant,
                &json!([call("call_1", "query_sql", json!({"again": true}))]),
            ),
            row(
                4,
                MessageRole::Tool,
                &json!([result("call_1", "query_sql", json!({"ok": 2}), 8, None)]),
            ),
        ];
        let out = provider_messages("system", &rows).unwrap();
        assert_eq!(
            roles(&out),
            vec![
                Role::System,
                Role::User,
                Role::Assistant,
                Role::Tool,
                Role::Assistant,
                Role::Tool,
            ]
        );
        assert_eq!(response_of(&out[3], 0), json!({"error": NOT_RUN}));
        assert_eq!(response_of(&out[5], 0), json!({"ok": 2}));
    }

    #[test]
    fn non_object_arguments_are_sent_as_empty() {
        let rows = vec![
            row(
                1,
                MessageRole::User,
                &json!([{"type": "text", "content": "Odd calls"}]),
            ),
            row(
                2,
                MessageRole::Assistant,
                &json!([
                    call("call_1", "lookup", json!("oops")),
                    call("call_2", "lookup", json!("{\"a\":1}")),
                ]),
            ),
            row(
                3,
                MessageRole::Tool,
                &json!([
                    result("call_1", "lookup", json!({}), 2, None),
                    result("call_2", "lookup", json!({}), 2, None),
                ]),
            ),
        ];
        let out = provider_messages("system", &rows).unwrap();
        let arguments: Vec<Option<Value>> = out[2]
            .parts
            .iter()
            .filter_map(|part| match part {
                Part::ToolCall { arguments, .. } => Some(arguments.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(arguments, vec![None, Some(json!({"a": 1}))]);
    }

    #[test]
    fn budget_stubs_then_drops_turns() {
        // Each turn: 150,000 characters of text that cannot be stubbed, and a 60,002-character
        // result. Stubbing every result is not enough, so the first turn goes too.
        let mut rows = Vec::new();
        for turn in 1..=3_i64 {
            let call_id = format!("call_{turn}");
            rows.push(row(
                turn * 3 - 2,
                MessageRole::User,
                &json!([{"type": "text", "content": "u".repeat(150_000)}]),
            ));
            rows.push(row(
                turn * 3 - 1,
                MessageRole::Assistant,
                &json!([call(&call_id, "query_sql", json!({}))]),
            ));
            rows.push(row(
                turn * 3,
                MessageRole::Tool,
                &json!([result(
                    &call_id,
                    "query_sql",
                    json!("r".repeat(60_000)),
                    60_002,
                    None
                )]),
            ));
        }
        let out = provider_messages("sys", &rows).unwrap();
        assert_eq!(
            roles(&out),
            vec![
                Role::System,
                Role::User,
                Role::Assistant,
                Role::Tool,
                Role::User,
                Role::Assistant,
                Role::Tool,
            ],
            "the system message and the last turn stay; the first turn is gone"
        );
        assert_eq!(response_of(&out[3], 0)["stubbed"], json!(true));
        assert_eq!(response_of(&out[6], 0)["stubbed"], json!(true));
        let total: usize = out.iter().map(message_chars).sum();
        assert!(total <= HISTORY_CHARS, "the history fits: {total}");
    }

    #[test]
    fn corrupt_parts_error() {
        let rows = vec![MessageRow {
            seq: 1,
            role: MessageRole::User,
            parts: "not json".to_owned(),
            context: None,
            status: MessageStatus::Complete,
            feedback: None,
            turn_trace_id: None,
            created_at: 0,
        }];
        assert!(matches!(
            provider_messages("system", &rows),
            Err(HistoryError::Corrupt)
        ));
    }

    #[test]
    fn consecutive_user_messages_are_merged() {
        let rows = vec![
            row(
                1,
                MessageRole::User,
                &json!([{"type": "text", "content": "first"}]),
            ),
            MessageRow {
                status: MessageStatus::Cancelled,
                ..row(2, MessageRole::Assistant, &json!([]))
            },
            row(
                3,
                MessageRole::User,
                &json!([{"type": "text", "content": "second"}]),
            ),
        ];
        let out = provider_messages("system", &rows).unwrap();
        assert_eq!(roles(&out), vec![Role::System, Role::User]);
        let texts: Vec<&str> = out[1]
            .parts
            .iter()
            .filter_map(|part| match part {
                Part::Text { content, .. } => Some(content.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(texts, vec!["first", "second"]);
    }
}
