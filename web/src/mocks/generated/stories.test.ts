import { beforeAll, expect, it } from "vitest";
import type { Schema } from "@/api/types";
import {
  createDataset,
  type Dataset,
  type Profile,
  percentile,
  rfc3339,
  visibleTraces,
} from "@/mocks/generated/dataset";
import {
  STORY_ISSUES,
  STORY_QUEUES,
  STORY_RULES,
  STORY_SCORE_DEFINITIONS,
  storyAlertEvent,
  storyAlertRule,
  storyHandlers,
  storyIssue,
  storyOccurrences,
  storyQueue,
  storyQueueItem,
} from "@/mocks/generated/stories";
import { CODING_STORY_PROFILE } from "@/mocks/generated/story-coding";
import { EXTRACTION_STORY_PROFILE } from "@/mocks/generated/story-extraction";
import {
  SSO_QUESTIONS,
  SUPPORT_STORY_PROFILE,
  supportFacts,
} from "@/mocks/generated/story-support";
import { VOICE_STORY_PROFILE } from "@/mocks/generated/story-voice";
import { server } from "@/mocks/node";
import { type StoryId, storyProject } from "@/mocks/projects";
import alertEventsFixture from "../../../../fixtures/api/static/alert-events.json";
import alertRulesFixture from "../../../../fixtures/api/static/alert-rules.json";

/** The screenshot clock (`website/scripts/screenshots.mjs`). */
const NOW = Date.parse("2026-11-19T12:00:00Z");
const PROJECT = `${location.origin}/api/v1/projects/acme%2Fsupport-bot`;
const MINUTE_MS = 60_000;
const HOUR_MS = 60 * MINUTE_MS;
const DAY_MS = 86_400_000;
const WEEK_MS = 7 * DAY_MS;
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
type Issue = (typeof STORY_ISSUES)[number];
/** The reason an example of `trace` at `position` shows: the issue's list entry, or the trace's own question. */
function reasonFor(issue: Issue, trace: Schema<"TraceSummary"> | undefined, position: number) {
  if (issue.reasons === "input_preview") {
    return trace?.input_preview;
  }
  return issue.reasons[position % issue.reasons.length];
}
const CODING_ISSUE = "01900000-0000-7000-8000-000000570102";
const VOICE_RULE = "01900000-0000-7000-8000-000000570203";
const SUPPORT_RULE = "01900000-0000-7000-8000-000000570201";
const TEAM_SLACK_ID = "01900000-0000-7000-8000-000000000500";
const ON_CALL_PAGER_ID = "01900000-0000-7000-8000-000000000503";
const FIXTURE_RULE = "01900000-0000-7000-8000-000000000510";
const FIXTURE_ISSUE = "01900000-0000-7000-8000-000000001c01";
const FIXTURE_QUEUE = "01900000-0000-7000-8000-000000000a01";
const STORY_IDS: readonly StoryId[] = ["support", "coding", "voice", "extraction"];
const STORY_OBJECT_IDS = new Set(
  [...STORY_ISSUES, ...STORY_RULES, ...STORY_QUEUES].map((object) => object.id),
);

/** Each story's traces: the support project has 5,000, the others 2,000 (`main.tsx`). */
const TRACES: Record<StoryId, number> = {
  support: 5_000,
  coding: 2_000,
  voice: 2_000,
  extraction: 2_000,
};

const PROFILES: Record<StoryId, Profile> = {
  support: SUPPORT_STORY_PROFILE,
  coding: CODING_STORY_PROFILE,
  voice: VOICE_STORY_PROFILE,
  extraction: EXTRACTION_STORY_PROFILE,
};

/** A story's dataset at the screenshot clock, from its story profile. */
function datasetOf(story: StoryId): Dataset {
  return createDataset({ seed: 1, now: NOW, traces: TRACES[story], profile: PROFILES[story] });
}

/** The API base of a story's project: `/api/v1/projects/<encoded ref>`. */
function baseOf(story: StoryId): string {
  return `${window.location.origin}/api/v1/projects/${encodeURIComponent(storyProject(story).ref)}`;
}

/** The one object of a story in a list of story objects. */
function oneOf<T extends { story: StoryId }>(objects: readonly T[], story: StoryId): T {
  const found = objects.find((object) => object.story === story);
  if (found === undefined) {
    throw new Error(`no story object of ${story}`);
  }
  return found;
}

/** Every story's handlers, each under its own project. */
function allStoryHandlers() {
  return STORY_IDS.flatMap((story) => storyHandlers(stories[story], storyProject(story).ref));
}

/** The four story datasets at the screenshot clock, built once for the tests. */
let stories: Record<StoryId, Dataset>;

beforeAll(() => {
  stories = {
    support: datasetOf("support"),
    coding: datasetOf("coding"),
    voice: datasetOf("voice"),
    extraction: datasetOf("extraction"),
  };
});

type StoryIssue = (typeof STORY_ISSUES)[number];
type Trace = Schema<"TraceSummary">;

/**
 * The issue's traces, recomputed from the dataset alone: the hero trace, then the generated
 * traces that match, start inside the 14 days before now, and are not the hero.
 */
function expectedTraces(dataset: Dataset, issue: StoryIssue) {
  const hero: Trace | undefined = dataset.traces[issue.hero.index(dataset)];
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

/** The visible traces of a dataset that start in `[from, to)`, recomputed for the rule checks. */
function startedIn(dataset: Dataset, from: number, to: number): Trace[] {
  return visibleTraces(dataset.traces, false, false).filter((trace) => {
    const started = Date.parse(trace.start_time);
    return started >= from && started < to;
  });
}

/** The numeric score `name` of each trace that carries it. */
function numericScoresOf(traces: readonly Trace[], name: string): number[] {
  return traces.flatMap((trace) => {
    const value = trace.scores.find((score) => score.name === name)?.value;
    return value?.data_type === "numeric" ? [value.value] : [];
  });
}

function mean(values: readonly number[]): number {
  return values.reduce((total, value) => total + value, 0) / values.length;
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

it("makes the first matching trace an example of every story issue, the newest one for the support issue", () => {
  for (const issue of STORY_ISSUES) {
    const dataset = stories[issue.story];
    const index = issue.hero.index(dataset);
    const hero = dataset.traces[index];
    // The support issue's hero is its newest matching trace; the others' is the first one by index.
    const earlier = dataset.traces.filter(
      (trace, position) =>
        visibleTraces([trace], false, false).length === 1 &&
        issue.matches(trace) &&
        (issue.story === "support"
          ? hero !== undefined && newestFirst(trace, hero) < 0
          : position < index),
    );
    expect(hero !== undefined && issue.matches(hero)).toBe(true);
    expect(earlier).toEqual([]);

    const { examples } = storyOccurrences(dataset, issue);
    const position = examples.findIndex((example) => example.trace_id === hero?.trace_id);
    expect(position).toBeGreaterThanOrEqual(0);
    expect(examples[position]?.reason).toBe(reasonFor(issue, hero, position));
    const spanId = issue.hero.spanId?.(dataset, index);
    expect(examples[position]?.span_id).toBe(spanId);
    // Only the hero's example carries a span.
    expect(examples.filter((example) => example.span_id !== undefined).length).toBe(
      spanId === undefined ? 0 : 1,
    );
    if (spanId !== undefined) {
      const span = dataset
        .traceDetail(hero?.trace_id ?? "")
        ?.spans.find((row) => row.span.span_id === spanId)?.span;
      expect(span?.status).toBe("error");
      expect(span?.name).toBe(issue.story === "coding" ? "write patch" : "ocr");
    }
  }
});

it("counts an issue's occurrences from the dataset, in the issue and in its days", () => {
  for (const issue of STORY_ISSUES) {
    const dataset = stories[issue.story];
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

it("counts the support issue's SSO traces in the last two weeks as the support facts do", () => {
  const dataset = stories.support;
  const { all } = expectedTraces(dataset, oneOf(STORY_ISSUES, "support"));
  const countIn = (from: number, to: number) =>
    all.filter((trace) => {
      const started = Date.parse(trace.start_time);
      return started >= from && started < to;
    }).length;
  const facts = supportFacts(dataset);
  expect(countIn(NOW - WEEK_MS, NOW)).toBe(facts.ssoThisWeek);
  expect(countIn(NOW - 2 * WEEK_MS, NOW - WEEK_MS)).toBe(facts.ssoPreviousWeek);
});

it("gives every story issue at most five examples, the hero and the newest generated ones, newest first", () => {
  for (const issue of STORY_ISSUES) {
    const dataset = stories[issue.story];
    const byId = new Map(dataset.traces.map((trace) => [trace.trace_id, trace]));
    const { generated } = expectedTraces(dataset, issue);
    const { examples } = storyOccurrences(dataset, issue);
    expect(examples.length).toBeLessThanOrEqual(5);
    if (generated.length > 0) {
      expect(examples.length).toBeGreaterThan(1);
    }

    const heroId = dataset.traces[issue.hero.index(dataset)]?.trace_id;
    const shown = examples.filter((example) => example.trace_id !== heroId);
    expect(shown.length).toBe(Math.min(GENERATED_EXAMPLES, generated.length));

    const order: [number, string][] = examples.map((example) => [
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

    // Every example says why it is there, and the reasons follow the list by position.
    examples.forEach((example, position) => {
      expect(example.reason).toBe(reasonFor(issue, byId.get(example.trace_id), position));
      expect(byId.has(example.trace_id)).toBe(true);
    });
  }
});

it("gives every example of a story issue its own reason, none repeated", () => {
  for (const issue of STORY_ISSUES) {
    const { examples } = storyOccurrences(stories[issue.story], issue);
    const reasons = examples.map((example) => example.reason ?? "");
    expect(reasons.every((reason) => reason.length > 0)).toBe(true);
    expect(new Set(reasons).size).toBe(examples.length);
    const distinct =
      issue.reasons === "input_preview" ? SSO_QUESTIONS.length : issue.reasons.length;
    expect(examples.length).toBe(Math.min(1 + GENERATED_EXAMPLES, distinct));
  }
});

it("shows the support SSO examples' own questions, each one different", () => {
  const issue = oneOf(STORY_ISSUES, "support");
  const dataset = stories.support;
  const byId = new Map(dataset.traces.map((trace) => [trace.trace_id, trace]));
  const { examples } = storyOccurrences(dataset, issue);
  const reasons = examples.map((example) => example.reason);
  expect(reasons).toEqual(examples.map((example) => byId.get(example.trace_id)?.input_preview));
  for (const reason of reasons) {
    expect(SSO_QUESTIONS).toContain(reason);
  }
  expect(new Set(reasons).size).toBe(examples.length);
});

it("writes the coding, voice and extraction examples with their own numbers", () => {
  const reasonsOf = (story: StoryId) =>
    storyOccurrences(stories[story], oneOf(STORY_ISSUES, story)).examples.map(
      (example) => example.reason,
    );
  expect(reasonsOf("coding")).toEqual([
    "Diff over 23 files: 212k tokens, over the 200k context",
    "Diff over 31 files: 240k tokens, over the 200k context",
    "Diff over 18 files: 205k tokens, over the 200k context",
    "Diff over 27 files: 226k tokens, over the 200k context",
    "Diff over 44 files: 263k tokens, over the 200k context",
  ]);
  for (const reason of reasonsOf("voice")) {
    expect(reason).toMatch(
      /^interruption 0\.\d: the agent spoke \d\.\d s before the caller finished$/,
    );
  }
  for (const reason of reasonsOf("extraction")) {
    expect(reason).toMatch(/^invoice-2026-06\d\d\.pdf: scanned image, no text layer$/);
  }
});

it("lists the story rules with their state and a fired event for the firing ones", () => {
  const rules = STORY_RULES.map((rule) => storyAlertRule(stories[rule.story], rule));
  expect(rules.map((rule) => rule.state)).toEqual(["firing", "firing", "firing", "firing"]);
  for (const rule of STORY_RULES) {
    const event = storyAlertEvent(stories[rule.story], rule);
    expect(event.kind).toBe(rule.state === "firing" ? "fired" : "resolved");
    expect(Date.parse(event.window_to) - Date.parse(event.window_from)).toBe(
      WINDOW_MS[rule.condition.window],
    );
  }
});

it("sets last_fired_at on the four firing rules, and no story rule is ok", () => {
  expect(STORY_RULES.filter((rule) => rule.state === "firing")).toHaveLength(4);
  expect(STORY_RULES.filter((rule) => rule.state === "ok")).toHaveLength(0);
  for (const rule of STORY_RULES) {
    const dataset = stories[rule.story];
    const stored = storyAlertRule(dataset, rule);
    if (rule.state === "firing") {
      expect(stored.last_fired_at).toBe(
        rfc3339(dataset.now - rule.changedAgoMs(dataset, dataset.now)),
      );
    } else {
      expect(stored).not.toHaveProperty("last_fired_at");
    }
  }
});

it("has the voice rule firing for two days, longer than the chart's 24 hours", () => {
  const dataset = stories.voice;
  const rule = oneOf(STORY_RULES, "voice");
  const alert = storyAlertRule(dataset, rule);
  expect(alert.last_fired_at).toBe(rfc3339(NOW - 2 * DAY_MS));
  expect(Date.parse(alert.last_fired_at ?? "")).toBeLessThan(NOW - DAY_MS);
});

it("computes each rule's value and count from its window at the clock", () => {
  const support = stories.support;
  const supportHour = startedIn(support, NOW - HOUR_MS, NOW);
  const supportAlert = storyAlertRule(support, oneOf(STORY_RULES, "support"));
  expect(supportAlert.last_value).toBeCloseTo(
    mean(numericScoresOf(supportHour, "correctness")),
    12,
  );
  expect(supportAlert.last_count).toBe(supportHour.length);

  const coding = stories.coding;
  const codingHour = startedIn(coding, NOW - HOUR_MS, NOW);
  const codingAlert = storyAlertRule(coding, oneOf(STORY_RULES, "coding"));
  expect(codingAlert.last_value).toBeCloseTo(
    codingHour.reduce((total, trace) => total + (trace.cost_total ?? 0), 0),
    9,
  );
  expect(codingAlert.last_count).toBe(codingHour.length);

  const voice = stories.voice;
  const voiceHour = startedIn(voice, NOW - HOUR_MS, NOW);
  const voiceAlert = storyAlertRule(voice, oneOf(STORY_RULES, "voice"));
  expect(voiceAlert.last_value).toBe(
    percentile(
      voiceHour.map((trace) => trace.duration_ns / 1_000_000),
      95,
    ),
  );
  expect(voiceAlert.last_count).toBe(voiceHour.length);
});

/** A rule's comparison of its value with its threshold, as the server reads the comparator. */
const BREACHES = {
  gt: (value: number, threshold: number) => value > threshold,
  gte: (value: number, threshold: number) => value >= threshold,
  lt: (value: number, threshold: number) => value < threshold,
  lte: (value: number, threshold: number) => value <= threshold,
} as const;

it("has each firing rule's value breach its threshold, on at least its minimum sample count", () => {
  for (const rule of STORY_RULES.filter((item) => item.state === "firing")) {
    const dataset = stories[rule.story];
    const value = rule.lastValue(dataset, dataset.now);
    const { comparator, threshold } = rule.condition;
    expect(value).toBeDefined();
    expect(value !== undefined && BREACHES[comparator](value, threshold)).toBe(true);
    expect(rule.lastCount(dataset, dataset.now)).toBeGreaterThanOrEqual(
      rule.condition.min_count ?? 0,
    );
  }
});

it("leaves the value out of an empty window, with a count of 0", () => {
  // The traces start before this clock, so the hour and the quarter hour after it hold none. Every
  // story rule fires now, so each one's window is the one that counts.
  for (const rule of STORY_RULES.filter((item) => item.state === "firing")) {
    const dataset = stories[rule.story];
    const later = { ...dataset, now: dataset.now + DAY_MS * 365 };
    expect(rule.lastCount(later, later.now)).toBe(0);
    if (rule.story === "support" || rule.story === "voice") {
      expect(rule.lastValue(later, later.now)).toBeUndefined();
      expect(storyAlertRule(later, rule)).not.toHaveProperty("last_value");
      expect(storyAlertEvent(later, rule)).not.toHaveProperty("value");
    }
  }
});

it("starts the support rule at the first hour of the last day under 0.7 correctness", () => {
  const support = stories.support;
  const first = Math.floor((NOW - DAY_MS) / HOUR_MS) * HOUR_MS;
  const hourlyMeans = Array.from({ length: 24 }, (_, hour) => {
    const start = first + hour * HOUR_MS;
    return mean(numericScoresOf(startedIn(support, start, start + HOUR_MS), "correctness"));
  });
  const dipped = hourlyMeans.findIndex((value) => value < 0.7);
  expect(dipped).toBeGreaterThanOrEqual(0);
  expect(hourlyMeans[23]).toBeLessThan(0.7);
  const alert = storyAlertRule(support, oneOf(STORY_RULES, "support"));
  expect(alert.last_fired_at).toBe(rfc3339(first + dipped * HOUR_MS));
});

const SCANNED_ISSUE = "01900000-0000-7000-8000-000000570104";
const EXTRACTION_SOURCE = "01900000-0000-7000-8000-000000570304";

it("serves the scanned PDFs' preview: 24 hourly points, the last with 14 and every earlier one at most 6", async () => {
  server.use(...allStoryHandlers());
  const extraction = baseOf("extraction");
  const preview = (await (
    await fetch(`${extraction}/alert-rules/issue-preview?issue_id=${SCANNED_ISSUE}&window=1h`)
  ).json()) as Schema<"IssueAlertPreview">;
  expect(preview.from).toBe(rfc3339(NOW - DAY_MS));
  expect(preview.to).toBe(rfc3339(NOW));
  expect(preview.points).toHaveLength(24);
  const counts = preview.points.map((point) => point.count);
  expect(counts[23]).toBe(14);
  expect(Math.max(...counts.slice(0, 23))).toBeLessThanOrEqual(6);

  const bySource = (await (
    await fetch(`${extraction}/alert-rules/issue-preview?source_id=${EXTRACTION_SOURCE}&window=1h`)
  ).json()) as Schema<"IssueAlertPreview">;
  expect(bySource.points).toEqual(preview.points);
});

/** The scanned PDFs' preview at the other windows, at the screenshot clock (which is not a day boundary). */
it.each([
  { window: "15m", points: 96, from: "2026-11-18T12:00:00Z", sum: 46 },
  { window: "6h", points: 12, from: "2026-11-16T12:00:00Z", sum: 46 },
  { window: "24h", points: 15, from: "2026-11-05T00:00:00Z", sum: 54 },
])(
  "serves the scanned PDFs' preview at $window: $points points from $from, $sum in all",
  async ({ window, points, from, sum }) => {
    server.use(...allStoryHandlers());
    const preview = (await (
      await fetch(
        `${baseOf("extraction")}/alert-rules/issue-preview?issue_id=${SCANNED_ISSUE}&window=${window}`,
      )
    ).json()) as Schema<"IssueAlertPreview">;
    expect(preview.window).toBe(window);
    expect(preview.from).toBe(rfc3339(Date.parse(from)));
    expect(preview.to).toBe(rfc3339(NOW));
    expect(preview.points).toHaveLength(points);
    expect(preview.points[0]?.time).toBe(rfc3339(Date.parse(from)));
    expect(preview.points.reduce((total, point) => total + point.count, 0)).toBe(sum);
  },
);

it("counts the partial last bucket up to the clock only", async () => {
  server.use(...allStoryHandlers());
  // 24h buckets from 2026-11-19T00:00 to the clock (12:00): the last point holds the day so far.
  const issue = STORY_ISSUES.find((item) => item.id === SCANNED_ISSUE);
  if (issue === undefined) {
    throw new Error("no scanned PDFs issue");
  }
  const dayStart = Date.parse("2026-11-19T00:00:00Z");
  const expected = expectedTraces(stories.extraction, issue).all.filter((trace) => {
    const started = Date.parse(trace.start_time);
    return started >= dayStart && started < NOW;
  }).length;
  const preview = (await (
    await fetch(
      `${baseOf("extraction")}/alert-rules/issue-preview?issue_id=${SCANNED_ISSUE}&window=24h`,
    )
  ).json()) as Schema<"IssueAlertPreview">;
  expect(preview.points[preview.points.length - 1]).toEqual({
    time: rfc3339(dayStart),
    count: expected,
  });
});

it("answers the preview queries the story does not take from the manifest's fixture", async () => {
  server.use(...allStoryHandlers());
  const extraction = baseOf("extraction");
  const queries = [
    `issue_id=01900000-0000-7000-8000-00000000ffff&window=1h`,
    `issue_id=${SCANNED_ISSUE}&source_id=${EXTRACTION_SOURCE}&window=1h`,
    `window=1h`,
    `issue_id=${SCANNED_ISSUE}&window=5m`,
    `issue_id=${SCANNED_ISSUE}`,
    `issue_id=${CODING_ISSUE}&window=1h`,
  ];
  for (const query of queries) {
    const response = await fetch(`${extraction}/alert-rules/issue-preview?${query}`);
    expect(response.status, query).toBe(200);
    const body = (await response.json()) as Schema<"IssueAlertPreview">;
    expect(body.from, query).toBe("2026-01-01T12:00:00.000000000Z");
  }
});

it("fires the scanned PDFs rule at 14 in the last hour, since its 11th scanned PDF", () => {
  const alert = storyAlertRule(stories.extraction, oneOf(STORY_RULES, "extraction"));
  expect(alert.state).toBe("firing");
  expect(alert.last_value).toBe(14);
  expect(alert.last_count).toBe(14);
  const starts = startedIn(stories.extraction, NOW - HOUR_MS, NOW)
    .filter((trace) => trace.tags.includes("scanned"))
    .map((trace) => Date.parse(trace.start_time))
    .sort((left, right) => left - right);
  expect(starts).toHaveLength(14);
  expect(Date.parse(alert.last_fired_at ?? "")).toBe(starts[10]);
});

it("sends the voice rule to the on-call pager and the other rules to Team Slack", () => {
  for (const rule of STORY_RULES) {
    const dataset = stories[rule.story];
    const alert = storyAlertRule(dataset, rule);
    const event = storyAlertEvent(dataset, rule);
    const expected = rule.id === VOICE_RULE ? ON_CALL_PAGER_ID : TEAM_SLACK_ID;
    expect(alert.channel_ids).toEqual([expected]);
    expect(event.deliveries.map((delivery) => delivery.channel_id)).toEqual([expected]);
  }
  const voice = storyAlertEvent(stories.voice, oneOf(STORY_RULES, "voice"));
  expect(
    voice.deliveries.map((delivery) => [delivery.channel_name, delivery.channel_kind]),
  ).toEqual([["On-call", "webhook"]]);
  const support = storyAlertEvent(stories.support, oneOf(STORY_RULES, "support"));
  expect(
    support.deliveries.map((delivery) => [delivery.channel_name, delivery.channel_kind]),
  ).toEqual([["Team Slack", "slack"]]);
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
    FIXTURE_ISSUE,
  ]);
});

it("serves the story issues and rules before the fixtures, and lets other ids fall through", async () => {
  server.use(...allStoryHandlers());

  for (const story of STORY_IDS) {
    const issues = (await (await fetch(`${baseOf(story)}/issues`)).json()) as Schema<"IssuePage">;
    expect(issues.data.map((issue) => issue.id)).toEqual([
      oneOf(STORY_ISSUES, story).id,
      ...(story === "support" ? [FIXTURE_ISSUE] : []),
    ]);
    const rules = (await (
      await fetch(`${baseOf(story)}/alert-rules`)
    ).json()) as Schema<"AlertRulePage">;
    expect(rules.data.slice(0, 1).map((rule) => rule.id)).toEqual([oneOf(STORY_RULES, story).id]);
  }

  const occurrences = await fetch(`${baseOf("coding")}/issues/${CODING_ISSUE}/occurrences`);
  expect(occurrences.status).toBe(200);

  const voice = (await (
    await fetch(`${baseOf("voice")}/alert-rules/${VOICE_RULE}`)
  ).json()) as Schema<"AlertRule">;
  expect(voice.id).toBe(VOICE_RULE);
  expect(voice.name).toBe("Slow replies on the voice desk");

  const fixture = (await (
    await fetch(`${PROJECT}/issues/${FIXTURE_ISSUE}`)
  ).json()) as Schema<"Issue">;
  expect(fixture.id).toBe(FIXTURE_ISSUE);
  expect(fixture.title).toBe("ValueError: could not parse the reply");
});

it("answers the story events routes, and lets the fixture rule's routes fall through", async () => {
  server.use(...allStoryHandlers());

  const detail = (await (
    await fetch(`${baseOf("coding")}/issues/${CODING_ISSUE}`)
  ).json()) as Schema<"Issue">;
  const issueEvents = (await (
    await fetch(`${baseOf("coding")}/issues/${CODING_ISSUE}/events`)
  ).json()) as Schema<"IssueEventPage">;
  expect(issueEvents.data.map((event) => [event.kind, event.at])).toEqual([
    ["created", detail.first_seen],
  ]);

  const voiceEvents = (await (
    await fetch(`${baseOf("voice")}/alert-rules/${VOICE_RULE}/events`)
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

  for (const story of STORY_IDS) {
    const alertEvents = (await (
      await fetch(`${baseOf(story)}/alert-events`)
    ).json()) as Schema<"AlertEventPage">;
    const storyEvents = STORY_RULES.filter((rule) => rule.story === story)
      .map((rule) => storyAlertEvent(stories[story], rule))
      .sort((left, right) => Date.parse(right.created_at) - Date.parse(left.created_at));
    const head = alertEvents.data.slice(0, storyEvents.length);
    expect(head.map((event) => event.id)).toEqual(storyEvents.map((event) => event.id));
    const times = head.map((event) => Date.parse(event.created_at));
    expect(times).toEqual([...times].sort((left, right) => right - left));
    // No story project lists the fixture events: they follow the story events only when unscoped.
    expect(alertEvents.data.slice(storyEvents.length)).toEqual([]);
  }
});

it("points every story queue's item at the queue's own target", () => {
  for (const queue of STORY_QUEUES) {
    const dataset = stories[queue.story];
    const item = storyQueueItem(dataset, queue);
    expect(item?.queue_id).toBe(queue.id);
    expect(item?.status).toBe("pending");
    // The queue's target is what its item is: a session queue's item names a session, not a trace.
    const target = storyQueue(dataset, queue).target;
    expect(target).toBe(queue.sessionId === undefined ? "trace" : "session");
    if (queue.sessionId === undefined) {
      expect(item?.trace_id).toBe(dataset.traces[queue.heroIndex ?? -1]?.trace_id);
      expect(item?.session_id).toBeUndefined();
    } else {
      expect(item?.session_id).toBe(queue.sessionId);
      expect(item?.trace_id).toBeUndefined();
    }
  }
});

it("targets the support queue at its session, whose first turn is the item's time", () => {
  const support = oneOf(STORY_QUEUES, "support");
  expect(support.sessionId).toBe("sess-0142");
  expect(support.heroIndex).toBeUndefined();
  const first = visibleTraces(stories.support.traces, false, false)
    .filter((trace) => trace.session_id === "sess-0142")
    .map((trace) => Date.parse(trace.start_time))
    .sort((left, right) => left - right)[0];
  const item = storyQueueItem(stories.support, support);
  expect(storyQueue(stories.support, support).target).toBe("session");
  expect(item?.session_id).toBe("sess-0142");
  expect(Date.parse(item?.target_time ?? "")).toBe(first);
  expect(storyQueue(stories.support, support).counts).toEqual({
    pending: 21,
    completed: 3,
    skipped: 1,
  });
});

it("gives every story queue its own score fields", () => {
  const names = new Map<string, string>([
    ["01900000-0000-7000-8000-000000000611", "correctness"],
    ["01900000-0000-7000-8000-000000000612", "relevance"],
    ["01900000-0000-7000-8000-000000000615", "tone"],
    ...STORY_SCORE_DEFINITIONS.map((definition): [string, string] => [
      definition.id,
      definition.name,
    ]),
  ]);
  expect(STORY_QUEUES.map((queue) => queue.definitionIds.map((id) => names.get(id)))).toEqual([
    ["correctness", "tone", "policy"],
    ["tests_pass", "verdict"],
    ["resolved", "tone"],
    ["schema_valid", "fields_correct"],
  ]);
  expect(STORY_SCORE_DEFINITIONS.map((definition) => definition.id)).toEqual([
    "01900000-0000-7000-8000-000000570601",
    "01900000-0000-7000-8000-000000570602",
    "01900000-0000-7000-8000-000000570603",
    "01900000-0000-7000-8000-000000570605",
    "01900000-0000-7000-8000-000000570606",
    "01900000-0000-7000-8000-000000570607",
    "01900000-0000-7000-8000-000000570608",
    "01900000-0000-7000-8000-000000570609",
    "01900000-0000-7000-8000-000000570610",
    "01900000-0000-7000-8000-000000570611",
  ]);
  expect(
    STORY_SCORE_DEFINITIONS.filter((definition) => definition.data_type === "categorical").map(
      (definition) => [definition.name, definition.categories.map((category) => category.label)],
    ),
  ).toEqual([
    ["verdict", ["approve", "request changes", "reject"]],
    ["fields_correct", ["all", "some", "none"]],
  ]);
});

it("serves the story queues, their items and the lease, and lets other ids fall through", async () => {
  server.use(...allStoryHandlers());

  for (const story of STORY_IDS) {
    const list = (await (await fetch(`${baseOf(story)}/annotation-queues`)).json()) as {
      data: { id: string }[];
    };
    expect(list.data.map((queue) => queue.id)).toEqual([
      oneOf(STORY_QUEUES, story).id,
      ...(story === "support" ? [FIXTURE_QUEUE] : []),
    ]);
  }

  const codingHero = stories.coding.traces[4]?.trace_id;
  const byTrace = (await (
    await fetch(`${baseOf("coding")}/annotation-queues?target_trace_id=${codingHero}`)
  ).json()) as {
    data: { id: string }[];
  };
  expect(byTrace.data.map((queue) => queue.id)).toEqual(["01900000-0000-7000-8000-000000570502"]);

  const queuePath = `${baseOf("voice")}/annotation-queues`;
  const itemPath = `${queuePath}/01900000-0000-7000-8000-000000570503/items/01900000-0000-7000-8000-000000570513`;
  const call = (await (
    await fetch(`${queuePath}/01900000-0000-7000-8000-000000570503`)
  ).json()) as Schema<"AnnotationQueue">;
  expect(call.name).toBe("Call review");
  expect(call.instructions).toBe(
    "Listen for whether the caller got what they asked for, and how the reply sounded.",
  );
  expect(call.counts).toEqual({ pending: 20, completed: 7, skipped: 2 });

  const voiceTrace = stories.voice.traces[5]?.trace_id;
  const item = (await (await fetch(itemPath)).json()) as Schema<"AnnotationQueueItem">;
  expect(item.trace_id).toBe(voiceTrace);
  const leased = (await (
    await fetch(`${itemPath}/lease`, { method: "POST" })
  ).json()) as Schema<"AnnotationQueueItem">;
  expect(leased).toEqual(item);

  const completed = (await (
    await fetch(`${queuePath}/01900000-0000-7000-8000-000000570503/items?status=completed`)
  ).json()) as { data: unknown[] };
  expect(completed.data).toEqual([]);

  const fixture = (await (
    await fetch(`${PROJECT}/annotation-queues/${FIXTURE_QUEUE}`)
  ).json()) as Schema<"AnnotationQueue">;
  expect(fixture.name).toBe("Relevance review");
});

it("serves the support queue as a session queue, and its item as a session item the reviewer can open", async () => {
  server.use(...allStoryHandlers());
  const support = oneOf(STORY_QUEUES, "support");
  const queuePath = `${baseOf("support")}/annotation-queues/${support.id}`;
  const queue = (await (await fetch(queuePath)).json()) as Schema<"AnnotationQueue">;
  expect(queue.target).toBe("session");
  const item = (await (
    await fetch(`${queuePath}/items/${support.itemId}`)
  ).json()) as Schema<"AnnotationQueueItem">;
  expect(item.id).toBe(support.itemId);
  expect(item.session_id).toBe("sess-0142");
  expect(item).not.toHaveProperty("trace_id");
  expect(item.queue_id).toBe(support.id);
});

it("counts a story queue's progress as completed plus skipped of its total", () => {
  const progress = STORY_QUEUES.map((queue) => {
    const { counts } = storyQueue(stories[queue.story], queue);
    const total = counts.pending + counts.completed + counts.skipped;
    return `${counts.completed + counts.skipped}/${total}`;
  });
  expect(progress).toEqual(["4/25", "9/15", "9/29", "21/52"]);
});

it("a project serves only its story's objects", async () => {
  const served = async (path: string) =>
    ((await (await fetch(path)).json()) as { data: { id: string }[] }).data
      .map((object) => object.id)
      .filter((id) => STORY_OBJECT_IDS.has(id));
  const coding = baseOf("coding");

  server.use(...storyHandlers(stories.coding, "acme/code-agent"));
  expect(await served(`${coding}/issues`)).toEqual([CODING_ISSUE]);
  expect(await served(`${coding}/alert-rules`)).toEqual(["01900000-0000-7000-8000-000000570202"]);
  expect(await served(`${coding}/annotation-queues`)).toEqual([
    "01900000-0000-7000-8000-000000570502",
  ]);

  const playground = `${window.location.origin}/api/v1/projects/acme%2Fplayground`;
  server.use(...storyHandlers(stories.coding, "acme/playground"));
  expect(await served(`${playground}/issues`)).toEqual([]);
  expect(await served(`${playground}/alert-rules`)).toEqual([]);
  expect(await served(`${playground}/annotation-queues`)).toEqual([]);

  server.use(...storyHandlers(stories.coding));
  expect(await served(`${baseOf("voice")}/alert-rules`)).toEqual(
    STORY_RULES.map((rule) => rule.id),
  );
  expect(await served(`${baseOf("voice")}/annotation-queues`)).toEqual(
    STORY_QUEUES.map((queue) => queue.id),
  );
});

it("a story project lists only its own alert rules and events", async () => {
  const codingRule = "01900000-0000-7000-8000-000000570202";
  const ruleIds = async (path: string) =>
    ((await (await fetch(path)).json()) as Schema<"AlertRulePage">).data.map((rule) => rule.id);

  server.use(...storyHandlers(stories.coding, "acme/code-agent"));
  const coding = baseOf("coding");
  expect(await ruleIds(`${coding}/alert-rules`)).toEqual([codingRule]);
  const codingEvents = (await (
    await fetch(`${coding}/alert-events`)
  ).json()) as Schema<"AlertEventPage">;
  const codingRuleObject = oneOf(STORY_RULES, "coding");
  expect(codingRule).toBe(codingRuleObject.id);
  expect(codingEvents.data.map((event) => event.id)).toEqual([
    storyAlertEvent(stories.coding, codingRuleObject).id,
  ]);

  server.use(...storyHandlers(stories.support, "acme/support-bot"));
  const support = baseOf("support");
  expect(await ruleIds(`${support}/alert-rules`)).toEqual([SUPPORT_RULE]);
  const supportEvents = (await (
    await fetch(`${support}/alert-events`)
  ).json()) as Schema<"AlertEventPage">;
  expect(supportEvents.data.map((event) => event.id)).toEqual([
    storyAlertEvent(stories.support, oneOf(STORY_RULES, "support")).id,
  ]);
});

it("lists only the story rule under acme/support-bot, and keeps the fixture rules unscoped", async () => {
  const ruleIds = async (path: string) =>
    ((await (await fetch(path)).json()) as Schema<"AlertRulePage">).data.map((rule) => rule.id);
  const eventIds = async (path: string) =>
    ((await (await fetch(path)).json()) as Schema<"AlertEventPage">).data.map((event) => event.id);
  const fixtureNames = new Set(alertRulesFixture.data.map((rule) => rule.name));
  expect(fixtureNames).toEqual(new Set(["High error rate", "Low helpfulness", "Daily cost"]));

  server.use(...storyHandlers(stories.support, "acme/support-bot"));
  const names = (
    (await (await fetch(`${baseOf("support")}/alert-rules`)).json()) as Schema<"AlertRulePage">
  ).data.map((rule) => rule.name);
  expect(names).toEqual([oneOf(STORY_RULES, "support").name]);
  for (const name of fixtureNames) {
    expect(names).not.toContain(name);
  }
  const events = await eventIds(`${baseOf("support")}/alert-events`);
  expect(events).toEqual([storyAlertEvent(stories.support, oneOf(STORY_RULES, "support")).id]);

  // Unscoped, the fixtures are listed after the story objects, as before.
  server.use(...storyHandlers(stories.support));
  const unscoped = await ruleIds(`${PROJECT}/alert-rules`);
  expect(unscoped).toEqual([
    ...STORY_RULES.map((rule) => rule.id),
    ...alertRulesFixture.data.map((rule) => rule.id),
  ]);
});

it("a story project lists only its own issues", async () => {
  server.use(...storyHandlers(stories.coding, "acme/code-agent"));
  const issues = (await (await fetch(`${baseOf("coding")}/issues`)).json()) as Schema<"IssuePage">;
  expect(issues.data.map((issue) => issue.id)).toEqual([CODING_ISSUE]);
});

it("a story's detail routes are scoped to its project", async () => {
  const dataset = stories.coding;
  const codingRule = "01900000-0000-7000-8000-000000570202";
  server.use(...storyHandlers(dataset, "acme/code-agent"));

  const voice = baseOf("voice");
  const issueFromVoice = await fetch(`${voice}/issues/${CODING_ISSUE}`);
  expect(issueFromVoice.status).not.toBe(200);
  const ruleFromVoice = await fetch(`${voice}/alert-rules/${codingRule}`);
  expect(ruleFromVoice.status).not.toBe(200);

  const coding = baseOf("coding");
  const issue = (await (await fetch(`${coding}/issues/${CODING_ISSUE}`)).json()) as Schema<"Issue">;
  expect(issue.id).toBe(CODING_ISSUE);
  expect(issue.title).toBe(storyIssue(dataset, oneOf(STORY_ISSUES, "coding"))?.title);
  const rule = (await (
    await fetch(`${coding}/alert-rules/${codingRule}`)
  ).json()) as Schema<"AlertRule">;
  expect(rule.id).toBe(codingRule);
  expect(rule.name).toBe(storyAlertRule(dataset, oneOf(STORY_RULES, "coding")).name);
});
