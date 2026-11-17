import type { CountBucket } from "@/components/charts/count-bar-chart";
import { CountBarChart } from "@/components/charts/count-bar-chart";
import { epochMs } from "@/lib/metrics";

/** Buckets of a user's `activity` (equal parts of `[from, to)`), as count-chart rows. Zero is empty. */
export function activityBuckets(
  values: readonly number[],
  from: string,
  to: string,
): CountBucket[] {
  const fromMs = epochMs(from);
  const toMs = epochMs(to);
  if (fromMs === undefined || toMs === undefined || toMs <= fromMs || values.length === 0) {
    return [];
  }
  const width = (toMs - fromMs) / values.length;
  return values.map((value, index) => ({
    time: Math.floor(fromMs + index * width),
    count: value > 0 ? value : null,
  }));
}

/** A user's activity over the list's range: traces per bucket, with a time axis and tooltip. */
export function ActivityChart({
  values,
  from,
  to,
}: {
  values: readonly number[];
  from: string;
  to: string;
}) {
  const rows = activityBuckets(values, from, to);
  if (rows.every((row) => row.count === null)) {
    return <p className="text-muted-foreground">No activity</p>;
  }
  return <CountBarChart rows={rows} label="Activity" seriesLabel="Traces" />;
}
