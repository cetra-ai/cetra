# SA · M2 — SAML providers on the server

Milestone 2 of track SA ([`README.md`](README.md)). It makes an org's one SSO provider either OIDC or SAML on the server, using `cetra-saml` (M1, [`01-m1-crate.md`](01-m1-crate.md)). The web UI and docs are M3.

When M2 is done:
- `org_sso_providers` holds one provider row per org with a `protocol`, and the SAML configuration lives in two new tables (D-SA-17);
- the feature `saml` exists and gates SAML setup (D-SA-4);
- `PUT /api/v1/orgs/{org}/sso/saml` saves a SAML provider from a metadata URL, pasted metadata or manual fields, and `PUT …/sso/provider` switches back to OIDC (D-SA-18);
- the login works end to end:
  - `GET /api/auth/sso/authorize?domain=` sends a SAML org's users to their IdP with an AuthnRequest;
  - `POST /api/auth/saml/{org_id}/acs` validates the Response and signs the user in through the same core as OIDC;
  - an unsolicited Response restarts an SP-initiated login (D-SA-5, D-SA-10, D-SA-12);
- `GET /api/auth/saml/{org_id}` serves the SP metadata, and `GET …/start` starts a login, which is also the IdP tile URL;
- metadata from a URL is refreshed daily (D-SA-11);
- break-glass, enforcement, the test sign-in, JIT and lapses work for SAML exactly as for OIDC;
- specs 04, 08, 09, 11 and 12 describe it, and `openapi.json` and the API examples include it.

## Prerequisites

- M1 is implemented and its checks pass (`crates/saml` exists with the API of `01-m1-crate.md`).
- Work continues on the track branch `saml-design`, in its worktree.
- **Migration number.** This plan uses `0031`. If `main` gains a `0031_*.sql` before this branch merges, the merge renames this one to the next free number and updates `PRE_RELEASE_NON_ADDITIVE` and the altering case to match.
- **Release check.** `fixtures/storage/golden/RELEASES` must still be empty, meaning no public release yet. If it is not, stop: D-SA-17's rebuild must become a format change, and the plan changes.

## Read first

1. `AGENTS.md` (rules, privacy, tests) and the conventions of this plan below.
2. [`README.md`](README.md): § Architecture, § Contracts, decisions D-SA-2 to D-SA-19.
3. `docs/plans/p-entitlements/05-m5-org-sso.md` § Decisions (D-P-51 to D-P-65): the OIDC org SSO this milestone extends.
4. The code facts gathered for this plan: `/home/sebastien/.config/Claude-extra/side-session-notes/local_1c672712-2cc7-40e9-8d6b-d582417356b7/sa-m2-code-facts.md` (paths, line numbers, function signatures; read it whole).
5. The code each phase names, before changing it. The main files are:
   - `crates/server/src/ee/org_sso/{mod,store,login,settings}.rs`;
   - `crates/server/src/auth/oidc/{flow,state_cookie}.rs`;
   - `crates/server/src/routes/oauth.rs`, `crates/server/src/auth/session.rs`, `crates/server/src/http/csrf.rs`, `crates/server/src/auth/limits.rs`;
   - `crates/server/src/api/{org_sso,entitlements,openapi}.rs`, `crates/server/src/test_support/mod.rs`, `crates/server/src/meta/migration_tests.rs`.
6. `crates/saml/src/lib.rs` and the modules it re-exports: the crate's public API.

## What it is, and what it is not

**It is:** server code under `crates/server/src/ee/org_sso/` (Elastic-2.0), its storage, its routes, its tests, the specs and the API contract.

**It is not:**
- the SAML settings form, the login error messages in the web UI, or the docs pages (M3);
- the security review and interop (M4);
- the cloud (track Q).

The only web edits here are the ones the regenerated API types force so that `just check` stays green:
- `feature-label.ts` and `plan.tsx` list the new feature;
- `provider-section.tsx` handles the now-optional `issuer` and `client_id`.

## Decisions this plan relies on

| ID | Summary |
|---|---|
| D-SA-2 | One provider per org; switching protocol deletes identities and resets the flags; refused while enforced. |
| D-SA-4 | Feature `saml`; SAML setup, changes and enabling need `saml` and `org_sso`; lapses follow D-P-61. |
| D-SA-5 | SP-initiated only; a Response without `InResponseTo` restarts through `GET /api/auth/saml/{org_id}/start`; loop guard; unknown IDs are `saml_invalid`. |
| D-SA-6 | Email and name attributes; JIT as OIDC. |
| D-SA-9 | Validation in `cetra-saml` (`validate_response`). |
| D-SA-10 | In-memory pending table with bounds, binder cookie, consume after the binder matches, ACS per-IP bucket, fixed RelayState. |
| D-SA-11 | Metadata from a URL (daily refresh, never in restore mode, entity ID never changes automatically), XML or manual fields. |
| D-SA-12 | SP entity ID and metadata at `/api/auth/saml/{org_id}`, ACS at `…/acs`; Origin exemption on the exact ACS path; form body capped at 256 KiB; new login codes `saml_invalid`, `saml_unsupported`. |
| D-SA-13 | Identity `org:<org_id>`, subject = NameID text. |
| D-SA-17 | One provider row with `protocol`; `org_sso_saml` and `org_sso_saml_certificates`; migration rebuild as a pre-release exception. |
| D-SA-18 | `PUT …/sso/saml`; `PUT …/sso/provider` stays OIDC; response shape. |
| D-SA-19 | `https` public URL required; metadata URL follows `issuer_problem` and the egress policy. |
| D-P-51–D-P-65 | Org SSO as built (owner-only settings, domains, enforcement, discovery, break-glass, test sign-in, lapses). |

## Versions

No new external dependency. `crates/server/Cargo.toml` gains the workspace crate `cetra-saml` in `[dependencies]`, and in `[dev-dependencies]` with the feature `test-idp`.

## Phases and dependencies

| Phase | Edits | Depends on | Parallel with |
|---|---|---|---|
| SA2.1 Storage | `crates/server/migrations/meta/0031_org_sso_saml.sql`, `crates/server/src/meta/migration_tests.rs`, `crates/server/src/ee/org_sso/store.rs`, `crates/server/src/api/org_sso.rs` (only `SsoProtocol`), `crates/server/Cargo.toml` | M1 | SA2.2 |
| SA2.2 Feature `saml` | `crates/server/src/api/entitlements.rs`, `crates/server/src/entitlements/mod.rs` (tests only), `crates/server/src/config.rs` (test only), `web/src/components/plan/feature-label.ts`, `web/src/routes/$org/settings/plan.tsx` | M1 | SA2.1 |
| SA2.3 Index, pending table, sign-in core | `crates/server/src/ee/org_sso/{mod.rs,login.rs,saml_pending.rs}`, `crates/server/src/test_support/mod.rs`, `crates/server/src/serve.rs` (`OrgSso::new` call only, if its signature changes) | SA2.1, SA2.2 | — |
| SA2.4 Settings API | `crates/server/src/api/org_sso.rs`, `crates/server/src/ee/org_sso/settings.rs`, `fixtures/api/static/org-sso.json`, `web/src/ee/org-sso/provider-section.tsx` | SA2.3 | SA2.5 |
| SA2.5 SAML login routes | `crates/server/src/ee/org_sso/{saml.rs,mod.rs,login.rs}`, `crates/server/src/auth/oidc/flow.rs`, `crates/server/src/http/csrf.rs`, `crates/server/src/http/access.rs`, `crates/server/src/auth/limits.rs` | SA2.3 | SA2.4 |
| SA2.6 Daily metadata refresh | `crates/server/src/ee/org_sso/{saml_refresh.rs,mod.rs}`, `crates/server/src/serve.rs` | SA2.4, SA2.5 | SA2.7, SA2.8 |
| SA2.7 OpenAPI, examples, golden, break-glass test | `crates/server/src/api/openapi.rs`, `crates/server/tests/openapi.rs`, `crates/server/src/golden_tests/{build.rs,dump.rs}`, `crates/server/src/admin.rs` (test only), `crates/server/src/http/access_suite.rs` (only if a SAML route needs an entry) | SA2.4, SA2.5 | SA2.6, SA2.8 |
| SA2.8 Specs | `docs/spec/04-metadata-db.md`, `08-auth.md`, `09-api.md`, `11-ui.md`, `12-operations.md`, `docs/spec/README.md` | SA2.4, SA2.5 | SA2.6, SA2.7 |
| SA2.9 Checks and review (coordinating session) | generated files, snapshot, fixes | all | — |

Phases marked parallel run at the same time in the same worktree. Their Edit lists do not overlap. Each one stages only its own files by path.

## Conventions for every phase

- **No checks while implementing (the user, 2026-11-20).** Do not run `cargo build`, `cargo test`, `cargo clippy`, `just check`, `pnpm`, vitest or any build. Write the code and tests, commit with the phase's subject, and stop. SA2.9 runs the checks once. `rustfmt --edition 2024 <file>` on your own files is allowed.
- Stage only your phase's files, by path (`git add <paths>`, never `git add -A`). If `git commit` fails on `index.lock`, wait a few seconds and retry.
- Every file under `crates/server/src/ee/` starts with exactly `// SPDX-License-Identifier: Elastic-2.0`. `crates/server/src/api/org_sso.rs`, the migration and `test_support` are **not** `ee` and must not carry it (`just check-ee`).
- Server errors are `ApiError`; login failures are `LoginError` redirects. No `unwrap()` / `expect()` outside tests without an invariant comment. Time comes from `state.clock.now_ns()`.
- **Privacy** (`AGENTS.md`). Logs, problem details and `Debug` output never contain an email, a NameID, an attribute value, a SAML message, a metadata document, a metadata URL, an IdP entity ID or SSO URL, a certificate, a binder or a request ID. Logs carry org IDs, user IDs, counts and error *class names* (a `ResponseError` or `MetadataError` variant via `{:?}`, which prints only the unit variant's name).
- Tests use the existing harness (`org_sso_app_with`, `TestApp`, `capture_logs`, `assert_logs_clean`) and `cetra_saml::test_idp`. The test public URL must be https for SAML: `config.public_url = serde_json::from_str("\"https://cetra.example\"").unwrap();` (precedent `crates/server/src/auth/session.rs:346`).
- Names used below, fixed for the whole plan:
  - binder cookie: `SAML_COOKIE = cetra_core::cetra_name!("_saml")`;
  - loop-guard cookie: `SAML_RESTART_COOKIE = cetra_core::cetra_name!("_saml_restart")`;
  - fixed RelayState: `RELAY_STATE = "cetra"`;
  - SP paths for org `o`, from `state.config.public_url.join(..)`: entity ID `/api/auth/saml/{o}`, ACS `/api/auth/saml/{o}/acs`, start `/api/auth/saml/{o}/start`. `{o}` is the org's UUID in lowercase hyphenated form (`OrgId`'s `Display`).

---

## SA2.1 — Storage

**Goal:** the provider row gains a protocol, and the SAML configuration has its tables and store functions (D-SA-17).

**Edit:** `crates/server/migrations/meta/0031_org_sso_saml.sql` (new), `crates/server/src/meta/migration_tests.rs`, `crates/server/src/ee/org_sso/store.rs`, `crates/server/src/api/org_sso.rs`, `crates/server/Cargo.toml`.

**Steps:**

1. `crates/server/Cargo.toml`: under `[dependencies]`, add `cetra-saml.workspace = true` (alphabetical among the `cetra-*` lines). Under `[dev-dependencies]`, add `cetra-saml = { workspace = true, features = ["test-idp"] }`.
2. `0031_org_sso_saml.sql`, exactly:
   ```sql
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
   ```
   No `;` or `--` may appear inside a string literal (`statements()` splits naively); none does.
3. `migration_tests.rs`:
   - Append `31, // 0031 rebuilds org_sso_providers for SAML (D-SA-17)` to `PRE_RELEASE_NON_ADDITIVE`.
   - Append an `AlteringCase` with `version: 31`:
     - `seed`: one `orgs` row (`'018f6b4e-7c1a-7c1a-8c1a-0000000000a7'`, `'Acme'`, `'acme'`, `'active'`, 1, 2), then one `org_sso_providers` row in the 0013 shape: `(id, org_id, display_name, issuer, client_id, client_secret_enc, enabled, enforced, jit_role, last_login_at, created_at, updated_at)` = `('018f6b4e-7c1a-7c1a-8c1a-0000000000a8', '018f6b4e-7c1a-7c1a-8c1a-0000000000a7', 'Okta', 'https://idp.test', 'client', X'00', 1, 0, 'member', 5, 3, 4)`.
     - `read`: `"SELECT id || '|' || org_id || '|' || protocol || '|' || display_name || '|' || issuer || '|' || client_id || '|' || hex(client_secret_enc) || '|' || enabled || '|' || enforced || '|' || jit_role || '|' || last_login_at || '|' || created_at || '|' || updated_at FROM org_sso_providers"`.
     - `expected`: `"018f6b4e-7c1a-7c1a-8c1a-0000000000a8|018f6b4e-7c1a-7c1a-8c1a-0000000000a7|oidc|Okta|https://idp.test|client|00|1|0|member|5|3|4"`.
4. `api/org_sso.rs`: add, next to `JitRole` and in its style (derives `Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema`, `#[serde(rename_all = "snake_case")]`):
   ```rust
   /// The protocol of an org's provider (D-SA-2, D-SA-17).
   pub enum SsoProtocol { Oidc, Saml }
   impl SsoProtocol {
       pub fn as_str(self) -> &'static str;               // "oidc" | "saml"
       pub fn from_str_opt(value: &str) -> Option<Self>;  // inverse of as_str
   }
   ```
5. `store.rs`, provider rows:
   - Replace the three OIDC fields of `ProviderRow` with `pub(crate) protocol: SsoProtocol` and `pub(crate) oidc: Option<OidcColumns>`:
     ```rust
     /// The OIDC columns of a provider row; `None` for a SAML row.
     pub(crate) struct OidcColumns { pub(crate) issuer: String, pub(crate) client_id: String, pub(crate) client_secret_enc: Vec<u8> }
     ```
     `ProviderRow`'s `Debug` prints `protocol`, and for OIDC `issuer` and `client_id` (as today); `OidcColumns` gets a manual `Debug` without the secret.
   - Every `SELECT` of provider columns (the `PROVIDER_BY_ORG` constant, `active_providers`) adds `p.protocol` (or `protocol`). `ProviderRecord` gets `protocol: String` and `Option<…>` for the three OIDC columns.
   - `provider_row` maps `protocol` with `SsoProtocol::from_str_opt`; unknown → `corrupt("org_sso_providers", "protocol")`. For `Oidc` all three columns must be present (else `corrupt(.., "issuer")`) → `Some(OidcColumns)`. For `Saml` → `None`.
6. `store.rs`, `put_provider` (the OIDC save) keeps its signature. Its body becomes:
   1. `let existing = provider_tx(&mut *conn, org).await?;`
   2. `Some(row)` with `row.protocol == SsoProtocol::Saml` (switch to OIDC):
      - `write.client_secret_enc` must be `Some`, else `corrupt("org_sso_providers", "client_secret_enc")`;
      - `DELETE FROM org_sso_saml WHERE provider_id = ?` (row id; certificates cascade);
      - `UPDATE org_sso_providers SET protocol = 'oidc', display_name = ?, issuer = ?, client_id = ?, client_secret_enc = ?, jit_role = ?, enabled = 0, enforced = 0, last_login_at = NULL, updated_at = ? WHERE org_id = ?`.
   3. `Some(_)` (OIDC): the existing `UPDATE` unchanged.
   4. `None`: the existing `INSERT`, with `protocol` written as `'oidc'` explicitly.
7. `store.rs`, SAML types:
   ```rust
   /// Attribute names an owner set; `None` uses the crate's defaults for that field (D-SA-6).
   #[derive(Debug, Clone, Default, PartialEq, Eq)]
   pub(crate) struct SamlAttributes {
       pub(crate) email: Option<String>,
       pub(crate) display_name: Option<String>,
       pub(crate) first_name: Option<String>,
       pub(crate) last_name: Option<String>,
   }

   /// One `org_sso_saml` row with its certificates in `position` order. `Debug` prints lengths
   /// and counts only (D-SA privacy).
   #[derive(Clone, PartialEq, Eq)]
   pub(crate) struct SamlRow {
       pub(crate) provider_id: Uuid,
       pub(crate) metadata_url: Option<String>,
       pub(crate) idp_entity_id: String,
       pub(crate) sso_url: String,
       pub(crate) certificates: Vec<Vec<u8>>,
       pub(crate) attributes: SamlAttributes,
       pub(crate) refreshed_at: Option<i64>,
       pub(crate) refresh_checked_at: Option<i64>,
       pub(crate) refresh_error: Option<String>,
       pub(crate) certificates_changed_at: Option<i64>,
   }

   /// What `put_saml_provider` writes.
   pub(crate) struct SamlWrite {
       pub(crate) display_name: String,
       pub(crate) jit_role: JitRole,
       pub(crate) metadata_url: Option<String>,
       pub(crate) idp_entity_id: String,
       pub(crate) sso_url: String,
       pub(crate) certificates: Vec<Vec<u8>>,
       pub(crate) attributes: SamlAttributes,
       /// `Some(now)` when the metadata was just fetched from `metadata_url`.
       pub(crate) refreshed_at: Option<i64>,
   }

   /// What a scheduled refresh found (SA2.6).
   pub(crate) enum RefreshOutcome {
       /// Usable metadata with the stored entity ID.
       Fetched { sso_url: String, certificates: Vec<Vec<u8>> },
       /// A failure class, stored in `refresh_error` (one of `REFRESH_ERRORS`).
       Failed(&'static str),
   }

   /// The stored failure classes (`refresh_error`), snake_case.
   pub(crate) const REFRESH_ERRORS: [&str; 6] = ["fetch_failed", "status", "too_large", "denied", "unusable", "entity_id_changed"];
   ```
   `SamlWrite` gets a manual `Debug` with counts only. `RefreshOutcome` derives `Debug`; its URL and certificates make that acceptable only because it is never logged. Add a doc line saying so, and never pass it to a `tracing` macro.
8. `store.rs`, SAML functions (signatures exact; `?` on sqlx errors as elsewhere):
   ```rust
   /// The SAML configuration of provider row `provider_id`.
   pub(crate) async fn saml(pool: &SqlitePool, provider_id: Uuid) -> Result<Option<SamlRow>, MetaError>;
   /// Same, in the write transaction.
   pub(crate) async fn saml_tx(conn: &mut SqliteConnection, provider_id: Uuid) -> Result<Option<SamlRow>, MetaError>;
   /// Every SAML configuration of an active org, keyed by provider id (for `OrgSso::reload`).
   pub(crate) async fn active_saml(pool: &SqlitePool) -> Result<HashMap<Uuid, SamlRow>, MetaError>;
   /// Saves a SAML provider: insert, replace, or switch from OIDC. Returns the provider row id.
   pub(crate) async fn put_saml_provider(conn: &mut SqliteConnection, org: OrgId, new_id: Uuid, write: SamlWrite, now: i64) -> Result<Uuid, MetaError>;
   /// SAML providers of active orgs with a metadata URL whose last refresh attempt is older than `cutoff`.
   pub(crate) async fn due_saml_refreshes(pool: &SqlitePool, cutoff: i64, limit: u32) -> Result<Vec<(OrgId, SamlRow)>, MetaError>;
   /// Records a scheduled refresh. Returns whether the certificate set changed.
   pub(crate) async fn record_refresh(conn: &mut SqliteConnection, provider_id: Uuid, outcome: RefreshOutcome, now: i64) -> Result<bool, MetaError>;
   ```
   - `saml` / `saml_tx` / `active_saml`:
     - Read `org_sso_saml` by `provider_id`. `active_saml` joins `org_sso_providers p` and `orgs o` with `o.state = 'active'`.
     - Then read `SELECT der FROM org_sso_saml_certificates WHERE provider_id = ? ORDER BY position`.
     - `active_saml` reads all certificate rows of those providers in one query, `ORDER BY provider_id, position`, and groups them.
     - A row with no certificate → `corrupt("org_sso_saml_certificates", "der")`.
   - `put_saml_provider` steps:
     1. `existing = provider_tx(conn, org)`.
     2. `None` → `INSERT INTO org_sso_providers (id, org_id, protocol, display_name, jit_role, created_at, updated_at) VALUES (?, ?, 'saml', ?, ?, ?, ?)` with `new_id`. `id = new_id`.
     3. `Some(row)` with `Oidc` → `UPDATE org_sso_providers SET protocol = 'saml', issuer = NULL, client_id = NULL, client_secret_enc = NULL, display_name = ?, jit_role = ?, enabled = 0, enforced = 0, last_login_at = NULL, updated_at = ? WHERE org_id = ?`. `id = row.id`.
     4. `Some(row)` with `Saml`:
        - `old = saml_tx(conn, row.id)` (`None` → `corrupt("org_sso_saml", "provider_id")`).
        - `UPDATE org_sso_providers SET display_name = ?, jit_role = ?, updated_at = ?, last_login_at = CASE WHEN ? THEN NULL ELSE last_login_at END WHERE org_id = ?`, binding `old.idp_entity_id != write.idp_entity_id` as the CASE condition.
        - `id = row.id`.
     5. `DELETE FROM org_sso_saml WHERE provider_id = ?` (cascades the certificates), then `INSERT INTO org_sso_saml (provider_id, metadata_url, idp_entity_id, sso_url, email_attribute, name_attribute, first_name_attribute, last_name_attribute, refreshed_at, refresh_checked_at, refresh_error, certificates_changed_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NULL, NULL)`. Both `refreshed_at` and `refresh_checked_at` take `write.refreshed_at`.
     6. Insert each certificate with `position` = its index (0-based).
     7. Return `id`.

     Identities are **not** deleted here; the handler decides (SA2.4).
   - `due_saml_refreshes`: `WHERE s.metadata_url IS NOT NULL AND COALESCE(s.refresh_checked_at, 0) < ? AND o.state = 'active' ORDER BY COALESCE(s.refresh_checked_at, 0), s.provider_id LIMIT ?`. It returns each row with its `org_id` and certificates, read as in `saml`.
   - `record_refresh`:
     - `Fetched`:
       - Compare the new `certificates` with the stored list as a set of byte vectors (order-insensitive).
       - If they differ: replace the certificate rows (delete, then insert in the new order) and set `certificates_changed_at = now`.
       - Always set `sso_url`, `refreshed_at = now`, `refresh_checked_at = now` and `refresh_error = NULL`.
       - Return whether they differed.
     - `Failed(class)`: `refresh_error = class`, `refresh_checked_at = now`; return `false`.
   - `delete_provider` needs no change: the cascades remove the SAML rows. Add a test.
9. `store.rs` tests (in its `#[cfg(test)] mod tests`, using `crate::meta::test_db()` as the existing ones do):
   - `an_oidc_row_reads_back_with_its_protocol`;
   - `a_saml_provider_round_trips`: insert via `put_saml_provider` with two certificates and an email attribute, then `provider` and `saml`. The certificates come back in order;
   - `switching_oidc_to_saml_clears_the_oidc_columns_and_the_flags`: OIDC with `enabled = enforced = true` and `last_login_at` set, then `put_saml_provider`. Protocol `saml`, `oidc == None`, flags off, `last_login_at == None`;
   - `switching_saml_to_oidc_drops_the_saml_rows`: then `saml()` is `None`, and the certificate count is 0 (`SELECT COUNT(*)`);
   - `an_oidc_switch_without_a_secret_is_a_caller_bug` → `MetaError::Corrupt`;
   - `replacing_saml_with_another_entity_id_resets_last_login`, and `…with_the_same_entity_id_keeps_it`;
   - `deleting_a_saml_provider_cascades`: zero rows left in both SAML tables;
   - `the_check_constraint_refuses_a_mixed_row`: a raw `INSERT` with `protocol = 'saml'` and an issuer fails;
   - `due_refreshes_skip_pasted_metadata_and_recent_checks`;
   - `a_refresh_with_new_certificates_records_the_change`, `a_refresh_with_the_same_set_in_another_order_is_no_change`, `a_failed_refresh_keeps_the_metadata`;
   - `active_saml_ignores_dropped_orgs`.

**Done when:** written and committed (no build or test).

**Commit:** `Store SAML providers beside OIDC (D-SA-17)`

---

## SA2.2 — Feature `saml`

**Goal:** add the `ee` feature `saml` (D-SA-4).

**Edit:** `crates/server/src/api/entitlements.rs`, `crates/server/src/entitlements/mod.rs` (tests), `crates/server/src/config.rs` (test), `web/src/components/plan/feature-label.ts`, `web/src/routes/$org/settings/plan.tsx`.

**Steps:**

1. `api/entitlements.rs`:
   - Append `Saml` as the **last** variant of `Feature` (bit index 4).
   - Add it to `ALL` (length 5), and to `name()` → `"saml"`.
   - Add it to `is_ee()` (the `matches!` list) and to `from_name("saml")`.
   - In `locked_detail()`, use the exact text `"SAML single sign-on needs a licence or plan that includes it."`. Match the style of the `OrgSso` arm: if that arm's sentence has a different form, copy its form and say SAML. Its doc comment: `/// SAML 2.0 single sign-on for an org's provider (track SA, ee).`
2. `entitlements/mod.rs` tests: wherever a test enumerates `Feature::ALL` or feature names, add `saml` the same way. Grep the file for `OrgSso` and `"org_sso"` in `#[cfg(test)]` code.
3. `config.rs` test at the loop over `["project_roles", "org_sso", "nope"]` (about line 2796): add `"saml"` to the ee features it expects to be refused in `[entitlements.defaults]`.
4. `web/src/components/plan/feature-label.ts`: add the `saml` case to the exhaustive switch with label `"SAML single sign-on"`.
5. `web/src/routes/$org/settings/plan.tsx`: add `saml` to the `FEATURES` list after `org_sso`, using the same label source.

**Tests:** the existing entitlement tests with `saml` added, plus `saml_is_an_ee_feature` (`Feature::Saml.is_ee()`, `from_name("saml") == Some(Feature::Saml)`, `name() == "saml"`).

**Done when:** written and committed.

**Commit:** `Add the saml feature (D-SA-4)`

---

## SA2.3 — Index, pending table, sign-in core

**Goal:** the routing index builds SAML providers, `OrgSso` holds the pending-request table and fetches metadata, and the OIDC callback's sign-in logic becomes a core both protocols call (README § Parts 2).

**Edit:** `crates/server/src/ee/org_sso/mod.rs`, `crates/server/src/ee/org_sso/login.rs`, `crates/server/src/ee/org_sso/saml_pending.rs` (new), `crates/server/src/test_support/mod.rs`, `crates/server/src/serve.rs` (only if `OrgSso::new`'s call needs no change, leave it).

**Steps:**

1. `saml_pending.rs` (new, `ee` header):
   ```rust
   //! Pending SAML AuthnRequests (D-SA-10): in memory, single use, bound to the browser.

   use std::collections::{HashMap, VecDeque};
   use std::net::IpAddr;

   pub(crate) const PENDING_TTL_NS: i64 = 600_000_000_000;
   pub(crate) const MAX_PER_IP: usize = 50;
   pub(crate) const MAX_PER_ORG: usize = 1_000;
   pub(crate) const MAX_TOTAL: usize = 100_000;

   /// One pending login. `Debug` hides the redirect and the binder hash.
   #[derive(Clone, PartialEq, Eq)]
   pub(crate) struct Pending {
       pub(crate) org: OrgId,
       pub(crate) test_owner: Option<uuid::Uuid>,
       pub(crate) redirect: String,
       /// Lowercase hex SHA-256 of the binder cookie's value.
       pub(crate) binder_sha256: String,
       pub(crate) ip: IpAddr,
       pub(crate) expires_ns: i64,
   }

   /// Why `take` found no usable entry. Both answer `saml_invalid`; the distinction is for tests.
   #[derive(Debug, Clone, Copy, PartialEq, Eq)]
   pub(crate) enum TakeError { Unknown, Binder }

   #[derive(Debug, Default)]
   pub(crate) struct PendingTable { entries: HashMap<String, Pending>, order: VecDeque<String> }

   impl PendingTable {
       /// Records `request_id`, evicting expired entries, then the oldest entry of the same IP
       /// or org while either is at its limit, then the oldest overall at `MAX_TOTAL`.
       pub(crate) fn insert(&mut self, request_id: String, entry: Pending, now: i64);
       /// The entry for `request_id` when it exists, is not expired, belongs to `org`, and
       /// `binder`'s SHA-256 matches (constant-time). It is removed only on success; a wrong
       /// binder leaves it in place (D-SA-10). An expired entry is removed and is `Unknown`.
       pub(crate) fn take(&mut self, request_id: &str, org: OrgId, binder: Option<&str>, now: i64) -> Result<Pending, TakeError>;
       /// Number of live entries (tests).
       pub(crate) fn len(&self) -> usize;
   }
   ```
   `insert` steps:
   1. Pop from the front of `order` while the front id is absent from `entries` or its entry is expired (`expires_ns <= now`), removing it from `entries`.
   2. While the count of entries with `entry.ip == ip` is `>= MAX_PER_IP`, remove the first id in `order` whose entry has that IP.
   3. Same for `org` with `MAX_PER_ORG`.
   4. While `entries.len() >= MAX_TOTAL`, pop and remove the front.
   5. Insert, and push the id to the back.

   `take` steps:
   - Missing or expired → `Unknown` (remove if expired).
   - `entry.org != org` → `Unknown`, leave it.
   - `binder` is `None`, or `!crate::auth::secret::constant_time_eq(&hex::encode(Sha256::digest(binder)), &entry.binder_sha256)` → `Binder`, leave it.
   - Otherwise remove it from `entries` and return it. `order` is cleaned lazily by `insert`.

   Tests: `an_entry_is_taken_once`, `a_wrong_binder_keeps_the_entry`, `a_missing_binder_keeps_the_entry`, `another_org_cannot_take_it`, `an_expired_entry_is_unknown_and_removed`, `the_51st_entry_of_an_ip_evicts_its_oldest`, `the_1001st_entry_of_an_org_evicts_its_oldest`, `other_ips_are_not_evicted_by_one_ip`, `debug_hides_the_binder_hash_and_redirect`.
2. `mod.rs`:
   - Declare `pub(crate) mod saml_pending;`.
   - `ProviderConfig` becomes an enum:
     ```rust
     #[derive(Clone, PartialEq, Eq)]
     enum ProviderConfig {
         Oidc { issuer: String, client_id: String, client_secret_enc: Vec<u8>, display_name: String, jit_role: JitRole },
         Saml { idp_entity_id: String, sso_url: String, certificates: Vec<Vec<u8>>, attributes: store::SamlAttributes, display_name: String, jit_role: JitRole },
     }
     ```
     Its `Debug` stays `ProviderConfig { .. }`.
   - `OrgProvider` becomes:
     ```rust
     #[derive(Debug)]
     pub(crate) struct OrgProvider { pub(crate) kind: ProviderKind, pub(crate) jit_role: JitRole, config: ProviderConfig }

     #[derive(Debug)]
     pub(crate) enum ProviderKind { Oidc(OidcProvider), Saml(SamlProvider) }

     /// A built SAML provider: the trusted metadata and the attribute names to read.
     #[derive(Debug)]
     pub(crate) struct SamlProvider { pub(crate) metadata: cetra_saml::IdpMetadata, pub(crate) attributes: cetra_saml::AttributeNames }
     ```
   - `pub(crate) fn attribute_names(attributes: &store::SamlAttributes) -> cetra_saml::AttributeNames`: start from `AttributeNames::default()`, and for each field that is `Some(name)`, replace that list with `vec![name.clone()]`.
   - `OrgSso` gains `policy: cetra_eval::EgressPolicy` (stored in `new`; it is `Copy`) and `saml_pending: std::sync::Mutex<saml_pending::PendingTable>`. Add the accessors:
     ```rust
     pub(crate) fn policy(&self) -> cetra_eval::EgressPolicy;
     /// Runs `f` on the pending table. A poisoned lock is recovered (`PoisonError::into_inner`),
     /// as `index` does.
     pub(crate) fn with_pending<T>(&self, f: impl FnOnce(&mut saml_pending::PendingTable) -> T) -> T;
     ```
   - `reload`:
     - Read `store::active_saml(meta.reader())` once, before the provider loop.
     - In the loop, match `row.protocol`:
       - `Oidc`: as today, reading `row.oidc` (a `None` here is impossible by the `CHECK`, but treat it as `unreadable += 1` with the same warn line).
       - `Saml`: take the `SamlRow` for `row.id` (missing → `unreadable`).
     - Build `ProviderConfig::Saml { .. }` from it. If the current index has an `Arc` with an equal config, keep it. Otherwise build `IdpMetadata::from_fields(&saml.idp_entity_id, &saml.sso_url, certs)`, where `certs` is `saml.certificates` mapped through `IdpCertificate::from_der` and collected. An error there is `unreadable` with the same warn line.
     - `ProviderKind::Saml(SamlProvider { metadata, attributes: attribute_names(&saml.attributes) })`.
   - The test `unchanged_providers_keep_their_metadata` and the `renamed.oidc.display_name()` line become a `match &renamed.kind { ProviderKind::Oidc(oidc) => …, _ => panic!() }`. Add `unchanged_saml_providers_keep_their_arc` and `a_saml_provider_with_an_unreadable_certificate_is_skipped`.
   - New method:
     ```rust
     /// Fetches IdP metadata from `url` through the guarded client (D-SA-11, D-SA-19).
     pub(crate) async fn fetch_metadata(&self, url: &str) -> Result<Vec<u8>, MetadataFetchError>;

     #[derive(Debug, Clone, Copy, PartialEq, Eq)]
     pub(crate) enum MetadataFetchError { Url, Denied, Unavailable, Status, TooLarge }
     ```
     Steps:
     1. `crate::config::issuer_problem(url).is_some()` → `Url`; parsing as `url::Url` fails → `Url`.
     2. Build `oauth2::http::Request::builder().method("GET").uri(url).header("accept", "application/samlmetadata+xml, application/xml, text/xml").body(Vec::new())` (an error → `Url`).
     3. `self.http.send(request).await`: `IdpHttpError::Denied` → `Denied`, `TooLarge` → `TooLarge`, `Request` → `Unavailable`.
     4. A status other than 200 → `Status`.
     5. Return the body.
3. `login.rs`, the sign-in core:
   ```rust
   /// What a protocol's front hands to the sign-in core: a verified subject and email for `org`.
   pub(crate) struct SignIn {
       pub(crate) org: OrgId,
       pub(crate) protocol: SsoProtocol,
       pub(crate) jit_role: JitRole,
       pub(crate) subject: String,
       /// Normalized (`normalize_email`).
       pub(crate) email: String,
       pub(crate) name: Option<String>,
       pub(crate) test: bool,
       pub(crate) test_owner: Option<Uuid>,
       pub(crate) redirect: String,
   }

   /// Steps 8–13 of the former `finish`: domain check, `admit`, audit lines, redirect.
   pub(crate) async fn sign_in_core(state: &AppState, input: SignIn) -> Result<LoggedIn, LoginError>;
   ```
   - `SignIn` gets a manual `Debug` that prints `org`, `protocol` and `test` only.
   - Move `finish`'s code from the domain check (its step 8: `domains = state.org_sso.domains_of(org)`, `DomainNotAllowed`) through the return of `LoggedIn` into `sign_in_core` **unchanged**, except:
     - the email, subject, name, test, test_owner and redirect come from `input`;
     - `jit` is `let entitlements = of(state, input.org); (entitlements.allows(Feature::OrgSso) && (input.protocol == SsoProtocol::Oidc || entitlements.allows(Feature::Saml))).then(|| input.jit_role.org_role())`.
   - `finish` keeps steps 1–7. Step 3 now requires `ProviderKind::Oidc(oidc)`; any other kind is `SsoUnavailable`. It then builds `SignIn { protocol: SsoProtocol::Oidc, jit_role: provider.jit_role, subject: claims.subject, email, name: display_name(claims.name), test: saved.test, test_owner: saved.test_owner, redirect: saved.redirect, .. }` and returns `sign_in_core(state, input).await`.
   - `authorize`, at the `provider.oidc.authorization_url` call, matches `ProviderKind::Oidc(oidc)`. A SAML kind is handled in SA2.5; until then it answers `unavailable()`. SA2.5 replaces that arm.
   - The existing login tests must still pass unchanged. No test edit is needed beyond compile fixes.
4. `test_support/mod.rs`:
   ```rust
   /// Saves a SAML provider for `org` from the test IdP's metadata for `keys`, with the flags
   /// given, verifies `domain`, sets `last_login_at` (as after a test sign-in), and reloads.
   pub(crate) async fn configure_org_saml(app: &TestApp, org: OrgId, keys: &[cetra_saml::test_idp::TestKey], domain: &str, enabled: bool, enforced: bool);

   /// [`org_sso_app_with`] for SAML: an https public URL and a licence listing `org_sso` and `saml`.
   pub(crate) async fn saml_app() -> (TestApp, Seeded, Member);
   ```
   - `configure_org_saml` parses `cetra_saml::test_idp::metadata_xml(keys)` with `cetra_saml::parse_metadata`. It writes `store::put_saml_provider` with `display_name: "Acme SAML"`, `JitRole::Viewer`, `metadata_url: None`, the entity ID, SSO URL and DER certificates, default attributes and `refreshed_at: None`. Then `set_flags`, `set_last_login`, the domain insert and verification, and `reload_org_sso`, all as `configure_org_sso` does.
   - `saml_app` is `org_sso_app_with(|config| { config.public_url = serde_json::from_str("\"https://cetra.example\"").expect("url"); config.license_key = Some(crate::config::Secret::new(license_key(&["org_sso", "saml"]))); })`.

**Tests:** `saml_pending.rs`'s list; the two `mod.rs` tests; `a_saml_provider_does_not_serve_the_oidc_callback` in `login.rs` (configure SAML, hand-craft the OIDC callback with a sealed `OAuthState` for that org as `state_is_bound_to_the_org_flow` does → `/login?error=sso_unavailable`).

**Done when:** written and committed.

**Commit:** `Route SAML providers and share the org sign-in core`

---

## SA2.4 — Settings API

**Goal:** owners save SAML providers and switch protocols (D-SA-18), with D-SA-2, D-SA-11 and D-SA-19 enforced.

**Edit:** `crates/server/src/api/org_sso.rs`, `crates/server/src/ee/org_sso/settings.rs`, `fixtures/api/static/org-sso.json`, `web/src/ee/org-sso/provider-section.tsx`.

**Steps:**

1. `api/org_sso.rs`, response types:
   - `OrgSsoProvider` gains `pub protocol: SsoProtocol` as its **first** field. `issuer` and `client_id` become `Option<String>` with `#[serde(skip_serializing_if = "Option::is_none", default)]`. It also gains `#[serde(skip_serializing_if = "Option::is_none", default)] pub saml: Option<OrgSsoSaml>`.
   - New types (same derives as the other response types):
     ```rust
     /// A SAML provider's settings (D-SA-11, D-SA-18). Never contains the metadata document.
     pub struct OrgSsoSaml {
         pub idp_entity_id: String,
         pub sso_url: String,
         #[serde(skip_serializing_if = "Option::is_none", default)]
         pub metadata_url: Option<String>,
         pub certificates: Vec<OrgSsoCertificate>,
         pub attributes: SamlAttributeNames,
         /// optional times use `crate::api::keys::optional_time` as `last_login_at` does
         pub refreshed_at: Option<i64>,
         pub refresh_error: Option<SamlRefreshError>,
         pub certificates_changed_at: Option<i64>,
     }
     /// A trusted IdP signing certificate, shown by hash and validity (D-SA-11).
     pub struct OrgSsoCertificate {
         pub sha256: String,
         /// `cetra_core::time_serde`, schema `String` format `DateTime`, as `created_at`
         pub not_before: i64,
         pub not_after: i64,
     }
     /// Attribute names an owner set; an absent field uses the defaults (D-SA-6).
     pub struct SamlAttributeNames {
         pub email: Option<String>,
         pub display_name: Option<String>,
         pub first_name: Option<String>,
         pub last_name: Option<String>,
     } // each `skip_serializing_if = "Option::is_none", default`
     /// Why the last scheduled refresh failed (`org_sso_saml.refresh_error`).
     pub enum SamlRefreshError { FetchFailed, Status, TooLarge, Denied, Unusable, EntityIdChanged } // snake_case = store::REFRESH_ERRORS
     /// The values an owner copies into the IdP (D-SA-12).
     pub struct OrgSsoSamlSp { pub entity_id: String, pub acs_url: String, pub start_url: String }
     ```
     `OrgSso` gains `pub saml_sp: OrgSsoSamlSp` after `callback_url`.
   - The request type, deny unknown fields, with a manual `Debug` that prints only which source is set and the certificate count:
     ```rust
     /// `PUT /api/v1/orgs/{org}/sso/saml` (D-SA-18). Exactly one source: `metadata_url`,
     /// `metadata_xml`, or `idp_entity_id` + `sso_url` + `certificates`.
     pub struct PutOrgSamlProvider {
         pub display_name: String,
         pub jit_role: JitRole,
         #[serde(default)] pub metadata_url: Option<String>,
         #[serde(default)] pub metadata_xml: Option<String>,
         #[serde(default)] pub idp_entity_id: Option<String>,
         #[serde(default)] pub sso_url: Option<String>,
         #[serde(default)] pub certificates: Option<Vec<String>>,
         #[serde(default)] pub attributes: SamlAttributeNames,
         #[serde(default)] pub confirm_entity_id_change: bool,
     }
     ```
     `SamlAttributeNames` also derives `Default`.
2. `settings.rs`, `view()`:
   - `saml_sp` uses the SP paths of § Conventions with `access.org.id`.
   - `provider_view` maps a row:
     - OIDC → `issuer` / `client_id` `Some`, `saml: None`;
     - SAML → `issuer` / `client_id` `None` and `saml: Some(..)` built from `store::saml(reader, row.id)`.
   - `OrgSsoCertificate` values come from `cetra_saml::IdpCertificate::from_der`, using its `sha256_hex()`, `not_before_ns()` and `not_after_ns()`. A stored certificate that no longer reads is shown with `sha256` = hex SHA-256 of the DER and both dates `0`.
   - `refresh_error` maps the stored string through `SamlRefreshError`'s snake_case names; an unknown string → `None`.
3. `settings.rs`, new route in `routes()`: `PUT /api/v1/orgs/{org}/sso/saml` → `put_org_saml_provider`, with the same `Access` (`OrgPerm::ManageSso`, session only) and `documented: true` as the PUT provider route.
4. `put_org_saml_provider` steps, in order:
   1. `require(&state, org, Feature::OrgSso)?` then `require(&state, org, Feature::Saml)?`.
   2. `!state.config.public_url.is_https()` → `ApiError::Conflict { detail: "SAML needs an https public URL (public_url).".to_owned() }`.
   3. `validate_saml(&body)`:
      - `display_name` is 1..=100 characters after trim (field `display_name`, the same message as OIDC's `validate`).
      - Each attribute name, when set, is 1..=256 characters with no control character (field `attributes.email` and so on, message `"invalid attribute name"`).
      - Sources: let `url = metadata_url.is_some()`, `xml = metadata_xml.is_some()`, `manual = any of idp_entity_id / sso_url / certificates is Some`. Exactly one of the three must hold, and `manual` needs all three fields. Otherwise field `metadata`, message `"give a metadata URL, the metadata XML, or the entity ID, SSO URL and certificates"`.
   4. Resolve the metadata:
      - URL:
        1. `refuse_outbound(&state, "identity provider check")?`.
        2. `state.org_sso.fetch_metadata(url).await`. `Url` → field `metadata_url`, `"metadata URL must be https, or http on a loopback host"`. Any other error → field `metadata_url`, `"the metadata could not be loaded"`.
        3. Then `cetra_saml::parse_metadata(&bytes)`. An error → field `metadata_url`, with the message `error.to_string()` (static texts).
        4. `refreshed_at = Some(now)`.
      - XML: `parse_metadata(xml.as_bytes())`; an error → field `metadata_xml`, `error.to_string()`.
      - Manual:
        1. Each certificate goes through `IdpCertificate::from_pem`. An error → field `certificates`, `error.to_string()`.
        2. Then `IdpMetadata::from_fields(entity, sso, certs)`. `EntityId` → field `idp_entity_id`; `SsoUrl` → field `sso_url`; any other → field `certificates`, each with `error.to_string()`.
   5. SSO URL egress: `url::Url::parse(metadata.sso_url())`, then `state.org_sso.policy().allows_url(&url)`. False → field `metadata`, `"the identity provider's sign-on URL is not allowed by the egress policy"`.
   6. The write transaction (`state.meta.write`):
      - Read `existing = store::provider_tx(conn, org)`.
      - `Some(row)` with `row.protocol == Oidc && row.enforced` → `ApiError::Conflict { detail: "Turn off enforcement before switching the identity provider's protocol." }`.
      - `Some(row)` with SAML: `old = store::saml_tx(conn, row.id)`. If `old.idp_entity_id != metadata.entity_id()` and `!body.confirm_entity_id_change` → `ApiError::Conflict { detail: "Changing the IdP entity ID deletes the linked identities; confirm the change." }`.
      - `let delete_identities = matches!(existing, Some(ref row) if row.protocol == Oidc) || entity_changed;`
      - `store::put_saml_provider(conn, org, Uuid::now_v7(), write, now)`.
      - If `delete_identities`, `store::delete_identities(conn, org)`.
      - The handler maps `ApiError`s out of the closure the way `put_org_sso_provider` does with its `unchanged` 409. Read that code and use the same pattern.
   7. `super::refresh(&state).await`. Then `tracing::info!(target: "audit", org_id = %org, user_id = %caller, protocol = "saml", "org sso provider saved")`. Return `view()`.
5. `put_org_sso_provider` (OIDC):
   - `existing.protocol == Saml && existing.enforced` → the same 409 as step 6.
   - A switch from SAML needs a `client_secret` (field `client_secret`, `"required"`, the existing message) and deletes the identities. The existing `changes_identity` handling covers it: treat a switch as `changes_identity = true`.
   - The audit line gains `protocol = "oidc"`.
6. `patch_org_sso`: when the stored provider is SAML and the request turns `enabled` or `enforced` on:
   - `require(&state, org, Feature::Saml)?` (after the existing `OrgSso` check);
   - the same https 409 as step 4.2.

   Turning things off needs neither (D-P-61).
7. `fixtures/api/static/org-sso.json`: add `"saml_sp"` after `"callback_url"` with
   - `"entity_id": "http://localhost:8080/api/auth/saml/<the org id used elsewhere in the fixtures for acme>"`,
   - `"acs_url"` (`…/acs`),
   - `"start_url"` (`…/start`).

   Find the acme org id in `fixtures/api` (`grep -r '"slug": "acme"' fixtures/api`). Add `"protocol": "oidc"` as the provider's first key. Keep the file's existing key order otherwise. `static_files_match_their_types` round-trips it byte for byte, so the key order must equal the struct field order.
8. `web/src/ee/org-sso/provider-section.tsx`: where it reads `provider.issuer` and `provider.client_id` (now optional in the generated types), use `provider.issuer ?? ""` and `provider.client_id ?? ""`. When the PUT body is built from the form, nothing changes (the OIDC PUT body did not change). No other web change.

**Tests** (`settings.rs` tests module; `saml_app()` and its helpers; `metadata_xml(&[TestKey::Idp])` as pasted XML):
- `an_owner_saves_a_saml_provider_from_xml`:
  - 200;
  - `provider.protocol == saml`, `enabled == false`;
  - `saml.idp_entity_id`, one certificate with a 64-hex `sha256`;
  - `saml_sp.acs_url` ends with `/acs`;
  - the routing index has a `ProviderKind::Saml` for the org.
- `manual_fields_save`: entity ID, SSO URL, one PEM.
- `a_metadata_url_is_fetched_and_refreshed_at_is_set`: serve the metadata from a loopback axum server, started like `MockIdp::start` with one route returning `metadata_xml`. `allow_private_network_egress` is already true in `org_sso_app_with`.
- `an_http_metadata_url_off_loopback_is_refused` → 400 on `metadata_url`.
- `a_failing_metadata_url_is_400`: the server answers 500.
- `zero_or_two_sources_are_400` → field `metadata`.
- `bad_metadata_is_400_with_a_static_message`: XML without a signing certificate → the field error's message equals `MetadataError::NoSigningCertificate.to_string()`.
- `saml_needs_both_features`: licence `["org_sso"]` only → 403 `plan_required`.
- `saml_needs_an_https_public_url`: `org_sso_app_with` licence `["org_sso", "saml"]`, default http URL → 409.
- `switching_from_oidc_deletes_identities_and_resets_flags`: `configure_org_sso` (OIDC, enabled), an identity row present → PUT saml → identities 0, `enabled == false`.
- `switching_is_refused_while_enforced`: OIDC enforced → 409; SAML enforced, then PUT OIDC → 409.
- `an_entity_id_change_needs_confirmation`: 409 without; with `confirm_entity_id_change: true` → 200, identities deleted, `last_login_at == None`.
- `the_same_entity_id_keeps_identities`.
- `switching_back_to_oidc_needs_a_secret`.
- `enabling_saml_needs_the_feature_and_turning_off_does_not`: licence lapsed (the `license_key_until` pattern of `lapse_allows_only_turning_off`).
- `admins_cannot_save_saml` → 403.
- `the_view_shows_the_sp_values_for_oidc_orgs_too`.
- `logs_carry_no_metadata`: `assert_logs_clean(&[IDP_ENTITY_ID, IDP_SSO_URL, "BEGIN CERTIFICATE"], put)`.
- `put_org_saml_provider_from_a_url_is_refused_in_restore_mode` → 503 `restore_mode`.

**Done when:** written and committed.

**Commit:** `Save SAML providers and switch protocols (D-SA-18)`

---

## SA2.5 — SAML login routes

**Goal:** the SP-initiated login, the ACS, the unsolicited restart and the SP metadata (D-SA-5, D-SA-10, D-SA-12).

**Edit:** `crates/server/src/ee/org_sso/saml.rs` (new), `crates/server/src/ee/org_sso/mod.rs` (one `mod` line), `crates/server/src/ee/org_sso/login.rs` (the authorize branch and tests), `crates/server/src/auth/oidc/flow.rs` (`LoginError`), `crates/server/src/http/csrf.rs`, `crates/server/src/http/access.rs` (`routes.extend`), `crates/server/src/auth/limits.rs`.

**Steps:**

1. `flow.rs`: add `LoginError::SamlInvalid` (doc: `/// A SAML response failed validation, or did not match a pending request (D-SA-9, D-SA-10).`) and `LoginError::SamlUnsupported` (doc: `/// A SAML response used a feature cetra refuses, such as an encrypted assertion (D-SA-8).`). Their codes are `"saml_invalid"` and `"saml_unsupported"`.
2. `limits.rs`: add `pub(crate) saml_acs: KeyedLimiter<IpAddr>` with the doc comment `/// 60 per minute per client IP for the SAML ACS (D-SA-10).`. Build it in `Limits::new` as `KeyedLimiter::new(Bucket { capacity: 60, per: Duration::from_secs(60) }, MAX_KEYS)`, and prune it in `Limits::prune` like `sso_discover`.
3. `csrf.rs`, in `must_check`, before the `session ||` expression: if the request's matched path (`request.extensions().get::<axum::extract::MatchedPath>()`) is exactly `"/api/auth/saml/{org_id}/acs"`, return `false`. If `MatchedPath` is absent, a fallback compares the raw path. It must have exactly the segments `["", "api", "auth", "saml", <36-char id>, "acs"]`. Never use a prefix test. The comment above it: `// The IdP's cross-site POST carries the IdP's Origin; the binder cookie and the signature protect the ACS (D-SA-10, D-SA-12).`
   - If the `csrf` middleware runs before routing, `MatchedPath` is not available there. Check where `csrf` sits in `finish()` (`http/mod.rs`): the layers run before the router matches. In that case use only the raw-path segment test and drop the `MatchedPath` branch.
4. `saml.rs` (new, `ee` header). Constants from § Conventions, plus:
   ```rust
   const ACS_BODY_LIMIT: usize = 256 * 1024;
   const RESTART_MAX_AGE_SECS: u64 = 120;
   const BINDER_MAX_AGE_SECS: u64 = 600;
   ```
   Routes (`pub(crate) fn routes() -> Vec<RouteDef>`, all `Access::Public`, `documented: true`):
   - `GET /api/auth/saml/{org_id}` → `sp_metadata`
   - `GET /api/auth/saml/{org_id}/start` → `start`
   - `POST /api/auth/saml/{org_id}/acs` → `acs`, with `post(acs).layer(axum::extract::DefaultBodyLimit::max(ACS_BODY_LIMIT))` (precedent `routes/annotation_queues.rs:83`).

   The path parameter is extracted with `error::Path<crate::routes::entitlements::OrgIdPath>` (field `org_id: Uuid`).

   Helpers:
   ```rust
   /// The SP entity ID, ACS URL and start URL of `org` (D-SA-12).
   pub(crate) fn sp_urls(state: &AppState, org: OrgId) -> (String, String, String);
   /// The org's SAML provider, when the org's provider is SAML.
   fn saml_provider(state: &AppState, org: OrgId) -> Option<(Arc<OrgProvider>, &SamlProvider)>; // or return Arc and match at the call site; lifetimes decide
   /// Starts an SP-initiated login: a pending entry, the binder cookie, the redirect (D-SA-10).
   pub(crate) async fn begin(state: &AppState, org: OrgId, provider: &SamlProvider, ip: IpAddr, test_owner: Option<Uuid>, redirect: String) -> Result<Response, ApiError>;
   fn cookie(name: &str, value: &str, path: &str, max_age: u64) -> HeaderValue; // "{name}={value}; HttpOnly; Secure; SameSite=None; Path={path}; Max-Age={max_age}"
   fn named_cookie(headers: &HeaderMap, name: &str) -> Option<String>;       // the body of state_cookie::cookie_value with `name`
   ```
   - `sp_urls` joins the three paths of § Conventions onto `state.config.public_url`.
   - `begin` steps:
     1. `!state.config.public_url.is_https()` → `redirect("/login?error=sso_unavailable", empty)`.
     2. `url::Url::parse(provider.metadata.sso_url())` and `state.org_sso.policy().allows_url(&url)`. Either failing → the same `sso_unavailable` redirect.
     3. `request_id = format!("_{}", hex::encode(random_bytes(16)?))`, where `crate::auth::secret::random_bytes` maps its error to `internal`.
     4. `binder = new_opaque_token().map_err(internal)?.plaintext`.
     5. `state.org_sso.with_pending(|table| table.insert(request_id.clone(), Pending { org, test_owner, redirect, binder_sha256: hex::encode(Sha256::digest(binder.as_bytes())), ip, expires_ns: now + PENDING_TTL_NS }, now))`.
     6. `let (entity, acs, _) = sp_urls(state, org)`. `location = cetra_saml::authn_request_redirect_url(&AuthnRequest { sp_entity_id: &entity, acs_url: &acs, sso_url: provider.metadata.sso_url(), request_id: &request_id, relay_state: RELAY_STATE, issue_instant_ns: now })`. An error → `internal`.
     7. `routes::oauth::redirect(&location, [cookie(SAML_COOKIE, &binder, <ACS path>, BINDER_MAX_AGE_SECS)])`. The cookie path is the ACS path without the origin: `format!("/api/auth/saml/{org}/acs")`.
   - `sp_metadata`:
     - org not parsed or no SAML provider for it in the index → `ApiError::NotFound`.
     - Otherwise 200, `Content-Type: application/samlmetadata+xml`, body `cetra_saml::sp_metadata_xml(&entity, &acs)`.
   - `start`:
     1. `refuse_outbound(&state, "external sign-in")?`.
     2. `state.limits.sso_discover.check(&ip, now)` (D-P-60's bucket) → 429.
     3. No SAML provider → `redirect("/login?error=sso_unavailable", empty)`.
     4. `state.org_sso.flags(org)` must be enabled, else the same redirect. A disabled provider is reached only through `authorize`'s test path (D-SA-5).
     5. `begin(&state, org, provider, ip, None, "/".to_owned())`.
   - `acs` (arguments: `State`, `ClientIp(ip)`, `headers`, the path, `body: axum::body::Bytes`). Steps:
     1. `state.limits.saml_acs.check(&ip, now)` → 429.
     2. No SAML provider for the org → `ApiError::NotFound`.
     3. `#[derive(Deserialize)] struct AcsForm { #[serde(rename = "SAMLResponse")] saml_response: String }` via `serde_urlencoded::from_bytes(&body)`. An error → `fail(LoginError::SamlInvalid)`.
     4. `received = cetra_saml::decode_response(&form.saml_response)`. An error → log the class and `fail(SamlInvalid)`.
     5. **Unsolicited** (`received.in_response_to()` is `None`):
        - `named_cookie(&headers, SAML_RESTART_COOKIE).is_some()` → `fail(SamlInvalid)` (the loop guard).
        - Otherwise `redirect(<start path>, [cookie(SAML_RESTART_COOKIE, "1", &format!("/api/auth/saml/{org}"), RESTART_MAX_AGE_SECS)])`. The start path is relative, `/api/auth/saml/{org}/start`.
     6. `let id = in_response_to.to_owned();`, then `state.org_sso.with_pending(|t| t.take(&id, org, named_cookie(&headers, SAML_COOKIE).as_deref(), now))`. An `Err(_)` → `fail(SamlInvalid)`.
     7. Flags: `let enabled = state.org_sso.flags(org).is_some_and(|f| f.enabled); if !enabled && pending.test_owner.is_none()` → `fail(SsoUnavailable)`.
     8. `let (entity, acs, _) = sp_urls(&state, org)`. Call `validate_response(&received, &ExpectedResponse { sp_entity_id: &entity, acs_url: &acs, idp: &provider.metadata, request_id: &id, now_ns: now, attributes: &provider.attributes })`:
        - `Err(ResponseError::EncryptedAssertion)` → `fail(SamlUnsupported)`;
        - any other `Err(e)` → `tracing::info!(target: "audit", org_id = %org, reason = ?e, "saml response refused")` and `fail(SamlInvalid)`.
     9. `email`: `assertion.email.as_deref().and_then(normalize_email)` (the same function `finish` uses), else `fail(EmailNotVerified)`.
     10. `sign_in_core(&state, SignIn { org, protocol: SsoProtocol::Saml, jit_role: org_provider.jit_role, subject: assertion.name_id, email, name: display_name(assertion.display_name), test: pending.test_owner.is_some(), test_owner: pending.test_owner, redirect: pending.redirect })`. An `Err(e)` → `fail(e)`.
     11. Success:
         - `start_session(&state, logged_in.user_id, user_agent, Some(ip), SessionOrigin::OrgProvider(org))`;
         - `tracing::info!(target: "audit", user_id = %logged_in.user_id, org_id = %org, provider = "saml", "login")`;
         - `redirect(&logged_in.redirect, [clear(SAML_COOKIE, acs path), clear(SAML_RESTART_COOKIE, saml path), session_cookie])`, where `clear` is `cookie(name, "", path, 0)`.
     - `fail(error: LoginError) -> Result<Response, ApiError>`: `Internal` → `Err(ApiError::Internal(..))`. Otherwise `redirect(&format!("/login?error={code}"), [clear(SAML_COOKIE, acs path)])`. The restart cookie is left in place, so a loop stops.
   - `mod.rs`: `pub(crate) mod saml;`.
   - `access.rs`: after `routes.extend(crate::ee::org_sso::login::routes());`, add `routes.extend(crate::ee::org_sso::saml::routes());`.
5. `login.rs`, `authorize`: replace SA2.3's temporary `unavailable()` arm for `ProviderKind::Saml(saml)` with `return crate::ee::org_sso::saml::begin(&state, route.org_id, saml, ip, test_owner, state_cookie::safe_redirect(query.redirect.as_deref())).await;`. That branch sits after the test-owner resolution and before the OIDC-only code.

**Tests** (`saml.rs` tests module). Helpers in that module:
- `begin_saml(app, cookie: Option<&str>) -> (String /*request id*/, String /*binder cookie value*/, TestResponse)`:
  1. GET `/api/auth/sso/authorize?domain=acme.test&redirect=%2Facme`.
  2. Read `Location`, parse the URL, take `SAMLRequest`, base64-decode, raw-inflate (`flate2::read::DeflateDecoder`) and parse with `roxmltree`. If `roxmltree` is not a server dependency, use a byte search for `ID="` instead; do not add a dependency.
  3. Take the `ID` attribute and the `cetra_saml` `Set-Cookie` value.
- `post_acs(app, org, b64, cookies: &[&str]) -> TestResponse`: POST with `content-type: application/x-www-form-urlencoded` and body `SAMLResponse=<urlencoded b64>&RelayState=cetra`, with `Origin: https://idp.test` and the given cookies.
- `options_for(app, org, request_id) -> ResponseOptions`: the test IdP defaults with `audiences = vec![entity]`, `recipient = acs`, `destination = Some(acs)`, and both `InResponseTo` values `= Some(request_id)`, from `sp_urls`. It also sets `name_id: "ada@acme.test"` and the email attribute `ada@acme.test`.

Cases:
- `a_saml_user_signs_in_and_joins_with_the_jit_role`: `saml_app`, then `configure_org_saml(.., &[Idp], "acme.test", true, false)`, then begin and post.
  - 302 to `/acme`, with a `cetra_session` cookie;
  - one `identities` row with provider `org:<org>` and subject `ada@acme.test`;
  - a `viewer` membership;
  - the binder cookie cleared (`Max-Age=0`).
- `the_authorize_redirect_carries_a_valid_authn_request`: Destination = `IDP_SSO_URL`, ACS URL, the 33-character ID, and the cookie attributes `HttpOnly`, `Secure`, `SameSite=None` and `Path=/api/auth/saml/<org>/acs`.
- `the_acs_ignores_a_foreign_origin`: the POST with `Origin: https://evil.test` still signs in (no 403), and with a session cookie of another user present too.
- `a_response_is_accepted_once`: the same POST again → `/login?error=saml_invalid`.
- `a_missing_binder_is_invalid_and_keeps_the_entry`: POST without the cookie → `saml_invalid`, then the same POST with the cookie → signed in.
- `another_browsers_binder_is_invalid`: two begins, response for the first with the second's cookie → `saml_invalid`.
- `a_restart_drops_pending_logins`: begin, then replace `app.state.org_sso` pending table contents via `with_pending(|t| *t = PendingTable::default())`, then post → `saml_invalid`.
- `an_unsolicited_response_restarts_once`:
  - Response with both `InResponseTo` `None` → 302 to `/api/auth/saml/<org>/start` with a `cetra_saml_restart` cookie;
  - the same POST with that cookie → `saml_invalid`;
  - GET `/start` → 302 to the IdP with a binder cookie.
- `an_unknown_request_id_is_invalid_not_a_restart`.
- `a_wrong_audience_is_invalid_and_logged_by_class`: `capture_logs`; the log has `Audience` and no email or entity ID.
- `an_encrypted_assertion_is_unsupported`: replace the Assertion as in M1's hostile corpus → `saml_unsupported`.
- `a_disabled_provider_serves_only_the_owners_test`: provider disabled; anonymous authorize → `sso_unavailable`; owner (`Member` cookie) authorize → post as the owner (`name_id: "owner@acme.test"`) → signed in, `last_login_at` set, no new member.
- `a_test_sign_in_signs_in_only_its_owner`: owner begins, response for another email → `sso_unavailable` (the core's existing rule).
- `the_start_url_needs_an_enabled_provider`.
- `enforcement_refuses_password_login_for_saml_domains`: `configure_org_saml(.., enabled, enforced)` → password login for `x@acme.test` → 403 `sso_required`. This shows that the shared routing applies.
- `the_lapse_signs_in_without_joining`: licence lapsed → sign-in works, no membership (JIT off).
- `jit_needs_the_saml_feature`: licence `["org_sso"]` only, provider configured directly → signs in, no membership.
- `email_must_be_in_a_verified_domain`: response email `ada@other.test` → `domain_not_allowed`.
- `no_email_is_email_not_verified`: NameID format `persistent`, no email attribute → `email_not_verified`.
- `the_acs_is_rate_limited`: 60 POSTs then the 61st → 429.
- `an_oversized_body_is_413`: a body of 256 KiB + 1.
- `the_acs_of_an_org_without_saml_is_404`; `the_sp_metadata_is_served` (content type, `entityID` equals the entity URL); `sp_metadata_of_an_oidc_org_is_404`.
- `an_http_public_url_makes_saml_unavailable`: `org_sso_app_with` licence `["org_sso", "saml"]` and the default http URL, the provider written directly → authorize → `sso_unavailable`.
- `logs_carry_no_saml_payload`: `assert_logs_clean(&["ada@acme.test", IDP_ENTITY_ID, <request id>, <binder>], full_flow)`.
- `start_is_refused_in_restore_mode`.

**Done when:** written and committed.

**Commit:** `Sign in through SAML providers (D-SA-5, D-SA-10, D-SA-12)`

---

## SA2.6 — Daily metadata refresh

**Goal:** providers imported from a metadata URL pick up rotated certificates on their own (D-SA-11).

**Edit:** `crates/server/src/ee/org_sso/saml_refresh.rs` (new), `crates/server/src/ee/org_sso/mod.rs` (one `mod` line), `crates/server/src/serve.rs`.

**Steps:**

1. `saml_refresh.rs` (`ee` header), modelled on `recheck.rs`:
   ```rust
   const DAY_NS: i64 = 86_400_000_000_000;
   const BATCH: u32 = 20;

   #[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
   pub(crate) struct Refresh { pub(crate) refreshed: usize, pub(crate) rotated: usize, pub(crate) failed: usize }

   /// Refreshes up to `BATCH` SAML providers whose metadata URL was last tried a day ago or more.
   pub(crate) async fn refresh_once(state: &AppState, now: i64) -> Result<Refresh, MetaError>;
   ```
   For each `(org, row)` from `store::due_saml_refreshes(reader, now - DAY_NS, BATCH)`:
   1. `state.org_sso.fetch_metadata(url).await`:
      - `Err(Denied)` → `Failed("denied")`;
      - `Err(TooLarge)` → `Failed("too_large")`;
      - `Err(Status)` → `Failed("status")`;
      - `Err(Url | Unavailable)` → `Failed("fetch_failed")`.
   2. `cetra_saml::parse_metadata(&bytes)`: `Err` → `Failed("unusable")`.
   3. `metadata.entity_id() != row.idp_entity_id` → `Failed("entity_id_changed")`.
   4. Otherwise `Fetched { sso_url, certificates: metadata.certificates().iter().map(|c| c.der().to_vec()).collect() }`.
   5. Write `store::record_refresh(conn, row.provider_id, outcome, now)`.
      - On `Ok(true)`: `tracing::warn!(target: "audit", org_id = %org, "org sso saml certificates changed")` and `rotated += 1`.
      - `Fetched` → `refreshed += 1`; `Failed` → `failed += 1` and `tracing::info!(org_id = %org, reason = class, "org sso saml metadata refresh failed")`.

   After the loop, if `rotated > 0` or any `Fetched` changed the SSO URL (compare it with `row.sso_url`), call `super::refresh(state).await`.
2. `mod.rs`: `pub(crate) mod saml_refresh;`.
3. `serve.rs`:
   - `const ORG_SSO_SAML_REFRESH_EVERY: Duration = Duration::from_secs(3600);` next to `ORG_SSO_RECHECK_EVERY`.
   - `fn spawn_org_sso_saml_refresh(state: AppState) -> JoinHandle<()>`, a copy of `spawn_org_sso_recheck`'s shape: interval, `MissedTickBehavior::Skip`, the first tick consumed, `refresh_once`, an `info` with the counts only when something rotated or failed, `warn` on `MetaError`.
   - Push it inside the existing `if !state.config.restore_mode { … }` block, after the recheck task.

**Tests** (in `saml_refresh.rs`; a loopback metadata server as in SA2.4, whose answer the test can change between runs):
- `a_rotated_certificate_is_picked_up`:
  - Save from the URL serving `[Idp]`, then switch the server to `[Idp, IdpNext]`.
  - Advance the manual clock one day plus one second, then `refresh_once` → `rotated == 1`.
  - The view's `certificates_changed_at` is set, and a response signed by `IdpNext` now signs in (reuse SA2.5's helpers via `super::saml::tests`. If they are private, copy the minimal begin and post code).
- `a_changed_entity_id_is_refused_and_kept`: the server serves metadata with another entity ID → `failed == 1`, `refresh_error == "entity_id_changed"`, and the certificates are unchanged.
- `a_failing_url_keeps_the_last_good_metadata`.
- `a_recent_check_is_not_due`: running twice in the same day refreshes once.
- `pasted_metadata_is_never_refreshed`.
- `logs_carry_no_url`.

**Done when:** written and committed.

**Commit:** `Refresh SAML metadata daily (D-SA-11)`

---

## SA2.7 — OpenAPI, examples, golden data, break-glass

**Goal:** the API contract, the contract tests, the golden data and the break-glass test cover SAML.

**Edit:** `crates/server/src/api/openapi.rs`, `crates/server/tests/openapi.rs`, `crates/server/src/golden_tests/build.rs`, `crates/server/src/golden_tests/dump.rs`, `crates/server/src/admin.rs` (test only), `crates/server/src/http/access_suite.rs` (only if a test there fails on the new routes by design; then add the minimal expectation).

**Steps:**

1. `openapi.rs`:
   - Register the new schemas next to the org SSO ones: `SsoProtocol`, `OrgSsoSaml`, `OrgSsoCertificate`, `SamlAttributeNames`, `SamlRefreshError`, `OrgSsoSamlSp`, `PutOrgSamlProvider`.
   - Add to `org_sso_paths` the operation `put_org_saml_provider` on `PUT /api/v1/orgs/{org}/sso/saml`, built like `put_org_sso_provider`: the same tag, parameters and security, `json_body("PutOrgSamlProvider")`, and `responses("200", json_of::<OrgSso>("The org's SSO settings."), &[400, 401, 403, 404, 409, 429, 503])`. The description names D-SA-18's sources, the `confirm_entity_id_change` rule, and the https requirement.
   - Add a public auth group, after `post_sso_discover`, built like `get_sso_authorize`:
     - `get_saml_metadata` on `GET /api/auth/saml/{org_id}`: a path parameter `org_id` (string, "Org UUID"). Success `200` with content type `application/samlmetadata+xml` (a `ResponseBuilder` with `.content("application/samlmetadata+xml", Content::new(Some(string_schema())))`, mirroring `json_named`). Errors `&[404]`.
     - `get_saml_start` on `GET /api/auth/saml/{org_id}/start`: `responses("302", redirect_to("To the org's IdP, or to /login?error=sso_unavailable."), &[429, 503])`.
     - `post_saml_acs` on `POST /api/auth/saml/{org_id}/acs`. Request body via a new helper `form_body(name)` that mirrors `json_body` with content type `application/x-www-form-urlencoded` and the schema `SamlAcsForm` (register a small `ToSchema` struct `SamlAcsForm { SAMLResponse: String, RelayState: Option<String> }` in `api/org_sso.rs`… that file belongs to SA2.4. Instead define the schema inline in `openapi.rs` with `ObjectBuilder` (two string properties, `SAMLResponse` required), registered under the name `SamlAcsForm`). Responses: `responses("302", redirect_to("To the saved page, the start URL for an unsolicited response, or /login?error=<code>."), &[404, 413, 429])`.

     All three use `.securities(Some(Vec::new()))`, or whatever the existing public auth operations use: copy `get_sso_authorize`.
   - The `get_oauth_callback` description's code list gains `saml_invalid` and `saml_unsupported` (D-SA-12). Keep the list's order and append them at the end.
2. `tests/openapi.rs`:
   - Add the four operations to the hand-written table: `("put", "/api/v1/orgs/{org}/sso/saml", "put_org_saml_provider")`, `("get", "/api/auth/saml/{org_id}", "get_saml_metadata")`, `("get", "/api/auth/saml/{org_id}/start", "get_saml_start")`, `("post", "/api/auth/saml/{org_id}/acs", "post_saml_acs")`.
   - Add the three public ones to the public `matches!` list (security `[]`), and `put_org_saml_provider` to the session-only list next to `put_org_sso_provider`.
   - Add their expected 4xx codes to the per-id ladder, matching step 1's lists.
3. `golden_tests/build.rs`:
   - After the org that has the OIDC provider, add a second golden org `beta` (slug `beta`, through the same helper the builder uses for orgs) with a SAML provider.
   - Write it with `store::put_saml_provider`, using fixed values: entity ID `https://idp.beta.test/metadata`, SSO URL `https://idp.beta.test/sso`, the DER of `cetra_saml::test_idp::TestKey::Idp.certificate()`, `metadata_url: Some("https://idp.beta.test/metadata")`, the email attribute `mail`, and `refreshed_at` = the builder's fixed time. Then one verified domain `beta.test`.

   `dump.rs`'s `sso_section`: add the SAML rows and the certificates, as hex SHA-256 rather than the DER, in the dump's existing style (read `sso_section` and follow it). `every_table_has_a_golden_row` then passes for both new tables.
4. `admin.rs` tests: `org_sso_disable_turns_a_saml_provider_off`, the twin of `org_sso_disable_turns_a_provider_and_enforcement_off`, with `store::put_saml_provider` as the seed.
5. `access_suite.rs`: no change is expected. Every principal gets 404 or 302 from the public SAML routes for the world's org, which has no SAML provider. If the suite needs an entry for a `Public` route that answers 404, add it where it lists comparable routes. Read the suite first, and change nothing if it passes by design.

**Done when:** written and committed.

**Commit:** `Document the SAML routes and cover them in the contract and golden tests`

---

## SA2.8 — Specs

**Goal:** the specs describe what M2 built.

**Edit:** `docs/spec/04-metadata-db.md`, `docs/spec/08-auth.md`, `docs/spec/09-api.md`, `docs/spec/11-ui.md`, `docs/spec/12-operations.md`, `docs/spec/README.md`.

**Steps** (line numbers from the code facts §7; the text is new prose in the specs' style: short, present tense, decision IDs in parentheses):
1. **Spec 04**, § "Per-org SSO (track P, `ee`)":
   - The intro says one provider per org, OIDC or SAML (D-SA-2, D-SA-17).
   - Replace the `org_sso_providers` DDL with migration 0031's, and add `org_sso_saml` and `org_sso_saml_certificates`.
   - A sentence: SAML rows have no sealed column, certificates are DER rows, and nothing is JSON.
2. **Spec 08**:
   - D141 paragraph (line 41): add the exemption of `POST /api/auth/saml/{org_id}/acs` (exact path) and why.
   - Line 72: append `saml_invalid`, `saml_unsupported` to the code list.
   - Line 77: replace "SAML: not in MVP." with "SAML: per org only, see § Per-org SSO (D-SA-7)."
   - Merge the duplicate "Lapses" bullets (lines 87–88) into one, keeping the D-P-8/D-P-61 text and adding "and `saml` for a SAML provider (D-SA-4)".
   - After the Per-org SSO bullets, add a subsection **"SAML providers (track SA, `ee`)"** with bullets for:
     - the protocol choice and the switch rules (D-SA-2);
     - the metadata sources and the refresh (D-SA-11);
     - the https rule (D-SA-19);
     - the SP endpoints (D-SA-12);
     - the login: authorize → AuthnRequest → ACS, the pending table and binder cookie, single use, the ACS bucket (D-SA-10);
     - the unsolicited restart and the loop guard (D-SA-5);
     - the validation contract by reference to D-SA-9 and `cetra-saml`;
     - the attributes and JIT (D-SA-6);
     - identities (D-SA-13);
     - no SLO (D-SA-16).
3. **Spec 09**:
   - Auth table: add rows for the three `/api/auth/saml/{org_id}` routes (public; the ACS without Origin; 256 KiB form).
   - Org SSO row (line 73): add `PUT /api/v1/orgs/{org}/sso/saml` with its rules, and the response's `protocol`, `saml` and `saml_sp`.
   - Break-glass row (line 319): "the provider, OIDC or SAML".
4. **Spec 11**, line 243 (Single sign-on settings): one sentence: "SAML providers are configured on the same page (M3 of track SA)." Line 54: add `saml_invalid`, `saml_unsupported` to the login error codes the page shows.
5. **Spec 12**:
   - The `restore_mode` row (line 18) adds "SAML metadata refresh off".
   - The features list (line 107) adds `saml`.
   - The egress sentence (line 163) adds "SAML metadata".
   - Where the org SSO background tasks would be listed (search for "recheck" or the startup/background section; if none exists, add one sentence in the egress paragraph): "SAML metadata from a URL is refreshed hourly in batches, each provider once a day (D-SA-11)".
6. **`docs/spec/README.md`** line 36: remove "SAML" from the out-of-MVP list.

**Done when:** written and committed.

**Commit:** `Describe SAML providers in the specs`

---

## SA2.9 — Generate, check, review (coordinating session)

**Steps:**
1. `just openapi` (regenerates `crates/server/openapi.json` and `web/src/api/schema.gen.ts`), `just api-examples` (manifest), then regenerate the `meta_schema` insta snapshot (`INSTA_UPDATE=always cargo test -p cetra-server meta::tests` or `cargo insta accept`, whichever the repo uses; check `just` recipes first). Commit `Regenerate the API contract and schema snapshot for SAML`.
2. `export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH` (check `ls ~/.local/share/nvm/`), then `just check`. Fix failures through Haiku fix phases with exact briefs, one commit each.
3. A Sonnet code review of M2's diff against this plan, D-SA-2…D-SA-19 and D-P-51…D-P-65, with special attention to D-SA-10 (binder, consume order), the CSRF exemption's exactness, and privacy in logs. Fix its findings, then check the fixes.
4. `docs/roadmap.md`: the SA row's status says M2 implemented. Update the PR #240 body (`gh api -X PATCH repos/cetra-ai/cetra/pulls/240 -f body=…`).
