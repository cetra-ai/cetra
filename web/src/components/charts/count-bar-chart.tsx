import { Bar, BarChart, Tooltip, XAxis, YAxis } from "recharts";
import { axisStyle, formatTick } from "@/components/charts/metric-chart";
import { type ChartConfig, ChartContainer } from "@/components/ui/chart";
import { formatTimestamp } from "@/lib/format";
import { type TimeZoneSetting, useTimeZone } from "@/lib/settings";

/** One bucket: its start in ms and its count, null when empty (no bar is drawn). */
export type CountBucket = { time: number; count: number | null };

/** A 64px bar chart of counts over time, with a time axis and a tooltip. */
export function CountBarChart({
  rows,
  label,
  seriesLabel,
}: {
  rows: readonly CountBucket[];
  /** The accessible name of the chart (`role="img"`). */
  label: string;
  /** The tooltip's row label, e.g. "Sessions" or "Traces". */
  seriesLabel: string;
}) {
  const zone = useTimeZone();
  const style = axisStyle(rows);
  const config = {
    count: { label: seriesLabel, color: "var(--chart-1)" },
  } satisfies ChartConfig;
  return (
    <div role="img" aria-label={label} className="h-16 w-full select-none">
      <ChartContainer
        config={config}
        initialDimension={{ width: 640, height: 64 }}
        className="aspect-auto h-16 w-full"
      >
        <BarChart
          data={[...rows]}
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
            content={<CountTooltip zone={zone} seriesLabel={seriesLabel} />}
          />
          <Bar dataKey="count" fill="var(--chart-1)" isAnimationActive={false} />
        </BarChart>
      </ChartContainer>
    </div>
  );
}

function CountTooltip({
  active,
  payload,
  zone,
  seriesLabel,
}: {
  active?: boolean;
  payload?: ReadonlyArray<{ payload?: CountBucket }>;
  zone: TimeZoneSetting;
  seriesLabel: string;
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
        <span className="text-muted-foreground">{seriesLabel}</span>
        <span className="font-mono tabular-nums">{row.count}</span>
      </div>
    </div>
  );
}
