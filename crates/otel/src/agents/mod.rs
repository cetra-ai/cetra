//! Coding-agent contexts and detection (spec 02 § Coding agents, D-CA-4, D-CA-9).
//!
//! The server stores one `AgentContext` per (agent, session id) per project
//! and passes the lookup to `normalize`, which only reads it.

mod claude_code;
mod codex;
mod copilot;
mod gemini_cli;
#[cfg(test)]
mod test_support;

use std::collections::HashMap;

use cetra_core::enums::SpanKind;
use cetra_core::span::Span;
use opentelemetry_proto::tonic::common::v1::{AnyValue, KeyValue};
use opentelemetry_proto::tonic::trace::v1::span::Event;

use crate::bag::AttrBag;
use crate::context::ContextFields;
use crate::payload::{Assembled, Payload};
use crate::usage::Usage;
use crate::value::{InvalidNumber, as_i64, as_str};
use crate::warn::{Warning, Warnings};

/// One coding-agent session's context (D-CA-9), as the server stores it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AgentContext {
    /// Canonical form of D-CA-4 (`canonical_repository`), or `None`.
    pub repository: Option<String>,
    pub branch: Option<String>,
    pub user: Option<String>,
    /// Last write, ns since the Unix epoch. `normalize` ignores it; the server's expiry and cap use it.
    pub updated_at: i64,
}

/// The contexts of one project, keyed by (agent, session id). `normalize` only reads it.
#[derive(Debug, Clone, Default)]
pub struct AgentContexts {
    contexts: HashMap<(String, String), AgentContext>,
}

impl AgentContexts {
    pub fn get(&self, agent: &str, session_id: &str) -> Option<&AgentContext> {
        self.contexts
            .get(&(agent.to_owned(), session_id.to_owned()))
    }

    pub fn insert(&mut self, agent: &str, session_id: &str, context: AgentContext) {
        self.contexts
            .insert((agent.to_owned(), session_id.to_owned()), context);
    }

    pub fn remove(&mut self, agent: &str, session_id: &str) -> Option<AgentContext> {
        self.contexts
            .remove(&(agent.to_owned(), session_id.to_owned()))
    }

    pub fn retain(&mut self, mut keep: impl FnMut(&str, &str, &AgentContext) -> bool) {
        self.contexts
            .retain(|(agent, session_id), context| keep(agent, session_id, context));
    }

    pub fn len(&self) -> usize {
        self.contexts.len()
    }

    pub fn is_empty(&self) -> bool {
        self.contexts.is_empty()
    }
}

/// `https://user:tok@GitHub.com:443/O/R.git/` → `github.com/o/r`; `git@github.com:o/r.git` → `github.com/o/r`;
/// `ssh://git@host/o/r` → `host/o/r`. `None` when no host and path remain.
pub fn canonical_repository(remote: &str) -> Option<String> {
    let mut text = remote.trim().to_lowercase();

    // Step 2: scheme, or scp-like `<user>@<host>:<path>`.
    if let Some(at) = text.find("://") {
        text = text[at + 3..].to_owned();
    } else if let Some(colon) = text.find(':') {
        let before = &text[..colon];
        if let Some(at) = before.find('@').filter(|_| !before.contains('/')) {
            text = format!("{}/{}", &text[at + 1..colon], &text[colon + 1..]);
        }
    }

    // Step 3: user-info, everything through the last `@` before the first `/`.
    let slash = text.find('/').unwrap_or(text.len());
    if let Some(at) = text[..slash].rfind('@') {
        text = text[at + 1..].to_owned();
    }

    // Step 4: host and path, with a `:<digits>` port removed from the host.
    let (host, path) = match text.find('/') {
        Some(slash) => (&text[..slash], &text[slash + 1..]),
        None => (text.as_str(), ""),
    };
    let host = match host.rsplit_once(':') {
        Some((name, port))
            if !port.is_empty() && port.bytes().all(|byte| byte.is_ascii_digit()) =>
        {
            name
        }
        _ => host,
    };

    // Step 5: trailing `/`, one `.git`, trailing `/`.
    let path = path.trim_end_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let path = path.trim_end_matches('/');

    // Step 6: the non-empty segments; at least two are needed.
    let segments: Vec<&str> = path
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect();
    if host.is_empty() || segments.len() < 2 {
        return None;
    }
    Some(format!("{host}/{}", segments.join("/")))
}

/// The coding agents that spec 02 § Coding agents recognizes (D-CA-3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CodingAgent {
    ClaudeCode,
    Codex,
    GeminiCli,
    Copilot,
}

impl CodingAgent {
    /// The `coding_agent` metadata value and the session context's agent name (D-CA-4).
    pub(crate) fn wire_name(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude-code",
            Self::Codex => "codex",
            Self::GeminiCli => "gemini-cli",
            Self::Copilot => "copilot",
        }
    }
}

/// Where a renamed tool span keeps its original name (D-CA-6).
pub(crate) const SPAN_NAME_ATTRIBUTE: &str = "cetra.span_name";

/// The D-CA-4 metadata keys.
pub(crate) mod key {
    pub(crate) const CODING_AGENT: &str = "coding_agent";
    pub(crate) const REPOSITORY: &str = "repository";
    pub(crate) const BRANCH: &str = "branch";
}

/// Redaction placeholders mean the content was not captured (D-CA-6).
pub(crate) fn is_redacted(text: &str) -> bool {
    matches!(text, "<REDACTED>" | "[REDACTED]")
}

/// Values the post-build step needs, read from the bag before it is consumed.
#[derive(Debug, Default)]
pub(crate) struct Finish {
    /// The tool's name, which a tool span takes (D-CA-6).
    pub(crate) tool_name: Option<String>,
    /// Codex only: the turn's id, which the spans are re-linked to (D-CA-6).
    pub(crate) turn_id: Option<String>,
}

/// D-CA-3. The first rule that matches wins.
pub(crate) fn detect_agent(
    service_name: Option<&str>,
    scope_name: Option<&str>,
    span_name: &str,
    attributes: &[KeyValue],
) -> Option<CodingAgent> {
    if span_name.starts_with("claude_code.") {
        return Some(CodingAgent::ClaudeCode);
    }
    if service_name.is_some_and(|name| name.starts_with("codex")) {
        return Some(CodingAgent::Codex);
    }
    let gemini_by_attribute =
        attribute(attributes, "gen_ai.agent.name").and_then(as_str) == Some("gemini-cli");
    if gemini_by_attribute || service_name == Some("gemini-cli") {
        return Some(CodingAgent::GeminiCli);
    }
    let copilot = scope_name
        .is_some_and(|name| name.starts_with("github.copilot") || name.starts_with("copilot"))
        || matches!(service_name, Some("github-copilot" | "copilot-chat"))
        || attributes
            .iter()
            .any(|attr| attr.key.starts_with("github.copilot."));
    if copilot {
        return Some(CodingAgent::Copilot);
    }
    None
}

/// The last attribute with `key`, which is the one the bag keeps.
fn attribute<'a>(attributes: &'a [KeyValue], key: &str) -> Option<&'a AnyValue> {
    attributes
        .iter()
        .rev()
        .find(|attr| attr.key == key)
        .and_then(|attr| attr.value.as_ref())
}

/// D-CA-6: Codex's internal spans are dropped and counted, never stored or rejected.
pub(crate) fn discarded(
    agent: Option<CodingAgent>,
    span_name: &str,
    attributes: &[KeyValue],
) -> bool {
    agent == Some(CodingAgent::Codex) && !codex::kept(span_name, attributes)
}

/// The values [`finish`] needs. Read before the bag is consumed.
pub(crate) fn finish_inputs(agent: Option<CodingAgent>, bag: &AttrBag<'_>) -> Finish {
    match agent {
        Some(CodingAgent::ClaudeCode) => Finish {
            tool_name: claude_code::tool_name(bag),
            turn_id: None,
        },
        Some(CodingAgent::Codex) => Finish {
            tool_name: peek_text(bag, "tool_name"),
            turn_id: codex::turn_id(bag),
        },
        Some(CodingAgent::GeminiCli | CodingAgent::Copilot) => Finish {
            tool_name: peek_text(bag, "gen_ai.tool.name"),
            turn_id: None,
        },
        None => Finish::default(),
    }
}

/// The span kind an agent decides, or `None` for the generic mapping.
pub(crate) fn kind(agent: Option<CodingAgent>, span_name: &str) -> Option<SpanKind> {
    match agent? {
        CodingAgent::ClaudeCode => claude_code::kind(span_name),
        CodingAgent::Codex => codex::kind(span_name),
        CodingAgent::GeminiCli => gemini_cli::kind(span_name),
        CodingAgent::Copilot => None,
    }
}

/// The D-CA-4 metadata, and the session context's person, repository and branch (D-CA-5, D-CA-9).
pub(crate) fn context(
    agent: Option<CodingAgent>,
    span_name: &str,
    attributes: &[KeyValue],
    bag: &mut AttrBag<'_>,
    fields: &mut ContextFields,
    agents: &AgentContexts,
) {
    let Some(agent) = agent else {
        return;
    };
    fields
        .metadata
        .insert(key::CODING_AGENT.to_owned(), agent.wire_name().to_owned());
    match agent {
        CodingAgent::ClaudeCode => claude_code::context(bag, fields, agents),
        CodingAgent::Codex => codex::context(span_name, attributes, bag, fields, agents),
        CodingAgent::GeminiCli => gemini_cli::context(fields, agents),
        CodingAgent::Copilot => copilot::context(bag, fields),
    }
}

/// Token columns the agent reads from its own names, or `None` for the generic rules.
pub(crate) fn usage(
    agent: Option<CodingAgent>,
    span_name: &str,
    kind: SpanKind,
    bag: &mut AttrBag<'_>,
    warnings: &mut Warnings,
) -> Option<Usage> {
    match agent? {
        CodingAgent::ClaudeCode => claude_code::usage(span_name, kind, bag, warnings),
        CodingAgent::Codex => codex::usage(span_name, kind, bag, warnings),
        CodingAgent::GeminiCli | CodingAgent::Copilot => None,
    }
}

/// Time to first token from the agent's own name, or `None`.
pub(crate) fn ttft(
    agent: Option<CodingAgent>,
    bag: &mut AttrBag<'_>,
    warnings: &mut Warnings,
) -> Option<i64> {
    match agent? {
        CodingAgent::ClaudeCode => claude_code::ttft(bag, warnings),
        CodingAgent::Codex | CodingAgent::GeminiCli | CodingAgent::Copilot => None,
    }
}

/// Payloads the agent reads from its own attributes and events (D-CA-6).
pub(crate) fn payload(
    agent: Option<CodingAgent>,
    span_name: &str,
    bag: &mut AttrBag<'_>,
    events: &[Event],
    assembled: &mut Assembled,
    warnings: &mut Warnings,
) {
    match agent {
        Some(CodingAgent::ClaudeCode) => {
            claude_code::payload(span_name, bag, events, assembled, warnings);
        }
        Some(CodingAgent::GeminiCli) => gemini_cli::payload(span_name, bag, assembled, warnings),
        Some(CodingAgent::Codex | CodingAgent::Copilot) | None => {}
    }
}

/// The span's name, parent and id, and its resource attributes, after the build (D-CA-6).
pub(crate) fn finish(
    agent: Option<CodingAgent>,
    span_name: &str,
    kind: SpanKind,
    inputs: &Finish,
    built: &mut Span,
) {
    match agent {
        Some(CodingAgent::ClaudeCode) => claude_code::finish(span_name, inputs, built),
        Some(CodingAgent::Codex) => codex::finish(span_name, inputs, built),
        Some(CodingAgent::GeminiCli) => gemini_cli::finish(span_name, inputs, built),
        Some(CodingAgent::Copilot) => copilot::finish(kind, inputs, built),
        None => {}
    }
}

/// A tool span takes the tool's name, and the original name moves to `cetra.span_name` (D-CA-6).
/// Nothing changes when the name is the same.
pub(crate) fn rename_tool(built: &mut Span, name: &str) {
    if built.name == name {
        return;
    }
    let original = std::mem::replace(&mut built.name, name.to_owned());
    built
        .attributes
        .insert(SPAN_NAME_ATTRIBUTE.to_owned(), original);
}

/// A non-empty string, peeked: the bag keeps the key unread.
fn peek_text(bag: &AttrBag<'_>, name: &str) -> Option<String> {
    bag.get(name)
        .and_then(as_str)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
}

/// A trimmed, non-empty string. It is taken when it is returned.
pub(crate) fn take_text(bag: &mut AttrBag<'_>, name: &str) -> Option<String> {
    let text = bag
        .get(name)
        .and_then(as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())?
        .to_owned();
    bag.take(name);
    Some(text)
}

/// A non-negative count, taken when it is read. A negative or non-numeric value is
/// invalid (rule 20) and stays unread.
pub(crate) fn count(bag: &mut AttrBag<'_>, name: &str, warnings: &mut Warnings) -> Option<i64> {
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

/// Sets the input and drops the input error of the convention that read it (N2 rule).
pub(crate) fn set_input(assembled: &mut Assembled, payload: Payload) {
    assembled.io.input = Some(payload);
    assembled.io.errors.retain(|code| !code.ends_with(":input"));
}

/// Sets the output and drops the output error of the convention that read it (N2 rule).
pub(crate) fn set_output(assembled: &mut Assembled, payload: Payload) {
    assembled.io.output = Some(payload);
    assembled
        .io
        .errors
        .retain(|code| !code.ends_with(":output"));
}

/// The session context of `agent` for `fields.session_id` (D-CA-5 rank 2, D-CA-9): its user,
/// repository and branch replace the span's own values. Nothing is removed when the context
/// has no value.
pub(crate) fn apply_session_context(
    agents: &AgentContexts,
    agent: &str,
    fields: &mut ContextFields,
) {
    let Some(stored) = fields
        .session_id
        .as_deref()
        .and_then(|session| agents.get(agent, session))
    else {
        return;
    };
    let user = stored
        .user
        .as_deref()
        .map(|text| text.trim().to_lowercase())
        .filter(|text| !text.is_empty());
    let repository = stored.repository.clone();
    let branch = stored.branch.clone();
    if let Some(user) = user {
        fields.user_id = Some(user);
    }
    if let Some(repository) = repository {
        fields
            .metadata
            .insert(key::REPOSITORY.to_owned(), repository);
    }
    if let Some(branch) = branch {
        fields.metadata.insert(key::BRANCH.to_owned(), branch);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::test_support::{kv, s};

    #[test]
    fn detect_agent_reads_each_signal() {
        let none: &[KeyValue] = &[];
        assert_eq!(
            detect_agent(None, None, "claude_code.tool", none),
            Some(CodingAgent::ClaudeCode)
        );
        assert_eq!(
            detect_agent(Some("codex_exec"), None, "exec_command", none),
            Some(CodingAgent::Codex)
        );
        assert_eq!(
            detect_agent(
                None,
                None,
                "llm_call",
                &[kv("gen_ai.agent.name", s("gemini-cli"))]
            ),
            Some(CodingAgent::GeminiCli)
        );
        assert_eq!(
            detect_agent(Some("gemini-cli"), None, "llm_call", none),
            Some(CodingAgent::GeminiCli)
        );
        assert_eq!(
            detect_agent(None, Some("github.copilot.chat"), "chat", none),
            Some(CodingAgent::Copilot)
        );
        assert_eq!(
            detect_agent(Some("github-copilot"), None, "chat", none),
            Some(CodingAgent::Copilot)
        );
        assert_eq!(
            detect_agent(
                Some("renamed"),
                None,
                "chat",
                &[kv("github.copilot.turn_id", s("t1"))]
            ),
            Some(CodingAgent::Copilot)
        );
        assert_eq!(
            detect_agent(
                Some("app"),
                Some("my.scope"),
                "chat",
                &[kv("gen_ai.request.model", s("m"))]
            ),
            None
        );
    }

    #[test]
    fn wire_names_are_the_d_ca_4_values() {
        let names: Vec<&str> = [
            CodingAgent::ClaudeCode,
            CodingAgent::Codex,
            CodingAgent::GeminiCli,
            CodingAgent::Copilot,
        ]
        .into_iter()
        .map(CodingAgent::wire_name)
        .collect();
        assert_eq!(names, ["claude-code", "codex", "gemini-cli", "copilot"]);
    }

    #[test]
    fn canonical_repository_forms() {
        let cases = [
            (
                "https://github.com/Acme/Widgets.git",
                Some("github.com/acme/widgets"),
            ),
            ("https://tok@github.com/o/r/", Some("github.com/o/r")),
            ("git@github.com:o/r.git", Some("github.com/o/r")),
            ("ssh://git@host.example:2222/o/r", Some("host.example/o/r")),
            (
                "https://gitlab.com/g/sub/proj.git",
                Some("gitlab.com/g/sub/proj"),
            ),
            ("HTTPS://GitHub.com/O/R", Some("github.com/o/r")),
            ("https://github.com/acme", None),
            ("/home/u/repo", None),
            ("", None),
        ];
        for (remote, want) in cases {
            assert_eq!(canonical_repository(remote).as_deref(), want, "{remote}");
        }
    }

    #[test]
    fn agent_contexts_insert_get_remove_retain() {
        let context = |user: &str| AgentContext {
            user: Some(user.to_owned()),
            ..AgentContext::default()
        };
        let mut contexts = AgentContexts::default();
        assert!(contexts.is_empty());
        contexts.insert("claude-code", "s1", context("a"));
        contexts.insert("codex", "s1", context("b"));
        contexts.insert("codex", "s2", context("c"));
        assert_eq!(contexts.len(), 3);
        assert_eq!(contexts.get("codex", "s1"), Some(&context("b")));
        assert_eq!(contexts.get("codex", "missing"), None);

        contexts.insert("codex", "s1", context("d"));
        assert_eq!(contexts.len(), 3);
        assert_eq!(contexts.get("codex", "s1"), Some(&context("d")));

        assert_eq!(contexts.remove("claude-code", "s1"), Some(context("a")));
        assert_eq!(contexts.remove("claude-code", "s1"), None);

        contexts.retain(|agent, _, _| agent == "codex");
        assert_eq!(contexts.len(), 2);
        assert!(contexts.get("claude-code", "s1").is_none());
        assert!(!contexts.is_empty());
    }

    #[test]
    fn copilot_pseudo_id_is_the_user_when_nothing_else_is() {
        use crate::agents::test_support::{request, run, span};
        let mut pseudo = span("chat", vec![kv("enduser.pseudo.id", s("pseudo-1"))], vec![]);
        pseudo.span_id = vec![0x31; 8];
        let normalized = run(
            &request("github-copilot", "", vec![pseudo]),
            &AgentContexts::default(),
        );
        assert_eq!(normalized.spans[0].user_id.as_deref(), Some("pseudo-1"));
        assert_eq!(
            normalized.spans[0]
                .metadata
                .get(key::CODING_AGENT)
                .map(String::as_str),
            Some("copilot")
        );
    }
}
