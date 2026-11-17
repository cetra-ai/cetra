# Users list: richer rows, summary strip, user drawer

Plan for Haiku implementers, one phase at a time. Read the whole of your phase before editing.

## Conventions (every phase)

- **Never guess.** If something in your phase is unclear, contradicts the code, or a step fails in a way the phase does not explain, STOP. Make no further edits. Your final message must start with `QUESTION:` followed by the question and what you found. You will be resumed with an answer.
- Work directly in `/home/sebastien/travaux/llm-observer` on `main`. Do not create branches or worktrees. Do not commit, stash, reset or switch branches.
- Edit only the files your phase lists. Another phase may run at the same time on other files.
- Before any `pnpm`, `npx` or `just` command: `export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH`.
- Web checks, run from `web/`: `npx tsc --noEmit`, `npx biome check <paths>`, `npx vitest run <paths>`. `rtk` wraps vitest and prints little; read the result with
  `python3 -c "import json;d=json.load(open('.vitest/json/output.json'));print(d['numPassedTests'],d['numFailedTests'])"`, and for failures
  `python3 -c "import json;d=json.load(open('.vitest/json/output.json'));[print(t['fullName'],t['failureMessages'][:1]) for r in d['testResults'] for t in r['assertionResults'] if t['status']=='failed']"`.
- Rust checks: `cargo test -p <crate> --all-features <filter>`, `cargo clippy -p <crate> --all-targets --all-features -- -D warnings`, `cargo fmt --all`.
- AGENTS.md rules apply: no `unwrap`/`expect` outside tests, no `unsafe`, no placeholders, no compatibility shims, errors through the crate's error enums, no payload content in logs or error messages.
- Final report: files changed, tests added, check results, anything not done.

## Decisions (already made, do not revisit)

| # | Decision |
|---|---|
| U-1 | `EndUserSummary` gains four fields: `error_trace_count: i64`, `activity: Vec<i64>`, `scores: Vec<UserScore>`, `metadata: BTreeMap<String, String>`. They are filled for the list and for `GET /users/{id}` (its `user`). |
| U-2 | `error_trace_count` = number of the user's traces with `status = 'error'`. Error rate in the UI = `error_trace_count / trace_count`. |
| U-3 | `activity` = exactly 24 trace counts (`ACTIVITY_BUCKETS`). The window is the request's `[from, to)` when both are given. Without a window (and for `GET /users/{id}`) it is `[min first_seen, max last_seen + 1)` over the rows being returned. Bucket width = `ceil((to - from) / 24)` ns; bucket = `(start_time - from) / width`, clamped to `0..=23`. |
| U-4 | `scores` = for each score name, the average `value_num` and the count of the live scores whose `trace_id` belongs to one of the user's traces in the list's scope (window, exclusions, filter). Only `data_type` `numeric` and `boolean` (boolean is 0/1, so the average is the true rate). Session-target scores (no `trace_id`) are not counted. One entry per `(name, data_type)`, sorted by name then data_type. |
| U-5 | `metadata` = the `metadata` map of the user's latest trace in scope (greatest `start_time`, ties by greatest `trace_id`), keeping only keys that start with `user.`, with the `user.` prefix removed, at most 20 keys (first 20 in key order). Omitted from JSON when empty. Values are kept as stored (JSON-encoded scalars or strings). |
| U-6 | Rows open in the list layouts of the Traces page (Drawer / Bottom / Page, Maximize, filmstrip), with `?user=<id>` in the URL. Esc closes. |
| U-7 | Summary strip above the table: one line from `GET /metrics` KPIs over the same range, filter and eval/exp flags, plus the loaded user count. |
| U-8 | Inline bars behind Traces, Tokens and Cost: width = value / max over the loaded rows. |
| U-9 | New list columns: `error_rate` ("Error rate"), `traces_per_session` ("Traces / session"), `activity` ("Activity", sparkline), one `score:<name>` column per score name present in the loaded rows, and `metadata` ("Metadata", hidden by default). None of them is sortable. |
| U-10 | UI docs (spec 11) are not updated in this plan. Spec 09 is updated in U2 because the API changes. |

## Phases and waves

| Wave | Phases (parallel inside a wave) |
|---|---|
| A | U1 storage · U3 UI building blocks + summary strip |
| B | U2 server (after U1) |
| C | U4 mock dataset · U5 list columns (both after U2 and U3) |
| D | U6 user drawer (after U4 and U5) |

---

## U1 — Storage: error_trace_count and user extras

**Files:** `crates/storage/src/reads/users.rs`, `crates/storage/src/reads/mod.rs` (re-exports only), `crates/storage/src/lib.rs` (re-exports only).

### Steps

1. In `reads/users.rs`, add `pub const ERROR_TRACE_COUNT: &str = "error_trace_count";` (the server reads it) and insert it into `USER_COLUMNS` right after `traces::ERROR_COUNT`.
2. In `aggregate_users`, add the aggregate
   `sum(when(col(traces::STATUS).eq(lit("error")), lit(1_i64)).otherwise(lit(0_i64))?)` aliased `ERROR_TRACE_COUNT`.
   `when` is `datafusion::logical_expr::when`; `.otherwise` returns a `Result`, map its error with `classify` (`.map_err(classify)?`). In the `select`, add `coalesce(vec![col(ERROR_TRACE_COUNT), lit(0_i64)]).alias(ERROR_TRACE_COUNT)` right after the `ERROR_COUNT` line.
3. Add the public API:

```rust
/// Number of activity buckets of a user row (spec 09 `EndUserSummary.activity`).
pub const ACTIVITY_BUCKETS: usize = 24;

/// Most `user.` metadata keys returned per user.
pub const MAX_USER_METADATA_KEYS: usize = 20;

/// Per-user extras for a page of users.
#[derive(Debug, Clone)]
pub struct UserExtrasRequest<'a> {
    /// Same filter as the list request.
    pub filter: Option<&'a str>,
    /// Same scope as the list request (for `user_detail`: no window).
    pub scope: ListScope,
    /// Users to compute, at most [`super::MAX_PAGE`]. Empty runs no query.
    pub user_ids: &'a [&'a str],
    /// Window the activity buckets split.
    pub activity: TimeWindow,
    /// Time allowed for all three queries together.
    pub timeout: Duration,
}

/// Results of [`user_extras`].
#[derive(Debug, Default)]
pub struct UserExtras {
    /// `user_id` Utf8, `bucket` Int64 (0..ACTIVITY_BUCKETS), `trace_count` Int64. Buckets with no traces are absent.
    pub activity: Vec<RecordBatch>,
    /// `user_id` Utf8, `name` Utf8, `data_type` Utf8, `avg` Float64, `count` Int64,
    /// sorted by `user_id`, `name`, `data_type`.
    pub scores: Vec<RecordBatch>,
    /// `user_id` Utf8 and `metadata` (the traces' Map column): one row per user, from their latest trace.
    pub metadata: Vec<RecordBatch>,
}
```

   `TimeWindow` is `crate::views::TimeWindow` (it has `from()` and `to()`). Check how `ListScope` imports it in `reads/mod.rs` and import it the same way.

4. Add `pub async fn user_extras(ctx: &QueryContext, request: UserExtrasRequest<'_>) -> Result<UserExtras, RunError>`:
   - `user_ids.len() > super::MAX_PAGE` → `RunError::InvalidRequest("too many users".to_owned())`.
   - Empty `user_ids` → `Ok(UserExtras::default())` without a query.
   - Wrap the work in `tokio::time::timeout(request.timeout, …)` exactly like `user_list` (`Err(_) => Err(RunError::Timeout)`).
   - Doc comment with `# Errors` like the other public functions.
5. The base frame (private `async fn scoped_user_traces(ctx, request) -> Result<DataFrame, RunError>`): copy the first half of `user_frame` (parse filter, `scoped_traces`, apply filter), then filter `col(traces::USER_ID).in_list(ids, false)` where `ids` are `lit(id)` of each user id. Do not filter `is_not_null` separately (the `in_list` already drops nulls).
6. **Activity:** from the base frame,
   - `width = (to - from + ACTIVITY_BUCKETS as i64 - 1) / ACTIVITY_BUCKETS as i64` computed in Rust with `i64` (`to > from` is guaranteed by `TimeWindow`). Use `i64::try_from(ACTIVITY_BUCKETS)` only if clippy complains about the cast; a `const BUCKETS_I64: i64 = 24;` next to `ACTIVITY_BUCKETS` is fine.
   - `bucket = (cast(col(traces::START_TIME), DataType::Int64) - lit(from)) / lit(width)` aliased `bucket` (`datafusion::logical_expr::cast`, `arrow::datatypes::DataType`).
   - `select([col(USER_ID), bucket])`, then `aggregate([col(USER_ID), col("bucket")], [count(lit(1_i64)).alias(TRACE_COUNT)])`, collect.
   - Do the clamping to `0..=23` in the server (U2), not here.
7. **Scores:** 
   - `traces_side = base.select([col(traces::TRACE_ID).alias("t_trace_id"), col(traces::USER_ID)])`.
   - `scores_side = ctx.session().table("scores")` filtered by `col(scores::DATA_TYPE).in_list([lit("numeric"), lit("boolean")], false).and(col(scores::VALUE_NUM).is_not_null()).and(col(scores::TRACE_ID).is_not_null())`, selecting `TRACE_ID`, `NAME`, `DATA_TYPE`, `VALUE_NUM` (`cetra_core::schema::scores`).
   - `scores_side.join(traces_side, JoinType::Inner, &[scores::TRACE_ID], &["t_trace_id"], None)` (`datafusion::common::JoinType` — check where other reads import `JoinType` from: `grep -rn JoinType crates/storage/src`).
   - `aggregate([USER_ID, NAME, DATA_TYPE], [avg(col(VALUE_NUM)).alias("avg"), count(lit(1_i64)).alias("count")])` (`functions_aggregate::expr_fn::avg`), sort by `user_id, name, data_type` ascending, collect.
   - The `scores` table holds live rows only (see `views/mod.rs` doc of the stored scores schema), so no LWW ranking is needed.
8. **Metadata:** from the base frame,
   - window `row_number().partition_by([col(USER_ID)]).order_by([col(START_TIME).sort(false, true), col(TRACE_ID).sort(false, true)]).build()` aliased `rank` — copy the shape of `rank_latest` in `reads/scores.rs`.
   - filter `rank = 1` (`lit(ScalarValue::UInt64(Some(1)))` as in `scores.rs`), select `USER_ID`, `traces::METADATA`, collect.
   - Do not filter keys here; the server does (U-5).
9. Collect each with `.collect().await.map_err(|error| ctx.fail(error))?` and drop empty batches (`filter(|batch| batch.num_rows() > 0)`), like `load` does.
10. Re-export `user_extras`, `UserExtrasRequest`, `UserExtras`, `ACTIVITY_BUCKETS`, `MAX_USER_METADATA_KEYS`, `ERROR_TRACE_COUNT` next to the existing `user_list` exports in `reads/mod.rs` and `lib.rs`.

### Tests (in the existing `#[cfg(test)] mod tests` of `reads/users.rs`)

Use `context_of(&spans)` with spans built by the existing helpers (`span_row` etc.; read how the current tests build spans before writing yours).

1. `error_trace_count_counts_error_traces`: two traces of one user, one with an error span; the row has `error_trace_count = 1`.
2. `activity_buckets_traces_by_start_time`: three traces of one user at `from`, `from + width`, `from + 2*width + 1`; a window `TimeWindow::new(from, from + 24*width)`; the activity batch has buckets 0, 1, 2 with count 1 each.
3. `scores_average_numeric_and_boolean_per_user`: needs scores in the context. Check `crate::test_support` for a helper that builds a scores table (`live_scores_table`); if none fits building two numeric scores 0.2 and 0.6 and one boolean `true` on the user's trace, STOP and ask. Expect `avg ≈ 0.4, count 2` for the numeric name and `avg 1.0, count 1` for the boolean one. A categorical score on the same trace is absent.
4. `metadata_comes_from_the_latest_trace`: two traces of one user with different `metadata` maps; the batch row holds the later trace's map. If the span helpers cannot set `metadata`, STOP and ask.
5. `empty_user_ids_run_no_query`: returns empty `UserExtras`.
6. The existing fixture comparison test (`users.json`) will now fail on the missing `error_trace_count` until U2 regenerates the fixture. Check how `values_match` compares rows: if extra actual columns are ignored, nothing to do; if not, STOP and ask.

### Done when

`cargo test -p cetra-storage --all-features reads::users` passes (except the fixture case named in test 6 if it was expected to fail, which you report), clippy clean, `cargo fmt --all` applied.

---

## U2 — Server: API fields, attach extras, fixtures, OpenAPI

**Depends on U1.** **Files:** `crates/server/src/api/users.rs`, `crates/server/src/api/convert.rs`, `crates/server/src/routes/users.rs`, `crates/server/src/api/openapi.rs` (only to register the new schema, if schemas are listed there), `fixtures/api/dataset/spans.json`, regenerated files (`crates/server/openapi.json`, `web/src/api/schema.gen.ts`, `fixtures/api/responses/**`, `fixtures/api/manifest.json`), `docs/spec/09-api.md`.

### Steps

1. `api/users.rs`:
   - New type:
     ```rust
     /// Average of one score name over a user's traces (spec 09 § Observability data).
     #[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
     pub struct UserScore {
         /// Score name.
         pub name: String,
         /// `numeric` or `boolean`.
         pub data_type: ScoreDataType,
         /// Mean `value_num`; for booleans the share of `true`.
         pub avg: f64,
         /// Number of scores averaged.
         pub count: i64,
     }
     ```
     Find the existing score data-type enum in `cetra_core::enums` (`grep -n "enum ScoreDataType\|Numeric" crates/core/src/enums.rs`). If none exists, STOP and ask.
   - `EndUserSummary` gains, after `error_count`:
     ```rust
     /// Traces whose status is `error`.
     pub error_trace_count: i64,
     ```
     and at the end:
     ```rust
     /// Trace counts in 24 equal buckets: the request's `[from, to)`, or without one
     /// `[first_seen, last_seen]` of the returned rows.
     pub activity: Vec<i64>,
     /// Average of each numeric and boolean score name over the user's traces.
     pub scores: Vec<UserScore>,
     /// `user.*` keys of the latest trace's metadata, without the prefix. Omitted when empty.
     #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
     pub metadata: BTreeMap<String, String>,
     ```
   - Update the snapshot tests in this file (they construct `EndUserSummary`): add the fields with realistic values.
2. `api/convert.rs`, `end_user_batch`: read `ERROR_TRACE_COUNT` (from `cetra_storage`) with `ints` + `req_i64`; set `activity: Vec::new(), scores: Vec::new(), metadata: BTreeMap::new()` (filled later).
3. `api/convert.rs`, add `pub(crate) fn user_extras(extras: &cetra_storage::UserExtras, window: (i64, i64)) -> Result<HashMap<String, UserExtra>, ConvertError>` with `pub(crate) struct UserExtra { activity: Vec<i64>, scores: Vec<UserScore>, metadata: BTreeMap<String, String> }`:
   - activity: for each row, `bucket.clamp(0, 23)` as usize, add `trace_count` into a `vec![0; ACTIVITY_BUCKETS]` per user. (The window argument is not needed for the math if storage already bucketed; drop the parameter if unused.)
   - scores: one `UserScore` per row, `data_type` parsed with the same helper used for score rows elsewhere in `convert.rs` (`grep -n "data_type" crates/server/src/api/convert.rs`).
   - metadata: `maps(batch, traces::METADATA)` + `map_strings`, keep keys starting with `user.`, strip the prefix, skip a key that becomes empty, take the first `MAX_USER_METADATA_KEYS` in key order.
4. `routes/users.rs`:
   - Add `async fn attach_user_extras(ctx: &QueryContext, users: &mut [EndUserSummary], filter: Option<&str>, scope: ListScope, deadline: Deadline) -> Result<(), ApiError>`:
     - return early on empty `users`;
     - activity window: `scope.window` if `Some`, else `TimeWindow::new(min first_seen, max last_seen + 1)` (check `TimeWindow::new`'s signature and error type; map its error the way `list_scope` does);
     - call `storage::user_extras` with the ids, `deadline.remaining()?`;
     - convert and assign each user's `activity` (default `vec![0; 24]` for a user without rows), `scores`, `metadata`.
   - `list_users`: create a `Deadline::start()` at the top (like `get_user`), keep using `READ_TIMEOUT` for the list call or switch it to `deadline.remaining()?` — use `deadline.remaining()?` for both. Call `attach_user_extras` with the same `filter` and `scope` (copy `scope`; derive `Clone, Copy` is already on `ListScope` — check).
   - `get_user`: call `attach_user_extras` with `filter: None` and `ListScope { window: None, include_eval, include_experiments }`.
5. Register `UserScore` in the OpenAPI schema list if `openapi.rs` lists components explicitly (look how `ScoreBrief` is registered).
6. `fixtures/api/dataset/spans.json`: on the root span of trace `a1000000000000000000000000000001` (user `user-42`; check), add or extend `"metadata"` with `"user.name": "\"Ada Example\""` and `"user.plan": "\"pro\""` (values are JSON-encoded strings, so the stored string includes the quotes). Read how other maps are written in this file first; if `metadata` never appears, look at `fixtures/otlp/*/expected.json` for the field's shape. If unsure, STOP and ask.
7. Regenerate: from the repo root, `just openapi` then `just api-examples`. Then `git diff --stat fixtures/api crates/server/openapi.json web/src/api/schema.gen.ts` and check that the users responses gained the fields and nothing unrelated changed except traces/spans carrying the new metadata.
8. `docs/spec/09-api.md`: in the `GET /users` row (§ Observability data) append: "Each user row also has `error_trace_count`, `activity` (24 trace counts over `[from, to)`, or over the rows' `[first_seen, last_seen]` without a window), `scores` (mean of each numeric or boolean score name over the user's traces in scope, with its count) and `metadata` (the `user.*` keys of the user's latest trace in scope, prefix removed, at most 20)."

### Tests

- `routes/users.rs` tests: add `list_users_returns_activity_scores_and_metadata`: from the seeded app, `user-42` has `activity.len() == 24`, `activity.iter().sum::<i64>() == trace_count`, and `metadata["name"] == "\"Ada Example\""`. If the seeded app has scores on its traces (`dataset/scores.json`), assert one `scores` entry by name; read `dataset/scores.json` to pick the expected average.
- `crates/server/tests/api_examples.rs` and `crates/storage` fixture tests must pass after regeneration.

### Done when

`cargo test -p cetra-server --all-features users`, `cargo test -p cetra-server --test api_examples`, `cargo test -p cetra-server --test openapi`, `cargo test -p cetra-storage --all-features reads::users` pass; clippy clean for both crates; `cargo fmt --all`.

---

## U3 — UI building blocks and the summary strip

**Runs in wave A, in parallel with U1.** Does not use the new API fields. **Files:** new `web/src/components/user/sparkline.tsx` (+ `.test.tsx`), new `web/src/components/user/bar-cell.tsx` (+ `.test.tsx`), new `web/src/components/user/users-summary.tsx` (+ `.test.tsx`), `web/src/routes/$org/$project/users/index.tsx`, `web/src/routes/$org/$project/users/index.test.tsx`.

### Steps

1. `sparkline.tsx`:
   ```tsx
   export function Sparkline({ values, label, className }: { values: readonly number[]; label: string; className?: string })
   ```
   - An `<svg role="img" aria-label={label} viewBox="0 0 {values.length} 1" preserveAspectRatio="none" className={cn("h-4 w-20 text-primary", className)}>`.
   - One `<rect>` per value: `x={i + 0.1}`, `width={0.8}`, `height={v / max}`, `y={1 - v / max}`, `fill="currentColor"`. `max = Math.max(1, ...values)`. A zero value draws no rect.
   - Empty `values` renders the svg with no rects.
   - Tests: 3 values `[0, 2, 4]` → 2 rects, the last one has `height="1"`; `aria-label` is set.
2. `bar-cell.tsx`:
   ```tsx
   export function BarCell({ value, max, children }: { value: number | undefined; max: number; children: ReactNode })
   ```
   - `<div className="relative flex h-full w-full items-center justify-end">`, an absolutely positioned `<span aria-hidden="true" data-bar className="absolute inset-y-1 right-0 rounded-sm bg-primary/15" style={{ width: \`${pct}%\` }} />`, then `<span className="relative">{children}</span>`.
   - `pct = value && max > 0 ? Math.min(100, (value / max) * 100) : 0`.
   - Tests: value 5 max 10 → `width: 50%`; undefined → `0%`.
3. `users-summary.tsx`:
   ```tsx
   export function UsersSummary({ kpis, users, more }: { kpis: readonly Schema<"Kpi">[] | undefined; users: number; more: boolean })
   ```
   - One row: `<div aria-label="Summary" className="flex h-8 shrink-0 items-center gap-4 overflow-x-auto border-b px-3 text-xs">`.
   - Items, each `<span className="shrink-0"><span className="text-muted-foreground">{label}</span> <span className="num">{value}</span></span>`:
     `Users` = `users` plus `+` when `more`; then `Traces`, `Error rate`, `p95 latency`, `Cost`, `Tokens` from `kpis` (metrics `traces`, `error_rate`, `latency_p95`, `cost_total`, `total_tokens`), formatted with `formatKpi` from `@/lib/metrics` and the value from `kpiDelta(kpi ?? {}).value` (same as `components/charts/kpi-tiles.tsx`). Missing → `—`. `kpis` undefined → every KPI shows `—`.
   - Tests: renders `Users 40+`, and the traces KPI value from a small hand-built `Kpi` array (read `Schema<"Kpi">` in `web/src/api/schema.gen.ts` for its shape).
4. `users/index.tsx`:
   - Query `useQuery({ ...metricsQuery(org, project, { from: resolved.from, to: resolved.to, filter: search.filter, includeEval: search.eval === 1, includeExperiments: search.exp === 1 }), enabled: !hold && !filterError })`. Check `metricsQuery`'s `from`/`to` types against `resolved` (strings vs numbers); convert like the dashboard route does (`routes/$org/$project/dashboard/index.tsx`).
   - Render `<UsersSummary kpis={metrics.data?.kpis} users={rows.length} more={list.hasNextPage} />` between `ListToolbar` and the table, only when the table renders (same condition as `DataTable`).
   - `index.test.tsx`: add a test that the "Summary" region shows `Users` and a value from the `get_metrics` fixture (`fixtures/api/responses` — find the metrics response and pick its `traces` KPI value).

### Done when

`npx tsc --noEmit`, `npx biome check src/components/user 'src/routes/$org/$project/users'`, `npx vitest run src/components/user 'src/routes/$org/$project/users'` all pass.

---

## U4 — Mock dataset: new user fields

**Depends on U2** (types in `schema.gen.ts`). **Files:** `web/src/mocks/generated/dataset.ts`, `web/src/mocks/generated/handlers.ts` (users list/detail handlers only), their tests (`web/src/mocks/generated/*.test.ts`).

### Steps

1. Read `usersFrom`, `users(...)`, `user(...)` in `dataset.ts` and `listUsers`/`oneUser` in `handlers.ts`.
2. Give `usersFrom` a second parameter `window: { from: number; to: number } | undefined` (ms). Compute per user:
   - `error_trace_count`: traces with `status === "error"`.
   - `activity`: 24 buckets; window = the argument, else `[min start, max end + 1)` over all rows passed in (the page's whole set, matching U-3); bucket width `Math.ceil((to - from) / 24)`; bucket `Math.min(23, Math.max(0, Math.floor((start - from) / width)))`.
   - `scores`: from each trace's `scores` (`ScoreBrief`), numeric and boolean only (boolean true = 1); group by `(name, data_type)`; `avg`, `count`; sort by name then data_type.
   - `metadata`: the latest trace's (greatest `start_time`, then `trace_id`) `metadata` entries whose key starts with `user.`, prefix removed, first 20 keys in key order. Omit the property when empty.
3. Pass the window from `listUsers` (the parsed `from`/`to`; both or neither) and `undefined` from `user(...)`.
4. Make the generator write `user.*` metadata on traces: find where generated traces get `metadata` and `user_id`. For each user id, derive deterministically from the user index: `user.name` = `"\"User NNN\""`, `user.plan` = one of `"\"free\""`, `"\"pro\""`, `"\"team\""` (index % 3), `user.country` = one of `"\"FR\""`, `"\"US\""`, `"\"DE\""`, `"\"JP\""` (index % 4). Only on traces that have a `user_id`. If the generator's randomness makes this hard to keep deterministic, STOP and ask.
5. Tests: in the existing dataset/handlers tests add one test: a user from `GET /users` has `activity.length === 24`, the activity sum equals `trace_count`, `error_trace_count <= trace_count`, and `metadata.plan` is one of the three values.

### Done when

`npx tsc --noEmit`, biome, and `npx vitest run src/mocks` pass.

---

## U5 — List columns

**Depends on U2 and U3** (wave C, parallel with U4). **Files:** `web/src/components/user-columns.tsx` (+ new `user-columns.test.tsx`), `web/src/lib/search/users.ts` (+ its test if one exists), `web/src/routes/$org/$project/users/index.tsx`, `web/src/routes/$org/$project/users/index.test.tsx`.

### Steps

1. `lib/search/users.ts`:
   - `USER_COLUMN_IDS` becomes `["user", "activity", "traces", "sessions", "traces_per_session", "tokens", "cost", "errors", "error_rate", "metadata", "first_seen", "last_seen"]`.
   - `isUserColumnId(id)` also accepts `score:<non-empty name>` exactly like `isTraceColumnId` in `lib/search/traces.ts`.
2. `user-columns.tsx`:
   - `userColumnSpecs(scoreNames: readonly string[])` returns the fixed specs (label map below; `metadata` without `defaultVisible`, every other one with `defaultVisible: true`) with one `{ id: \`score:${name}\`, label: name, defaultVisible: true }` per name inserted right after `error_rate`. Change the return type to `ColumnSpec & { label: string }` the way `trace-columns.tsx` does.
   - Labels: `activity` "Activity", `traces_per_session` "Traces / session", `error_rate` "Error rate", `metadata` "Metadata".
   - `export function userScoreNames(rows: readonly Schema<"EndUserSummary">[]): string[]` — distinct `score.name`, sorted with `localeCompare`.
   - `orderedUserColumns(rows, visible)` builds the defs with `max` values computed from `rows` (traces, tokens, cost) and the score names.
   - Cells:
     - `activity` (size 96): `<Sparkline values={row.activity} label={\`${row.trace_count} traces over the range\`} />`.
     - `traces`, `tokens`, `cost`: wrap the current content in `<BarCell value={…} max={max…}>`.
     - `traces_per_session` (size 104, right): `session_count > 0 ? (trace_count / session_count).toFixed(1) : "—"`.
     - `error_rate` (size 88, right): `trace_count > 0 ? \`${Math.round((error_trace_count / trace_count) * 100)}%\` : "—"`, class `text-destructive` when `error_trace_count > 0`.
     - `score:<name>` (size 96, right): the first entry of `row.scores` with that name; numeric → `avg.toFixed(2)`, boolean → `${Math.round(avg * 100)}%`; missing → muted `—`. A `Tooltip` with `${count} scores`.
     - `metadata` (size 220): `key: value` pairs joined by `·`, each value shown decoded (`JSON.parse` when it parses to a string or number, else the raw text), `truncate` class, `title` attribute with the full text.
3. `users/index.tsx`: `specs = useMemo(() => userColumnSpecs(userScoreNames(rows)), [rows])`, `tableColumns = useMemo(() => orderedUserColumns(rows, visible), [rows, visible])`.
4. Tests (`user-columns.test.tsx`): render a `DataTable` or call the cell renderers on a hand-built `EndUserSummary` (copy the shape from `fixtures/api/responses/users.json` after U2): error rate `25%` for 1/4, traces/session `2.0`, score boolean `75%`, sparkline present, metadata text `name: Ada Example`. `index.test.tsx`: the header row contains "Error rate" and "Activity".

### Done when

tsc, biome on the touched files, and `npx vitest run src/components 'src/routes/$org/$project/users' src/lib/search` pass.

---

## U6 — User drawer (list layouts)

**Depends on U4 and U5.** **Files:** `web/src/lib/search/users.ts`, new `web/src/components/user/user-panel.tsx` (+ `.test.tsx`), `web/src/routes/$org/$project/users/index.tsx`, `web/src/routes/$org/$project/users/index.test.tsx`.

Read `web/src/routes/$org/$project/traces/index.tsx` first: the Users page copies its `SplitPane` wiring (lines around `useListLayout`, the scroll restore `useLayoutEffect`, `useShortcut("dismiss")`, `<SplitPane name=… mode={layout} filmstrip=… open=… start=… end=…>`, `selectedRowId`, `onActivate`, `onSelectionMove`).

### Steps

1. `lib/search/users.ts`: add `user?: string` to `UsersSearch`, parsed with `parseString(search.user, 256)` (check `parseString`'s signature). Add `export function withoutUser(search)` if a helper is needed in the route.
2. `user-panel.tsx`:
   ```tsx
   export function UserPanel({ userId, row, includeEval, includeExperiments, layout, onClose }: {
     userId: string;
     row?: Schema<"EndUserSummary">;   // the list row when loaded
     includeEval: boolean;
     includeExperiments: boolean;
     layout?: ReactNode;               // <TraceLayoutControls />
     onClose: () => void;
   })
   ```
   - Data: `useQuery({ ...userQuery(org, project, userId, { includeEval, includeExperiments }), placeholderData: keepPreviousData })` for sessions; `user = row ?? detail.data?.user`. While neither exists show three `Skeleton` lines.
   - Header (`border-b px-3 py-1.5`, like `TraceHeader`): `IdText value={userId} kind="user"`, then on the right `{layout}`, a `Button variant="ghost" size="xs" asChild` with a `Link` "Open page" to `/$org/$project/users/$userId` (search `eval`/`exp` flags), and a close `Button variant="ghost" size="icon-xs" aria-label="Close"` with the `X` icon calling `onClose`. Copy the close button markup from `components/trace/trace-header.tsx`.
   - Body (`overflow-auto`, sections with `text-[10.5px]` small headings `text-muted-foreground uppercase`):
     1. **Totals**: traces, sessions, traces/session, error rate, tokens, cost, first seen → last seen (`TimeText`).
     2. **Activity**: `<Sparkline values={user.activity} label="Activity" className="h-12 w-full" />`.
     3. **Scores**: one line per entry: name, value (same format as U5), `count` muted. "No scores" when empty.
     4. **Metadata**: a two-column `dl`, keys and decoded values (same decoding as U5). "No metadata" when empty.
     5. **Recent sessions**: up to 20 from `detail.data.sessions`, each a `Link` to `/$org/$project/sessions/$sessionId` showing start time (`TimeText`), trace count and cost. "No sessions" when empty.
   - Tests: with MSW defaults (`fixtures/api` mocks serve `get_user` for `user-42`), render inside the existing test helpers used by other component tests (look at `components/trace/*.test.tsx` for the render-with-router/query helper) and assert: the header shows `user-42`, "Open page" links to `/acme/support-bot/users/user-42`, Close calls `onClose`, the Metadata section shows `name` and `Ada Example`.
3. `users/index.tsx`:
   - Wrap the page in `<div className="relative h-full min-h-0 flex-1"><div className="absolute inset-0"><SplitPane name="users" mode={layout} …></div></div>` like Traces.
   - `start` = the current toolbar + summary + table column.
   - `open = search.user !== undefined`; `end = search.user ? <UserPanel userId={search.user} row={rows.find((r) => r.user_id === search.user)} … layout={<TraceLayoutControls />} onClose={() => navigate(... withoutUser)} /> : null`.
   - `filmstrip`: `<Filmstrip items={rows.map((row) => ({ id: row.user_id, label: row.user_id, detail: formatCost(row.cost_total), status: row.error_trace_count > 0 ? "error" : "ok" }))} selectedId={search.user} onSelect={(id) => navigate({… user: id})} onBack={() => navigate(withoutUser)} />`. Check the `status` type (`Schema<"SpanStatus">`) accepts `"ok"` and `"error"`.
   - `DataTable`: `selectedRowId={search.user}`, `onActivate` now sets `user: row.user_id` (no longer navigates to the user page), `onSelectionMove` sets it with `replace: true`.
   - Dismiss shortcut and scroll restore: copy from Traces, using `[aria-label="Users"] [data-scroll-container]`.
   - `index.test.tsx`: update any test that expected a row click to open the user page; add: clicking a row sets `?user=` and shows the panel with "Open page"; Esc closes it (use `waitFor` for the exit animation, as the Traces tests do).

### Done when

tsc, biome, `npx vitest run src/components/user 'src/routes/$org/$project/users' src/lib/search` pass, then the full `npx vitest run src` passes.
