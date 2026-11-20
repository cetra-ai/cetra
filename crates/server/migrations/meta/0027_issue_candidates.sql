-- Track IC M3: LLM issue sources, candidates, spend and refits (D-IC-1, D-IC-6, D-IC-12, D-IC-19).
ALTER TABLE issue_sources ADD COLUMN sample_rate REAL;
ALTER TABLE issue_sources ADD COLUMN daily_cap INTEGER;
ALTER TABLE issue_sources ADD COLUMN score_names TEXT;
ALTER TABLE issue_sources ADD COLUMN feedback_sources TEXT;
ALTER TABLE issue_sources ADD COLUMN chat_connection_id TEXT REFERENCES llm_connections(id);
ALTER TABLE issue_sources ADD COLUMN chat_model TEXT;
ALTER TABLE issue_sources ADD COLUMN embed_connection_id TEXT REFERENCES llm_connections(id);
ALTER TABLE issue_sources ADD COLUMN embed_model TEXT;
ALTER TABLE issue_sources ADD COLUMN embed_dimension INTEGER;
ALTER TABLE issue_sources ADD COLUMN floor REAL;
ALTER TABLE issue_sources ADD COLUMN min_issue_size INTEGER;
ALTER TABLE issue_sources ADD COLUMN budget_usd REAL;
ALTER TABLE issue_sources ADD COLUMN budget_period TEXT;
ALTER TABLE issue_sources ADD COLUMN paused_detail TEXT;
ALTER TABLE issue_sources ADD COLUMN next_refit_at INTEGER;

ALTER TABLE issues ADD COLUMN centroid BLOB;
ALTER TABLE issues ADD COLUMN member_count INTEGER NOT NULL DEFAULT 0;

CREATE TABLE issue_candidates (
    id                     TEXT PRIMARY KEY,
    project_id             TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    source_id              TEXT NOT NULL REFERENCES issue_sources(id) ON DELETE CASCADE,
    target                 TEXT NOT NULL CHECK (target IN ('trace', 'span', 'session')),
    trace_id               BLOB,
    span_id                BLOB,
    session_id             TEXT,
    user_id                TEXT,
    trace_start            INTEGER,
    score_id               TEXT,
    dedup_key              TEXT NOT NULL UNIQUE,
    status                 TEXT NOT NULL CHECK (status IN ('queued', 'running', 'pending', 'assigned', 'outlier', 'dropped', 'failed', 'skipped')),
    attempts               INTEGER NOT NULL DEFAULT 0,
    claims                 INTEGER NOT NULL DEFAULT 0,
    interruptions          INTEGER NOT NULL DEFAULT 0,
    load_retries           INTEGER NOT NULL DEFAULT 0,
    not_before             INTEGER NOT NULL,
    error_class            TEXT,
    issue_id               TEXT REFERENCES issues(id) ON DELETE SET NULL,
    summary                TEXT,
    summary_prompt_version INTEGER,
    vector                 BLOB,
    embed_model            TEXT,
    dimension              INTEGER,
    call_trace_id          BLOB,
    cost_usd               REAL NOT NULL DEFAULT 0,
    created_at             INTEGER NOT NULL,
    updated_at             INTEGER NOT NULL
);
CREATE INDEX issue_candidates_ready ON issue_candidates (not_before) WHERE status = 'queued';
CREATE INDEX issue_candidates_source ON issue_candidates (source_id, status, created_at);
CREATE INDEX issue_candidates_trace ON issue_candidates (project_id, trace_id);

CREATE TABLE issue_source_days (
    source_id  TEXT NOT NULL REFERENCES issue_sources(id) ON DELETE CASCADE,
    day        INTEGER NOT NULL,
    candidates INTEGER NOT NULL DEFAULT 0,
    spent_usd  REAL NOT NULL DEFAULT 0,
    PRIMARY KEY (source_id, day)
);

CREATE TABLE issue_refits (
    id          TEXT PRIMARY KEY,
    source_id   TEXT NOT NULL REFERENCES issue_sources(id) ON DELETE CASCADE,
    trigger     TEXT NOT NULL CHECK (trigger IN ('schedule', 'manual')),
    started_at  INTEGER NOT NULL,
    finished_at INTEGER,
    pending     INTEGER NOT NULL DEFAULT 0,
    clusters    INTEGER NOT NULL DEFAULT 0,
    created     INTEGER NOT NULL DEFAULT 0,
    joined      INTEGER NOT NULL DEFAULT 0,
    outliers    INTEGER NOT NULL DEFAULT 0,
    cost_usd    REAL NOT NULL DEFAULT 0,
    error_class TEXT
);
CREATE INDEX issue_refits_source ON issue_refits (source_id, started_at DESC);
