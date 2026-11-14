# I · M3 — Queue, online rules and manual runs

Implementation plan for milestone M3 of track I. It has 12 sub-phases, I3.1 to I3.12. **Haiku** implements each one in one go, with no context beyond this plan and the files it names. Sub-phases that do not depend on each other run in parallel worktrees (§ Parallel waves). Each sub-phase ends with `just check` green and one commit. Sonnet reviews the code after every three or four sub-phases.

**Prerequisites** (checked when this plan was written, 2026-11-14):
- M2 is implemented on branch `claude/i-m2-impl` (migration `0006_evaluators.sql`, `eval/test_run.rs`, `eval/trace_load.rs`, the evaluators pages). Start from `main` once M2 has merged, otherwise from `claude/i-m2-impl` with `main` merged in. Check that `crates/server/src/eval/test_run.rs` exists.
- This plan lives on branch `claude/i-m3-plan`. Check with `git log --oneline -1 -- docs/plans/i-online-eval/03-m3-queue-rules.md` that it is there.
- Node `26.13.0` is installed at `$HOME/.local/share/nvm/v26.13.0/bin`. It is not on the non-interactive `PATH`.

**Read first** (every sub-phase):
- `AGENTS.md`, all of it.
- This file's sections up to § I3.1, then your own sub-phase.
- `docs/plans/i-online-eval/00-design.md` § Decisions: D-I-5, D-I-6, D-I-9, D-I-11 to D-I-19, D-I-22 to D-I-24, D-I-27 to D-I-30, D-I-32, D-I-34; § Queue (M3).
- `docs/plans/i-online-eval/02-m2-evaluators.md` § Conventions and § Shared reference: M2's patterns (routes, `MetaDb::write` closures, fixtures, pages) are the model for M3.
- `docs/mvp-decisions.md`: D155–D167, D-G-26 to D-G-30, D-G-38, D-K-14, and § Track I.
- `docs/spec/04-metadata-db.md` § Evaluation; `docs/spec/06-evaluation.md` § Online rules; `docs/spec/09-api.md` § Evaluation & alerts.
- The code each sub-phase points to, before you edit it.

## What M3 is, and what it is not

When M3 is done:
- project members create, edit, pause-resume and delete **online rules** under `/api/v1/projects/{project}/online-rules`. Saving checks the filter with storage and the mapping against the evaluator (D-I-30). Creating or enabling a rule calls the active-rule check (D-I-32) inside the write transaction.
- a durable **job queue** (`eval_jobs`) runs in process: a dispatcher claims jobs by strict priority, round-robin across projects, within each connection's concurrency and rate (D-I-6). Workers run the evaluator, commit the judge trace, write the evaluator score (D-I-18) and finish the job, with retries (D-I-13), pauses and the breaker (D-I-14) and budgets (D-I-15).
- a janitor, a pruner and a budget ticker run as loops; startup re-queues interrupted jobs; shutdown stops the loops and gives workers 3 s.
- `POST …/evaluators/{id}/run` enqueues a **manual run** at the highest priority (D-I-22). `GET …/eval-jobs` and `GET …/eval-jobs/{id}` list and read jobs.
- storage has strict exclusions, `span_refresh`, `validate_filter`, and score filters by `evaluator_id` and `rule_id`.
- the web has an "Online rules" project page (list, editor, rule page with jobs, spend, banners, resume and the score distribution), a "Run evaluator" dialog in the trace view, and a "View judge trace" link on evaluator scores.

Nothing creates **rule** jobs yet: the trigger is M4 and backfill is M6. M3 runs rule jobs in tests, by inserting them with `meta::eval_jobs::insert_job`, which M4 calls.

**Out of scope:** `PendingTraces`, the sweeper, sampling and the live enqueue (M4); `POST /online-rules/preview` and the editor's match counter (M4); the annotation panel (M5); backfill (M6); plan enforcement beyond the existing hook (track P).

## Decisions this plan relies on

| # | Summary |
|---|---|
| D-I-5 | A successful connection test sets the connection `ok` and, from M3 on, resumes the rules paused for that connection. |
| D-I-6 | One dispatcher, `eval.workers` workers. A claim needs a worker slot, a connection slot and a bucket token (`max(1, rpm/6)` tokens, refilled at `rpm/60` per second). A webhook evaluator runs at most 4 jobs at once. Strict priority (manual, rule, backfill), round-robin across projects. |
| D-I-11 | `dedup_key` is unique: `rule:<rule_id>:<trace_hex>:<span_hex or ->` or `manual:<job_id>`. Rule enqueues are `ON CONFLICT DO NOTHING`. Live enqueues are capped at 10 000 per rule per UTC hour. |
| D-I-12 | `queued → running` (claims += 1) `→ done / failed / queued`; `queued → skipped`. The worker increments `attempts` right before the call. No lease: startup and the janitor (jobs running for more than 10 minutes) re-queue `running` jobs and add 1 to `interruptions`; a job already interrupted 3 times fails (`interrupted too often`). `finish` is fenced by `claims`. |
| D-I-13 | Retryable errors back off `min(5 s · 2^(attempts−1), 10 min) ± 20 %`, at most 6 attempts. Load failures retry 3 times, 30 s apart (`load_retries`). Failed result writes retry 3 times, 5 s apart, then take the retryable path. |
| D-I-14 | Pause classes: a connection error pauses every enabled rule whose evaluator uses the connection (`'connection'`) and re-queues the job with its attempt given back (`attempts − 1`); a webhook evaluator's rules pause with `'errors'`; a manual job fails. A job whose connection changed since its claim re-queues without pausing anything. The breaker pauses a rule (`'errors'`) when at least 20 of its last 50 evaluated jobs exist and half or more failed. Resume clears a pause; a successful connection test or a key change resumes `'connection'` pauses. |
| D-I-15 | Budgets on judge rules only, per UTC day or month, kept per day in `rule_spend`. Estimate `ceil(chars/4)` input tokens + `max_output_tokens` at the model price; reservations in memory; over budget pauses the rule (`'budget'`) and skips its queued jobs. A 30 s ticker clears `'budget'` pauses when the period turns. |
| D-I-16 | Judge trace id per run: SHA-256(`llmobs.judge-trace.v1` ‖ job id ‖ the run's `claims` as `u32` BE)[..16]. |
| D-I-17 | The executor fails a job whose target trace has an `llmobs-eval` span. |
| D-I-18 | `Writer::Evaluator { kind, evaluator_id, rule_id }`: source by kind, author = rule or evaluator, `session_id` and `user_id` from the root span, metadata = `judge.*` plus `<prefix>.eval.job_id` and `<prefix>.eval.evaluator_version`. |
| D-I-19 | Deleting an evaluator that rules use is a 409; a PATCH that would break a rule is a 409 that lists the rules. |
| D-I-22 | `POST /evaluators/{id}/run` → `202` with the job; origin `Test`; no budget; counts usage. |
| D-I-23 | `trace_spans` load with `around = trace_start_ns`; storage `Timeout`/`Busy` is a load retry; a truncated trace sets `<prefix>.eval.truncated`. |
| D-I-24 | `ProjectPerm::ManageEvaluation` writes; `View` reads. |
| D-I-27 | Pages: `/online-rules`, `/online-rules/new`, `/online-rules/$id`; "Run evaluator"; "View judge trace". |
| D-I-28 | Hourly pruning: finished jobs older than 7 days deleted; queued jobs older than 24 h (7 days for backfill) skipped (`expired`); jobs of inactive projects or orgs deleted. |
| D-I-30 | Rule validation: filter parsed by storage; 1–32 mapped variables; no `expected`/`item.*` expressions; no bare `span.*` in a trace rule; judge template variables all mapped; a check maps `output` (and `expected` when needed); `sample_rate ∈ (0, 1]`; `target` and `evaluator_id` immutable. Limits: name 1–128 characters, filter ≤ 4 096 bytes, each expression ≤ 1 024 bytes, `budget_usd` ≤ 1 000 000, 500 rules per project. The evaluator checks run again inside the rule's write, and an evaluator PATCH checks its rules inside its own write. |
| D-I-32 | `count_enabled_rules` + `rule_limit::check`, inside the write transaction, on create with `enabled` and on a PATCH that enables. |
| D-I-34 | Every read goes through `Storage::query` and A2's reads. |

## Versions

No new dependency. `chrono`, `futures`, `sha2` and `tokio` are already dependencies of `llmobs-server`.

## Conventions for every sub-phase

M1's and M2's conventions apply unchanged (`01-m1-secrets-connections.md` § Conventions, `02-m2-evaluators.md` § Conventions): Node `PATH`, `just` recipes, contract regeneration (`just openapi`, `just api-examples`), dead code, temporary files in the scratchpad, no `unwrap`/`expect` outside tests, privacy, the test-clock rule for evaluator runs, request types without derived `Debug` when they carry payload, `MetaDb::write` closures that own their values, STOP rules, one commit per sub-phase with no trailer lines, and the report. In addition, these lessons from M1's and M2's implementation:

- **Logs of a check run go under your worktree's own `target/`** (for example `target/check-rust.log`), never in a shared directory: parallel worktrees would overwrite each other's logs.
- **`just check-rust` runs as one background task**, and you wait for it to finish (read the log to the end, check the exit status). Never start a second run while one is going; never stop waiting early and commit.
- **Clippy denies** (beyond M2's list) `large_enum_variant` (box the big variant's payload), `needless_pass_by_value` (take `&T` unless you consume it), `collapsible_if` (use `if a && let Some(x) = …`) and `type_complexity` (name the type with a `type` alias or a small struct).
- **`crates/server/tests/openapi.rs` pins every operation.** A new or changed operation updates, in that file:
  - `OPERATIONS`, the `(method, path, operation_id)` list, compared with the document as a whole;
  - `QUERY_OR_BODY`, the operations whose query string can be a 400;
  - the `expected_errors` if-chain in `document_lint`, which pins each operation's 4xx codes (an id that no branch names falls back to `{401, 403, 404, 429}`, or `{400, 401, 403, 404, 429}` when it is in `QUERY_OR_BODY`);
  - two page lists: `pages` near the component-name check (`(page, item)` pairs) and a second `pages` near the nullability test (`required_nullable`). A new page type goes in both.
- **Dead code across sub-phases.** rustc treats an item with `#[allow(dead_code)]` as used, and so is everything it reaches. When a sub-phase adds an entry point whose caller comes later, it marks that entry point (and any field nothing reads yet) with an item-level `#[allow(dead_code)] // <sub-phase> calls it.` and removes the allows of the items the entry point now reaches. Each sub-phase lists the allows it adds and removes.
- **After a merge of another branch, Biome reports import-order errors** in the files the merge touched. Run `just fmt`, then check the diff is only import order.
- **Vitest workers can die with `SIGILL`** when the machine is loaded. That is not a test failure: rerun only the files that crashed (`pnpm --dir web exec vitest run <files>`), then rerun `just check-web`.
- **The storage test `crash_tests` can time out under load.** Rerun that one test alone (`cargo test -p llmobs-storage crash_tests -- --test-threads=1`); report it if it fails alone.
- **Tests that run an evaluator set the manual clock to the real time before seeding** (M2 § Conventions gives the code). This covers every test that reaches `EvalRunner::run`: test runs, `run_job`, `dispatch_once` and manual runs.
- **Log-capture tests** (`crate::test_support::capture_logs`) use plain `#[tokio::test]`, never `flavor = "multi_thread"`.
- **Background loops** follow track K's pattern (`crates/server/src/alerts/mod.rs`): a timing struct with `DEFAULT` and `TEST`, `interval` with `MissedTickBehavior::Skip`, and a `*_once(&AppState, now) -> Result<Report, _>` function that tests call directly with the manual clock. Loops log errors and go on.
- **Never hold a `std::sync::Mutex` guard across an `.await`.** Take what you need, drop the guard, then await.
- **`MetaDb::write` is the single writer.** Keep each write short: no storage read, provider call or sleep inside a write closure, and no scan that a reader connection can do (`state.meta.reader()`).
- `web/src/routeTree.gen.ts` is gitignored. Never commit it.

## Parallel waves

Each wave starts when the waves before it are merged into the M3 integration branch. Sub-phases in one wave touch different files (the table names the shared ones; the second one to merge resolves a trivial conflict there).

| Wave | Sub-phases | Depends on | Shared files to merge by hand |
|---|---|---|---|
| 1 | I3.1 storage reads · I3.2 tables and rows · I3.3 crate edits and policy types · I3.4 shared executor core and evaluator scores | M2 | `crates/server/src/eval/mod.rs` (module lines, I3.3 and I3.4); `crates/server/src/scores.rs` (I3.1 touches one test literal, I3.4 the writer); `docs/spec/06-evaluation.md` (I3.3 and I3.4 edit different sections) |
| 2 | I3.5 API contract | wave 1 (I3.1's `ScoreListParams` and I3.3's `api/online_rules.rs`) | — |
| 3 | I3.6 job execution · I3.10 web: rules list and editor | wave 2 | none (the web phase only reads the contract; I3.5 adds every M3 mock and query helper) |
| 4 | I3.7 dispatcher and loops · I3.8 rules API · I3.11 web: rule page · I3.12 web: run evaluator and judge-trace links | I3.6 (server phases); I3.10 (web phases: the list, the editor, the shared mapping table) | `crates/server/src/eval/mod.rs` (I3.7 and I3.8 add to `EvalQueue` in different `impl` blocks); `docs/spec/06-evaluation.md` (I3.7 and I3.8 edit different bullets) |
| 5 | I3.9 manual runs and the jobs API | I3.7, I3.8 | — |

The Sonnet code reviews fit the waves: after wave 1 (I3.1–I3.4), after waves 2–3 (I3.5, I3.6, I3.10), and after waves 4–5 (I3.7–I3.9, I3.11, I3.12).

Contract files (`openapi.json`, `schema.gen.ts`, `fixtures/api/manifest.json`) are regenerated by I3.1, I3.5, I3.8 and I3.9. After merging a wave, run `just openapi` and `just api-examples` once and commit any diff as `Regenerate the API contract`.

## Final layout after M3

```
crates/otel/src/cost.rs                    PriceTable::price_for (I3.3)
crates/eval/src/judge/…, runner.rs         Adapter::provider_name public; RunInput.judge_trace_id doc (I3.3)
crates/storage/src/filter/scope.rs, mod.rs strict_exclusions (I3.1)
crates/storage/src/reads/traces.rs         strict flag on the trace and span reads; span_refresh (I3.1)
crates/storage/src/reads/alerts.rs         check_alert_filter removed (I3.1)
crates/storage/src/reads/mod.rs            validate_filter (I3.1)
crates/storage/src/reads/scores.rs         ScoreListRequest.evaluator_id, .rule_id (I3.1)
crates/server/migrations/meta/00NN_eval_queue.sql   (I3.2)
crates/server/src/
  meta/online_rules.rs, eval_jobs.rs, rule_spend.rs   (I3.2)
  api/online_rules.rs                      BudgetPeriod (I3.3); rule types (I3.5)
  api/eval_jobs.rs                         job types, run request (I3.5)
  api/openapi.rs                           online_rule_paths, eval_job_paths (I3.5)
  eval/mod.rs                              EvalQueue (I3.6), EvalTiming, spawn, recover, EvalTasks (I3.7)
  eval/budget.rs, breaker.rs, retry.rs, hourly.rs, rule_limit.rs   pure policy types (I3.3)
  eval/execute.rs                          shared run core (I3.4); run_job, finish (I3.6)
  eval/test_run.rs                         uses execute.rs (I3.4)
  eval/trace_load.rs                       LoadError::Saturated (I3.6)
  eval/dispatch.rs                         dispatch_once, ConnectionLimit, TokenBucket, loops (I3.7)
  eval/rule_check.rs                       rule ↔ evaluator checks (I3.8)
  scores.rs                                Writer::Evaluator (I3.4)
  http/mod.rs, test_support/mod.rs         AppInner.eval (I3.6)
  http/error.rs                            ApiError::ConflictWith (I3.8)
  serve.rs                                 recover, spawn, shutdown order, Drop aborts the loops (I3.7)
  test_support/mock_llm.rs                 MockLlm::start_with_delay (I3.7)
  routes/online_rules.rs                   rules API (I3.8)
  routes/evaluators.rs                     delete and PATCH 409s (I3.8)
  routes/connections.rs                    resume on test success and key change (I3.8)
  routes/eval_jobs.rs                      jobs API and manual runs (I3.9)
  routes/scores.rs, api/scores.rs          evaluator_id and rule_id filters (I3.1)
fixtures/api/static/{online-rules,online-rule,eval-jobs,eval-job,eval-job-queued}.json (I3.5)
fixtures/api/requests/{online-rule-create,evaluator-run}.json (I3.5)
web/src/api/queries-online-rules.ts        (I3.5)
web/src/mocks/writes.ts                    M3 write mocks (I3.5)
web/src/components/evaluators/mapping-table.tsx   shared variable-mapping editor (I3.10)
web/src/components/online-rules/rule-form.tsx     (I3.10)
web/src/routes/$org/$project/online-rules/{index,new}.tsx, $ruleId/edit.tsx (+ tests)   (I3.10)
web/src/routes/$org/$project/online-rules/$ruleId/index.tsx (+ test)                   (I3.11)
web/src/components/online-rules/{rule-banner,jobs-table,score-distribution,rule-conflicts}.tsx   (I3.11)
web/src/components/trace/run-evaluator-dialog.tsx (+ test), scores-tab.tsx             (I3.12)
```

## Shared reference

### The migration number

The next free number when you start I3.2. `main` has `0008_entitlements.sql` (track P's M2), so the number is `0009`. Check `crates/server/migrations/meta/` on your base **and** `git log origin/main --oneline -- crates/server/migrations/meta/`. Call it `NN` below. Everything that depends on it:
- the file name `crates/server/migrations/meta/00NN_eval_queue.sql`;
- the `meta_schema` snapshot (`crates/server/tests/snapshots/meta_schema.snap`, written by `insta::assert_snapshot!("meta_schema", …)`);
- `docs/spec/04-metadata-db.md` (the new tables), and `00-design.md` § Modules and § Tables, which name the file `0009_eval_queue.sql` (edit them only when `NN` is not `0009`).

The migration tests need no edit: `open_twice_is_idempotent`, `open_existing_checks_the_file_and_the_version` (`meta/mod.rs`) and `schema_mismatch_names_the_versions` (`admin.rs`) all compute the version with `expected_version()`, which reads the highest embedded migration.

If another branch takes the number after you, the one that merges second renumbers on rebase (00-design § Scope). A migration that already merged is never renumbered.

### Fixed identifiers (fixtures and tests)

| Thing | Value |
|---|---|
| Online rule | `01900000-0000-7000-8000-000000000700`, name `Relevance on answers`, trace rule on evaluator `…0500`, filter `status = 'ok'`, mapping `{input: trace.input, output: trace.output}`, sample rate 0.25, budget 5 USD a day, enabled |
| Second rule | `01900000-0000-7000-8000-000000000701`, name `Relevance on spans`, span rule on `…0500`, mapping `{input: span.input, output: span.output}`, sample rate 1, no budget, enabled, paused (`errors`) |
| Eval job (done) | `01900000-0000-7000-8000-000000000800`, rule job of `…0700` on trace `a1000000000000000000000000000001`, judge trace `e1000000000000000000000000000002`, score `01900000-0000-7000-8000-000000000900` |
| Eval job (failed) | `01900000-0000-7000-8000-000000000801`, rule job of `…0700` on trace `a1000000000000000000000000000002`, 6 attempts, `retryable`, `provider status 503` |
| Eval job (manual, queued) | `01900000-0000-7000-8000-000000000802`, manual run of `…0500` on trace `a1000000000000000000000000000001` |

### Online rule contract

JSON field names are exact. Times are RFC 3339 (`llmobs_core::time_serde`; `crate::api::keys::optional_time` for optional times). Every `Option` field has `#[serde(default, skip_serializing_if = "Option::is_none")]`. Requests are `#[serde(deny_unknown_fields)]`. The enums are `snake_case`, `Copy`, with `as_str` and `from_name -> Option<Self>` like M2's `EvaluatorKind`.

- **`RuleTarget`**: `"trace"` | `"span"`.
- **`BudgetPeriod`**: `"day"` | `"month"` (added in I3.3, `ToSchema` included; I3.5 registers it).
- **`PauseReason`**: `"budget"` | `"errors"` | `"connection"`.
- **`RuleSpend`**: `{period_start: time, spent_usd: number, reserved_usd: number}`. `period_start` is the start of the current UTC day or month.
- **`JobCounts`**: `{queued, running, done, failed, skipped}`, each a non-negative integer, always present.

**`OnlineRule`** (response), fields in this order:

| Field | Type | Notes |
|---|---|---|
| `id` | UUID | |
| `name` | string | 1–128 characters after trimming |
| `evaluator_id` | UUID | immutable |
| `target` | `RuleTarget` | immutable |
| `filter` | string, omitted when none | filter DSL over the target's view |
| `variable_mapping` | object of string to string | variable → expression |
| `sample_rate` | number | `(0, 1]` |
| `budget_usd` | number, omitted when none | |
| `budget_period` | `BudgetPeriod`, omitted when none | present exactly when `budget_usd` is |
| `enabled` | bool | |
| `paused_reason` | `PauseReason`, omitted when not paused | |
| `paused_at` | time, omitted when not paused | |
| `paused_detail` | string, omitted when none | an `EvalError` display or a fixed text; never payload |
| `spend` | `RuleSpend`, omitted when no budget | |
| `jobs_last_24h` | `JobCounts` | jobs of the rule created in the last 24 h, by status |
| `dropped_last_hour` | integer | live matches dropped by the hourly cap in the current UTC hour (D-I-11); counted in memory, 0 after a restart |
| `created_by` | UUID, omitted when unknown | |
| `created_at`, `updated_at` | time | |

**`OnlineRulePage`**: `{data: OnlineRule[], next_cursor: string | null, org_enabled_rules: integer}`. It is not `Page<OnlineRule>`: it carries D-I-32's count. `next_cursor` is always present (no `skip_serializing_if`) and is `null` on the last page, like `Page<T>`. Its schema is written by hand like `Page<T>`'s `PartialSchema` in `api/common.rs`, with `org_enabled_rules` required.

**`CreateOnlineRuleRequest`**: `name`, `evaluator_id`, `target`, `filter?`, `variable_mapping` (object of string to string), `sample_rate?` (default 1.0), `budget_usd?`, `budget_period?`, `enabled?` (default true). Derive `Debug`: rules carry configuration, not payload or secrets.

**`UpdateOnlineRuleRequest`**: `name?`, `filter?` (`""` clears it), `variable_mapping?`, `sample_rate?`, `budget_usd?`, `budget_period?`, `clear_budget?` (bool), `enabled?`. There is no `evaluator_id` or `target`: sending either is a 400 (unknown field). Optional fields cannot be `null` (spec 09 § Conventions), so removing the budget is `clear_budget: true`.

**Errors:**

| Cause | Status | Code / message |
|---|---|---|
| a field fails § Rule validation | 400 | `bad_request` with `errors[]` |
| the filter does not parse | 400 | `invalid_filter`, field `filter`, with `position` |
| duplicate name in the project | 409 | `conflict`, `an online rule with this name already exists` |
| the project already has 500 rules | 409 | `conflict`, `a project has at most 500 online rules` |
| the PATCH's stored row changed between read and write | 409 | `conflict`, `online rule changed, retry` |
| the evaluator changed between the validation and the write (create or PATCH) | 409 | `conflict`, `evaluator changed, retry` |
| create or enable over the plan's active-rule limit (D-I-32) | 403 | `plan_limit`, `Your plan allows <n> active online rule(s).` (singular for 1) |
| unknown rule | 404 | `not_found` |

The 403 uses `ApiError::PlanLimit`, which track K added (D-K-14). D-I-32 says M3 uses it directly when it exists, so there is no interim 409.

### Rule validation

Run on create, and on PATCH against the merged result (stored values overlaid by the body). All field errors are collected. The evaluator is the stored one, read as a `rule_check::Subject` (I3.8): its stored config is parsed, and a webhook's sealed values are never opened, because no check needs them.

| # | Check | Field | Message |
|---|---|---|---|
| 1 | `name` trimmed is 1–128 characters (`chars().count()`) | `name` | `name must be 1 to 128 characters` |
| 2 | create: `evaluator_id` names an evaluator of this project | `evaluator_id` | `evaluator not found in this project` |
| 3 | `filter`, when present and not blank, parses for the target's view (`llmobs_storage::validate_filter`) | — | the `invalid_filter` problem (not a field error; checked last, after every other check passed) |
| 4 | `filter` is at most 4 096 bytes | `filter` | `filter must be at most 4096 bytes` |
| 5 | `variable_mapping` has 1–32 entries | `variable_mapping` | `map 1 to 32 variables` |
| 6 | `VariableMapping::from_json` accepts it | `variable_mapping` | the `MappingError` display |
| 7 | `mapping.needs_item()` is false | `variable_mapping` | `expected and item.* are for experiments only` |
| 8 | trace rule and `mapping.needs_target_span()` | `variable_mapping` | `span.* needs a span rule; use a selector such as span[name=…]` |
| 9 | judge: every name in `PromptTemplate::compile(prompt)?.variables()` is a mapping key | `variable_mapping.<name>` | `variable <name> is used by the prompt but not mapped` |
| 10 | check: `output` is a mapping key; and `expected` too for `exact_match`, `levenshtein`, and `contains` with empty `values` | `variable_mapping.output` / `variable_mapping.expected` | `the check needs the output variable` / `the check needs the expected variable` |
| 11 | `sample_rate` is finite and in `(0, 1]` | `sample_rate` | `sample_rate must be above 0 and at most 1` |
| 12 | `budget_usd` and `budget_period`: both or neither (after the merge; `clear_budget` with either is also this error) | `budget_usd` | `give budget_usd and budget_period together` |
| 13 | `budget_usd` finite, `> 0` and `<= 1_000_000` | `budget_usd` | `budget_usd must be above 0 and at most 1000000` |
| 14 | a budget on a non-judge evaluator | `budget_usd` | `budgets apply to LLM judge evaluators only` |
| 15 | a budget, and the judge's model has no price (`budget::model_price` returns `None`, § I3.3) | `budget_usd` | `the judge model has no price, so the rule cannot have a budget` |
| 16 | every mapping expression is at most 1 024 bytes (`len()`) | `variable_mapping.<name>` | `expression must be at most 1024 bytes` |

The limits (checks 1, 4, 5, 13, 16 and the 500 rules of a project) are D-I-30's. Checks 8–10 and 14–15 are the "rule ↔ evaluator" checks. `eval/rule_check.rs` (I3.8) implements them once; the evaluator PATCH re-runs them for every rule of the evaluator (D-I-19).

### Eval job contract

- **`EvalJobKind`**: `"rule"` | `"manual"` | `"backfill"`.
- **`EvalJobStatus`**: `"queued"` | `"running"` | `"done"` | `"failed"` | `"skipped"`.
- `error_class` reuses M1's `TestErrorClass` (`retryable` | `pause` | `permanent`).

**`EvalJob`** (response), fields in this order: `id`, `kind`, `status`, `rule_id?`, `evaluator_id`, `trace_id` (32 lowercase hex), `span_id?` (16 lowercase hex), `attempts`, `error_class?`, `error?`, `judge_trace_id?` (32 hex), `score_id?` (UUID), `input_tokens?`, `output_tokens?`, `cost_usd?`, `latency_ms?`, `not_before` (time), `created_at` (time), `finished_at?` (time).

**`EvalJobListParams`** (query, `IntoParams`, `deny_unknown_fields`): `rule_id?`, `evaluator_id?`, `status?` (`EvalJobStatus`), `kind?` (`EvalJobKind`), `limit` (`Limit`, default 50), `cursor?`. Newest first (`created_at DESC, id DESC`). The answer is `Page<EvalJob>`, registered as `EvalJobPage`. The cursor is base64url JSON `{created_at, id}` like track K's event cursor (`routes/alert_rules.rs`, `encode_cursor`/`decode_cursor`).

**`RunEvaluatorRequest`** (`POST …/evaluators/{id}/run`, `deny_unknown_fields`, **`Deserialize` only** because `TimeParam` has no `Serialize`, `Debug` derived): `trace_id` (string, 32 hex), `span_id?` (string, 16 hex), `trace_start` (`TimeParam`, required: the trace's `start_time` from the trace detail the UI loaded), `variable_mapping` (object of string to string). It answers `202` with the queued `EvalJob`.

| Rule | Field | Message |
|---|---|---|
| `trace_id` not 32 hex or all zeros | `trace_id` | `trace_id must be 32 hex digits` |
| `span_id` not 16 hex or all zeros | `span_id` | `span_id must be 16 hex digits` |
| `variable_mapping` has 0 or more than 32 entries | `variable_mapping` | `map 1 to 32 variables` |
| an expression over 1 024 bytes | `variable_mapping.<name>` | `expression must be at most 1024 bytes` |
| `VariableMapping::from_json` fails | `variable_mapping` | the `MappingError` display |
| `needs_item()` | `variable_mapping` | `expected and item.* are for experiments only` |
| `needs_target_span()` without `span_id` | `variable_mapping` | `span.* needs span_id` |
| judge evaluator: a prompt variable that is not a mapping key (rule check 9, through `rule_check::unmapped_variables`, I3.8) | `variable_mapping.<name>` | `variable <name> is used by the prompt but not mapped` |
| unknown evaluator in the project | — | 404 |
| more than 10 runs and tests a minute by this user (the shared `limits.connection_tests` limiter, D-I-21) | — | 429 |

### Job execution

`eval::execute::run_job(state, job: ClaimedJob) -> Result<(), MetaError>` (I3.6) runs one claimed job. `ClaimedJob` is the `JobRow` read with the candidates, `claims` (its value after the claim) and `connection_updated_at` (the candidate's `connection_updated_at`, `None` for a check or a webhook). The dispatch permits are not in it: `run_claimed` (I3.7) holds them and drops them after `run_job` returns. `now` is read from `state.clock` at each step that needs it. The steps, and what each failure does (the **finish** column names a `meta::eval_jobs::Finish` variant, § I3.2):

| Step | Work | On failure |
|---|---|---|
| 1 | fetch the project row (`meta::projects::project_by_id(reader, project_id)`) and its org row (`meta::orgs::org_by_id(reader, project.org_id)`); both must exist and have `state == "active"` (read the two functions' return types for the field names) | `Skipped { error: "project unavailable" }` |
| 2 | `evaluators::get_evaluator(project, evaluator_id)`; missing → return `Ok(())` (the row cascades with it) | — |
| 3 | rule jobs: `online_rules::get_rule(project, rule_id)`; missing → return. Disabled or paused → `Requeue { not_before: now }` and return (the dispatcher will not claim it while the rule is paused) | — |
| 4 | `evaluator::load` (opens a webhook's sealed values); a judge's connection row from `connections::get_connection`, its adapter from `Adapter::from_name(&row.adapter)` (it returns `Result<Adapter, UnknownName>`), and its key from `state.connections.get`. When the row's `updated_at` differs from `job.connection_updated_at`, the connection changed since the claim: `Requeue { not_before: now }` and return | `Failed { class: Permanent, error: "evaluator could not be loaded" }`, `"connection not found"`, or `"unknown adapter"` when `from_name` fails |
| 5 | the mapping: the rule's `variable_mapping`, or the job's for a manual run; `VariableMapping::from_json` | `Failed { Permanent, <MappingError display> }` |
| 6 | `trace_load::load_trace(state, project, trace_id, span_id, Some(trace_start_ns))` | `Saturated` (no load permit within 30 s) → `Requeue { not_before: now + retry::SATURATED_REQUEUE_NS }`, no load retry spent; `NotFound`/`NoTargetSpan` → load retry, final error `trace not found`; `Busy` → load retry, final error `storage unavailable`; `TooLarge` → `Failed { Permanent, "trace too large" }`; `EvaluationTrace` → `Failed { Permanent, "target is an evaluation trace" }`; `Failed`/`Convert` → `Failed { Permanent, "storage read failed" }`. A load retry is `LoadRetry { not_before: now + 30 s }` while `load_retries < 3`, else `Failed { Permanent, <final error> }` |
| 7 | `llmobs_eval::resolve(&mapping, Some(&TraceData::new(&spans, span_id)), None)` | `Failed { Permanent, <MappingError display> }` |
| 8 | the live score definition of `output.name` (`state.definitions.live`), checked with `fits_definition` | `Failed { Permanent, "score definition mismatch" }` |
| 9 | a levenshtein check with `output` or `expected` over 64 KiB | `Failed { Permanent, "levenshtein inputs must be at most 64 KiB each" }` |
| 10 | rule or backfill job, judge evaluator, rule with a budget: render the prompt, `budget::model_price`, estimate, reserve (§ I3.6). The reservation is a guard (`budget::Reservation`, § I3.3) | no price → `Failed { Permanent, "model has no price" }` and pause the rule (`errors`, detail `model has no price`); over budget → `Skipped { error: "budget" }`, pause the rule (`budget`), skip the rule's other queued jobs |
| 11 | `start_attempt`: one write, `attempts += 1`, fenced by `claims` | no row → return (re-claimed or deleted; dropping the guard releases the reservation) |
| 12 | `job_judge_trace_id(job id, claims)`; `execute::run_evaluator` with origin `Rule(rule_id)` for rule and backfill jobs, `Test` for manual runs | — (the run returns an outcome either way) |
| 13 | `execute::judge_usage`; `state.usage.add_judge` | — |
| 14 | commit the judge trace, up to 3 tries 5 s apart (`execute::commit_judge_trace`) | the retryable path (below), error `could not store the result` |
| 15 | success only: `scores::write` with `Writer::Evaluator`, up to 3 tries 5 s apart; a `BadRequest` answer is not retried | `BadRequest` → `Failed { Permanent, "score definition mismatch" }`; other errors → the retryable path, error `could not store the result` |
| 16 | `finish` (§ I3.6): `rule_spend`, the job row, pauses and the connection status in one write | — |

The reservation guard releases the reservation when it is dropped without being settled: on every early exit, on a panic (unwinding drops it) and when the worker task is aborted at shutdown. Step 16 settles it with the actual cost.

**Dispositions after the call** (step 16):

| Outcome | Rule or backfill job | Manual job |
|---|---|---|
| `Ok` and the score was written | `Done` | `Done` |
| `Err`, class `Retryable` | `Retry { not_before: now + retry::backoff_ns(job id, attempts) }` while `attempts < 6`, else `Failed { Retryable, … }` | same |
| `Err`, class `Pause`, judge | connection `failing` (error text); pause every enabled, unpaused rule whose evaluator uses the connection (`connection`); `PauseRequeue { not_before: now }` (the attempt is given back, D-I-14) | `Failed { Pause, … }`; connection `failing` |
| `Err`, class `Pause`, judge, and the connection's stored `updated_at` is no longer `job.connection_updated_at` (the key changed during the call) | `PauseRequeue { not_before: now }` only: no pause, no status change (D-I-14) | `Failed { Pause, … }` only |
| `Err`, class `Pause`, webhook | pause every enabled, unpaused rule of the evaluator (`errors`, detail = the error text); `PauseRequeue { not_before: now }` | `Failed { Pause, … }` |
| `Err`, class `Permanent` | `Failed { Permanent, … }` | same |
| the retryable path after a write failure (steps 14–15) | as `Retryable` above, error `could not store the result` | same |

**The breaker.** Every exit after step 12 carries `reached_evaluator: bool`. It is true for `Done`, and for a `Failed` whose run error is `EvalError::Judge`, `Webhook` or `Egress` (`matches!(error, EvalError::Judge(_) | EvalError::Webhook(_) | EvalError::Egress(_))`). It is false for every exit before step 12, for the write failures of steps 14–15 (`could not store the result`, `score definition mismatch`) and for every other variant. After a rule job's `finish` matched its row, the breaker records it (§ I3.3) when `reached_evaluator` is true. A trip pauses the rule (`errors`, detail `half or more of the last 50 jobs failed`).

The judge's `cost` for the budget and `rule_spend` is the `llm` span's `cost_total`; when the `llm` span has tokens but no `cost_total`, it is the estimate; with no `llm` span it is 0.

Every pause is an audit event: `tracing::info!(target: "audit", rule_id, project_id, reason, "online rule paused")`. Logs never carry the detail text.

**Accepted by design** (not bugs to fix):
- The janitor can re-queue a job that is still running, so a slow judge may be called twice (the fence drops the first result).
- The hourly cap and the breaker windows reset on restart.
- Manual runs take priority 0, so one org's manual runs can delay every rule job, bounded only by the per-user limit of 10 a minute (D-I-21).
- The in-memory budget can count one job's spend twice until the period turns or the server restarts: job A's `finish` commits its `rule_spend`, job B then gets `NotLoaded` and `load`s the period (A's spend included), and A's `settle` adds it again. The error only over-counts, so a rule can pause early but never overspend.
- A rule can pause on `budget` while its actual spend is below the budget: `Over` counts the other running jobs' reservations, which are estimates at `max_output_tokens`. The pause skips the rule's queued jobs; the user resumes it or raises the budget.
- The dispatcher takes a connection's rate-limit token at claim. A job that then requeues (steps 3, 4 or 6) or loses its claim has used that token. Tokens are never refunded.

---

## I3.1 — Storage reads: strict exclusions, span refresh, filter validation, score filters

**Goal:** the reads M3, M4 and M6 need. Trace and span reads can apply strict exclusions; `span_refresh` exists; `validate_filter` replaces K's `check_alert_filter`; `GET /scores` filters by `evaluator_id` and `rule_id`.

**Edit only:** `crates/storage/src/filter/scope.rs`, `crates/storage/src/filter/mod.rs` (the `pub(crate) use scope::{…}` line only), `crates/storage/src/reads/{mod,traces,alerts,scores}.rs`, `crates/storage/src/lib.rs` (re-exports), `crates/storage/src/query/traces.rs` and `crates/storage/tests/trace_bench.rs` (struct literals only), `crates/server/src/routes/{alert_rules,read,traces,scores,streams}.rs`, `crates/server/src/mcp/traces.rs`, `crates/server/src/api/{convert,scores}.rs`, `crates/server/src/scores.rs` (one test literal), `crates/server/openapi.json`, `web/src/api/schema.gen.ts`, `docs/spec/05-query.md`, `docs/spec/09-api.md`.

**Commit:** `Add strict exclusions, span refresh and score filters to storage reads`

### Strict exclusions (`filter/scope.rs`)

```rust
/// Exclusions that no request flag and no filter can turn off (D-I-9): evaluation traces and
/// experiment traces. On `spans`, the test is on the span's trace, not on the span. `trace_ids`,
/// when given, are the only traces the caller reads: the `spans` subquery is limited to them.
pub(crate) async fn strict_exclusions(
    ctx: &QueryContext,
    view: FilterView,
    trace_ids: Option<&[String]>,
) -> Result<Expr, RunError>
```
Add `strict_exclusions` to the `pub(crate) use scope::{exclusions, search, time_window};` line of `filter/mod.rs`, so `reads/traces.rs` can import it from `crate::filter`.
- `Traces`: the conjunction that `exclusions(ctx, FilterView::Traces, false, false, false)` returns (both clauses). It always returns `Some` for those arguments; map `None` to `RunError::Internal(DataFusionError::Internal("…".into()))` rather than unwrapping.
- `Spans`: `trace_id NOT IN (SELECT trace_id FROM traces WHERE [trace_id IN (<ids>) AND] <bad>)`, where `<bad>` is `environment = 'llmobs-eval' OR metadata['<experiment key>'] IS NOT NULL` on the `traces` view. Build `<bad>` as `NOT (<traces conjunction above>)`, the subquery with `ctx.session().table("traces")` → `.filter(bad)` (and, when `trace_ids` is `Some`, a first `.filter(col(traces::TRACE_ID).in_list(<lowercased ids as lit>, false))`, so DataFusion pushes the id test below the view's aggregation instead of aggregating every trace) → `.select(vec![col(traces::TRACE_ID)])`, and the expression with `datafusion::logical_expr::not_in_subquery(col(spans::TRACE_ID), Arc::new(plan.into_unoptimized_plan()))` (the `search` function above builds an `in_subquery` the same way).
- Any other view: `RunError::InvalidRequest("strict exclusions apply to traces and spans".to_owned())`.

`filter/scope.rs` starts with `#![allow(dead_code)]` (line 4). Remove it only if nothing in the module is unused afterwards.

### The strict flag (`reads/traces.rs`)

- `TraceRefreshRequest`, `TraceListRequest` and `SpanListRequest` gain `pub strict_exclusions: bool` (doc: "Apply [`strict_exclusions`]: evaluation and experiment rows are left out even when the filter names `environment` or the include flags are set (D-I-9)").
- `ListPlan` gains `strict: bool`, set from the request in `ListPlan::traces` and `ListPlan::spans`.
- In `refresh` and `list_frame`: when strict, push `strict_exclusions(ctx, view, ids).await?` **instead of** calling `exclusions`; otherwise keep the current code. `refresh` passes its ids (`Some`); the lists pass `None` (their window is on the span or trace start, which does not bound the trace row, so the subquery cannot be cut by time).
- Every existing struct literal (server and storage, tests included; `rg -n "TraceRefreshRequest \{|TraceListRequest \{|SpanListRequest \{" crates`) gets `strict_exclusions: false`.

### `span_refresh` (`reads/traces.rs`)

```rust
/// Most spans `span_refresh` returns for one trace (D-I-9).
pub const MAX_REFRESH_SPANS_PER_TRACE: usize = 200;

/// Spans of the given traces that match a span filter, for the online-rule sweeper (D-I-9).
#[derive(Debug, Clone, Copy)]
pub struct SpanRefreshRequest<'a> {
    /// Filter text over `spans`. Empty or whitespace means no filter.
    pub filter: Option<&'a str>,
    /// Trace ids, 32 hex digits, either case. At most [`MAX_REFRESH_IDS`].
    pub trace_ids: &'a [&'a str],
    /// Most spans kept per trace, 1 to [`MAX_REFRESH_SPANS_PER_TRACE`].
    pub per_trace: usize,
    /// Apply [`strict_exclusions`] instead of the default exclusions.
    pub strict_exclusions: bool,
    /// Time allowed for planning and running.
    pub timeout: Duration,
}

/// The columns `span_refresh` returns, in this order.
pub const SPAN_REFRESH_COLUMNS: &[&str] = &[spans::TRACE_ID, spans::SPAN_ID, spans::START_TIME];

pub async fn span_refresh(ctx: &QueryContext, request: SpanRefreshRequest<'_>) -> Result<Vec<RecordBatch>, RunError>
```
- Validation as `trace_refresh`: parse the filter for `FilterView::Spans` first; more than `MAX_REFRESH_IDS` ids or a bad id → `InvalidRequest`; `per_trace` outside `1..=MAX_REFRESH_SPANS_PER_TRACE` → `InvalidRequest("per_trace must be between 1 and 200")`; no ids → `Ok(vec![])` without a query.
- The frame: `spans` table; conjuncts = (strict exclusions with `Some(&ids)`, or the default exclusions with `references_environment` from the parsed filter) AND `trace_id IN (ids)`; then the filter (`filter.apply`); then a window column `rn = row_number() OVER (PARTITION BY trace_id ORDER BY start_time ASC, span_id ASC)` (`frame.window(vec![…alias("rn")])`; build the expression with DataFusion's `row_number()` window function and `ExprFunctionExt`'s `.partition_by(…)`, `.order_by(…)`, `.build()?`; find the exact path in the pinned version with `rg -n "pub fn row_number" ~/.cargo/registry/src/*/datafusion-functions-window-*/src`); then `.filter(col("rn").lt_eq(lit(per_trace as u64)))`; then sort by `trace_id ASC, start_time ASC, span_id ASC` (three `SortExpr`s, `col(…).sort(true, false)`); then `select(SPAN_REFRESH_COLUMNS)`.
- Run it with `frame.collect()` under the request timeout, like `trace_refresh`. The answer is at most 500 × 200 rows of three columns. The window sorts the matching spans of at most 500 traces on the columns it needs (the scan reads only the filter's columns and these three); DataFusion's memory pool bounds that sort, and running out is a `RunError` like any other read.
- Export `SpanRefreshRequest`, `span_refresh`, `MAX_REFRESH_SPANS_PER_TRACE` and `SPAN_REFRESH_COLUMNS` from `reads/mod.rs` and `lib.rs`.

M3 has no production caller of `span_refresh` (M4's sweeper is the first). It is a public storage read with its own tests, so it is not dead code.

### `validate_filter` (`reads/mod.rs`)

```rust
/// Parses `text` as a filter of `view` without running anything (D-I-30). Empty or blank text
/// is accepted (it means no filter).
///
/// # Errors
///
/// [`RunError::Invalid`] when the text does not parse or names an unknown column;
/// [`RunError::InvalidRequest`] for a view other than traces or spans.
pub fn validate_filter(ctx: &QueryContext, view: ViewName, text: &str) -> Result<(), RunError>
```
`ViewName::Traces` → `FilterView::Traces`, `ViewName::Spans` → `FilterView::Spans`, others → `InvalidRequest("filters are checked for traces and spans only")`. The body is `check_alert_filter`'s, with that view. Its blank test, `present(Some(text))`, is already in `reads/mod.rs` (`pub(crate) fn present`, which `reads/alerts.rs` imports), so `validate_filter` calls it in place and nothing moves.

Delete `check_alert_filter` from `reads/alerts.rs`, its re-exports, and move its test to `reads/mod.rs` as `validate_filter_accepts_blank_and_rejects_bad_text` (same cases, `ViewName::Traces`), plus a spans case (`validate_filter(&ctx, ViewName::Spans, "span_kind = 'llm'")` is `Ok`, and `ViewName::Sessions` is `InvalidRequest`). The one caller, `routes/alert_rules.rs` `checked_condition`, calls `llmobs_storage::validate_filter(&ctx, ViewName::Traces, text)`.

### Score filters (`reads/scores.rs`, server)

- `ScoreListRequest` gains `pub evaluator_id: Option<&'a str>` and `pub rule_id: Option<&'a str>` (doc: "`evaluator_id` / `rule_id` columns, compared with the hyphenated lowercase UUID text"). In the conjunct builder, after `source`: `col(scores::EVALUATOR_ID).eq(lit(id))` and the same for `rule_id`. Read `append_uuid_opt` in `crates/core/src/batch.rs` to confirm the stored text is `Uuid::to_string()`; if it is another form, compare with that form and say so in the report.
- `api/scores.rs` `ScoreListParams` gains, after `source`: `evaluator_id: Option<Uuid>` ("Scores written by this evaluator.") and `rule_id: Option<Uuid>` ("Scores written by this online rule."), both `#[serde(default)]`.
- `routes/scores.rs` passes `params.evaluator_id.map(|id| id.to_string())` (keep the `String` alive in a local, then `.as_deref()`). Every other `ScoreListRequest` literal gets `evaluator_id: None, rule_id: None`.
- `just openapi` regenerates the contract.

### Spec

- `docs/spec/05-query.md` § Default exclusions, a new last paragraph: "**Strict exclusions** (D-I-9). Online-rule matching (`trace_refresh`, `span_refresh`, and the trace and span lists used by preview and backfill) sets `strict_exclusions`: evaluation and experiment traces are left out whatever the filter names. On spans the test is on the span's trace: `trace_id NOT IN (SELECT trace_id FROM traces WHERE environment = 'llmobs-eval' OR metadata['llmobs.experiment_id'] IS NOT NULL)`."
- `docs/spec/09-api.md` § Scores & annotations, the `GET /scores` row: "filter by target, name, source, `evaluator_id`, `rule_id`, time".

### Tests

`crates/storage` (read the existing `trace_refresh` tests, e.g. `refresh_keeps_the_listed_traces_and_the_default_exclusions`, and copy their setup):
- `refresh_strict_drops_eval_traces_even_when_the_filter_names_environment`: three traces (normal, `llmobs-eval`, experiment); filter `environment = 'llmobs-eval'` with `include_eval: true` and `strict_exclusions: true` returns nothing; the same with `strict_exclusions: false` returns the eval trace.
- `trace_list_strict_ignores_the_include_flags`: `include_eval` and `include_experiments` true, strict true → only the normal trace.
- `span_list_strict_tests_the_trace`: a trace whose root has `environment = 'production'` but whose trace row is an experiment (metadata `llmobs.experiment_id` on the root) — its spans are left out under strict and kept without it.
- `span_refresh_caps_each_trace_and_orders_by_start`: trace A with 5 spans, trace B with 2; `per_trace: 3` → A's 3 earliest then B's 2, columns exactly `SPAN_REFRESH_COLUMNS`.
- `span_refresh_applies_the_filter_and_strict_exclusions`: filter `span_kind = 'llm'` keeps only llm spans; an eval trace's spans are absent.
- `span_refresh_validates_its_request`: 501 ids, a 31-digit id, `per_trace: 0` and `201` are `InvalidRequest`; a bad filter is `Invalid`; no ids is `Ok(vec![])`.
- `score_list_filters_by_evaluator_and_rule`: three scores with different `evaluator_id`/`rule_id`; each filter returns its one row.
- the moved `validate_filter` test.

`crates/server`:
- `routes/scores.rs`: `scores_filter_by_evaluator_id_and_rule_id` over HTTP (`?evaluator_id=…`, `?rule_id=…`), and `a malformed evaluator_id is 400`.
- `routes/alert_rules.rs`: the existing invalid-filter test still passes unchanged.

### Done when

`just check` passes; `openapi.json` shows the two new `GET /scores` parameters.

---

## I3.2 — Tables and rows

**Goal:** migration `00NN_eval_queue.sql` with `online_rules`, `eval_jobs` and `rule_spend`, and the three `meta` modules that read and write them.

**Edit only:** `crates/server/migrations/meta/00NN_eval_queue.sql` (new), `crates/server/src/meta/{online_rules,eval_jobs,rule_spend}.rs` (new), `crates/server/src/meta/mod.rs` (module list), the `meta_schema` snapshot, `docs/spec/04-metadata-db.md`, `docs/plans/i-online-eval/00-design.md` (the migration's name, only when `NN` is not `0009`).

**Commit:** `Store online rules, eval jobs and rule spend`

### Migration

§ The migration number says how to choose `NN` and what depends on it.

```sql
CREATE TABLE online_rules (
  id TEXT PRIMARY KEY,
  project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  evaluator_id TEXT NOT NULL REFERENCES evaluators(id),
  name TEXT NOT NULL,
  target TEXT NOT NULL,
  filter TEXT,
  variable_mapping TEXT NOT NULL,
  sample_rate REAL NOT NULL DEFAULT 1.0,
  budget_usd REAL,
  budget_period TEXT,
  enabled INTEGER NOT NULL DEFAULT 1,
  paused_reason TEXT,
  paused_at INTEGER,
  paused_detail TEXT,
  created_by TEXT REFERENCES users(id) ON DELETE SET NULL,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  UNIQUE(project_id, name)
);
CREATE INDEX online_rules_evaluator ON online_rules(evaluator_id);

CREATE TABLE eval_jobs (
  id TEXT PRIMARY KEY,
  project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  kind TEXT NOT NULL,
  priority INTEGER NOT NULL,
  rule_id TEXT REFERENCES online_rules(id) ON DELETE CASCADE,
  evaluator_id TEXT NOT NULL REFERENCES evaluators(id) ON DELETE CASCADE,
  variable_mapping TEXT,
  trace_id BLOB NOT NULL,
  span_id BLOB,
  trace_start_ns INTEGER NOT NULL,
  dedup_key TEXT NOT NULL UNIQUE,
  status TEXT NOT NULL,
  attempts INTEGER NOT NULL DEFAULT 0,
  claims INTEGER NOT NULL DEFAULT 0,
  load_retries INTEGER NOT NULL DEFAULT 0,
  interruptions INTEGER NOT NULL DEFAULT 0,
  not_before INTEGER NOT NULL,
  error_class TEXT,
  error TEXT,
  judge_trace_id BLOB,
  score_id TEXT,
  evaluator_version INTEGER,
  input_tokens INTEGER,
  output_tokens INTEGER,
  cost_usd REAL,
  latency_ms INTEGER,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  finished_at INTEGER
);
CREATE INDEX eval_jobs_ready ON eval_jobs(status, priority, not_before);
CREATE INDEX eval_jobs_rule ON eval_jobs(rule_id, created_at);
CREATE INDEX eval_jobs_project ON eval_jobs(project_id, created_at);
CREATE INDEX eval_jobs_finished ON eval_jobs(finished_at) WHERE finished_at IS NOT NULL;

CREATE TABLE rule_spend (
  rule_id TEXT NOT NULL REFERENCES online_rules(id) ON DELETE CASCADE,
  day INTEGER NOT NULL,
  spent_usd REAL NOT NULL DEFAULT 0,
  PRIMARY KEY (rule_id, day)
);
```

`online_rules.evaluator_id` has the default `NO ACTION`: deleting a project cascades to evaluators and rules in one statement; deleting an evaluator that rules use is refused by the route (I3.8). `eval_jobs_project` serves the jobs list (newest first per project); it is not in the design's table list, which this plan's commit updates.

**Existing tests:** only the `meta_schema` snapshot changes (§ The migration number). Regenerate the snapshot with `INSTA_UPDATE=always cargo test -p llmobs-server schema_matches_the_snapshot`. Its diff must add only the three tables, the five named indexes, and SQLite's automatic indexes (the primary keys of `online_rules` and `eval_jobs`, `rule_spend`'s composite key, `UNIQUE(project_id, name)` and `UNIQUE(dedup_key)`). List them in the report.

### `meta/online_rules.rs`

Stored strings: `target` is `"trace"`/`"span"`, `budget_period` `"day"`/`"month"`, `paused_reason` `"budget"`/`"errors"`/`"connection"`. Define `pub(crate) const PAUSE_BUDGET: &str = "budget";`, `PAUSE_ERRORS`, `PAUSE_CONNECTION`. A stored value outside these lists is `MetaError::Corrupt` when the row is read.

```rust
const PROJECT_CAP: i64 = 500;

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RuleRow {
    pub(crate) id: Uuid,
    pub(crate) project_id: ProjectId,
    pub(crate) evaluator_id: Uuid,
    pub(crate) name: String,
    pub(crate) target: String,
    pub(crate) filter: Option<String>,
    pub(crate) variable_mapping: String,      // JSON object text
    pub(crate) sample_rate: f64,
    pub(crate) budget_usd: Option<f64>,
    pub(crate) budget_period: Option<String>,
    pub(crate) enabled: bool,
    pub(crate) paused_reason: Option<String>,
    pub(crate) paused_at: Option<i64>,
    pub(crate) paused_detail: Option<String>,
    pub(crate) created_by: Option<Uuid>,
    pub(crate) created_at: i64,
    pub(crate) updated_at: i64,
}

/// The editable columns. Insert takes them with the id, project, evaluator, target and author;
/// update replaces them all.
pub(crate) struct RuleFields {
    pub(crate) name: String,
    pub(crate) filter: Option<String>,
    pub(crate) variable_mapping: String,
    pub(crate) sample_rate: f64,
    pub(crate) budget_usd: Option<f64>,
    pub(crate) budget_period: Option<String>,
    pub(crate) enabled: bool,
}

/// The fixed columns of a new rule.
pub(crate) struct NewRule {
    pub(crate) id: Uuid,
    pub(crate) project_id: ProjectId,
    pub(crate) evaluator_id: Uuid,
    pub(crate) target: String,
    pub(crate) created_by: Option<Uuid>,
}
```

Functions (each documented with `# Errors`, `MetaError` when the query fails or a row does not decode):

| Function | What |
|---|---|
| `list_rules(pool, project) -> Vec<RuleRow>` | ordered by `name, id` |
| `get_rule(pool, project, id) -> Option<RuleRow>` | |
| `get_rule_tx(conn, project, id) -> Option<RuleRow>` | the same on the writer |
| `insert_rule(conn, new: NewRule, fields: RuleFields, now) -> RuleRow` | counts the project's rules first (`Conflict("a project has at most 500 online rules")` at 500); `unique_or(error, "an online rule with this name already exists")` like `meta/evaluators.rs` |
| `update_rule(conn, project, id, fields, now) -> Option<RuleRow>` | replaces the editable columns and `updated_at`; `None` when missing; name clash → the same `Conflict` |
| `delete_rule(conn, project, id) -> bool` | |
| `count_enabled_rules(conn, org) -> u64` | `SELECT COUNT(*) FROM online_rules r JOIN projects p ON p.id = r.project_id WHERE p.org_id = ? AND p.state = 'active' AND r.enabled = 1` (D-I-32), like K's `count_enabled_org_rules` in `meta/alerts.rs`; paused rules count, rules of a project being deleted do not |
| `rules_of_evaluator(conn, evaluator_id) -> Vec<RuleRow>` | every rule of the evaluator, ordered by `name, id` |
| `pause_rule(conn, id, reason, detail: Option<&str>, now) -> bool` | `UPDATE … SET paused_reason = ?, paused_at = ?, paused_detail = ?, updated_at = ? WHERE id = ? AND enabled = 1 AND paused_reason IS NULL`; true when a row changed |
| `pause_rules_of_connection(conn, connection_id, detail, now) -> Vec<Uuid>` | the same update (`'connection'`) for `evaluator_id IN (SELECT id FROM evaluators WHERE connection_id = ?)`, with `RETURNING id` |
| `pause_rules_of_evaluator(conn, evaluator_id, reason, detail, now) -> Vec<Uuid>` | the same for `evaluator_id = ?`, `RETURNING id` |
| `resume_rule(conn, project, id, now) -> Option<RuleRow>` | clears `paused_reason`, `paused_at`, `paused_detail`, sets `updated_at`; `None` when missing |
| `resume_connection_pauses(conn, connection_id, now) -> Vec<Uuid>` | clears the pause of rules paused with `'connection'` whose evaluator uses the connection, `RETURNING id` |
| `clear_budget_pauses(conn, day_start, month_start, now) -> Vec<Uuid>` | clears `'budget'` pauses where `(budget_period = 'day' AND paused_at < day_start) OR (budget_period = 'month' AND paused_at < month_start) OR budget_usd IS NULL`, `RETURNING id` |

`paused_at` is set only by the pause functions; `updated_at` moves on every write. The optimistic PATCH check (I3.8) compares whole rows, so a pause between read and write makes the PATCH a 409 retry, which is fine.

### `meta/eval_jobs.rs`

```rust
pub(crate) const KIND_RULE: &str = "rule";
pub(crate) const KIND_MANUAL: &str = "manual";
pub(crate) const KIND_BACKFILL: &str = "backfill";

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct JobRow {
    pub(crate) id: Uuid,
    pub(crate) project_id: ProjectId,
    pub(crate) kind: String,
    pub(crate) priority: u8,
    pub(crate) rule_id: Option<Uuid>,
    pub(crate) evaluator_id: Uuid,
    pub(crate) variable_mapping: Option<String>,
    pub(crate) trace_id: TraceId,
    pub(crate) span_id: Option<SpanId>,
    pub(crate) trace_start_ns: i64,
    pub(crate) status: String,
    pub(crate) attempts: u32,
    pub(crate) claims: u32,
    pub(crate) load_retries: u32,
    pub(crate) interruptions: u32,
    pub(crate) not_before: i64,
    pub(crate) error_class: Option<String>,
    pub(crate) error: Option<String>,
    pub(crate) judge_trace_id: Option<TraceId>,
    pub(crate) score_id: Option<Uuid>,
    pub(crate) evaluator_version: Option<u32>,
    pub(crate) input_tokens: Option<u64>,
    pub(crate) output_tokens: Option<u64>,
    pub(crate) cost_usd: Option<f64>,
    pub(crate) latency_ms: Option<u64>,
    pub(crate) created_at: i64,
    pub(crate) updated_at: i64,
    pub(crate) finished_at: Option<i64>,
}

/// A job to enqueue.
pub(crate) struct NewJob {
    pub(crate) id: Uuid,
    pub(crate) project_id: ProjectId,
    pub(crate) kind: &'static str,
    pub(crate) rule_id: Option<Uuid>,
    pub(crate) evaluator_id: Uuid,
    pub(crate) variable_mapping: Option<String>,
    pub(crate) trace_id: TraceId,
    pub(crate) span_id: Option<SpanId>,
    pub(crate) trace_start_ns: i64,
}

/// One queued job the dispatcher may claim, with what its permits need.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Candidate {
    pub(crate) job: JobRow,
    pub(crate) evaluator_kind: String,
    pub(crate) connection_id: Option<Uuid>,
    /// `(max_concurrency, requests_per_minute, updated_at)` of the judge's connection.
    pub(crate) connection_limits: Option<(u32, u32, i64)>,
}

/// The token counts and cost of one finished call.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct JobUsage {
    pub(crate) input_tokens: Option<u64>,
    pub(crate) output_tokens: Option<u64>,
    pub(crate) cost_usd: Option<f64>,
    pub(crate) latency_ms: Option<u64>,
}

/// How `finish` leaves the job. Every variant sets `updated_at = now`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Finish {
    /// `done`; sets `finished_at`, `judge_trace_id`, `score_id`, `evaluator_version`, the usage; clears `error_class`, `error`.
    Done { judge_trace_id: TraceId, score_id: Uuid, evaluator_version: u32, usage: JobUsage },
    /// `failed`; sets `finished_at`, `error_class`, `error`, and `judge_trace_id`, `evaluator_version`, the usage when given.
    Failed { class: &'static str, error: String, judge_trace_id: Option<TraceId>, evaluator_version: Option<u32>, usage: JobUsage },
    /// back to `queued` after a retryable failure; sets `not_before`, `error_class = 'retryable'`, `error`, `judge_trace_id`, the usage.
    Retry { not_before: i64, error: String, judge_trace_id: Option<TraceId>, usage: JobUsage },
    /// back to `queued` with `load_retries += 1`, `not_before`, `error_class = 'retryable'`, `error`.
    LoadRetry { not_before: i64, error: &'static str },
    /// back to `queued` with `not_before`, nothing else (a rule that is no longer runnable, a changed
    /// connection before the call, no trace-load permit).
    Requeue { not_before: i64 },
    /// back to `queued` with `not_before` and `attempts = MAX(attempts - 1, 0)`: a pause gives back
    /// the attempt the run counted (D-I-14).
    PauseRequeue { not_before: i64 },
    /// `skipped`; sets `finished_at`, `error`, clears `error_class`.
    Skipped { error: &'static str },
}
```

Box the big variants' fields if clippy reports `large_enum_variant` (§ Conventions).

| Function | What |
|---|---|
| `dedup_key(job: &NewJob) -> String` | `rule:<rule_id>:<trace hex>:<span hex or ->` for `rule` and `backfill`, `manual:<id>` for `manual` (D-I-11) |
| `priority_of(kind) -> u8` | `manual` 0, `rule` 1, `backfill` 2 |
| `insert_job(conn, job: NewJob, now) -> bool` | `INSERT … ON CONFLICT(dedup_key) DO NOTHING`, `status = 'queued'`, `not_before = now`; true when a row was inserted. Backfill's `DO UPDATE` rule (D-I-11) is M6's own function |
| `get_job(pool, project, id) -> Option<JobRow>` | |
| `list_jobs(pool, project, filter: &JobFilter, before: Option<(i64, Uuid)>, limit: i64) -> Vec<JobRow>` | `JobFilter { rule_id, evaluator_id, status: Option<&'static str>, kind: Option<&'static str> }` (all `Option`, `Copy`); each is `(?n IS NULL OR column = ?n)`; newest first `created_at DESC, id DESC`, strictly before `before` like `meta/alerts.rs` `list_events` |
| `candidates(pool, now, saturated: &[Uuid], limit: i64) -> Vec<Candidate>` | § The candidate query. It takes the **reader** pool: the scan must never hold the single writer |
| `claim(conn, id, now) -> Option<u32>` | `UPDATE eval_jobs SET status = 'running', claims = claims + 1, updated_at = ? WHERE id = ? AND status = 'queued' RETURNING claims` |
| `start_attempt(conn, id, claims, now) -> Option<u32>` | `UPDATE … SET attempts = attempts + 1, updated_at = ? WHERE id = ? AND status = 'running' AND claims = ? RETURNING attempts` |
| `finish(conn, id, claims, finish: Finish, now) -> bool` | one `UPDATE … WHERE id = ? AND status = 'running' AND claims = ?` per variant (columns as in the enum docs); false when no row matched (D-I-12: drop the result) |
| `skip_queued_of_rule(conn, rule_id, error: &'static str, now) -> u64` | queued jobs of the rule → `skipped` with `error`, `finished_at = now` |
| `requeue_stale(conn, older_than, now) -> (u64, u64)` | the janitor: the interruption rule below for `running` jobs with `updated_at < older_than`. Returns `(requeued, failed)` |
| `recover(conn, now) -> (u64, u64)` | startup: the interruption rule for every `running` job. Returns `(requeued, failed)` |

**The interruption rule.** Both functions do, in one transaction and in this order (D-I-12): `running` jobs (of the cut-off, for the janitor) with `interruptions >= 3` → `failed`, `error_class = 'permanent'`, `error = 'interrupted too often'`, `finished_at = now`, `updated_at = now`; then the other ones → `queued`, `not_before = now`, `interruptions = interruptions + 1`, `updated_at = now`. The SQL text is fixed: a shared private helper runs the two statements with `updated_at < ?` bound to the cut-off, and `recover` passes `i64::MAX`.
| `prune(conn, now) -> PruneReport` | § Pruning |
| `counts_since(pool, project, since) -> HashMap<Uuid, JobCounts>` | `SELECT rule_id, status, COUNT(*) … WHERE project_id = ? AND rule_id IS NOT NULL AND created_at >= ? GROUP BY rule_id, status`. `JobCounts { queued, running, done, failed, skipped: u64 }` (`Default`) lives in this module |

`trace_id`, `span_id` and `judge_trace_id` are stored as their 16 and 8 raw bytes (`TraceId::as_bytes`/`from_bytes`, `SpanId` likewise; read `crates/core/src/ids.rs`). A blob of the wrong length is `MetaError::Corrupt`.

**The candidate query.** One statement on the reader pool; `saturated` is bound as a JSON array of UUID strings (`serde_json::to_string`), so the SQL text stays fixed:

```sql
SELECT * FROM (
  SELECT j.<every column>, e.kind AS evaluator_kind, e.connection_id,
         c.max_concurrency, c.requests_per_minute, c.updated_at AS connection_updated_at,
         ROW_NUMBER() OVER (PARTITION BY j.priority, j.project_id ORDER BY j.not_before, j.id) AS rn
  FROM eval_jobs j
  JOIN evaluators e ON e.id = j.evaluator_id
  JOIN projects p ON p.id = j.project_id
  JOIN orgs o ON o.id = p.org_id
  LEFT JOIN online_rules r ON r.id = j.rule_id
  LEFT JOIN llm_connections c ON c.id = e.connection_id
  WHERE j.status = 'queued' AND j.not_before <= ?1
    AND p.state = 'active' AND o.state = 'active'
    AND (j.rule_id IS NULL OR (r.enabled = 1 AND r.paused_reason IS NULL))
    AND COALESCE(e.connection_id, e.id) NOT IN (SELECT value FROM json_each(?2))
)
WHERE rn <= 20
ORDER BY priority, not_before, id
LIMIT ?3
```

`orgs.state` and `projects.state` are both `'active'` or `'deleting'` (`0001_identity.sql`).

**Pruning** (`prune`, D-I-28), in one transaction, in this order, returning `PruneReport { deleted, expired, inactive, spend: u64 }`:
1. `inactive`: `DELETE FROM eval_jobs WHERE project_id IN (SELECT p.id FROM projects p JOIN orgs o ON o.id = p.org_id WHERE p.state <> 'active' OR o.state <> 'active')`.
2. `expired`: queued jobs with `created_at < now − 24 h` (kind ≠ backfill) or `< now − 7 days` (backfill) → `skipped`, `error = 'expired'`, `finished_at = now`.
3. `deleted`: `DELETE … WHERE finished_at IS NOT NULL AND finished_at < now − 7 days AND status IN ('done', 'failed', 'skipped')`.
4. `spend`: `rule_spend::prune_spend(conn, now − 62 days)` (§ below).

### `meta/rule_spend.rs`

| Function | What |
|---|---|
| `add_spend(conn, rule_id, day, usd)` | `INSERT INTO rule_spend (rule_id, day, spent_usd) SELECT ?, ?, ? WHERE EXISTS (SELECT 1 FROM online_rules WHERE id = ?) ON CONFLICT(rule_id, day) DO UPDATE SET spent_usd = spent_usd + excluded.spent_usd`; a non-finite or negative `usd` adds 0. A rule deleted during a run inserts nothing, instead of failing the foreign key and with it the whole `finish` write (§ I3.6). SQLite needs a `WHERE` clause on an `INSERT … SELECT` before `ON CONFLICT` to parse it; the `WHERE EXISTS` is that clause |
| `spent_between(pool, rule_id, from_day, to_day) -> f64` | `SELECT COALESCE(SUM(spent_usd), 0) … WHERE rule_id = ? AND day >= ? AND day < ?` |
| `prune_spend(conn, before_day) -> u64` | deletes rows with `day < before_day`; 62 days keep two whole months for the month view |

There is no batch read: the rules list calls `spent_between` once per budgeted rule of the page it answers.

### Dead code

Nothing outside the tests calls the three modules before I3.6–I3.9. Start each of `meta/online_rules.rs`, `meta/eval_jobs.rs` and `meta/rule_spend.rs` with the repo's pattern for meta modules (`meta/api_keys.rs` line 2):
```rust
#![cfg_attr(not(test), allow(dead_code))] // I3.6 to I3.9 call these.
```
I3.6, I3.7, I3.8 and I3.9 each try to remove these attributes (§ Conventions › Dead code): a sub-phase removes a module's attribute when `just check-rust` passes without it, and reports what it removed. After I3.9, an item that is still unused gets an item-level `#[allow(dead_code)]` naming its M4 caller, and the module attribute goes.

### Spec

`docs/spec/04-metadata-db.md` § Evaluation: replace the `online_rules`, `eval_jobs` and `rule_spend` DDL with the migration's (keep `pending_traces` as it is; M4 replaces it). Replace "Done/failed `eval_jobs` are pruned after 7 days." with: "`eval_jobs` is the durable queue (spec 06 § Online rules › Execution; D-I-11, D-I-12): `dedup_key` makes enqueues idempotent, `claims` fences a worker's finish, and there is no lease. Every hour, finished jobs older than 7 days are deleted, queued jobs older than 24 h (7 days for backfill) are skipped as `expired`, and jobs of inactive projects or orgs are deleted (D-I-28). `rule_spend` holds one row per rule and UTC day; a month is the sum of its days; rows older than 62 days are deleted."

### Tests

With `crate::meta::test_db()`, an org, a project, a connection, an evaluator (`meta::evaluators::insert_evaluator`, read it), and rules and jobs from the new functions:

`meta/online_rules.rs`:
- round trip of every column; list ordered by name;
- duplicate name → `Conflict`; the 500 cap;
- `update_rule` of an unknown id → `None`; `delete_rule` reports whether a row went;
- `count_enabled_rules` counts enabled rules of every project of the org, paused ones included, and not another org's;
- `pause_rule` pauses once (the second call returns false) and never a disabled rule;
- `pause_rules_of_connection` pauses the rules of every evaluator of that connection, across two projects of the org, and returns their ids; `resume_connection_pauses` clears only `'connection'` pauses;
- `clear_budget_pauses` clears a day pause from before `day_start` and keeps one after it, and the same for month;
- deleting the project cascades.

`meta/eval_jobs.rs`:
- `insert_job` twice with the same rule and target inserts once (dedup); manual jobs never collide;
- `claim` → `Some(1)`, a second `claim` → `None`; `start_attempt` with a stale `claims` → `None`;
- `finish` with a stale `claims` is false and changes nothing; each `Finish` variant sets exactly its columns (one assertion block per variant);
- `candidates`: priority order; at most 20 per (priority, project): 25 jobs of project A and 1 of project B at the same priority give 21 rows including B's; `saturated` excludes a connection's jobs and a webhook evaluator's jobs (by evaluator id); a paused or disabled rule's jobs are excluded, manual jobs are not; `not_before` in the future is excluded; a `deleting` project's jobs are excluded;
- `recover`: `interruptions` 3 → failed `interrupted too often`; `interruptions` 2 with `claims` 6 → queued with `interruptions` 3 (claims do not count);
- `requeue_stale` only touches `running` rows older than the cut-off, and applies the same `interruptions` rule;
- `finish` with `PauseRequeue` after `start_attempt` leaves `attempts` where it was before the attempt, and never below 0;
- `list_jobs`: each `JobFilter` field alone, and `before` pages newest first (`limit` 1, three rows, the cursor of each page is the last row's `(created_at, id)`), with two rows sharing a `created_at` ordered by `id DESC`;
- `prune`: one case per step;
- `counts_since` groups by rule and status and ignores manual jobs;
- deleting the rule cascades its jobs; deleting the evaluator cascades its jobs.

`meta/rule_spend.rs`: `add_spend` accumulates per day; `add_spend` for a rule id that does not exist is `Ok` and inserts nothing; `spent_between` sums a range; `prune_spend` deletes before the day; a NaN adds 0.

### Done when

`just check` passes at this commit, with the three modules' `cfg_attr(not(test), allow(dead_code))` in place (§ Dead code above).

---

## I3.3 — Crate edits and the queue's policy types

**Goal:** the small crate edits M3 needs, and the pure types the executor and the dispatcher use: budget periods and estimates, the breaker window, retry backoff, the hourly cap and the active-rule check. Each is plain Rust with unit tests and no database.

**Edit only:** `crates/otel/src/cost.rs`, `crates/eval/src/judge/` (the file that defines `Adapter`; find it with `rg -n "pub enum Adapter" crates/eval`), `crates/eval/src/trace.rs` (`provider_name` moves to `Adapter`), `crates/eval/src/runner.rs` (one doc line), `crates/server/src/api/mod.rs`, `crates/server/src/api/online_rules.rs` (new: `BudgetPeriod` only), `crates/server/src/eval/mod.rs` (module lines), `crates/server/src/eval/{budget,breaker,retry,hourly,rule_limit}.rs` (new), `docs/spec/06-evaluation.md`.

**Commit:** `Add the queue's budget, breaker, retry and limit types`

### Crate edits

- `crates/otel/src/cost.rs`, on `impl PriceTable`:
  ```rust
  /// The row that prices `model` for `provider` at `at`, as span costing would pick it.
  #[must_use]
  pub fn price_for(&self, model: &str, provider: Option<&str>, at: i64) -> Option<&PriceRow> {
      lookup(self, model, provider, at)
  }
  ```
  Test in `cost.rs`'s tests: a model of the built-in table (pick one from `prices/builtin.json`) returns a row with `price_input` set; an unknown model returns `None`.
- `crates/eval`: turn the private `provider_name(adapter)` of `trace.rs` into `impl Adapter { /// The `gen_ai.provider.name` the judge span records, and the provider prices are looked up with. #[must_use] pub fn provider_name(self) -> &'static str }` next to `Adapter::from_name`, and call it from `trace.rs`. No behaviour change; the judge-trace snapshots must not move.
- `crates/eval/src/runner.rs`: `RunInput.judge_trace_id`'s doc becomes "Id of the judge trace. The server derives one per attempt (D-I-16)."

### `api/online_rules.rs`

```rust
//! Online rule API types (spec 09 § Evaluation & alerts, D-I-15, D-I-30). I3.5 adds the rest.

/// The period of a rule budget, in UTC.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum BudgetPeriod { Day, Month }
```
with `as_str` and `from_name`, like `EvaluatorKind`. Add `pub mod online_rules;` to `api/mod.rs`. Register nothing in `openapi.rs` yet (I3.5 does, when a route uses it).

### `eval/budget.rs` (D-I-15)

```rust
pub(crate) const DAY_NS: i64 = 86_400_000_000_000;

/// The UTC midnight at or before `now`.
pub(crate) fn day_start(now: i64) -> i64;            // now - now.rem_euclid(DAY_NS)
/// 00:00 UTC on the first day of `now`'s month (chrono: `DateTime::from_timestamp_nanos`, then
/// `with_day(1)` and midnight; convert back with `timestamp_nanos_opt`, falling back to `day_start(now)`).
pub(crate) fn month_start(now: i64) -> i64;
/// The start of the current period.
pub(crate) fn period_start(period: BudgetPeriod, now: i64) -> i64;

/// The judge model's price row when it prices both input and output, else `None` (D-I-15).
pub(crate) fn model_price<'a>(prices: &'a PriceTable, model: &str, adapter: Adapter, now: i64) -> Option<&'a PriceRow>;
// prices.price_for(model, Some(adapter.provider_name()), now) filtered on price_input and price_output being Some.

/// The cost estimate of one judge call: ceil(prompt_chars / 4) input tokens and
/// `max_output_tokens` output tokens at the row's base prices (USD per million tokens). Tiers are ignored.
pub(crate) fn estimate_usd(prompt_chars: usize, max_output_tokens: u32, price: &PriceRow) -> f64;
```

`Budgets` keeps, per rule, the spend of the current period and the reservations, in memory. A reservation is a guard: dropping it without settling gives the amount back, so an early return, a panic (unwinding drops it) or an aborted worker task can never leak a reservation.

```rust
#[derive(Debug, Default)]
pub(crate) struct Budgets { rules: Mutex<HashMap<Uuid, RuleBudget>> }

#[derive(Debug, Clone, Copy, PartialEq)]
struct RuleBudget { period_start: i64, spent: f64, reserved: f64 }

#[derive(Debug)]
pub(crate) enum Reserve<'a> {
    /// Reserved: settle the guard after the call, or drop it to give the amount back.
    Reserved(Reservation<'a>),
    /// `spent + reserved + estimate > budget`: pause the rule.
    Over,
    /// The rule's spend for this period is not loaded: call `load`, then try again.
    NotLoaded,
}

/// One reservation. Dropping it releases the amount; `settle` turns it into the actual cost.
#[derive(Debug)]
#[must_use = "dropping a reservation releases it"]
pub(crate) struct Reservation<'a> {
    budgets: &'a Budgets,
    rule: Uuid,
    period_start: i64,
    amount: f64,
    done: bool,
}

impl Reservation<'_> {
    /// After a call: the reservation becomes `actual` spend.
    pub(crate) fn settle(mut self, actual: f64);
}

impl Drop for Reservation<'_> {
    fn drop(&mut self) { if !self.done { /* release: reserved -= amount, clamped at 0 */ } }
}

impl Budgets {
    pub(crate) fn try_reserve(&self, rule: Uuid, period_start: i64, budget: f64, estimate: f64) -> Reserve<'_>;
    /// Sets the spend read from `rule_spend` for `period_start`, only when the rule has no entry
    /// for that period. An entry for the same period is kept as it is: another job loaded it first,
    /// and settles may have added to it since that read.
    pub(crate) fn load(&self, rule: Uuid, period_start: i64, spent: f64);
    /// The view of the rule: `(spent, reserved)` for `period_start`, or `None`.
    pub(crate) fn view(&self, rule: Uuid, period_start: i64) -> Option<(f64, f64)>;
    /// Drops the rule (deleted, or its budget changed).
    pub(crate) fn forget(&self, rule: Uuid);
}
```

`try_reserve` returns `NotLoaded` when the rule has no entry or its `period_start` differs. Settling and releasing do nothing when the stored period differs (the period turned during the call: the new period's spend is read from `rule_spend`, which `finish` updates) or the entry was forgotten. Reservations never go below 0 (clamp). `settle` and `Drop` take the `Mutex` briefly and never panic (`unwrap_or_else(PoisonError::into_inner)`). The executor (I3.6) reads `rule_spend` when it gets `NotLoaded` — so the view is loaded on a rule's first check and at each period change, not at startup.

### `eval/breaker.rs` (D-I-14)

```rust
pub(crate) const WINDOW: usize = 50;
pub(crate) const MIN_JOBS: usize = 20;

#[derive(Debug, Default)]
pub(crate) struct Breakers { windows: Mutex<HashMap<Uuid, VecDeque<bool>>> }

impl Breakers {
    /// Records one job that reached the evaluator (`failed` true when it failed). True when the
    /// rule must pause: at least `MIN_JOBS` recorded and half or more failed. A trip clears the window.
    pub(crate) fn record(&self, rule: Uuid, failed: bool) -> bool;
    /// Clears the rule's window (resume, a connection key change, delete).
    pub(crate) fn reset(&self, rule: Uuid);
}
```

### `eval/retry.rs` (D-I-13)

```rust
pub(crate) const MAX_ATTEMPTS: u32 = 6;
pub(crate) const LOAD_RETRIES: u32 = 3;
pub(crate) const LOAD_RETRY_NS: i64 = 30_000_000_000;
pub(crate) const WRITE_TRIES: u32 = 3;
pub(crate) const WRITE_RETRY: Duration = Duration::from_secs(5);
/// A job that got no trace-load permit in 30 s goes back to the queue this much later, without
/// spending a load retry: the wait measured the other jobs, not this trace (D-I-23).
pub(crate) const SATURATED_REQUEUE_NS: i64 = 5_000_000_000;

/// `min(5 s · 2^(attempts−1), 10 min)` scaled by a factor in `[0.8, 1.2)` taken from
/// SHA-256(`llmobs.retry.v1` ‖ job id ‖ attempts as u32 BE): deterministic, so a test can pin it.
pub(crate) fn backoff_ns(job: Uuid, attempts: u32) -> i64;
```
The factor is `0.8 + 0.4 · (first 8 bytes as big-endian u64) / 2^64`. `attempts` 0 is treated as 1. Use saturating arithmetic.

### `eval/hourly.rs` (D-I-11)

```rust
pub(crate) const HOURLY_CAP: u32 = 10_000;

#[derive(Debug, Default)]
pub(crate) struct HourlyCaps { rules: Mutex<HashMap<Uuid, HourCount>> }
#[derive(Debug, Clone, Copy)]
struct HourCount { hour: i64, admitted: u32, dropped: u64 }

impl HourlyCaps {
    /// Counts one live match of `rule` in `now`'s UTC hour. False (and counted as dropped) past the cap.
    pub(crate) fn admit(&self, rule: Uuid, now: i64) -> bool;
    /// Matches dropped in `now`'s UTC hour.
    pub(crate) fn dropped(&self, rule: Uuid, now: i64) -> u64;
    pub(crate) fn forget(&self, rule: Uuid);
}
```
The hour is `now.div_euclid(3_600_000_000_000)`; a new hour resets both counts. `admit` has no production caller until M4's sweeper: give it an item-level `#[allow(dead_code)]` with the comment `// M4's sweeper calls it (D-I-11).` (M2 § Conventions).

### `eval/rule_limit.rs` (D-I-32)

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the plan allows {limit} active online rules")]
pub(crate) struct RuleLimitError { pub(crate) limit: u64 }

/// Ok when `active_after` (the enabled rules of the org, counted with this one) fits `limit`.
pub(crate) fn check(active_after: u64, limit: Option<u64>) -> Result<(), RuleLimitError>;

/// The org's active-rule limit. Track P's M4 returns
/// `entitlements::of(state, org).limits.max_active_online_rules`; until then every org is
/// unlimited (D-I-32), like `alerts::quota::rule_limit`.
pub(crate) fn limit(_state: &AppState, _org: OrgId) -> Option<u64> { None }

impl From<RuleLimitError> for ApiError  // ApiError::PlanLimit { detail: "Your plan allows {n} active online rule." / "rules." }
```

### Spec

`docs/spec/06-evaluation.md` § Online rules › Execution, the **Budget** bullet: replace "prompt chars/4 × input price + max_output_tokens × output price" with "`ceil(prompt chars / 4)` input tokens + `max_output_tokens` output tokens at the model's base prices (`PriceTable::price_for`, tiers ignored)", and add "A rule's spend is kept per UTC day (`rule_spend`); a month sums its days; reservations are in memory (D-I-15)."

### Tests

Unit tests in each module:
- `budget`: `day_start` and `month_start` on 2026-03-15T13:00Z, on an exact midnight, and on 2024-02-29; `estimate_usd` for 400 chars, 100 output tokens at 1 and 4 USD per million is `100·1e-6 + 100·4e-6`; `model_price` is `None` for a row without `price_output`; `Budgets`: `NotLoaded` first, then `Reserved`, then `Over` at the edge (`spent + reserved + estimate > budget` is over, equal is not); `settle` moves the reservation to spend; dropping a `Reservation` gives it back; a guard dropped after `forget` or after the period turned changes nothing; `load` for the period already loaded keeps the entry (load 1.0, reserve 0.5, settle 0.5 → spent 1.5; `load(…, 1.0)` again → still 1.5); a new period is `NotLoaded` again; `forget`.
- `breaker`: 19 failures do not trip; the 20th with ≥ half failed trips and clears the window; 50 successes then 20 failures do not trip (window is the last 50: 30 ok, 20 failed → 40 % failed); 25 ok + 25 failed trips; `reset`.
- `retry`: `backoff_ns` for attempts 1–8 lies in `[0.8, 1.2)` × `5 s · 2^(n−1)` capped at 10 min; the same job and attempt give the same value; two jobs differ.
- `hourly`: 10 000 admitted then false, `dropped` counts; a new hour resets.
- `rule_limit`: `check(5, None)` ok; `check(1, Some(1))` ok; `check(2, Some(1))` is `Err(RuleLimitError { limit: 1 })`; the `ApiError` is `PlanLimit` with `Your plan allows 1 active online rule.`, and with limit 3, `… 3 active online rules.`

Items with no production caller before I3.6 get the item-level `#[allow(dead_code)]` with `// I3.6 uses it.` (or `I3.7`, `I3.8`, `M4`). The sub-phase that first uses an item removes its attribute (§ Conventions › Dead code). Expected: I3.6 removes those of `budget` (except `Budgets::view` and `Budgets::forget`), `Breakers::record` and `retry`; I3.8 those of `rule_limit`, `HourlyCaps::{dropped, forget}`, `Budgets::{view, forget}` and `Breakers::reset`; `HourlyCaps::admit` keeps its M4 attribute. The compiler decides: a sub-phase removes an attribute when the build passes without it, and reports what it removed.

### Done when

`just check` passes, and `cargo insta test -p llmobs-eval` reports no snapshot change.

---

## I3.4 — The shared executor core and evaluator scores

**Goal:** the run path of M2's test run moves into `eval/execute.rs`, so test runs (now) and queued jobs (I3.6) run evaluators, count usage and commit judge traces the same way. `scores::write` accepts evaluator scores.

**Edit only:** `crates/server/src/eval/execute.rs` (new), `crates/server/src/eval/test_run.rs`, `crates/server/src/eval/mod.rs` (module line), `crates/server/src/scores.rs`, `docs/spec/06-evaluation.md`.

**Commit:** `Share the evaluator run path and write evaluator scores`

### `eval/execute.rs`

Read `eval/test_run.rs` first: the code below is moved out of it, not rewritten.

```rust
//! Running one evaluator: the shared path of test runs (D-I-21) and queued jobs (D-I-12).

/// What one run needs besides the evaluator.
pub(crate) struct RunArgs<'a> {
    pub(crate) project: &'a ProjectRow,
    pub(crate) evaluator: Evaluator,                     // moved into spawn_blocking for checks
    pub(crate) connection: Option<Arc<JudgeConnection>>,
    pub(crate) webhook_secret: Option<Secret>,
    pub(crate) variables: Variables,
    pub(crate) target: EvalTarget,
    pub(crate) origin: EvalOrigin,
    pub(crate) judge_trace_id: TraceId,
    pub(crate) now: i64,
}

/// Runs the evaluator once. A check runs inside `spawn_blocking` (D-I-23).
///
/// # Errors
///
/// [`ExecuteError::Panicked`] when the blocking task panicked or was cancelled.
pub(crate) async fn run_evaluator(state: &AppState, args: RunArgs<'_>) -> Result<EvalRun, ExecuteError>;

/// Tokens and cost of the run's `llm` span, when it has one (D-I-18).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct JudgeUsage { pub(crate) input_tokens: Option<u64>, pub(crate) output_tokens: Option<u64>, pub(crate) cost_usd: Option<f64> }
pub(crate) fn judge_usage(run: &EvalRun) -> Option<JudgeUsage>;

/// Adds the usage to `usage_daily` (zero for absent counts) (D-I-31).
pub(crate) fn count_judge_usage(state: &AppState, project: &ProjectRow, now: i64, usage: JudgeUsage);

/// Commits the judge trace in its own task (D-G-38). Its spans are not billable (D-I-31).
pub(crate) async fn commit_judge_trace(state: &AppState, project: &ProjectRow, now: i64, trace: Normalized) -> Result<CommitReceipt, IngestError>;

/// The judge trace id of a job's run (D-I-16): the first 16 bytes of
/// SHA-256(`llmobs.judge-trace.v1` ‖ job id (16 bytes) ‖ the run's claims as u32 BE).
pub(crate) fn job_judge_trace_id(job: Uuid, claims: u32) -> TraceId;

/// A check's `output` or `expected` over 64 KiB (the distance is quadratic).
pub(crate) fn levenshtein_too_large(evaluator: &Evaluator, variables: &Variables) -> bool;

#[derive(Debug, thiserror::Error)]
pub(crate) enum ExecuteError {
    #[error("the evaluator task failed")]
    Panicked(#[source] tokio::task::JoinError),
}
```

- `run_evaluator` is `test_run.rs`'s two `runner.run(RunInput { … })` branches, unchanged, building `RunInput` from `args` (`ingest` from `state.ingest.context(project, now)`, `prices` from `state.ingest.prices()`). The check branch keeps `spawn_blocking` + `Handle::block_on`.
- `judge_usage` finds the `llm` span (`span.span_kind == SpanKind::Llm`) as `test_run.rs` does. `count_judge_usage` is the `state.usage.add_judge(…)` call moved here.
- `commit_judge_trace` is the `reserve_exact(0)` + `commit_in_task(…, CommitTarget { request_bytes: 0, … })` pair.
- `job_judge_trace_id`: `TraceId::from_bytes` refuses all zeros; SHA-256 output is never all zeros in practice, but handle it: on `Err`, flip the last byte to 1 and build again (comment the invariant). Add the constant `const JUDGE_TRACE_TAG: &[u8] = b"llmobs.judge-trace.v1";` as a plain literal, like `DERIVE_TAG` in `scores.rs`, with the comment "A hash input, not an attribute name: the D46 rename must not change judge trace ids."
- `levenshtein_too_large` is `test_run.rs`'s check, moved; `render` moves with it if `test_run.rs` no longer needs it, else make it `pub(crate)` in `execute.rs` and import it.

`test_run.rs` keeps its body rules, its answer building and its 4xx/503 mapping, and calls these functions. `ExecuteError::Panicked` maps to `ApiError::Internal`. The test-run tests must pass unchanged: that is the proof the move kept the behaviour.

`job_judge_trace_id` has no production caller before I3.6: item-level `#[allow(dead_code)]` with `// I3.6 uses it.`

### `scores.rs`: `Writer::Evaluator` (D-I-18)

```rust
/// An evaluator through the queue: a rule's job, or a manual run (`rule_id: None`).
Evaluator { kind: EvaluatorKind, evaluator_id: Uuid, rule_id: Option<Uuid> },
```
- `source`: `(Self::Evaluator { kind, .. }, None)` and `(…, Some(s))` when `s` equals the kind's source → the kind's source: `LlmJudge` → `ScoreSource::LlmJudge`, `Check` → `Check`, `Webhook` → `Webhook`. Any other requested source → `None` (refused).
- `author`: `rule_id.unwrap_or(evaluator_id)`.
- `author_user`: `None`.
- A new private `fn evaluator_ids(self) -> (Option<Uuid>, Option<Uuid>)`: `(Some(evaluator_id), rule_id)` for `Evaluator`, `(None, None)` otherwise. `write` sets `evaluator_id` and `rule_id` on each row from it, instead of the current `None`s.

`EvaluatorKind` is `crate::api::evaluators::EvaluatorKind`. `Writer` stays `Copy`. The job id is not in the writer: the executor puts it in the score metadata (I3.6). This narrows D-I-18's `Writer::Evaluator { …, job_id }`; the design was updated with this plan.

The variant has no production caller before I3.6: item-level `#[allow(dead_code)]` on the variant with `// I3.6 writes evaluator scores.` (Tests use it.)

### Spec

`docs/spec/06-evaluation.md` § Scores: after the source list, add "Evaluator scores (`llm_judge`, `check`, `webhook`) are written by the eval queue with `evaluator_id` set, and `rule_id` set for rule jobs; their derived id's author is the rule, or the evaluator for a manual run (D-I-18)."

### Tests

`scores.rs` tests:
- `evaluator_writer_sets_source_ids_and_author`: write one item with `Writer::Evaluator { kind: Check, evaluator_id: E, rule_id: Some(R) }`; the stored row has `source = check`, `evaluator_id = E`, `rule_id = R`, `author_user_id = None`; its derived id equals `derive_id(project, target, name, Check, R)`. Then with `rule_id: None` the derived id uses `E`.
- `evaluator_writer_refuses_another_source`: `source: Some(Human)` → 400 on `scores[0].source`.
- `evaluator_scores_count_in_usage`: after the write, `state.usage` flushed (`state.usage.flush(&state.meta)`) gives `usage_daily.scores = 1` for the day (read how `usage.rs` tests read `usage_daily`). This is the design's "evaluator scores count in `usage_daily.scores`".

`execute.rs` tests:
- `job_judge_trace_id_is_pinned`: compute SHA-256 of the tag ‖ the 16 bytes of `01900000-0000-7000-8000-000000000800` ‖ `00 00 00 01` (claims 1) outside this code (for example `printf` with `xxd -r -p | sha256sum`), pin the first 32 hex digits, and assert. A different `claims` gives a different id.
- `judge_usage_reads_the_llm_span` on a hand-built `EvalRun` (or the result of a check run: no `llm` span → `None`).

The test-run tests in `routes/evaluators.rs` pass unchanged.

### Done when

`just check` passes.

---

## I3.5 — API contract: rule and job types, OpenAPI, fixtures, mocks

**Goal:** every M3 API type, its OpenAPI operations, the example fixtures, the web mocks and query helpers. The routes come in I3.8 and I3.9; the web pages build on this contract in parallel with them.

**Edit only:** `crates/server/src/api/online_rules.rs`, `crates/server/src/api/eval_jobs.rs` (new), `crates/server/src/api/mod.rs`, `crates/server/src/api/openapi.rs`, `crates/server/src/api_examples.rs`, `crates/server/tests/openapi.rs`, `crates/server/tests/api_examples.rs`, `crates/server/openapi.json`, `fixtures/api/static/{online-rules,online-rule,eval-jobs,eval-job,eval-job-queued}.json` (new), `fixtures/api/requests/{online-rule-create,evaluator-run}.json` (new), `fixtures/api/manifest.json`, `fixtures/api/README.md`, `web/src/api/schema.gen.ts`, `web/src/api/queries-online-rules.ts` (new), `web/src/mocks/writes.ts`, `web/src/mocks/writes.test.ts`.

**Commit:** `Add the online rule and eval job API contract`

### Types

In `api/online_rules.rs`, after `BudgetPeriod`: `RuleTarget`, `PauseReason`, `RuleSpend`, `JobCounts` (the API copy: `ToSchema`, `Serialize`, `Deserialize`, five `u64` fields), `OnlineRule`, `OnlineRulePage`, `CreateOnlineRuleRequest`, `UpdateOnlineRuleRequest`, as § Online rule contract. `variable_mapping` is `BTreeMap<String, String>`. `OnlineRulePage` gets a hand-written `PartialSchema` + `ToSchema` (copy `Page<T>`'s in `api/common.rs`, adding `org_enabled_rules`: integer, minimum 0, required).

In `api/eval_jobs.rs`: `EvalJobKind`, `EvalJobStatus`, `EvalJob`, `EvalJobListParams`, `RunEvaluatorRequest`, as § Eval job contract. `trace_id`, `span_id` and `judge_trace_id` are `TraceId`/`SpanId` (they serialize as lowercase hex; check how `api/scores.rs` declares them in the schema and copy that). `RunEvaluatorRequest.trace_id`/`span_id` are `String`s parsed by the route, like `EvaluatorTestRequest`'s.

Unit tests next to the types: `OnlineRule` round-trips the fixture and omits absent options; `UpdateOnlineRuleRequest` refuses `target` and `evaluator_id` (unknown fields) and `null` values; `EvalJobListParams` parses `rule_id=…&status=failed&kind=manual&limit=10`.

### OpenAPI (`api/openapi.rs`)

Register the schemas in the schema list of `openapi.rs` (`set.insert::<T>()` for each type; `set.insert_page::<EvalJob>("EvalJobPage")`; `set.insert::<OnlineRulePage>()`, which works because its hand-written `ToSchema::name()` is `OnlineRulePage`) and add `online_rule_paths` and `eval_job_paths`, built like `evaluator_paths`. Tag `evaluation`. Operations:

| operation_id | Method and path | Request | 2xx | Errors | Description |
|---|---|---|---|---|---|
| `list_online_rules` | `GET {PROJECT}/online-rules` | `PageParams` | `200 OnlineRulePage` | `QUERY_ERRORS` | "Any project member can read. `org_enabled_rules` counts the enabled rules of the whole org (D-I-32)." |
| `post_online_rule` | `POST {PROJECT}/online-rules` | `CreateOnlineRuleRequest` | `201 OnlineRule` | 400, 401, 403, 404, 409, 429 | "Project members and above. API keys cannot call it. 403 `plan_limit` when the org is at its active-rule limit." |
| `get_online_rule` | `GET {PROJECT}/online-rules/{id}` | — | `200 OnlineRule` | 400, 401, 403, 404, 429 | "Any project member can read." |
| `patch_online_rule` | `PATCH {PROJECT}/online-rules/{id}` | `UpdateOnlineRuleRequest` | `200 OnlineRule` | 400, 401, 403, 404, 409, 429 | "Project members and above. `target` and `evaluator_id` cannot change. Enabling a rule checks the active-rule limit." |
| `delete_online_rule` | `DELETE {PROJECT}/online-rules/{id}` | — | `204` | 400, 401, 403, 404, 429 | "Project members and above. Queued jobs of the rule are deleted with it." |
| `post_online_rule_resume` | `POST {PROJECT}/online-rules/{id}/resume` | — | `200 OnlineRule` | 400, 401, 403, 404, 429 | "Project members and above. Clears the rule's pause." |
| `list_eval_jobs` | `GET {PROJECT}/eval-jobs` | `EvalJobListParams` | `200 EvalJobPage` | `QUERY_ERRORS` | "Any project member can read. Newest first. Jobs are kept 7 days after they finish." |
| `get_eval_job` | `GET {PROJECT}/eval-jobs/{id}` | — | `200 EvalJob` | 400, 401, 403, 404, 429 | "Any project member can read." |
| `post_evaluator_run` | `POST {PROJECT}/evaluators/{id}/run` | `RunEvaluatorRequest` | `202 EvalJob` | 400, 401, 403, 404, 429 | "Project members and above. API keys cannot call it. Queues one run at the highest priority and answers the job; poll `GET /eval-jobs/{id}`. Shares the 10 per minute per user limit with test runs." |

`tests/openapi.rs` (§ Conventions):
- `OPERATIONS`: the nine `(method, path, operation_id)` tuples of the table, with the paths written out (`/api/v1/projects/{project}/online-rules`, `/api/v1/projects/{project}/online-rules/{id}`, `/api/v1/projects/{project}/online-rules/{id}/resume`, `/api/v1/projects/{project}/eval-jobs`, `/api/v1/projects/{project}/eval-jobs/{id}`, `/api/v1/projects/{project}/evaluators/{id}/run`), after the evaluator operations;
- `QUERY_OR_BODY`: `list_online_rules` and `list_eval_jobs`;
- `expected_errors`: a branch `post_online_rule || patch_online_rule` → `{400, 401, 403, 404, 409, 429}`, and a branch `get_online_rule || delete_online_rule || post_online_rule_resume || get_eval_job || post_evaluator_run` → `{400, 401, 403, 404, 429}`, both before the `READ_OPERATIONS` branch;
- the two `pages` lists: `("OnlineRulePage", "OnlineRule")` and `("EvalJobPage", "EvalJob")` in the first, both names in the second. If the `next_cursor` assertions in the first loop need `OnlineRulePage`'s schema to look exactly like `Page<T>`'s, make it so.

The routes come in I3.8 and I3.9. No test requires a documented operation to have a route (`http/access.rs` checks the other direction: every documented route is in the document), so the operations can land first.

### Fixtures

The values are § Fixed identifiers; times are `2026-01-01T…` with nine fractional digits, as `evaluator.json`.
- `static/online-rule.json`: rule `…0700` (budget, `spend: {period_start: "2026-01-01T00:00:00.000000000Z", spent_usd: 0.42, reserved_usd: 0.01}`, `jobs_last_24h: {queued: 2, running: 1, done: 40, failed: 3, skipped: 0}`, `dropped_last_hour: 0`, `created_by` …0100).
- `static/online-rules.json`: `{data: [rule …0700, rule …0701], next_cursor: null, org_enabled_rules: 2}`; `…0701` is paused (`paused_reason: "errors"`, `paused_at`, `paused_detail: "half or more of the last 50 jobs failed"`), no budget, no `spend`.
- `static/eval-job.json`: job `…0800`, `done`, 1 attempt, tokens 120 / 8, `cost_usd: 0.00021`, `latency_ms: 850`, `finished_at`.
- `static/eval-jobs.json`: `{data: [job …0800, job …0801], next_cursor: null}`; `…0801` is `failed`, 6 attempts, `error_class: "retryable"`, `error: "provider status 503"`.
- `static/eval-job-queued.json`: job `…0802`, `manual`, `queued`, 0 attempts, no `rule_id`.
- `requests/online-rule-create.json`: the create body of rule `…0700`.
- `requests/evaluator-run.json`: `{trace_id: "a1000000000000000000000000000001", trace_start: "2026-01-01T10:00:00.000Z", variable_mapping: {input: "trace.input", output: "trace.output"}}`. Check `trace_start` against trace 1's `start_time` in `fixtures/api/dataset/spans.json` and use that value.

`api_examples.rs`: `list_online_rules` → `online-rules.json`; `get_online_rule` (`id` …0700) → `online-rule.json`; `post_online_rule` (201) → `online-rule.json`; `post_online_rule_resume` (`id` …0701, 200) → `online-rule.json`; `list_eval_jobs` → `eval-jobs.json`; `get_eval_job` (`id` …0800) → `eval-job.json`; `post_evaluator_run` (`id` …0500, **202**) → `eval-job-queued.json`, written with struct update syntax because no helper takes both path params and a status: `Example { status: 202, ..example("post_evaluator_run", params(&[("id", "01900000-0000-7000-8000-000000000500")]), BTreeMap::new(), "static/eval-job-queued.json", "application/json") }`. `tests/api_examples.rs`: round-trip every file with its type, and the two request files (`RunEvaluatorRequest` has no `Serialize`: deserialize it only, as M2 does for `evaluator-test.json`), and add them to the request ↔ operation list. Run `just api-examples` for `manifest.json`.

`fixtures/api/README.md` § Fixed identifiers: add the rule, the second rule and the three jobs.

### Web

`web/src/api/queries-online-rules.ts`, modelled on `queries-evaluation.ts`:
- `onlineRulesInit(org, project)` (`limit: 100`), `onlineRulesKey(org, project)`;
- `onlineRuleQuery(org, project, ruleId)`, `onlineRuleKey(org, project, ruleId)`;
- `evalJobsInit(org, project, filters: { rule_id?: string; evaluator_id?: string; status?: Schema<"EvalJobStatus">; kind?: Schema<"EvalJobKind"> })` (`limit: 50`), `evalJobsKey(org, project)`;
- `evalJobQuery(org, project, jobId)`.

`web/src/mocks/writes.ts`: add `post_online_rule`, `patch_online_rule`, `delete_online_rule`, `post_online_rule_resume` and `post_evaluator_run` to `WRITE_OPERATIONS` and `RESPONSES`:
- `post_online_rule`: `{ ...onlineRuleDocument, ...named(body) }` with status 201;
- `patch_online_rule`: `{ ...onlineRuleDocument, ...named(body) }`;
- `delete_online_rule`: `noContent()`;
- `post_online_rule_resume`: `onlineRuleDocument` without `paused_reason`, `paused_at`, `paused_detail`;
- `post_evaluator_run`: `evalJobQueuedDocument` with status 202.
`writes.test.ts` passes with `LATER_TRACKS` still empty.

Run `just openapi` after the Rust types, then the web checks.

### Done when

`just check` passes. `openapi.json` has the nine operations, `schema.gen.ts` has the types, and `just web-dev` would serve the new GET fixtures (not required to look).

---

## I3.6 — Job execution

**Goal:** `execute::run_job` runs one claimed job end to end (§ Job execution) and `finish` writes its outcome, with retries, load retries, write retries, pauses, the breaker and budgets. `AppInner` gains `eval: EvalQueue`. The dispatcher (I3.7) is the only production caller; the tests call `run_job` on jobs they insert and claim.

**Edit only:** `crates/server/src/eval/mod.rs`, `crates/server/src/eval/execute.rs`, `crates/server/src/eval/trace_load.rs` (the `Saturated` error only), `crates/server/src/eval/test_run.rs` (its mapping of `Saturated` only), `crates/server/src/eval/{budget,breaker,retry}.rs` (remove `dead_code` attributes only), `crates/server/src/scores.rs` (remove the variant's `dead_code` attribute only), `crates/server/src/meta/{online_rules,eval_jobs,rule_spend}.rs` (remove the module `dead_code` attribute only, § I3.2 › Dead code), `crates/server/src/http/mod.rs` (`AppInner.eval`), `crates/server/src/serve.rs` (construct it only), `crates/server/src/test_support/mod.rs` (construct it only), `docs/spec/06-evaluation.md`.

**Commit:** `Run queued evaluator jobs`

### `EvalQueue` (`eval/mod.rs`)

```rust
/// The eval queue's in-memory state (D-I-6, D-I-14, D-I-15). One per process, in `AppInner`.
#[derive(Debug, Default)]
pub(crate) struct EvalQueue {
    /// Wakes the dispatcher: a job was queued, or a worker finished.
    wake: tokio::sync::Notify,
    pub(crate) budgets: budget::Budgets,
    pub(crate) breakers: breaker::Breakers,
    pub(crate) hourly: hourly::HourlyCaps,
}

impl EvalQueue {
    /// Wakes the dispatcher (I3.7 waits on it).
    pub(crate) fn wake(&self) { self.wake.notify_one(); }
}
```
Dead code (§ Conventions): `run_job`, `ClaimedJob`, `EvalQueue::wake` and the field `hourly` have no production caller or reader until I3.7 and I3.8. Give each an item-level `#[allow(dead_code)] // I3.7 calls it.` (`// I3.8 reads it.` for `hourly`). Everything `run_job` reaches then counts as used, so remove the I3.3 attributes of what it calls.

`AppInner` gains `pub(crate) eval: crate::eval::EvalQueue` (doc: "The eval queue's in-memory state (D-I-6)."); `serve.rs` and `test_support/mod.rs` set `eval: crate::eval::EvalQueue::default()`. I3.7 adds fields and replaces `Default` with `EvalQueue::new(workers)`.

### `run_job` and `finish` (`eval/execute.rs`)

```rust
/// A job the dispatcher claimed. `claims` is the value after the claim; `finish` is fenced by it.
#[derive(Debug)]
pub(crate) struct ClaimedJob {
    pub(crate) row: JobRow,
    pub(crate) claims: u32,
    /// The judge connection's `updated_at` when the job was claimed (D-I-14); `None` for checks and webhooks.
    pub(crate) connection_updated_at: Option<i64>,
}

/// Runs one claimed job and records its outcome (§ Job execution). Every step's failure is a job
/// outcome; `Err` is only a `meta.db` failure, which leaves the job `running` for the janitor.
pub(crate) async fn run_job(state: &AppState, job: ClaimedJob) -> Result<(), MetaError>;
```

Follow § Job execution step by step; write each step as a small private `async fn` returning `Result<T, Exit>`, where `Exit` is a private enum of the early outcomes (`Finish(Finish)`, `Gone`, `PauseAndFinish { … }`), so `run_job` reads as the table. Details the table does not give:

- **`trace_load.rs`.** Add `LoadError::Saturated` (`#[error("storage is busy")]`) and return it when the permit wait times out (the first `map_err` of `load_trace_capped`); the storage `Busy` and `Timeout` cases stay `Busy`. `test_run.rs` matches `LoadError::Busy | LoadError::Saturated` in its existing `Busy` arm, so test runs answer as before. Add a `trace_load.rs` test: with every permit held (the existing permit test shows how) and the clock paused (`#[tokio::test(start_paused = true)]`), `load_trace` returns `Saturated`.
- **Origin.** `EvalOrigin::Rule(rule_id)` when `row.rule_id` is set (rule and backfill), else `EvalOrigin::Test`.
- **Target.** `EvalTarget { trace_id: Some(row.trace_id), span_id: row.span_id }`.
- **Budget (step 10).** Only when the job has a rule, the rule has `budget_usd` and `budget_period`, and the evaluator is a judge:
  1. `prompt = PromptTemplate::compile(&judge.prompt).and_then(|t| t.render(&variables))`; a template error is `Failed { Permanent, <TemplateError display> }` (it cannot happen for a saved evaluator, but do not unwrap).
  2. `adapter` is the one step 4 parsed (`Adapter::from_name` failing there already ended the job as `"unknown adapter"`); `price = budget::model_price(&state.ingest.prices(), &judge.model, adapter, now)`. `None` → `Failed { Permanent, "model has no price" }` and `pause_rule(rule, PAUSE_ERRORS, Some("model has no price"))`.
  3. `estimate = budget::estimate_usd(prompt.chars().count(), judge.max_output_tokens, price)`; `period_start = budget::period_start(period, now)`.
  4. `state.eval.budgets.try_reserve(rule, period_start, budget, estimate)`; on `NotLoaded`, read `rule_spend::spent_between(reader, rule, period_start, now_day_end)` (`now_day_end = budget::day_start(now) + DAY_NS`), `load`, and try once more. A second `NotLoaded` can only follow a `forget` between the two calls (the rule was edited or deleted); treat it as `Requeue { not_before: now }` and log a `debug` with the rule id.
  5. `Over` → one write: `pause_rule(rule, PAUSE_BUDGET, Some("budget reached"))`, `skip_queued_of_rule(rule, "budget")`, and this job `Finish::Skipped { error: "budget" }`.
  Keep the `Reservation` guard in a local of `run_job` until step 16 settles it. Do not move it into a spawned task: it borrows `state.eval.budgets`.
- **The call (step 12).** `run_evaluator(state, RunArgs { … })`. `ExecuteError::Panicked` → `Failed { Permanent, "internal error" }`.
- **Latency.** `latency_ms` is the wall time of `run_evaluator` (`Instant`).
- **Writes (steps 14–15).** Try up to `retry::WRITE_TRIES` times with `tokio::time::sleep(retry::WRITE_RETRY)` between tries (tests use `#[tokio::test(start_paused = true)]` or a failing storage that recovers; see Tests). The judge trace commit comes first; the score write only on success.
- **The score (step 15).** One `ScoreCreate`:
  - `score_id: None`; `target`: `ScoreTarget::Span { trace_id, span_id }` when `row.span_id` is set, else `ScoreTarget::Trace { trace_id }`;
  - `session_id` and `user_id`: the root span's (the first span of the loaded trace whose `parent_span_id` is `None`; `LoadedTrace.spans` puts the target first, so search, do not take index 0);
  - `name`: the evaluator's `output.name`; `value`, `reason`: the outcome's; `source: None`;
  - `metadata`: the outcome's `metadata` (`judge.*`), plus `llmobs.eval.job_id` = the job id, `llmobs.eval.evaluator_version` = the evaluator row's `version`, and `llmobs.eval.truncated = "true"` when the load was truncated. Build the keys with `llmobs_core::llmobs_name!(".eval.job_id")` etc., as constants in `execute.rs`.
  - `scores::write(state, &project, Writer::Evaluator { kind, evaluator_id, rule_id }, vec![item])`; the answer's single id is the `score_id`.
- **Cost (D-I-15, D-I-18).** `actual = usage.cost_usd`, else the estimate when `usage` has tokens, else 0. `count_judge_usage` runs right after the call (step 13), before any write, as test runs do.
- **The exit after the call.** Steps 12–15 end in one private `struct Ended { finish: Finish, reached_evaluator: bool, pause: Pause, actual: f64 }`, where `Pause` is `None`, `Connection { connection_id, error_text }`, `Evaluator { error_text }` or `MarkConnection { connection_id, error_text }` (a manual judge job). `reached_evaluator` follows § Job execution › The breaker.
- **`finish` (step 16)** is one `MetaDb::write` that, in order:
  1. adds `actual` to `rule_spend` for `day_start(now)` when the job has a budgeted rule and `actual > 0`. This runs even when step 3 finds the job re-claimed: the provider was paid, and the in-memory settle below counts it too, so memory and `rule_spend` agree. When the rule was deleted during the run, `add_spend` inserts nothing (§ I3.2) and the write goes on;
  2. for `Pause::Connection` and `Pause::MarkConnection`: reads the connection row (`connections::get_connection_tx`); when it is missing or its `updated_at` is not `job.connection_updated_at`, the connection changed during the call, so the pause becomes `Pause::None` (and a rule job's `finish` stays `PauseRequeue`);
  3. calls `eval_jobs::finish(conn, id, claims, variant, now)`; when it returns false, it stops there and commits step 1 only (the job was re-claimed or deleted; D-I-12 drops the result silently);
  4. applies the pause that is left: `pause_rules_of_connection` + `connections::set_connection_status(conn, connection_id, "failing", Some(&error_text), now)`, or `pause_rules_of_evaluator(…, PAUSE_ERRORS, Some(&error_text))`, or (`MarkConnection`) only the connection status;
  5. returns whether the row matched and the ids of the rules it paused, for the audit log.
  After the write: `reservation.settle(actual)` when there is one (always, matched or not), log one audit line per paused rule, and, when the row matched and `reached_evaluator`, record the breaker. A trip is one more short write: `pause_rule(rule, PAUSE_ERRORS, Some("half or more of the last 50 jobs failed"))`, then its audit line.
- **Logs.** One `debug` per job: job id, kind, status, attempts, error class, latency. Never the error text of a judge or webhook failure at `info` or above (it is safe, but it is the user's content area); never variables, prompts or answers.


### Spec

`docs/spec/06-evaluation.md` § Online rules › Execution: replace the **Worker pool**, **Job**, **Retries**, **Budget** (keep I3.3's sentence) and **Error breaker** bullets with D-I-12 to D-I-16's rules in short form: the states and fencing (no lease), the step list (load the evaluator and rule; decrypt; load the trace; guard; resolve; check the definition; estimate and reserve; count the attempt; run; commit the judge trace; write the score with `Writer::Evaluator`; finish), the retry table (D-I-13), the pause classes and the breaker (D-I-14), and the per-attempt judge trace (D-I-16).

### Tests (`eval/execute.rs`)

Helpers in the test module: `seeded(app) -> Fixture` (the clock set to real time first, § Conventions; an org, a project, a mock-LLM connection on `MockLlm`, a judge evaluator whose prompt uses `{{ input }}` and `{{ output }}`, a stored target trace via `app.commit_spans` with a root span carrying `session_id` and `user_id`), `rule_job(…)` (inserts a rule and a job with `insert_job`, then `claim`s it and builds `ClaimedJob` with the connection row's current `updated_at`), and `job(app, id)` to read the row back. Use `MockLlm::start(vec![completion(…)])` as M2's test-run tests do, and `MockWebhook` for webhook cases.

- `a_rule_job_writes_the_score_and_the_judge_trace`: `done`, 1 attempt, `judge_trace_id == job_judge_trace_id(id, 1)` (claims 1), `score_id` set; `GET /scores?rule_id=` returns one `llm_judge` score with `evaluator_id`, `rule_id`, the root's `session_id` and `user_id`, metadata `judge.trace_id` and `llmobs.eval.job_id`; `GET /traces/{judge trace}` finds the judge trace in `llmobs-eval`.
- `a_manual_job_has_no_rule_id_and_no_budget`.
- `a_retryable_error_backs_off_then_fails_after_six_attempts`: mock 503 forever; run, re-claim (set `not_before` past with the manual clock: `app.state.clock.set(row.not_before)`), run again… after the 6th attempt the job is `failed`, `retryable`; each retry's `not_before` equals `now + backoff_ns(id, attempts)`.
- `a_pause_error_pauses_the_connection_rules_and_requeues`: mock 401; two rules in two projects on the same connection; the job is `queued` with `attempts` 0 (the run's attempt was given back), both rules `paused_reason = 'connection'`, the connection `failing` with `provider status 401`.
- `a_key_change_during_the_call_requeues_without_pausing`: call the step-16 function (`finish_job(state, &job, ended, reservation)`; write step 16 as this standalone `async fn` so tests can drive it) on a claimed and started rule job, with `Ended { finish: PauseRequeue { … }, pause: Pause::Connection { … }, … }` and a `connection_updated_at` older than the row's (a key PATCH in between): the job is `queued`, `attempts` back to 0, no rule paused, the connection status unchanged.
- `an_unknown_adapter_fails`: set the connection row's `adapter` to `nope` with SQL in the test → `failed`, `permanent`, `unknown adapter`, the mock got no request.
- `a_changed_connection_before_the_call_requeues`: `connection_updated_at` one less than the row's → `queued`, `attempts` 0, the mock got no request.
- `a_webhook_pause_pauses_the_evaluator_rules`: webhook 401.
- `a_manual_pause_fails_and_marks_the_connection`: no rule paused.
- `a_permanent_error_fails_at_once`: mock answers `"not json"` → `failed`, `permanent`.
- `a_missing_trace_retries_three_times_then_fails`: `load_retries` 1, 2, 3, then `failed` with `trace not found`.
- `no_load_permit_requeues_without_a_load_retry`: hold every `trace_loads` permit, run under `start_paused = true` → `queued`, `not_before = now + SATURATED_REQUEUE_NS`, `load_retries` 0.
- `an_evaluation_trace_is_refused`: a target in `llmobs-eval` → `failed`, `target is an evaluation trace`, no judge trace.
- `a_mapping_error_writes_no_judge_trace`: `output` mapped to `span[name=nope].output` without `?` → `failed`, `judge_trace_id` NULL.
- `a_definition_mismatch_fails`: a categorical live definition named like the numeric output.
- `budget_over_pauses_and_skips`: budget 0.000001 USD; two queued jobs of the rule; running one → it is `skipped` (`budget`), the other is `skipped`, the rule is paused `budget`, the mock got no request.
- `budget_spend_is_recorded`: a generous budget; the judge's model is one the built-in table prices for `openai` (pick it from `crates/otel/prices/builtin.json`), and the mock's reply names that model (build the body like `completion` does, with your `model`), so the `llm` span has a `cost_total`; after the run `rule_spend` for today equals that `cost_total`, and `budgets.view` shows `reserved == 0`.
- `a_month_budget_sums_the_days`: `budget_period: month`, `rule_spend` rows on two earlier days of this month adding up to just under the budget → the job is `skipped` (`budget`); with the same rows in the previous month, it runs.
- `a_lost_claim_still_records_the_spend`: a budgeted rule; run a `ClaimedJob` whose `claims` is stale → the row is untouched, `rule_spend` has the cost and `budgets.view` shows `reserved == 0`.
- `a_lost_start_attempt_releases_the_reservation`: re-claim the job between the reservation and step 11 (call the step functions directly) → `budgets.view` shows `reserved == 0`.
- `an_unpriced_answer_spends_the_estimate`: the same, but the mock's reply names `gpt-test` (unpriced): `rule_spend` equals the estimate.
- `model_without_price_fails_and_pauses`: budgeted rule, model `gpt-test` (unpriced) → `failed`, `model has no price`, rule paused `errors`.
- `a_stale_claim_drops_the_result`: claim, then `requeue_stale` + `claim` again (claims 2); run the first `ClaimedJob` (claims 1): the job row is untouched by its finish, but the score and the judge trace exist (D-I-12 accepts this).
- `the_breaker_pauses_after_twenty_failures`: 20 jobs answering `"not json"` (permanent judge errors) → the rule is paused `errors` after the 20th.
- `a_paused_rule_requeues_its_claimed_job`.
- `a_deleted_evaluator_ends_quietly`: delete the evaluator (cascades the job), run → `Ok(())`.
- `a_rule_deleted_during_the_run_ends_quietly`: a **budgeted** rule, so `finish` reaches `add_spend` with `actual > 0`; delete the rule before step 16 (run the steps directly, then `finish_job`): the job row cascades, `finish` matches nothing, the result is `Ok(())`, and `rule_spend` has no row for the rule.
- `a_check_job_runs_to_done` (a `contains` check, the `spawn_blocking` path) and `a_webhook_job_runs_to_done` (`MockWebhook`): each `done` with its source.
- `a_truncated_trace_marks_the_score`: test the metadata helper directly: `truncated: true` adds `llmobs.eval.truncated = "true"`, `false` adds nothing (a real trace over `MAX_SPANS` is too slow to commit).
- `a_large_levenshtein_input_fails`: `output` over 64 KiB → `failed`, `levenshtein inputs must be at most 64 KiB each`, no judge trace.
- `a_store_failure_does_not_count_for_the_breaker`: 20 `finish_job` calls with `Ended { reached_evaluator: false, … }` with `Failed { Permanent, "could not store the result" }` → the rule is not paused.
- `judge_usage_and_score_units_are_counted`: after one done job and `state.usage.flush`, `usage_daily` has `judge_input_tokens = 10`, `judge_output_tokens = 3` (the mock's usage) and `scores = 1`, and `spans` counts none of the judge spans (D-I-31).

The write-retry path (steps 14–15) is hard to trigger with real storage; test the helper that loops (`retry_write(tries, delay, f)`) with a closure that fails twice then succeeds, and with one that always fails (three calls, then the error), under `#[tokio::test(start_paused = true)]`. Steps 14 and 15 must both go through it.

### Done when

`just check` passes.

---

## I3.7 — Dispatcher, workers and loops

**Goal:** the dispatcher claims jobs within the worker, connection, rate and webhook limits (D-I-6), runs each in its own task, and the janitor, pruner and budget ticker run as loops. Startup recovers interrupted jobs; shutdown stops the loops, gives workers 3 s, then aborts them.

**Edit only:** `crates/server/src/eval/mod.rs`, `crates/server/src/eval/dispatch.rs` (new), `crates/server/src/eval/execute.rs` (remove the I3.6 `dead_code` attributes only), `crates/server/src/meta/{online_rules,eval_jobs,rule_spend}.rs` (remove the module `dead_code` attribute only, § I3.2 › Dead code), `crates/server/src/serve.rs`, `crates/server/src/test_support/mod.rs` (the constructor only), `crates/server/src/test_support/mock_llm.rs` (`start_with_delay` only), `docs/spec/06-evaluation.md`, `docs/spec/12-operations.md`.

**Commit:** `Dispatch eval jobs and run the queue loops`

### `EvalQueue` grows (`eval/mod.rs`)

```rust
pub(crate) struct EvalQueue {
    wake: Notify,
    stop: tokio::sync::watch::Sender<bool>,
    /// `eval.workers` slots (D-I-6).
    workers: Arc<Semaphore>,
    /// Per connection: its slots and its token bucket, rebuilt when the connection's
    /// `max_concurrency` or `requests_per_minute` changes (not on other edits, such as a key change).
    connections: Mutex<HashMap<Uuid, Arc<dispatch::ConnectionLimit>>>,
    /// Per webhook evaluator: 4 slots.
    webhooks: Mutex<HashMap<Uuid, Arc<Semaphore>>>,
    /// Per priority: the project whose job was claimed last, where the next pass's rotation starts (D-I-6).
    last_project: Mutex<HashMap<u8, ProjectId>>,
    pub(crate) budgets: Budgets,
    pub(crate) breakers: Breakers,
    pub(crate) hourly: HourlyCaps,
}

impl EvalQueue {
    pub(crate) fn new(workers: u32) -> Self;
    pub(crate) fn wake(&self);
    /// Tells every eval loop to stop.
    pub(crate) fn stop(&self);
    /// A receiver of the stop flag. Loops wait with `receiver.wait_for(|stopped| *stopped)`, which
    /// also returns at once when `stop` was called before the receiver was made (`changed()` would not).
    pub(crate) fn stopped(&self) -> tokio::sync::watch::Receiver<bool>;
}
```
Write `Debug` by hand (counts only). `serve.rs` and `test_support` build it with `EvalQueue::new(config.eval.workers)`.

`EvalTiming` (like `AlertTiming`):

| Field | `DEFAULT` | `TEST` |
|---|---|---|
| `dispatch_every` | 1 s | 20 ms |
| `janitor_every` | 60 s | 100 ms |
| `prune_every` | 3 600 s | 3 600 s |
| `budget_every` | 30 s | 100 ms |
| `drain` | 3 s | 3 s |

### `eval/dispatch.rs`

```rust
/// Webhook evaluators run at most this many jobs at once (D-I-6).
pub(crate) const WEBHOOK_SLOTS: usize = 4;
/// Most candidates one pass reads; a pass reads `min(CANDIDATES, CANDIDATES_PER_SLOT × free workers)`.
const CANDIDATES: i64 = 400;
/// Candidates read per free worker slot: some candidates are skipped by the pick (a connection
/// that runs out of slots or tokens during the pass).
const CANDIDATES_PER_SLOT: i64 = 4;
/// A job running longer than this goes back to the queue (D-I-12).
const STALE_NS: i64 = 600_000_000_000;

/// A connection's slots and token bucket (D-I-6).
#[derive(Debug)]
pub(crate) struct ConnectionLimit {
    /// `(max_concurrency, requests_per_minute)` it was built for.
    limits: (u32, u32),
    slots: Arc<Semaphore>,
    bucket: Mutex<TokenBucket>,
}

/// `capacity = max(1, rpm / 6)` tokens (10 s of burst), refilled at `rpm / 60` per second.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TokenBucket { capacity: f64, tokens: f64, per_ns: f64, last: i64 }

impl TokenBucket {
    pub(crate) fn new(rpm: u32, now: i64) -> Self;   // starts full
    /// Refills for the time since `last`, then takes one token when there is one.
    pub(crate) fn try_take(&mut self, now: i64) -> bool;
    /// Whether a token is there now, without taking it.
    pub(crate) fn has_token(&mut self, now: i64) -> bool;
}

/// The permits a running job holds. Dropping it frees them.
#[derive(Debug)]
pub(crate) struct Permits { _worker: OwnedSemaphorePermit, _slot: Option<OwnedSemaphorePermit> }

/// One dispatch pass: reads candidates, claims what the limits allow, and spawns each job into
/// `running`. Returns how many jobs it started.
pub(crate) async fn dispatch_once(state: &AppState, now: i64, running: &mut JoinSet<()>) -> Result<usize, MetaError>;

/// Re-queues jobs running for more than 10 minutes, and fails those interrupted too often (D-I-12).
/// Returns `(requeued, failed)`.
pub(crate) async fn janitor_once(state: &AppState, now: i64) -> Result<(u64, u64), MetaError>;

/// Hourly pruning (D-I-28).
pub(crate) async fn prune_once(state: &AppState, now: i64) -> Result<PruneReport, MetaError>;

/// Clears `'budget'` pauses whose period has turned (D-I-15), and wakes the dispatcher when it cleared one.
pub(crate) async fn budget_once(state: &AppState, now: i64) -> Result<usize, MetaError>;
```

**`dispatch_once`** (design § Queue). The scan runs on a reader; the writer is held only for the claims:
1. `free = workers.available_permits()`; if 0, return `Ok(0)`.
2. `saturated`: every connection id whose `slots` has no permit or whose bucket `has_token(now)` is false, and every webhook evaluator id whose semaphore has no permit. Collect under each `Mutex`, then release it.
3. `eval_jobs::candidates(state.meta.reader(), now, &saturated, min(CANDIDATES, CANDIDATES_PER_SLOT × free))`.
4. The limits the pick needs, under the maps' `Mutex`es and released before the pick: for each candidate's connection, the `Arc<ConnectionLimit>` from the map, created from the candidate's `connection_limits` when missing, or replaced when its `limits` differ from the candidate's `(max_concurrency, requests_per_minute)` (running jobs keep the old one's permits until they finish, so the new limit can be exceeded for one call's duration, which is accepted); for each webhook evaluator, its semaphore, created with `WEBHOOK_SLOTS` when missing. Copy `last_project` too.
5. `pick` (below), synchronous: no `.await` between the start of the pick and its end.
6. Nothing picked → return `Ok(0)`. Otherwise one `MetaDb::write` whose closure owns the picked ids (`Vec<Uuid>`) and calls `eval_jobs::claim(conn, id, now)` for each, returning `Vec<Option<u32>>` in the same order. The permits stay outside the closure. A `None` (claimed or removed since the read) drops that job's permits. A write error drops every permit and returns the error.
7. For each claimed job: `running.spawn(run_claimed(state.clone(), ClaimedJob { row, claims, connection_updated_at }, permits))`, where `connection_updated_at` is the third value of the candidate's `connection_limits`.
8. Store, for each priority, the project of the last job claimed at it in `last_project`. Return the number spawned.

**The pick** (pure function `pick(candidates, limits: &PickLimits, last_project: &HashMap<u8, ProjectId>, now) -> Vec<(Candidate, Permits)>`, where `PickLimits` holds the worker semaphore and the step-4 maps; unit-tested without a database):
- Group by `priority` ascending. Within a priority, order the projects by id, starting with the first id greater than `last_project[priority]` and wrapping around (from the smallest id when there is no entry). Take rounds: in each round, for each project in that order, its next candidate (candidates of a project keep the query's `not_before, id` order). Stop when the worker semaphore has no permit left.
- For each candidate: `workers.clone().try_acquire_owned()`; then, for a judge, the connection's `slots.clone().try_acquire_owned()` first and only then `bucket.try_take(now)`, so a token is never spent for a job that has no slot; for a webhook, its semaphore; checks need only the worker permit. When a connection or webhook permit or token fails, add its id to a local `blocked` set and skip every later candidate with that id in this pass. Partial permits are dropped.

**`run_claimed(state, job, permits)`:**
```rust
let id = job.row.id; let claims = job.claims;
// The permits live here, not in the job: they are dropped after the job's finish.
let result = AssertUnwindSafe(execute::run_job(&state, job)).catch_unwind().await;
match result {
    Ok(Ok(())) => {}
    Ok(Err(error)) => tracing::error!(job_id = %id, error = %error, "eval job could not be recorded"),
    Err(_panic) => { /* finish Failed { Permanent, "internal error" }, fenced by claims; log at error with the job id only */ }
}
drop(permits);
state.eval.wake();
```
(`futures::FutureExt::catch_unwind`. This replaces D-I-12's "caught as a `JoinError`"; the design was updated with this plan.)

**`janitor_once`** = `requeue_stale(conn, now − STALE_NS, now)`; wake when it re-queued a job. **`prune_once`** = `eval_jobs::prune`. **`budget_once`** = `clear_budget_pauses(conn, day_start(now), month_start(now), now)`; for each cleared rule, `budgets.forget(rule)` and one audit line `online rule resumed` with `reason = "budget period"`.

### Loops, startup and shutdown (`eval/mod.rs`)

```rust
/// Startup recovery (D-I-12): `running` jobs go back to `queued` with one more interruption,
/// except those already interrupted 3 times, which fail. Logs the counts.
pub(crate) async fn recover(state: &AppState) -> Result<(), MetaError>;

/// The eval loops. Shutdown stops and joins them; it never aborts a job mid-write except after `drain`.
pub(crate) struct EvalTasks { handles: Vec<JoinHandle<()>> }

pub(crate) fn spawn(state: &AppState, timing: EvalTiming) -> EvalTasks;

impl EvalTasks {
    /// Stops every loop and waits for it. The dispatcher loop gives its workers `timing.drain`,
    /// then aborts them; their jobs stay `running` until the next start.
    pub(crate) async fn shutdown(self, state: &AppState);
    /// Aborts every loop now. Aborting the dispatcher drops its `JoinSet`, which aborts the workers.
    pub(crate) fn abort(&self);
}
```
- **Every loop runs one pass as soon as it starts**, which `spawn` does right after `recover`: tokio's `interval` completes its first tick at once, and unlike `alerts/mod.rs` the eval loops do **not** discard it (a server that restarts often would otherwise never prune).
- **Dispatcher loop:** owns `JoinSet<()>`. Each turn: `tokio::select!` on `stopped.wait_for(|stopped| *stopped)`, `interval.tick()` (`dispatch_every`) and `state.eval.wake.notified()`; then reap finished workers (`while running.try_join_next().is_some() {}`), then `dispatch_once(&state, state.clock.now_ns(), &mut running)` (log errors). On stop: `tokio::time::timeout(timing.drain, async { while running.join_next().await.is_some() {} })`, then `running.abort_all()` and drain the aborted ones (`while running.join_next().await.is_some() {}`), logging how many were aborted.
- **Janitor, prune and budget loops:** `interval` + `MissedTickBehavior::Skip` + `select!` with `stopped.wait_for(|stopped| *stopped)`; call the `*_once` function; log errors.
- `shutdown`: `state.eval.stop()`, then await each handle (the dispatcher's includes the drain). Wrap the whole in `tokio::time::timeout(timing.drain + Duration::from_secs(5), …)`; on timeout abort what is left and log it.

`serve.rs`:
- after `bootstrap::prepare` succeeds: `if let Err(error) = crate::eval::recover(&state).await { release_state(state).await; return Err(error.into()); }`;
- next to `alerts::spawn_loops`: `let eval_tasks = crate::eval::spawn(&state, crate::eval::EvalTiming::DEFAULT);` stored in a new `Running` field `eval_tasks: Option<crate::eval::EvalTasks>`;
- `Running::shutdown`: right after `let serve_error = …` (the HTTP drain) and before `for task in self.tasks.drain(..)`: `if let (Some(eval), Some(state)) = (self.eval_tasks.take(), self.state.as_ref()) { eval.shutdown(state).await; }`. Update the method's doc to list the step.
- `impl Drop for Running` already exists (`serve.rs`, it aborts `serve`, `otlp_serve`, `grpc_serve` and `tasks`). Extend it, do not add a second `impl`: after its `for task in &self.tasks` loop, add `if let Some(eval) = &self.eval_tasks { eval.abort(); }`. `shutdown(mut self)` already `take()`s and `drain`s what it awaits, so after a clean shutdown `Drop` finds `eval_tasks` empty. Without it a `Running` dropped without `shutdown` leaves the loops holding `AppState`, and `<data_dir>/LOCK` is never released. Judge-trace commits that workers started are covered by the existing `wait_for_commits` step after it (D-G-38).

### Spec

- `docs/spec/06-evaluation.md` § Online rules › Execution: replace the claim and lease sentences with the dispatcher: "One dispatcher claims jobs every second or when woken, by strict priority (manual, rule, backfill), one job per project per round, when a worker slot, the connection's concurrency slot and a token of its bucket (`max(1, rpm/6)` tokens, refilled at `rpm/60` per second) are free; a webhook evaluator runs at most 4 jobs at once (D-I-6). A janitor re-queues jobs running for more than 10 minutes, and startup re-queues every `running` job; both count an interruption, and a job interrupted a fourth time fails (D-I-12)." And the **Rate limits per connection** bullet: "`max_concurrency` and `requests_per_minute` per connection, shared by every rule of every project (D-I-4, D-I-6)."
- `docs/spec/12-operations.md` § Process lifecycle: in **Startup**, after the key sentence, add "Then `running` eval jobs go back to `queued` (a job interrupted a fourth time fails), and the eval dispatcher, janitor, pruner and budget ticker start, each running one pass at once (D-I-12)." In **Shutdown**, replace "stop workers (leases expire and are re-queued)" with "stop the eval loops and give eval workers 3 s, then abort them; their jobs stay `running` and the next start re-queues them (D-I-12)".

### Tests

`dispatch.rs` unit tests (no database):
- `token_bucket_bursts_then_refills`: rpm 60 → capacity 10; 10 takes succeed, the 11th fails; after 1 s one more succeeds; rpm 3 → capacity 1.
- `pick_is_strict_priority_then_round_robin`: candidates P0 (project A ×2), P1 (A ×3, B ×1) with 4 workers and no `last_project` → A0, A0, then A1, B1 (round-robin within P1).
- `pick_rotates_across_passes`: P1 candidates of projects A and B (5 each, A's id smaller), 1 worker per pass, `last_project` carried from pass to pass as `dispatch_once` does → A, B, A, B.
- `pick_skips_a_full_connection`: connection with 1 slot, two candidates on it and one check → the first and the check.
- `pick_takes_no_token_without_a_slot`: a connection with 0 free slots and a full bucket → nothing picked, the bucket still full.
- `pick_skips_a_webhook_at_four`.

`dispatch.rs` tests with `TestApp` (clock set to real time when an evaluator runs):
- `dispatch_runs_a_manual_job_to_done`: insert a manual job with `insert_job`, `dispatch_once` → 1, `running.join_next().await` → the job is `done`.
- `dispatch_respects_eval_workers`: `with_config(|c| c.eval.workers = 2)`, 5 check jobs, one pass starts 2.
- `dispatch_respects_the_connection_slots`: connection `max_concurrency` 1; the mock provider delays its answer (`MockLlm::start_with_delay`, below); two jobs → one starts; after it finishes, the next pass starts the other.
- `dispatch_respects_the_bucket`: connection `requests_per_minute` 6 (capacity 1); three jobs; the first pass starts one, a pass at the same `now` starts none, a pass 10 s later starts one more.
- `dispatch_holds_a_webhook_at_four`: six webhook jobs on one evaluator with a delayed `MockWebhook` (or a webhook URL that never answers before the test ends) → one pass starts 4.
- `the_rotation_serves_every_project`: 25 manual jobs of project A then 1 of project B, `eval.workers = 1`, jobs on a delayed mock; two passes (each after the previous job finished) start one job of A and then B's job.
- `the_dispatcher_wakes_on_notify`: insert a manual check job (the probe) on a committed trace **before** `spawn`; spawn the loops with `dispatch_every` of 1 hour (a custom `EvalTiming`); poll the probe's row every 50 ms until it is `done` (at most 5 s): the start pass ran and the dispatcher is now waiting. Then insert a second manual check job and call `state.eval.wake()` → poll the same way: the second job is `done` within 5 s. Without the wake, the next pass would be an hour away.
- `dispatch_skips_paused_rules_and_inactive_projects`.
- `a_changed_connection_gets_new_limits`: PATCH the connection's `max_concurrency`, and the next pass builds a new `ConnectionLimit` with it; a PATCH of the key alone keeps the old `ConnectionLimit` (same `Arc`).
- `a_panicked_job_fails_with_internal_error`: no real job can be made to panic, so write the panic arm as its own `async fn finish_panicked(state, id, claims)` and test it: a claimed job becomes `failed`, `permanent`, `internal error`; with a stale `claims` nothing changes.
- `janitor_requeues_stale_jobs`, `prune_once_*` (one per D-I-28 case), `budget_once_clears_a_turned_day_and_keeps_today`.
- `recover_requeues_and_fails_crash_loops`.
- `shutdown_drains_then_aborts` (real time, `EvalTiming::TEST`): `spawn`, a job whose mock answer takes 10 s, `shutdown` returns within about 3 s and the job is still `running`.
- `serve.rs`: the existing tests pass unchanged with the loops running (`healthz_over_tcp_and_shutdown_releases_the_lock` covers a clean `shutdown`). Add `a_dropped_running_releases_the_lock`: `bind` in a temp dir, drop the `Running` without `shutdown`, then `bind` again on the same dir until it succeeds, for at most 5 s (copy `a_second_bind_stops_on_the_lock`'s setup); it must succeed.

**`MockLlm::start_with_delay(replies, delay: Duration)`** (`test_support/mock_llm.rs`): `MockState` gains `delay: Duration`; `start(replies)` becomes `start_with_delay(replies, Duration::ZERO)`. In `handle`, take the reply, drop the `replies` lock, then `tokio::time::sleep(state.delay).await` before answering. `Reply` does not change, so no other test file changes.

### Done when

`just check` passes, and `shutdown_drains_then_aborts` and `the_dispatcher_wakes_on_notify` pass five runs in a row (`for i in 1 2 3 4 5; do cargo test -p llmobs-server <name> || break; done`): they use real time, so a flaky one must be fixed, not retried. I3.7 removes the I3.6 `dead_code` attributes of `run_job`, `ClaimedJob` and `EvalQueue::wake`; the `hourly` field keeps its attribute until I3.8.

---

## I3.8 — Rules API

**Goal:** the online rule routes, with D-I-30's validation and D-I-32's check; the evaluator delete and PATCH 409s (D-I-19); connection tests and key changes resume `'connection'` pauses (D-I-5, D-I-14).

**Edit only:** `crates/server/src/routes/online_rules.rs` (new), `crates/server/src/routes/mod.rs` (module line), `crates/server/src/http/access.rs` (one `routes.extend` line), `crates/server/src/eval/rule_check.rs` (new), `crates/server/src/eval/mod.rs` (module line, and an `impl EvalQueue` block with `forget_rule`), `crates/server/src/eval/{budget,rule_limit}.rs` (remove `dead_code` attributes only), `crates/server/src/meta/online_rules.rs` (one new function, and its module `dead_code` attribute), `crates/server/src/meta/{eval_jobs,rule_spend}.rs` (remove the module `dead_code` attribute only, § I3.2 › Dead code), `crates/server/src/routes/evaluators.rs`, `crates/server/src/routes/connections.rs`, `crates/server/src/http/error.rs`, `crates/server/src/api/openapi.rs` (409 on `delete_evaluator`), `crates/server/tests/openapi.rs` (move `delete_evaluator` from the `{400, 401, 403, 404, 429}` branch to the branch with 409: `post_evaluator || patch_evaluator || …`), `crates/server/openapi.json`, `web/src/api/schema.gen.ts`, `docs/spec/06-evaluation.md`, `docs/spec/09-api.md`.

**Commit:** `Add the online rules API`

### `ApiError::ConflictWith` (`http/error.rs`)

```rust
/// A conflict that names the items in the way (D-I-19): each error's `field` is
/// `rules.<rule id>` and its message says why.
#[error("conflict")]
ConflictWith { detail: &'static str, errors: Vec<FieldError> },
```
Its problem is `conflict` with the detail and `errors` (like `BadRequest`). Test it next to the existing `invalid_filter_is_400_with_a_position` test.

### `eval/rule_check.rs`

```rust
/// What the rule checks need of an evaluator. Built from the stored row without opening a
/// webhook's sealed values (no check needs the URL, headers or secret).
pub(crate) enum Subject {
    Judge(JudgeConfig),
    Check(Check),
    Webhook,
}

impl Subject {
    /// From a stored row: `kind` through `EvaluatorKind::from_name`; a judge's or check's `config`
    /// through `EvaluatorConfig::from_parts(kind, &document)` as `evaluator::load` does; a webhook
    /// is `Webhook` without reading `secret_enc`.
    ///
    /// # Errors
    ///
    /// [`EvaluatorError::Corrupt`] when a column does not decode.
    pub(crate) fn from_row(row: &EvaluatorRow) -> Result<Self, EvaluatorError>;
    /// From a parsed configuration (the evaluator PATCH's new value).
    pub(crate) fn from_config(config: &EvaluatorConfig) -> Self;
}

/// What a rule asks of its evaluator (§ Rule validation, checks 8–10, 14–15).
pub(crate) struct RuleShape<'a> {
    pub(crate) target: RuleTarget,
    pub(crate) mapping: &'a VariableMapping,
    pub(crate) variables: &'a BTreeSet<String>,   // the mapping's keys
    pub(crate) budgeted: bool,
}

/// Every check of the rule against `subject`, as field errors (§ Rule validation).
/// `adapter` is the judge's connection adapter (`None` for other kinds, or when the connection is gone,
/// which fails check 15). `prices` and `now` price the judge's model.
pub(crate) fn check_rule(shape: &RuleShape<'_>, subject: &Subject, adapter: Option<Adapter>, prices: &PriceTable, now: i64) -> Vec<FieldError>;

/// Check 9 alone: one error per variable of the judge's prompt that `variables` lacks, in the
/// prompt's order. `check_rule` calls it for judges; the manual-run route (I3.9) calls it too.
/// A prompt that no longer compiles gives one error on `variable_mapping` with the `TemplateError`
/// display (a saved evaluator always compiles, but do not unwrap).
pub(crate) fn unmapped_variables(judge: &JudgeConfig, variables: &BTreeSet<String>) -> Vec<FieldError>;
```
Read `crates/eval/src/config.rs` for the exact names of `JudgeConfig` and `Check` and their re-exports. Unit tests: one per check (8, 9, 10 with each check kind, 14, 15), `unmapped_variables` alone (two of three prompt variables mapped → one error on the third), a clean case, and `Subject::from_row` of a webhook row whose `secret_enc` is garbage (it is never opened, so the result is `Webhook`).

### `routes/online_rules.rs`

Routes (all `Access::Project`, `session_only: false`, `documented: true`):

| Method | Path | Perm | Handler |
|---|---|---|---|
| GET | `/api/v1/projects/{project}/online-rules` | `View` | `list_online_rules` |
| POST | `/api/v1/projects/{project}/online-rules` | `ManageEvaluation` | `create_online_rule` |
| GET | `/api/v1/projects/{project}/online-rules/{id}` | `View` | `get_online_rule` |
| PATCH | `/api/v1/projects/{project}/online-rules/{id}` | `ManageEvaluation` | `update_online_rule` |
| DELETE | `/api/v1/projects/{project}/online-rules/{id}` | `ManageEvaluation` | `delete_online_rule` |
| POST | `/api/v1/projects/{project}/online-rules/{id}/resume` | `ManageEvaluation` | `resume_online_rule` |

**`rule_json(state, row, counts, now) -> Result<OnlineRule, ApiError>`**: the row's columns; `variable_mapping` parsed from its JSON (`corrupt` on failure, as `routes/evaluators.rs` does); `spend` when the rule has a budget: `period_start = budget::period_start(period, now)`, `spent_usd = rule_spend::spent_between(reader, id, period_start, day_start(now) + DAY_NS)`, `reserved_usd = state.eval.budgets.view(id, period_start).map_or(0.0, |(_, reserved)| reserved)`; `jobs_last_24h` from `counts` (default zeros); `dropped_last_hour = state.eval.hourly.dropped(id, now)`.

**List:** `list_rules`, then `page_named` over `(name, id, row)` first, so only the page's rows are turned into JSON; then `counts_since(reader, project, now − 24 h)` and `rule_json` for each row of the page (one `spent_between` per budgeted rule of the page); `org_enabled_rules = count_enabled_rules` on a reader connection (`state.meta.reader().acquire().await?`, then `&mut *conn`). Answer `OnlineRulePage`. Read `page_named` in `routes/orgs.rs` and map its page's items after it returns.

**Create:**
1. `principal::session_user` (as the evaluator routes); trim `name`.
2. Load the evaluator row (`get_evaluator`; missing → check 2) and its `rule_check::Subject::from_row`, and for a judge read its connection row for the adapter: `Adapter::from_name(&row.adapter)` returns `Result<Adapter, UnknownName>`; an `Err` means a stored value that does not decode, so map it to `MetaError::Corrupt { table: "llm_connections", column: "adapter" }` (a 500 `corrupt`, as for any undecodable row). A missing connection row gives `adapter = None`.
3. Run § Rule validation checks 1, 2, 4–7, 11–13, 16 here and checks 8–10, 14–15 with `rule_check::check_rule`; collect every error; a non-empty list is `400`.
4. Check 3: when the filter is not blank, `open_query(state, project, None)` then `llmobs_storage::validate_filter(&ctx, view, text)?` (`view` is `ViewName::Traces` or `ViewName::Spans`; the `?` turns `RunError::Invalid` into the `invalid_filter` problem on field `filter`, as K's route does).
5. One `MetaDb::write`, whose closure owns the evaluator row read in step 2: `get_evaluator_tx` again; missing → the outcome `EvaluatorGone` → 400 on `evaluator_id` with check 2's message; its `version` or `updated_at` differs from the row of step 2 → `Conflict("evaluator changed, retry")` (409), because the checks of step 3 ran against the old one; when `enabled`, `rule_limit::check(count_enabled_rules(conn, org)? + 1, limit)`, where `limit = rule_limit::limit(&state, org)` is read before the write (an `Outcome::OverLimit(RuleLimitError)` result, not an `Err`, so nothing is written); then `insert_rule`.
6. Audit `tracing::info!(target: "audit", rule_id, project_id, user_id, "online rule created")`; `201` with `rule_json`.

Store the filter trimmed, `None` when blank; the mapping as `serde_json::to_string(&BTreeMap)`.

**Update:** read the row (404), merge the body over it (`""` filter clears; `clear_budget` clears both budget fields), validate the merged rule as on create (the evaluator is the stored one), then one write: the stored row must equal the one read (else `Conflict("online rule changed, retry")`); the evaluator row is re-read and compared as on create (`evaluator changed, retry`; a missing evaluator cannot happen while the rule exists, because the delete refuses it, so treat it as the same 409); when `enabled` goes from false to true, the limit check as on create; `update_rule`; and when the budget amount or period changed and the rule is paused `'budget'`, clear the pause in the same write (`resume_rule`, D-I-15). After the write: when the budget changed, `state.eval.budgets.forget(id)`. Audit `online rule updated`. `200`.

**Delete:** `delete_rule` (its jobs and spend cascade); then `state.eval.forget_rule(id)`, a new method that calls `budgets.forget`, `breakers.reset` and `hourly.forget`. Audit `online rule deleted`. `204`.

**Resume:** `resume_rule` (404 when missing; a rule that is not paused answers `200` unchanged); `state.eval.breakers.reset(id)`; `state.eval.wake()`; audit `online rule resumed` with `reason = "manual"`. `200`.

### Evaluator routes (`routes/evaluators.rs`)

- **Delete:** inside the write, `rules_of_evaluator(conn, id)`; when not empty, nothing is deleted and the answer is `409 ConflictWith { detail: "online rules use this evaluator", errors: one per rule (field `rules.<rule id>`, message "<rule name>: uses this evaluator") }`. Change the write's result from `bool` to a small enum. Rule names are configuration, not payload, so they may appear in the message.
- **PATCH:** the rule checks run inside `write_update` (the write that already re-reads the evaluator and the judge's connection), so a rule saved between a read and the write cannot slip past. Pass `write_update` the new `Subject::from_config(&prepared.evaluator.config)` and `state.ingest.prices()` (an `Arc`, owned by the closure). After the connection check: `rules_of_evaluator(conn, id)`; for each rule, `check_rule` with its mapping (from its JSON; a stored mapping that no longer parses is a `MetaError::Corrupt`), its target, `budgeted = budget_usd.is_some()`, and the adapter of the connection row read in that transaction (`Adapter::from_name(&row.adapter)`; an `Err` is `MetaError::Corrupt { table: "llm_connections", column: "adapter" }`, as on create). Any error → a new `Outcome::BreaksRules(Vec<FieldError>)`, nothing written → `409 ConflictWith { detail: "the change would break online rules that use this evaluator", errors: one per broken rule (field `rules.<rule id>`), message = "<rule name>: <that rule's first error message>" }`.

### Connection routes (`routes/connections.rs`, D-I-5, D-I-14)

- **Test success:** replace the `set_status(… "ok" …)` call with one write that sets the status `ok` (only when it is not) and calls `resume_connection_pauses(conn, id, now)`. After it: `breakers.reset` for each resumed rule, one audit line each (`online rule resumed`, `reason = "connection test"`), and `state.eval.wake()` when any resumed. Keep the "best effort" handling of a failed write.
- **PATCH with `api_key`:** in the same write as `update_connection`, `set_connection_status(conn, id, "ok", None, now)` and `resume_connection_pauses(conn, id, now)`. After it: reset the breaker of **every** rule whose evaluator uses the connection (new `meta::online_rules::rule_ids_of_connection(conn, connection_id) -> Vec<Uuid>`, read in the same write), audit the resumed ones, wake.

### Spec

- `docs/spec/06-evaluation.md` § Online rules, after the first paragraph: "Rules are managed under `/online-rules` by project members (D-I-24). Saving a rule checks the filter with storage for the rule's view, the mapping against the evaluator (every variable of a judge prompt mapped; a check's `output`, and `expected` when it needs one; no `expected` or `item.*` expressions; no bare `span.*` in a trace rule) and the budget (judges only, with a priced model) (D-I-30). `target` and `evaluator_id` cannot change. Limits: a name of 1–128 characters, a filter of at most 4 096 bytes, 1–32 variables of at most 1 024 bytes each, `budget_usd` at most 1 000 000, and 500 rules per project (D-I-30). The checks against the evaluator run again in the rule's write transaction, and an evaluator PATCH runs them for every rule of the evaluator in its own write (D-I-19). On the Free plan an org has at most 1 active rule, an enabled one, paused or not; creating or enabling one past the limit is `403 plan_limit`, checked in the write transaction; a smaller limit never disables existing rules (D-I-32)."
- `docs/spec/09-api.md` § Evaluation & alerts: replace the `GET/POST /online-rules` row with `GET/POST /online-rules` · `GET/PATCH/DELETE /online-rules/{id}` (the list answers `org_enabled_rules`; a project has at most 500 rules, the 501st is a `409`), add `POST /online-rules/{id}/resume`, and add to the evaluators row "`DELETE` and a `PATCH` that would break a rule are `409` with the rules in `errors` (D-I-19)".

### Tests (`routes/online_rules.rs`, over HTTP)

- `create_get_list_round_trip`: a judge rule; `GET` matches; the list has `org_enabled_rules: 1` and `jobs_last_24h` zeros; `spend` present with `period_start` at today's UTC midnight.
- `create_validates_every_field`: one request per row of § Rule validation (1, 2, 4–16); each is a 400 on its field with its message.
- `a_duplicate_name_is_409` and `the_501st_rule_is_409` (insert 500 rows with `meta::online_rules::insert_rule` in one write, then `POST` the next).
- `budget_and_clear_budget_together_is_400` (check 12).
- `a_deleted_evaluator_at_write_time_is_400`: drive the write helper with an evaluator id that is gone (`EvaluatorGone` → 400 on `evaluator_id`).
- `a_changed_evaluator_at_write_time_is_409`: call the write helper with an evaluator row whose `version` is one less than the stored one → `evaluator changed, retry`.
- `a_webhook_rule_is_saved_without_opening_its_secret`: a webhook evaluator whose `secret_enc` was overwritten with garbage by SQL in the test; creating a rule on it succeeds.
- `invalid_filter_is_400_with_position` and `a_span_filter_is_checked_on_the_spans_view` (`span_kind = 'llm'` is accepted for a span rule and refused for a trace rule).
- `patch_refuses_target_and_evaluator_id`, `patch_merges_and_clears_the_filter`, `clear_budget_removes_the_budget`, `patch_of_a_changed_row_is_409` (call the update's write helper with the rule row as read, after pausing the rule with `pause_rule`: the rows differ → 409 `online rule changed, retry`).
- `the_active_rule_limit_refuses_create_and_enable`: replace the limit for the test — `rule_limit::limit` returns `None`, so test the route's write helper with `Some(1)` directly (extract `fn insert_checked(conn, …, limit: Option<u64>)`), and assert a 403 `plan_limit` problem built from its `RuleLimitError`. A disabled create passes at the limit; enabling it fails.
- `a_paused_rule_counts_as_active` (the count includes it).
- `resume_clears_the_pause_and_resets_the_breaker`.
- `a_budget_change_clears_a_budget_pause`.
- `viewer_reads_member_writes_api_key_cannot_write`.
- `delete_cascades_jobs_and_spend`.

`routes/evaluators.rs`: `delete_with_rules_is_409_listing_them`; `patch_that_drops_a_mapped_prompt_variable_is_409` (a rule maps `input` and `output`; a PATCH adds `{{ context }}` to the prompt → 409 naming the rule, and the evaluator's `version` is unchanged); `patch_that_moves_to_an_unpriced_model_breaks_a_budgeted_rule`; `a_rule_saved_during_the_patch_is_checked` (call `write_update` directly with a rule inserted after the PATCH's reads: it still answers `BreaksRules`).

`routes/connections.rs`: `a_successful_test_resumes_connection_pauses` (two paused rules, one `'connection'`, one `'errors'`: only the first resumes); `a_key_change_resumes_and_sets_ok`.

### Done when

`just check` passes.

---

## I3.9 — Manual runs and the jobs API

**Goal:** `POST …/evaluators/{id}/run` queues a manual run; `GET …/eval-jobs` and `GET …/eval-jobs/{id}` read jobs. An end-to-end test runs a manual job through the real loops.

**Edit only:** `crates/server/src/routes/eval_jobs.rs` (new), `crates/server/src/routes/mod.rs`, `crates/server/src/http/access.rs` (one line), `crates/server/src/meta/{online_rules,eval_jobs,rule_spend}.rs` (the module `dead_code` attributes, and item-level allows for what M4 calls; § I3.2 › Dead code), `crates/server/openapi.json`, `web/src/api/schema.gen.ts`, `docs/spec/06-evaluation.md`, `docs/spec/09-api.md`.

**Commit:** `Add manual evaluator runs and the eval jobs API`

### Routes (`routes/eval_jobs.rs`)

| Method | Path | Perm | Handler |
|---|---|---|---|
| GET | `/api/v1/projects/{project}/eval-jobs` | `View` | `list_eval_jobs` |
| GET | `/api/v1/projects/{project}/eval-jobs/{id}` | `View` | `get_eval_job` |
| POST | `/api/v1/projects/{project}/evaluators/{id}/run` | `ManageEvaluation` | `run_evaluator` |

- `job_json(row: &JobRow) -> Result<EvalJob, ApiError>`: the row's columns; `kind`, `status` and `error_class` through `from_name` (`corrupt` on an unknown name).
- **List:** `EvalJobListParams` → `JobFilter` (`as_str` of the enums); the cursor and `limit + 1` fetch exactly like `routes/alert_rules.rs` `list_events`; `Page<EvalJob>`.
- **Run:**
  1. `session_user`; the shared limiter `state.limits.connection_tests.check(&user.id, now)` → 429 like `test_run.rs`;
  2. the evaluator of the project (404);
  3. § Eval job contract's body rules (collect errors; 400). For a judge, `rule_check::Subject::from_row(&row)` (a `Corrupt` error is a 500) gives the `JudgeConfig`, and `rule_check::unmapped_variables(&judge, &keys)` runs with the body mapping's keys;
  4. `insert_job(conn, NewJob { id: Uuid::now_v7(), kind: KIND_MANUAL, rule_id: None, variable_mapping: Some(json text), trace_start_ns: body.trace_start.ns(), … }, now)` in one write, then `get_job` (it was just inserted; a `None` is `ApiError::Internal`);
  5. `state.eval.wake()`; audit `tracing::info!(target: "audit", job_id, evaluator_id, project_id, user_id, "evaluator run queued")`;
  6. `202` with `job_json`.

### Spec

- `docs/spec/06-evaluation.md` § Online rules › Execution, a new bullet: "**Manual runs:** `POST /evaluators/{id}/run` queues one job at the highest priority, with the caller's mapping, and answers `202` with it; the UI polls `GET /eval-jobs/{id}`. Every variable of a judge's prompt must be mapped (`400` otherwise). Manual runs have no budget, count usage, and share the test runs' limit of 10 a minute per user (D-I-22)."
- `docs/spec/09-api.md` § Evaluation & alerts: replace `GET /online-rules/{id}/jobs` with `GET /eval-jobs?rule_id=&evaluator_id=&status=&kind=` (newest first, kept 7 days after they finish) and `GET /eval-jobs/{id}`, and add `POST /evaluators/{id}/run` `{trace_id, span_id?, trace_start, variable_mapping}` → `202` with the job (D-I-22).

### Tests

- `run_queues_a_manual_job`: `202`, `kind: manual`, `status: queued`, `attempts: 0`; the row's `priority` is 0 and its `dedup_key` `manual:<id>`.
- `run_validates_its_body` (one case per row of the table), including `run_with_an_unmapped_prompt_variable_is_400`: a judge whose prompt uses `{{ input }}` and `{{ output }}`, a body mapping only `output` → 400 on `variable_mapping.input` with `variable input is used by the prompt but not mapped`, and no job row is inserted; `run_of_an_unknown_evaluator_is_404`, `run_is_rate_limited_with_tests` (10 test runs then a run → 429), `viewer_and_api_key_cannot_run`.
- `jobs_list_filters_and_pages`: 3 jobs of two rules and one manual; each filter (`rule_id`, `evaluator_id`, `status`, `kind`); `limit=1` pages through with the cursor, newest first; a malformed cursor is 400; another project's job is not listed and its `GET` is 404.
- `a_manual_run_completes_through_the_loops` (end to end, real time; clock set to real time first): `crate::eval::spawn(&app.state, EvalTiming::TEST)`, `POST …/run` against a `MockLlm` judge, poll `GET /eval-jobs/{id}` every 50 ms for at most 10 s until `done`; then `GET /scores?evaluator_id=…` has one `llm_judge` score without `rule_id`, and `GET /traces/{judge_trace_id}` finds the judge trace (the trace detail takes only `around`, `TraceDetailParams` in `api/traces.rs`; without it storage reads around the current time, which is when the judge trace was written; if the read misses it, pass `?around=<the job's finished_at>`); finally `EvalTasks::shutdown`.

### Done when

`just check` passes. Every documented M3 operation has a route. None of `meta/{online_rules,eval_jobs,rule_spend}.rs` keeps its module-level `dead_code` attribute; any item still unused has an item-level allow naming its M4 caller.

---

## I3.10 — Web: online rules list and editor

**Goal:** the "Online rules" sidebar entry, the list page and the editor (new and edit). The variable-mapping editor of M2's test panel becomes a shared component.

**Edit only:** `web/src/components/shell/nav.ts`, `web/src/components/evaluators/mapping-table.tsx` and `.test.tsx` (new), `web/src/components/evaluators/error-hints.ts` (new), `web/src/components/evaluators/test-panel.tsx` (use the two new modules), `web/src/components/online-rules/rule-form.tsx` (new), `web/src/routes/$org/$project/online-rules/{index,new}.tsx` and `.test.tsx` (new), `web/src/routes/$org/$project/online-rules/$ruleId/edit.tsx` and `.test.tsx` (new), `web/src/lib/roles.ts` (only if a helper is missing; `canManageEvaluation` exists).

**Commit:** `Add the online rules list and editor`

### Shared modules

- `mapping-table.tsx`: move out of `test-panel.tsx` (read it first) the mapping row type, its constructor, the prompt-variable parser, the default expression, the initial mapping and the mapping table's JSX. What changes, exactly:
  - `MappingRow` keeps its fields `{ id: number; variable: string; expression: string }`, so the JSX keeps `key={row.id}`; it is exported.
  - The module-level `rowId` counter moves too, behind `export function nextRowId(): number`. `newMappingRow(variable, expression)` (exported) and `test-panel.tsx`'s own `newVariableRow` both call `nextRowId()`, so the two kinds of rows still never share an id.
  - `variableNames(prompt)` is renamed `promptVariables(prompt: string): string[]` (same body).
  - `expressionFor(variable)` is replaced by `defaultExpression(variable: string, target: "trace" | "span"): string`: `input` → `trace.input` / `span.input`, `output` → `trace.output` / `span.output`, others `""`. With `"trace"` it returns what `expressionFor` returned.
  - `initialMapping(defaults: Schema<"TemplateMapping">[], prompt: string, target: "trace" | "span" = "trace"): MappingRow[]`: the same body, with `defaultExpression(variable, target)`.
  - New: `mappingObject(rows: MappingRow[]): Record<string, string>` (rows with an empty variable are left out).
  - New: `MappingTable({ rows, onChange, disabled, errorFor }: { rows: MappingRow[]; onChange: (rows: MappingRow[]) => void; disabled?: boolean; errorFor?: (variable: string) => string | undefined })`, the test panel's mapping JSX with its "Add variable" and remove buttons; the test panel passes its `editMapping` as `onChange`.
- `error-hints.ts`: move `HINTS` from `test-panel.tsx` as `export const ERROR_HINTS: Record<Schema<"TestErrorClass">, string>`.
- `test-panel.tsx` imports both. `test-panel.test.tsx` passes unchanged: that is the proof.
- `mapping-table.test.tsx`: `defaultExpression` for both targets; `mappingObject` drops empty rows; adding and removing rows calls `onChange`.

### Nav

`nav.ts`: add `{ to: "/$org/$project/online-rules", label: "Online rules", icon: ListChecks }` after Evaluators (`ListChecks` from `lucide-react`).

### List page (`online-rules/index.tsx`)

Like `evaluators/index.tsx`: infinite query with `onlineRulesInit`; header "Online rules" with "New rule" (members; `DisabledReason` "Only members can create online rules"); the line "{n} active rules in this organization" from the first page's `org_enabled_rules` ("1 active rule" singular). Table columns:
- **Name**: a link to `/$org/$project/online-rules/$ruleId/edit` (I3.11 points it at the rule page);
- **Evaluator**: its name from the evaluators list (`evaluatorsInit`), else the id with `IdText`;
- **Target**: "Traces" / "Spans";
- **Status**: a badge with `ruleStatusLabel(rule)`, exported from `rule-form.tsx`: "Disabled" (not enabled), "Paused: budget", "Paused: errors", "Paused: connection", else "Active";
- **Sample**: the rate as a percent (`25 %`);
- **Spend**: "$0.42 of $5.00 today" (`day`) or "… this month", "—" without a budget (use `formatCost` from `lib/format.ts` when its output fits; check it);
- **Last 24 h**: "40 done · 3 failed" (omit zero parts; "—" when all zero).

Empty state: "No online rules" and "A rule runs an evaluator on the traces or spans that match its filter."

### Editor (`components/online-rules/rule-form.tsx`, `new.tsx`, `$ruleId/edit.tsx`)

`RuleForm({ org, project, rule }: { org; project; rule?: Schema<"OnlineRule"> })`, with `react-hook-form` if `evaluator-form.tsx` uses it (read it and use the same tools). Fields:
- **Name** (1–128 characters).
- **Evaluator**: a select of the project's evaluators (name and kind); disabled on edit, with help "The evaluator cannot change after the rule is created."
- **Target**: "Traces" / "Spans" radio; disabled on edit.
- **Filter**: monospace textarea; help "Filter DSL over the traces view (spans view for span rules), for example `status = 'error'`. Empty matches everything."
- **Variables**: `MappingTable`. On create, picking an evaluator (or changing the target) refills it with `defaultMappingFor(evaluator: Schema<"Evaluator">, target): MappingRow[]`, exported from `mapping-table.tsx` (I3.12 reuses it): for a judge, `promptVariables(config.prompt)` with `defaultExpression`; for a check, `output`, plus `expected` for `exact_match`, `levenshtein` and `contains` without values; for a webhook, `input` and `output`. On edit it starts from the rule's mapping.
- **Sample rate**: a percent input (0 < value ≤ 100, step 0.1); sent as `value / 100`.
- **Budget** (judges only): a "Limit spend" checkbox, then amount (USD, > 0) and period ("Per day" / "Per month"). For other kinds: the text "Budgets apply to LLM judge evaluators only." and no inputs.
- **Enabled**: a switch, default on.

Save:
- create → `POST`; `201` → navigate to the list (I3.11 changes it to the rule page); invalidate `onlineRulesKey`.
- edit → `PATCH` with only the changed fields; unchecking "Limit spend" on a rule with a budget sends `clear_budget: true`; `200` → navigate to the list; invalidate the list and `onlineRuleKey`.
- `400` with `errors`: each error under its field (`name`, `evaluator_id`, `filter`, `variable_mapping` and `variable_mapping.<name>` (under that row), `sample_rate`, `budget_usd`), others in a form-level alert.
- `400 invalid_filter`: the message under the filter, with "line L, column C" from `position`.
- `403 plan_limit`: an alert with the problem's detail and "A paused rule still counts as active: disable or delete a rule to free the slot."
- `409`: an alert with the detail.

Edit page: loads the rule (`onlineRuleQuery`; 404 → not found), renders `RuleForm`, and has "Delete" (members) with a confirm dialog → `DELETE` → list. Viewers see a read-only form and "Only members can edit online rules".

### Tests (`renderApp`, MSW fixtures; override endpoints with `server.use`)

`online-rules/index.test.tsx`: lists both fixture rules with their status ("Active", "Paused: errors"), spend ("$0.42 of $5.00 today"), counts ("40 done · 3 failed · 2 queued · 1 running" — use the order done, failed, queued, running, skipped), the org line "2 active rules in this organization"; "New rule" is disabled for a viewer; the empty state.

`online-rules/new.test.tsx`:
- `creates a judge rule`: pick "Relevance", the mapping fills `input → trace.input`, `output → trace.output`; set the sample to 25 and a 5 USD daily budget; the `POST` body is exactly the fixture's create body.
- `span target refills span expressions`.
- `a check evaluator hides the budget` (override the evaluators list with a check evaluator).
- `shows field errors`, `shows an invalid filter with its position`, `shows the plan limit message`.

`$ruleId/edit.test.tsx`: `sends only changed fields`; `unchecking the budget sends clear_budget`; `evaluator and target are disabled`; `deletes after confirming`; `a viewer sees a read-only form`.

### Done when

`just check-web` passes; then `just check`.

---

## I3.11 — Web: the rule page

**Goal:** `/online-rules/$ruleId` shows the rule, its pause banner and resume, its spend, its jobs and its score distribution. List rows and the editor lead to it.

**Edit only:** `web/src/routes/$org/$project/online-rules/$ruleId/index.tsx` and `.test.tsx` (new), `web/src/components/online-rules/{rule-banner,jobs-table,score-distribution,rule-conflicts}.tsx` (new, with `score-distribution.test.ts` for its pure function), `web/src/routes/$org/$project/online-rules/index.tsx` (the name link), `web/src/components/online-rules/rule-form.tsx` (navigation after save), `web/src/api/queries.ts` (`scoresQuery`'s parameter type only), `web/src/routes/$org/$project/evaluators/$evaluatorId.tsx` and `.test.tsx` and `web/src/components/evaluators/evaluator-form.tsx` (the 409 conflict list only), and their existing tests where the link or navigation is asserted.

**Commit:** `Add the online rule page`

### Page

Loader: `ensureQueryData(onlineRuleQuery(…))`, a 404 → `notFound()` (as the trace page does). Layout, top to bottom:
- **Header:** name, the status badge (`ruleStatusLabel`), "Edit" (members) → `$ruleId/edit`, and "Resume" (members, only when paused) → `POST …/resume`, then invalidate the rule and the list. Viewers see neither button.
- **Banner** (`rule-banner.tsx`, `role="alert"`) when paused:
  - `budget`: "Paused: the daily budget of $5.00 is spent. The rule resumes when the day turns (UTC), or when you raise the budget." ("monthly" / "month" for `month`).
  - `errors`: "Paused: too many jobs failed ({paused_detail}). Fix the cause, then resume the rule." (no parentheses without a detail).
  - `connection`: "Paused: the LLM connection {name} is failing ({paused_detail}). Rules in other projects that use it are paused too. Fix it in Settings › LLM connections; a successful connection test resumes them." `{name}` comes from the evaluator's `config.connection_id` looked up in the org's connections list (M1's query; find it with `rg -n "llm-connections" web/src/api`); "the LLM connection" when it cannot be found.
- **Details:** evaluator (a link to `/evaluators/$evaluatorId`), target, filter (monospace, "—" when none), sample rate, the mapping (read-only rows), enabled.
- **Spend** (when `spend`): "$0.42 spent of $5.00 today" (or "this month"), "+$0.01 reserved" when `reserved_usd > 0`, and a bar of width `min(100, spent / budget · 100) %` with `aria-label="Spend"` and `aria-valuenow`.
- **Last 24 hours:** the five counts, and "{n} matches dropped this hour by the 10 000 per hour cap" when `dropped_last_hour > 0`.
- **Score distribution** (`score-distribution.tsx`): `scoresQuery(org, project, { rule_id: ruleId, limit: 1000 })`. `scoresQuery` in `queries.ts` types its query as `{ trace_id: string; span_id?; target?; limit? }`; change it to `{ trace_id?: string; span_id?: string; rule_id?: string; target?: Schema<"ScoreTargetKind">; limit?: number }` (the generated params have `rule_id` after I3.1). Existing callers pass `trace_id` and still type-check. The pure function `distribution(scores: Schema<"Score">[]): { kind: "numeric" | "boolean" | "categorical" | "text" | "empty"; bins: { label: string; count: number }[] }`:
  - numeric: 10 equal bins from the minimum to the maximum value, labels `"0.00–0.10"` (two decimals); all values equal → one bin labelled with the value;
  - boolean: `true` and `false`;
  - categorical: one bin per label, most frequent first, then by label;
  - text: no bins (`kind: "text"`, shown as "Text scores have no distribution.");
  - no scores: `empty` ("No scores yet.").
  Deleted scores are left out. The title is "Score distribution (latest {n} scores)". Each bar is a `div` with `aria-label="{label}: {count}"` and a width proportional to the largest bin.
- **Jobs** (`jobs-table.tsx`): `$api.useInfiniteQuery` on `GET …/eval-jobs` with `evalJobsInit(org, project, { rule_id })`, "Load more". Columns: Status (badge), Kind, Target (a link to `/$org/$project/traces/$traceId`, with `search={{ span: span_id }}` when there is one), Attempts, Error (`error_class`: message, truncated with a tooltip), Cost, Latency, Created, and "View judge trace" (a link to the judge trace) when `judge_trace_id` is set. `JobsTable({ org, project, filters })` is reusable.

**Evaluator conflicts** (`rule-conflicts.tsx`, D-I-19): `RuleConflicts({ org, project, problem })` renders nothing unless the problem is a `409` whose `errors` have fields starting with `rules.`; then a list with one item per error: a link to `/$org/$project/online-rules/$ruleId` (the id after `rules.`) whose text is the error's message (it starts with the rule's name). The evaluator page's delete error and the evaluator form's save error render it under their existing message (read how each keeps the caught error; keep the problem object, not only its message).

Then change the list's name link to the rule page, and make `RuleForm` navigate to the rule page after a create (`201`'s id) and after an edit; the edit page's "Cancel" goes to the rule page too. Update the I3.10 tests that assert those targets.

### Tests

`score-distribution.test.ts`: each kind, the equal-values bin, deleted scores ignored.

`$ruleId/index.test.tsx` (fixture rule `…0700`; override with `server.use` for the others):
- `shows the rule, spend and counts`: "$0.42 spent of $5.00 today", "+$0.01 reserved", the five counts.
- `shows the errors banner and resumes`: override `GET` with rule `…0701`; the banner text; "Resume" sends `POST …/0701/resume`, and the banner disappears after the refetch (override the `GET` again with the resumed rule).
- `the connection banner names the connection`.
- `the budget banner says when it resumes`.
- `lists jobs with judge trace links`: the two fixture jobs; the done one links "View judge trace" to `/acme/support-bot/traces/e1000000000000000000000000000002`; the failed one shows `provider status 503`.
- `shows the score distribution`: override `GET /scores` with three numeric scores 0.1, 0.5, 0.9 → bins with counts 1.
- `a viewer has no edit or resume`.
- `unknown rule shows not found`.

`evaluators/$evaluatorId.test.tsx`: `a delete refused by rules lists them` (override `DELETE` with a 409 `ConflictWith` naming rule `…0700`: the message and a link to `/acme/support-bot/online-rules/01900000-0000-7000-8000-000000000700`); `a save refused by rules lists them` (the same on `PATCH`).

### Done when

`just check-web` passes; then `just check`.

---

## I3.12 — Web: run evaluator and judge-trace links

**Goal:** a member runs an evaluator on the open trace or span from the trace view and sees the outcome; every evaluator score links to its judge trace. Spec 11 and the roadmap describe M3.

**Edit only:** `web/src/components/trace/run-evaluator-dialog.tsx` and `.test.tsx` (new), `web/src/components/trace/trace-view.tsx` and `trace-header.tsx` (place the button), `web/src/components/trace/scores-tab.tsx` and `.test.tsx`, `docs/spec/11-ui.md`, `docs/roadmap.md` (the I row).

**Commit:** `Run evaluators from the trace view and link judge traces`

### The dialog (`run-evaluator-dialog.tsx`)

`RunEvaluatorButton({ trace, spanId, selectedIsRoot }: { trace: Schema<"TraceSummary">; spanId?: string; selectedIsRoot: boolean })` renders "Run evaluator" (a small button in the trace header's action group: add an optional `actions?: ReactNode` prop to `TraceHeader`, rendered before the copy button, and pass the button from `TraceView`). For viewers it is disabled with the tooltip "Only members can run evaluators" (`DisabledReason`). It opens a dialog:
- **Evaluator**: a select of the project's evaluators.
- **Target**: "Whole trace" or "Selected span" (only when `spanId` is set and is not a root span); default "Whole trace". The button gets `selectedIsRoot: boolean` from `TraceView`, which has the trace's spans: true when the selected span has no `parent_span_id` (read how `trace-view.tsx` finds the selected span and reuse that).
- **Variables**: `MappingTable`, refilled with `defaultMappingFor(evaluator, target)` (I3.10) when the evaluator or the target changes.
- **Run** → `POST …/evaluators/{id}/run` with `{trace_id, span_id?, trace_start: trace.start_time, variable_mapping}`.

After a `202`, the dialog shows the job: "Queued", "Running" (spinner), then the result, polling `evalJobQuery` with `refetchInterval: 1000` while the status is `queued` or `running`:
- `done`: "Score written" and "View judge trace" (a link to `/$org/$project/traces/$traceId` with `judge_trace_id`); invalidate the score queries of the trace so the Scores tab refreshes.
- `failed`: "Run failed", the error and `ERROR_HINTS[error_class]` (I3.10).
- `skipped`: "Skipped: {error}".
- `429`: "Too many runs. Wait a minute."; `400` with errors: each under its field; `404`: "Evaluator not found."

### Scores tab

In `scores-tab.tsx`, for a score whose `source` is `llm_judge`, `check` or `webhook` and whose `metadata["judge.trace_id"]` is set, the Source cell adds a link "View judge trace" to `/$org/$project/traces/$traceId` with that id.

### Spec and roadmap

- `docs/spec/11-ui.md` § Evaluators & online rules:
  - first bullet (evaluators), append: "Viewers see the editor read-only and the test panel replaced by 'Only members can run tests.' A saved webhook's URL, headers and signing secret are write-only: their fields show only when 'Replace webhook settings' is checked."
  - replace the **Online rules** bullet with: "Online rules (track I, D-I-27, D-I-30): list with status (active, disabled, paused: budget / errors / connection), sample rate, spend against the budget, the last 24 h of jobs, and the org's active-rule count; editor (evaluator and target fixed after create, filter DSL, variable mapping prefilled from the evaluator, sampling as a percent, budget for judges only, enabled); rule page with the pause banner and Resume, spend, recent jobs with their judge traces, and the distribution of the rule's latest 1 000 scores. The filter's live match counter (M4) and the backfill dialog (M6) come later."
  - add: "Trace view: **Run evaluator** queues a manual run on the trace or the selected span and shows its outcome (D-I-22). Every evaluator score links to its judge trace ('View judge trace')."
- `docs/roadmap.md`, the I row's status: "**In progress.** M1 merged (#63). M2 (evaluators, test runs, score definitions) <its state>. M3 (queue, online rules, manual runs) implemented on branch `<your branch>`. M4–M6 to follow." Read the row first and keep what is still true.

### Tests

`run-evaluator-dialog.test.tsx` (render the trace page through `renderApp`):
- `runs on the whole trace and shows the result`: pick "Relevance"; the mapping is `input → trace.input`, `output → trace.output`; Run → the body is the fixture `requests/evaluator-run.json`; the `GET /eval-jobs/…0802` override answers `queued` once, then the done job → "Score written" and the judge-trace link.
- `runs on the selected span with span expressions`.
- `the root span offers only the whole trace`.
- `shows a failed run with its hint`.
- `429 shows the wait message`.
- `a viewer cannot run`.

`scores-tab.test.tsx`: `an evaluator score links its judge trace`; a human score has no link.

### Done when

`just check` passes. Optional: `just web-dev` shows the pages on the mock data; say in the report whether you looked.

---

## After M3

Report: the twelve commits, the migration number you used, deviations, and anything unverified. Every M3 operation in `openapi.json` has a route, `just check` is green on the merged branch, and the I row of the roadmap says M3 is implemented.
