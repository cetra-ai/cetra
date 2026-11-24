-- Audit records (track EA, D-EA-1 to D-EA-9, D-EA-20). No foreign keys: records
-- outlive the users, orgs and projects they name. No CHECK: actions, kinds and
-- outcomes are append-only names, and a CHECK would make each new name a format
-- change (D-R-34).
CREATE TABLE audit_events (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    id TEXT NOT NULL UNIQUE,
    at INTEGER NOT NULL,
    org_id TEXT,
    project_id TEXT,
    action TEXT NOT NULL,
    outcome TEXT NOT NULL,
    actor_kind TEXT NOT NULL,
    actor_id TEXT,
    via TEXT,
    credential_id TEXT,
    target_kind TEXT,
    target_id TEXT,
    details TEXT NOT NULL DEFAULT '{}',
    ip TEXT,
    user_agent TEXT
);
CREATE INDEX audit_events_org ON audit_events (org_id, seq);
CREATE INDEX audit_events_org_action ON audit_events (org_id, action, seq);
CREATE INDEX audit_events_org_actor ON audit_events (org_id, actor_id, seq);
CREATE INDEX audit_events_org_target ON audit_events (org_id, target_id, seq);
CREATE INDEX audit_events_actor ON audit_events (actor_id);
CREATE INDEX audit_events_at ON audit_events (at);
