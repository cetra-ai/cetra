import { type HttpHandler, HttpResponse, http } from "msw";
import type { Schema } from "@/api/types";
import { type Dataset, rfc3339, visibleTraces } from "@/mocks/generated/dataset";
import alertRulesFixture from "../../../../fixtures/api/static/alert-rules.json";
import queuesFixture from "../../../../fixtures/api/static/annotation-queues.json";
import issuesFixture from "../../../../fixtures/api/static/issues.json";
import projectAlertEventsFixture from "../../../../fixtures/api/static/project-alert-events.json";
import { projectOfRef, type StoryId, storyProject } from "../projects";

/**
 * The homepage stories' issues and alert rules (design §12, M13), next to the generated traces.
 * Each is tied to its story's hand-written trace (`SHOWCASES`), and has an id of its own, so the
 * fixtures keep theirs. Only the large mock serves them.
 */

const PROJECT = "/api/v1/projects/:project";
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
  /** The story whose project serves the issue. */
  story: StoryId;
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
    story: "support",
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
    story: "coding",
    eventId: "01900000-0000-7000-8000-000000570112",
    sourceId: "01900000-0000-7000-8000-000000570302",
    sourceName: "Errors",
    kind: "error",
    title: "run_tests failed: 1 failed, 47 passed",
    description: "pytest reports failures after the agent's first patch.",
    hero: { index: 4, spanId: "0000000400000009", reason: "1 failed, 47 passed" },
    matches: (trace) => trace.status === "error",
  },
  {
    id: "01900000-0000-7000-8000-000000570103",
    story: "voice",
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
    story: "extraction",
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
  /** The story whose project serves the rule. */
  story: StoryId;
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
    story: "support",
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
    story: "coding",
    eventId: "01900000-0000-7000-8000-000000570212",
    deliveryId: "01900000-0000-7000-8000-000000570222",
    name: "Error rate above 5% on code-agent",
    condition: {
      metric: "error_rate",
      window: "15m",
      comparator: "gt",
      threshold: 0.05,
      min_count: 20,
    },
    state: "firing",
    lastValue: 0.083,
    lastCount: 36,
    changedAgoMs: 6 * MINUTE_MS,
  },
  {
    id: "01900000-0000-7000-8000-000000570203",
    story: "voice",
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
    story: "extraction",
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

const MEMBER_ID = "01900000-0000-7000-8000-000000000100";
/** The generated definitions the queues reuse (`SCORE_DEFINITIONS` in handlers.ts): correctness and relevance for Support answers, tone for Call review. */
const CORRECTNESS_ID = "01900000-0000-7000-8000-000000000611";
const RELEVANCE_ID = "01900000-0000-7000-8000-000000000612";
const TONE_ID = "01900000-0000-7000-8000-000000000615";
const DEFINED_AT = "2026-01-01T09:00:00.000000000Z";
const LEASE_MS = 5 * MINUTE_MS;

type StoryQueue = {
  id: string;
  /** The story whose project serves the queue. */
  story: StoryId;
  itemId: string;
  name: string;
  description: string;
  instructions: string;
  /** The review fields, in order. */
  definitionIds: readonly string[];
  counts: Schema<"QueueCounts">;
  /** Dataset index of the hero trace, the queue's one item. */
  heroIndex: number;
};

export const STORY_SCORE_DEFINITIONS: Schema<"ScoreDefinition">[] = [
  {
    id: "01900000-0000-7000-8000-000000570601",
    name: "tests_pass",
    data_type: "boolean",
    categories: [],
    description: "Whether the tests pass after the patch.",
    direction: "higher",
    created_at: DEFINED_AT,
    updated_at: DEFINED_AT,
  },
  {
    id: "01900000-0000-7000-8000-000000570602",
    name: "verdict",
    data_type: "categorical",
    categories: [{ label: "approve" }, { label: "request changes" }, { label: "reject" }],
    description: "Your review of the patch.",
    direction: "none",
    created_at: DEFINED_AT,
    updated_at: DEFINED_AT,
  },
  {
    id: "01900000-0000-7000-8000-000000570603",
    name: "resolved",
    data_type: "boolean",
    categories: [],
    description: "Whether the caller's request was handled.",
    direction: "higher",
    created_at: DEFINED_AT,
    updated_at: DEFINED_AT,
  },
  {
    id: "01900000-0000-7000-8000-000000570605",
    name: "schema_valid",
    data_type: "boolean",
    categories: [],
    description: "Whether the extracted invoice passes the schema check.",
    direction: "higher",
    created_at: DEFINED_AT,
    updated_at: DEFINED_AT,
  },
  {
    id: "01900000-0000-7000-8000-000000570606",
    name: "fields_correct",
    data_type: "categorical",
    categories: [{ label: "all" }, { label: "some" }, { label: "none" }],
    description: "How many fields match the invoice.",
    direction: "none",
    created_at: DEFINED_AT,
    updated_at: DEFINED_AT,
  },
];

export const STORY_QUEUES: readonly StoryQueue[] = [
  {
    id: "01900000-0000-7000-8000-000000570501",
    story: "support",
    itemId: "01900000-0000-7000-8000-000000570511",
    name: "Support answers",
    description: "Support answers that people review after the judges.",
    instructions:
      "Score each answer against the customer's question. Skip the ones without a question.",
    definitionIds: [CORRECTNESS_ID, RELEVANCE_ID],
    counts: { pending: 12, completed: 4, skipped: 1 },
    heroIndex: 3,
  },
  {
    id: "01900000-0000-7000-8000-000000570502",
    story: "coding",
    itemId: "01900000-0000-7000-8000-000000570512",
    name: "Patch review",
    description: "Patches from the coding agent, before they are merged.",
    instructions: "Check that the patch fixes the issue and that the tests pass.",
    definitionIds: ["01900000-0000-7000-8000-000000570601", "01900000-0000-7000-8000-000000570602"],
    counts: { pending: 6, completed: 9, skipped: 0 },
    heroIndex: 4,
  },
  {
    id: "01900000-0000-7000-8000-000000570503",
    story: "voice",
    itemId: "01900000-0000-7000-8000-000000570513",
    name: "Call review",
    description: "Calls that the voice agent handled.",
    instructions:
      "Listen for whether the caller got what they asked for, and how the reply sounded.",
    definitionIds: ["01900000-0000-7000-8000-000000570603", TONE_ID],
    counts: { pending: 20, completed: 7, skipped: 2 },
    heroIndex: 5,
  },
  {
    id: "01900000-0000-7000-8000-000000570504",
    story: "extraction",
    itemId: "01900000-0000-7000-8000-000000570514",
    name: "Invoice fields",
    description: "Invoices that the extraction pipeline read.",
    instructions: "Compare the extracted fields with the invoice.",
    definitionIds: ["01900000-0000-7000-8000-000000570605", "01900000-0000-7000-8000-000000570606"],
    counts: { pending: 31, completed: 18, skipped: 3 },
    heroIndex: 6,
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

/** An issue's traces: the hero (if the dataset has it), the generated occurrences, and both. */
type Occurrences = { hero: Trace | undefined; generated: Trace[]; matched: Trace[] };

/** The hero trace (if the dataset has it), then the generated occurrences, newest first. */
function occurrenceTraces(dataset: Dataset, issue: StoryIssue): Occurrences {
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

/** The traces of an issue that matches at least one, or undefined when none does. */
function matchedTraces(dataset: Dataset, issue: StoryIssue): Occurrences | undefined {
  const traces = occurrenceTraces(dataset, issue);
  return traces.matched.length === 0 ? undefined : traces;
}

/** Oldest first. */
function oldestFirst(traces: readonly Trace[]): Trace[] {
  return [...traces].sort(
    (left, right) => Date.parse(left.start_time) - Date.parse(right.start_time),
  );
}

/** The issue's occurrences, grouped by UTC day, with the first examples. */
function occurrencesOf(issue: StoryIssue, traces: Occurrences): Schema<"IssueOccurrences"> {
  const { hero, generated, matched } = traces;
  const byDay = new Map<number, Trace[]>();
  for (const trace of matched) {
    const day = Math.floor(Date.parse(trace.start_time) / DAY_MS) * DAY_MS;
    let inDay = byDay.get(day);
    if (inDay === undefined) {
      inDay = [];
      byDay.set(day, inDay);
    }
    inDay.push(trace);
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

export function storyOccurrences(dataset: Dataset, issue: StoryIssue): Schema<"IssueOccurrences"> {
  return occurrencesOf(issue, occurrenceTraces(dataset, issue));
}

/** The issue, or undefined when no trace of the dataset matches it. */
export function storyIssue(dataset: Dataset, issue: StoryIssue): Schema<"Issue"> | undefined {
  const traces = matchedTraces(dataset, issue);
  if (traces === undefined) {
    return undefined;
  }
  const { matched } = traces;
  const sorted = oldestFirst(matched);
  const oldest = sorted[0];
  const newest = sorted[sorted.length - 1];
  if (oldest === undefined || newest === undefined) {
    return undefined;
  }
  return {
    id: issue.id,
    project_id: storyProject(issue.story).id,
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

export function storyIssueEvents(
  dataset: Dataset,
  issue: StoryIssue,
): Schema<"IssueEventPage"> | undefined {
  const traces = matchedTraces(dataset, issue);
  const first = traces === undefined ? undefined : oldestFirst(traces.matched)[0];
  if (first === undefined) {
    return undefined;
  }
  return { data: [{ id: issue.eventId, kind: "created", at: first.start_time }] };
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

export function storyQueue(dataset: Dataset, queue: StoryQueue): Schema<"AnnotationQueue"> {
  return {
    id: queue.id,
    name: queue.name,
    description: queue.description,
    instructions: queue.instructions,
    target: "trace",
    definition_ids: [...queue.definitionIds],
    version: 1,
    created_by: MEMBER_ID,
    created_at: ago(dataset, 7 * DAY_MS),
    updated_at: ago(dataset, HOUR_MS),
    counts: queue.counts,
  };
}

/** The queue's one item: the hero trace, pending and leased to the mock user. Undefined when the dataset has no such trace. */
export function storyQueueItem(
  dataset: Dataset,
  queue: StoryQueue,
): Schema<"AnnotationQueueItem"> | undefined {
  const hero: Trace | undefined = dataset.traces[queue.heroIndex];
  if (hero === undefined) {
    return undefined;
  }
  return {
    id: queue.itemId,
    queue_id: queue.id,
    trace_id: hero.trace_id,
    target_time: hero.start_time,
    status: "pending",
    leased_by_user_id: MEMBER_ID,
    lease_expires_at: rfc3339(dataset.now + LEASE_MS),
    added_by: "user",
    added_by_id: MEMBER_ID,
    created_at: ago(dataset, 6 * HOUR_MS),
  };
}

/** A path parameter as text; msw types it as possibly repeated or missing. */
function idParam(value: string | readonly string[] | undefined): string {
  return typeof value === "string" ? value : "";
}

/**
 * The story issues, rules and queues, before the fixtures. A story issue that matches no trace is
 * not served at all. A handler that returns undefined lets the request fall through to the
 * fixtures' handlers. With `project` (an API ref, `acme/<slug>`) the routes use its literal prefix
 * and serve only the objects of that project's story (none for a ref that is no story project);
 * without it, every object is served under `/api/v1/projects/:project`.
 */
export function storyHandlers(dataset: Dataset, project?: string): HttpHandler[] {
  const prefix =
    project === undefined ? PROJECT : `/api/v1/projects/${encodeURIComponent(project)}`;
  const scope = project === undefined ? undefined : projectOfRef(project)?.story;
  const served = (story: StoryId) => project === undefined || story === scope;
  const issues = STORY_ISSUES.filter((issue) => served(issue.story));
  const rules = STORY_RULES.filter((rule) => served(rule.story));
  const queues = STORY_QUEUES.filter((queue) => served(queue.story));
  const issueById = (id: string) => issues.find((issue) => issue.id === id);
  const ruleById = (id: string) => rules.find((rule) => rule.id === id);
  const queueById = (id: string) => queues.find((queue) => queue.id === id);
  const QUEUE = `${prefix}/annotation-queues`;
  const heroItem = (queue: StoryQueue) => storyQueueItem(dataset, queue);
  /** The item of a story queue, when `itemId` names that queue's item. */
  const itemOf = (queueId: string, itemId: string) => {
    const queue = queueById(queueId);
    return queue !== undefined && queue.itemId === itemId ? heroItem(queue) : undefined;
  };
  return [
    http.get(`${prefix}/issues`, () =>
      HttpResponse.json({
        data: [
          ...issues.flatMap((issue) => {
            const detail = storyIssue(dataset, issue);
            return detail === undefined ? [] : [detail];
          }),
          ...issuesFixture.data,
        ],
        next_cursor: null,
      }),
    ),
    http.get(`${PROJECT}/issues/:id`, ({ params }) => {
      const issue = issueById(idParam(params.id));
      const detail = issue ? storyIssue(dataset, issue) : undefined;
      return detail ? HttpResponse.json(detail) : undefined;
    }),
    http.get(`${PROJECT}/issues/:id/occurrences`, ({ params }) => {
      const issue = issueById(idParam(params.id));
      const traces = issue ? matchedTraces(dataset, issue) : undefined;
      return issue && traces ? HttpResponse.json(occurrencesOf(issue, traces)) : undefined;
    }),
    http.get(`${PROJECT}/issues/:id/events`, ({ params }) => {
      const issue = issueById(idParam(params.id));
      const events = issue ? storyIssueEvents(dataset, issue) : undefined;
      return events ? HttpResponse.json(events) : undefined;
    }),
    http.get(`${prefix}/alert-rules`, () =>
      HttpResponse.json({
        data: [...rules.map((rule) => storyAlertRule(dataset, rule)), ...alertRulesFixture.data],
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
    http.get(`${prefix}/alert-events`, () => {
      const events = rules
        .map((rule) => storyAlertEvent(dataset, rule))
        .sort((left, right) => Date.parse(right.created_at) - Date.parse(left.created_at));
      return HttpResponse.json({
        data: [...events, ...projectAlertEventsFixture.data],
        next_cursor: null,
      });
    }),
    http.get(QUEUE, ({ request }) => {
      const search = new URL(request.url).searchParams;
      if (search.has("target_session_id")) {
        return undefined;
      }
      const traceId = search.get("target_trace_id");
      if (traceId !== null) {
        const found = queues
          .filter((queue) => dataset.traces[queue.heroIndex]?.trace_id === traceId)
          .map((queue) => storyQueue(dataset, queue));
        return found.length === 0
          ? undefined
          : HttpResponse.json({ data: found, next_cursor: null });
      }
      return HttpResponse.json({
        data: [...queues.map((queue) => storyQueue(dataset, queue)), ...queuesFixture.data],
        next_cursor: null,
      });
    }),
    http.get(`${QUEUE}/:id`, ({ params }) => {
      const queue = queueById(idParam(params.id));
      return queue ? HttpResponse.json(storyQueue(dataset, queue)) : undefined;
    }),
    http.get(`${QUEUE}/:id/items`, ({ params, request }) => {
      const queue = queueById(idParam(params.id));
      const item = queue ? heroItem(queue) : undefined;
      if (item === undefined) {
        return undefined;
      }
      const status = new URL(request.url).searchParams.get("status");
      return HttpResponse.json({
        data: status === null || status === "pending" ? [item] : [],
        next_cursor: null,
      });
    }),
    http.get(`${QUEUE}/:id/items/:item_id`, ({ params }) => {
      const item = itemOf(idParam(params.id), idParam(params.item_id));
      return item ? HttpResponse.json(item) : undefined;
    }),
    http.post(`${QUEUE}/:id/items/:item_id/lease`, ({ params }) => {
      const item = itemOf(idParam(params.id), idParam(params.item_id));
      return item ? HttpResponse.json(item) : undefined;
    }),
    http.post(`${QUEUE}/:id/next`, ({ params }) => {
      const queue = queueById(idParam(params.id));
      const item = queue ? heroItem(queue) : undefined;
      return item ? HttpResponse.json(item) : undefined;
    }),
  ];
}
