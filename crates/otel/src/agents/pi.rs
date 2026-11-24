//! Pi spans (spec 02 § Coding agents; D-IN-27 item 2, D-IN-28).
//!
//! Pi's spans come from cetra's own pi extension, which sets the scope `cetra-pi`. Their
//! kinds are the generic GenAI reading. Only a tool span is renamed, after its tool, and
//! nothing is dropped.

use cetra_core::span::Span;

use super::{Finish, rename_tool};

/// The prefix of the tool spans (`execute_tool <name>`), which are renamed (D-CA-6).
const TOOL_PREFIX: &str = "execute_tool ";

/// An `execute_tool` span takes the tool's name (D-CA-6).
pub(crate) fn finish(span_name: &str, inputs: &Finish, built: &mut Span) {
    if span_name.starts_with(TOOL_PREFIX)
        && let Some(name) = inputs.tool_name.as_deref()
    {
        rename_tool(built, name);
    }
}

#[cfg(test)]
mod tests {
    use cetra_core::enums::SpanKind;

    use crate::agents::AgentContexts;
    use crate::agents::test_support::{kv, request, run, s, span};

    #[test]
    fn pi_tool_span_is_renamed_and_kinds_are_generic() {
        let agent = span(
            "invoke_agent pi",
            vec![kv("gen_ai.operation.name", s("invoke_agent"))],
            vec![],
        );
        let mut llm = span(
            "chat model-x",
            vec![kv("gen_ai.operation.name", s("chat"))],
            vec![],
        );
        llm.span_id = vec![0x33; 8];
        let mut tool = span(
            "execute_tool bash",
            vec![
                kv("gen_ai.operation.name", s("execute_tool")),
                kv("gen_ai.tool.name", s("bash")),
            ],
            vec![],
        );
        tool.span_id = vec![0x44; 8];
        let normalized = run(
            &request("app", "cetra-pi", vec![agent, llm, tool]),
            &AgentContexts::default(),
        );
        assert_eq!(normalized.discarded, 0);
        assert_eq!(normalized.spans.len(), 3);
        let agent = &normalized.spans[0];
        assert_eq!(agent.name, "invoke_agent pi");
        assert_eq!(agent.span_kind, SpanKind::Agent);
        assert_eq!(normalized.spans[1].span_kind, SpanKind::Llm);
        let tool = &normalized.spans[2];
        assert_eq!(tool.name, "bash");
        assert_eq!(tool.span_kind, SpanKind::Tool);
        assert_eq!(
            tool.attributes.get("cetra.span_name").map(String::as_str),
            Some("execute_tool bash")
        );
        assert_eq!(
            tool.metadata.get("coding_agent").map(String::as_str),
            Some("pi")
        );
    }
}
