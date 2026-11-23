# EA M5 — SCIM Users and tokens

**Summary.**
- This milestone lets an org's identity provider provision and deprovision the org's members over SCIM 2.0 (RFC 7643 and 7644), with every member at the org's default role. Groups and role mappings are M6.
- It adds:
  - the `scim` feature;
  - per-org `scim-cetra-…` bearer tokens and the Provisioning settings page;
  - the `/scim/v2` discovery documents and the full Users resource;
  - the verified-domain rule (D-EA-13) and deprovisioning (D-EA-16);
  - the `scim_managed` locks (D-EA-18) and the lapse rules (D-EA-19);
  - audit records for every SCIM operation;
  - on the cloud, the org token's `sso_domains`.

**Prerequisites.**
- M1 is green.
- M2 is fully committed and green (all of B1–B6 and its fix phase). M5 edits `audit/`, `routes/members.rs`, `routes/invites.rs`, `routes/datasets.rs`, `routes/annotation_queues.rs` and other files M1 and M2 touched, so it starts from a clean tree.
- Branch `claude/ea-enterprise-admin` in `/home/sebastien/travaux/llm-observer/.claude/worktrees/ea-design`.
- The next free migration number when C1a starts: check `crates/server/migrations/meta/`. It is `0032` unless an M2 phase added one; use the next free number and say so.

**Read first.**
- `docs/plans/ea-enterprise-admin/README.md` § SCIM 2.0 (all of it), D-EA-12 to D-EA-19.
- `AGENTS.md` § Rules, § Naming, § Tests.
- The M1 plan's shared rules (`01-m1-audit-recording.md`) and `crates/server/src/audit/` (the API).
- The facts file `/home/sebastien/.config/Claude-extra/side-session-notes/local_1c672712-2cc7-40e9-8d6b-d582417356b7/ea-m5-code-facts.md`. Phases cite its sections (§1–§8); lines may have moved, so locate by name.
- RFC 7644 § 3.4.2 (filtering), § 3.5.2 (PATCH), § 3.12 (errors); RFC 7643 § 4.1 (User). The plan restates everything the code needs.

## What it is, and what it is not

**What works when it's done:**
- An owner enables SCIM on Org settings → Provisioning and creates a token (at most two live). They paste `<public_url>/scim/v2` and the token into Okta or Entra ID.
- The IdP creates, reads, lists, filters, updates, deactivates, reactivates and deletes users.
  - Users must have an email of a verified domain of the org.
  - Deactivation removes the membership, revokes the user's sessions and PATs, and disables the account when no membership is left.
- While SCIM is on (and entitled):
  - org SSO JIT is off;
  - invitations to verified-domain emails are refused, and so is accepting one;
  - a managed member's role, overrides and removal are refused by hand with `409 scim_managed`.
- A lapsed org keeps accepting deactivations and deletes, and refuses creates and reactivations. The locks lift.
- Every SCIM response and error is SCIM-shaped (`application/scim+json`).

**Out of scope:**
- Groups, role mappings and recomputing roles from groups: M6. Every provisioned member gets `default_role`.
- Okta and Entra validator fixtures, and the setup guides: M6.
- The control plane's side of `sso_domains`: track Q.

## Decisions this plan relies on

| ID | Summary |
|---|---|
| D-EA-12 | `/scim/v2`, token names the org, per-org resource ids, `eq` filters only, others `400 invalidFilter`, no Bulk/sort/ETag |
| D-EA-13 | Only verified-domain emails; link with D-P-66; new accounts `email_verified_via = 'idp'`; email change only when both emails are in the org's domains and the user has no other org |
| D-EA-15 | `scim-cetra-…` tokens, two live at most, owners with a session, SHA-256, `Principal::Scim`, 600/min per token |
| D-EA-16 | Deactivation removes membership and overrides, revokes sessions and PATs, disables the account when no membership is left; last owner refused; instance admins only lose the membership |
| D-EA-17 | Cloud: the org token's `sso_domains` count as verified domains for SCIM |
| D-EA-18 | Managed = linked by an active SCIM row; JIT off; verified-domain invites refused; manual changes refused; disabling or a lapse lifts the locks |
| D-EA-19 | Lapse: removals accepted, creates, reactivations and raises refused |
| D-P-54, D-P-66, D-P-58 | Domains; email proof and the unproven-account reset; the instance-admin exemption |
| D176, D-P-20 | Seats (viewers are free) |

**Plan-level choices:**
- **P-1. Verified domains.** On every instance, the org's SCIM domains are its verified `org_sso_domains` rows (read in the write transaction with `store::domains_tx`, filtered on `verified_at`), minus domains the instance configuration claims (`instance_claims`, D-P-54).
  - When `entitlements.control_plane` is true, the org token's `sso_domains` are added (a union).
  - The in-memory `OrgSso` index is **not** used: it skips orgs without a provider.
  - Helpers: `instance_claims(&AuthSettings, domain) -> bool` (`ee/org_sso/mod.rs:~319`, call it with `&state.config.auth`); `normalize_domain(raw) -> Option<String>` (`ee/org_sso/mod.rs:~288`).
- **P-2. One error rewrite.** A middleware layer on the router turns every `application/problem+json` response to a `/scim/v2` path into a SCIM error with the same status. So 401, 403, 404, 405, 413, 415 and 429 from `authenticate`, `enforce`, extractors and fallbacks need no per-site code. SCIM handlers return `ScimError` directly for SCIM-specific `scimType`s.
- **P-3. Shared code, no second implementation.**
  - `meta::users::revoke_access_tx` (sessions and PATs) replaces the private body of `routes/internal_users.rs` `revoke_access`.
  - `meta::users::reset_unproven_tx` (the D-P-66 reset) replaces the inline block in `ee/org_sso/login.rs` `admit`.
  - Both old call sites use the new functions in the same phase.
- **P-4. `default_role`.** `org_scim.default_role` is `viewer` or `member` (D-EA-14's `scim_default_role`), `viewer` by default.
- **P-6. Instance admins.** SCIM never touches an instance admin's credentials (D-P-58, and org SSO's `LinkRefused`). Creating one links the membership only (no `reset_unproven_tx`), and deactivating one only removes the membership and marks the row inactive.
- **P-7. Inactive means no access.** A SCIM row with `active = false` never has a membership in the org. A `POST` or `PUT` with `active: false` for a user who is a member runs the deactivation (C3 step 10).
- **P-8. State in closures.** Clone `AppState` (`let state = state.clone();`) and the ctx before a `meta.write` closure that needs them; the closure cannot borrow.
- **P-5. Owners and admins.** In M5, SCIM never changes an existing member's role. A provisioned user who is already a member keeps their role; only new memberships get `default_role`. M6 adds recomputation from groups.

## Versions

No new dependency.

## Phase graph

```
C1a (store, feature, catalog, key kind, module skeleton) ──┬── C1b (principal, access, limits, caches)
                                                           ├── C1c (shared lifecycle helpers)
                                                           └── C2 (SCIM protocol: errors, filter, patch, schema)
C1a + C1b + C1c + C2 ── C3 (Users and discovery endpoints)
C1a + C1b + C1c ─────── C4 (Provisioning API, locks, JIT off)
C1a ─────────────────── C5 (cloud sso_domains)
C4 ──────────────────── C6 (web)
C3 + C4 + C5 ────────── C7 (docs)
```

- In parallel: C1b with C1c and C2; then C3 with C4 and C5 (C5 after C1b, which also edits `test_support/mod.rs`); then C6 with C7.
- C1a writes `ee/scim/mod.rs` in its final form (every `mod` line, `routes()` and `locks()`). Later phases only create their files, so no two phases edit `mod.rs`.
- `crates/server/openapi.json` and `web/src/api/schema.gen.ts` are generated, by the coordinating session only, after C4 (`just openapi`).
- `fixtures/api` regeneration (`just api-examples`) is the coordinator's too.

**Shared rules** (in every brief):
- M1's shared rules apply: no compile, test, lint or format; commit only your files; remove bindings that only fed deleted lines; clone ctx into closures; `ee/` files start with `// SPDX-License-Identifier: Elastic-2.0`.
- SCIM code lives under `crates/server/src/ee/scim/`.
- Never log or record an email, a name, an external id, a token value or a request body. Log ids and counts.

---

### C1a — Tables, the `scim` feature, the audit actions and the key kind

**Goal:** the SCIM tables and their store functions, the feature, the token kind and the audit actions.

**Edit:**
- `crates/server/migrations/meta/00NN_scim.sql` (new; NN = next free number)
- `crates/server/src/ee/mod.rs` (`pub(crate) mod scim;`)
- `crates/server/src/ee/scim/mod.rs` (new, final form: step 7)
- `crates/server/src/ee/scim/store.rs` (new)
- `crates/server/src/api/entitlements.rs`
- `crates/server/src/entitlements/mod.rs` (tests only)
- `crates/server/src/auth/secret.rs`
- `crates/server/src/routes/api_keys.rs` (its exhaustive `KeyKind` matches, ~265 and ~273, only)
- `crates/server/src/api/common.rs` (`ProblemCode::ScimManaged`)
- `crates/server/src/http/error.rs` (the `ApiError` variant and its mapping)
- `crates/server/src/audit/catalog/scim.rs` (new), `crates/server/src/audit/catalog/mod.rs`, `crates/server/src/audit/mod.rs` (`Actor::Scim` and its `kind`/`id` arms, `TargetKind::ScimToken`; not `Actor::of`, which is C1b's), `crates/server/src/audit/tests.rs`

**Steps:**

1. Migration:

   ```sql
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
   ```

   `user_name` holds the normalized email (`users::normalize_email`). The SCIM row is per org; the `users` row is instance-wide.
2. `ee/scim/store.rs` (SPDX header; `#![cfg_attr(not(test), allow(dead_code))]` until C3/C4 call everything). Types and functions, all on `&mut SqliteConnection` unless noted:

   ```rust
   pub(crate) struct OrgScim { pub(crate) enabled: bool, pub(crate) default_role: OrgRole, pub(crate) updated_at: i64 }
   pub(crate) struct ScimTokenRow { pub(crate) id: Uuid, pub(crate) org_id: OrgId, pub(crate) name: String, pub(crate) prefix: String, pub(crate) created_by: Option<Uuid>, pub(crate) created_at: i64, pub(crate) last_used_at: Option<i64>, pub(crate) revoked_at: Option<i64> }
   pub(crate) struct ScimUserRow { pub(crate) id: Uuid, pub(crate) org_id: OrgId, pub(crate) user_id: Uuid, pub(crate) user_name: String, pub(crate) external_id: Option<String>, pub(crate) given_name: Option<String>, pub(crate) family_name: Option<String>, pub(crate) display_name: Option<String>, pub(crate) department: Option<String>, pub(crate) employee_number: Option<String>, pub(crate) active: bool, pub(crate) disabled_account: bool, pub(crate) created_at: i64, pub(crate) updated_at: i64 }
   pub(crate) struct ScimUserFields { pub(crate) user_name: String, pub(crate) external_id: Option<String>, pub(crate) given_name: Option<String>, pub(crate) family_name: Option<String>, pub(crate) display_name: Option<String>, pub(crate) department: Option<String>, pub(crate) employee_number: Option<String> }

   /// Max live tokens per org (D-EA-15).
   pub(crate) const MAX_LIVE_TOKENS: u64 = 2;

   pub(crate) async fn org_scim(pool: &SqlitePool, org: OrgId) -> Result<OrgScim, MetaError>; // a missing row = disabled, viewer, updated_at 0
   pub(crate) async fn org_scim_tx(conn, org) -> Result<OrgScim, MetaError>;
   pub(crate) async fn put_org_scim(conn, org, enabled: bool, default_role: OrgRole, now) -> Result<(), MetaError>; // upsert
   pub(crate) async fn insert_token(conn, org, name: &str, prefix: &str, hash: &str, created_by: Uuid, now) -> Result<ScimTokenRow, MetaError>;
       // counts live tokens first: >= MAX_LIVE_TOKENS -> MetaError::Conflict("at most two live SCIM tokens")
   pub(crate) async fn list_tokens(pool, org) -> Result<Vec<ScimTokenRow>, MetaError>; // live only, newest first
   pub(crate) async fn revoke_token(conn, org, id: Uuid, now) -> Result<Option<String>, MetaError>; // returns the hash for cache eviction
   pub(crate) async fn token_by_hash(pool, hash) -> Result<Option<(ScimTokenRow, bool /* org active */)>, MetaError>; // JOIN orgs.state = 'active'
   pub(crate) async fn set_tokens_last_used(conn, &[(Uuid, i64)]) -> Result<(), MetaError>; // same UPDATE shape as personal_tokens::set_last_used
   pub(crate) async fn insert_user(conn, org, user_id: Uuid, fields: &ScimUserFields, now) -> Result<ScimUserRow, MetaError>; // unique violation -> MetaError::Conflict("uniqueness")
   pub(crate) async fn user_tx(conn, org, id: Uuid) -> Result<Option<ScimUserRow>, MetaError>;
   pub(crate) async fn user(pool, org, id) -> Result<Option<ScimUserRow>, MetaError>;
   pub(crate) async fn user_by_user_name_tx(conn, org, user_name: &str) -> Result<Option<ScimUserRow>, MetaError>;
   pub(crate) async fn user_by_external_id_tx(conn, org, external_id: &str) -> Result<Option<ScimUserRow>, MetaError>;
   pub(crate) async fn user_of_member_tx(conn, org, user_id: Uuid) -> Result<Option<ScimUserRow>, MetaError>;
   pub(crate) async fn list_users(pool, org, filter: Option<&UserFilter>, start_index: u32, count: u32) -> Result<(u64 /* total */, Vec<ScimUserRow>), MetaError>; // ORDER BY created_at, id; OFFSET start_index-1
   pub(crate) async fn update_user(conn, org, id, fields: &ScimUserFields, now) -> Result<(), MetaError>;
   pub(crate) async fn set_user_active(conn, org, id, active: bool, disabled_account: bool, now) -> Result<(), MetaError>;
   pub(crate) async fn delete_user(conn, org, id) -> Result<bool, MetaError>;
   pub(crate) async fn is_managed_tx(conn, org, user_id) -> Result<bool, MetaError>; // EXISTS active row
   pub(crate) async fn managed_user_ids(pool, org) -> Result<std::collections::HashSet<Uuid>, MetaError>; // active rows' user ids

   /// What a list filter matches (C2 parses it).
   pub(crate) enum UserFilter { UserName(String), ExternalId(String), Email(String) }
   ```

   - `Email` matches `user_name` (the email).
   - `UserName` and `Email` values are normalized with `users::normalize_email` by the caller. An invalid one matches nothing.
   - `OrgRole` parse and format use `roles::parse_org_role` and `org_role_str`. A stored `default_role` that is not `viewer` or `member` is `MetaError::Corrupt { table: "org_scim", column: "default_role" }`.
3. `api/entitlements.rs`: add `Scim` at the end of `Feature`:
   - doc: "SCIM 2.0 provisioning (track EA M5): Enterprise or an enterprise licence.";
   - append to `ALL` and bump its length (`[Self; 5]` now; M3 appends `audit_log` after it), name `"scim"`, `is_ee` true;
   - `locked_detail`: "SCIM provisioning is part of the Enterprise plan and the self-hosted licence.".

   In `entitlements/mod.rs` tests, add `scim` to the round-trip test's known names. The `audit_logs` unknown-name assertion stays: M3 adds `audit_log`.
4. `auth/secret.rs`:
   - `KeyKind::Scim` with tag `scim` (`scim-cetra-` + 32 base62);
   - `split_tail` tries `("scim-", Scim)` first;
   - tail length 32; `display_prefix` covers it;
   - tests: `scim_keys_round_trip_and_parse`, and `a_scim_key_is_not_an_api_key` (`parse_key` returns `Scim`).

   Fix every exhaustive `match` on `KeyKind` in the crate. Search `KeyKind::Personal`: an arm that stores or serves API keys answers as for `Personal`, unreachable for `Scim`. Return the same error the `Personal` arm returns, or use the existing fallback, and list each site in the report.
5. `api/common.rs`: `ProblemCode::ScimManaged` (doc: "The member is managed by the org's identity provider (D-EA-18)."), serialized `scim_managed`. `http/error.rs`: `ApiError::ScimManaged`, 409, code `ScimManaged`, title "Managed by the identity provider", detail "This member is managed by your identity provider. Change it there.".
6. Audit:
   - `audit/catalog/scim.rs` `ScimAction` (group `scim`):

     | Variant | Name | Fields |
     |---|---|---|
     | `Enabled` | `scim.enabled` | |
     | `Disabled` | `scim.disabled` | |
     | `DefaultRoleChanged { old_role: &'static str, new_role: &'static str }` | `scim.default_role_changed` | |
     | `TokenCreated` | `scim.token_created` | target `scim_token` |
     | `TokenRevoked` | `scim.token_revoked` | target `scim_token` |
     | `UserProvisioned { scim_user_id: Uuid, role: &'static str, linked: bool }` | `scim.user_provisioned` | target `user`; `linked`: an existing account |
     | `UserUpdated { scim_user_id: Uuid, fields: Vec<&'static str> }` | `scim.user_updated` | attribute names only |
     | `UserDeactivated { scim_user_id: Uuid, account_disabled: bool }` | `scim.user_deactivated` | |
     | `UserReactivated { scim_user_id: Uuid }` | `scim.user_reactivated` | |
     | `UserDeleted { scim_user_id: Uuid }` | `scim.user_deleted` | |

     `scim.role_raise_refused` is M6's: it is added there, with its first caller.
   - `catalog/mod.rs`: module, re-export, `Action::Scim` and `From`.
   - `audit/mod.rs`:
     - `Actor::Scim { token_id: Uuid }`: kind `scim_token`, id the token id;
     - `TargetKind::ScimToken` → `scim_token`.
   - `audit/tests.rs`: samples of every new variant; the `scim` prefix in the group map.

7. `ee/scim/mod.rs` (SPDX header), final form:

   ```rust
   //! SCIM 2.0 provisioning (track EA M5, D-EA-12 to D-EA-19).
   pub(crate) mod domains;
   pub(crate) mod error;
   pub(crate) mod filter;
   pub(crate) mod patch;
   pub(crate) mod schema;
   pub(crate) mod settings;
   pub(crate) mod store;
   pub(crate) mod users;

   /// The `/scim/v2` routes and the Provisioning API routes.
   pub(crate) fn routes() -> Vec<crate::http::access::RouteDef> {
       let mut routes = users::routes();
       routes.extend(settings::routes());
       routes
   }

   /// D-EA-18: SCIM is on and entitled for `org`. Read outside a write; pass the bool in.
   pub(crate) async fn locks(state: &AppState, org: OrgId) -> Result<bool, MetaError> {
       Ok(store::org_scim(state.meta.reader(), org).await?.enabled
           && crate::entitlements::of(state, org).allows(Feature::Scim))
   }
   ```

   `domains`, `error`, `filter`, `patch`, `schema`, `settings` and `users` are created by C2, C3 and C4. The tree compiles once they are committed.

**Tests** (in the files above):
- store: `org_scim_defaults_when_missing`, `a_third_live_token_is_refused`, `revoking_returns_the_hash_once`, `token_lookup_needs_an_active_org`, `user_name_and_external_id_are_unique_per_org`, `list_users_pages_and_filters`, `is_managed_only_for_active_rows`, `org_delete_cascades`.
- secret: as step 4.

**Done when:** committed.

**Commit:** `Add SCIM tables, the scim feature and its audit actions`

---

### C1b — The SCIM principal, access, limits and caches

**Goal:** a `scim-cetra-` bearer resolves to `Principal::Scim`, which only `Access::Scim` routes accept.

**Edit:** `crates/server/src/audit/mod.rs` (`Actor::of`'s `Principal::Scim` arm only), `crates/server/src/auth/principal.rs`, `crates/server/src/auth/cache.rs` (only if a type alias is needed), `crates/server/src/auth/last_used.rs`, `crates/server/src/auth/limits.rs`, `crates/server/src/http/access.rs`, `crates/server/src/http/access_suite.rs`, `crates/server/src/http/mod.rs` (`AppInner` fields and the fallback), `crates/server/src/serve.rs` (`AppInner` construction, last-used flush), `crates/server/src/test_support/mod.rs` (`AppInner` construction, `license_key` callers untouched), and the one-arm matches in `crates/server/src/routes/annotation_queues.rs`, `routes/datasets.rs`, `routes/deletions.rs`, `routes/me.rs`, `routes/saved_views.rs`, `routes/scores.rs`, `routes/streams.rs` (facts §1 list).

**Steps:**
1. **`Principal::Scim { org_id: OrgId, token_id: Uuid }`.**
   - The hand-written `Debug` prints both ids.
   - `session_user` answers `Forbidden` for it.
   - `audit::Actor::of` maps it to `Actor::Scim { token_id }`.
   - Each listed exhaustive match gets a `Principal::Scim { .. }` arm that answers like its `ControlPlane` arm. Copy that arm's result exactly, whether `NotFound`, `Forbidden` or `None`.
2. **`bearer_principal`:**
   - after `parse_key`, when `kind == KeyKind::Scim`, return `scim_principal(state, &hash, now)`;
   - `scim_principal` uses a new `scim_cache: KeyCache<CachedScim { id: Uuid, org_id: OrgId }>` (60 s, as `pat_cache`). On a miss it calls `ee::scim::store::token_by_hash(state.meta.reader(), hash)`;
   - a missing, revoked or inactive-org token is `Unauthorized`;
   - it records `state.scim_last_used.record(id, now)`.
3. **`auth/last_used.rs`:** `Table::ScimTokens`, `LastUsed::scim_tokens()`, with flush via `ee::scim::store::set_tokens_last_used`. **`http/mod.rs`** `AppInner`: `scim_cache` and `scim_last_used`, built in `serve.rs` and `test_support/mod.rs`. `serve.rs` flushes `scim_last_used` wherever it flushes `token_last_used` (the 60 s task and shutdown).
4. **`auth/limits.rs`:** a field `scim: KeyedLimiter<Uuid>` with `Bucket { capacity: 600, per: Duration::from_secs(60) }` and `MAX_KEYS`, built in `Limits::new` and pruned in `prune`.
5. **`http/access.rs`:**
   - `Access::Scim`. In `prepare`: `Access::Scim` accepts only `Principal::Scim`, otherwise `Forbidden`.
   - `Access::User` and `Access::Pat` refuse `Principal::Scim` with `Forbidden`. `Access::Org` and `Access::Project` refuse it with `NotFound`. `Key`, `InstanceAdmin` and `ControlPlane` refuse it as they refuse any other principal.
   - `route_table_matches_parameters_and_openapi_security`: the exhaustive match gets `Access::Scim` with the `Key`/`ControlPlane` arm (bearer). SCIM routes are undocumented, so it is not reached.
6. **`http/mod.rs` `fallback`:** treat `/scim/v2` and `/scim/v2/*` as API paths (a 404, not the UI). The 404's problem body is turned into a SCIM error by C2's layer.
7. **`http/access_suite.rs`:**
   - `checked_routes` also keeps `Access::Scim` routes;
   - `Who::Scim` (a token of the world's org, minted in `build()` with `ee::scim::store::insert_token` and `secret::new_key(KeyKind::Scim)`, its secret added to `needles`);
   - `expected`: `Access::Scim` → allowed only for `Who::Scim`, else 403. `Who::Scim` on any other access: `User` 403, `Org` 404, `Project` 404, `Key` 403, `Pat` 403, `InstanceAdmin` 403, `ControlPlane` 403;
   - `assert_every_route_declares_access` lists `Access::Scim`;
   - the world's licence (`license_key(&["project_roles"])`) adds `"scim"` and `"org_sso"`.

**Tests:**
- `auth/principal.rs`: `a_scim_token_resolves_to_its_org`, `a_revoked_scim_token_is_refused_after_eviction`, `a_scim_token_of_a_dropped_org_is_refused`.
- `http/access.rs`: `scim_access_refuses_other_principals`.
- `auth/limits.rs`: `scim_bucket_allows_600_then_refuses`.
- The access suite covers the rest.

**Done when:** committed.

**Commit:** `Resolve SCIM tokens to a principal of their org`

---

### C1c — Shared lifecycle helpers

**Goal:** one implementation each of "revoke a user's access" and "reset an unproven account", used by the control plane, org SSO and SCIM.

**Edit:** `crates/server/src/meta/users.rs`, `crates/server/src/routes/internal_users.rs`, `crates/server/src/ee/org_sso/login.rs`.

**Steps:**
1. `meta/users.rs`:

   ```rust
   /// Deletes the user's web sessions and revokes their PATs; returns the revoked PAT hashes,
   /// which the caller evicts from `pat_cache` after commit (D-EA-16, D-P-39).
   pub(crate) async fn revoke_access_tx(conn: &mut SqliteConnection, user: Uuid, now: i64) -> Result<Vec<String>, MetaError>;

   /// D-P-66: an account whose email is not proven loses its sessions, PATs and password
   /// before an IdP links it, and its email becomes proven via `idp`.
   /// Returns the revoked PAT hashes. A proven account is untouched (empty vec).
   pub(crate) async fn reset_unproven_tx(conn: &mut SqliteConnection, user: &UserRow, now: i64) -> Result<Vec<String>, MetaError>;
   ```

   - `revoke_access_tx` is `web_sessions::delete_user_sessions(conn, user, None)` then `personal_tokens::revoke_user_tokens(conn, user, now)`.
   - `reset_unproven_tx` holds exactly the body of the block at `ee/org_sso/login.rs:~505-512`: sessions, tokens, `set_password_hash(None)`, `set_email_verified(.., EmailVerifiedVia::Idp)`. Nothing more.
   - Also add:

     ```rust
     /// Changes a user's email (SCIM userName change, D-EA-13). A taken address is
     /// `MetaError::Conflict("uniqueness")`. Identities keep the email they were linked with.
     pub(crate) async fn set_email_tx(conn: &mut SqliteConnection, user: Uuid, email: &str, now: i64) -> Result<(), MetaError>;
     ```

     It runs `UPDATE users SET email = ?, updated_at = ? WHERE id = ?`, mapping a unique violation the way `insert_user` does, with the message `"uniqueness"`. The email must be normalized by the caller (`debug_assert` as `insert_user` does).
2. `routes/internal_users.rs` `revoke_access`: replace its two calls with `users::revoke_access_tx`. The disable logic and the audit record stay.
3. `ee/org_sso/login.rs` `admit`: replace the inline block with `users::reset_unproven_tx(conn, &user, now)`.

**Tests:** `meta/users.rs`: `set_email_changes_and_refuses_a_taken_address`, `revoke_access_removes_sessions_and_tokens`, `reset_unproven_leaves_a_proven_account`, `reset_unproven_clears_password_sessions_and_tokens`. The existing org SSO and control-plane tests keep passing.

**Done when:** committed.

**Commit:** `Share the access revocation and unproven-account reset`

---

### C2 — SCIM protocol pieces

**Goal:** SCIM errors and responses, the error rewrite layer, the filter parser, the PATCH normalizer and the resource representations. These are pure code with unit tests.

**Edit:** `crates/server/src/ee/scim/error.rs`, `filter.rs`, `patch.rs`, `schema.rs` (all new). `mod.rs` is C1a's; `UserFilter` and `ScimUserRow` are in C1a's `store.rs`. Each new file starts with `#![cfg_attr(not(test), allow(dead_code))]` (C3 removes the attribute from the four files once it calls them).

**Steps:**

1. **`error.rs`:**

   ```rust
   pub(crate) const ERROR_SCHEMA: &str = "urn:ietf:params:scim:api:messages:2.0:Error";
   pub(crate) const CONTENT_TYPE: &str = "application/scim+json";

   #[derive(Debug)]
   pub(crate) struct ScimError { pub(crate) status: StatusCode, pub(crate) scim_type: Option<&'static str>, pub(crate) detail: &'static str, pub(crate) retry_after_s: Option<u64> }
   impl ScimError {
       pub(crate) fn invalid_filter() -> Self;   // 400, "invalidFilter", "Only eq filters on userName, externalId and emails are supported."
       pub(crate) fn invalid_value(detail: &'static str) -> Self; // 400, "invalidValue"
       pub(crate) fn invalid_syntax() -> Self;   // 400, "invalidSyntax", "The request body is not a valid SCIM resource."
       pub(crate) fn uniqueness() -> Self;       // 409, "uniqueness", "A user with this userName or externalId already exists."
       pub(crate) fn mutability(detail: &'static str) -> Self; // 400, "mutability" (RFC 7644 § 3.12)
       pub(crate) fn not_found() -> Self;        // 404, None, "Resource not found."
       pub(crate) fn conflict(detail: &'static str) -> Self; // 409, None
       pub(crate) fn forbidden(detail: &'static str) -> Self; // 403, None
       pub(crate) fn internal() -> Self;         // 500, None, "Internal error."
       pub(crate) fn rate_limited(retry_after_s: u64) -> Self; // 429, None, "Too many requests.", with Retry-After
   }
   impl IntoResponse for ScimError { /* body below, header Content-Type: application/scim+json, and Retry-After when retry_after_s is Some */ }
   impl From<MetaError> for ScimError { /* Conflict("uniqueness") -> uniqueness(); Conflict("an org needs an owner") -> conflict("An org needs an owner."); other Conflict -> conflict("Conflict."); NotFound -> not_found(); else internal() with tracing::error!(error = %error, "scim request failed") */ }
   impl From<ApiError> for ScimError { /* PlanRequired -> forbidden("SCIM provisioning needs the Enterprise plan or licence."); PlanLimit -> forbidden("The org has no seat left."); RateLimited { retry_after_s } -> rate_limited(retry_after_s); NotFound -> not_found(); others -> by status with detail "Request refused." */ }
   ```

   Body, with `status` a **string**, as RFC 7644 § 3.12 requires:

   ```json
   {"schemas":["urn:ietf:params:scim:api:messages:2.0:Error"],"status":"409","scimType":"uniqueness","detail":"…"}
   ```

   `scimType` is omitted when `None`.

   `pub(crate) async fn scim_errors(request: Request, next: Next) -> Response` is the layer:
   1. When the request path is `/scim/v2` or starts with `/scim/v2/`, run `next`.
   2. If the response's `Content-Type` is `application/problem+json`, replace it with the SCIM error body for the same status, with a fixed detail per status class: 401 "Invalid or missing bearer token.", 403 "Request refused.", 404 "Resource not found.", 405 "Method not allowed.", 413 "Request body too large.", 415 "Use Content-Type application/scim+json.", 429 "Too many requests.", other "Request failed.".
   3. Keep the `Retry-After` header.
   4. Never copy the problem's detail, which can name internals.

   Other paths pass through untouched. C3 installs the layer (in `http/mod.rs` `finish`, outermost after `log_request`).

   `pub(crate) struct ScimJson<T>(pub T)` is the response wrapper: serializes `T` with `serde_json` and sets `Content-Type: application/scim+json`. Request bodies use the existing `error::Json<T>`, which accepts `application/scim+json` (axum 0.8 accepts `+json`); its problem rejection is rewritten by the layer.
2. **`filter.rs`.** Grammar (case-insensitive attribute and operator names, RFC 7644 § 3.4.2.2):

   ```
   filter    = attrPath SP "eq" SP compValue
   attrPath  = "userName" | "externalId" | "emails.value" | "emails[type eq \"work\"].value"
   compValue = DQUOTE *char DQUOTE      ; JSON string escaping: \" \\ \/ \b \f \n \r \t \uXXXX
   ```

   ```rust
   pub(crate) fn parse_user_filter(input: &str) -> Result<UserFilter, ScimError>;   // any other attribute, operator, "and"/"or"/"not", presence, unquoted value -> invalid_filter()
   pub(crate) fn parse_group_filter(input: &str) -> Result<GroupFilter, ScimError>; // M6; do NOT add now
   ```

   - Surrounding whitespace is trimmed, and exactly one space separates tokens, but extra spaces are accepted.
   - `emails.value` and `emails[type eq "work"].value` both give `UserFilter::Email`.
   - Values longer than 320 characters are `invalid_filter()`.

   Tests:
   - `parses_user_name_eq`, `parses_external_id_eq`, `parses_emails_value_and_work_filter`;
   - `attribute_names_are_case_insensitive`, `escaped_quotes_in_values`;
   - `refuses_other_operators` (`co`, `sw`, `pr`, `gt`), `refuses_and_or_not`, `refuses_unknown_attributes`, `refuses_unquoted_values`, `refuses_trailing_garbage`.
   - `the_parser_never_panics`: 2,000 random strings of 0 to 80 characters drawn from the characters of `userName eq "x" []().\` with `rand` and a fixed seed (`StdRng::seed_from_u64(7)`, using the API of the workspace's `rand` version), each parsed without a panic.
3. **`patch.rs`.** Turns a PATCH body into a list of normalized changes:

   ```rust
   pub(crate) const PATCH_SCHEMA: &str = "urn:ietf:params:scim:api:messages:2.0:PatchOp";
   pub(crate) const ENTERPRISE_USER: &str = "urn:ietf:params:scim:schemas:extension:enterprise:2.0:User";

   #[derive(Debug, Clone, PartialEq)]
   pub(crate) enum UserChange {
       Active(bool), UserName(String), ExternalId(Option<String>),
       GivenName(Option<String>), FamilyName(Option<String>), DisplayName(Option<String>),
       Department(Option<String>), EmployeeNumber(Option<String>),
       /// A `work` or primary email; equals the userName in cetra.
       Email(String),
   }
   pub(crate) fn user_changes(body: &serde_json::Value) -> Result<Vec<UserChange>, ScimError>;
   ```

   - `body.schemas` must contain `PATCH_SCHEMA`, else `invalid_syntax()`. `body.Operations` (also accepted as `operations`) is a non-empty array of at most 100 ops.
   - `op` is matched case-insensitively. `add` and `replace` set; `remove` clears an optional attribute (`ExternalId(None)`, …). `remove` of `active` or `userName` is `invalid_value("…")`.
   - **With a `path`:** strip a leading `ENTERPRISE_USER + ":"` (so `…:department` → `department`). Paths, case-insensitive:
     - `active`, `userName`, `externalId`, `displayName`;
     - `name.givenName`, `name.familyName`;
     - `department`, `employeeNumber`;
     - `emails[type eq "work"].value` and `emails.value`.

     Anything else, such as `title` or `phoneNumbers`, is accepted as a no-op (D-EA-12: understood ops on unsupported attributes are no-ops).
   - **Without a `path`:** `value` must be an object. Each key is handled as a path, and `name` as an object (`{"name":{"givenName":"A"}}`). A key equal to `ENTERPRISE_USER` holding an object is expanded, and dotted keys (`"name.givenName"`) work.
   - **Values:**
     - `active` accepts `true`, `false`, `"true"`, `"false"`, `"True"` and `"False"`; anything else is `invalid_value("active must be a boolean")`.
     - String attributes accept a string, or `null` to clear.
     - `emails` as a value is an array of objects. The first with `type == "work"`, else `primary == true`, else the first, gives `Email(value)`.

   Tests:
   - `okta_deactivate` (`{"op":"replace","value":{"active":false}}`);
   - `entra_deactivate_with_string_false` (`{"op":"Replace","path":"active","value":"False"}`);
   - `entra_enterprise_department_path`, `name_object_and_dotted_keys`, `emails_work_filter_path`;
   - `unsupported_attribute_is_a_noop`, `missing_schema_is_invalid_syntax`, `remove_active_is_invalid`, `more_than_100_ops_is_invalid`;
   - `capitalised_operations_key`.
4. **`schema.rs`.** Serialization of a user and the discovery documents:

   ```rust
   pub(crate) const USER_SCHEMA: &str = "urn:ietf:params:scim:schemas:core:2.0:User";
   pub(crate) const LIST_SCHEMA: &str = "urn:ietf:params:scim:api:messages:2.0:ListResponse";
   pub(crate) fn user_resource(row: &ScimUserRow, base_url: &str) -> serde_json::Value;
   pub(crate) fn list_response(total: u64, start_index: u32, resources: Vec<serde_json::Value>) -> serde_json::Value; // {schemas:[LIST_SCHEMA], totalResults, startIndex, itemsPerPage: resources.len(), Resources}
   pub(crate) fn service_provider_config(base_url: &str) -> serde_json::Value;
   pub(crate) fn resource_types(base_url: &str) -> serde_json::Value;   // a ListResponse of the User type only in M5
   pub(crate) fn schemas() -> serde_json::Value;                        // a ListResponse of the core User and enterprise User schemas, attributes as supported
   ```

   `user_resource` gives:

   ```json
   {"schemas":[USER_SCHEMA, ENTERPRISE_USER],"id":"<scim id>","externalId":"…","userName":"…",
    "name":{"givenName":"…","familyName":"…"},"displayName":"…","active":true,
    "emails":[{"value":"<userName>","type":"work","primary":true}],
    "urn:ietf:params:scim:schemas:extension:enterprise:2.0:User":{"department":"…","employeeNumber":"…"},
    "meta":{"resourceType":"User","created":"<RFC 3339>","lastModified":"<RFC 3339>","location":"<base>/Users/<id>"}}
   ```

   - Absent optional values are omitted (no nulls). The enterprise object is omitted when both of its values are absent, and its schema URN is then not listed.
   - Times are formatted with `chrono`: `chrono::DateTime::from_timestamp(secs, nanos)` from the `i64` ns value, then `.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)`, as `backup.rs:~549` does.
   - `service_provider_config`:

     ```json
     {"schemas":["urn:ietf:params:scim:schemas:core:2.0:ServiceProviderConfig"],
      "patch":{"supported":true},"bulk":{"supported":false,"maxOperations":0,"maxPayloadSize":0},
      "filter":{"supported":true,"maxResults":200},"changePassword":{"supported":false},
      "sort":{"supported":false},"etag":{"supported":false},
      "authenticationSchemes":[{"type":"oauthbearertoken","name":"OAuth Bearer Token","description":"A scim-cetra- token from Org settings → Provisioning.","primary":true}]}
     ```

   Tests: `user_resource_omits_absent_values`, `user_resource_lists_the_enterprise_schema_only_when_used`, `list_response_shape`, `service_provider_config_advertises_exactly_what_is_supported`.

**Done when:** committed.

**Commit:** `Add SCIM errors, filters, PATCH and resource shapes`

---

### C3 — The Users resource and discovery

**Goal:** `/scim/v2` serves discovery and the Users resource with D-EA-13, D-EA-16 and D-EA-19.

**Edit:** `crates/server/src/ee/scim/users.rs` (new, with `pub(crate) fn routes()`), `crates/server/src/ee/scim/domains.rs` (new), `crates/server/src/meta/users.rs` (`ensure_member_tx`, step 6), the four C2 files (removing their `allow(dead_code)` line only), `crates/server/src/http/access.rs` (one line: `routes.extend(crate::ee::scim::routes())`), `crates/server/src/http/mod.rs` (install `scim_errors` in `finish`).

**Steps:**

1. **Routes**, all `Access::Scim`, `documented: false`. The base is `/scim/v2`:
   - `GET /scim/v2/ServiceProviderConfig`, `GET /scim/v2/ResourceTypes`, `GET /scim/v2/Schemas`;
   - `GET /scim/v2/Users`, `POST /scim/v2/Users`;
   - `GET /scim/v2/Users/{id}`, `PUT /scim/v2/Users/{id}`, `PATCH /scim/v2/Users/{id}`, `DELETE /scim/v2/Users/{id}`.
2. **Every handler starts with `admit(&state, &principal) -> Result<(OrgId, Uuid), ScimError>`**:
   1. Take `Principal::Scim { org_id, token_id }`.
   2. Run `state.limits.scim.check(&token_id, now)`. On refusal, return `ScimError::rate_limited(retry.0.as_secs().max(1))`.
   3. Load `store::org_scim` and require `enabled` (else `forbidden("SCIM is turned off for this organization.")`).

   Entitlement is checked per operation (step 6), not here, so a lapsed org still deprovisions. `base_url` = `config.public_url` + `/scim/v2`, without a trailing slash.
3. **`domains.rs`:**

   ```rust
   /// The org's SCIM domains (P-1): verified org_sso_domains rows minus instance-claimed ones,
   /// plus the org token's sso_domains on a control-plane cell. Lowercase, sorted, deduplicated.
   pub(crate) async fn scim_domains_tx(state: &AppState, conn: &mut SqliteConnection, org: OrgId, now: i64) -> Result<Vec<String>, MetaError>;
   pub(crate) fn in_domains(domains: &[String], email: &str) -> bool; // exact match on the part after the last '@'
   ```

   The token's list comes from `state.entitlements.sso_domains(org, now)` (C5). Until C5 lands, call it anyway: C3 and C5 agree on that signature, `fn sso_domains(&self, org: OrgId, now: i64) -> Vec<String>`.
4. **`GET /Users`:**
   - query `filter` (optional, parsed with `parse_user_filter`), `startIndex` (default 1, values < 1 become 1), `count` (default 100, max 200, negative becomes 0);
   - `attributes` and `excludedAttributes` are accepted and ignored for Users;
   - answers `list_response(total, startIndex, resources)`. With `count = 0`, it answers `totalResults` and no resources.
5. **`GET /Users/{id}`:** the org's row or 404. An `{id}` that is not a UUID is a 404.
6. **`POST /Users`** (201, `Location: <base>/Users/<id>`, body = the resource). Requires `entitlements::of(state, org).allows(Feature::Scim)`, else `forbidden(...)` (lapse, D-EA-19). Then:
   1. Parse the body as a User:
      - `userName` (required string) and `externalId`;
      - `name.givenName`, `name.familyName`, `displayName`, `active` (default true; with the same lenient boolean rules as PATCH), `emails`;
      - the enterprise extension object's `department` and `employeeNumber`.

      `password` and unknown attributes are ignored. A missing or non-string `userName` is `invalid_value("userName is required")`.
   2. `email = normalize_email(userName)`. A failure is `invalid_value("userName must be an email address")`. When `emails` names a work or primary email that differs from `userName` after normalization, it is `invalid_value("emails must match userName")`.
   3. In one `meta.write`:
      1. `scim_domains_tx`. If `!in_domains`: `invalid_value("userName is not in a verified domain of this organization")`.
      2. A SCIM row with that `user_name` or `external_id` already in the org: `uniqueness()`.
      3. **Link or create:**
         - `users::user_by_email_tx`. When the account exists and is disabled and SCIM did not disable it: `conflict("This account is disabled.")`.
         - When it exists and is **not** an instance admin, call `reset_unproven_tx(conn, &user, now)` and keep the revoked hashes. An instance admin is linked without any credential change (P-6).
         - When there is no account, `insert_user(NewUser { email, name: display name per step 7, password_hash: None, email_verified_at: Some(now), email_verified_via: Some(EmailVerifiedVia::Idp), is_instance_admin: false })`.
      4. **Membership**, when `active`: `ensure_member_tx(conn, org, user_id, default_role, max_seats, now)`, a new function in `meta/users.rs`:

         ```rust
         /// Adds `user` to `org` with `role` unless already a member (the role then stays, P-5),
         /// within `max_seats` for a non-viewer role, and removes the org's pending invite for
         /// the user's email. `Ok(None)` = already a member; `Ok(Some(role))` = added;
         /// `Err(MetaError::Conflict("seat limit"))` = no seat.
         pub(crate) async fn ensure_member_tx(conn: &mut SqliteConnection, org: OrgId, user: Uuid, role: OrgRole, max_seats: Option<u64>, now: i64) -> Result<Option<OrgRole>, MetaError>;
         ```

         The seat check is `seat_members_tx + seat_invites_tx` with `over_limit(max_seats, seats, 1)`, only when `role != Viewer`. The invite removal is `invites::pending_invite_for_tx` + `delete_invite`. `max_seats` comes from `state.entitlements.resolve(org, now).limits.max_seats`, read before the write. The SCIM handler maps `Conflict("seat limit")` to `forbidden("The org has no seat left.")`. Reactivation (step 8) calls the same function.
         When `active` is false, no membership is created, and an existing membership is removed by the deactivation of step 10 (P-7).
      5. `store::insert_user(..)`, with `active` from the body.
      6. Record `ScimAction::UserProvisioned { scim_user_id, role, linked }`, `.org(org)`, target `(User, user_id)`, ctx with `Actor::Scim { token_id }` (the extractor's ctx gives it once C1a's arm exists).
   4. After commit, evict the revoked hashes from `pat_cache`.
7. **Name rule** (D-EA-13): `users.name` is set from `displayName`, else `"<givenName> <familyName>"` trimmed, else unchanged. It is set only when the user belongs to no other org (`SELECT COUNT(*) FROM org_members WHERE user_id = ? AND org_id != ?` = 0). This applies on create, PUT and PATCH.
8. **`PUT /Users/{id}`** replaces the attributes (absent optional attributes are cleared). **`PATCH`** applies `user_changes`. Both, in one `meta.write`:
   1. **Load the row (404).**
   2. **`userName` or `Email` change:** a `UserChange::Email` alone is a userName change. An `Email` and a `UserName` in the same request that differ after normalization are `invalid_value("emails must match userName")`. For PUT, the same rule applies to the body's `emails` and `userName`.
      - normalize, then require both the old and new emails in `scim_domains_tx`, the new one free (`user_by_email_tx` is none, and no SCIM row has it), and no other org membership;
      - else `mutability("userName can only change within the organization's verified domains")` (400);
      - then `users::set_email` (find the existing function that changes a user's email; if none exists, stop and report: do not write raw SQL on `users` here).
   3. **`active` false → true** (reactivation): requires the `scim` feature (else `forbidden`), and the row's `user_name` still in `scim_domains_tx` (else `invalid_value("userName is not in a verified domain of this organization")`). Re-add the membership with `ensure_member_tx`. Re-enable the account (`users::set_disabled(conn, user, None, now)`) only when `disabled_account` is set. Record `UserReactivated`.
   4. **`active` true → false** (deactivation, from PATCH or from a PUT whose body has `active: false`): run step 10's deactivation.
   5. **Other attributes:** `store::update_user`; the name rule; record `UserUpdated { fields }` with the changed SCIM attribute names (`userName`, `externalId`, `name.givenName`, `name.familyName`, `displayName`, `department`, `employeeNumber`).
   6. **Lapse:** without the `scim` feature, any change other than `active: false` is `forbidden(...)`. Deactivation always works.

   Answer 200 with the resource.
9. **`DELETE /Users/{id}`** (204):
   1. The deactivation of step 10, if the row is active.
   2. `store::delete_user`.
   3. Record `UserDeleted`.

   Works when lapsed.
10. **Deactivation**, one shared `async fn deactivate(conn, state_bits, org, row, now) -> Result<(Vec<String>, bool), ScimError>`:
    1. When the user is a member, take their role. An `owner` who is the org's last active owner: `conflict("An org needs an owner.")`, which rolls back. Then `members::remove_org_member` (which clears overrides), and the last-owner check after removal, as `routes/members.rs` does.
    2. When the user is an instance admin, skip steps 3 and 4 (D-P-58, P-6).
    3. When the row was active before this request, `users::revoke_access_tx`, which gives the hashes. This is instance-wide, so it ends the user's sessions in other orgs too (D-EA-16); the docs say so. A row that was already inactive revokes nothing.
    4. When the user now has no membership (`SELECT COUNT(*) FROM org_members WHERE user_id = ?` = 0) and is not disabled: `users::set_disabled(Some(now))`, `disabled_account = true`.
    5. `store::set_user_active(false, disabled_account)`.
    6. Record `UserDeactivated { account_disabled }`.
    7. Evict the hashes after commit.
11. **Discovery:** the three GETs answer `schema.rs`'s documents in `ScimJson`.
12. **`http/mod.rs` `finish`:** add `.layer(axum::middleware::from_fn(crate::ee::scim::error::scim_errors))` directly after the `authenticate` layer and before the `log_request` layer, in code order. In axum a layer listed later is outermost, so `scim_errors` wraps `authenticate`, `csrf`, the body limit, `enforce` and the handlers, and rewrites their problem responses on `/scim/v2`. The fallbacks run inside the router, so they are covered too.

**Tests** (`ee/scim/users.rs`, through `TestApp` with a licence listing `scim` and `org_sso`, a verified domain row inserted directly, `org_scim.enabled = 1`, and a token minted via the store):
- `discovery_documents_are_scim_json`.
- `create_provisions_a_new_viewer_without_password` (201, `Location`, `users.email_verified_via = 'idp'`, membership `viewer`, one `scim.user_provisioned` row).
- `create_links_an_existing_unproven_account_and_resets_it`.
- `create_outside_verified_domains_is_invalid_value`.
- `create_duplicate_user_name_is_uniqueness_409`.
- `create_when_lapsed_is_403_scim_error`.
- `create_at_the_seat_limit_with_member_default_is_refused`; `viewer_default_ignores_the_seat_limit`.
- `list_filters_by_user_name_and_pages`; `unsupported_filter_is_invalid_filter`; `count_zero_returns_only_the_total`.
- `get_of_another_orgs_user_is_404` (two orgs, two tokens).
- `patch_entra_deactivate_revokes_sessions_and_disables_a_single_org_account`.
- `deactivating_a_multi_org_user_keeps_the_account`.
- `deactivating_the_last_owner_is_409_and_changes_nothing`.
- `deactivating_an_instance_admin_only_removes_membership` (the row becomes inactive, the account and sessions stay).
- `create_for_an_instance_admin_links_without_touching_credentials`.
- `post_inactive_for_an_existing_member_removes_the_membership`.
- `reactivating_after_the_domain_lapsed_is_invalid_value`.
- `email_only_patch_changes_the_user_name`.
- `reactivate_restores_membership_and_reenables`.
- `reactivate_when_lapsed_is_refused`; `deactivate_when_lapsed_works`.
- `delete_deactivates_then_removes_the_row`.
- `user_name_change_outside_domains_is_mutability`.
- `put_clears_absent_optional_attributes`.
- `unauthenticated_request_gets_a_scim_401`; `unknown_scim_path_gets_a_scim_404`; `wrong_method_gets_a_scim_405`.
- `scim_disabled_org_is_403`.
- `no_email_or_token_in_logs` (`capture_logs`, needles: the email, the token).

**Done when:** committed.

**Commit:** `Serve SCIM Users and discovery`

---

### C4 — Provisioning API, locks and JIT

**Goal:** owners manage SCIM, managed members are locked, and JIT is off while SCIM is on.

**Edit:**
- `crates/server/src/ee/scim/settings.rs` (new, with `pub(crate) fn routes()`);
- `crates/server/src/api_examples.rs` (examples for the four Provisioning operations, following `get_org_sso`'s entries, ~807);
- `crates/server/src/auth/roles.rs` (`OrgPerm::ManageProvisioning`);
- `crates/server/src/api/scim.rs` (new DTOs) and `crates/server/src/api/mod.rs`;
- `crates/server/src/api/openapi.rs`;
- `crates/server/src/api/members.rs` (`scim_managed` on `OrgMember`);
- `crates/server/src/routes/members.rs`, `crates/server/src/routes/invites.rs`, `crates/server/src/ee/project_roles.rs`, `crates/server/src/ee/org_sso/login.rs`;

**Steps:**

1. `OrgPerm::ManageProvisioning`, minimum role `Owner` (next to `ManageSso`), with its row in the roles test.
2. DTOs (`api/scim.rs`, `ToSchema`, serde `snake_case`, `deny_unknown_fields` on requests):

   ```rust
   pub struct ScimSettings { pub enabled: bool, pub default_role: OrgRole, pub base_url: String, pub verified_domains: u32, pub tokens: Vec<ScimToken> }
   pub struct ScimToken { pub id: Uuid, pub name: String, pub prefix: String, pub created_at: i64 /* time_serde */, pub created_by: Option<Uuid>, pub last_used_at: Option<i64> /* optional_time */ }
   pub struct UpdateScimSettings { pub enabled: Option<bool>, pub default_role: Option<OrgRole> }
   pub struct CreateScimTokenRequest { pub name: String }
   pub struct CreatedScimToken { pub token: ScimToken, pub secret: String } // shown once
   ```

   Times use the same `time_serde` / `optional_time` attributes as `api/keys.rs` `PersonalToken`.
3. Routes (`documented: true`, `Access::Org { perm: OrgPerm::ManageProvisioning, session_only: true }`):
   - **`GET /api/v1/orgs/{org}/scim`:** the settings. `verified_domains` counts `scim_domains_tx`'s list. Reading needs no feature, so the page shows locked.
   - **`PATCH /api/v1/orgs/{org}/scim`:**
     - turning `enabled` on or changing `default_role` needs `require(Feature::Scim)`, and enabling also needs at least one SCIM domain (`409 conflict` "Verify a domain first.");
     - turning it off is always allowed;
     - in one write: `put_org_scim`, then record `ScimAction::Enabled`, `Disabled` or `DefaultRoleChanged`;
     - `default_role` must be `viewer` or `member` (`400` field error otherwise).
   - **`POST /api/v1/orgs/{org}/scim/tokens`** (201):
     - needs `Feature::Scim`;
     - the name is checked inline: trimmed, 1 to 100 characters, else `field_error("name", "must be 1 to 100 characters")` (the rule of `routes/tokens.rs` `checked_name`, which is private);
     - `secret::new_key(KeyKind::Scim)`, `store::insert_token`. The third live token is `409 conflict` "Revoke a token first.";
     - record `TokenCreated`.
   - **`DELETE /api/v1/orgs/{org}/scim/tokens/{id}`** (204): always allowed. `store::revoke_token`, record `TokenRevoked`, then `state.scim_cache.evict(&hash)`.

   Add the four paths and the DTOs to `api/openapi.rs` the way the org SSO paths are added (security `session`).
4. **Locks:** `ee::scim::locks` (C1a's `mod.rs`).

5. **Guards**, each computing `let locks = ee::scim::locks(&state, org).await?;` before the write and, inside the closure, when `locks && store::is_managed_tx(conn, org, target).await?`, returning a new outcome variant mapped to `ApiError::ScimManaged`:
   - `routes/members.rs` `update_org_member`: after `target_role` is read.
   - `routes/members.rs` `remove_org_member`: after `target_role` is read. This covers self-leave too.
   - `ee/project_roles.rs` `set_project_override` and `remove_project_override`: before the write.
   - `routes/invites.rs` `create_invite`: when `locks` and the normalized email is in `scim_domains_tx` → `ScimManaged`.
   - `routes/invites.rs` `accept_invite`: the same check on the invite's email, before `add_org_member`.
6. **JIT off:** in `ee/org_sso/login.rs` `finish`, the `jit:` value of `Admit` becomes `None` when `ee::scim::locks(state, org)` is true.
7. **`OrgMember.scim_managed: bool`** (`api/members.rs`): `routes/members.rs` sets it from C1a's `store::managed_user_ids(pool, org)`, run once per list, when `locks` is true; false otherwise. Update every `OrgMember` literal in the crate (search `OrgMember {`).

**Tests** (some call C3's `/scim/v2` endpoints; C3 runs at the same time and the tests run at the end):
- `settings_require_owner_session` (admin → 403, PAT → 403);
- `enabling_needs_the_feature_and_a_domain`;
- `disabling_works_when_lapsed`;
- `token_create_shows_the_secret_once_and_caps_at_two`;
- `revoked_token_stops_working_at_once`;
- `managed_member_role_change_is_409_scim_managed`, `managed_member_removal_and_self_leave_are_409`, `managed_member_override_is_409`;
- `invite_to_a_verified_domain_is_409_while_scim_is_on`, `invite_to_an_outsider_works`, `accepting_an_old_invite_to_a_verified_domain_is_409`;
- `locks_lift_when_scim_is_disabled`, `locks_lift_on_lapse`;
- `org_sso_jit_is_off_while_scim_is_on` (in `ee/org_sso/login.rs` tests);
- `member_list_marks_managed_members`.

**Done when:** committed.

**Commit:** `Add SCIM settings, tokens and the scim_managed locks`

---

### C5 — Cloud: `sso_domains` in the org token

**Goal:** a control-plane cell takes the org's verified domains from the org token.

**Edit:** `crates/token/src/lib.rs`, `crates/server/src/entitlements/tokens.rs`, `crates/server/src/entitlements/mod.rs`, `crates/server/src/test_support/mod.rs` (the `org_token` helper; coordinate: C1b also edits `test_support/mod.rs`, so C5 runs after C1b is committed).

**Steps:**

1. `OrgClaims` gets `#[serde(default, skip_serializing_if = "Vec::is_empty")] pub sso_domains: Vec<String>`, documented as "The org's verified SSO domains on the control plane, used by cells for SCIM only (D-EA-17)." Update the struct literal in its test (`lib.rs:~665`). This is additive: older tokens verify and parse unchanged.
2. `OrgGrant.sso_domains: Vec<String>`. `grant_from_token` normalizes each domain with `ee::org_sso::normalize_domain` (drop the invalid ones), then sorts and deduplicates. Update every `OrgGrant` literal (`tokens.rs:~83`, `entitlements/mod.rs:~505, ~527`).
3. `EntitlementsState::sso_domains(&self, org: OrgId, now: i64) -> Vec<String>`: the grant's list while `now < grace_ends_at`; empty otherwise, and on a cell without `control_plane`.
4. `test_support::org_token`: add an `sso_domains: &[&str]` parameter (update its callers in `test_support/mod.rs`), or add `org_token_with_domains` beside it if the callers are elsewhere. Choose the sibling when callers exist outside `test_support`.

**Tests:**
- `crates/token`: `sso_domains_round_trip_and_default_to_empty`.
- `entitlements`: `grant_keeps_normalized_sso_domains`, `sso_domains_empty_after_grace`.
- `ee/scim/users.rs` is C3's: the cloud case is tested by C3 only if C5 is committed first. Otherwise the coordinator adds `cloud_scim_uses_token_domains` after both.

**Done when:** committed.

**Commit:** `Carry the org's SSO domains in the org token`

---

### C6 — Provisioning page and managed members in the web UI

**Goal:** owners manage SCIM in the UI. Managed members show as managed, with their controls disabled.

**Prerequisite:** the coordinating session has run `just openapi` and `just api-examples` after C4, so `schema.gen.ts` has the SCIM types and `scim_managed`, and `fixtures/api` has the Provisioning responses.

**Edit:**
- `web/src/routes/$org/settings/provisioning.tsx` (new) and `provisioning.test.tsx` (new);
- `web/src/ee/scim/provisioning-settings.tsx`, `web/src/ee/scim/tokens-section.tsx` (new, SPDX Elastic-2.0);
- `web/src/api/queries-settings.ts` (`orgScimQuery`);
- `web/src/components/settings/settings-nav.ts` and its test;
- `web/src/components/settings/org-members.tsx` and its test (the role select and Remove live there), and `web/src/routes/$org/settings/members.test.tsx` if the member tests are there;
- `web/src/ee/project-roles/member-access-panel.tsx`;
- `web/src/components/plan/feature-label.ts` (`featureOffer` is an exhaustive switch over `Feature`: add `scim`);
- `web/src/ee/license-card.tsx` (`FEATURE_NAMES`).

`fixtures/api` is generated by the coordinator (`just api-examples`) from C4's examples; C6 does not edit it.

**Steps:**

1. **Nav:** after "Single sign-on", add `{ to: "/$org/settings/provisioning", label: "Provisioning", icon: UsersRound, description: "SCIM from your identity provider", keywords: ["scim", "okta", "entra", "provisioning"], ownerOnly: true }`.
2. **Page:** the `sso.tsx` structure with title "Provisioning" and description "Create, update and remove members from your identity provider over SCIM 2.0." Owners see `<ProvisioningSettings org={org} />`; others see "Only owners can manage provisioning.".
3. **`ProvisioningSettings`:**
   - When `entitlements.locked("scim")`: `<LockedFeature feature="scim" … />`.
   - Otherwise:
     - a status card: the on/off switch, disabled with the reason "Verify a domain on the Single sign-on page first" when `verified_domains = 0`; the default role select (Viewer, Member); the base URL with `CopyButton`;
     - a tokens card: a table (name, prefix, created, last used) with Revoke (`TypeToConfirmDialog` "revoke"), and Create (name field) showing the secret once with a copy button and "You won't see this token again.". Create is disabled with a reason when two tokens exist.
   - Errors go through `problemMessage`.
4. **Members page:**
   - when `member.scim_managed`, the role select and Remove are disabled with "Managed by your identity provider" (`DisabledReason`);
   - the member access panel disables override changes for managed members;
   - a `scim_managed` 409 from the server shows its message.
5. Add `"scim"` to `FEATURE_NAMES` in `web/src/ee/license-card.tsx`, and a `scim` case to `featureOffer` in `feature-label.ts` ("SCIM provisioning is part of the Enterprise plan and the self-hosted licence.").

**Tests** (`vitest`, MSW `server.use` overrides):
- `provisioning.test.tsx`:
  - `shows the locked state without the feature`;
  - `owner enables SCIM and copies the base URL`;
  - `switch disabled without a verified domain`;
  - `creates a token and shows it once`;
  - `create disabled at two tokens`;
  - `revokes a token after confirmation`;
  - `non-owner sees the note`.
- `members.test.tsx`: `managed member controls are disabled with the reason`.
- `settings-nav.test.ts`: the new item is owner-only.

**Done when:** committed.

**Commit:** `Add the Provisioning settings page`

---

### C7 — Docs

**Goal:** docs, specs and naming match M5.

**Edit:**
- `website/src/content/docs/docs/platform/scim.mdx` (new), `website/src/content/docs/docs/platform/plans-and-licence.mdx`, and the docs sidebar config if pages are listed there (find how `sso.mdx` is registered);
- `docs/spec/04-metadata-db.md`, `docs/spec/08-auth.md`, `docs/spec/09-api.md`, `docs/spec/11-ui.md`, `docs/spec/12-operations.md`;
- `AGENTS.md` (§ Naming);
- `CHANGELOG.md`.

**Steps:**
1. **`scim.mdx`**, following `website/STYLE.md` and `sso.mdx`'s structure:
   - what SCIM does in cetra; prerequisites (a verified domain on the Single sign-on page, the Enterprise plan or the licence);
   - enabling it and creating a token;
   - the base URL; the supported attributes and filters;
   - deprovisioning behaviour (D-EA-16), including that it ends the person's sessions in every org of the instance; the locks (D-EA-18), which apply to changes by hand, not to the control plane's member API; and lapses (D-EA-19);
   - "Groups and role mappings, and step-by-step Okta and Entra ID guides, are coming next" is **not** written: describe only what exists.

   Code blocks are marked untested per `check-samples.mjs`'s convention for HTTP examples.
2. **`plans-and-licence.mdx`:** add SCIM to the licence and Enterprise features, as the row format there requires. The pricing test may count features: run nothing, report it.
3. **Specs:**
   - 04: the SCIM tables.
   - 08: § SCIM (tokens, principal, domains, deprovisioning, locks, lapses), with `ManageProvisioning` and `scim_managed` in § Authorization.
   - 09: the Provisioning routes and the `/scim/v2` surface (outside OpenAPI).
   - 11: the Provisioning page and the managed-member state.
   - 12: `scim` in § Licence keys.
4. **`AGENTS.md` § Naming:** API keys are `sk-cetra-…`, `pk-cetra-…`, `pat-cetra-…` and `scim-cetra-…`.
5. **`CHANGELOG.md` `Added`:** "SCIM 2.0 user provisioning (Enterprise and the self-hosted licence): per-org tokens, Okta and Entra ID compatible Users endpoint, deprovisioning that revokes access, and members managed by the identity provider."

**Done when:** committed.

**Commit:** `Document SCIM user provisioning`

---

## After the phases (coordinating session)

1. Run `just openapi` after C4 and commit the generated files. Run `just api-examples` if the manifest needs it.
2. Review each diff.
3. Run `just check` once, with Node on the PATH (`export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH`).
4. Fix phase, then a Sonnet code review of M5.
5. Roadmap: "M5 implemented". Send track Q the `sso_domains` request (the design's § Requests already lists it).
