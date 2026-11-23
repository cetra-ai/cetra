-- Track SA M2 (D-SA-17): an org's one SSO provider is OIDC or SAML. Pre-release rebuild:
-- SQLite cannot drop NOT NULL. No table references org_sso_providers.
CREATE TABLE org_sso_providers_new (
    id TEXT PRIMARY KEY,
    org_id TEXT NOT NULL UNIQUE REFERENCES orgs(id) ON DELETE CASCADE,
    protocol TEXT NOT NULL DEFAULT 'oidc',
    display_name TEXT NOT NULL,
    issuer TEXT,
    client_id TEXT,
    client_secret_enc BLOB,
    enabled INTEGER NOT NULL DEFAULT 0,
    enforced INTEGER NOT NULL DEFAULT 0,
    jit_role TEXT NOT NULL DEFAULT 'viewer',
    last_login_at INTEGER,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    CHECK ((protocol = 'oidc' AND issuer IS NOT NULL AND client_id IS NOT NULL AND client_secret_enc IS NOT NULL)
        OR (protocol = 'saml' AND issuer IS NULL AND client_id IS NULL AND client_secret_enc IS NULL))
);
INSERT INTO org_sso_providers_new
    (id, org_id, protocol, display_name, issuer, client_id, client_secret_enc, enabled, enforced,
     jit_role, last_login_at, created_at, updated_at)
SELECT id, org_id, 'oidc', display_name, issuer, client_id, client_secret_enc, enabled, enforced,
       jit_role, last_login_at, created_at, updated_at
  FROM org_sso_providers;
DROP TABLE org_sso_providers;
ALTER TABLE org_sso_providers_new RENAME TO org_sso_providers;

CREATE TABLE org_sso_saml (
    provider_id TEXT PRIMARY KEY REFERENCES org_sso_providers(id) ON DELETE CASCADE,
    metadata_url TEXT,
    idp_entity_id TEXT NOT NULL,
    sso_url TEXT NOT NULL,
    email_attribute TEXT,
    name_attribute TEXT,
    first_name_attribute TEXT,
    last_name_attribute TEXT,
    refreshed_at INTEGER,
    refresh_checked_at INTEGER,
    refresh_error TEXT,
    certificates_changed_at INTEGER
);

CREATE TABLE org_sso_saml_certificates (
    provider_id TEXT NOT NULL REFERENCES org_sso_saml(provider_id) ON DELETE CASCADE,
    position INTEGER NOT NULL,
    der BLOB NOT NULL,
    PRIMARY KEY (provider_id, position)
);
