import { type HttpHandler, HttpResponse, http } from "msw";
import type { Schema } from "@/api/types";
import { type Dataset, percentile, rfc3339, visibleTraces } from "@/mocks/generated/dataset";
import { STEP_HOURS } from "@/mocks/generated/story-coding";
import { SSO_QUESTIONS, SUPPORT_SESSION } from "@/mocks/generated/story-support";
import alertRulesFixture from "../../../../fixtures/api/static/alert-rules.json";
import queuesFixture from "../../../../fixtures/api/static/annotation-queues.json";
import issuesFixture from "../../../../fixtures/api/static/issues.json";
import projectAlertEventsFixture from "../../../../fixtures/api/static/project-alert-events.json";
import { projectOfRef, type StoryId, storyProject } from "../projects";

/**
 * The homepage stories' issues and alert rules (design §12, M13), next to the generated traces.
 * Each is tied to its story's first matching trace (M18), and has an id of its own, so the
 * fixtures keep theirs. Only the large mock serves them.
 */

const PROJECT = "/api/v1/projects/:project";
const MINUTE_MS = 60_000;
const HOUR_MS = 60 * MINUTE_MS;
const DAY_MS = 24 * HOUR_MS;
/** Occurrences count over the 14 days before now, as the issue page asks for. */
const OCCURRENCE_WINDOW_MS = 14 * DAY_MS;
/** Example traces after the hero trace. */
const GENERATED_EXAMPLES = 4;
/** The support rule's threshold: the average correctness under it fires the rule. */
const CORRECTNESS_THRESHOLD = 0.7;

type Trace = Schema<"TraceSummary">;

/** A notification channel of a rule, as the delivery names it. */
type StoryChannel = { id: string; name: string; kind: Schema<"ChannelKind"> };

const TEAM_SLACK: StoryChannel = {
  id: "01900000-0000-7000-8000-000000000500",
  name: "Team Slack",
  kind: "slack",
};
/** The voice desk's webhook channel, named "On-call". */
export const ON_CALL_PAGER: StoryChannel = {
  id: "01900000-0000-7000-8000-000000000503",
  name: "On-call",
  kind: "webhook",
};

/** Whether a visible trace has the tag. */
const hasTag = (tag: string) => (trace: Trace) => trace.tags.includes(tag);

/** The index of the first visible trace of a dataset that matches, or -1 when none does. */
function firstMatch(matches: (trace: Trace) => boolean): (dataset: Dataset) => number {
  return (dataset) =>
    dataset.traces.findIndex(
      (trace) => matches(trace) && visibleTraces([trace], false, false).length === 1,
    );
}

/** The index of the newest visible trace of a dataset that matches, or -1 when none does (ties as `newestFirst`). */
function newestMatch(matches: (trace: Trace) => boolean): (dataset: Dataset) => number {
  return (dataset) => {
    let best: number | undefined;
    dataset.traces.forEach((trace, index) => {
      if (!matches(trace) || visibleTraces([trace], false, false).length !== 1) {
        return;
      }
      const current = best === undefined ? undefined : dataset.traces[best];
      if (current === undefined || newestFirst(trace, current) < 0) {
        best = index;
      }
    });
    return best ?? -1;
  };
}

/** The span of the trace at `index` that is an error and has the name, when the trace has one. */
function errorSpan(name: string): (dataset: Dataset, index: number) => string | undefined {
  return (dataset, index) => {
    const trace = dataset.traces[index];
    if (trace === undefined) {
      return undefined;
    }
    return dataset
      .traceDetail(trace.trace_id)
      ?.spans.find((row) => row.span.name === name && row.span.status === "error")?.span.span_id;
  };
}

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
  /**
   * The story's first matching trace: its dataset index, and the span the issue is about. The span
   * is undefined when the issue is about the whole trace.
   */
  hero: {
    index: (dataset: Dataset) => number;
    spanId?: (dataset: Dataset, index: number) => string | undefined;
  };
  /** The generated traces that are occurrences too. */
  matches: (trace: Trace) => boolean;
  /**
   * The reasons the issue's examples show, one per example by position: the hero and the generated
   * traces, newest first. The list cycles, and an issue has at most
   * five examples, so its five reasons are all different. `"input_preview"`: each example's reason
   * is its own trace's question, so the examples show the questions the traces were asked.
   */
  reasons: readonly string[] | "input_preview";
};

const isSsoQuestion = (trace: Trace) =>
  SSO_QUESTIONS.some((question) => trace.input_preview === question);

/** The reason of the example of `trace` at `position`: the issue's list entry, or the trace's question. */
function reasonOf(issue: StoryIssue, trace: Trace, position: number): string | undefined {
  if (issue.reasons === "input_preview") {
    return trace.input_preview;
  }
  const { reasons } = issue;
  return reasons.length === 0 ? undefined : reasons[position % reasons.length];
}

/** The scanned-PDF issue, which the extraction rule counts (M19). */
const SCANNED_ISSUE_ID = "01900000-0000-7000-8000-000000570104";

export const STORY_ISSUES: readonly StoryIssue[] = [
  {
    id: "01900000-0000-7000-8000-000000570101",
    story: "support",
    eventId: "01900000-0000-7000-8000-000000570111",
    sourceId: "01900000-0000-7000-8000-000000570301",
    sourceName: "Topics",
    kind: "topic",
    title: "Customers asking for SSO on the Starter plan",
    description: "Customers ask whether SSO comes with the Starter plan.",
    hero: { index: newestMatch(isSsoQuestion) },
    matches: isSsoQuestion,
    reasons: "input_preview",
  },
  {
    id: "01900000-0000-7000-8000-000000570102",
    story: "coding",
    eventId: "01900000-0000-7000-8000-000000570112",
    sourceId: "01900000-0000-7000-8000-000000570302",
    sourceName: "Errors",
    kind: "error",
    title: "ContextWindowExceeded: diff over 23 files",
    description: "Runs stop when the diff no longer fits the model's context.",
    hero: {
      index: firstMatch(hasTag("context-window")),
      spanId: errorSpan("write patch"),
    },
    matches: hasTag("context-window"),
    reasons: [
      "Diff over 23 files: 212k tokens, over the 200k context",
      "Diff over 31 files: 240k tokens, over the 200k context",
      "Diff over 18 files: 205k tokens, over the 200k context",
      "Diff over 27 files: 226k tokens, over the 200k context",
      "Diff over 44 files: 263k tokens, over the 200k context",
    ],
  },
  {
    id: "01900000-0000-7000-8000-000000570103",
    story: "voice",
    eventId: "01900000-0000-7000-8000-000000570113",
    sourceId: "01900000-0000-7000-8000-000000570303",
    sourceName: "Bad scores",
    kind: "score",
    title: "Agent talked over the caller",
    description: "The agent starts speaking before the caller has finished.",
    hero: { index: firstMatch(hasTag("interrupted")) },
    matches: hasTag("interrupted"),
    reasons: [
      "interruption 0.3: the agent spoke 0.4 s before the caller finished",
      "interruption 0.2: the agent spoke 0.9 s before the caller finished",
      "interruption 0.4: the agent spoke 0.2 s before the caller finished",
      "interruption 0.1: the agent spoke 1.3 s before the caller finished",
      "interruption 0.2: the agent spoke 0.6 s before the caller finished",
    ],
  },
  {
    id: SCANNED_ISSUE_ID,
    story: "extraction",
    eventId: "01900000-0000-7000-8000-000000570114",
    sourceId: "01900000-0000-7000-8000-000000570304",
    sourceName: "Errors",
    kind: "error",
    title: "Scanned PDF: OCR returned no text",
    description: "Scanned invoices give OCR nothing to read, so no fields are extracted.",
    hero: {
      index: firstMatch(hasTag("scanned")),
      spanId: errorSpan("ocr"),
    },
    matches: hasTag("scanned"),
    reasons: [
      "invoice-2026-0611.pdf: scanned image, no text layer",
      "invoice-2026-0604.pdf: scanned image, no text layer",
      "invoice-2026-0617.pdf: scanned image, no text layer",
      "invoice-2026-0608.pdf: scanned image, no text layer",
      "invoice-2026-0623.pdf: scanned image, no text layer",
    ],
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
  /** The channel the rule notifies. */
  channel: StoryChannel;
  state: "firing" | "ok";
  /**
   * The rule's value, its count and the time it last changed, before `now` of the dataset. The value
   * is undefined when the window has no sample to take it from.
   */
  lastValue: (dataset: Dataset, now: number) => number | undefined;
  lastCount: (dataset: Dataset, now: number) => number;
  /** When it last fired (firing), or resolved (ok), before now. */
  changedAgoMs: (dataset: Dataset, now: number) => number;
};

/** The visible traces that start in `[from, to)`. */
export function startedBetween(dataset: Dataset, from: number, to: number): Trace[] {
  return visibleTraces(dataset.traces, false, false).filter((trace) => {
    const started = Date.parse(trace.start_time);
    return started >= from && started < to;
  });
}

/** The value of the score `name` of each trace that carries it, of the data type `dataType`. */
export function scoreValues<T extends "numeric" | "boolean">(
  traces: readonly Trace[],
  name: string,
  dataType: T,
): (T extends "numeric" ? number : boolean)[] {
  return traces.flatMap((trace) => {
    const value = trace.scores.find((score) => score.name === name)?.value;
    return value?.data_type === dataType
      ? [value.value as T extends "numeric" ? number : boolean]
      : [];
  });
}

/** The mean of the values, or undefined for an empty window (no sample to take a mean of). */
function mean(values: readonly number[]): number | undefined {
  return values.length === 0
    ? undefined
    : values.reduce((total, value) => total + value, 0) / values.length;
}

/** The cost of the traces (USD). */
function costOf(traces: readonly Trace[]): number {
  return traces.reduce((total, trace) => total + (trace.cost_total ?? 0), 0);
}

/**
 * How long before `now` the first UTC hour of the last 24 began whose mean correctness is under
 * the threshold: the hourly buckets of the alert preview, oldest first. A dataset without such an
 * hour (another story's, in the unscoped tests) reports the whole day, so the handler never throws.
 */
function firstDippedHourAgo(dataset: Dataset, now: number): number {
  const first = Math.floor((now - DAY_MS) / HOUR_MS) * HOUR_MS;
  for (let start = first; start < now; start += HOUR_MS) {
    const average = mean(
      scoreValues(startedBetween(dataset, start, start + HOUR_MS), "correctness", "numeric"),
    );
    if (average !== undefined && average < CORRECTNESS_THRESHOLD) {
      return now - start;
    }
  }
  return DAY_MS;
}

/** The cost step of the coding project: the window of its cost rule starts this long before the step. */
const COST_STEP_MS = STEP_HOURS * HOUR_MS;

const CORRECTNESS_CONDITION = {
  metric: "score",
  window: "1h",
  comparator: "lt",
  threshold: CORRECTNESS_THRESHOLD,
  min_count: 10,
  score_name: "correctness",
  aggregation: "avg",
} as const;

/** The start times (ms) of the issue's occurrences, as the issue page counts them (occurrenceTraces). */
function startsOf(dataset: Dataset, issue: StoryIssue): number[] {
  return occurrenceTraces(dataset, issue).matched.map((trace) => Date.parse(trace.start_time));
}

/** The start times (ms) of the scanned-PDF issue's occurrences. */
function scannedStarts(dataset: Dataset): number[] {
  const issue = STORY_ISSUES.find((item) => item.id === SCANNED_ISSUE_ID);
  return issue === undefined ? [] : startsOf(dataset, issue);
}

/** The scanned PDFs that start in the hour before `now`: the extraction rule's value and count. */
function scannedLastHour(dataset: Dataset, now: number): number[] {
  return scannedStarts(dataset).filter((started) => started >= now - HOUR_MS && started < now);
}

/** The windows the issue preview takes: the API's windows of 15 minutes and up. */
type PreviewWindow = Exclude<Schema<"AlertWindow">, "1m" | "5m">;

/** The span the preview covers before `to`, as the API's description gives it. */
const PREVIEW_SPAN_MS: Record<PreviewWindow, number> = {
  "15m": DAY_MS,
  "1h": DAY_MS,
  "6h": 3 * DAY_MS,
  "24h": 14 * DAY_MS,
};

function previewWindowOf(value: string | null): PreviewWindow | undefined {
  return value === "15m" || value === "1h" || value === "6h" || value === "24h" ? value : undefined;
}

/**
 * The issue preview of the issues: one point per window-wide bucket from the bucket holding `to`
 * minus the span, up to `to` (the dataset's now). A point counts the visible traces of the issues
 * that start in its bucket (the last point only up to `to`), oldest first.
 */
function issuePreview(
  dataset: Dataset,
  issues: readonly StoryIssue[],
  bucket: PreviewWindow,
): Schema<"IssueAlertPreview"> {
  const width = WINDOW_MS[bucket];
  const to = dataset.now;
  const from = Math.floor((to - PREVIEW_SPAN_MS[bucket]) / width) * width;
  const starts = issues.flatMap((issue) => startsOf(dataset, issue));
  const points: Schema<"IssueAlertPoint">[] = [];
  for (let start = from; start < to; start += width) {
    const end = Math.min(start + width, to);
    points.push({
      time: rfc3339(start),
      count: starts.filter((started) => started >= start && started < end).length,
    });
  }
  return { from: rfc3339(from), to: rfc3339(to), window: bucket, points };
}

export const STORY_RULES: readonly StoryRule[] = [
  {
    id: "01900000-0000-7000-8000-000000570201",
    story: "support",
    eventId: "01900000-0000-7000-8000-000000570211",
    deliveryId: "01900000-0000-7000-8000-000000570221",
    name: "Answer correctness under 0.7",
    condition: CORRECTNESS_CONDITION,
    channel: TEAM_SLACK,
    state: "firing",
    lastValue: (dataset, now) =>
      mean(scoreValues(startedBetween(dataset, now - HOUR_MS, now), "correctness", "numeric")),
    lastCount: (dataset, now) => startedBetween(dataset, now - HOUR_MS, now).length,
    changedAgoMs: (dataset, now) => firstDippedHourAgo(dataset, now),
  },
  {
    id: "01900000-0000-7000-8000-000000570202",
    story: "coding",
    eventId: "01900000-0000-7000-8000-000000570212",
    deliveryId: "01900000-0000-7000-8000-000000570222",
    name: "Spend over $40 an hour",
    condition: { metric: "cost", window: "1h", comparator: "gt", threshold: 40 },
    channel: TEAM_SLACK,
    state: "firing",
    lastValue: (dataset, now) => costOf(startedBetween(dataset, now - HOUR_MS, now)),
    lastCount: (dataset, now) => startedBetween(dataset, now - HOUR_MS, now).length,
    changedAgoMs: () => COST_STEP_MS,
  },
  {
    id: "01900000-0000-7000-8000-000000570203",
    story: "voice",
    eventId: "01900000-0000-7000-8000-000000570213",
    deliveryId: "01900000-0000-7000-8000-000000570223",
    name: "Slow replies on the voice desk",
    condition: {
      metric: "latency_p95",
      target: "trace",
      window: "1h",
      comparator: "gt",
      threshold: 1_500,
      min_count: 20,
    },
    channel: ON_CALL_PAGER,
    state: "firing",
    lastValue: (dataset, now) =>
      percentile(
        startedBetween(dataset, now - HOUR_MS, now).map((trace) => trace.duration_ns / 1_000_000),
        95,
      ),
    lastCount: (dataset, now) => startedBetween(dataset, now - HOUR_MS, now).length,
    // Firing for two days: longer than the chart's 24 hours.
    changedAgoMs: () => 2 * DAY_MS,
  },
  {
    id: "01900000-0000-7000-8000-000000570204",
    story: "extraction",
    eventId: "01900000-0000-7000-8000-000000570214",
    deliveryId: "01900000-0000-7000-8000-000000570224",
    name: "Scanned PDFs over 10 an hour",
    condition: {
      metric: "issue_occurrences",
      issue_id: SCANNED_ISSUE_ID,
      window: "1h",
      comparator: "gt",
      threshold: 10,
    },
    channel: TEAM_SLACK,
    state: "firing",
    lastValue: (dataset, now) => scannedLastHour(dataset, now).length,
    lastCount: (dataset, now) => scannedLastHour(dataset, now).length,
    changedAgoMs: (dataset, now) => {
      // The hour went over 10 at its 11th scanned PDF, in start order.
      const eleventh = scannedLastHour(dataset, now).sort((left, right) => left - right)[10];
      return eleventh === undefined ? DAY_MS : now - eleventh;
    },
  },
];

const MEMBER_ID = "01900000-0000-7000-8000-000000000100";
/** The generated definitions the queues reuse (`SCORE_DEFINITIONS` in handlers.ts): correctness and tone for Support answers and Call review. */
const CORRECTNESS_ID = "01900000-0000-7000-8000-000000000611";
const TONE_ID = "01900000-0000-7000-8000-000000000615";
const POLICY_ID = "01900000-0000-7000-8000-000000570607";
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
  /** Dataset index of the hero trace, the queue's one item (a trace queue). */
  heroIndex?: number;
  /** The session of the queue's one item (a session queue; the item has no trace). */
  sessionId?: string;
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
  {
    id: POLICY_ID,
    name: "policy",
    data_type: "boolean",
    categories: [],
    description: "Whether the answer follows the support policy.",
    direction: "higher",
    created_at: DEFINED_AT,
    updated_at: DEFINED_AT,
  },
  {
    id: "01900000-0000-7000-8000-000000570608",
    name: "total",
    data_type: "boolean",
    categories: [],
    description: "Whether the extracted total matches the invoice.",
    direction: "higher",
    created_at: DEFINED_AT,
    updated_at: DEFINED_AT,
  },
  {
    id: "01900000-0000-7000-8000-000000570609",
    name: "vat_number",
    data_type: "boolean",
    categories: [],
    description: "Whether the extracted VAT number matches the invoice.",
    direction: "higher",
    created_at: DEFINED_AT,
    updated_at: DEFINED_AT,
  },
  {
    id: "01900000-0000-7000-8000-000000570610",
    name: "vendor",
    data_type: "boolean",
    categories: [],
    description: "Whether the extracted vendor matches the invoice.",
    direction: "higher",
    created_at: DEFINED_AT,
    updated_at: DEFINED_AT,
  },
  {
    id: "01900000-0000-7000-8000-000000570611",
    name: "date",
    data_type: "boolean",
    categories: [],
    description: "Whether the extracted date matches the invoice.",
    direction: "higher",
    created_at: DEFINED_AT,
    updated_at: DEFINED_AT,
  },
];

export const STORY_QUEUES: readonly StoryQueue[] = [
  {
    id: "01900000-0000-7000-8000-000000570501",
    story: "support",
    itemId: "01900000-0000-7000-8000-000000570511",
    name: "Answers with a thumbs-down",
    description: "Support answers that people review after the judges.",
    instructions:
      "Read the whole conversation. Grade the answer that got the thumbs-down: is it correct, is the tone right, does it follow the policy?",
    definitionIds: [CORRECTNESS_ID, TONE_ID, POLICY_ID],
    counts: { pending: 21, completed: 3, skipped: 1 },
    sessionId: SUPPORT_SESSION,
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
  const hero: Trace | undefined = dataset.traces[issue.hero.index(dataset)];
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
function occurrencesOf(
  dataset: Dataset,
  issue: StoryIssue,
  traces: Occurrences,
): Schema<"IssueOccurrences"> {
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

  // The hero and the newest generated occurrences, newest first like the page lists them; the hero keeps
  // its span, and the reasons go by position in that order.
  const shown = [...(hero ? [hero] : []), ...generated.slice(0, GENERATED_EXAMPLES)].sort(
    newestFirst,
  );
  const heroSpanId = hero ? issue.hero.spanId?.(dataset, issue.hero.index(dataset)) : undefined;
  const examples: Schema<"IssueExample">[] = shown.map((trace, position) => {
    const reason = reasonOf(issue, trace, position);
    const spanId = trace.trace_id === hero?.trace_id ? heroSpanId : undefined;
    return {
      trace_id: trace.trace_id,
      created_at: trace.start_time,
      issue_id: issue.id,
      ...(reason === undefined ? {} : { reason }),
      ...(spanId === undefined ? {} : { span_id: spanId }),
    };
  });
  return { days, examples };
}

export function storyOccurrences(dataset: Dataset, issue: StoryIssue): Schema<"IssueOccurrences"> {
  return occurrencesOf(dataset, issue, occurrenceTraces(dataset, issue));
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
  const value = rule.lastValue(dataset, dataset.now);
  return {
    id: rule.id,
    name: rule.name,
    condition: rule.condition,
    state: rule.state,
    enabled: true,
    channel_ids: [rule.channel.id],
    cooldown_s: 3600,
    notify_while_firing: false,
    ...(value === undefined ? {} : { last_value: value }),
    last_count: rule.lastCount(dataset, dataset.now),
    last_evaluated_at: ago(dataset, MINUTE_MS),
    ...(firing ? { last_fired_at: ago(dataset, rule.changedAgoMs(dataset, dataset.now)) } : {}),
    created_at: ago(dataset, 7 * DAY_MS),
    updated_at: ago(dataset, 7 * DAY_MS),
  };
}

export function storyAlertEvent(dataset: Dataset, rule: StoryRule): Schema<"AlertEvent"> {
  const windowTo = ago(dataset, rule.changedAgoMs(dataset, dataset.now));
  const value = rule.lastValue(dataset, dataset.now);
  return {
    id: rule.eventId,
    rule_id: rule.id,
    kind: rule.state === "firing" ? "fired" : "resolved",
    ...(value === undefined ? {} : { value }),
    count: rule.lastCount(dataset, dataset.now),
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
        channel_id: rule.channel.id,
        channel_name: rule.channel.name,
        channel_kind: rule.channel.kind,
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
    target: queue.sessionId === undefined ? "trace" : "session",
    definition_ids: [...queue.definitionIds],
    version: 1,
    created_by: MEMBER_ID,
    created_at: ago(dataset, 7 * DAY_MS),
    updated_at: ago(dataset, HOUR_MS),
    counts: queue.counts,
  };
}

/**
 * The queue's one item, pending and leased to the mock user: the session of a session queue, or the
 * hero trace of a trace queue. Undefined when the dataset has no such session or trace.
 */
export function storyQueueItem(
  dataset: Dataset,
  queue: StoryQueue,
): Schema<"AnnotationQueueItem"> | undefined {
  const common = {
    id: queue.itemId,
    queue_id: queue.id,
    status: "pending" as const,
    leased_by_user_id: MEMBER_ID,
    lease_expires_at: rfc3339(dataset.now + LEASE_MS),
    added_by: "user" as const,
    added_by_id: MEMBER_ID,
    created_at: ago(dataset, 6 * HOUR_MS),
  };
  if (queue.sessionId !== undefined) {
    // The session's earliest visible turn is its target time.
    const first = oldestFirst(
      visibleTraces(dataset.traces, false, false).filter(
        (trace) => trace.session_id === queue.sessionId,
      ),
    )[0];
    return first === undefined
      ? undefined
      : {
          ...common,
          session_id: queue.sessionId,
          target_time: first.start_time,
        };
  }
  const hero: Trace | undefined =
    queue.heroIndex === undefined ? undefined : dataset.traces[queue.heroIndex];
  if (hero === undefined) {
    return undefined;
  }
  return { ...common, trace_id: hero.trace_id, target_time: hero.start_time };
}

/** A path parameter as text; msw types it as possibly repeated or missing. Shared by the story handlers. */
export function idParam(value: string | readonly string[] | undefined): string {
  return typeof value === "string" ? value : "";
}

/**
 * The story issues, rules and queues, before the fixtures. A story issue that matches no trace is
 * not served at all. A handler that returns undefined lets the request fall through to the
 * fixtures' handlers. With `project` (an API ref, `acme/<slug>`) the routes use its literal prefix
 * and serve only the objects of that project's story (none for a ref that is no story project);
 * without it, every object is served under `/api/v1/projects/:project`. The fixture issues and queues
 * of the lists belong to the support project: they are listed only without `project` or under
 * support. The fixture alert rules and events are listed only without `project`.
 */
export function storyHandlers(dataset: Dataset, project?: string): HttpHandler[] {
  const prefix =
    project === undefined ? PROJECT : `/api/v1/projects/${encodeURIComponent(project)}`;
  const scope = project === undefined ? undefined : projectOfRef(project)?.story;
  const served = (story: StoryId) => project === undefined || story === scope;
  const withFixtures = project === undefined || scope === "support";
  // The fixtures' alert rules and events belong to no story project: only the unscoped mock lists them.
  const withAlertFixtures = project === undefined;
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
          ...(withFixtures ? issuesFixture.data : []),
        ],
        next_cursor: null,
      }),
    ),
    http.get(`${prefix}/issues/:id`, ({ params }) => {
      const issue = issueById(idParam(params.id));
      const detail = issue ? storyIssue(dataset, issue) : undefined;
      return detail ? HttpResponse.json(detail) : undefined;
    }),
    http.get(`${prefix}/issues/:id/occurrences`, ({ params }) => {
      const issue = issueById(idParam(params.id));
      const traces = issue ? matchedTraces(dataset, issue) : undefined;
      return issue && traces ? HttpResponse.json(occurrencesOf(dataset, issue, traces)) : undefined;
    }),
    http.get(`${prefix}/issues/:id/events`, ({ params }) => {
      const issue = issueById(idParam(params.id));
      const events = issue ? storyIssueEvents(dataset, issue) : undefined;
      return events ? HttpResponse.json(events) : undefined;
    }),
    http.get(`${prefix}/alert-rules`, () =>
      HttpResponse.json({
        data: [
          ...rules.map((rule) => storyAlertRule(dataset, rule)),
          ...(withAlertFixtures ? alertRulesFixture.data : []),
        ],
        next_cursor: null,
      }),
    ),
    // Before `alert-rules/:id`, which would take `issue-preview` for a rule id.
    http.get(`${prefix}/alert-rules/issue-preview`, ({ request }) => {
      const search = new URL(request.url).searchParams;
      const bucket = previewWindowOf(search.get("window"));
      const issueId = search.get("issue_id");
      const sourceId = search.get("source_id");
      // Exactly one of the two ids, a window the preview takes, and an issue or source of the project.
      if (bucket === undefined || (issueId === null) === (sourceId === null)) {
        return undefined;
      }
      const counted =
        issueId === null
          ? issues.filter((issue) => issue.sourceId === sourceId)
          : issues.filter((issue) => issue.id === issueId);
      return counted.length === 0
        ? undefined
        : HttpResponse.json(issuePreview(dataset, counted, bucket));
    }),
    http.get(`${prefix}/alert-rules/:id`, ({ params }) => {
      const rule = ruleById(idParam(params.id));
      return rule ? HttpResponse.json(storyAlertRule(dataset, rule)) : undefined;
    }),
    http.get(`${prefix}/alert-rules/:id/events`, ({ params }) => {
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
        data: [...events, ...(withAlertFixtures ? projectAlertEventsFixture.data : [])],
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
          .filter(
            (queue) =>
              queue.heroIndex !== undefined &&
              dataset.traces[queue.heroIndex]?.trace_id === traceId,
          )
          .map((queue) => storyQueue(dataset, queue));
        return found.length === 0
          ? undefined
          : HttpResponse.json({ data: found, next_cursor: null });
      }
      return HttpResponse.json({
        data: [
          ...queues.map((queue) => storyQueue(dataset, queue)),
          ...(withFixtures ? queuesFixture.data : []),
        ],
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
