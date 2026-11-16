-- Auth review #9 (D-P-66): how the address was verified. NULL on a verified row is an
-- address of unknown origin and counts as unproven for D148's password clearing.
ALTER TABLE users ADD COLUMN email_verified_via TEXT
    CHECK (email_verified_via IS NULL
        OR email_verified_via IN ('mail_token', 'idp', 'invite', 'control_plane', 'operator'));

-- Auth review #12 (D-P-69): whether an identity provider started the session, so that
-- `auth.enforce_sso_domains` can refuse the password sessions that predate it.
ALTER TABLE web_sessions ADD COLUMN via_provider INTEGER NOT NULL DEFAULT 0
    CHECK (via_provider IN (0, 1));
