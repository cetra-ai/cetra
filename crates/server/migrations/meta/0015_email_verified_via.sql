-- Auth review #9 (D-P-60): how the address was verified. NULL on a verified row is an
-- address of unknown origin and counts as unproven for D148's password clearing.
ALTER TABLE users ADD COLUMN email_verified_via TEXT
    CHECK (email_verified_via IS NULL
        OR email_verified_via IN ('mail_token', 'idp', 'invite', 'control_plane', 'operator'));
