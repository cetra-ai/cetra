import { type HttpHandler, HttpResponse, http } from "msw";
import type { Schema } from "@/api/types";
import { type Dataset, rfc3339, visibleTraces } from "@/mocks/generated/dataset";
import alertRulesFixture from "../../../../fixtures/api/static/alert-rules.json";
import issuesFixture from "../../../../fixtures/api/static/issues.json";
import projectAlertEventsFixture from "../../../../fixtures/api/static/project-alert-events.json";

/**
 * The homepage stories' issues and alert rules (design §12, M13), next to the generated traces.
 * Each is tied to its story's hand-written trace (`SHOWCASES`), and has an id of its own, so the
 * fixtures keep theirs. Only the large mock serves them.
 */

const PROJECT = "/api/v1/projects/:project";
const PROJECT_ID = "01900000-0000-7000-8000-000000000001";
const TEAM_SLACK = "01900000-0000-7000-8000-000000000500";
const MINUTE_MS = 60_000;
const HOUR_MS = 60 * MINUTE_MS;
const DAY_MS = 24 * HOUR_MS;
/** Occurrences count over the 14 days before now, as the issue page asks for. */
const OCCURRENCE_WINDOW_MS = 14 * DAY_MS;
/** Example traces after the hero trace. */
const GENERATED_EXAMPLES = 4;

type Trace = Schema<"TraceSummary">;

type StoryIssue = {
  id: string;
  eventId: string;
  sourceId: string;
  sourceName: string;
  kind: Schema<"IssueSourceKind">;
  title: string;
  description: string;
  /** The story's hand-written trace: its dataset index, the span the issue is about, and why. */
  hero: { index: number; spanId?: string; reason: string };
  /** The generated traces that are occurrences too. */
  matches: (trace: Trace) => boolean;
  /** The reason shown for a generated example; absent shows the trace id. */
  reasonOf?: (trace: Trace) => string | undefined;
};

export const STORY_ISSUES: readonly StoryIssue[] = [
  {
    id: "01900000-0000-7000-8000-000000570101",
    eventId: "01900000-0000-7000-8000-000000570111",
    sourceId: "01900000-0000-7000-8000-000000570301",
    sourceName: "Topics",
    kind: "topic",
    title: "Customers ask how to export traces to CSV",
    description: "Questions about exporting traces, from the Traces page or through the API.",
    hero: { index: 3, reason: "How can I export my traces to a CSV file?" },
    matches: (trace) => trace.input_preview === "How can I export my traces to a CSV file?",
    reasonOf: (trace) => trace.input_preview,
  },
  {
    id: "01900000-0000-7000-8000-000000570102",
    eventId: "01900000-0000-7000-8000-000000570112",
    sourceId: "01900000-0000-7000-8000-000000570302",
    sourceName: "Errors",
    kind: "error",
    title: "run_tests failed: 1 failed, 47 passed",
    description: "pytest reports failures after the agent's first patch.",
    hero: { index: 4, spanId: "0000000400000009", reason: "1 failed, 47 passed" },
    matches: (trace) => trace.status === "error" && trace.tags.includes("retry"),
  },
  {
    id: "01900000-0000-7000-8000-000000570103",
    eventId: "01900000-0000-7000-8000-000000570113",
    sourceId: "01900000-0000-7000-8000-000000570303",
    sourceName: "Latency budget",
    kind: "score",
    title: "Reply started more than 800 ms after the caller",
    description: "The time from the end of the caller's turn to the first audio of the reply.",
    hero: { index: 5, spanId: "0000000500000005", reason: "check_availability took 1.22 s" },
    matches: (trace) => trace.tags.includes("voice") && trace.duration_ns > 3_000_000_000,
  },
  {
    id: "01900000-0000-7000-8000-000000570104",
    eventId: "01900000-0000-7000-8000-000000570114",
    sourceId: "01900000-0000-7000-8000-000000570304",
    sourceName: "Checks",
    kind: "score",
    title: "Invoice total doesn't match its line items",
    description:
      "The schema check fails when the extracted total differs from the sum of the line items.",
    hero: { index: 6, spanId: "0000000600000005", reason: "Total 1,248.00, line items 1,284.00" },
    matches: (trace) => trace.status === "error" && trace.tags.includes("billing"),
  },
];

type StoryRule = {
  id: string;
  eventId: string;
  deliveryId: string;
  name: string;
  condition: Schema<"AlertCondition">;
  state: "firing" | "ok";
  lastValue: number;
  lastCount: number;
  /** When it last fired (firing), or resolved (ok), before now. */
  changedAgoMs: number;
};

export const STORY_RULES: readonly StoryRule[] = [
  {
    id: "01900000-0000-7000-8000-000000570201",
    eventId: "01900000-0000-7000-8000-000000570211",
    deliveryId: "01900000-0000-7000-8000-000000570221",
    name: "Answer correctness under 0.7 on support-bot",
    condition: {
      metric: "score",
      window: "1h",
      comparator: "lt",
      threshold: 0.7,
      min_count: 10,
      score_name: "correctness",
      aggregation: "avg",
      filter: "tags @> ['email']",
    },
    state: "ok",
    lastValue: 0.86,
    lastCount: 42,
    changedAgoMs: 3 * HOUR_MS,
  },
  {
    id: "01900000-0000-7000-8000-000000570202",
    eventId: "01900000-0000-7000-8000-000000570212",
    deliveryId: "01900000-0000-7000-8000-000000570222",
    name: "Error rate above 5% on code-agent",
    condition: {
      metric: "error_rate",
      window: "15m",
      comparator: "gt",
      threshold: 0.05,
      min_count: 20,
      filter: "tags @> ['retry']",
    },
    state: "firing",
    lastValue: 0.083,
    lastCount: 36,
    changedAgoMs: 6 * MINUTE_MS,
  },
  {
    id: "01900000-0000-7000-8000-000000570203",
    eventId: "01900000-0000-7000-8000-000000570213",
    deliveryId: "01900000-0000-7000-8000-000000570223",
    name: "Slow replies on voice-agent",
    condition: {
      metric: "latency_p95",
      target: "trace",
      window: "15m",
      comparator: "gt",
      threshold: 1_500,
      min_count: 20,
      filter: "tags @> ['voice']",
    },
    state: "firing",
    lastValue: 2_800,
    lastCount: 54,
    changedAgoMs: 4 * MINUTE_MS,
  },
  {
    id: "01900000-0000-7000-8000-000000570204",
    eventId: "01900000-0000-7000-8000-000000570214",
    deliveryId: "01900000-0000-7000-8000-000000570224",
    name: "Schema check pass rate under 97%",
    condition: {
      metric: "score",
      window: "1h",
      comparator: "lt",
      threshold: 0.97,
      min_count: 20,
      score_name: "schema_valid",
      aggregation: "pass_rate",
      filter: "tags @> ['billing']",
    },
    state: "ok",
    lastValue: 0.985,
    lastCount: 210,
    changedAgoMs: 5 * HOUR_MS,
  },
];

const WINDOW_MS: Record<Schema<"AlertWindow">, number> = {
  "1m": MINUTE_MS,
  "5m": 5 * MINUTE_MS,
  "15m": 15 * MINUTE_MS,
  "1h": HOUR_MS,
  "6h": 6 * HOUR_MS,
  "24h": DAY_MS,
};

/** `now` minus `ms`, as RFC 3339. */
function ago(dataset: Dataset, ms: number): string {
  return rfc3339(dataset.now - ms);
}

function byText(left: string, right: string): number {
  return left < right ? -1 : left > right ? 1 : 0;
}

/** Newest first; ties by trace id, descending. */
function newestFirst(left: Trace, right: Trace): number {
  return (
    Date.parse(right.start_time) - Date.parse(left.start_time) ||
    byText(right.trace_id, left.trace_id)
  );
}

function distinctUsers(traces: readonly Trace[]): number {
  return new Set(traces.flatMap((trace) => (trace.user_id === undefined ? [] : [trace.user_id])))
    .size;
}

/** The hero trace (if the dataset has it), then the generated occurrences, newest first. */
function occurrenceTraces(
  dataset: Dataset,
  issue: StoryIssue,
): { hero: Trace | undefined; generated: Trace[]; matched: Trace[] } {
  const hero: Trace | undefined = dataset.traces[issue.hero.index];
  const earliest = dataset.now - OCCURRENCE_WINDOW_MS;
  const generated = visibleTraces(dataset.traces, false, false)
    .filter((trace) => trace.trace_id !== hero?.trace_id && issue.matches(trace))
    .filter((trace) => {
      const started = Date.parse(trace.start_time);
      return started >= earliest && started <= dataset.now;
    })
    .sort(newestFirst);
  return { hero, generated, matched: hero ? [hero, ...generated] : generated };
}

/** The issue's occurrences, grouped by UTC day, with the first examples. */
export function storyOccurrences(dataset: Dataset, issue: StoryIssue): Schema<"IssueOccurrences"> {
  const { hero, generated, matched } = occurrenceTraces(dataset, issue);
  const byDay = new Map<number, Trace[]>();
  for (const trace of matched) {
    const day = Math.floor(Date.parse(trace.start_time) / DAY_MS) * DAY_MS;
    byDay.set(day, [...(byDay.get(day) ?? []), trace]);
  }
  const days = [...byDay.entries()]
    .sort(([left], [right]) => left - right)
    .map(([day, traces]) => ({
      day: rfc3339(day),
      count: traces.length,
      users: distinctUsers(traces),
    }));

  const examples: Schema<"IssueExample">[] = [];
  if (hero) {
    examples.push({
      trace_id: hero.trace_id,
      created_at: hero.start_time,
      reason: issue.hero.reason,
      issue_id: issue.id,
      ...(issue.hero.spanId === undefined ? {} : { span_id: issue.hero.spanId }),
    });
  }
  for (const trace of generated.slice(0, GENERATED_EXAMPLES)) {
    const reason = issue.reasonOf?.(trace);
    examples.push({
      trace_id: trace.trace_id,
      created_at: trace.start_time,
      issue_id: issue.id,
      ...(reason === undefined ? {} : { reason }),
    });
  }
  return { days, examples };
}

export function storyIssue(dataset: Dataset, issue: StoryIssue): Schema<"Issue"> {
  const { matched } = occurrenceTraces(dataset, issue);
  const oldestFirst = [...matched].sort(
    (left, right) => Date.parse(left.start_time) - Date.parse(right.start_time),
  );
  const oldest = oldestFirst[0];
  const newest = oldestFirst[oldestFirst.length - 1];
  if (oldest === undefined || newest === undefined) {
    throw new Error(`story issue ${issue.id} has no traces; the dataset is too small`);
  }
  return {
    id: issue.id,
    project_id: PROJECT_ID,
    source_id: issue.sourceId,
    source_name: issue.sourceName,
    kind: issue.kind,
    title: issue.title,
    description: issue.description,
    score_name: `issue:${issue.id}`,
    state: "open",
    regressed: false,
    merged_from: [],
    first_seen: oldest.start_time,
    last_seen: newest.start_time,
    created_at: oldest.start_time,
    updated_at: newest.start_time,
    count: matched.length,
    users: distinctUsers(matched),
  };
}

export function storyIssueEvents(dataset: Dataset, issue: StoryIssue): Schema<"IssueEventPage"> {
  return {
    data: [{ id: issue.eventId, kind: "created", at: storyIssue(dataset, issue).first_seen }],
  };
}

export function storyAlertRule(dataset: Dataset, rule: StoryRule): Schema<"AlertRule"> {
  const firing = rule.state === "firing";
  return {
    id: rule.id,
    name: rule.name,
    condition: rule.condition,
    state: rule.state,
    enabled: true,
    channel_ids: [TEAM_SLACK],
    cooldown_s: 3600,
    notify_while_firing: false,
    last_value: rule.lastValue,
    last_count: rule.lastCount,
    last_evaluated_at: ago(dataset, MINUTE_MS),
    ...(firing ? { last_fired_at: ago(dataset, rule.changedAgoMs) } : {}),
    created_at: ago(dataset, 7 * DAY_MS),
    updated_at: ago(dataset, 7 * DAY_MS),
  };
}

export function storyAlertEvent(dataset: Dataset, rule: StoryRule): Schema<"AlertEvent"> {
  const windowTo = ago(dataset, rule.changedAgoMs);
  return {
    id: rule.eventId,
    rule_id: rule.id,
    kind: rule.state === "firing" ? "fired" : "resolved",
    value: rule.lastValue,
    count: rule.lastCount,
    threshold: rule.condition.threshold,
    comparator: rule.condition.comparator,
    metric: rule.condition.metric,
    window: rule.condition.window,
    window_to: windowTo,
    window_from: rfc3339(Date.parse(windowTo) - WINDOW_MS[rule.condition.window]),
    created_at: windowTo,
    deliveries: [
      {
        id: rule.deliveryId,
        channel_id: TEAM_SLACK,
        channel_name: "Team Slack",
        channel_kind: "slack",
        status: "sent",
        attempts: 1,
        updated_at: windowTo,
      },
    ],
  };
}

/** A path parameter as text; msw types it as possibly repeated or missing. */
function idParam(value: string | readonly string[] | undefined): string {
  return typeof value === "string" ? value : "";
}

/**
 * The story issues and rules, before the fixtures. A handler that returns undefined lets the
 * request fall through to the fixtures' handlers.
 */
export function storyHandlers(dataset: Dataset): HttpHandler[] {
  const issueById = (id: string) => STORY_ISSUES.find((issue) => issue.id === id);
  const ruleById = (id: string) => STORY_RULES.find((rule) => rule.id === id);
  return [
    http.get(`${PROJECT}/issues`, () =>
      HttpResponse.json({
        data: [...STORY_ISSUES.map((issue) => storyIssue(dataset, issue)), ...issuesFixture.data],
        next_cursor: null,
      }),
    ),
    http.get(`${PROJECT}/issues/:id`, ({ params }) => {
      const issue = issueById(idParam(params.id));
      return issue ? HttpResponse.json(storyIssue(dataset, issue)) : undefined;
    }),
    http.get(`${PROJECT}/issues/:id/occurrences`, ({ params }) => {
      const issue = issueById(idParam(params.id));
      return issue ? HttpResponse.json(storyOccurrences(dataset, issue)) : undefined;
    }),
    http.get(`${PROJECT}/issues/:id/events`, ({ params }) => {
      const issue = issueById(idParam(params.id));
      return issue ? HttpResponse.json(storyIssueEvents(dataset, issue)) : undefined;
    }),
    http.get(`${PROJECT}/alert-rules`, () =>
      HttpResponse.json({
        data: [
          ...STORY_RULES.map((rule) => storyAlertRule(dataset, rule)),
          ...alertRulesFixture.data,
        ],
        next_cursor: null,
      }),
    ),
    http.get(`${PROJECT}/alert-rules/:id`, ({ params }) => {
      const rule = ruleById(idParam(params.id));
      return rule ? HttpResponse.json(storyAlertRule(dataset, rule)) : undefined;
    }),
    http.get(`${PROJECT}/alert-rules/:id/events`, ({ params }) => {
      const rule = ruleById(idParam(params.id));
      return rule
        ? HttpResponse.json({ data: [storyAlertEvent(dataset, rule)], next_cursor: null })
        : undefined;
    }),
    http.get(`${PROJECT}/alert-events`, () => {
      const events = STORY_RULES.map((rule) => storyAlertEvent(dataset, rule)).sort(
        (left, right) => Date.parse(right.created_at) - Date.parse(left.created_at),
      );
      return HttpResponse.json({
        data: [...events, ...projectAlertEventsFixture.data],
        next_cursor: null,
      });
    }),
  ];
}
