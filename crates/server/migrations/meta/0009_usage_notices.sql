-- Track P M3 (D-P-18, D-P-38). One row per org, month and threshold, so each notice is sent once.
CREATE TABLE usage_notices (
    org_id TEXT NOT NULL REFERENCES orgs(id) ON DELETE CASCADE,
    month TEXT NOT NULL,
    threshold INTEGER NOT NULL,
    sent_at INTEGER NOT NULL,
    PRIMARY KEY (org_id, month, threshold)
);
