-- Track V: the prompt registry (D-V-7, D-V-8, D-V-10, D-V-11, spec 04 § Prompts).
CREATE TABLE prompts (
  id TEXT PRIMARY KEY,
  project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  name TEXT NOT NULL,                   -- immutable (D-V-2)
  type TEXT NOT NULL CHECK (type IN ('text', 'chat')),
  description TEXT,
  tags TEXT NOT NULL DEFAULT '[]',      -- JSON array of strings
  latest_version INTEGER NOT NULL,
  created_by TEXT,                      -- a user id or key:<key id>
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  UNIQUE(project_id, name)
);

CREATE TABLE prompt_versions (
  prompt_id TEXT NOT NULL REFERENCES prompts(id) ON DELETE CASCADE,
  version INTEGER NOT NULL,
  template TEXT NOT NULL,               -- JSON: a string (text) or an array (chat)
  config TEXT NOT NULL DEFAULT '{}',    -- JSON object (D-V-6)
  commit_message TEXT,
  created_by TEXT,
  created_at INTEGER NOT NULL,
  PRIMARY KEY (prompt_id, version)
);

CREATE TABLE prompt_labels (
  prompt_id TEXT NOT NULL REFERENCES prompts(id) ON DELETE CASCADE,
  label TEXT NOT NULL,
  version INTEGER NOT NULL,
  updated_by TEXT,
  updated_at INTEGER NOT NULL,
  PRIMARY KEY (prompt_id, label),
  FOREIGN KEY (prompt_id, version) REFERENCES prompt_versions(prompt_id, version)
);

CREATE TABLE prompt_label_events (
  id INTEGER PRIMARY KEY,               -- rowid: the history's order
  prompt_id TEXT NOT NULL REFERENCES prompts(id) ON DELETE CASCADE,
  label TEXT NOT NULL,
  from_version INTEGER,                 -- NULL: the label was new
  to_version INTEGER,                   -- NULL: the label was removed
  actor TEXT,
  at INTEGER NOT NULL
);
CREATE INDEX prompt_label_events_prompt ON prompt_label_events(prompt_id, id);

CREATE TABLE prompt_settings (
  project_id TEXT PRIMARY KEY REFERENCES projects(id) ON DELETE CASCADE,
  protected_labels TEXT NOT NULL,       -- JSON array; no row means ["production"] (D-V-10)
  updated_by TEXT,
  updated_at INTEGER NOT NULL
);
