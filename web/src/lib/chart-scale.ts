import type { Schema } from "@/api/types";

/** A bucket width the UI can request: `auto` is the server's own rule and the UI never sends it. */
export type ChartBucket = Exclude<Schema<"MetricsBucket">, "auto">;

const MINUTE_MS = 60_000;
const HOUR_MS = 60 * MINUTE_MS;
const DAY_MS = 24 * HOUR_MS;

/** Largest number of buckets the UI asks for; the server allows far more (spec 05). */
const MAX_BUCKETS = 60;

const BUCKET_STEPS: readonly [ChartBucket, number][] = [
  ["1m", MINUTE_MS],
  ["5m", 5 * MINUTE_MS],
  ["10m", 10 * MINUTE_MS],
  ["15m", 15 * MINUTE_MS],
  ["30m", 30 * MINUTE_MS],
  ["1h", HOUR_MS],
  ["6h", 6 * HOUR_MS],
  ["12h", 12 * HOUR_MS],
  ["1d", DAY_MS],
];

const TIME_STEPS_MS: readonly number[] = [
  1 * MINUTE_MS,
  5 * MINUTE_MS,
  10 * MINUTE_MS,
  15 * MINUTE_MS,
  30 * MINUTE_MS,
  HOUR_MS,
  2 * HOUR_MS,
  3 * HOUR_MS,
  6 * HOUR_MS,
  12 * HOUR_MS,
  DAY_MS,
  2 * DAY_MS,
  7 * DAY_MS,
];

const DURATION_UNITS = [
  { label: "µs", ns: 1_000 },
  { label: "ms", ns: 1_000_000 },
  { label: "s", ns: 1_000_000_000 },
  { label: "min", ns: 60_000_000_000 },
] as const;

/** Rounds to 12 significant digits so that multiples of a step carry no float noise. */
function clean(value: number): number {
  return Number(value.toPrecision(12));
}

/**
 * Ticks from 0 up to the first multiple of a 1-2-5 step that covers `max`, with at most
 * `count` steps and no step below `minStep` (1 for counts). A non-positive or non-finite
 * `max` gives `[0, 1]`.
 */
export function niceTicks(max: number, count = 4, minStep = 0): number[] {
  if (!Number.isFinite(max) || max <= 0) {
    return [0, 1];
  }
  const steps = Math.max(1, Math.floor(count));
  // Starting two decades below `max` finds the smallest fitting step; `-324` is the
  // smallest exponent a finite positive number can have (below it, steps underflow to 0).
  const first = Math.max(Math.floor(Math.log10(max)) - 2, -324);
  for (let exponent = first; ; exponent += 1) {
    for (const mantissa of [1, 2, 5]) {
      const step = Number(`${mantissa}e${exponent}`);
      if (!(step > 0) || step < minStep) {
        continue;
      }
      const intervals = Math.ceil(max / step - 1e-9);
      if (intervals <= steps) {
        const ticks: number[] = [];
        for (let index = 0; index <= intervals; index += 1) {
          ticks.push(clean(index * step));
        }
        return ticks;
      }
    }
  }
}

/**
 * Ticks and a formatter for a duration axis in nanoseconds. One unit is chosen for the
 * whole axis from its top tick: µs below 1 ms, ms below 1 s, s below 120 s, else minutes.
 */
export function durationTicks(
  maxNs: number,
  count = 4,
): { ticks: number[]; format: (ns: number) => string } {
  const topNs = niceTicks(maxNs, count).at(-1) ?? 1;
  const unit =
    topNs < 1_000_000
      ? DURATION_UNITS[0]
      : topNs < 1_000_000_000
        ? DURATION_UNITS[1]
        : topNs < 120_000_000_000
          ? DURATION_UNITS[2]
          : DURATION_UNITS[3];
  const ticks = niceTicks(maxNs / unit.ns, count).map((tick) => clean(tick * unit.ns));
  return {
    ticks,
    format: (ns) => `${clean(ns / unit.ns)} ${unit.label}`,
  };
}

/**
 * Ticks of a time axis in `[fromMs, toMs]`, on the smallest step that gives at most
 * `maxTicks` of them. Steps under a day sit on the minute grid of the shown zone (UTC when
 * `utc`, else local time); steps of a day or more sit on midnight. When no step fits (a
 * range longer than about eight weeks), the 7-day ticks are returned as they are.
 */
export function timeTicks(fromMs: number, toMs: number, utc: boolean, maxTicks = 8): number[] {
  if (!Number.isFinite(fromMs) || !Number.isFinite(toMs) || toMs < fromMs) {
    return [];
  }
  for (const step of TIME_STEPS_MS) {
    const ticks = gridTicks(step, fromMs, toMs, utc, maxTicks);
    if (ticks.length <= maxTicks) {
      return ticks;
    }
  }
  return gridTicks(TIME_STEPS_MS.at(-1) ?? 7 * DAY_MS, fromMs, toMs, utc, Infinity);
}

/** Ticks of one step in the range, stopping once more than `limit` are found. */
function gridTicks(
  stepMs: number,
  fromMs: number,
  toMs: number,
  utc: boolean,
  limit: number,
): number[] {
  const ticks: number[] = [];
  if (stepMs < DAY_MS) {
    const stepMinutes = stepMs / MINUTE_MS;
    for (const midnight of dayStarts(fromMs, toMs, utc)) {
      const day = new Date(midnight);
      for (let minute = 0; minute < 1440; minute += stepMinutes) {
        const tick = utc
          ? midnight + minute * MINUTE_MS
          : new Date(day.getFullYear(), day.getMonth(), day.getDate(), 0, minute).getTime();
        if (tick >= fromMs && tick <= toMs) {
          ticks.push(tick);
          if (ticks.length > limit) {
            return ticks;
          }
        }
      }
    }
    return ticks;
  }
  const everyDays = Math.round(stepMs / DAY_MS);
  for (const midnight of dayStarts(fromMs, toMs, utc)) {
    const dayNumber = Math.round(Date.UTC(...calendarDay(midnight, utc)) / DAY_MS);
    if (((dayNumber % everyDays) + everyDays) % everyDays === 0 && midnight >= fromMs) {
      ticks.push(midnight);
      if (ticks.length > limit) {
        return ticks;
      }
    }
  }
  return ticks;
}

/** Midnights (UTC or local) of every calendar day that overlaps `[fromMs, toMs]`. */
function dayStarts(fromMs: number, toMs: number, utc: boolean): number[] {
  const starts: number[] = [];
  const first = utc ? Math.floor(fromMs / DAY_MS) * DAY_MS : localMidnight(new Date(fromMs));
  // Walk calendar days by their date, so local days stay whole across DST changes.
  let cursor = new Date(first);
  for (let guard = 0; guard < 1_000_000; guard += 1) {
    const start = utc ? cursor.getTime() : localMidnight(cursor);
    if (start > toMs) {
      break;
    }
    starts.push(start);
    cursor = utc
      ? new Date(start + DAY_MS)
      : new Date(cursor.getFullYear(), cursor.getMonth(), cursor.getDate() + 1);
  }
  return starts;
}

function localMidnight(date: Date): number {
  return new Date(date.getFullYear(), date.getMonth(), date.getDate()).getTime();
}

/** The `[year, monthIndex, day]` of a midnight in the given zone. */
function calendarDay(midnight: number, utc: boolean): [number, number, number] {
  const date = new Date(midnight);
  return utc
    ? [date.getUTCFullYear(), date.getUTCMonth(), date.getUTCDate()]
    : [date.getFullYear(), date.getMonth(), date.getDate()];
}

/**
 * The smallest bucket width giving at most 60 buckets over `[fromMs, toMs)`, counted as
 * `ceil(span / width)`; a span too long for that gives one-day buckets.
 */
export function chooseBucket(fromMs: number, toMs: number): ChartBucket {
  const span = toMs - fromMs;
  for (const [bucket, widthMs] of BUCKET_STEPS) {
    if (Math.ceil(span / widthMs) <= MAX_BUCKETS) {
      return bucket;
    }
  }
  return "1d";
}

/**
 * Ticks of a percent axis: `max` is a rate (0 to 1), so the ticks are nice percentages divided by
 * 100. The step is at least a tenth of a percent, so that every label is different.
 */
export function percentTicks(max: number): number[] {
  return niceTicks(max * 100, 4, 0.1).map((tick) => Number((tick / 100).toPrecision(12)));
}
