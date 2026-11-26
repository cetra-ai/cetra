import { beforeEach, expect, it } from "vitest";
import type { Schema } from "@/api/types";
import type { Dashboard, Widget } from "@/lib/dashboards/types";
import { validateDashboard } from "@/lib/dashboards/validate";
import { dashboardHandlers, seedDashboards } from "@/mocks/dashboards";
import {
  createDataset,
  type Dataset,
  type Profile,
  SUPPORT_PROFILE,
  visibleTraces,
} from "@/mocks/generated/dataset";
import { largeHandlers } from "@/mocks/generated/handlers";
import { CODING_PROFILE, EXTRACTION_PROFILE, VOICE_PROFILE } from "@/mocks/generated/profiles";
import { tracePredicate } from "@/mocks/generated/widget-query";
import { server } from "@/mocks/node";
import { type StoryId, storyProject } from "@/mocks/projects";
import {
  STORY_DASHBOARD_IDS,
  STORY_DASHBOARD_STORIES,
  storyDashboards,
} from "@/mocks/story-dashboards";

const NOW = Date.parse("2026-11-19T12:00:00.000Z");
const DAY_MS = 86_400_000;
const PROJECT = `${location.origin}/api/v1/projects/acme%2Fsupport-bot`;
const RANGE = {
  from: new Date(NOW - 30 * DAY_MS).toISOString(),
  to: new Date(NOW + DAY_MS).toISOString(),
};

const PROFILES: Record<StoryId, Profile> = {
  support: SUPPORT_PROFILE,
  coding: CODING_PROFILE,
  voice: VOICE_PROFILE,
  extraction: EXTRACTION_PROFILE,
};

/** A story's dataset of `traces` traces, built with the story's profile. */
function datasetOf(story: StoryId, traces: number): Dataset {
  return createDataset({ seed: 1, now: NOW, traces, profile: PROFILES[story] });
}

/** The API base of a story's project: `/api/v1/projects/<encoded ref>`. */
function baseOf(story: StoryId): string {
  return `${window.location.origin}/api/v1/projects/${encodeURIComponent(storyProject(story).ref)}`;
}

type Body = Schema<"WidgetQueryRequest">;

beforeEach(() => {
  server.use(...largeHandlers(datasetOf("support", 5000)));
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

/** The visible traces of `dataset` in the range. */
function inRangeTraces(dataset: Dataset) {
  return visibleTraces(dataset.traces, false, false).filter((trace) => inRange(trace.start_time));
}

/** The visible traces of `dataset` in the range that carry `tag`. */
function taggedTraces(dataset: Dataset, tag: string) {
  return inRangeTraces(dataset).filter((trace) => trace.tags.includes(tag));
}

it("the story dashboards have the planned ids, names and widget titles", () => {
  expect(STORY_DASHBOARD_IDS).toEqual({
    answerQuality: "01900000-0000-7000-8000-000000570401",
    codeAgentHealth: "01900000-0000-7000-8000-000000570402",
    voiceLatency: "01900000-0000-7000-8000-000000570403",
    invoiceExtraction: "01900000-0000-7000-8000-000000570404",
    codingAgents: "01900000-0000-7000-8000-000000570405",
  });
  const dashboards = storyDashboards();
  expect(dashboards.map((dashboard) => dashboard.id)).toEqual([
    "01900000-0000-7000-8000-000000570401",
    "01900000-0000-7000-8000-000000570402",
    "01900000-0000-7000-8000-000000570403",
    "01900000-0000-7000-8000-000000570404",
    "01900000-0000-7000-8000-000000570405",
  ]);
  expect(new Set(dashboards.map((dashboard) => dashboard.id)).size).toBe(5);
  expect(dashboards.map((dashboard) => dashboard.name)).toEqual([
    "Prompt quality by version",
    "Code agent health",
    "Voice latency",
    "Invoice extraction",
    "Coding agents",
  ]);
  expect(dashboards.map((dashboard) => dashboard.definition.widgets.map((w) => w.title))).toEqual([
    [
      "Correctness by day",
      "Answers by release",
      "Cost per answer by release",
      "p95 latency by release",
    ],
    ["Failed runs per day", "Error rate", "Cost per run", "p95 duration"],
    ["p95 reply time", "Reply time by day", "Calls over 3 s", "Calls per day"],
    ["Invoices per day", "Schema check failures", "Cost per invoice", "Invoices by model"],
    ["Cost by repository", "Cost by coding agent"],
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
  expect(second[0]?.name).toBe("Prompt quality by version");
  expect(second[0]?.definition.widgets).toHaveLength(4);
});

it("seedDashboards still returns only the fixture dashboard", () => {
  expect(seedDashboards()).toHaveLength(1);
});

it("the browser mock store lists the story dashboards after the fixture one", async () => {
  server.use(...dashboardHandlers([...seedDashboards(), ...storyDashboards()]));
  for (const dashboard of storyDashboards()) {
    const story = STORY_DASHBOARD_STORIES[dashboard.id];
    if (story === undefined) {
      throw new Error(`no story of dashboard ${dashboard.id}`);
    }
    const list = await fetch(`${baseOf(story)}/dashboards`);
    const body = (await list.json()) as { data: { name: string }[] };
    const inStory = storyDashboards()
      .filter((candidate) => STORY_DASHBOARD_STORIES[candidate.id] === story)
      .map((candidate) => candidate.name);
    expect(body.data.map((item) => item.name)).toEqual([...inStory, "Support overview"].sort());

    const one = await fetch(`${baseOf(story)}/dashboards/${dashboard.id}`);
    const stored = (await one.json()) as Dashboard;
    expect(stored.definition.widgets).toHaveLength(dashboard.definition.widgets.length);
    expect(stored.name).toBe(dashboard.name);
  }
});

it("the large mock answers every widget of every story dashboard with data", async () => {
  for (const dashboard of storyDashboards()) {
    const story = STORY_DASHBOARD_STORIES[dashboard.id];
    if (story === undefined) {
      throw new Error(`no story of dashboard ${dashboard.id}`);
    }
    server.use(...largeHandlers(datasetOf(story, 5000)));
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
  const dataset = datasetOf("voice", 5000);
  server.use(...largeHandlers(dataset));
  const calls = storyDashboard(STORY_DASHBOARD_IDS.voiceLatency).definition.widgets[3];
  if (calls === undefined) {
    throw new Error("no widget");
  }
  const answer = await ask(calls, "series");
  const body = answer.json as { series: { points: { value?: number }[] }[] };
  const counted = body.series
    .flatMap((series) => series.points)
    .reduce((sum, point) => sum + (point.value ?? 0), 0);
  expect(counted).toBe(taggedTraces(dataset, "voice").length);
});

it("the rate widgets match the dataset", async () => {
  const coding = datasetOf("coding", 5000);
  server.use(...largeHandlers(coding));
  const code = storyDashboard(STORY_DASHBOARD_IDS.codeAgentHealth).definition.widgets[1];
  if (code === undefined) {
    throw new Error("no widget");
  }
  const traces = inRangeTraces(coding);
  const errorRate = (await ask(code, "total")).json as { rows: { values: number[] }[] };
  expect(errorRate.rows[0]?.values[0]).toBeCloseTo(
    traces.filter((trace) => trace.error_count > 0).length / traces.length,
    6,
  );

  const voice = datasetOf("voice", 5000);
  server.use(...largeHandlers(voice));
  const voiceWidget = storyDashboard(STORY_DASHBOARD_IDS.voiceLatency).definition.widgets[2];
  if (voiceWidget === undefined) {
    throw new Error("no widget");
  }
  const voiceCalls = taggedTraces(voice, "voice");
  const overThree = (await ask(voiceWidget, "total")).json as { rows: { values: number[] }[] };
  expect(overThree.rows[0]?.values[0]).toBeCloseTo(
    voiceCalls.filter((trace) => trace.duration_ns > 3_000_000_000).length / voiceCalls.length,
    6,
  );
});

it("the hero traces are counted", () => {
  // Each story's second widget carries its tag filter (the scores widget is the first one), except
  // the coding and support ones: their widgets count every trace of the project.
  const heroes: [StoryId, number, string][] = [
    ["support", 3, STORY_DASHBOARD_IDS.answerQuality],
    ["coding", 4, STORY_DASHBOARD_IDS.codeAgentHealth],
    ["voice", 5, STORY_DASHBOARD_IDS.voiceLatency],
    ["extraction", 6, STORY_DASHBOARD_IDS.invoiceExtraction],
  ];
  for (const [story, index, id] of heroes) {
    const second = storyDashboard(id).definition.widgets[1];
    if (second === undefined) {
      throw new Error(`no second widget in ${id}`);
    }
    const trace = datasetOf(story, 5000).traces[index];
    expect(trace).toBeDefined();
    if (trace === undefined) {
      throw new Error(`no trace ${index}`);
    }
    const filter = queryOf(second).filter;
    if (story === "coding" || story === "support") {
      expect(filter).toBeUndefined();
      if (story === "coding") {
        expect(trace.error_count).toBeGreaterThan(0);
      }
      continue;
    }
    const predicate = filter === undefined ? undefined : tracePredicate(filter);
    expect(predicate?.(trace)).toBe(true);
  }
});

it("the coding agents dashboard's cost by repository sums each repository's cost from the runs' metadata", async () => {
  const coding = datasetOf("coding", 5000);
  server.use(...largeHandlers(coding));
  const byRepository = storyDashboard(STORY_DASHBOARD_IDS.codingAgents).definition.widgets[0];
  if (byRepository === undefined) {
    throw new Error("no widget");
  }
  const answer = await ask(byRepository, "total");
  expect(answer.status).toBe(200);
  const body = answer.json as { rows: { group?: string; values: number[] }[] };
  const expected = new Map<string, number>();
  for (const trace of inRangeTraces(coding)) {
    const repository = trace.metadata?.repository;
    if (repository !== undefined) {
      expected.set(repository, (expected.get(repository) ?? 0) + (trace.cost_total ?? 0));
    }
  }
  expect(body.rows.map((row) => row.group)).toEqual(
    [...expected.entries()].sort((left, right) => right[1] - left[1]).map(([name]) => name),
  );
  for (const row of body.rows) {
    expect(row.values[0]).toBeCloseTo(expected.get(row.group ?? "") ?? Number.NaN, 6);
  }
});
