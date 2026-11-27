import { expect, it } from "vitest";
import type { Schema } from "@/api/types";
import {
  autoChartConfig,
  buildChart,
  CHART_MAX_POINTS,
  type ChartSpec,
  chartColumns,
  chartLabel,
  chartSpec,
} from "@/lib/sql/chart";

const TS = "timestamp";

function result(
  columns: { name: string; type: string }[],
  rows: Record<string, unknown>[],
): Schema<"SqlQueryResult"> {
  return { columns, rows: rows as Schema<"SqlQueryResult">["rows"], truncated: false };
}

function specOf(document: Schema<"SqlQueryResult">): ChartSpec {
  const spec = chartSpec(document);
  if (spec === null) {
    throw new Error("expected a chart");
  }
  return spec;
}

it("draws a line over the first timestamp column, sorted by time", () => {
  const spec = specOf(
    result(
      [
        { name: "day", type: TS },
        { name: "cost", type: "float" },
      ],
      [
        { day: "2026-01-02T00:00:00.000000000Z", cost: 2 },
        { day: "2026-01-01T00:00:00.000000000Z", cost: 1 },
      ],
    ),
  );
  expect(spec.kind).toBe("line");
  expect(spec.x).toEqual({ name: "day", kind: "timestamp" });
  expect(spec.series).toEqual(["cost"]);
  expect(spec.data).toEqual([
    { __x: "2026-01-01T00:00:00.000000000Z", cost: 1 },
    { __x: "2026-01-02T00:00:00.000000000Z", cost: 2 },
  ]);
  expect(spec.clipped).toBe(false);
  expect(chartLabel(spec)).toBe("Line chart of cost by day");
});

it("pivots a single numeric column by a text column", () => {
  const d1 = "2026-01-01T00:00:00.000000000Z";
  const d2 = "2026-01-02T00:00:00.000000000Z";
  const spec = specOf(
    result(
      [
        { name: "day", type: TS },
        { name: "model", type: "string" },
        { name: "cost", type: "float" },
      ],
      [
        { day: d1, model: "a", cost: 1 },
        { day: d1, model: "b", cost: 2 },
        { day: d2, model: "a", cost: 3 },
      ],
    ),
  );
  expect(spec).toEqual({
    kind: "line",
    x: { name: "day", kind: "timestamp" },
    series: ["a", "b"],
    pivot: { column: "model", measure: "cost" },
    data: [
      { __x: d1, a: 1, b: 2 },
      { __x: d2, a: 3 },
    ],
    clipped: false,
  });
  expect(chartLabel(spec)).toBe("Line chart of cost by day, one series per model");
});

it("does not pivot with two numeric columns", () => {
  const spec = specOf(
    result(
      [
        { name: "day", type: TS },
        { name: "model", type: "string" },
        { name: "a", type: "integer" },
        { name: "b", type: "integer" },
      ],
      [{ day: "2026-01-01T00:00:00.000000000Z", model: "m", a: 1, b: 2 }],
    ),
  );
  expect(spec.pivot).toBeUndefined();
  expect(spec.series).toEqual(["a", "b"]);
});

it("does not pivot when the second numeric column is only dropped as unsafe", () => {
  const spec = specOf(
    result(
      [
        { name: "day", type: TS },
        { name: "model", type: "string" },
        { name: "cost", type: "float" },
        { name: "__proto__", type: "float" },
      ],
      [{ day: "2026-01-01T00:00:00.000000000Z", model: "m", cost: 1, ["__proto__"]: 9 }],
    ),
  );
  expect(spec.pivot).toBeUndefined();
  expect(spec.series).toEqual(["cost"]);
  expect(spec.clipped).toBe(true);
});

it("keeps a dotted pivot value as its own series key", () => {
  const spec = specOf(
    result(
      [
        { name: "day", type: TS },
        { name: "model", type: "string" },
        { name: "cost", type: "float" },
      ],
      [{ day: "2026-01-01T00:00:00.000000000Z", model: "gpt-4.1", cost: 1 }],
    ),
  );
  expect(spec.pivot).toEqual({ column: "model", measure: "cost" });
  expect(spec.series).toEqual(["gpt-4.1"]);
  expect(spec.data).toEqual([{ __x: "2026-01-01T00:00:00.000000000Z", "gpt-4.1": 1 }]);
});

it("draws a bar chart over a text column", () => {
  const spec = specOf(
    result(
      [
        { name: "name", type: "string" },
        { name: "traces", type: "integer" },
        { name: "cost", type: "float" },
      ],
      [
        { name: "a", traces: 1, cost: 0.5 },
        { name: "b", traces: 2, cost: 1.5 },
      ],
    ),
  );
  expect(spec.kind).toBe("bar");
  expect(spec.series).toEqual(["traces", "cost"]);
  expect(chartLabel(spec)).toBe("Bar chart of traces, cost by name");
});

it("returns null without an x or a numeric column", () => {
  expect(chartSpec(result([{ name: "a", type: "string" }], [{ a: "x" }]))).toBeNull();
  expect(chartSpec(result([{ name: "n", type: "integer" }], [{ n: 1 }]))).toBeNull();
});

it("clips series, points and pivot values", () => {
  const seven = specOf(
    result(
      [
        { name: "name", type: "string" },
        ...["a", "b", "c", "d", "e", "f", "g"].map((name) => ({ name, type: "integer" })),
      ],
      [{ name: "x", a: 1, b: 1, c: 1, d: 1, e: 1, f: 1, g: 1 }],
    ),
  );
  expect(seven.series).toEqual(["a", "b", "c", "d", "e"]);
  expect(seven.clipped).toBe(true);

  const many = specOf(
    result(
      [
        { name: "day", type: TS },
        { name: "n", type: "integer" },
      ],
      Array.from({ length: 1001 }, (_, index) => ({
        day: new Date(Date.UTC(2026, 0, 1, 0, 0, index)).toISOString(),
        n: index,
      })),
    ),
  );
  expect(many.data).toHaveLength(CHART_MAX_POINTS);
  expect(many.clipped).toBe(true);

  const pivot = specOf(
    result(
      [
        { name: "day", type: TS },
        { name: "model", type: "string" },
        { name: "cost", type: "float" },
      ],
      ["a", "b", "c", "d", "e", "f"].map((model, index) => ({
        day: "2026-01-01T00:00:00.000000000Z",
        model,
        cost: index,
      })),
    ),
  );
  expect(pivot.series).toEqual(["a", "b", "c", "d", "e"]);
  expect(pivot.clipped).toBe(true);
});

it("reads decimal strings and keeps null", () => {
  const spec = specOf(
    result(
      [
        { name: "day", type: TS },
        { name: "amount", type: "decimal" },
      ],
      [
        { day: "2026-01-01T00:00:00.000000000Z", amount: "0.25" },
        { day: "2026-01-02T00:00:00.000000000Z", amount: null },
      ],
    ),
  );
  expect(spec.data).toEqual([
    { __x: "2026-01-01T00:00:00.000000000Z", amount: 0.25 },
    { __x: "2026-01-02T00:00:00.000000000Z", amount: null },
  ]);
});

it("treats a date column as a date axis and sorts it", () => {
  const spec = specOf(
    result(
      [
        { name: "d", type: "date" },
        { name: "n", type: "integer" },
      ],
      [
        { d: "2026-01-02", n: 2 },
        { d: "2026-01-01", n: 1 },
      ],
    ),
  );
  expect(spec.x).toEqual({ name: "d", kind: "date" });
  expect(spec.kind).toBe("line");
  expect(spec.data.map((point) => point.n)).toEqual([1, 2]);
});

it("sorts an unparseable timestamp last", () => {
  const spec = specOf(
    result(
      [
        { name: "day", type: TS },
        { name: "n", type: "integer" },
      ],
      [
        { day: "not a time", n: 3 },
        { day: "2026-01-01T00:00:00.000000000Z", n: 1 },
      ],
    ),
  );
  expect(spec.data.map((point) => point.n)).toEqual([1, 3]);
});

it("keeps odd names as series keys and drops unsafe ones", () => {
  const odd = specOf(
    result(
      [
        { name: "day", type: TS },
        { name: "model", type: "string" },
        { name: "cost", type: "float" },
      ],
      [{ day: "2026-01-01T00:00:00.000000000Z", model: "a;}b{", cost: 1 }],
    ),
  );
  expect(odd.series).toEqual(["a;}b{"]);
  expect(odd.data).toEqual([{ __x: "2026-01-01T00:00:00.000000000Z", "a;}b{": 1 }]);

  const plainX = specOf(
    result(
      [
        { name: "day", type: TS },
        { name: "x", type: "integer" },
      ],
      [{ day: "2026-01-01T00:00:00.000000000Z", x: 4 }],
    ),
  );
  expect(plainX.series).toEqual(["x"]);
  expect(plainX.data).toEqual([{ __x: "2026-01-01T00:00:00.000000000Z", x: 4 }]);

  const unsafe = specOf(
    result(
      [
        { name: "day", type: TS },
        { name: "model", type: "string" },
        { name: "cost", type: "float" },
      ],
      [
        { day: "2026-01-01T00:00:00.000000000Z", model: "__proto__", cost: 1 },
        { day: "2026-01-01T00:00:00.000000000Z", model: "safe", cost: 2 },
      ],
    ),
  );
  expect(unsafe.series).toEqual(["safe"]);
  expect(unsafe.clipped).toBe(true);

  const nullPivot = specOf(
    result(
      [
        { name: "day", type: TS },
        { name: "model", type: "string" },
        { name: "cost", type: "float" },
      ],
      [{ day: "2026-01-01T00:00:00.000000000Z", model: null, cost: 1 }],
    ),
  );
  expect(nullPivot.series).toEqual(["NULL"]);
  expect(nullPivot.data).toEqual([{ __x: "2026-01-01T00:00:00.000000000Z", NULL: 1 }]);
});

const MODEL_ROWS = [
  { day: "2026-01-01T00:00:00.000000000Z", model: "a", cost: 1, traces: 4 },
  { day: "2026-01-02T00:00:00.000000000Z", model: "b", cost: 2, traces: 5 },
];
const MODEL_COLUMNS = [
  { name: "day", type: TS },
  { name: "model", type: "string" },
  { name: "cost", type: "float" },
  { name: "traces", type: "integer" },
];

it("autoChartConfig is the automatic choice: line over time, pivot when one measure", () => {
  expect(
    autoChartConfig(
      result(
        [
          { name: "day", type: TS },
          { name: "model", type: "string" },
          { name: "cost", type: "float" },
        ],
        MODEL_ROWS,
      ),
    ),
  ).toEqual({ kind: "line", x: "day", y: ["cost"], splitBy: "model" });
  expect(
    autoChartConfig(
      result(
        [
          { name: "day", type: TS },
          { name: "cost", type: "float" },
          { name: "traces", type: "integer" },
        ],
        [{ day: "2026-01-01T00:00:00.000000000Z", cost: 1, traces: 2 }],
      ),
    ),
  ).toEqual({ kind: "line", x: "day", y: ["cost", "traces"], splitBy: null });
  expect(
    autoChartConfig(
      result(
        [
          { name: "name", type: "string" },
          { name: "n", type: "integer" },
        ],
        [],
      ),
    ),
  ).toEqual({ kind: "bar", x: "name", y: ["n"], splitBy: null });
});

it("autoChartConfig is null without an x or a safe numeric column", () => {
  expect(autoChartConfig(result([{ name: "a", type: "string" }], []))).toBeNull();
  expect(autoChartConfig(result([{ name: "n", type: "integer" }], []))).toBeNull();
});

it("buildChart draws a bar chart over time in row order", () => {
  const spec = buildChart(result(MODEL_COLUMNS, [...MODEL_ROWS].reverse()), {
    kind: "bar",
    x: "day",
    y: ["cost"],
    splitBy: null,
  });
  expect(spec).toEqual({
    kind: "bar",
    x: { name: "day", kind: "timestamp" },
    series: ["cost"],
    data: [
      { __x: "2026-01-02T00:00:00.000000000Z", cost: 2 },
      { __x: "2026-01-01T00:00:00.000000000Z", cost: 1 },
    ],
    clipped: false,
  });
});

it("buildChart sorts a line over a numeric x by value, nulls last", () => {
  const spec = buildChart(
    result(
      [
        { name: "n", type: "integer" },
        { name: "cost", type: "float" },
      ],
      [
        { n: 10, cost: 3 },
        { n: null, cost: 9 },
        { n: 2, cost: 1 },
      ],
    ),
    { kind: "line", x: "n", y: ["cost"], splitBy: null },
  );
  expect(spec?.x).toEqual({ name: "n", kind: "number" });
  expect(spec?.data).toEqual([
    { __x: 2, cost: 1 },
    { __x: 10, cost: 3 },
    { __x: null, cost: 9 },
  ]);
});

it("buildChart keeps row order for a line over a text x", () => {
  const spec = buildChart(
    result(
      [
        { name: "name", type: "string" },
        { name: "cost", type: "float" },
      ],
      [
        { name: "b", cost: 2 },
        { name: "a", cost: 1 },
      ],
    ),
    { kind: "line", x: "name", y: ["cost"], splitBy: null },
  );
  expect(spec?.data.map((point) => point.__x)).toEqual(["b", "a"]);
});

it("buildChart pivots a bar chart by a text column with one y", () => {
  const spec = buildChart(result(MODEL_COLUMNS, MODEL_ROWS), {
    kind: "bar",
    x: "day",
    y: ["cost"],
    splitBy: "model",
  });
  if (spec === null) {
    throw new Error("expected a chart");
  }
  expect(spec.kind).toBe("bar");
  expect(spec.pivot).toEqual({ column: "model", measure: "cost" });
  expect(spec.series).toEqual(["a", "b"]);
  expect(chartLabel(spec)).toBe("Bar chart of cost by day, one series per model");
});

it("buildChart ignores splitBy when there are several y", () => {
  const spec = buildChart(result(MODEL_COLUMNS, MODEL_ROWS), {
    kind: "line",
    x: "day",
    y: ["cost", "traces"],
    splitBy: "model",
  });
  expect(spec?.pivot).toBeUndefined();
  expect(spec?.series).toEqual(["cost", "traces"]);
  expect(spec?.data).toEqual([
    { __x: "2026-01-01T00:00:00.000000000Z", cost: 1, traces: 4 },
    { __x: "2026-01-02T00:00:00.000000000Z", cost: 2, traces: 5 },
  ]);
});

it("buildChart is null for a missing x, no y, a non-numeric y, or a bad splitBy", () => {
  const base = { kind: "line" as const, x: "day", y: ["cost"], splitBy: null };
  expect(buildChart(result(MODEL_COLUMNS, MODEL_ROWS), { ...base, x: "missing" })).toBeNull();
  expect(buildChart(result(MODEL_COLUMNS, MODEL_ROWS), { ...base, y: [] })).toBeNull();
  expect(buildChart(result(MODEL_COLUMNS, MODEL_ROWS), { ...base, y: ["model"] })).toBeNull();
  expect(buildChart(result(MODEL_COLUMNS, MODEL_ROWS), { ...base, splitBy: "cost" })).toBeNull();
});

it("chartColumns lists x, y and split-by candidates", () => {
  expect(
    chartColumns(
      result(
        [
          ...MODEL_COLUMNS,
          { name: "d", type: "date" },
          { name: "__proto__", type: "float" },
          { name: "flag", type: "boolean" },
        ],
        [],
      ),
    ),
  ).toEqual({
    x: ["day", "model", "cost", "traces", "d", "__proto__"],
    y: ["cost", "traces"],
    splitBy: ["model"],
  });
});
