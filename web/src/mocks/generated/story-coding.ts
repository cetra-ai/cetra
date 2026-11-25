import type { Schema } from "@/api/types";
import {
  type Dataset,
  evenSpread,
  isEligibleTrace,
  type Mark,
  type MarkedSpan,
  type MarksContext,
  mergeMarks,
  type Profile,
} from "@/mocks/generated/dataset";
import { CODING_PROFILE } from "@/mocks/generated/profiles";

type TraceSummary = Schema<"TraceSummary">;

const HOUR_MS = 3_600_000;
/** The cost step: the window starts `now − 14 h` (Y). */
export const STEP_HOURS = 14;
/** Monday: the context-window window starts `now − 84 h` (M). */
const MONDAY_HOURS = 84;
const RETRY_SHARE = 0.18;
const CONTEXT_WINDOW_SHARE = 0.09;
/** Every whole hour since the step costs this much (USD). */
const HOUR_COST_USD = 45;

/** The eligible runs starting in `[from, to)`, with their index and start (ms). */
type Run = { index: number; trace: TraceSummary; startMs: number };

/** The eligible runs that start in `[from, to)`, in index order. */
function runsIn(
  traces: readonly TraceSummary[],
  eligible: (index: number) => boolean,
  from: number,
  to: number,
): Run[] {
  return traces.flatMap((trace, index) => {
    const startMs = Date.parse(trace.start_time);
    return eligible(index) && startMs >= from && startMs < to ? [{ index, trace, startMs }] : [];
  });
}

/** A generated trace's cost; the generator always sets it, the `?? 0` only narrows the optional field. */
function costOf(trace: TraceSummary): number {
  return trace.cost_total ?? 0;
}

/** The marked spans of a `run_tests` run: a timeout each, and the last one passes on every fifth run. */
function retrySpans(index: number): MarkedSpan[] {
  const count = 6 + (index % 4);
  return Array.from({ length: count }, (_, k): MarkedSpan => {
    const last = k === count - 1;
    const passes = last && index % 5 === 0;
    const base: MarkedSpan = {
      name: "run_tests",
      kind: "tool",
      fromMs: 2_000 + k * 31_000,
      durationMs: 30_000,
      input: { format: "json", value: { path: "packages/api", command: "pytest packages/api" } },
      output: passes
        ? { format: "text", value: "48 passed" }
        : { format: "text", value: "timed out after 30 s" },
    };
    if (passes) {
      return base;
    }
    return {
      ...base,
      error: {
        message: "timed out after 30 s",
        stacktrace: "TimeoutError: pytest packages/api timed out after 30 s",
      },
    };
  });
}

/**
 * The coding project's marks (M17): API retries since Y, each hour costing 45 USD; context-window errors since M.
 * A run picked by both gets both marks merged.
 */
function codingMarks(
  traces: readonly TraceSummary[],
  context: MarksContext,
): ReadonlyMap<number, Mark> {
  const step = context.now - STEP_HOURS * HOUR_MS;
  const monday = context.now - MONDAY_HOURS * HOUR_MS;
  const marks = new Map<number, Mark>();
  const add = (index: number, change: Mark): void => {
    const previous = marks.get(index);
    marks.set(index, previous ? mergeMarks(previous, change) : change);
  };

  // Retries: an even spread over the runs since Y; the cost factor makes each whole hour cost 45 USD.
  const stepRuns = runsIn(traces, context.eligible, step, Number.POSITIVE_INFINITY);
  const retried = new Set(evenSpread(stepRuns, RETRY_SHARE).map((run) => run.index));
  const hourOf = (run: Run): number => Math.floor((run.startMs - step) / HOUR_MS);
  const unmarked = new Map<number, number>();
  const marked = new Map<number, number>();
  for (const run of stepRuns) {
    const totals = retried.has(run.index) ? marked : unmarked;
    totals.set(hourOf(run), (totals.get(hourOf(run)) ?? 0) + costOf(run.trace));
  }
  for (const run of stepRuns.filter((item) => retried.has(item.index))) {
    const hour = hourOf(run);
    const factor = Math.max(
      1,
      (HOUR_COST_USD - (unmarked.get(hour) ?? 0)) / (marked.get(hour) ?? 1),
    );
    add(run.index, {
      tags: ["api-retry"],
      scores: [
        { name: "tests_pass", source: "check", value: { data_type: "boolean", value: false } },
      ],
      costFactor: factor,
      spans: retrySpans(run.index),
    });
  }

  // Context window: an even spread over the runs since M.
  const mondayRuns = runsIn(traces, context.eligible, monday, Number.POSITIVE_INFINITY);
  for (const run of evenSpread(mondayRuns, CONTEXT_WINDOW_SHARE)) {
    add(run.index, {
      tags: ["context-window"],
      error: {
        count: 1,
        spanName: "write patch",
        message: "ContextWindowExceeded: diff over 23 files",
        stacktrace: "ContextWindowExceeded: the prompt is 212,431 tokens; the limit is 200,000",
      },
    });
  }
  return marks;
}

/** The coding agent's story profile (M17): the M16 profile with the retry and context-window marks. */
export const CODING_STORY_PROFILE: Profile = { ...CODING_PROFILE, marks: codingMarks };

export type CodingFacts = {
  /** Runs tagged `api-retry` over the runs since Y. */
  retryShare: number;
  /** Their share of the cost since Y. */
  retryCostShare: number;
  /** Mean cost per run since Y over the mean in `[Y − 24 h, Y)`. */
  costPerRunRatio: number;
  /** The cost of each whole hour since Y, USD. */
  hourlyCostSinceStep: readonly number[];
  /** The cost of each whole hour in `[now − 24 h, Y)`, USD. */
  hourlyCostBefore: readonly number[];
  /** Runs tagged `context-window` over the runs since M. */
  contextWindowShare: number;
  /** Runs tagged `context-window` that start before M. */
  contextWindowBeforeMonday: number;
  /** Mean over the whole hours in `[now − 24 h, Y)` of each hour's `tests_pass` rate. */
  testsPassRateBefore: number;
  /** The same over the whole hours in `[Y, now)`. */
  testsPassRateSince: number;
};

/** The facts of a dataset built from `CODING_STORY_PROFILE`: the eligible runs only (M17). */
export function codingFacts(dataset: Dataset): CodingFacts {
  // The index is the dataset's order, so the facts never read it from a trace id.
  const runs = dataset.traces.flatMap((trace, index): Run[] =>
    isEligibleTrace(CODING_STORY_PROFILE, trace, index)
      ? [{ index, trace, startMs: Date.parse(trace.start_time) }]
      : [],
  );
  const now = dataset.now;
  const step = now - STEP_HOURS * HOUR_MS;
  const before = now - 24 * HOUR_MS;
  const monday = now - MONDAY_HOURS * HOUR_MS;
  const inRange = (from: number, to: number): Run[] =>
    runs.filter((run) => run.startMs >= from && run.startMs < to);
  const sum = (list: readonly Run[]): number =>
    list.reduce((total, run) => total + costOf(run.trace), 0);
  const mean = (list: readonly Run[]): number => sum(list) / list.length;
  const tagged = (list: readonly Run[], tag: string): number =>
    list.filter((run) => run.trace.tags.includes(tag)).length;
  const hours = (from: number, count: number): Run[][] =>
    Array.from({ length: count }, (_, hour) =>
      inRange(from + hour * HOUR_MS, from + (hour + 1) * HOUR_MS),
    );
  const passRate = (windowHours: Run[][]): number => {
    const rates = windowHours.flatMap((hourRuns) => {
      const scored = hourRuns.flatMap((run) => {
        const score = run.trace.scores.find((item) => item.name === "tests_pass");
        return score?.value.data_type === "boolean" ? [score.value.value] : [];
      });
      return scored.length === 0 ? [] : [scored.filter(Boolean).length / scored.length];
    });
    return rates.reduce((total, rate) => total + rate, 0) / rates.length;
  };

  const sinceStep = inRange(step, now);
  const retried = sinceStep.filter((run) => run.trace.tags.includes("api-retry"));
  const sinceMonday = inRange(monday, now);
  const hourlyBefore = hours(before, Math.floor((step - before) / HOUR_MS));
  return {
    retryShare: retried.length / sinceStep.length,
    retryCostShare: sum(retried) / sum(sinceStep),
    costPerRunRatio: mean(sinceStep) / mean(inRange(step - 24 * HOUR_MS, step)),
    hourlyCostSinceStep: hours(step, Math.floor((now - step) / HOUR_MS)).map(sum),
    hourlyCostBefore: hourlyBefore.map(sum),
    contextWindowShare: tagged(sinceMonday, "context-window") / sinceMonday.length,
    contextWindowBeforeMonday: tagged(inRange(Number.NEGATIVE_INFINITY, monday), "context-window"),
    testsPassRateBefore: passRate(hourlyBefore),
    testsPassRateSince: passRate(hours(step, Math.floor((now - step) / HOUR_MS))),
  };
}
