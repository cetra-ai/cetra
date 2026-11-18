CREATE TABLE dashboards (
  id TEXT PRIMARY KEY,                -- UUIDv7
  project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  name TEXT NOT NULL,
  description TEXT,
  definition TEXT NOT NULL,           -- JSON: {widgets: [Widget]}, validated by the server
  version INTEGER NOT NULL DEFAULT 1, -- +1 on every update; a PATCH names the version it read
  created_by TEXT REFERENCES users(id) ON DELETE SET NULL,
  updated_by TEXT REFERENCES users(id) ON DELETE SET NULL,
  created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL
);
CREATE INDEX dashboards_project ON dashboards(project_id, name);
