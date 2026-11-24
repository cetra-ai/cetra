-- Audit stream destinations (track EA M4, D-EA-11). One per org (`org_id`) and one for the
-- instance (`org_id` null). `config_enc` seals the StreamConfig (URL, signing secret, optional
-- header) with the row id as associated data (D-I-2). `cursor_seq` is the last `audit_events.seq`
-- the destination has accepted (D-EA-20); `lost_before_seq` is set when retention deleted records
-- above the cursor (plan P-5).
CREATE TABLE audit_stream_destinations (
    id TEXT PRIMARY KEY,
    org_id TEXT REFERENCES orgs(id) ON DELETE CASCADE,
    url_host TEXT NOT NULL,
    config_enc BLOB NOT NULL,
    enabled INTEGER NOT NULL DEFAULT 1,
    cursor_seq INTEGER NOT NULL DEFAULT 0,
    attempts INTEGER NOT NULL DEFAULT 0,
    next_attempt_at INTEGER NOT NULL DEFAULT 0,
    last_success_at INTEGER,
    last_error TEXT,
    failing_since INTEGER,
    alerted_at INTEGER,
    lost_before_seq INTEGER,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE UNIQUE INDEX audit_stream_destinations_scope ON audit_stream_destinations (coalesce(org_id, ''));
