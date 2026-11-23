-- SCIM provisioning (track EA M5, D-EA-12 to D-EA-19). No CHECK on text columns (D-R-34).
CREATE TABLE org_scim (
    org_id TEXT PRIMARY KEY REFERENCES orgs(id) ON DELETE CASCADE,
    enabled INTEGER NOT NULL DEFAULT 0,
    default_role TEXT NOT NULL DEFAULT 'viewer',
    updated_at INTEGER NOT NULL
);
CREATE TABLE scim_tokens (
    id TEXT PRIMARY KEY,
    org_id TEXT NOT NULL REFERENCES orgs(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    prefix TEXT NOT NULL,
    token_hash TEXT NOT NULL UNIQUE,
    created_by TEXT REFERENCES users(id) ON DELETE SET NULL,
    created_at INTEGER NOT NULL,
    last_used_at INTEGER,
    revoked_at INTEGER
);
CREATE INDEX scim_tokens_org ON scim_tokens (org_id);
CREATE TABLE scim_users (
    id TEXT PRIMARY KEY,
    org_id TEXT NOT NULL REFERENCES orgs(id) ON DELETE CASCADE,
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    user_name TEXT NOT NULL,
    external_id TEXT,
    given_name TEXT,
    family_name TEXT,
    display_name TEXT,
    department TEXT,
    employee_number TEXT,
    active INTEGER NOT NULL DEFAULT 1,
    disabled_account INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE UNIQUE INDEX scim_users_org_user_name ON scim_users (org_id, user_name);
CREATE UNIQUE INDEX scim_users_org_external_id ON scim_users (org_id, external_id) WHERE external_id IS NOT NULL;
CREATE UNIQUE INDEX scim_users_org_user ON scim_users (org_id, user_id);
