# Dashboard templates and Overview additions (D-C-21)

The Overview loses its section headings (done in this branch's first commit) and gains two horizontal-bar charts: top users by cost and top trace names by cost. "New dashboard" offers three templates besides Blank — Cost, Latency and Quality — whose widgets are copied into the new dashboard. Templates and the Overview are built from shared preset helpers.

**Prerequisites:** `main` at or after `7e27d97`. This branch's first commit removes the Overview's headings and writes D-C-21 and spec 11 (§ Dashboard: the Overview paragraph and the header sentence about "New dashboard").

**Read first:** `docs/mvp-decisions.md` D-C-21, D-C-16, D-C-19 (60-column grid); `docs/spec/11-ui.md` § Dashboard; `web/src/lib/dashboards/overview.ts`, `user-overview.ts`, `types.ts`, `validate.ts`, `fields.ts`; `web/src/components/dashboard/dashboard-dialogs.tsx` (`NewDashboardDialog`, `NewForm`) and its test; `web/src/components/ui/radio-group.tsx`.

## What it is, and what it is not

Done when: the Overview ends with "Top users by cost" (right of "Top models by cost") and "Top trace names by cost" (below it); the user page's Usage leaves out the evaluation cost and the top users; the New dashboard dialog has a "Start from" choice (Blank, Cost, Latency, Quality), and creating with a template sends that template's definition.

Out of scope: value labels at the end of horizontal bars (a later display change); templates in the Duplicate or Edit flows; server-side templates.

## Decisions this plan relies on

| ID | Summary |
|---|---|
| D-C-21 | No Overview headings; two cost bars in the Overview; Cost/Latency/Quality templates copied into a new custom dashboard; contents as listed there. |
| D-C-16 | Widget kinds and limits (breakdown limit 1–20; `bar_h` needs a breakdown; `number` one measure, no breakdown). |
| D-C-19 | 60 columns; half width is 30. |

User's answers (2026-11-19): option 1 + 2; contents as proposed (see D-C-21).

## Versions

No new dependency.

## Phases

```
DT1 (preset helpers + Overview additions) ─► DT2 (templates + dialog)
```
Sequential: DT2 imports DT1's `preset.ts`.

---

## DT1 — Preset helpers and the two Overview bars

**Goal:** the Overview and the coming templates share their building blocks, and the Overview gains top users and top trace names by cost.

**Edit:**
- `web/src/lib/dashboards/preset.ts` (new)
- `web/src/lib/dashboards/overview.ts`, `overview.test.ts`
- `web/src/lib/dashboards/user-overview.ts`, `user-overview.test.ts`
- `web/src/routes/$org/$project/dashboard/index.test.tsx` (the `CHARTS` list only)

**Contracts:**

```ts
// web/src/lib/dashboards/preset.ts
/** Building blocks of the built-in dashboards: the Overview and the templates (D-C-21). */
import { COLUMNS } from "./draft";
import type { Chart, Measure, Widget, WidgetQuery, WidgetSource } from "./types";

/** Rows of a chart. */
export const CHART_HEIGHT = 7;
/** Rows of a number tile. */
export const NUMBER_HEIGHT = 4;
/** Half the grid's width. */
export const HALF = COLUMNS / 2;
/** LLM spans with a model. */
export const LLM = "span_kind = 'llm' AND model IS NOT NULL";
export const ERROR = "status = 'error'";

/** A number tile `w` columns wide at column `x` of the first row. */
export function numberAt(id: string, title: string, x: number, w: number, query: WidgetQuery): Widget;
/** A chart half the width and `CHART_HEIGHT` rows tall, at column `x` and row `y`. */
export function chartAt(
  id: string,
  title: string,
  x: number,
  y: number,
  settings: Chart,
  query: WidgetQuery,
): Widget;
/** A query over one source: its measures, an optional filter and breakdown. */
export function query(
  source: WidgetSource,
  measures: Measure[],
  options?: { filter?: string; breakdown?: WidgetQuery["breakdown"] },
): WidgetQuery;
```

- `numberAt` returns `{ id, title, kind: "chart", chart: { type: "number" }, query, layout: { x, y: 0, w, h: NUMBER_HEIGHT } }`.
- `chartAt` returns `{ id, title, kind: "chart", chart: settings, query, layout: { x, y, w: HALF, h: CHART_HEIGHT } }`.
- `query` is moved unchanged from `overview.ts` (omits `filter` and `breakdown` when not given).

**Steps:**
1. Create `preset.ts` as above.
2. In `overview.ts`: delete its local `CHART_HEIGHT`, `HALF`, `LLM`, `ERROR`, `chart` and `query`, and import them from `./preset` (`chart` becomes `chartAt`). Keep `TILE = COLUMNS / 5` and turn its local `number(id, title, slot, q)` into calls of `numberAt(id, title, slot * TILE, TILE, q)` (delete the local `number`). Rewrite the "Top models by cost" literal as `chartAt("ov-19", "Top models by cost", 0, 39, { type: "table", share: true }, query(…same…))`. The resulting `OVERVIEW` must be equal to today's (a test checks it, step 4).
3. Append, after `ov-19`:
   ```ts
   chartAt(
     "ov-20",
     "Top users by cost",
     HALF,
     39,
     { type: "bar_h" },
     query("traces", [{ agg: "sum", field: "cost_total", label: "Cost" }], {
       filter: "user_id IS NOT NULL",
       breakdown: { field: "user_id", limit: 10, other: false },
     }),
   ),
   chartAt(
     "ov-21",
     "Top trace names by cost",
     0,
     46,
     { type: "bar_h" },
     query("traces", [{ agg: "sum", field: "cost_total", label: "Cost" }], {
       breakdown: { field: "name", limit: 10, other: false },
     }),
   ),
   ```
4. `user-overview.ts`: `EXCLUDED = new Set(["ov-18", "ov-20"])` and its doc comment says "without the evaluation cost and the top users (they rank users)".

**Tests:**
- `overview.test.ts`:
  - the existing order test ends with `"Top users by cost", "Top trace names by cost"` (update `titles.at(-1)` and add the two before it as needed);
  - `the two cost bars are horizontal bars of the 10 largest, by user and by trace name` (chart type, breakdown field and limit, the user one filters `user_id IS NOT NULL`);
  - existing `validateDefinition(OVERVIEW)` and "no two widgets overlap" tests keep passing (they cover the new widgets).
  - `the refactor keeps the earlier widgets`: snapshot-free check — the first 15 widgets' ids, titles and layouts equal this list: copy them from the current `OVERVIEW` before editing (write the expected `[id, title, x, y, w, h]` tuples literally in the test).
- `user-overview.test.ts`: the titles list gains `"Top trace names by cost"` at the end (not "Top users by cost"); the layout list gains one entry `[30, 32, 30, 7]`.
- `dashboard/index.test.tsx`: add both titles to `CHARTS`.

**Done when:** `cd web && pnpm exec vitest run src/lib/dashboards src/routes/'$org'/'$project'/dashboard src/routes/'$org'/'$project'/users --maxWorkers=8` passes; `pnpm exec tsc -b --noEmit`; `node_modules/.bin/biome check web/src` (repo root); `just check-web`; `rm -rf web/.vitest`.

**Commit:** `Add top users and trace names by cost to the Overview`

---

## DT2 — Templates and the New dashboard choice

**Goal:** "New dashboard" can start from Blank, Cost, Latency or Quality.

**Edit:**
- `web/src/lib/dashboards/templates.ts` (new), `templates.test.ts` (new)
- `web/src/components/dashboard/dashboard-dialogs.tsx` (`NewForm` only), `dashboard-dialogs.test.tsx`
- `CHANGELOG.md`

**Contracts:**

```ts
// web/src/lib/dashboards/templates.ts
/** Templates of "New dashboard" (D-C-21): their widgets are copied into the new dashboard. */
import { CHART_HEIGHT, chartAt, ERROR, HALF, LLM, numberAt, query } from "./preset";
import type { Definition } from "./types";

export type TemplateId = "cost" | "latency" | "quality";

export type DashboardTemplate = {
  id: TemplateId;
  name: string;
  /** One line under the name in the dialog. */
  description: string;
  definition: Definition;
};

const QUARTER = 15; // four tiles in the first row
const THIRD = 20; // three tiles in the first row
const LLM_TTFT = "span_kind = 'llm' AND ttft_ns IS NOT NULL";

export const TEMPLATES: readonly DashboardTemplate[] = [
  {
    id: "cost",
    name: "Cost",
    description: "Cost by model, environment, user and trace name, and p95 cost.",
    definition: {
      widgets: [
        numberAt("cost-1", "Total cost", 0, QUARTER, query("traces", [{ agg: "sum", field: "cost_total" }])),
        numberAt("cost-2", "Avg cost per trace", QUARTER, QUARTER, query("traces", [{ agg: "avg", field: "cost_total" }])),
        numberAt("cost-3", "LLM calls", 2 * QUARTER, QUARTER, query("spans", [{ agg: "count" }], { filter: "span_kind = 'llm'" })),
        numberAt("cost-4", "Tokens", 3 * QUARTER, QUARTER, query("traces", [{ agg: "sum", field: "total_tokens" }])),
        chartAt("cost-5", "Cost over time by model", 0, 4, { type: "area", stacked: true },
          query("spans", [{ agg: "sum", field: "cost_total", label: "Cost" }], { filter: LLM, breakdown: { field: "model", limit: 5, other: true } })),
        chartAt("cost-6", "Cost by environment", HALF, 4, { type: "bar_h" },
          query("traces", [{ agg: "sum", field: "cost_total", label: "Cost" }], { breakdown: { field: "environment", limit: 10, other: false } })),
        chartAt("cost-7", "Top users by cost", 0, 4 + CHART_HEIGHT, { type: "bar_h" },
          query("traces", [{ agg: "sum", field: "cost_total", label: "Cost" }], { filter: "user_id IS NOT NULL", breakdown: { field: "user_id", limit: 20, other: false } })),
        chartAt("cost-8", "Top trace names by cost", HALF, 4 + CHART_HEIGHT, { type: "bar_h" },
          query("traces", [{ agg: "sum", field: "cost_total", label: "Cost" }], { breakdown: { field: "name", limit: 20, other: false } })),
        chartAt("cost-9", "Top span names by cost", 0, 4 + 2 * CHART_HEIGHT, { type: "bar_h" },
          query("spans", [{ agg: "sum", field: "cost_total", label: "Cost" }], { filter: "cost_total IS NOT NULL", breakdown: { field: "name", limit: 20, other: false } })),
        chartAt("cost-10", "p95 cost per trace by name", HALF, 4 + 2 * CHART_HEIGHT, { type: "line" },
          query("traces", [{ agg: "p95", field: "cost_total", label: "p95" }], { breakdown: { field: "name", limit: 5, other: false } })),
        chartAt("cost-11", "p95 cost per LLM call", 0, 4 + 3 * CHART_HEIGHT, { type: "line" },
          query("spans", [
            { agg: "p95", field: "cost_input", label: "Input" },
            { agg: "p95", field: "cost_output", label: "Output" },
          ], { filter: LLM })),
      ],
    },
  },
  {
    id: "latency",
    name: "Latency",
    description: "Trace and LLM latency percentiles, slowest traces and time to first token.",
    definition: {
      widgets: [
        numberAt("lat-1", "p50 trace duration", 0, THIRD, query("traces", [{ agg: "p50", field: "duration_ns" }])),
        numberAt("lat-2", "p95 trace duration", THIRD, THIRD, query("traces", [{ agg: "p95", field: "duration_ns" }])),
        numberAt("lat-3", "p95 TTFT", 2 * THIRD, THIRD, query("spans", [{ agg: "p95", field: "ttft_ns" }], { filter: LLM_TTFT })),
        chartAt("lat-4", "p95 duration by trace name", 0, 4, { type: "line" },
          query("traces", [{ agg: "p95", field: "duration_ns", label: "p95" }], { breakdown: { field: "name", limit: 5, other: false } })),
        chartAt("lat-5", "Slowest trace names", HALF, 4, { type: "bar_h" },
          query("traces", [{ agg: "p95", field: "duration_ns", label: "p95" }], { breakdown: { field: "name", limit: 10, other: false } })),
        chartAt("lat-6", "LLM latency by model", 0, 4 + CHART_HEIGHT, { type: "line" },
          query("spans", [{ agg: "p95", field: "duration_ns", label: "p95" }], { filter: LLM, breakdown: { field: "model", limit: 5, other: false } })),
        chartAt("lat-7", "TTFT by model", HALF, 4 + CHART_HEIGHT, { type: "line" },
          query("spans", [{ agg: "p95", field: "ttft_ns", label: "p95" }], { filter: `${LLM_TTFT} AND model IS NOT NULL`, breakdown: { field: "model", limit: 5, other: false } })),
      ],
    },
  },
  {
    id: "quality",
    name: "Quality",
    description: "Error rates by trace and span name, and scores over time.",
    definition: {
      widgets: [
        numberAt("qual-1", "Error rate", 0, THIRD, query("traces", [{ agg: "rate", where: ERROR, label: "Error rate" }])),
        numberAt("qual-2", "Errors", THIRD, THIRD, query("traces", [{ agg: "count", where: ERROR, label: "Errors" }])),
        numberAt("qual-3", "Traces", 2 * THIRD, THIRD, query("traces", [{ agg: "count" }])),
        chartAt("qual-4", "Error rate over time", 0, 4, { type: "line" },
          query("traces", [{ agg: "rate", where: ERROR, label: "Error rate" }])),
        chartAt("qual-5", "Error rate by trace name", HALF, 4, { type: "bar_h" },
          query("traces", [{ agg: "rate", where: ERROR, label: "Error rate" }], { breakdown: { field: "name", limit: 10, other: false } })),
        chartAt("qual-6", "Top failing span names", 0, 4 + CHART_HEIGHT, { type: "bar_h" },
          query("spans", [{ agg: "count", label: "Errors" }], { filter: ERROR, breakdown: { field: "name", limit: 10, other: false } })),
        chartAt("qual-7", "Scores over time", HALF, 4 + CHART_HEIGHT, { type: "line" },
          query("scores", [{ agg: "avg", field: "value_num" }], { filter: "value_num IS NOT NULL", breakdown: { field: "name", limit: 6, other: false } })),
      ],
    },
  },
];
```

(Biome reformats the long lines; keep the content exactly.)

**Steps:**
1. Write `templates.ts` as above.
2. `NewForm` (`dashboard-dialogs.tsx`):
   - state `const [start, setStart] = useState<"blank" | TemplateId>("blank")`;
   - after `DashboardFields`, a `fieldset` with `legend` "Start from" (class `text-caption text-muted-foreground`) and a `RadioGroup` (`@/components/ui/radio-group`) with `value={start}` and `onValueChange` setting it; one item per choice: `blank` ("Blank", "An empty dashboard.") then each of `TEMPLATES` (`template.name`, `template.description`). Each item is a `label` row: `RadioGroupItem` with `id` and `value`, then the name (`text-ui font-medium`) and the description (`text-caption text-muted-foreground`) stacked;
   - on submit, `definition` is `start === "blank" ? { widgets: [] } : TEMPLATES.find((t) => t.id === start)?.definition ?? { widgets: [] }`;
   - the dialog description reads "Create a dashboard and open it.".
3. `CHANGELOG.md` § Unreleased → Added: "Dashboard templates — Cost, Latency and Quality — in the New dashboard dialog; the Overview adds top users and top trace names by cost and drops its section headings (D-C-21)."

**Tests:**
- `templates.test.ts`:
  - `every template passes the dashboard's own validation` (`validateDefinition(t.definition)` is `[]` for each);
  - `templates' widgets have unique ids and do not overlap` (reuse the overlap helper shape from `overview.test.ts`, written locally);
  - `the ids and names are Cost, Latency and Quality, in that order`;
  - `each number row fills the width` (number tiles' `w` sums to `COLUMNS` per template).
- `dashboard-dialogs.test.tsx`:
  - the existing `creates an empty dashboard and reports it` still sends `{ widgets: [] }` (Blank is the default);
  - `creates a dashboard from a template`: pick the "Cost" radio, submit, the POST body's `definition` equals `TEMPLATES[0].definition`;
  - `lists Blank and the three templates under Start from`.

**Done when:** `cd web && pnpm exec vitest run src/lib/dashboards src/components/dashboard --maxWorkers=8` passes; `tsc`; Biome; `just check-web`; `rm -rf web/.vitest`; by hand in the dev server: create a dashboard from each template on the large mock and see every widget draw.

**Commit:** `Add Cost, Latency and Quality dashboard templates`
