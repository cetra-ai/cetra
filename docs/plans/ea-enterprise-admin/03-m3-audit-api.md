# EA M3 — Audit log API, export and pages

**Summary.**
- This milestone opens the audit records to the people who may read them. It adds:
  - the `audit_log` feature;
  - `OrgPerm::ViewAudit`;
  - the org and instance read routes, with filters and `seq` cursor paging;
  - CSV and NDJSON exports, recorded before they stream;
  - names and emails resolved at read time (current members only in an org log);
  - the per-org `audit_retention_days` limit, and the per-org purge;
  - the Audit log pages in org settings and `/admin`, with the locked state;
  - the docs page.
- The stream is M4.

**Prerequisites.**
- M1, M2 and M5 are committed and green. M5 is required because it edits the same shared files: `api/entitlements.rs` (`Feature`), `auth/roles.rs`, `audit/catalog/mod.rs`, `api/openapi.rs`, `api/mod.rs`, `api_examples.rs`, `http/access.rs`, the token crate, `settings-nav.ts` and `feature-label.ts`.
- M6 may run beside M3 only on files neither lists.
- Branch `claude/ea-enterprise-admin` in `/home/sebastien/travaux/llm-observer/.claude/worktrees/ea-design`.

**Read first.**
- `docs/plans/ea-enterprise-admin/README.md` § Audit log (Read API and UI, Retention, Privacy), D-EA-3, D-EA-7, D-EA-8, D-EA-10, D-EA-19, D-EA-20.
- The M1 plan's shared rules (`01-m1-audit-recording.md`).
- `crates/server/src/audit/` (the catalog and `record_alone`).
- The facts file `/home/sebastien/.config/Claude-extra/side-session-notes/local_1c672712-2cc7-40e9-8d6b-d582417356b7/ea-m3-m4-code-facts.md`: §0–§7. Lines may have moved: locate by name.

## What it is, and what it is not

**What works when it's done:**
- An org admin or owner opens Org settings → Audit log and sees the org's records, newest first. They can:
  - filter by time, action (exact names or a group such as `member.`), actor, target, project and outcome;
  - open a record's details;
  - export the filtered records as CSV or NDJSON.
- Users who are still members show their name and email. Others show "Former member" or "Deleted user" with the id.
- An instance admin does the same with the instance log on `/admin` → Audit log.
- Without `audit_log` (org) or a licence listing it (instance), the routes answer `403 plan_required` and the pages show the lock. A cloud cell's instance log needs no licence.
- Each org's records are purged at its `audit_retention_days` (from the org token, else `audit.retention_days`), with a floor of 30 days.

**Out of scope:**
- the stream (M4);
- the website's pricing rows: the marketing site waits until the product is frozen, per the user's standing rule.

## Decisions this plan relies on

| ID | Summary |
|---|---|
| D-EA-3 | Ids stored; names and emails joined at read time; erased users read as "Deleted user" |
| D-EA-7 | `audit.retention_days` default 365, 0 = forever, ≥ 30; the cloud's `audit_retention_days` limit replaces it per org |
| D-EA-8 | The instance log (null `org_id`) for instance admins; instance admins never read org logs |
| D-EA-10 | `OrgPerm::ViewAudit` for admins and owners, session or PAT, no API key |
| D-EA-19 | `audit_log` (ee; Enterprise, licence); lapses stop reads and exports; recording continues; a cloud cell's instance log needs nothing |
| D-EA-20 | `seq` is the cursor |
| D-P-12, D-P-13, D-P-14, D-P-15 | Feature names, limits, `plan_required`, locked features stay visible |

**Plan-level choices:**
- **P-1. Paging.** Default 100 per page, max 500, through a dedicated `AuditLimit` type (`PageParams`'s `Limit` allows 1000). The cursor is base64url(JSON `{"seq": N}`), newest first (`seq < N ORDER BY seq DESC`).
- **P-2. Actions filter.** `action` is a comma-separated list of at most 20 entries. Each entry matches `^[a-z_]+(\.[a-z_]+)?\.?$`: an entry ending with `.` is a group prefix (`LIKE 'member.%'`), any other an exact name. Anything else is `400 field_error("action", "…")`.
- **P-3. Display resolution.** Per page, one query per resolved kind (`users`, `org_members`, `api_keys`, `projects`, `org_sso_providers`). Each row gets `actor` and `target` objects with `status`:
  - `present`: name and email are shown;
  - `former_member`: in an org log, the user exists but is not a member;
  - `deleted`: no `users` row;
  - `unresolved`: a kind that is not resolved.
- **P-4. Retention limit.** `Limits.audit_retention_days` has no instance default (the defaults stay `None`), so existing limits, responses and fixtures do not change.
  - The purge reads it per org: `None` (no claim, or `null`) → the instance's `audit.retention_days`; `Some(0)` → keep forever; `Some(d)` → `max(d, 30)` days.
  - A lapsed org's grant falls back to the defaults, so it gets the instance value again. The docs say so.
- **P-5. Instance gate.** The instance routes need `state.entitlements.license().state.active_features(now)` to contain `Feature::AuditLog`, or `config.entitlements.control_plane` to be true (a cloud cell, D-EA-19).
- **P-6. CSV safety.** A CSV cell that starts with `=`, `+`, `-`, `@`, a tab or a carriage return is prefixed with `'`. User agents and names are attacker-chosen.
- **P-7. Export cap.** 1,000,000 rows, streamed in pages of 1,000 (reader pool), names resolved per page. The stream stops after the cap; the docs say so.
- **P-8. Nav.** Org settings nav items get an optional `adminOnly` flag (shown to admins and owners), like the project nav's.

## Versions

No new dependency. `csv` 1.4 is already a dependency of `cetra-server`.

## Phase graph

```
D2 (feature, limit, purge) ──┐
D3 (perm, catalog, read, export, routes) ──┴── D5 (web) ── D6 (docs)
```

- D2 and D3 run in parallel: their files are disjoint. D3's code uses `Feature::AuditLog`, which D2 adds; that only matters at compile time, and the checks run once both are committed.
- D3 holds the read routes and the exports together (no phase leaves a route without its handler).
- D5 needs `just openapi` and `just api-examples` (coordinator) after D3.
- D6 runs alongside D5.
- There is no D1 or D4: their content is in D3.

**Shared rules:** M1's shared rules, plus P-8 of M5 (clone state into closures). `ee/` files start with `// SPDX-License-Identifier: Elastic-2.0`.

---

### D2 — The `audit_log` feature, the retention limit and the per-org purge

**Goal:** the feature exists, orgs carry `audit_retention_days`, and the purge applies it.

**Edit:** `crates/server/src/api/entitlements.rs`, `crates/server/src/entitlements/mod.rs`, `crates/server/src/entitlements/tokens.rs`, `crates/token/src/lib.rs`, `crates/server/src/serve.rs`, `crates/server/src/audit/retention.rs`, `crates/cli/src/main.rs`.

**Steps:**
1. **`Feature::AuditLog`**, appended after `Scim`:
   - `ALL` length bumped;
   - name `"audit_log"`, `is_ee` true;
   - `locked_detail`: "The audit log is part of the Enterprise plan and the self-hosted licence.".

   The `entitlements/mod.rs` round-trip test learns `audit_log`. Its `from_name("audit_logs") == None` assertion stays: the name is singular.
2. **`LimitsClaim.audit_retention_days`** in `crates/token/src/lib.rs`:

   ```rust
   #[serde(default, with = "double_option", skip_serializing_if = "Option::is_none")]
   pub audit_retention_days: Option<Option<u64>>,
   ```

   Add it to the `org_claims_round_trip` literal and a single-key round-trip sub-test like `max_annotation_queues`'s.
3. **`Limits.audit_retention_days: Option<u64>`** (`entitlements/mod.rs`):
   - `Defaults::from_config` sets it to `None`: there is no `[entitlements.defaults]` key (P-4);
   - `merge_limits` (`entitlements/tokens.rs`) merges it like the others, with tests for value, `null` and missing;
   - every `Limits { … }` literal in the crate gets the field: search `Limits {` in `crates/server/src` and fix each in this phase's files. A literal in another file uses `..Default::default()` if it already does; otherwise report it.
4. **`OrgLimits.audit_retention_days`** (`api/entitlements.rs`), `#[serde(default, skip_serializing_if = "Option::is_none")]`, and the `From<Limits>` mapping.
5. **`crates/cli/src/main.rs` `limit_rows`:** add `("audit_retention_days", limits.audit_retention_days)`, bump the array length, and update its test. This limit reads differently from the others: `None` prints "instance default" and `0` prints "kept forever". Special-case it where the rows are formatted. The `OrgLimits` field doc says the same ("Omitted: the instance's `audit.retention_days`; 0: kept forever"), as an exception to the struct's "omitted is unlimited" note.
6. **`audit/retention.rs`:**
   - Replace `purge` with:

     ```rust
     /// Purges the instance log at `instance_days` (0 = keep) and each org's records at its
     /// own limit (`None` = keep), never below 30 days (D-EA-7, plan P-4). Returns the count.
     pub(crate) async fn purge(state: &AppState, now: i64) -> Result<u64, MetaError>;
     async fn purge_scope(meta: &MetaDb, org: Option<OrgId>, days: u32, now: i64) -> Result<u64, MetaError>;
     ```

   - `purge` steps:
     1. `purge_scope(None, config.audit.retention_days)` when non-zero.
     2. `SELECT id FROM orgs` on the reader (every org, active or not; dropped orgs' records are already gone).
     3. For each org, `state.entitlements.resolve(org, now).limits.audit_retention_days`:
        - `None` → the instance's `audit.retention_days` (skip when 0);
        - `Some(0)` → skip;
        - `Some(d)` → `max(d, 30)`, saturating into `u32`.

        Call `purge_scope(Some(org), days, now)`.
   - `purge_scope` batches like today, adding `AND org_id IS ?` (bind NULL for the instance).
   - `serve.rs` `spawn_audit_purge` calls `purge(&state, now)`.
   - Tests:
     - `an_orgs_limit_overrides_the_instance_default` (via `test_support::set_limits`);
     - `a_limit_below_30_counts_as_30`;
     - `a_null_limit_keeps_everything`;
     - `the_instance_log_uses_the_config`.
     - The existing tests adapt to the new signature. They build their app with `TestApp::with_config(|c| c.audit.retention_days = 30)` (or `0` for keep-all), since `purge` now takes the instance days from the config.
     - `test_support::set_limits` ignores a grant that is not newer than the current one, so successive calls in one test use strictly increasing `issued_at`.

**Done when:** committed.

**Commit:** `Add the audit_log feature and per-org audit retention`

---

### D3 — Read routes and exports

**Goal:** the permission, the export action, the org and instance logs with filters, paging and resolved names, and their CSV and NDJSON exports.

**Edit:**
- `crates/server/src/auth/roles.rs`, `crates/server/src/audit/catalog/audit.rs` (new), `crates/server/src/audit/catalog/mod.rs`, `crates/server/src/audit/tests.rs`;
- `crates/server/src/ee/mod.rs` (`pub(crate) mod audit;`), `crates/server/src/ee/audit/mod.rs`, `read.rs` and `export.rs` (new);
- `crates/server/src/api/audit.rs` (new), `crates/server/src/api/mod.rs`, `crates/server/src/api/openapi.rs`;
- `crates/server/src/http/access.rs` (one `routes.extend(crate::ee::audit::routes());` line);
- `crates/server/src/api_examples.rs`, `crates/server/tests/api_examples.rs`, `crates/server/tests/openapi.rs`;
- `fixtures/api/static/org-audit-events.json` and `fixtures/api/static/admin-audit-events.json` (new), `fixtures/api/README.md`.

**Steps 0 (permission and action):**
- `OrgPerm::ViewAudit`, with minimum role `Admin` (in the `Admin` group of `min_role`), and one assert in `permissions_follow_the_spec_table`.
- `audit/catalog/audit.rs` `AuditAction` (group `audit`), with one variant `#[serde(rename = "audit.export_started")] ExportStarted { format: &'static str, filters: Vec<&'static str> }`:
  - `format` is `csv` or `ndjson`;
  - `filters` holds the names of the filters that were set (`from`, `to`, `action`, `actor_id`, `target_id`, `project_id`, `outcome`), never their values.

  Wire it in `catalog/mod.rs` (`mod`, `pub(crate) use`, `Action::Audit`, `From`), and add a sample and the `audit` prefix to `audit/tests.rs`.

**Steps:**

1. **DTOs** (`api/audit.rs`). `AuditEvent`, `AuditActor` and `AuditRef` derive `Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema`. The enums (`AuditOutcome`, `AuditRefStatus`, `AuditExportFormat`) derive `Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema` with serde `snake_case`. `AuditLimit` copies `api/common.rs` `Limit`'s derives and impls (`Debug, Clone, Copy, PartialEq, Eq, Serialize` transparent, `ToSchema` with `value_type = u32`, a hand `Deserialize` with bounds 1–500, `Default` = 100, and `get()`).

   ```rust
   pub struct AuditEvent {
       pub id: Uuid,
       #[serde(with = "cetra_core::time_serde")] #[schema(value_type = String, format = DateTime)]
       pub at: i64,
       pub action: String,
       pub outcome: AuditOutcome,
       pub actor: AuditActor,
       #[serde(default, skip_serializing_if = "Option::is_none")] pub target: Option<AuditRef>,
       #[serde(default, skip_serializing_if = "Option::is_none")] pub project: Option<AuditRef>,
       /// The action's details: ids, names of changed fields, roles, counts. Never payload.
       #[schema(value_type = Object)] pub details: serde_json::Value,
       #[serde(default, skip_serializing_if = "Option::is_none")] pub ip: Option<String>,
       #[serde(default, skip_serializing_if = "Option::is_none")] pub user_agent: Option<String>,
   }
   pub enum AuditOutcome { Success, Failure }
   pub enum AuditRefStatus { Present, FormerMember, Deleted, Unresolved }
   pub struct AuditActor {
       pub kind: String,                  // user | api_key | scim_token | control_plane | cli | system | anonymous
       #[serde(default, skip_serializing_if = "Option::is_none")] pub id: Option<String>,
       #[serde(default, skip_serializing_if = "Option::is_none")] pub via: Option<String>,
       #[serde(default, skip_serializing_if = "Option::is_none")] pub credential_id: Option<String>,
       #[serde(default, skip_serializing_if = "Option::is_none")] pub name: Option<String>,
       #[serde(default, skip_serializing_if = "Option::is_none")] pub email: Option<String>,
       pub status: AuditRefStatus,
   }
   pub struct AuditRef {
       pub kind: String, pub id: String,
       #[serde(default, skip_serializing_if = "Option::is_none")] pub name: Option<String>,
       #[serde(default, skip_serializing_if = "Option::is_none")] pub email: Option<String>,
       pub status: AuditRefStatus,
   }
   #[derive(Debug, Clone, PartialEq, Eq, Deserialize, IntoParams)] #[serde(deny_unknown_fields)]
   pub struct AuditListParams {
       #[serde(default)] pub from: Option<TimeParam>,
       #[serde(default)] pub to: Option<TimeParam>,
       #[serde(default)] pub action: Option<String>,
       #[serde(default)] pub actor_id: Option<String>,
       #[serde(default)] pub target_id: Option<String>,
       #[serde(default)] pub project_id: Option<Uuid>,
       #[serde(default)] pub outcome: Option<AuditOutcome>,
       #[serde(default)] #[param(minimum = 1, maximum = 500, default = 100)] pub limit: AuditLimit,
       #[serde(default)] pub cursor: Option<String>,
   }
   /// 1 to 500, default 100 (plan P-1); copy `api/common.rs` `Limit`'s Deserialize with these bounds
   /// and the message "limit must be between 1 and 500".
   pub struct AuditLimit(pub u32);
   ```

   `AuditExportParams` is the same struct without `limit` and `cursor`, plus `format: AuditExportFormat` (`csv` | `ndjson`, required). Step 6 uses it.

   - `from` is inclusive and `to` exclusive, as `UserListParams` documents.
   - `actor_id` and `target_id` are at most 64 characters (`field_error` otherwise).
   - `project_id` is accepted on the instance route but matches nothing there: instance records have no project.
2. **`ee/audit/read.rs`:**

   ```rust
   pub(crate) enum Scope { Org(OrgId), Instance }
   pub(crate) struct Filters { pub(crate) from: Option<i64>, pub(crate) to: Option<i64>, pub(crate) actions: Vec<ActionMatch>, pub(crate) actor_id: Option<String>, pub(crate) target_id: Option<String>, pub(crate) project_id: Option<ProjectId>, pub(crate) outcome: Option<&'static str> }
   pub(crate) enum ActionMatch { Exact(String), Prefix(String) }
   pub(crate) struct Row {
       pub(crate) seq: i64, pub(crate) id: String, pub(crate) at: i64,
       pub(crate) org_id: Option<String>, pub(crate) project_id: Option<String>,
       pub(crate) action: String, pub(crate) outcome: String,
       pub(crate) actor_kind: String, pub(crate) actor_id: Option<String>, pub(crate) via: Option<String>, pub(crate) credential_id: Option<String>,
       pub(crate) target_kind: Option<String>, pub(crate) target_id: Option<String>,
       pub(crate) details: String, pub(crate) ip: Option<String>, pub(crate) user_agent: Option<String>,
   }
   /// P-2 validation; errors are `field_error(<name>, …)`. Seven parameters (clippy's limit).
   pub(crate) fn filters(from: Option<&TimeParam>, to: Option<&TimeParam>, action: Option<&str>, actor_id: Option<&str>, target_id: Option<&str>, project_id: Option<Uuid>, outcome: Option<AuditOutcome>) -> Result<Filters, ApiError>;
   pub(crate) async fn page(pool: &SqlitePool, scope: &Scope, filters: &Filters, before_seq: Option<i64>, limit: u32) -> Result<Vec<Row>, MetaError>;
   pub(crate) async fn resolve(pool: &SqlitePool, scope: &Scope, rows: &[Row]) -> Result<Vec<AuditEvent>, MetaError>;
   pub(crate) fn encode_cursor(seq: i64) -> String; // base64url of format!("{{\"seq\":{seq}}}"): no serde, so no error path
   pub(crate) fn decode_cursor(text: &str) -> Result<i64, ApiError>; // invalid → field_error("cursor", "invalid cursor")
   ```

   - **`page`** builds `SELECT … FROM audit_events WHERE <scope> AND …` with `sqlx::QueryBuilder`:
     - scope `org_id = ?` or `org_id IS NULL`;
     - `at >= ?`, `at < ?`;
     - `(action = ? OR action LIKE ? ESCAPE '\' …)`. A prefix's `_` and `%` are escaped with `\`: copy `meta/datasets.rs`'s private `escape_like` (~280) into `read.rs`. In the Rust string literal the clause is `"ESCAPE '\\'"`;
     - `actor_id = ?`, `target_id = ?`, `project_id = ?`, `outcome = ?`;
     - `seq < ?`;
     - `ORDER BY seq DESC LIMIT ?`.
   - **`resolve`** (P-3):
     - collect user ids from user actors and `user` targets, and fetch `id, name, email` from `users WHERE id IN (…)`;
     - for an org scope, also fetch `user_id FROM org_members WHERE org_id = ? AND user_id IN (…)`;
     - **org scope only**: fetch `api_keys.name` for `api_key` actors and targets through the org's projects (`SELECT k.id, k.name FROM api_keys k JOIN projects p ON p.id = k.project_id WHERE p.org_id = ? AND k.id IN (…)`), `projects.name WHERE org_id = ? AND id IN (…)` for project refs (the row's `project_id` and `project` targets), and `org_sso_providers.display_name WHERE org_id = ?` for `sso_provider` targets;
     - **instance scope**: resolve users only. Every other kind is `unresolved` (D-EA-8: an instance admin learns nothing about an org's objects).
     - Status rules:
       - **User:** no `users` row → `deleted`, no name or email. Org scope and not a member → `former_member`, no name or email. Otherwise `present` with name and email.
       - **Key, project, provider:** a row → `present` with its name (no email); none → `deleted`.
       - **Every other kind:** `unresolved`, id only.
     - Use the base64url cursor model of `routes/admin.rs:~486-505` (`encode_cursor`, `cursor_error`) with a struct `{ seq: i64 }`.
     - `details` is parsed from the stored JSON. A parse failure gives `{}` and logs `tracing::warn!(id = %id, "audit details do not decode")`.
     - Ids are bound as text. Use `QueryBuilder` with `separated(", ")`, as `meta/users.rs:~503` `providers_by_user` does.
3. **Routes** (`ee/audit/mod.rs`, SPDX header, with the export handlers of step 6 in `export.rs`):

   ```rust
   pub(crate) mod export;
   pub(crate) mod read;
   pub(crate) fn routes() -> Vec<RouteDef> // the four below
   ```

   - `GET /api/v1/orgs/{org}/audit-events`: `Access::Org { perm: OrgPerm::ViewAudit, session_only: false }`, documented. Handler `list_org`: `require(&state, org, Feature::AuditLog)?`, then `filters`, `decode_cursor`, `page(limit + 1)`, `resolve`, and `Page { data, next_cursor }` (the next cursor is the last kept row's seq when more rows exist).
   - `GET /api/v1/orgs/{org}/audit-events/export`: same access, documented; operation id `get_org_audit_export`; handler `export::export_org` (step 6).
   - `GET /api/v1/admin/audit-events`: `Access::InstanceAdmin`, documented. Handler `list_instance`: the P-5 gate (else `ApiError::PlanRequired { detail: Feature::AuditLog.locked_detail() }`), then the same steps with `Scope::Instance`.
   - `GET /api/v1/admin/audit-events/export`: `Access::InstanceAdmin`, documented; operation id `get_admin_audit_export`; handler `export::export_instance` (step 6).

   The list operation ids are `list_org_audit_events` and `list_admin_audit_events`.
4. **OpenAPI** (`api/openapi.rs`):
   - add `tag("audit", "Audit records of an organization and of the instance.")`;
   - `audit_paths` for the four routes, modelled on `list_org_deletions` and `list_admin_users`. The exports use a `ResponseBuilder` with `text/csv` and `application/x-ndjson` string content, as `export_result()` does. The 403 text mentions `plan_required`.
   - Schemas: `AuditEvent`, `AuditActor`, `AuditRef`, `AuditOutcome`, `AuditRefStatus`, `AuditExportFormat`, plus `insert_page::<AuditEvent>("AuditEventPage")`.
5. **Examples:**
   - `api_examples.rs`: `example("list_org_audit_events", params(&[("org","acme")]), BTreeMap::new(), "static/org-audit-events.json", "application/json")` and `example("list_admin_audit_events", …, "static/admin-audit-events.json", …)`.
   - The two static files are canonical pretty JSON pages of 3 records each (one `member.role_changed` with a present actor, one `auth.login` from an anonymous actor with an IP, one with a `former_member` target), using the ids of `fixtures/api/README.md`'s fixed-identifier table. Extend the table with three audit event ids from a free range. `…0d01` to `…0d04` are dataset items, so grep `fixtures/api/README.md` and `api_examples.rs` and pick e.g. `…1000`, `…1001`, `…1002` when unused.
     - The files must round-trip through the DTOs: `details` keys in `serde_json`'s map order, `at` written with nine fractional digits (`…T12:00:00.000000000Z`).
   - **`crates/server/tests/api_examples.rs`:**
     - `generated_files_roundtrip` gets `"list_org_audit_events" | "list_admin_audit_events" => roundtrip::<Page<AuditEvent>>(…)` arms, as the deletions page has, with imports;
     - `manifest_covers_the_read_operations` exempts `get_org_audit_export` and `get_admin_audit_export`, as it exempts `get_dataset_export`.
   - **`crates/server/tests/openapi.rs`:** add the four operation ids to `OPERATIONS`, `"audit"` to `TAGS`, the routes with query parameters to `QUERY_OR_BODY`, the two admin ids to the session-security list (the `list_admin_users` group), and their expected errors to the per-id `expected_errors` branches (lists: `[400, 401, 403, 404, 429]`; admin: `[400, 401, 403, 429]`; exports the same as their lists).
6. **Exports** (`ee/audit/export.rs`): the steps below.

**Tests** (`ee/audit/mod.rs` tests, with `TestApp::with_license(&["audit_log"])` (`test_support:~266`); members via `test_support::member_in_org`, PATs and keys through the existing helpers of `routes/tokens.rs` and `routes/api_keys.rs` tests, an erased user by `DELETE FROM users` on the writer):
- `admins_read_the_org_log_newest_first`;
- `members_and_api_keys_cannot_read` (403 member, 404 key);
- `without_the_feature_it_is_plan_required`;
- `filters_by_action_group_actor_target_project_outcome_and_time`;
- `invalid_action_filter_is_400`;
- `paging_by_cursor_never_skips_or_repeats` (250 records, limit 100: three pages, all seqs once);
- `former_members_and_deleted_users_show_no_email`;
- `the_instance_log_holds_only_instance_records`;
- `instance_admins_cannot_read_an_org_log_without_membership`;
- `instance_log_needs_the_licence_or_a_cloud_cell`;
- `details_never_carry_an_email` (over the example records).
- `instance_scope_resolves_no_org_object_names`.
- `instance_project_filter_matches_nothing`.

In `ee/audit/`, refer to the core module as `crate::audit` (`use crate::audit::{Ctx, Event, record_alone};`): a bare `audit::` there names `ee::audit`.

The generated `crates/server/openapi.json`, `fixtures/api/manifest.json` and `web/src/api/schema.gen.ts` are regenerated by the coordinator after this phase. `manifest_covers_the_read_operations` and the OpenAPI contract test fail until then: expected.

#### D3 step 6 in detail — exports

**Steps:**
1. Two handlers, with `export_org` mirroring D3's gate and `export_instance` the instance gate:

   ```rust
   pub(crate) async fn export_org(State(state): State<AppState>, Extension(access): Extension<OrgAccess>, ctx: audit::Ctx, error::Query(params): error::Query<AuditExportParams>) -> Result<Response, ApiError>;
   pub(crate) async fn export_instance(State(state): State<AppState>, ctx: audit::Ctx, error::Query(params): error::Query<AuditExportParams>) -> Result<Response, ApiError>;
   ```
2. Order: the gate (`require(…, Feature::AuditLog)` or the P-5 instance gate), then `read::filters` (400 on bad input), then `audit::record_alone(&state.meta, &ctx, Event::new(AuditAction::ExportStarted { format, filters }), now)` (with `.org(org)` for the org export, none for the instance), then the stream. A failed record refuses the export, and a refused or invalid request writes nothing. The export's first page may include its own `audit.export_started` record: intended.
3. **The stream:**
   - `futures::stream::try_unfold` over a cursor struct `{ before_seq: Option<i64>, written: u64, header_due: bool, done: bool }`;
   - each step reads `read::page(pool, scope, filters, before_seq, 1000)` on `state.meta.reader().clone()` and `read::resolve`, then encodes;
   - it stops when a page is short, or when `written` reaches `MAX_EXPORT_ROWS = 1_000_000`;
   - errors log `tracing::error!(error = %error, "audit export failed")` and end the body with `io::Error::other("audit export failed")`, as the dataset export does.
4. **CSV:**
   - `csv::WriterBuilder::new().terminator(csv::Terminator::CRLF)`;
   - header once: `at,action,outcome,actor_kind,actor_id,actor_name,actor_email,actor_status,via,credential_id,target_kind,target_id,target_name,target_email,target_status,project_id,project_name,ip,user_agent,details`;
   - `at` is RFC 3339 with milliseconds, UTC (chrono, as `backup.rs:~549`); `details` is the compact JSON;
   - every cell passes through `fn safe_cell(value: &str) -> Cow<str>` (P-6).
5. **NDJSON:** one `AuditEvent` per line (`serde_json::to_writer` + `\n`).
6. **Response:**
   - `Content-Type` `text/csv; charset=utf-8` or `application/x-ndjson`;
   - `Content-Disposition: attachment; filename="audit-events-<YYYYMMDDTHHMMSSZ>.<csv|ndjson>"`, which matches the web's `[A-Za-z0-9._-]+` check;
   - `Cache-Control: no-store`.

**Tests:**
- `csv_export_has_the_header_and_escapes_formulas` (a user agent `=cmd()` becomes `'=cmd()`);
- `ndjson_export_lines_parse_as_audit_events`;
- `export_is_recorded_before_streaming` (`audit.export_started` with `filters` names only);
- `export_respects_filters`;
- `export_without_the_feature_is_plan_required`;
- `export_stops_at_the_cap` (the cap is `#[cfg(not(test))] 1_000_000` / `#[cfg(test)] 1_500`; 2,500 rows written → exactly 1,500 exported, which truncates mid-page).

**Done when:** committed (with step 6).

**Commit:** `Serve and export the audit log`

---

### D5 — Audit log pages

**Goal:** the org and admin pages.

**Prerequisite:** the coordinator has run `just openapi` and `just api-examples` after D3.

**Edit:**
- `web/src/components/settings/settings-nav.ts`, its test, and `settings-home.tsx` (`visibleOrgNav`, ~30, the only consumer that filters `ownerOnly`);
- `web/src/routes/$org/settings/audit-log.tsx` (new) and `audit-log.test.tsx` (new);
- `web/src/routes/admin/audit-log.tsx` (new) and test (new), and `web/src/routes/admin/route.tsx`;
- `web/src/ee/audit/audit-log-view.tsx` (new, SPDX Elastic-2.0), `web/src/ee/audit/audit-export-menu.tsx` (new), `web/src/ee/audit/audit-record-sheet.tsx` (new);
- `web/src/lib/search/audit.ts` (new) and test (new);
- `web/src/components/plan/feature-label.ts`, `web/src/ee/license-card.tsx`, `web/src/routes/$org/settings/plan.tsx` and its test.

**Steps:**
1. **Nav:**
   - an `adminOnly` flag on org nav items. The array is `as const`, so give **every** org item `adminOnly: false` except the new one, which has `true`. `visibleOrgNav` also hides `adminOnly` items unless `atLeast(role, "admin")` (`@/lib/roles`);
   - the new item `{ to: "/$org/settings/audit-log", label: "Audit log", icon: ScrollText, description: "Who did what, and when", keywords: ["audit", "log", "security", "compliance"], ownerOnly: false, adminOnly: true }` goes after "Provisioning";
   - `settings-nav.test.ts` fixtures that build items with `ownerOnly` also get `adminOnly`.
2. **`lib/search/audit.ts`:**
   - `parseAuditSearch(search)` returns `{ range: "24h" | "7d" | "30d" | "90d" | "all", action?: string, actor?: string, target?: string, outcome?: "success" | "failure", open?: string }`, using `lib/search/common.ts` helpers (`parseEnum`, `parseString(value, 200)`); the default range is `7d`;
   - `auditQuery(search, now)` maps it to the API's query (`from` = now − range, as RFC 3339; no `from` for `all`). The view computes `now` once per search change (`useMemo` keyed on the search, rounded down to the minute), so the query key is stable between renders.
3. **`AuditLogView`** (`{ scope: { org: string } | "instance" }`):
   - `$api.useInfiniteQuery` on the list route with `pageParamName: "cursor"`;
   - filters above the table: a range select, an action text input (debounced 300 ms, a comma list, with placeholder `member., auth.login`), actor id, target id, and an outcome select;
   - the table (`Table*`): time (`TimeText`), actor (name, email or the status label with `IdText`), action, target, IP;
   - "Load more" when there is a next page;
   - a row click sets `open` (the record's id) and shows `AuditRecordSheet`: every field, details as `<pre>` JSON, ids with `CopyButton`;
   - empty state "No records match these filters.";
   - status labels: `former_member` → "Former member"; `deleted` → "Deleted user" for a user, "Deleted key", "Deleted project" or "Deleted provider" by kind; `unresolved` → the kind.
4. **`AuditExportMenu`:** a dropdown with "Export CSV" and "Export NDJSON", using `api.GET(path, { params, parseAs: "blob" })`, `saveBlob` and `exportName`, as `components/datasets/export-menu.tsx` does. Write one `api.GET` call per literal path (org or admin) in an `if`, so that openapi-fetch types each.
5. **Pages:**
   - `$org/settings/audit-log.tsx`: `SettingsPage` titled "Audit log", description "Records of sign-ins, membership, keys, settings and data exports in this organization.". Admins and owners see `<LockedFeature feature="audit_log" …/>` when `useOrgEntitlements(org).locked("audit_log")`, else the view. Others see "Only admins and owners can read the audit log.".
   - `admin/audit-log.tsx`: the `AdminTab` union gains `/admin/audit-log`, with the tab "Audit log". The page shows the view with `scope="instance"`, and on a `plan_required` problem, "The instance audit log needs an enterprise licence that lists audit_log." with a link to the Settings tab.
6. **Feature labels:**
   - `featureOffer` gets `case "audit_log": return "Available with Enterprise or an enterprise licence";`;
   - `FEATURE_NAMES` in `license-card.tsx` and `plan.tsx` get "Audit log", and `plan.tsx`'s `FEATURES` list gets `audit_log`;
   - `plan.tsx`'s limits table gets "Audit retention": "Instance default" when absent, "Kept forever" for 0, else `${n} days`.

**Tests** (vitest, MSW `server.use`):
- `audit-log.test.tsx`: `admin sees records with names and the former-member label`, `filters go to the URL and the query`, `opening a record shows its details`, `member sees the note`, `locked without the feature`, `export downloads a CSV`.
- admin test: `instance admin sees the instance log`, `plan_required shows the licence note`.
- `audit.test.ts`: the parser's defaults and bounds.
- `settings-nav.test.ts`: `adminOnly` items are hidden from members.
- The plan page test: the audit retention row.

**Done when:** committed.

**Commit:** `Add the audit log pages`

---

### D6 — Docs

**Edit:**
- `website/src/content/docs/docs/platform/audit-log.mdx` (new), and its sidebar entry in `website/astro.config.mjs` (next to `sso`, ~124);
- `website/src/content/docs/docs/platform/plans-and-licence.mdx`;
- `website/src/content/docs/docs/self-hosting/configuration-reference.md` (the `audit.retention_days` description now mentions the org limit, which replaces it per org, 0 meaning kept forever);
- `docs/spec/04-metadata-db.md`, `docs/spec/08-auth.md`, `docs/spec/09-api.md`, `docs/spec/11-ui.md`, `docs/spec/12-operations.md`;
- `docs/plans/ea-enterprise-admin/README.md`, `docs/mvp-decisions.md` (D-P-12, D-P-13 lists);
- `CHANGELOG.md`.

**Steps:**
1. **`audit-log.mdx`:**
   - what is recorded (the catalog groups) and what is not (data work);
   - privacy (ids stored, names resolved on read, former members);
   - retention (365 days, `audit.retention_days`; the org limit on the cloud, where 0 keeps records forever; a lapsed org returns to the instance value);
   - reading, filtering, exporting (the cap) and the instance log;
   - plans: Enterprise and the licence.

   Code blocks are marked untested per `check-samples.mjs`.
2. **`plans-and-licence.mdx`:** the audit log in the licence and Enterprise feature lists, and `audit_retention_days` in the limits paragraph.
3. **Specs:**
   - 08: § Audit records gains reading, `ViewAudit` and the gates.
   - 09: the four routes.
   - 11: the two pages and the `adminOnly` nav flag.
   - 12: `audit_retention_days` in the limits list, and the per-org purge.
   - 04: no change unless an index was added.
4. **Design:**
   - in § Record format, drop `device` from `via` (M1 records `session` and `pat` only; device approvals are session actions);
   - § Read API: the `seq` cursor and the response shape (`actor` and `target` with `status`).

   **Decisions:** add `audit_log` to D-P-12's names and `audit_retention_days` to D-P-13's limits, each with "(D-EA-19)" or "(D-EA-7)".
5. **`CHANGELOG.md` `Added`:** "Audit log pages and API for organization admins and instance admins, with filters and CSV/NDJSON export (Enterprise and the self-hosted licence)."

**Commit:** `Document the audit log`

---

## After the phases (coordinating session)

1. Run `just openapi` and `just api-examples` after D3, and commit.
2. Review the diffs. Run `just check` once (Node on the PATH).
3. Fix phase; a Sonnet code review of M3.
4. Roadmap: "M3 implemented".
5. Request to track Q: the `audit_retention_days` limit and the `audit_log` feature for Enterprise (the design's § Requests).
