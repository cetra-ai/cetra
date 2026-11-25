import { expect, it } from "vitest";
import { createDataset, type Dataset } from "@/mocks/generated/dataset";
import {
  CODING_STORY_PROFILE,
  type CodingFacts,
  codingFacts,
} from "@/mocks/generated/story-coding";

const NOW = Date.parse("2026-11-19T12:00:00Z");
const LATER = Date.parse("2026-12-03T09:30:00Z");
const SEED = 1;
const TRACES = 2000;

function build(now: number): Dataset {
  return createDataset({ seed: SEED, now, traces: TRACES, profile: CODING_STORY_PROFILE });
}

const dataset = build(NOW);
const facts = codingFacts(dataset);

/** The ranges every fact must keep at any clock (the plan's tolerances). */
function expectWithinRanges(value: CodingFacts): void {
  expect(Math.abs(value.retryShare - 0.18)).toBeLessThanOrEqual(0.01);
  expect(value.retryCostShare).toBeGreaterThanOrEqual(0.6);
  expect(value.retryCostShare).toBeLessThanOrEqual(0.85);
  expect(value.costPerRunRatio).toBeGreaterThanOrEqual(2.6);
  expect(value.costPerRunRatio).toBeLessThanOrEqual(3.4);
  expect(value.hourlyCostSinceStep).toHaveLength(14);
  for (const hour of value.hourlyCostSinceStep) {
    expect(Math.abs(hour - 45)).toBeLessThanOrEqual(0.5);
  }
  expect(value.hourlyCostBefore).toHaveLength(10);
  for (const hour of value.hourlyCostBefore) {
    expect(hour).toBeLessThan(40);
  }
  expect(Math.abs(value.contextWindowShare - 0.09)).toBeLessThanOrEqual(0.01);
  expect(value.contextWindowBeforeMonday).toBe(0);
  expect(value.testsPassRateSince).toBeLessThan(value.testsPassRateBefore);
}

it("pins the share of the runs that retry since the step at 18%", () => {
  expect(facts.retryShare).toBeCloseTo(0.1785, 3);
});

it("pins the share of the cost of the retried runs since the step at about 70%", () => {
  expect(facts.retryCostShare).toBeCloseTo(0.6993, 3);
});

it("pins the cost per run since the step at 2.73 times the cost per run before it", () => {
  expect(facts.costPerRunRatio).toBeCloseTo(2.73, 2);
});

it("makes every whole hour since the step cost 45 USD", () => {
  expect(facts.hourlyCostSinceStep).toHaveLength(14);
  for (const hour of facts.hourlyCostSinceStep) {
    expect(Math.abs(hour - 45)).toBeLessThanOrEqual(0.5);
  }
});

it("keeps every whole hour in the 24 hours before the step under 40 USD", () => {
  expect(facts.hourlyCostBefore).toHaveLength(10);
  for (const hour of facts.hourlyCostBefore) {
    expect(hour).toBeLessThan(40);
  }
});

it("pins the share of the runs with a context-window error since Monday at 9%", () => {
  expect(facts.contextWindowShare).toBeCloseTo(0.0896, 3);
});

it("has no context-window error before Monday", () => {
  expect(facts.contextWindowBeforeMonday).toBe(0);
});

it("pins the tests_pass rate before the step and since it, the second lower", () => {
  // Context-window runs since Monday fail tests_pass too (M1), so the rate before the step is lower as well.
  expect(facts.testsPassRateBefore).toBeCloseTo(0.7983, 3);
  expect(facts.testsPassRateSince).toBeCloseTo(0.5787, 3);
  expect(facts.testsPassRateSince).toBeLessThan(facts.testsPassRateBefore);
});

it("keeps every fact within its range at the later clock", () => {
  expectWithinRanges(codingFacts(build(LATER)));
});

it("marks the api-retry runs' last run_tests spans with the packages/api command and the timeout", () => {
  const trace = dataset.traces.find((item) => item.tags.includes("api-retry"));
  if (!trace) {
    throw new Error("expected an api-retry run");
  }
  const index = dataset.traces.indexOf(trace);
  const marked = 6 + (index % 4);
  const firstMarked = trace.span_count - marked;
  const spans = (dataset.traceDetail(trace.trace_id)?.spans ?? [])
    .map((row) => row.span)
    .filter((span) => Number.parseInt(span.span_id.slice(8), 16) - 1 >= firstMarked);
  expect(spans).toHaveLength(marked);
  spans.forEach((span, k) => {
    expect(span.name).toBe("run_tests");
    const detail = dataset.spanDetail(trace.trace_id, span.span_id);
    expect(detail?.input).toEqual({
      format: "json",
      value: { path: "packages/api", command: "pytest packages/api" },
    });
    const last = k === marked - 1;
    if (last && index % 5 === 0) {
      expect(detail?.output).toEqual({ format: "text", value: "48 passed" });
      expect(span.status).toBe("ok");
    } else {
      expect(detail?.output).toEqual({ format: "text", value: "timed out after 30 s" });
      expect(span.status).toBe("error");
      expect(span.status_message).toBe("timed out after 30 s");
    }
  });
});

it("fails tests_pass on the context-window runs that carry it, as their error does (M1)", () => {
  const runs = dataset.traces.filter(
    (trace) => trace.tags.includes("context-window") && trace.error_count > 0,
  );
  const withScore = runs.filter((trace) =>
    trace.scores.some((score) => score.name === "tests_pass"),
  );
  expect(withScore.length).toBeGreaterThan(0);
  for (const trace of withScore) {
    expect(trace.scores.find((score) => score.name === "tests_pass")?.value).toEqual({
      data_type: "boolean",
      value: false,
    });
  }
});

it("keeps the facts' ranges on the dataset it returns", () => {
  expectWithinRanges(facts);
});
