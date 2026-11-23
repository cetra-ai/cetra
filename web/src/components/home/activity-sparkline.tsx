import type { BarShapeProps } from "recharts";
import { Bar, BarChart, XAxis, YAxis } from "recharts";
import { BucketBar, roundsTop } from "@/components/charts/plot-helpers";
import { type ChartConfig, ChartContainer } from "@/components/ui/chart";
import { epochMs } from "@/lib/metrics";
import { HOUR_MS } from "@/lib/org-home";

/** One hour of a project's traces: all of them, and the error traces among them. */
export type ActivityCounts = { traces: number; errors: number };

/** One bar: its start in ms, the OK and error traces; null draws no segment. */
export type SparkRow = { time: number; ok: number | null; errors: number | null };

const CONFIG = {
  ok: { label: "OK", color: "var(--chart-ok)" },
  errors: { label: "Errors", color: "var(--chart-error)" },
} satisfies ChartConfig;

/** The stack of a bar, bottom first: errors sit on top of the OK traces (the traces histogram's colours). */
const STACK = ["ok", "errors"] as const;

/** Bucket `i` starts `i` hours after `from`. Zero segments are null, so no empty bar is drawn. */
export function sparklineRows(from: string, buckets: readonly ActivityCounts[]): SparkRow[] {
  const start = epochMs(from);
  if (start === undefined) {
    return [];
  }
  return buckets.map((bucket, index) => {
    const errors = Math.max(0, Math.min(bucket.errors, bucket.traces));
    const ok = Math.max(0, bucket.traces - errors);
    return {
      time: start + index * HOUR_MS,
      ok: ok > 0 ? ok : null,
      errors: errors > 0 ? errors : null,
    };
  });
}

/**
 * A 32 px stacked bar chart of a project's last 24 hours, without axes or tooltip: on a card the
 * stretched name link covers it (OH-7), so a tooltip could never show.
 */
export function ActivitySparkline({
  from,
  buckets,
}: {
  from: string;
  buckets: readonly ActivityCounts[];
}) {
  const rows = sparklineRows(from, buckets);
  return (
    <div role="img" aria-label="Traces in the last 24 hours" className="h-8 w-full select-none">
      <ChartContainer
        config={CONFIG}
        initialDimension={{ width: 280, height: 32 }}
        className="aspect-auto h-8 w-full"
      >
        <BarChart
          data={rows}
          barCategoryGap="15%"
          margin={{ top: 1, right: 0, bottom: 0, left: 0 }}
          accessibilityLayer={false}
        >
          <XAxis dataKey="time" hide />
          <YAxis hide domain={[0, "auto"]} />
          <Bar
            dataKey="ok"
            stackId="traces"
            fill="var(--chart-ok)"
            isAnimationActive={false}
            shape={(props: BarShapeProps) => (
              <BucketBar {...props} roundTop={roundsTop(props.payload, "ok", STACK)} />
            )}
          />
          <Bar
            dataKey="errors"
            stackId="traces"
            fill="var(--chart-error)"
            isAnimationActive={false}
            shape={(props: BarShapeProps) => (
              <BucketBar {...props} roundTop={roundsTop(props.payload, "errors", STACK)} />
            )}
          />
        </BarChart>
      </ChartContainer>
    </div>
  );
}
