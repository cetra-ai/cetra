# AQ M2 — Queues and items on the server

**Summary.** This milestone builds annotation queues in `meta.db` and their API:
- queue CRUD with an ordered list of score definitions;
- items added by id, after a light storage lookup and a deletion-guard check;
- the item list and counts;
- the atomic lease and the reviewer actions (`next`, lease, complete, skip, reopen);
- the `queue_id` check in score writes;
- the GDPR deletion cascade, retention, and the data window on items;
- the `max_annotation_queues` plan limit;
- OpenAPI, the API fixtures and the specs.

Adds by filter are M3; UI is M4; SDK is M5.

**Prerequisites.** M1 is committed (`Add queue_id to scores`): `ScoreCreate.queue_id: Option<Uuid>` and `Score.queue_id` exist. Work on branch `claude/aq-impl` in `/home/sebastien/travaux/llm-observer/.claude/worktrees/aq-annotation-queues-f38be7`.

**Read first.**
- `docs/plans/aq-annotation-queues/00-design.md`, all of it, and D-AQ-1 to D-AQ-11 in `docs/mvp-decisions.md`.
- `AGENTS.md` § Rules, § Tests and § Commands.
- The mirrors, named in each phase:
  - `crates/server/src/meta/dashboards.rs` and `crates/server/src/routes/dashboards.rs` (CRUD, version, cursor);
  - `crates/server/src/routes/projects.rs:124-160` (a limit inside the writer);
  - `crates/server/src/deletion/resolve.rs` (the cascade);
  - `crates/server/src/routes/datasets.rs:2455-2690` (`add_from_traces`: skipped list and guard);
  - `crates/storage/src/reads/traces.rs:468-537` (`trace_refresh` and its `refresh` body);
  - `crates/storage/src/reads/sessions.rs:191-237` (`extras`);
  - `crates/storage/src/reads/deletions.rs:193` (`eval_trace_ids`).

## What it is, and what it is not

**What works when it's done:**
- A member creates a trace, span or session queue with 1–50 live score definitions, and lists, reads, edits and deletes it.
- Items are added by id. Targets that are unknown, hidden, evaluation traces or guarded are listed in `skipped`.
- Items are listed and counted, without those the data window hides.
- A person claims items one at a time with `next` and holds a lease of 5 minutes. They complete, skip or reopen items, and renew or release the lease.
- A score's `queue_id` must name a queue of the project.
- GDPR deletions and retention delete items. Drops cascade.
- The Free plan's `max_annotation_queues = 1` is enforced.

**Out of scope:** adds by filter (M3); web pages (M4); SDK methods (M5).

## Decisions this plan relies on

| ID | Summary |
|---|---|
| D-AQ-2 | One target kind per queue, immutable |
| D-AQ-3 | One reviewer per item, no assignment |
| D-AQ-4 | Lease 5 min, renewed every 60 s, one per person per queue, complete/skip refused while another holds a live lease |
| D-AQ-5 | `pending` / `completed` / `skipped`; reopen; completing needs no score |
| D-AQ-6 | `scores.queue_id` checked to name a queue of the project |
| D-AQ-7 | 1–50 ordered definitions; instructions |
| D-AQ-8 | Adds by id 1–1 000 with `skipped`; guard re-checked inside the insert transaction; unique target per queue; 100 queues per project; 100 000 items per queue (`422 too_many_items`, nothing added) |
| D-AQ-9 | Cascade by deletion scope traces and their sessions; retention by `target_time`; data window hides items |
| D-AQ-10 | `max_annotation_queues`, `403 plan_limit` |
| D-AQ-11 | `ManageQueues` (member, `sk` write key, read-write PAT); `Review` (member, no key); reads `View`; user ids, never names |
| D-I-26 | Score definitions are archived, never deleted |
| D-M-6 | The deletion guard: trace ids and user ids, 24 h |
| D-P-13, D-P-14 | Limits list; `plan_limit` |

Plan-level choices (implementation detail the design leaves to the plan):
- **Column name.** The queue's creator column is `created_by` (as `dashboards`), the design's `created_by_user_id`.
- **Ids.** Item `trace_id` and `span_id` are stored as lowercase hex TEXT. The cascade compares them with `lower(hex(deletion_traces.trace_id))`.
- **Stale version.** `ApiError::StaleVersion` gains a `what: &'static str`. Dashboards pass `"dashboard"`, so their text is unchanged; queues pass `"annotation queue"`.
- **Lease conflict.** A lease conflict is `ApiError::ConflictWith { detail: "another reviewer holds this item", errors: [FieldError { field: "leased_by_user_id", message: <holder uuid> }] }`.
- **Item cap.** The 100 000-item cap uses a SQLite `SAVEPOINT`: insert, count, and `ROLLBACK TO` when over. That way, existing targets never count.
- **Add answer.** `201` for an add whatever the counts (a `skipped`-only answer is still `201`).

## Versions

No new dependency.

## Phase graph

```
Q1 storage reads ─► Q2a tables + queue/item store ─► Q2b lease/cascade store ─► Q3 plan limit ─► Q4a queue routes ─► Q4b item routes ─► Q5 review routes ─► Q6 cascades and score check
```

All phases are Rust in the same workspace, so they run one after the other (two agents compiling and running `just check` on the same tree would see each other's half-done edits). M3 (storage), M4 (web) and M5 (SDKs) start after Q5, when the API contract is complete.

Rules for every phase:
- Rust: compile and test only the targeted tests named by the phase while iterating. Run `just check` once, right before the commit.
- Node: prefix `just openapi`, `just api-examples` and `just check` with `export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH && `.
- Logs carry ids and counts only: never names, instructions, reasons or session ids (AGENTS.md § Privacy; a session id may identify a person).

---

### Q1 — Storage reads for item targets and the cascade

**Goal.** Light storage reads that find trace, span and session targets by id with their `target_time`, and the session ids of a set of traces.

**Edit:**
- `crates/storage/src/reads/targets.rs` (new)
- `crates/storage/src/reads/deletions.rs`
- `crates/storage/src/reads/mod.rs`
- `crates/storage/src/lib.rs` (the `pub use reads::{…}` list only)

**Steps.**
1. Create `crates/storage/src/reads/targets.rs`. Its module doc reads: `//! Light lookups of annotation-queue targets by id (D-AQ-8): which traces, spans and sessions exist and are visible, with their target time.` It has these public items:
   ```rust
   /// Most ids one lookup takes (D-AQ-8: an add takes 1–1 000 targets).
   pub const MAX_TARGET_IDS: usize = 1_000;

   #[derive(Debug, Clone, PartialEq, Eq)]
   pub struct TraceTarget { pub trace_id: TraceId, pub start_time: i64 }

   #[derive(Debug, Clone, PartialEq, Eq)]
   pub struct SpanTarget { pub trace_id: TraceId, pub span_id: SpanId, pub trace_start: i64 }

   #[derive(Debug, Clone, PartialEq, Eq)]
   pub struct SessionTarget {
       pub session_id: String,
       /// Start of the session's earliest visible trace.
       pub start_time: i64,
       /// The session's visible trace ids, sorted, for the deletion-guard check.
       pub trace_ids: Vec<TraceId>,
   }

   pub async fn trace_targets(ctx: &QueryContext, ids: &[TraceId], timeout: Duration) -> Result<Vec<TraceTarget>, RunError>;
   pub async fn span_targets(ctx: &QueryContext, ids: &[(TraceId, SpanId)], timeout: Duration) -> Result<Vec<SpanTarget>, RunError>;
   pub async fn session_targets(ctx: &QueryContext, ids: &[&str], timeout: Duration) -> Result<Vec<SessionTarget>, RunError>;
   ```
2. Shared rules for the three:
   - Empty `ids`: return `Ok(vec![])` without a query.
   - More than `MAX_TARGET_IDS`: return the same `RunError` variant that `trace_refresh` returns for more than `MAX_REFRESH_IDS` (read it there and reuse it).
   - Each runs one DataFusion query, or two for spans, under `tokio::time::timeout(timeout, …)`, mapping the elapsed timeout to `RunError::Timeout` as `eval_trace_ids` does.
   - The context's visibility (`visible_from`) applies as for every view read.
   - **Evaluation traces are excluded and experiment traces are included.** For the trace rows, start from `reads::scoped_traces(ctx, ListScope { window: None, include_eval: false, include_experiments: true }, false)` (`reads/mod.rs`, around line 132). It already applies those exclusions and the context's visibility.
   - `start_time` is read as `i64` nanoseconds with the same Arrow downcast the existing trace reads use on that column (see how `reads/traces.rs` converts `start_time`).
   - Answer order: by trace id (`trace_targets`, `span_targets`) or by session id (`session_targets`). Ids not found are absent. Duplicate input ids give one answer row.
3. `trace_targets`: mirror `refresh` (`reads/traces.rs:495-537`) for the `IN` list. Over `scoped_traces` (above), filter `trace_id IN (…)` with the hex of each id, and select `trace_id` and `start_time`.
4. `span_targets`:
   - call `trace_targets` with the distinct trace ids of the pairs;
   - over the `spans` view, select `trace_id` and `span_id` where `trace_id IN (…)` of the found traces, and keep the rows whose pair is in the request (an in-memory `HashSet<(TraceId, SpanId)>`);
   - answer each found pair with its trace's `start_time` as `trace_start`.
5. `session_targets`: mirror `extras` (`reads/sessions.rs:191-237`). Over `scoped_traces` (above), filter `session_id IN (…)` and select `session_id`, `trace_id` and `start_time`. Group in memory per session: `start_time` is the minimum, and `trace_ids` are the sorted distinct ids.
6. In `crates/storage/src/reads/deletions.rs`, add the following, mirroring `eval_trace_ids` (one scan of the project's `spans`, in-memory match per batch, timeout):
   ```rust
   /// The distinct non-null `session_id`s of the spans of `traces` (D-AQ-9: annotation-queue
   /// session items follow the deletion of any of their traces). Sorted. Exempt visibility comes
   /// from the caller's context.
   pub async fn session_ids_of_traces(ctx: &QueryContext, traces: &HashSet<TraceId>, timeout: Duration) -> Result<Vec<String>, RunError>;
   ```
   An empty `traces` returns `Ok(vec![])` at once.
7. In `reads/mod.rs`, add `mod targets;` and re-export the new items, and export `session_ids_of_traces` with the other deletion reads. In `crates/storage/src/lib.rs`, add them to the `pub use reads::{…}` list.

**Tests** (in `targets.rs` and `deletions.rs`). The helpers of the `reads/deletions.rs` tests (`context_of`, `span_row`, `id`, `TIMEOUT`) are private to that test module: copy the ones you need into `targets.rs`'s test module. For the window test, build the context with a `visible_from` the way an existing window test in `reads/traces.rs` does (search its tests for `visible_from`).
- `trace_targets_finds_visible_traces`: two traces plus an unknown id return the two with their start times, sorted.
- `trace_targets_leave_out_evaluation_traces`: an `environment` evaluation trace is absent; an experiment trace is present.
- `trace_targets_respect_the_window`: with a `visible_from` after the trace's start, the trace is absent. Build the context with `QueryOptions` as the other window tests do.
- `trace_targets_dedup_and_empty`: duplicates give one row; an empty input returns empty.
- `too_many_ids_are_refused`: 1 001 ids return the error.
- `span_targets_match_pairs`: a known pair is answered with `trace_start`; a known trace with an unknown span id and an unknown trace are absent.
- `session_targets_group_traces`: a session of three traces at t1 < t2 < t3 answers `start_time = t1` and three sorted ids. An eval trace of the same session is not among them. An unknown session is absent.
- `session_ids_of_traces_collects_sessions`: traces of two sessions plus a trace without a session return the two session ids, sorted; an empty set returns empty.

**Done when.**
- `cargo fmt --all --check`
- `cargo test -p cetra-storage reads::targets` and `cargo test -p cetra-storage session_ids_of_traces`
- `just lint`
- `export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH && just check` (once, before the commit)

**Commit:** `Add storage lookups for annotation queue targets`

---

### Q2a — Migration and the queue and item store

**Goal.** The three tables and the store functions of queues and items (create, read, update, delete, add, list), tested at the store level.

**Edit:**
- `crates/server/migrations/meta/0023_annotation_queues.sql` (new)
- `crates/server/src/meta/annotation_queues.rs` (new)
- `crates/server/src/meta/mod.rs` (module line only)
- `crates/server/tests/snapshots/meta_schema.snap`
- `docs/spec/04-metadata-db.md`

**Steps.**
1. Write the migration exactly:
   ```sql
   -- Annotation queues (track AQ, D-AQ-2–D-AQ-9).
   CREATE TABLE annotation_queues (
     id TEXT PRIMARY KEY,                -- UUIDv7
     project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
     name TEXT NOT NULL,
     description TEXT,
     instructions TEXT,                  -- Markdown
     target TEXT NOT NULL CHECK (target IN ('trace', 'span', 'session')),
     version INTEGER NOT NULL DEFAULT 1,
     created_by TEXT REFERENCES users(id) ON DELETE SET NULL,
     created_at INTEGER NOT NULL,
     updated_at INTEGER NOT NULL,
     UNIQUE (project_id, name)
   );

   CREATE TABLE annotation_queue_definitions (
     queue_id TEXT NOT NULL REFERENCES annotation_queues(id) ON DELETE CASCADE,
     definition_id TEXT NOT NULL REFERENCES score_definitions(id) ON DELETE CASCADE,
     position INTEGER NOT NULL,
     PRIMARY KEY (queue_id, definition_id)
   );

   CREATE TABLE annotation_queue_items (
     id TEXT PRIMARY KEY,                -- UUIDv7
     queue_id TEXT NOT NULL REFERENCES annotation_queues(id) ON DELETE CASCADE,
     project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
     trace_id TEXT,                      -- lowercase hex; trace and span items
     span_id TEXT,                       -- lowercase hex; span items
     session_id TEXT,                    -- session items
     target_time INTEGER NOT NULL,       -- ns: trace start, or the session's earliest visible trace start
     status TEXT NOT NULL DEFAULT 'pending' CHECK (status IN ('pending', 'completed', 'skipped')),
     skip_reason TEXT,
     leased_by_user_id TEXT REFERENCES users(id) ON DELETE SET NULL,
     lease_expires_at INTEGER,
     done_by_user_id TEXT REFERENCES users(id) ON DELETE SET NULL,
     done_at INTEGER,
     added_by_kind TEXT NOT NULL CHECK (added_by_kind IN ('user', 'pat', 'key')),
     added_by_id TEXT NOT NULL,
     created_at INTEGER NOT NULL,
     updated_at INTEGER NOT NULL
   );
   CREATE UNIQUE INDEX annotation_queue_items_target ON annotation_queue_items(
     queue_id, coalesce(trace_id, ''), coalesce(span_id, ''), coalesce(session_id, '')
   );
   CREATE INDEX annotation_queue_items_claim ON annotation_queue_items(queue_id, status, id);
   CREATE INDEX annotation_queue_items_trace ON annotation_queue_items(project_id, trace_id) WHERE trace_id IS NOT NULL;
   CREATE INDEX annotation_queue_items_session ON annotation_queue_items(project_id, session_id) WHERE session_id IS NOT NULL;
   CREATE INDEX annotation_queue_items_time ON annotation_queue_items(project_id, target_time);
   ```
2. Create `meta/annotation_queues.rs`. Module doc: `` //! `annotation_queues`, `annotation_queue_definitions` and `annotation_queue_items` rows (spec 04 § Annotation queues, D-AQ-2–D-AQ-9). SQL text is built only from constants here; every caller value is bound. `` Follow `meta/dashboards.rs`: private `FromRow` records, `*_from` converters that map bad text to `corrupt(table, column)`, and `AssertSqlSafe` for built SQL. Constants and types:
   ```rust
   pub(crate) const MAX_QUEUES_PER_PROJECT: u64 = 100;
   pub(crate) const MAX_ITEMS_PER_QUEUE: u64 = 100_000;
   pub(crate) const MAX_DEFINITIONS: usize = 50;
   pub(crate) const LEASE_NS: i64 = 300_000_000_000; // 5 minutes (D-AQ-4)

   #[derive(Debug, Clone, Copy, PartialEq, Eq)]
   pub(crate) enum Target { Trace, Span, Session }          // text 'trace' | 'span' | 'session'
   #[derive(Debug, Clone, Copy, PartialEq, Eq)]
   pub(crate) enum Status { Pending, Completed, Skipped }   // text 'pending' | 'completed' | 'skipped'
   #[derive(Debug, Clone, Copy, PartialEq, Eq)]
   pub(crate) enum AddedByKind { User, Pat, Key }           // text 'user' | 'pat' | 'key'

   #[derive(Debug, Clone, PartialEq, Eq, Default)]
   pub(crate) struct Counts { pub(crate) pending: u64, pub(crate) completed: u64, pub(crate) skipped: u64 }

   #[derive(Debug, Clone, PartialEq, Eq)]
   pub(crate) struct QueueRow {
       pub(crate) id: Uuid, pub(crate) project_id: ProjectId, pub(crate) name: String,
       pub(crate) description: Option<String>, pub(crate) instructions: Option<String>,
       pub(crate) target: Target, pub(crate) definition_ids: Vec<Uuid>, // by position
       pub(crate) version: i64, pub(crate) created_by: Option<Uuid>,
       pub(crate) created_at: i64, pub(crate) updated_at: i64,
   }

   #[derive(Debug, Clone, PartialEq, Eq)]
   pub(crate) struct ItemRow {
       pub(crate) id: Uuid, pub(crate) queue_id: Uuid, pub(crate) project_id: ProjectId,
       pub(crate) trace_id: Option<String>, pub(crate) span_id: Option<String>, pub(crate) session_id: Option<String>,
       pub(crate) target_time: i64, pub(crate) status: Status, pub(crate) skip_reason: Option<String>,
       pub(crate) leased_by_user_id: Option<Uuid>, pub(crate) lease_expires_at: Option<i64>,
       pub(crate) done_by_user_id: Option<Uuid>, pub(crate) done_at: Option<i64>,
       pub(crate) added_by_kind: AddedByKind, pub(crate) added_by_id: Uuid,
       pub(crate) created_at: i64, pub(crate) updated_at: i64,
   }

   pub(crate) struct NewQueue { pub(crate) name: String, pub(crate) description: Option<String>, pub(crate) instructions: Option<String>, pub(crate) target: Target, pub(crate) definition_ids: Vec<Uuid>, pub(crate) created_by: Option<Uuid> }

   /// `None` leaves a field unchanged; `Some(None)` clears description or instructions.
   pub(crate) struct QueueChanges { pub(crate) name: Option<String>, pub(crate) description: Option<Option<String>>, pub(crate) instructions: Option<Option<String>>, pub(crate) definition_ids: Option<Vec<Uuid>> }

   pub(crate) enum QueueCreate { Created(QueueRow), OverPlanLimit(u64), UnknownDefinitions(Vec<usize>) }
   pub(crate) enum QueueUpdate { Updated(QueueRow), Stale(i64), Missing, UnknownDefinitions(Vec<usize>) }

   /// One target to insert; exactly the fields of the queue's kind are set.
   pub(crate) struct NewItem { pub(crate) trace_id: Option<String>, pub(crate) span_id: Option<String>, pub(crate) session_id: Option<String>, pub(crate) target_time: i64 }
   pub(crate) enum ItemsInsert { Done { added: u64, existing: u64 }, TooMany }

   pub(crate) enum ReviewOutcome { Done(ItemRow), Missing, NotPending, Pending, HeldBy(Uuid) }
   ```
   The `Target`, `Status` and `AddedByKind` enums each get `fn as_str(self) -> &'static str` and `fn parse(text: &str) -> Option<Self>`.
3. Functions, with their exact signatures. Times are passed in, never read from a clock. `visible_from: Option<i64>` means "leave out items with `target_time < visible_from`".
   - `pub(crate) async fn insert_queue_tx(conn: &mut SqliteConnection, project: ProjectId, org: OrgId, queue: NewQueue, plan_limit: Option<u64>, now: i64) -> Result<QueueCreate, MetaError>`. Steps, in order:
     1. Count the project's queues. At `MAX_QUEUES_PER_PROJECT` or more, return `Err(MetaError::Conflict("too many annotation queues"))`.
     2. If `plan_limit` is `Some(limit)`, count the org's queues (`count_org_queues_tx`). If `crate::entitlements::over_limit(Some(limit), count, 1)`, return `Ok(OverPlanLimit(limit))`.
     3. Check the definitions with `live_definitions_tx`. If any index is missing, return `Ok(UnknownDefinitions(indexes))`.
     4. Insert the queue with `id = Uuid::now_v7()` and `version = 1`, then the definition rows with `position` 0..n. A unique violation on the name maps through `unique_or(err, "an annotation queue with this name exists")`.
     5. Return the row.
   - `pub(crate) async fn count_org_queues_tx(conn: &mut SqliteConnection, org: OrgId) -> Result<u64, MetaError>`: `SELECT COUNT(*) FROM annotation_queues WHERE project_id IN (SELECT id FROM projects WHERE org_id = ?)`.
   - `async fn live_definitions_tx(conn, project: ProjectId, ids: &[Uuid]) -> Result<Vec<usize>, MetaError>`: the indexes of `ids` that are not a non-archived `score_definitions` row of the project.
   - `pub(crate) async fn get(pool: &SqlitePool, project: ProjectId, id: Uuid) -> Result<Option<QueueRow>, MetaError>` and `get_on(conn: &mut SqliteConnection, …)`, the same in a transaction. Both load `definition_ids` ordered by `position`.
   - `pub(crate) async fn list(pool: &SqlitePool, project: ProjectId, after: Option<(&str, Uuid)>, limit: usize, holding: Option<Holding<'_>>, visible_from: Option<i64>) -> Result<(Vec<QueueRow>, Option<(String, Uuid)>), MetaError>`.
     - Keyset on `(name, id)`, fetching `limit + 1`, as `dashboards::list` does.
     - `pub(crate) enum Holding<'a> { Trace(&'a str), Session(&'a str) }` keeps the queues that hold a visible item with that `trace_id` (trace and span items) or that `session_id`.
   - `pub(crate) async fn counts(pool: &SqlitePool, project: ProjectId, queues: &[Uuid], visible_from: Option<i64>) -> Result<HashMap<Uuid, Counts>, MetaError>`: one `SELECT queue_id, status, COUNT(*) … GROUP BY queue_id, status`. Queues with no items are absent from the map; callers use `Counts::default()`.
   - `pub(crate) async fn update_tx(conn: &mut SqliteConnection, project: ProjectId, id: Uuid, version: i64, changes: QueueChanges, now: i64) -> Result<QueueUpdate, MetaError>`.
     - Check `definition_ids`, when given, as on create.
     - Then run one `UPDATE … WHERE project_id = ? AND id = ? AND version = ?`, with `version = version + 1` and `updated_at = now`. With `definition_ids`, replace the definition rows: delete all of them and insert them again with new positions.
     - With 0 rows affected, answer `Stale(current)` or `Missing`, as dashboards do.
     - Map a name conflict as on create.
   - `pub(crate) async fn delete(conn: &mut SqliteConnection, project: ProjectId, id: Uuid) -> Result<bool, MetaError>`: the items and definition rows go by FK cascade.
   - `pub(crate) async fn existing_ids(pool: &SqlitePool, project: ProjectId, ids: &[Uuid]) -> Result<HashSet<Uuid>, MetaError>`: the queues of the project among `ids`; empty in, empty out. Mirror `meta::experiments::existing_ids`.
   - `pub(crate) async fn insert_items_tx(conn: &mut SqliteConnection, queue: Uuid, project: ProjectId, items: &[NewItem], added_by: (AddedByKind, Uuid), now: i64) -> Result<ItemsInsert, MetaError>`. Steps, in order:
     1. `SAVEPOINT add_items`.
     2. For each item, `INSERT … ON CONFLICT DO NOTHING` (no conflict target, because the unique index is an expression index), with `id = Uuid::now_v7()`, `status = 'pending'` and `created_at = updated_at = now`. Sum `rows_affected()` as `added`.
     3. Count the queue's items. Above `MAX_ITEMS_PER_QUEUE`, run `ROLLBACK TO add_items; RELEASE add_items` and return `Ok(TooMany)`.
     4. Otherwise run `RELEASE add_items` and return `Done { added, existing: items.len() as u64 - added }`.
   - `pub(crate) async fn list_items(pool: &SqlitePool, queue: Uuid, status: Option<Status>, after: Option<Uuid>, limit: usize, visible_from: Option<i64>) -> Result<(Vec<ItemRow>, Option<Uuid>), MetaError>`: by `id`, fetching `limit + 1`.
   - `pub(crate) async fn get_item_on(conn: &mut SqliteConnection, queue: Uuid, item: Uuid, visible_from: Option<i64>) -> Result<Option<ItemRow>, MetaError>`.
   - `pub(crate) async fn delete_item(conn: &mut SqliteConnection, queue: Uuid, item: Uuid) -> Result<bool, MetaError>`.
4. Register `pub(crate) mod annotation_queues;` in `meta/mod.rs` (alphabetical).
5. Bless the schema snapshot with `INSTA_UPDATE=always cargo test -p cetra-server --lib schema_matches_the_snapshot` (find its exact path with `cargo test -p cetra-server --lib -- --list | grep schema_matches`). Check the diff: only the three tables and their indexes are added.
6. In `docs/spec/04-metadata-db.md`, add `## Annotation queues` after § Dashboards:
   - the migration's SQL;
   - one paragraph: "One kind per queue (D-AQ-2). Items are unique per target; `target_time` drives retention and the data window (D-AQ-9). The lease is `leased_by_user_id` + `lease_expires_at`, 5 minutes, claimed atomically on the writer (D-AQ-4). At most 100 queues per project, 100 000 items per queue, 50 definitions per queue."

**Tests** (in `meta/annotation_queues.rs`, with `crate::meta::test_db()`). Seed orgs, projects and users as the `meta/experiments.rs` tests do. Seed score definitions with `meta::score_definitions::insert_definition` and `DefinitionFields`: that module's test helpers (`org`, `project`, `fields`, `archived`) are private, so copy what you need or call `insert_definition` directly. Tests:
- `create_get_list_and_delete`: create two queues and check `get`, the `list` order by name, the cursor, and that `delete` removes the definitions and items (FK).
- `name_is_unique_per_project`: a second create with the same name is `MetaError::Conflict`.
- `project_cap_is_100`: the 101st create is `Conflict("too many annotation queues")`.
- `plan_limit_counts_the_org`: with `plan_limit = Some(1)` and a queue in another project of the same org, the create returns `OverPlanLimit(1)`; with `None` it creates.
- `unknown_or_archived_definitions_are_reported`: an archived definition and a definition of another project are reported by index.
- `update_checks_the_version`: `Updated` bumps `version` and replaces the definitions in their new order; a stale version answers `Stale(current)`; a missing id answers `Missing`; `Some(None)` clears the description.
- `items_are_unique_per_target`: adding the same trace twice, and the same `(trace, span)` twice, counts as `existing`; trace T and the span item (T, S) can live in different queues.
- `item_cap_rolls_back`: with a test-only lower cap (make the cap a function argument, `insert_items_tx_capped(…, cap)`, called by `insert_items_tx` with `MAX_ITEMS_PER_QUEUE`), an add past the cap answers `TooMany` and inserts nothing.
- `existing_ids_ignores_other_projects`.

**Done when.**
- `cargo fmt --all --check`
- `cargo test -p cetra-server --lib meta::annotation_queues`
- `cargo test -p cetra-server --lib schema_matches_the_snapshot`
- `just lint`
- `export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH && just check` (once, before the commit)

**Commit:** `Add annotation queue tables and store`

---

### Q2b — Lease, review and cascade store functions

**Goal.** The store functions of the reviewer actions, the deletion cascade and retention, tested at the store level.

**Edit:**
- `crates/server/src/meta/annotation_queues.rs`

**Steps.**
1. Add these functions to `meta/annotation_queues.rs`, with Q2a's types (`ReviewOutcome`, `Status`, `LEASE_NS`):
   - `pub(crate) async fn claim_next_tx(conn: &mut SqliteConnection, queue: Uuid, user: Uuid, after: Option<Uuid>, visible_from: Option<i64>, now: i64) -> Result<Option<ItemRow>, MetaError>`. Steps, in order:
     1. Run one `UPDATE annotation_queue_items SET leased_by_user_id = ?, lease_expires_at = ?, updated_at = ? WHERE id = (SELECT id FROM annotation_queue_items WHERE queue_id = ? AND status = 'pending' AND (leased_by_user_id IS NULL OR lease_expires_at <= ? OR leased_by_user_id = ?) AND (? IS NULL OR target_time >= ?) AND id > ? ORDER BY id LIMIT 1) RETURNING <item columns>`. Bind `lease_expires_at = now + LEASE_NS`. Bind the `id >` bound to `after` as text, or to `''` without `after`.
     2. If no row comes back and `after` was given, run the same statement once more with `''` (the wrap-around).
     3. Only when a row was claimed, release the user's leases on the queue's other items: set `leased_by_user_id` and `lease_expires_at` to NULL where `queue_id = ? AND leased_by_user_id = ? AND id <> <claimed id>`. With nothing claimed, the user's current lease stays.
     4. Return the row or `None`.
   - `pub(crate) async fn lease_tx(conn, queue: Uuid, item: Uuid, user: Uuid, visible_from: Option<i64>, now: i64) -> Result<ReviewOutcome, MetaError>`.
     - First set the lease on `item` (one `UPDATE … WHERE id = ? AND queue_id = ? AND status = 'pending' AND (leased_by_user_id IS NULL OR lease_expires_at <= ? OR leased_by_user_id = ?) AND (? IS NULL OR target_time >= ?) RETURNING …`).
     - Only when that row was updated, release the user's other leases in the queue (`id <> item`), then return `Done(row)`. A failed lease never drops the reviewer's current lease, because the transaction commits on `Ok`.
     - If no row is updated, read the item (`get_item_on`) to name the cause: `Missing` (absent or hidden), `NotPending`, or `HeldBy(holder)`.
   - `pub(crate) async fn release_tx(conn, queue: Uuid, item: Uuid, user: Uuid) -> Result<(), MetaError>`: clears the lease when the user holds it, and does nothing otherwise.
   - `pub(crate) async fn finish_tx(conn, queue: Uuid, item: Uuid, user: Uuid, status: Status, skip_reason: Option<String>, visible_from: Option<i64>, now: i64) -> Result<ReviewOutcome, MetaError>`. `status` is `Completed` or `Skipped`.
     - Run one `UPDATE … SET status = ?, skip_reason = ?, done_by_user_id = ?, done_at = ?, leased_by_user_id = NULL, lease_expires_at = NULL, updated_at = ? WHERE id = ? AND queue_id = ? AND status = 'pending' AND (leased_by_user_id IS NULL OR lease_expires_at <= ? OR leased_by_user_id = ?) AND (? IS NULL OR target_time >= ?) RETURNING …`.
     - If no row comes back, answer `Missing`, `NotPending` or `HeldBy` as `lease_tx` does.
     - `skip_reason` is stored only for `Skipped`.
   - `pub(crate) async fn reopen_tx(conn, queue: Uuid, item: Uuid, visible_from: Option<i64>, now: i64) -> Result<ReviewOutcome, MetaError>`.
     - Set `status = 'pending'` and clear `skip_reason`, `done_by_user_id`, `done_at` and the lease, where the status is `completed` or `skipped`.
     - If no row is updated, answer `Missing`, or `Pending` for an item that is already pending.
   - `pub(crate) async fn has_session_items(pool: &SqlitePool, project: ProjectId) -> Result<bool, MetaError>`: whether any item of the project has a `session_id`. The cascade uses it to skip the storage scan.
   - `pub(crate) async fn delete_items_of_deletion_tx(conn: &mut SqliteConnection, deletion: Uuid, project: ProjectId, sessions: &[String]) -> Result<u64, MetaError>`.
     - `DELETE FROM annotation_queue_items WHERE project_id = ? AND trace_id IN (SELECT lower(hex(trace_id)) FROM deletion_traces WHERE deletion_id = ?)`.
     - Then delete where `project_id = ? AND session_id IN (…)`, in chunks of 500 binds.
     - Return the total deleted.
   - `pub(crate) async fn delete_items_before(conn: &mut SqliteConnection, project: ProjectId, cutoff: i64) -> Result<u64, MetaError>`: `DELETE … WHERE project_id = ? AND target_time < ?`.

**Tests** (in `meta/annotation_queues.rs`, with Q2a's test seeding):
- `claim_takes_the_first_free_item_and_wraps`: two users, three items. User A gets item 1; user B gets item 2; A, with `after = 1`, gets item 3 and loses item 1's lease. B, with `after = 3`, wraps around and gets item 1. An expired lease (now past `lease_expires_at`) is claimable again.
- `claim_respects_the_window`: an item with `target_time` before `visible_from` is never claimed and is absent from `list_items` and `counts`.
- `finish_and_reopen`:
  - complete by the holder works;
  - finishing an item another user holds answers `HeldBy`;
  - finishing a completed item answers `NotPending`;
  - skip stores the reason;
  - reopen clears the done fields and the reason;
  - reopening a pending item answers `Pending`.
- `lease_renews_and_releases`: `lease_tx` renews for the holder, answers `HeldBy` for another user while the lease is live, and succeeds for them after expiry. `release_tx` by the holder clears the lease; by another user it changes nothing.
- `failed_lease_keeps_the_current_lease`: user A holds item 1; B holds item 2; A's `lease_tx` on item 2 answers `HeldBy(B)` and A still holds item 1. A `claim_next_tx` that finds nothing free keeps A's lease too.
- `cascade_deletes_trace_span_and_session_items`:
  - Insert a `deletions` row first (the FK of `deletion_traces`), with the insert function and `NewDeletion` the tests of `deletion/mod.rs` use, then its traces with `meta::deletions::insert_traces`.
  - Trace and span items of those traces are deleted.
  - Session items of the `sessions` argument are deleted.
  - The other items stay.
- `delete_items_before_cutoff`: items before the cutoff go and the others stay.

**Done when.**
- `cargo fmt --all --check`
- `cargo test -p cetra-server --lib meta::annotation_queues`
- `just lint`
- `export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH && just check` (once, before the commit)

**Commit:** `Add annotation queue lease and cascade store`

---

### Q3 — The `max_annotation_queues` limit

**Goal.** The new limit exists everywhere limits are carried (D-AQ-10); nothing checks it yet (Q4 does).

**Edit:**
- `crates/token/src/lib.rs`
- `crates/server/src/entitlements/mod.rs`
- `crates/server/src/entitlements/tokens.rs`
- `crates/server/src/config.rs`
- `crates/server/src/api/entitlements.rs`
- `crates/cli/src/main.rs`
- `crates/server/openapi.json` and `web/src/api/schema.gen.ts` (generated)
- `website/src/content/docs/docs/self-hosting/configuration-reference.md`
- `docs/spec/12-operations.md`
- `docs/mvp-decisions.md` (the D-P-13 row only)

**Steps.** In each place below, add `max_annotation_queues` right after `max_active_online_rules`, copying that field's attributes and doc style.
1. `crates/token/src/lib.rs` `LimitsClaim`: add the field `/// Annotation queues per org (D-AQ-10).` with `#[serde(default, with = "double_option", skip_serializing_if = "Option::is_none")] pub max_annotation_queues: Option<Option<u64>>,`. Add `max_annotation_queues: Some(None)` to the test literal. Change the doc comment "The nine limits of D-P-13" to "The ten limits of D-P-13".
2. `crates/server/src/entitlements/mod.rs`:
   - `Limits` gets `pub(crate) max_annotation_queues: Option<u64>`, and its doc says ten limits;
   - `Defaults::from_config` copies `config.max_annotation_queues`.
3. `crates/server/src/entitlements/tokens.rs` `merge_limits`: add `max_annotation_queues: merge(claim.max_annotation_queues, defaults.max_annotation_queues),`, and fix any exhaustive literal in its tests.
4. `crates/server/src/config.rs` `EntitlementDefaults`: add the field, `None` in `Default`, and the matching assertion in the defaults tests (around line 2587).
5. `crates/server/src/api/entitlements.rs` `OrgLimits`: add the field with `#[serde(default, skip_serializing_if = "Option::is_none")]`, and map it in `From<Limits>`.
6. `crates/cli/src/main.rs` `limit_rows`: make the array length 10 and add `("max_annotation_queues", limits.max_annotation_queues)` last. Add `max_annotation_queues` to the `OrgLimits { … }` literal near line 1165, and fix any CLI test or snapshot that pins the rows. Leave `fixtures/license/payloads/*.json` and `web/src/routes/$org/settings/plan.tsx` alone (an omitted limit takes the default; the web row is M4).
7. `just openapi`.
8. `configuration-reference.md`: after the `entitlements.defaults.max_active_online_rules` row, add `| \`entitlements.defaults.max_annotation_queues\` | | integer | unset (unlimited) | Annotation queues per organization (D-AQ-10). |`, matching that table's columns.
9. `docs/spec/12-operations.md`: add `max_annotation_queues` to the two lists of limit names (the `entitlements.defaults.<limit>` row and the § Entitlements paragraph).
10. `docs/mvp-decisions.md` D-P-13: append `max_annotation_queues` to the list, followed by "(D-AQ-10)".

**Tests:**
- `crates/token`: an old token without the key verifies and leaves it `None`. A claim with `"max_annotation_queues": 1` round-trips.
- `entitlements/tokens.rs`: `merge_limits` takes the claim's value, and falls back to the default when the claim omits the key.
- `config.rs`: `CETRA_ENTITLEMENTS__DEFAULTS__MAX_ANNOTATION_QUEUES=2` loads as `Some(2)` (mirror the existing env test for another limit, if there is one; otherwise use a TOML config test).
- `the_configuration_reference_matches_the_loader` passes.

**Done when.**
- `cargo fmt --all --check`
- `cargo test -p cetra-token`
- `cargo test -p cetra-server --lib entitlements config`
- `cargo test -p cetra-cli`
- `just lint`
- `export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH && just check` (once, before the commit)

**Commit:** `Add the max_annotation_queues limit`

---

### Q4a — Queue routes

**Goal.** Queue CRUD with permissions, errors, OpenAPI, fixtures and specs.

**Edit:**
- `crates/server/src/auth/roles.rs`
- `crates/server/src/http/error.rs` and `crates/server/src/api/common.rs` (the `StaleVersion` change and `too_many_items`)
- `crates/server/src/routes/dashboards.rs` (the two `StaleVersion` constructors only)
- `crates/server/src/api/annotation_queues.rs` (new) and `crates/server/src/api/mod.rs`
- `crates/server/src/routes/annotation_queues.rs` (new) and `crates/server/src/routes/mod.rs`
- `crates/server/src/http/access.rs` (route registration line only)
- `crates/server/src/api/openapi.rs`
- `crates/server/src/api_examples.rs` and `crates/server/tests/api_examples.rs`
- `fixtures/api/static/annotation-queue*.json` (new), `fixtures/api/requests/annotation-queue*.json` (new), `fixtures/api/manifest.json` (generated) and `fixtures/api/README.md`
- `crates/server/openapi.json` and `web/src/api/schema.gen.ts` (generated)
- `docs/spec/08-auth.md` and `docs/spec/09-api.md`
- `website/src/content/docs/docs/reference/api/overview.mdx` (error table row)

**Steps.**
1. **Permissions** (`auth/roles.rs`): add `ManageQueues` and `Review` to `ProjectPerm`, with these doc comments:
   - `/// Annotation queues: create, edit, delete, add and remove items (D-AQ-11). Member and above; a `write` API key; a read-write personal access token.`
   - `/// Work annotation queue items: next, lease, complete, skip, reopen (D-AQ-11). Member and above; never an API key, because a lease names a person.`

   In `min_role`, both go in the Member arm. In `key_scope`, `ManageQueues => Some(KeyScope::Write)` and `Review => None`. Extend `permissions_follow_the_spec_table` with both.
2. **Errors:**
   - `ApiError::StaleVersion { current }` becomes `StaleVersion { what: &'static str, current: i64 }`, rendering `"the {what} changed; current version is {current}"`. The existing constructors (`routes/dashboards.rs` around lines 460 and 533) and the test in `http/error.rs` (around line 638) pass `what: "dashboard"`, so the dashboards text and `fixtures/api/static/dashboard-stale.json` are unchanged.
   - Add `ProblemCode::TooManyItems` (serde `too_many_items`, status 422, the same reason phrase as `TooManyTraces`), to the status test.
   - Add `ApiError::TooManyItems`, whose problem has detail `"the queue would hold more than 100000 items"`.
3. **API types** in `api/annotation_queues.rs`. Use the derives and attributes of `api/dashboards.rs`: `ToSchema`, `deny_unknown_fields` on bodies, optional fields omitted when `None`, and times as RFC 3339 through `cetra_core::time_serde` with `#[schema(value_type = String, format = DateTime)]`.
   ```rust
   #[derive(..., ToSchema)] #[serde(rename_all = "snake_case")]
   pub enum QueueTarget { Trace, Span, Session }
   #[derive(..., ToSchema)] #[serde(rename_all = "snake_case")]
   pub enum QueueItemStatus { Pending, Completed, Skipped }
   #[derive(..., ToSchema)] #[serde(rename_all = "snake_case")]
   pub enum QueueItemAddedBy { User, Pat, Key }

   pub struct QueueCounts { pub pending: u64, pub completed: u64, pub skipped: u64 }

   pub struct AnnotationQueue {
       pub id: Uuid, pub name: String,
       pub description: Option<String>, pub instructions: Option<String>,   // omitted when None
       pub target: QueueTarget, pub definition_ids: Vec<Uuid>,
       pub version: i64, pub created_by: Option<Uuid>,                       // omitted when None
       pub created_at: i64, pub updated_at: i64,                             // RFC 3339
       pub counts: QueueCounts,
   }
   pub struct CreateAnnotationQueue { pub name: String, pub description: Option<String>, pub instructions: Option<String>, pub target: QueueTarget, pub definition_ids: Vec<Uuid> }
   /// An omitted field is unchanged; "" clears description or instructions.
   pub struct UpdateAnnotationQueue { pub version: i64, pub name: Option<String>, pub description: Option<String>, pub instructions: Option<String>, pub definition_ids: Option<Vec<Uuid>> }

   pub struct AnnotationQueueListParams {   // IntoParams, deny_unknown_fields
       pub limit: Limit, pub cursor: Option<String>,
       /// Only queues holding a visible trace or span item of this trace (32 hex digits).
       pub target_trace_id: Option<String>,
       /// Only queues holding a visible item of this session.
       pub target_session_id: Option<String>,
   }

   pub struct AnnotationQueueItem {
       pub id: Uuid, pub queue_id: Uuid,
       pub trace_id: Option<String>, pub span_id: Option<String>, pub session_id: Option<String>,   // omitted when None
       pub target_time: i64,                       // RFC 3339
       pub status: QueueItemStatus,
       pub skip_reason: Option<String>,
       /// Set only while the lease is live.
       pub leased_by_user_id: Option<Uuid>, pub lease_expires_at: Option<i64>,
       pub done_by_user_id: Option<Uuid>, pub done_at: Option<i64>,
       pub added_by: QueueItemAddedBy, pub added_by_id: Uuid,
       pub created_at: i64,
   }
   pub struct AnnotationQueueItemListParams { pub limit: Limit, pub cursor: Option<String>, pub status: Option<QueueItemStatus> }

   pub struct QueueItemTarget { pub trace_id: Option<String>, pub span_id: Option<String>, pub session_id: Option<String> }
   pub struct AddQueueItems { pub items: Vec<QueueItemTarget> }
   pub struct SkippedQueueItem { pub index: u64, pub message: String }
   pub struct AddedQueueItems { pub added: u64, pub existing: u64, pub skipped: Vec<SkippedQueueItem> }
   ```
   Pages use `api::common::Page<AnnotationQueue>` and `Page<AnnotationQueueItem>`.
4. **Routes** in `routes/annotation_queues.rs` (`pub(crate) fn routes() -> Vec<RouteDef>`, mirroring `routes/dashboards.rs`).
   - All routes are `documented: true` and `session_only: false`.
   - Path parameters are named `{project}`, `{id}` and `{item_id}`.
   - `P = /api/v1/projects/{project}/annotation-queues`.

   | Method & path | Perm | Handler | Body limit |
   |---|---|---|---|
   | `GET P` | `View` | `list_queues` | |
   | `POST P` | `ManageQueues` | `create_queue` | 64 KiB |
   | `GET P/{id}` | `View` | `get_queue` | |
   | `PATCH P/{id}` | `ManageQueues` | `update_queue` | 64 KiB |
   | `DELETE P/{id}` | `ManageQueues` | `delete_queue` | |
   | `GET P/{id}/items` | `View` | `list_items` | |
   | `POST P/{id}/items` | `ManageQueues` | `add_items` | 1 MiB |
   | `GET P/{id}/items/{item_id}` | `View` | `get_item` | |
   | `DELETE P/{id}/items/{item_id}` | `ManageQueues` | `remove_item` | |

   Q4a adds the five queue rows; Q4b adds the four item rows. Register `routes()` in `http/access.rs` `routes()` after the dashboards line, and add `pub(crate) mod annotation_queues;` to `routes/mod.rs`.
5. **Validation** (create and update). Collect `FieldError`s and refuse with `400 bad_request`, as dashboards do:
   - `name`: trimmed, 1–100 characters (`chars().count()`).
   - `description`: at most 1 000 characters.
   - `instructions`: at most 10 000 characters.
   - `definition_ids`: 1–`MAX_DEFINITIONS` ids, all distinct (`definition_ids` "duplicate definition").
   - A stored empty description or instructions is `None` (on create too).
   - Unknown or archived definitions: field `definition_ids[{i}]`, message `"unknown or archived score definition"`.
6. **`create_queue`.** Steps, in order:
   1. Read `now`, `limit = crate::entitlements::of(&state, access.project.org_id).limits.max_annotation_queues` and the creator (`principal::session_user(&principal).ok().map(|u| u.id)`: a key creates with `created_by` none).
   2. In `state.meta.write`, call `insert_queue_tx`.
   3. Map the outcome: `OverPlanLimit(l)` → `crate::entitlements::plan_limit(l, ("annotation queue", "annotation queues"))`; `UnknownDefinitions` → 400; `Created` → `201` with counts zero.
   4. Log `tracing::info!(project_id, queue_id, "annotation queue created")`.
7. **`list_queues`.**
   - Decode the cursor with a private `QueueCursor { name: String, id: Uuid }` type, base64 JSON as `DashboardCursor`.
   - Validate `target_trace_id` by parsing it as `cetra_core::TraceId` (32 hex digits; store `to_hex()`) and `target_session_id` (1–255 bytes). Both given at once → 400 on `target_session_id`, message `"give one target"`.
   - `visible_from = Visibility::window(&state, access.project.org_id).visible_from()`.
   - Call `list`, then `counts` for the page's ids.
8. **`get_queue`**: 404 when absent. Counts use the window.
9. **`update_queue`:**
   - Validate as on create. Empty `""` maps to `Some(None)` for description and instructions.
   - `Stale(current)` → `ApiError::StaleVersion { what: "annotation queue", current }`.
   - `Missing` → 404. Otherwise `200`.
10. **`delete_queue`:** `204`, or 404. Scores are untouched.
11. **OpenAPI** (`api/openapi.rs`):
    - add the schemas to `collect_schemas`, with page names `AnnotationQueuePage` and `AnnotationQueueItemPage`;
    - add `tag("annotation_queues", "Annotation queues: lists of traces, spans or sessions that reviewers label one at a time.")`;
    - add `annotation_queue_paths`, modelled on `dashboard_paths`, with these operation ids: `list_annotation_queues`, `create_annotation_queue`, `get_annotation_queue`, `update_annotation_queue`, `delete_annotation_queue` (Q4b adds the item operations to the same function);
    - each description states who may call it, in the style of the dashboards descriptions;
    - responses: create `201` with `400, 401, 403, 404, 409, 413, 415, 429`; update `200` with `400, 401, 403, 404, 409, 413, 415, 429`;
    - run `just openapi`.
12. **Fixtures.**
    - Add static examples with fixed ids from a free range: queue `…0a01`. Use the format of `fixtures/api/README.md` § Fixed identifiers, and the README's score-definition ids for `definition_ids`. The files: `static/annotation-queues.json` (a page), `static/annotation-queue.json`, and the request `requests/annotation-queue-create.json`.
    - Manifest rows in `api_examples.rs`: `example("list_annotation_queues", …)`, `example("get_annotation_queue", …)` and `example_created("create_annotation_queue", …)` (the helpers are in that file).
    - In `tests/api_examples.rs`: roundtrip arms for the new operation ids in `generated_files_roundtrip` and the static roundtrips, a `("create_annotation_queue", "requests/annotation-queue-create.json")` row in the request list (around line 520) with its deserialize arm, and the `use` imports. The request types need `Serialize` as well as `Deserialize` for these roundtrips.
    - Run `just api-examples`, and add the ids to the README table.
13. **Specs.**
    - `docs/spec/08-auth.md`: in the permission table, add "Create and edit annotation queues, add and remove items" next to the annotate row (member and above; `sk` write key). Add one line after the annotations paragraph: "Annotation queues: `ManageQueues` (member, a `write` key, a read-write PAT) for queues and items; `Review` (member, sessions and read-write PATs only) for next, lease, complete, skip and reopen (D-AQ-11)."
    - `docs/spec/09-api.md`: add `### Annotation queues` after § Scores & annotations, with this phase's routes (Q4b and Q5 add theirs). Add `too_many_items` (422) to the error-code list.
    - `overview.mdx`: add the `too_many_items` row to the error table.

**Tests** (in `routes/annotation_queues.rs`, with `TestApp`, `seed`, `add_member`, `personal_token`, `write_key`, `set_limits` and `meta::score_definitions::insert_definition` to make definitions):
- `create_list_get_update_delete`: the full CRUD through the API with the owner cookie; `version` bumps; the list holds zero counts.
- `create_validates_fields`: an empty name, a 101-character name, 51 definitions, a duplicate definition, and an archived or unknown definition each give `400` on the right field.
- `duplicate_name_is_409`.
- `stale_version_is_409_with_the_queue_wording`: the detail says "the annotation queue changed; current version is 2".
- `free_plan_allows_one_queue`: after `set_limits(&app, org, …, &json!({"max_annotation_queues": 1}))`, the second create answers `403 plan_limit`.
- `queue_permissions`: a viewer can `GET` but gets `403` on `POST`; an `sk` write key creates with `created_by` absent; a read key cannot create; a read-only PAT cannot create; a read-write PAT can.
- The access suite (`http/access_suite.rs`), the OpenAPI tests and the api-examples tests pass unchanged; `auth::roles` covers both new perms.

**Done when.**
- `cargo fmt --all --check`
- `cargo test -p cetra-server --lib routes::annotation_queues`
- `cargo test -p cetra-server --lib auth::roles http::access`
- `cargo test -p cetra-server --test openapi --test api_examples`
- `just lint`
- `just check-web` passes too (the web mocks read the manifest); if a web handler list needs an entry for the new operations, report it instead of editing `web/`.
- `export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH && just check` (once, before the commit)

**Commit:** `Add annotation queue routes`

---

### Q4b — Item routes

**Goal.** The item list, one item by id, adds by id and item removal, with OpenAPI, fixtures and spec.

**Edit:**
- `crates/server/src/api/annotation_queues.rs`
- `crates/server/src/routes/annotation_queues.rs`
- `crates/server/src/api/openapi.rs`
- `crates/server/src/api_examples.rs` and `crates/server/tests/api_examples.rs`
- `fixtures/api/static/annotation-queue-items.json`, `fixtures/api/static/annotation-queue-item.json`, `fixtures/api/static/annotation-queue-items-added.json` and `fixtures/api/requests/annotation-queue-items-add.json` (new), `fixtures/api/manifest.json` (generated) and `fixtures/api/README.md`
- `crates/server/openapi.json` and `web/src/api/schema.gen.ts` (generated)
- `docs/spec/09-api.md`

**Steps.** The four item rows of Q4a's step 4 table, and their handlers:
1. **`list_items`:** 404 when the queue is absent. The cursor is the base64 JSON `{id}`; `limit` is 1–1 000, default 50. The window applies.
   - `leased_by_user_id` and `lease_expires_at` are emitted only when `lease_expires_at > now`.
    - `added_by_id` is the user, PAT or key id.
2. **`add_items`.** Steps, in order:
    1. 404 when the queue is absent.
    2. `items` must hold 1–1 000 entries (400 on `items`).
    3. Shape per kind:
       - trace queue: `trace_id` set, `span_id` and `session_id` absent;
       - span queue: `trace_id` and `span_id` set, `session_id` absent;
       - session queue: `session_id` set, the others absent.

       A wrong shape is **skipped** with the message `"a {kind} queue takes {fields}"`, for example `"a span queue takes trace_id and span_id"`.
    4. A `trace_id` or `span_id` that is not 32 or 16 hex digits is a 400 field error, `items[{i}].trace_id` / `items[{i}].span_id`, `"invalid trace id"` / `"invalid span id"`. A `session_id` outside 1–255 bytes is a 400 on `items[{i}].session_id`.
    5. Open the query context with `routes::read::open_query(&state, org, project, None)` (data window).
    6. Look the well-formed targets up with Q1's `trace_targets`, `span_targets` or `session_targets`, with the read timeout the read routes use. A target not found is skipped with `"the trace was not found"`, `"the span was not found"` or `"the session was not found"`.
    7. In `state.meta.write`, with a clone of `state` moved in:
       - for each found target, check `state.deletions.guard.blocking(project, trace, &[], now)`: on the trace id for trace and span items, and on every id of `SessionTarget::trace_ids` for session items;
       - a blocked target is skipped with `"a deletion blocks this target"`;
       - call `insert_items_tx` with the rest, with `added_by` from the principal: `Principal::Session { user, .. }` → (`AddedByKind::User`, `user.id`); `Principal::Pat { token_id, .. }` → (`AddedByKind::Pat`, `token_id`); `Principal::ApiKey { key_id, .. }` → (`AddedByKind::Key`, `key_id`); `Principal::ControlPlane { .. }` → `ApiError::NotFound` (it never reaches project routes).
    8. `TooMany` → `ApiError::TooManyItems`. `Done` → `201 AddedQueueItems`, with `skipped` sorted by index.
    9. Log `tracing::info!(project_id, queue_id, added, existing, skipped = skipped.len(), "annotation queue items added")`.
3. **`remove_item`:** `204`, or 404.
   **`get_item`:** 404 when the queue or the item is absent, or the item is hidden by the data window (`get_item_on` through a reader: add `pub(crate) async fn get_item(pool: &SqlitePool, queue: Uuid, item: Uuid, visible_from: Option<i64>) -> Result<Option<ItemRow>, MetaError>` to `meta/annotation_queues.rs`, and add that file to this phase's Edit list). The lease fields are emitted as in `list_items`.
4. **OpenAPI:** add `list_annotation_queue_items`, `get_annotation_queue_item`, `add_annotation_queue_items` and `remove_annotation_queue_item` to `annotation_queue_paths`. Add gets `201` with `400, 401, 403, 404, 413, 415, 422, 429, 503`. Run `just openapi`.
5. **Fixtures:**
   - items `…0a11` and `…0a12` in `static/annotation-queue-items.json` (a page), `static/annotation-queue-item.json` (item `…0a11`, pending, with a live lease by Ada `…0100`), `static/annotation-queue-items-added.json` and `requests/annotation-queue-items-add.json`;
   - manifest rows `example("list_annotation_queue_items", …)`, `example("get_annotation_queue_item", …)` (`manifest_covers_the_read_operations` requires every GET) and `example_created("add_annotation_queue_items", …)`;
   - the roundtrip arms, the request row and the imports in `tests/api_examples.rs`;
   - `just api-examples`, and the README ids.
6. **Spec:** add the four rows to `docs/spec/09-api.md` § Annotation queues.

**Tests** (in `routes/annotation_queues.rs`, with Q4a's helpers plus `commit_trace_at` and `commit_spans`):
- `add_items_by_id`, in a trace queue:
  - two committed traces and an unknown one are added;
  - `added = 2`, and the unknown trace is skipped with its index;
  - a second identical add gives `added = 0, existing = 2`.
- `add_items_checks_the_shape`: a span target in a trace queue is skipped with the kind message; a bad hex id is a 400.
- `add_span_and_session_items`: a span queue and a session queue, with committed spans that share a `session_id`. `target_time` is the trace start, and the session's earliest trace start.
- `add_items_skips_evaluation_traces`.
- `add_items_respects_the_guard`: after `app.state.deletions.guard.add_traces(project, &[t], Uuid::now_v7(), now)`, `t` is skipped with `"a deletion blocks this target"`. A session containing `t` is skipped too.
- `too_many_items_is_422`. The cap cannot be lowered from a route test, so test the mapping at the store level in Q2, and here check that the route maps `ItemsInsert::TooMany` to 422 through a small unit test of the mapping function. If the mapping is inline, extract `fn add_outcome(…) -> Result<…, ApiError>` to test it.
- `items_list_pages_and_filters_by_status`.
- `get_item_by_id`: answers the item; an unknown id, an item of another queue and a hidden item are 404.
- `list_by_target_trace`: `?target_trace_id=` returns only the queues holding that trace.
- `item_permissions`: a viewer lists items but gets `403` on add; an `sk` write key adds with `added_by` `key`; a read-write PAT adds with `added_by` `pat`; the owner's cookie adds with `added_by` `user`.
- `items_hidden_by_the_window`: with `set_limits(… {"data_access_days": 30})` and a trace older than 30 days added through the store, the item is absent from the list and the counts.
- The access suite, the OpenAPI tests and the api-examples tests pass.

**Done when.**
- `cargo fmt --all --check`
- `cargo test -p cetra-server --lib routes::annotation_queues`
- `cargo test -p cetra-server --test openapi --test api_examples`
- `just lint`
- `export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH && just check` (once, before the commit)

**Commit:** `Add annotation queue item routes`

---

### Q5 — Reviewer routes

**Goal.** `next`, lease renew and release, complete, skip and reopen, documented and tested.

**Edit:**
- `crates/server/src/api/annotation_queues.rs`
- `crates/server/src/routes/annotation_queues.rs`
- `crates/server/src/api/openapi.rs`
- `crates/server/src/api_examples.rs` and `crates/server/tests/api_examples.rs`
- `fixtures/api/requests/annotation-queue-next.json` and `fixtures/api/requests/annotation-queue-skip.json` (new), and `fixtures/api/manifest.json`
- `docs/spec/09-api.md`
- `crates/server/openapi.json` and `web/src/api/schema.gen.ts` (generated)

**Steps.**
1. Types: `pub struct NextQueueItem { pub after: Option<Uuid> }` and `pub struct SkipQueueItem { pub reason: Option<String> }`. Both derive `Serialize` and `Deserialize`, have `deny_unknown_fields`, and accept `{}`. **Bodies:** `next` and `skip` take a required JSON body through the usual `error::Json<…>` extractor, so clients send `{}` at least (an empty body is the extractor's usual 400/415). `lease`, `complete` and `reopen` declare no body extractor at all, and any body is ignored. The OpenAPI documents a required body for `next` and `skip`, and none for the others.
2. Routes. All have perm `Review` and a body limit of 16 KiB:

   | Method & path | Handler | Success |
   |---|---|---|
   | `POST P/{id}/next` | `next_item` | `200 AnnotationQueueItem`, or `204` with no body when nothing is free |
   | `POST P/{id}/items/{item_id}/lease` | `lease_item` | `200 AnnotationQueueItem` |
   | `DELETE P/{id}/items/{item_id}/lease` | `release_item` | `204` |
   | `POST P/{id}/items/{item_id}/complete` | `complete_item` | `200 AnnotationQueueItem` |
   | `POST P/{id}/items/{item_id}/skip` | `skip_item` | `200 AnnotationQueueItem` |
   | `POST P/{id}/items/{item_id}/reopen` | `reopen_item` | `200 AnnotationQueueItem` |

3. Every handler follows these steps:
   1. The user is `principal::session_user(&principal)?.id`. Keys never reach it, because `Review` has no key scope.
   2. 404 when the queue is absent from the project.
   3. `visible_from` comes from the window; `now` from `state.clock`.
   4. Call the Q2 store function in `state.meta.write`.
   5. Map the outcome:
      - `Done(row)` → `200` with the item;
      - `Missing` → 404;
      - `NotPending` → `ApiError::Conflict { detail: "the item is not pending" }`;
      - `Pending` (reopen) → `ApiError::Conflict { detail: "the item is pending" }`;
      - `HeldBy(u)` → `ApiError::ConflictWith { detail: "another reviewer holds this item", errors: vec![FieldError { field: "leased_by_user_id".into(), message: u.to_string(), position: None }] }`.
4. `skip_item`: a `reason` is trimmed, empty becomes `None`, and more than 1 000 characters is a 400 on `reason`.
5. `next_item`: `after`, when given, need not exist (it is only a bound). `release_item` answers `204` even for an unknown item or one the caller does not hold.
6. Log ids only: `"annotation queue item completed"`, `"… skipped"` and `"… reopened"`. Do not log lease claims and renewals (too chatty).
7. OpenAPI operation ids: `next_annotation_queue_item`, `lease_annotation_queue_item`, `release_annotation_queue_item`, `complete_annotation_queue_item`, `skip_annotation_queue_item`, `reopen_annotation_queue_item`.
   - Each description says: "A session or a read-write personal access token with role member or above; API keys cannot call it."
   - The 409 responses are declared.
   - Run `just openapi`.
8. Fixtures: the request bodies `requests/annotation-queue-next.json` (`{}`) and `requests/annotation-queue-skip.json` (`{"reason": "off topic"}`). Add an `example("next_annotation_queue_item", …)` manifest row for the `200` answer (file `static/annotation-queue-item.json`, from Q4b), the two request rows with their deserialize arms and imports in `tests/api_examples.rs`, and the roundtrip arm. Run `just api-examples`. Add the six rows to `docs/spec/09-api.md` § Annotation queues (add `docs/spec/09-api.md` to this phase's Edit list).

**Tests** (in `routes/annotation_queues.rs`):
- `two_reviewers_never_get_the_same_item`:
  - Ada and member Bob call `next` on a queue of three items. They get different items.
  - Ada's second `next` with `after` moves on and releases her first item.
  - When all three are leased by others, `next` answers `204`.
- `lease_expires_after_five_minutes`: Bob's item becomes claimable by Ada after `app.state.clock.advance(Duration::from_secs(301))`.
- `renew_and_release`: the holder renews (`200`, `lease_expires_at` moves); another user gets `409` with `errors[0].field == "leased_by_user_id"` and the holder id as message; after release, the other user's lease succeeds.
- `complete_skip_reopen`:
  - complete by the holder → `completed`, with `done_by_user_id` set and no lease;
  - completing again → `409 "the item is not pending"`;
  - reopen → `pending`;
  - skip with reason `"  off topic "` → `skipped`, reason `"off topic"`;
  - reopen of a pending item → `409`.
- `complete_without_lease_is_allowed_when_free`, and refused when another user holds a live lease: the problem body is `409 conflict`, its `errors` array holds one entry with `field` `leased_by_user_id` and the holder's id as `message`.
- `bodies`: `next` with `{}` works; `next` with `{"unknown": 1}` is `400`; `complete` with no body works.
- `keys_cannot_review`: an `sk` write key gets `403` on `next`. A read-only PAT gets `403`; a read-write PAT works and its user is the lease holder.
- `viewer_cannot_review`: `403`.
- `hidden_items_are_404`: an item outside the data window answers 404 on complete and is never returned by `next`.

**Done when.**
- `cargo fmt --all --check`
- `cargo test -p cetra-server --lib routes::annotation_queues`
- `cargo test -p cetra-server --test openapi --test api_examples`
- `just lint`
- `export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH && just check` (once, before the commit)

**Commit:** `Add annotation queue reviewer routes`

---

### Q6 — Score check, deletion cascade and retention

**Goal.** `queue_id` on a score must name a queue of the project; GDPR deletions and retention delete items; docs and changelog are complete.

**Edit:**
- `crates/server/src/scores.rs`
- `crates/server/src/deletion/resolve.rs`
- `crates/server/src/retention.rs`
- `CHANGELOG.md`
- `docs/spec/06-evaluation.md` (one sentence)

**Steps.**
1. **Score check.** In `scores::write`, right after the `experiment_id` existence block (around lines 190–209), add the same block for `queue_id`. It calls `crate::meta::annotation_queues::existing_ids(state.meta.reader(), project.id, &ids)` and pushes `field(index, "queue_id", "unknown annotation queue")` for each unknown id. The check runs for every writer.
2. **Cascade.** In `deletion/resolve.rs`:
   1. The resolve context `ctx` is built with `Visibility::exempt()` (keep it so: old sessions must be found whatever the data window).
   2. After `scope` is built and **before** it is moved into the meta transaction closure, compute the sessions:
      ```rust
      let queue_sessions: Vec<String> = if crate::meta::annotation_queues::has_session_items(state.meta.reader(), project)
          .await
          .map_err(|e| super::from_meta(&e))?
      {
          let traces: HashSet<TraceId> = scope.iter().copied().collect();
          let mut found = cetra_storage::session_ids_of_traces(&ctx, &traces, RESOLVE_TIMEOUT)
              .await
              .map_err(|error| super::from_run(&error, row.kind))?;
          found.extend(sessions.iter().cloned());
          found.sort();
          found.dedup();
          found
      } else {
          Vec::new()
      };
      ```
      (Use the exact error-mapping helpers the other reads of this function use, if their names differ.)
   3. Change the closure's result from `(bool, Cascade)` to `(bool, Cascade, u64)`. Inside `if stored { … }`, next to `delete_jobs` and `delete_copies_of_tx`, call `let queue_items = crate::meta::annotation_queues::delete_items_of_deletion_tx(conn, id, project, &queue_sessions).await?;` and return `(true, cascade, queue_items)`; the other branch returns `(false, Cascade::default(), 0)`.
   4. In the existing `audit` log event that reports the cascade, add the field `queue_items` and log the event when `queue_items > 0` too (the condition becomes `cascade.dataset_item_revisions > 0 || cascade.experiment_items > 0 || queue_items > 0`).
   5. `deletion_sessions` is not written for non-user kinds; nothing else in `deletion/` changes.
3. **Retention.** In `retention.rs`, in both `retention_once` (the loop, after `keep` is computed) and `apply_now`:
   1. Add one private helper used by both: `async fn drop_queue_items(state: &AppState, project: ProjectId, keep: NaiveDate) -> Result<u64, MetaError>`. It computes `cutoff = keep.and_hms_opt(0, 0, 0).and_then(|t| t.and_utc().timestamp_nanos_opt())` (return `Ok(0)` when `None`; a real day never is) and runs `state.meta.write(move |conn| Box::pin(async move { annotation_queues::delete_items_before(conn, project, cutoff).await }))`.
   2. In `retention_once`, call it only after the `let Some(days)` guard (projects kept forever are skipped, as today), right after `keep` is computed, whatever the storage result. In `apply_now`, call it after `keep_from`.
   3. In both, an error is logged with `tracing::warn!(project_id = %project, "annotation queue retention failed")`; in `retention_once` it also adds 1 to `run.failures`.
4. In `docs/spec/06-evaluation.md`, extend the sentence M1 added: "From M2, a `queue_id` that names no queue of the project is refused (`400`, field `scores[i].queue_id`)."
5. `CHANGELOG.md` § `[Unreleased]` § Added: "Annotation queues on the server: queues of traces, spans or sessions with their score definitions and instructions, items added by id, a 5-minute lease with next, complete, skip and reopen, the GDPR and retention cascade, and the `max_annotation_queues` plan limit (Free: 1)."

**Tests:**
- `scores.rs`: `unknown_queue_is_refused` (mirror `unknown_experiment_is_refused`): a `queue_id` that is random or belongs to another project's queue gives `400` on `scores[0].queue_id`. A real queue of the project is accepted.
- `deletion/mod.rs` tests (mirror the J cascade tests): `trace_deletion_removes_queue_items`.
  - Set up a trace item, a span item of the same trace, a session item whose session holds that trace, and an unrelated item.
  - Run a `trace_ids` deletion through the worker (`run_once`).
  - The first three are gone and the unrelated one stays.
- `user_deletion_removes_session_items`: a session item of the user's session is gone after a `user` deletion.
- `cascade_skips_the_scan_without_session_items`: with no session items, the deletion still applies. This checks that the `has_session_items` path compiles and runs.
- `retention.rs`: `hourly_run_drops_old_queue_items`. Mirror `hourly_run_drops_old_days`: an item whose `target_time` is before `keep_from` is deleted and a newer one stays. Also test `apply_now` through the project `PATCH`, if the existing tests do it that way; otherwise call `apply_now` directly.

**Done when.**
- `cargo fmt --all --check`
- `cargo test -p cetra-server --lib scores deletion retention`
- `just lint`
- `export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH && just check` (once, before the commit)

**Commit:** `Check score queue ids and cascade queue items`
