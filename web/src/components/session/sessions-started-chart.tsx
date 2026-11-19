import type { Schema } from "@/api/types";
import { CountBarChart, type CountBucket } from "@/components/charts/count-bar-chart";
import { epochMs } from "@/lib/metrics";

const BUCKETS = 24;

/**
 * Splits `[from, to]` into 24 buckets and counts the sessions that start in each.
 * A bucket with no session is `null`, so it draws no bar. Sessions outside the
 * range are left out, and a start exactly at `to` counts in the last bucket.
 */
export function startedBuckets(
  sessions: readonly Pick<Schema<"SessionSummary">, "start_time">[],
  from: string,
  to: string,
): CountBucket[] {
  const fromMs = epochMs(from);
  const toMs = epochMs(to);
  if (fromMs === undefined || toMs === undefined || toMs <= fromMs) {
    return [];
  }
  const width = (toMs - fromMs) / BUCKETS;
  const counts = new Array<number>(BUCKETS).fill(0);
  for (const session of sessions) {
    const time = epochMs(session.start_time);
    if (time === undefined || time < fromMs || time > toMs) {
      continue;
    }
    const index = Math.min(BUCKETS - 1, Math.floor((time - fromMs) / width));
    counts[index] = (counts[index] ?? 0) + 1;
  }
  return counts.map((count, index) => ({
    time: Math.floor(fromMs + index * width),
    count: count > 0 ? count : null,
  }));
}

export function SessionsStartedChart({
  sessions,
  from,
  to,
}: {
  sessions: readonly Pick<Schema<"SessionSummary">, "start_time">[];
  from: string;
  to: string;
}) {
  const rows = startedBuckets(sessions, from, to);
  return <CountBarChart rows={rows} label="Sessions started" seriesLabel="Sessions" />;
}
