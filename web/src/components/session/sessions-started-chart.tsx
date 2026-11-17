import { Bar, BarChart, Tooltip, XAxis, YAxis } from "recharts";
import type { Schema } from "@/api/types";
import { axisStyle, formatTick } from "@/components/charts/metric-chart";
import { type ChartConfig, ChartContainer } from "@/components/ui/chart";
import { formatTimestamp } from "@/lib/format";
import { epochMs } from "@/lib/metrics";
import { type TimeZoneSetting, useTimeZone } from "@/lib/settings";

const BUCKETS = 24;

/** One bucket of the list's range: its start in ms and how many sessions started in it. */
export type StartedBucket = { time: number; count: number | null };

const CONFIG = {
  count: { label: "Sessions", color: "var(--chart-1)" },
} satisfies ChartConfig;

/**
 * Splits `[from, to]` into 24 buckets and counts the sessions that start in each.
 * A bucket with no session is `null`, so it draws no bar. Sessions outside the
 * range are left out, and a start exactly at `to` counts in the last bucket.
 */
export function startedBuckets(
  sessions: readonly Pick<Schema<"SessionSummary">, "start_time">[],
  from: string,
  to: string,
): StartedBucket[] {
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
  const zone = useTimeZone();
  const rows = startedBuckets(sessions, from, to);
  const style = axisStyle(rows);
  return (
    <div role="img" aria-label="Sessions started" className="h-16 w-full select-none">
      <ChartContainer
        config={CONFIG}
        initialDimension={{ width: 640, height: 64 }}
        className="aspect-auto h-16 w-full"
      >
        <BarChart
          data={rows}
          barCategoryGap="20%"
          maxBarSize={28}
          margin={{ top: 2, right: 0, bottom: 0, left: 0 }}
          accessibilityLayer={false}
        >
          <XAxis
            dataKey="time"
            tick={{ fontSize: 10, fill: "var(--muted-foreground)" }}
            tickFormatter={(value) => formatTick(value, zone, style)}
            minTickGap={40}
            height={20}
            tickMargin={4}
            axisLine={false}
            tickLine={false}
          />
          <YAxis hide domain={[0, "auto"]} />
          <Tooltip
            cursor={{ fill: "var(--muted-foreground)", fillOpacity: 0.12 }}
            content={<StartedTooltip zone={zone} />}
          />
          <Bar dataKey="count" fill="var(--chart-1)" isAnimationActive={false} />
        </BarChart>
      </ChartContainer>
    </div>
  );
}

function StartedTooltip({
  active,
  payload,
  zone,
}: {
  active?: boolean;
  payload?: ReadonlyArray<{ payload?: StartedBucket }>;
  zone: TimeZoneSetting;
}) {
  const row = payload?.[0]?.payload;
  if (!active || !row || row.count === null) {
    return null;
  }
  return (
    <div className="grid min-w-32 gap-1 rounded-md border bg-popover px-2.5 py-1.5 text-xs shadow-md">
      <div className="font-mono text-[11px] text-muted-foreground">
        {formatTimestamp(new Date(row.time).toISOString(), zone, "datetime")}
      </div>
      <div className="flex items-center justify-between gap-4">
        <span className="text-muted-foreground">Sessions</span>
        <span className="font-mono tabular-nums">{row.count}</span>
      </div>
    </div>
  );
}
