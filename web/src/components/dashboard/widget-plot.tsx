import { type KeyboardEvent, useMemo, useState } from "react";
import type { BarShapeProps } from "recharts";
import {
  Area,
  Bar,
  CartesianGrid,
  ComposedChart,
  Legend,
  Line,
  ReferenceArea,
  ReferenceLine,
  Tooltip,
  XAxis,
  YAxis,
} from "recharts";
import {
  AXIS_TICK,
  axisStyle,
  BucketBar,
  formatTick,
  largestValue,
  lineDot,
  solitary,
  xAxisTicks,
} from "@/components/charts/plot-helpers";
import { indexOf, useDragZoom } from "@/components/charts/use-drag-zoom";
import { type ChartConfig, ChartContainer } from "@/components/ui/chart";
import { axisTicks, formatValue } from "@/lib/dashboards/format";
import type { PlotColumn } from "@/lib/dashboards/plot";
import { formatTimestamp } from "@/lib/format";
import type { ChartRow } from "@/lib/metrics";
import { type TimeZoneSetting, useTimeZone } from "@/lib/settings";

export type PlotKind = "line" | "bar" | "area";

/** Axis labels: `ticks` undefined lets Recharts choose them; `domain` is `[0, top]` unless `auto`. */
type Axis = {
  ticks?: number[];
  domain: [number, number] | ["auto", "auto"];
  format: (value: number) => string;
};

/**
 * One time chart of a widget: a category X axis of buckets, a value axis, and the series as
 * lines, bars or areas. Only the chart under the pointer shows its tooltip; all the time charts of
 * the dashboard share a cursor. The chart is one keyboard stop: arrows move between buckets,
 * Home and End jump to the ends, Enter opens the bucket (D-C-3).
 */
export function WidgetPlot({
  title,
  kind,
  stacked,
  columns,
  rows,
  bucketNs,
  maxTicks,
  fadeBelow,
  onBucket,
  onZoom,
}: {
  title: string;
  kind: PlotKind;
  stacked: boolean;
  columns: PlotColumn[];
  rows: ChartRow[];
  bucketNs: number;
  /** The most X-axis labels the plot draws. */
  maxTicks: number;
  /** Bars and areas: buckets with fewer rows than this are faded. */
  fadeBelow?: number;
  /** A bucket was opened: its time, and its column's group (`null` for the other group). */
  onBucket?: (timeMs: number, group: string | null | undefined) => void;
  onZoom?: (fromMs: number, toMs: number) => void;
}) {
  const zone = useTimeZone();
  const style = axisStyle(rows);
  const config = useMemo(() => chartConfig(columns), [columns]);
  const axis = useMemo(
    () => valueAxis(columns, rows, kind, stacked),
    [columns, rows, kind, stacked],
  );
  const [active, setActive] = useState<number | null>(null);
  const index = active !== null && active < rows.length ? active : null;
  const activeRow = index === null ? undefined : rows[index];
  // Only the chart under the pointer shows its tooltip; the others share its cursor line.
  const [hovered, setHovered] = useState(false);
  const drag = useDragZoom({ rows, bucketNs, onZoom });
  const firstGroup = columns[0]?.group;

  function onKeyDown(event: KeyboardEvent<HTMLDivElement>) {
    if (rows.length === 0) return;
    const last = rows.length - 1;
    let next: number | null | undefined;
    if (event.key === "ArrowRight") next = index === null ? 0 : Math.min(last, index + 1);
    else if (event.key === "ArrowLeft") next = index === null ? last : Math.max(0, index - 1);
    else if (event.key === "Home") next = 0;
    else if (event.key === "End") next = last;
    else if (event.key === "Escape") next = null;
    else if ((event.key === "Enter" || event.key === " ") && activeRow && onBucket) {
      event.preventDefault();
      onBucket(activeRow.time, firstGroup);
      return;
    }
    if (next === undefined) return;
    event.preventDefault();
    setActive(next);
  }

  const bucketOf = (column: PlotColumn) =>
    onBucket ? (timeMs: number) => onBucket(timeMs, column.group) : undefined;
  const fadeKey = (column: PlotColumn) =>
    fadeBelow === undefined ? undefined : `${column.key}__rows`;

  return (
    // biome-ignore lint/a11y/useSemanticElements: a chart is a focusable group with a custom keyboard model (D-C-3)
    <div
      role="group"
      aria-roledescription="chart"
      aria-label={`${title}. Arrow keys move between time buckets${onBucket ? "; Enter opens the bucket" : ""}.`}
      // biome-ignore lint/a11y/noNoninteractiveTabindex: the chart is one keyboard stop (D-C-3)
      tabIndex={0}
      className="focus-ring mt-2 flex min-h-0 flex-1 flex-col rounded-sm"
      onKeyDown={onKeyDown}
      onBlur={(event) => {
        if (!event.currentTarget.contains(event.relatedTarget)) setActive(null);
      }}
      onMouseEnter={() => setHovered(true)}
      onMouseLeave={() => {
        setHovered(false);
        drag.onMouseLeave();
      }}
      onClickCapture={drag.onClickCapture}
    >
      <ChartContainer
        config={config}
        data-chart-container=""
        initialDimension={{ width: 640, height: 160 }}
        className="aspect-auto min-h-0 w-full flex-1"
      >
        <ComposedChart
          data={rows}
          margin={{ top: 8, right: 12, left: 0, bottom: 0 }}
          accessibilityLayer={false}
          syncId="dashboard"
          syncMethod="value"
          onMouseDown={drag.onMouseDown}
          onMouseMove={drag.onMouseMove}
          onMouseUp={drag.onMouseUp}
          // An area has no per-point target, so the chart's own click opens the bucket.
          onClick={
            kind === "area" && onBucket
              ? (state) => {
                  const at = indexOf(state);
                  const time = at === undefined ? undefined : rows[at]?.time;
                  if (time !== undefined) onBucket(time, undefined);
                }
              : undefined
          }
        >
          <CartesianGrid stroke="var(--border)" strokeOpacity={0.7} vertical={false} />
          <XAxis
            dataKey="time"
            tick={AXIS_TICK}
            ticks={xAxisTicks(rows, zone === "utc", maxTicks)}
            interval={0}
            tickFormatter={(value) => formatTick(value, zone, style)}
            minTickGap={28}
            axisLine={false}
            tickLine={false}
          />
          <YAxis
            tick={AXIS_TICK}
            ticks={axis.ticks}
            domain={axis.domain}
            tickFormatter={(value) => (typeof value === "number" ? axis.format(value) : "—")}
            width={72}
            axisLine={false}
            tickLine={false}
          />
          <Tooltip
            content={(props) =>
              hovered ? (
                <SeriesTooltip
                  {...props}
                  columns={columns}
                  zone={zone}
                  axisStyle={style}
                  fadeBelow={fadeBelow}
                />
              ) : null
            }
            cursor={{ stroke: "var(--border)" }}
          />
          {columns.length > 1 ? <Legend iconSize={8} wrapperStyle={{ fontSize: 11 }} /> : null}
          {columns.map((column) => {
            if (kind === "bar") {
              return (
                <Bar
                  key={column.key}
                  dataKey={column.key}
                  name={column.label}
                  fill={column.color}
                  stackId={stacked ? "stack" : undefined}
                  isAnimationActive={false}
                  shape={(props: BarShapeProps) => (
                    <BucketBar
                      {...props}
                      onBucket={bucketOf(column)}
                      fadeKey={fadeKey(column)}
                      fadeBelow={fadeBelow}
                    />
                  )}
                />
              );
            }
            if (kind === "area") {
              return (
                <Area
                  key={column.key}
                  dataKey={column.key}
                  name={column.label}
                  stroke={column.color}
                  fill={column.color}
                  fillOpacity={0.6}
                  stackId={stacked ? "stack" : undefined}
                  dot={false}
                  connectNulls={false}
                  isAnimationActive={false}
                  type="linear"
                />
              );
            }
            return (
              <Line
                key={column.key}
                dataKey={column.key}
                name={column.label}
                stroke={column.color}
                strokeWidth={1.5}
                legendType="plainline"
                dot={lineDot(column.key, bucketOf(column), solitary(rows, column.key))}
                activeDot={{ r: 3, strokeWidth: 1.5 }}
                connectNulls={false}
                isAnimationActive={false}
                type="linear"
              />
            );
          })}
          {drag.shade ? (
            <ReferenceArea
              x1={rows[drag.shade.start]?.time}
              x2={rows[drag.shade.end]?.time}
              stroke="none"
              fill="var(--ring)"
              fillOpacity={0.15}
              ifOverflow="extendDomain"
            />
          ) : null}
          {activeRow ? (
            <ReferenceLine
              x={activeRow.time}
              stroke="var(--ring)"
              strokeWidth={1.5}
              ifOverflow="extendDomain"
            />
          ) : null}
        </ComposedChart>
      </ChartContainer>
      <p className="sr-only" aria-live="polite">
        {activeRow ? bucketSummary(activeRow, columns, zone, style) : ""}
      </p>
    </div>
  );
}

/**
 * The value axis: round ticks from 0 to the top of the largest value (the stacked sum of a stacked
 * chart). A chart with a negative value (a score) sizes its axis to the data instead.
 */
function valueAxis(
  columns: readonly PlotColumn[],
  rows: readonly ChartRow[],
  kind: PlotKind,
  stacked: boolean,
): Axis {
  const first = columns[0];
  const kindOf = first?.kind ?? "count";
  const stackedColumns = columns.map((column) => ({
    key: column.key,
    stacked: stacked && kind !== "line",
  }));
  if (smallestValue(columns, rows) < 0) {
    return { domain: ["auto", "auto"], format: axisTicks(kindOf, 1).format };
  }
  const scale = axisTicks(kindOf, largestValue(stackedColumns, rows));
  const top = scale.ticks.at(-1) ?? 1;
  return { ticks: scale.ticks, domain: [0, top], format: scale.format };
}

/** The smallest present value of the columns, or `Infinity` when there is none. */
function smallestValue(columns: readonly PlotColumn[], rows: readonly ChartRow[]): number {
  let min = Number.POSITIVE_INFINITY;
  for (const row of rows) {
    for (const column of columns) {
      const value = row[column.key];
      if (typeof value === "number" && !Number.isNaN(value)) {
        min = Math.min(min, value);
      }
    }
  }
  return min;
}

function chartConfig(columns: readonly PlotColumn[]): ChartConfig {
  const config: ChartConfig = {};
  for (const column of columns) {
    config[column.key] = { label: column.label };
  }
  return config;
}

function SeriesTooltip({
  active,
  payload,
  columns,
  zone,
  axisStyle: style,
  fadeBelow,
}: {
  active?: boolean;
  payload?: ReadonlyArray<{ payload?: ChartRow }>;
  columns: readonly PlotColumn[];
  zone: TimeZoneSetting;
  axisStyle: "time" | "datetime";
  fadeBelow?: number;
}) {
  const row = payload?.[0]?.payload;
  if (!active || !row) {
    return null;
  }
  return (
    <div className="grid min-w-32 gap-1 rounded-md border bg-popover px-2.5 py-1.5 text-caption shadow-md">
      <div className="font-mono text-caption text-muted-foreground">
        {formatTimestamp(new Date(row.time).toISOString(), zone, style)}
      </div>
      {columns.map((column) => {
        const rowsOf = row[`${column.key}__rows`];
        const thin = fadeBelow !== undefined && typeof rowsOf === "number" && rowsOf < fadeBelow;
        return (
          <div key={column.key} className="flex items-center justify-between gap-3">
            <span className="flex items-center gap-1.5 text-muted-foreground">
              <span
                className="size-2 shrink-0 rounded-[2px]"
                style={{ background: column.color }}
              />
              {column.label}
            </span>
            <span className="font-mono text-foreground tabular-nums">
              {formatValue(column.kind, row[column.key])}
              {thin ? ` (${rowsOf} rows)` : ""}
            </span>
          </div>
        );
      })}
    </div>
  );
}

function bucketSummary(
  row: ChartRow,
  columns: readonly PlotColumn[],
  zone: TimeZoneSetting,
  style: "time" | "datetime",
): string {
  const values = columns
    .map((column) => `${column.label} ${formatValue(column.kind, row[column.key])}`)
    .join(", ");
  return `${formatTimestamp(new Date(row.time).toISOString(), zone, style)}: ${values}`;
}
