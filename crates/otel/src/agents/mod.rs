//! Coding-agent contexts and detection (spec 02 § Coding agents, D-CA-4, D-CA-9).
//!
//! The server stores one `AgentContext` per (agent, session id) per project
//! and passes the lookup to `normalize`, which only reads it.

use std::collections::HashMap;

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

#[cfg(test)]
mod tests {
    use super::*;

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
}
