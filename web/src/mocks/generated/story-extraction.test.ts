import { expect, it } from "vitest";
import { createDataset, isEligibleTrace, visibleTraces } from "@/mocks/generated/dataset";
import { EXTRACTION_PROFILE } from "@/mocks/generated/profiles";
import {
  EXTRACTION_STORY_PROFILE,
  type ExtractionFacts,
  extractionFacts,
} from "@/mocks/generated/story-extraction";

const NOW = Date.parse("2026-11-19T12:00:00Z");
const OTHER_NOW = Date.parse("2026-12-03T09:30:00Z");

/** The file names of the four comma vendors, by `index % 20` (0 to 3). */
const COMMA_FILES = [
  "invoice-2026-0521.pdf (2 pages)",
  "invoice-2026-0524.pdf (1 page)",
  "invoice-2026-0527.pdf (3 pages)",
  "invoice-2026-0530.pdf (1 page)",
] as const;

it("gives each eligible run the vendor its index selects: a comma vendor, or the M16 turn in turn order", () => {
  const dataset = createDataset({
    seed: 1,
    now: NOW,
    traces: 2_000,
    profile: EXTRACTION_STORY_PROFILE,
  });
  let checked = 0;
  dataset.traces.forEach((trace, index) => {
    if (!isEligibleTrace(EXTRACTION_STORY_PROFILE, trace, index)) {
      return;
    }
    const comma = COMMA_FILES[index % 20];
    const turns = EXTRACTION_PROFILE.turns;
    const expected = index % 20 < 4 ? comma : turns[index % turns.length]?.question;
    if (trace.tags.includes("batch-1119")) {
      // A batch run is an M16 vendor's invoice with its own number: same vendor, same page count.
      expect(index % 20).toBeGreaterThanOrEqual(4);
      expect(trace.input_preview?.replace(/invoice-2026-1[12]\d\d/, "")).toBe(
        expected?.replace(/invoice-2026-\d{4}/, ""),
      );
    } else {
      expect(trace.input_preview).toBe(expected);
    }
    checked += 1;
  });
  expect(checked).toBeGreaterThan(1_000);
});

it("gives every run of the batch its own invoice file", () => {
  const dataset = createDataset({
    seed: 1,
    now: NOW,
    traces: 2_000,
    profile: EXTRACTION_STORY_PROFILE,
  });
  const files = dataset.traces
    .filter((trace) => trace.tags.includes("batch-1119"))
    .map((trace) => trace.input_preview?.split(" ")[0]);
  expect(files).toHaveLength(200);
  expect(new Set(files).size).toBe(200);
});

function factsAt(now: number): ExtractionFacts {
  const dataset = createDataset({
    seed: 1,
    now,
    traces: 2_000,
    profile: EXTRACTION_STORY_PROFILE,
  });
  return extractionFacts(dataset);
}

const SAMPLES = [
  ["the screenshot clock", NOW],
  ["another clock", OTHER_NOW],
] as const;

it.each(SAMPLES)("Nordic Freight re-extracts 38%% of its runs at %s", (_, now) => {
  expect(Math.abs(factsAt(now).nordicFreightRetryShare - 0.38)).toBeLessThanOrEqual(0.02);
});

it.each(SAMPLES)("the other comma vendors re-extract 30%% of their runs at %s", (_, now) => {
  const { commaVendors } = factsAt(now);
  for (const share of Object.values(commaVendors)) {
    expect(Math.abs(share - 0.3)).toBeLessThanOrEqual(0.03);
  }
});

it.each(SAMPLES)("the other vendors re-extract at most 7%% of their runs at %s", (_, now) => {
  expect(factsAt(now).otherVendorsRetryShare).toBeLessThanOrEqual(0.07);
});

it.each(SAMPLES)("3%% of the runs in 30 days are scanned PDFs at %s", (_, now) => {
  expect(Math.abs(factsAt(now).scannedShare - 0.03)).toBeLessThanOrEqual(0.006);
});

it.each(SAMPLES)("at least 12 scanned PDFs came in the last hour at %s", (_, now) => {
  expect(factsAt(now).scannedLastHour).toBeGreaterThanOrEqual(12);
});

it.each(SAMPLES)(
  "no hour before the last one reaches the alert's 10 scanned PDFs at %s",
  (_, now) => {
    const { scannedHourlyBefore } = factsAt(now);
    expect(scannedHourlyBefore).toBeLessThan(10);
    expect(scannedHourlyBefore).toBe(6);
  },
);

it.each(SAMPLES)("the batch has 200 runs at %s", (_, now) => {
  expect(factsAt(now).batchSize).toBe(200);
});

it.each(SAMPLES)("three batch runs fail twice at %s", (_, now) => {
  expect(factsAt(now).batchFailedTwice).toBe(3);
});

it.each(SAMPLES)("the batch costs 0.4816 USD at %s", (_, now) => {
  expect(factsAt(now).batchCost).toBeCloseTo(0.4816, 3);
});

it("the batch tag selects the 200 batch runs, and the three failures have two errors", () => {
  const dataset = createDataset({
    seed: 1,
    now: NOW,
    traces: 2_000,
    profile: EXTRACTION_STORY_PROFILE,
  });
  const batch = visibleTraces(dataset.traces, false, false).filter((trace) =>
    trace.tags.includes("batch-1119"),
  );
  expect(batch).toHaveLength(200);
  expect(batch.filter((trace) => trace.error_count === 2)).toHaveLength(3);
});

it("shows the schema failure as the output of the batch's three failed runs, and of no other run", () => {
  const dataset = createDataset({
    seed: 1,
    now: NOW,
    traces: 2_000,
    profile: EXTRACTION_STORY_PROFILE,
  });
  const batch = visibleTraces(dataset.traces, false, false).filter((trace) =>
    trace.tags.includes("batch-1119"),
  );
  const failed = batch.filter((trace) => trace.error_count === 2);
  expect(failed).toHaveLength(3);
  for (const trace of failed) {
    expect(trace.output_preview).toBe(EXTRACTION_PROFILE.failedAnswer);
  }
  expect(EXTRACTION_PROFILE.failedAnswer).toBe(
    "Schema check failed twice: total does not match the line items.",
  );
  for (const trace of batch.filter((item) => item.error_count === 0)) {
    expect(trace.output_preview).not.toBe(EXTRACTION_PROFILE.failedAnswer);
  }
});

it("puts the batch's three failures at its newest three runs, so the newest-first list shows them first", () => {
  const dataset = createDataset({
    seed: 1,
    now: NOW,
    traces: 2_000,
    profile: EXTRACTION_STORY_PROFILE,
  });
  const batch = visibleTraces(dataset.traces, false, false).filter((trace) =>
    trace.tags.includes("batch-1119"),
  );
  const newest = [...batch]
    .sort((left, right) => Date.parse(right.start_time) - Date.parse(left.start_time))
    .slice(0, 3);
  expect(newest.map((trace) => trace.error_count)).toEqual([2, 2, 2]);
});

it("names all 200 batch runs process-invoice, in the summary and at the root of their detail", () => {
  const dataset = createDataset({
    seed: 1,
    now: NOW,
    traces: 2_000,
    profile: EXTRACTION_STORY_PROFILE,
  });
  const batch = visibleTraces(dataset.traces, false, false).filter((trace) =>
    trace.tags.includes("batch-1119"),
  );
  expect(batch).toHaveLength(200);
  for (const trace of batch) {
    expect(trace.name).toBe("process-invoice");
    const root = dataset.traceDetail(trace.trace_id)?.spans[0]?.span;
    expect(root?.name).toBe("process-invoice");
  }
  expect(dataset.traces.filter((trace) => trace.name === "process-invoice")).toHaveLength(200);
});
