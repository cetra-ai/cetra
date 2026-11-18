import { describe, expect, it } from "vitest";
import type { ChartRow } from "@/lib/metrics";
import { tickBudget, xAxisTicks } from "./plot-helpers";

const MINUTE_MS = 60_000;
const START = Date.UTC(2026, 0, 1, 0, 0);

/** Rows every `stepMinutes` for `count` buckets, from midnight UTC of 2026-01-01. */
function rowsEvery(stepMinutes: number, count: number): ChartRow[] {
  return Array.from({ length: count }, (_, index) => ({
    time: START + index * stepMinutes * MINUTE_MS,
  }));
}

describe("tickBudget", () => {
  it("allows one X-axis label per 90 px of plot width, and at least one", () => {
    expect(tickBudget(640)).toBe(7);
    expect(tickBudget(180)).toBe(2);
    expect(tickBudget(105)).toBe(1);
    expect(tickBudget(90)).toBe(1);
    expect(tickBudget(10)).toBe(1);
    expect(tickBudget(0)).toBe(1);
  });
});

describe("xAxisTicks", () => {
  it("returns at most the budget of ticks when the round times are more", () => {
    // A day of ten-minute buckets: round times every hour are 24, far more than a budget of 3.
    const rows = rowsEvery(10, 144);
    const ticks = xAxisTicks(rows, true, 3);
    expect(ticks.length).toBeGreaterThan(0);
    expect(ticks.length).toBeLessThanOrEqual(3);
    for (const tick of ticks) {
      expect(rows.some((row) => row.time === tick)).toBe(true);
    }
  });

  it("returns at most the budget of ticks when no round time falls on a row", () => {
    // Buckets of 7 minutes start at 0, 7, 14 ...: no row is a round hour, so every k-th row is taken.
    const rows = rowsEvery(7, 100);
    for (const budget of [2, 3, 5]) {
      expect(xAxisTicks(rows, true, budget).length).toBeLessThanOrEqual(budget);
    }
  });

  it("returns the one row nearest the middle when the budget is one", () => {
    // 144 rows: the middle is between rows 71 and 72, and the round-up one is row 72.
    const rows = rowsEvery(10, 144);
    expect(xAxisTicks(rows, true, 1)).toEqual([rows[72]?.time]);
    // Five rows: the middle row is row 2.
    const five = rowsEvery(10, 5);
    expect(xAxisTicks(five, true, 1)).toEqual([five[2]?.time]);
    // A single row is its own middle.
    const one = rowsEvery(10, 1);
    expect(xAxisTicks(one, true, 1)).toEqual([one[0]?.time]);
  });

  it("returns no ticks for no rows", () => {
    expect(xAxisTicks([], true, 4)).toEqual([]);
    expect(xAxisTicks([], true, 1)).toEqual([]);
  });
});
