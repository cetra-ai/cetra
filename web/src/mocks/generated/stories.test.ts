import { beforeAll, expect, it } from "vitest";
import type { Schema } from "@/api/types";
import {
  createDataset,
  type Dataset,
  type Profile,
  rfc3339,
  SUPPORT_PROFILE,
  visibleTraces,
} from "@/mocks/generated/dataset";
import { CODING_PROFILE, EXTRACTION_PROFILE, VOICE_PROFILE } from "@/mocks/generated/profiles";
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
import { server } from "@/mocks/node";
import { type StoryId, storyProject } from "@/mocks/projects";
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
const FIXTURE_ISSUE = "01900000-0000-7000-8000-000000001c01";
const FIXTURE_QUEUE = "01900000-0000-7000-8000-000000000a01";
const STORY_IDS: readonly StoryId[] = ["support", "coding", "voice", "extraction"];
const STORY_OBJECT_IDS = new Set(
  [...STORY_ISSUES, ...STORY_RULES, ...STORY_QUEUES].map((object) => object.id),
);

const PROFILES: Record<StoryId, Profile> = {
  support: SUPPORT_PROFILE,
  coding: CODING_PROFILE,
  voice: VOICE_PROFILE,
  extraction: EXTRACTION_PROFILE,
};

/** A story's dataset: its generation profile, so its hero trace sits at the story's index. */
function datasetOf(story: StoryId, traces: number): Dataset {
  return createDataset({ seed: 1, now: NOW, traces, profile: PROFILES[story] });
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

/** The four story datasets of 2,000 traces, built once for the tests. */
let stories: Record<StoryId, Dataset>;

beforeAll(() => {
  stories = {
    support: datasetOf("support", 2000),
    coding: datasetOf("coding", 2000),
    voice: datasetOf("voice", 2000),
    extraction: datasetOf("extraction", 2000),
  };
});

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
  for (const issue of STORY_ISSUES) {
    const dataset = stories[issue.story];
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

it("gives every story issue at most five examples, the generated ones newest first", () => {
  for (const issue of STORY_ISSUES) {
    const dataset = stories[issue.story];
    const byId = new Map(dataset.traces.map((trace) => [trace.trace_id, trace]));
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
  const rules = STORY_RULES.map((rule) => storyAlertRule(stories[rule.story], rule));
  expect(rules.map((rule) => rule.state)).toEqual(["ok", "firing", "firing", "ok"]);
  for (const rule of STORY_RULES) {
    const event = storyAlertEvent(stories[rule.story], rule);
    expect(event.kind).toBe(rule.state === "firing" ? "fired" : "resolved");
    expect(Date.parse(event.window_to) - Date.parse(event.window_from)).toBe(
      WINDOW_MS[rule.condition.window],
    );
  }
});

it("sets last_fired_at on the two firing rules and omits it on the two ok ones", () => {
  expect(STORY_RULES.filter((rule) => rule.state === "firing")).toHaveLength(2);
  expect(STORY_RULES.filter((rule) => rule.state === "ok")).toHaveLength(2);
  for (const rule of STORY_RULES) {
    const dataset = stories[rule.story];
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
    FIXTURE_ISSUE,
  ]);
});

it("serves the story issues and rules before the fixtures, and lets other ids fall through", async () => {
  server.use(...allStoryHandlers());

  for (const story of STORY_IDS) {
    const issues = (await (await fetch(`${baseOf(story)}/issues`)).json()) as Schema<"IssuePage">;
    expect(issues.data.map((issue) => issue.id)).toEqual([
      oneOf(STORY_ISSUES, story).id,
      FIXTURE_ISSUE,
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
  expect(voice.name).toBe("Slow replies on voice-agent");

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
    expect(alertEvents.data.slice(storyEvents.length).map((event) => event.id)).toEqual(
      projectAlertEventsFixture.data.map((event) => event.id),
    );
  }
});

it("points every story queue's item at the story's hero trace", () => {
  STORY_QUEUES.forEach((queue, position) => {
    const dataset = stories[queue.story];
    const item = storyQueueItem(dataset, queue);
    const hero = dataset.traces[queue.heroIndex];
    expect(item?.trace_id).toBe(hero?.trace_id);
    expect(item?.queue_id).toBe(queue.id);
    expect(item?.status).toBe("pending");
    expect(queue.heroIndex).toBe(STORY_ISSUES[position]?.hero.index);
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
    ["correctness", "relevance"],
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
    expect(list.data.slice(0, 1).map((queue) => queue.id)).toEqual([oneOf(STORY_QUEUES, story).id]);
    expect(list.data[1]?.id).toBe(FIXTURE_QUEUE);
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

it("counts a story queue's progress as completed plus skipped of its total", () => {
  const progress = STORY_QUEUES.map((queue) => {
    const { counts } = storyQueue(stories[queue.story], queue);
    const total = counts.pending + counts.completed + counts.skipped;
    return `${counts.completed + counts.skipped}/${total}`;
  });
  expect(progress).toEqual(["5/17", "9/15", "9/29", "21/52"]);
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
