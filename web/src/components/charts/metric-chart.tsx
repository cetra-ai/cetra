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
import type { Schema } from "@/api/types";
import { LazyMount } from "@/components/charts/lazy-mount";
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
import {
  CardHeading,
  TOP_MODELS_TITLE,
  TOP_MODELS_UNIT,
  TopModels,
} from "@/components/charts/top-models";
import { indexOf, useDragZoom } from "@/components/charts/use-drag-zoom";
import { type ChartConfig, ChartContainer } from "@/components/ui/chart";
import { Skeleton } from "@/components/ui/skeleton";
import { durationTicks, niceTicks, percentTicks } from "@/lib/chart-scale";
import { formatTimestamp } from "@/lib/format";
import {
  type ChartRow,
  differenceSeries,
  foldGroups,
  formatCount,
  formatMetric,
  formatPercent,
  groupColorIndex,
  type MetricName,
  type MetricSeries,
  PALETTE,
  presentSum,
  seriesByMetric,
  toChartRows,
} from "@/lib/metrics";
import { type TimeZoneSetting, useTimeZone } from "@/lib/settings";

// Two rows on wide screens; the rest mount when scrolled near.
const EAGER_CHARTS = 4;
// Height of a chart card (`h-[220px]` in ChartCard), kept by placeholders.
const CARD_HEIGHT = 220;
// Error-rate buckets with fewer traces than this are drawn faded: their rate is noisy.
const THIN_BUCKET = 5;
// Hidden columns: the traces of each bucket, read by the error-rate chart (not drawn). One per
// group, keyed by `traceCountKey`.
const TRACES_KEY = "\u001ftraces";
// X-axis label budgets: a card's axis has at most six labels, a Scores mini chart four.
const X_TICKS = 6;
const MINI_X_TICKS = 4;
// Scores charts shown in the Scores card; more than this is a "+N more" line.
const SCORE_CHARTS = 6;
const METRIC_LABEL: Record<MetricName, string> = {
  traces: "Traces",
  errors: "Errors",
  error_rate: "Error rate",
  latency_p50: "p50",
  latency_p95: "p95",
  latency_p99: "p99",
  cost: "Cost",
  input_tokens: "Input",
  output_tokens: "Output",
  cache_read_tokens: "Cache read",
  cache_write_tokens: "Cache write",
  ttft_p50: "p50",
  ttft_p95: "p95",
  llm_latency_p50: "p50",
  llm_latency_p95: "p95",
  cost_by_model: "Cost",
  score_avg: "Score",
  eval_cost: "Evaluation cost",
};

type ChartKind = "stacked" | "line" | "bar" | "area" | "scores";
type SummaryKind = "counts" | "sum" | "parts" | "series";

type ChartSpec = {
  title: string;
  unit: string;
  metrics: readonly MetricName[];
  kind: ChartKind;
  summary: SummaryKind;
  /** Keep the largest this many groups; the rest are summed into `other`. Default 8. */
  groups?: number;
  /** Error rate: bars, with buckets of few traces faded. */
  faded?: boolean;
  /** The reason shown when the chart has no data. */
  empty: string;
};

const CHARTS: readonly ChartSpec[] = [
  {
    title: "Traces & errors",
    unit: "count",
    metrics: ["traces", "errors"],
    kind: "stacked",
    summary: "counts",
    empty: "No traces in range.",
  },
  {
    title: "Error rate",
    unit: "%",
    metrics: ["error_rate"],
    kind: "bar",
    summary: "series",
    faded: true,
    empty: "No traces in range.",
  },
  {
    title: "Latency",
    unit: "duration",
    metrics: ["latency_p50", "latency_p95", "latency_p99"],
    kind: "line",
    summary: "series",
    empty: "No traces in range.",
  },
  {
    title: "LLM latency by model",
    unit: "duration",
    metrics: ["llm_latency_p50", "llm_latency_p95"],
    kind: "line",
    summary: "series",
    empty: "No data: needs LLM spans with a model.",
  },
  {
    title: "Cost by model",
    unit: "USD",
    metrics: ["cost_by_model"],
    kind: "area",
    summary: "sum",
    groups: 3,
    empty: "No traces in range.",
  },
  {
    title: "Tokens by type",
    unit: "tokens",
    metrics: ["input_tokens", "output_tokens", "cache_read_tokens", "cache_write_tokens"],
    kind: "stacked",
    summary: "parts",
    empty: "No traces in range.",
  },
  {
    title: "TTFT",
    unit: "duration",
    metrics: ["ttft_p50", "ttft_p95"],
    kind: "line",
    summary: "series",
    empty: "No data: needs streamed LLM spans (time to first token).",
  },
  {
    title: "Scores",
    unit: "score",
    metrics: ["score_avg"],
    kind: "scores",
    summary: "series",
    empty: "No scores in range.",
  },
  {
    title: "Evaluation cost",
    unit: "USD",
    metrics: ["eval_cost"],
    kind: "bar",
    summary: "sum",
    empty: "No evaluation runs in range.",
  },
];

/** What a column shows in the tooltip and the screen-reader summary. */
type Tip = { label: string; value: string };

type Column = {
  key: string;
  label: string;
  metric: MetricName;
  color: string;
  dashed?: string;
  mark: "bar" | "line" | "area";
  stacked: boolean;
  series: MetricSeries;
  /** Not drawn, not in the tooltip or the legend; it only feeds other columns. */
  hidden?: boolean;
  /** The tooltip line, when it is not the column's own label and value. */
  tip?: (row: ChartRow) => Tip;
  /** Error rate: the key of the hidden traces column of the same group, for the faded bars. */
  countKey?: string;
};

export function DashboardBody({
  response,
  onBucket,
  onZoom,
  onModel,
}: {
  response: Schema<"MetricsResponse">;
  onBucket: (timeMs: number) => void;
  /** Drag across buckets: the absolute range to show, from the first bucket's start to the last one's end. */
  onZoom?: (fromMs: number, toMs: number) => void;
  onModel?: (model: string) => void;
}) {
  const byMetric = seriesByMetric(response);
  const colors = groupColorIndex(
    response.top_models.map((model) => model.model),
    response.series.flatMap((item) => (item.group ? [item.group] : [])),
  );
  return (
    <div className="mt-3 grid grid-cols-1 gap-3 xl:grid-cols-2">
      {CHARTS.map((spec, index) => (
        <LazyMount
          key={spec.title}
          eager={index < EAGER_CHARTS}
          minHeight={CARD_HEIGHT}
          fallback={<ChartCardSkeleton title={spec.title} unit={spec.unit} />}
        >
          <ChartCard
            spec={spec}
            byMetric={byMetric}
            colors={colors}
            bucketNs={response.bucket_ns}
            onBucket={onBucket}
            onZoom={onZoom}
          />
        </LazyMount>
      ))}
      <TopModels models={response.top_models} onModel={onModel} />
    </div>
  );
}

/** The dashboard body while its data loads: the same grid of cards, with their titles. */
export function DashboardBodySkeleton() {
  return (
    <div className="mt-3 grid grid-cols-1 gap-3 xl:grid-cols-2">
      {CHARTS.map((spec) => (
        <ChartCardSkeleton key={spec.title} title={spec.title} unit={spec.unit} />
      ))}
      <ChartCardSkeleton title={TOP_MODELS_TITLE} unit={TOP_MODELS_UNIT} />
    </div>
  );
}

/** A chart card that is still loading: the card's box and header, with a pulsing chart area. */
function ChartCardSkeleton({ title, unit }: { title: string; unit: string }) {
  return (
    <section
      aria-label={title}
      aria-busy="true"
      className="flex h-[220px] min-w-0 flex-col rounded-md border p-3"
    >
      <CardHeading title={title} unit={unit} />
      <Skeleton className="mt-2 flex-1" />
    </section>
  );
}

function ChartCard({
  spec,
  byMetric,
  colors,
  bucketNs,
  onBucket,
  onZoom,
}: {
  spec: ChartSpec;
  byMetric: Map<MetricName, MetricSeries[]>;
  colors: ReadonlyMap<string, number>;
  bucketNs: number;
  onBucket: (timeMs: number) => void;
  onZoom?: (fromMs: number, toMs: number) => void;
}) {
  const described = describe(spec, byMetric);
  if (!described.hasData) {
    return (
      <section
        aria-label={described.label}
        className="flex min-w-0 flex-col rounded-md border p-3 xl:col-span-2"
      >
        <CardHeading title={spec.title} unit={spec.unit} />
        <p className="mt-2 text-muted-foreground text-body">{spec.empty}</p>
      </section>
    );
  }
  if (spec.kind === "scores") {
    return (
      <ScoresCard
        label={described.label}
        byMetric={byMetric}
        bucketNs={bucketNs}
        onBucket={onBucket}
        onZoom={onZoom}
      />
    );
  }
  const columns = columnsFor(spec, byMetric, colors);
  const rows = toChartRows(
    columns.map((column) => ({ ...column.series, group: column.key })),
    bucketNs,
  );
  return (
    <section
      aria-label={described.label}
      className="flex h-[220px] min-w-0 flex-col rounded-md border p-3"
    >
      <CardHeading title={spec.title} unit={spec.unit} />
      <MetricPlot
        title={spec.title}
        spec={spec}
        columns={columns}
        rows={rows}
        bucketNs={bucketNs}
        maxTicks={X_TICKS}
        onBucket={onBucket}
        onZoom={onZoom}
      />
    </section>
  );
}

/** The Scores card: one small line chart per score name, sorted by name. */
function ScoresCard({
  label,
  byMetric,
  bucketNs,
  onBucket,
  onZoom,
}: {
  label: string;
  byMetric: Map<MetricName, MetricSeries[]>;
  bucketNs: number;
  onBucket: (timeMs: number) => void;
  onZoom?: (fromMs: number, toMs: number) => void;
}) {
  const series = (byMetric.get("score_avg") ?? [])
    .filter((item) => item.points.some((point) => point.value !== undefined))
    .sort((left, right) => scoreName(left).localeCompare(scoreName(right)));
  const shown = series.slice(0, SCORE_CHARTS);
  const more = series.length - shown.length;
  // Each score's name is one level below its card's title.
  const NameTag = "h3";
  return (
    <section
      aria-label={label}
      className="flex min-w-0 flex-col rounded-md border p-3 xl:col-span-2"
    >
      <CardHeading title="Scores" unit="score" />
      <div className="mt-2 grid gap-3 sm:grid-cols-2 xl:grid-cols-3">
        {shown.map((item) => {
          const name = scoreName(item);
          const spec: ChartSpec = {
            title: name,
            unit: "score",
            metrics: ["score_avg"],
            kind: "line",
            summary: "series",
            empty: "No scores in range.",
          };
          const column: Column = {
            key: "score",
            label: name,
            metric: "score_avg",
            color: "var(--chart-1)",
            mark: "line",
            stacked: false,
            series: item,
          };
          const rows = toChartRows([{ ...item, group: column.key }], bucketNs);
          return (
            <div key={name} className="flex h-40 min-w-0 flex-col">
              <NameTag className="truncate text-caption font-medium">{name}</NameTag>
              <MetricPlot
                title={name}
                spec={spec}
                columns={[column]}
                rows={rows}
                bucketNs={bucketNs}
                maxTicks={MINI_X_TICKS}
                onBucket={onBucket}
                onZoom={onZoom}
              />
            </div>
          );
        })}
      </div>
      {more > 0 ? (
        <p className="mt-2 text-caption text-muted-foreground">+{more} more scores</p>
      ) : null}
    </section>
  );
}

function scoreName(item: MetricSeries): string {
  return item.group ?? METRIC_LABEL.score_avg;
}

function MetricPlot({
  title,
  spec,
  columns: allColumns,
  rows,
  bucketNs,
  maxTicks,
  onBucket,
  onZoom,
}: {
  title: string;
  spec: ChartSpec;
  columns: Column[];
  rows: ChartRow[];
  bucketNs: number;
  /** The most X-axis labels the plot draws. */
  maxTicks: number;
  onBucket: (timeMs: number) => void;
  onZoom?: (fromMs: number, toMs: number) => void;
}) {
  const zone = useTimeZone();
  const style = axisStyle(rows);
  const columns = useMemo(() => allColumns.filter((column) => !column.hidden), [allColumns]);
  const config = useMemo(() => chartConfig(columns), [columns]);
  const yMetric = columns[0]?.metric ?? "traces";
  const yAxis = useMemo(() => valueAxis(spec, columns, rows), [spec, columns, rows]);
  const [active, setActive] = useState<number | null>(null);
  const index = active !== null && active < rows.length ? active : null;
  const activeRow = index === null ? undefined : rows[index];
  // Only the chart under the pointer shows its tooltip; the others share its cursor line.
  const [hovered, setHovered] = useState(false);
  const drag = useDragZoom({ rows, bucketNs, onZoom });

  function onKeyDown(event: KeyboardEvent<HTMLDivElement>) {
    if (rows.length === 0) return;
    const last = rows.length - 1;
    let next: number | null | undefined;
    if (event.key === "ArrowRight") next = index === null ? 0 : Math.min(last, index + 1);
    else if (event.key === "ArrowLeft") next = index === null ? last : Math.max(0, index - 1);
    else if (event.key === "Home") next = 0;
    else if (event.key === "End") next = last;
    else if (event.key === "Escape") next = null;
    else if ((event.key === "Enter" || event.key === " ") && activeRow) {
      event.preventDefault();
      onBucket(activeRow.time);
      return;
    }
    if (next === undefined) return;
    event.preventDefault();
    setActive(next);
  }

  return (
    // biome-ignore lint/a11y/useSemanticElements: a chart is a focusable group with a custom keyboard model (D-C-3)
    <div
      role="group"
      aria-roledescription="chart"
      aria-label={`${title}. Arrow keys move between time buckets; Enter opens the traces of the bucket.`}
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
            spec.kind === "area"
              ? (state) => {
                  const at = indexOf(state);
                  const time = at === undefined ? undefined : rows[at]?.time;
                  if (time !== undefined) onBucket(time);
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
            ticks={yAxis.ticks}
            domain={yAxis.domain}
            tickFormatter={(value) =>
              typeof value === "number" ? yAxis.format(value) : formatMetric(yMetric, null)
            }
            width={72}
            axisLine={false}
            tickLine={false}
          />
          <Tooltip
            content={(props) =>
              hovered ? (
                <SeriesTooltip {...props} columns={columns} zone={zone} axisStyle={style} />
              ) : null
            }
            cursor={{ stroke: "var(--border)" }}
          />
          {columns.length > 1 ? <Legend iconSize={8} wrapperStyle={{ fontSize: 11 }} /> : null}
          {columns.map((column) => {
            if (column.mark === "bar") {
              return (
                <Bar
                  key={column.key}
                  dataKey={column.key}
                  name={column.label}
                  fill={column.color}
                  stackId={column.stacked ? "stack" : undefined}
                  isAnimationActive={false}
                  shape={(props: BarShapeProps) => (
                    <BucketBar
                      {...props}
                      onBucket={onBucket}
                      fadeKey={column.countKey}
                      fadeBelow={THIN_BUCKET}
                    />
                  )}
                />
              );
            }
            if (column.mark === "area") {
              return (
                <Area
                  key={column.key}
                  dataKey={column.key}
                  name={column.label}
                  stroke={column.color}
                  fill={column.color}
                  fillOpacity={0.6}
                  stackId="stack"
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
                strokeDasharray={column.dashed}
                legendType="plainline"
                dot={lineDot(column.key, onBucket, solitary(rows, column.key))}
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

function SeriesTooltip({
  active,
  payload,
  columns,
  zone,
  axisStyle: style,
}: {
  active?: boolean;
  payload?: ReadonlyArray<{ payload?: ChartRow }>;
  columns: Column[];
  zone: TimeZoneSetting;
  axisStyle: "time" | "datetime";
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
        const tip = tipOf(column, row);
        return (
          <div key={column.key} className="flex items-center justify-between gap-3">
            <span className="flex items-center gap-1.5 text-muted-foreground">
              <span
                className="size-2 shrink-0 rounded-[2px]"
                style={{ background: column.color }}
              />
              {tip.label}
            </span>
            <span className="font-mono text-foreground tabular-nums">{tip.value}</span>
          </div>
        );
      })}
    </div>
  );
}

/**
 * The X-axis tick times: the row times that fall on round times of the shown zone (see
 * `timeTicks`). When none does (for example one-day buckets in a local zone), every k-th row
 * with k = ceil(rows / maxTicks), so the axis has at most about `maxTicks` labels.
 */
function bucketSummary(
  row: ChartRow,
  columns: readonly Column[],
  zone: TimeZoneSetting,
  style: "time" | "datetime",
): string {
  const values = columns
    .map((column) => {
      const tip = tipOf(column, row);
      return `${tip.label} ${tip.value}`;
    })
    .join(", ");
  return `${formatTimestamp(new Date(row.time).toISOString(), zone, style)}: ${values}`;
}

/** The tooltip line of a column for one bucket: its tip, or its label and value. */
function tipOf(column: Column, row: ChartRow): Tip {
  if (column.tip) {
    return column.tip(row);
  }
  return {
    label: column.label,
    value: formatMetric(column.metric, row[column.key] ?? null),
  };
}

function columnsFor(
  spec: ChartSpec,
  byMetric: Map<MetricName, MetricSeries[]>,
  colors: ReadonlyMap<string, number>,
): Column[] {
  if (spec.metrics.includes("errors")) {
    return countColumns(byMetric, colors);
  }
  const present = spec.metrics.filter((metric) => (byMetric.get(metric) ?? []).length > 0);
  const multi = present.length > 1;
  const columns: Column[] = [];
  present.forEach((metric, metricIndex) => {
    for (const series of foldGroups(byMetric.get(metric) ?? [], spec.groups)) {
      const group = series.group;
      const key = columnKey(metric, group, multi);
      const label = columnLabel(metric, group, multi);
      // A faded error-rate column reads the traces of its own group for the count and the fade.
      const countKey = spec.faded && metric === "error_rate" ? traceCountKey(group) : undefined;
      columns.push({
        key,
        label,
        metric,
        color: colorOf(spec, metricIndex, group, colors),
        dashed: dashOf(metric),
        mark: markOf(spec.kind),
        stacked: spec.kind === "stacked" || spec.kind === "area",
        series,
        countKey,
        tip:
          countKey === undefined
            ? undefined
            : (row) => rateTip(label, row[key] ?? null, row[countKey] ?? null),
      });
    }
  });
  if (spec.faded) {
    // One hidden column of traces per group, so each error-rate column has its own count.
    for (const series of foldGroups(byMetric.get("traces") ?? [])) {
      columns.push({
        key: traceCountKey(series.group),
        label: METRIC_LABEL.traces,
        metric: "traces",
        color: "var(--muted-foreground)",
        mark: "bar",
        stacked: false,
        series,
        hidden: true,
      });
    }
  }
  return columns;
}

/** The key of the hidden traces column that counts the traces of one group (or of all). */
function traceCountKey(group: string | undefined): string {
  return group === undefined ? TRACES_KEY : `${TRACES_KEY}\u001f${group}`;
}

/**
 * The traces and errors chart. Without groups it is the stacked OK traces (traces without errors)
 * at the bottom and errors on top; the OK tooltip line gives the total, so it reads
 * `Traces <total>`. With groups, each group's traces are bars (not stacked) and its errors a
 * line, in the group's colour.
 */
function countColumns(
  byMetric: Map<MetricName, MetricSeries[]>,
  colors: ReadonlyMap<string, number>,
): Column[] {
  const traces = byMetric.get("traces") ?? [];
  const errors = byMetric.get("errors") ?? [];
  if (traces.some((item) => item.group) || errors.some((item) => item.group)) {
    const columns: Column[] = [];
    for (const series of foldGroups(traces)) {
      columns.push({
        key: columnKey("traces", series.group, true),
        label: columnLabel("traces", series.group, true),
        metric: "traces",
        color: groupOrFixed(series.group, "var(--chart-1)", colors),
        mark: "bar",
        stacked: false,
        series,
      });
    }
    for (const series of foldGroups(errors)) {
      columns.push({
        key: columnKey("errors", series.group, true),
        label: columnLabel("errors", series.group, true),
        metric: "errors",
        color: groupOrFixed(series.group, "var(--chart-error)", colors),
        mark: "line",
        stacked: false,
        series,
      });
    }
    return columns;
  }
  const okTraces = traces[0];
  const errorSeries = errors[0];
  const columns: Column[] = [];
  if (okTraces) {
    const ok = differenceSeries(okTraces, errorSeries);
    columns.push({
      key: "ok",
      label: "OK",
      metric: "traces",
      color: "var(--chart-1)",
      mark: "bar",
      stacked: true,
      series: ok,
      tip: (row) => {
        const okCount = row.ok;
        const errorCount = row.errors ?? 0;
        const total = typeof okCount === "number" ? okCount + errorCount : null;
        return { label: METRIC_LABEL.traces, value: formatMetric("traces", total) };
      },
    });
  }
  if (errorSeries) {
    columns.push({
      key: "errors",
      label: METRIC_LABEL.errors,
      metric: "errors",
      color: "var(--chart-error)",
      mark: "bar",
      stacked: true,
      series: errorSeries,
    });
  }
  return columns;
}

/**
 * Error rate of a bucket, under the column's label; a bucket with few traces says so, since its
 * rate is noisy.
 */
function rateTip(label: string, rate: number | null, traces: number | null): Tip {
  const value = formatMetric("error_rate", rate);
  if (traces !== null && traces < THIN_BUCKET) {
    return { label, value: `${value} (${formatCount(traces)} traces)` };
  }
  return { label, value };
}

/**
 * The colour of one column. A group keeps its dashboard-wide colour from `groupColorIndex`;
 * the folded `other` group (not in the index) is muted. Token types take one palette colour
 * each; every other chart uses one hue for all its lines (the dash tells p50, p95 and p99
 * apart).
 */
function colorOf(
  spec: ChartSpec,
  metricIndex: number,
  group: string | undefined,
  colors: ReadonlyMap<string, number>,
): string {
  if (group !== undefined) {
    return groupColor(group, colors);
  }
  if (spec.summary === "parts") {
    return PALETTE[metricIndex % PALETTE.length] ?? PALETTE[0];
  }
  return PALETTE[0];
}

/** A group's dashboard-wide colour; a group not in the index (the folded `other`) is muted. */
function groupColor(group: string, colors: ReadonlyMap<string, number>): string {
  const index = colors.get(group);
  if (index === undefined) {
    return "var(--muted-foreground)";
  }
  return PALETTE[index % PALETTE.length] ?? PALETTE[0];
}

/** The colour of a series: its group's colour, or `fixed` when it has no group. */
function groupOrFixed(
  group: string | undefined,
  fixed: string,
  colors: ReadonlyMap<string, number>,
): string {
  return group === undefined ? fixed : groupColor(group, colors);
}

/** `ticks` undefined lets Recharts choose them; `domain` is `[0, top]` unless it is `auto`. */
type ValueAxis = {
  ticks?: number[];
  domain: [number, number] | ["auto", "auto"];
  format: (value: number) => string;
};

/**
 * Ticks, domain and label format of the value axis. The top is the last tick, so the domain
 * starts at 0 and ends on a round value. The largest value is the largest present point of
 * any column, or the sum of the stacked columns. A Scores chart with a negative score has no
 * fixed ticks or domain: Recharts sizes the axis to the data.
 */
function valueAxis(
  spec: ChartSpec,
  columns: readonly Column[],
  rows: readonly ChartRow[],
): ValueAxis {
  if (spec.metrics.includes("score_avg") && smallestValue(columns, rows) < 0) {
    return {
      domain: ["auto", "auto"],
      format: (value: number) => formatMetric("score_avg", value),
    };
  }
  const max = largestValue(columns, rows);
  const scale =
    spec.unit === "duration"
      ? durationTicks(max)
      : spec.unit === "%"
        ? { ticks: percentTicks(max), format: formatPercent }
        : {
            ticks: niceTicks(max, 4, minStepOf(spec)),
            format: (value: number) => formatMetric(columns[0]?.metric ?? "traces", value),
          };
  const top = scale.ticks.at(-1) ?? 1;
  return { ticks: scale.ticks, domain: [0, top], format: scale.format };
}

/**
 * The smallest step of a value axis. Counts and tokens are whole numbers: no half steps (0, 1,
 * 1, 2 would repeat labels). Scores are read to two decimals.
 */
function minStepOf(spec: ChartSpec): number {
  if (spec.unit === "count" || spec.unit === "tokens") {
    return 1;
  }
  return spec.metrics.includes("score_avg") ? 0.01 : 0;
}

/**
 * Percent ticks: `max` is a rate (0 to 1), so the ticks are nice percentages divided by 100.
 * The step is at least a tenth of a percent, so that every label is different.
 */
/** The largest value to fit: the sum of the stacked columns in a row, or the largest one. */
/** The smallest present value of the columns, or `Infinity` when there is none. */
function smallestValue(columns: readonly Column[], rows: readonly ChartRow[]): number {
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

function columnKey(metric: MetricName, group: string | undefined, multi: boolean): string {
  if (group) {
    return multi ? `${metric}\u001f${group}` : group;
  }
  return multi ? metric : "all";
}

function columnLabel(metric: MetricName, group: string | undefined, multi: boolean): string {
  const name = METRIC_LABEL[metric];
  if (!group) {
    return name;
  }
  return multi ? `${group} ${name}` : group;
}

function markOf(kind: ChartKind): "bar" | "line" | "area" {
  if (kind === "line" || kind === "scores") {
    return "line";
  }
  return kind === "area" ? "area" : "bar";
}

function dashOf(metric: MetricName): string | undefined {
  if (metric.endsWith("p99")) {
    return "1.5 3";
  }
  if (metric.endsWith("p95")) {
    return "4 3";
  }
  return undefined;
}

function chartConfig(columns: Column[]): ChartConfig {
  const config: ChartConfig = {};
  for (const column of columns) {
    config[column.key] = { label: column.label };
  }
  return config;
}

function describe(
  spec: ChartSpec,
  byMetric: Map<MetricName, MetricSeries[]>,
): { label: string; hasData: boolean } {
  const folded = spec.metrics.flatMap((metric) => foldGroups(byMetric.get(metric) ?? []));
  const hasData = folded.some((item) => item.points.some((point) => point.value !== undefined));
  if (!hasData) {
    return { label: `${spec.title}: no data in range`, hasData: false };
  }
  const clauses = summaryClauses(spec, byMetric, folded);
  if (clauses.length === 0) {
    return { label: `${spec.title}: no data in range`, hasData: false };
  }
  return { label: `${spec.title}: ${clauses.join(", ")} in range`, hasData: true };
}

function summaryClauses(
  spec: ChartSpec,
  byMetric: Map<MetricName, MetricSeries[]>,
  folded: readonly MetricSeries[],
): string[] {
  if (spec.summary === "counts") {
    const clauses: string[] = [];
    const traces = presentSum(byMetric.get("traces") ?? []);
    const errors = presentSum(byMetric.get("errors") ?? []);
    if (traces !== undefined) {
      clauses.push(countClause(traces, "trace", "traces"));
    }
    if (errors !== undefined) {
      clauses.push(countClause(errors, "error", "errors"));
    }
    return clauses;
  }
  if (spec.summary === "sum") {
    const metric = spec.metrics[0];
    const sum = presentSum(folded);
    return metric && sum !== undefined ? [formatMetric(metric, sum)] : [];
  }
  if (spec.summary === "parts") {
    const clauses: string[] = [];
    for (const metric of spec.metrics) {
      const sum = presentSum(byMetric.get(metric) ?? []);
      if (sum !== undefined) {
        clauses.push(`${METRIC_LABEL[metric]} ${formatMetric(metric, sum)}`);
      }
    }
    return clauses;
  }
  const clauses: string[] = [];
  for (const item of folded) {
    const last = lastPresent(item);
    if (last === undefined) {
      continue;
    }
    const text = formatMetric(item.metric, last);
    clauses.push(
      folded.length === 1 && !item.group
        ? text
        : `${item.group ?? METRIC_LABEL[item.metric]} ${text}`,
    );
  }
  return clauses;
}

function countClause(value: number, singular: string, plural: string): string {
  const rounded = Math.round(value);
  return `${formatCount(rounded)} ${rounded === 1 ? singular : plural}`;
}

function lastPresent(series: MetricSeries): number | undefined {
  let last: number | undefined;
  for (const point of series.points) {
    if (point.value !== undefined) {
      last = point.value;
    }
  }
  return last;
}
