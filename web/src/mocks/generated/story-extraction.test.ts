import { expect, it } from "vitest";
import { createDataset, visibleTraces } from "@/mocks/generated/dataset";
import {
  EXTRACTION_STORY_PROFILE,
  type ExtractionFacts,
  extractionFacts,
} from "@/mocks/generated/story-extraction";

const NOW = Date.parse("2026-11-19T12:00:00Z");
const OTHER_NOW = Date.parse("2026-12-03T09:30:00Z");

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

it.each(SAMPLES)("the batch costs 0.4776 USD at %s", (_, now) => {
  expect(factsAt(now).batchCost).toBeCloseTo(0.4776, 3);
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
