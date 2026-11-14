-- Track P M2 (D-P-11, D-P-30). orgs.limits was never read; org tokens replace it (D179).
ALTER TABLE orgs DROP COLUMN limits;

-- The newest accepted org token, as received (D-P-11).
CREATE TABLE org_entitlements (
    org_id TEXT PRIMARY KEY REFERENCES orgs(id) ON DELETE CASCADE,
    token TEXT NOT NULL,
    token_id TEXT NOT NULL,
    issued_at INTEGER NOT NULL,
    received_at INTEGER NOT NULL
);

-- An org's data window that stopped applying, and when (D-P-30). NULL days = unlimited.
CREATE TABLE org_window_ends (
    org_id TEXT NOT NULL REFERENCES orgs(id) ON DELETE CASCADE,
    ended_at INTEGER NOT NULL,
    data_access_days INTEGER,
    PRIMARY KEY (org_id, ended_at)
);
