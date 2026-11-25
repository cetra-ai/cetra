# Trace list totals (D-C-30)

The trace list gets a line under its table with the trace count, the error count and the total cost: of the checked rows when some are checked, otherwise of every trace the list matches (range, filter, search, flags), not only the loaded pages. The whole-list figures come from a new endpoint, `GET /api/v1/projects/{project}/traces/totals`, backed by a new storage read, `trace_totals`, that aggregates the list's rows in one scan. The checked rows' figures are summed in the browser. Implementers: the `glm-5-3` agent (Haiku when GLM has no credit).

**Prerequisites:** none. Branch `claude/trace-list-totals`, cut from `origin/main` at `ac0dfcb`. The spec and decision edits are already on the branch (commit "Plan trace list totals").

**Read first:**
- `docs/mvp-decisions.md` § Track C, row D-C-30.
- `docs/spec/09-api.md` § Observability data, rows `GET /traces` and `GET /traces/totals`.
- `docs/spec/05-query.md` § Trace and span APIs (row "Trace totals") and § Dashboard queries, paragraph "Counts are integers; … costs are summed as `Decimal128(38, 18)`".
- `docs/spec/11-ui.md` § Traces (list), bullet "Totals line under the table".
- `crates/storage/src/reads/traces.rs`: `TraceListRequest`, `trace_list`, `match_count` and `count_matches` (the pattern to follow: parse the filter, then `tokio::time::timeout` around an inner async fn), `scope_frame`, `single_row`, and the test module's helpers `context`, `scope`, `listed`, `rows`, `day`, `context_for`, `copies_of`, and the test `search_uses_the_window_and_rejects_a_missing_one`.
- `crates/storage/src/reads/alerts.rs` `alert_value`, arm `AlertMeasure::Cost` (an aggregate with `exact_cost_sum` read through `f64_at`).
- `crates/storage/src/reads/columns.rs` (`require`, `i64_at`, `f64_at`, `internal`).
- `crates/server/src/routes/traces.rs` (`routes`, `list_traces`, the test module) and `crates/server/src/routes/read.rs` (`check_search`, `list_scope`, `Deadline`, `open_query`).
- `crates/server/src/api/traces.rs` (`TraceListParams`, `TraceDetail`).
- `crates/server/src/api/openapi.rs`: `trace_paths`, and the component set around the line `set.insert::<TraceDetail>();`.
- `crates/server/src/api_examples.rs` `generate`, the `list_traces` examples.
- `web/src/routes/$org/$project/traces/index.tsx` and its test file; `web/src/api/queries.ts` (`traceDetailQuery`); `web/src/lib/format.ts` (`formatCost`); `web/src/components/data-table/data-table.tsx` (row checkbox `aria-label="Select row"`).

## What it is, and what it is not

When done:
- `GET /traces/totals?filter=…&search=…&from=…&to=…&include_eval=…&include_experiments=…` answers `{"trace_count": 203, "error_trace_count": 3, "cost_total": 4.12}` for the same rows `GET /traces` pages through.
- Under the traces table, a line reads `203 traces · 3 errors · $4.12`; with two rows checked, `2 of 203 selected · 1 error · $0.0027`.
- OpenAPI, the generated web schema, and the mock fixtures (`fixtures/api/responses/trace-totals.json`) include the endpoint.
- Spec 05, 09, 11, the decision, the CHANGELOG and the docs page describe it.

Out of scope:
- Totals on the Spans, Sessions or Users lists (none planned).
- Live updates of the totals: traces that live mode adds after the window's end are not counted until Refresh (spec 11).
- Token totals or other measures (the decision names three figures).
- Screenshots of the docs site (the website session recaptures them).

## Decisions this plan relies on

| ID | Summary |
|---|---|
| D-C-30 | Totals line; `GET /traces/totals` `{trace_count, error_trace_count, cost_total}`; selection summed in the browser |
| D-A2-4 | Costs are summed as `Decimal128(38, 18)` and returned as `f64` (`exact_cost_sum`) |
| D-C-16 | The histogram counts errors as `status = 'error'`; the totals use the same test |
| D-G-39 | `search` is at most 1,024 bytes (`check_search`) |

User's answer (2026-11-25): a new summary endpoint, rather than `search` on `POST /dashboards/query` or no server change.

## Versions

No new dependency.

## Conventions for every phase

- Work in `/home/sebastien/travaux/llm-observer/.claude/worktrees/ecstatic-gauss-a3941d` on branch `claude/trace-list-totals`.
- **Do not build, test, lint or run `just` in a phase** (the user's machine load rule). The coordinator compiles, regenerates and runs the checks once at the end. "Done when" below is therefore: the files are edited as written, and the commit is made.
- Follow the plan exactly. If a step is unclear, contradicts the code, or cannot be done as written, stop and report the question instead of improvising.
- Edit only the files the phase's **Edit** list names.
- No `unwrap()`/`expect()` outside tests. Error messages never contain filter or search text.
- Commit once, with the phase's subject, no attribution lines.
- Report: what was done, any deviation and why, anything left undone.

## Phase graph

```
P1a (storage) ──► P1b (server) ──► [coordinator: compile, just openapi, just api-examples, commit] ──► P2 (web) ──► P3 (docs, changelog)
```

P2 needs the regenerated `web/src/api/schema.gen.ts` and `fixtures/api/responses/trace-totals.json`. P3 touches only docs files; it runs after P2 so it can describe the final UI text, and could run in parallel with P2 (disjoint files) if the coordinator wants.

| Phase | Files |
|---|---|
| P1a | `crates/storage/src/reads/traces.rs`, `crates/storage/src/reads/mod.rs`, `crates/storage/src/lib.rs` |
| P1b | `crates/server/src/api/traces.rs`, `crates/server/src/routes/traces.rs`, `crates/server/src/api/openapi.rs`, `crates/server/src/api_examples.rs`, `crates/server/tests/openapi.rs`, `crates/server/tests/api_examples.rs` |
| coordinator | `crates/server/openapi.json`, `web/src/api/schema.gen.ts`, `fixtures/api/manifest.json`, `fixtures/api/responses/trace-totals.json` (generated) |
| P2 | `web/src/api/queries.ts`, `web/src/components/trace/trace-totals.tsx` (new), `web/src/components/trace/trace-totals.test.tsx` (new), `web/src/routes/$org/$project/traces/index.tsx`, `web/src/routes/$org/$project/traces/index.test.tsx`, `web/src/mocks/generated/handlers.ts`, `web/src/mocks/generated/handlers.test.ts` |
| P3 | `website/src/content/docs/docs/tracing/traces.mdx`, `CHANGELOG.md` |

---

## P1a and P1b — Storage read and API endpoint

P1a does the storage part: § Contracts "Storage", steps 1–6, the storage tests. Its **Edit:** `crates/storage/src/reads/traces.rs`, `crates/storage/src/reads/mod.rs`, `crates/storage/src/lib.rs`. **Commit:** `Add the trace_totals storage read`.

P1b does the server part: § Contracts "Server", steps 7–11, the server tests. Its **Edit:** `crates/server/src/api/traces.rs`, `crates/server/src/routes/traces.rs`, `crates/server/src/api/openapi.rs`, `crates/server/src/api_examples.rs`, `crates/server/tests/openapi.rs`, `crates/server/tests/api_examples.rs`. **Commit:** `Add GET /traces/totals for the trace list totals`.

**Goal:** `GET /api/v1/projects/{project}/traces/totals` answers the trace count, error trace count and exact cost sum of the trace list's rows.

### Contracts

Storage, in `crates/storage/src/reads/traces.rs` (after `MatchCountRequest`):

```rust
/// The trace list's rows to total (D-C-30): the list's filter, search, window and exclusions,
/// without sort, cursor or limit.
#[derive(Debug, Clone, Copy)]
pub struct TraceTotalsRequest<'a> {
    /// Filter text. Empty or whitespace means no filter.
    pub filter: Option<&'a str>,
    /// Search text. Empty or whitespace means no search.
    pub search: Option<&'a str>,
    /// Window and default exclusions.
    pub scope: ListScope,
    /// Time allowed for planning and running.
    pub timeout: Duration,
}

/// Totals of the trace list's rows (D-C-30).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TraceTotals {
    /// Traces the list would page through.
    pub trace_count: i64,
    /// Of those, the traces whose `status` is `error`.
    pub error_trace_count: i64,
    /// Exact sum of their `cost_total` (D-A2-4); a null cost adds nothing; 0 for no trace.
    pub cost_total: f64,
}
```

```rust
/// Totals of the rows [`trace_list`] would page through for the same filter, search and scope.
///
/// # Errors
///
/// [`RunError::Invalid`] for filter text; [`RunError::InvalidRequest`] for a search without a
/// window; [`RunError::Timeout`] past `timeout`; DataFusion failures through [`classify`] and
/// `ctx.fail` (a non-finite or out-of-range cost fails the sum, as in the widget query).
pub async fn trace_totals(
    ctx: &QueryContext,
    request: TraceTotalsRequest<'_>,
) -> Result<TraceTotals, RunError>
```

Column aliases (private constants in `traces.rs`):

```rust
const TOTAL_TRACES: &str = "trace_count";
const TOTAL_ERRORS: &str = "error_trace_count";
const TOTAL_COST: &str = "cost_total";
```

Server, in `crates/server/src/api/traces.rs` (after `TraceListParams`):

```rust
/// Query parameters of `GET /api/v1/projects/{project}/traces/totals`.
///
/// Spec 09 § Observability data.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, IntoParams)]
#[serde(deny_unknown_fields)]
pub struct TraceTotalsParams {
    /// Filter DSL expression over the `traces` view (spec 05).
    #[serde(default)]
    pub filter: Option<String>,
    /// Full-text search (spec 05 § Full-text search). At most 1,024 bytes (D-G-39).
    #[serde(default)]
    pub search: Option<String>,
    /// Inclusive range start. RFC 3339 or epoch milliseconds.
    #[serde(default)]
    pub from: Option<TimeParam>,
    /// Exclusive range end. RFC 3339 or epoch milliseconds.
    #[serde(default)]
    pub to: Option<TimeParam>,
    /// Include traces from the reserved environments (`cetra-eval`, `cetra-playground`, `cetra-assistant`). Default false. Spec 05 § Default exclusions.
    #[serde(default)]
    pub include_eval: bool,
    /// Include experiment traces. Default false. Spec 05 § Default exclusions.
    #[serde(default)]
    pub include_experiments: bool,
}

/// Totals of the traces a trace list matches (D-C-30).
///
/// Spec 09 § Observability data.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct TraceTotals {
    /// Traces the list would page through.
    pub trace_count: i64,
    /// Of those, traces with status `error`.
    pub error_trace_count: i64,
    /// Sum of their `cost_total` in USD; 0 when none has a cost.
    pub cost_total: f64,
}
```

(`TimeParam` is already imported in that file for `TraceListParams`; reuse the same imports.)

Route: `GET /api/v1/projects/{project}/traces/totals`, `Access::Project { perm: ProjectPerm::View, session_only: false }`, `documented: true`, handler `get(trace_totals)`.

OpenAPI operation: id `get_trace_totals`, tag `traces`, summary `Get trace list totals`, description `EXCLUSIONS` followed by ` Counts and sums every trace the list matches, not one page.` (build it with `format!("{EXCLUSIONS} Counts and sums every trace the list matches, not one page.")` as `stream` does), parameters `with_query::<TraceTotalsParams>(vec![project_param()])`, response `"200"` `json_of::<TraceTotals>("Totals of the matching traces.")` with `READ_ERRORS`. Path `format!("{PROJECT}/traces/totals")`.

### Steps

Storage (`crates/storage/src/reads/traces.rs`):
1. Add `TraceTotalsRequest`, `TraceTotals` and the three alias constants as above.
2. Add `pub async fn trace_totals` as a sibling of `match_count`:
   1. Parse the filter first: `let filter = match present(request.filter) { Some(text) => Some(ctx.parse_filter(FilterView::Traces, text)?), None => None };`.
   2. Run `totals_of(ctx, &request, filter)` under `tokio::time::timeout(request.timeout, …)`; an elapsed timeout is `Err(RunError::Timeout)`, as `match_count` does.
3. Add the private `async fn totals_of(ctx: &QueryContext, request: &TraceTotalsRequest<'_>, filter: Option<Filter>) -> Result<TraceTotals, RunError>`:
   1. `let frame = scope_frame(ctx, FilterView::Traces, LIST, filter, request.search, request.scope, false).await?;`
   2. Aggregate with no group keys: `frame.aggregate(vec![], vec![count(lit(1_i64)).alias(TOTAL_TRACES), count(when(col(traces::STATUS).eq(lit(SpanStatus::Error.as_str())), lit(1_i64)).end().map_err(classify)?).alias(TOTAL_ERRORS), exact_cost_sum(col(traces::COST_TOTAL)).alias(TOTAL_COST)]).map_err(classify)?`. `count` of a `CASE` without `ELSE` counts only the error rows and is never null, so `i64_at` can read it.
   3. `.collect().await.map_err(|error| ctx.fail(error))?`.
   4. `let batch = single_row(&batches)?.ok_or_else(|| RunError::Internal(DataFusionError::Internal("trace totals returned no row".to_owned())))?;` (`single_row` is the existing private fn of this file).
   5. Read `trace_count = i64_at(require(&batch, TOTAL_TRACES)?, 0)?`, `error_trace_count = i64_at(require(&batch, TOTAL_ERRORS)?, 0)?`, `cost_total = f64_at(require(&batch, TOTAL_COST)?, 0)?.unwrap_or(0.0)` (the sum is null when no row has a cost).
4. Imports to add in `traces.rs`: `use datafusion::functions_aggregate::expr_fn::count;`, `use datafusion::logical_expr::when;` (or extend the existing `datafusion::logical_expr::{…}` import), `use super::columns::{f64_at, i64_at, require};`, `use crate::views::exact_cost_sum;` (extend the existing `crate::views::{…}` import). `SpanStatus`, `traces`, `col`, `lit`, `Filter`, `FilterView`, `present`, `classify` are already imported.
5. `crates/storage/src/reads/mod.rs`: add `TraceTotals`, `TraceTotalsRequest`, `trace_totals` to the `pub use traces::{…}` list (alphabetical, as rustfmt orders it).
6. `crates/storage/src/lib.rs`: add the same three names to the `pub use reads::{…}` list.

Server:
7. `crates/server/src/api/traces.rs`: add `TraceTotalsParams` and `TraceTotals` as above.
8. `crates/server/src/routes/traces.rs`:
   1. Add the `RouteDef` for `/api/v1/projects/{project}/traces/totals` right after the `/traces` one.
   2. Import `TraceTotals` and `TraceTotalsParams` from `crate::api::traces`.
   3. Add the handler, in this order of operations:
      ```rust
      async fn trace_totals(
          State(state): State<AppState>,
          Extension(access): Extension<ProjectAccess>,
          error::Query(params): error::Query<TraceTotalsParams>,
      ) -> Result<error::Json<TraceTotals>, ApiError> {
          check_search(params.search.as_deref())?;
          let scope = list_scope(params.from, params.to, params.include_eval, params.include_experiments)?;
          let deadline = Deadline::start();
          let ctx = open_query(&state, access.project.org_id, access.project.id, None).await?;
          let totals = storage::trace_totals(
              &ctx,
              storage::TraceTotalsRequest {
                  filter: params.filter.as_deref(),
                  search: params.search.as_deref(),
                  scope,
                  timeout: deadline.remaining()?,
              },
          )
          .await?;
          Ok(error::Json(TraceTotals {
              trace_count: totals.trace_count,
              error_trace_count: totals.error_trace_count,
              cost_total: totals.cost_total,
          }))
      }
      ```
9. `crates/server/src/api/openapi.rs`:
   1. Add `TraceTotals, TraceTotalsParams` to the `use super::traces::{…}` import near the top (it lists `SpanDetail, …, TraceStreamParams, TraceSummary`).
   2. Add `set.insert::<TraceTotals>();` next to `set.insert::<TraceDetail>();`.
   3. In `trace_paths`, build the `totals` operation as in § Contracts and add `.path(format!("{PROJECT}/traces/totals"), PathItem::new(HttpMethod::Get, totals))` right after the `/traces` path.
10. `crates/server/src/api_examples.rs` `generate`: right after the `list_traces` example for `responses/traces.json`, add
    ```rust
    let totals: TraceTotals = fetch(&app, cookie, "/traces/totals").await;
    out.add("get_trace_totals", &[], &[], "responses/trace-totals.json", &totals);
    ```
    and add `TraceTotals` to the `crate::api::traces::{…}` import.
11. Integration tests that enumerate the operations and examples:
    1. `crates/server/tests/openapi.rs`: add `("get", "/api/v1/projects/{project}/traces/totals", "get_trace_totals")` to `OPERATIONS` (next to the `list_traces` entry); add `"get_trace_totals"` to `READ_OPERATIONS`; add `"get_trace_totals"` to `EXPECTED` in `assert_exclusions`.
    2. `crates/server/tests/api_examples.rs`: in `generated_files_roundtrip`, add the arm `"get_trace_totals" => roundtrip::<TraceTotals>(&example.file),` next to the `list_traces` arm, and add `TraceTotals` to the `use cetra_server::api::traces::{…}` import.

### Tests

Storage, in the `tests` module of `crates/storage/src/reads/traces.rs` (import `TraceTotalsRequest`, `trace_totals` from `super`). A local helper:

```rust
fn totals_request<'a>(scope: ListScope, filter: Option<&'a str>, search: Option<&'a str>) -> TraceTotalsRequest<'a> {
    TraceTotalsRequest { filter, search, scope, timeout: TIMEOUT }
}
```

- `totals_match_the_listed_rows`: with `context()`, for each case — (a) `scope(None, false)`, no filter, no search; (b) `scope(None, true)`; (c) `scope(None, false)` with filter `status = 'error'`; (d) `scope(None, false)` with filter `cost_total > 0.002`; (e) `scope(Some(day()), false)` with search `order` — call `trace_list` with the same scope, filter and search, `TraceSort::StartTime`, `SortOrder::Desc`, limit 1000 (build it with `listed` and set `filter`/`search`), and `trace_totals`. Assert `trace_count == rows(&page).len()`, `error_trace_count ==` the rows whose `"status"` is `"error"`, and `(cost_total - Σ row["cost_total"].as_f64().unwrap_or(0.0)).abs() < 1e-12`. Also assert case (a) has `trace_count > 0` and `error_trace_count > 0`, so the test is not vacuous.
- `totals_of_no_rows_are_zero`: filter `name = 'no such trace'` → `TraceTotals { trace_count: 0, error_trace_count: 0, cost_total: 0.0 }`.
- `totals_sum_costs_exactly`: spans `copies_of(t, 1, t as i64 * 1_000, 1)` for `t in 1..=10` (flattened), each with `cost_total = Some(0.1)`; set `status = SpanStatus::Error` on trace 3's span; `context_for(&spans)`; `scope(None, false)` → `trace_count == 10`, `error_trace_count == 1`, `cost_total == 1.0` (exact `assert_eq!`). If `span_row` gives these spans an environment or kind that the default exclusions drop, stop and report.
- `totals_reject_a_bad_filter_and_a_search_without_window`: filter `error_count >` → `Err(RunError::Invalid(_))`; `scope(None, false)` with search `order` → `Err(RunError::InvalidRequest(message))` with `message == "search needs a time range"`.

Server, in the `tests` module of `crates/server/src/routes/traces.rs`:
- `totals_cover_the_whole_list`: `loaded()`; for queries `""`, `"?include_eval=true"`, `"?filter=status%20%3D%20'error'"`: `GET /traces/totals{query}` → 200; `GET /traces{query}` with `limit=1000` added (`?limit=1000` or `&limit=1000`) → its `data`; assert `trace_count` equals `data.len()`, `error_trace_count` equals the items whose `status` is `"error"`, and `cost_total` equals the sum of `cost_total` (absent or null as 0) within `1e-12`.
- `totals_reject_bad_parameters`: `?filter=name%20%3D%20'x'%20)` → 400 with `code` `invalid_filter` and `errors[0].field == "filter"`; `?sort=cost_total` → 400 (unknown field); a `search` of 1,025 `a`s → 400 (as `search_is_at_most_1024_bytes` checks the list); `?from=2026-01-02T00:00:00Z&to=2026-01-01T00:00:00Z` → 400.

**Done when:** the phase's files are edited as above and committed. (The coordinator compiles, runs `cargo test -p cetra-storage --lib reads::traces::tests::totals` and `cargo test -p cetra-server --lib routes::traces::tests::totals`, then `just openapi` and `just api-examples`.)

**Commit:** see P1a and P1b above.

---

## Coordinator step — regenerate

After P1b (on ubuntu-dev, or locally when it is down): compile and run the P1 tests, then `just openapi`, `just api-examples`; check that `fixtures/api/responses/trace-totals.json` holds `{"trace_count": 5, "error_trace_count": 1, "cost_total": 0.00955}` (cost within rounding) (the 5 default-list traces of the example dataset, one in error) and that `web/src/api/schema.gen.ts` has the path `/api/v1/projects/{project}/traces/totals` and the schema `TraceTotals`. Commit `Regenerate the OpenAPI document and API examples for trace totals`.

---

## P2 — Totals line on the traces list

**Goal:** the traces list shows the totals line of spec 11 under its table.

**Edit:** `web/src/api/queries.ts`, `web/src/components/trace/trace-totals.tsx` (new), `web/src/components/trace/trace-totals.test.tsx` (new), `web/src/routes/$org/$project/traces/index.tsx`, `web/src/routes/$org/$project/traces/index.test.tsx`, `web/src/mocks/generated/handlers.ts`, `web/src/mocks/generated/handlers.test.ts`.

### Contracts

`web/src/api/queries.ts` (after `traceDetailQuery`):

```ts
export function traceTotalsQuery(
  org: string,
  project: string,
  query: {
    from: string;
    to: string;
    filter?: string;
    search?: string;
    includeEval?: boolean;
    includeExperiments?: boolean;
  },
) {
  return $api.queryOptions("get", "/api/v1/projects/{project}/traces/totals", {
    params: {
      path: { project: projectRef(org, project) },
      query: {
        from: query.from,
        to: query.to,
        ...(query.filter ? { filter: query.filter } : {}),
        ...(query.search ? { search: query.search } : {}),
        ...(query.includeEval ? { include_eval: true } : {}),
        ...(query.includeExperiments ? { include_experiments: true } : {}),
      },
    },
  });
}
```

`web/src/components/trace/trace-totals.tsx`:

```ts
import type { Schema } from "@/api/types";
import { formatCost } from "@/lib/format";

/** Trace count, error count and cost of a set of traces (D-C-30). */
export type Totals = { traceCount: number; errorTraceCount: number; costTotal: number };

/** The API's totals as `Totals`. */
export function totalsOf(data: Schema<"TraceTotals">): Totals;

/** Totals of the loaded rows whose id is in `ids`: errors are rows with status `error`, a missing cost adds 0. */
export function selectionTotals(
  rows: readonly Schema<"TraceSummary">[],
  ids: ReadonlySet<string>,
): Totals;

/** `1 trace`, `2 traces`, `1,204 traces`: en-US grouping, singular for exactly 1. */
export function countText(n: number, singular: string, plural: string): string;

type Props = {
  /** Totals of the checked rows; undefined when no row is checked. */
  selected: Totals | undefined;
  /** Totals of the whole list once loaded. */
  list: Totals | undefined;
  /** The list totals query's status. */
  listStatus: "pending" | "error" | "success";
};

export function TraceTotalsLine({ selected, list, listStatus }: Props) // no return type annotation (React 19 has no global JSX namespace)
```

(`countLabel` in `web/src/components/data-table/new-rows.tsx` means something else; do not reuse or rename it.)

Rendered markup (exact):

```tsx
<div
  role="status"
  aria-label="Totals"
  className="flex h-7 shrink-0 items-center border-t border-border px-3 text-caption text-muted-foreground"
>
  {/* content */}
</div>
```

Content, where `sep` is `<span aria-hidden="true">{" · "}</span>`, `errors(t)` is `<span className={t.errorTraceCount > 0 ? "text-destructive" : undefined}>{countText(t.errorTraceCount, "error", "errors")}</span>` and `cost(t)` is `<span className="num" title="Total cost">{formatCost(t.costTotal)}</span>`:
1. `selected !== undefined`: `<span>{list ? `${n(selected.traceCount)} of ${n(list.traceCount)} selected` : `${n(selected.traceCount)} selected`}</span>`, sep, `errors(selected)`, sep, `cost(selected)`; `n` formats with `toLocaleString("en-US")`.
2. else `listStatus === "success" && list`: `<span>{countText(list.traceCount, "trace", "traces")}</span>`, sep, `errors(list)`, sep, `cost(list)`.
3. else `listStatus === "error"`: `Totals unavailable`.
4. else: `Counting…` (the Unicode ellipsis, U+2026).

So the text content reads `203 traces · 3 errors · $4.12`, `2 of 203 selected · 1 error · $0.0027`, `Counting…`, `Totals unavailable`.

### Steps

1. Add `traceTotalsQuery` to `web/src/api/queries.ts`.
2. Create `trace-totals.tsx` with `totalsOf`, `selectionTotals`, `countText` and `TraceTotalsLine` as specified. `totalsOf` maps `trace_count`, `error_trace_count`, `cost_total`. `selectionTotals` iterates `rows` once, keeping rows with `ids.has(row.trace_id)`: count them, count `row.status === "error"`, sum `row.cost_total ?? 0`.
3. In `index.tsx`:
   1. Import `traceTotalsQuery` (extend the `@/api/queries` import) and `selectionTotals`, `totalsOf`, `TraceTotalsLine` from `@/components/trace/trace-totals`.
   2. Right after the `rows` `useMemo`, add `const selectedTotals = useMemo(() => (checkedIds.size > 0 ? selectionTotals(rows, checkedIds) : undefined), [rows, checkedIds]);`.
   3. Right after the `const onboarding = …` statement (before `const reason`), add
      ```ts
      const totals = useQuery({
        ...traceTotalsQuery(org, project, {
          from: resolved.from,
          to: resolved.to,
          filter: search.filter,
          search: search.search,
          includeEval: search.eval === 1,
          includeExperiments: search.exp === 1,
        }),
        ...LIST_CACHE,
        enabled: !hold && !filterError && !probing && !onboarding && rows.length > 0,
      });
      ```
      (`useQuery` and `LIST_CACHE` are already imported.)
   4. In the JSX branch that renders `<TracesHistogram …/>` and `<DataTable …/>`, add after `<DataTable …/>`, inside the same fragment:
      ```tsx
      {rows.length > 0 ? (
        <TraceTotalsLine
          selected={selectedTotals}
          list={totals.data ? totalsOf(totals.data) : undefined}
          listStatus={totals.status}
        />
      ) : null}
      ```
   No other change to `index.tsx`.
4. `web/src/mocks/generated/handlers.ts` (the large generated dataset's handlers, used by some tests and the dev server): add a function after `listTraces`:
   ```ts
   function traceTotals(dataset: Dataset, request: Request): Response {
     const parsed = readQuery(new URL(request.url));
     if (parsed instanceof Response) {
       return parsed;
     }
     const predicate = parsed.filter ? tracePredicate(parsed.filter) : undefined;
     const rows = filterTraces(dataset.traces, parsed, predicate);
     return HttpResponse.json({
       trace_count: rows.length,
       error_trace_count: rows.filter((trace) => trace.status === "error").length,
       cost_total: rows.reduce((sum, trace) => sum + (trace.cost_total ?? 0), 0),
     });
   }
   ```
   and register `http.get(`${PROJECT}/traces/totals`, ({ request }) => traceTotals(dataset, request)),` right after the `${PROJECT}/traces` handler and before `${PROJECT}/traces/:trace_id/spans/:span_id`. If `readQuery` rejects a request without `limit` or `sort`, stop and report.

### Tests

`web/src/components/trace/trace-totals.test.tsx` (render with `render` from `@testing-library/react`; read the line with `screen.getByRole("status", { name: "Totals" })` and compare `textContent`):
- `shows the list totals`: `list = {traceCount: 203, errorTraceCount: 3, costTotal: 4.12}`, `listStatus "success"`, no selection → `203 traces · 3 errors · $4.12`; the `3 errors` span has class `text-destructive`.
- `uses the singular and groups thousands`: `{1, 1, 0}` → `1 trace · 1 error · $0`; `{1204, 0, 12.5}` → `1,204 traces · 0 errors · $12.50`, and the `0 errors` span has no `text-destructive` class.
- `shows counting and unavailable`: `listStatus "pending"`, `list undefined` → `Counting…`; `listStatus "error"` → `Totals unavailable`.
- `shows the selection against the list`: `selected {2, 1, 0.0027}`, `list {203, 3, 4.12}` → `2 of 203 selected · 1 error · $0.0027`; with `list undefined` and `listStatus "pending"` → `2 selected · 1 error · $0.0027`.
- `selectionTotals sums the checked rows`: `import tracesFixture from "../../../../fixtures/api/responses/traces.json";` and `const rows = tracesFixture.data as Schema<"TraceSummary">[];` (the cast is needed: JSON infers `status: string`). The fixture's order is …6 (`unset`, cost 0.0027), …4 (`ok`, 0.00133), …3 (`error`, no cost), …2 (`ok`, 0.00246), …1 (`ok`, 0.00306). `ids = new Set(["a1000000000000000000000000000006", "a1000000000000000000000000000003", "a10000000000000000000000000000ff"])` (the last is in no row) → `{traceCount: 2, errorTraceCount: 1, costTotal: 0.0027}`; an empty set → `{0, 0, 0}`.

`web/src/routes/$org/$project/traces/index.test.tsx` (new tests at the end of the file; follow the file's `renderApp` and `server.use` pattern):
- `shows the list totals under the table`: `server.use(http.get("/api/v1/projects/:project/traces/totals", () => HttpResponse.json({ trace_count: 203, error_trace_count: 3, cost_total: 4.12 })))`; `const { urls, stop } = recordUrls();` `renderApp("/acme/support-bot/traces?filter=status%20%3D%20'error'&search=order")`; await the line's text `203 traces · 3 errors · $4.12`; then the recorded URL whose `pathname` ends with `/traces/totals` has `filter` `status = 'error'`, `search` `order`, and the same `from` and `to` as the recorded URL whose `pathname` ends with `/traces` (the list); match on the path suffix, since the project ref is encoded; `stop()`.
- `totals the checked rows`: same totals handler; `const { user } = await renderApp("/acme/support-bot/traces")`; `await user.click(await rowBox(0))` and `await user.click(await rowBox(2))` (the file's existing `rowBox` helper; traces …6 and …3 in the fixture's order) → line text `2 of 203 selected · 1 error · $0.0027`; unchecking both returns to `203 traces · 3 errors · $4.12`.
- `says when the totals are unavailable`: the totals handler (same path) returns `problemResponse` 500 (as other tests of the file use `problemResponse`) → `Totals unavailable`, and the table still renders.
- `shows no totals for an empty list`: in the existing test `shows the plain empty state when the default filter matches nothing`, add at its end `expect(screen.queryByRole("status", { name: "Totals" })).not.toBeInTheDocument();`.

Tests that do not override `/traces/totals` get the generated fixture (served for any query) or, with `largeHandlers`, the new generated handler.

`web/src/mocks/generated/handlers.test.ts`, new test `totals the traces the list matches`: `createDataset({ seed: 1, now: NOW, traces: 400 })`, `server.use(...largeHandlers(dataset))`; fetch `${PROJECT}/traces?limit=1000` and `${PROJECT}/traces/totals`; assert `trace_count` equals the list's length, `error_trace_count` its rows with `status === "error"`, and `cost_total` is within `1e-9` of the sum of `cost_total ?? 0`; then the same with `include_eval=true` on both.

**Done when:** the files are edited as above and committed.

**Commit:** `Show totals under the traces list`

---

## P3 — Docs and changelog

**Goal:** the docs page and the changelog describe the totals line and the endpoint.

**Edit:** `website/src/content/docs/docs/tracing/traces.mdx`, `CHANGELOG.md`.

### Steps

1. `traces.mdx` § The trace list, after the bullet list of § The toolbar and before the paragraph "Tick the box of one or more rows…", add a paragraph:
   > The line under the table totals the list: the number of traces, how many failed and their cost, over every trace that matches the time range, filter and search, not only the rows loaded so far. Tick rows to total those instead; the line then reads, for example, "3 of 203 selected · 1 error · $0.42". Traces that arrive in live mode are counted after **Refresh**.
2. `CHANGELOG.md` § `[Unreleased]` › `### Added`, append a line at the end of that list:
   > - The trace list shows the trace count, error count and cost of the checked rows, or of every trace the list matches, under its table; `GET /api/v1/projects/{project}/traces/totals` answers the same totals for a filter, search and time range.

Follow `website/STYLE.md` (no new banned terms; the words above are plain).

**Done when:** the two files are edited and committed.

**Commit:** `Document the trace list totals`

---

## Final checks (coordinator)

1. Sync the branch to ubuntu-dev and run `just check` there (with Node on the PATH), plus `just docs-gen` if `check-docs` asks for it.
2. Sonnet code review of the branch against `origin/main`; fixes by an implementer with an exact brief.
3. Open the PR; update `docs/roadmap.md` only if a Track C row lists UI follow-ups this closes (none expected).

## Changes after the code review and checks

- The line is a `<section aria-label="Totals">` (a named region), not `role="status"`: the layout tests assert no status region on the page, a live region would announce every checkbox change, and Biome's `useSemanticElements` refuses `role="group"` on a `div`.
- `selectionTotals` counts every checked id (`ids.size`), so the line agrees with "Add N to dataset"; errors and cost still come from the loaded rows among them.
- While the table shows the previous filter's placeholder rows, the line reads `Counting…`.
- `trace-view.test.tsx` overrides `/traces/:trace_id` while rendering the list, which also matched `/traces/totals`; the test now answers the totals first.
- Spec 11 and the docs say the totals are fetched once per window and refresh with Refresh or a change of range, filter, search or flags.
