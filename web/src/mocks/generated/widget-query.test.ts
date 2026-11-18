import { beforeEach, expect, it } from "vitest";
import type { Schema } from "@/api/types";
import { createDataset, visibleTraces } from "@/mocks/generated/dataset";
import { largeHandlers } from "@/mocks/generated/handlers";
import { server } from "@/mocks/node";

const NOW = Date.parse("2026-06-01T00:00:00.000Z");
const DAY_MS = 86_400_000;
const URL_PATH = `${location.origin}/api/v1/projects/acme%2Fsupport-bot/dashboards/query`;
const RANGE = {
  from: new Date(NOW - 30 * DAY_MS).toISOString(),
  to: new Date(NOW + DAY_MS).toISOString(),
};

type Body = Schema<"WidgetQueryRequest">;

async function ask(
  body: Partial<Body> & Pick<Body, "query">,
): Promise<{ status: number; json: unknown }> {
  const response = await fetch(URL_PATH, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ ...RANGE, shape: "total", ...body }),
  });
  return { status: response.status, json: await response.json() };
}

const dataset = createDataset({ seed: 3, now: NOW, traces: 300 });
beforeEach(() => {
  server.use(...largeHandlers(dataset));
});

function inRange(start: string): boolean {
  const time = Date.parse(start);
  return time >= Date.parse(RANGE.from) && time < Date.parse(RANGE.to);
}

it("counts the visible traces of the range in every bucket", async () => {
  const visible = visibleTraces(dataset.traces, false, false).filter((trace) =>
    inRange(trace.start_time),
  );
  const answer = await ask({
    shape: "series",
    bucket: "1d",
    query: { source: "traces", measures: [{ agg: "count" }] },
  });
  const body = answer.json as Schema<"WidgetSeriesResponse"> & { shape: string };
  expect(answer.status).toBe(200);
  expect(body.shape).toBe("series");
  const counted = body.series
    .flatMap((series) => series.points)
    .reduce((sum, point) => sum + (point.value ?? 0), 0);
  expect(counted).toBe(visible.length);
});

it("keeps the largest groups of a breakdown and sums the rest into other", async () => {
  const all = await ask({ query: { source: "traces", measures: [{ agg: "count" }] } });
  const total = (all.json as { rows: { values: number[] }[] }).rows[0]?.values[0] ?? 0;
  const split = await ask({
    query: {
      source: "traces",
      measures: [{ agg: "count" }],
      breakdown: { field: "status", limit: 1, other: true },
    },
  });
  const rows = (split.json as { rows: { group?: string; other: boolean; values: number[] }[] })
    .rows;
  expect(rows).toHaveLength(2);
  expect(rows[0]?.other).toBe(false);
  expect(rows[1]).toMatchObject({ other: true });
  expect((rows[0]?.values[0] ?? 0) + (rows[1]?.values[0] ?? 0)).toBe(total);
});

it("answers a rate as the share of the rows that match its condition", async () => {
  const visible = visibleTraces(dataset.traces, false, false).filter((trace) =>
    inRange(trace.start_time),
  );
  const errors = visible.filter((trace) => trace.status === "error").length;
  const answer = await ask({
    query: { source: "traces", measures: [{ agg: "rate", where: "status = 'error'" }] },
  });
  const rows = (answer.json as { rows: { values: number[] }[] }).rows;
  expect(rows[0]?.values[0]).toBeCloseTo(errors / visible.length, 10);
});

it("answers a total with its previous range when asked to compare", async () => {
  const answer = await ask({
    compare: true,
    query: { source: "traces", measures: [{ agg: "count" }] },
  });
  const rows = (answer.json as { rows: { values: number[]; previous: number[] }[] }).rows;
  expect(rows[0]?.previous).toHaveLength(1);
  expect(rows[0]?.previous[0]).toBeGreaterThanOrEqual(0);
});

it("splits LLM spans by model, with no time to first token", async () => {
  const llm = await ask({
    query: {
      source: "spans",
      filter: "span_kind = 'llm' AND model IS NOT NULL",
      measures: [{ agg: "sum", field: "cost_total", label: "Cost" }],
      breakdown: { field: "model", limit: 2, other: false },
    },
  });
  const rows = (llm.json as { rows: { group?: string; values: number[] }[] }).rows;
  expect(rows.length).toBeGreaterThan(0);
  expect(rows.length).toBeLessThanOrEqual(2);
  const ttft = await ask({
    query: {
      source: "spans",
      filter: "span_kind = 'llm' AND ttft_ns IS NOT NULL",
      measures: [{ agg: "p50", field: "ttft_ns" }],
    },
  });
  expect((ttft.json as { rows: { values: unknown[] }[] }).rows[0]?.values[0]).toBeNull();
});

it("lets a filter that names environment reach the evaluation traces", async () => {
  const evaluation = visibleTraces(dataset.traces, true, false).filter(
    (trace) => trace.environment === "cetra-eval" && inRange(trace.start_time),
  ).length;
  const answer = await ask({
    filter: "environment = 'cetra-eval'",
    query: { source: "traces", measures: [{ agg: "count" }] },
  });
  expect((answer.json as { rows: { values: number[] }[] }).rows[0]?.values[0]).toBe(evaluation);
});

it("refuses a filter it cannot evaluate, with invalid_filter", async () => {
  const answer = await ask({
    query: { source: "traces", filter: "metadata['team'] = 'a'", measures: [{ agg: "count" }] },
  });
  expect(answer.status).toBe(400);
  expect(answer.json).toMatchObject({
    code: "invalid_filter",
    detail: "The mock cannot evaluate this filter.",
    errors: [{ field: "query.filter" }],
  });
});

it("ranks a dashboard split's groups by the first measure, keeps 20 and has no other group", async () => {
  // Six models are too few to pass the limit: every trace gets one of thirty.
  const wide = {
    ...dataset,
    traces: dataset.traces.map((trace, index) => ({
      ...trace,
      models: [`model-${String(index % 30).padStart(2, "0")}`],
    })),
  };
  server.use(...largeHandlers(wide));
  const measures = [{ agg: "sum" as const, field: "cost_total" }];
  const split = await ask({ group_by: "model", query: { source: "spans", measures } });
  const rows = (split.json as { rows: { group?: string; other: boolean; values: number[] }[] })
    .rows;
  expect(rows).toHaveLength(20);
  expect(rows.some((row) => row.other)).toBe(false);
  // The split is the breakdown of the twenty largest groups, with no other group.
  const breakdown = await ask({
    query: {
      source: "spans",
      measures,
      breakdown: { field: "model", limit: 20, other: false },
    },
  });
  expect(rows).toEqual((breakdown.json as { rows: unknown[] }).rows);
  const values = rows.map((row) => row.values[0] ?? 0);
  expect([...values].sort((left, right) => right - left)).toEqual(values);
});
