Status: plan, written 2026-11-20 from the code at `ad50b2d`. It assumes M2's `cetra-otel` API (design § Normalization API).

# CA · M3 — Session context: `agent_contexts`, `POST /v1/agent-context`, the stamp

Milestone M3 of track CA. It covers the design's `docs/plans/ca-coding-agents/00-design.md` § Session context and § Normalization API › The server's side, and decision D-CA-9.

**Haiku implements it one phase at a time.** If a step is unclear or contradicts the code, **stop and report**; do not improvise.

## Prerequisites

- **M2 is done.** `cetra_otel` exports `AgentContext`, `AgentContexts` and `canonical_repository`. `normalize` takes `agents: &AgentContexts` as its fourth argument. `accept_request` passes `&AgentContexts::default()`.
- Branch `ca-m3-context`, cut from the M2 branch (`ca-m2-normalization`) after M2's last commit.
- If `cetra_otel::AgentContexts` does not exist, stop.
- `export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH` before `just`.
- Load rule: compile and test Rust once per phase, right before the commit.

## Read first

- `AGENTS.md` (privacy: no email, key or payload value in logs).
- The design § Session context.
- `crates/server/src/routes/otlp.rs`: `routes()`, `refuse_over_cap`, `export_logs`, and the test module's `post_traces`.
- `crates/server/src/routes/feedback.rs`: `DefaultBodyLimit` and `crate::http::error::Json`.
- `crates/server/src/http/access.rs`:
  - `RouteDef`, `ProjectAccess`;
  - `routes()`;
  - the test `route_table_matches_parameters_and_openapi_security`.
- `crates/server/src/http/mod.rs`: `AppInner`, `otlp_router`.
- `crates/server/src/api/openapi.rs`: the `post_feedback` operation (around line 2462), its `.path(…)` registration, and `set.insert::<FeedbackRequest>()`.
- `crates/server/src/serve.rs`:
  - `bind`, especially the pending-traces load at lines 510–513;
  - `spawn_last_used`, `Running::shutdown`.
- `crates/server/src/ingest/mod.rs`: `Ingest::new`, `accept_request` (around lines 349–381).
- `crates/server/src/eval/pending.rs` and `crates/server/src/meta/pending_traces.rs`: the model for an in-memory map plus `meta.db` rows.
- `crates/server/src/deletion/resolve.rs`: `resolve`, the meta write at lines 87–105.
- `crates/server/src/deletion/mod.rs`: `drop_project_step`, `drop_org_step`.
- `crates/server/src/retention.rs`: `effective_days`, `retention_once`.
- `crates/server/src/test_support/mod.rs`: `TestApp`, `Seeded`, `TestRequest`, `capture_logs`, `assert_logs_clean`.

## What it is, and what it is not

**When M3 is done:**
- `POST /v1/agent-context` stores a session's context.
- Later spans of that session, on HTTP or gRPC, carry `metadata['repository']`, `metadata['branch']` and the person (D-CA-5), stamped by `normalize`.
- Contexts survive restarts.
- Contexts expire, are capped per project, and go with a user deletion or a project drop.

**Not in M3:**
- The CLI hook (M4).
- Docs pages (M5b).
- The end-to-end test (M5b).

## Decisions this plan relies on

| ID | Summary |
|---|---|
| D-CA-9 | The route, the body, last write wins, absent keeps and `null` clears, `meta.db` plus a map loaded before the listeners, expiry of 30 days or the retention when shorter, 10,000 per project, `normalize`'s fourth argument, best-effort deletion, no body value logged |
| D-CA-5 | The person's rank: `normalize` applies it (M2) |
| D-I-8 | The ingest path reads memory, never `meta.db` |
| D-M-7 | A project drop cascades through foreign keys |

**Where the store lives.** It is inside `Ingest`, exposed as `state.ingest.contexts()`, as `prices()` is. There is no `AppInner` field and no new `Ingest::new` parameter.

**Eviction is chosen in SQL.** `AgentContexts` has no iteration, so the oldest row is found in `meta.db`, in the same write (S1 `delete_oldest`).

**Characters** means `str::chars().count()`. A post for a new key whose optional fields are all absent stores an empty context: it takes a slot, which is what the hook's first post does.

**Write skipping, made exact.** An entry's `updated_at` is the time of its last `meta.db` write. A post that changes no field, onto an entry written less than an hour ago, writes nothing anywhere. Otherwise it writes `meta.db`, then the map, with `updated_at = now`. Expiry is therefore at most one hour late, which D-CA-9 allows.

## Versions

No new dependency.

## Phases

```
S1 (migration + meta queries) ──► S2 (store, load, sweep, drops) ──► S3 (route + OpenAPI) ──► S4 (stamp + deletion) ──► S5 (specs)
```

All run in order.

---

## S1 — Migration and `meta.db` queries

**Goal:** the `agent_contexts` table and its queries.

**Edit:**
- `crates/server/migrations/meta/0023_agent_contexts.sql` (new). Use the next free number if `0023` is taken, and say so.
- `crates/server/src/meta/agent_contexts.rs` (new)
- `crates/server/src/meta/mod.rs` (the `mod` line)
- `crates/server/tests/snapshots/meta_schema.snap` (updated by insta)

**Steps:**
1. The migration:

   ```sql
   -- Coding-agent session contexts (D-CA-9): the repository, branch and person a hook reported.
   CREATE TABLE agent_contexts (
     project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
     agent TEXT NOT NULL,          -- claude-code | codex | gemini-cli | copilot
     session_id TEXT NOT NULL,
     repository TEXT,              -- canonical form (D-CA-4)
     branch TEXT,
     user_id TEXT,                 -- the hook's `user`, lowercased
     updated_at INTEGER NOT NULL,  -- ns since the Unix epoch, time of the last write
     PRIMARY KEY (project_id, agent, session_id)
   );
   CREATE INDEX agent_contexts_user ON agent_contexts(project_id, user_id) WHERE user_id IS NOT NULL;
   CREATE INDEX agent_contexts_updated ON agent_contexts(project_id, updated_at);
   ```

2. `meta/agent_contexts.rs`:

   ```rust
   #[derive(Debug, Clone, PartialEq, Eq)]
   pub(crate) struct AgentContextRow {
       pub(crate) project_id: ProjectId,
       pub(crate) agent: String,
       pub(crate) session_id: String,
       pub(crate) repository: Option<String>,
       pub(crate) branch: Option<String>,
       pub(crate) user_id: Option<String>,
       pub(crate) updated_at: i64,
   }

   /// Every row of active projects in active orgs (same filter as `load_pending`).
   pub(crate) async fn load_contexts(pool: &SqlitePool) -> Result<Vec<AgentContextRow>, MetaError>;
   /// Inserts or replaces one row. Writes nothing when the project no longer exists (`INSERT … SELECT … WHERE EXISTS`, as `write_pending`).
   /// Returns whether a row was written.
   pub(crate) async fn upsert_context(conn: &mut SqliteConnection, row: &AgentContextRow) -> Result<bool, MetaError>;
   /// When the project has at least `cap` rows, deletes the row with the smallest `(updated_at, agent, session_id)` and returns its key.
   /// `SELECT agent, session_id FROM agent_contexts WHERE project_id = ?1 ORDER BY updated_at, agent, session_id LIMIT 1`, run only when `COUNT(*) >= cap`.
   pub(crate) async fn delete_oldest(conn: &mut SqliteConnection, project: ProjectId, cap: usize) -> Result<Option<(String, String)>, MetaError>;
   /// Deletes the project's rows whose `updated_at < before`. Returns how many.
   pub(crate) async fn delete_expired(conn: &mut SqliteConnection, project: ProjectId, before: i64) -> Result<u64, MetaError>;
   /// Deletes the project's rows of `user_id`. Returns how many.
   pub(crate) async fn delete_user_contexts(conn: &mut SqliteConnection, project: ProjectId, user_id: &str) -> Result<u64, MetaError>;
   ```

   `ProjectId` is bound with `.bind(project.to_string())` and read back through `Uuid` and `ProjectId::from_uuid`, as in `pending_traces.rs`. Follow the style of `meta/pending_traces.rs`: the `sqlx::query` style, how `ProjectId` is bound and read, and how errors map to `MetaError`.

**Tests** (`meta/agent_contexts.rs`, on a migrated temporary `meta.db`, the way `pending_traces.rs` tests do):
- `upsert_then_load_round_trips`.
- `upsert_replaces_the_same_key`.
- `upsert_for_a_dropped_project_writes_nothing` (returns `false`).
- `delete_oldest_picks_the_smallest_updated_at_then_key`: under the cap it returns `None`; at the cap it deletes and returns the oldest row, with a tie broken by `(agent, session_id)`.
- `load_skips_inactive_projects`: mark the project or org as not active, the way the `load_pending` test does.
- `delete_expired_removes_only_older_rows`.
- `delete_user_contexts_removes_only_that_user_in_that_project`.
- `project_delete_cascades`: `DELETE FROM projects` removes the rows.
- The schema snapshot (`meta_schema`): run `cargo insta accept`, or the repo's way of updating it (see `meta/mod.rs:494`), and review that the diff is only the new table and indexes.

**Done when:**
- `cargo test -p cetra-server meta::` passes.
- `cargo clippy -p cetra-server --all-targets -- -D warnings` passes.

The migration is additive: it does not bump `META_FORMAT_VERSION`.

**Commit:** `Add the agent_contexts table`

---

## S2 — The in-memory store, its load, the sweep and the drops

**Goal:** `AgentContextStore` in `AppInner`, shared with `Ingest`, loaded before the listeners open, swept hourly, and purged on project or org drop.

**Edit:**
- `crates/server/src/agent_context.rs` (new)
- `crates/server/src/lib.rs` (the `mod` line)
- `crates/server/src/ingest/mod.rs` (`Ingest` field, constructor body, `contexts()` accessor)
- `crates/server/src/serve.rs`
- `crates/server/src/retention.rs` (the extraction of step 4)
- `crates/server/src/deletion/mod.rs` (the drop steps and their tests)

**Steps:**
1. `agent_context.rs`:

   ```rust
   /// Most contexts kept per project (D-CA-9).
   pub(crate) const MAX_PER_PROJECT: usize = 10_000;
   /// Longest life of a context (D-CA-9); a shorter project retention wins.
   pub(crate) const MAX_AGE_DAYS: u32 = 30;
   /// A post that changes nothing is written again only after this long.
   pub(crate) const REWRITE_AFTER_NS: i64 = 3_600 * 1_000_000_000;
   /// How often the sweep runs.
   pub(crate) const SWEEP_EVERY: Duration = Duration::from_secs(3_600);

   /// Per-project contexts. `normalize` reads one project's map under its read lock.
   #[derive(Debug, Default)]
   pub(crate) struct AgentContextStore {
       projects: std::sync::RwLock<HashMap<ProjectId, Arc<std::sync::RwLock<cetra_otel::AgentContexts>>>>,
       /// Serializes writers (route, sweep, deletions), so the meta write and the map change stay in step.
       writes: tokio::sync::Mutex<()>,
   }

   impl AgentContextStore {
       /// The project's map, if it has one. Ingest clones the `Arc` into its blocking task.
       pub(crate) fn project(&self, project: ProjectId) -> Option<Arc<std::sync::RwLock<cetra_otel::AgentContexts>>>;
       /// Replaces the whole content with `rows` (startup).
       pub(crate) fn load(&self, rows: Vec<AgentContextRow>);
       /// Drops a project's map (project or org drop).
       pub(crate) fn forget_project(&self, project: ProjectId);
       pub(crate) fn writes(&self) -> &tokio::sync::Mutex<()>;
       /// The project's map, created empty if absent.
       pub(crate) fn project_or_insert(&self, project: ProjectId) -> Arc<std::sync::RwLock<cetra_otel::AgentContexts>>;
   }
   ```

   Lock poisoning is recovered with `unwrap_or_else(PoisonError::into_inner)`, as `eval/pending.rs` does. Rows map to `AgentContext { repository, branch, user: user_id, updated_at }`.
2. **`Ingest`.** Add a field `contexts: Arc<AgentContextStore>`, created in `Ingest::new` as `Arc::new(AgentContextStore::default())`, and `pub(crate) fn contexts(&self) -> &Arc<AgentContextStore>` with the doc comment `/// Coding-agent session contexts (D-CA-9).`. Everywhere below, `state.ingest.contexts()` means `state.ingest.contexts()`.
3. **Startup** (`serve.rs`, right after the pending-traces load at lines 510–513):

   ```rust
   // Losing the load only loses the contexts already posted; hooks post again at the next prompt.
   match crate::meta::agent_contexts::load_contexts(state.meta.reader()).await {
       Ok(rows) => state.ingest.contexts().load(rows),
       Err(error) => tracing::warn!(error = %error, "agent contexts could not be loaded; starting empty"),
   }
   ```

4. **The sweep task.** `fn spawn_agent_context_sweep(state: AppState) -> JoinHandle<()>` in `agent_context.rs`, on the `spawn_last_used` pattern: an interval of `SWEEP_EVERY` with the first tick skipped, and `MissedTickBehavior::Skip`. Push it into `tasks` in `serve.rs` next to the others. Each run calls `pub(crate) async fn sweep_once(state: &AppState, now_ns: i64) -> Result<u64, MetaError>`:
   2. **Before** taking the lock, get each active project's effective retention. Extract from `retention::retention_once` a function:

      ```rust
      pub(crate) async fn effective_retention(state: &AppState, now_ns: i64) -> Result<Vec<(ProjectId, Option<u32>)>, MetaError>
      ```

      It returns, for each active project, `effective_days(project_days, org cap)`, computed exactly as `retention_once` does today. `retention_once` then calls it and keeps its own failure counting and logs. Existing retention tests must pass unchanged. Then take the lock.
   3. For each project with a map: `days = min(MAX_AGE_DAYS, effective_days)`, where a `None` retention means `MAX_AGE_DAYS`, and `before = now_ns − days × 86_400 × 10⁹`.
   4. In one `state.meta.write`, call `delete_expired(conn, project, before)` for every project.
   5. Then, for each map, `retain(|_, _, c| c.updated_at >= before)`.
   6. Return the total deleted. On an error, log `warn!(error = %error, "agent contexts could not be swept")` and change no map.
   7. Map guards are scoped in blocks and never held across an `.await` (clippy `await_holding_lock`).

   The sweep task is spawned in restore mode too, because it is local. `restore_mode_skips_the_sending_and_dns_loops` counts tasks and must still pass.
5. **Drops.** In `deletion/mod.rs`, under `let _writes = state.ingest.contexts().writes().lock().await;`, as `prices().lock_writes()` is used there, call `state.ingest.contexts().forget_project(project)`:
   - in `drop_project_step`, after `state.clear_answers(project);`;
   - in `drop_org_step`, for each project, after its `clear_answers`.

   The rows themselves go through the cascade.

**Tests** (`agent_context.rs`):
- `load_then_project_returns_the_entries`.
- `forget_project_drops_the_map`.
- `sweep_removes_entries_older_than_30_days`: a project without retention; entries at `now − 31 d` and `now − 1 d`.
- `sweep_uses_a_shorter_project_retention`: a project with `retention_days = 7`; an entry at `now − 8 d` goes.
- `sweep_deletes_the_rows_too`: `load_contexts` afterwards no longer has them.
- `a_dropped_project_has_no_map`, in `deletion/mod.rs` tests next to `project_drop_removes_the_project`: after the drop, `project()` is `None`.
- `contexts_survive_a_restart`, in the `serve.rs` tests next to the ones that inspect `pending_traces` after `bind`: write a row with `upsert_context`, shut down, `bind` again, and `project(...)` has it.
- The existing server tests still pass with the new `Ingest::new` parameter.

**Done when:**
- `cargo test -p cetra-server agent_context` passes.
- `cargo test -p cetra-server deletion` passes.
- `cargo test -p cetra-server retention` passes.
- `cargo test -p cetra-server serve` passes.
- `cargo clippy -p cetra-server --all-targets -- -D warnings` passes.

**Commit:** `Keep coding-agent session contexts in memory and sweep them`

---

## S3 — `POST /v1/agent-context`

**Goal:** the route, on the main port and on 4318, documented in `openapi.json`.

**Edit:**
- `crates/server/src/api/agent_context.rs` (new: the body type)
- `crates/server/src/api/mod.rs` (the `mod` line)
- `crates/server/src/routes/agent_context.rs` (new: the handler and its tests)
- `crates/server/src/routes/mod.rs` (the `mod` line)
- `crates/server/src/routes/otlp.rs` (`routes()`, the module and `routes()` doc comments, and `refuse_over_cap` made `pub(crate)`)
- `crates/server/tests/openapi.rs`
- `crates/server/src/http/mod.rs` (the `otlp_router` doc comment only)
- `crates/server/src/api/openapi.rs`
- `crates/server/openapi.json`
- `web/src/api/schema.gen.ts` (both regenerated by `just openapi`)

**Steps:**
1. **The body type** goes in `api/agent_context.rs`, as `pub struct` like every request body in `api/`. Reuse the existing `present` helper (`api/online_rules.rs`, `pub(super)`; `backfills.rs` imports it as `super::online_rules::present`) instead of the local function below:

   ```rust
   /// A coding agent's session context, sent by `cetra agents hook` (D-CA-9).
   #[derive(Debug, Deserialize, utoipa::ToSchema)]
   #[serde(deny_unknown_fields)]
   pub(crate) struct AgentContextBody {
       /// `claude-code`, `codex`, `gemini-cli` or `copilot`.
       pub(crate) agent: String,
       /// The id the agent puts on its spans, 1 to 256 characters.
       pub(crate) session_id: String,
       /// The `origin` URL without user-info. Absent keeps the stored value; `null` clears it.
       #[serde(default, deserialize_with = "present")]
       #[schema(value_type = Option<String>)]
       pub(crate) repository: Option<Option<String>>,
       /// The branch, 1 to 256 characters. Absent keeps; `null` clears.
       #[serde(default, deserialize_with = "present")]
       #[schema(value_type = Option<String>)]
       pub(crate) branch: Option<Option<String>>,
       /// The person, 1 to 256 characters, lowercased by the server. Absent keeps; `null` clears.
       #[serde(default, deserialize_with = "present")]
       #[schema(value_type = Option<String>)]
       pub(crate) user: Option<Option<String>>,
   }

   /// `Some(v)` for a present field, including `null` (`Some(None)`); an absent field stays `None` through `default`.
   fn present<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
   where D: serde::Deserializer<'de>, T: serde::Deserialize<'de> {
       T::deserialize(deserializer).map(Some)
   }
   ```

   The precedent is `UpdateDatasetRequest.description` (`api/datasets.rs`). `openapi.rs`'s `drop_null_from_omitted_optionals` strips `null` from optional properties unless they are listed in `NULL_IS_A_VALUE`, so add `("AgentContextBody", "repository")`, `("AgentContextBody", "branch")` and `("AgentContextBody", "user")` to that list.
2. **The handler** `async fn post_agent_context(State(state), Extension(access): Extension<ProjectAccess>, body: crate::http::error::Json<AgentContextBody>) -> Result<StatusCode, ApiError>`. Use the extractor exactly as `feedback.rs` does, with the route layered by `DefaultBodyLimit::max(4096)`.
   1. `refuse_over_cap(&state, &access).await?`. Make `refuse_over_cap` `pub(crate)` in `otlp.rs` if it is private.
   2. **Validate**, collecting every error into `ApiError::BadRequest { detail: Some("invalid request".into()), errors }`, where `errors: Vec<FieldError>` (`crate::api::common::FieldError { field, message, position: None }`). Copy the 5-line local `field_error(field, message)` that other `api/` modules define. Unknown fields and a missing `agent` are already 400 through the `Json` extractor's rejection.
      - `agent` must be one of the four wire names: `agent` "must be claude-code, codex, gemini-cli or copilot";
      - `session_id` must be 1–256 characters: `session_id` "must be 1 to 256 characters";
      - a present string `branch` or `user` must be 1–256 characters after trimming: the same message.

      Messages never contain the submitted value.
   3. **Normalize:**
      - `repository`: `Some(Some(raw))` becomes `Some(cetra_otel::canonical_repository(&raw))`, so an uncanonicalizable URL is stored as `null`;
      - `user`: trimmed and lowercased;
      - `branch`: trimmed.
   4. **Write**, holding `state.ingest.contexts().writes().lock().await`:
      1. Read the current entry from `project_or_insert(project)` under the read lock.
      2. Build the merged `AgentContext`: for each field, `None` (absent) keeps the old value and `Some(v)` sets `v`.
      3. If an old entry exists, its fields equal the merged ones, and `now − old.updated_at < REWRITE_AFTER_NS`, return `204` with no write.
      4. Otherwise set `merged.updated_at = now`, where now is the server clock in ns (`state.clock`, as other handlers read it).
      6. In one `state.meta.write`: if the key is new, `delete_oldest(conn, project, MAX_PER_PROJECT)`; then `upsert_context`. Return `(evicted, written)` from the closure. An error is a 500 (`MetaError` converts with `?`), with the map unchanged.
      7. If `written` is false (the project was dropped meanwhile), return `204` without touching the map. Otherwise, under the map's write lock, insert the merged context and remove `evicted` if any.
      8. Map guards are scoped in blocks; clone what you need. No guard is held across an `.await`.
   5. **Log** `tracing::info!(target: "ingest", project_id = %access.project.id, agent = %agent, written, "agent context")`. `written` is a bool. Log no other field.
   6. Return `StatusCode::NO_CONTENT`.
3. **Registration.** In `otlp.rs` `routes()`, append a `RouteDef { method: POST, path: "/v1/agent-context", access: Access::Key { scope: KeyScope::Ingest }, documented: true, handler: post(post_agent_context).layer(DefaultBodyLimit::max(4096)) }`. Update the doc comments of `routes()` and `otlp_router` to list the fourth route.
4. **OpenAPI** (`api/openapi.rs`), following `post_feedback`:
   - the operation `operation("post_agent_context", "ingest", "Send a coding agent's session context")`;
   - the description: "Called by `cetra agents hook`. Needs an `sk` key with the `ingest` scope. Later spans of the session carry the repository, branch and person (D-CA-9).";
   - the bearer security;
   - `json_body("AgentContextBody")`;
   - responses as `post_logout` declares them: `responses("204", ResponseBuilder::new().description("Stored.").build(), &[400, 401, 402, 403, 413])`.

   Register it with `.path("/v1/agent-context".to_owned(), PathItem::new(HttpMethod::Post, op))`, and add `set.insert::<AgentContextBody>()` next to `FeedbackRequest`. If tags are a closed list, add `ingest` with the description "Calls made by coding agents and SDKs on the ingest key." Then run `just openapi`.
5. **`tests/openapi.rs`:**
   - add `("post", "/v1/agent-context", "post_agent_context")` to `OPERATIONS`;
   - add `ingest` to `TAGS` if you added that tag;
   - add `"post_agent_context"` to the bearer-only `matches!` list;
   - add a branch in the expected-errors chain giving exactly `["400", "401", "402", "403", "413"]` for `post_agent_context`.

**Tests** (`routes/agent_context.rs`, `#[tokio::test(flavor = "current_thread")]`, with `TestApp`, `Seeded.ingest_key` and a local `post_context(app, key, body)` helper like `post_traces`):
- `stores_a_context_and_answers_204`: the map and `load_contexts` hold the canonical repository `github.com/acme/widgets`, given `https://github.com/Acme/Widgets.git`, plus the branch and the lowercased user.
- `absent_keeps_and_null_clears`: post `{repository, branch}`, then `{branch: null}`; the repository is kept and the branch cleared.
- `unchanged_post_within_an_hour_writes_nothing`: `app.state.clock.advance(Duration::from_secs(60))` between two identical posts; `updated_at` is unchanged.
- `unchanged_post_after_an_hour_rewrites`: advance by 3 601 s; `updated_at` moves.
- `posts_for_a_dropped_project_leave_no_map`: drop the project row directly in `meta.db`, post, and `project()` is `None` (`written` false).
- `bad_request_body_leaks_nothing`: a 400 body holding `dev@example.com` and `feature/secret`; neither the logs nor the problem JSON contain them.
- `rejects_unknown_agent_bad_session_and_unknown_fields`: 400s with the field paths; unknown field `foo` is 400.
- `needs_the_ingest_scope`: `read_key` gets 403; no key gets 401.
- `refused_over_the_units_cap`: 402, following the OTLP logs test's setup.
- `body_over_4_kib_is_413`.
- Eviction is covered by S1's `delete_oldest` test.
- `served_on_the_otlp_port`: `crate::http::otlp_router(app.state.clone())` with `tower::ServiceExt::oneshot` gives 204.
- `logs_hold_no_values`: `assert_logs_clean(&["dev@example.com", "feature/secret", ingest_key], post)`.
- The route-table test `route_table_matches_parameters_and_openapi_security` passes, with the new documented route found in `openapi.json`.

**Done when:**
- `cargo test -p cetra-server agent_context` passes.
- `cargo test -p cetra-server access` passes.
- `cargo test -p cetra-server --test openapi` passes.
- `cargo test -p cetra-server --test api_examples` passes.
- `cargo clippy -p cetra-server --all-targets -- -D warnings` passes.
- `git diff --stat` shows `openapi.json` and `schema.gen.ts` changed only by the new operation and schema.

**Commit:** `Add POST /v1/agent-context`

---

## S4 — The stamp at ingest, and deletion by user

**Goal:** spans received after a context carry it, and a user deletion removes the user's contexts.

**Edit:**
- `crates/server/src/ingest/mod.rs` (`accept_request` only)
- `crates/server/src/ingest/grpc.rs` (tests only)
- `crates/server/src/deletion/resolve.rs`
- `crates/server/src/deletion/mod.rs` (tests only)
- `crates/server/src/routes/otlp.rs` (tests only)

**Steps:**
1. **`accept_request`.**
   - Before `spawn_blocking`: `let contexts = self.contexts.project(project.id);`, an `Option<Arc<RwLock<AgentContexts>>>`.
   - Inside the closure, replace M2's `&AgentContexts::default()` with:

     ```rust
     let empty = AgentContexts::default();
     let guard = contexts.as_ref().map(|lock| lock.read().unwrap_or_else(PoisonError::into_inner));
     let agents = guard.as_deref().unwrap_or(&empty);
     let mut normalized = normalize(&request, &ctx, &prices, agents);
     ```

     The guard is dropped at the end of the closure, before the WAL append, which happens later outside it.
2. **`resolve.rs`.** Take `state.ingest.contexts().writes().lock().await` **before** the meta write, so no post re-inserts in between, and hold it until the map retain below. In the meta write closure, after `store_counts` returns `stored == true`, and only when `row.kind` is `DeletionKind::User`, call `crate::meta::agent_contexts::delete_user_contexts(conn, project, user_id)` with the deletion's `user_id`. Capture an owned copy before the closure, as the other captured values are. After the write succeeds, remove the same entries from the map:
   - `state.ingest.contexts().project(project)`, then write-lock it, then `retain(|_, _, c| c.user.as_deref() != Some(user_id))`.

   A deletion by trace or by filter does nothing here.

**Tests:**
- `otlp.rs` tests, `stamped_after_a_context`:
  1. Post `/v1/agent-context` for `claude-code` / session `s1` with repository, branch and user.
  2. Post `/v1/traces` with the M1 fixture `fixtures/otlp/claude_code/bash-tool/input.json`, its `session.id` and `gen_ai.conversation.id` replaced by `s1`. Load the file, and replace the placeholder string `capture-volatile-N` that M1 wrote for the session with `s1`. If the fixture's session placeholder is not identifiable, stop and report.
  3. Read the stored spans with `SELECT metadata['repository'], metadata['branch'], user_id FROM raw_spans`, through `raw_query`, as `stored_spans` does (not `stored_rows`). The test uses `#[tokio::test(flavor = "multi_thread")]`, like the other OTLP ingest tests: every span has `metadata['repository'] = "github.com/acme/widgets"` and `metadata['branch']`, and `user_id` is the context's user.
- `not_stamped_for_another_session`: session `s2` gets no branch from `s1`'s context. Its `metadata['repository']` still comes from the span's own `vcs.*` attributes (M2).
- `not_stamped_before_the_context`: traces first, then the context: the stored spans have no branch.
- `stamp_on_grpc_too`, in `ingest/grpc.rs` tests with their `send(...)` helper: the same as the first test, through gRPC.
- `user_deletion_removes_the_users_contexts`, in `deletion/mod.rs` tests with `new_row(&app, project, DeletionKind::User)`, `insert(...)` and `worker(&app)`, as `user_resolves_traces_and_sessions` does: two contexts with users `a@example.com` and `b@example.com`; after deleting `a@example.com`, only `b`'s row and map entry remain.
- `trace_deletion_keeps_contexts`, in the same place.

**Done when:**
- `cargo test -p cetra-server` passes (the server crate in full, once, before the commit).
- `cargo clippy -p cetra-server --all-targets -- -D warnings` passes.

**Commit:** `Stamp coding-agent spans with their session context`

---

## S5 — Specs

**Goal:** the specs describe what S1–S4 built.

**Edit:**
- `docs/spec/04-metadata-db.md`
- `docs/spec/08-auth.md`
- `docs/spec/09-api.md`
- `docs/spec/12-operations.md`
- `docs/spec/README.md` (S11, the 4318 line)
- `website/src/content/docs/docs/reference/otlp.mdx`, `website/src/content/docs/docs/get-started/run-cetra.mdx` and `website/src/content/docs/docs/self-hosting/index.md`: wherever they say port 4318 serves only the OTLP routes, add "and `/v1/agent-context`". Find the exact files with `grep -rn "4318" website/src/content/docs`.
- `docs/plans/ca-coding-agents/00-design.md` (§ Session context, one bullet)

**Steps:**
1. **Spec 04**, `## Evaluation`, after the `pending_traces` table and its prose: a `### Coding-agent session contexts (D-CA-9)` subsection with:
   - S1's DDL;
   - two sentences: "Written by `POST /v1/agent-context` (last write wins; an unchanged post within an hour writes nothing), loaded into memory before the listeners open, read by ingest from memory only. An hourly sweep deletes rows older than 30 days or the project's retention when shorter; a project keeps at most 10,000 rows, the oldest evicted; a deletion by user deletes the user's rows."
2. **Spec 08**, line 124: "`ingest` (OTLP + scores write)" becomes "`ingest` (OTLP, scores write and the coding-agent context)".
3. **Spec 09:**
   - The top table gets the row `` | `/v1/agent-context` | coding-agent session context (D-CA-9); in the OpenAPI document; `204` | sk (ingest scope) | ``.
   - The listener paragraph (line 3) names the fourth route.
4. **Design.** In § Session context, the bullet "Skipping writes. A post that changes nothing updates only the map's `updated_at`. `meta.db` gets the new `updated_at` at most once an hour per entry, …" becomes "Skipping writes. A post that changes nothing, onto an entry written less than an hour ago, writes nothing; otherwise `meta.db` and the map get the new `updated_at`. Expiry is measured from that write."
5. **Spec 12**, § Process lifecycle, Startup: after "Pending traces are loaded from pending_traces before the ports open (D-I-8)…", add "Coding-agent contexts are loaded from `agent_contexts` before the ports open (D-CA-9); a failed load starts empty."

**Done when:** `export PATH=…; just check` passes.

**Commit:** `Document the coding-agent session context in specs 04, 08, 09 and 12`
