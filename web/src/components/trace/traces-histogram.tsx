import { keepPreviousData, useQuery } from "@tanstack/react-query";
import { useState } from "react";
import { Bar, BarChart, ReferenceArea, Tooltip, XAxis, YAxis } from "recharts";
import { metricsQuery } from "@/api/queries";
import type { Schema } from "@/api/types";
import { axisStyle, formatTick } from "@/components/charts/metric-chart";
import { type ChartConfig, ChartContainer } from "@/components/ui/chart";
import { formatTimestamp } from "@/lib/format";
import { bucketRange, epochMs, seriesByMetric } from "@/lib/metrics";
import { type TimeZoneSetting, useTimeZone } from "@/lib/settings";

type Props = {
  org: string;
  project: string;
  from: string;
  to: string;
  filter?: string;
  includeEval: boolean;
  includeExperiments: boolean;
  onRange: (fromIso: string, toIso: string) => void;
};

/** One bucket: `ok` traces and `errors` (error traces) starting at `time` ms. */
export type HistogramRow = { time: number; ok: number; errors: number };

const CONFIG = {
  ok: { label: "OK", color: "var(--status-ok)" },
  errors: { label: "Errors", color: "var(--status-error)" },
} satisfies ChartConfig;

/**
 * One row per bucket present in either series, sorted by time. A missing point
 * counts as 0. `ok` is traces minus errors, never below 0.
 */
export function histogramRows(response: Pick<Schema<"MetricsResponse">, "series">): HistogramRow[] {
  const byMetric = seriesByMetric(response);
  const traces = pointTotals(byMetric.get("traces") ?? []);
  const errors = pointTotals(byMetric.get("errors") ?? []);
  const times = new Set<number>([...traces.keys(), ...errors.keys()]);
  return [...times]
    .sort((left, right) => left - right)
    .map((time) => {
      const total = traces.get(time) ?? 0;
      const failed = errors.get(time) ?? 0;
      return { time, ok: Math.max(0, total - failed), errors: failed };
    });
}

/**
 * The range a drag between two bucket starts selects: from the start of the
 * earlier bucket to the end of the later one. `undefined` when both are the
 * same bucket (a click selects nothing).
 */
export function selectionRange(
  firstTime: number,
  lastTime: number,
  bucketNs: number,
): { from: string; to: string } | undefined {
  if (firstTime === lastTime) {
    return undefined;
  }
  const earlier = Math.min(firstTime, lastTime);
  const later = Math.max(firstTime, lastTime);
  return {
    from: bucketRange(earlier, bucketNs).from,
    to: bucketRange(later, bucketNs).to,
  };
}

export function TracesHistogram({
  org,
  project,
  from,
  to,
  filter,
  includeEval,
  includeExperiments,
  onRange,
}: Props) {
  const zone = useTimeZone();
  const query = useQuery({
    ...metricsQuery(org, project, { from, to, filter, includeEval, includeExperiments }),
    placeholderData: keepPreviousData,
  });
  const [drag, setDrag] = useState<{ start: number; end: number } | null>(null);

  if (query.isError) {
    return null;
  }
  if (!query.data) {
    return <div role="img" aria-label="Traces over time" className="h-16 w-full" />;
  }

  const bucketNs = query.data.bucket_ns;
  const rows = histogramRows(query.data);
  const tickStyle = axisStyle(rows);
  const selected = drag ? [drag.start, drag.end].sort((left, right) => left - right) : undefined;
  const low = selected?.[0];
  const high = selected?.[1];

  function indexOf(state: { activeTooltipIndex?: unknown } | undefined): number | undefined {
    const index = state?.activeTooltipIndex;
    return typeof index === "number" && index >= 0 && index < rows.length ? index : undefined;
  }

  function finishDrag() {
    if (!drag) {
      return;
    }
    setDrag(null);
    const first = rows[drag.start];
    const last = rows[drag.end];
    if (!first || !last) {
      return;
    }
    const range = selectionRange(first.time, last.time, bucketNs);
    if (range) {
      onRange(range.from, range.to);
    }
  }

  return (
    <div role="img" aria-label="Traces over time" className="h-16 w-full select-none">
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
          onMouseDown={(state) => {
            const index = indexOf(state);
            if (index !== undefined) {
              setDrag({ start: index, end: index });
            }
          }}
          onMouseMove={(state) => {
            const index = indexOf(state);
            if (drag && index !== undefined) {
              setDrag({ start: drag.start, end: index });
            }
          }}
          onMouseUp={finishDrag}
          onMouseLeave={() => setDrag(null)}
        >
          <XAxis
            dataKey="time"
            tick={{ fontSize: 10, fill: "var(--muted-foreground)" }}
            tickFormatter={(value) => formatTick(value, zone, tickStyle)}
            minTickGap={40}
            height={14}
            axisLine={false}
            tickLine={false}
          />
          <YAxis hide domain={[0, "auto"]} />
          <Tooltip
            cursor={{ fill: "var(--muted-foreground)", fillOpacity: 0.12 }}
            content={<HistogramTooltip zone={zone} />}
          />
          <Bar
            dataKey="ok"
            stackId="traces"
            fill="var(--status-ok)"
            fillOpacity={0.7}
            isAnimationActive={false}
          />
          <Bar
            dataKey="errors"
            stackId="traces"
            fill="var(--status-error)"
            isAnimationActive={false}
          />
          {low !== undefined && high !== undefined && rows[low] && rows[high] ? (
            <ReferenceArea
              x1={rows[low].time}
              x2={rows[high].time}
              fill="var(--ring)"
              fillOpacity={0.2}
              stroke="none"
            />
          ) : null}
        </BarChart>
      </ChartContainer>
    </div>
  );
}

function HistogramTooltip({
  active,
  payload,
  zone,
}: {
  active?: boolean;
  payload?: ReadonlyArray<{ payload?: HistogramRow }>;
  zone: TimeZoneSetting;
}) {
  const row = payload?.[0]?.payload;
  if (!active || !row) {
    return null;
  }
  return (
    <div className="grid min-w-32 gap-1 rounded-md border bg-popover px-2.5 py-1.5 text-xs shadow-md">
      <div className="font-mono text-[11px] text-muted-foreground">
        {formatTimestamp(new Date(row.time).toISOString(), zone, "datetime")}
      </div>
      <div className="flex items-center justify-between gap-4">
        <span className="text-muted-foreground">OK</span>
        <span className="font-mono tabular-nums">{row.ok}</span>
      </div>
      <div className="flex items-center justify-between gap-4">
        <span className="text-muted-foreground">Errors</span>
        <span className="font-mono tabular-nums">{row.errors}</span>
      </div>
    </div>
  );
}

function pointTotals(series: readonly Schema<"MetricSeries">[]): Map<number, number> {
  const totals = new Map<number, number>();
  for (const item of series) {
    for (const point of item.points) {
      const time = epochMs(point.time);
      if (time !== undefined) {
        totals.set(time, (totals.get(time) ?? 0) + (point.value ?? 0));
      }
    }
  }
  return totals;
}
