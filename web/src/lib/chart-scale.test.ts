import { describe, expect, it } from "vitest";
import { chooseBucket, durationTicks, niceTicks, timeTicks } from "@/lib/chart-scale";

const MINUTE = 60_000;
const DAY = 86_400_000;
const SECOND_NS = 1_000_000_000;

describe("niceTicks", () => {
  it("picks the smallest 1-2-5 step that fits the count", () => {
    expect(niceTicks(0.16)).toEqual([0, 0.05, 0.1, 0.15, 0.2]);
    expect(niceTicks(22)).toEqual([0, 10, 20, 30]);
    expect(niceTicks(1)).toEqual([0, 0.5, 1]);
    expect(niceTicks(7)).toEqual([0, 2, 4, 6, 8]);
  });

  it("rounds away float noise in the ticks", () => {
    expect(niceTicks(0.30000000000000004, 3)).toEqual([0, 0.1, 0.2, 0.3]);
  });

  it("gives [0, 1] for a zero, negative or non-finite maximum", () => {
    for (const max of [0, -3, Number.NaN, Number.POSITIVE_INFINITY]) {
      expect(niceTicks(max)).toEqual([0, 1]);
    }
  });

  it("handles tiny and sub-normal maxima without hanging", () => {
    const ticks = niceTicks(1e-7);
    expect(ticks[0]).toBe(0);
    expect(ticks.at(-1)).toBeGreaterThanOrEqual(1e-7);
    expect(ticks.length).toBeLessThanOrEqual(5);
    const tiny = niceTicks(Number.MIN_VALUE);
    expect(tiny[0]).toBe(0);
    expect(tiny.at(-1)).toBeGreaterThanOrEqual(Number.MIN_VALUE);
  });
});

describe("durationTicks", () => {
  it("uses seconds for an axis topping out under two minutes", () => {
    const axis = durationTicks(10 * SECOND_NS);
    expect(axis.ticks).toEqual([0, 5 * SECOND_NS, 10 * SECOND_NS]);
    expect(axis.format(0)).toBe("0 s");
    expect(axis.format(5 * SECOND_NS)).toBe("5 s");
    expect(axis.format(10 * SECOND_NS)).toBe("10 s");
  });

  it("uses milliseconds below one second", () => {
    const axis = durationTicks(250_000_000);
    expect(axis.ticks.at(-1)).toBe(300_000_000);
    expect(axis.format(100_000_000)).toBe("100 ms");
  });

  it("uses microseconds below one millisecond", () => {
    const axis = durationTicks(250_000);
    expect(axis.ticks).toEqual([0, 100_000, 200_000, 300_000]);
    expect(axis.format(100_000)).toBe("100 µs");
    const sub = durationTicks(500);
    expect(sub.format(200)).toBe("0.2 µs");
  });

  it("uses minutes from 120 seconds up, with no trailing zeros", () => {
    const axis = durationTicks(300 * SECOND_NS);
    expect(axis.format(90 * SECOND_NS)).toBe("1.5 min");
    expect(axis.format(0)).toBe("0 min");
  });

  it("gives a usable axis for a zero maximum", () => {
    const axis = durationTicks(0);
    expect(axis.ticks[0]).toBe(0);
    expect(axis.ticks.length).toBeGreaterThan(1);
  });
});

describe("timeTicks", () => {
  it("puts ticks on whole UTC times on the smallest step giving at most eight", () => {
    const from = Date.UTC(2026, 0, 1, 0, 7);
    const to = Date.UTC(2026, 0, 1, 3, 0);
    expect(timeTicks(from, to, true)).toEqual([
      Date.UTC(2026, 0, 1, 0, 30),
      Date.UTC(2026, 0, 1, 1, 0),
      Date.UTC(2026, 0, 1, 1, 30),
      Date.UTC(2026, 0, 1, 2, 0),
      Date.UTC(2026, 0, 1, 2, 30),
      Date.UTC(2026, 0, 1, 3, 0),
    ]);
  });

  it("puts local ticks on the local minute grid", () => {
    const from = new Date(2026, 0, 1, 0, 7).getTime();
    const to = new Date(2026, 0, 1, 3, 0).getTime();
    const ticks = timeTicks(from, to, false);
    expect(ticks.length).toBeGreaterThan(0);
    expect(ticks.length).toBeLessThanOrEqual(8);
    for (const tick of ticks) {
      expect(tick).toBeGreaterThanOrEqual(from);
      expect(tick).toBeLessThanOrEqual(to);
      expect(new Date(tick).getMinutes() % 30).toBe(0);
    }
  });

  it("puts day steps on UTC midnight and on day numbers divisible by the step", () => {
    const from = Date.UTC(2026, 0, 1);
    const to = Date.UTC(2026, 0, 20);
    expect(timeTicks(from, to, true)).toEqual([
      Date.UTC(2026, 0, 1),
      Date.UTC(2026, 0, 8),
      Date.UTC(2026, 0, 15),
    ]);
  });

  it("falls back to the seven-day grid when no step fits", () => {
    const from = Date.UTC(2026, 0, 1);
    const to = Date.UTC(2026, 11, 31);
    const ticks = timeTicks(from, to, true);
    expect(ticks.length).toBeGreaterThan(8);
    for (const tick of ticks) {
      expect((tick / DAY) % 7).toBe(0);
    }
  });

  it("gives no ticks for a NaN bound or a reversed range", () => {
    expect(timeTicks(Number.NaN, 1, true)).toEqual([]);
    expect(timeTicks(10, 5, true)).toEqual([]);
  });
});

describe("chooseBucket", () => {
  const cases: [string, number, string][] = [
    ["15 minutes", 15 * MINUTE, "1m"],
    ["1 hour", 60 * MINUTE, "1m"],
    ["6 hours", 6 * 60 * MINUTE, "10m"],
    ["24 hours", DAY, "30m"],
    ["7 days", 7 * DAY, "6h"],
    ["30 days", 30 * DAY, "12h"],
    ["60 days", 60 * DAY, "1d"],
    ["90 days", 90 * DAY, "1d"],
  ];
  for (const [name, span, bucket] of cases) {
    it(`gives ${bucket} for ${name}`, () => {
      expect(chooseBucket(1_000_000_000_000, 1_000_000_000_000 + span)).toBe(bucket);
    });
  }

  it("allows exactly 60 buckets and not 61", () => {
    const from = 1_000_000_000_000;
    expect(chooseBucket(from, from + 60 * MINUTE)).toBe("1m");
    expect(chooseBucket(from, from + 61 * MINUTE)).toBe("5m");
  });

  it("gives one-minute buckets for an empty range and one day for an invalid one", () => {
    expect(chooseBucket(5, 5)).toBe("1m");
    expect(chooseBucket(Number.NaN, 5)).toBe("1d");
  });
});

it("keeps whole steps for counts", () => {
  expect(niceTicks(2, 4, 1)).toEqual([0, 1, 2]);
  expect(niceTicks(0.4, 4, 1)).toEqual([0, 1]);
  expect(niceTicks(22, 4, 1)).toEqual([0, 10, 20, 30]);
});
