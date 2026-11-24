import { expect, it } from "vitest";
import type { Schema } from "@/api/types";
import { createDataset } from "@/mocks/generated/dataset";
import {
  STORY_ISSUES,
  STORY_RULES,
  storyAlertEvent,
  storyAlertRule,
  storyHandlers,
  storyIssue,
  storyOccurrences,
} from "@/mocks/generated/stories";
import { server } from "@/mocks/node";

const NOW = Date.parse("2026-06-01T00:00:00.000Z");
const PROJECT = `${location.origin}/api/v1/projects/acme%2Fsupport-bot`;
const MINUTE_MS = 60_000;
const WINDOW_MS: Record<string, number> = {
  "1m": MINUTE_MS,
  "5m": 5 * MINUTE_MS,
  "15m": 15 * MINUTE_MS,
  "1h": 60 * MINUTE_MS,
  "6h": 360 * MINUTE_MS,
  "24h": 1440 * MINUTE_MS,
};

it("makes the hero trace the first example of every story issue", () => {
  const dataset = createDataset({ seed: 1, now: NOW, traces: 2000 });
  for (const issue of STORY_ISSUES) {
    const hero = dataset.traces[issue.hero.index];
    const { examples } = storyOccurrences(dataset, issue);
    expect(examples[0]?.trace_id).toBe(hero?.trace_id);
    expect(examples[0]?.reason).toBe(issue.hero.reason);
    if (issue.hero.spanId !== undefined) {
      expect(examples[0]?.span_id).toBe(issue.hero.spanId);
      const spans = dataset.traceDetail(hero?.trace_id ?? "")?.spans ?? [];
      expect(spans.some((row) => row.span.span_id === issue.hero.spanId)).toBe(true);
    }
  }
});

it("counts an issue's occurrences the same way in the issue and its days", () => {
  const dataset = createDataset({ seed: 1, now: NOW, traces: 2000 });
  for (const issue of STORY_ISSUES) {
    const detail = storyIssue(dataset, issue);
    if (detail === undefined) {
      throw new Error(`story issue ${issue.id} matches no trace`);
    }
    const { days, examples } = storyOccurrences(dataset, issue);
    expect(detail.count).toBe(days.reduce((sum, day) => sum + day.count, 0));
    for (const day of days) {
      expect(detail.users).toBeGreaterThanOrEqual(day.users);
    }
    expect(Date.parse(detail.first_seen)).toBeLessThanOrEqual(Date.parse(detail.last_seen));
    for (const example of examples) {
      expect(dataset.traces.some((trace) => trace.trace_id === example.trace_id)).toBe(true);
    }
  }
});

it("gives every story issue at most five examples, the generated ones newest first", () => {
  const dataset = createDataset({ seed: 1, now: NOW, traces: 2000 });
  for (const issue of STORY_ISSUES) {
    const { examples } = storyOccurrences(dataset, issue);
    expect(examples.length).toBeLessThanOrEqual(5);
    const times = examples.slice(1).map((example) => Date.parse(example.created_at));
    expect(times).toEqual([...times].sort((left, right) => right - left));
  }
});

it("lists the story rules with their state and a fired event for the firing ones", () => {
  const dataset = createDataset({ seed: 1, now: NOW, traces: 2000 });
  const rules = STORY_RULES.map((rule) => storyAlertRule(dataset, rule));
  expect(rules.map((rule) => rule.state)).toEqual(["ok", "firing", "firing", "ok"]);
  for (const rule of STORY_RULES) {
    const event = storyAlertEvent(dataset, rule);
    expect(event.kind).toBe(rule.state === "firing" ? "fired" : "resolved");
    expect(Date.parse(event.window_to) - Date.parse(event.window_from)).toBe(
      WINDOW_MS[rule.condition.window],
    );
  }
});

it("leaves out a story issue that matches no trace", async () => {
  const dataset = createDataset({ seed: 1, now: NOW, traces: 3 });
  const unmatched = STORY_ISSUES.filter((issue) => storyIssue(dataset, issue) === undefined);
  const matched = STORY_ISSUES.filter((issue) => storyIssue(dataset, issue) !== undefined);
  expect(unmatched.length).toBeGreaterThan(0);
  for (const issue of unmatched) {
    expect(storyOccurrences(dataset, issue).examples).toEqual([]);
  }

  server.use(...storyHandlers(dataset));
  const response = await fetch(`${PROJECT}/issues`);
  expect(response.status).toBe(200);
  const issues = (await response.json()) as Schema<"IssuePage">;
  expect(issues.data.map((issue) => issue.id)).toEqual([
    ...matched.map((issue) => issue.id),
    "01900000-0000-7000-8000-000000001c01",
  ]);
});

it("serves the story issues and rules before the fixtures, and lets other ids fall through", async () => {
  const dataset = createDataset({ seed: 1, now: NOW, traces: 2000 });
  server.use(...storyHandlers(dataset));

  const issues = (await (await fetch(`${PROJECT}/issues`)).json()) as Schema<"IssuePage">;
  const issueIds = issues.data.map((issue) => issue.id);
  expect(issueIds.slice(0, 4)).toEqual(STORY_ISSUES.map((issue) => issue.id));
  expect(issueIds.slice(4)).toEqual(["01900000-0000-7000-8000-000000001c01"]);

  const rules = (await (await fetch(`${PROJECT}/alert-rules`)).json()) as Schema<"AlertRulePage">;
  expect(rules.data.slice(0, 4).map((rule) => rule.id)).toEqual(STORY_RULES.map((rule) => rule.id));

  const occurrences = await fetch(
    `${PROJECT}/issues/01900000-0000-7000-8000-000000570102/occurrences`,
  );
  expect(occurrences.status).toBe(200);

  const voice = (await (
    await fetch(`${PROJECT}/alert-rules/01900000-0000-7000-8000-000000570203`)
  ).json()) as Schema<"AlertRule">;
  expect(voice.id).toBe("01900000-0000-7000-8000-000000570203");
  expect(voice.name).toBe("Slow replies on voice-agent");

  const fixture = (await (
    await fetch(`${PROJECT}/issues/01900000-0000-7000-8000-000000001c01`)
  ).json()) as Schema<"Issue">;
  expect(fixture.id).toBe("01900000-0000-7000-8000-000000001c01");
  expect(fixture.title).toBe("ValueError: could not parse the reply");
});
