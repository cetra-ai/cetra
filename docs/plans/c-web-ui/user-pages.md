# Users list, user drawer and user page: polish

Plan for **Haiku** implementers, one phase at a time; **Sonnet** reviews. Read the whole of your phase, and the Conventions and Decisions, before editing.

It builds on the Sessions-list work that is uncommitted in this checkout (`docs/plans/c-web-ui/sessions-list.md`): `SessionSummary` already has `error_trace_count`, `first_input_preview` and `last_output_preview`, and `session-columns.tsx` has a `first_message` column and a private `RelativeTime`.

## Conventions (every phase)

- **Never guess.** If something in your phase is unclear, contradicts the code, or a step fails in a way the phase does not explain, STOP. Make no further edits. Your final message must start with `QUESTION:` followed by the question and what you found (file, line, error text). You will be resumed with an answer.
- Work in the worktree `/home/sebastien/travaux/llm-observer/.claude/worktrees/user-pages` (branch `claude/user-pages`), never in `/home/sebastien/travaux/llm-observer` itself. Every path in this plan is relative to the worktree. Do not create branches or worktrees. Do not commit, stash, reset, checkout or switch branches.
- Cargo commands run with `export CARGO_TARGET_DIR=/home/sebastien/travaux/llm-observer/target` (shares built dependencies with the main checkout).
- Edit only the files your phase lists. Other phases run at the same time on other files. If you need a change in another file, STOP and ask.
- Before any node command: `export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH`.
- Web checks, from `web/`: `./node_modules/.bin/vitest run <paths>`, `npx tsc -b` (no errors). Biome from the repo root: `node_modules/.bin/biome check --write <paths>` (a "schema version does not match" line is informational). Delete `web/.vitest/` if a run creates it.
- Rust checks: `cargo test -p <crate> --all-features <filter>`, `cargo clippy -p <crate> --all-targets --all-features -- -D warnings`, `cargo fmt -p <crate>`.
- AGENTS.md rules apply: no `unwrap`/`expect` outside tests, no placeholders, no compatibility shims (when a signature changes, update every caller and delete the old one), errors through the crate's error types, no payload content in logs or error messages, tests for every behaviour change.
- The UI root font size is 13px: use explicit pixel text sizes (`text-[15px]`, `text-[13px]`, `text-[12px]`, `text-[11px]`, `text-[10px]`).
- Tests use the MSW mocks; a test that needs another answer overrides the endpoint with `server.use` (see `web/src/routes/$org/$project/users/$userId.test.tsx`). Never mock modules.
- Final report: files changed, tests added, the exact check commands you ran and their result, anything not done.

## Decisions (already made, do not revisit)

| # | Decision |
|---|---|
| UP-1 | `GET /users/{user_id}` takes optional `from` and `to` (RFC 3339 or epoch ms, same parsing as `GET /users`; either may be absent, as in `routes/read.rs::window`). With a window, the user row (totals, `activity`, `scores`, `metadata`) and `sessions` are computed over the user's traces whose `start_time` is in `[from, to)` plus the default exclusions; a session is listed when one of the user's traces in scope belongs to it, and its numbers cover that session's traces in scope. Without a window nothing changes. 404 when the user has no trace in scope. The activity buckets are the window when one is given. |
| UP-2 | The user page URL takes `range` or `from`+`to`, `eval`, `exp` and `tab` (`sessions` or `traces`; absent = Overview). Without `range`/`from`/`to` the page uses the preset `30d`. The drawer's "Open page" passes the list's `range` or `from`/`to`. |
| UP-3 | On the user page a 404 from the user query is not a "Not found" page: it renders the header (user id, range picker) and an empty state "No traces from this user in this range". Other errors throw as today. The route loader no longer turns a 404 into `notFound()`. |
| UP-4 | Identity comes from the user's `metadata` (the `user.*` keys without the prefix, JSON-encoded values): `name`, `email`, `plan`. A user with a `name` shows the name, with the id after it in mono, muted; otherwise the id alone. `plan` is a coloured pill (`categoryClasses(plan).pill`). `email` is shown in the drawer and page headers only. |
| UP-5 | The traces of one user are selected with the traces-view DSL filter `user_id = '<id>'`, the id quoted by the DSL's own quoting (`'` doubled). One helper builds it: `userTraceFilter(userId)`. |
| UP-6 | The user page has three tabs: **Overview** (Scores and Metadata cards, then the project dashboard's KPI row and charts computed for this user through `GET /metrics` with the UP-5 filter), **Sessions** (the sessions table, without the User column), **Traces** (the traces list filtered to this user, without the User column, infinite scroll). No Scores tab: the score averages are on Overview and their trend is the dashboard's Scores chart. |
| UP-7 | Users list: the User cell follows UP-4; First seen and Last seen show relative time ("3h ago") with the exact time on hover; the summary strip shows each KPI's change against the previous period, coloured as the dashboard's KPI tiles; under the strip a "Top users by cost" row shows the 5 users with the highest cost in the list's scope (same range, filter, eval/exp flags), from `GET /users?sort=cost_total&order=desc&limit=5`; clicking one opens the drawer. |
| UP-8 | The drawer: header with name (or id), plan pill, "Open page"; metric chips (traces, sessions, tokens, cost, error rate); an activity bar chart with time axis and tooltip over the list's range; scores; metadata; recent sessions showing the first message, relative start time, traces, cost, and a red dot when a trace failed. The drawer's user query passes the list's range (UP-1), so the drawer and the row agree. |
| UP-9 | The generated demo mock (`?mock=large`) honours UP-1's window on `GET /users/{id}`, and honours a filter of exactly the UP-5 form on `GET /traces` and `GET /metrics` (other filters stay ignored, as today). The fixture mock (`fixtures/api`) is not changed: its entries match query subsets, so extra params still match. |

## Waves

- **Wave A (parallel):** UP1 (server) ∥ UP2 (web helpers) ∥ UP3 (generated mock).
- **Wave B:** UP5 (drawer and `userQuery` range). Needs UP1 (regenerated `schema.gen.ts`) and UP2.
- **Wave C (parallel):** UP4 (list) ∥ UP6 (user page). Need UP2 and UP5.
- Sonnet code review after wave B and after wave C.
- **Wave D:** coordinator: spec 11, CHANGELOG, `just check-web`, `just check-rust`, browser check.

---

## UP1 — Server: a time window on the user detail

**Edit only:** `crates/storage/src/reads/users.rs`, `crates/server/src/api/users.rs`, `crates/server/src/routes/users.rs`, `crates/server/src/api/openapi.rs`, `crates/server/src/api/common.rs` (one doc comment), `docs/spec/09-api.md`; plus what `just openapi` regenerates (`crates/server/openapi.json`, `web/src/api/schema.gen.ts`) and what `just api-examples` regenerates under `fixtures/api/` (expected: nothing; if it changes files, list them in your report).

Steps:

1. Storage, `crates/storage/src/reads/users.rs`:
   - Change `pub async fn user_detail(ctx: &QueryContext, user_id: &str, include_eval: bool, include_experiments: bool, timeout: Duration)` to `pub async fn user_detail(ctx: &QueryContext, user_id: &str, scope: ListScope, timeout: Duration) -> Result<Option<UserDetailData>, RunError>`.
   - Change the private `async fn load(ctx, user_id, include_eval, include_experiments)` to `async fn load(ctx: &QueryContext, user_id: &str, scope: ListScope)`, and use the given `scope` instead of building `ListScope { window: None, … }`. Everything else in `load` stays: `scoped_traces(ctx, scope, false)` already applies the window, so the user row and the sessions plan (built from `excluded_plan`) follow it.
   - Update the doc comment of `user_detail` to say the scope's window and exclusions select the traces (spec 09 § Observability data, UP-1).
   - Update every existing test call to pass `ListScope { window: None, include_eval: …, include_experiments: … }`: grep `user_detail(` in that file (calls near lines 773, 791, 957, 961, 972, maybe more). The only non-test caller is `crates/server/src/routes/users.rs` (step 3).
   - New test `user_detail_follows_the_window`: use the file's test helpers `context_of(&spans)`, `user_span(trace, start, user)` and `window_of(from, to)` (near lines 995–1015). Build spans of one user at three different times (one span = one trace); set `span.session_id = Some(…)` so two of them share one session and the third has another session (`user_span` sets no session); call `user_detail` with a `TimeWindow` covering only the first two traces and check `trace_count == 2`, `first_seen`/`last_seen` from those two traces, and exactly one session row whose trace count is 2. Then a window that covers no trace of the user → `Ok(None)`.
2. Server types, `crates/server/src/api/users.rs`: add

   ```rust
   /// Query parameters of `GET /api/v1/projects/{project}/users/{user_id}`.
   ///
   /// Spec 09 § Observability data.
   #[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize, IntoParams)]
   #[serde(deny_unknown_fields)]
   pub struct UserDetailParams {
       /// Inclusive range start. RFC 3339 or epoch milliseconds.
       #[serde(default)]
       pub from: Option<TimeParam>,
       /// Exclusive range end. RFC 3339 or epoch milliseconds.
       #[serde(default)]
       pub to: Option<TimeParam>,
       /// Include traces from the evaluation environment. Default false. Spec 05 § Default exclusions.
       #[serde(default)]
       pub include_eval: bool,
       /// Include experiment traces. Default false. Spec 05 § Default exclusions.
       #[serde(default)]
       pub include_experiments: bool,
   }
   ```

   This derive line compiles as written: `TimeParam` is `Debug, Clone, Copy, PartialEq, Eq` (`api/common.rs:289`) and `Option` gives `Default`. The struct is not `Copy` (unlike `ExclusionParams`); that is fine.
3. Route, `crates/server/src/routes/users.rs`, `get_user`:
   - Take `error::Query(params): error::Query<UserDetailParams>` instead of `ExclusionParams`; add `UserDetailParams` to the `crate::api::users::{…}` import (line 12) and remove `ExclusionParams` from this file's `crate::api::common::{…}` import if nothing else in the file uses it. `ExclusionParams` itself stays: `routes/sessions.rs` and `get_session` use it.
   - Build `let scope = list_scope(params.from, params.to, params.include_eval, params.include_experiments)?;` once, pass it to `storage::user_detail(&ctx, &path.user_id, scope, deadline.remaining()?)`, and pass the same `scope` to `attach_user_extras` (replace the hand-built `ListScope { window: None, … }`).
   - OpenAPI: `get_user` is documented by hand in `crates/server/src/api/openapi.rs` (~line 2310, `operation("get_user", …).parameters(Some(with_query::<ExclusionParams>(…)))`). Change only `with_query::<ExclusionParams>` to `with_query::<UserDetailParams>` in that operation and add `UserDetailParams` to that file's imports. Do not touch its `.description(...)` text. `ExclusionParams` stays imported there (`get_session` uses it).
   - `crates/server/src/api/common.rs` ~line 351: the `ExclusionParams` doc comment says "session and user detail endpoints"; make it "the session detail endpoint".
4. Route tests in the same file (use the `loaded()` / `get_json` helpers; the module has `use super::*`). The existing detail test `detail_lists_the_user_sessions_with_scores` must keep passing unchanged. Add:
   - `detail_with_a_window_counts_only_traces_in_it`: read the example dataset's user-42 traces (the existing list test says 3 traces, 1 session; read `fixtures/api/dataset/spans.json` for their start times) and request `/users/user-42?from=<A>&to=<B>` with a window that holds a known subset; assert `trace_count` and `first_seen` from the dataset (compute them from the data, don't copy the response).
   - `detail_outside_the_window_is_404`: a window before every trace → 404.
   - `detail_rejects_unknown_params`: `?foo=1` → `StatusCode::BAD_REQUEST` (as `routes/deletions.rs` tests near lines 816 and 1631 assert for unknown params).
5. Spec 09, `docs/spec/09-api.md`, in the `GET /users` · `GET /users/{user_id}` row: add "`GET /users/{user_id}` also takes `from` and `to`: the user row and `sessions` then cover the user's traces that start in `[from, to)`, a session's numbers cover its traces in that window, and the activity buckets span the window; 404 when the user has no trace in scope".
6. Regenerate: `just openapi`, then `just api-examples`. Check `git diff --stat` on `crates/server/openapi.json` and `web/src/api/schema.gen.ts`: the only change must be the new query parameters of `get_user`. If anything else changes, STOP and ask.

**Tests:** the storage test and the three route tests above.

**Done when:** `cargo test -p cetra-storage --all-features users`, `cargo test -p cetra-server --all-features users`, `cargo test -p cetra-server --test openapi`, clippy and fmt on both crates pass; `schema.gen.ts` shows `from?` and `to?` on the `get_user` query.

---

## UP2 — Web helpers

**Edit only:** `web/src/lib/dsl/dsl.ts` (+ its test file), `web/src/lib/search/users.ts` (+ `users.test.ts`), new `web/src/lib/user-identity.ts` (+ `user-identity.test.ts`), `web/src/components/user-columns.tsx` (import change only), `web/src/components/user/user-panel.tsx` (import change only), new `web/src/components/relative-time.tsx` (+ test), `web/src/components/session-columns.tsx` (remove `RelativeTime`, import the shared one), `web/src/components/charts/kpi-tiles.tsx`, new `web/src/components/charts/count-bar-chart.tsx` (+ test), `web/src/components/session/sessions-started-chart.tsx` (+ its test if needed), new `web/src/components/user/activity-chart.tsx` (+ test), new `web/src/components/user/user-chips.tsx` (+ test), `web/src/api/queries.ts`.

Steps:

1. DSL quoting, `web/src/lib/dsl/dsl.ts`: rename the private `function quote(value: string): string` (~line 498) to `export function quoteString(value: string): string` and rename all its call sites in that file (~lines 208, 210, 482, 490; grep `quote(` to be sure). Test in `web/src/lib/dsl/dsl.test.ts`: `quoteString("it's")` is `'it''s'`.
2. `web/src/lib/search/users.ts`:
   - Add `import { quoteString } from "@/lib/dsl/dsl";` and

     ```ts
     /** The traces-view filter that selects one end user's traces (UP-5). */
     export function userTraceFilter(userId: string): string {
       return `user_id = ${quoteString(userId)}`;
     }
     ```
   - Extend the page search (UP-2): replace the existing `UserPageSearch` type and `parseUserPageSearch` function in place (today they handle only `eval` and `exp`; the existing test in `users.test.ts` must still pass):

     ```ts
     export const USER_PAGE_TABS = ["sessions", "traces"] as const;
     export type UserPageTab = (typeof USER_PAGE_TABS)[number];

     /** Search params of the user page. The list keeps its own URL. */
     export type UserPageSearch = {
       range?: Preset;
       from?: string;
       to?: string;
       eval?: 1;
       exp?: 1;
       tab?: UserPageTab;
     };
     ```

     `parseUserPageSearch` parses `range` (`parseEnum(search.range, PRESETS)`), `from`/`to` (kept only when both parse and `from < to`, exactly as `parseUsersSearch` does), `eval`, `exp`, and `tab` (`parseEnum(search.tab, USER_PAGE_TABS)`), and returns only the keys that are set.
   - Add `export const USER_PAGE_DEFAULT_RANGE: Preset = "30d";` and

     ```ts
     /** The page's range: the URL's, else the last 30 days (UP-2). */
     export function userPageRange(search: UserPageSearch): RangeState {
       return search.range || (search.from && search.to)
         ? rangeFromSearch(search)
         : { kind: "preset", preset: USER_PAGE_DEFAULT_RANGE };
     }
     ```

     (import `rangeFromSearch` and `RangeState` from `@/lib/time-range`).
   - Add the URL form of a range, used by every link that carries the page's range:

     ```ts
     /** A range as URL params: the preset, or the absolute pair. Always explicit. */
     export function rangeParams(state: RangeState): { range: Preset } | { from: string; to: string } {
       return state.kind === "preset" ? { range: state.preset } : { from: state.from, to: state.to };
     }
     ```
     Test: both kinds.
   - Tests in `users.test.ts`: `userTraceFilter("o'brien")` is `user_id = 'o''brien'`; `parseUserPageSearch` keeps `range=7d`, `tab=traces`, drops `tab=bogus`, drops `from` without `to` and an inverted pair; `userPageRange({})` is the 30d preset, `userPageRange({ range: "7d" })` is 7d, an absolute pair stays absolute.
3. Identity, new `web/src/lib/user-identity.ts`:
   - Move `decodeValue` and `scoreText` from `web/src/components/user-columns.tsx` here unchanged (keep their doc comments; `scoreText` needs `import type { Schema } from "@/api/types"`), export them, and change the imports in `user-columns.tsx` (it uses `decodeValue` in `MetadataCell` and `scoreText` in `ScoreCell`) and `user/user-panel.tsx` to `@/lib/user-identity`. Remove the old exports from `user-columns.tsx`. (This avoids an import cycle between `user-columns.tsx` and `user/user-chips.tsx`.) Grep for other importers of `decodeValue` and `scoreText` and update them too (if one is outside the files this phase lists, STOP and ask).
   - Add

     ```ts
     export type UserIdentity = { name?: string; email?: string; plan?: string };

     /** `name`, `email` and `plan` from a user's metadata, decoded; empty strings count as absent (UP-4). */
     export function userIdentity(metadata: Record<string, string> | undefined): UserIdentity
     ```

     Each field is `decodeValue(metadata[key]).trim()` when the key exists and the result is not empty.
   - Tests: all three present (JSON-encoded strings decode), missing metadata → `{}`, a `name` of `"\"\""` (empty JSON string) is absent, a non-JSON value is kept as is.
4. Relative time, new `web/src/components/relative-time.tsx`: move the private `RelativeTime` function out of `web/src/components/session-columns.tsx` (keep its body and doc comment), export it as `export function RelativeTime({ value }: { value: string })`, and import it in `session-columns.tsx`. In `session-columns.tsx`, delete the stray line `/** A preview on one line; the full text shows on hover. */` directly above `RelativeTime` (`Preview` is defined and documented in `trace-columns.tsx`). Remove imports in `session-columns.tsx` that become unused. Test `relative-time.test.tsx`: renders "2h ago" for a time two hours before `Date.now()` (use `vi.useFakeTimers({ now: … })` or compute from `Date.now()`), and the `dateTime` attribute is the input.
5. KPI delta, `web/src/components/charts/kpi-tiles.tsx`: export the existing `Delta` component (`export function Delta`) and add `export const KPI_ADVERSE: Record<KpiMetric, boolean>` built from `TILES` (`traces: false, error_rate: true, latency_p95: true, cost_total: true, total_tokens: false`); make `TILES` use `KPI_ADVERSE[metric]` instead of repeating the booleans. No behaviour change; existing `kpi-tiles.test.tsx` must pass.
6. Count bar chart, new `web/src/components/charts/count-bar-chart.tsx`: move the chart markup of `SessionsStartedChart` (the `ChartContainer`/`BarChart`/`XAxis`/`YAxis`/`Tooltip`/`Bar` block and the tooltip component) into

   ```ts
   /** One bucket: its start in ms and its count, null when empty (no bar is drawn). */
   export type CountBucket = { time: number; count: number | null };

   /** A 64px bar chart of counts over time, with a time axis and a tooltip. */
   export function CountBarChart({ rows, label, seriesLabel }: {
     rows: readonly CountBucket[];
     /** The accessible name of the chart (`role="img"`). */
     label: string;
     /** The tooltip's row label, e.g. "Sessions" or "Traces". */
     seriesLabel: string;
   })
   ```

   `SessionsStartedChart` becomes `startedBuckets(...)` + `<CountBarChart rows={rows} label="Sessions started" seriesLabel="Sessions" />`; Delete `StartedBucket` and use `CountBucket` instead (it is used only inside `sessions-started-chart.tsx`). The chart config's `count.label` comes from `seriesLabel`. The existing `sessions-started-chart.test.tsx` must pass unchanged. Test `count-bar-chart.test.tsx`: renders `role="img"` with the given name and one bar per non-null row (copy the bar-counting approach of `sessions-started-chart.test.tsx`).
7. Activity chart, new `web/src/components/user/activity-chart.tsx`:

   ```ts
   /** Buckets of a user's `activity` (equal parts of `[from, to)`), as count-chart rows. Zero is empty. */
   export function activityBuckets(values: readonly number[], from: string, to: string): CountBucket[]

   export function ActivityChart({ values, from, to }: { values: readonly number[]; from: string; to: string })
   ```

   `activityBuckets`: `fromMs = epochMs(from)`, `toMs = epochMs(to)` (`@/lib/metrics`); if either is undefined, `toMs <= fromMs` or `values` is empty, return `[]`; else `width = (toMs - fromMs) / values.length`, row `i` is `{ time: Math.floor(fromMs + i * width), count: values[i] > 0 ? values[i] : null }`. `ActivityChart` renders `<CountBarChart rows={…} label="Activity" seriesLabel="Traces" />`, or a muted "No activity" line when every value is 0 or the rows are empty. Tests: bucket times and null for zero; empty for an inverted range; "No activity" for all zeros.
8. Metric chips, new `web/src/components/user/user-chips.tsx`:

   ```ts
   /** A user's totals as metric chips: traces, sessions, tokens, cost, error rate (UP-8). */
   export function UserChips({ user }: { user: Schema<"EndUserSummary"> })
   ```

   Use `MetricChip` (`@/components/trace/metric-chip`) with lucide icons `ListTree` "Traces" (`${trace_count} traces`, singular for 1), `MessagesSquare` "Sessions" (same rule), `Coins` "Tokens" (`formatTokens`, only when `total_tokens` is defined), `DollarSign` "Cost" (`formatCost`, only when `cost_total` is defined), `TriangleAlert` "Error rate" (`formatPercent(error_trace_count / trace_count)` from `@/lib/metrics`; only when `trace_count > 0`; when `error_trace_count > 0` add `className="border-status-error/40 text-status-error"`). Wrap them in `<div className="flex flex-wrap items-center gap-2">`.

   In the same file add the shared pieces the drawer (UP5), the list (UP4) and the page (UP6) use:

   ```ts
   /** The user's plan as a coloured pill. */
   export function PlanPill({ plan }: { plan: string })
   // <Badge variant="secondary" className={cn("h-4 px-1.5 text-[10px]", categoryClasses(plan).pill)}>{plan}</Badge>
   // Badge from "@/components/ui/badge", categoryClasses from "@/lib/colors", cn from "cn".

   /** Score averages: name, badge, count; "No scores" when empty. */
   export function UserScores({ scores }: { scores: readonly Schema<"UserScore">[] })

   /** The metadata as a key/value list; "No metadata" when empty. */
   export function UserMetadata({ metadata }: { metadata: Record<string, string> | undefined })
   ```

   `UserScores` and `UserMetadata` copy the markup of the "Scores" and "Metadata" sections' contents in `user/user-panel.tsx` (the `ul`/`dl` and the empty lines; not the `Section` heading), importing `decodeValue` and `scoreText` from `@/lib/user-identity`. Do not change `user-panel.tsx` in this phase beyond its `decodeValue`/`scoreText` imports (UP5 switches it to these components).

   Tests: chip texts for a sample user, the error chip has the error class only when errors exist, tokens/cost chips are absent when undefined; `PlanPill` renders its text; `UserScores` and `UserMetadata` render rows and their empty lines.
9. Queries, `web/src/api/queries.ts`: add

   ```ts
   /** The users with the highest cost in a list's scope (UP-7). */
   export function topUsersQuery(
     org: string,
     project: string,
     query: { from: string; to: string; filter?: string; includeEval?: boolean; includeExperiments?: boolean; limit: number },
   )
   ```

   returning `$api.queryOptions("get", "/api/v1/projects/{project}/users", { params: { path: { project: projectRef(org, project) }, query: { limit, from, to, sort: "cost_total", order: "desc", …filter/include flags only when set, as userListOptions does } } })`. Do not change `userQuery` in this phase (UP5 does).

**Tests:** as listed in each step.

**Done when:** vitest passes for every touched test file plus `src/components/session-columns.test.tsx`, `src/components/session/sessions-started-chart.test.tsx`, `src/components/charts/kpi-tiles.test.tsx`, `src/components/user-columns.test.tsx`, `src/components/user/user-panel.test.tsx` and `src/lib/dsl`; `npx tsc -b` has no errors; Biome is clean on the touched files.

---

## UP3 — Generated mock: user window and user filter

**Edit only:** `web/src/mocks/generated/dataset.ts`, `web/src/mocks/generated/handlers.ts`, and their tests `dataset.test.ts`, `handlers.test.ts`.

Steps:

1. `dataset.ts`:
   - `Dataset.user` gains a window: `user(userId: string, includeEval: boolean, includeExperiments: boolean, window: { from?: number; to?: number }): Schema<"EndUserDetail"> | undefined`. Rows are the visible traces of the user whose `Date.parse(start_time)` is `>= from` (when set) and `< to` (when set). The activity window passed to `userSummaries` is `{ from, to }` when both are set, else `undefined` (today's behaviour). Sessions are built from those rows only (as today, from `rows`).
   - `Dataset.metrics` gains a last parameter `userId: string | undefined`: when set, both `visibleTraces(...)` and the eval traces passed to `metricsFrom` are first restricted to `trace.user_id === userId`.
2. `handlers.ts`:
   - Add

     ```ts
     /** The user id of a filter of exactly the form `user_id = '<id>'` (UP-5), else undefined. */
     export function userIdOfFilter(filter: string | undefined): string | undefined
     ```

     Match `/^\s*user_id\s*=\s*'((?:[^']|'')*)'\s*$/` and return the captured text with `''` replaced by `'`.
   - `oneUser` (~line 174): pass `{ from: parsed.from, to: parsed.to }` (only the keys that are defined) to `dataset.user`. The only callers of `dataset.user(` and `dataset.metrics(` are this file (~lines 183, 204) and the tests.
   - `metrics` (~line 190): pass `userIdOfFilter(parsed.filter)` to `dataset.metrics`.
   - `filterTraces`: when `userIdOfFilter(query.filter)` is defined, also keep only traces with that `user_id`. `filterTraces` also serves the spans list; that is intended (spans of the user's traces).
   - Do **not** change `ignoredFilter` itself (sessions, users and spans still ignore filters and keep flagging them). Only in the traces list handler (~line 80, `HttpResponse.json(..., { headers: ignoredFilter(parsed) })`) and in `metrics` (~line 211) replace `ignoredFilter(parsed)` with `userIdOfFilter(parsed.filter) === undefined ? ignoredFilter(parsed) : {}`.
3. Tests:
   - `dataset.test.ts`: for a user with traces on several days, a window around one day gives that day's trace count, `first_seen` in the window, and only sessions with a trace in the window; a window with no trace → `undefined`; `metrics(…, userId)` counts only that user's traces (compare the `traces` KPI with a manual count).
   - `handlers.test.ts`: `userIdOfFilter("user_id = 'o''brien'")` is `o'brien`; other filters → undefined; `GET /traces?filter=user_id%20%3D%20'<id>'` returns only that user's traces and no ignored-filter header; `GET /users/<id>?from&to` honours the window.

**Done when:** vitest passes for `src/mocks`, `npx tsc -b` has no errors, Biome is clean on the touched files.

---

## UP5 — Drawer

Runs after UP1 and UP2. **Edit only:** `web/src/api/queries.ts` (`userQuery` only), `web/src/components/user/user-panel.tsx` (+ `user-panel.test.tsx`), `web/src/routes/$org/$project/users/index.tsx` (the `<UserPanel …>` props only) and `index.test.tsx`, `web/src/routes/$org/$project/users/$userId.tsx` (the two `userQuery` calls only: pass the page's resolved range, see step 1) and `$userId.test.tsx`.

Steps:

1. `userQuery(org, project, userId, flags: { from?: string; to?: string; includeEval?: boolean; includeExperiments?: boolean })`: add `from` and `to` to the query when set. In `$userId.tsx`, for now, pass the range from `userPageRange(search)` resolved with `resolveRange(range, Date.now())` in the loader and the component (UP6 rewrites this page; keep the change minimal so it compiles and its tests pass). If the loader's `loaderDeps` must change for this, add `range`, `from`, `to` to it. The loader and the component resolve `Date.now()` separately, so their keys differ by milliseconds; that is accepted here (UP6 rewrites it).
2. `UserPanel` gets new required props `from: string` and `to: string` (the list's resolved range) and an optional `range` prop of type `{ range?: Preset; from?: string; to?: string }` — the list's URL range params — used for the "Open page" link's search (merge into `pageSearch`). Pass `from`, `to` into `userQuery`. In `users/index.tsx` pass `from={resolved.from} to={resolved.to}` and `range={search.range ? { range: search.range } : search.from && search.to ? { from: search.from, to: search.to } : {}}`.
3. Layout of the drawer body (keep the outer `<section aria-label="User">`, the header bar with layout controls / Open page / Close, the loading and error states):
   - Header bar: replace `<IdText value={userId} kind="user" />` with: the name (from `userIdentity(user?.metadata)`) in `text-[13px] font-semibold` followed by the id in `font-mono text-[11px] text-muted-foreground`, or the `IdText` alone when there is no name; then `<PlanPill plan={plan} />` (`@/components/user/user-chips`) when there is a plan. While `user` is not loaded, show the `IdText` as today.
   - Body, `p-3`, `flex flex-col gap-4`:
     1. `email` (when present) as a muted `text-[12px]` line, then `<UserChips user={user} />`, then the time span line as today.
     2. Section "Activity": `<ActivityChart values={user.activity} from={from} to={to} />`.
     3. Sections "Scores" and "Metadata": keep the `Section` headings, and replace their contents with `<UserScores scores={user.scores} />` and `<UserMetadata metadata={user.metadata} />` (from UP2). Delete the old "Totals" section (the chips replace it), the `Stat` component, the `Sparkline` import, and any other code of `user-panel.tsx` that becomes unused.
     4. Section "Recent sessions": each session (first 20) is a `Link` to the session page, rendered as a block with two lines: line 1 — a red dot (`<span aria-label="Has errors" className="size-1.5 shrink-0 rounded-full bg-status-error" />`) when `error_trace_count > 0`, then `first_input_preview` truncated to one line (`truncate`), or the `session_id` in mono when there is no preview; line 2 (muted, `text-[11px]`) — `<RelativeTime value={session.start_time} />` · `N traces` · `formatCost(cost_total)` · `formatDuration(duration_ns)`. Use `title` on line 1 with the full preview.
4. Tests (`user-panel.test.tsx`; render it the way the existing test does). The existing assertion `getByText("Ada Example")` (~line 14) will now match twice (header and metadata): that break is intended; change it to `getAllByText(...)` or scope it with `within`. New: the name and id show when metadata has a name; the plan pill shows; the chips show; the activity chart has `role="img"` named "Activity"; a recent session shows its first message and links to `/…/sessions/<id>`; the error dot shows for a session with `error_trace_count > 0` (override the user endpoint with `server.use` returning a fixture copy with that field changed). And in `users/index.test.tsx` nothing changes unless a test breaks — if one breaks, fix the test only when the break is the intended new layout, and list it in the report.

**Done when:** vitest passes for `src/components/user`, `src/routes/$org/$project/users`; `npx tsc -b` has no errors; Biome clean.

---

## UP4 — Users list

Runs after UP2 and UP5. **Edit only:** `web/src/components/user-columns.tsx` (+ `user-columns.test.tsx`), `web/src/components/user/users-summary.tsx` (+ test), new `web/src/components/user/top-users.tsx` (+ test), `web/src/routes/$org/$project/users/index.tsx` (+ `index.test.tsx`).

Steps:

1. User cell (`user-columns.tsx`, column `user`): `size` 220; remove `mono: true` from its `meta` (keep the icon). Cell: with `userIdentity(row.metadata).name` → `<span className="flex min-w-0 items-center gap-1.5"><span className="truncate font-medium">{name}</span><span className="truncate font-mono text-[11px] text-muted-foreground">{user_id}</span>{plan pill}</span>`; without a name → the id in `font-mono` plus the plan pill. The plan pill is `<PlanPill plan={plan} />` from `@/components/user/user-chips`. Set `title` on the outer span to `name ? \`${name} (${user_id})\` : user_id`.
2. `first_seen` and `last_seen` cells: `<RelativeTime value={…} />` (`@/components/relative-time`); `size` 104 for both.
3. The filmstrip in `users/index.tsx`: `label: userIdentity(row.metadata).name ?? row.user_id`.
4. Summary deltas (`users-summary.tsx`): the private `Item` component's props become `{ label: string; value: string; delta?: KpiDelta; adverse?: boolean; previous?: string }` (`KpiDelta` from `@/lib/metrics`). Each KPI item renders its value and then `<Delta adverse={KPI_ADVERSE[item.metric]} direction={delta.direction} changePct={delta.changePct} />` (from `@/components/charts/kpi-tiles`), where `delta = kpiDelta(kpi ?? {})`. Wrap the item in a `<span title={\`Previous: ${previous}\`}>` with `previous` formatted like `KpiTile` does (`formatKpi` or "—"). The "Users" item has no delta.
5. Top users, new `web/src/components/user/top-users.tsx`:

   ```ts
   export function TopUsers({ users, onSelect }: {
     users: readonly Schema<"EndUserSummary">[];
     onSelect: (userId: string) => void;
   })
   ```

   Renders nothing when `users` is empty or every user's `cost_total` is undefined or 0. Otherwise a `<section aria-label="Top users by cost" className="flex h-8 shrink-0 items-center gap-2 overflow-x-auto border-b px-3 text-[12px]">` with a muted label "Top by cost" then one `<button type="button">` per user (users with a positive `cost_total`, in the given order): `focus-ring relative inline-flex shrink-0 items-center gap-1.5 overflow-hidden rounded-md border px-2 py-0.5`; inside, an absolutely positioned background bar `bg-chart-1/15` whose width is `cost / maxCost * 100%`, then the name (or id in mono) and `formatCost(cost_total)` in `num`. `aria-label` is `` `${name ?? user_id}: ${formatCost(cost)}` ``. Click → `onSelect(user_id)`.
6. Route (`users/index.tsx`): a `useQuery({ ...topUsersQuery(org, project, { from: resolved.from, to: resolved.to, filter: search.filter, includeEval, includeExperiments, limit: 5 }), enabled: !hold && !filterError })`; render `<TopUsers users={top.data?.data ?? []} onSelect={(id) => navigate(… withUser(prev, id))} />` right under `<UsersSummary …/>`.
7. Tests:
   - `user-columns.test.tsx`: a row with metadata name/plan shows the name, the id and the plan; a row without metadata shows the id; first/last seen render relative text.
   - `users-summary.test.tsx`: with kpis having `value` and `previous`, the delta percentage shows (e.g. value 120, previous 100 → "20%").
   - `top-users.test.tsx`: renders buttons in order with costs; nothing for an empty list; clicking calls `onSelect` with the id.
   - `users/index.test.tsx`: the fixture mock serves the same users page for `sort=cost_total&limit=5` (its entries match query subsets), unsorted. Override with `server.use(http.get("/api/v1/projects/:project/users", …))` that returns a two-user page sorted by cost when `new URL(request.url).searchParams.get("sort") === "cost_total"`, and otherwise falls through (`return undefined` lets the next handler answer — check MSW's passthrough idiom used in other tests, e.g. grep `server.use` in `web/src/routes`; if none falls through, return the fixture page for the other case by importing the fixture JSON the way `$userId.test.tsx` imports fixtures). Assert the "Top users by cost" region lists them in order, and clicking one opens the drawer (`?user=` in the URL).

**Done when:** vitest passes for `src/components/user-columns.test.tsx`, `src/components/user`, `src/routes/$org/$project/users`; `npx tsc -b` has no errors; Biome clean.

---

## UP6 — User page

Runs after UP2 and UP5. **Edit only:** `web/src/routes/$org/$project/users/$userId.tsx` (+ `$userId.test.tsx`), new `web/src/components/user/user-overview.tsx`, new `web/src/components/user/user-traces.tsx`.

Steps:

1. Route:
   - `validateSearch: parseUserPageSearch`; `loaderDeps: ({ search }) => ({ eval: search.eval, exp: search.exp, range: search.range, from: search.from, to: search.to })`.
   - Remove the route's `loader` and `loaderDeps` entirely (its `Date.now()` never matched the component's window, so it prefetched nothing; UP-3 also drops its `notFound()`). The component loads the user. Remove imports that become unused (`notFound`, `problemOf` if unused, …). Replace the `useMemo` window that UP5 added with the anchor pattern below.
   - Component: keep a range anchor like the list (`const [anchor, setAnchor] = useState(() => Date.now())`, reset when the range key changes; copy the pattern from `users/index.tsx` lines with `rangeKey` / `seenRange`). `resolved = resolveRange(userPageRange(search), anchor)`.
   - `detail = useQuery(userQuery(…, { from: resolved.from, to: resolved.to, includeEval, includeExperiments }))`. While pending: a header skeleton (`<Skeleton className="h-6 w-64" />` in the header area). On error: when `problemOf(error)?.status === 404`, render the header (id only) and `<EmptyState icon={Users} title="No traces from this user in this range" />`; otherwise throw.
2. Header (`UserHeader`, replace today's), modelled on `SessionHeader` in `web/src/components/session/session-view.tsx` (read it): `<header className="flex shrink-0 flex-col gap-2 border-b px-4 py-3">`.
   - `range` below means the page's `RangeState` (`userPageRange(search)`), and "range params" means `rangeParams(range)` from UP2 (always explicit, so a page opened without `range` still links with `range: "30d"`).
   - Row 1: back link "Users" (`ChevronLeft`, to `/$org/$project/users` with search `{ ...rangeParams(range), eval/exp when set }`), "/", the name (or the id when no name) in `text-[15px] font-semibold select-text`, `CopyButton` for the id (`label="user ID"`), the id in `font-mono text-[12px] text-muted-foreground` when a name is shown, `<PlanPill plan={plan} />` when present, the email in muted `text-[12px]` when present. Right side (`ml-auto flex items-center gap-2`): `<TimeRangePicker state={range} onChange={(next) => …} />` (`@/components/time-range/time-range-picker`); on change navigate with `search: (prev) => { const { range: _r, from: _f, to: _t, ...rest } = prev; return { ...rest, ...rangeParams(next) }; }` and reset the anchor as the list does. Then a ghost `xs` Button-Link "Dashboard" (`LayoutDashboard` icon) to `/$org/$project/dashboard` with search `{ ...rangeParams(range), filter: userTraceFilter(userId), eval/exp when set }`, and the existing "Delete user data…" button and dialog unchanged.
   - Row 2: `<UserChips user={user} />` preceded by a `MetricChip` `Clock` "Time span" with first → last seen (as `SessionHeader` does).
3. Tabs: `Tabs` from `@/components/ui/tabs`, `value={search.tab ?? "overview"}`; `onValueChange` navigates with `tab` set (omitted for `overview`), `replace: true`. `TabsList` under the header (`px-4 pt-2`), triggers "Overview", "Sessions (N)" (N = `detail.sessions.length`, shown as `100+` when it equals 100), "Traces". Each `TabsContent` fills the remaining height (`min-h-0 flex-1`), only the active tab's content mounts (Radix default). Wrap the whole page body (header, `Tabs`, `TabsList` and every `TabsContent`) in one `<TooltipProvider>` (`@/components/ui/tooltip`), replacing today's wrapper around the `DataTable`: `KpiRow`, `DataTable` and `TimeText` use tooltips.
4. Overview, new `user-overview.tsx`:

   ```ts
   export function UserOverview({ user, from, to, range, includeEval, includeExperiments }: {
     user: Schema<"EndUserSummary">;
     from: string;
     to: string;
     /** The page's URL range params, carried to the traces list when a bucket is opened. */
     range: { range?: Preset; from?: string; to?: string };
     includeEval: boolean;
     includeExperiments: boolean;
   })
   ```

   Scrollable (`overflow-auto p-4`). Top: a grid `grid grid-cols-1 gap-3 md:grid-cols-2` of two cards (`rounded-md border p-3`, each with a heading `text-[11px] uppercase text-muted-foreground`): "Scores" with `<UserScores scores={user.scores} />` and "Metadata" with `<UserMetadata metadata={user.metadata} />` (from `@/components/user/user-chips`). The `range` prop is not needed by the metrics query; drop it from the props if `onBucket` doesn't use it (it doesn't: the bucket gives `from`/`to`). Below, a heading "Usage" and `metricsQuery(org, project, { from, to, filter: userTraceFilter(user.user_id), includeEval, includeExperiments })` through `useQuery`: loading → `KpiRowSkeleton` + `DashboardBodySkeleton`; data → `<KpiRow kpis={data.kpis} />` + `<DashboardBody response={data} onBucket={…} />`, where `onBucket(timeMs)` navigates to `/$org/$project/traces` with `bucketRange(timeMs, data.bucket_ns)` as `from`/`to`, `filter: userTraceFilter(user.user_id)` and the eval/exp flags (copy `openBucket` in `dashboard-view.tsx`). An error from the metrics query shows a muted "Could not load the usage charts." line instead of throwing. `UserOverview` relies on the page's `TooltipProvider`; it does not add its own.
5. Sessions tab: today's `DataTable` of `detail.sessions`, with columns `orderedSessionColumns(sessions, visibleColumnIds(specs, undefined).filter((id) => id !== "user"))`, `rowTone` red for sessions with `error_trace_count > 0` if `DataTable` `rowTone` fits (`(row) => row.error_trace_count > 0 ? "error" : undefined`).
6. Traces tab, new `user-traces.tsx`:

   ```ts
   export function UserTraces({ userId, from, to, includeEval, includeExperiments }: {
     userId: string; from: string; to: string; includeEval: boolean; includeExperiments: boolean;
   })
   ```

   `useInfiniteQuery(traceListOptions(org, project, { from, to, filter: userTraceFilter(userId), includeEval, includeExperiments }))` (`traceListOptions` is in `@/components/data-table/use-infinite-list`). Rows = `query.data?.pages.flatMap((page) => page.data) ?? []`. Columns: `orderedTraceColumns(rows, visibleColumnIds(traceColumnSpecs(scoreNames(rows)), undefined).filter((id) => id !== "user"))`. `DataTable` with `tableId="user-traces"`, `ariaLabel="Traces"`, `getRowId` trace id, `rowTone={traceRowTone}`, infinite props from the query, `emptyState={<EmptyState icon={ListTree} title="No traces" />}`, `onActivate` → navigate to `/$org/$project/traces/$traceId` with `params: { org, project, traceId }` and no `search` (that route accepts only `span` and `mode`). No sorting UI (omit `sorting`/`onSortingChange`). A query error throws.
7. Tests:
   - `$userId.test.tsx`: keep the existing tests passing, adjusting only what the new layout changes (e.g. "3 traces" now in a chip, the sessions grid is under the Sessions tab: click the tab first). New: Overview shows the Scores and Metadata cards and the KPI row; the Traces tab lists user-42's traces and opens a trace page; the Dashboard link's `href` contains `filter=user_id` and `range`; a 404 from `GET /users/:id` (`server.use`) shows "No traces from this user in this range" and the range picker; `?tab=traces` opens on the Traces tab.
   - No standalone component tests for `UserOverview` and `UserTraces` (they need the router and project context): test them through `renderApp` (`web/src/test/render.tsx`) in `$userId.test.tsx`, on `/acme/support-bot/users/user-42` (Overview) and `/acme/support-bot/users/user-42?tab=traces`, with `screen` and `server.use`. So UP6 creates no `user-overview.test.tsx` / `user-traces.test.tsx`.

**Done when:** vitest passes for `src/routes/$org/$project/users`, `src/components/user`; `npx tsc -b` has no errors; Biome clean.

---

## Wave D — Coordinator

- Spec 11 § Users: describe the list (identity cell, relative times, deltas in the strip, top users row), the drawer (UP-8) and the page (UP-2, UP-3, UP-6), with its URL params.
- CHANGELOG `[Unreleased]` → `Added`: the user page's tabs, usage charts and time range; the list's identity cell, top users and KPI deltas; `GET /users/{user_id}` `from`/`to`.
- `just check-web`, `just check-rust` (or at least storage and server tests, clippy, fmt, deny), browser check of `/acme/support-bot/users?range=30d&mock=large`, the drawer and the page in light and dark themes.
