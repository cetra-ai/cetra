# EA M2 — Audit catalog completion

**Summary.**
- This milestone records the rest of the catalog: the project and org objects (evaluators, online rules and backfills, score definitions, alert rules and channels, LLM connections, model prices, the assistant setting, prompts, datasets, issue sources, dashboards, annotation queues) and data leaving the product (SQL runs and exports, dataset exports, MCP tool calls).
- The remaining `target: "audit"` lines that are data work become ordinary `info` lines, so `target = "audit"` means the catalog and nothing else.
- When it is done, no `target: "audit"` line is left outside `audit::record`.

**Prerequisites.**
- M1 is committed and green (`just check-rust` passed after its fix phase).
- Branch `claude/ea-enterprise-admin` in `/home/sebastien/travaux/llm-observer/.claude/worktrees/ea-design`.
- M5 may run at the same time on other files (§ Phase graph).

**Read first.**
- `docs/plans/ea-enterprise-admin/README.md` § Event catalog, § Writing, § Privacy.
- `docs/plans/ea-enterprise-admin/01-m1-audit-recording.md`: shared rules, P-1 (record last before the success `Ok`), and the A1 API.
- `crates/server/src/audit/mod.rs` and `crates/server/src/audit/catalog/` (the API you extend).
- The facts file `/home/sebastien/.config/Claude-extra/side-session-notes/local_1c672712-2cc7-40e9-8d6b-d582417356b7/ea-m2-code-facts.md`: every site, its closure and its success expression. Phases cite its sections (A1–A11, B, C, D, E). Lines may have moved: locate by function name.

## What it is, and what it is not

**What works when it's done:**
- Creating, changing or deleting any object listed in § Catalog writes one record, in the transaction of the change.
- A worker pausing or resuming an online rule or an issue source writes a record with actor `system`.
- A SQL console run or export, a dataset export, and an MCP tool call that names a project each write a record **before** any data is read. The record carries a SHA-256 of the query text, never the text.
- Data work (dataset items, imports, adds from traces, version restores, experiments, issue edits and merges, refits, evaluator runs and tests, connection and channel tests, playground runs, assistant conversations and turns) keeps its log line, without the `audit` target.

**Out of scope:**
- reading the records (M3);
- the stream (M4);
- saved views, which are personal UI state;
- agent contexts, which are ingest-side;
- demo-org visitors' SQL runs (see P-4).

## Decisions this plan relies on

| ID | Summary |
|---|---|
| D-EA-2 | Same transaction; actions with no `meta.db` change record before they run; one record per bulk request |
| D-EA-4 | Typed catalog; access, configuration, deletions and data leaving the product; not data edits |
| D-EA-5 | Allow-listed details: field names, never values; never names, URLs, SQL text or payload |
| D-EA-6 | SQL runs (SHA-256 of the text), exports and MCP tool calls are recorded before they run |
| M1 P-1, P-2 | Record last before the success `Ok`; one catalog file per name prefix |

**New decisions** (recorded by B6 as D-EA-21 to D-EA-23 in the design and `docs/mvp-decisions.md`; they apply the user's choice of D-EA-6 to cases the design did not list):
- **D-EA-21 (P-1 below).** What is recorded beyond the design's list:
  - score definitions;
  - backfills, which send history to a provider;
  - prompt versions and labels: label names are configuration identifiers (`production`), not personal names, so D-EA-5 allows them;
  - manual resumes, and worker pauses and resumes of online rules (actor `system`).
- **D-EA-22 (P-3).** MCP tool calls are recorded when the tool names a project; `list_projects` is not.
- **D-EA-23 (P-4).** SQL runs by demo-org visitors are not recorded.

**Plan-level choices** (detail the design leaves to the plan):
- **P-1. Classification.** The design's rule: configuration and access are recorded, data work is not.
  - Recorded:
    - score definitions;
    - backfills, which send history to a provider;
    - manual and system pauses and resumes of online rules (a state change of a configured object; workers record as `system`, like M1's domain lapse). Issue-source pauses and resumes stay plain `info`: their store functions do not report whether anything changed (`issues::pause_source` returns `()` and is a no-op on a paused source; `clear_budget_pauses` returns a count).
    - prompt versions and labels.
  - Not recorded, kept as plain `info`:
    - experiments;
    - issue edits, merges and refits;
    - dataset items, imports, adds from traces and version restores;
    - saved views;
    - evaluator runs and tests;
    - connection and channel tests;
    - playground runs;
    - assistant conversations, feedback and turns.

  P6 corrects the design's "a dataset import" bulk example, which contradicts its own "Not in the catalog" list.
- **P-2. Changed fields.** An update records `fields`: the JSON names of the request body fields that were present (`Some`), never their values.
  - Compute this `Vec<&'static str>` at the top of the handler, before the body is moved or destructured, in a local named `changed_fields` (`update_evaluator` already has a local `fields`).
  - Dashboards, annotation queues and model prices are replaced as whole documents, so their `Updated` variants carry no field list.
- **P-6. Where to record.** Record in the handler's own write closure, after the helper returns a success, without changing helper signatures:

  ```rust
  let out = helper(conn, …).await?;
  if let Outcome::Done(row) = &out { audit::record(conn, &ctx, event, now).await?; }
  Ok(out)
  ```

  This avoids `too_many_arguments` and breaking the helpers' test callers. The only signature changes this plan makes are `prices::write` (gets `ctx: &audit::Ctx`) and the nine MCP tool functions (get `audit: &audit::Ctx`).
- **P-7. Ctx in closures.** A `meta.write` closure cannot borrow: clone the ctx before it (`let ctx = ctx.clone();`, or `audit.clone()` from a `&Ctx` parameter) and move the clone in.
- **P-8. Time.** `now` is the handler's `state.clock.now_ns()`, or the `now` the surrounding worker code already uses.
- **P-3. MCP hook.** Each tool that names a project records `mcp.tool_called` right after its `user_project(..)` call succeeds, before any read. That covers 9 call sites. `list_projects` names no project and is not recorded. `McpServer` gets an `audit: audit::Ctx` field built in the `mcp` handler, so records carry the PAT, IP and user agent.
- **P-4. Demo visitors.** SQL runs by a demo-org visitor (`visitor.is_some()` in `run_sql`) are not recorded. The demo org is public, and recording anonymous visitors would only grow the table.
- **P-5. Numbers.** Counts and versions are stored as `u32`. A wider source value is converted with `u32::try_from(v).unwrap_or(u32::MAX)`.

## Versions

No new dependency. `sha2` 0.11 is already a dependency of `cetra-server` (`use sha2::{Digest, Sha256}` as in `auth/secret.rs`).

## Phase graph

```
B1 (catalog) ──┬── B2 (evaluation)
               ├── B3 (alerts, connections, prices, assistant)
               ├── B4 (prompts, datasets, experiments, issues)
               ├── B5 (dashboards, queues, SQL, MCP)
               └── B6 (docs)        [after B2–B5]
```

- B2 to B5 run in parallel after B1. Their **Edit** lists are disjoint.
- M5's catalog phase (it adds `audit/catalog/scim.rs` and `TargetKind` variants) runs after B1, never with it. M5's other phases may run at the same time. M5 owns `routes/members.rs`, `routes/invites.rs`, `ee/**`, `auth/**`, `http/**` and `meta/**` (except files named here). M2 owns `audit/catalog/**` and the files below. Neither touches the other's files.

**Shared rules** (in every brief): M1's shared rules, unchanged. Notably:
- no compile, test, lint or format;
- commit only your files with `git commit -- <files>`;
- remove bindings that only fed a deleted line;
- `audit::Ctx` is an extractor that never fails, so add `ctx: audit::Ctx` to a handler rather than `Extension(principal)`;
- record inside the closure, last before the success `Ok`, never on a not-found, conflict or refused branch.

---

### B1 — The M2 catalog

**Goal:** the new action groups and target kinds, with the M1 tests extended.

**Edit:** `crates/server/src/audit/catalog/mod.rs`, new files under `crates/server/src/audit/catalog/` (one per group below), `crates/server/src/audit/mod.rs` (`TargetKind` only, plus removing `#![allow(dead_code)]` and its comment), `crates/server/src/audit/tests.rs`.

**Steps:**

1. Add one file per group, in the M1 style:
   - `#[derive(Debug, Clone, PartialEq, serde::Serialize)]`;
   - `#[serde(tag = "action", content = "details")]`;
   - `#[serde(rename = "<group>.<name>")]` per variant;
   - `#[serde(rename_all = "snake_case")]` on struct variants.

   Add each to `catalog/mod.rs`'s `mod`/`pub(crate) use` lines, a variant of `Action` (appended after the existing ones), and an `impl From<…> for Action`.

   | File / enum | Variant | Name | Fields |
   |---|---|---|---|
   | `evaluator.rs` `EvaluatorAction` | `Created { kind: &'static str, url_host: Option<String> }` | `evaluator.created` | `kind` from `EvaluatorKind::as_str`; `url_host` only for webhooks (the host, never the URL) |
   | | `Updated { fields: Vec<&'static str>, url_host: Option<String> }` | `evaluator.updated` | |
   | | `Deleted` | `evaluator.deleted` | |
   | `online_rule.rs` `OnlineRuleAction` | `Created { evaluator_id: uuid::Uuid }` | `online_rule.created` | |
   | | `Updated { fields: Vec<&'static str>, budget_resumed: bool }` | `online_rule.updated` | |
   | | `Deleted` | `online_rule.deleted` | |
   | | `Paused { reason: &'static str }` | `online_rule.paused` | the pause constant the code stores (`PAUSE_*` in `eval/`), as is |
   | | `Resumed { reason: &'static str }` | `online_rule.resumed` | `manual` for the resume route; `budget period` for the budget worker (today's spelling) |
   | | `BackfillStarted { backfill_id: uuid::Uuid, max_targets: u32 }` | `online_rule.backfill_started` | |
   | `score_definition.rs` `ScoreDefinitionAction` | `Created { data_type: &'static str }` | `score_definition.created` | |
   | | `Updated { fields: Vec<&'static str> }` | `score_definition.updated` | |
   | | `Archived` | `score_definition.archived` | |
   | `alert_rule.rs` `AlertRuleAction` | `Created { metric: &'static str, enabled: bool }` | `alert_rule.created` | |
   | | `Updated { fields: Vec<&'static str> }` | `alert_rule.updated` | |
   | | `Deleted` | `alert_rule.deleted` | |
   | `alert_channel.rs` `AlertChannelAction` | `Created { kind: &'static str }` | `alert_channel.created` | never URL or recipients |
   | | `Updated { fields: Vec<&'static str> }` | `alert_channel.updated` | |
   | | `Deleted` | `alert_channel.deleted` | |
   | `llm_connection.rs` `LlmConnectionAction` | `Created { adapter: &'static str }` | `llm_connection.created` | |
   | | `Updated { fields: Vec<&'static str>, key_changed: bool }` | `llm_connection.updated` | |
   | | `Deleted` | `llm_connection.deleted` | |
   | `model_price.rs` `ModelPriceAction` | `Created` / `Updated` / `Deleted` | `model_price.created` / `.updated` / `.deleted` | scope is the event's org and optional project |
   | `assistant.rs` `AssistantAction` | `SettingChanged { enabled: bool, connection_id: Option<uuid::Uuid> }` | `assistant.setting_changed` | never the model name |
   | `prompt.rs` `PromptAction` | `Created` | `prompt.created` | |
   | | `Updated { fields: Vec<&'static str> }` | `prompt.updated` | |
   | | `Deleted` | `prompt.deleted` | |
   | | `VersionCreated { version: u32 }` | `prompt.version_created` | |
   | | `LabelSet { label: String, version: u32 }` | `prompt.label_set` | labels are configuration (`production`), not payload |
   | | `LabelRemoved { label: String }` | `prompt.label_removed` | |
   | | `ProtectedLabelsChanged { count: u32 }` | `prompt.protected_labels_changed` | |
   | `dataset.rs` `DatasetAction` | `Created` | `dataset.created` | |
   | | `Updated { fields: Vec<&'static str> }` | `dataset.updated` | |
   | | `Deleted` | `dataset.deleted` | |
   | | `ExportStarted { version: u32, format: &'static str }` | `dataset.export_started` | `version_of` returns `i64`; convert by P-5 |
   | `issue_source.rs` `IssueSourceAction` | `Created { kind: &'static str }` | `issue_source.created` | |
   | | `Updated { fields: Vec<&'static str> }` | `issue_source.updated` | |
   | | `Deleted` | `issue_source.deleted` | |
   | `dashboard.rs` `DashboardAction` | `Created` / `Updated` / `Deleted` | `dashboard.created` / `.updated` / `.deleted` | |
   | | `OverviewSaved { created: bool }` | `dashboard.overview_saved` | |
   | | `OverviewReset` | `dashboard.overview_reset` | |
   | `annotation_queue.rs` `AnnotationQueueAction` | `Created` / `Updated` / `Deleted` | `annotation_queue.created` / `.updated` / `.deleted` | not items |
   | `sql.rs` `SqlAction` | `Run { query_sha256: String }` | `sql.run` | lowercase hex SHA-256 of the query text |
   | | `ExportStarted { query_sha256: String, format: &'static str }` | `sql.export_started` | `csv`, `ndjson` or `parquet` |
   | `mcp.rs` `McpAction` | `ToolCalled { tool: &'static str, query_sha256: Option<String> }` | `mcp.tool_called` | the hash only for `query_sql` |

2. In `audit/mod.rs` `TargetKind`, add variants and their `as_str` spellings:
   - `Evaluator` → `evaluator`, `OnlineRule` → `online_rule`, `ScoreDefinition` → `score_definition`;
   - `AlertRule` → `alert_rule`, `AlertChannel` → `alert_channel`, `LlmConnection` → `llm_connection`, `ModelPrice` → `model_price`;
   - `Prompt` → `prompt`, `Dataset` → `dataset`, `IssueSource` → `issue_source`;
   - `Dashboard` → `dashboard`, `AnnotationQueue` → `annotation_queue`.

   Update the doc comment's list.
3. In `audit/mod.rs`, add `pub(crate) use crate::auth::secret::hash_token as sha256_hex;`. `hash_token` is exactly the lowercase hex SHA-256 of its input. Also add:

   ```rust
   /// The org and project of an online rule, for worker records.
   pub(crate) async fn scope_of_rule(conn: &mut sqlx::SqliteConnection, rule: uuid::Uuid) -> Result<Option<(OrgId, ProjectId)>, MetaError>
   ```

   It runs `SELECT p.org_id, p.id FROM online_rules r JOIN projects p ON p.id = r.project_id WHERE r.id = ?` (check the table and column names in `crates/server/migrations/meta/0010_eval_queue.sql` or wherever `online_rules` is created), parsing the two ids as `meta` does elsewhere. A parse failure is `MetaError::Corrupt { table: "projects", column: "id" }`.
4. Remove `#![allow(dead_code)]` and its comment from `audit/mod.rs`: M1 gave every item a caller.
5. `audit/tests.rs`:
   - extend `samples()` with one value of every new variant;
   - extend the group-prefix mapping of `every_action_name_is_group_dot_snake` with the new groups;
   - add `sha256_hex_matches_a_known_vector` (`"abc"` → `ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad`);
   - add `scope_of_rule_finds_the_project_and_org` and `scope_of_rule_of_an_unknown_rule_is_none`. The test seeds a project and rule with the existing `meta` insert helpers; use whatever helper `meta/online_rules.rs` tests use.

   `details_hold_no_forbidden_field` covers the new samples unchanged. `url_host`, `query_sha256`, `key_changed` and `label` are allowed.

**Done when:** committed.

**Commit:** `Add the project-object and data-export audit actions`

---

### B2 — Evaluation objects

**Goal:** evaluators, online rules, backfills, score definitions and worker pauses write records. Evaluator runs and tests lose the audit target.

**Edit:** `crates/server/src/routes/evaluators.rs`, `crates/server/src/routes/online_rules.rs`, `crates/server/src/routes/backfills.rs`, `crates/server/src/routes/score_definitions.rs`, `crates/server/src/routes/eval_jobs.rs`, `crates/server/src/eval/test_run.rs`, `crates/server/src/eval/dispatch.rs`, `crates/server/src/eval/execute.rs`.

**Steps** (facts § A1, A2, A6, B, C). The event for every project object is `.org(access.project.org_id).project(access.project.id).target(TargetKind::X, id)`:
1. **Evaluators:**
   - create: `EvaluatorAction::Created { kind, url_host: host }` on the `Updated(row)` success.
   - update: in the handler's closure, after `write_update(..)` returns `Outcome::Updated(..)` (P-6), record `Updated { fields: changed_fields, url_host }`.
   - delete: record in the `store::delete_evaluator(..) == true` branch.
   - Delete the three tracing lines.
2. **Online rules:**
   - create: in the handler's closure, when `insert_checked` returned `RuleWrite::Done{row, ..}`, record `Created { evaluator_id }` (P-6).
   - update: when `update_checked` returned `RuleWrite::Done{row, budget_resumed}`, record `Updated { fields: changed_fields, budget_resumed }`.
   - delete: record when `delete_rule` returned true.
   - resume: record `Resumed { reason: "manual" }` in the `Some(row)` arm.
   - Delete the four tracing lines, including the "online rule resumed / budget changed" line, which `budget_resumed: true` replaces.
3. **Backfills:** in the handler's closure, when `create_checked` returned `Create::Created(..)`, record `OnlineRuleAction::BackfillStarted { backfill_id, max_targets }`, target `(OnlineRule, rule_id)`. Delete the tracing line.
4. **Score definitions:** create and update as in facts § A6, recording on the row-returned branch. Archive: in the closure, read `SELECT archived_at IS NOT NULL FROM score_definitions WHERE id = ? AND project_id = ?` before `archive_tx` (check the column name in the migration), and record `Archived` only when it was not archived and `archive_tx` returned true. Delete the three lines.
5. **Evaluator runs and tests:** in `routes/eval_jobs.rs:~188` and `eval/test_run.rs:~318`, drop `target: "audit"`. Message and fields are unchanged.
6. **System pauses:**
   - `eval/execute.rs`: `audit_pause` is called at ~1192 and ~1213, after the writes that pause rules. The pause helpers (`meta/online_rules.rs` ~346, ~373, ~455) return the ids of the rules they paused.
     - In each write closure that calls one of those helpers, iterate the returned ids. For each, call `audit::scope_of_rule(conn, id)`; when `Some((org, project))`, record `OnlineRuleAction::Paused { reason }` with `audit::Ctx::system()`, `.org(org).project(project)`, target `(OnlineRule, id)`. This includes the breaker trip at ~1205, inside its own closure.
     - Then delete `audit_pause` and its calls.
     - Do not edit `meta/online_rules.rs` (M5 may be editing `meta/**`).
   - `eval/dispatch.rs` `budget_once`: inside its write closure, after `clear_budget_pauses` returns the resumed rule ids, record `OnlineRuleAction::Resumed { reason: "budget period" }` per id, scoped with `scope_of_rule`. The issue-source resume line drops the audit target (plain `info`).
   - Delete the rule audit lines.
7. **Tests:**
   - `routes/evaluators.rs` `no_webhook_values_in_logs`:
     - keep its secret asserts;
     - replace the "evaluator created/updated" text asserts with rows whose details hold `url_host`;
     - assert that the sealed values appear neither in the logs nor in `details`.
   - `routes/online_rules.rs:~2170-2195`: assert an `online_rule.updated` row with `budget_resumed: true` after the PATCH that clears a pause, and `false` on the second.
   - `routes/backfills.rs:~925`: an `online_rule.backfill_started` row.
   - New:
     - `evaluator_delete_in_use_records_nothing`;
     - `archiving_twice_records_once`;
     - `rule_resume_is_recorded_as_manual`;
     - `score_definition_archive_is_recorded`;
     - `a_budget_pause_is_recorded_by_system` (in `eval/execute.rs` tests, if a pause test exists there; otherwise next to the existing pause test).

**Done when:** committed.

**Commit:** `Record evaluation objects and rule pauses`

---

### B3 — Alerts, connections, prices and the assistant setting

**Goal:** alert rules and channels, LLM connections, model prices and the assistant setting write records. Tests, playground runs and assistant conversations lose the audit target.

**Edit:** `crates/server/src/routes/alert_rules.rs`, `crates/server/src/routes/channels.rs`, `crates/server/src/routes/connections.rs`, `crates/server/src/routes/model_prices.rs`, `crates/server/src/prices.rs`, `crates/server/src/routes/assistant.rs`, `crates/server/src/assistant/turn.rs`, `crates/server/src/routes/playground.rs`.

**Steps** (facts § A3, A4, A5, A10, B, C):
1. **Alert rules:**
   - create: `Created { metric, enabled }`.
   - update: `Updated { fields }`, from `RulePatch`'s `Some` fields by their JSON names; only when `Some(row)`.
   - delete: when true.
   - Delete three lines.
   - `audit_logs_hold_no_names`: keep, and also assert that the row's `details` contain neither `Secret name` nor `x-secret`.
2. **Channels:**
   - create: `Created { kind }`.
   - update: in the handler's closure, when `patch_channel_on` returned `Ok(Some(row))` (the inner value is an `Option`), record `Updated { fields: changed_fields }` (P-6).
   - delete: when true.
   - `test_channel` drops the audit target.
   - `logs_hold_no_secrets`: also assert that no row's `details` holds the URL, recipients or `SECRET`.
3. **LLM connections** (org scope: `.org(access.org.id).target(LlmConnection, id)`, no project):
   - create: `Created { adapter }`.
   - update: record right after `update_connection` returns `Some(row)`, before both returns, with `Updated { fields, key_changed: key_sent }`.
   - delete: when it removed the row; the conflict path rolls back.
   - `test_connection`, `test_embedding` and `after_connection_write`'s "online rule resumed" line: drop the audit target. A rule resumed by a key change or a passing test gets its own `online_rule.resumed` row, reason `connection` (D-EA-21, added after this plan).
   - `no_key_material_in_logs`: assert rows instead of the text, and that no key text is in `details`.
4. **Model prices:**
   - `prices::write` gets a `ctx: &audit::Ctx` parameter (clone it into its closure, P-7). Before `change` is consumed (`let stored = match change`, ~625), compute the action (`ModelPriceAction::Created/Updated/Deleted` from the `PriceChange` variant) and the id. Before `Ok(WriteOutcome::Done(stored))`, record that action with `.org(org)`, `.project(p)` when `Some`, and target `(ModelPrice, id)`.
   - Its six callers in `routes/model_prices.rs` pass `&ctx`. Delete their six lines.
   - Update the test at `~1099-1115` to rows.
5. **Assistant setting:** `put_org_assistant`: inside the closure after `store::put_settings`, record `AssistantAction::SettingChanged { enabled, connection_id }`, `.org(org)`. Delete the line. `routes/assistant.rs`'s conversation, feedback and turn-start lines, and `assistant/turn.rs:~865`, drop the audit target.
6. **Playground:** `routes/playground.rs:~422` drops the audit target.
7. **Tests:**
   - New: `channel_update_records_field_names_only`, `connection_key_change_is_flagged`, `org_price_write_is_recorded_without_project`, `assistant_setting_change_is_recorded`.
   - The class (b) tests (`audit_logs_hold_no_title`, the playground and turn canaries) keep passing unchanged, since only the target changes.

**Done when:** committed.

**Commit:** `Record alert, connection, price and assistant setting changes`

---

### B4 — Prompts, datasets, experiments and issues

**Goal:** prompts, datasets (and their exports) and issue sources write records. Dataset items, experiments and issue edits lose the audit target.

**Edit:** `crates/server/src/routes/prompts.rs`, `crates/server/src/routes/datasets.rs`, `crates/server/src/routes/experiments.rs`, `crates/server/src/routes/issues.rs`, `crates/server/src/issues/worker.rs`, `crates/server/src/issues/refit.rs`.

**Steps** (facts § A7, A8, A9, B, C, D2). Prompts and datasets accept API keys: `audit::Ctx` gives `Actor::ApiKey`, so nothing special is needed.
1. **Prompts:** record in each handler's closure when the helper returned `Written::Done(..)` (P-6), without changing the helpers:
   - `create_tx`: `Created`;
   - `update_prompt` (`Some(row)`): `Updated { fields }`;
   - `delete_tx`: `Deleted`;
   - `add_version_tx`: `VersionCreated { version }`;
   - `set_label_tx`: `LabelSet { label, version }`;
   - `remove_label_tx`: `LabelRemoved { label }`;
   - `put_settings`: `ProtectedLabelsChanged { count }`.

   `Written::Protected` and `Missing` record nothing. Delete seven lines. Test `~2018-2031`: assert a `prompt.created` row with `actor_kind = 'api_key'` instead of `actor=key:`.
2. **Datasets:**
   - create: `Created`.
   - update: in the handler's closure, when `update_tx` returned `Ok(row)`, `Updated { fields: changed_fields }` (P-6).
   - delete: when true.
   - Delete three lines.
   - `export_items`: add `ctx`. After `find`, `version_of` and the format are resolved, and before `Body::from_stream`, call `audit::record_alone(&state.meta, &ctx, Event::new(DatasetAction::ExportStarted { version, format }).org(..).project(..).target(Dataset, id), now)`. On error, return it (500): no export without its record.
   - The item, import, restore and add-from-trace(s) lines (facts § B, datasets) drop the audit target. Their tests keep passing.
   - Test `~3312-3326`: assert a `dataset.created` row with `actor_kind = 'api_key'` and no name in `details`.
3. **Experiments:** the three lines drop the audit target.
4. **Issues:**
   - Issue sources create, update and delete as facts § A9, with `Created { kind }` and `Updated { fields }`.
   - `patch_issue`, `post_issue_merge` and `post_issue_source_refit` drop the audit target.
   - `issues/worker.rs` `apply_pause` and `pause_connection_sources`, and `issues/refit.rs` `budget_stop`: drop the audit target (plain `info`, D-EA-21).
5. **Tests:**
   - New: `prompt_label_set_is_recorded_with_the_label`, `protected_prompt_write_records_nothing`, `dataset_export_is_recorded_before_streaming` (the row exists when the response headers arrive), `issue_source_update_records_field_names`.
   - `issues/worker.rs:~2578`'s `assert_logs_clean` stays.

**Done when:** committed.

**Commit:** `Record prompt, dataset and issue source changes and dataset exports`

---

### B5 — Dashboards, annotation queues, SQL and MCP

**Goal:** dashboards and annotation queues write records. SQL runs and exports and MCP tool calls are recorded before they run.

**Edit:** `crates/server/src/routes/dashboards.rs`, `crates/server/src/routes/annotation_queues.rs`, `crates/server/src/routes/sql.rs`, `crates/server/src/mcp/mod.rs`, `crates/server/src/mcp/sql.rs`, `crates/server/src/mcp/prompts.rs`, `crates/server/src/mcp/traces.rs`, `crates/server/src/mcp/issues.rs`, `crates/server/src/mcp/tools.rs`.

**Steps** (facts § A11, D1, D3):
1. **Dashboards:**
   - `create_dashboard`: `Created`.
   - `update_dashboard`: `Updated` on `UpdateOutcome::Updated` only.
   - `delete_dashboard`: `Deleted` when true. It has no principal extractor; add `ctx`.
   - `save_overview`: `OverviewSaved { created }`.
   - `reset_overview`: `OverviewReset` when true.
   - All are recorded inside the closures, target `(Dashboard, id)`, replacing the plain info lines (keep none).
2. **Annotation queues:** `create_queue` (`QueueCreate::Created`), `update_queue` (`QueueUpdate::Updated`) and `delete_queue` (true), target `(AnnotationQueue, id)`, replacing their plain info lines. Item lines are unchanged.
3. **SQL:** in `run_sql`, add `ctx: audit::Ctx`. Before `visitor` is consumed, compute `let audit = visitor.is_none().then_some(&ctx);` (an `Option<&audit::Ctx>`; demo visitors get `None`, P-4), and pass it to `run_json` and `start_export` as a new parameter. When it is `Some`:
   - in `run_json`, right after `check_request(body)?`, record `SqlAction::Run { query_sha256: audit::sha256_hex(&checked.query) }`;
   - in `start_export`, after the role check and `check_request`, record `SqlAction::ExportStarted { query_sha256, format }` (format spelled `csv` | `ndjson` | `parquet`).

   Both use `audit::record_alone`, `.org(access.project.org_id).project(access.project.id)`, no target, with `state.clock.now_ns()`. On error, return the error: the query does not run. Params values are never hashed or recorded.
4. **MCP:**
   - `McpServer` gets a field `audit: audit::Ctx`. The `mcp` handler adds the `ctx: audit::Ctx` extractor and stores it. The PAT principal gives `Actor::User { via: Pat, credential_id }`.
   - The tools are free functions, `fn(&AppState, &AuthUser, JsonObject) -> Result<Value, ApiError>`. Add a parameter `audit: &audit::Ctx` to the nine tools below, and pass `&self.audit` from `run_tool`.
   - Add a free function to `mcp/mod.rs`:

     ```rust
     pub(crate) async fn record_tool(state: &AppState, audit: &audit::Ctx, tool: &'static str, project: &ProjectAccess, query_sha256: Option<String>) -> Result<(), ApiError>
     ```

     It calls `audit::record_alone` with `McpAction::ToolCalled { tool, query_sha256 }`, `.org(project.project.org_id).project(project.project.id)`, and maps `MetaError` with `?`.
   - Call it right after each successful `user_project(..)`, before any read:
     - `mcp/sql.rs` `query_sql` (with the query's hash) and `list_saved_views`;
     - `mcp/prompts.rs` `list_prompts` and `get_prompt`;
     - `mcp/traces.rs` `search_traces`, `get_trace` and `get_span`;
     - `mcp/issues.rs` `list_issues`;
     - `mcp/tools.rs` `get_schema` (bind the access it discards today).

     `list_projects` records nothing.
5. **Tests:**
   - `routes/sql.rs` `logs_carry_no_query_text`: also assert one `sql.run` row per valid query it sends (two), each with the `query_sha256` of its query, and that no query text appears anywhere in `audit_events`. The invalid and empty bodies record nothing.
   - New:
     - `sql_export_is_recorded_with_format`;
     - `demo_visitor_sql_is_not_recorded` (if a demo test helper exists; else report it);
     - `dashboard_create_update_delete_are_recorded`;
     - `queue_create_is_recorded_items_are_not`;
     - `mcp_query_sql_is_recorded_with_hash_and_pat` (`via = 'pat'`, `credential_id` = the PAT id);
     - `mcp_list_projects_is_not_recorded`;
     - `mcp_forbidden_project_records_nothing`.

**Done when:** committed.

**Commit:** `Record dashboards, queues, SQL runs and MCP tool calls`

---

### B6 — Docs

**Goal:** the design and specs match M2.

**Edit:** `docs/plans/ea-enterprise-admin/README.md`, `docs/mvp-decisions.md`, `docs/spec/08-auth.md`, `docs/spec/05-query.md`, `docs/spec/09-api.md`, `CHANGELOG.md`.

**Steps:**
1. **Design** § Writing, "One request, one record": replace the example list with "(adds by filter to a queue, a batch of SCIM group members)". In § Event catalog:
   - "Project objects" lists exactly the groups of B1;
   - "Not in the catalog" adds experiments, issue edits, merges and refits, dataset imports, adds from traces, version restores, saved views and connection and channel tests;
   - worker pauses and resumes are recorded with actor `system`.
2. **Spec 08** § Audit records: one sentence naming the M2 groups, and that SQL runs, exports and MCP tool calls record a SHA-256 of the query text before running. Demo visitors are not recorded.
3. **Spec 05:** in the SQL console section, the same sentence about the record before the run.
4. **Spec 09:**
   - under the MCP section, "each tool call that names a project is recorded in the audit log";
   - at ~194 (playground) and ~263 (assistant), remove the statements that runs and turns are audit events: they are plain log lines now.
5. **Decisions:** add D-EA-21, D-EA-22 and D-EA-23 (§ New decisions above) to the design's decision table (with rejected alternatives: "record every data edit", "record no worker change", "record `list_projects`", "record demo visitors") and to `docs/mvp-decisions.md` § Track EA in the same row format. Add their IDs to the roadmap row's decision range? No: the roadmap is the coordinator's.
6. **`CHANGELOG.md`:** extend M1's `Added` line with "configuration changes of evaluators, rules, alerts, connections, prices, prompts, datasets, issue sources, dashboards and annotation queues, SQL runs, exports and MCP tool calls".

**Done when:** committed.

**Commit:** `Document the full audit catalog`

---

## After the phases (coordinating session)

1. Review each diff against its phase.
2. `git grep -n 'target: "audit"' -- crates/` must list only `audit/mod.rs` and M1's unknown-account line in `routes/auth.rs`.
3. Run `just check-rust` once, then a Haiku fix phase for what fails.
4. A Sonnet code review of M1 and M2 together; a Haiku fix phase for its findings.
5. Roadmap: "M2 implemented".
