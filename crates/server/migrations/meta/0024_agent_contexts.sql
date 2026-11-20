-- Coding-agent session contexts (D-CA-9): the repository, branch and person a hook reported.
CREATE TABLE agent_contexts (
  project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  agent TEXT NOT NULL,          -- claude-code | codex | gemini-cli | copilot
  session_id TEXT NOT NULL,
  repository TEXT,              -- canonical form (D-CA-4)
  branch TEXT,
  user_id TEXT,                 -- the hook's `user`, lowercased
  updated_at INTEGER NOT NULL,  -- ns since the Unix epoch, time of the last write
  PRIMARY KEY (project_id, agent, session_id)
);
CREATE INDEX agent_contexts_user ON agent_contexts(project_id, user_id) WHERE user_id IS NOT NULL;
CREATE INDEX agent_contexts_updated ON agent_contexts(project_id, updated_at);
