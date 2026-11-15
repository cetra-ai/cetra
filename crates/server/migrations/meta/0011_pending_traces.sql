CREATE TABLE pending_traces (
  project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  trace_id BLOB NOT NULL,
  first_seen INTEGER NOT NULL,
  last_seen INTEGER NOT NULL,
  min_start INTEGER NOT NULL,
  root_ended INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (project_id, trace_id)
);
