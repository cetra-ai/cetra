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
   pub(crate) async fn upsert_context(conn: &mut SqliteConnection, row: &AgentContextRow) -> Result<(), MetaError>;
   /// Deletes one row.
   pub(crate) async fn delete_context(conn: &mut SqliteConnection, project: ProjectId, agent: &str, session_id: &str) -> Result<(), MetaError>;
   /// Deletes the project's rows whose `updated_at < before`. Returns how many.
   pub(crate) async fn delete_expired(conn: &mut SqliteConnection, project: ProjectId, before: i64) -> Result<u64, MetaError>;
   /// Deletes the project's rows of `user_id`. Returns how many.
   pub(crate) async fn delete_user_contexts(conn: &mut SqliteConnection, project: ProjectId, user_id: &str) -> Result<u64, MetaError>;
   ```

   Follow the style of `meta/pending_traces.rs`: the `sqlx::query` style, how `ProjectId` is bound and read, and how errors map to `MetaError`.

**Tests** (`meta/agent_contexts.rs`, on a migrated temporary `meta.db`, the way `pending_traces.rs` tests do):
- `upsert_then_load_round_trips`.
- `upsert_replaces_the_same_key`.
- `upsert_for_a_dropped_project_writes_nothing`.
- `load_skips_inactive_projects`: mark the project or org as not active, the way the `load_pending` test does.
- `delete_expired_removes_only_older_rows`.
- `delete_user_contexts_removes_only_that_user_in_that_project`.
- `project_delete_cascades`: `DELETE FROM projects` removes the rows.
- The schema snapshot (`meta_schema`): run `cargo insta accept`, or the repo's way of updating it (see `meta/mod.rs:494`), and review that the diff is only the new table and indexes.

**Done when:**
- `cargo test -p cetra-server meta::` passes.
- `cargo clippy -p cetra-server --all-targets -- -D warnings` passes.

**Commit:** `Add the agent_contexts table`

---

## S2 — The in-memory store, its load, the sweep and the drops

**Goal:** `AgentContextStore` in `AppInner`, shared with `Ingest`, loaded before the listeners open, swept hourly, and purged on project or org drop.

**Edit:**
- `crates/server/src/agent_context.rs` (new)
- `crates/server/src/lib.rs` (the `mod` line)
- `crates/server/src/http/mod.rs` (`AppInner` field)
- `crates/server/src/ingest/mod.rs` (`Ingest` field and constructor only)
- `crates/server/src/serve.rs`
- `crates/server/src/test_support/mod.rs` (state construction)
- `crates/server/src/deletion/mod.rs` (drop steps)

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
2. **`AppInner` and `Ingest`.**
   - Add `pub(crate) agent_contexts: Arc<crate::agent_context::AgentContextStore>` to `AppInner`, after `pending`, with the doc comment `/// Coding-agent session contexts (D-CA-9).`
   - Give `Ingest` a field `contexts: Arc<AgentContextStore>`, set by a new last parameter of `Ingest::new`.
   - In `serve.rs` and `test_support`, create one `Arc::new(AgentContextStore::default())` and pass the same `Arc` to both.
3. **Startup** (`serve.rs`, right after the pending-traces load at lines 510–513):

   ```rust
   // Losing the load only loses the contexts already posted; hooks post again at the next prompt.
   match crate::meta::agent_contexts::load_contexts(state.meta.reader()).await {
       Ok(rows) => state.agent_contexts.load(rows),
       Err(error) => tracing::warn!(error = %error, "agent contexts could not be loaded; starting empty"),
   }
   ```

4. **The sweep task.** `fn spawn_agent_context_sweep(state: AppState) -> JoinHandle<()>` in `agent_context.rs`, on the `spawn_last_used` pattern: an interval of `SWEEP_EVERY` with the first tick skipped, and `MissedTickBehavior::Skip`. Push it into `tasks` in `serve.rs` next to the others. Each run calls `pub(crate) async fn sweep_once(state: &AppState, now_ns: i64) -> Result<u64, MetaError>`:
   1. Take `writes().lock().await`.
   2. Get the active projects and their retention days. Reuse the calls `retention::retention_once` makes (`projects_for_retention`, the org cap, `effective_days`) and read them first. If the cap needs an `await` per org, do it the same way. If this cannot be done without copying more than about 15 lines, stop and report.
   3. For each project with a map: `days = min(MAX_AGE_DAYS, effective_days)`, where a `None` retention means `MAX_AGE_DAYS`, and `before = now_ns − days × 86_400 × 10⁹`.
   4. In one `state.meta.write`, call `delete_expired(conn, project, before)` for every project.
   5. Then, for each map, `retain(|_, _, c| c.updated_at >= before)`.
   6. Return the total deleted. On an error, log `warn!(error = %error, "agent contexts could not be swept")` and change no map.
5. **Drops.** In `deletion/mod.rs`, call `state.agent_contexts.forget_project(project)`:
   - in `drop_project_step`, after `state.clear_answers(project);`;
   - in `drop_org_step`, for each project, after its `clear_answers`.

   The rows themselves go through the cascade.

**Tests** (`agent_context.rs`):
- `load_then_project_returns_the_entries`.
- `forget_project_drops_the_map`.
- `sweep_removes_entries_older_than_30_days`: a project without retention; entries at `now − 31 d` and `now − 1 d`.
- `sweep_uses_a_shorter_project_retention`: a project with `retention_days = 7`; an entry at `now − 8 d` goes.
- `sweep_deletes_the_rows_too`: `load_contexts` afterwards no longer has them.
- `a_dropped_project_has_no_map`: through `TestApp`, drop a project through the deletion step the existing drop tests use, then `project()` is `None`.
- The existing server tests still pass with the new `Ingest::new` parameter.

**Done when:**
- `cargo test -p cetra-server agent_context` passes.
- `cargo test -p cetra-server deletion` passes.
- `cargo clippy -p cetra-server --all-targets -- -D warnings` passes.

**Commit:** `Keep coding-agent session contexts in memory and sweep them`

---

## S3 — `POST /v1/agent-context`

**Goal:** the route, on the main port and on 4318, documented in `openapi.json`.

**Edit:**
- `crates/server/src/routes/agent_context.rs` (new)
- `crates/server/src/routes/mod.rs` (the `mod` line)
- `crates/server/src/routes/otlp.rs` (`routes()` and its doc comment only)
- `crates/server/src/http/mod.rs` (the `otlp_router` doc comment only)
- `crates/server/src/api/openapi.rs`
- `crates/server/openapi.json`
- `web/src/api/schema.gen.ts` (both regenerated by `just openapi`)

**Steps:**
1. **The body type** (`routes/agent_context.rs`):

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
       #[schema(value_type = Option<String>, nullable)]
       pub(crate) repository: Option<Option<String>>,
       /// The branch, 1 to 256 characters. Absent keeps; `null` clears.
       #[serde(default, deserialize_with = "present")]
       #[schema(value_type = Option<String>, nullable)]
       pub(crate) branch: Option<Option<String>>,
       /// The person, 1 to 256 characters, lowercased by the server. Absent keeps; `null` clears.
       #[serde(default, deserialize_with = "present")]
       #[schema(value_type = Option<String>, nullable)]
       pub(crate) user: Option<Option<String>>,
   }

   /// `Some(v)` for a present field, including `null` (`Some(None)`); an absent field stays `None` through `default`.
   fn present<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
   where D: serde::Deserializer<'de>, T: serde::Deserialize<'de> {
       T::deserialize(deserializer).map(Some)
   }
   ```

   Check how other request bodies in `crates/server/src/api` derive their schema (`utoipa::ToSchema` and its attribute syntax) and match that. If `#[schema(value_type = …, nullable)]` is spelled differently there, follow the code and report it.
2. **The handler** `async fn post_agent_context(State(state), Extension(access): Extension<ProjectAccess>, body: crate::http::error::Json<AgentContextBody>) -> Result<StatusCode, ApiError>`. Use the extractor exactly as `feedback.rs` does, with the route layered by `DefaultBodyLimit::max(4096)`.
   1. `refuse_over_cap(&state, &access).await?`. Make `refuse_over_cap` `pub(crate)` in `otlp.rs` if it is private.
   2. **Validate**, collecting every error, in the shape other routes use for a 400 with field paths (find the helper `feedback.rs` or `scores.rs` uses):
      - `agent` must be one of the four wire names: `agent` "must be claude-code, codex, gemini-cli or copilot";
      - `session_id` must be 1–256 characters: `session_id` "must be 1 to 256 characters";
      - a present string `branch` or `user` must be 1–256 characters after trimming: the same message.

      Messages never contain the submitted value.
   3. **Normalize:**
      - `repository`: `Some(Some(raw))` becomes `Some(cetra_otel::canonical_repository(&raw))`, so an uncanonicalizable URL is stored as `null`;
      - `user`: trimmed and lowercased;
      - `branch`: trimmed.
   4. **Write**, holding `state.agent_contexts.writes().lock().await`:
      1. Read the current entry from `project_or_insert(project)` under the read lock.
      2. Build the merged `AgentContext`: for each field, `None` (absent) keeps the old value and `Some(v)` sets `v`.
      3. If an old entry exists, its fields equal the merged ones, and `now − old.updated_at < REWRITE_AFTER_NS`, return `204` with no write.
      4. Otherwise set `merged.updated_at = now`, where now is the server clock in ns (`state.clock`, as other handlers read it).
      5. If the key is new and the map already has `MAX_PER_PROJECT` entries, the evicted key is the one with the smallest `updated_at`; ties go to the smallest `(agent, session_id)`.
      6. In one `state.meta.write`: `upsert_context`, plus `delete_context` for the evicted key. On an error, return it as a 500 (`ApiError` from `MetaError`, as other handlers do), with the map unchanged.
      7. Then, under the map's write lock, insert the merged context and remove the evicted key.
   5. **Log** `tracing::info!(target: "ingest", project_id = %access.project.id, agent = %agent, request_bytes, written, "agent context")`. `request_bytes` is the body length if the extractor exposes it; otherwise leave it out and say so. `written` is a bool. Log no other field.
   6. Return `StatusCode::NO_CONTENT`.
3. **Registration.** In `otlp.rs` `routes()`, append a `RouteDef { method: POST, path: "/v1/agent-context", access: Access::Key { scope: KeyScope::Ingest }, documented: true, handler: post(post_agent_context).layer(DefaultBodyLimit::max(4096)) }`. Update the doc comments of `routes()` and `otlp_router` to list the fourth route.
4. **OpenAPI** (`api/openapi.rs`), following `post_feedback`:
   - the operation `operation("post_agent_context", "ingest", "Send a coding agent's session context")`;
   - the description: "Called by `cetra agents hook`. Needs an `sk` key with the `ingest` scope. Later spans of the session carry the repository, branch and person (D-CA-9).";
   - the bearer security;
   - `json_body("AgentContextBody")`;
   - responses `204` (no body; find how other 204 operations are declared, for example a DELETE, and copy it), plus `400, 401, 402, 403, 413`.

   Register the path, and insert the schema. If operation tags are a closed list in `openapi.rs`, add `ingest` with the description "Calls made by coding agents and SDKs on the ingest key." Then run `just openapi`.

**Tests** (`routes/agent_context.rs`, `#[tokio::test(flavor = "current_thread")]`, with `TestApp`, `Seeded.ingest_key` and a local `post_context(app, key, body)` helper like `post_traces`):
- `stores_a_context_and_answers_204`: the map and `load_contexts` hold the canonical repository `github.com/acme/widgets`, given `https://github.com/Acme/Widgets.git`, plus the branch and the lowercased user.
- `absent_keeps_and_null_clears`: post `{repository, branch}`, then `{branch: null}`; the repository is kept and the branch cleared.
- `unchanged_post_within_an_hour_writes_nothing`: the second identical post leaves `updated_at` unchanged. Use the test clock, as other tests set it.
- `unchanged_post_after_an_hour_rewrites`.
- `rejects_unknown_agent_bad_session_and_unknown_fields`: 400s with the field paths; unknown field `foo` is 400.
- `needs_the_ingest_scope`: `read_key` gets 403; no key gets 401.
- `refused_over_the_units_cap`: 402, following the OTLP logs test's setup.
- `body_over_4_kib_is_413`.
- `cap_evicts_the_oldest`: lower the cap through a `#[cfg(test)]` const override. The simplest form: make the handler use a `max` read from a function `max_per_project()` that returns `MAX_PER_PROJECT` outside tests and `3` under a test-only flag. **Instead,** test the eviction rule as a pure function `fn evict_key(map: &AgentContexts, cap: usize) -> Option<(String, String)>` with cap 3, and keep the handler on `MAX_PER_PROJECT`.
- `served_on_the_otlp_port`: through `otlp_router`, the same way any existing test reaches port 4318's router. If none does, assert that `otlp::routes()` contains the path.
- `logs_hold_no_values`: `assert_logs_clean(&["dev@example.com", "feature/secret", ingest_key], post)`.
- The route-table test `route_table_matches_parameters_and_openapi_security` passes, with the new documented route found in `openapi.json`.

**Done when:**
- `cargo test -p cetra-server agent_context` passes.
- `cargo test -p cetra-server access` passes.
- `cargo test -p cetra-server --test openapi` passes.
- `cargo clippy -p cetra-server --all-targets -- -D warnings` passes.
- `git diff --stat` shows `openapi.json` and `schema.gen.ts` changed only by the new operation and schema.

**Commit:** `Add POST /v1/agent-context`

---

## S4 — The stamp at ingest, and deletion by user

**Goal:** spans received after a context carry it, and a user deletion removes the user's contexts.

**Edit:**
- `crates/server/src/ingest/mod.rs` (`accept_request` only)
- `crates/server/src/deletion/resolve.rs`
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
2. **`resolve.rs`.** In the meta write closure, after `store_counts` returns `stored == true`, and only when `row.kind` is `DeletionKind::User`, call `crate::meta::agent_contexts::delete_user_contexts(conn, project, user_id)` with the deletion's `user_id`. Capture an owned copy before the closure, as the other captured values are. After the write succeeds, remove the same entries from the map:
   - `state.agent_contexts.project(project)`, then write-lock it, then `retain(|_, _, c| c.user.as_deref() != Some(user_id))`;
   - hold `writes().lock().await` while doing it.

   A deletion by trace or by filter does nothing here.

**Tests:**
- `otlp.rs` tests, `stamped_after_a_context`:
  1. Post `/v1/agent-context` for `claude-code` / session `s1` with repository, branch and user.
  2. Post `/v1/traces` with the M1 fixture `fixtures/otlp/claude_code/bash-tool/input.json`, its `session.id` and `gen_ai.conversation.id` replaced by `s1`. Load the file, and replace the placeholder string `capture-volatile-N` that M1 wrote for the session with `s1`. If the fixture's session placeholder is not identifiable, stop and report.
  3. Read the stored spans the way existing OTLP tests do: every span has `metadata['repository'] = "github.com/acme/widgets"` and `metadata['branch']`, and `user_id` is the context's user.
- `not_stamped_for_another_session`: session `s2` gets no branch from `s1`'s context. Its `metadata['repository']` still comes from the span's own `vcs.*` attributes (M2).
- `not_stamped_before_the_context`: traces first, then the context: the stored spans have no branch.
- `stamp_on_grpc_too`: the same as the first test through the gRPC export path, the way the existing gRPC tests call it. If there is no in-process gRPC test helper, assert instead that `accept_request` is the only `normalize` call site outside tests: `grep -n "normalize(" crates/server/src --include=*.rs`. Report which way you took.
- `user_deletion_removes_the_users_contexts`: two contexts with users `a@example.com` and `b@example.com`. A deletion by user `a@example.com` resolves the way existing resolve tests drive it. Afterwards only `b`'s row and map entry remain.
- `trace_deletion_keeps_contexts`.

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

**Steps:**
1. **Spec 04**, `## Evaluation`, after the `pending_traces` table and its prose: a `### Coding-agent session contexts (D-CA-9)` subsection with:
   - S1's DDL;
   - two sentences: "Written by `POST /v1/agent-context` (last write wins; an unchanged post within an hour writes nothing), loaded into memory before the listeners open, read by ingest from memory only. An hourly sweep deletes rows older than 30 days or the project's retention when shorter; a project keeps at most 10,000 rows, the oldest evicted; a deletion by user deletes the user's rows."
2. **Spec 08**, line 124: "`ingest` (OTLP + scores write)" becomes "`ingest` (OTLP, scores write and the coding-agent context)".
3. **Spec 09:**
   - The top table gets the row `` | `/v1/agent-context` | coding-agent session context (D-CA-9); in the OpenAPI document; `204` | sk (ingest scope) | ``.
   - The listener paragraph (line 3) names the fourth route.
4. **Spec 12**, § Process lifecycle, Startup: after "Pending traces are loaded from pending_traces before the ports open (D-I-8)…", add "Coding-agent contexts are loaded from `agent_contexts` before the ports open (D-CA-9); a failed load starts empty."

**Done when:** `export PATH=…; just check` passes.

**Commit:** `Document the coding-agent session context in specs 04, 08, 09 and 12`
