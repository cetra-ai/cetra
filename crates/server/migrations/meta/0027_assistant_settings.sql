-- The org's in-app assistant setting (track AS, D-AS-3).
CREATE TABLE org_assistant_settings (
  org_id TEXT PRIMARY KEY REFERENCES orgs(id) ON DELETE CASCADE,
  enabled INTEGER NOT NULL DEFAULT 0,
  connection_id TEXT REFERENCES llm_connections(id) ON DELETE SET NULL,
  model TEXT,
  updated_by TEXT REFERENCES users(id) ON DELETE SET NULL,
  updated_at INTEGER NOT NULL
);
