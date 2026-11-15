CREATE TABLE online_rules (
  id TEXT PRIMARY KEY,
  project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  evaluator_id TEXT NOT NULL REFERENCES evaluators(id),
  name TEXT NOT NULL,
  target TEXT NOT NULL,
  filter TEXT,
  variable_mapping TEXT NOT NULL,
  sample_rate REAL NOT NULL DEFAULT 1.0,
  budget_usd REAL,
  budget_period TEXT,
  enabled INTEGER NOT NULL DEFAULT 1,
  paused_reason TEXT,
  paused_at INTEGER,
  paused_detail TEXT,
  created_by TEXT REFERENCES users(id) ON DELETE SET NULL,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  UNIQUE(project_id, name)
);
CREATE INDEX online_rules_evaluator ON online_rules(evaluator_id);

CREATE TABLE eval_jobs (
  id TEXT PRIMARY KEY,
  project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  kind TEXT NOT NULL,
  priority INTEGER NOT NULL,
  rule_id TEXT REFERENCES online_rules(id) ON DELETE CASCADE,
  evaluator_id TEXT NOT NULL REFERENCES evaluators(id) ON DELETE CASCADE,
  variable_mapping TEXT,
  trace_id BLOB NOT NULL,
  span_id BLOB,
  trace_start_ns INTEGER NOT NULL,
  dedup_key TEXT NOT NULL UNIQUE,
  status TEXT NOT NULL,
  attempts INTEGER NOT NULL DEFAULT 0,
  claims INTEGER NOT NULL DEFAULT 0,
  load_retries INTEGER NOT NULL DEFAULT 0,
  interruptions INTEGER NOT NULL DEFAULT 0,
  not_before INTEGER NOT NULL,
  error_class TEXT,
  error TEXT,
  judge_trace_id BLOB,
  score_id TEXT,
  evaluator_version INTEGER,
  input_tokens INTEGER,
  output_tokens INTEGER,
  cost_usd REAL,
  latency_ms INTEGER,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  finished_at INTEGER
);
CREATE INDEX eval_jobs_ready ON eval_jobs(status, priority, not_before);
CREATE INDEX eval_jobs_rule ON eval_jobs(rule_id, created_at);
CREATE INDEX eval_jobs_project ON eval_jobs(project_id, created_at);
CREATE INDEX eval_jobs_finished ON eval_jobs(finished_at) WHERE finished_at IS NOT NULL;

CREATE TABLE rule_spend (
  rule_id TEXT NOT NULL REFERENCES online_rules(id) ON DELETE CASCADE,
  day INTEGER NOT NULL,
  spent_usd REAL NOT NULL DEFAULT 0,
  PRIMARY KEY (rule_id, day)
);
