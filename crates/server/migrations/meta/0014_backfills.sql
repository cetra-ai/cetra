CREATE TABLE backfills (
  id TEXT PRIMARY KEY,
  rule_id TEXT NOT NULL REFERENCES online_rules(id) ON DELETE CASCADE,
  from_ns INTEGER NOT NULL,
  to_ns INTEGER NOT NULL,
  max_targets INTEGER NOT NULL,
  sample_rate REAL NOT NULL,
  status TEXT NOT NULL,
  matched INTEGER NOT NULL DEFAULT 0,
  enqueued INTEGER NOT NULL DEFAULT 0,
  error TEXT,
  created_by TEXT REFERENCES users(id) ON DELETE SET NULL,
  created_at INTEGER NOT NULL,
  finished_at INTEGER
);
CREATE UNIQUE INDEX backfills_running ON backfills(rule_id) WHERE status = 'running';
CREATE INDEX backfills_rule ON backfills(rule_id, created_at);
