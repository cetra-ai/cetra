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
- **P-4. Retention default.** `Defaults::from_config` also reads `[audit] retention_days`: `0` gives `None` (keep), otherwise `Some(days)`. So a missing token claim means the instance default, and a JSON `null` means keep. The purge applies `max(days, 30)`, so a token value below 30 counts as 30.
- **P-5. Instance gate.** The instance routes need `state.entitlements.license().state.active_features(now)` to contain `Feature::AuditLog`, or `config.entitlements.control_plane` to be true (a cloud cell, D-EA-19).
- **P-6. CSV safety.** A CSV cell that starts with `=`, `+`, `-`, `@`, a tab or a carriage return is prefixed with `'`. User agents and names are attacker-chosen.
- **P-7. Export cap.** 1,000,000 rows, streamed in pages of 1,000 (reader pool), names resolved per page. The stream stops after the cap; the docs say so.
- **P-8. Nav.** Org settings nav items get an optional `adminOnly` flag (shown to admins and owners), like the project nav's.

## Versions

No new dependency. `csv` 1.4 is already a dependency of `cetra-server`.

## Phase graph

```
D1 (perm, catalog) ──┐
D2 (feature, limit, purge) ──┼── D3 (read routes) ── D4 (export) ── D5 (web) ── D6 (docs)
```

- D1 and D2 run in parallel; their files are disjoint.
- D3 creates `ee/audit/mod.rs` in its final form, including the export routes whose handlers D4 writes. D4 starts after D3, because both depend on the read query.
- D5 needs `just openapi` (coordinator) after D4.
- D6 runs alongside D5.

**Shared rules:** M1's shared rules, plus P-8 of M5 (clone state into closures). `ee/` files start with `// SPDX-License-Identifier: Elastic-2.0`.

---

### D1 — `ViewAudit` and the `audit.export_started` action

**Goal:** the permission and the audit action of exports.

**Edit:** `crates/server/src/auth/roles.rs`, `crates/server/src/audit/catalog/audit.rs` (new), `crates/server/src/audit/catalog/mod.rs`, `crates/server/src/audit/tests.rs`.

**Steps:**
1. `OrgPerm::ViewAudit` with minimum role `Admin` (in the `Admin` group of `min_role`), and one assert in `permissions_follow_the_spec_table`.
2. `audit/catalog/audit.rs` `AuditAction` (group `audit`), one variant:

   ```rust
   #[serde(rename = "audit.export_started")]
   ExportStarted { format: &'static str, filters: Vec<&'static str> },
   ```

   - `format` is `csv` or `ndjson`.
   - `filters` holds the names of the query filters that were set (`from`, `to`, `action`, `actor_id`, `target_id`, `project_id`, `outcome`), never their values.

   Wire it in `catalog/mod.rs` (`mod`, `pub(crate) use`, `Action::Audit`, `From`) and add a sample and the `audit` prefix to `audit/tests.rs`.

**Done when:** committed.

**Commit:** `Add ViewAudit and the audit.export_started action`

---

### D2 — The `audit_log` feature, the retention limit and the per-org purge

**Goal:** the feature exists, orgs carry `audit_retention_days`, and the purge applies it.

**Edit:** `crates/server/src/api/entitlements.rs`, `crates/server/src/entitlements/mod.rs`, `crates/server/src/entitlements/tokens.rs`, `crates/token/src/lib.rs`, `crates/server/src/serve.rs`, `crates/server/src/test_support/mod.rs` (the `Defaults::from_config` call only), `crates/server/src/audit/retention.rs`, `crates/cli/src/main.rs`.

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
   - `Defaults::from_config(config: &EntitlementDefaults, audit: &crate::config::AuditConfig)` sets it to `(audit.retention_days != 0).then_some(u64::from(audit.retention_days))` (P-4);
   - update both call sites (`serve.rs:~431`, `test_support/mod.rs:~361`) to pass `&config.audit`;
   - `merge_limits` (`entitlements/tokens.rs`) merges it like the others, with tests for value, `null` and missing;
   - every `Limits { … }` literal in the crate gets the field: search `Limits {` and `OrgGrant {`.
4. **`OrgLimits.audit_retention_days`** (`api/entitlements.rs`), `#[serde(default, skip_serializing_if = "Option::is_none")]`, and the `From<Limits>` mapping.
5. **`crates/cli/src/main.rs` `limit_rows`:** add `("audit_retention_days", limits.audit_retention_days)`, bump the array length, and update its test.
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
     2. `SELECT DISTINCT org_id FROM audit_events WHERE org_id IS NOT NULL` on the reader.
     3. For each org, `state.entitlements.resolve(org, now).limits.audit_retention_days`. `Some(d)` → `purge_scope(Some(org), max(d, 30) as u32 (saturating), now)`.
   - `purge_scope` batches like today, adding `AND org_id IS ?` (bind NULL for the instance).
   - `serve.rs` `spawn_audit_purge` calls `purge(&state, now)`.
   - Tests:
     - `an_orgs_limit_overrides_the_instance_default` (via `test_support::set_limits`);
     - `a_limit_below_30_counts_as_30`;
     - `a_null_limit_keeps_everything`;
     - `the_instance_log_uses_the_config`.
     - The existing tests adapt to the new signature.

**Done when:** committed.

**Commit:** `Add the audit_log feature and per-org audit retention`

---

### D3 — Read routes

**Goal:** `GET` the org and instance logs with filters, paging and resolved names.

**Edit:** `crates/server/src/ee/mod.rs` (`pub(crate) mod audit;`), `crates/server/src/ee/audit/mod.rs` (new), `crates/server/src/ee/audit/read.rs` (new), `crates/server/src/api/audit.rs` (new), `crates/server/src/api/mod.rs`, `crates/server/src/api/openapi.rs`, `crates/server/src/http/access.rs` (one `routes.extend(crate::ee::audit::routes());` line), `crates/server/src/api_examples.rs`, `fixtures/api/static/org-audit-events.json` (new), `fixtures/api/static/admin-audit-events.json` (new).

**Steps:**

1. **DTOs** (`api/audit.rs`). All derive `Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema`, with serde `snake_case`:

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

   `AuditExportParams` is the same struct without `limit` and `cursor`, plus `format: AuditExportFormat` (`csv` | `ndjson`, required). D4 uses it; define it here so the OpenAPI is complete.

   - `from` is inclusive and `to` exclusive, as `UserListParams` documents.
   - `actor_id` and `target_id` are at most 64 characters (`field_error` otherwise).
   - `project_id` is accepted on the instance route but matches nothing there: instance records have no project.
2. **`ee/audit/read.rs`:**

   ```rust
   pub(crate) enum Scope { Org(OrgId), Instance }
   pub(crate) struct Filters { pub(crate) from: Option<i64>, pub(crate) to: Option<i64>, pub(crate) actions: Vec<ActionMatch>, pub(crate) actor_id: Option<String>, pub(crate) target_id: Option<String>, pub(crate) project_id: Option<ProjectId>, pub(crate) outcome: Option<&'static str> }
   pub(crate) enum ActionMatch { Exact(String), Prefix(String) }
   pub(crate) struct Row { /* every audit_events column, typed */ }
   pub(crate) fn filters(params: &AuditListParams-like fields) -> Result<Filters, ApiError>; // P-2 validation
   pub(crate) async fn page(pool: &SqlitePool, scope: &Scope, filters: &Filters, before_seq: Option<i64>, limit: u32) -> Result<Vec<Row>, MetaError>;
   pub(crate) async fn resolve(pool: &SqlitePool, scope: &Scope, rows: &[Row]) -> Result<Vec<AuditEvent>, MetaError>;
   pub(crate) fn encode_cursor(seq: i64) -> String;
   pub(crate) fn decode_cursor(text: &str) -> Result<i64, ApiError>; // invalid → field_error("cursor", "invalid cursor")
   ```

   - **`page`** builds `SELECT … FROM audit_events WHERE <scope> AND …` with `sqlx::QueryBuilder`:
     - scope `org_id = ?` or `org_id IS NULL`;
     - `at >= ?`, `at < ?`;
     - `(action = ? OR action LIKE ? ESCAPE '\' …)`, where a prefix's `_` is escaped as in `meta/datasets.rs:~271`;
     - `actor_id = ?`, `target_id = ?`, `project_id = ?`, `outcome = ?`;
     - `seq < ?`;
     - `ORDER BY seq DESC LIMIT ?`.
   - **`resolve`** (P-3):
     - collect user ids from user actors and `user` targets, and fetch `id, name, email` from `users WHERE id IN (…)`;
     - for an org scope, also fetch `user_id FROM org_members WHERE org_id = ? AND user_id IN (…)`;
     - fetch `api_keys.name` for `api_key` actors and targets, `projects.name` for project refs (the row's `project_id` and `project` targets), and `org_sso_providers.display_name` for `sso_provider` targets.
     - Status rules:
       - **User:** no `users` row → `deleted`, no name or email. Org scope and not a member → `former_member`, no name or email. Otherwise `present` with name and email.
       - **Key, project, provider:** a row → `present` with its name (no email); none → `deleted`.
       - **Every other kind:** `unresolved`, id only.
     - `details` is parsed from the stored JSON. A parse failure gives `{}` and logs `tracing::warn!(id = %id, "audit details do not decode")`.
     - Ids are bound as text. Use `QueryBuilder` with `separated(", ")`, as `meta/users.rs:~503` `providers_by_user` does.
3. **Routes** (`ee/audit/mod.rs`, SPDX header, final form, with the export handlers that D4 writes in `export.rs`):

   ```rust
   pub(crate) mod export;
   pub(crate) mod read;
   pub(crate) fn routes() -> Vec<RouteDef> // the four below
   ```

   - `GET /api/v1/orgs/{org}/audit-events`: `Access::Org { perm: OrgPerm::ViewAudit, session_only: false }`, documented. Handler `list_org`: `require(&state, org, Feature::AuditLog)?`, then `filters`, `decode_cursor`, `page(limit + 1)`, `resolve`, and `Page { data, next_cursor }` (the next cursor is the last kept row's seq when more rows exist).
   - `GET /api/v1/orgs/{org}/audit-events/export`: same access; handler `export::export_org` (D4).
   - `GET /api/v1/admin/audit-events`: `Access::InstanceAdmin`, documented. Handler `list_instance`: the P-5 gate (else `ApiError::PlanRequired { detail: Feature::AuditLog.locked_detail() }`), then the same steps with `Scope::Instance`.
   - `GET /api/v1/admin/audit-events/export`: `Access::InstanceAdmin`; handler `export::export_instance` (D4).

   Until D4 lands, `export.rs` does not exist. D3 and D4 are sequential, and checks run at the end.
4. **OpenAPI** (`api/openapi.rs`):
   - add `tag("audit", "Audit records of an organization and of the instance.")`;
   - `audit_paths` for the four routes, modelled on `list_org_deletions` and `list_admin_users`. The exports use a `ResponseBuilder` with `text/csv` and `application/x-ndjson` string content, as `export_result()` does. The 403 text mentions `plan_required`.
   - Schemas: `AuditEvent`, `AuditActor`, `AuditRef`, `AuditOutcome`, `AuditRefStatus`, `AuditExportFormat`, plus `insert_page::<AuditEvent>("AuditEventPage")`.
5. **Examples:**
   - `api_examples.rs`: `example("list_org_audit_events", params(&[("org","acme")]), BTreeMap::new(), "static/org-audit-events.json", "application/json")` and `example("list_admin_audit_events", …, "static/admin-audit-events.json", …)`.
   - The two static files are canonical pretty JSON pages of 3 records each (one `member.role_changed` with a present actor, one `auth.login` from an anonymous actor with an IP, one with a `former_member` target), using the ids of `fixtures/api/README.md`'s fixed-identifier table. Extend the table with three audit event ids.

**Tests** (`ee/audit/mod.rs` tests, with `TestApp::with_license(&["audit_log"])` or the existing licence helper):
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

**Done when:** committed.

**Commit:** `Serve the audit log to admins and instance admins`

---

### D4 — Exports

**Goal:** CSV and NDJSON exports of the filtered records, recorded before they stream.

**Edit:** `crates/server/src/ee/audit/export.rs` (new).

**Steps:**
1. Two handlers, with `export_org` mirroring D3's gate and `export_instance` the instance gate:

   ```rust
   pub(crate) async fn export_org(State(state): State<AppState>, Extension(access): Extension<OrgAccess>, ctx: audit::Ctx, error::Query(params): error::Query<AuditExportParams>) -> Result<Response, ApiError>;
   pub(crate) async fn export_instance(State(state): State<AppState>, ctx: audit::Ctx, error::Query(params): error::Query<AuditExportParams>) -> Result<Response, ApiError>;
   ```
2. Before streaming, `audit::record_alone(&state.meta, &ctx, Event::new(AuditAction::ExportStarted { format, filters }), now)`, with `.org(org)` for the org export (no org for the instance). A failed record refuses the export.
3. **The stream:**
   - `futures::stream::try_unfold` over a cursor struct `{ before_seq: Option<i64>, written: u64, header_due: bool, done: bool }`;
   - each step reads `read::page(pool, scope, filters, before_seq, 1000)` on `state.meta.reader().clone()` and `read::resolve`, then encodes;
   - it stops when a page is short, or when `written` reaches `MAX_EXPORT_ROWS = 1_000_000`;
   - errors log `tracing::error!(error = %error, "audit export failed")` and end the body with `io::Error::other("audit export failed")`, as the dataset export does.
4. **CSV:**
   - `csv::WriterBuilder::new().terminator(csv::Terminator::CRLF)`;
   - header once: `at,action,outcome,actor_kind,actor_id,actor_name,actor_email,actor_status,via,credential_id,target_kind,target_id,target_name,target_status,project_id,project_name,ip,user_agent,details`;
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
- `export_stops_at_the_cap` (lower the cap through a `#[cfg(test)]` constant, e.g. 2,500 rows written and a cap of 2,000 → 2,000 rows).

**Done when:** committed.

**Commit:** `Export the audit log as CSV and NDJSON`

---

### D5 — Audit log pages

**Goal:** the org and admin pages.

**Prerequisite:** the coordinator has run `just openapi` and `just api-examples` after D4.

**Edit:**
- `web/src/components/settings/settings-nav.ts`, its test, `settings-home.tsx` and the other nav consumers that filter `ownerOnly` (search `ownerOnly` in `web/src`);
- `web/src/routes/$org/settings/audit-log.tsx` (new) and `audit-log.test.tsx` (new);
- `web/src/routes/admin/audit-log.tsx` (new) and test (new), and `web/src/routes/admin/route.tsx`;
- `web/src/ee/audit/audit-log-view.tsx` (new, SPDX Elastic-2.0), `web/src/ee/audit/audit-export-menu.tsx` (new), `web/src/ee/audit/audit-record-sheet.tsx` (new);
- `web/src/lib/search/audit.ts` (new) and test (new);
- `web/src/components/plan/feature-label.ts`, `web/src/ee/license-card.tsx`, `web/src/routes/$org/settings/plan.tsx` and its test.

**Steps:**
1. **Nav:**
   - an optional `adminOnly: true` on org nav items; every consumer that filters `ownerOnly` also hides `adminOnly` items unless the role is `admin` or `owner` (`atLeast(role, "admin")` from `@/lib/roles`);
   - the new item `{ to: "/$org/settings/audit-log", label: "Audit log", icon: ScrollText, description: "Who did what, and when", keywords: ["audit", "log", "security", "compliance"], adminOnly: true }` goes after "Provisioning".
2. **`lib/search/audit.ts`:**
   - `parseAuditSearch(search)` returns `{ range: "24h" | "7d" | "30d" | "90d" | "all", action?: string, actor?: string, target?: string, outcome?: "success" | "failure", open?: string }`, using `lib/search/common.ts` helpers (`parseEnum`, `parseString(value, 200)`); the default range is `7d`;
   - `auditQuery(search, now)` maps it to the API's query (`from` = now − range, as RFC 3339; no `from` for `all`).
3. **`AuditLogView`** (`{ scope: { org: string } | "instance" }`):
   - `$api.useInfiniteQuery` on the list route with `pageParamName: "cursor"`;
   - filters above the table: a range select, an action text input (debounced 300 ms, a comma list, with placeholder `member., auth.login`), actor id, target id, and an outcome select;
   - the table (`Table*`): time (`TimeText`), actor (name, email or the status label with `IdText`), action, target, IP;
   - "Load more" when there is a next page;
   - a row click sets `open` (the record's id) and shows `AuditRecordSheet`: every field, details as `<pre>` JSON, ids with `CopyButton`;
   - empty state "No records match these filters.";
   - status labels: `former_member` → "Former member", `deleted` → "Deleted user", `unresolved` → the kind.
4. **`AuditExportMenu`:** a dropdown with "Export CSV" and "Export NDJSON", using `api.GET(exportPath, { params, parseAs: "blob" })`, `saveBlob` and `exportName`, as `components/datasets/export-menu.tsx` does.
5. **Pages:**
   - `$org/settings/audit-log.tsx`: `SettingsPage` titled "Audit log", description "Records of sign-ins, membership, keys, settings and data exports in this organization.". Admins and owners see `<LockedFeature feature="audit_log" …/>` when `useOrgEntitlements(org).locked("audit_log")`, else the view. Others see "Only admins and owners can read the audit log.".
   - `admin/audit-log.tsx`: the `AdminTab` union gains `/admin/audit-log`, with the tab "Audit log". The page shows the view with `scope="instance"`, and on a `plan_required` problem, "The instance audit log needs an enterprise licence that lists audit_log." with a link to the Settings tab.
6. **Feature labels:**
   - `featureOffer` gets `case "audit_log": return "Available with Enterprise or an enterprise licence";`;
   - `FEATURE_NAMES` in `license-card.tsx` and `plan.tsx` get "Audit log";
   - `plan.tsx`'s limits table gets "Audit retention" (`unlimited` or `${n} days`).

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
- `website/src/content/docs/docs/platform/audit-log.mdx` (new), registered like `sso.mdx`;
- `website/src/content/docs/docs/platform/plans-and-licence.mdx`;
- `website/src/content/docs/docs/self-hosting/configuration-reference.md` (the `audit.retention_days` description now mentions the org limit);
- `docs/spec/04-metadata-db.md`, `docs/spec/08-auth.md`, `docs/spec/09-api.md`, `docs/spec/11-ui.md`, `docs/spec/12-operations.md`;
- `docs/plans/ea-enterprise-admin/README.md`, `docs/mvp-decisions.md` (D-P-12, D-P-13 lists);
- `CHANGELOG.md`.

**Steps:**
1. **`audit-log.mdx`:**
   - what is recorded (the catalog groups) and what is not (data work);
   - privacy (ids stored, names resolved on read, former members);
   - retention (365 days, `audit.retention_days`, the org limit on the cloud);
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

1. Run `just openapi` and `just api-examples` after D4, and commit.
2. Review the diffs. Run `just check` once (Node on the PATH).
3. Fix phase; a Sonnet code review of M3.
4. Roadmap: "M3 implemented".
5. Request to track Q: the `audit_retention_days` limit and the `audit_log` feature for Enterprise (the design's § Requests).
