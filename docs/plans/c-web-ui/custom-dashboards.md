# Custom dashboards

Track C (web UI), with storage and server work. Plan for 8 phases (CD1–CD8), each one commit. Do one phase, nothing more. Decisions: D-C-16, D-C-17 (CD8). Specs, already updated: 04 § Dashboards, 05 § Dashboard queries, 08 (role table), 09 (endpoints), 11 § Dashboard.

## Goal

Members build their own dashboards. A dashboard is a free 12-column grid of widgets. A widget is a builder query drawn as a line, bar, area, horizontal bar, number or table, or a heading. The built-in Overview becomes a preset of builder widgets. When every chart reads the new widget query, `GET /metrics` is removed.

## Decisions (made by the user, do not revisit)

1. Several named dashboards per project, shared with every member. Viewers read them, members edit them, and API keys only read.
2. Widgets are builder queries (source, measures, breakdown, filter), not SQL. The Overview is a preset of builder widgets.
3. Chart types: line, bar, area (bar and area can be stacked), horizontal bar, number, table. No pie or donut.
4. The layout is a free grid with drag and resize, using `react-grid-layout`.
5. The dashboard's range, filter, group and Include menu apply to every widget. A widget's filter adds to them.
6. Out of scope: SQL widgets, dashboard variables, alerts from a widget, per-plan limits, MCP tools.

## Rules for every phase

- Read AGENTS.md. No compatibility layers, no placeholders, no unneeded traits, generics or wrappers.
- **Do not guess.** If the code, the spec or this plan disagree, or something here is not true of the code, stop and end your final message with one line starting `QUESTION:`.
- Specs are the source of truth. Change them only where this plan says.
- Work in the worktree you were given. Do not merge other branches.
- While iterating, run targeted tests only:
  - Rust: `cargo test -p <crate> <filter>`.
  - Web: `pnpm exec vitest run <paths> --maxWorkers=8`, then delete `web/.vitest`.
- Before the commit, run the phase's full check. Rust phases run `just check-rust`, web phases `just check-web`, and CD6 and CD7 run `just check`.
- Before any Node command: `export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH`.
- Format web files with `node_modules/.bin/biome check --write <files>`, run from the repo root.
- Commits: one per phase, short imperative subject, no attribution line.
- Report the files you changed, any deviation and the test results.

## Facts about the current code (verified)

**Storage**
- `crates/storage/src/reads/metrics.rs` (2.8k lines) computes the fixed dashboard. Pieces to reuse:
  - the bucket helpers (`BucketWidth`, `Bucket`, `resolve`, `bucket_count`, `bucket_grid`, `align_down`, `AUTO_WIDTHS`, the limits 120 and 2,000);
  - `trace_ids`, the semi-join plan over traces in `[from − 1 day, to)`;
  - `scoped_traces` and `present` from `reads/mod.rs`, and `exact_cost_sum` from `views/sessions.rs`;
  - `approx_percentile_cont` and `date_bin`.
- Filters: `crate::filter::{Filter, FilterView}`. Errors: `crate::query_error::{RunError, classify}`. `RunError::Timeout` is the timeout.
- `cetra_core::schema::{traces, spans, scores}` hold the column-name constants. Traces have every token column (input, output, cache_read, cache_write, reasoning, total).

**Server**
- `crates/server/src/routes/metrics.rs` holds `GET /metrics` and `MetricsCache`, a 10 s cache with shared in-flight queries keyed by project and org data window, which `clear_project` empties (D-M-11).
- `crates/server/src/api/metrics.rs` has the API types, and `crates/server/src/api/openapi.rs` registers them.
- Saved views are the pattern for a project-scoped CRUD resource with a JSON body:
  - `crates/server/src/meta/saved_views.rs`
  - `crates/server/src/routes/saved_views.rs`
  - `crates/server/src/api/saved_views.rs`
  - `crates/server/migrations/meta/0003_saved_views.sql`
- The newest migration is `0020_evaluator_versions.sql`. Another session may be adding `0021` (score direction), so check the folder on your branch and take the next free number.
- `Access::reads_by_post` in `crates/server/src/http/access.rs` lets a read-only PAT and a `read` key call a POST that reads, as `POST /sql` does.
- The schema snapshots are `crates/server/tests/snapshots/meta_schema.snap` and `crates/storage/src/catalog/schema.txt`. Only the first one changes here.

**Web**
- `GET /metrics` is read by:
  - the dashboard (`components/dashboard/dashboard-view.tsx`, `components/charts/{metric-chart,kpi-tiles,top-models}.tsx`);
  - `components/trace/traces-histogram.tsx`;
  - `components/user/user-overview.tsx`;
  - `components/alerts/{rule-preview,rule-trend}.tsx`.
- Helpers in `web/src/lib/metrics.ts`:
  - `groupColorIndex`
  - `kpiDelta`
  - `seriesByMetric`
  - `toChartRows`
  - `epochMs`
  - `bucketRange`
- Scales: `web/src/lib/chart-scale.ts` (`niceTicks`, `durationTicks`, `timeTicks`, `chooseBucket`). Zoom: `web/src/components/charts/use-drag-zoom.ts`.
- `web/src/api/queries.ts` has `metricsQuery`. The API types are generated into `web/src/api/schema.gen.ts` by `just openapi`.
- Mocks:
  - `web/src/mocks/generated/dataset.ts` builds the mock data, with buckets aligned to the epoch.
  - The MSW handlers are under `web/src/mocks/`.
  - Example answers are in `fixtures/api/`.
- `react-grid-layout` **2.3.0** (2026-10-05) is past `minimumReleaseAge` and ships its own types. It is not installed yet.

---

## CD1 — Widget query engine (storage)

**Edit**
- New `crates/storage/src/reads/buckets.rs`: move the bucket helpers out of `metrics.rs`, which then imports them.
- New `crates/storage/src/reads/widgets.rs` with its tests. Register it in `reads/mod.rs` and re-export from the crate root where `metrics` is.

**Steps**
1. Types, following spec 05 § Dashboard queries:
   - `WidgetSource` (Traces, Spans, Scores)
   - `Agg` (Count, CountDistinct, Sum, Avg, Min, Max, P50, P75, P90, P95, P99, Rate)
   - `Measure { agg, field: Option<String>, filter: Option<String> }` (the `where`)
   - `Breakdown { field: String, limit: usize, other: bool }`
   - `WidgetQuery { source, filter: Option<String>, measures: Vec<Measure>, breakdown: Option<Breakdown> }`
   - `Shape` (Series, Total)
   - `WidgetRequest<'a>` with: project scope (as `MetricsRequest` has it), `window: TimeWindow`, `bucket: Bucket`, dashboard `filter`, `group_by: Option<MetricsGroupBy>`, include flags, `query`, `shape`, `compare`, `timeout`.
2. Field validation, as a pure function: `validate(&WidgetQuery) -> Result<(), Vec<FieldError>>`.
   - It uses the field table of spec 05 and the measure rules (1–6 measures; `rate` needs `where`; `count` and `rate` take no field; numeric fields for sum, avg, min, max and the percentiles; `count_distinct` fields; map keys of 1–256 characters; `limit` 1–20).
   - Each `FieldError` has a path such as `query.measures[1].field` and a message.
   - The server and the CD3 dashboard validation both call this function.
3. Planning:
   - Base rows: the source view, the window on its time column (`created_at` for scores), the default exclusions, and the widget filter.
   - For spans and scores, add the dashboard filter through the `trace_ids` semi-join. Traces take it directly.
   - The environment test for the exclusions looks at all three expressions.
   - Scores: live rows, `target <> 'session'`, as the current score series does (copy its predicate).
4. Measures. One aggregate expression per measure:
   - `count`: `count(*)`, or a count of the `where` matches.
   - `rate`: matches over `count(*)`, as f64. Null when there are no rows.
   - `sum`: a sum, except `cost_total`, `cost_input` and `cost_output`, which use the exact Decimal path as `exact_cost_sum` does.
   - Percentiles: `approx_percentile_cont`.
   - `count_distinct`: a distinct count.
   - A measure with `where` aggregates over `CASE WHEN where THEN field END`.
5. Breakdown (or `group_by` when there is no breakdown and the source is not scores):
   - Unnest list fields, as `group_by=model` does today over `models`.
   - Pass 1: the top `limit` values by the first measure over the whole window, nulls last, then by value ascending.
   - Pass 2: group by bucket (Series) or not (Total), only for those values.
   - Pass 3 (with `other`): the same aggregate over the rows whose value is not in the top set (after unnest).
   - `group_by` maps to a breakdown with `limit: 20, other: false`, on `models` for traces and `model` for spans.
6. Shapes:
   - Series uses the bucket grid, with `rows = count(*)` per point. A bucket with no rows gets no point, and a null aggregate gives a point without a value.
   - Total gives one row per group. `compare` runs the same query over the previous range of the same length.
7. Output types:
   - `WidgetData::Series { bucket_ns, series: Vec<WidgetSeries { measure, group, other, points: Vec<WidgetPoint { time, value: Option<f64>, rows: u64 }> }> }`
   - `WidgetData::Total { rows: Vec<TotalRow { group, other, values, previous: Option<Vec<Option<f64>>> }> }`
8. The whole query runs under `tokio::time::timeout(timeout)`, as `metrics` does. Invalid DSL comes back as `RunError::Invalid`, with the expression's path in the error so the server can name it. **If `RunError` cannot carry a path, stop and ask.**

**Tests** (in `widgets.rs`, using the test support that `metrics.rs`'s tests use)
- For each source, a count series over a known fixture equals the counts by hand.
- `rate` of `status = 'error'` equals errors over traces.
- p95 of `duration_ns` matches the current `latency_p95` series on the same fixture. **Parity tests against `metrics()`** cover traces, errors, error rate, latency p50/p95/p99, tokens, TTFT p50/p95, LLM latency by model, cost by model, score averages, eval cost and top models.
- Breakdown: the top-N order, the tie order, the `other` group computed over the remaining rows (a percentile test proves it is not a sum), and the unnest of `tags` and `models`.
- `group_by`: it is ignored when there is a breakdown, and for scores.
- The dashboard filter reaches spans and scores through the semi-join.
- A widget filter naming `environment` keeps `cetra-eval` traces.
- Decimal cost sums: a sum of 0.1 ten times gives exactly 1.
- `compare` covers the previous range.
- Empty buckets give no point.
- `validate`: one test per rule, with the exact paths.
- A timeout gives `RunError::Timeout`.

**Done when** `just check-rust` passes and `metrics.rs` behaves exactly as before.

---

## CD2 — `POST /dashboards/query` (server)

**Edit** `crates/server/src/api/` (new `dashboards.rs`), `crates/server/src/routes/` (new `dashboards.rs`, registered in `routes/mod.rs`), `routes/metrics.rs` (the cache), `api/openapi.rs`, `http/access.rs`, `fixtures/api/`, `crates/server/openapi.json` and `web/src/api/schema.gen.ts` (both through `just openapi`).

**Steps**
1. API types follow spec 05 exactly, in snake_case JSON (`where` is the measure's key):
   - request: `WidgetQueryRequest`;
   - answers: `WidgetSeriesResponse` and `WidgetTotalResponse`, as one `WidgetQueryResponse` enum tagged by `shape`;
   - each type has `utoipa` schemas.
2. Make `MetricsCache` generic over the key and the answer, so the same cache (10 s, shared in-flight queries, `clear_project`) also serves the widget query. The key is `(project, data window days, canonical JSON of the body)`. Keep one cache instance per endpoint in the app state. **CD6 removes the metrics instance; if it then has only one user, make it concrete again.**
3. Route `POST /api/v1/projects/{project}/dashboards/query`:
   - Project read access. Add this path to `reads_by_post`.
   - Call `storage::widgets::validate`: a failure is `400 bad_request` with `errors[{field: path, message}]`.
   - Run the query. Invalid DSL is `400 invalid_filter`, with the path in `errors[].field` and its position.
   - Map timeout, busy and query failures as `get_metrics` does.
   - Apply the org's data window (D-P-21) the way `get_metrics` does.
4. Add `fixtures/api/` example answers for one series and one total, checked against the types like the other fixtures. Then run `just openapi`.

**Tests** (in the route module, following `routes/metrics.rs`'s tests)
- 200 for series and for total.
- 400 with the exact path for a bad field, a bad `where` and a bad dashboard filter.
- A read-only PAT and a `read` key may call it, and a key of another project gets 404 or 403 as the other reads do.
- Cache: the same body is computed once, and `clear_project` empties it.
- The data window clamps the range.

**Done when** `just check-rust` passes, the fixtures check passes, and the OpenAPI files are regenerated.

---

## CD3 — Dashboards CRUD (server)

**Edit**
- the new migration `crates/server/migrations/meta/00NN_dashboards.sql`;
- `meta/dashboards.rs`;
- `routes/dashboards.rs`;
- `api/dashboards.rs`;
- `openapi.rs`;
- `fixtures/api/`;
- the `meta_schema.snap` snapshot;
- the OpenAPI files.

**Steps**
1. The migration creates the table of spec 04 § Dashboards exactly.
2. `meta/dashboards.rs` provides `list` (by name then id, with a cursor), `get`, `create`, `update` (compare-and-set on `version`) and `delete`. Follow `saved_views.rs`. Creating the 101st dashboard of a project is refused.
3. Validate the definition in one function:
   - serde types `Definition { widgets: Vec<Widget> }`, `Widget`, `Layout` and `Chart`, following spec 04;
   - every rule of spec 04, including unique ids and the chart and shape rules;
   - `storage::widgets::validate` for each query, with paths prefixed `definition.widgets[i].query.`;
   - the measure `label` (0–60 characters);
   - the 256 KiB limit on the serialized definition.
   - It returns every error at once.
4. Routes follow spec 09. Access:
   - `GET`: project read, keys included.
   - `POST`, `PATCH`, `DELETE`: project member and above, through a session or a read-write PAT. API keys are 403.
   - A wrong `version` is `409 conflict`, with the stored version in `detail`.
   - `PATCH` raises `version` and sets `updated_by` and `updated_at`.
   - Deleting a project cascades to its dashboards.
5. Add `fixtures/api/` examples for the list, one dashboard and a 409. Then run `just openapi`.

**Tests**
- CRUD round trip.
- Pages and the cursor.
- Every validation rule, with exact paths.
- The 101st dashboard is refused.
- 409 on a stale version.
- Roles: a viewer reads but cannot write; a member writes; a `read` key reads; an API key cannot write.
- The cascade on project deletion.
- The schema snapshot is updated.

**Done when** `just check-rust` passes.

---

## CD4 — Widget renderer and the Overview preset (web)

**Edit**
- `web/src/api/queries.ts`: add `widgetQuery`.
- New `web/src/lib/dashboards/` with `types.ts` (the definition types, taken from the generated schema), `overview.ts` (the preset) and `format.ts` (value format by field).
- New `web/src/components/dashboard/widget.tsx` (one widget: it fetches and draws) and `widget-charts.tsx`.
- `web/src/components/dashboard/dashboard-view.tsx`.
- The MSW handler for `POST /dashboards/query` under `web/src/mocks/`, computed from the generated dataset.
- Tests.

**Steps**
1. `widgetQuery(org, project, body)` uses `$api.queryOptions("post", ".../dashboards/query")`. The bucket is `chooseBucket(from, to)` as today, and `shape` comes from the chart type (spec 04).
2. `format.ts`:
   - `duration_ns` and `ttft_ns` are durations;
   - `cost_*` are USD;
   - `rate` is a percentage;
   - everything else is a count, except `value_num`, which is a number with 2 decimals.
   - The axis ticks use the existing `chart-scale` functions as `metric-chart.tsx` does today.
3. Chart components, moved and generalized from `metric-chart.tsx`, `kpi-tiles.tsx` and `top-models.tsx`:
   - **line, bar and area**: a category X axis with `xAxisTicks`, the same Y ticks rules, `stacked`, `split` (small multiples, at most 6), `fade_below` (using the point's `rows`);
   - **bar_h**;
   - **number**: the value, the `kpiDelta` badge, the "Previous period" tooltip, and a sparkline from a second, series query;
   - **table**: with `share`;
   - **heading**.
   - Colours: an error measure (`where` exactly `status = 'error'`) uses `var(--chart-error)`, and a breakdown on `model` or `models` uses `groupColorIndex`.
   - Keep `syncId="dashboard"`, the hovered-only tooltip, `useDragZoom`, the bucket click and the keyboard navigation of D-C-3.
   - The bucket click opens the traces or spans list with the dashboard filter, the widget filter and the group value. Only those two sources get click and zoom.
4. `overview.ts`: the Overview widgets of spec 11 § Dashboard, in that order, with a fixed layout:
   - the headings span the full width;
   - the number widgets take w=2 each (five of them, then a gap);
   - the charts take w=6 and h=8;
   - the top-models table takes w=6.
   - **Each widget's query must reproduce today's chart.** The CD1 parity tests list the mappings; use the same ones.
5. `dashboard-view.tsx` draws the Overview from `overview.ts` on a plain CSS grid (`grid-cols-12`, `col-span-w`, `row-span-h`, with rows of 40 px). Remove what goes:
   - the sections and their collapse with its localStorage key (D-C-16 drops them);
   - the KPI scroll-to-chart;
   - the dashboard's use of `metricsQuery`.
   - Delete the now-unused code in `metric-chart.tsx`, `kpi-tiles.tsx` and `top-models.tsx`. **`user-overview.tsx` still uses some of it until CD6. If a piece is shared, keep it and say which.**
6. The mock handler computes the series and totals from the generated dataset for the queries the Overview and the tests use. **If the handler would need a general query engine in JS, stop and ask.**

**Tests**
- Each chart type renders from a fixture answer.
- The format by field.
- The error colour.
- `fade_below`.
- `split`.
- The number badge and its tooltip.
- `share`.
- The bucket click opens the right list and filter.
- Zoom.
- An empty widget shows its reason, and an error shows a retry.
- The Overview page test (rewrite `routes/$org/$project/dashboard/index.test.tsx`): the headings, the five numbers, every chart title, and the bucket parameter `30m` for 24 h.

**Done when** `just check-web` passes and the Overview looks like today's dashboard, minus the sections. Check it in the browser with `just web-dev` and include a screenshot.

---

## CD5 — Custom dashboards (web)

**Edit**
- `web/package.json` (`react-grid-layout` exactly `2.3.0`, plus its CSS import) and `pnpm-lock.yaml`.
- The new route `web/src/routes/$org/$project/dashboard/$dashboardId.tsx`.
- `components/dashboard/` (switcher, grid, edit mode, widget editor).
- `api/queries.ts`.
- MSW handlers and fixtures for the dashboard CRUD.
- Tests.

**Steps**
1. Switcher in the dashboard header:
   - It lists Overview, then `GET /dashboards` by name, then "New dashboard" for members.
   - "New dashboard" asks for a name, creates an empty dashboard and opens it.
   - Duplicate (on the Overview too) asks for a name and POSTs a copy of the definition.
   - Rename and Delete (with a confirm) are for members on a custom dashboard.
2. Grid: `react-grid-layout` with 12 columns and a row height of 40 px.
   - It is static (no drag or resize) outside edit mode.
   - Below 768 px it is one column in `(y, x)` order and edit mode is off.
   - Widgets are the CD4 components.
3. Edit mode (members only):
   - Edit, Save and Cancel controls.
   - "Add widget" opens the editor; "Add heading" appends a full-width heading.
   - Each widget has Edit, Duplicate and Remove.
   - Save sends `PATCH` with `{version, definition}`. On a 409, a dialog offers "Reload" (losing the edits) or "Keep editing".
   - A server 400 shows each error next to its widget, using the path.
   - Leaving the page with unsaved changes asks for confirmation (TanStack Router's blocker).
4. Widget editor (a dialog):
   - the source;
   - 1–6 measures, each with an aggregate, a field (only the fields valid for that aggregate and source, from a table in `lib/dashboards/fields.ts` that mirrors spec 05), an optional condition (the filter bar of the source view, in single-expression mode) and a label;
   - the breakdown (field, limit, other);
   - the widget filter;
   - the chart type and its options;
   - the title;
   - a live preview over the dashboard's range.
   - Client validation mirrors the server rules for quick feedback; the server stays the judge.
5. Viewers see no editing controls. The Overview never shows Edit.

**Tests**
- The switcher lists and navigates.
- Create, rename, duplicate (including from the Overview) and delete.
- Edit mode: add a widget through the editor, remove one, and save sends the expected definition and version.
- The 409 dialog.
- Server 400 errors land on the right widget.
- The unsaved-changes prompt.
- A viewer sees no controls.
- One column on a narrow screen.
- The editor offers only valid fields for each aggregate.

**Done when** `just check-web` passes. Do a browser check: create a dashboard, add three widget types, drag and resize, save and reload, and include a screenshot.

---

## CD6 — Move the remaining charts, remove `GET /metrics`

**Edit**
- web: `traces-histogram.tsx`, `user-overview.tsx`, `rule-preview.tsx`, `rule-trend.tsx`, `lib/metrics.ts`, `api/queries.ts`, the mocks and tests;
- server: `routes/metrics.rs`, `api/metrics.rs`, `openapi.rs`, `fixtures/api/`;
- storage: `reads/metrics.rs`;
- docs: spec 05 (delete § Dashboard metrics (fixed dashboard)), spec 09 (delete the `GET /metrics` row) and every other mention found by `grep -rn "/metrics\b\|metrics endpoint" docs/spec`;
- the generated OpenAPI files.

**Steps**
1. Each consumer reads `widgetQuery` with the query that matches its current series:
   - histogram: traces count, plus a count where `status = 'error'`;
   - user overview: its KPIs and charts, with its filter;
   - alert previews: the measure that matches the alert metric (spec 07). **Check every alert metric against spec 07 § metrics, and stop if one has no widget equivalent.**
2. Delete `GET /metrics`, its types, its cache instance, the storage `metrics()` and everything only it used, and `metricsQuery` along with the `lib/metrics.ts` helpers that are no longer used. Keep the bucket helpers in `reads/buckets.rs`.
3. Make the cache concrete again if it has one user left.

**Tests**: update the consumers' tests to the new mock. Delete the tests of removed code, and nothing else.

**Done when** `just check` passes and `grep -rn "metricsQuery\|/metrics\"" web/src crates` finds nothing (OTLP `/v1/metrics` excepted).

---

## CD7 — Close-out

**Edit** `CHANGELOG.md`, `docs/roadmap.md` (the status of track C), and spec 11 if the browser checks found differences.

**Steps**
1. CHANGELOG `[Unreleased]`:
   - Added: custom dashboards, the widget query endpoint and the dashboards API.
   - Removed: `GET /metrics`.
   - Changed: the Overview has no collapsible sections.
2. Run `just check` and `just e2e`.

**Done when** both pass.

---

## CD8 — Browser-local dashboards in the demo org (web)

**Depends on** plan `docs/plans/demo-org/` phase DO2, which adds `demo` to `OrgMembership` (`GET /api/auth/me`). **Do not start before that field exists in `web/src/api/schema.gen.ts`; if it is named or shaped differently, stop and ask.**

**Edit** `web/src/lib/dashboards/store.ts` (new), the dashboard components of CD5, mocks, tests, spec 11 only if behaviour differs from § Dashboard → Demo org.

**Steps**
1. `store.ts`: one `DashboardStore` type (`list`, `get`, `create`, `update`, `remove`), two implementations: `apiStore` (the CD5 calls, moved here) and `localStore` (localStorage key `${STORAGE_PREFIX}dashboards.<org>/<project>`, try/catch around every access with an in-memory fallback, as `web/src/lib/settings.ts` does). `useDashboardStore()` picks `localStore` when the current org membership has `demo` true and the role is `viewer`, else `apiStore`.
2. `localStore` checks the 04 § Dashboards rules with the client validation of CD5 (move it here if needed) and answers the same errors the server would (`bad_request` with paths), so the edit-mode error display works unchanged. IDs are UUIDv7 generated in the browser; `version` still increments. The 100-dashboard cap applies.
3. With `localStore`, the switcher lists the Overview and the local dashboards only (never the project's server dashboards), the user gets the editing controls despite the viewer role, and edit mode shows the banner of spec 11.
4. The CD5 components read and write only through the store; TanStack Query keys stay the same so invalidation does not change.

**Tests**: `localStore` CRUD round trip, validation errors with paths, the cap, broken or missing localStorage (fallback), JSON that does not parse is ignored (an empty list, no crash); a page test with a demo viewer membership in the `/api/auth/me` mock: create, edit, reload (still there), no network call to `/dashboards`, server dashboards not listed, the banner; a demo member (not viewer) and a non-demo viewer use the API.

**Done when** `just check-web` passes.
