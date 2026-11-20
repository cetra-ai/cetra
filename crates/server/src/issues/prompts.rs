//! Prompts and output schemas of the LLM issue sources (D-IC-4, D-IC-19).
//!
//! The texts are part of the behaviour: a change to one changes what the model is asked, so
//! the tests pin them. A change to a summary prompt bumps [`SUMMARY_PROMPT_VERSION`].

use std::collections::HashSet;

use cetra_core::enums::PayloadFormat;
use cetra_core::ids::SpanId;
use cetra_core::message::{Message, Part, Role, parse_messages};
use cetra_core::score::ScoreValue;
use cetra_core::span::Span;
use serde_json::{Value, json};

use crate::meta::issues::SourceKind;

/// The version of the summary prompt and schema, stored with each summary.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) const SUMMARY_PROMPT_VERSION: u32 = 1;
/// The structured-output name of the summary call.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) const SUMMARY_NAME: &str = "issue_summary";
/// The structured-output name of the naming call.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) const NAME_NAME: &str = "issue_name";
/// Longest text of a payload in a prompt, in characters (D-IC-19).
#[cfg_attr(not(test), allow(dead_code))]
const PAYLOAD_CHARS: usize = 4_000;
/// Stands in for an empty list of titles.
const NONE: &str = "(none)";
/// Longest score value in a prompt, in characters.
const SCORE_VALUE_CHARS: usize = 200;

/// The schema of the summary call: a summary and whether the interaction is a real issue.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn summary_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "summary": {"type": "string"},
            "is_issue": {"type": "boolean"}
        },
        "required": ["summary", "is_issue"],
        "additionalProperties": false
    })
}

/// The schema of the naming call: a title and a description.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn name_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "title": {"type": "string"},
            "description": {"type": "string"}
        },
        "required": ["title", "description"],
        "additionalProperties": false
    })
}

/// The summary prompt of a bad score or a bad feedback score. `reason` prints `(none)` when
/// absent.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn problem_prompt(
    score_name: &str,
    score_value: &str,
    reason: Option<&str>,
    input: &str,
    output: &str,
) -> String {
    format!(
        "You review one interaction of an LLM application that received a bad score. Describe the failure mechanism in generic words: what went wrong and how. Do not include names, numbers, quotes or details specific to this conversation. Use at most 30 words. If the evidence shows no real problem, set is_issue to false.\n\nScore: {score_name} = {score_value}\nReason: {}\n\nInput:\n{input}\n\nOutput:\n{output}",
        reason.unwrap_or(NONE)
    )
}

/// The summary prompt of a new production trace, from its first user message.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn topic_prompt(user_message: &str) -> String {
    format!(
        "Write what the user wants in this request, in at most 15 generic words, without names, numbers or quotes. If the request is empty or unreadable, set is_issue to false.\n\nRequest:\n{user_message}"
    )
}

/// The naming prompt of a refit cluster. `titles` are the titles of the issues the source
/// already has; an empty list prints `(none)`.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn name_prompt(kind: SourceKind, summaries: &[&str], titles: &[&str]) -> String {
    let what = match kind {
        SourceKind::Topic => "recurring user request",
        SourceKind::Error | SourceKind::Score | SourceKind::Feedback => "recurring problem",
    };
    let titles = if titles.is_empty() {
        NONE.to_owned()
    } else {
        bullets(titles)
    };
    format!(
        "These summaries describe one {what} in an LLM application. Write a short title (at most 80 characters) and a description (at most 400 characters) of what they have in common, in generic words. The title must differ from these existing titles:\n{titles}\n\nSummaries:\n{}",
        bullets(summaries)
    )
}

/// The text of a score value in a prompt: a boolean as `true` or `false`, a number as itself, a
/// category as its label, and text cut to [`SCORE_VALUE_CHARS`] characters.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn score_text(value: &ScoreValue) -> String {
    match value {
        ScoreValue::Boolean(value) => value.to_string(),
        ScoreValue::Numeric(value) => value.to_string(),
        ScoreValue::Categorical { label, .. } => label.clone(),
        ScoreValue::Text(text) => text.chars().take(SCORE_VALUE_CHARS).collect(),
    }
}

/// The root input and output of a trace, rendered to text and each cut to 4 000 characters
/// (D-IC-19). Empty when the spans have no root.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn previews(spans: &[Span]) -> (String, String) {
    let Some(root) = root(spans) else {
        return (String::new(), String::new());
    };
    (
        cut(render(root.input.as_deref(), root.input_format)),
        cut(render(root.output.as_deref(), root.output_format)),
    )
}

/// The first user message of a trace (D-IC-19), cut to 4 000 characters: the text parts of the
/// first `user` message of a `messages` input, joined with a newline, else the rendered input.
/// Empty when the spans have no root.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn first_user_message(spans: &[Span]) -> String {
    let Some(root) = root(spans) else {
        return String::new();
    };
    let parsed = match root.input_format {
        Some(PayloadFormat::Messages) => root.input.as_deref().map(parse_messages),
        _ => None,
    };
    if let Some(Ok(messages)) = parsed {
        let first = messages
            .iter()
            .find(|message| message.role == Role::User)
            .map(text_parts)
            .unwrap_or_default();
        return cut(first);
    }
    cut(render(root.input.as_deref(), root.input_format))
}

/// The root span: the earliest, by `(start_time, span_id)`, of the spans without a parent or
/// whose parent is not among `spans`.
fn root(spans: &[Span]) -> Option<&Span> {
    let ids: HashSet<SpanId> = spans.iter().map(|span| span.span_id).collect();
    spans
        .iter()
        .filter(|span| {
            span.parent_span_id
                .is_none_or(|parent| !ids.contains(&parent))
        })
        .min_by_key(|span| (span.start_time, span.span_id))
}

/// A payload as prompt text. `messages` renders one `role: text` line per message; `json` is
/// compact JSON; `text` is as is. Text that does not parse as its format is kept as it is.
/// Missing text is empty.
fn render(text: Option<&str>, format: Option<PayloadFormat>) -> String {
    let Some(text) = text else {
        return String::new();
    };
    match format {
        Some(PayloadFormat::Messages) => match parse_messages(text) {
            Ok(messages) => messages
                .iter()
                .map(|message| format!("{}: {}", message.role.as_str(), text_parts(message)))
                .collect::<Vec<_>>()
                .join("\n"),
            Err(_) => text.to_owned(),
        },
        Some(PayloadFormat::Json) => match serde_json::from_str::<Value>(text) {
            Ok(value) => serde_json::to_string(&value).unwrap_or_else(|_| text.to_owned()),
            Err(_) => text.to_owned(),
        },
        Some(PayloadFormat::Text) | None => text.to_owned(),
    }
}

/// The text parts of a message, joined with a newline. Other parts are left out.
fn text_parts(message: &Message) -> String {
    message
        .parts
        .iter()
        .filter_map(|part| match part {
            Part::Text { content, .. } => Some(content.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The first [`PAYLOAD_CHARS`] characters of `text`.
fn cut(text: String) -> String {
    text.chars().take(PAYLOAD_CHARS).collect()
}

/// `- item` lines, one per item.
fn bullets(items: &[&str]) -> String {
    items
        .iter()
        .map(|item| format!("- {item}"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A span with `id`, `parent` and `start`. Its other fields come from a fixed JSON record.
    fn span(id: u64, parent: Option<u64>, start: i64) -> Span {
        let mut span: Span = serde_json::from_str(
            r#"{
                "project_id": "01900000-0000-7000-8000-000000000001",
                "trace_id": "a1000000000000000000000000000001",
                "span_id": "b100000000000011",
                "name": "run",
                "span_kind": "span",
                "start_time": "2026-01-01T10:00:00.000000000Z",
                "end_time": "2026-01-01T10:00:00.000000000Z",
                "status": "unset",
                "ingest_time": "2026-01-01T10:00:00.000000000Z",
                "schema_version": 1
            }"#,
        )
        .expect("span JSON");
        span.span_id = SpanId::from_bytes(id.to_be_bytes()).expect("not all zero");
        span.parent_span_id =
            parent.map(|parent| SpanId::from_bytes(parent.to_be_bytes()).expect("not all zero"));
        span.start_time = start;
        span
    }

    fn input(mut span: Span, text: &str, format: PayloadFormat) -> Span {
        span.input = Some(text.to_owned());
        span.input_format = Some(format);
        span
    }

    const CHAT: &str = r#"[{"role":"system","parts":[{"type":"text","content":"be kind"}]},{"role":"user","parts":[{"type":"text","content":"first"},{"type":"uri","modality":null,"mime_type":null,"uri":"https://example.com/a.png"},{"type":"text","content":"second"}]},{"role":"assistant","parts":[{"type":"text","content":"ok"}]}]"#;

    #[test]
    fn summary_schema_is_pinned() {
        assert_eq!(
            summary_schema(),
            json!({"type":"object","properties":{"summary":{"type":"string"},"is_issue":{"type":"boolean"}},"required":["summary","is_issue"],"additionalProperties":false})
        );
    }

    #[test]
    fn name_schema_is_pinned() {
        assert_eq!(
            name_schema(),
            json!({"type":"object","properties":{"title":{"type":"string"},"description":{"type":"string"}},"required":["title","description"],"additionalProperties":false})
        );
    }

    #[test]
    fn problem_prompt_text_is_pinned() {
        assert_eq!(
            problem_prompt("quality", "0.2", None, "in", "out"),
            "You review one interaction of an LLM application that received a bad score. Describe the failure mechanism in generic words: what went wrong and how. Do not include names, numbers, quotes or details specific to this conversation. Use at most 30 words. If the evidence shows no real problem, set is_issue to false.\n\nScore: quality = 0.2\nReason: (none)\n\nInput:\nin\n\nOutput:\nout"
        );
        assert!(
            problem_prompt("quality", "false", Some("too slow"), "in", "out")
                .contains("\nReason: too slow\n")
        );
    }

    #[test]
    fn topic_prompt_text_is_pinned() {
        assert_eq!(
            topic_prompt("book a table"),
            "Write what the user wants in this request, in at most 15 generic words, without names, numbers or quotes. If the request is empty or unreadable, set is_issue to false.\n\nRequest:\nbook a table"
        );
    }

    #[test]
    fn name_prompt_text_is_pinned() {
        assert_eq!(
            name_prompt(SourceKind::Score, &["a", "b"], &[]),
            "These summaries describe one recurring problem in an LLM application. Write a short title (at most 80 characters) and a description (at most 400 characters) of what they have in common, in generic words. The title must differ from these existing titles:\n(none)\n\nSummaries:\n- a\n- b"
        );
        assert_eq!(
            name_prompt(SourceKind::Topic, &["a"], &["Old one", "Old two"]),
            "These summaries describe one recurring user request in an LLM application. Write a short title (at most 80 characters) and a description (at most 400 characters) of what they have in common, in generic words. The title must differ from these existing titles:\n- Old one\n- Old two\n\nSummaries:\n- a"
        );
    }

    #[test]
    fn score_text_renders_each_kind() {
        assert_eq!(score_text(&ScoreValue::Boolean(true)), "true");
        assert_eq!(score_text(&ScoreValue::Boolean(false)), "false");
        assert_eq!(score_text(&ScoreValue::Numeric(0.5)), "0.5");
        let category = ScoreValue::Categorical {
            label: "bad".to_owned(),
            value: None,
        };
        assert_eq!(score_text(&category), "bad");
        assert_eq!(
            score_text(&ScoreValue::Text("x".repeat(300)))
                .chars()
                .count(),
            SCORE_VALUE_CHARS
        );
    }

    #[test]
    fn previews_render_messages_text_and_json() {
        let mut root = input(span(1, None, 100), CHAT, PayloadFormat::Messages);
        root.output = Some(r#"{"a": 1,  "b":[2]}"#.to_owned());
        root.output_format = Some(PayloadFormat::Json);
        let (in_preview, out_preview) = previews(&[root]);
        assert_eq!(
            in_preview,
            "system: be kind\nuser: first\nsecond\nassistant: ok"
        );
        assert_eq!(out_preview, r#"{"a":1,"b":[2]}"#);
    }

    #[test]
    fn previews_keep_text_and_text_that_does_not_parse() {
        let text = input(span(1, None, 100), "hello there", PayloadFormat::Text);
        assert_eq!(previews(&[text]).0, "hello there");
        let broken = input(span(1, None, 100), "{nope", PayloadFormat::Messages);
        assert_eq!(previews(&[broken]).0, "{nope");
        let broken_json = input(span(1, None, 100), "not json", PayloadFormat::Json);
        assert_eq!(previews(&[broken_json]).0, "not json");
    }

    #[test]
    fn previews_use_the_earliest_root() {
        // Span 4 has a parent outside the slice, so it is a root and starts first.
        let spans = [
            input(span(1, None, 200), "one", PayloadFormat::Text),
            input(span(2, None, 100), "two", PayloadFormat::Text),
            input(span(3, Some(1), 10), "three", PayloadFormat::Text),
            input(span(4, Some(99), 50), "four", PayloadFormat::Text),
        ];
        assert_eq!(previews(&spans).0, "four");
    }

    #[test]
    fn previews_break_a_start_tie_by_span_id() {
        let spans = [
            input(span(5, None, 100), "five", PayloadFormat::Text),
            input(span(2, None, 100), "two", PayloadFormat::Text),
        ];
        assert_eq!(previews(&spans).0, "two");
    }

    #[test]
    fn previews_are_cut_to_4000_characters() {
        let long = input(span(1, None, 100), &"a".repeat(5_000), PayloadFormat::Text);
        assert_eq!(previews(&[long]).0.chars().count(), PAYLOAD_CHARS);
    }

    #[test]
    fn an_empty_slice_has_no_previews_and_no_user_message() {
        assert_eq!(previews(&[]), (String::new(), String::new()));
        assert_eq!(first_user_message(&[]), "");
    }

    #[test]
    fn first_user_message_takes_the_first_user_text_parts() {
        let root = input(span(1, None, 100), CHAT, PayloadFormat::Messages);
        assert_eq!(first_user_message(&[root]), "first\nsecond");
    }

    #[test]
    fn first_user_message_is_empty_without_a_user_message() {
        let root = input(
            span(1, None, 100),
            r#"[{"role":"system","parts":[{"type":"text","content":"be kind"}]}]"#,
            PayloadFormat::Messages,
        );
        assert_eq!(first_user_message(&[root]), "");
    }

    #[test]
    fn first_user_message_is_the_input_text_or_json() {
        let text = input(span(1, None, 100), "plain request", PayloadFormat::Text);
        assert_eq!(first_user_message(&[text]), "plain request");
        let json = input(span(1, None, 100), r#"{"q": "x"}"#, PayloadFormat::Json);
        assert_eq!(first_user_message(&[json]), r#"{"q":"x"}"#);
    }

    #[test]
    fn first_user_message_cuts_long_text() {
        let long = input(span(1, None, 100), &"b".repeat(5_000), PayloadFormat::Text);
        assert_eq!(first_user_message(&[long]).chars().count(), PAYLOAD_CHARS);
    }
}
