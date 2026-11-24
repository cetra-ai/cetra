import { expect, it } from "vitest";
import type { Schema } from "@/api/types";
import { createDataset, type Dataset, rfc3339, visibleTraces } from "@/mocks/generated/dataset";
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
import alertEventsFixture from "../../../../fixtures/api/static/alert-events.json";
import projectAlertEventsFixture from "../../../../fixtures/api/static/project-alert-events.json";

const NOW = Date.parse("2026-06-01T00:00:00.000Z");
const PROJECT = `${location.origin}/api/v1/projects/acme%2Fsupport-bot`;
const MINUTE_MS = 60_000;
const DAY_MS = 86_400_000;
const WINDOW_MS: Record<string, number> = {
  "1m": MINUTE_MS,
  "5m": 5 * MINUTE_MS,
  "15m": 15 * MINUTE_MS,
  "1h": 60 * MINUTE_MS,
  "6h": 360 * MINUTE_MS,
  "24h": 1440 * MINUTE_MS,
};
/** Occurrences count over the 14 days before now. */
const OCCURRENCE_WINDOW_MS = 14 * DAY_MS;
/** Example traces after the hero trace. */
const GENERATED_EXAMPLES = 4;
const CODING_ISSUE = "01900000-0000-7000-8000-000000570102";
const VOICE_RULE = "01900000-0000-7000-8000-000000570203";
const FIXTURE_RULE = "01900000-0000-7000-8000-000000000510";

type StoryIssue = (typeof STORY_ISSUES)[number];
type Trace = Schema<"TraceSummary">;

/**
 * The issue's traces, recomputed from the dataset alone: the hero trace, then the generated
 * traces that match, start inside the 14 days before now, and are not the hero.
 */
function expectedTraces(dataset: Dataset, issue: StoryIssue) {
  const hero: Trace | undefined = dataset.traces[issue.hero.index];
  const generated = visibleTraces(dataset.traces, false, false).filter((trace) => {
    const started = Date.parse(trace.start_time);
    return (
      trace.trace_id !== hero?.trace_id &&
      issue.matches(trace) &&
      started >= dataset.now - OCCURRENCE_WINDOW_MS &&
      started <= dataset.now
    );
  });
  return { hero, generated, all: hero ? [hero, ...generated] : generated };
}

function dayOf(trace: Trace): number {
  return Math.floor(Date.parse(trace.start_time) / DAY_MS) * DAY_MS;
}

function distinctUsers(traces: readonly Trace[]): number {
  return new Set(
    traces.map((trace) => trace.user_id).filter((user): user is string => user != null),
  ).size;
}

/** Newest first; ties by trace id, descending. */
function newestFirst(left: Trace, right: Trace): number {
  return (
    Date.parse(right.start_time) - Date.parse(left.start_time) ||
    (right.trace_id < left.trace_id ? -1 : right.trace_id > left.trace_id ? 1 : 0)
  );
}

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

it("counts an issue's occurrences from the dataset, in the issue and in its days", () => {
  const dataset = createDataset({ seed: 1, now: NOW, traces: 2000 });
  for (const issue of STORY_ISSUES) {
    const detail = storyIssue(dataset, issue);
    if (detail === undefined) {
      throw new Error(`story issue ${issue.id} matches no trace`);
    }
    const { all } = expectedTraces(dataset, issue);
    const { days } = storyOccurrences(dataset, issue);

    expect(detail.count).toBe(new Set(all.map((trace) => trace.trace_id)).size);
    expect(detail.users).toBe(distinctUsers(all));

    const expectedDays = [...new Set(all.map(dayOf))].sort((left, right) => left - right);
    const keys = days.map((day) => Date.parse(day.day));
    expect(keys).toEqual(expectedDays);
    expect(new Set(keys).size).toBe(keys.length);
    expect(keys).toEqual([...keys].sort((left, right) => left - right));
    for (const day of days) {
      const inDay = all.filter((trace) => dayOf(trace) === Date.parse(day.day));
      expect(day.count).toBe(inDay.length);
      expect(day.users).toBe(distinctUsers(inDay));
    }

    const starts = all.map((trace) => Date.parse(trace.start_time));
    expect(Date.parse(detail.first_seen)).toBe(Math.min(...starts));
    expect(Date.parse(detail.last_seen)).toBe(Math.max(...starts));
  }
});

it("gives every story issue at most five examples, the generated ones newest first", () => {
  const dataset = createDataset({ seed: 1, now: NOW, traces: 2000 });
  const byId = new Map(dataset.traces.map((trace) => [trace.trace_id, trace]));
  for (const issue of STORY_ISSUES) {
    const { generated } = expectedTraces(dataset, issue);
    const { examples } = storyOccurrences(dataset, issue);
    expect(examples.length).toBeLessThanOrEqual(5);
    if (generated.length > 0) {
      expect(examples.length).toBeGreaterThan(1);
    }

    const shown = examples.slice(1);
    expect(shown.length).toBe(Math.min(GENERATED_EXAMPLES, generated.length));

    const order: [number, string][] = shown.map((example) => [
      Date.parse(example.created_at),
      example.trace_id,
    ]);
    const sorted = [...order].sort(
      ([leftTime, leftId], [rightTime, rightId]) =>
        rightTime - leftTime || (rightId < leftId ? -1 : rightId > leftId ? 1 : 0),
    );
    expect(order).toEqual(sorted);

    const newest = [...generated].sort(newestFirst).slice(0, GENERATED_EXAMPLES);
    expect(shown.map((example) => example.trace_id)).toEqual(newest.map((trace) => trace.trace_id));

    for (const example of shown) {
      if (issue.kind === "topic") {
        expect(example.reason).toBe(byId.get(example.trace_id)?.input_preview);
      } else {
        expect(example.reason).toBeUndefined();
      }
    }
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

it("sets last_fired_at on the two firing rules and omits it on the two ok ones", () => {
  const dataset = createDataset({ seed: 1, now: NOW, traces: 2000 });
  expect(STORY_RULES.filter((rule) => rule.state === "firing")).toHaveLength(2);
  expect(STORY_RULES.filter((rule) => rule.state === "ok")).toHaveLength(2);
  for (const rule of STORY_RULES) {
    const stored = storyAlertRule(dataset, rule);
    if (rule.state === "firing") {
      expect(stored.last_fired_at).toBe(rfc3339(dataset.now - rule.changedAgoMs));
    } else {
      expect(stored).not.toHaveProperty("last_fired_at");
    }
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

  const occurrences = await fetch(`${PROJECT}/issues/${CODING_ISSUE}/occurrences`);
  expect(occurrences.status).toBe(200);

  const voice = (await (
    await fetch(`${PROJECT}/alert-rules/${VOICE_RULE}`)
  ).json()) as Schema<"AlertRule">;
  expect(voice.id).toBe(VOICE_RULE);
  expect(voice.name).toBe("Slow replies on voice-agent");

  const fixture = (await (
    await fetch(`${PROJECT}/issues/01900000-0000-7000-8000-000000001c01`)
  ).json()) as Schema<"Issue">;
  expect(fixture.id).toBe("01900000-0000-7000-8000-000000001c01");
  expect(fixture.title).toBe("ValueError: could not parse the reply");
});

it("answers the story events routes, and lets the fixture rule's routes fall through", async () => {
  const dataset = createDataset({ seed: 1, now: NOW, traces: 2000 });
  server.use(...storyHandlers(dataset));

  const detail = (await (
    await fetch(`${PROJECT}/issues/${CODING_ISSUE}`)
  ).json()) as Schema<"Issue">;
  const issueEvents = (await (
    await fetch(`${PROJECT}/issues/${CODING_ISSUE}/events`)
  ).json()) as Schema<"IssueEventPage">;
  expect(issueEvents.data.map((event) => [event.kind, event.at])).toEqual([
    ["created", detail.first_seen],
  ]);

  const voiceEvents = (await (
    await fetch(`${PROJECT}/alert-rules/${VOICE_RULE}/events`)
  ).json()) as Schema<"AlertEventPage">;
  expect(voiceEvents.data.map((event) => event.kind)).toEqual(["fired"]);

  const fixtureRule = await fetch(`${PROJECT}/alert-rules/${FIXTURE_RULE}`);
  expect(fixtureRule.status).toBe(200);
  expect(((await fixtureRule.json()) as Schema<"AlertRule">).name).toBe("High error rate");

  const fixtureEvents = await fetch(`${PROJECT}/alert-rules/${FIXTURE_RULE}/events`);
  expect(fixtureEvents.status).toBe(200);
  const body = (await fixtureEvents.json()) as Schema<"AlertEventPage">;
  expect(body.data.map((event) => event.id)).toEqual(
    alertEventsFixture.data.map((event) => event.id),
  );

  const alertEvents = (await (
    await fetch(`${PROJECT}/alert-events`)
  ).json()) as Schema<"AlertEventPage">;
  const storyEvents = STORY_RULES.map((rule) => storyAlertEvent(dataset, rule)).sort(
    (left, right) => Date.parse(right.created_at) - Date.parse(left.created_at),
  );
  const head = alertEvents.data.slice(0, 4);
  expect(head.map((event) => event.id)).toEqual(storyEvents.map((event) => event.id));
  const times = head.map((event) => Date.parse(event.created_at));
  expect(times).toEqual([...times].sort((left, right) => right - left));
  expect(alertEvents.data.slice(4).map((event) => event.id)).toEqual(
    projectAlertEventsFixture.data.map((event) => event.id),
  );
});
