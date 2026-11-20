-- Annotation queues (track AQ, D-AQ-2–D-AQ-9).
CREATE TABLE annotation_queues (
  id TEXT PRIMARY KEY,                -- UUIDv7
  project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  name TEXT NOT NULL,
  description TEXT,
  instructions TEXT,                  -- Markdown
  target TEXT NOT NULL CHECK (target IN ('trace', 'span', 'session')),
  version INTEGER NOT NULL DEFAULT 1,
  created_by TEXT REFERENCES users(id) ON DELETE SET NULL,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  UNIQUE (project_id, name)
);

CREATE TABLE annotation_queue_definitions (
  queue_id TEXT NOT NULL REFERENCES annotation_queues(id) ON DELETE CASCADE,
  definition_id TEXT NOT NULL REFERENCES score_definitions(id) ON DELETE CASCADE,
  position INTEGER NOT NULL,
  PRIMARY KEY (queue_id, definition_id)
);

CREATE TABLE annotation_queue_items (
  id TEXT PRIMARY KEY,                -- UUIDv7
  queue_id TEXT NOT NULL REFERENCES annotation_queues(id) ON DELETE CASCADE,
  project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  trace_id TEXT,                      -- lowercase hex; trace and span items
  span_id TEXT,                       -- lowercase hex; span items
  session_id TEXT,                    -- session items
  target_time INTEGER NOT NULL,       -- ns: trace start, or the session's earliest visible trace start
  status TEXT NOT NULL DEFAULT 'pending' CHECK (status IN ('pending', 'completed', 'skipped')),
  skip_reason TEXT,
  leased_by_user_id TEXT REFERENCES users(id) ON DELETE SET NULL,
  lease_expires_at INTEGER,
  done_by_user_id TEXT REFERENCES users(id) ON DELETE SET NULL,
  done_at INTEGER,
  added_by_kind TEXT NOT NULL CHECK (added_by_kind IN ('user', 'pat', 'key')),
  added_by_id TEXT NOT NULL,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL
);
CREATE UNIQUE INDEX annotation_queue_items_target ON annotation_queue_items(
  queue_id, coalesce(trace_id, ''), coalesce(span_id, ''), coalesce(session_id, '')
);
CREATE INDEX annotation_queue_items_claim ON annotation_queue_items(queue_id, status, id);
CREATE INDEX annotation_queue_items_trace ON annotation_queue_items(project_id, trace_id) WHERE trace_id IS NOT NULL;
CREATE INDEX annotation_queue_items_session ON annotation_queue_items(project_id, session_id) WHERE session_id IS NOT NULL;
CREATE INDEX annotation_queue_items_time ON annotation_queue_items(project_id, target_time);
