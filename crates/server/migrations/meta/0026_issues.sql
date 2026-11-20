-- Track IC: issue sources, issues and their events (D-IC-1, D-IC-10).
CREATE TABLE issue_sources (
    id            TEXT PRIMARY KEY,
    project_id    TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    name          TEXT NOT NULL,
    kind          TEXT NOT NULL,
    filter        TEXT,
    enabled       INTEGER NOT NULL DEFAULT 1,
    paused_reason TEXT,
    paused_at     INTEGER,
    created_by    TEXT REFERENCES users(id) ON DELETE SET NULL,
    created_at    INTEGER NOT NULL,
    updated_at    INTEGER NOT NULL,
    UNIQUE (project_id, name)
);

CREATE TABLE issues (
    id          TEXT PRIMARY KEY,
    project_id  TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    source_id   TEXT NOT NULL REFERENCES issue_sources(id) ON DELETE CASCADE,
    fingerprint TEXT,
    title       TEXT NOT NULL,
    description TEXT,
    state       TEXT NOT NULL CHECK (state IN ('open', 'resolved', 'ignored', 'merged')),
    regressed   INTEGER NOT NULL DEFAULT 0,
    merged_into TEXT REFERENCES issues(id) ON DELETE SET NULL,
    first_seen  INTEGER NOT NULL,
    last_seen   INTEGER NOT NULL,
    resolved_at INTEGER,
    resolved_by TEXT REFERENCES users(id) ON DELETE SET NULL,
    created_at  INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL
);
CREATE UNIQUE INDEX issues_source_fingerprint ON issues (source_id, fingerprint)
    WHERE fingerprint IS NOT NULL;
CREATE INDEX issues_project_last_seen ON issues (project_id, last_seen DESC, id);
CREATE INDEX issues_merged_into ON issues (merged_into) WHERE merged_into IS NOT NULL;

CREATE TABLE issue_events (
    id            TEXT PRIMARY KEY,
    issue_id      TEXT NOT NULL REFERENCES issues(id) ON DELETE CASCADE,
    kind          TEXT NOT NULL CHECK (kind IN ('created', 'state_changed', 'renamed', 'merged', 'regressed')),
    actor_user_id TEXT REFERENCES users(id) ON DELETE SET NULL,
    from_value    TEXT,
    to_value      TEXT,
    at            INTEGER NOT NULL
);
CREATE INDEX issue_events_issue ON issue_events (issue_id, at, id);
