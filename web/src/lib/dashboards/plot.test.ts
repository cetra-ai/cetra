import { describe, expect, it } from "vitest";
import type { Schema } from "@/api/types";
import { groupClause, seriesPlot, totalTable } from "./plot";
import type { WidgetQuery } from "./types";

type Series = Schema<"WidgetSeriesResponse">;
type Total = Schema<"WidgetTotalResponse">;

const H1 = "2026-01-01T10:00:00.000000000Z";
const H2 = "2026-01-01T10:10:00.000000000Z";
const H3 = "2026-01-01T10:30:00.000000000Z";

const answer = (series: Series["series"]): Series => ({ bucket_ns: 600_000_000_000, series });

describe("seriesPlot", () => {
  it("names a plain measure by its label, and keys it by its index", () => {
    const query: WidgetQuery = {
      source: "traces",
      measures: [{ agg: "count", label: "Traces" }],
    };
    const plot = seriesPlot(
      query,
      answer([{ measure: 0, other: false, points: [{ time: H1, value: 2, rows: 2 }] }]),
    );
    expect(plot.columns).toEqual([
      { key: "m0", label: "Traces", kind: "count", color: "var(--chart-1)", group: undefined },
    ]);
    expect(plot.bucketMs).toBe(600_000);
  });

  it("gives an error measure the error colour and labels a group", () => {
    const query: WidgetQuery = {
      source: "traces",
      measures: [{ agg: "count", where: "status = 'error'", label: "Errors" }],
      breakdown: { field: "name" },
    };
    const plot = seriesPlot(
      query,
      answer([
        { measure: 0, group: "support", other: false, points: [{ time: H1, value: 1, rows: 1 }] },
        { measure: 0, other: true, points: [{ time: H1, value: 4, rows: 4 }] },
      ]),
    );
    expect(plot.columns.map((column) => [column.key, column.label, column.color])).toEqual([
      ["m0:support", "support", "var(--chart-error)"],
      ["m0:\u001fother", "Other", "var(--chart-error)"],
    ]);
    expect(plot.columns.map((column) => column.group)).toEqual(["support", null]);
  });

  it("labels a group by its measure too when the query has several measures", () => {
    const query: WidgetQuery = {
      source: "spans",
      measures: [
        { agg: "sum", field: "cost_total", label: "Cost" },
        { agg: "count", label: "Calls" },
      ],
      breakdown: { field: "model" },
    };
    const plot = seriesPlot(
      query,
      answer([
        { measure: 0, group: "model-a", other: false, points: [{ time: H1, value: 1, rows: 1 }] },
        { measure: 1, other: true, points: [{ time: H1, value: 4, rows: 4 }] },
      ]),
    );
    expect(plot.columns.map((column) => column.label)).toEqual(["Cost · model-a", "Calls · Other"]);
  });

  it("colours a model by its place in the ranking, and keeps it when a chart shows fewer", () => {
    const query: WidgetQuery = {
      source: "spans",
      measures: [{ agg: "sum", field: "cost_total", label: "Cost" }],
      breakdown: { field: "model" },
    };
    const plot = seriesPlot(
      query,
      answer([
        { measure: 0, group: "gpt-4o", other: false, points: [{ time: H1, value: 1, rows: 1 }] },
        { measure: 0, group: "claude", other: false, points: [{ time: H1, value: 2, rows: 1 }] },
      ]),
      { ranking: ["claude", "gpt-4o", "mistral"] },
    );
    expect(plot.columns.map((column) => column.color)).toEqual([
      "var(--chart-2)",
      "var(--chart-1)",
    ]);
    expect(plot.columns[0]?.label).toBe("gpt-4o");
  });

  it("fills the buckets between points, with gaps and the rows of each bucket", () => {
    const query: WidgetQuery = {
      source: "traces",
      measures: [{ agg: "p95", field: "duration_ns" }],
    };
    const plot = seriesPlot(
      query,
      answer([
        {
          measure: 0,
          other: false,
          points: [
            { time: H1, value: 5, rows: 3 },
            { time: H3, value: 9, rows: 1 },
          ],
        },
      ]),
    );
    expect(plot.rows.map((row) => row.time)).toEqual([
      Date.parse(H1),
      Date.parse(H1) + 600_000,
      Date.parse(H1) + 1_200_000,
      Date.parse(H3),
    ]);
    expect(plot.rows[0]).toMatchObject({ m0: 5, m0__rows: 3 });
    expect(plot.rows[1]).toMatchObject({ m0: null, m0__rows: null });
    expect(plot.rows.at(-1)).toMatchObject({ m0: 9, m0__rows: 1 });
  });

  it("draws a sum or a count at 0 in a bucket without rows, and keeps a gap for an average", () => {
    const query: WidgetQuery = {
      source: "traces",
      measures: [
        { agg: "sum", field: "cost_total" },
        { agg: "count" },
        { agg: "count_distinct", field: "user_id" },
        { agg: "avg", field: "cost_total" },
      ],
    };
    const points = [
      { time: H1, value: 2, rows: 2 },
      { time: H3, value: 4, rows: 1 },
    ];
    const plot = seriesPlot(
      query,
      answer([0, 1, 2, 3].map((measure) => ({ measure, other: false, points }))),
    );
    expect(plot.rows[1]).toMatchObject({
      m0: 0,
      m0__rows: 0,
      m1: 0,
      m1__rows: 0,
      m2: 0,
      m2__rows: 0,
      m3: null,
      m3__rows: null,
    });
  });

  it("draws a missing group of a stacked sum at 0 where another group has rows", () => {
    const query: WidgetQuery = {
      source: "traces",
      measures: [{ agg: "sum", field: "cost_total" }],
      breakdown: { field: "release", limit: 5, other: false },
    };
    const plot = seriesPlot(
      query,
      answer([
        { measure: 0, group: "v1", other: false, points: [{ time: H1, value: 1, rows: 1 }] },
        { measure: 0, group: "v2", other: false, points: [{ time: H3, value: 3, rows: 1 }] },
      ]),
    );
    const [v1, v2] = plot.columns.map((column) => column.key);
    expect(plot.rows[0]?.[v2 ?? ""]).toBe(0);
    expect(plot.rows.at(-1)?.[v1 ?? ""]).toBe(0);
  });

  it("takes the buckets of every series", () => {
    const query: WidgetQuery = { source: "traces", measures: [{ agg: "count" }] };
    const plot = seriesPlot(
      query,
      answer([
        { measure: 0, other: false, points: [{ time: H1, value: 1, rows: 1 }] },
        { measure: 0, group: "b", other: false, points: [{ time: H2, value: 1, rows: 1 }] },
      ]),
    );
    expect(plot.rows).toHaveLength(2);
  });
});

describe("totalTable", () => {
  const query: WidgetQuery = {
    source: "spans",
    measures: [
      { agg: "sum", field: "cost_total", label: "Cost" },
      { agg: "count", label: "LLM calls" },
    ],
    breakdown: { field: "model" },
  };
  const total: Total = {
    rows: [
      { group: "a", other: false, values: [3, 10] },
      { group: "b", other: false, values: [1, 2] },
      { other: true, values: [0, 1] },
    ],
  };

  it("gives one column per measure and each row's share of the first measure", () => {
    const table = totalTable(query, total, true);
    expect(table.columns.map((column) => column.label)).toEqual(["Cost", "LLM calls"]);
    expect(table.rows.map((row) => row.share)).toEqual([0.75, 0.25, 0]);
    expect(table.rows[2]).toMatchObject({ group: null, other: true });
  });

  it("has no shares without `share`", () => {
    expect(totalTable(query, total, false).rows[0]?.share).toBeUndefined();
  });
});

describe("groupClause", () => {
  it("keeps the rows of a model: a list on traces, a column on spans", () => {
    expect(groupClause("traces", "model", "gpt-4o")).toBe("models @> ['gpt-4o']");
    expect(groupClause("spans", "model", "gpt-4o")).toBe("model = 'gpt-4o'");
    expect(groupClause("traces", "environment", "prod")).toBe("environment = 'prod'");
  });
});
