import { expect, it } from "vitest";
import type { Chart, Definition, Widget, WidgetQuery } from "./types";
import { validateDashboard, validateDefinition } from "./validate";

function query(measures: unknown[], extra: Record<string, unknown> = {}): WidgetQuery {
  return { source: "traces", measures, ...extra } as WidgetQuery;
}

function chart(overrides: Partial<Widget> & Record<string, unknown> = {}): Widget {
  return {
    id: "w1",
    title: "Requests",
    kind: "chart",
    layout: { x: 0, y: 0, w: 6, h: 4 },
    chart: { type: "line" },
    query: query([{ agg: "count" }]),
    ...overrides,
  } as Widget;
}

function heading(overrides: Record<string, unknown> = {}): Widget {
  return {
    id: "h1",
    title: "Traffic",
    kind: "heading",
    layout: { x: 0, y: 0, w: 60, h: 1 },
    ...overrides,
  } as Widget;
}

function definitionOf(...widgets: Widget[]): Definition {
  return { widgets };
}

/** The issues of a definition with one widget, as `field: message` strings. */
function issuesOf(widget: Widget) {
  return validateDefinition(definitionOf(widget)).map(
    (issue) => `${issue.field}: ${issue.message}`,
  );
}

const valid = { name: "Support", definition: definitionOf(chart()) };

it("accepts a valid dashboard", () => {
  expect(validateDashboard(valid)).toEqual([]);
  expect(
    validateDashboard({ name: "Support", description: "", definition: { widgets: [] } }),
  ).toEqual([]);
});

it("checks the name: trimmed, 1 to 100 characters", () => {
  expect(validateDashboard({ name: "   " })).toEqual([
    { field: "name", message: "must be 1 to 100 characters" },
  ]);
  expect(validateDashboard({ name: `  ${"n".repeat(100)}  ` })).toEqual([]);
  expect(validateDashboard({ name: "n".repeat(101) })).toEqual([
    { field: "name", message: "must be 1 to 100 characters" },
  ]);
});

it("checks the description: trimmed, at most 500 characters, blank is fine", () => {
  expect(validateDashboard({ name: "a", description: "d".repeat(501) })).toEqual([
    { field: "description", message: "must be at most 500 characters" },
  ]);
  expect(validateDashboard({ name: "a", description: `  ${"d".repeat(500)}  ` })).toEqual([]);
  expect(validateDashboard({ name: "a", description: " \t " })).toEqual([]);
});

it("checks the definition only when it is given", () => {
  expect(validateDashboard({ name: "Support" })).toEqual([]);
  const widgets = Array.from({ length: 51 }, (_, index) => heading({ id: `h${index}` }));
  expect(validateDashboard({ name: "a", definition: { widgets } })).toEqual([
    { field: "definition.widgets", message: "must have at most 50 widgets" },
  ]);
});

it("refuses a definition over 256 KiB at definition", () => {
  const big = chart({
    query: query([{ agg: "count" }], { filter: "x".repeat(262_200) }),
  });
  expect(validateDefinition(definitionOf(big))).toEqual([
    { field: "definition", message: "must be at most 262144 bytes" },
  ]);
});

it("checks widget ids: 1 to 64 characters of letters, digits, _ and -, unique", () => {
  expect(issuesOf(chart({ id: "bad id!" }))).toEqual([
    "definition.widgets[0].id: must be 1 to 64 characters of letters, digits, _ or -",
  ]);
  expect(issuesOf(chart({ id: "a".repeat(65) }))).toEqual([
    "definition.widgets[0].id: must be 1 to 64 characters of letters, digits, _ or -",
  ]);
  expect(
    validateDefinition(definitionOf(heading({ id: "same" }), heading({ id: "same" }))),
  ).toEqual([{ field: "definition.widgets[1].id", message: "must be unique in the dashboard" }]);
});

it("checks titles: a heading needs 1 to 100 trimmed characters, a chart at most 100", () => {
  expect(issuesOf(heading({ title: "   " }))).toEqual([
    "definition.widgets[0].title: a heading needs 1 to 100 characters",
  ]);
  expect(issuesOf(chart({ title: "" }))).toEqual([]);
  expect(issuesOf(chart({ title: "t".repeat(101) }))).toEqual([
    "definition.widgets[0].title: must be at most 100 characters",
  ]);
});

it("checks the layout bounds and that x + w fits the grid", () => {
  expect(issuesOf(chart({ layout: { x: 60, y: 0, w: 1, h: 1 } }))).toEqual([
    "definition.widgets[0].layout.x: must be 0 to 59",
    "definition.widgets[0].layout.w: x + w must be at most 60",
  ]);
  expect(issuesOf(chart({ layout: { x: 0, y: 1001, w: 1, h: 1 } }))).toEqual([
    "definition.widgets[0].layout.y: must be 0 to 1000",
  ]);
  expect(issuesOf(chart({ layout: { x: 0, y: 0, w: 61, h: 1 } }))).toEqual([
    "definition.widgets[0].layout.w: must be 1 to 60",
  ]);
  expect(issuesOf(chart({ layout: { x: 40, y: 0, w: 21, h: 1 } }))).toEqual([
    "definition.widgets[0].layout.w: x + w must be at most 60",
  ]);
  expect(issuesOf(chart({ layout: { x: 0, y: 0, w: 1, h: 41 } }))).toEqual([
    "definition.widgets[0].layout.h: must be 1 to 40",
  ]);
});

it("refuses a chart on a heading and a query on a heading", () => {
  expect(issuesOf(heading({ chart: { type: "line" }, query: query([{ agg: "count" }]) }))).toEqual([
    "definition.widgets[0].chart: a heading has no chart",
    "definition.widgets[0].query: a heading has no query",
  ]);
});

it("needs chart and query on a chart widget", () => {
  expect(issuesOf(chart({ chart: undefined, query: undefined }))).toEqual([
    "definition.widgets[0].chart: a chart needs chart",
    "definition.widgets[0].query: a chart needs a query",
  ]);
});

it("checks the chart options by type", () => {
  const stacked = (type: string) => issuesOf(chart({ chart: { type, stacked: true } as Chart }));
  expect(stacked("line")).toEqual([
    "definition.widgets[0].chart.stacked: only bar and area charts stack",
  ]);
  expect(stacked("area")).toEqual([]);
  expect(
    issuesOf(
      chart({
        chart: { type: "bar", split: true },
        query: query([{ agg: "count" }], { breakdown: { field: "name" } }),
      }),
    ),
  ).toEqual(["definition.widgets[0].chart.split: only line charts split"]);
  expect(issuesOf(chart({ chart: { type: "line", fade_below: 5 } }))).toEqual([
    "definition.widgets[0].chart.fade_below: only bar charts fade",
  ]);
  expect(issuesOf(chart({ chart: { type: "bar", fade_below: 0 } }))).toEqual([
    "definition.widgets[0].chart.fade_below: must be 1 to 1000",
  ]);
  expect(issuesOf(chart({ chart: { type: "bar", fade_below: 1001 } }))).toEqual([
    "definition.widgets[0].chart.fade_below: must be 1 to 1000",
  ]);
  expect(issuesOf(chart({ chart: { type: "bar", fade_below: 5 } }))).toEqual([]);
  expect(issuesOf(chart({ chart: { type: "bar", share: true } }))).toEqual([
    "definition.widgets[0].chart.share: only table charts show a share",
  ]);
});

it("needs a breakdown for a split line chart, at most 6 groups", () => {
  const split = (breakdown?: Record<string, unknown>) =>
    issuesOf(
      chart({
        chart: { type: "line", split: true },
        query: query([{ agg: "count" }], breakdown ? { breakdown } : {}),
      }),
    );
  expect(split()).toEqual([
    "definition.widgets[0].query.breakdown: a split chart needs a breakdown",
  ]);
  expect(split({ field: "models", limit: 7 })).toEqual([
    "definition.widgets[0].query.breakdown.limit: a split chart has at most 6 groups",
  ]);
  expect(split({ field: "models", limit: 6 })).toEqual([]);
  expect(split({ field: "models" })).toEqual([]);
});

it("needs a breakdown for a horizontal bar chart", () => {
  expect(issuesOf(chart({ chart: { type: "bar_h" } }))).toEqual([
    "definition.widgets[0].query.breakdown: a horizontal bar chart needs a breakdown",
  ]);
});

it("needs one measure and no breakdown for a number chart", () => {
  expect(
    issuesOf(
      chart({
        chart: { type: "number" },
        query: query([{ agg: "count" }, { agg: "count" }]),
      }),
    ),
  ).toEqual(["definition.widgets[0].query.measures: a number chart has exactly one measure"]);
  expect(
    issuesOf(
      chart({
        chart: { type: "number" },
        query: query([{ agg: "count" }], { breakdown: { field: "name" } }),
      }),
    ),
  ).toEqual(["definition.widgets[0].query.breakdown: a number chart has no breakdown"]);
});

it("checks the measure count: 1 to 6", () => {
  expect(issuesOf(chart({ query: query([]) }))).toEqual([
    "definition.widgets[0].query.measures: must have 1 to 6 measures",
  ]);
  expect(
    issuesOf(chart({ query: query(Array.from({ length: 7 }, () => ({ agg: "count" }))) })),
  ).toEqual(["definition.widgets[0].query.measures: must have 1 to 6 measures"]);
});

it("checks each aggregate's field", () => {
  expect(issuesOf(chart({ query: query([{ agg: "count", field: "name" }]) }))).toEqual([
    "definition.widgets[0].query.measures[0].field: count takes no field",
  ]);
  expect(issuesOf(chart({ query: query([{ agg: "p95" }]) }))).toEqual([
    "definition.widgets[0].query.measures[0].field: needs a numeric field",
  ]);
  expect(issuesOf(chart({ query: query([{ agg: "sum", field: "name" }]) }))).toEqual([
    "definition.widgets[0].query.measures[0].field: not a numeric field of this source",
  ]);
  expect(issuesOf(chart({ query: query([{ agg: "count_distinct" }]) }))).toEqual([
    "definition.widgets[0].query.measures[0].field: count_distinct needs a field",
  ]);
  expect(issuesOf(chart({ query: query([{ agg: "count_distinct", field: "tags" }]) }))).toEqual([
    "definition.widgets[0].query.measures[0].field: count_distinct does not take this field",
  ]);
  expect(issuesOf(chart({ query: query([{ agg: "count_distinct", field: "trace_id" }]) }))).toEqual(
    [],
  );
  expect(
    issuesOf(chart({ query: query([{ agg: "count_distinct", field: "metadata['user']" }]) })),
  ).toEqual([]);
});

it("refuses a bare map column as a field and checks map keys", () => {
  expect(issuesOf(chart({ query: query([{ agg: "count_distinct", field: "metadata" }]) }))).toEqual(
    ["definition.widgets[0].query.measures[0].field: count_distinct does not take this field"],
  );
  expect(
    issuesOf(
      chart({ query: query([{ agg: "count_distinct", field: `metadata['${"k".repeat(257)}']` }]) }),
    ),
  ).toEqual([
    "definition.widgets[0].query.measures[0].field: count_distinct does not take this field",
  ]);
});

it("needs a where for rate, and takes no field", () => {
  expect(issuesOf(chart({ query: query([{ agg: "rate", where: "  " }]) }))).toEqual([
    "definition.widgets[0].query.measures[0].where: rate needs a where",
  ]);
  expect(
    issuesOf(chart({ query: query([{ agg: "rate", field: "name", where: "status = 'error'" }]) })),
  ).toEqual(["definition.widgets[0].query.measures[0].field: rate takes no field"]);
  expect(issuesOf(chart({ query: query([{ agg: "rate", where: "error_count > 0" }]) }))).toEqual(
    [],
  );
});

it("checks the breakdown field and limit", () => {
  expect(
    issuesOf(chart({ query: query([{ agg: "count" }], { breakdown: { field: "duration_ns" } }) })),
  ).toEqual(["definition.widgets[0].query.breakdown.field: not a breakdown field of this source"]);
  expect(
    issuesOf(
      chart({ query: query([{ agg: "count" }], { breakdown: { field: "name", limit: 21 } }) }),
    ),
  ).toEqual(["definition.widgets[0].query.breakdown.limit: must be 1 to 20"]);
  expect(
    issuesOf(
      chart({ query: query([{ agg: "count" }], { breakdown: { field: "name", limit: 20 } }) }),
    ),
  ).toEqual([]);
});

it("checks measure labels: at most 60 characters", () => {
  expect(issuesOf(chart({ query: query([{ agg: "count", label: "l".repeat(61) }]) }))).toEqual([
    "definition.widgets[0].query.measures[0].label: must be at most 60 characters",
  ]);
});

it("reports every error at once, in path order", () => {
  const bad = chart({
    id: "bad id",
    title: "t".repeat(101),
    layout: { x: 59, y: 0, w: 2, h: 0 },
    query: query([{ agg: "count", field: "name" }, { agg: "rate" }], {
      breakdown: { field: "attributes" },
    }),
  });
  expect(issuesOf(bad)).toEqual([
    "definition.widgets[0].id: must be 1 to 64 characters of letters, digits, _ or -",
    "definition.widgets[0].title: must be at most 100 characters",
    "definition.widgets[0].layout.w: x + w must be at most 60",
    "definition.widgets[0].layout.h: must be 1 to 40",
    "definition.widgets[0].query.measures[0].field: count takes no field",
    "definition.widgets[0].query.measures[1].where: rate needs a where",
    "definition.widgets[0].query.breakdown.field: not a breakdown field of this source",
  ]);
});

it("names the dashboard fields first, then the definition, in one result", () => {
  const issues = validateDashboard({
    name: "",
    description: "d".repeat(501),
    definition: definitionOf(heading({ title: "" })),
  });
  expect(issues.map((issue) => issue.field)).toEqual([
    "name",
    "description",
    "definition.widgets[0].title",
  ]);
});
