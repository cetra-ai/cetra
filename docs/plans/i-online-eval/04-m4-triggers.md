# I · M4 — Triggers: trace completion, sampling and the live enqueue

Implementation plan for milestone M4 of track I. It has 8 sub-phases, I4.1 to I4.8. **Haiku** implements each one in one go, with no context beyond this plan and the files it names. Sub-phases that do not depend on each other run in parallel worktrees (§ Parallel waves). Each sub-phase ends with `just check` green and one commit. Sonnet reviews the code after every three or four sub-phases.

**Prerequisites** (check them before you start):
- M3 is implemented: `crates/server/src/eval/dispatch.rs`, `eval/hourly.rs`, `meta/online_rules.rs`, `meta/eval_jobs.rs` and `routes/online_rules.rs` exist. Start from `main` once M3 has merged, otherwise from M3's integration branch with `main` merged in.
- G's fix F4 is on `main`: `crates/server/src/ingest/mod.rs` has `commit_in_task` and `wait_for_commits`. It had landed when this plan was written.
- This plan lives on branch `claude/i-m3-impl` (M3's integration branch). Check with `git log --oneline -1 -- docs/plans/i-online-eval/04-m4-triggers.md` that it is there.
- Node `26.13.0` is installed at `$HOME/.local/share/nvm/v26.13.0/bin`. It is not on the non-interactive `PATH`.

**Read first** (every sub-phase):
- `AGENTS.md`, all of it.
- This file's sections up to § I4.1, then your own sub-phase.
- `docs/plans/i-online-eval/00-design.md` § Decisions: D-I-7 to D-I-11, D-I-17, D-I-28, D-I-29, D-I-34; § Trigger (M4).
- `docs/plans/i-online-eval/03-m3-queue-rules.md` § Conventions for every sub-phase and § Shared reference. M3's patterns (loops, `MetaDb::write` closures, routes, fixtures, pages) are the model for M4.
- `docs/spec/02-ingestion.md` § Pipeline; `docs/spec/06-evaluation.md` § Online rules › Triggering; `docs/spec/04-metadata-db.md` § Evaluation.
- The code each sub-phase points to, before you edit it.

## What M4 is, and what it is not

When M4 is done:
- `Ingest::commit` records, in memory only, every trace of a project that has an enabled, unpaused online rule (`PendingTraces`, D-I-8). Evaluation and experiment spans are never recorded.
- a **flusher** writes the changed entries to the new `pending_traces` table every 5 s and at shutdown. Startup loads the table back.
- a **sweeper** runs every 2 s. It reads the enabled, unpaused rules, takes the completed traces of each project in batches, matches each rule through storage with strict exclusions (`trace_refresh` or `span_refresh`), samples by SHA-256, applies the hourly cap, and enqueues **rule jobs** (D-I-9 to D-I-11). The M3 dispatcher then runs them. This is the first production source of rule jobs.
- projects have a `trace_completion_timeout_s` setting (10–3600 s, default 60) in the API and on the project's general settings page.
- `POST …/online-rules/preview` counts what a filter matches over a window (the last 24 h by default, at most 10 000, strict exclusions), and the rule editor shows that count as you type.

**Out of scope:** the annotation panel (M5); backfill (M6); the full `llmobs serve` end-to-end test (M6; M4 has an in-process one); plan enforcement (track P).

## Decisions this plan relies on

| # | Summary |
|---|---|
| D-I-7 | Trace and span rules fire when the trace completes: its root span has ended and 5 s passed without a new span, or `trace_completion_timeout_s` (default 60, 10–3600) passed without one. Traces that complete while no rule tracks the project are not evaluated. |
| D-I-8 | `PendingTraces` lives in memory, updated by `commit`, with no database access on the ingest path. It holds at most 200 000 traces; past that new traces are dropped, counted, and a `warn` is logged at most once a minute. A flusher persists the changes every 5 s (at most 1 000 rows per transaction, deletes first) and at shutdown; startup loads them. |
| D-I-9 | Every 2 s the sweeper takes completed traces per project, in batches of at most 500, at most 2 000 per project per tick. A batch spans at most 12 h of `min_start`, and its reads use `around` = the middle of that range. One read per rule per batch, with strict exclusions. A failed read keeps the batch for the next tick; after 5 failed ticks a trace is dropped with a `warn`. A trace leaves the map only when its `last_seen` did not change during the reads. Ticks never overlap. |
| D-I-10 | Sampling: SHA-256(`llmobs.sample.v1` ‖ rule id ‖ trace id ‖ span id for span rules), first 8 bytes big-endian, below `(rate · 2^64) as u64`; `rate >= 1` always samples. A span rule samples first, then keeps the 50 earliest sampled spans of each trace. |
| D-I-11 | Rule jobs: `dedup_key` `rule:<rule>:<trace>:<span or ->`, `ON CONFLICT DO NOTHING`. Live enqueues are capped at 10 000 per rule per UTC hour; extra matches are dropped and counted (`dropped_last_hour`). |
| D-I-17 | `commit` never tracks evaluation or experiment spans; every matching read uses strict exclusions. |
| D-I-28 | Projects or orgs that are not `active` are not tracked; their pending traces are dropped. |
| D-I-29 | Timing constants live in `EvalTiming` (`DEFAULT`, `TEST`). |
| D-I-34 | Every read goes through `Storage::query` and A2's reads. |

**This plan changes four points of the design** (`00-design.md` and `docs/mvp-decisions.md` already carry them; I4.8 checks they are still there):
1. **No rule or settings cache, no tracked-set rebuild on writes.** The sweeper reads the enabled, unpaused rules and their projects' timeouts with one reader query at the start of every tick, and rebuilds the tracked set from it. A rule created, resumed or paused is picked up within one tick (2 s). D-I-8 said "rebuilt after every rule create, update, delete, pause and resume", and D-I-9 said "rules and settings from in-memory caches; rule writes refresh the rules cache; settings every 60 s". The per-tick read removes every invalidation hook from M3's code paths (routes, `finish`, the breaker, the budget ticker, connection tests). The cost is one indexed query on a reader connection every 2 s.
2. **A rule whose filter no longer parses is skipped** for the batch (with a `warn` naming the rule id), instead of failing the batch for every rule of the project.
3. **Rule enqueues check the rule.** `insert_rule_job` inserts only while the rule exists and is enabled, so a rule deleted between the tick's read and the write does not fail the write (a foreign-key error would have failed the whole batch five times and dropped its traces).
4. **`POST /online-rules/preview`** answers `{matched, capped, from, to}` and uses a new storage read, `match_count`, instead of paging `trace_list`/`span_list`.

The project setting is stored in `projects.settings` (JSON, key `trace_completion_timeout_s`), as spec 04 already says. `ProjectRow`, `Project` and `UpdateProjectRequest` gain the field (I4.3).

## Versions

No new dependency. `sha2`, `chrono`, `serde_json` and `tokio` are already dependencies of `llmobs-server`.

## Conventions for every sub-phase

M3's conventions apply unchanged (`03-m3-queue-rules.md` § Conventions for every sub-phase, which includes M1's and M2's): the Node `PATH`, the `just` recipes, contract regeneration (`just openapi`, `just api-examples`), dead code across sub-phases, logs of a check run under your worktree's own `target/`, `just check-rust` as one background task, the clippy denies, `tests/openapi.rs` pinning every operation, the test-clock rule for evaluator runs, log-capture tests on plain `#[tokio::test]`, background loops on K's pattern, no `std::sync::Mutex` guard across an `.await`, short `MetaDb::write` closures that own their values, no `unwrap`/`expect` outside tests, privacy in logs, one commit per sub-phase with no trailer lines, and the report.

In addition:
- **`PendingTraces` is touched on every commit.** Keep its lock short: no allocation-heavy work, no logging and no `.await` while holding it. Decide what to log under the lock, drop the guard, then log.
- **A poisoned lock** is recovered with `.lock().unwrap_or_else(PoisonError::into_inner)`, as `ingest/mod.rs` does for its price table.
- **Times.** `first_seen` and `last_seen` are ingest times (`CommitTarget.now_ns`, which is `state.clock`), never span times. `min_start` is the earliest span `start_time` (client time). Completion compares `now` with `last_seen` only.
- **Logs never carry filter text, span names, attribute values or trace content.** Rule ids, project ids, trace ids and counts are fine.
- **Report structs are logged field by field.** A field read only by a derived `Debug` or `PartialEq`, or by tests, is unread for rustc's dead-code lint. Log `FlushReport` and `SweepReport` with one named field each (`tracing::debug!(upserts = report.upserts, deletes = report.deletes, …)`), never `?report`.
- **Dead-code allows in M4.** The table lists every item-level `#[allow(dead_code)]` M4 adds, who adds it and who removes it. Use the comment given. After I4.7, `rg -n "I4\.[0-9] (calls|wires|reads) it" crates/server` must be empty.

  | Item | Added by | Comment | Removed by |
  |---|---|---|---|
  | `meta::pending_traces::{PendingRow, load_pending, write_pending}` | I4.2 | `// I4.5 calls it.` | I4.5 |
  | `meta::online_rules::trigger_rules` | I4.2 | `// I4.5 calls it.` | I4.5 |
  | `meta::online_rules::TriggerRule` (the struct: its `id`, `evaluator_id`, `target`, `filter` and `sample_rate` are first read by the sweeper) | I4.2 | `// I4.7 reads it.` | I4.7 |
  | `meta::eval_jobs::insert_rule_job` | I4.2 | `// I4.7 calls it.` | I4.7 |
  | `eval::pending::{MAX_PENDING, ROOT_IDLE_NS, Entry, Changes, PendingTraces}` and the methods `new`, `observe`, `set_tracked`, `take_changes`, `restore`, `load`, `len` | I4.4 | `// I4.5 wires it.` | I4.5 |
  | `eval::pending::{MAX_FAILED_READS, Completed}` and the methods `completed`, `finish`, `read_failed` | I4.4 | `// I4.7 calls it.` | I4.7 |
  | `eval::sampling::sampled` | I4.4 | `// I4.7 calls it.` | I4.7 |
  | `eval::trigger::ProjectRules` (its `timeout_ns` and `rules` are first read by the sweeper) | I4.5 | `// I4.7 reads it.` | I4.7 |
  | `eval::hourly::HourlyCaps::admit` | I3.3 | `// M4's sweeper calls it (D-I-11).` | I4.7 |

  An allow on a struct covers its fields. If `just check` reports an unused item that M4 adds and this table does not name, STOP and report it. Do not add an allow of your own.

## Parallel waves

Each wave starts when the waves before it are merged into the M4 integration branch. The sub-phases in one wave touch different files; the table names the shared ones (the second to merge resolves a trivial conflict there).

| Wave | Sub-phases | Depends on | Shared files to merge by hand |
|---|---|---|---|
| 1 | I4.1 storage `match_count` · I4.2 `pending_traces` table, trigger rule read, guarded rule enqueue · I4.3 project timeout setting (server and contract) · I4.4 sampling and `PendingTraces` (memory only) | M3 | `crates/server/src/eval/mod.rs` (I4.4 adds module lines only); `crates/server/openapi.json`, `web/src/api/schema.gen.ts` (I4.3 only; I4.1 changes no operation) |
| 2 | I4.5 wiring: `commit` argument, `AppInner.pending`, flusher, startup load · I4.6 preview API | I4.2, I4.3, I4.4 (I4.5: I4.3 supplies the `COMPLETION_TIMEOUT_*` constants); I4.1 (I4.6) | none |
| 3 | I4.7 the sweeper and the end-to-end test · I4.8 web: match counter, timeout field, specs and roadmap | I4.5 (I4.7); I4.3, I4.6 (I4.8) | none |

The Sonnet code reviews fit the waves: after wave 1 (I4.1–I4.4), and after waves 2–3 (I4.5–I4.8).

Contract files (`openapi.json`, `schema.gen.ts`, `fixtures/api/manifest.json`) are regenerated by I4.3 and I4.6. After merging a wave, run `just openapi` and `just api-examples` once and commit any diff as `Regenerate the API contract`.

## Final layout after M4

```
crates/storage/src/reads/traces.rs          match_count, MatchCountRequest, MAX_MATCH_COUNT (I4.1)
crates/storage/src/reads/mod.rs, lib.rs     re-exports (I4.1)
crates/server/migrations/meta/00MM_pending_traces.sql   (I4.2)
crates/server/src/
  meta/pending_traces.rs                    PendingRow, load_pending, write_pending (I4.2)
  meta/online_rules.rs                      TriggerRule, trigger_rules (I4.2)
  meta/eval_jobs.rs                         insert_rule_job (I4.2)
  meta/projects.rs                          trace_completion_timeout_s (I4.3)
  api/account.rs                            Project / UpdateProjectRequest field (I4.3)
  routes/projects.rs                        PATCH validates and stores it (I4.3)
  eval/sampling.rs                          sampled (I4.4)
  eval/pending.rs                           PendingTraces (I4.4)
  eval/trigger.rs                           refresh_rules, load, flush_once (I4.5); sweep_once (I4.7)
  eval/mod.rs                               EvalTiming.flush_every, flusher loop, final flush (I4.5);
                                            EvalTiming.sweep_every, sweeper loop (I4.7)
  ingest/mod.rs                             commit(…, pending, …) (I4.5)
  http/mod.rs, serve.rs, test_support/mod.rs   AppInner.pending, startup load (I4.5)
  api/convert.rs                            match_rows (I4.7)
  api/online_rules.rs                       preview types (I4.6)
  routes/online_rules.rs                    POST /online-rules/preview (I4.6)
fixtures/api/static/online-rule-preview.json, requests/online-rule-preview.json   (I4.6)
web/src/components/online-rules/match-counter.tsx (+ used by rule-form.tsx)       (I4.8)
web/src/routes/$org/$project/settings/general.tsx                                  (I4.8)
```

## Shared reference

### The migration number

The next free number on `origin/main` when you start I4.2; renumber when you rebase if another track takes it first. Check `crates/server/migrations/meta/` on your base **and** `git log origin/main --oneline -- crates/server/migrations/meta/`. Call it `MM` below. Everything that depends on it:
- the file name `crates/server/migrations/meta/00MM_pending_traces.sql`;
- the `meta_schema` snapshot;
- `docs/spec/04-metadata-db.md` (the table), and `00-design.md` § Modules and § Tables, which name the migration file: give them the number you used.

The migration tests need no edit: `open_twice_is_idempotent`, `open_existing_checks_the_file_and_the_version` (`meta/mod.rs`) and `schema_mismatch_names_the_versions` (`admin.rs`) all compute the version with `expected_version()`, which reads the highest embedded migration.

If another branch lands a migration on `main` before M4 merges, the one that merges second renumbers (00-design § Scope).

### Fixed identifiers (fixtures and tests)

| Thing | Value |
|---|---|
| Preview request | `{target: "trace", filter: "status = 'ok'"}` |
| Preview answer | `{matched: 1234, capped: false, from: "2026-01-01T00:00:00.000000000Z", to: "2026-01-02T00:00:00.000000000Z"}` |

The rule, job and evaluator ids are M3's (`03-m3-queue-rules.md` § Fixed identifiers).

### Completion and sweeping in one place

For one tracked project with timeout `T` seconds, at time `now`:
- an entry is **completed** when `root_ended && now − last_seen >= 5 s`, or `now − last_seen >= T s`;
- the sweeper takes at most 2 000 completed entries, ordered by `min_start` then trace id, and cuts them into **batches**: a batch has at most 500 entries, and its last entry's `min_start` is at most 12 h after its first's. A new batch starts when either limit is reached. The batch's `around` is `first + (last − first) / 2`;
- per batch, one `QueryContext` with `QueryOptions { around: Some(around) }`, then one read per rule of the project. `QueryOptions` has the one field `around` today (`crates/storage/src/lib.rs`); `..QueryOptions::default()` would be `clippy::needless_update`. Whoever adds `visible_from` (D-I-34) updates this call site:
  - trace rule: `trace_refresh` with the rule's filter, the batch's ids and `strict_exclusions: true`;
  - span rule: `span_refresh` with the rule's filter, the ids, `per_trace: 200` and `strict_exclusions: true`;
- every matched row is sampled (`sampling::sampled`). For a span rule, the sampled spans of one trace are kept in `start_time` order and cut at 50. The kept rows are the batch's **candidates**;
- only when every rule's read of the batch has succeeded does each candidate ask `state.eval.hourly.admit(rule, now)`. `false` drops it (`admit` counts it as dropped). A batch whose read fails therefore takes no unit of any rule's hourly cap, and adds nothing to the report's `matched`, `sampled` or `capped`, however often it is retried;
- the admitted targets are enqueued as rule jobs (`insert_rule_job`), in writes of at most 1 000 jobs;
- when every read and write of the batch succeeded, the batch's entries whose `last_seen` did not change leave the map (`PendingTraces::finish`). Otherwise each entry's failed-read count grows by 1, and an entry at 5 leaves the map with a `warn`.

### Error outcomes of a batch read

| Error | What the sweeper does |
|---|---|
| `storage.query` fails (`QueryError::Busy` or another) | the batch failed |
| `RunError::Invalid` or `RunError::InvalidRequest` from one rule's read | skip that rule for this batch; `warn!(rule_id = %id, "an online rule's filter could not be read; the rule was skipped")` at most once per rule per tick; the batch goes on |
| any other `RunError` (`Timeout`, `TooLarge`, `Internal`, …) | the batch failed |
| a row that does not convert (`ConvertError`) | the batch failed |
| the enqueue write fails (`MetaError`) | the batch failed (rule jobs are idempotent, so the retry cannot duplicate them) |

---

## I4.1 — Storage: `match_count`

**Goal:** one storage read that counts the rows of the traces or spans view matching a filter in a window, with strict exclusions, stopping at a cap. The preview (I4.6) is its caller.

**Edit only:** `crates/storage/src/reads/traces.rs` (`match_count`, and `trace_refresh`'s strict columns), `crates/storage/src/reads/mod.rs` (re-exports), `crates/storage/src/lib.rs` (re-exports), `docs/spec/05-query.md`.

**Commit:** `Add a capped match count to storage reads`

### `reads/traces.rs`

```rust
/// Most rows [`match_count`] counts (D-I-9's preview).
pub const MAX_MATCH_COUNT: usize = 10_000;

/// Rows of a view that match a filter in a window, for the online-rule preview (D-I-9).
#[derive(Debug, Clone, Copy)]
pub struct MatchCountRequest<'a> {
    /// `ViewName::Traces` or `ViewName::Spans`.
    pub view: ViewName,
    /// Filter text over the view. Empty or whitespace means no filter.
    pub filter: Option<&'a str>,
    /// `start_time ∈ [from, to)`.
    pub window: TimeWindow,
    /// The count stops at `cap + 1`. From 1 to [`MAX_MATCH_COUNT`].
    pub cap: usize,
    /// Apply strict exclusions instead of the default ones.
    pub strict_exclusions: bool,
    /// Time allowed for planning and running.
    pub timeout: Duration,
}

/// Counts the matching rows, at most `cap + 1`: a result above `cap` means "more than `cap`".
///
/// # Errors
///
/// [`RunError::InvalidRequest`] for another view or a `cap` outside `1..=MAX_MATCH_COUNT`;
/// [`RunError::Invalid`] for filter text; [`RunError::Timeout`]; DataFusion failures through
/// [`classify`] and `ctx.fail`.
pub async fn match_count(ctx: &QueryContext, request: MatchCountRequest<'_>) -> Result<usize, RunError>
```

The body, in order:
1. `view`: `ViewName::Traces` → table `LIST` (`"traces"`) and `FilterView::Traces`; `ViewName::Spans` → `SPANS` and `FilterView::Spans`; any other → `InvalidRequest("match counts are for traces and spans only")`.
2. `cap` outside `1..=MAX_MATCH_COUNT` → `InvalidRequest("cap must be between 1 and 10000")`.
3. Parse the filter (`present(request.filter)` then `ctx.parse_filter(view, text)?`), as `list_frame` does.
4. Under `tokio::time::timeout(request.timeout, …)` (`Err(_)` → `RunError::Timeout`):
   - `frame = ctx.session().table(table).await.map_err(classify)?`;
   - conjuncts: `time_window(request.window)`; then, when `strict_exclusions`, `strict_exclusions(ctx, view, None).await?` (M3's function); otherwise `exclusions(ctx, view, false, false, references_environment)?` when it returns `Some`;
   - `frame.filter(<conjunction>)`, then `filter.apply(frame, ctx)` when there is a filter;
   - `frame.limit(0, Some(cap + 1)).map_err(classify)?.count().await.map_err(|error| ctx.fail(error))`.

Read `list_frame` first: the steps above are its first half without the cursor, the search, the sort and the column selection. Check that `DataFrame::count` exists in the pinned DataFusion (`rg -n "pub async fn count" ~/.cargo/registry/src/*/datafusion-[0-9]*/src/dataframe/mod.rs`); if it does not, `select` one column and sum the collected batches' `num_rows()`.

Export `MatchCountRequest`, `match_count` and `MAX_MATCH_COUNT` from `reads/mod.rs` and `lib.rs`, next to M3's `span_refresh` exports.

`match_count` has no production caller before I4.6. It is a public storage read with its own tests, so it is not dead code. Its `strict_exclusions: false` branch has no production caller at all (the preview always passes `true`); it stays, because `match_count_strict_drops_eval_and_experiment_traces` compares both.

### `trace_refresh`: two columns when strict

M3's `trace_refresh` returns every column of the traces view. Its strict reads are the sweeper's (I4.7), which needs only the trace id and the start time, for up to 500 traces per rule per tick. In `trace_refresh`'s frame, when `request.strict_exclusions` is true, select only those two columns after the filter: `frame.select_columns(&[traces::TRACE_ID, traces::START_TIME])` (use the column-name constants the module already uses; `rg -n "START_TIME" crates/storage/src/reads/traces.rs`). Add, next to M3's `SPAN_REFRESH_COLUMNS`:

```rust
/// The columns `trace_refresh` returns with `strict_exclusions: true`, in this order.
pub const TRACE_REFRESH_STRICT_COLUMNS: &[&str] = &[traces::TRACE_ID, traces::START_TIME];
```

and export it from `reads/mod.rs` and `lib.rs`. Non-strict calls (the list refresh routes) are unchanged. If an M3 test of `trace_refresh` with `strict_exclusions: true` reads another column, change it to read `trace_id` only, and say so in the report.

### Spec

`docs/spec/05-query.md`, after M3's **Strict exclusions** paragraph: "**Match counts** (D-I-9). The online-rule preview counts the traces or spans that a filter matches in a window with strict exclusions, stopping at 10 001 rows (`match_count`), so the editor can show '10 000 or more' without reading every row. A spans count with strict exclusions also reads the project's traces to leave out evaluation and experiment traces, and that subquery is not bounded by the window (§ Strict exclusions). On a large project such a count can therefore hit its 10 s timeout and answer `422 query_timeout`. This is accepted: the editor then says the count could not be loaded. With strict exclusions, `trace_refresh` returns only `trace_id` and `start_time`, the two columns the trigger needs."

The cost described in that last sentence is accepted by this plan. Do not bound the subquery.

### Tests (`reads/traces.rs`, copy the setup of the `trace_refresh` and M3 `span_refresh` tests)

- `match_count_counts_in_the_window`: four traces, two inside the window, one before, one after → 2.
- `match_count_applies_the_filter`: filter `status = 'error'` over two ok traces and one error trace → 1.
- `match_count_strict_drops_eval_and_experiment_traces`: a normal, an `llmobs-eval` and an experiment trace; filter `environment = 'llmobs-eval'` → 0 with strict, 1 without.
- `match_count_counts_spans`: a trace of 3 spans, 2 with `span_kind = 'llm'`; view spans, filter `span_kind = 'llm'` → 2.
- `match_count_stops_at_cap_plus_one`: 5 traces, `cap: 2` → 3.
- `strict_trace_refresh_returns_two_columns`: a `trace_refresh` with `strict_exclusions: true` → the batches' schema has exactly `TRACE_REFRESH_STRICT_COLUMNS`; with `false` → the full row, as before.
- `match_count_validates_its_request`: `ViewName::Sessions`, `cap: 0` and `cap: 10_001` are `InvalidRequest`; a bad filter is `Invalid`.

### Done when

`just check` passes.

---

## I4.2 — The `pending_traces` table, the trigger's rule read and the guarded enqueue

**Goal:** migration `00MM_pending_traces.sql`; `meta/pending_traces.rs` to load and write the table; `meta::online_rules::trigger_rules`, the one query the sweeper runs each tick; `meta::eval_jobs::insert_rule_job`, a rule enqueue that does nothing when the rule is gone or disabled.

**Edit only:** `crates/server/migrations/meta/00MM_pending_traces.sql` (new), `crates/server/src/meta/pending_traces.rs` (new), `crates/server/src/meta/mod.rs` (module list), `crates/server/src/meta/online_rules.rs` (one struct and one function, with tests), `crates/server/src/meta/eval_jobs.rs` (one function, with tests), the `meta_schema` snapshot, `docs/spec/04-metadata-db.md`, `docs/plans/i-online-eval/00-design.md` (the migration's name).

**Commit:** `Store pending traces and read the rules the trigger needs`

### Migration

§ The migration number says how to choose `MM` and what depends on it.

```sql
CREATE TABLE pending_traces (
  project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  trace_id BLOB NOT NULL,
  first_seen INTEGER NOT NULL,
  last_seen INTEGER NOT NULL,
  min_start INTEGER NOT NULL,
  root_ended INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (project_id, trace_id)
);
```

The failed-read count is not stored: a restart gives every trace five fresh tries, which is fine.

**Existing tests:** only the `meta_schema` snapshot changes (§ The migration number). Regenerate the snapshot with `INSTA_UPDATE=always cargo test -p llmobs-server schema_matches_the_snapshot`. Its diff must add only the table and SQLite's automatic index of its primary key. Say so in the report.

### `meta/pending_traces.rs`

```rust
//! `pending_traces` rows: the persisted copy of the trigger's in-memory map (D-I-8).

/// One pending trace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PendingRow {
    pub(crate) project_id: ProjectId,
    pub(crate) trace_id: TraceId,
    pub(crate) first_seen: i64,
    pub(crate) last_seen: i64,
    pub(crate) min_start: i64,
    pub(crate) root_ended: bool,
}

/// Every row of an active project of an active org, ordered by `project_id, trace_id`.
///
/// # Errors
///
/// [`MetaError`] when the query fails, or `MetaError::Corrupt` for a `trace_id` blob that is not 16 bytes.
pub(crate) async fn load_pending(pool: &SqlitePool) -> Result<Vec<PendingRow>, MetaError>;

/// Deletes `deletes`, then upserts `upserts`, on the caller's connection (the caller's
/// `MetaDb::write` is the transaction). An upsert for a project that no longer exists writes nothing.
///
/// # Errors
///
/// [`MetaError`] when a statement fails.
pub(crate) async fn write_pending(
    conn: &mut SqliteConnection,
    deletes: &[(ProjectId, TraceId)],
    upserts: &[PendingRow],
) -> Result<(), MetaError>;
```

- `load_pending`: `SELECT t.project_id, t.trace_id, t.first_seen, t.last_seen, t.min_start, t.root_ended FROM pending_traces t JOIN projects p ON p.id = t.project_id JOIN orgs o ON o.id = p.org_id WHERE p.state = 'active' AND o.state = 'active' ORDER BY t.project_id, t.trace_id`. `trace_id` is the 16 raw bytes (`TraceId::from_bytes`, as `meta/eval_jobs.rs` reads its blobs); `project_id` decodes as the other meta modules decode it.
- `write_pending`: one `DELETE FROM pending_traces WHERE project_id = ? AND trace_id = ?` per delete, then one statement per upsert:
  ```sql
  INSERT INTO pending_traces (project_id, trace_id, first_seen, last_seen, min_start, root_ended)
  SELECT ?1, ?2, ?3, ?4, ?5, ?6 WHERE EXISTS (SELECT 1 FROM projects WHERE id = ?1)
  ON CONFLICT(project_id, trace_id) DO UPDATE SET
    first_seen = excluded.first_seen, last_seen = excluded.last_seen,
    min_start = excluded.min_start, root_ended = excluded.root_ended
  ```
  The `WHERE EXISTS` makes a project deleted since the snapshot a no-op instead of a foreign-key error that would fail the whole flush. (SQLite needs a `WHERE` on an `INSERT … SELECT` that has an `ON CONFLICT` clause; this one is it.)

### `meta/online_rules.rs`: the trigger's read

```rust
/// What the trigger needs of one enabled, unpaused rule of an active project (D-I-9).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TriggerRule {
    pub(crate) id: Uuid,
    pub(crate) project_id: ProjectId,
    pub(crate) evaluator_id: Uuid,
    /// `"trace"` or `"span"`.
    pub(crate) target: String,
    pub(crate) filter: Option<String>,
    pub(crate) sample_rate: f64,
    /// The project's `settings.trace_completion_timeout_s`, as stored (not range-checked here).
    pub(crate) completion_timeout_s: Option<i64>,
}

/// Every enabled, unpaused rule of an active project of an active org, ordered by `project_id, id`.
///
/// # Errors
///
/// [`MetaError`] when the query fails, or `MetaError::Corrupt` for a stored `target` outside the list.
pub(crate) async fn trigger_rules(pool: &SqlitePool) -> Result<Vec<TriggerRule>, MetaError>;
```

```sql
SELECT r.id, r.project_id, r.evaluator_id, r.target, r.filter, r.sample_rate,
       CAST(CASE WHEN json_valid(p.settings)
                 THEN json_extract(p.settings, '$.trace_completion_timeout_s') END AS INTEGER)
         AS completion_timeout_s
FROM online_rules r
JOIN projects p ON p.id = r.project_id
JOIN orgs o ON o.id = p.org_id
WHERE r.enabled = 1 AND r.paused_reason IS NULL AND p.state = 'active' AND o.state = 'active'
ORDER BY r.project_id, r.id
```

`json_valid` keeps a hand-edited, invalid `settings` from failing the query: such a project gets `None`, the default. The cast turns a stored string into an integer (a non-number becomes 0, which I4.7's range check treats as unset).

### `meta/eval_jobs.rs`: `insert_rule_job`

```rust
/// Enqueues a rule job (`kind = 'rule'`, D-I-11) only while its rule exists and is enabled.
/// `ON CONFLICT(dedup_key) DO NOTHING`. True when a row was inserted.
///
/// # Errors
///
/// [`MetaError`] when the insert fails. A missing or disabled rule is `Ok(false)`, not an error.
pub(crate) async fn insert_rule_job(conn: &mut SqliteConnection, job: NewJob, now: i64) -> Result<bool, MetaError>;
```

Read M3's `insert_job` first and copy its column list, its bindings, `dedup_key(&job)` and `priority_of(KIND_RULE)`. The one difference: `INSERT INTO eval_jobs (<columns>) SELECT <the same placeholders> WHERE EXISTS (SELECT 1 FROM online_rules WHERE id = ?<rule_id placeholder> AND enabled = 1) ON CONFLICT(dedup_key) DO NOTHING`. Number every placeholder of this statement explicitly, `?1` to `?n` in column order, and reuse the rule id's number inside `EXISTS`. Never mix anonymous `?` with `?N` in one statement: SQLite numbers anonymous ones after the highest explicit number, so the bindings would shift. A `job` whose `kind` is not `KIND_RULE` or whose `rule_id` is `None` is a `MetaError::Corrupt { table: "eval_jobs", column: "kind" }` (a programming error, reported, never a panic). A paused rule still gets its job: paused rules keep their queued jobs (D-I-14).

`trigger_rules`, `TriggerRule`, `insert_rule_job` and the `pending_traces` items have no production caller before I4.5 and I4.7. Add the allows that § Conventions › Dead-code allows in M4 lists for I4.2, with their comments.

### Spec

`docs/spec/04-metadata-db.md` § Evaluation: replace the `pending_traces` DDL with the migration's, and its comment with: "`pending_traces` is the persisted copy of the trigger's in-memory map (spec 06 § Online rules › Triggering, D-I-8): the flusher writes the changed entries every 5 s and at shutdown, and startup loads them. `first_seen` and `last_seen` are ingest times; `min_start` is the earliest span `start_time` seen." In the `projects` DDL comment, keep `trace_completion_timeout_s` and add "(integer seconds, 10–3600; absent means 60, D-I-7)".

### Tests

With `crate::meta::test_db()`, an org, two projects, a connection, an evaluator and rules from M3's functions:

`meta/pending_traces.rs`:
- `write_then_load_round_trips`: two rows of project A, one of B → `load_pending` returns the three, ordered.
- `an_upsert_replaces_the_columns`: write a row, write it again with a later `last_seen` and `root_ended: true` → one row with the new values.
- `deletes_run_before_upserts`: the same key in `deletes` and `upserts` of one call → the row exists afterwards.
- `an_upsert_for_a_missing_project_writes_nothing`: a random `ProjectId` → `Ok(())`, no row.
- `load_skips_inactive_projects_and_orgs`: set project B to `deleting` (M3's tests show the SQL) → only A's rows.
- `deleting_the_project_cascades`.
- `a_bad_trace_id_blob_is_corrupt`: insert a 3-byte blob by SQL → `load_pending` is `MetaError::Corrupt`.

`meta/online_rules.rs`:
- `trigger_rules_returns_enabled_unpaused_rules_of_active_projects`: an enabled rule, a disabled one, a paused one, one in a `deleting` project, one in a `deleting` org → only the first.
- `trigger_rules_reads_the_project_timeout`: `UPDATE projects SET settings = '{"trace_completion_timeout_s": 120}'` → `Some(120)`; `'{}'` → `None`; `'not json'` → `None`; `'{"trace_completion_timeout_s": "abc"}'` → `Some(0)`.

`meta/eval_jobs.rs`:
- `insert_rule_job_inserts_once`: the second call with the same target returns false; one row with `kind = 'rule'`, `priority = 1`.
- `insert_rule_job_needs_an_enabled_rule`: a disabled rule → false, no row; a deleted rule id → false, no row and no error; a paused rule → true.
- `insert_rule_job_refuses_other_kinds`: a manual `NewJob` → `MetaError::Corrupt`.

### Done when

`just check` passes.

---

## I4.3 — The project's trace completion timeout

**Goal:** `trace_completion_timeout_s` is stored in `projects.settings`, read into `ProjectRow`, shown in `Project` and set by `PATCH /projects/{project}`.

**Edit only:** `crates/server/src/meta/projects.rs`, `crates/server/src/api/account.rs`, `crates/server/src/routes/projects.rs`, `crates/server/src/routes/auth.rs` (the `Project` literal of the setup response only), `crates/server/tests/snapshots/project.snap`, `crates/server/openapi.json`, `fixtures/api/static/project.json`, `fixtures/api/static/projects.json`, `web/src/api/schema.gen.ts`, and the `web/src` files where `just check-web` reports a missing `trace_completion_timeout_s` (type errors only: add `trace_completion_timeout_s: 60`), `docs/spec/09-api.md`.

**Commit:** `Add the project trace completion timeout`

### `meta/projects.rs`

```rust
/// Default seconds without a new span before a trace counts as complete (D-I-7).
pub(crate) const COMPLETION_TIMEOUT_DEFAULT_S: u32 = 60;
pub(crate) const COMPLETION_TIMEOUT_MIN_S: u32 = 10;
pub(crate) const COMPLETION_TIMEOUT_MAX_S: u32 = 3600;
```

- `ProjectRow` gains, after `retention_days`: `/// `settings.trace_completion_timeout_s`; `None` means the default (D-I-7). pub(crate) trace_completion_timeout_s: Option<u32>,` and a method `pub(crate) fn completion_timeout_s(&self) -> u32` (the value, else `COMPLETION_TIMEOUT_DEFAULT_S`). Update the struct's doc ("`settings` is not read in D." → "Of `settings`, only `trace_completion_timeout_s` is read (D-I-7).").
- `ProjectRecord` gains `trace_completion_timeout_s: Option<i64>`. Every `SELECT` and `RETURNING` list in this file that builds a `ProjectRecord` adds `CAST(CASE WHEN json_valid(settings) THEN json_extract(settings, '$.trace_completion_timeout_s') END AS INTEGER) AS trace_completion_timeout_s` (with `projects.settings` in the query that joins `orgs`). They are the four queries of `insert_project` (its `RETURNING`), `project_by_id`, `project_by_slugs` (columns written `projects.retention_days, projects.state, …`) and `projects_of_org`; `rg -n "ProjectRecord" crates/server/src/meta/projects.rs` checks that none is missed.
- `project_row` maps it: a value in `COMPLETION_TIMEOUT_MIN_S..=COMPLETION_TIMEOUT_MAX_S` → `Some`, anything else (including a hand-edited out-of-range value) → `None`. Out-of-range is not `Corrupt`: the setting is advisory, and the default is safe.
- `ProjectPatch` gains `/// New completion timeout, already range-checked. pub(crate) trace_completion_timeout_s: Option<u32>,`. Every `ProjectPatch { … }` literal (`rg -n "ProjectPatch \{" crates/server/src`) gets `trace_completion_timeout_s: None` where it is not the PATCH route.
- `update_project` becomes one statement (the four-arm `match` goes away):
  ```sql
  UPDATE projects SET
    name = COALESCE(?1, name),
    retention_days = CASE WHEN ?2 THEN ?3 ELSE retention_days END,
    settings = CASE WHEN ?4 IS NULL THEN settings
                    ELSE json_set(CASE WHEN json_valid(settings) THEN settings ELSE '{}' END,
                                  '$.trace_completion_timeout_s', ?4) END,
    updated_at = ?5
  WHERE id = ?6
  ```
  with `?1 = patch.name`, `?2 = patch.retention_days.is_some()`, `?3 = encode_retention(days)?` when `Some(days)` else `None`, `?4 = patch.trace_completion_timeout_s.map(i64::from)`. The existing `update_project` tests must pass unchanged.

### API (`api/account.rs`)

- `Project` gains, after `retention_days`: `/// Seconds without a new span after which a trace counts as complete for online rules (D-I-7). 10 to 3600; 60 unless set. pub trace_completion_timeout_s: u32,` (always present).
- `UpdateProjectRequest` gains: `/// Seconds without a new span after which a trace counts as complete for online rules, 10 to 3600. Absent leaves it unchanged. #[serde(default, skip_serializing_if = "Option::is_none")] pub trace_completion_timeout_s: Option<u32>,`. There is no way to unset it; sending 60 restores the default behaviour.
- The test that builds a `Project` in this file (`rg -n "let project = Project" crates/server/src/api/account.rs`) adds `trace_completion_timeout_s: 60`. Its snapshot `crates/server/tests/snapshots/project.snap` changes: run `INSTA_UPDATE=always cargo test -p llmobs-server project`; the diff must be only `"trace_completion_timeout_s": 60,` after `"retention_days": 30,`.
- `routes/auth.rs` builds a `Project` for the setup response (`project: Project {` near line 193): add `trace_completion_timeout_s: created.created.project.completion_timeout_s(),`. No other server file builds an `api::account::Project` (`rg -n "Project \{" crates/server/src` also lists `Access::Project` and `WebhookProject`, which are other types).

### Route (`routes/projects.rs`)

`update_project`:
- The handler already has `Extension(principal)` and `let actor_id = principal::session_user(&principal)?.id;`. Reuse them; add no extractor.
- After the retention check, `let timeout = body.trace_completion_timeout_s.map(checked_timeout).transpose()?;`, with a new helper next to `checked_retention` in the same style:
  ```rust
  fn checked_timeout(seconds: u32) -> Result<u32, ApiError> {
      if (COMPLETION_TIMEOUT_MIN_S..=COMPLETION_TIMEOUT_MAX_S).contains(&seconds) {
          Ok(seconds)
      } else {
          Err(field_error("trace_completion_timeout_s", "must be 10 to 3600"))
      }
  }
  ```
  Like the name and retention checks, it returns the first error at once.
- Pass `timeout` as `ProjectPatch.trace_completion_timeout_s`.
- After the write, when `timeout` is `Some(seconds)`, write the audit line with the existing `actor_id`: `tracing::info!(target: "audit", project_id = %id, actor_id = %actor_id, seconds, "trace completion timeout changed")`. `project_json` sets `trace_completion_timeout_s: project.completion_timeout_s()`. The permission stays the route's (`ManageSettings`).

The trigger reads the value itself each tick (I4.2's query), so nothing else changes here.

### Contract

`just openapi`; the fixtures `project.json` and every project of `projects.json` gain `"trace_completion_timeout_s": 60` after `retention_days` (or where `retention_days` would be, when absent). `just api-examples`. Run `just check-web`; where TypeScript reports a `Project` object without the field (test fixtures in `web/src`), add `trace_completion_timeout_s: 60`, and nothing else.

### Spec

`docs/spec/09-api.md` § Projects, the `PATCH /projects/{project}` row: add "`trace_completion_timeout_s` (10–3600 s; when online rules consider a trace complete, D-I-7)", and the `Project` shape lists the field.

### Tests (`routes/projects.rs`, over HTTP)

- `a_new_project_has_the_default_timeout`: `GET` answers `trace_completion_timeout_s: 60`.
- `patch_sets_the_timeout`: `PATCH {"trace_completion_timeout_s": 120}` → `200` with 120; `GET` returns 120; the name and retention are unchanged; `projects.settings` holds `{"trace_completion_timeout_s":120}` (read it by SQL).
- `patch_refuses_an_out_of_range_timeout`: 9 and 3601 → `400` on `trace_completion_timeout_s`.
- `a_timeout_change_is_audited`: with `capture_logs` (plain `#[tokio::test]`), PATCH 120 as the session user. The log has `trace completion timeout changed` with the user's id; a PATCH of the name alone logs no such line.
- `patch_keeps_other_settings_keys`: set `settings = '{"other": 1}'` by SQL, PATCH 30 → `settings` has both keys.
- `a_garbage_settings_value_reads_as_the_default`: `settings = '{"trace_completion_timeout_s": 99999}'` by SQL → `GET` answers 60.

`meta/projects.rs`: `update_project_sets_each_field_alone` (name only, retention only, timeout only; each leaves the others as they were).

### Done when

`just check` passes; `openapi.json` shows the field on `Project` and `UpdateProjectRequest`.

---

## I4.4 — Sampling and `PendingTraces`

**Goal:** the two pure building blocks of the trigger: the sampling function (D-I-10) and the in-memory map of pending traces (D-I-8). No database, no `AppState`, no wiring.

**Edit only:** `crates/server/src/eval/sampling.rs` (new), `crates/server/src/eval/pending.rs` (new), `crates/server/src/eval/mod.rs` (two module lines).

**Commit:** `Add trigger sampling and the pending-trace map`

### `eval/sampling.rs` (D-I-10)

```rust
//! Deterministic sampling of rule targets (D-I-10).

/// A hash input, not an attribute name: the D46 rename must not change which targets are sampled.
const SAMPLE_TAG: &[u8] = b"llmobs.sample.v1";

/// Whether `rule` samples this target. `rate >= 1.0` always does; `rate` that is not above 0
/// (or NaN) never does. Otherwise the first 8 bytes, big-endian, of SHA-256(tag ‖ rule id (16 bytes)
/// ‖ trace id (16) ‖ span id (8, span rules only)) must be below `(rate · 2^64) as u64`.
pub(crate) fn sampled(rate: f64, rule: Uuid, trace: TraceId, span: Option<SpanId>) -> bool;
```

`(rate * 18_446_744_073_709_551_616.0) as u64` (Rust's float-to-int cast saturates; rates in `(0, 1)` stay below `u64::MAX`). Use `sha2::Sha256` as `eval/execute.rs` does for the judge trace id.

Tests:
- `rate_one_always_samples` and `rate_zero_never_samples` (0.0, -1.0, `f64::NAN`).
- `sampling_is_pinned`: rule `01900000-0000-7000-8000-000000000700`, trace `a1000000000000000000000000000001`, no span. Compute the first 8 bytes of SHA-256 of the tag ‖ the 16 rule bytes ‖ the 16 trace bytes outside this code, in a file under your scratchpad: `{ printf 'llmobs.sample.v1'; printf '%s%s' 01900000000070008000000000000700 a1000000000000000000000000000001 | xxd -r -p; } | sha256sum`, then take the first 16 hex digits. Call that big-endian `u64` value `h`. Write `h` as a literal in the test (`const H: u64 = 0x…;`) with a comment giving the command. Derive the two rates in the test, never as hand-written float literals: f64 granularity near `h / 2^64` would make them fragile.
- Use `let above = (H as f64 + 1e6) / 2f64.powi(64);` and `let below = (H as f64 - 1e6) / 2f64.powi(64);`.
- Assert `sampled(above, …)` is true and `sampled(below, …)` is false.
- Also assert that `draw(rule, trace, None) == H`, which pins the hash itself.
- If `H` is within `1e6` of 0 or of `u64::MAX`, the margins do not fit. STOP and report it.
- `the_span_changes_the_draw`: the same rule and trace with two span ids give different hashes (test a private `draw(rule, trace, span) -> u64` helper that `sampled` uses).
- `a_quarter_samples_about_a_quarter`: 10 000 trace ids (`TraceId::from_bytes` of `i.to_be_bytes()` padded, skipping 0) at rate 0.25 → between 2 300 and 2 700 sampled.

### `eval/pending.rs` (D-I-8)

```rust
//! The traces the online-rule trigger waits on (D-I-8). Memory only: `commit` updates it, the
//! sweeper reads and removes completed traces, and the flusher persists the changes.

/// Most traces held at once (D-I-8).
pub(crate) const MAX_PENDING: usize = 200_000;
/// A trace whose root ended is complete after this long without a new span (D-I-7).
pub(crate) const ROOT_IDLE_NS: i64 = 5_000_000_000;
/// Failed sweeps after which a trace is dropped (D-I-9).
pub(crate) const MAX_FAILED_READS: u8 = 5;
/// The full-map warning is logged at most this often.
const WARN_EVERY_NS: i64 = 60_000_000_000;

/// One pending trace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Entry {
    /// Ingest time of the first span seen.
    pub(crate) first_seen: i64,
    /// Ingest time of the last span seen.
    pub(crate) last_seen: i64,
    /// Earliest span `start_time` seen.
    pub(crate) min_start: i64,
    /// A span without a parent was seen.
    pub(crate) root_ended: bool,
    /// Sweeps whose reads failed (memory only).
    pub(crate) failed_reads: u8,
}

/// A completed trace, as the sweeper saw it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Completed {
    pub(crate) trace_id: TraceId,
    pub(crate) last_seen: i64,
    pub(crate) min_start: i64,
}

/// What the flusher writes: entries changed since the last flush, and keys removed since.
#[derive(Debug, Default, Clone, PartialEq)]
pub(crate) struct Changes {
    pub(crate) upserts: Vec<(ProjectId, TraceId, Entry)>,
    pub(crate) deletes: Vec<(ProjectId, TraceId)>,
}

pub(crate) struct PendingTraces { max: usize, inner: Mutex<Inner> }

#[derive(Default)]
struct Inner {
    tracked: HashSet<ProjectId>,
    projects: HashMap<ProjectId, HashMap<TraceId, Entry>>,
    len: usize,
    changed: HashSet<(ProjectId, TraceId)>,
    removed: HashSet<(ProjectId, TraceId)>,
    dropped_full: u64,
    warned_at: Option<i64>,
}
```

`Default` builds `PendingTraces::new(MAX_PENDING)`. Write `Debug` by hand: `len` and `tracked.len()` only.

Methods (each takes the lock once, briefly):

| Method | What |
|---|---|
| `new(max: usize) -> Self` | an empty map holding at most `max` traces (tests pass small values) |
| `observe(&self, project: ProjectId, spans: &[Span], now: i64)` | Nothing when `project` is not tracked. For each span: skip it when `span.environment.as_deref() == Some(environment::EVAL)` or `span.metadata.contains_key(metadata_key::EXPERIMENT_ID)` (D-I-17). Otherwise for `(project, span.trace_id)`: an existing entry gets `last_seen = max(last_seen, now)` (concurrent commits can call with an older `now`; `last_seen` must never move back, or `finish` could remove a trace that grew), `min_start = min(min_start, span.start_time)`, `root_ended |= span.parent_span_id.is_none()`; a new one, when `len < max`, is inserted with `first_seen = last_seen = now`, `min_start = span.start_time`, `root_ended = span.parent_span_id.is_none()`, `failed_reads = 0` (and `len += 1`); when `len >= max`, `dropped_full += 1` and nothing is inserted. Every inserted or updated key goes into `changed` and out of `removed`. The warning decision is made and recorded under the lock: the call dropped at least one trace, and `warned_at` is `None` or at least `WARN_EVERY_NS` before `now`. When it holds, set `warned_at` and copy `dropped_full` and `len` into locals. After the guard is dropped, `tracing::warn!(dropped = <dropped_full>, pending = <len>, "online-rule trigger map is full; new traces are not evaluated")`. |
| `set_tracked(&self, projects: HashSet<ProjectId>)` | Replaces the tracked set. Every entry of a project not in the new set is removed, its key put in `removed` and taken out of `changed`. |
| `completed(&self, project, now, timeout_ns: i64, limit: usize) -> Vec<Completed>` | The project's entries with `root_ended && now − last_seen >= ROOT_IDLE_NS` or `now − last_seen >= timeout_ns` (saturating subtraction). **Under the lock**, one pass over the project's map that pushes each matching entry as a `Completed` (`trace_id`, `last_seen`, `min_start`: 40 bytes) into a `Vec` created with `Vec::with_capacity(limit * 4)`, and **stops once it holds `limit * 4`** (at most 8 000 entries, 320 KB, for `PER_PROJECT_TICK`). No sort under the lock. **After the guard is dropped**, sort by `(min_start, trace_id)` (`TraceId`'s bytes) and keep the first `limit` (`select_nth_unstable_by` then sort the first `limit`, when longer). Every `commit` takes this lock, so it must not be held across a sort or a 200 000-entry copy. Because the pass stops early in hash order, a tick does not always take the globally oldest completed traces; the rest are taken on the next ticks. That is accepted. |
| `finish(&self, project, done: &[Completed])` | Removes each entry whose `last_seen` still equals the `Completed`'s (a trace that grew during the reads stays, D-I-9); removed keys go into `removed`. |
| `read_failed(&self, project, failed: &[Completed]) -> usize` | For each entry still present: `failed_reads += 1`; at `MAX_FAILED_READS` the entry is removed (into `removed`). Returns how many were removed. It does not mark entries `changed` (the count is not stored). |
| `take_changes(&self) -> Changes` | **Under the lock:** take both sets out with `std::mem::take`. Build `upserts`, every key of `changed` whose entry exists, with a copy of the entry; `deletes` is every key of `removed`. **After the guard is dropped:** sort both lists by key (project id, then trace id bytes), so the flush order is deterministic. |
| `restore(&self, changes: Changes)` | After a failed flush. It consumes `changes`: iterate `changes.upserts` and `changes.deletes` by value (`into_iter`). Each upsert key whose entry still exists goes back into `changed`; each delete key that has no entry now goes back into `removed`. |
| `load(&self, rows: impl IntoIterator<Item = (ProjectId, TraceId, Entry)>)` | Startup. An entry of a tracked project is inserted while `len < max`, without marking it `changed`. An entry of a tracked project past `max`, and an entry of an untracked project, go into `removed` only, so the next flush deletes their rows instead of keeping them across restarts. |
| `len(&self) -> usize` | Traces held. The flusher logs it (I4.5). |

There is no `is_tracked` and no `dropped_full()` getter. The `dropped_full` field is read only by the full-map warning.

`ProjectId` and `TraceId` must be `Hash + Eq` (check `crates/core/src/ids.rs`; both derive it). `TraceId`'s order for sorting: compare `as_bytes()`. `Span` is `llmobs_core::span::Span` (check the import in `test_support/mod.rs`); `environment::EVAL` and `metadata_key::EXPERIMENT_ID` are in `llmobs_core::names` (as `ingest/mod.rs` imports `environment::EVAL`).

Nothing here has a production caller before I4.5 and I4.7. Add the allows that § Conventions › Dead-code allows in M4 lists for I4.4, item by item, with their comments. A module-level allow is not enough.

### Tests (`eval/pending.rs`, unit tests with hand-built `Span`s)

Helpers: `span(trace: u8, parent: bool, start: i64) -> Span` built from `test_support::dataset_span(0)` with its ids and fields replaced; `track(&pending, project)`.

- `untracked_projects_are_ignored`.
- `eval_and_experiment_spans_are_ignored`: environment `llmobs-eval`; metadata `llmobs.experiment_id` → no entry.
- `observe_tracks_first_last_min_and_root`: two calls at `now` 10 and 20 with starts 500 and 300, the second with a root → `first_seen 10`, `last_seen 20`, `min_start 300`, `root_ended`.
- `the_map_is_capped`: `new(2)`, three traces → `len() == 2`. A span of an existing trace still updates it. With `capture_logs`, the warning is logged once (`dropped=1`) for two overflowing calls within a minute, and again after a minute.
- `completed_uses_root_idle_and_timeout`: a rooted trace at `last_seen` 0 is complete at 5 s, not at 4.9 s; an unrooted one only at the timeout; sorted by `min_start`; `limit` cuts.
- `completed_collects_at_most_four_times_the_limit`: 50 completed traces, `limit` 5 → 5 returned, all from completed traces, sorted.
- `last_seen_never_moves_back`: observe at `now` 20, then at 10 → `last_seen` is 20.
- `finish_keeps_a_trace_that_grew`: take `completed`, observe the trace again, `finish` → it stays; another trace leaves and appears in `deletes`.
- `read_failed_drops_after_five`: four calls keep it, the fifth removes it and returns 1.
- `set_tracked_drops_other_projects`.
- `take_changes_then_restore`: changes → taken (both lists, sorted) → `take_changes` again is empty → `restore` → the same changes again; a key observed between take and restore keeps a single `changed` mark.
- `load_skips_untracked_projects`: their keys come back as `deletes`.
- `load_past_max_deletes_the_rest`: `new(2)`, load three rows of a tracked project. `len() == 2`, and the third key comes back as a delete.

### Done when

`just check` passes.

---

## I4.5 — Wiring: the `commit` argument, `AppInner.pending`, the flusher and startup

**Goal:** every commit feeds `PendingTraces`; the flusher persists it every 5 s and at shutdown; startup loads the tracked set and the table before the server binds.

**Edit only:** `crates/server/src/ingest/mod.rs` (`commit`, its doc, its tests' call sites, one new test), `crates/server/src/http/mod.rs` (`AppInner.pending`), `crates/server/src/serve.rs` (construct it; the startup call; the final flush in `Running::shutdown`; one test), `crates/server/src/test_support/mod.rs` (construct it; `commit_spans`), `crates/server/src/eval/trigger.rs` (new), `crates/server/src/eval/mod.rs` (module line, `EvalTiming.flush_every`, the flusher loop), `crates/server/src/eval/pending.rs` and `crates/server/src/meta/{pending_traces,online_rules}.rs` (remove the I4.5 `dead_code` attributes of § Conventions › Dead-code allows in M4, nothing else), `docs/spec/02-ingestion.md`, `docs/spec/12-operations.md`.

**Commit:** `Track pending traces from commits and persist them`

### `AppInner.pending`

`pub(crate) pending: crate::eval::pending::PendingTraces` (doc: "Traces the online-rule trigger waits on (D-I-8)."). `serve.rs` and `test_support/mod.rs` set `pending: crate::eval::pending::PendingTraces::default()`.

### `Ingest::commit`

Signature: `commit(&self, storage: &Storage, usage: &Usage, live: &LiveBus, pending: &PendingTraces, target: CommitTarget, normalized: Normalized)`. Right after `live.publish(target.project_id, &spans);`, add `pending.observe(target.project_id, &spans, target.now_ns);`. Doc: add "then records the trace in the trigger's pending map when its project has an online rule (D-I-8; memory only)". `commit_in_task` passes `&state.pending`; `TestApp::commit_spans` passes `&self.state.pending`; every test call in `ingest/mod.rs` passes `&app.state.pending` (`rg -n "\.commit\(" crates/server/src`).

New test in `ingest/mod.rs`: `commit_records_tracked_projects_only`: `app.state.pending.set_tracked([project A])`; commit a dataset span to A and one to B → `pending.len() == 1`; a judge-trace span (`environment = llmobs-eval`) to A adds nothing.

### `eval/trigger.rs`

```rust
//! The online-rule trigger (D-I-7 to D-I-11): the rules it runs, the flusher and startup. I4.7 adds the sweeper.

/// Most `pending_traces` rows written per transaction (D-I-8).
const FLUSH_CHUNK: usize = 1_000;

/// The enabled, unpaused rules of one tracked project, and its completion timeout.
#[derive(Debug, Clone, PartialEq)]
#[allow(dead_code)] // I4.7 reads it.
pub(crate) struct ProjectRules {
    pub(crate) timeout_ns: i64,
    pub(crate) rules: Vec<TriggerRule>,
}

/// Reads the rules (`meta::online_rules::trigger_rules` on the reader pool), groups them by
/// project, and sets the tracked set to their projects (D-I-8).
///
/// # Errors
///
/// [`MetaError`] when the read fails; the tracked set is then left as it was.
pub(crate) async fn refresh_rules(state: &AppState) -> Result<BTreeMap<ProjectId, ProjectRules>, MetaError>;

/// Startup: [`refresh_rules`], then `load_pending` into the map.
///
/// # Errors
///
/// [`MetaError`] when either read fails.
pub(crate) async fn load(state: &AppState) -> Result<(), MetaError>;

/// What one flush wrote.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FlushReport { pub(crate) upserts: usize, pub(crate) deletes: usize }

/// Writes the map's changes to `pending_traces` (D-I-8).
///
/// # Errors
///
/// [`MetaError`] when a write fails. The changes not yet written go back to the map.
pub(crate) async fn flush_once(state: &AppState) -> Result<FlushReport, MetaError>;
```

- **`refresh_rules`.** `timeout_ns` is the project's `completion_timeout_s` when it is in `COMPLETION_TIMEOUT_MIN_S..=COMPLETION_TIMEOUT_MAX_S` (I4.3's constants, `meta::projects`), else `COMPLETION_TIMEOUT_DEFAULT_S`, times `1_000_000_000`. Then `state.pending.set_tracked(<the map's keys>)`.
- **`load`.** `refresh_rules(state).await?`, then `load_pending(state.meta.reader())` mapped to `(project_id, trace_id, Entry { first_seen, last_seen, min_start, root_ended, failed_reads: 0 })`, then `state.pending.load(…)`.
- **`flush_once`.** `changes = state.pending.take_changes()`. Split into parts with the pure helper `flush_parts(changes: Changes, chunk: usize) -> Vec<(Vec<(ProjectId, TraceId)>, Vec<PendingRow>)>`: all deletes first, then all upserts, each part at most `chunk` rows in all (a part may hold the last deletes and the first upserts). Write each part in its own `MetaDb::write` (`write_pending(conn, &deletes, &upserts)`). On the first error, gather that part and every later one back into one `Changes`, call `state.pending.restore(…)`, and return the error. Return the counts.

### Startup and shutdown

- `serve.rs`, right after M3's `crate::eval::recover(&state)` call: `if let Err(error) = crate::eval::trigger::load(&state).await { tracing::warn!(error = %error, "pending traces could not be loaded; tracking starts empty"); }`. Not loading only loses tracking of the traces that were pending, so startup goes on.
- `EvalTiming` gains `flush_every`: `DEFAULT` 5 s, `TEST` 100 ms.
- `eval::spawn` adds the **flusher loop** on M3's pattern: an interval with `MissedTickBehavior::Skip`, `select!` on the stop flag, and a first pass at once.
  - `Ok(report)` with `report != FlushReport::default()` logs `tracing::debug!(upserts = report.upserts, deletes = report.deletes, pending = state.pending.len(), "pending traces flushed")`.
  - `Err(error)` logs `tracing::warn!(error = %error, "pending traces could not be flushed")`.
- **The final flush is in `Running::shutdown` (`serve.rs`), not in `EvalTasks::shutdown`.** Commits whose request was dropped still run after the HTTP drain (D-G-38) and call `observe`, so the last flush must come after them.
  - In the `if let Some(state) = self.state.take()` block, right after the `wait_for_commits` timeout and before the `last_used` flush, add: `if let Err(error) = crate::eval::trigger::flush_once(&state).await { tracing::error!(error = %error, "pending traces flush failed"); }`.
  - The eval loops, the flusher included, are already stopped by then (M3 stops them right after the HTTP drain), so this flush cannot race the flusher.
  - Update the method's doc comment to list the step.
  - `EvalTasks::shutdown` is unchanged.

### Spec

- `docs/spec/02-ingestion.md` § Pipeline, replace the online-rules paragraph ("For each committed span, matching online rules (06) …") with: "After each append, `commit` records the trace in the online-rule trigger's in-memory map when its project has an enabled, unpaused rule: first and last ingest time, earliest span start, and whether a root span was seen (D-I-8). Evaluation and experiment spans are never recorded (D-I-17). Nothing on the ingest path touches `meta.db`: a flusher writes the changed entries every 5 s. Rules are matched later, when the trace completes (06 § Online rules › Triggering, D-I-7)."
- `docs/spec/12-operations.md` § Process lifecycle: in **Startup**, after M3's eval sentence, add "Pending traces are loaded from `pending_traces` before the ports open (D-I-8)." In **Shutdown**, right after the eval-worker text M3 wrote there, add: "then wait for detached ingest commits (D-G-38) and flush the trigger's pending map one last time, so the traces those commits recorded are kept (D-I-8)". The flush belongs after the commits, not with the eval loops.

### Tests (`eval/trigger.rs`, `TestApp`)

- `flush_parts_puts_deletes_first_and_cuts_at_the_chunk`: 3 deletes and 4 upserts with `chunk` 3 → `[(3 deletes, 0 upserts), (0, 3), (0, 1)]`.
- `flush_writes_changes_and_deletes`: track project A (`set_tracked`), `commit_spans` of a rooted trace → `flush_once` → one `pending_traces` row with `root_ended = 1`; `pending.finish` it → `flush_once` → no row.
- `a_second_flush_writes_nothing`: `FlushReport::default()` on the second call.
- `refresh_rules_tracks_the_projects_of_enabled_rules`: a rule in A, a disabled rule in B → tracked `{A}`; the timeout is 60 s, or 120 s after `UPDATE projects SET settings = '{"trace_completion_timeout_s":120}'`, and 60 s for `'{"trace_completion_timeout_s":5}'`.
- `load_restores_tracked_entries_and_deletes_the_rest`: write two rows (A tracked, B untracked) with `write_pending`, `load` → `pending.len() == 1`; `flush_once` → B's row is gone.
- `shutdown_flushes_the_map`, in `serve.rs`'s tests, next to `shutdown_flushes_usage`. It copies that test's setup: `bind(with_init(config(dir.path())), None)` and `running.state`.
  - Create a real rule, so the sweeper's per-tick refresh (I4.7) keeps the project tracked. In one `state.meta.write`, insert a check evaluator and an enabled trace rule in the init project, with M3's `meta::evaluators` and `meta::online_rules` functions. `with_init` creates exactly one project, "Support bot". Read its id and its org's with `SELECT id, org_id FROM projects` on `state.meta.reader()`.
  - Call `crate::eval::trigger::refresh_rules(&state).await.unwrap()`. A sweeper tick of the running server (I4.7) that read the rules before the insert could still apply its stale tracked set after this call. Rule it out: sleep twice the `sweep_every` of the timing `bind` uses (once I4.7 exists; before it, no sweeper runs and one call is enough), then call `refresh_rules` again. Every tick that starts after the insert tracks the project too.
  - Then `state.pending.observe(project, &[span], state.clock.now_ns())` with one unrooted span (`test_support::dataset_span(0)` with `parent_span_id` set). An unrooted trace stays pending for 60 s, so no sweep takes it.
  - Run `running.shutdown().await.unwrap()`, reopen `MetaDb`, and assert that the `pending_traces` row exists.

### Done when

`just check` passes.

---

## I4.6 — The preview API

**Goal:** `POST …/online-rules/preview` counts what a rule's filter matches over a window (D-I-9), with its contract, fixtures and mock.

**Edit only:** `crates/server/src/api/online_rules.rs`, `crates/server/src/api/openapi.rs`, `crates/server/src/api_examples.rs`, `crates/server/tests/openapi.rs`, `crates/server/tests/api_examples.rs`, `crates/server/src/routes/online_rules.rs`, `crates/server/openapi.json`, `fixtures/api/static/online-rule-preview.json` (new), `fixtures/api/requests/online-rule-preview.json` (new), `fixtures/api/manifest.json`, `web/src/api/schema.gen.ts`, `docs/spec/06-evaluation.md`, `docs/spec/09-api.md`. **Not** `web/src/mocks/writes.ts`. Its test, `writes.test.ts`, only covers write paths under a fixed list of prefixes, which does not include online rules, so adding the preview there would fail it. Web tests mock the preview with `server.use` (I4.8).

**Commit:** `Add the online rule preview`

### Types (`api/online_rules.rs`)

```rust
/// `POST /online-rules/preview`: what a filter matches (D-I-9).
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct OnlineRulePreviewRequest {
    /// The rule's target: the view the filter is read on.
    pub target: RuleTarget,
    /// Filter DSL over the target's view. Absent or blank matches everything.
    #[serde(default)]
    pub filter: Option<String>,
    /// Window start. Default: `to` minus 24 hours.
    #[serde(default)]
    pub from: Option<TimeParam>,
    /// Window end. Default: now.
    #[serde(default)]
    pub to: Option<TimeParam>,
}

/// The preview's count.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct OnlineRulePreview {
    /// Matching traces or spans, at most 10 000.
    pub matched: u64,
    /// More than 10 000 matched.
    pub capped: bool,
    /// The window that was counted. RFC 3339.
    #[serde(with = "llmobs_core::time_serde")] #[schema(value_type = String, format = DateTime)]
    pub from: i64,
    #[serde(with = "llmobs_core::time_serde")] #[schema(value_type = String, format = DateTime)]
    pub to: i64,
}
```

`OnlineRulePreviewRequest` is `Deserialize` only because `TimeParam` has no `Serialize` (as M3's `RunEvaluatorRequest`). Read how `RunEvaluatorRequest` declares `TimeParam` in its schema and do the same.

### Route (`routes/online_rules.rs`)

| Method | Path | Perm | Handler |
|---|---|---|---|
| POST | `/api/v1/projects/{project}/online-rules/preview` | `ManageEvaluation` | `preview_online_rules` |

Add it to the module's route list (`Access::Project`, `session_only: false`, `documented: true`), so `http/access.rs` needs no change. A static segment wins over `{id}` in axum's router, and no `POST /online-rules/{id}` exists, so the paths do not clash. Because `/online-rules/preview` is registered with POST only, a `GET` on it answers `405`: it never falls through to `{id}`. The test `preview_is_not_a_rule_id` pins both.

The handler:
1. `now = state.clock.now_ns()`; `to = body.to.map_or(now, TimeParam::ns)`; `from = body.from.map_or(to − 24 h, TimeParam::ns)`. Collect field errors: `from >= to` → `from`: `from must be before to`; `to − from > 30 days` → `from`: `the window is at most 30 days`; a filter over 4 096 bytes → `filter`: `filter must be at most 4096 bytes` (D-I-30's limit). Non-empty → `400`.
2. `ctx = open_query(&state, project, None).await?` (`routes/read.rs`).
3. `count = llmobs_storage::match_count(&ctx, MatchCountRequest { view, filter: body.filter.as_deref(), window: TimeWindow::new(from, to)?, cap: MAX_MATCH_COUNT, strict_exclusions: true, timeout: PREVIEW_TIMEOUT }).await?` with `view` from the target (`Traces` / `Spans`) and `const PREVIEW_TIMEOUT: Duration = Duration::from_secs(10);`. `?` maps `RunError::Invalid` to the `invalid_filter` problem on field `filter`, a timeout to `422 query_timeout`, and `QueryError::Busy` to `503`, as the read routes do.
4. `200` with `matched = min(count, MAX_MATCH_COUNT)`, `capped = count > MAX_MATCH_COUNT`, `from`, `to`.

There is no per-user limit: a preview is a bounded read (10 s, the query memory pool), like a trace list. Log nothing but a `debug` with the project id, the target and the count.

### OpenAPI

`online_rule_paths` gains the operation:

| operation_id | Method and path | Request | 2xx | Errors | Description |
|---|---|---|---|---|---|
| `post_online_rule_preview` | `POST {PROJECT}/online-rules/preview` | `OnlineRulePreviewRequest` | `200 OnlineRulePreview` | `READ_ERRORS` | "Project members and above. API keys cannot call it. Counts the traces or spans the filter matches in the window (default: the last 24 hours, at most 30 days), at most 10 000, leaving out evaluation and experiment traces whatever the filter says (D-I-9)." |

Register both schemas. `tests/openapi.rs` (M3 § Conventions): add the tuple to `OPERATIONS`; add `post_online_rule_preview` to `QUERY_OR_BODY`; in `expected_errors`, a branch `*id == "post_online_rule_preview"` → `{"400", "401", "403", "404", "422", "429"}` before the `READ_OPERATIONS` branch (if the read branch already gives that set, add the id to that branch instead and say so).

### Fixtures and mock

- `static/online-rule-preview.json`: § Fixed identifiers' answer.
- `requests/online-rule-preview.json`: § Fixed identifiers' request.
- `api_examples.rs`: `post_online_rule_preview` → `online-rule-preview.json` (status 200). `tests/api_examples.rs`: round-trip the response; deserialize the request file only; add it to the request ↔ operation list. `just api-examples`.
- No dev-mode mock. `manifest.json` POST entries need no handler (the manifest test accepts them). Under `just web-dev` the preview has no mock, so the counter shows its load error there. That is accepted.

### Spec

- `docs/spec/06-evaluation.md` § Online rules, after M3's validation paragraph: "`POST /online-rules/preview` `{target, filter?, from?, to?}` counts what a filter matches in a window (the last 24 hours by default, at most 30 days), at most 10 000, with strict exclusions; the editor shows the count as the filter is typed (D-I-9)."
- `docs/spec/09-api.md` § Evaluation & alerts: add `POST /online-rules/preview` `{target, filter?, from?, to?}` → `{matched, capped, from, to}`.

### Tests (`routes/online_rules.rs`, over HTTP; commit traces with `app.commit_spans`)

- `preview_counts_the_last_24_hours`: two ok traces in the last day, one older (two days ago), one error trace → `{target: "trace", filter: "status = 'ok'"}` gives `matched 2`, `capped false`, `to` = now, `from` = now − 24 h. Set the manual clock so that "now" is after the dataset's span times (read their `start_time` in `fixtures/api/dataset/spans.json` and pick `now` one hour after the latest).
- `preview_counts_spans_for_a_span_target`.
- `preview_leaves_out_evaluation_traces`: an `llmobs-eval` trace with filter `environment = 'llmobs-eval'` → 0.
- `preview_validates_the_window`: `from` after `to`; a 31-day window; a 4 097-byte filter → 400 on their fields.
- `an_invalid_filter_is_400_with_position`.
- `viewer_and_api_key_cannot_preview` (403).
- `preview_is_not_a_rule_id`: `GET …/online-rules/preview` answers `405 Method Not Allowed`, not the rule route, and `POST …/online-rules/preview` reaches the preview (`200`).

The `capped` case is covered by storage's cap test (I4.1); committing 10 001 traces here would be slow.

### Done when

`just check` passes; `openapi.json` has the operation.

---

## I4.7 — The sweeper

**Goal:** `sweep_once` completes traces, matches each rule through storage, samples, caps, and enqueues rule jobs (§ Completion and sweeping in one place). It runs every 2 s. An in-process end-to-end test takes a committed trace to a rule's score.

**Edit only:** `crates/server/src/eval/trigger.rs`, `crates/server/src/eval/mod.rs` (`EvalTiming.sweep_every`, the sweeper loop), `crates/server/src/eval/hourly.rs`, `crates/server/src/eval/sampling.rs`, `crates/server/src/eval/pending.rs`, `crates/server/src/meta/eval_jobs.rs` and `crates/server/src/meta/online_rules.rs` (remove the I4.7 `dead_code` attributes of § Conventions › Dead-code allows in M4, nothing else), `crates/server/src/api/convert.rs` (one converter), `docs/spec/06-evaluation.md`, `docs/spec/12-operations.md`.

**Commit:** `Enqueue rule jobs when traces complete`

### `api/convert.rs`

```rust
/// One matched row of a trigger read: the trace, the span for a span rule, and the row's `start_time`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MatchRow { pub(crate) trace_id: TraceId, pub(crate) span_id: Option<SpanId>, pub(crate) start_time: i64 }

/// The rows of `trace_refresh` (`with_span: false`: columns `trace_id`, `start_time`) or
/// `span_refresh` (`with_span: true`: `trace_id`, `span_id`, `start_time`), in row order.
pub(crate) fn match_rows(batches: &[RecordBatch], with_span: bool) -> Result<Vec<MatchRow>, ConvertError>;
```

Use the module's private helpers (`strings` for the hex ids, `times` for `start_time`) and parse the ids the way `trace_batch` does. A null id or time is a `ConvertError`, like the other converters'. Tests next to the others: a trace batch and a span batch built with the `trace_refresh`/`span_refresh` columns.

### `sweep_once` (`eval/trigger.rs`)

```rust
/// Most completed traces taken per project per tick (D-I-9).
const PER_PROJECT_TICK: usize = 2_000;
/// Most traces in one batch: one `trace_refresh` or `span_refresh` call.
const BATCH: usize = llmobs_storage::MAX_REFRESH_IDS;
/// Widest `min_start` spread in one batch, so `around ± 1 day` covers it (D-I-9).
const BATCH_SPREAD_NS: i64 = 12 * 3_600_000_000_000;
/// Span rules keep this many sampled spans per trace (D-I-10).
pub(crate) const MAX_SAMPLED_SPANS: usize = 50;
/// Time allowed for one rule's read.
const READ_TIMEOUT: Duration = Duration::from_secs(10);
/// Most jobs inserted per write.
const ENQUEUE_CHUNK: usize = 1_000;

/// What one sweep did. Logged at `debug`.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SweepReport {
    pub(crate) completed: usize,      // traces taken
    pub(crate) matched: usize,        // rows the reads returned
    pub(crate) sampled: usize,        // targets kept by sampling (and the 50-span cut)
    pub(crate) capped: usize,         // targets refused by the hourly cap
    pub(crate) enqueued: usize,       // rows inserted
    pub(crate) failed_batches: usize,
    pub(crate) dropped: usize,        // traces dropped after 5 failed reads
}

/// One sweep (§ Completion and sweeping in one place).
///
/// # Errors
///
/// [`MetaError`] only when the rules cannot be read; a failed batch is counted, not returned.
pub(crate) async fn sweep_once(state: &AppState, now: i64) -> Result<SweepReport, MetaError>;

/// Cuts completed traces (sorted by `min_start`) into batches with their `around` (D-I-9).
fn batches(completed: &[Completed]) -> Vec<(i64, &[Completed])>;
```

`sweep_once`, step by step:
1. `rules = refresh_rules(state).await?` (I4.5; this also updates the tracked set).
2. For each `(project, project_rules)`: `completed = state.pending.completed(project, now, project_rules.timeout_ns, PER_PROJECT_TICK)`; skip when empty. For each `(around, batch)` of `batches(&completed)`: `let outcome = sweep_batch(state, project, &project_rules.rules, around, batch, now, &mut report).await;` then `settle(state, project, batch, outcome, &mut report)`.

```rust
/// Ends one batch: `Ok` removes its traces that did not grow; `Err` counts a failed read for
/// each and drops those at `MAX_FAILED_READS`. Returns the traces dropped.
fn settle(state: &AppState, project: ProjectId, batch: &[Completed], outcome: Result<(), ()>, report: &mut SweepReport) -> usize
```

   - `Ok(())` → `state.pending.finish(project, batch)`; return 0.
   - `Err(())` → `report.failed_batches += 1`; `dropped = state.pending.read_failed(project, batch)`; when `dropped > 0`, `tracing::warn!(project_id = %project, dropped, "pending traces dropped after 5 failed reads")` and `report.dropped += dropped`; return `dropped`.

   `settle` takes `&AppState` only for `state.pending`; its unit test builds a `TestApp`.
3. `report.completed` is the sum of the `completed` lengths. When `report.enqueued > 0`, `state.eval.wake()`. Return the report.

`sweep_batch(…) -> Result<(), ()>`. The error has already been logged inside; `()` keeps the signature honest about that. It has three steps in this order: read, then admit, then enqueue. Only the reads can fail before any hourly-cap unit is spent.
1. **Read.** `let candidates = read_candidates(state, project, rules, around, batch).await?;`
2. **Admit.** `report.matched += candidates.matched; report.sampled += candidates.targets.len();` then `let jobs = admit_targets(&state.eval.hourly, candidates.targets, now, report);`.
3. **Enqueue.** For each chunk of `ENQUEUE_CHUNK` jobs, run one `MetaDb::write`. Its closure owns the chunk and calls `insert_rule_job` for each job; count the `true`s into `report.enqueued`. On a write error, log `error!(project_id, error = %error, "rule jobs could not be queued")` and return `Err(())`. The units the batch's admitted targets took stay spent; the retry admits them again. A write error is rare, so this is accepted.

```rust
/// What the reads of one batch found, before the hourly cap.
#[derive(Debug, Default)]
struct Candidates<'a> {
    /// Rows the reads returned, over every rule.
    matched: usize,
    /// Sampled targets in rule order: the rule they belong to, then the row.
    targets: Vec<(&'a TriggerRule, MatchRow)>,
}

/// Every rule's read of one batch and the sampling; no hourly cap, no report.
async fn read_candidates<'a>(state: &AppState, project: ProjectId, rules: &'a [TriggerRule], around: i64, batch: &[Completed]) -> Result<Candidates<'a>, ()>;

/// Asks the hourly cap for each candidate, in order, and builds the jobs of the admitted ones.
fn admit_targets(hourly: &HourlyCaps, targets: Vec<(&TriggerRule, MatchRow)>, now: i64, report: &mut SweepReport) -> Vec<NewJob>;
```

`read_candidates` takes neither `&HourlyCaps` nor `&mut SweepReport`. That signature is what guarantees a failed read spends no cap unit and counts nothing.
1. `source = state.storage.query(project, QueryOptions { around: Some(around) }).await` (the one field `QueryOptions` has; § Completion and sweeping in one place); `ctx = QueryContext::new(source, None)`, as `routes/read.rs` `open_query_on` does. On any error, log `warn!(project_id, "a trigger read could not start")` and return `Err(())`.
2. `ids: Vec<String>` is the batch's trace ids as lowercase hex (`TraceId::to_string`), then `refs: Vec<&str>`.
3. For each rule:
   - `"trace"` → `llmobs_storage::trace_refresh(&ctx, TraceRefreshRequest { filter: rule.filter.as_deref(), search: None, trace_ids: &refs, include_eval: false, include_experiments: false, strict_exclusions: true, timeout: READ_TIMEOUT })`, then `match_rows(&batches, false)` (strict, so the batches have exactly `TRACE_REFRESH_STRICT_COLUMNS`, I4.1).
   - `"span"` → `llmobs_storage::span_refresh(&ctx, SpanRefreshRequest { filter: rule.filter.as_deref(), trace_ids: &refs, per_trace: MAX_REFRESH_SPANS_PER_TRACE, strict_exclusions: true, timeout: READ_TIMEOUT })`, then `match_rows(&batches, true)`.
   - Errors follow § Error outcomes of a batch read. A skipped rule goes on to the next rule. A batch failure logs `warn!(project_id, rule_id, kind = error.kind(), "a trigger read failed")` and returns `Err(())`.
   - Add `rows.len()` to `matched`, then push the rule's sampled rows:
     - **trace rule:** each row with `sampling::sampled(rule.sample_rate, rule.id, row.trace_id, None)`.
     - **span rule:** group consecutive rows by `trace_id`; `span_refresh` returns them grouped and ordered by `start_time`. In each group, keep the rows with `sampled(rule.sample_rate, rule.id, row.trace_id, row.span_id)`, then the first `MAX_SAMPLED_SPANS`.

`admit_targets`: for each `(rule, row)`, when `hourly.admit(rule.id, now)` is false, add 1 to `report.capped` and skip it. Otherwise push `NewJob { id: Uuid::now_v7(), project_id: rule.project_id, kind: KIND_RULE, rule_id: Some(rule.id), evaluator_id: rule.evaluator_id, variable_mapping: None, trace_id: row.trace_id, span_id: row.span_id, trace_start_ns: row.start_time }`.

A target that is already queued (a trace that completed twice) still takes one unit of the hourly cap before its insert does nothing. This is accepted: the cap guards against runaway loops, not exact counts.

Remove the I4.7 rows of § Conventions › Dead-code allows in M4, `HourlyCaps::admit` included.

### The loop

`EvalTiming` gains `sweep_every`: `DEFAULT` 2 s, `TEST` 50 ms. `eval::spawn` adds the **sweeper loop** on M3's pattern: one pass at once, then on the interval; a tick awaits `sweep_once` to the end before the next (`MissedTickBehavior::Skip`), so ticks never overlap; `Ok(report)`, when `report != SweepReport::default()`, is logged at `debug` with one named field per report field (`completed = report.completed, matched = report.matched, sampled = …, capped = …, enqueued = …, failed_batches = …, dropped = …`), never `?report` (§ Conventions). `Err` is logged at `error` with the error.

### Spec

- `docs/spec/06-evaluation.md` § Online rules › Triggering, replace the section (both bullets on span and trace rules, and the skip bullet) with: "Trace and span rules fire when the trace is complete: its root span has ended and 5 s passed without a new span, or `trace_completion_timeout_s` (project setting, 10–3600 s, default 60) passed without one (D-I-7). `commit` records traces in memory for projects with an enabled, unpaused rule; evaluation and experiment spans are never recorded; a flusher persists the map every 5 s (D-I-8). Every 2 s a sweeper reads the enabled rules, takes each project's completed traces in batches of at most 500 (at most 2 000 per project per tick), and reads each rule's matches through storage (`trace_refresh` or `span_refresh`, at most 200 spans per trace) with strict exclusions, so a filter cannot reach evaluation or experiment traces (D-I-9, D-I-17). A read that fails is retried at the next tick; a trace is dropped after 5 failed ticks. Traces that complete while a rule is disabled, paused, or not yet created are not evaluated by it; backfill is the catch-up." Replace the **Sampling** paragraph with D-I-10's rule (SHA-256, the tag, the 50 earliest sampled spans), and add: "Live enqueues are capped at 10 000 per rule per UTC hour; extra matches are dropped and shown as `dropped_last_hour` (D-I-11)."
- `docs/spec/12-operations.md` § Process lifecycle, **Startup**: the eval loop list gains "the trigger's sweeper and flusher".

### Tests (`eval/trigger.rs`, `TestApp`)

**Helpers:**
- A project with a check evaluator. No provider is needed: the sweeper only enqueues.
- `rule(app, target, filter, rate) -> Uuid` inserts with M3's `insert_rule` in a write, **then calls `trigger::refresh_rules(&app.state).await.unwrap()`**. `commit` only records traces of projects already tracked, so without that call the first trace would never enter the map.
- Every test creates its rules before committing its traces. A test that changes a rule by SQL (filter, enabled) calls `refresh_rules` again before committing.
-  `trace(app, project, id, rooted, start) ` commits spans with `app.commit_spans` (copy `dataset_span` and change ids).
- `jobs(app) -> Vec<JobRow>` reads every row by SQL.

**Clock:** use the manual clock, and never compute a later `now` by hand.
- Set it so the spans' `start_time` lies within a day of it (as I4.6's tests do) **and** it sits at 30 minutes past a UTC hour: `now - now.rem_euclid(3_600_000_000_000) + 1_800_000_000_000`. Then no test crosses an hour boundary.
- Commit, then `app.state.clock.advance(Duration::from_secs(6))`, then `sweep_once(&app.state, app.state.clock.now_ns())`.
- Every `admit`, `sweep_once` and HTTP read in a test uses the clock's current value, so they all see the same hour.

- `batches_cut_at_500_and_12_hours` (pure): 1 200 entries at one `min_start` → 500, 500, 200; entries at 0 h, 6 h, 13 h → two batches with `around` 3 h and 13 h.
- `a_completed_trace_enqueues_a_trace_rule_job`: one rooted trace, a trace rule at rate 1 → after the sweep one `queued` job, `kind rule`, `priority 1`, `dedup_key rule:<rule>:<trace>:-`, `trace_start_ns` = the trace's start; the map is empty; `report.enqueued == 1`.
- `an_unrooted_trace_waits_for_the_timeout`: no root → nothing at +6 s; a job at +61 s. With the project timeout set to 10 s by `PATCH` (I4.3), a job at +11 s.
- `the_filter_selects_traces`: filter `status = 'error'`, one ok and one error trace → one job; both traces leave the map.
- `a_span_rule_enqueues_matching_spans`: three `llm` spans and one `chain` span, filter `span_kind = 'llm'` → three jobs with their `span_id`s and `start_time`s.
- `a_span_rule_keeps_the_50_earliest_sampled_spans`: 60 matching spans at rate 1 → 50 jobs, the earliest.
- `sampling_selects_the_same_targets_as_sampled`: 20 traces at rate 0.5 → the job set equals the traces for which `sampling::sampled` is true.
- `evaluation_traces_are_never_enqueued`: an `llmobs-eval` trace with a rule filtered on `environment = 'llmobs-eval'` → no entry in the map, no job (and if one is forced into the map with `pending.load`, the strict read still returns nothing).
- `the_hourly_cap_drops_and_counts`: advance the clock 6 s, then call `state.eval.hourly.admit(rule, state.clock.now_ns())` 10 000 times. Sweep one trace at the same clock value → no job, `report.capped == 1`, and `GET /online-rules/{id}` answers `dropped_last_hour: 1`.
- `admit_targets_caps_in_order` (pure, on a fresh `HourlyCaps`): call `admit` 9 999 times for rule A, then `admit_targets` with three candidates of A and one of B → two jobs (A's first and B's), `report.capped == 2`.

That a failed read spends no cap unit is guaranteed by the types: `read_candidates` takes no `HourlyCaps` and no report. There is no separate test for it. Do not add a test-only failure hook to storage.

- `a_trace_completed_twice_gets_one_job`: sweep, commit another span of the same trace, sweep again → still one job.
- `a_rule_with_an_unreadable_filter_is_skipped`: rule A's filter set by SQL to `status = = 'x'`, rule B fine → B's job is queued, the trace leaves the map, and the log has the skip warning (`capture_logs`, plain `#[tokio::test]`).
- `a_failed_batch_keeps_its_traces`: a storage read cannot be made to fail reliably in a test, so test `settle` (step 2) directly: `Err` four times keeps the trace and counts `failed_batches == 4`, the fifth drops it, returns 1 and counts `dropped == 1`; `Ok` finishes it.
- `a_deleted_rule_during_the_sweep_enqueues_nothing`: call the enqueue helper with a `NewJob` of a deleted rule → `Ok`, 0 inserted.
- `untracked_projects_are_not_read`: a project without rules → its trace never enters the map.
- **End to end:** `a_trace_rule_runs_from_commit_to_score` (`#[tokio::test(flavor = "multi_thread")]`, M3 § Conventions' test clock: set the manual clock to the real time first): a judge evaluator on `MockLlm` (as M3's `execute.rs` tests), a trace rule created over HTTP (`POST …/online-rules`, rate 1), then `trigger::refresh_rules(&app.state).await.unwrap()` so the project is tracked before the commit (the sweeper's first pass would otherwise race it), `crate::eval::spawn(&app.state, EvalTiming::TEST)`, `commit_spans` of a rooted trace, then `app.state.clock.advance(Duration::from_secs(6))`; poll `GET …/eval-jobs?rule_id=` every 50 ms for at most 10 s until one job is `done`; `GET /scores?rule_id=` has one `llm_judge` score on the trace; finally `EvalTasks::shutdown`. Run it five times in a row (`for i in 1 2 3 4 5; do cargo test -p llmobs-server a_trace_rule_runs_from_commit_to_score || break; done`); a flaky run must be fixed, not retried.

### Done when

`just check` passes, and the end-to-end test passes five runs in a row. `HourlyCaps::admit` and `span_refresh` have production callers; no I4.x `dead_code` attribute is left (`rg -n "I4\.[0-9] (calls|wires|reads) it|M4's sweeper calls it" crates/server` is empty).

---

## I4.8 — Web: the match counter, the timeout setting, specs and roadmap

**Goal:** the rule editor shows how many traces or spans its filter matched in the last 24 hours; the project's general settings set the trace completion timeout; spec 11 and the roadmap describe M4.

**Edit only:** `web/src/components/online-rules/match-counter.tsx` and `.test.tsx` (new), `web/src/components/online-rules/rule-form.tsx` (render the counter), `web/src/test/online-rule-preview.ts` (new: the default preview handler), `web/src/test/setup.ts` (install it in `beforeEach`), `web/src/routes/$org/$project/online-rules/new.test.tsx` and `$ruleId/edit.test.tsx` (new cases only), `web/src/routes/$org/$project/settings/general.tsx` and `.test.tsx`, `docs/spec/11-ui.md`, `docs/roadmap.md` (the I row), `docs/plans/i-online-eval/00-design.md` (only to check that this plan's four design changes are there; if they are not, add them as written in § Decisions this plan relies on and say so in the report).

**Commit:** `Show rule match counts and the trace completion timeout`

### `match-counter.tsx`

`MatchCounter({ org, project, target, filter, sampleRate }: { org: string; project: string; target: "trace" | "span"; filter: string; sampleRate: number })`. Model it on K's `components/alerts/rule-preview.tsx` (read it first):
- debounce the trimmed filter by 500 ms (`useState` + `useEffect` with `setTimeout`, as `RulePreview` does);
- `$api.useQuery("post", "/api/v1/projects/{project}/online-rules/preview", { params: { path: { project: projectRef(org, project) } }, body: { target, ...(filter ? { filter } : {}) } }, { staleTime: 60_000, placeholderData: keepPreviousData })`;
- loading (no data yet): "Counting matches…" with a small `Skeleton`;
- data: "{n} traces matched in the last 24 hours" (`spans` for a span target; "1 trace" singular; numbers with `toLocaleString("en-US")`), or "More than 10,000 traces matched in the last 24 hours" when `capped`. When `0 < sampleRate < 1` and `matched > 0`: append " · about {Math.round(matched × sampleRate)} would be evaluated at {sampleRate × 100} %";
- error: an `invalid_filter` problem → `FormError` "The filter is invalid: {filterMessage(problem)}" (as `RulePreview`); any other → "The match count could not be loaded.";
- the element has `role="status"` and `aria-live="polite"`.

`rule-form.tsx`: render `<MatchCounter … />` right under the filter field, for members only (the preview needs `ManageEvaluation`; viewers see the read-only form without it). Pass the form's current target, filter and sample rate (as a fraction).

### The default preview handler (`web/src/test/online-rule-preview.ts`)

`web/src/test/setup.ts` throws "Unhandled requests" after any test that made an unmocked request, and I4.6 adds no dev-mode mock. Every test that mounts `RuleForm` as a member (M3's `new.test.tsx` and `$ruleId/edit.test.tsx`, and the rule-form tests) would now send an unmocked `POST …/online-rules/preview`. So:
- `online-rule-preview.ts` exports `onlineRulePreviewHandler()`: `http.post("*/api/v1/projects/:project/online-rules/preview", () => HttpResponse.json(<fixtures/api/static/online-rule-preview.json>))`. Import the fixture by relative path, as `web/src/mocks/saved-views.ts` imports `fixtures/api/static/me.json`.
- `setup.ts`'s `beforeEach` adds it: `server.use(...savedViewHandlers(seedSavedViews()), onlineRulePreviewHandler());`.
- A test that needs another answer overrides it with its own `server.use(http.post(…))`, which takes precedence (MSW checks the latest handlers first).

M3's existing tests then pass unchanged.

### General settings (`settings/general.tsx`)

A second section under the existing form, "Online evaluation", with its own `<form aria-label="Online evaluation">` and a submit button named **"Save timeout"** (admins only, like the rest of the page; the same `DisabledReason`). Not "Save": `settings/general.test.tsx` finds the first form's button with `getByRole("button", { name: "Save" })` about seven times, and a second "Save" would make those queries ambiguous. Testing Library matches a string name exactly, so "Save timeout" does not collide:
- a number input labelled "Trace completion timeout (seconds)", initial value `data.trace_completion_timeout_s`, `min 10`, `max 3600`, step 1;
- help: "Online rules evaluate a trace when its root span has ended and 5 seconds pass without a new span, or after this many seconds without a new span. 10 to 3600; default 60.";
- Save sends `PATCH` with `{ trace_completion_timeout_s }` only; a value outside 10–3600 or not an integer shows "Enter a whole number from 10 to 3600." under the field and sends nothing; a `400` with a field error shows it under the field; success shows the existing `FormSuccess` pattern and invalidates the project query (`projectQuery` key, as the first form does).

### Spec and roadmap

- `docs/spec/11-ui.md` § Evaluators & online rules, in M3's **Online rules** bullet, replace "The filter's live match counter (M4) and the backfill dialog (M6) come later." with "Under the filter, the editor shows how many traces or spans it matched in the last 24 hours (evaluation and experiment traces left out, at most 10 000) and how many the sample rate would keep (D-I-9). The backfill dialog (M6) comes later." § Settings, project **General**: add "the trace completion timeout used by online rules (10–3600 s, default 60; D-I-7)".
- `docs/roadmap.md`, the I row's status: "… M4 (trace completion trigger, sampling, preview) implemented on branch `<your branch>`. M5–M6 to follow." Read the row first and keep what is still true.

### Tests

`match-counter.test.tsx` (render `RuleForm` through `renderApp` on the new-rule page; override the preview with `server.use(http.post(…/online-rules/preview, …))`):
- `shows the match count`: the fixture → "1,234 traces matched in the last 24 hours".
- `shows the sampled estimate`: sample 25 → "about 309 would be evaluated at 25 %".
- `debounces the filter`: type `status = 'error'`; the requests' bodies contain the full filter once, not one request per key (as K's `debounces the filter in the preview`).
- `says more than 10,000 when capped`.
- `counts spans for a span rule`.
- `shows an invalid filter`: a `400 invalid_filter` → "The filter is invalid: …".
- `shows a load error`: a `503` → "The match count could not be loaded."

`online-rules/new.test.tsx`: the existing tests still pass with the default handler (the counter must not break the exact `POST` body assertion: the preview is a different request). `$ruleId/edit.test.tsx`: `a viewer sees no counter` (no `status` element with "matched").

`settings/general.test.tsx`: the existing tests pass unchanged (their `name: "Save"` queries still find one button). `saves the trace completion timeout` (click "Save timeout"; the `PATCH` body is exactly `{trace_completion_timeout_s: 120}`); `refuses an out-of-range timeout` (5 → the message, no request); `shows a server field error`; `a non-admin cannot change it`.

### Done when

`just check-web` passes; then `just check`. Optional: `just web-dev` shows the counter on the mock data; say in the report whether you looked.

---

## After M4

Report: the eight commits, the migration number you used, deviations, and anything unverified. `just check` is green on the merged branch, the end-to-end test of I4.7 passes five runs in a row there, and the I row of the roadmap says M4 is implemented.

## Open questions (with the defaults this plan uses)

1. **Rule and settings caches** (D-I-8, D-I-9). Default: none; one reader query per sweep tick, and the tracked set follows within 2 s (§ Decisions, change 1). The alternative, rebuilding on every rule write, pause and resume, needs hooks in six M3 code paths and is easy to miss in one.
2. **Preview rate limit.** Default: none; the read is bounded by its 10 s timeout and the query memory pool, the editor debounces 500 ms, and the route needs `ManageEvaluation`. If abuse shows up, the per-user limiter of test runs could be reused at a higher rate.
3. **Unsetting the timeout.** Default: no unset; `PATCH` with 60 gives the default behaviour. `settings` keeps the key.
4. **Hourly cap and re-completed traces.** Default: a target that is already queued still takes one unit of its rule's hourly cap. Exact counting would need the insert result before the cap decision, inside the write.
5. **A pending-trace load failure at startup.** Default: a `warn`, and tracking starts empty; the server still starts.
6. **Who sets the timeout.** Default: whoever may `PATCH` the project today (`ManageSettings`, project admins), not `ManageEvaluation`, because it is a project setting on the general page.
