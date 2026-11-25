//! Qwen Code spans (spec 02 § Coding agents; D-CA-5, D-CA-6, D-IN-28).
//!
//! Qwen Code is a Gemini CLI fork. Its session context comes from its hook, and its tool
//! spans take the tool's name. The generic rules decide the rest.

use cetra_core::enums::SpanKind;
use cetra_core::span::Span;

use super::{AgentContexts, Finish, apply_session_context, rename_tool};
use crate::context::ContextFields;

const WIRE_NAME: &str = "qwen-code";
/// The tool span, renamed after its tool (D-CA-6).
const TOOL: &str = "qwen-code.tool";
/// The hook's span, which is internal and dropped.
const HOOK: &str = "qwen-code.hook";
/// The scope prefix of the OpenTelemetry HTTP instrumentations, which are dropped.
const INSTRUMENTATION_SCOPE: &str = "@opentelemetry/instrumentation-";

/// The span kinds of Qwen Code's spans. Other names use the generic mapping.
pub(crate) fn kind(span_name: &str) -> Option<SpanKind> {
    Some(match span_name {
        "qwen-code.interaction" => SpanKind::Agent,
        _ => return None,
    })
}

/// The hook's span and the HTTP instrumentation's spans are internal (D-IN-28).
pub(crate) fn discarded(span_name: &str, scope_name: &str) -> bool {
    span_name == HOOK || scope_name.starts_with(INSTRUMENTATION_SCOPE)
}

/// The session context of the span's session id (D-CA-5 rank 2, D-CA-9).
pub(crate) fn context(fields: &mut ContextFields, agents: &AgentContexts) {
    apply_session_context(agents, WIRE_NAME, fields);
}

/// A tool span takes the tool's name (D-CA-6).
pub(crate) fn finish(span_name: &str, inputs: &Finish, built: &mut Span) {
    if span_name == TOOL
        && let Some(name) = inputs.tool_name.as_deref()
    {
        rename_tool(built, name);
    }
}

#[cfg(test)]
mod tests {
    use cetra_core::enums::SpanKind;

    use crate::agents::test_support::{kv, request, run, s, span};
    use crate::agents::{AgentContext, AgentContexts};

    #[test]
    fn qwen_code_spans_get_their_kinds_and_tool_name() {
        let interaction = span("qwen-code.interaction", vec![], vec![]);
        let mut llm = span(
            "qwen-code.llm_request",
            vec![kv("gen_ai.operation.name", s("chat"))],
            vec![],
        );
        llm.span_id = vec![0x33; 8];
        let mut tool = span(
            "qwen-code.tool",
            vec![kv("gen_ai.tool.name", s("read_file"))],
            vec![],
        );
        tool.span_id = vec![0x44; 8];
        let normalized = run(
            &request("qwen-code", "", vec![interaction, llm, tool]),
            &AgentContexts::default(),
        );
        assert_eq!(normalized.discarded, 0);
        let by_name = |name: &str| {
            normalized
                .spans
                .iter()
                .find(|span| span.name == name)
                .expect("span")
        };
        let agent = by_name("qwen-code.interaction");
        assert_eq!(agent.span_kind, SpanKind::Agent);
        assert_eq!(
            agent.metadata.get("coding_agent").map(String::as_str),
            Some("qwen-code")
        );
        assert_eq!(by_name("qwen-code.llm_request").span_kind, SpanKind::Llm);
        let tool = by_name("read_file");
        assert_eq!(tool.span_kind, SpanKind::Tool);
        assert_eq!(
            tool.attributes.get("cetra.span_name").map(String::as_str),
            Some("qwen-code.tool")
        );
    }

    #[test]
    fn qwen_code_hook_and_http_spans_are_discarded() {
        let hook = span("qwen-code.hook", vec![], vec![]);
        let mut http = span("GET", vec![], vec![]);
        http.span_id = vec![0x33; 8];
        let mut kept = span("qwen-code.hook", vec![], vec![]);
        kept.span_id = vec![0x44; 8];
        let mut kept_http = span("GET", vec![], vec![]);
        kept_http.span_id = vec![0x55; 8];

        let qwen = request("qwen-code", "", vec![hook]);
        let qwen_http = request(
            "qwen-code",
            "@opentelemetry/instrumentation-undici",
            vec![http],
        );
        let other = request(
            "app",
            "@opentelemetry/instrumentation-undici",
            vec![kept.clone(), kept_http],
        );
        let other_scope = request("app", "", vec![kept]);

        let normalized = run(&qwen, &AgentContexts::default());
        assert!(normalized.spans.is_empty());
        assert_eq!(normalized.discarded, 1);

        let normalized = run(&qwen_http, &AgentContexts::default());
        assert!(normalized.spans.is_empty());
        assert_eq!(normalized.discarded, 1);

        let normalized = run(&other, &AgentContexts::default());
        assert_eq!(normalized.spans.len(), 2);
        assert_eq!(normalized.discarded, 0);

        let normalized = run(&other_scope, &AgentContexts::default());
        assert_eq!(normalized.spans.len(), 1);
        assert_eq!(normalized.discarded, 0);
    }

    #[test]
    fn qwen_code_session_context_sets_repository_and_branch() {
        let mut agents = AgentContexts::default();
        agents.insert(
            "qwen-code",
            "s1",
            AgentContext {
                repository: Some("github.com/o/r".to_owned()),
                branch: Some("main".to_owned()),
                ..AgentContext::default()
            },
        );
        let llm = span(
            "qwen-code.llm_request",
            vec![kv("session.id", s("s1"))],
            vec![],
        );
        let normalized = run(&request("qwen-code", "", vec![llm]), &agents);
        let span = &normalized.spans[0];
        assert_eq!(
            span.metadata.get("repository").map(String::as_str),
            Some("github.com/o/r")
        );
        assert_eq!(
            span.metadata.get("branch").map(String::as_str),
            Some("main")
        );
    }
}
