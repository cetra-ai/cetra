//! Claude Code spans (spec 02 § Coding agents; D-CA-5, D-CA-6, D110).
//!
//! The generic rules have already run when these apply. They decide the kind
//! and the usage of an LLM request, the person and repository of the session,
//! and the payloads of an interaction and a tool.

use cetra_core::enums::SpanKind;
use cetra_core::span::Span;
use opentelemetry_proto::tonic::trace::v1::span::Event;
use serde_json::{Map, Value as Json};

use super::key::{BRANCH, REPOSITORY};
use super::{AgentContexts, Finish, SPAN_NAME_ATTRIBUTE, is_redacted};
use crate::bag::AttrBag;
use crate::context::ContextFields;
use crate::payload::{Assembled, Payload};
use crate::usage::{Usage, read_millis, take};
use crate::value::{InvalidNumber, as_i64, as_str, to_json};
use crate::warn::{Warning, Warnings};

const WIRE_NAME: &str = "claude-code";

/// The tool attributes that make up a tool span's input (D-CA-6).
const TOOL_DETAILS: [&str; 4] = ["full_command", "file_path", "skill_name", "subagent_type"];

/// Where a tool's output text can be, in order (D-CA-6).
const TOOL_OUTPUT_KEYS: [&str; 3] = ["output", "diff", "content"];

pub(crate) fn kind(span_name: &str) -> Option<SpanKind> {
    Some(match span_name {
        "claude_code.interaction" => SpanKind::Agent,
        "claude_code.llm_request" => SpanKind::Llm,
        "claude_code.tool" => SpanKind::Tool,
        _ => SpanKind::Span,
    })
}

/// Tokens from Claude Code's own names. `input_tokens` excludes the cache, so the
/// cache is added back (D110). `None` unless this is an LLM request with a count.
pub(crate) fn usage(
    span_name: &str,
    kind: SpanKind,
    bag: &mut AttrBag<'_>,
    warnings: &mut Warnings,
) -> Option<Usage> {
    if kind != SpanKind::Llm || span_name != "claude_code.llm_request" {
        return None;
    }
    let uncached = count(bag, "input_tokens", warnings);
    let output = count(bag, "output_tokens", warnings);
    if uncached.is_none() && output.is_none() {
        return None;
    }
    let cache_read = count(bag, "cache_read_tokens", warnings);
    let cache_write = count(bag, "cache_creation_tokens", warnings);
    let input = uncached.map(|uncached| {
        uncached
            .saturating_add(cache_read.unwrap_or(0))
            .saturating_add(cache_write.unwrap_or(0))
    });
    let total = input
        .zip(output)
        .map(|(input, output)| input.saturating_add(output));
    Some(Usage {
        input_tokens: input,
        output_tokens: output,
        cache_read_tokens: cache_read,
        cache_write_tokens: cache_write,
        reasoning_tokens: None,
        total_tokens: total,
    })
}

/// A non-negative count. A negative or non-numeric value is invalid (rule 20) and stays unread.
fn count(bag: &mut AttrBag<'_>, name: &str, warnings: &mut Warnings) -> Option<i64> {
    let value = bag.get(name)?;
    match as_i64(value) {
        Ok(Some(number)) if number >= 0 => {
            bag.take(name);
            Some(number)
        }
        Ok(Some(_)) | Err(InvalidNumber) => {
            warnings.add(Warning::InvalidNumber);
            None
        }
        Ok(None) => None,
    }
}

/// `ttft_ms`, in milliseconds.
pub(crate) fn ttft(bag: &mut AttrBag<'_>, warnings: &mut Warnings) -> Option<i64> {
    take(bag, "ttft_ms", warnings, read_millis)
}

/// Person, repository and branch (D-CA-5, D-CA-9). The person is the first of: the
/// account email, the session context's user, the account uuid, the account id, and
/// the generic `user_id`. The context's repository wins over the span's VCS URL, and
/// its branch is used as it is.
pub(crate) fn context(bag: &mut AttrBag<'_>, fields: &mut ContextFields, agents: &AgentContexts) {
    let stored = fields
        .session_id
        .as_deref()
        .and_then(|session| agents.get(WIRE_NAME, session));
    let email = take_text(bag, "user.email").map(|text| text.to_lowercase());
    let context_user = stored
        .and_then(|context| context.user.as_deref())
        .map(|text| text.trim().to_lowercase())
        .filter(|text| !text.is_empty());
    let user = email
        .or(context_user)
        .or_else(|| take_text(bag, "user.account_uuid"))
        .or_else(|| take_text(bag, "user.account_id"))
        .or_else(|| fields.user_id.clone());
    let repository = stored
        .and_then(|context| context.repository.clone())
        .or_else(|| {
            take_text(bag, "vcs.repository.url.full")
                .and_then(|url| super::canonical_repository(&url))
        });
    let branch = stored.and_then(|context| context.branch.clone());

    if let Some(user) = user {
        fields.user_id = Some(user);
    }
    if let Some(repository) = repository {
        fields.metadata.insert(REPOSITORY.to_owned(), repository);
    }
    if let Some(branch) = branch {
        fields.metadata.insert(BRANCH.to_owned(), branch);
    }
}

/// A trimmed, non-empty string. It is taken when it is returned.
fn take_text(bag: &mut AttrBag<'_>, name: &str) -> Option<String> {
    let text = bag
        .get(name)
        .and_then(as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())?
        .to_owned();
    bag.take(name);
    Some(text)
}

/// Interaction prompts become the input, and tool details and output become the tool's
/// input and output (D-CA-6). Redacted text is no content.
pub(crate) fn payload(
    span_name: &str,
    bag: &mut AttrBag<'_>,
    events: &[Event],
    assembled: &mut Assembled,
    warnings: &mut Warnings,
) {
    match span_name {
        "claude_code.interaction" => interaction(bag, assembled),
        "claude_code.tool" => {
            tool_input(bag, assembled, warnings);
            tool_output(events, assembled);
        }
        _ => {}
    }
}

fn interaction(bag: &mut AttrBag<'_>, assembled: &mut Assembled) {
    let prompt = bag.get("user_prompt").and_then(as_str).map(str::to_owned);
    bag.drop_payload("user_prompt");
    if let Some(text) = prompt.filter(|text| !text.is_empty() && !is_redacted(text)) {
        set_input(assembled, Payload::Text(text));
    }
}

/// The detail attributes, as a JSON object, when at least one is a non-empty string.
fn tool_input(bag: &mut AttrBag<'_>, assembled: &mut Assembled, warnings: &mut Warnings) {
    let present: Vec<&str> = TOOL_DETAILS
        .into_iter()
        .filter(|name| bag.get(name).is_some())
        .collect();
    let has_text = present.iter().any(|name| {
        bag.get(name)
            .and_then(as_str)
            .is_some_and(|text| !text.is_empty())
    });
    if !has_text {
        return;
    }
    let mut object = Map::new();
    for name in present {
        if let Some(value) = bag.take(name) {
            object.insert(name.to_owned(), to_json(value, warnings));
        }
    }
    set_input(assembled, Payload::Json(Json::Object(object)));
}

/// The first `tool.output` event is dropped from the span's events in every case. Its
/// first usable text among `output`, `diff` and `content` is the tool's output.
fn tool_output(events: &[Event], assembled: &mut Assembled) {
    let Some((index, event)) = events
        .iter()
        .enumerate()
        .find(|(_, event)| event.name == "tool.output")
    else {
        return;
    };
    assembled.dropped_events.push(index);
    let text = TOOL_OUTPUT_KEYS.into_iter().find_map(|name| {
        event
            .attributes
            .iter()
            .rev()
            .find(|attr| attr.key == name)
            .and_then(|attr| attr.value.as_ref())
            .and_then(as_str)
            .filter(|text| !text.is_empty() && !is_redacted(text))
            .map(str::to_owned)
    });
    if let Some(text) = text {
        set_output(assembled, Payload::Text(text));
    }
}

/// Sets the input and drops the convention's input error, which no longer applies.
fn set_input(assembled: &mut Assembled, payload: Payload) {
    assembled.io.input = Some(payload);
    assembled.io.errors.retain(|code| !code.ends_with(":input"));
}

fn set_output(assembled: &mut Assembled, payload: Payload) {
    assembled.io.output = Some(payload);
    assembled
        .io
        .errors
        .retain(|code| !code.ends_with(":output"));
}

/// The tool's name, from `gen_ai.tool.name`, else `tool_name`. Peeked, not taken.
pub(crate) fn tool_name(bag: &AttrBag<'_>) -> Option<String> {
    ["gen_ai.tool.name", "tool_name"]
        .into_iter()
        .find_map(|name| {
            bag.get(name)
                .and_then(as_str)
                .filter(|text| !text.is_empty())
                .map(str::to_owned)
        })
}

/// A tool span takes its tool's name. The original name moves to `cetra.span_name`.
pub(crate) fn finish(span_name: &str, inputs: &Finish, built: &mut Span) {
    if span_name != "claude_code.tool" {
        return;
    }
    let Some(name) = inputs.tool_name.as_deref() else {
        return;
    };
    if built.name == name {
        return;
    }
    let original = std::mem::replace(&mut built.name, name.to_owned());
    built
        .attributes
        .insert(SPAN_NAME_ATTRIBUTE.to_owned(), original);
}

#[cfg(test)]
mod tests {
    use cetra_core::enums::{CostSource, PayloadFormat};
    use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
    use opentelemetry_proto::tonic::common::v1::KeyValue;
    use opentelemetry_proto::tonic::trace::v1::Span as OtlpSpan;

    use crate::agents::AgentContext;
    use crate::agents::test_support::{event, i, kv, request, run, run_priced, s, span};
    use crate::cost::{PriceRow, PriceScope};

    use super::*;

    fn claude(spans: Vec<OtlpSpan>) -> ExportTraceServiceRequest {
        request("claude-code", "com.anthropic.claude_code.tracing", spans)
    }

    fn llm_request(attributes: Vec<KeyValue>) -> ExportTraceServiceRequest {
        claude(vec![span("claude_code.llm_request", attributes, vec![])])
    }

    #[test]
    fn claude_llm_request_tokens_add_cache_back() {
        let request = llm_request(vec![
            kv("input_tokens", i(100)),
            kv("cache_read_tokens", i(300)),
            kv("cache_creation_tokens", i(20)),
            kv("output_tokens", i(12)),
        ]);
        let normalized = run(&request, &AgentContexts::default());
        let span = &normalized.spans[0];
        assert_eq!(span.span_kind, SpanKind::Llm);
        assert_eq!(span.input_tokens, Some(420));
        assert_eq!(span.cache_read_tokens, Some(300));
        assert_eq!(span.cache_write_tokens, Some(20));
        assert_eq!(span.output_tokens, Some(12));
        assert_eq!(span.total_tokens, Some(432));
    }

    #[test]
    fn claude_llm_request_output_only() {
        let normalized = run(
            &llm_request(vec![kv("output_tokens", i(5))]),
            &AgentContexts::default(),
        );
        let span = &normalized.spans[0];
        assert_eq!(span.output_tokens, Some(5));
        assert_eq!(span.input_tokens, None);
        assert_eq!(span.total_tokens, None);
    }

    #[test]
    fn claude_llm_request_without_tokens_uses_the_generic_path() {
        let normalized = run(
            &llm_request(vec![kv("gen_ai.usage.input_tokens", i(7))]),
            &AgentContexts::default(),
        );
        assert_eq!(normalized.spans[0].span_kind, SpanKind::Llm);
        assert_eq!(normalized.spans[0].input_tokens, Some(7));
    }

    #[test]
    fn claude_llm_request_is_priced_with_cache() {
        let row = PriceRow {
            scope: PriceScope::Builtin,
            model_name: "claude-sonnet-4-5".to_owned(),
            match_pattern: "claude-sonnet-4-5".to_owned(),
            provider: None,
            valid_from: 0,
            price_input: Some(3.0),
            price_output: Some(15.0),
            price_cache_read: Some(0.3),
            price_cache_write: Some(3.75),
            price_reasoning: None,
            tiers: None,
        };
        let request = llm_request(vec![
            kv("gen_ai.request.model", s("claude-sonnet-4-5")),
            kv("input_tokens", i(100)),
            kv("cache_read_tokens", i(300)),
            kv("cache_creation_tokens", i(20)),
            kv("output_tokens", i(12)),
        ]);
        let normalized = run_priced(&request, vec![row]);
        let span = &normalized.spans[0];
        // Uncached input is 420 - 300 - 20 = 100 tokens. Prices are USD per 1M tokens.
        let input = (100.0 * 3.0 + 300.0 * 0.3 + 20.0 * 3.75) / 1_000_000.0;
        let output = 12.0 * 15.0 / 1_000_000.0;
        assert_eq!(span.cost_source, Some(CostSource::Computed));
        assert!((span.cost_input.expect("input cost") - input).abs() < 1e-12);
        assert!((span.cost_output.expect("output cost") - output).abs() < 1e-12);
        assert!((span.cost_total.expect("total cost") - (input + output)).abs() < 1e-12);
    }

    #[test]
    fn claude_ttft_from_ms() {
        let normalized = run(
            &llm_request(vec![kv("ttft_ms", i(12))]),
            &AgentContexts::default(),
        );
        assert_eq!(normalized.spans[0].ttft_ns, Some(12_000_000));
    }

    #[test]
    fn claude_interaction_prompt_is_text_input() {
        let request = claude(vec![span(
            "claude_code.interaction",
            vec![kv("user_prompt", s("fix the build"))],
            vec![],
        )]);
        let normalized = run(&request, &AgentContexts::default());
        let span = &normalized.spans[0];
        assert_eq!(span.span_kind, SpanKind::Agent);
        assert_eq!(span.input.as_deref(), Some("fix the build"));
        assert_eq!(span.input_format, Some(PayloadFormat::Text));
        assert!(!span.attributes.contains_key("user_prompt"));
    }

    #[test]
    fn claude_redacted_prompt_is_no_input() {
        let request = claude(vec![span(
            "claude_code.interaction",
            vec![kv("user_prompt", s("<REDACTED>"))],
            vec![],
        )]);
        let normalized = run(&request, &AgentContexts::default());
        let span = &normalized.spans[0];
        assert_eq!(span.input, None);
        assert!(!span.attributes.contains_key("user_prompt"));
    }

    #[test]
    fn claude_tool_is_named_after_the_tool() {
        let request = claude(vec![span(
            "claude_code.tool",
            vec![
                kv("gen_ai.tool.name", s("Bash")),
                kv("full_command", s("echo hello")),
            ],
            vec![event("tool.output", vec![kv("output", s("hello"))])],
        )]);
        let normalized = run(&request, &AgentContexts::default());
        let span = &normalized.spans[0];
        assert_eq!(span.name, "Bash");
        assert_eq!(span.span_kind, SpanKind::Tool);
        assert_eq!(
            span.attributes.get("cetra.span_name").map(String::as_str),
            Some("claude_code.tool")
        );
        assert_eq!(
            span.input.as_deref(),
            Some(r#"{"full_command":"echo hello"}"#)
        );
        assert_eq!(span.input_format, Some(PayloadFormat::Json));
        assert_eq!(span.output.as_deref(), Some("hello"));
        assert_eq!(span.output_format, Some(PayloadFormat::Text));
        assert!(span.events.is_empty());
    }

    #[test]
    fn claude_tool_without_events_has_no_output() {
        let request = claude(vec![span(
            "claude_code.tool",
            vec![
                kv("gen_ai.tool.name", s("Read")),
                kv("file_path", s("/tmp/a")),
            ],
            vec![],
        )]);
        let normalized = run(&request, &AgentContexts::default());
        let span = &normalized.spans[0];
        assert_eq!(span.name, "Read");
        assert_eq!(span.output, None);
        assert_eq!(span.input.as_deref(), Some(r#"{"file_path":"/tmp/a"}"#));
    }

    #[test]
    fn claude_children_are_spans() {
        let request = claude(vec![span(
            "claude_code.tool.execution",
            vec![kv("gen_ai.tool.name", s("Bash"))],
            vec![],
        )]);
        let normalized = run(&request, &AgentContexts::default());
        let span = &normalized.spans[0];
        assert_eq!(span.span_kind, SpanKind::Span);
        assert_eq!(span.name, "claude_code.tool.execution");
        assert!(!span.attributes.contains_key("cetra.span_name"));
    }

    #[test]
    fn claude_metadata_from_vcs() {
        let request = llm_request(vec![kv(
            "vcs.repository.url.full",
            s("https://github.com/Acme/Widgets.git"),
        )]);
        let normalized = run(&request, &AgentContexts::default());
        let metadata = &normalized.spans[0].metadata;
        assert_eq!(
            metadata.get("repository").map(String::as_str),
            Some("github.com/acme/widgets")
        );
        assert_eq!(
            metadata.get("coding_agent").map(String::as_str),
            Some("claude-code")
        );
        assert!(!metadata.contains_key("branch"));
    }

    #[test]
    fn claude_context_stamps_branch_repository_and_user() {
        let mut agents = AgentContexts::default();
        agents.insert(
            "claude-code",
            "s1",
            AgentContext {
                repository: Some("gitlab.com/x/y".to_owned()),
                branch: Some("main".to_owned()),
                user: Some("dev@example.com".to_owned()),
                updated_at: 1,
            },
        );
        let request = llm_request(vec![
            kv("session.id", s("s1")),
            kv("user.id", s("anon")),
            kv(
                "vcs.repository.url.full",
                s("https://github.com/acme/other.git"),
            ),
        ]);
        let normalized = run(&request, &agents);
        let span = &normalized.spans[0];
        assert_eq!(span.session_id.as_deref(), Some("s1"));
        assert_eq!(span.user_id.as_deref(), Some("dev@example.com"));
        assert_eq!(
            span.metadata.get("repository").map(String::as_str),
            Some("gitlab.com/x/y")
        );
        assert_eq!(
            span.metadata.get("branch").map(String::as_str),
            Some("main")
        );
    }

    #[test]
    fn claude_email_beats_the_context_user() {
        let mut agents = AgentContexts::default();
        agents.insert(
            "claude-code",
            "s1",
            AgentContext {
                user: Some("dev@example.com".to_owned()),
                ..AgentContext::default()
            },
        );
        let request = llm_request(vec![
            kv("session.id", s("s1")),
            kv("user.email", s(" A@B.example ")),
        ]);
        let normalized = run(&request, &agents);
        assert_eq!(normalized.spans[0].user_id.as_deref(), Some("a@b.example"));
    }

    #[test]
    fn claude_account_uuid_when_no_email_and_no_context() {
        let request = llm_request(vec![
            kv("user.account_uuid", s("u1")),
            kv("user.id", s("anon")),
        ]);
        let normalized = run(&request, &AgentContexts::default());
        assert_eq!(normalized.spans[0].user_id.as_deref(), Some("u1"));
    }

    #[test]
    fn claude_anonymous_id_last() {
        let request = llm_request(vec![kv("user.id", s("anon"))]);
        let normalized = run(&request, &AgentContexts::default());
        assert_eq!(normalized.spans[0].user_id.as_deref(), Some("anon"));
    }

    #[test]
    fn no_agent_no_coding_agent_key() {
        let request = request(
            "app",
            "",
            vec![span(
                "chat",
                vec![kv("gen_ai.request.model", s("m"))],
                vec![],
            )],
        );
        let normalized = run(&request, &AgentContexts::default());
        assert!(!normalized.spans[0].metadata.contains_key("coding_agent"));
    }

    #[test]
    fn client_branch_kept_when_the_agent_has_none() {
        let request = llm_request(vec![kv("cetra.metadata.branch", s("x"))]);
        let normalized = run(&request, &AgentContexts::default());
        assert_eq!(
            normalized.spans[0]
                .metadata
                .get("branch")
                .map(String::as_str),
            Some("x")
        );
    }
}
