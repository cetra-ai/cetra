# I · M6 — Backfill, the `cetra serve` end-to-end test, and the closeout of track I

Implementation plan for milestone M6 of track I. It has 8 sub-phases, I6.1 to I6.8. **Haiku** implements each one in one go, with no context beyond this plan and the files it names. Sub-phases that do not depend on each other run in parallel worktrees (§ Parallel waves). Each sub-phase ends with `just check` green and one commit. Sonnet reviews the code after every three or four sub-phases.

**Prerequisites** (check them before you start):
- M4 is merged on `main`: `crates/server/src/eval/trigger.rs` has `sweep_once`, `crates/server/src/eval/sampling.rs` has `sampled`, `crates/server/src/api/convert.rs` has `match_rows`, and `crates/server/migrations/meta/0011_pending_traces.sql` exists. Start from `main`.
- Track P's data window is on `main`: `crates/server/src/entitlements/window.rs` has `query_options` and `Visibility`. It had landed when this plan was written.
- Check that this plan file, `docs/plans/i-online-eval/06-m6-backfill-closeout.md`, exists.
- The decision rows D-I-38 to D-I-43 exist in `docs/mvp-decisions.md` § Track I and `docs/plans/i-online-eval/00-design.md` § Decisions, and D-I-25 ends with the sentence that points to them (the coordinator adds them with this plan). If they are missing, STOP and report.
- Node `26.13.0` is installed at `$HOME/.local/share/nvm/v26.13.0/bin`. It is not on the non-interactive `PATH`.

**Read first** (every sub-phase):
- `AGENTS.md`, all of it.
- This file's sections up to § I6.1, then your own sub-phase.
- `docs/plans/i-online-eval/00-design.md` § Decisions: D-I-6, D-I-10, D-I-11, D-I-14, D-I-15, D-I-17, D-I-18, D-I-24, D-I-25, D-I-28, D-I-34, D-I-35; § Tables; § Queue (M3); § Risks.
- `docs/plans/i-online-eval/03-m3-queue-rules.md` § Conventions for every sub-phase and § Shared reference, and `docs/plans/i-online-eval/04-m4-triggers.md` § Conventions for every sub-phase. Their patterns (loops, `MetaDb::write` closures, routes, fixtures, pages, dead-code allows) are the model for M6.
- `docs/spec/06-evaluation.md` § Online rules; `docs/spec/04-metadata-db.md` § Evaluation; `docs/spec/09-api.md` § Evaluation & alerts.
- The code each sub-phase points to, before you edit it.

## What M6 is, and what it is not

When M6 is done:
- a project member can **backfill** an online rule over stored traces (D-I-25): `POST …/online-rules/{id}/backfills/estimate` answers what a window matches, how many targets the sample keeps and what they would cost; `POST …/online-rules/{id}/backfills` creates a `backfills` row and answers `202`; `GET …/online-rules/{id}/backfills` lists the rule's latest 50.
- a **backfill runner** (one more eval loop) pages `trace_list` or `span_list` over the window with the rule's filter and strict exclusions, samples each target, skips the targets this rule already scored, and enqueues backfill jobs (priority 2) with D-I-11's re-queue rule. The M3 dispatcher runs them like rule jobs.
- startup marks a backfill left `running` by a stop or a crash `failed` (`interrupted`).
- the rule page has a **Backfills** section and a **"Run on existing data"** dialog with the estimate.
- a Rust test starts a real `cetra serve` (in-process `serve::bind`, as `crates/cli/tests/e2e.rs` does), sends OTLP, and reads back a judge score written by the live trigger and one written by a backfill, against the capture tool's mock provider.
- specs 04, 06, 09, 11 and 12 describe backfill; `00-design.md`, `docs/mvp-decisions.md` and the roadmap's track I row are closed out.

**Out of scope:** the annotation panel and score ownership (M5, planned in parallel); a cancel button for a running backfill (D-I-25: there is none; disabling the rule stops its jobs); pruning `backfills` rows (they go with their rule); experiments (track J).

**Cost controls stay as they are.** The estimate is advisory: nothing refuses a backfill because of its cost. An unbudgeted judge rule has no spend cap beyond `max_targets` and the connection's rate limits (D-P-13). A budgeted rule's backfill jobs spend its budget when they run and can pause the live rule (`budget`, D-I-15). The monthly units cap is checked per job when it runs (D-I-35).

## Decisions this plan relies on

| # | Summary |
|---|---|
| D-I-6 | Strict priority manual, rule, backfill; backfill can wait behind live load. |
| D-I-10 | Sampling: SHA-256(`cetra.sample.v1` ‖ rule id ‖ trace id ‖ span id for span rules), first 8 bytes BE, below `(rate · 2^64) as u64`. |
| D-I-11 | `dedup_key` `rule:<rule>:<trace>:<span or ->` for rule **and** backfill jobs. A backfill enqueue is `ON CONFLICT DO UPDATE … WHERE status IN ('failed','skipped')`: it re-queues a failed or skipped target and leaves done, queued and running ones alone. The update's column list is exact (§ I6.1). Backfill has its own cap, `max_targets`, not the hourly cap. |
| D-I-14 | Paused rules keep their queued jobs; the dispatcher does not claim a paused or disabled rule's jobs. |
| D-I-15 | Backfill jobs spend the rule's budget when they run, and can pause the live rule. |
| D-I-17 | Every matching read (live, preview, backfill) uses strict exclusions. |
| D-I-18 | An evaluator score's derived id has the rule id as its author and the evaluator kind's source. |
| D-I-24 | Backfill is `ProjectPerm::ManageEvaluation` (API keys cannot use it); reads are `ProjectPerm::View`. |
| D-I-25 | Backfill (user's choice): window at most 30 days, `to` at most now − 5 min, `max_targets` 1–10 000, `sample_rate?`; rule enabled and unpaused (else 409); one running backfill per rule (409, partial unique index); estimate = sampled × (template length + 2 000 characters per mapped variable) / 4 input tokens + `max_output_tokens`, at the model's price; skip targets already scored by the rule; backfill jobs expire after 7 days; a backfill running at shutdown is `failed` (`interrupted`) at the next start; no cancel. |
| D-I-28 | Pruning already expires queued backfill jobs after 7 days (`meta::eval_jobs::prune`, M3). M6 changes nothing there. |
| D-I-34 | Every read goes through `Storage::query` with `entitlements::query_options(Visibility::window(state, org), …)`. A window that reaches past the plan window matches nothing there. |
| D-I-35 | The units cap is checked by the executor when a job runs (`skipped`, `quota`); creating a backfill does not check it. |

**New decisions of this plan** (the coordinator adds them to `docs/mvp-decisions.md` § Track I and `00-design.md` § Decisions, and amends D-I-25, with this plan; I6.8 only checks they match the code):

| # | Decision |
|---|---|
| D-I-38 | **The estimate is its own operation.** `POST /online-rules/{id}/backfills/estimate` takes the create body and answers `200 BackfillEstimate`; `POST /online-rules/{id}/backfills` answers `202 Backfill`. This replaces D-I-25's `dry_run` flag: each operation has one success status and one schema, as track M's `POST /deletions/preview` beside `POST /deletions`. |
| D-I-39 | **The backfill runner is an eval loop.** Creating a backfill inserts the row and wakes the runner. The runner (`backfill_once`, every 5 s or on a wake-up; TEST 50 ms) runs the `running` backfills of active projects oldest first, one at a time, each to its end, and checks the stop flag between pages. There is no task per request, so shutdown joins the runner like the other eval loops. A backfill the runner was running at shutdown stays `running`, and `eval::recover` marks it `failed` (`interrupted`) at the next start, before the loops start. One backfill's error never holds up the others: the runner logs it and goes on to the next (D-I-41). |
| D-I-40 | **What a backfill reads and enqueues.** Pages of at most 1 000 rows of `trace_list` (trace rules) or `span_list` (span rules), `start_time ∈ [from, to)`, newest first, with the rule's current filter and strict exclusions. The first `max_targets` rows are the backfill's targets (`matched`); the estimate reads up to `max_targets + 1` to say "more than". Each target is sampled with D-I-10's function, the rule's id and the backfill's `sample_rate` (default: the rule's), so a backfill at the rule's rate picks the targets live sampling would have picked. A span rule's backfill does not apply the live 50-spans-per-trace cut; `max_targets` bounds it. Targets that already have this rule's score are skipped: per page, the derived score ids (D-G-28: the rule as author, the evaluator kind's source, the evaluator's current output name) are looked up with one `scores_by_id` read. This replaces D-I-25's "the `rule_id` score filter", which would read every score of the rule; a score written under an older output name does not count. Each page is one `meta.db` write: the rule must still exist and be enabled, then every job is upserted (D-I-11) and `matched` and `enqueued` are updated. |
| D-I-41 | **Failures during a run.** A storage read that fails fails the backfill: `storage unavailable` for `QueryError::Busy` and `RunError::Timeout`, `invalid filter` for `RunError::Invalid` (the rule's stored filter no longer parses), `storage read failed` for anything else (another `RunError`, a `QueryError`, a row that does not convert). The jobs already enqueued stay; there is no retry, and running the backfill again is cheap (dedup keys and the scored check). A rule disabled during a run fails it (`rule disabled`). A rule deleted during a run deletes its backfills (`ON DELETE CASCADE`) and the runner stops at its next write: D-I-25's "marks the backfill failed" can only hold for a rule that still exists. A rule paused during a run does not stop it (D-I-14). A stored rule, project or evaluator that does not decode (`MetaError::Corrupt`) fails the backfill (`rule could not be read`, or `evaluator could not be read`). Any other `meta.db` error (`MetaError::Db`) leaves the backfill `running`: the runner logs it, goes on to the next backfill, and runs this one again from the start at its next pass. An undecodable `backfills` row fails the whole pass (accepted: status is constrained and rows are written only by this code). A rule whose filter, variable mapping, sample rate, target or evaluator changes during a run fails it (`rule changed`); a pause, resume or budget change does not. An edit to the evaluator itself (its template or output) during a run is not detected. |
| D-I-42 | **Limits and the list.** `from < to`, `to ≤ now − 5 min`, `to − from ≤ 30 days`, `max_targets` 1–10 000, `sample_rate` in `(0, 1]`; every failing field is reported (`400`). Creating checks, in one write, that the rule exists in the project (`404`), is enabled and unpaused (`409`), and has no running backfill (`409`). The estimate's cost is for judges only: per target `ceil((template chars + 2 000 × mapped variables) / 4)` input tokens and `max_output_tokens` output tokens; the cost is absent when the model has no price. The estimate does not subtract already-scored targets: it is an upper bound. `GET /online-rules/{id}/backfills` answers the newest 50 (`BackfillPage`, `next_cursor` always `null`, as alert rules are not paged); rows are kept until their rule is deleted. A backfill's start is an audit event. |
| D-I-43 | **A tool crate as a test dependency.** A `tools/` crate may be a dev-dependency of a shipped crate, for tests only: `cetra-capture`'s mock provider serves the judge in `cetra-cli`'s `cetra serve` end-to-end test. A `tools/` crate is never a normal dependency of a shipped crate. AGENTS.md § Layout's crate graph says so. |

**This plan also reads the design as follows** (no new decision needed):
- The migration is `00MM_backfills.sql`, the next free number on `origin/main` when I6.1 starts (`0014` when this plan was written; § The migration number), not the design's original `0012`. It adds an index `backfills_rule(rule_id, created_at)` for the list.
- `backfills` has no `project_id`: every read joins `online_rules` for it.
- Backfill jobs do not go through the hourly cap (D-I-11) and are not tracked by `PendingTraces`.
- **The dispatch scan with a backfill backlog** (`00-design.md` § Risks). A backfill adds at most 10 000 queued jobs of one rule. M3's `dispatch_once` test of a paused rule's 5 000 queued jobs (`eval/dispatch.rs`) already covers the path such a backlog takes (filtered in SQL before the per-project window); raising it to 10 000 would test the same path at twice the cost, so M6 leaves it. If profiles show the scan, the design's next step (an index over the queued rows) applies.

## Versions

No new external dependency. I6.7 adds the workspace's own capture tool as a test dependency:
- root `Cargo.toml` `[workspace.dependencies]`: `cetra-capture = { path = "tools/capture" }` (a path dependency; `deny.toml` already has `allow-wildcard-paths = true`);
- `crates/cli/Cargo.toml` `[dev-dependencies]`: `cetra-capture.workspace = true` and `axum.workspace = true` (already a workspace dependency, `0.8.9`).

`cetra-capture` is never a normal dependency of a shipped crate (D-I-43; I6.7 updates AGENTS.md's crate graph to say so).

## Conventions for every sub-phase

M3's and M4's conventions apply unchanged (`03-m3-queue-rules.md` § Conventions for every sub-phase and `04-m4-triggers.md` § Conventions for every sub-phase): the Node `PATH`, the `just` recipes, contract regeneration (`just openapi`, `just api-examples`), logs of a check run under your worktree's own `target/`, `just check-rust` as one background task, the clippy denies, `tests/openapi.rs` pinning every operation, the test-clock rule for evaluator runs, log-capture tests on plain `#[tokio::test]`, background loops on K's pattern, no `std::sync::Mutex` guard across an `.await`, short `MetaDb::write` closures that own their values, no `unwrap`/`expect` outside tests, privacy in logs, report structs logged field by field, one commit per sub-phase with no trailer lines, and the report.

In addition:
- **Logs never carry filter text, payloads or score values.** Backfill, rule, project ids, counts, statuses and error strings from this plan are fine.
- **Every storage read of M6 builds its options with `query_options(Visibility::window(state, org), None)`** (`crate::entitlements`), never `QueryOptions::new` or a struct literal: `entitlements::window::tests::only_query_options_builds_query_options` fails otherwise. `eval/backfill.rs` reads storage directly, so I6.3 adds it to `direct_storage_reads_are_listed` in the same file. It does not use `routes::read::open_query_on`: that helper answers `ApiError`, which loses the storage error classes the runner stores (D-I-41), and `eval` code does not call into `routes`, as `eval/trigger.rs` and `eval/trace_load.rs` do not.
- **Test helpers.** Tests that commit traces copy the private helpers of `eval/trigger.rs`'s second test module (`set_clock`, `trace_id`, `span_id` and `span`, which builds on `test_support::dataset_span(0)`; read them first), with the imports they need (`use crate::test_support::{TestApp, capture_logs, dataset_span};`, `TraceId`, `SpanId`, `Span`), into their own test module. Do not make the trigger's helpers `pub`.
- **Dead-code allows in M6.** The table lists every item-level `#[allow(dead_code)]` M6 adds, who adds it and who removes it. Use the comment given: `// I6.x calls it.` on functions and constants, `// I6.x reads it.` on structs (an allow on a struct covers its fields). After I6.5, `rg -n "I6\.[0-9] (calls|reads) it" crates/server` must be empty. An item reached only from an allowed item is not dead and needs no allow of its own; the table still names a few such items, so that no field read only by a later sub-phase is missed.

  | Item | Added by | Comment | Removed by |
  |---|---|---|---|
  | `meta::backfills::{running_backfills, write_page, finish_backfill, fail_running}` | I6.1 | `// I6.4 calls it.` | I6.4 |
  | `meta::backfills::{insert_backfill, list_backfills}` | I6.1 | `// I6.5 calls it.` | I6.5 |
  | `meta::backfills::BackfillRow` (the struct: `status`, `matched`, `enqueued`, `error`, `created_by`, `created_at` and `finished_at` are first read by the routes) | I6.1 | `// I6.5 reads it.` | I6.5 |
  | `meta::backfills::NewBackfill` (the struct: first built by the create route) | I6.1 | `// I6.5 reads it.` | I6.5 |
  | `meta::backfills::BackfillEnd` (the enum: its variants are first built by the runner) | I6.1 | `// I6.4 reads it.` | I6.4 |
  | `meta::backfills::PageWrite` (the enum: `Written.enqueued` is first read by the runner) | I6.1 | `// I6.4 reads it.` | I6.4 |
  | `eval::backfill::{Pager::new, Pager::next_page, unscored, Scoring::from_row, BackfillReadError::stored, sampled_rows, BackfillRule::new}` | I6.3 | `// I6.4 calls it.` | I6.4 |
  | `eval::backfill::{ERROR_RULE_DISABLED, ERROR_EVALUATOR, ERROR_RULE}` | I6.3 | `// I6.4 calls it.` | I6.4 |
  | `eval::backfill::{Scoring, TargetPage}` (the structs: `TargetPage.ctx` is first read by the runner) | I6.3 | `// I6.4 reads it.` | I6.4 |
  | `eval::backfill::{validate, estimate}` | I6.3 | `// I6.5 calls it.` | I6.5 |
  | `eval::backfill::{Window, JudgeCost, Estimate, CostEstimate}` (the structs: the estimate's fields are first read by the route) | I6.3 | `// I6.5 reads it.` | I6.5 |
  | `eval::EvalQueue::wake_backfills` | I6.3 | `// I6.5 calls it.` | I6.5 |
  | the `backfill_wake` field of `EvalQueue` (read by I6.4's loop) | — | — (reached through `wake_backfills`) | — |

  Every other M6 type is built and read by the sub-phase that adds it (`RunEnd` in I6.4, the route's private `Create` in I6.5). If `just check` reports an unused item that M6 adds and this table does not name, STOP and report it. Do not add an allow of your own.

## Parallel waves

Each wave starts when the waves before it are merged into the M6 integration branch. Sub-phases in one wave touch different files.

| Wave | Sub-phases | Depends on | Shared files to merge by hand |
|---|---|---|---|
| 1 | I6.1 `backfills` table and rows · I6.2 API contract · I6.3 backfill reads, sampling, scored check and estimate | M4 | none (I6.3 adds a module line to `eval/mod.rs`; I6.1 and I6.2 do not touch it) |
| 2 | I6.4 the runner, startup recovery, spec 06 and 12 · I6.6 web: backfills section and dialog, spec 11 | I6.1 and I6.3 (I6.4); I6.2 (I6.6) | none |
| 3 | I6.5 the routes, spec 09 | I6.1, I6.2, I6.3, I6.4 (it edits the allow lines of `eval/backfill.rs`, `eval/mod.rs` and `meta/backfills.rs` after I6.4 did) | none |
| 4 | I6.7 the `cetra serve` end-to-end test | I6.4, I6.5 | none |
| 5 | I6.8 closeout: specs check, `00-design.md`, roadmap | every other sub-phase | none |

I6.5 runs after I6.4, not beside it, because both edit the allow lines of the same three files. The Sonnet code reviews: after wave 1 (I6.1–I6.3), after waves 2–3 (I6.4–I6.6), and after waves 4–5 (I6.7, I6.8).

Contract files (`openapi.json`, `schema.gen.ts`, `fixtures/api/manifest.json`) are regenerated by I6.2 only. After merging a wave, run `just openapi` and `just api-examples` once and commit any diff as `Regenerate the API contract`.

**M5 runs at the same time.** M5 (annotation panel, score ownership) edits `crates/server/src/scores.rs`, `docs/spec/06-evaluation.md` § Scores, `docs/spec/11-ui.md`, and possibly `http/access.rs`, `api/openapi.rs`, `crates/server/openapi.json`, `web/src/api/schema.gen.ts`, `crates/server/src/api_examples.rs`, `crates/server/tests/openapi.rs`, `crates/server/tests/api_examples.rs`, `fixtures/api/manifest.json`, `docs/roadmap.md` (the I row), `docs/plans/i-online-eval/00-design.md` and `docs/mvp-decisions.md`. M6 touches those files only where its sub-phases say so: I6.3 adds one function to `scores.rs` (next to `derive_id`, nowhere else); I6.2 the contract files; I6.5 one line of `http/access.rs`; I6.4 one subsection of spec 06; I6.6 one bullet of spec 11; I6.8 the row and the design's milestone and module lines. Whichever milestone merges second resolves the conflicts there and reruns `just openapi` and `just api-examples`.

## Final layout after M6

```
Cargo.toml                                  cetra-capture path dependency (I6.7)
crates/server/migrations/meta/00MM_backfills.sql            (I6.1; 0014 today)
crates/server/src/
  meta/backfills.rs                         BackfillRow, NewBackfill, insert, list, running, write_page, finish, fail_running (I6.1)
  meta/eval_jobs.rs                         upsert_backfill_job (I6.1)
  meta/mod.rs                               module line (I6.1)
  api/backfills.rs                          CreateBackfillRequest, BackfillStatus, Backfill, BackfillEstimate (I6.2)
  api/mod.rs, api/openapi.rs                module line; backfill_paths, schemas (I6.2)
  api_examples.rs                           three examples (I6.2)
  scores.rs                                 rule_score_id (I6.3)
  eval/backfill.rs                          validate, Pager, sampled_rows, unscored, estimate (I6.3); backfill_once, run_backfill (I6.4)
  eval/mod.rs                               module line, EvalQueue.backfill_wake, wake_backfills (I6.3);
                                            EvalTiming.backfill_every, the runner loop, recover marks backfills failed (I6.4)
  entitlements/window.rs                    "eval/backfill.rs" in direct_storage_reads_are_listed (I6.3)
  routes/backfills.rs                       list, estimate, create (I6.5)
  routes/mod.rs, http/access.rs             module line, route list (I6.5)
crates/server/tests/openapi.rs, tests/api_examples.rs          (I6.2)
crates/cli/Cargo.toml, crates/cli/tests/e2e.rs                 online_rules_and_backfill_end_to_end (I6.7)
fixtures/api/static/{backfills,backfill,backfill-estimate}.json, requests/{backfill-create,backfill-estimate}.json (I6.2)
web/src/api/queries-online-rules.ts          backfillsQuery, backfillsKey (I6.2)
web/src/mocks/writes.ts                      post_backfill, post_backfill_estimate (I6.2)
web/src/components/online-rules/backfills-table.tsx, backfill-dialog.tsx (+ tests)   (I6.6)
web/src/routes/$org/$project/online-rules/$ruleId/index.tsx (+ test)                  (I6.6)
AGENTS.md                                    the tools/ dev-dependency rule (I6.7)
docs/spec/04, 06, 09, 11, 12; docs/roadmap.md; 00-design.md     (I6.1–I6.8)
```

## Shared reference

### The migration number

The next free number on `origin/main` when you start I6.1; renumber when you rebase if another track takes it first. When this plan was written, `main` had `0001`–`0013` (`0012_deletion_feed.sql` and `0013_org_sso.sql` are other tracks'), so the number was `0014`. Check `crates/server/migrations/meta/` on your base **and** `git log origin/main --oneline -- crates/server/migrations/meta/`. Call it `MM` below. Everything that depends on it:
- the file name `crates/server/migrations/meta/00MM_backfills.sql`;
- the `meta_schema` snapshot (`crates/server/tests/snapshots/meta_schema.snap`);
- `docs/spec/04-metadata-db.md` (the table), and `00-design.md` § Modules, § Tables and § Scope, which name the migration file (I6.8 gives them the number you used).

The migration tests need no edit: `open_twice_is_idempotent`, `open_existing_checks_the_file_and_the_version` (`meta/mod.rs`) and `schema_mismatch_names_the_versions` (`admin.rs`) compute the version with `expected_version()`.

If another branch lands a migration on `main` before M6 merges, the one that merges second renumbers. A migration that already merged is never renumbered.

### Fixed identifiers (fixtures and tests)

| Thing | Value |
|---|---|
| Backfill (done) | `01900000-0000-7000-8000-000000000b00`, rule `…0700`, `from` `2025-12-25T00:00:00.000000000Z`, `to` `2026-01-01T00:00:00.000000000Z`, `max_targets` 1000, `sample_rate` 0.25, `done`, `matched` 1000, `enqueued` 240, `created_by` `…0100`, `created_at` `2026-01-01T10:00:00.000000000Z`, `finished_at` `2026-01-01T10:00:05.000000000Z` |
| Backfill (failed) | `01900000-0000-7000-8000-000000000b01`, rule `…0700`, `from` `2025-12-31T00:00:00.000000000Z`, `to` `2026-01-01T00:00:00.000000000Z`, `max_targets` 100, `sample_rate` 1, `failed`, `matched` 40, `enqueued` 40, `error` `interrupted`, `created_by` `…0100`, `created_at` `2026-01-01T09:00:00.000000000Z`, `finished_at` `2026-01-01T09:30:00.000000000Z` |
| Backfill (the `202` answer) | `01900000-0000-7000-8000-000000000b02`, rule `…0700`, the done one's window, `max_targets` 1000, `sample_rate` 0.25, `running`, `matched` 0, `enqueued` 0, `created_by` `…0100`, `created_at` `2026-01-01T11:00:00.000000000Z` |
| Request body (create and estimate) | `{"from": "2025-12-25T00:00:00Z", "to": "2026-01-01T00:00:00Z", "max_targets": 1000, "sample_rate": 0.25}` |
| Estimate answer | `{"matched": 1000, "capped": true, "sampled": 250, "estimated_input_tokens": 1062500, "estimated_output_tokens": 256000, "estimated_cost_usd": 0.31, "from": "2025-12-25T00:00:00.000000000Z", "to": "2026-01-01T00:00:00.000000000Z"}` |

The rule, evaluator, user and job ids are M3's (`03-m3-queue-rules.md` § Fixed identifiers; `fixtures/api/README.md`).

### Backfill contract

JSON field names are exact. Times are RFC 3339 (`cetra_core::time_serde`; optional times as `OnlineRule.paused_at` declares them). Every `Option` field of a response has `#[serde(default, skip_serializing_if = "Option::is_none")]`. Requests are `#[serde(deny_unknown_fields)]`.

- **`BackfillStatus`**: `"running"` | `"done"` | `"failed"`. `snake_case`, `Copy`, with `as_str` and `from_name -> Option<Self>` like `PauseReason`.
- **`CreateBackfillRequest`** (body of both POSTs; **`Deserialize` only**, because `TimeParam` has no `Serialize`; `Debug` derived): `from` (`TimeParam`, required), `to` (`TimeParam`, required), `max_targets` (`u32`, required), `sample_rate?` (`f64`; absent means the rule's; `null` is refused with `super::online_rules::present`, as `UpdateOnlineRuleRequest` does).
- **`Backfill`** (response), fields in this order: `id` (UUID), `rule_id` (UUID), `status` (`BackfillStatus`), `from` (time), `to` (time), `max_targets` (integer), `sample_rate` (number), `matched` (integer: targets read so far, at most `max_targets`), `enqueued` (integer: jobs inserted or re-queued), `error?` (string, only on `failed`), `created_by?` (UUID), `created_at` (time), `finished_at?` (time).
- **`BackfillPage`**: `Page<Backfill>` registered with `insert_page::<Backfill>("BackfillPage")`; `next_cursor` is always `null`.
- **`BackfillEstimate`** (response), fields in this order: `matched` (integer, at most `max_targets`), `capped` (bool: more than `max_targets` rows matched), `sampled` (integer), `estimated_input_tokens?` (integer, judges only), `estimated_output_tokens?` (integer, judges only), `estimated_cost_usd?` (number, judges with a priced model only), `from` (time), `to` (time).

| Operation | Errors |
|---|---|
| `list_backfills` | 400 (bad path id), 401, 403, 404 (unknown rule), 429 |
| `post_backfill_estimate` | 400 (fields, § Validation; `invalid_filter` if the stored filter no longer parses), 401, 403, 404, 422 (`query_timeout`), 429, 503 (`busy`) |
| `post_backfill` | 400 (fields), 401, 403, 404, 409 (`conflict`: `the rule must be enabled and not paused to backfill`, or `a backfill of this rule is already running`), 429 |

### Validation (`eval::backfill::validate`)

| # | Check | Field | Message |
|---|---|---|---|
| 1 | `to <= now − 5 min` | `to` | `to must be at least 5 minutes ago` |
| 2 | `from < to` | `from` | `from must be before to` |
| 3 | when 2 holds: `to − from <= 30 days` | `from` | `the window is at most 30 days` |
| 4 | `max_targets` in `1..=10_000` | `max_targets` | `max_targets must be 1 to 10000` |
| 5 | `sample_rate`, when given, is finite and in `(0, 1]` | `sample_rate` | `sample_rate must be above 0 and at most 1` |

Every failing check is reported, in this order.

### Stored error strings

| Constant | Text | Where |
|---|---|---|
| `meta::backfills::ERROR_INTERRUPTED` | `interrupted` | startup recovery (D-I-39) |
| `eval::backfill::ERROR_RULE_DISABLED` | `rule disabled` | the rule was disabled during the run (D-I-41) |
| `eval::backfill::ERROR_RULE_CHANGED` | `rule changed` | a run field of the rule changed during the run (D-I-41) |
| `eval::backfill::ERROR_STORAGE_UNAVAILABLE` | `storage unavailable` | `QueryError::Busy`, `RunError::Timeout` |
| `eval::backfill::ERROR_INVALID_FILTER` | `invalid filter` | `RunError::Invalid`: the rule's stored filter no longer parses |
| `eval::backfill::ERROR_STORAGE_FAILED` | `storage read failed` | any other read or conversion error |
| `eval::backfill::ERROR_RULE` | `rule could not be read` | the stored rule or its project does not decode (`MetaError::Corrupt`) |
| `eval::backfill::ERROR_EVALUATOR` | `evaluator could not be read` | the rule's evaluator row is missing or does not decode |

The web maps each to a sentence (I6.6).

---

## I6.1 — The `backfills` table and its rows

**Goal:** migration `00MM_backfills.sql`; `meta/backfills.rs` to insert, list, read, update and fail backfills; `meta::eval_jobs::upsert_backfill_job`, D-I-11's backfill enqueue.

**Edit only:** `crates/server/migrations/meta/00MM_backfills.sql` (new), `crates/server/src/meta/backfills.rs` (new), `crates/server/src/meta/mod.rs` (module list), `crates/server/src/meta/eval_jobs.rs` (one function, with tests), `crates/server/tests/snapshots/meta_schema.snap`, `docs/spec/04-metadata-db.md`.

**Commit:** `Store online rule backfills`

### Migration

§ The migration number says how to choose `MM`.

```sql
CREATE TABLE backfills (
  id TEXT PRIMARY KEY,
  rule_id TEXT NOT NULL REFERENCES online_rules(id) ON DELETE CASCADE,
  from_ns INTEGER NOT NULL,
  to_ns INTEGER NOT NULL,
  max_targets INTEGER NOT NULL,
  sample_rate REAL NOT NULL,
  status TEXT NOT NULL,
  matched INTEGER NOT NULL DEFAULT 0,
  enqueued INTEGER NOT NULL DEFAULT 0,
  error TEXT,
  created_by TEXT REFERENCES users(id) ON DELETE SET NULL,
  created_at INTEGER NOT NULL,
  finished_at INTEGER
);
CREATE UNIQUE INDEX backfills_running ON backfills(rule_id) WHERE status = 'running';
CREATE INDEX backfills_rule ON backfills(rule_id, created_at);
```

**Existing tests:** only the `meta_schema` snapshot changes. Regenerate it with `INSTA_UPDATE=always cargo test -p cetra-server schema_matches_the_snapshot`. Its diff must add only the table, its two indexes and SQLite's automatic index of its primary key. Say so in the report.

### `meta/backfills.rs`

```rust
//! `backfills` rows: an online rule's runs over stored traces (spec 04 § Evaluation; D-I-25,
//! D-I-39 to D-I-42). A backfill is `running` until the runner finishes it; startup fails the
//! ones a stop or a crash left running.

/// A backfill the runner has not finished.
pub(crate) const STATUS_RUNNING: &str = "running";
/// Every page was read and enqueued.
pub(crate) const STATUS_DONE: &str = "done";
/// Stopped early; `error` says why.
pub(crate) const STATUS_FAILED: &str = "failed";
/// The error of a backfill that was running when the server stopped (D-I-39).
pub(crate) const ERROR_INTERRUPTED: &str = "interrupted";
/// Most backfills `list_backfills` returns (D-I-42).
const LIST_LIMIT: i64 = 50;

const STATUSES: [&str; 3] = [STATUS_RUNNING, STATUS_DONE, STATUS_FAILED];

/// One stored backfill, with its rule's project.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct BackfillRow {
    /// UUIDv7.
    pub(crate) id: Uuid,
    /// The rule it runs.
    pub(crate) rule_id: Uuid,
    /// The rule's project (read through `online_rules`).
    pub(crate) project_id: ProjectId,
    /// Window start, inclusive, nanoseconds.
    pub(crate) from_ns: i64,
    /// Window end, exclusive, nanoseconds.
    pub(crate) to_ns: i64,
    /// Most targets read.
    pub(crate) max_targets: u32,
    /// Sampling rate in `(0, 1]`.
    pub(crate) sample_rate: f64,
    /// One of `STATUSES`.
    pub(crate) status: String,
    /// Targets read so far.
    pub(crate) matched: u64,
    /// Jobs inserted or re-queued so far.
    pub(crate) enqueued: u64,
    /// Why a failed backfill stopped.
    pub(crate) error: Option<String>,
    /// User who started it.
    pub(crate) created_by: Option<Uuid>,
    /// Start time, nanoseconds.
    pub(crate) created_at: i64,
    /// When it reached `done` or `failed`, nanoseconds.
    pub(crate) finished_at: Option<i64>,
}

/// A backfill to insert. `Copy`, so `insert_backfill` takes it by value without a clippy
/// `needless_pass_by_value`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct NewBackfill {
    pub(crate) id: Uuid,
    pub(crate) rule_id: Uuid,
    pub(crate) from_ns: i64,
    pub(crate) to_ns: i64,
    pub(crate) max_targets: u32,
    pub(crate) sample_rate: f64,
    pub(crate) created_by: Option<Uuid>,
}

/// What one page's write did (D-I-40, D-I-41).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PageWrite {
    /// The jobs were written; `enqueued` were inserted or re-queued.
    Written { enqueued: u64 },
    /// The rule no longer exists (its backfills went with it).
    RuleGone,
    /// The rule is disabled; nothing was written.
    RuleDisabled,
}

/// How a backfill ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BackfillEnd {
    Done,
    Failed(&'static str),
}
```

Functions (doc comments in the style of `meta/online_rules.rs`, with `# Errors`):

| Function | What |
|---|---|
| `insert_backfill(conn: &mut SqliteConnection, new: NewBackfill, now: i64) -> Result<BackfillRow, MetaError>` | `INSERT INTO backfills (id, rule_id, from_ns, to_ns, max_targets, sample_rate, status, matched, enqueued, created_by, created_at) VALUES (?, ?, ?, ?, ?, ?, 'running', 0, 0, ?, ?)`, the error mapped with `super::unique_or(error, "a backfill of this rule is already running")` (the partial unique index), then read back on `conn` by id with the § columns query below. A row that cannot be read back is `MetaError::Corrupt { table: "backfills", column: "id" }`. |
| `list_backfills(pool: &SqlitePool, rule: Uuid) -> Result<Vec<BackfillRow>, MetaError>` | the rule's newest `LIST_LIMIT`: `… WHERE b.rule_id = ? ORDER BY b.created_at DESC, b.id DESC LIMIT 50`. |
| `running_backfills(pool: &SqlitePool) -> Result<Vec<BackfillRow>, MetaError>` | `running` backfills of active projects of active orgs, oldest first: `… JOIN projects p ON p.id = r.project_id JOIN orgs o ON o.id = p.org_id WHERE b.status = 'running' AND p.state = 'active' AND o.state = 'active' ORDER BY b.created_at, b.id`. |
| `write_page(conn: &mut SqliteConnection, backfill: Uuid, rule: Uuid, jobs: &[NewJob], matched: u64, now: i64) -> Result<PageWrite, MetaError>` | 1. `SELECT enabled FROM online_rules WHERE id = ?` → no row: `RuleGone`; `0`: `RuleDisabled` (write nothing). 2. `eval_jobs::upsert_backfill_job(conn, job, now)` for each `job` of `jobs` (a `&NewJob`), counting the `true`s. 3. `UPDATE backfills SET matched = ?1, enqueued = enqueued + ?2 WHERE id = ?3 AND status = 'running'`. → `Written { enqueued }`. `matched` is the run's total so far, not a delta. |
| `finish_backfill(conn: &mut SqliteConnection, id: Uuid, end: BackfillEnd, now: i64) -> Result<bool, MetaError>` | `UPDATE backfills SET status = ?, error = ?, finished_at = ? WHERE id = ? AND status = 'running'` (`Done` → `STATUS_DONE`, `NULL`; `Failed(text)` → `STATUS_FAILED`, `text`). True when a row changed. |
| `fail_running(conn: &mut SqliteConnection, now: i64) -> Result<u64, MetaError>` | startup: `UPDATE backfills SET status = 'failed', error = 'interrupted', finished_at = ? WHERE status = 'running'` (bind `ERROR_INTERRUPTED`); the rows changed. |

**The columns query.** Every read selects `b.id, b.rule_id, r.project_id, b.from_ns, b.to_ns, b.max_targets, b.sample_rate, b.status, b.matched, b.enqueued, b.error, b.created_by, b.created_at, b.finished_at FROM backfills b JOIN online_rules r ON r.id = b.rule_id`. Use a `macro_rules! backfill_columns` returning that string literal, as `eval_jobs.rs`'s `job_columns!` does, so the queries stay static. Decode through a private `BackfillRecord` (`sqlx::FromRow`) and `row_from(record) -> Result<BackfillRow, MetaError>`: UUIDs and the project id parse as `meta/online_rules.rs` parses them (copy its private helpers); `status` must be in `STATUSES`; `max_targets` through `u32::try_from`, `matched` and `enqueued` through `u64::try_from`; any failure is `MetaError::Corrupt { table: "backfills", column: <the column> }`.

### `meta/eval_jobs.rs`: `upsert_backfill_job`

```rust
/// Enqueues a backfill job (`kind = 'backfill'`, priority 2, D-I-11). A job with the same
/// dedup key that `failed` or was `skipped` is re-queued as a backfill job; a queued, running or
/// done one is left alone. True when a row was inserted or re-queued.
///
/// # Errors
///
/// Returns [`MetaError`] when the statement fails, or `MetaError::Corrupt` for a `job` whose
/// kind is not `KIND_BACKFILL` or that has no rule.
pub(crate) async fn upsert_backfill_job(conn: &mut SqliteConnection, job: &NewJob, now: i64) -> Result<bool, MetaError>;
```

Read `insert_rule_job` first and copy its checks (`MetaError::Corrupt { table: "eval_jobs", column: "kind" }` for another kind or `rule_id: None`). Do **not** copy its bindings: this statement has no `variable_mapping` placeholder (it is `NULL`), so the numbers differ. The statement, every placeholder numbered:

```sql
INSERT INTO eval_jobs
  (id, project_id, kind, priority, rule_id, evaluator_id, variable_mapping, trace_id,
   span_id, trace_start_ns, dedup_key, status, attempts, claims, load_retries,
   interruptions, not_before, created_at, updated_at)
VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, ?7, ?8, ?9, ?10, 'queued', 0, 0, 0, 0, ?11, ?11, ?11)
ON CONFLICT(dedup_key) DO UPDATE SET
  status = 'queued', kind = excluded.kind, priority = excluded.priority,
  attempts = 0, interruptions = 0, load_retries = 0,
  error_class = NULL, error = NULL, judge_trace_id = NULL, score_id = NULL,
  evaluator_version = NULL, input_tokens = NULL, output_tokens = NULL, cost_usd = NULL,
  latency_ms = NULL, finished_at = NULL,
  trace_start_ns = excluded.trace_start_ns,
  not_before = excluded.not_before, created_at = excluded.created_at,
  updated_at = excluded.updated_at
WHERE eval_jobs.status IN ('failed', 'skipped')
```

bound in this order, one `.bind` per number: `?1 = job.id.to_string()`, `?2 = job.project_id.to_string()`, `?3 = KIND_BACKFILL`, `?4 = i64::from(priority_of(KIND_BACKFILL))`, `?5 = rule_id.to_string()`, `?6 = job.evaluator_id.to_string()`, `?7 = job.trace_id.as_bytes().to_vec()`, `?8 = job.span_id.map(|span| span.as_bytes().to_vec())`, `?9 = job.trace_start_ns`, `?10 = dedup_key(job)`, `?11 = now`. `job` is borrowed (clippy's `needless_pass_by_value`), so the strings are built from it and nothing is moved. `claims` is not reset (it fences finishes and names judge traces, D-I-16). `evaluator_version` is reset too: it records which version the last run used. `rows_affected() > 0` is the answer (a skipped `DO UPDATE` affects no row).

### Spec

`docs/spec/04-metadata-db.md` § Evaluation: add the migration's DDL after `pending_traces`, and after the `pending_traces` paragraph: "`backfills` holds an online rule's runs over stored traces (spec 06 § Online rules › Backfill; D-I-25, D-I-39 to D-I-42). A backfill is `running` until the runner reads its last page; `matched` counts the targets read and `enqueued` the jobs inserted or re-queued. At most one backfill per rule is `running` (partial unique index). Startup marks the ones left `running` `failed` with `error = 'interrupted'`. Rows go with their rule."

### Tests

With `crate::meta::test_db()`, an org, a project, a check evaluator and rules from M3's functions (copy the setup of `meta/eval_jobs.rs`'s tests).

`meta/backfills.rs`:
- `insert_then_list_round_trips`: insert two (second one after finishing the first) → `list_backfills` returns both, newest first, with `project_id` set, `status` and the counters as inserted.
- `a_second_running_backfill_is_a_conflict`: two inserts for one rule → the second is `MetaError::Conflict("a backfill of this rule is already running")`; after `finish_backfill(…, Done, …)` of the first, a third insert succeeds.
- `the_list_holds_the_newest_fifty`: 51 finished rows with increasing `now` → 50, the oldest left out.
- `running_backfills_are_oldest_first_and_of_active_projects`: three running backfills on three rules, inserted with `now` 1, 2 and 3, one in a `deleting` project (M3's tests show the SQL) → the other two, oldest first.
- `write_page_writes_jobs_and_counters`: two backfill `NewJob`s passed as `&jobs`, `matched` 2 → `Written { enqueued: 2 }`; the row has `matched` 2, `enqueued` 2; a second call with the same jobs and `matched` 3 → `Written { enqueued: 0 }`, `matched` 3, `enqueued` 2.
- `write_page_needs_an_enabled_rule`: disable the rule → `RuleDisabled`, no job, counters unchanged; delete the rule → `RuleGone` (and the backfill row is gone too: cascade).
- `finish_backfill_ends_a_running_backfill_once`: `Failed("rule disabled")` → true, `status = 'failed'`, the error, `finished_at`; a second call → false.
- `fail_running_marks_interrupted`: one running, one done → returns 1; the running one is `failed`, `interrupted`; the done one is unchanged.
- `a_bad_status_is_corrupt`: set `status = 'paused'` by SQL → `list_backfills` is `MetaError::Corrupt`.

`meta/eval_jobs.rs`:
Every call in these tests passes `&job`.
- `upsert_backfill_job_inserts_a_backfill_job`: `kind = 'backfill'`, `priority = 2`, `status = 'queued'`, `dedup_key rule:<rule>:<trace>:-`.
- `upsert_backfill_job_requeues_failed_and_skipped_jobs`: insert a rule job (`insert_rule_job`), set by SQL `status = 'failed'`, `attempts = 6`, `claims = 7`, `error_class = 'retryable'`, `error = 'x'`, `score_id`, the usage columns, `finished_at`, `created_at = 1` → `true`; the row has `kind = 'backfill'`, `priority = 2`, `status = 'queued'`, `attempts = 0`, `claims = 7`, every reset column `NULL`, `created_at = not_before = updated_at = now`, the same `id`. Same for `skipped`.
- `upsert_backfill_job_leaves_live_jobs_alone`: queued, running and done rule jobs → `false`, rows unchanged (a queued rule job stays `kind = 'rule'`, priority 1).
- `upsert_backfill_job_refuses_other_kinds`: a rule `NewJob` → `MetaError::Corrupt`.

`insert_backfill`, `list_backfills`, `running_backfills`, `write_page`, `finish_backfill` and `fail_running` have no production caller before I6.4 and I6.5. Add the allows that § Conventions › Dead-code allows in M6 lists for I6.1, item by item, with their comments.

### Done when

`just check` passes.

---

## I6.2 — The backfill API contract

**Goal:** the M6 API types, their three OpenAPI operations, the example fixtures, the web mocks and query helpers. The routes come in I6.5; the web builds on this contract in I6.6.

**Edit only:** `crates/server/src/api/backfills.rs` (new), `crates/server/src/api/mod.rs` (module line), `crates/server/src/api/openapi.rs`, `crates/server/src/api_examples.rs`, `crates/server/tests/openapi.rs`, `crates/server/tests/api_examples.rs`, `crates/server/openapi.json`, `fixtures/api/static/{backfills,backfill,backfill-estimate}.json` (new), `fixtures/api/requests/{backfill-create,backfill-estimate}.json` (new), `fixtures/api/manifest.json`, `fixtures/api/README.md`, `web/src/api/schema.gen.ts`, `web/src/api/queries-online-rules.ts`, `web/src/mocks/writes.ts`, `web/src/mocks/writes.test.ts` (only if it lists operations by hand; read it first).

**Commit:** `Add the backfill API contract`

### Types (`api/backfills.rs`)

```rust
//! Online rule backfill API types (spec 09 § Evaluation & alerts; D-I-25, D-I-38 to D-I-42).
```

`CreateBackfillRequest`, `BackfillStatus`, `Backfill` and `BackfillEstimate`, exactly as § Backfill contract. Copy the time attributes of `OnlineRulePreview` (`#[serde(with = "cetra_core::time_serde")]` and `#[schema(value_type = String, format = DateTime)]`) for `from`, `to` and `created_at`, and those of `OnlineRule.paused_at` for `finished_at`. `CreateBackfillRequest` derives `Debug, Deserialize, ToSchema` with `#[serde(deny_unknown_fields)]`; its `sample_rate` is `#[serde(default, deserialize_with = "super::online_rules::present")] pub sample_rate: Option<f64>`. Doc every field with the § Backfill contract text. `max_targets` doc: "Most targets the backfill reads, 1 to 10 000 (D-I-25)."

Unit tests in the file: `Backfill` round-trips `static/backfill.json` and omits `error` and `finished_at` when absent; `CreateBackfillRequest` parses the request fixture, refuses an unknown field (`dry_run`) and `"sample_rate": null`; `BackfillStatus::from_name` is the inverse of `as_str` for the three values and `None` for `paused`.

### OpenAPI (`api/openapi.rs`)

Register, after the eval job schemas: `set.insert::<BackfillStatus>(); set.insert::<Backfill>(); set.insert_page::<Backfill>("BackfillPage"); set.insert::<CreateBackfillRequest>(); set.insert::<BackfillEstimate>();`. Add `fn backfill_paths(paths: PathsBuilder) -> PathsBuilder`, built like `online_rule_paths`, called right after `eval_job_paths(paths)`. Tag `evaluation`; the path parameter is `path_uuid("id", "Online rule id.")`.

| operation_id | Method and path | Request | 2xx | Errors | Description |
|---|---|---|---|---|---|
| `list_backfills` | `GET {PROJECT}/online-rules/{id}/backfills` | — | `200 BackfillPage` (`json_named("BackfillPage", "The rule's latest 50 backfills, newest first.")`) | `&[400, 401, 403, 404, 429]` | "Any project member can read. The newest 50 backfills of the rule; the list is not paged." |
| `post_backfill_estimate` | `POST {PROJECT}/online-rules/{id}/backfills/estimate` | `CreateBackfillRequest` | `200 BackfillEstimate` | `READ_ERRORS` | "Project members and above. API keys cannot call it. Counts the rule's targets in the window (at most `max_targets`, newest first, evaluation and experiment traces left out), how many the sample rate keeps, and, for an LLM judge, the tokens and cost they would use. Targets the rule already scored are counted; the run skips them (D-I-38, D-I-40)." |
| `post_backfill` | `POST {PROJECT}/online-rules/{id}/backfills` | `CreateBackfillRequest` | `202 Backfill` | `&[400, 401, 403, 404, 409, 429]` | "Project members and above. API keys cannot call it. Starts a backfill: up to `max_targets` targets of the window, newest first, are sampled and queued at the lowest priority, skipping targets the rule already scored. 409 when the rule is disabled or paused, or already has a running backfill (D-I-25)." |

`tests/openapi.rs`:
- `OPERATIONS`: the three `(method, path, operation_id)` tuples, after `post_evaluator_run`.
- `QUERY_OR_BODY`: add `post_backfill_estimate` (next to `post_online_rule_preview`).
- `expected_errors`: add `*id == "post_backfill"` to the branch of `post_online_rule` / `patch_online_rule`; `*id == "list_backfills"` to the branch of `get_online_rule`; `*id == "post_backfill_estimate"` to the branch that holds `post_online_rule_preview`.
- the two `pages` lists: `("BackfillPage", "Backfill")` in the first, `"BackfillPage"` in the second, next to the `EvalJobPage` entries (read how `EvalJobPage` appears in both, and copy).

### Fixtures and examples

- `static/backfills.json`: `{"data": [<the done backfill>, <the failed backfill>], "next_cursor": null}` (§ Fixed identifiers).
- `static/backfill.json`: the `202` answer.
- `static/backfill-estimate.json`: the estimate answer.
- `requests/backfill-create.json` and `requests/backfill-estimate.json`: the request body (the same content in both).
- `api_examples.rs`, after `post_evaluator_run`:
  - `example("list_backfills", params(&[("id", "01900000-0000-7000-8000-000000000700")]), BTreeMap::new(), "static/backfills.json", "application/json")` and the same entry with `"01900000-0000-7000-8000-000000000701"` (the paused span rule's page). `manifest_matches_the_operations` requires every path parameter but `project`, and the mocks (`web/src/mocks/handlers.ts`) serve only an entry whose parameters match, so these are the only two rules whose list is mocked; any other rule id answers `404` in the mocks;
  - `example("post_backfill_estimate", params(&[("id", "01900000-0000-7000-8000-000000000700")]), BTreeMap::new(), "static/backfill-estimate.json", "application/json")`;
  - `example_accepted("post_backfill", params(&[("id", "01900000-0000-7000-8000-000000000700")]), "static/backfill.json")`.
- `tests/api_examples.rs`: round-trip the three responses (`Page<Backfill>`, `BackfillEstimate`, `Backfill`) in the static list and in the manifest `match` (`"list_backfills" => roundtrip::<Page<Backfill>>(…)`, `"post_backfill_estimate" => …`, `"post_backfill" => …`); deserialize the two request files only (no `Serialize`), as the preview request is; add them to the request ↔ operation list.
- `just openapi`, then `just api-examples`.
- `fixtures/api/README.md` § Fixed identifiers: a row "Backfills | `…0b00` (`done`), `…0b01` (`failed`, `interrupted`), `…0b02` (`running`, the `202` answer of `post_backfill`), all of rule `…0700`. `backfills.json`, `backfill.json` and the requests use them."

### Web contract

- `web/src/api/queries-online-rules.ts`:
  ```ts
  export function backfillsQuery(org: string, project: string, ruleId: string) {
    return $api.queryOptions(
      "get",
      "/api/v1/projects/{project}/online-rules/{id}/backfills",
      onlineRuleInit(org, project, ruleId),
    );
  }

  /** The key of one rule's backfill list, for invalidation. */
  export function backfillsKey(org: string, project: string, ruleId: string) {
    return backfillsQuery(org, project, ruleId).queryKey;
  }
  ```
- `web/src/mocks/writes.ts`: import `backfill.json` and `backfill-estimate.json` as the other fixtures are imported; add `post_backfill_estimate` (`/api/v1/projects/{project}/online-rules/{id}/backfills/estimate`) and `post_backfill` (`…/backfills`) to `WRITE_OPERATIONS`, after `post_online_rule_resume`, and to `RESPONSES`: `post_backfill_estimate: () => HttpResponse.json(backfillEstimateDocument)`, `post_backfill: () => HttpResponse.json(backfillDocument, { status: 202 })`.

### Done when

`just check` passes; `openapi.json` has the three operations and `schema.gen.ts` the five schemas.

---

## I6.3 — Backfill reads: validation, paging, sampling, the scored check and the estimate

**Goal:** everything a backfill reads, with no table and no route: request validation, a pager over `trace_list`/`span_list` with strict exclusions, sampling, the already-scored check and the estimate. I6.4 (runner) and I6.5 (routes) call it.

**Edit only:** `crates/server/src/eval/backfill.rs` (new), `crates/server/src/eval/mod.rs` (module line; `EvalQueue.backfill_wake` and `wake_backfills`), `crates/server/src/scores.rs` (`rule_score_id` next to `derive_id`, and one test), `crates/server/src/entitlements/window.rs` (one entry in `direct_storage_reads_are_listed`).

**Commit:** `Read and estimate online rule backfills`

### `scores.rs`: `rule_score_id`

Right after `derive_id`:

```rust
/// The `score_id` that [`write`] derives for a rule's evaluator score on `target` without a
/// given id (D-I-18): the rule is the author and the evaluator kind gives the source. Backfill
/// looks these ids up to skip targets the rule already scored (D-I-40).
pub(crate) fn rule_score_id(
    project: ProjectId,
    target: &ScoreTarget,
    name: &str,
    kind: EvaluatorKind,
    rule: Uuid,
) -> Uuid {
    derive_id(project, target, name, evaluator_source(kind), rule)
}
```

Test `rule_score_id_matches_the_written_id`: copy the setup of the existing `Writer::Evaluator` tests in this file; `write` one score with `Writer::Evaluator { kind: EvaluatorKind::LlmJudge, evaluator_id, rule_id: Some(rule) }` and no `score_id` on a trace target → the returned id equals `rule_score_id(project, &target, name, EvaluatorKind::LlmJudge, rule)`. Same on a span target. If `write` normalizes the name before deriving (trim, case), the test shows it; then apply the same normalization inside `rule_score_id` and say so in the report.

### `eval/mod.rs`

- `pub(crate) mod backfill;` in the module list.
- `EvalQueue` gains, after `wake`:
  ```rust
  /// Wakes the backfill runner: a backfill was created (D-I-39).
  backfill_wake: Notify,
  ```
  (`Notify::new()` in `new`), and
  ```rust
  /// Wakes the backfill runner.
  pub(crate) fn wake_backfills(&self) {
      self.backfill_wake.notify_one();
  }
  ```

### `entitlements/window.rs`

In the test `direct_storage_reads_are_listed`, add `"eval/backfill.rs"` to the allowed list, after `"eval/trace_load.rs"`. Nothing else in the file changes.

### `eval/backfill.rs`

```rust
//! Online rule backfill (D-I-25, D-I-38 to D-I-42): validation, the paged reads, sampling, the
//! already-scored check and the estimate. I6.4 adds the runner.

/// Most rows one page reads (storage's `MAX_PAGE`).
const PAGE: usize = cetra_storage::MAX_PAGE;
/// Time allowed for one page read or one scored-ids read.
const READ_TIMEOUT: Duration = Duration::from_secs(10);
/// Most targets one backfill reads (D-I-25).
pub(crate) const MAX_TARGETS: u32 = 10_000;
/// Widest window (D-I-25).
pub(crate) const MAX_WINDOW_NS: i64 = 30 * DAY_NS;
/// `to` must be at least this far in the past (D-I-25).
pub(crate) const SETTLE_NS: i64 = 5 * 60 * 1_000_000_000;
/// Characters the estimate counts for each mapped variable (D-I-25).
const CHARS_PER_VARIABLE: usize = 2_000;

pub(crate) const ERROR_RULE_DISABLED: &str = "rule disabled";
pub(crate) const ERROR_STORAGE_UNAVAILABLE: &str = "storage unavailable";
pub(crate) const ERROR_INVALID_FILTER: &str = "invalid filter";
pub(crate) const ERROR_STORAGE_FAILED: &str = "storage read failed";
pub(crate) const ERROR_RULE: &str = "rule could not be read";
pub(crate) const ERROR_EVALUATOR: &str = "evaluator could not be read";
```

`DAY_NS` is `crate::eval::budget::DAY_NS`. Doc every constant as § Stored error strings says.

**Validation.**

```rust
/// A checked backfill window and its limits (D-I-42).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Window {
    pub(crate) from: i64,
    pub(crate) to: i64,
    pub(crate) max_targets: u32,
    pub(crate) sample_rate: f64,
}

/// Checks a request at `now` (§ Validation). `sample_rate` absent takes `rule_rate`.
///
/// # Errors
///
/// Every failing field, in the table's order.
pub(crate) fn validate(from: i64, to: i64, max_targets: u32, sample_rate: Option<f64>, rule_rate: f64, now: i64) -> Result<Window, Vec<FieldError>>;
```

Use `crate::eval::rule_check::field_error` for each error, with § Validation's fields and messages. Use saturating arithmetic (`now.saturating_sub(SETTLE_NS)`, `to.saturating_sub(from)`).

**The rule.**

```rust
/// What a backfill needs of its rule.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct BackfillRule {
    pub(crate) id: Uuid,
    pub(crate) project: ProjectId,
    pub(crate) org: OrgId,
    pub(crate) target: RuleTarget,
    /// The rule's filter, as stored.
    pub(crate) filter: Option<String>,
}

impl BackfillRule {
    /// From a stored rule and its project's org.
    ///
    /// # Errors
    ///
    /// `MetaError::Corrupt { table: "online_rules", column: "target" }` for a stored target
    /// that `RuleTarget::from_name` refuses.
    pub(crate) fn new(rule: &RuleRow, org: OrgId) -> Result<Self, MetaError>;
}
```

**Read errors.**

```rust
/// A backfill read that failed (D-I-41).
#[derive(Debug, thiserror::Error)]
pub(crate) enum BackfillReadError {
    #[error("storage query could not start: {0}")]
    Query(#[from] QueryError),
    #[error("storage read failed: {0}")]
    Run(#[from] RunError),
    #[error("stored rows could not be converted: {0}")]
    Convert(#[from] ConvertError),
}

impl BackfillReadError {
    /// The error a failed backfill stores (§ Stored error strings).
    pub(crate) fn stored(&self) -> &'static str;
}

impl From<BackfillReadError> for ApiError { … }
```

`stored`: `Query(QueryError::Busy)` and `Run(RunError::Timeout)` → `ERROR_STORAGE_UNAVAILABLE`; `Run(RunError::Invalid(_))` → `ERROR_INVALID_FILTER`; everything else → `ERROR_STORAGE_FAILED`. The `From` impl: `Query(error)` → `ApiError::from(error)`, `Run(error)` → `ApiError::from(error)` (so `Invalid` is `400 invalid_filter`, `Timeout` is `422`, `Busy` is `503`, as on the read routes), `Convert(error)` → `ApiError::Internal(Box::new(error))`. `ConvertError` is `crate::api::convert::ConvertError`. Check that the `Display` of `QueryError`, `RunError` and `ConvertError` never quotes row content (read them); they do not when this plan was written.

**The pager.**

```rust
/// One page of matches, newest first, and the query context it was read with (the scored
/// check reuses it).
pub(crate) struct TargetPage {
    pub(crate) ctx: QueryContext,
    pub(crate) rows: Vec<MatchRow>,
}

/// Walks a rule's matches in a window, newest first, page by page, up to `limit` rows
/// (D-I-40). Each page opens its own query context, so a long run never holds one storage
/// snapshot.
pub(crate) struct Pager<'a> {
    state: &'a AppState,
    rule: &'a BackfillRule,
    window: TimeWindow,
    limit: usize,
    page: usize,
    read: usize,
    cursor: Option<String>,
    done: bool,
}

impl<'a> Pager<'a> {
    pub(crate) fn new(state: &'a AppState, rule: &'a BackfillRule, window: TimeWindow, limit: usize) -> Self; // page = PAGE

    /// A smaller page, so tests walk several pages over a few traces.
    #[cfg(test)]
    fn with_page(self, page: usize) -> Self;

    /// The next page, or `None` once `limit` rows were read or the list ended.
    ///
    /// # Errors
    ///
    /// [`BackfillReadError`] when the query cannot start, the read fails or a row does not convert.
    pub(crate) async fn next_page(&mut self) -> Result<Option<TargetPage>, BackfillReadError>;
}
```

`next_page`, in order:
1. `if self.done || self.read >= self.limit { return Ok(None) }`. `ask = self.page.min(self.limit - self.read)`.
2. `#[cfg(test)] if let Some(error) = faults::take_read() { return Err(error.into()); }` (§ Tests).
3. `let source = self.state.storage.query(self.rule.project, query_options(Visibility::window(self.state, self.rule.org), None)).await?;` then `let ctx = QueryContext::new(source, None)?;` (as `eval/trigger.rs` opens its reads).
4. The read, with `scope = ListScope { window: Some(self.window), include_eval: false, include_experiments: false }`, `strict_exclusions: true`, `search: None`, `filter: self.rule.filter.as_deref()`, `limit: ask`, `cursor: self.cursor.as_deref()`, `timeout: READ_TIMEOUT`:
   - `RuleTarget::Trace` → `cetra_storage::trace_list(&ctx, TraceListRequest { …, sort: TraceSort::StartTime, order: SortOrder::Desc })`;
   - `RuleTarget::Span` → `cetra_storage::span_list(&ctx, SpanListRequest { …, sort: SpanSort::StartTime, order: SortOrder::Desc })`.
   (Use the storage crate's `TraceSort`, `SpanSort` and `SortOrder`, not the API's.)
5. `rows = convert::match_rows(&page.batches, rule.target == RuleTarget::Span)?` (it reads `trace_id`, `span_id` and `start_time` by name, so list batches work). `self.read += rows.len(); self.cursor = page.next_cursor; self.done = self.cursor.is_none() || rows.is_empty();`.
6. `Ok(Some(TargetPage { ctx, rows }))`.

**Sampling.**

```rust
/// The rows `sampling::sampled` keeps for `rule` at `rate`, in order (D-I-10, D-I-40).
pub(crate) fn sampled_rows(rule: Uuid, rate: f64, rows: Vec<MatchRow>) -> Vec<MatchRow>;
```

Each row with `sampling::sampled(rate, rule, row.trace_id, row.span_id)`. There is no per-trace cut (D-I-40).

**The scored check.**

```rust
/// What the scored check needs of the rule's evaluator (D-I-40).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Scoring {
    /// The evaluator's current output score name.
    pub(crate) name: String,
    pub(crate) kind: EvaluatorKind,
}

impl Scoring {
    /// From the stored evaluator, without opening a webhook's sealed values. `None` when the
    /// kind or the output score does not decode.
    pub(crate) fn from_row(row: &EvaluatorRow) -> Option<Self>;
}

/// `rows` without the targets that already have this rule's score (one `scores_by_id` read).
/// No rows, no read.
///
/// # Errors
///
/// [`BackfillReadError`] when the read fails or a score row does not convert.
pub(crate) async fn unscored(ctx: &QueryContext, rule: &BackfillRule, scoring: &Scoring, rows: Vec<MatchRow>) -> Result<Vec<MatchRow>, BackfillReadError>;
```

- `Scoring::from_row`: `kind = EvaluatorKind::from_name(&row.kind)?`; the name is the `name` of the output score parsed from `row.output_score` with the type `eval/evaluator.rs`'s `load` parses it into (`rg -n "output_score" crates/server/src/eval/evaluator.rs`).
- `unscored`: for each row, `target` is `ScoreTarget::Span { trace_id, span_id }` when `span_id` is `Some`, else `ScoreTarget::Trace { trace_id }`; `id = scores::rule_score_id(rule.project, &target, &scoring.name, scoring.kind, rule.id)`. Collect the ids as hyphenated strings, `cetra_storage::scores_by_id(ctx, &refs, READ_TIMEOUT).await?`, then `convert::scores(&batches, rule.project)?` into a `HashSet<Uuid>` of `score_id`s; keep the rows whose id is not in it, in order. A page holds at most `PAGE` = 1 000 rows, which is `scores_by_id`'s limit.

**The estimate.**

```rust
/// What the estimate needs of a judge evaluator (D-I-42).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct JudgeCost {
    /// `chars().count()` of the prompt template.
    pub(crate) template_chars: usize,
    /// Mapped variables of the rule.
    pub(crate) variables: usize,
    pub(crate) max_output_tokens: u32,
    /// The model's price (`budget::model_price`), `None` when it has none.
    pub(crate) price: Option<PriceRow>,
}

/// Tokens and cost of the sampled targets.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct CostEstimate {
    pub(crate) input_tokens: u64,
    pub(crate) output_tokens: u64,
    pub(crate) cost_usd: Option<f64>,
}

/// What a backfill would do (D-I-38, D-I-42).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Estimate {
    pub(crate) matched: u64,
    pub(crate) capped: bool,
    pub(crate) sampled: u64,
    /// Judges only.
    pub(crate) cost: Option<CostEstimate>,
}

/// D-I-25's estimate for `sampled` targets.
pub(crate) fn estimate_cost(sampled: u64, judge: &JudgeCost) -> CostEstimate;

/// Reads up to `max_targets + 1` rows; `matched` is at most `max_targets`, `capped` says more
/// matched, and `sampled` counts the kept ones among the first `max_targets`.
///
/// # Errors
///
/// [`BackfillReadError`] from a page read.
pub(crate) async fn estimate(state: &AppState, rule: &BackfillRule, window: Window, judge: Option<&JudgeCost>) -> Result<Estimate, BackfillReadError>;
```

- `estimate_cost`: `chars = judge.template_chars + CHARS_PER_VARIABLE * judge.variables` (saturating); `per_input = chars.div_ceil(4) as u64`; `input_tokens = sampled.saturating_mul(per_input)`; `output_tokens = sampled.saturating_mul(u64::from(judge.max_output_tokens))`; `cost_usd = judge.price.as_ref().map(|price| sampled as f64 * budget::estimate_usd(chars, judge.max_output_tokens, price)).filter(|cost| cost.is_finite())`.
- `estimate`: `TimeWindow::new(window.from, window.to)?` (a `RunError`, converted by `?`); `limit = window.max_targets as usize + 1`; walk a `Pager`, keeping every row in one `Vec` (at most 10 001). `capped = rows.len() > max_targets`; truncate to `max_targets`; `matched = rows.len()`; `sampled = sampled_rows(rule.id, window.sample_rate, rows).len()`; `cost = judge.map(|judge| estimate_cost(sampled, judge))`.

### Tests (`eval/backfill.rs`)

A `#[cfg(test)] mod faults` with one thread-local, copied from `eval/trigger.rs`'s: `fail_next_read(error: RunError)` and `take_read() -> Option<RunError>`.

Pure:
- `validate_accepts_a_window_and_defaults_the_rate`: a 7-day window ending 10 minutes before `now`, `max_targets` 1000, no rate, rule rate 0.25 → `Window { …, sample_rate: 0.25 }`.
- `validate_reports_every_field`: `to` 1 minute before `now`, `from` after `to`, `max_targets` 0, `sample_rate` 1.5 → four errors in the table's order (`to`, `from`, `max_targets`, `sample_rate`).
- `validate_caps_the_window`: 30 days exactly → `Ok`; 30 days + 1 ns → `from: the window is at most 30 days`; `max_targets` 10 000 → `Ok`, 10 001 → the error; `sample_rate` 0.0, −1, `NAN` → the error; 1.0 → `Ok`.
- `estimate_cost_counts_tokens_and_cost`: 4 targets, template 100 characters, 2 variables, 50 output tokens, a priced row (build a `PriceRow` as `eval/budget.rs`'s tests do) → `input_tokens = 4 × 1025`, `output_tokens = 200`, `cost_usd = 4 × estimate_usd(4100, 50, &price)`.
- `estimate_cost_without_a_price_has_no_cost`.
- `stored_errors_follow_the_kind`: `Busy` and `Timeout` → `storage unavailable`; `RunError::Invalid(…)` (build one by validating a bad filter, or with the variant's constructor; read `query_error.rs`) → `invalid filter`; `RunError::Failed(…)` and `RunError::InvalidRequest(…)` → `storage read failed`.

With `TestApp` (copy `set_clock`, `trace_id`, `span_id`, `span` and their imports, `dataset_span` included, from `eval/trigger.rs`'s tests; rules with M3's `insert_rule` on a check evaluator; spans committed with `app.commit_spans`; windows built around the committed `start_time`s):
- `pages_walk_newest_first_until_the_limit`: five traces → `Pager::new(…, limit 4).with_page(2)` returns pages of 2 and 2, newest first by `start_time`, then `None`; with limit 10: 2, 2, 1, then `None`.
- `the_window_is_half_open`: a trace at `from` is read, one at `to` is not.
- `pages_use_the_rule_filter_and_strict_exclusions`: filter `status = 'error'` over an ok trace, an error trace and a`cetra-eval` error trace → one row; a rule filtered on `environment = 'cetra-eval'` → no row.
- `a_span_rule_pages_spans`: a trace with three spans, rule filter `span_kind = 'llm'` on two → two rows with their `span_id`s.
- `a_failed_read_is_a_read_error`: `faults::fail_next_read(RunError::Timeout)` → `Err`, `stored()` is `storage unavailable`.
- `sampled_rows_match_the_live_sampler`: 20 traces at rate 0.5 → exactly the rows for which `sampling::sampled(0.5, rule, trace, None)` is true.
- `unscored_drops_targets_the_rule_scored`: three traces; write one score through `scores::write` with `Writer::Evaluator { kind: Check, evaluator_id, rule_id: Some(rule) }` on trace 2, named as the evaluator's output → `unscored` keeps traces 1 and 3. A score of another rule on trace 3 does not drop it. No rows → `Ok(vec![])`.
- `estimate_counts_matched_capped_and_sampled`: five traces, rate 1: `max_targets` 3 → `matched 3, capped true, sampled 3`; `max_targets` 10 → `matched 5, capped false`. A check rule → `cost: None`.

Add the allows that § Conventions › Dead-code allows in M6 lists for I6.3.

### Done when

`just check` passes.

---

## I6.4 — The backfill runner and startup recovery

**Goal:** a loop runs each `running` backfill to its end (D-I-39, D-I-40, D-I-41); startup fails the backfills a stop or a crash left running.

**Edit only:** `crates/server/src/eval/backfill.rs` (the runner and its tests; remove the I6.4 allows), `crates/server/src/eval/mod.rs` (`EvalTiming.backfill_every`, the loop in `spawn`, `recover`), `crates/server/src/eval/dispatch.rs` (one test, next to the existing `recover` test), `crates/server/src/meta/backfills.rs` (remove the I6.4 allows only), `docs/spec/06-evaluation.md`, `docs/spec/12-operations.md`.

**Commit:** `Run online rule backfills`

### The runner (`eval/backfill.rs`)

```rust
/// How one run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RunEnd {
    /// Every page was read; the backfill is `done`.
    Done,
    /// Stopped early; the backfill is `failed`.
    Failed,
    /// The rule or its project is gone; nothing left to update.
    Gone,
    /// The server is stopping; the backfill stays `running` (D-I-39).
    Stopped,
}

/// Runs every `running` backfill of an active project, oldest first, one at a time (D-I-39).
/// Returns how many ended `Done` or `Failed`. One backfill's `meta.db` error is logged and the
/// pass goes on to the next (D-I-41).
///
/// # Errors
///
/// [`MetaError`] only when the list of running backfills cannot be read.
pub(crate) async fn backfill_once(state: &AppState) -> Result<usize, MetaError>;

async fn run_backfill(state: &AppState, row: &BackfillRow) -> Result<RunEnd, MetaError>;
```

`backfill_once`: `rows = backfills::running_backfills(state.meta.reader()).await?`; for each, return early when `state.eval.is_stopped()`, then `match run_backfill(state, &row).await`:
- `Ok(Done | Failed)` → count it; `Ok(Gone | Stopped)` → nothing;
- `Err(MetaError::Corrupt { .. })` (a stored rule, project or evaluator that does not decode) → one `state.meta.write` with `finish_backfill(conn, row.id, BackfillEnd::Failed(ERROR_RULE), now)`, `tracing::warn!(backfill_id = %row.id, rule_id = %row.rule_id, error = %error, "a backfill's rule could not be read")`, count it (a failed finish write is logged at `error` and not counted);
- `Err(error)` of any other variant (`MetaError::Db` and the rest) → `tracing::error!(backfill_id = %row.id, error = %error, "a backfill pass failed; it runs again from the start next pass")`, and go on to the next row. The backfill stays `running`.

So one bad backfill never holds up the others, and a decode error does not keep its rule locked (a `running` backfill makes new ones `409`).

`run_backfill`, in order (every `now` is `state.clock.now_ns()` when the write is made):
1. `project = meta::projects::project_by_id(reader, row.project_id)` → `None` → `Gone`. `rule = meta::online_rules::get_rule(reader, row.project_id, row.rule_id)` → `None` → `Gone`. `rule = BackfillRule::new(&rule_row, project.org_id)?` (a bad stored target is `MetaError::Corrupt`, which `backfill_once` turns into `rule could not be read`).
2. `evaluator = meta::evaluators::get_evaluator(reader, row.project_id, rule_row.evaluator_id)`; `Scoring::from_row`. A missing evaluator or `None` → finish `Failed(ERROR_EVALUATOR)` → `Failed`.
3. `window = TimeWindow::new(row.from_ns, row.to_ns)` → an error → finish `Failed(ERROR_STORAGE_FAILED)` → `Failed` (validated rows cannot hit it).
4. `pager = Pager::new(state, &rule, window, row.max_targets as usize)`; `matched: u64 = 0`; `enqueued: u64 = 0`. Loop:
   - `if state.eval.is_stopped() { return Ok(RunEnd::Stopped) }`;
   - `page = pager.next_page().await` → `Err(error)` → `tracing::warn!(backfill_id = %row.id, rule_id = %row.rule_id, error = error.stored(), "a backfill read failed")`, finish `Failed(error.stored())` → `Failed`. `Ok(None)` → finish `Done` → `Done`;
   - `matched += rows.len()`; `kept = sampled_rows(rule.id, row.sample_rate, page.rows)`; `kept = unscored(&page.ctx, &rule, &scoring, kept).await` (an error: as the read error above);
   - `jobs`: one `NewJob { id: Uuid::now_v7(), project_id: row.project_id, kind: KIND_BACKFILL, rule_id: Some(rule.id), evaluator_id: rule_row.evaluator_id, variable_mapping: None, trace_id, span_id, trace_start_ns: start_time }` per kept row;
   - one `state.meta.write(move |conn| Box::pin(async move { backfills::write_page(conn, id, rule_id, &jobs, matched, now).await }))` (the closure owns `jobs` and lends it; write it even when `jobs` is empty, so `matched` moves): `Written { enqueued: n }` → `enqueued += n`, and `state.eval.wake()` when `n > 0`; `RuleGone` → `Gone`; `RuleDisabled` → finish `Failed(ERROR_RULE_DISABLED)` → `Failed`. A `MetaError` returns `Err`.
5. Finishing is one `state.meta.write` with `backfills::finish_backfill(conn, id, end, now)`; its `bool` is ignored (a row that changed since is left alone).
6. After `Done` or `Failed`, `tracing::info!(backfill_id = %row.id, rule_id = %row.rule_id, project_id = %row.project_id, matched, enqueued, status, "backfill finished")` with `status` the stored status string. `Gone` logs `debug!` with the ids.

A `RuleGone` or a deleted project makes the rule's backfill rows go with the rule (cascade), so nothing is left to mark (D-I-41).

### The loop and recovery (`eval/mod.rs`)

- `EvalTiming` gains `/// Backfill runner tick; a created backfill also wakes it (D-I-39). pub(crate) backfill_every: Duration,` with `DEFAULT` 5 s and `TEST` 50 ms. (`dispatch.rs` builds one literal with `..EvalTiming::TEST`; it needs no edit.)
- `spawn` adds `tokio::spawn(run_backfills(state.clone(), stop.clone(), timing.backfill_every))` before the budget ticker (which takes `stop` by value), and its doc lists "the backfill runner".
- ```rust
  /// The backfill runner (D-I-39). A pass runs to its end before the next; it stops between pages
  /// when the server stops.
  async fn run_backfills(state: AppState, mut stop: watch::Receiver<bool>, every: Duration)
  ```
  On K's pattern, like `run_sweeper`, with one more `select!` arm: `() = state.eval.backfill_wake.notified() => {}`. `Ok(finished)` with `finished > 0` logs `tracing::debug!(finished, "backfill pass")`; `Err(error)` logs `tracing::error!(error = %error, "backfill pass failed")`.
- `recover`: in the same `MetaDb::write` closure, after `eval_jobs::recover(conn, now)`, call `crate::meta::backfills::fail_running(conn, now)`; return the three counts. When the backfill count is above 0, `tracing::info!(backfills = count, "running backfills marked interrupted at startup")`. Update the function's doc: "… and `running` backfills fail as `interrupted` (D-I-39)."

Remove the I6.4 rows of § Conventions › Dead-code allows in M6.

### Spec

- `docs/spec/06-evaluation.md` § Online rules: delete the **Backfill** bullet of § Execution, and add after § Execution:

  "### Backfill

  "Run on existing data" runs a rule over stored traces (D-I-25). `POST /online-rules/{id}/backfills/estimate` `{from, to, max_targets, sample_rate?}` answers how many targets the window holds (at most `max_targets`, and whether more matched), how many the sample keeps, and for a judge the tokens and cost: per target `ceil((template characters + 2 000 × mapped variables) / 4)` input tokens and `max_output_tokens` output tokens at the model's price (D-I-38, D-I-42). `POST /online-rules/{id}/backfills` with the same body starts one: the window is at most 30 days and ends at least 5 minutes ago, `max_targets` is 1–10 000, `sample_rate` defaults to the rule's; the rule must be enabled and unpaused, and a rule runs one backfill at a time (`409` otherwise).

  A backfill runner, one of the eval loops, runs each backfill to its end, oldest first (D-I-39). It reads `trace_list` or `span_list` page by page, newest first, with the rule's filter and strict exclusions, so evaluation and experiment traces are never targets (D-I-17); the plan's data window applies (D-I-34). Each target is sampled as live targets are (D-I-10); targets the rule already scored are skipped by looking up their derived score ids; the rest are queued as backfill jobs at the lowest priority, re-queuing a failed or skipped job of the same target (D-I-11, D-I-40). The rule's budget applies when the jobs run (D-I-15), and queued backfill jobs expire after 7 days (D-I-28). A failed read fails the backfill and keeps the jobs already queued; a rule disabled during a run fails it; a deleted rule takes its backfills with it; a backfill running at shutdown is marked `failed` (`interrupted`) at the next start (D-I-41). There is no cancel; disabling the rule stops its jobs."
- `docs/spec/12-operations.md` § Process lifecycle:
  - **Startup**, after "Then `running` eval jobs go back to `queued` (a job interrupted a fourth time fails)": add ", `running` backfills are marked `failed` (`interrupted`, D-I-39)"; in the list of loops that start, add "the backfill runner" after "the trigger's flusher and sweeper".
  - **Shutdown**, after "stop the eval loops": add "(the backfill runner stops between pages; if a page read is still in flight when the eval loops' drain ends, the runner is aborted with them; either way the backfill stays `running` and the next start fails it as `interrupted`, D-I-39)".

### Tests (`eval/backfill.rs`, `TestApp`)

Helpers: the same copied trigger helpers as I6.3 (`set_clock`, `trace_id`, `span_id`, `span` on `dataset_span(0)`, and their imports); a project with a check evaluator (no provider: the runner only enqueues); `rule(app, target, filter, rate) -> Uuid` with M3's `insert_rule`; `backfill(app, rule, from, to, max_targets, rate) -> Uuid` with `insert_backfill` in a write; `jobs(app)` and `backfill_row(app, id)` read by SQL. Windows are built around the committed `start_time`s; the runner does not check `to ≤ now − 5 min` (the route does). Every backfill a test inserts states its `max_targets`: 10 000 unless the test says otherwise. A test that inserts several backfills and depends on their order calls `app.state.clock.advance(Duration::from_secs(1))` between the inserts, so their `created_at` differ.

- `a_backfill_queues_its_window`: three traces, two inside the window and one after `to` → `backfill_once` returns 1; the row is `done` with `matched 2, enqueued 2, finished_at` set; two jobs `kind backfill`, `priority 2`, `status queued`, `dedup_key rule:<rule>:<trace>:-`, `trace_start_ns` = each trace's `start_time`.
- `max_targets_keeps_the_newest`: five traces, `max_targets` 2 → the two newest traces have jobs; `matched 2`.
- `a_span_rule_queues_spans`: filter `span_kind = 'llm'`, two matching spans in one trace → two jobs with their `span_id`s.
- `evaluation_traces_are_never_targets`: a`cetra-eval` trace and a rule filtered on `environment = 'cetra-eval'` → `done`, `matched 0`, no job.
- `sampling_matches_the_live_sampler`: 20 traces at backfill rate 0.5 → the job set equals the traces for which `sampling::sampled(0.5, rule, trace, None)` is true.
- `scored_targets_are_skipped`: write this rule's score on one of three traces (`scores::write`, `Writer::Evaluator { rule_id: Some(rule), .. }`) → two jobs; `matched 3, enqueued 2`.
- `a_failed_job_is_requeued_and_a_done_one_is_not`: insert rule jobs for traces 1 and 2, set trace 1's to `failed` and trace 2's to `done` by SQL → after the run, trace 1's row is `queued`, `kind backfill`; trace 2's stays `done`, `kind rule`; `enqueued 1`.
- `a_second_run_queues_nothing_new`: run, then insert a second backfill of the same window and run → `matched` the same, `enqueued 0`.
- `a_disabled_rule_fails_the_backfill`: disable the rule by SQL before `backfill_once` → `failed`, `rule disabled`, no job.
- `a_failed_read_fails_the_backfill`: `faults::fail_next_read(RunError::Timeout)` → `failed`, `storage unavailable`; another backfill with `RunError::Failed("x".to_owned())` → `storage read failed`.
- `a_deleted_rule_takes_its_backfill`: delete the rule → `backfill_once` returns 0 and no row is left.
- `a_corrupt_rule_fails_its_backfill_and_the_next_one_runs`: two rules, a backfill each (advance the clock 1 s between the inserts, so the first is older); set the first rule's `target = 'bogus'` by SQL → `backfill_once` returns 2; the first backfill is `failed`, `rule could not be read`; the second is `done` with its jobs.
- `an_invalid_stored_filter_fails_the_backfill`: set the rule's `filter = 'status = = 1'` by SQL → `failed`, `invalid filter`.
- `a_stopped_runner_leaves_the_backfill_running`: `state.eval.stop()` → `backfill_once` returns 0; the row is still `running`.
- `pages_record_progress`: `backfill_once` always uses 1 000-row pages, so commit 1 200 traces in one `commit_spans` and run one backfill with `max_targets` 10 000 (two pages, 1 000 and 200): `matched 1200`, `enqueued 1200`. If committing 1 200 traces makes the test take more than 10 s, use 1 001.
- `recover_fails_running_backfills`, in `eval/dispatch.rs`'s tests right after `recover_requeues_and_fails_crash_loops` (copy its setup): insert a check evaluator and a rule first (`backfills.rule_id` is a foreign key), then a running and a done backfill of that rule → `crate::eval::recover(&app.state)` → the running one is `failed`, `interrupted`, with `finished_at`; the done one is unchanged.
- `the_loop_runs_a_backfill_when_woken` (`#[tokio::test(flavor = "multi_thread")]`): `let tasks = crate::eval::spawn(&app.state, EvalTiming { backfill_every: Duration::from_secs(3_600), ..EvalTiming::TEST })`; wait 200 ms for the loops' first passes; insert a backfill, `state.eval.wake_backfills()`; poll the row every 20 ms for at most 5 s until `done`; `tasks.shutdown(&app.state).await`.

### Done when

`just check` passes. No I6.4 allow is left (`rg -n "I6\.4 calls it" crates/server` is empty).

---

## I6.5 — The backfill routes

**Goal:** `GET …/online-rules/{id}/backfills`, `POST …/backfills/estimate` and `POST …/backfills` (D-I-38, D-I-42).

**Edit only:** `crates/server/src/routes/backfills.rs` (new), `crates/server/src/routes/mod.rs` (module line), `crates/server/src/http/access.rs` (one `routes.extend` line), `crates/server/src/routes/online_rules.rs` (`async fn judge_adapter` becomes `pub(crate) async fn judge_adapter`; nothing else), `crates/server/src/eval/backfill.rs`, `crates/server/src/eval/mod.rs` and `crates/server/src/meta/backfills.rs` (remove the I6.5 allows only), `docs/spec/09-api.md`.

**Commit:** `Add the online rule backfill routes`

### Routes (`routes/backfills.rs`)

```rust
//! Online rule backfill routes (spec 09 § Evaluation & alerts, spec 06 § Online rules ›
//! Backfill; D-I-25, D-I-38 to D-I-42).
```

| Method | Path | Perm | Handler |
|---|---|---|---|
| GET | `/api/v1/projects/{project}/online-rules/{id}/backfills` | `View` | `list_backfills` |
| POST | `/api/v1/projects/{project}/online-rules/{id}/backfills/estimate` | `ManageEvaluation` | `estimate_backfill` |
| POST | `/api/v1/projects/{project}/online-rules/{id}/backfills` | `ManageEvaluation` | `create_backfill` |

`pub(crate) fn routes() -> Vec<RouteDef>` as in `routes/online_rules.rs` (`Access::Project`, `session_only: false`, `documented: true`). `http/access.rs`: `routes.extend(crate::routes::backfills::routes());` right after the `online_rules` line.

**`list_backfills`:** `store::get_rule(reader, project, id)` → `None` → `404`; `backfills::list_backfills(reader, id)`; answer `Page { data: <each row through backfill_json>, next_cursor: None }` (`crate::api::common::Page`).

**`estimate_backfill`** (`error::Json<CreateBackfillRequest>`):
1. `now = state.clock.now_ns()`; rule as in the list (`404`).
2. `window = backfill::validate(body.from.ns(), body.to.ns(), body.max_targets, body.sample_rate, rule.sample_rate, now)`; `Err(errors)` → `400` (`ApiError::BadRequest { detail: None, errors }`).
3. `rule_spec = BackfillRule::new(&rule, access.project.org_id)?` (a `MetaError` converts).
4. `judge`: the evaluator row (`evaluators::get_evaluator(reader, project, rule.evaluator_id)`, missing → `ApiError::Internal` with `MetaError::Corrupt { table: "online_rules", column: "evaluator_id" }`), then `rule_check::Subject::from_row(&row)` (an error → `ApiError::Internal`). For `Subject::Judge(judge)`: its adapter through `super::online_rules::judge_adapter(&state, org, &subject).await?` (made `pub(crate)` by this sub-phase; it reads the connection itself, so do not read it again), and `price = adapter.and_then(|adapter| budget::model_price(&state.ingest.prices(), &judge.model, adapter, now).cloned())`. `JudgeCost { template_chars: judge.prompt.chars().count(), variables: <entries of the rule's variable_mapping JSON object>, max_output_tokens: judge.max_output_tokens, price }`. Other kinds → `None`.
5. `estimate = tokio::time::timeout(ESTIMATE_TIMEOUT, backfill::estimate(&state, &rule_spec, window, judge.as_ref())).await` with `const ESTIMATE_TIMEOUT: Duration = Duration::from_secs(30);` (`routes::read::READ_TIMEOUT`'s value); the elapsed case → `ApiError::Query(RunError::Timeout)`; the inner error converts with `?`.
6. `200` with `BackfillEstimate { matched, capped, sampled, estimated_input_tokens: cost.map(|c| c.input_tokens), estimated_output_tokens: …, estimated_cost_usd: cost.and_then(|c| c.cost_usd), from: window.from, to: window.to }`. Log `tracing::debug!(project_id, rule_id, matched, sampled, "backfill estimated")`.

**`create_backfill`** (`Extension(principal)`, `error::Json<CreateBackfillRequest>`):
1. `user = principal::session_user(&principal)?` (as `create_online_rule`); `now`.
2. The rule from the reader (`404`); `validate` as above (`400`).
3. One `state.meta.write` whose closure owns its values: `store::get_rule_tx(conn, project, id)` → `None` → `Create::Missing`; `!row.enabled || row.paused_reason.is_some()` → `Create::Inactive`; else `backfills::insert_backfill(conn, NewBackfill { id: Uuid::now_v7(), rule_id: id, from_ns: window.from, to_ns: window.to, max_targets: window.max_targets, sample_rate: window.sample_rate, created_by: Some(user_id) }, now)` → `Create::Created(Box<BackfillRow>)`. A private `enum Create { Missing, Inactive, Created(Box<BackfillRow>) }`. The `MetaError::Conflict` of a running backfill converts to `409` through `?`.
4. `Missing` → `404`; `Inactive` → `ApiError::Conflict { detail: "the rule must be enabled and not paused to backfill" }` (read `ApiError`'s `Conflict` variant for the exact shape; it is what `MetaError::Conflict` converts into).
5. `state.eval.wake_backfills()`; `tracing::info!(target: "audit", backfill_id = %row.id, rule_id = %id, project_id = %project, user_id = %user_id, max_targets = window.max_targets, "online rule backfill started")`.
6. `202` with `backfill_json(&row)`.

`backfill_json(row: &BackfillRow) -> Result<Backfill, ApiError>`: `status` through `BackfillStatus::from_name` (unknown → `ApiError::Internal` with `MetaError::Corrupt { table: "backfills", column: "status" }`); `error` only when `status` is `failed`; the other fields copied.

Remove the I6.5 rows of § Conventions › Dead-code allows in M6.

### Spec

`docs/spec/09-api.md` § Evaluation & alerts: replace the row `POST /online-rules/{id}/backfill` with:
- `GET /online-rules/{id}/backfills` | the rule's newest 50 backfills (`running`, `done`, `failed` with `error`), not paged (D-I-42)
- `POST /online-rules/{id}/backfills/estimate` | `{from, to, max_targets, sample_rate?}` → `{matched, capped, sampled, estimated_input_tokens?, estimated_output_tokens?, estimated_cost_usd?, from, to}`; counts the rule's targets in the window (D-I-38)
- `POST /online-rules/{id}/backfills` | the same body → `202` with the `running` backfill; `409` when the rule is disabled or paused or already has a running backfill; the window is at most 30 days and ends at least 5 minutes ago; `max_targets` 1–10 000 (D-I-25, D-I-42)

### Tests (`routes/backfills.rs`, over HTTP)

Setup: copy `routes/online_rules.rs`'s test setup (seeded project, session cookie, `app.commit_spans`). Set the manual clock one hour after the latest committed `start_time`, so `to = now − 10 min` is valid and covers the traces. For a judge, create a connection and judge evaluator as M3's route tests do; the priced model is the one M3's budget tests use (`rg -n "const PRICED" crates/server/src/eval/execute.rs`), and `gpt-test` has no price.

- `estimate_counts_the_window`: a judge trace rule at rate 1 with a priced model, three traces in the window, `max_targets` 2 → `200`, `matched 2`, `capped true`, `sampled 2`, `estimated_output_tokens = 2 × max_output_tokens`, `estimated_input_tokens = 2 × ceil((prompt chars + 2000 × 2) / 4)`, `estimated_cost_usd` equal to `2 × budget::estimate_usd(…)` computed in the test; `from`, `to` echoed.
- `estimate_without_a_price_has_tokens_only`: `gpt-test` → no `estimated_cost_usd`; both token fields present.
- `a_check_rule_estimate_has_no_cost_fields`.
- `estimate_validates_the_body`: `to` 1 minute ago, `from` after `to`, a 31-day window, `max_targets` 0 and 10 001, `sample_rate` 0 and 1.5 → `400` with the field named; an unknown field (`dry_run`) → `400`.
- `create_answers_202_and_lists_it`: `202`, `status running`, `matched 0`, `enqueued 0`, `created_by` the user; `GET …/backfills` lists it.
- `a_second_backfill_while_one_runs_is_409`.
- `a_disabled_or_paused_rule_is_409`: disable by `PATCH`; pause by SQL (`paused_reason = 'errors'`) → `409` with the message.
- `another_projects_rule_is_404`: a rule id of the second project → `404` on all three routes.
- `viewers_and_api_keys_cannot_start_or_estimate`: viewer → `403` on both POSTs, `200` on the list; an `sk` key → `403` on both POSTs.
- `create_is_audited` (`capture_logs`, plain `#[tokio::test]`): the log has `online rule backfill started` with the backfill and user ids.
- `the_list_is_newest_first`: three backfills (two finished by `finish_backfill` in a write) → newest first; a `failed` one shows its `error`, a `done` one has none.

### Done when

`just check` passes. No I6.x allow is left: `rg -n "I6\.[0-9] (calls|reads) it" crates/server` is empty (I6.4 is merged before I6.5 starts).

---

## I6.6 — Web: the backfills section and the "Run on existing data" dialog

**Goal:** the rule page lists the rule's backfills and lets members start one after an estimate (D-I-27).

**Edit only:** `web/src/components/online-rules/backfills-table.tsx` and `.test.tsx` (new), `web/src/components/online-rules/backfill-dialog.tsx` and `.test.tsx` (new), `web/src/routes/$org/$project/online-rules/$ruleId/index.tsx` and `index.test.tsx`, `docs/spec/11-ui.md`.

**Commit:** `Add backfills to the online rule page`

Read first: `web/src/components/trace/run-evaluator-dialog.tsx` (a dialog with a form and a mutation), `web/src/components/online-rules/jobs-table.tsx` (a table of the rule page), `web/src/components/deletions/delete-matching-dialog.tsx` (an estimate before a write), `web/src/lib/format.ts` (`formatTimestamp`, `formatCost`).

### `backfills-table.tsx`

```ts
/** Refetch every 2 s while a backfill runs, else not at all. */
export function backfillRefetchInterval(data: Schema<"BackfillPage"> | undefined): number | false;

export function BackfillsTable({ org, project, ruleId }: { org: string; project: string; ruleId: string });
```

- `useQuery({ ...backfillsQuery(org, project, ruleId), refetchInterval: (query) => backfillRefetchInterval(query.state.data) })`.
- Empty list: "No backfills yet."
- A `Table` (`components/ui/table`) with the columns **Started** (`formatTimestamp(created_at)`, as `jobs-table.tsx` formats its times), **Window** (`{from} – {to}`, both formatted), **Status**, **Matched**, **Queued** (`enqueued`), numbers with `toLocaleString("en-US")`.
- Status: `Running` (with a small `Loader2` spinner), `Done`, `Failed`. A failed row shows its error under the status, mapped: `interrupted` → "Interrupted by a server restart. Run it again.", `rule disabled` → "The rule was disabled.", `storage unavailable` → "Storage was busy. Run it again.", `invalid filter` → "The rule's filter no longer parses. Fix the rule, then run it again.", `storage read failed` → "The traces could not be read.", `rule could not be read` → "The rule could not be read.", `evaluator could not be read` → "The evaluator could not be read."; any other text is shown as is.
- A load error: "The backfills could not be loaded."

### `backfill-dialog.tsx`

```ts
export function BackfillDialog({
  open,
  onOpenChange,
  rule,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  rule: Schema<"OnlineRule">;
});
```

A `Dialog` titled "Run on existing data", description "Evaluate traces that were stored before the rule existed or while it was off. Targets the rule already scored are skipped." (`spans` instead of `traces` for a span rule). The API takes any window; the dialog offers three, all ending 6 minutes before now (the server wants at least 5; the extra minute covers clock skew):
- **Window** (`Select`, label "Window"): "Last 24 hours" (`24h`), "Last 7 days" (`7d`, the default), "Last 30 days" (`30d`). `to = new Date(Date.now() − 6 × 60 000)`, `from = to − 1, 7 or 30 days`, sent as `toISOString()`.
- **Max targets** (number input, label "Max targets", default 1000, `min 1`, `max 10000`): a value that is not a whole number from 1 to 10 000 shows "Enter a whole number from 1 to 10,000." under the field.
- **Sample rate** (number input, label "Sample rate (%)", default `rule.sample_rate × 100`, step any): a value not above 0 or above 100 shows "Enter a percentage above 0 and at most 100." Sent as a fraction.
- **"Estimate"** (secondary button): sends `POST …/backfills/estimate` with `{from, to, max_targets, sample_rate}` (`$api.useMutation`). While pending: "Estimating…". The result, in a `role="status"` region:
  - "{matched} traces match." (`spans`; "1 trace"); when `capped`: "More than {max_targets} traces match; the newest {max_targets} are used.";
  - "{sampled} would be evaluated.";
  - with `estimated_cost_usd`: "Estimated cost: {formatCost(cost)} ({input} input and {output} output tokens at most)."; with tokens but no cost: "The judge model has no price, so the cost cannot be estimated."; no token fields (not a judge): nothing more.
  - An error: field errors under their fields (`fieldErrors`); anything else as a `FormError` with `problemMessage(error, "The estimate could not be loaded.")`.
- **"Run backfill"** (primary button): disabled until an estimate is shown for the current inputs; any input change clears the estimate. Sends `POST …/backfills` with the same body. On `202`: invalidate `backfillsKey`, `onlineRuleKey` and `evalJobsKey`, then close. A `409` or another problem: `FormError` with `problemMessage(error, "The backfill could not be started.")`. While pending, Esc and the outside click do not close the dialog (as `DeleteMatchingDialog` does).

### The rule page (`$ruleId/index.tsx`)

A section `aria-label="Backfills"` between "Last 24 hours" and "Score distribution": an `h2` "Backfills", a **"Run on existing data"** button for members (`canManage`), then `<BackfillsTable … />`. The button is disabled, wrapped in `DisabledReason`, when:
- `!rule.enabled` → "Enable the rule to run it on existing data.";
- `rule.paused_reason !== undefined` → "Resume the rule to run it on existing data.";
- a backfill of the list is `running` → "A backfill of this rule is running." (read the list with the same `backfillsQuery`; React Query shares it with the table).
The button opens `<BackfillDialog … />`. Viewers see the table without the button.

### Spec

`docs/spec/11-ui.md` § Evaluators & online rules, in the **Online rules** bullet, replace "The backfill dialog (M6) comes later." with "The rule page lists the rule's backfills (status, window, matched and queued counts, refreshed while one runs) and offers "Run on existing data": a window of the last 24 hours, 7 or 30 days, a target cap and a sample rate, an estimate of the matches, the sampled targets and a judge's cost, then the run (D-I-25, D-I-38, D-I-42)."

### Tests

Render through `renderApp` on `/acme/support-bot/online-rules/01900000-0000-7000-8000-000000000700` (the GET mocks come from the manifest; `list_backfills` answers `static/backfills.json` for rules `…0700` and `…0701` only; override with `server.use`).

`backfills-table.test.tsx`:
- `lists backfills`: the fixture → two rows; "Done", "1,000", "240"; "Failed" with "Interrupted by a server restart. Run it again."
- `says when there are none`: `{data: [], next_cursor: null}` → "No backfills yet."
- `refetches only while one runs`: `backfillRefetchInterval` of the fixture → `false`; with one `running` row → `2000`; `undefined` → `false`.

`backfill-dialog.test.tsx` (open it from the rule page, as an admin):
- `estimates then runs`: click "Run on existing data", "Estimate" → "More than 1,000 traces match; the newest 1,000 are used.", "250 would be evaluated.", "Estimated cost: $0.31 …"; click "Run backfill" → the `POST …/backfills` body has `max_targets: 1000`, `sample_rate: 0.25`, and `to − from` = 7 days; the dialog closes and the backfill list is requested again.
- `needs a fresh estimate after a change`: estimate, then change "Max targets" → "Run backfill" is disabled.
- `validates the inputs`: max targets 0 → the message, no request; sample rate 0 → the message.
- `says when the model has no price`: an estimate without `estimated_cost_usd` → "The judge model has no price, so the cost cannot be estimated."
- `shows a conflict`: `POST …/backfills` answers `409` with detail "a backfill of this rule is already running" → shown; the dialog stays open.

`$ruleId/index.test.tsx` (new cases only):
- `a viewer sees backfills without the button`.
- `a disabled rule cannot be backfilled`: the rule with `enabled: false` → the button is disabled and the reason is in the document.
- `a running backfill blocks another`: the list with one `running` row → the button is disabled with "A backfill of this rule is running."

### Done when

`just check-web` passes; then `just check`. Optional: `just web-dev` shows the section and the dialog on the mock data; say in the report whether you looked.

---

## I6.7 — The `cetra serve` end-to-end test

**Goal:** one Rust test takes a trace from OTLP to a judge score through a real server (D-G-20's harness): the live trigger scores a new trace, and a backfill scores a trace that was stored before the rule existed, against the capture tool's mock provider.

**Edit only:** `Cargo.toml` (one `[workspace.dependencies]` line), `crates/cli/Cargo.toml` (`[dev-dependencies]`), `crates/cli/tests/e2e.rs` (one test and its helpers), `Cargo.lock`, `AGENTS.md` (one line of § Layout's crate graph; `CLAUDE.md` is a symlink to it).

**Commit:** `Test online evaluation end to end`

### Dependencies

§ Versions: `cetra-capture = { path = "tools/capture" }` in the root `[workspace.dependencies]`, next to the other `cetra-*` path entries; `axum.workspace = true` and `cetra-capture.workspace = true` in `crates/cli/Cargo.toml` `[dev-dependencies]`, in alphabetical order. Run `just deny`; if cargo-deny refuses the path dependency, STOP and report.

`AGENTS.md` § Layout, **Crate dependency graph (enforced)**: after the bullet "- `ui` depends on no workspace crate.", add exactly:

"- A `tools/` crate is never a dependency of a shipped crate. It may be a dev-dependency, for tests only: `cetra-cli`'s end-to-end test uses `cetra-capture`'s mock provider (D-I-43)."

### The test (`crates/cli/tests/e2e.rs`)

Read the whole file first: `start`, `Api`, `fixture_request`, `post_otlp_http` and `poll_json` are reused. Read `tools/capture/src/mock.rs` (`Mock::new`, `router`, `finish`, `read_facts`) and `tools/capture/src/scenario.rs` (`Turn`).

```rust
/// The judge prompt of the end-to-end evaluator.
const JUDGE_PROMPT: &str = "Question: {{ input }}\nAnswer: {{ output }}";

/// One chat turn of the mock provider: the judge's numeric answer.
fn judge_turn() -> cetra_capture::Turn {
    serde_json::from_value(json!({
        "api": "openai.chat",
        "model": "gpt-test",
        "message": {"text": "{\"explanation\": \"Relevant.\", \"value\": 0.9}"},
        "finish": "stop",
        "usage": {"input": 10, "output": 3},
        "ttft_ms": 0
    }))
    .unwrap()
}

/// `sdk-generator-text`'s one root span (it has an input and an output), on `trace`,
/// starting at `start_ns` and lasting 998 ms.
fn timed_request(trace: &str, start_ns: i64) -> ExportTraceServiceRequest;

/// Nanoseconds since the Unix epoch, now.
fn now_ns() -> i64;
```

`timed_request`: `fixture_request("sdk-generator-text", trace)`, then set the span's `start_time_unix_nano` to `start_ns` and `end_time_unix_nano` to `start_ns + 998_000_000` (both `u64::try_from(…).unwrap()`).

`#[tokio::test(flavor = "multi_thread")] async fn online_rules_and_backfill_end_to_end()`, step by step:
1. **Mock provider.** `let mock = cetra_capture::Mock::new(vec![judge_turn(), judge_turn()]);` bind `tokio::net::TcpListener` on `127.0.0.1:0`, keep its address, `let router = mock.router();` and `let provider = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });`.
2. **Server.** `start(dir.path(), "\n[security]\nallow_private_network_egress = true\n")`. `project = format!("/api/v1/projects/{}", server.proto_id)`.
3. **An old trace, before any rule.** `old = format!("{:032x}", 31)`; send `timed_request(&old, now_ns() − 20 min)` with `post_otlp_http` (`INIT_KEY`, protobuf). No rule exists, so the trigger does not track it.
4. **Connection, judge and rule**, over HTTP with the admin's session (`server.api.post`):
   - `POST /api/v1/orgs/acme/llm-connections` `{"name": "Mock", "adapter": "openai", "api_key": "sk-test-key-0123456789", "base_url": "http://<mock addr>/v1", "default_model": "gpt-test", "max_concurrency": 4, "requests_per_minute": 600}`;
   - `POST {project}/evaluators`, the judge of `eval/trigger.rs`'s `http_judge` with `JUDGE_PROMPT` (name `Relevance`, numeric output 0–1, `max_output_tokens` 1024, score `relevance`);
   - `POST {project}/online-rules` `{"name": "Relevance on traces", "evaluator_id": …, "target": "trace", "variable_mapping": {"input": "trace.input", "output": "trace.output"}, "sample_rate": 1.0, "enabled": true}`.
5. **A live trace, sent until it is scored.** The sweeper picks up the new rule's project within one tick (2 s); a span committed before that is not recorded. So: `live = format!("{:032x}", 32)`, `request = timed_request(&live, now_ns() − 1 s)` built once; then up to 4 times: send `request` (the same trace and span every time), and poll `{project}/scores?rule_id={rule}` every 200 ms for up to 10 s until `data` has one score. Panic after the fourth try with the last body. Resending is safe: once the project is tracked, the trace completes 5 s after its last span, and a trace that completes again finds its job's dedup key taken (D-I-11), so the judge is called once. There is no fixed sleep.
6. **The live score.** The one score has `trace_id == live`, `name == "relevance"`, `source == "llm_judge"`. The mock has exactly two turns (this one and the backfill's): a judge call that was retried, or made twice, shows up in step 11 as an error `request 3: no scripted turn left`.
7. **Estimate.** `from = now_ns() − 1 h`, `to = now_ns() − 6 min`, as RFC 3339 (`chrono::DateTime::from_timestamp_nanos(ns).to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)`). `POST {project}/online-rules/{rule}/backfills/estimate` `{from, to, max_targets: 100}` → `matched 1`, `capped false`, `sampled 1`, `estimated_input_tokens == (JUDGE_PROMPT.chars().count() + 4000).div_ceil(4)`, `estimated_output_tokens == 1024`, no `estimated_cost_usd` (`gpt-test` has no price). The live trace is after `to`, and its judge trace is an evaluation trace, so neither counts.
8. **Backfill.** `POST {project}/online-rules/{rule}/backfills` with the same body → `status == "running"`. Poll `GET {project}/online-rules/{rule}/backfills` until `data[0].status == "done"` (30 s) → `matched 1`, `enqueued 1`. Poll the scores until there are 2; the new one is on `old`.
9. **Nothing is paid twice.** A second `POST …/backfills` with the same body; poll until its `status` is `done` → `matched 1`, `enqueued 0`.
10. **Jobs.** `GET {project}/eval-jobs?rule_id={rule}` → two jobs, both `done`, kinds `rule` and `backfill`.
11. **The provider.** `let log = mock.finish();` → `log.errors` is empty and `log.served.len() == 2`.
12. `server.running.shutdown().await.unwrap(); provider.abort();`.

If the mock refuses the judge's request (an error in `log.errors` naming the request), read `read_facts` and the judge's request body (`crates/eval/src/judge/`), and STOP and report: the fix belongs to the capture tool's owner, not to this test.

Run it five times in a row (`for i in 1 2 3 4 5; do cargo test -p cetra-cli --test e2e online_rules_and_backfill_end_to_end || break; done`); a flaky run must be fixed, not retried.

### Done when

`just check` passes, and the test passes five runs in a row. `just e2e` also passes (it runs the same test binary with the UI embedded).

---

## I6.8 — Closeout of track I

**Goal:** the specs, the design and the roadmap say what track I built; the decision rows the coordinator added match the code.

**Edit only:** `docs/spec/04-metadata-db.md`, `docs/spec/06-evaluation.md`, `docs/spec/09-api.md`, `docs/spec/11-ui.md`, `docs/spec/12-operations.md` (corrections only, if the checks below find one), `docs/plans/i-online-eval/00-design.md` (header, § Modules, § Tables, § Scope, § Milestones), `docs/roadmap.md` (the I row only).

**Commit:** `Close out track I`

### Checks

Read each spec section M6 changed (I6.1, I6.4, I6.5, I6.6) against the merged code and fix any statement that no longer matches: the migration number, the three routes and their status codes, the error strings, the loop list at startup and shutdown. Then:
- `rg -n "dry_run|comes later|not written yet|/backfill\b" docs/spec docs/plans/i-online-eval/00-design.md` finds nothing about backfill (fix each hit that is). The `dry_run` hits in the D-I-25 and D-I-38 rows of `00-design.md` are expected: leave them.
- D-I-38 to D-I-43 in `docs/mvp-decisions.md` § Track I and `00-design.md` § Decisions, and D-I-25's closing sentence, match what was built (the error strings, the routes, the loop). If one does not, do not edit it: list the difference in the report for the coordinator.

### `00-design.md`

The coordinator already updated the header, the plan links, § Scope, the migrations line of § Modules and the § Tables block (with the `backfills_rule` index), using `00MM` and "the next free number, `0014` today". Only:
- replace each `00MM` and each "next free number … `0014` today" with the number I6.1 used, in § Modules, § Tables and § Scope, and verify that the § Tables DDL is the migration's exact text;
- § Modules: `backfill.rs` gets the comment `validate, Pager, estimate, backfill_once (M6)` if it has none;
- verify that the header and § Milestones link both plans; fix them if not.

### Roadmap (`docs/roadmap.md`, the I row)

Read the row first and keep what is still true.
- The plans cell: "design doc, then one plan per milestone (M1–M6 written)".
- The status cell:
  - when M5 is already merged on `main`: "**Done.** M1 (#63), M2 (#76), M3 (#100), M4 (#…), M5 (#…) and M6 (#…) are merged: LLM connections and secret encryption, evaluators and test runs, score definitions, online rules with the durable queue, budgets and pauses, the completion trigger with sampling, manual runs, backfill, the annotation panel, and a`cetra serve` end-to-end test. D-I-1 to D-I-43." (fill in the PR numbers from `git log --oneline origin/main` merges; use the M5 and M6 PR numbers once known, else "PR pending");
  - when M5 is not merged yet: "**In progress.** M1–M4 and M6 (backfill, the end-to-end test, the closeout) are merged or in PR; M5 (annotation panel, score ownership) is pending. The track is done when M5 merges." The session that merges the last of M5 and M6 sets the **Done** text above.

### Done when

`just check` passes (docs only; it must stay green).

---

## After M6

Report: the eight commits, the migration number you used, deviations, and anything unverified. `just check` is green on the merged branch, the end-to-end test of I6.7 passes five runs in a row there and under `just e2e`, and the I row of the roadmap says what I6.8 decided.

## Open questions (with the defaults this plan uses)

1. **Estimate as a separate operation** (D-I-38). Default: `POST …/backfills/estimate` beside `POST …/backfills`, like `POST /deletions/preview`, instead of D-I-25's `dry_run` flag, so each operation has one success status and one schema in the typed client.
2. **Skipping scored targets** (D-I-40). Default: derived score ids looked up per page with `scores_by_id`, instead of the `rule_id` score filter, which would page through every score of the rule. A score written under an older output name (the evaluator was renamed) does not count, so such a target is evaluated again.
3. **Which targets a capped backfill takes** (D-I-40). Default: the newest `max_targets` of the window. The alternative (oldest first) would favour data closest to the retention cut-off.
4. **Span rules and the 50-spans-per-trace cut.** Default: no per-trace cut in a backfill; `max_targets` bounds the run. A trace with many matching spans can use much of the cap.
5. **A failed read** (D-I-41). Default: the backfill fails at once, with no retry; running it again is cheap. A retry loop (like the trigger's five failed reads) would add state to a rare path.
6. **A rule deleted during a run.** Default: the backfill rows go with the rule (`ON DELETE CASCADE`, as the design's table says), so "marked failed" (D-I-25) cannot be observed; the runner just stops.
7. **The dialog's windows.** Default: three presets ending 6 minutes ago (24 h, 7 days, 30 days); the API takes any window. A custom range can come later with the shared time-range picker.
8. **The capture mock in a shipped crate's tests** (D-I-43). Default: `cetra-capture` becomes a dev-dependency of `cetra-cli` for I6.7; it is never a normal dependency. The alternative, a small hand-written OpenAI mock in `e2e.rs`, would duplicate `test_support/mock_llm.rs`, which the CLI crate cannot use (it is `#[cfg(test)]` in the server).
