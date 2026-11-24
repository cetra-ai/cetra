import { beforeEach, expect, it } from "vitest";
import type { Schema } from "@/api/types";
import type { Dashboard, Widget } from "@/lib/dashboards/types";
import { validateDashboard } from "@/lib/dashboards/validate";
import { dashboardHandlers, seedDashboards } from "@/mocks/dashboards";
import { createDataset, visibleTraces } from "@/mocks/generated/dataset";
import { largeHandlers } from "@/mocks/generated/handlers";
import { tracePredicate } from "@/mocks/generated/widget-query";
import { server } from "@/mocks/node";
import { STORY_DASHBOARD_IDS, storyDashboards } from "@/mocks/story-dashboards";

const NOW = Date.parse("2026-11-19T12:00:00.000Z");
const DAY_MS = 86_400_000;
const PROJECT = `${location.origin}/api/v1/projects/acme%2Fsupport-bot`;
const RANGE = {
  from: new Date(NOW - 30 * DAY_MS).toISOString(),
  to: new Date(NOW + DAY_MS).toISOString(),
};

type Body = Schema<"WidgetQueryRequest">;

const dataset = createDataset({ seed: 1, now: NOW, traces: 5000 });

beforeEach(() => {
  server.use(...largeHandlers(dataset));
});

function queryOf(widget: Widget): NonNullable<Widget["query"]> {
  if (widget.query === undefined) {
    throw new Error(`${widget.id} has no query`);
  }
  return widget.query;
}

/** Asks the large mock for one widget. `series` takes a daily bucket; `total` may compare. */
async function ask(
  widget: Widget,
  shape: "series" | "total",
  compare = false,
): Promise<{ status: number; json: unknown }> {
  const body: Partial<Body> & Pick<Body, "query"> = {
    ...RANGE,
    shape,
    query: queryOf(widget),
    ...(shape === "series" ? { bucket: "1d" } : {}),
    ...(compare ? { compare: true } : {}),
  };
  const response = await fetch(`${PROJECT}/dashboards/query`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
  });
  return { status: response.status, json: await response.json() };
}

function storyDashboard(id: string): Dashboard {
  const found = storyDashboards().find((dashboard) => dashboard.id === id);
  if (found === undefined) {
    throw new Error(`no story dashboard ${id}`);
  }
  return found;
}

function inRange(start: string): boolean {
  const time = Date.parse(start);
  return time >= Date.parse(RANGE.from) && time < Date.parse(RANGE.to);
}

/** The visible traces in the range that carry `tag`. */
function taggedTraces(tag: string) {
  return visibleTraces(dataset.traces, false, false).filter(
    (trace) => inRange(trace.start_time) && trace.tags.includes(tag),
  );
}

it("the story dashboards have the planned ids, names and widget titles", () => {
  expect(STORY_DASHBOARD_IDS).toEqual({
    answerQuality: "01900000-0000-7000-8000-000000570401",
    codeAgentHealth: "01900000-0000-7000-8000-000000570402",
    voiceLatency: "01900000-0000-7000-8000-000000570403",
    invoiceExtraction: "01900000-0000-7000-8000-000000570404",
  });
  const dashboards = storyDashboards();
  expect(dashboards.map((dashboard) => dashboard.id)).toEqual([
    "01900000-0000-7000-8000-000000570401",
    "01900000-0000-7000-8000-000000570402",
    "01900000-0000-7000-8000-000000570403",
    "01900000-0000-7000-8000-000000570404",
  ]);
  expect(new Set(dashboards.map((dashboard) => dashboard.id)).size).toBe(4);
  expect(dashboards.map((dashboard) => dashboard.name)).toEqual([
    "Answer quality",
    "Code agent health",
    "Voice latency",
    "Invoice extraction",
  ]);
  expect(dashboards.map((dashboard) => dashboard.definition.widgets.map((w) => w.title))).toEqual([
    ["Correctness by day", "Answers per day", "Cost per answer", "Answers by model"],
    ["Failed runs per day", "Error rate", "Cost per run", "p95 duration"],
    ["p95 reply time", "Reply time by day", "Calls over 3 s", "Calls per day"],
    ["Invoices per day", "Schema check failures", "Cost per invoice", "Invoices by model"],
  ]);
  for (const dashboard of dashboards) {
    for (const widget of dashboard.definition.widgets) {
      expect([0, 30]).toContain(widget.layout.x);
      expect(widget.layout.w).toBe(30);
    }
  }
});

it("every story dashboard passes the dashboard validator", () => {
  for (const dashboard of storyDashboards()) {
    expect(dashboard.description?.length ?? 0).toBeLessThanOrEqual(500);
    expect(
      validateDashboard({
        name: dashboard.name,
        description: dashboard.description,
        definition: dashboard.definition,
      }),
    ).toEqual([]);
  }
});

it("storyDashboards returns fresh objects", () => {
  const first = storyDashboards();
  const firstWidgets = first[0]?.definition.widgets;
  if (firstWidgets === undefined) {
    throw new Error("no widgets");
  }
  firstWidgets.pop();
  first[0] = { ...first[0], name: "changed" } as Dashboard;
  const second = storyDashboards();
  expect(second[0]?.name).toBe("Answer quality");
  expect(second[0]?.definition.widgets).toHaveLength(4);
});

it("seedDashboards still returns only the fixture dashboard", () => {
  expect(seedDashboards()).toHaveLength(1);
});

it("the browser mock store lists the story dashboards after the fixture one", async () => {
  server.use(...dashboardHandlers([...seedDashboards(), ...storyDashboards()]));
  const list = await fetch(`${PROJECT}/dashboards`);
  const body = (await list.json()) as { data: { name: string }[] };
  expect(body.data.map((dashboard) => dashboard.name)).toEqual([
    "Answer quality",
    "Code agent health",
    "Invoice extraction",
    "Support overview",
    "Voice latency",
  ]);
  for (const dashboard of storyDashboards()) {
    const one = await fetch(`${PROJECT}/dashboards/${dashboard.id}`);
    const stored = (await one.json()) as Dashboard;
    expect(stored.definition.widgets).toHaveLength(4);
    expect(stored.name).toBe(dashboard.name);
  }
});

it("the large mock answers every widget of every story dashboard with data", async () => {
  for (const dashboard of storyDashboards()) {
    for (const widget of dashboard.definition.widgets) {
      const type = widget.chart?.type;
      if (type === "bar_h") {
        const answer = await ask(widget, "total");
        expect(answer.status).toBe(200);
        const body = answer.json as { shape: string; rows: { values: unknown[] }[] };
        expect(body.shape).toBe("total");
        expect(body.rows.length).toBeGreaterThanOrEqual(2);
        expect(typeof body.rows[0]?.values[0]).toBe("number");
        continue;
      }
      if (type === "number") {
        const total = await ask(widget, "total", true);
        expect(total.status).toBe(200);
        const body = total.json as { rows: { values: unknown[] }[] };
        expect(body.rows.length).toBeGreaterThanOrEqual(1);
        expect(typeof body.rows[0]?.values[0]).toBe("number");
      }
      const answer = await ask(widget, "series");
      expect(answer.status).toBe(200);
      const body = answer.json as {
        shape: string;
        series: { points: { value?: unknown }[] }[];
      };
      expect(body.shape).toBe("series");
      const counted = body.series.some((series) =>
        series.points.some((point) => typeof point.value === "number"),
      );
      expect(counted).toBe(true);
    }
  }
});

it("the tag filter reaches only tagged traces", async () => {
  const calls = storyDashboard(STORY_DASHBOARD_IDS.voiceLatency).definition.widgets[3];
  if (calls === undefined) {
    throw new Error("no widget");
  }
  const answer = await ask(calls, "series");
  const body = answer.json as { series: { points: { value?: number }[] }[] };
  const counted = body.series
    .flatMap((series) => series.points)
    .reduce((sum, point) => sum + (point.value ?? 0), 0);
  expect(counted).toBe(taggedTraces("voice").length);
});

it("the rate widgets match the dataset", async () => {
  const code = storyDashboard(STORY_DASHBOARD_IDS.codeAgentHealth).definition.widgets[1];
  const voice = storyDashboard(STORY_DASHBOARD_IDS.voiceLatency).definition.widgets[2];
  if (code === undefined || voice === undefined) {
    throw new Error("no widget");
  }
  const retry = taggedTraces("retry");
  const errorRate = (await ask(code, "total")).json as { rows: { values: number[] }[] };
  expect(errorRate.rows[0]?.values[0]).toBeCloseTo(
    retry.filter((trace) => trace.error_count > 0).length / retry.length,
    6,
  );

  const voiceCalls = taggedTraces("voice");
  const overThree = (await ask(voice, "total")).json as { rows: { values: number[] }[] };
  expect(overThree.rows[0]?.values[0]).toBeCloseTo(
    voiceCalls.filter((trace) => trace.duration_ns > 3_000_000_000).length / voiceCalls.length,
    6,
  );
});

it("the hero traces are counted", () => {
  // Each story's second widget carries its tag filter (the scores widget is the first one).
  const heroes: [number, string][] = [
    [3, STORY_DASHBOARD_IDS.answerQuality],
    [4, STORY_DASHBOARD_IDS.codeAgentHealth],
    [5, STORY_DASHBOARD_IDS.voiceLatency],
    [6, STORY_DASHBOARD_IDS.invoiceExtraction],
  ];
  for (const [index, id] of heroes) {
    const second = storyDashboard(id).definition.widgets[1];
    if (second === undefined) {
      throw new Error(`no second widget in ${id}`);
    }
    const filter = queryOf(second).filter;
    const predicate = filter === undefined ? undefined : tracePredicate(filter);
    const trace = dataset.traces[index];
    expect(trace).toBeDefined();
    if (trace === undefined) {
      throw new Error(`no trace ${index}`);
    }
    expect(predicate?.(trace)).toBe(true);
  }
});
