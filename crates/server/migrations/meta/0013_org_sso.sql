-- Track P M5: per-org OIDC providers and email domains (D-P-52, D-P-54).
CREATE TABLE org_sso_providers (
    id TEXT PRIMARY KEY,                -- UUIDv7; the row id in client_secret_enc's associated data
    org_id TEXT NOT NULL UNIQUE REFERENCES orgs(id) ON DELETE CASCADE,
    display_name TEXT NOT NULL,
    issuer TEXT NOT NULL,
    client_id TEXT NOT NULL,
    client_secret_enc BLOB NOT NULL,    -- sealed (D-I-2)
    enabled INTEGER NOT NULL DEFAULT 0,
    enforced INTEGER NOT NULL DEFAULT 0,
    jit_role TEXT NOT NULL DEFAULT 'viewer',   -- 'viewer' | 'member'
    last_login_at INTEGER,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);

CREATE TABLE org_sso_domains (
    id TEXT PRIMARY KEY,
    org_id TEXT NOT NULL REFERENCES orgs(id) ON DELETE CASCADE,
    domain TEXT NOT NULL,               -- lowercase, no trailing dot
    token TEXT NOT NULL,                -- 32 lowercase hex; the TXT value's random part (public)
    verified_at INTEGER,
    last_checked_at INTEGER,
    created_at INTEGER NOT NULL,
    UNIQUE (org_id, domain)
);

CREATE UNIQUE INDEX org_sso_domains_verified ON org_sso_domains(domain) WHERE verified_at IS NOT NULL;

-- The org whose provider started the session (D-P-59).
ALTER TABLE web_sessions ADD COLUMN sso_org_id TEXT REFERENCES orgs(id) ON DELETE SET NULL;
