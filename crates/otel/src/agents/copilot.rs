//! GitHub Copilot spans (spec 02 § Coding agents; D-CA-4, D-CA-6).
//!
//! Copilot has no hook, so there is no session lookup. Its repository and branch come
//! from its own git attributes, and its tool spans take the tool's name.

use cetra_core::enums::SpanKind;
use cetra_core::span::Span;

use super::{Finish, canonical_repository, key, rename_tool, take_text};
use crate::bag::AttrBag;
use crate::context::ContextFields;

/// The repository (canonical, D-CA-4) and branch from Copilot's git attributes. The first
/// attribute that gives a value is used, and it is taken.
pub(crate) fn context(bag: &mut AttrBag<'_>, fields: &mut ContextFields) {
    let repository = take_text(bag, "github.copilot.git.repository")
        .and_then(|url| canonical_repository(&url))
        .or_else(|| {
            take_text(bag, "copilot_chat.repo.remote_url")
                .and_then(|url| canonical_repository(&url))
        });
    let branch = take_text(bag, "github.copilot.git.branch")
        .or_else(|| take_text(bag, "copilot_chat.repo.head_branch_name"));
    if let Some(repository) = repository {
        fields
            .metadata
            .insert(key::REPOSITORY.to_owned(), repository);
    }
    if let Some(branch) = branch {
        fields.metadata.insert(key::BRANCH.to_owned(), branch);
    }
}

/// A span of kind tool takes the tool's name (D-CA-6).
pub(crate) fn finish(kind: SpanKind, inputs: &Finish, built: &mut Span) {
    if kind != SpanKind::Tool {
        return;
    }
    if let Some(name) = inputs.tool_name.as_deref() {
        rename_tool(built, name);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::AgentContexts;
    use crate::agents::test_support::{kv, request, run, s, span};

    #[test]
    fn copilot_tool_is_renamed_and_git_metadata_set() {
        let tool = span(
            "execute_tool bash",
            vec![
                kv("gen_ai.operation.name", s("execute_tool")),
                kv("gen_ai.tool.name", s("bash")),
                kv(
                    "github.copilot.git.repository",
                    s("https://github.com/Acme/Widgets.git"),
                ),
                kv("github.copilot.git.branch", s("main")),
            ],
            vec![],
        );
        let normalized = run(
            &request("github-copilot", "github.copilot.chat", vec![tool]),
            &AgentContexts::default(),
        );
        let span = &normalized.spans[0];
        assert_eq!(span.name, "bash");
        assert_eq!(span.span_kind, SpanKind::Tool);
        assert_eq!(
            span.attributes.get("cetra.span_name").map(String::as_str),
            Some("execute_tool bash")
        );
        assert_eq!(
            span.metadata.get("repository").map(String::as_str),
            Some("github.com/acme/widgets")
        );
        assert_eq!(
            span.metadata.get("branch").map(String::as_str),
            Some("main")
        );
        assert_eq!(
            span.metadata.get("coding_agent").map(String::as_str),
            Some("copilot")
        );
    }
}
