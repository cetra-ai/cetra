import { useQuery } from "@tanstack/react-query";
import { useState } from "react";
import type { BarShapeProps } from "recharts";
import { Bar, BarChart, ReferenceArea, Tooltip, XAxis, YAxis } from "recharts";
import { widgetQuery } from "@/api/dashboards";
import type { Schema } from "@/api/types";
import { axisStyle, BucketBar, formatTick, roundsTop } from "@/components/charts/plot-helpers";
import { type ChartConfig, ChartContainer } from "@/components/ui/chart";
import { chooseBucket } from "@/lib/chart-scale";
import { formatTimestamp } from "@/lib/format";
import { LIST_CACHE } from "@/lib/list-anchor";
import { bucketRange, epochMs } from "@/lib/metrics";
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
  ok: { label: "OK", color: "var(--chart-ok)" },
  errors: { label: "Errors", color: "var(--chart-error)" },
} satisfies ChartConfig;

const NS_PER_MS = 1_000_000;
/** The stack of a bar, bottom first: the order the segments render in. */
const TRACE_STACK = ["ok", "errors"] as const;

/**
 * One row per bucket of the grid from the bucket containing `fromMs` to the bucket containing
 * `toMs - 1`, in steps of the answer's bucket width and aligned to the epoch, so a bucket without
 * points is a row of 0 OK and 0 errors. The first measure counts the OK traces and the second the
 * error traces.
 */
export function histogramRows(
  answer: Schema<"WidgetSeriesResponse">,
  fromMs: number,
  toMs: number,
): HistogramRow[] {
  const width = answer.bucket_ns / NS_PER_MS;
  if (!(width > 0)) {
    return [];
  }
  const ok = pointTotals(answer, 0);
  const errors = pointTotals(answer, 1);
  const last = Math.floor((toMs - 1) / width) * width;
  const rows: HistogramRow[] = [];
  for (let time = Math.floor(fromMs / width) * width; time <= last; time += width) {
    rows.push({ time, ok: ok.get(time) ?? 0, errors: errors.get(time) ?? 0 });
  }
  return rows;
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
    ...widgetQuery(org, project, {
      from,
      to,
      shape: "series",
      bucket: chooseBucket(Date.parse(from), Date.parse(to)),
      ...(filter ? { filter } : {}),
      include_eval: includeEval,
      include_experiments: includeExperiments,
      query: {
        source: "traces",
        measures: [
          { agg: "count", where: "status <> 'error'", label: "OK" },
          { agg: "count", where: "status = 'error'", label: "Errors" },
        ],
      },
    }),
    ...LIST_CACHE,
  });
  const [drag, setDrag] = useState<{ start: number; end: number } | null>(null);

  if (query.isError) {
    return null;
  }
  if (query.data?.shape !== "series") {
    return <div role="img" aria-label="Traces over time" className="h-20 w-full" />;
  }

  const bucketNs = query.data.bucket_ns;
  const rows = histogramRows(query.data, Date.parse(from), Date.parse(to));
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
    <div role="img" aria-label="Traces over time" className="h-20 w-full select-none">
      <ChartContainer
        config={CONFIG}
        initialDimension={{ width: 640, height: 80 }}
        className="aspect-auto h-20 w-full"
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
            height={20}
            tickMargin={4}
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
            fill="var(--chart-ok)"
            isAnimationActive={false}
            shape={(props: BarShapeProps) => (
              <BucketBar {...props} roundTop={roundsTop(props.payload, "ok", TRACE_STACK)} />
            )}
          />
          <Bar
            dataKey="errors"
            stackId="traces"
            fill="var(--chart-error)"
            isAnimationActive={false}
            shape={(props: BarShapeProps) => (
              <BucketBar {...props} roundTop={roundsTop(props.payload, "errors", TRACE_STACK)} />
            )}
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
    <div className="grid min-w-32 gap-1 rounded-md border bg-popover px-2.5 py-1.5 text-caption shadow-md">
      <div className="font-mono text-caption text-muted-foreground">
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

/** The points of one measure of a series answer, by bucket start in epoch milliseconds. */
function pointTotals(answer: Schema<"WidgetSeriesResponse">, measure: number): Map<number, number> {
  const totals = new Map<number, number>();
  for (const item of answer.series) {
    if (item.measure !== measure) {
      continue;
    }
    for (const point of item.points) {
      const time = epochMs(point.time);
      if (time !== undefined) {
        totals.set(time, (totals.get(time) ?? 0) + (point.value ?? 0));
      }
    }
  }
  return totals;
}
