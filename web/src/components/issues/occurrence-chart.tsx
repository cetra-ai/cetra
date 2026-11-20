import type { Schema } from "@/api/types";
import { CountBarChart, type CountBucket } from "@/components/charts/count-bar-chart";
import { epochMs } from "@/lib/metrics";

/** Days the occurrence chart shows, the last one included (D-IC-16). */
export const OCCURRENCE_DAYS = 14;

const DAY_MS = 24 * 60 * 60 * 1000;

/**
 * The period of the chart: the start of the UTC day `OCCURRENCE_DAYS - 1` days before the day
 * of `now`, up to `now`. `fromMs` is the first bucket's start.
 */
export function occurrenceWindow(now: number): { from: string; to: string; fromMs: number } {
  const date = new Date(now);
  const today = Date.UTC(date.getUTCFullYear(), date.getUTCMonth(), date.getUTCDate());
  const fromMs = today - (OCCURRENCE_DAYS - 1) * DAY_MS;
  return { from: new Date(fromMs).toISOString(), to: date.toISOString(), fromMs };
}

/** One bucket per day from `fromMs`, oldest first. A day the API leaves out counts zero. */
export function occurrenceBuckets(
  days: readonly Pick<Schema<"IssueDay">, "day" | "count">[],
  fromMs: number,
): CountBucket[] {
  const counts = new Map<number, number>();
  for (const day of days) {
    const time = epochMs(day.day);
    if (time !== undefined) {
      counts.set(time, day.count);
    }
  }
  return Array.from({ length: OCCURRENCE_DAYS }, (_, index) => {
    const time = fromMs + index * DAY_MS;
    return { time, count: counts.get(time) ?? 0 };
  });
}

/** Occurrences of an issue per UTC day over the last 14 days. */
export function OccurrenceChart({
  days,
  fromMs,
}: {
  days: readonly Pick<Schema<"IssueDay">, "day" | "count">[];
  fromMs: number;
}) {
  const rows = occurrenceBuckets(days, fromMs);
  return <CountBarChart rows={rows} label="Occurrences per day" seriesLabel="Occurrences" />;
}
