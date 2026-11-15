# M8 — Evaluation cascade

Read [README.md](README.md) first, especially § Design › Evaluation traces and Cascades, and § Coordination (I).

**Prerequisite — check before anything else.** Track I's M3 ([`../i-online-eval/03-m3-queue-rules.md`](../i-online-eval/03-m3-queue-rules.md): the job queue, the executor and manual runs) must be merged:
- some file in `crates/server/migrations/meta/` contains `CREATE TABLE eval_jobs` (`grep -l 'CREATE TABLE eval_jobs' crates/server/migrations/meta/`);
- `crates/server/src/meta/eval_jobs.rs` exists and `grep -n 'enum Finish' crates/server/src/meta/eval_jobs.rs` finds the job outcome type (I3.2);
- `grep -n 'fn run_job' crates/server/src/eval/execute.rs` finds the job runner (I3.6);
- `crates/server/src/routes/eval_jobs.rs` exists and holds the manual-run handler (I3.9).

If any is missing, stop: commit nothing, and report "M8 waits for track I's M3".

**Goal:** a deletion reaches the evaluation side of `meta.db` and I's executor:
1. **Cascade.** In the transaction that stores a resolved scope (M3's `resolve`), delete the `eval_jobs` rows of the scope's traces that are not `running`.
2. **Guard.** I's job runner stops a job about a guarded trace at its start (trace-id entries) and once its target is loaded, before anything is reserved or sent, and again before it commits anything; I's test run and manual run refuse a guarded trace. The guard semantics are `DeletionGuard::blocking` (D-M-6): trace-id entries always block; a user entry blocks a trace that holds data of that user ingested at or before that deletion's cut-off.
3. **Late copies.** A judge trace or a score committed after a deletion's cut-off is not covered by that deletion in storage (it only covers `ingest_time <= cut-off`). So the runner checks the guard once more **after** each commit, and when it blocks then, asks for a follow-up `trace_ids` deletion of the judge trace and the target, with a cut-off of now. Because the guard learns every deletion before its cut-off is read (`routes/deletions.rs` fills it once before reading `now` and again with `now`; D-M-6), a copy stamped after a cut-off is always seen by the check that follows its commit, except for a `filter` row's traces that arrived between its matching read and its cut-off (§ Late copies).

**Edit only:**
- `crates/server/src/deletion/resolve.rs` (the cascade, below);
- `crates/server/src/deletion/guard.rs` (drop the `dead_code` allowance);
- `crates/server/src/deletion/mod.rs` (one function, `follow_up`, below);
- `crates/server/src/meta/deletions.rs` (one function, `insert_follow_up`, below);
- `crates/server/src/meta/eval_jobs.rs`: two functions (`jobs_for_traces`, `delete_jobs`), one `Finish` variant (`Deleted`) and its arm in `finish`;
- `crates/server/src/eval/execute.rs` (`run_job` and the code `finish` is called from), `crates/server/src/eval/test_run.rs`, `crates/server/src/routes/eval_jobs.rs` (`run_evaluator`) — only at the points named;
- `docs/spec/03-storage.md` § Cascades (the two "not built yet" sentences), `docs/spec/06-evaluation.md` (one sentence, below), `docs/roadmap.md` (row M).

No API type changes: a deleted job is `status: skipped` with `error: "deleted"`, and `error` is free text in `EvalJob` (I3.5). `error_class` stays `NULL`, as for I's other skips (`budget`, `expired`, `project unavailable`). `just openapi` must leave `openapi.json` and `schema.gen.ts` unchanged.

**Commit:** `Cascade deletions to evaluation jobs`

## Read first

- M3's `deletion/resolve.rs` (the scope transaction) and `deletion/guard.rs`: `blocking(project, trace, users: &[(&str, i64)], now) -> Option<Uuid>` returns the id of the deletion that blocks; `users` pairs each user id of the loaded spans with the smallest `ingest_time` of that user's spans. `add_traces(project, traces, deletion, created_at)`, `add_user(project, user, deletion, created_at)`. Entries count for 24 h (`GUARD_NS`) after their deletion's `created_at` on the server clock.
- `routes/deletions.rs` `create_deletion`: after the filter's read it fills the guard (`fill_guard`), then reads the cut-off (`now`), then fills it again with `now`, all before it stores the row, for `trace_ids`, `filter` (the matched ids) and `user` rows. § Late copies relies on that order; M8 does not edit this file.
- `meta/deletions.rs`: `deletion_by_id`, `insert_deletion`, `insert_traces`, `NewDeletion`, `RequestDetails`.
- I's M3 plan, [`03-m3-queue-rules.md`](../i-online-eval/03-m3-queue-rules.md): § Job execution (the 16 steps of `run_job`, the dispositions, the breaker's `reached_evaluator`), § I3.2 › `meta/eval_jobs.rs` (`JobRow`, `JobUsage`, `Finish`, `finish`, `insert_job`, `claim`), § I3.6 (`finish` and the budget reservation guard, the `seeded` / `rule_job` / `job` test helpers), § I3.9 (`run_evaluator`).
- I's code as merged: `meta/eval_jobs.rs`, `eval/execute.rs`, `eval/test_run.rs`, `routes/eval_jobs.rs`. Where the code and the plan differ, follow the code and list the difference in your report.
- `llmobs_core::span::Span`: `user_id: Option<String>`, `ingest_time: i64`. `TraceId::from_slice` (`crates/core/src/ids.rs`).

## Cascade

`crates/server/src/meta/eval_jobs.rs`, next to I's other functions:

```rust
/// The ids of the jobs of `project` whose `trace_id` is in `traces`, except `running` ones
/// (D-M-6: those are stopped by the deletion guard). Runs on the reader pool.
pub(crate) async fn jobs_for_traces(pool: &SqlitePool, project: ProjectId, traces: &[TraceId]) -> Result<Vec<Uuid>, MetaError>;

/// Deletes the jobs `ids`, in chunks of 500, and returns the rows deleted. A job that is
/// `running` by now is left alone (`AND status <> 'running'`).
pub(crate) async fn delete_jobs(conn: &mut SqliteConnection, ids: &[Uuid]) -> Result<u64, MetaError>;
```

`eval_jobs` has an index on `(project_id, created_at)` but none on `trace_id`, and a scope can hold 100,000 traces, so `jobs_for_traces` scans the project's jobs once on the **reader pool**, never inside the single-writer transaction:
1. An empty `traces` returns `Ok(Vec::new())` at once.
2. `let wanted: HashSet<TraceId> = traces.iter().copied().collect();`
3. `SELECT id, trace_id FROM eval_jobs WHERE project_id = ? AND status <> 'running'` (bind the project id exactly as `insert_job` binds `project_id`; per the I3.2 schema it is `TEXT`, so `project.to_string()`), keeping the ids whose `TraceId::from_slice(&trace_id)` is in `wanted`. A blob or id that does not decode is skipped, never a panic.

`delete_jobs`: an empty `ids` returns `Ok(0)`; else `DELETE FROM eval_jobs WHERE status <> 'running' AND id IN (?, …)` in chunks of 500 (`sqlx::QueryBuilder<Sqlite>`), summing `rows_affected()`.

`deletion/resolve.rs`, in this order:
1. `state.deletions.guard.add_traces(project, &scope, row.id, row.created_at)` once the scope is known (`scope` includes the evaluation traces). It is idempotent and harmless if the transaction below rolls back; M3's `register` adds the same entries again.
2. `let jobs = crate::meta::eval_jobs::jobs_for_traces(state.meta.reader(), project, &scope).await` (a read error is handled like the resolution's other reads).
3. In the scope `meta.write`, after `store_counts` and only when it returned `true`: `crate::meta::eval_jobs::delete_jobs(conn, &jobs).await?`.

A job inserted between steps 2 and 3, or one that was `running` at step 2, keeps its row: the guard (step 1) stops it at check 0 or check A. Call `delete_jobs` directly from `resolve.rs`: there is no cascade module. Track J adds its own statement to this transaction when it has tables (README § Coordination).

`running` jobs keep their rows, as do the `skipped` rows they become, for up to 7 days (D-I-28 prunes finished jobs); they hold ids and usage, not payload. Accepted.

## Late copies: follow-up deletions

`crates/server/src/meta/deletions.rs`:

```rust
/// Stores a `trace_ids` deletion `id` of `traces` that follows `parent` (D-M-6): same org,
/// project and actor, `requested` = the trace count, status `resolving`, `created_at = now`.
/// It skips the open-request limit (`count_open_in_project`).
pub(crate) async fn insert_follow_up(conn: &mut SqliteConnection, parent: &DeletionRow, id: Uuid, traces: &[TraceId], now: i64) -> Result<(), MetaError>;
```

It is `insert_deletion` with `NewDeletion { id, org_id: parent.org_id, project_id: parent.project_id, kind: DeletionKind::TraceIds, user_id: None, request: RequestDetails { requested: Some(u32::try_from(traces.len()).unwrap_or(u32::MAX)), ..Default::default() }, actor_kind: parent.actor_kind, actor_id: parent.actor_id, state: DeletionState::Resolving, created_at: now }` (read the field names from `NewDeletion` and `DeletionRow`), then `insert_traces(conn, id, traces)`. Follow-ups still count as open rows, so a burst of them can make the next `POST` of that project answer `409` until the worker catches up; bounded by the job concurrency, accepted.

`crates/server/src/deletion/mod.rs`:

```rust
/// Asks for the deletion of copies committed after `parent`'s cut-off (D-M-6): a judge trace
/// and the target it scored. Stores the follow-up row, wakes the worker and writes the audit
/// event `deletion requested` with `follow_up_of = parent`. A missing `parent` is waited for:
/// the guard learns a deletion before `create_deletion` stores its row (`routes/deletions.rs`
/// fills it before the insert), so `follow_up` reads the row up to 20 times, 50 ms apart (about
/// one second), and only then gives up with `warn!` and `Ok(())`. That last case is an orphan
/// entry from a `POST` that failed after filling the guard: no deletion was stored, so nothing
/// needs erasing. A `parent` without a project is a bug: `warn!` and `Ok(())`.
///
/// # Errors
///
/// Returns [`MetaError`] when the parent cannot be read or the row cannot be stored.
pub(crate) async fn follow_up(state: &AppState, parent: Uuid, traces: &[TraceId]) -> Result<(), MetaError>;
```

Order: `deletion_by_id(state.meta.reader(), parent)` (`None`, or `project_id` `None` → `warn!`, `Ok(())`); `let id = Uuid::now_v7(); let now = state.clock.now_ns();`; the `meta.write` with `insert_follow_up`; `state.deletions.wake.notify_one()`; the audit line (`deletion_id`, `follow_up_of`, `kind = "trace_ids"`, `org_id`, `project_id`, actor; never ids of traces or users). Do **not** add guard entries here: the parent already guards the target for 24 h, and an entry with the follow-up's id could reach a concurrent job before the row is stored. The worker adds the follow-up's traces when it resolves it. The follow-up row is an ordinary `trace_ids` deletion (a dropped project's follow-up is marked applied by the worker, like any row of a dropped project): the worker resolves it (its evaluation-trace read finds the judge trace again), registers it with storage, and the Data tab lists it under the same actor. Its cut-off is later than the copies, so storage covers them, and the target's own rows a second time.

## The `Deleted` outcome

`meta/eval_jobs.rs`, a new `Finish` variant after `Skipped`:

```rust
    /// `skipped` because the target trace was deleted (D-M-6); sets `finished_at`,
    /// `error = 'deleted'` and the usage (all `None` before the call); clears `error_class`.
    Deleted { usage: JobUsage },
```

Its arm in `finish` is the `Skipped` arm's fenced `UPDATE` (`WHERE id = ? AND status = 'running' AND claims = ?`) with `error = 'deleted'` and the four usage columns bound as the `Done` arm binds them. It records no `judge_trace_id`: when checks B2 or C block, the judge trace already stored is erased by the follow-up deletion (§ Late copies).

Adding the variant can break exhaustive `match`es on `Finish` elsewhere in I's code: fix every one the compiler reports, in any `crates/server/src` file (that is in scope even outside § Edit only). None may treat `Deleted` as a pause, a retry or a breaker input.

How `run_job` finishes with it:
- **Before the call** (checks 0 and A): the same path as I's pre-call skips (`Skipped { error: "project unavailable" }`, step 1), with `Finish::Deleted { usage: JobUsage::default() }`. No `Ended`, no budget reservation (check A is before step 10), no pause, no connection status, no breaker.
- **After the call** (checks B, B2 and C): through the same exit as every other post-call outcome, I3.6's `Ended` and step 16's `finish` write:
  ```rust
  Ended { finish: Finish::Deleted { usage }, reached_evaluator: false, pause: Pause::None, actual }
  ```
  - `usage` is the `JobUsage` the `Done` path builds: step 13's `JudgeUsage` (tokens and cost) plus the `latency_ms` measured around `run_evaluator`.
  - `actual` follows I3.6 § Cost: `usage.cost_usd`, else the estimate when there are tokens, else 0.
  - So `rule_spend` gets `actual` and the reservation is settled with it, as for `Done` (the provider was paid; D-I-15).
  - Step 13 already counted the judge usage (`count_judge_usage`, D-I-18); do not count it again.
  - `reached_evaluator: false`: a deletion says nothing about the evaluator's health (D-I-14). `Pause::None`: when the call itself failed with a pause-class error (a `401`, say) and check B then blocks, the error is dropped on purpose; the deletion wins.

## Guard in the job runner

Every check is the same call, with a `// D-M-6` comment:

```rust
let now = state.clock.now_ns();
// D-M-6: the target trace was deleted.
if let Some(deletion) = state.deletions.guard.blocking(job.project_id, job.trace_id, &users, now) { /* … */ }
```

Use the executor's real names for the job's project, trace id and loaded spans (§ Job execution names `job: ClaimedJob`, which holds the `JobRow`). `users` is built once after the load: for each distinct non-empty `user_id` of the loaded spans, the smallest `ingest_time` of its spans (`Vec<(&str, i64)>`, or owned `String`s if the borrow does not outlive the commits).

- **Check 0**, at the very start of `run_job`, before step 1: trace id only (`users = &[]`). It catches a job re-queued from `running` (retries, the janitor, a restart) that the cascade left, without loading the trace. Blocks → finish `Deleted` before the call.
- **Check A**, right after step 6 (`trace_load::load_trace`) succeeds and before step 7, with `users`. Nothing was reserved or sent yet. Blocks → finish `Deleted` before the call.
- **Check B**, before step 14's judge-trace commit, inside the retry loop so that it runs before each of the up to 3 tries. Blocks → commit no judge trace and no score; finish `Deleted { usage }` (after the call).
- **Check B2**, after step 14 committed the judge trace and before step 15's `scores::write` (and before each of its retries): `after_commit(…)` (test 13 defines it). Blocks → it stored the follow-up deletion of `[judge_trace_id, job.trace_id]`; write no score, finish `Deleted { usage }`.
- **Check C**, after step 15's `scores::write` succeeded, before step 16: `after_commit(…)` again. Blocks → the follow-up covers the score too (the score's `trace_id` is the target); finish `Deleted { usage }` instead of `Done`.

Inside `after_commit`, a `follow_up` error is logged (`warn!`, job id and deletion id only) and the job still finishes `Deleted`: the copy then stays until a later deletion of the target or retention removes it. That is the only accepted window for copies after a cut-off.

Accepted window, to list in the report: a deletion registered during steps 7–11 (after check A, before the call) still lets that one call go out, because moving check A later would mean a held reservation and a counted attempt. Nothing it returns is kept: check B, B2 or C catches it.

A check that blocks never returns an error: `run_job` returns `Ok(())` after `finish`, as it does for I's other skips.

## Guard in test runs and manual runs

- `eval/test_run.rs` (D-I-21), for a test run whose target is a trace: right after `trace_load::load_trace` succeeds and before the judge call, the same check as A. The loaded spans live inside the trace arm of the source `match`, so compute `users` (as in the runner: each user id with its oldest `ingest_time`; owned `String`s, or borrows of a value that outlives the commit) there and carry them out of the arm (`None` for a literal-variables run). Check again with the same values right before it commits the test judge trace (`execute::commit_judge_trace`, I3.4), and once more right after the commit. When one of the first two blocks, answer `ApiError::NotFound` (the trace is gone from the caller's point of view) and commit nothing. When the check after the commit (`execute`'s `after_commit`, made `pub(crate)`) blocks, it stored the follow-up deletion of the test judge trace and the target; answer `ApiError::NotFound`. A test run on literal variables has no target and is not checked. If the second check blocks, the judge usage was already counted; leave it counted.
- `routes/eval_jobs.rs` `run_evaluator` (D-I-22), between I3.9's step 3 (the body rules) and step 4 (`insert_job`): `let now = state.clock.now_ns(); if state.deletions.guard.blocking(project_id, trace_id, &[], now).is_some() { return Err(ApiError::NotFound); }`, with a `// D-M-6` comment. It does not load the trace; check A in the runner covers end users. The rate limiter has already counted the request (step 1); that is accepted.

Remove `#![cfg_attr(not(test), allow(dead_code))]` from `deletion/guard.rs`. If clippy then reports an unused item there, delete the item rather than restoring the allowance.

## Spec and roadmap

- `docs/spec/03-storage.md` § Cascades: drop "Not built yet: this waits for track M's M8, which waits for I's M3." and "The guard exists and the request route fills it; the judge executor does not call it yet (M8)."
- `docs/spec/06-evaluation.md` § Online rules › Execution, a new bullet: "**Deleted targets:** a job whose target trace was deleted (D-M-6) ends `skipped` with error `deleted`; when the deletion lands during the judge call, the job keeps the call's cost and usage, and a judge trace or score it already committed is erased by a follow-up deletion of the same actor. Test runs and manual runs on a deleted trace answer `404`."
- `docs/roadmap.md` row M: status **Done**, M8 added to the shipped list.

## Tests

`meta/eval_jobs.rs` tests. Create jobs with I's `insert_job` (with a real project and evaluator row, built the way I's `meta/eval_jobs.rs` tests build them; copy their helper), then `UPDATE eval_jobs SET status = ? WHERE id = ?` for `running` and `done`. A second project and evaluator are needed for the "other project" job.

1. `deletes_jobs_of_deleted_traces_except_running`: jobs for trace A in `queued`, `done`, `running`; a `queued` job for trace B; a `queued` job for trace A in another project. `jobs_for_traces(project, [A])` returns the `queued` and `done` jobs of A; `delete_jobs` of them returns 2; the `running` job of A, the job of B and the other project's job remain. A job of that list set to `running` before `delete_jobs` is not deleted.
2. `empty_scope_deletes_nothing`: `jobs_for_traces` of `[]` is empty, and `delete_jobs(&[])` returns 0.
3. `finish_deleted_writes_skipped_with_usage`: a claimed job; `finish(…, Finish::Deleted { usage: JobUsage { input_tokens: Some(10), output_tokens: Some(5), cost_usd: Some(0.01), latency_ms: Some(20) } }, now)` → `true`; the row is `skipped`, `error = 'deleted'`, `error_class` `NULL`, `finished_at = now`, and carries the usage. With a stale `claims` it returns `false` and changes nothing.
4. `resolution_cascades_to_eval_jobs` (in `deletion/mod.rs` tests): a `queued` job for trace A (A committed with `ingest_time = now`, as in M3), then a `trace_ids` deletion of A and `run_once` → the job row is gone, the deletion is `pending`.

Spans in tests 4–9 are committed with explicit `ingest_time` (and `user_id = "u1"` where named), as in M3: the fixture's own `ingest_time` is later than the manual clock. Guard entries count only within 24 h of the server clock (`GUARD_NS`), and executor tests set that clock to the real time, so every `ingest_time` and every `created_at` passed to `add_traces`/`add_user` is derived from `app.state.clock.now_ns()`, never a constant.

Executor tests go in `eval/execute.rs`'s test module and reuse I3.6's helpers (`seeded`, `rule_job`, `job`), `MockLlm` and M3 § Conventions' test clock (set the manual clock to the real time first). `seeded` commits the target trace with a root span carrying `user_id`; where a test needs `ingest_time = t` or `user_id = "u1"`, commit a second target trace with its own trace id via `app.commit_spans` (the way `seeded` commits its own) and insert the job for that trace, so the seeded trace is left as it is.

5. `a_job_on_a_deleted_trace_is_skipped`: `state.deletions.guard.add_traces(project, &[A], Uuid::now_v7(), now)` before `run_job` → the job row is `skipped` with `error = 'deleted'` and no usage; no score for A (`GET /scores`), no judge trace in the project's trace list with `include_eval=true`; `mock.requests()` is empty.
6. `a_job_on_a_deleted_users_old_trace_is_skipped`: `let t = app.state.clock.now_ns() - 1_000_000_000;` (one second ago); a target trace with two spans of `user_id = "u1"`, one with `ingest_time = t` and one with `ingest_time = t + 2` (newer data in the same trace); `add_user(project, "u1", Uuid::now_v7(), t + 1)` → skipped as in 5.
7. `new_data_of_a_deleted_user_is_still_evaluated`: the same `t`, both spans at `ingest_time = t`, and `add_user(project, "u1", Uuid::now_v7(), t - 1)` (the deletion predates all the data) → the job ends `done` with a score.
8. `a_trace_deleted_during_the_judge_call_is_not_committed` (check B): the judge connection on `MockLlm::start_with_delay(vec![completion(…)], Duration::from_millis(500))` (I3.7); spawn `run_job`; poll `mock.requests().len() == 1` every 10 ms (at most 5 s), then `add_traces(project, &[A], Uuid::now_v7(), now)`; await the task → the job is `skipped`, `error = 'deleted'`, with the tokens of the mock's reply and `cost_usd` `NULL` (`completion` names the unpriced model `gpt-test`); the provider was called once; no score and no judge trace exist.
9. `deleted_after_the_call_spends_the_actual_cost`: the same as 8, with I3.6 `budget_spend_is_recorded`'s setup (a generous budget, a model the built-in table prices, and a mock reply naming it): the job row's `cost_usd` is `Some(c)` with `c > 0`, `rule_spend` for today equals `c`, and `budgets.view` shows `reserved == 0`. (No judge trace is stored, so read the cost from the job row, not from an `llm` span.)
10. `manual_run_on_a_deleted_trace_is_404` (`routes/eval_jobs.rs` tests): guard holds A → `POST …/evaluators/{id}/run` for A answers `404` and no job row is inserted.
11. `test_run_on_a_deleted_trace_is_404` (`eval/test_run.rs` tests): guard holds A → `POST …/evaluators/test` with target trace A answers `404`; `mock.requests()` is empty.
12. `test_run_deleted_during_the_call_commits_nothing`: the delayed `MockLlm` of test 8. `tokio::join!` the test-run request future and a helper future that polls `mock.requests()` every 10 ms (at most 5 s) and then calls `add_traces(project, &[A], Uuid::now_v7(), now)` → the request answers `404`, and no judge trace for A exists.

13. `a_copy_committed_after_the_cut_off_gets_a_follow_up` (`eval/execute.rs`): checks B2 and C cannot be reached on purpose by timing, so they share one private helper, `async fn after_commit(state: &AppState, project: ProjectId, target: TraceId, users: &[(&str, i64)], judge_trace_id: TraceId) -> bool` (true when it blocked; it calls `blocking` and, on a hit, `follow_up`), and `run_job` and `test_run` call it at those points. The test: `let now = app.state.clock.now_ns();` stores a real `trace_ids` deletion row `P` of trace A (`insert_deletion` + `insert_traces`, `created_at = now`) and adds A to the guard with `P.id`; commits a stand-in judge trace `J` that targets A, with an explicit `ingest_time = now - 1_000_000_000` (copy how I's tests commit a judge trace, or commit one span with `environment = llmobs-eval` and `metadata['llmobs.eval.target_trace_id'] = A`), and checks that `J` is listed by `GET /traces?include_eval=true`; then `after_commit(&app.state, project, A, &[], J)` → `true`. The new row is the project's deletion whose id is not `P.id`: it has `P`'s actor, `requested = 2` and traces {J, A}. After `crate::deletion::run_once` (it processes `P` too; only the follow-up's state is asserted) the follow-up is `pending` and `J` is no longer listed with `include_eval=true`. With an empty guard, `after_commit` returns `false` and stores nothing.
14. `follow_up_of_a_missing_parent_does_nothing` (`deletion/mod.rs` tests): `follow_up(state, Uuid::now_v7(), &[A])` → `Ok(())` and no row is stored.
15. `insert_follow_up_copies_the_actor` (`meta/deletions.rs` tests): the new row has the parent's org, project and actor, kind `trace_ids`, status `resolving`, `created_at = now`, its traces stored; with `MAX_OPEN` open rows in the project it is still inserted.

## Done when

- [ ] `resolve.rs` fills the guard, reads the job ids on the reader pool, and deletes them with `meta::eval_jobs::delete_jobs` in the scope transaction; `running` jobs are left to the guard. No `deletion/cascade.rs`.
- [ ] `run_job` checks the guard at start, after loading, before each commit and after each commit; a block after a commit stores a follow-up deletion; test runs and manual runs refuse a guarded trace; every call site has a `// D-M-6` comment.
- [ ] `Finish::Deleted` settles the budget with the actual cost after the call and touches no pause, connection status or breaker.
- [ ] The 15 tests pass; `just check` is green; `just openapi` changes nothing; one commit `Cascade deletions to evaluation jobs`.
- [ ] Report the function and line of each guard call, and anything in I's code that differed from this file (names, the step order, where `finish` is called).
