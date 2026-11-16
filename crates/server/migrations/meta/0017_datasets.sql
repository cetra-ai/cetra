-- Track J: datasets, versions, item revisions, experiments and their results (D-J-1, spec 04).
CREATE TABLE datasets (
  id TEXT PRIMARY KEY,
  project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  name TEXT NOT NULL,
  description TEXT,
  input_schema TEXT,                  -- JSON Schema text, or NULL
  expected_schema TEXT,
  current_version INTEGER NOT NULL DEFAULT 0,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  UNIQUE(project_id, name)
);

CREATE TABLE dataset_versions (
  dataset_id TEXT NOT NULL REFERENCES datasets(id) ON DELETE CASCADE,
  version INTEGER NOT NULL,
  description TEXT NOT NULL,
  created_by TEXT,                    -- user id, or 'key:<key id>'
  created_at INTEGER NOT NULL,
  PRIMARY KEY (dataset_id, version)
);

CREATE TABLE dataset_items (
  revision_id TEXT PRIMARY KEY,
  item_id TEXT NOT NULL,              -- stable across revisions
  dataset_id TEXT NOT NULL REFERENCES datasets(id) ON DELETE CASCADE,
  input TEXT NOT NULL,                -- JSON
  expected_output TEXT,               -- JSON, NULL when absent
  metadata TEXT NOT NULL DEFAULT '{}',
  source_trace_id BLOB,               -- 16 bytes
  source_span_id BLOB,                -- 8 bytes
  valid_from_version INTEGER NOT NULL,
  valid_to_version INTEGER,
  created_at INTEGER NOT NULL
);
CREATE INDEX dataset_items_live ON dataset_items(dataset_id, valid_to_version, item_id);
CREATE INDEX dataset_items_item ON dataset_items(dataset_id, item_id, valid_from_version);
CREATE INDEX dataset_items_source ON dataset_items(source_trace_id) WHERE source_trace_id IS NOT NULL;
CREATE UNIQUE INDEX dataset_items_one_live ON dataset_items(dataset_id, item_id) WHERE valid_to_version IS NULL;

CREATE TABLE experiments (
  id TEXT PRIMARY KEY,
  project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  dataset_id TEXT REFERENCES datasets(id) ON DELETE SET NULL,
  dataset_name TEXT NOT NULL,         -- copied at create, shown after the dataset is deleted
  dataset_version INTEGER NOT NULL,
  name TEXT NOT NULL,
  description TEXT,
  metadata TEXT NOT NULL DEFAULT '{}',
  repetitions INTEGER NOT NULL DEFAULT 1,
  item_count INTEGER NOT NULL,        -- live items of the pinned version
  status TEXT NOT NULL CHECK (status IN ('running', 'completed', 'failed')),
  error TEXT,
  created_by TEXT,
  created_at INTEGER NOT NULL,
  finished_at INTEGER
);
CREATE INDEX experiments_project ON experiments(project_id, created_at);
CREATE INDEX experiments_dataset ON experiments(dataset_id, created_at);

CREATE TABLE experiment_items (
  experiment_id TEXT NOT NULL REFERENCES experiments(id) ON DELETE CASCADE,
  dataset_item_id TEXT NOT NULL,      -- item_id (stable)
  repetition INTEGER NOT NULL,
  revision_id TEXT NOT NULL,          -- the pinned revision; no FK (D-J-9 deletes by it)
  input TEXT NOT NULL,                -- snapshot of the revision
  expected_output TEXT,
  trace_id BLOB,
  output TEXT,                        -- JSON
  error TEXT,
  latency_ns INTEGER,
  created_at INTEGER NOT NULL,
  PRIMARY KEY (experiment_id, dataset_item_id, repetition)
);
CREATE INDEX experiment_items_trace ON experiment_items(trace_id) WHERE trace_id IS NOT NULL;
CREATE INDEX experiment_items_revision ON experiment_items(revision_id);
