import { expect, it } from "vitest";
import type { Schema } from "@/api/types";
import { distribution } from "./score-distribution";

type Score = Schema<"Score">;

/** A score with the given value; the other fields are the ones the distribution does not read. */
function scoreOf(value: Score["value"], deleted = false): Score {
  return {
    project_id: "01900000-0000-7000-8000-000000000001",
    score_id: "01900000-0000-7000-8000-000000000900",
    target: { type: "trace", trace_id: "a1000000000000000000000000000001" },
    name: "relevance",
    value,
    source: "llm_judge",
    created_at: "2026-01-01T10:00:00.000000000Z",
    updated_at: "2026-01-01T10:00:00.000000000Z",
    ingest_time: "2026-01-01T10:00:00.000000000Z",
    deleted,
  } as Score;
}

it("bins numeric scores into ten equal bins from the minimum to the maximum", () => {
  const result = distribution([
    scoreOf({ data_type: "numeric", value: 0 }),
    scoreOf({ data_type: "numeric", value: 0.05 }),
    scoreOf({ data_type: "numeric", value: 1 }),
  ]);

  expect(result.kind).toBe("numeric");
  expect(result.bins).toHaveLength(10);
  expect(result.bins[0]).toEqual({ label: "0.00–0.10", count: 2 });
  expect(result.bins[9]).toEqual({ label: "0.90–1.00", count: 1 });
  expect(result.bins.reduce((sum, bin) => sum + bin.count, 0)).toBe(3);
});

it("uses one bin labelled with the value when all numeric scores are equal", () => {
  const result = distribution([
    scoreOf({ data_type: "numeric", value: 0.5 }),
    scoreOf({ data_type: "numeric", value: 0.5 }),
  ]);

  expect(result).toEqual({ kind: "numeric", bins: [{ label: "0.50", count: 2 }] });
});

it("counts true and false for boolean scores", () => {
  const result = distribution([
    scoreOf({ data_type: "boolean", value: true }),
    scoreOf({ data_type: "boolean", value: true }),
    scoreOf({ data_type: "boolean", value: false }),
  ]);

  expect(result).toEqual({
    kind: "boolean",
    bins: [
      { label: "true", count: 2 },
      { label: "false", count: 1 },
    ],
  });
});

it("orders categorical bins by frequency, then by label", () => {
  const label = (name: string) => scoreOf({ data_type: "categorical", value: { label: name } });
  const result = distribution([label("b"), label("c"), label("a"), label("c"), label("b")]);

  expect(result).toEqual({
    kind: "categorical",
    bins: [
      { label: "b", count: 2 },
      { label: "c", count: 2 },
      { label: "a", count: 1 },
    ],
  });
});

it("has no bins for text scores", () => {
  expect(distribution([scoreOf({ data_type: "text", value: "fine" })])).toEqual({
    kind: "text",
    bins: [],
  });
});

it("is empty without scores, and ignores deleted ones", () => {
  expect(distribution([])).toEqual({ kind: "empty", bins: [] });
  expect(distribution([scoreOf({ data_type: "boolean", value: true }, true)])).toEqual({
    kind: "empty",
    bins: [],
  });

  const result = distribution([
    scoreOf({ data_type: "numeric", value: 0.2 }),
    scoreOf({ data_type: "numeric", value: 0.9 }, true),
  ]);
  expect(result).toEqual({ kind: "numeric", bins: [{ label: "0.20", count: 1 }] });
});
