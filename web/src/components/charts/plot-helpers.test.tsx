import { describe, expect, it } from "vitest";
import type { ChartRow } from "@/lib/metrics";
import {
  axisStyle,
  formatTick,
  roundsTop,
  tickBudget,
  topRoundedBarPath,
  xAxisTicks,
} from "./plot-helpers";

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

describe("topRoundedBarPath", () => {
  it("rounds a wide bar by 2 px", () => {
    expect(topRoundedBarPath(10, 20, 30, 40)).toBe(
      "M10,60 L10,22 Q10,20 12,20 L38,20 Q40,20 40,22 L40,60 Z",
    );
  });

  it("clamps the radius to half a thin bar's width", () => {
    expect(topRoundedBarPath(0, 0, 2, 40)).toBe("M0,40 L0,1 Q0,0 1,0 L1,0 Q2,0 2,1 L2,40 Z");
  });

  it("clamps the radius to a short bar's height", () => {
    expect(topRoundedBarPath(0, 9, 10, 1)).toBe("M0,10 L0,10 Q0,9 1,9 L9,9 Q10,9 10,10 L10,10 Z");
  });
});

describe("roundsTop", () => {
  it("rounds a lone bar with a positive value", () => {
    expect(roundsTop({ v: 3 }, "v")).toBe(true);
  });

  it("leaves zero, negative and missing values square", () => {
    expect(roundsTop({ v: 0 }, "v")).toBe(false);
    expect(roundsTop({ v: -2 }, "v")).toBe(false);
    expect(roundsTop({}, "v")).toBe(false);
    expect(roundsTop({ v: null }, "v")).toBe(false);
  });

  it("rounds only the top segment of a stack", () => {
    const stack = ["ok", "errors"];
    const row = { ok: 5, errors: 2 };
    expect(roundsTop(row, "ok", stack)).toBe(false);
    expect(roundsTop(row, "errors", stack)).toBe(true);
  });

  it("falls to the segment below when the top one is empty", () => {
    const stack = ["ok", "errors"];
    expect(roundsTop({ ok: 5, errors: 0 }, "ok", stack)).toBe(true);
    expect(roundsTop({ ok: 5, errors: 0 }, "errors", stack)).toBe(false);
    expect(roundsTop({ ok: 5 }, "ok", stack)).toBe(true);
  });

  it("rounds the top of three", () => {
    const stack = ["a", "b", "c"];
    const row = { a: 1, b: 2, c: 0 };
    expect(roundsTop(row, "a", stack)).toBe(false);
    expect(roundsTop(row, "b", stack)).toBe(true);
    expect(roundsTop(row, "c", stack)).toBe(false);
  });
});

describe("axisStyle and formatTick", () => {
  it("labels a range under a day with hours and minutes, and a longer one with the date", () => {
    const day = rowsEvery(60, 24);
    expect(axisStyle(day)).toBe("time");
    expect(axisStyle(rowsEvery(60, 25))).toBe("datetime");
    expect(axisStyle([])).toBe("time");
    const at = Date.UTC(2026, 10, 19, 11, 51, 23, 72);
    expect(formatTick(at, "utc", "time")).toBe("11:51");
    expect(formatTick(at, "utc", "datetime")).toBe("11-19 11:51");
    expect(formatTick(String(at), "utc", "time")).toBe("11:51");
    expect(formatTick("not a time", "utc", "time")).toBe("");
  });
});
