//! Pi, OpenCode and Oh My Pi: GenAI-shaped agents with no rules of their own beyond the tool
//! rename.

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

#[test]
fn opencode_tool_span_is_renamed_and_kinds_are_generic() {
    let agent = span(
        "invoke_agent opencode",
        vec![
            kv("gen_ai.operation.name", s("invoke_agent")),
            kv("gen_ai.provider.name", s("openai")),
            kv("gen_ai.request.model", s("gpt-4o-mini")),
        ],
        vec![],
    );
    let mut llm = span(
        "chat gpt-4o-mini",
        vec![
            kv("gen_ai.operation.name", s("chat")),
            kv("gen_ai.provider.name", s("openai")),
            kv("gen_ai.request.model", s("gpt-4o-mini")),
        ],
        vec![],
    );
    llm.span_id = vec![0x33; 8];
    let mut tool = span(
        "execute_tool read",
        vec![
            kv("gen_ai.operation.name", s("execute_tool")),
            kv("gen_ai.tool.name", s("read")),
            kv("gen_ai.provider.name", s("openai")),
            kv("gen_ai.request.model", s("gpt-4o-mini")),
        ],
        vec![],
    );
    tool.span_id = vec![0x44; 8];
    let normalized = run(
        &request("opencode", "cetra-opencode", vec![agent, llm, tool]),
        &AgentContexts::default(),
    );
    assert_eq!(normalized.discarded, 0);
    assert_eq!(normalized.spans.len(), 3);
    let agent = &normalized.spans[0];
    assert_eq!(agent.name, "invoke_agent opencode");
    assert_eq!(agent.span_kind, SpanKind::Agent);
    assert_eq!(normalized.spans[1].span_kind, SpanKind::Llm);
    let tool = &normalized.spans[2];
    assert_eq!(tool.name, "read");
    assert_eq!(tool.span_kind, SpanKind::Tool);
    assert_eq!(
        tool.attributes.get("cetra.span_name").map(String::as_str),
        Some("execute_tool read")
    );
    assert_eq!(
        tool.metadata.get("coding_agent").map(String::as_str),
        Some("opencode")
    );
}

#[test]
fn oh_my_pi_tool_span_is_renamed_and_kinds_are_generic() {
    let agent = span(
        "invoke_agent omp",
        vec![
            kv("gen_ai.operation.name", s("invoke_agent")),
            kv("gen_ai.provider.name", s("openai")),
            kv("gen_ai.request.model", s("gpt-4o-mini")),
        ],
        vec![],
    );
    let mut llm = span(
        "chat gpt-4o-mini",
        vec![
            kv("gen_ai.operation.name", s("chat")),
            kv("gen_ai.provider.name", s("openai")),
            kv("gen_ai.request.model", s("gpt-4o-mini")),
        ],
        vec![],
    );
    llm.span_id = vec![0x33; 8];
    let mut tool = span(
        "execute_tool read",
        vec![
            kv("gen_ai.operation.name", s("execute_tool")),
            kv("gen_ai.tool.name", s("read")),
            kv("gen_ai.provider.name", s("openai")),
            kv("gen_ai.request.model", s("gpt-4o-mini")),
        ],
        vec![],
    );
    tool.span_id = vec![0x44; 8];
    let normalized = run(
        &request("omp", "@oh-my-pi/pi-agent-core", vec![agent, llm, tool]),
        &AgentContexts::default(),
    );
    assert_eq!(normalized.discarded, 0);
    assert_eq!(normalized.spans.len(), 3);
    let agent = &normalized.spans[0];
    assert_eq!(agent.name, "invoke_agent omp");
    assert_eq!(agent.span_kind, SpanKind::Agent);
    assert_eq!(normalized.spans[1].span_kind, SpanKind::Llm);
    let tool = &normalized.spans[2];
    assert_eq!(tool.name, "read");
    assert_eq!(tool.span_kind, SpanKind::Tool);
    assert_eq!(
        tool.attributes.get("cetra.span_name").map(String::as_str),
        Some("execute_tool read")
    );
    assert_eq!(
        tool.metadata.get("coding_agent").map(String::as_str),
        Some("oh-my-pi")
    );
}
