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
2. **Guard.** I's job runner stops a job about a guarded trace at its start (trace-id entries) and once its target is loaded, before anything is reserved or sent, and again before it commits anything; I's test run and manual run refuse a guarded trace. The guard semantics are M3's `DeletionGuard::blocks` (D-M-6): trace-id entries always block; a user entry blocks only a trace whose data was ingested at or before that deletion's cut-off.

**Edit only:**
- `crates/server/src/deletion/cascade.rs` (new), `crates/server/src/deletion/mod.rs` (declare it), `crates/server/src/deletion/resolve.rs` (call it);
- `crates/server/src/deletion/guard.rs` (drop the `dead_code` allowance);
- `crates/server/src/meta/eval_jobs.rs`: one function (`delete_for_traces`), one `Finish` variant (`Deleted`) and its arm in `finish`;
- `crates/server/src/eval/execute.rs` (`run_job` and the code `finish` is called from), `crates/server/src/eval/test_run.rs`, `crates/server/src/routes/eval_jobs.rs` (`run_evaluator`) — only at the points named;
- `docs/spec/03-storage.md` § Cascades (the two "not built yet" sentences), `docs/spec/06-evaluation.md` (one sentence, below), `docs/roadmap.md` (row M).

No API type changes: a deleted job is `status: skipped` with `error: "deleted"`, and `error` is free text in `EvalJob` (I3.5). `error_class` stays `NULL`, as for I's other skips (`budget`, `expired`, `project unavailable`). `just openapi` must leave `openapi.json` and `schema.gen.ts` unchanged.

**Commit:** `Cascade deletions to evaluation jobs`

## Read first

- M3's `deletion/resolve.rs` (the scope transaction) and `deletion/guard.rs` (`blocks(project, trace, users, newest_ingest_ns, now)`, `add_traces`, `add_user`).
- I's M3 plan, [`03-m3-queue-rules.md`](../i-online-eval/03-m3-queue-rules.md): § Job execution (the 16 steps of `run_job`, the dispositions, the breaker's `reached_evaluator`), § I3.2 › `meta/eval_jobs.rs` (`JobRow`, `JobUsage`, `Finish`, `finish`, `insert_job`, `claim`), § I3.6 (`finish` and the budget reservation guard, the `seeded` / `rule_job` / `job` test helpers), § I3.9 (`run_evaluator`).
- I's code as merged: `meta/eval_jobs.rs`, `eval/execute.rs`, `eval/test_run.rs`, `routes/eval_jobs.rs`. Where the code and the plan differ, follow the code and list the difference in your report.
- `llmobs_core::span::Span`: `user_id: Option<String>`, `ingest_time: i64`. `TraceId::from_slice` (`crates/core/src/ids.rs`).

## Cascade

`crates/server/src/meta/eval_jobs.rs`, next to I's other functions:

```rust
/// Deletes the jobs of `project` whose `trace_id` is in `traces`, except `running` ones
/// (D-M-6: those are stopped by the deletion guard). Returns the rows deleted. Deletes in
/// chunks of 500 ids.
pub(crate) async fn delete_for_traces(conn: &mut SqliteConnection, project: ProjectId, traces: &[TraceId]) -> Result<u64, MetaError>;
```

`eval_jobs` has an index on `(project_id, created_at)` but none on `trace_id`, and a scope can hold 100,000 traces, so scan the project's jobs once instead of once per chunk:
1. An empty `traces` returns `Ok(0)` at once.
2. `let wanted: HashSet<TraceId> = traces.iter().copied().collect();`
3. `SELECT id, trace_id FROM eval_jobs WHERE project_id = ? AND status <> 'running'` (bind the project id exactly as `insert_job` binds `project_id`; per the I3.2 schema it is `TEXT`, so `project.to_string()`), keeping the ids whose `TraceId::from_slice(&trace_id)` is in `wanted`. A blob that does not decode is skipped, never a panic.
4. `DELETE FROM eval_jobs WHERE id IN (?, …)` in chunks of 500 (`sqlx::QueryBuilder<Sqlite>`), summing `rows_affected()`.

The scan runs inside the single-writer transaction. It is bounded by the project's jobs of the last 7 days (D-I-28 prunes older finished ones). Accepted.

`crates/server/src/deletion/cascade.rs`:

```rust
//! `meta.db` rows that follow a deletion's scope (spec 03 § Cascades, D-M-7).
//!
//! Called in the transaction that stores a resolved scope. Track J adds its dataset-item
//! deletion here (README § Coordination).

/// Runs every cascade of a stored scope. Returns the number of `eval_jobs` rows deleted.
/// Today it has one statement; it exists so that track J adds its dataset-item deletion here
/// instead of in `resolve.rs` (README § Coordination).
pub(crate) async fn traces_deleted(conn: &mut SqliteConnection, project: ProjectId, traces: &[TraceId]) -> Result<u64, MetaError> {
    crate::meta::eval_jobs::delete_for_traces(conn, project, traces).await
}
```

`deletion/resolve.rs`: in the scope `meta.write`, after `store_counts` and only when it returned `true`, call `crate::deletion::cascade::traces_deleted(conn, project, &scope).await?` (`scope` includes the evaluation traces). Right before that `meta.write`, call `guard.add_traces(project, &scope, row.created_at)`: it is idempotent and harmless if the transaction rolls back, and it closes the gap in which a running job could pass its last check between the cascade and M3's `register`. `register` adds the same entries again. When `store_counts` returns `false` (the row is no longer `resolving`), the entries stay in the guard for 24 h although nothing was cascaded; that is harmless and accepted.

`running` jobs keep their rows, as do the `skipped` rows they become, for up to 7 days (D-I-28 prunes finished jobs); they hold ids and usage, not payload. Accepted.

## The `Deleted` outcome

`meta/eval_jobs.rs`, a new `Finish` variant after `Skipped`:

```rust
    /// `skipped` because the target trace was deleted (D-M-6); sets `finished_at`,
    /// `error = 'deleted'` and the usage (all `None` before the call); clears `error_class`.
    Deleted { usage: JobUsage },
```

Its arm in `finish` is the `Skipped` arm's fenced `UPDATE` (`WHERE id = ? AND status = 'running' AND claims = ?`) with `error = 'deleted'` and the four usage columns bound as the `Done` arm binds them. It records no `judge_trace_id` (check B2 can leave one stored; § Guard in the job runner says why that is accepted).

Adding the variant can break exhaustive `match`es on `Finish` elsewhere in I's code: fix every one the compiler reports, in any `crates/server/src` file (that is in scope even outside § Edit only). None may treat `Deleted` as a pause, a retry or a breaker input.

How `run_job` finishes with it:
- **Before the call** (checks 0 and A): the same path as I's pre-call skips (`Skipped { error: "project unavailable" }`, step 1), with `Finish::Deleted { usage: JobUsage::default() }`. No `Ended`, no budget reservation (check A is before step 10), no pause, no connection status, no breaker.
- **After the call** (checks B and B2): through the same exit as every other post-call outcome, I3.6's `Ended` and step 16's `finish` write:
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
if state.deletions.guard.blocks(job.project_id, job.trace_id, &users, newest, now) { /* finish Deleted */ }
```

Use the executor's real names for the job's project, trace id and loaded spans (§ Job execution names `job: ClaimedJob`, which holds the `JobRow`).

- **Check 0**, at the very start of `run_job`, before step 1: trace id only (`users = &[]`, `newest = 0`). It catches a job re-queued from `running` (retries, the janitor, a restart) that the cascade left, without loading the trace.
- **Check A**, right after step 6 (`trace_load::load_trace`) succeeds and before step 7: `users` = the distinct non-empty `user_id`s of the loaded spans (`Vec<&str>`), `newest` = the largest `ingest_time` of the loaded spans. Nothing was reserved or sent yet.
- **Check B**, before step 14's judge-trace commit, inside the retry loop so that it runs before each of the up to 3 tries: the same `users` and `newest`. If it blocks, commit no judge trace and no score; finish `Deleted { usage }` (after the call).
- **Check B2**, before step 15's `scores::write`, inside its retry loop likewise. If it blocks now, the judge trace is already stored: it holds a copy of the target's data and targets a trace that is now deleted. Write no score and finish `Deleted { usage }`. The job row does not point at that judge trace (`Deleted` has no `judge_trace_id`). The stored judge trace is found and deleted by the next deletion whose scope reads evaluation traces after it was committed; M3's resolution of *this* deletion may have run before it. List this window in the report; do not try to delete the judge trace from the executor.

Accepted windows, to list in the report: a deletion registered during steps 7–11 (after check A, before the call) still lets that one call go out, because moving check A later would mean a held reservation and a counted attempt; and check B2's stored judge trace above.

A check that blocks never returns an error: `run_job` returns `Ok(())` after `finish`, as it does for I's other skips.

## Guard in test runs and manual runs

- `eval/test_run.rs` (D-I-21), for a test run whose target is a trace: right after `trace_load::load_trace` succeeds and before the judge call, the same check as A. The loaded spans live inside the trace arm of the source `match`, so compute `users` (owned `String`s, or a `Vec<&str>` borrowed from a value that outlives the commit) and `newest` there and carry them out of the arm (`None` for a literal-variables run). Check again with the same values right before it commits the test judge trace (`execute::commit_judge_trace`, I3.4). When either blocks, answer `ApiError::NotFound` (the trace is gone from the caller's point of view) and commit nothing. A test run on literal variables has no target and is not checked. If the second check blocks, the judge usage was already counted; leave it counted.
- `routes/eval_jobs.rs` `run_evaluator` (D-I-22), between I3.9's step 3 (the body rules) and step 4 (`insert_job`): `let now = state.clock.now_ns(); if state.deletions.guard.blocks(project_id, trace_id, &[], 0, now) { return Err(ApiError::NotFound); }`, with a `// D-M-6` comment. It does not load the trace; check A in the runner covers end users. The rate limiter has already counted the request (step 1); that is accepted.

Remove `#![cfg_attr(not(test), allow(dead_code))]` from `deletion/guard.rs`. If clippy then reports an unused item there, delete the item rather than restoring the allowance.

## Spec and roadmap

- `docs/spec/03-storage.md` § Cascades: drop "Not built yet: this waits for track M's M8, which waits for I's M3." and "The guard exists; the judge executor does not call it yet (M8)."
- `docs/spec/06-evaluation.md` § Online rules › Execution, a new bullet: "**Deleted targets:** a job whose target trace was deleted (D-M-6) ends `skipped` with error `deleted` and writes nothing; when the deletion lands during the judge call, the job keeps the call's cost and usage. Test runs and manual runs on a deleted trace answer `404`."
- `docs/roadmap.md` row M: status **Done**, M8 added to the shipped list.

## Tests

`meta/eval_jobs.rs` and `deletion/cascade.rs` tests. Create jobs with I's `insert_job` (with a real project and evaluator row, built the way I's `meta/eval_jobs.rs` tests build them; copy their helper), then `UPDATE eval_jobs SET status = ? WHERE id = ?` for `running` and `done`. A second project and evaluator are needed for the "other project" job.

1. `deletes_jobs_of_deleted_traces_except_running`: jobs for trace A in `queued`, `done`, `running`; a `queued` job for trace B; a `queued` job for trace A in another project. `delete_for_traces(project, [A])` returns 2; the `running` job of A, the job of B and the other project's job remain.
2. `empty_scope_deletes_nothing`: returns 0.
3. `finish_deleted_writes_skipped_with_usage`: a claimed job; `finish(…, Finish::Deleted { usage: JobUsage { input_tokens: Some(10), output_tokens: Some(5), cost_usd: Some(0.01), latency_ms: Some(20) } }, now)` → `true`; the row is `skipped`, `error = 'deleted'`, `error_class` `NULL`, `finished_at = now`, and carries the usage. With a stale `claims` it returns `false` and changes nothing.
4. `resolution_cascades_to_eval_jobs` (in `deletion/mod.rs` tests): a `queued` job for trace A (A committed with `ingest_time = now`, as in M3), then a `trace_ids` deletion of A and `run_once` → the job row is gone, the deletion is `pending`.

Spans in tests 4–8 are committed with explicit `ingest_time` (and `user_id = "u1"` where named), as in M3: the fixture's own `ingest_time` is later than the manual clock.

Executor tests go in `eval/execute.rs`'s test module and reuse I3.6's helpers (`seeded`, `rule_job`, `job`), `MockLlm` and M3 § Conventions' test clock (set the manual clock to the real time first). `seeded` commits the target trace with a root span carrying `user_id`; where a test needs `ingest_time = t` or `user_id = "u1"`, commit a second target trace with its own trace id via `app.commit_spans` (the way `seeded` commits its own) and insert the job for that trace, so the seeded trace is left as it is.

5. `a_job_on_a_deleted_trace_is_skipped`: `state.deletions.guard.add_traces(project, &[A], now)` before `run_job` → the job row is `skipped` with `error = 'deleted'` and no usage; no score for A (`GET /scores`), no judge trace in the project's trace list with `include_eval=true`; `mock.requests()` is empty.
6. `a_job_on_a_deleted_users_old_trace_is_skipped`: target trace whose spans have `user_id = "u1"` and `ingest_time = t`; `add_user(project, "u1", t + 1)` → skipped as in 5.
7. `new_data_of_a_deleted_user_is_still_evaluated`: same, but `add_user(project, "u1", t - 1)` (the deletion predates the data) → the job ends `done` with a score.
8. `a_trace_deleted_during_the_judge_call_is_not_committed` (check B): the judge connection on `MockLlm::start_with_delay(vec![completion(…)], Duration::from_millis(500))` (I3.7); spawn `run_job`; poll `mock.requests().len() == 1` every 10 ms (at most 5 s), then `add_traces(project, &[A], now)`; await the task → the job is `skipped`, `error = 'deleted'`, with the tokens of the mock's reply and `cost_usd` `NULL` (`completion` names the unpriced model `gpt-test`); the provider was called once; no score and no judge trace exist.
9. `deleted_after_the_call_spends_the_actual_cost`: the same as 8, with I3.6 `budget_spend_is_recorded`'s setup (a generous budget, a model the built-in table prices, and a mock reply naming it): the job row's `cost_usd` is `Some(c)` with `c > 0`, `rule_spend` for today equals `c`, and `budgets.view` shows `reserved == 0`. (No judge trace is stored, so read the cost from the job row, not from an `llm` span.)
10. `manual_run_on_a_deleted_trace_is_404` (`routes/eval_jobs.rs` tests): guard holds A → `POST …/evaluators/{id}/run` for A answers `404` and no job row is inserted.
11. `test_run_on_a_deleted_trace_is_404` (`eval/test_run.rs` tests): guard holds A → `POST …/evaluators/test` with target trace A answers `404`; `mock.requests()` is empty.
12. `test_run_deleted_during_the_call_commits_nothing`: the delayed `MockLlm` of test 8. `tokio::join!` the test-run request future and a helper future that polls `mock.requests()` every 10 ms (at most 5 s) and then calls `add_traces(project, &[A], now)` → the request answers `404`, and no judge trace for A exists.

## Done when

- [ ] `meta::eval_jobs::delete_for_traces` runs in the scope transaction through `deletion::cascade::traces_deleted`; `running` jobs are left to the guard.
- [ ] `run_job` checks the guard at start, after loading and before each commit; test runs and manual runs refuse a guarded trace; every call site has a `// D-M-6` comment.
- [ ] `Finish::Deleted` settles the budget with the actual cost after the call and touches no pause, connection status or breaker.
- [ ] The 12 tests pass; `just check` is green; `just openapi` changes nothing; one commit `Cascade deletions to evaluation jobs`.
- [ ] Report the function and line of each guard call, and anything in I's code that differed from this file (names, the step order, where `finish` is called).
