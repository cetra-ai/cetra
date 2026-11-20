Status: plan, written 2026-11-20 from the code at `1592a89`.

# CA · M5a — `session_id` breakdown and the "Coding agents" template

Milestone M5a of track CA (design `docs/plans/ca-coding-agents/00-design.md` § Dashboard template, D-CA-11). **Haiku implements it one phase at a time.** If a step is unclear or contradicts the code, **stop and report**; do not improvise.

## Prerequisites

- Branch `ca-m5a-dashboard`, cut from `claude/ca-coding-agents-22e274`. It is independent of M1–M4.
- `export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH` before any `pnpm` or `just` command.
- Load rules:
  - Rust is compiled and tested once per phase, right before the commit.
  - Web tests run on the touched files only while iterating (`pnpm --dir web exec vitest run <paths> --maxWorkers=8`).
  - The full `just check` runs only in the last phase.

## Read first

- Spec 05 § Dashboard queries (`docs/spec/05-query.md`: the breakdown-field table and the `count_distinct` rule).
- `crates/storage/src/reads/widgets.rs`: `SourceFields`, `TRACES`, `IDENTITY_FIELDS`, `is_breakdown`, `validate_shape`, and these tests:
  - `breakdown_field_and_limit_are_checked`;
  - `count_distinct_user_ids_on_traces`.
- `web/src/lib/dashboards/`:
  - `templates.ts`, `templates.test.ts`;
  - `preset.ts` (`numberAt`, `chartAt`, `query`, `LLM`, `HALF`, `CHART_HEIGHT`);
  - `fields.ts`, `fields.test.ts`;
  - `validate.ts`.
- `web/src/components/dashboard/dashboard-dialogs.tsx` and `dashboard-dialogs.test.tsx`.
- Spec 11 § Dashboard, the sentence naming the templates.

## What it is, and what it is not

**When M5a is done:**
- A widget query on the `traces` source may break down by `session_id`.
- "New dashboard" offers a fourth template, "Coding agents", which creates the widgets listed in D3.

**Not in M5a:**
- Normalization of agent spans (M2). Until M2 lands, the template's widgets show "no data" on a real server.
- `session_id` on the `spans` source: the design adds it to `traces` only.
- Rendering on the generated mock dataset. Its widget-query mock cannot evaluate `metadata[...]` filters (`web/src/mocks/generated/widget-query.ts`, `Unsupported`), so the template's widgets show the mock's error state in `just web-dev`. That is accepted, and the template tests do not query.

## Decisions this plan relies on

| ID | Summary |
|---|---|
| D-CA-11 | "Coding agents" template; `session_id` becomes a traces breakdown field |
| D-CA-4 | `metadata['coding_agent']`, `metadata['repository']`, `metadata['branch']` |
| D-CA-7 | Cost is the API-price equivalent for subscriptions (the template description says so) |
| D-C-21 | Templates are defined in the UI and copied into the new dashboard |

## Versions

No new dependency.

## Phases

```
D1 (storage: session_id breakdown) ──► D2 (web: field list) ──► D3 (web: template, spec 11)
```

D1 and D2 do not share files, but D3's widgets need D2's field list to validate. Run them in order.

---

## D1 — `session_id` is a traces breakdown field

**Goal:** the storage accepts and computes a `traces` breakdown on `session_id`.

**Edit:**
- `crates/storage/src/reads/widgets.rs`
- `docs/spec/05-query.md`

**Steps:**
1. In `widgets.rs`, the `TRACES` `SourceFields`: add `traces::SESSION_ID` to `breakdown`, right after `traces::USER_ID`. Nothing else changes. `IDENTITY_FIELDS` already lists it, and `is_distinct` is unchanged.
2. In spec 05 § Dashboard queries, the fields table, the traces row: insert `` `session_id`, `` after `` `user_id`, `` in the breakdown column.

**Tests** (in `widgets.rs` `mod tests`):
- Extend `breakdown_field_and_limit_are_checked`: on `WidgetSource::Traces`, `breakdown("session_id", 5)` validates with no error. On `WidgetSource::Spans`, `breakdown("session_id", 5)` still gives the error `breakdown.field` "not a breakdown field of this source".
- New async test `breakdown_by_session_id_on_traces`, following `count_distinct_user_ids_on_traces`, with the same helpers (`span(trace, 0, minute(…), None)`, `context_of(&spans)`, `build`, `answer`, `total_pairs`).
  - Data: four traces, each one root `llm` span:
    - two with `session_id = Some("s1")` and `cost_total` 1.0 and 2.0;
    - one with `Some("s2")` and 5.0;
    - one with `session_id = None` and 9.0.

    Set `session_id` and `cost_total` on the root span, the way the existing tests do.
  - Query: `build(WidgetSource::Traces, vec![measure(Agg::Sum, Some("cost_total"), None)], split("session_id", 5, false))`, total shape.
  - Assert `total_pairs(&data) == vec![(Some("s2".into()), false, vec![Some(5.0)]), (Some("s1".into()), false, vec![Some(3.0)])]`: largest first, and the null-session trace left out (spec 05). If a helper's name or signature differs, use the one `count_distinct_user_ids_on_traces` uses and say so in the report.

**Done when:**
- `cargo test -p cetra-storage widgets` passes.
- `cargo clippy -p cetra-storage --all-targets -- -D warnings` passes.

**Commit:** `Allow session_id as a traces breakdown in widget queries`

---

## D2 — The web field list knows `session_id`

**Goal:** the widget editor and `validateDefinition` accept a `traces` breakdown on `session_id`.

**Edit:**
- `web/src/lib/dashboards/fields.ts`
- `web/src/lib/dashboards/fields.test.ts`

**Steps:**
1. In `fields.ts`, `SOURCES.traces.breakdown`: insert `"session_id"` after `"user_id"`, giving `["name", "status", "environment", "release", "user_id", "session_id", "tags", "models"]`.
2. Update `fields.test.ts`:
   - Its `breakdownFields("traces")` expectation gets the same list.
   - Its `countDistinctFields("traces")` expectation becomes `["name", "status", "environment", "release", "user_id", "session_id", "trace_id"]`. `countDistinctFields` appends the identity fields that are not already breakdown fields, so `session_id` now comes from the breakdown list, before `trace_id`.
   - Read the current assertion first. If it is not built that way, stop and report.

**Tests:** the updated `fields.test.ts`, plus `validate.test.ts` unchanged.

**Done when:** `pnpm --dir web exec vitest run src/lib/dashboards/fields.test.ts src/lib/dashboards/validate.test.ts --maxWorkers=8` passes.

**Commit:** `Offer session_id as a traces breakdown in the widget editor`

---

## D3 — The "Coding agents" template

**Goal:** "New dashboard" offers "Coding agents", with the widgets of D-CA-11.

**Edit:**
- `web/src/lib/dashboards/templates.ts`
- `web/src/lib/dashboards/templates.test.ts`
- `web/src/components/dashboard/dashboard-dialogs.test.tsx`
- `docs/spec/11-ui.md`
- `website/src/content/docs/docs/tracing/custom-dashboards.mdx`
- `CHANGELOG.md`

**Steps:**
1. In `templates.ts`:
   - `TemplateId` becomes `"cost" | "latency" | "quality" | "coding_agents"`.
   - Add `const AGENT = "metadata['coding_agent'] IS NOT NULL";` and `const FIFTH = 12;` next to `QUARTER` and `THIRD`.
   - Append the template below as the **last** element of `TEMPLATES`.
2. The template:

   ```ts
   {
     id: "coding_agents",
     name: "Coding agents",
     description:
       "Cost, sessions and tool use of coding agents (Claude Code, Codex, Gemini CLI, Copilot) by person, repository and branch. For subscription plans the cost is what the tokens would cost at API prices.",
     definition: {
       widgets: [
         numberAt("agents-1", "Cost", 0, FIFTH,
           query("traces", [{ agg: "sum", field: "cost_total" }], { filter: AGENT })),
         numberAt("agents-2", "Sessions", FIFTH, FIFTH,
           query("traces", [{ agg: "count_distinct", field: "session_id" }], { filter: AGENT })),
         numberAt("agents-3", "People", 2 * FIFTH, FIFTH,
           query("traces", [{ agg: "count_distinct", field: "user_id" }], { filter: AGENT })),
         numberAt("agents-4", "Turns", 3 * FIFTH, FIFTH,
           query("traces", [{ agg: "count" }], { filter: AGENT })),
         numberAt("agents-5", "Tokens", 4 * FIFTH, FIFTH,
           query("traces", [{ agg: "sum", field: "total_tokens" }], { filter: AGENT })),
         chartAt("agents-6", "Cost over time by agent", 0, 4, { type: "area", stacked: true },
           query("traces", [{ agg: "sum", field: "cost_total", label: "Cost" }], {
             filter: AGENT,
             breakdown: { field: "metadata['coding_agent']", limit: 5, other: true },
           })),
         chartAt("agents-7", "Cost over time by model", HALF, 4, { type: "area", stacked: true },
           query("spans", [{ agg: "sum", field: "cost_total", label: "Cost" }], {
             filter: `${LLM} AND ${AGENT}`,
             breakdown: { field: "model", limit: 5, other: true },
           })),
         chartAt("agents-8", "Top people by cost", 0, 4 + CHART_HEIGHT, { type: "bar_h" },
           query("traces", [{ agg: "sum", field: "cost_total", label: "Cost" }], {
             filter: `${AGENT} AND user_id IS NOT NULL`,
             breakdown: { field: "user_id", limit: 10, other: false },
           })),
         chartAt("agents-9", "Top repositories by cost", HALF, 4 + CHART_HEIGHT, { type: "bar_h" },
           query("traces", [{ agg: "sum", field: "cost_total", label: "Cost" }], {
             filter: `${AGENT} AND metadata['repository'] IS NOT NULL`,
             breakdown: { field: "metadata['repository']", limit: 10, other: false },
           })),
         chartAt("agents-10", "Top branches by cost", 0, 4 + 2 * CHART_HEIGHT, { type: "bar_h" },
           query("traces", [{ agg: "sum", field: "cost_total", label: "Cost" }], {
             filter: `${AGENT} AND metadata['branch'] IS NOT NULL`,
             breakdown: { field: "metadata['branch']", limit: 10, other: false },
           })),
         chartAt("agents-11", "Top sessions by cost", HALF, 4 + 2 * CHART_HEIGHT, { type: "bar_h" },
           query("traces", [{ agg: "sum", field: "cost_total", label: "Cost" }], {
             filter: `${AGENT} AND session_id IS NOT NULL`,
             breakdown: { field: "session_id", limit: 10, other: false },
           })),
         chartAt("agents-12", "Tool calls by tool", 0, 4 + 3 * CHART_HEIGHT, { type: "table" },
           query("spans", [
             { agg: "count", label: "Calls" },
             { agg: "rate", where: "status = 'error'", label: "Error rate" },
             { agg: "p95", field: "duration_ns", label: "p95 duration" },
           ], {
             filter: `span_kind = 'tool' AND ${AGENT}`,
             breakdown: { field: "name", limit: 20, other: true },
           })),
         chartAt("agents-13", "Cost per turn by agent", HALF, 4 + 3 * CHART_HEIGHT, { type: "table" },
           query("traces", [
             { agg: "p50", field: "cost_total", label: "p50 cost" },
             { agg: "p95", field: "cost_total", label: "p95 cost" },
           ], {
             filter: AGENT,
             breakdown: { field: "metadata['coding_agent']", limit: 5, other: false },
           })),
       ],
     },
   }
   ```

   This literal was checked against `templates.ts`, `preset.ts` and `validate.ts`: no `version` key, no `share` on tables, and argument orders as written.
3. `templates.test.ts`:
   - The test asserting the ids and names becomes: ids `["cost", "latency", "quality", "coding_agents"]` and names `["Cost", "Latency", "Quality", "Coding agents"]`. Rename it "the ids and names are Cost, Latency, Quality and Coding agents, in that order".
   - The other tests (validation, no overlap, number tiles summing to 60) must pass unchanged: 5 × 12 = 60.
   - Add the test `the coding agents template filters every widget on coding_agent`: every widget's `query.filter` contains `metadata['coding_agent'] IS NOT NULL`.
4. `dashboard-dialogs.test.tsx`: the test "lists Blank and the three templates under Start from" becomes "lists Blank and the four templates under Start from", with labels `["Blank", "Cost", "Latency", "Quality", "Coding agents"]`. The `it.each(TEMPLATES)` test covers the new template automatically.
5. `website/src/content/docs/docs/tracing/custom-dashboards.mdx`, the "Start from" list, after the **Quality** bullet:

   ```
      - **Coding agents**: cost, sessions and tool calls of coding agents by person, repository, branch and session.
   ```

6. `CHANGELOG.md` § `[Unreleased]`, under `### Added` (create the heading after `### Upgrade notes` if absent):

   ```
   - A "Coding agents" dashboard template, and `session_id` as a breakdown of trace widgets.
   ```

7. Spec 11 § Dashboard: "Blank, or one of the templates Cost, Latency and Quality" becomes "Blank, or one of the templates Cost, Latency, Quality and Coding agents (D-CA-11)".

**Tests:** listed in steps 3 and 4.

**Done when:**
- `pnpm --dir web exec vitest run src/lib/dashboards src/components/dashboard --maxWorkers=8` passes.
- `export PATH=…; just check` passes. It includes `check-docs`, whose `check-terms` may flag the new bullet's wording; if it does, report the message.
- `docs/roadmap.md` is not changed: the coordinator updates the CA row.

**Commit:** `Add the Coding agents dashboard template`
