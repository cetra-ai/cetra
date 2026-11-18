import { useQuery } from "@tanstack/react-query";
import { cn } from "cn";
import type { ReactNode } from "react";
import { widgetQuery } from "@/api/dashboards";
import { problemMessage } from "@/api/problem";
import type { Schema } from "@/api/types";
import type { KpiTone } from "@/components/charts/kpi-tiles";
import { Button } from "@/components/ui/button";
import { Skeleton } from "@/components/ui/skeleton";
import { type ChartBucket, chooseBucket } from "@/lib/chart-scale";
import { formatValue, measureKind } from "@/lib/dashboards/format";
import {
  groupClause,
  isErrorMeasure,
  type PlotColumn,
  seriesPlot,
  splitField,
  totalTable,
} from "@/lib/dashboards/plot";
import type { Chart, Measure, Widget as WidgetModel, WidgetQuery } from "@/lib/dashboards/types";
import { bucketRange, groupColorIndex, PALETTE } from "@/lib/metrics";
import { WidgetBarH } from "./widget-bar-h";
import { WidgetNumber } from "./widget-number";
import { WidgetPlot } from "./widget-plot";
import { WidgetTable } from "./widget-table";

type Groups = Schema<"MetricsGroupBy">;

/** What the dashboard applies to every widget, and the colours of its models. */
export type WidgetContext = {
  org: string;
  project: string;
  /** The range, RFC 3339: the start is inclusive, the end exclusive. */
  from: string;
  to: string;
  /** The dashboard's filter, a traces expression. */
  filter?: string;
  /** The dashboard's split. */
  groupBy?: Groups;
  includeEval: boolean;
  includeExperiments: boolean;
  /** The models by cost, most expensive first. Undefined until it loads. */
  ranking?: readonly string[];
};

/** The range of a clicked bucket and the clause of its group, for the list it opens. */
export type BucketTarget = { from: string; to: string; clause?: string };

/** The class of a widget's title bar: the drag handle of the grid in edit mode. */
export const DRAG_HANDLE = "widget-drag-handle";

/**
 * One widget of a dashboard: a card with a time chart, a horizontal bar chart, a number, or a
 * table, or a heading. A chart fetches its answer and shows a skeleton while it loads, the error
 * with a retry when the query fails, and the reason when there is no data. `actions` sits in the
 * title bar, at its end.
 */
export function Widget({
  widget,
  context,
  actions,
  onBucket,
  onZoom,
  onModel,
}: {
  widget: WidgetModel;
  context: WidgetContext;
  /** Controls of the title bar, such as a widget's menu. */
  actions?: ReactNode;
  /** A bucket was opened. Only traces and spans widgets have one. */
  onBucket?: (target: BucketTarget) => void;
  onZoom?: (fromMs: number, toMs: number) => void;
  onModel?: (model: string) => void;
}) {
  if (widget.kind === "heading") {
    return (
      <TitleBar actions={actions}>
        <h2 className="min-w-0 flex-1 text-heading font-medium">{widget.title}</h2>
      </TitleBar>
    );
  }
  if (!widget.chart || !widget.query) {
    return (
      <TitleBar actions={actions}>
        <p className="min-w-0 flex-1 text-body text-muted-foreground">{widget.title}: no query.</p>
      </TitleBar>
    );
  }
  return (
    <ChartWidget
      title={widget.title}
      chart={widget.chart}
      query={widget.query}
      context={context}
      actions={actions}
      onBucket={onBucket}
      onZoom={onZoom}
      onModel={onModel}
    />
  );
}

/**
 * A widget's title bar: its title, then its actions. It drags the widget wherever the grid is
 * movable (edit mode, or a member's dashboard outside it).
 */
function TitleBar({ actions, children }: { actions?: ReactNode; children: ReactNode }) {
  return (
    <div className={cn(DRAG_HANDLE, "flex min-w-0 items-center gap-2")}>
      {children}
      {actions}
    </div>
  );
}

function ChartWidget({
  title,
  chart,
  query,
  context,
  actions,
  onBucket,
  onZoom,
  onModel,
}: {
  title: string;
  chart: Chart;
  query: WidgetQuery;
  context: WidgetContext;
  actions?: ReactNode;
  onBucket?: (target: BucketTarget) => void;
  onZoom?: (fromMs: number, toMs: number) => void;
  onModel?: (model: string) => void;
}) {
  const { org, project } = context;
  const totalShape = chart.type === "bar_h" || chart.type === "table" || chart.type === "number";
  const isNumber = chart.type === "number";
  const answer = useQuery(
    widgetQuery(org, project, bodyOf(context, totalShape ? "total" : "series", query, isNumber)),
  );
  // A number's sparkline is the series of the same query.
  const sparkline = useQuery({
    ...widgetQuery(org, project, bodyOf(context, "series", query, isNumber)),
    enabled: isNumber,
  });
  const data = answer.data;
  const card = (children: ReactNode, busy = false) => (
    <section
      aria-label={title}
      aria-busy={busy || undefined}
      className="flex min-h-[220px] min-w-0 flex-col overflow-hidden rounded-md border p-3 md:h-full md:min-h-0"
    >
      <TitleBar actions={actions}>
        <h3 className="min-w-0 flex-1 text-body font-medium">{title}</h3>
      </TitleBar>
      {children}
    </section>
  );
  if (answer.isError && !data) {
    return card(
      <div className="mt-2 flex flex-col items-start gap-2">
        <p role="alert" className="text-destructive text-body">
          {problemMessage(answer.error, "The query failed.")}
        </p>
        <Button type="button" variant="outline" size="sm" onClick={() => void answer.refetch()}>
          Retry
        </Button>
      </div>,
    );
  }
  if (!data) {
    return card(<Skeleton className="mt-2 flex-1" />, true);
  }
  const empty = <p className="mt-2 text-muted-foreground text-body">{emptyReason(query)}</p>;
  if (data.shape === "series") {
    return card(
      <SeriesBody
        title={title}
        chart={chart}
        query={query}
        answer={data}
        context={context}
        onBucket={onBucket}
        onZoom={onZoom}
        empty={empty}
      />,
    );
  }
  if (chart.type === "number") {
    const measure = query.measures[0];
    const value = data.rows[0]?.values[0] ?? null;
    const sparkValues = sparklineOf(query, sparkline.data);
    if (value === null && !sparkValues?.some((item) => item !== null)) {
      return card(empty);
    }
    return card(
      <WidgetNumber
        label={`${title}: ${formatValue(measure ? measureKind(measure) : "number", value)}`}
        kind={measure ? measureKind(measure) : "number"}
        value={value}
        previous={data.rows[0]?.previous?.[0] ?? null}
        tone={numberTone(measure)}
        sparkline={sparkValues}
      />,
    );
  }
  const table = totalTable(query, data, chart.share ?? false);
  if (table.rows.every((row) => row.values.every((value) => value === null))) {
    return card(empty);
  }
  if (chart.type === "bar_h") {
    const groups = table.rows.flatMap((row) => (row.group ? [row.group] : []));
    const colors = groupColorIndex(context.ranking ?? groups, groups);
    const byModel = isModelField(splitField(query, context.groupBy));
    const first = query.measures[0];
    return card(
      <WidgetBarH
        plot={table}
        colorOf={(group, other) => {
          if (first && isErrorMeasure(first)) return "var(--chart-error)";
          if (other) return "var(--muted-foreground)";
          if (!group || !byModel) return PALETTE[0];
          return PALETTE[(colors.get(group) ?? 0) % PALETTE.length] ?? PALETTE[0];
        }}
      />,
    );
  }
  return card(
    <WidgetTable
      plot={table}
      breakdownField={query.breakdown?.field}
      onModel={
        onModel && (query.source === "traces" || query.source === "spans") ? onModel : undefined
      }
    />,
  );
}

function SeriesBody({
  title,
  chart,
  query,
  answer,
  context,
  onBucket,
  onZoom,
  empty,
}: {
  title: string;
  chart: Chart;
  query: WidgetQuery;
  answer: Schema<"WidgetSeriesResponse">;
  context: WidgetContext;
  onBucket?: (target: BucketTarget) => void;
  onZoom?: (fromMs: number, toMs: number) => void;
  empty: ReactNode;
}) {
  const plot = seriesPlot(query, answer, {
    ranking: context.ranking,
    groupBy: context.groupBy,
  });
  if (plot.rows.length === 0 || plot.columns.length === 0) {
    return empty;
  }
  // Scores have no bucket click and no zoom: only traces and spans lists are opened.
  const clickable = query.source !== "scores";
  const field = splitField(query, context.groupBy);
  const open =
    onBucket && clickable
      ? (timeMs: number, group: string | null | undefined) => {
          // The other group has no filter of its own, so its bucket is not opened.
          if (group === null) return;
          const range = bucketRange(timeMs, answer.bucket_ns);
          onBucket({
            ...range,
            ...(group !== undefined && field !== undefined
              ? { clause: groupClause(query.source, field, group) }
              : {}),
          });
        }
      : undefined;
  const zoom = clickable ? onZoom : undefined;
  if (chart.type === "line" && chart.split) {
    const groups = sortedGroups(plot.columns);
    const shown = groups.slice(0, SPLIT_CHARTS);
    // The mini charts share the card's height in equal rows, up to SPLIT_COLUMNS to a row.
    const gridColumns = Math.min(shown.length, SPLIT_COLUMNS);
    const gridRows = Math.ceil(shown.length / gridColumns);
    return (
      <div className="mt-2 flex min-h-0 flex-1 flex-col gap-1">
        <div
          className="grid min-h-0 flex-1 gap-3"
          style={{
            gridTemplateColumns: `repeat(${gridColumns}, minmax(0, 1fr))`,
            gridTemplateRows: `repeat(${gridRows}, minmax(0, 1fr))`,
          }}
        >
          {shown.map((group) => {
            const members = plot.columns.filter((column) => column.group === group);
            const name = group === null ? "Other" : (group ?? "");
            return (
              <div key={name} className="flex min-h-0 min-w-0 flex-col">
                <span className="truncate text-caption font-medium">{name}</span>
                <WidgetPlot
                  title={`${title} · ${name}`}
                  kind="line"
                  stacked={false}
                  columns={members}
                  rows={plot.rows}
                  bucketNs={answer.bucket_ns}
                  onBucket={open}
                  onZoom={zoom}
                />
              </div>
            );
          })}
        </div>
        {groups.length > shown.length ? (
          <p className="text-caption text-muted-foreground">+{groups.length - shown.length} more</p>
        ) : null}
      </div>
    );
  }
  return (
    <WidgetPlot
      title={title}
      kind={chart.type === "area" ? "area" : chart.type === "bar" ? "bar" : "line"}
      stacked={chart.stacked ?? false}
      columns={plot.columns}
      rows={plot.rows}
      bucketNs={answer.bucket_ns}
      fadeBelow={chart.type === "bar" ? chart.fade_below : undefined}
      onBucket={open}
      onZoom={zoom}
    />
  );
}

/**
 * The groups of a split, sorted by name; the other group (`null`) comes last. The split charts
 * and their "+N more" line follow this order.
 */
function sortedGroups(columns: readonly PlotColumn[]): (string | null | undefined)[] {
  return [...new Set(columns.map((column) => column.group))].sort((left, right) => {
    if (left === null) return right === null ? 0 : 1;
    if (right === null) return -1;
    return (left ?? "").localeCompare(right ?? "");
  });
}

/** Split charts shown in a widget; more groups than this are a "+N more" line. */
const SPLIT_CHARTS = 6;
/** Split charts side by side at most; more groups wrap to a new row. */
const SPLIT_COLUMNS = 3;

/** The bucket width for a range: the smallest that gives at most 60 buckets. */
function bucketOf(context: WidgetContext): ChartBucket {
  return chooseBucket(Date.parse(context.from), Date.parse(context.to));
}

/**
 * The request of a widget. A number ignores the dashboard's split: it is the total, with the
 * previous period, and its sparkline is the series of the same query without a split.
 */
function bodyOf(
  context: WidgetContext,
  shape: "series" | "total",
  query: WidgetQuery,
  number: boolean,
): Schema<"WidgetQueryRequest"> {
  const base = {
    from: context.from,
    to: context.to,
    shape,
    query,
    include_eval: context.includeEval,
    include_experiments: context.includeExperiments,
    ...(context.filter ? { filter: context.filter } : {}),
    ...(context.groupBy && !number ? { group_by: context.groupBy } : {}),
  } satisfies Partial<Schema<"WidgetQueryRequest">>;
  if (shape === "series") {
    return { ...base, bucket: bucketOf(context) };
  }
  return number ? { ...base, compare: true } : base;
}

/** A number's tone: a rise of an error rate, a duration or a cost is bad news. */
function numberTone(measure: Measure | undefined): KpiTone {
  if (!measure) {
    return "neutral";
  }
  const kind = measureKind(measure);
  if (kind === "rate") {
    return isErrorMeasure(measure) ? "up-bad" : "neutral";
  }
  return kind === "duration" || kind === "cost" ? "up-bad" : "neutral";
}

/** The values of a number's sparkline: the first measure, one per bucket. */
function sparklineOf(
  query: WidgetQuery,
  answer: Schema<"WidgetQueryResponse"> | undefined,
): (number | null)[] | undefined {
  if (answer?.shape !== "series") {
    return undefined;
  }
  const plot = seriesPlot(query, answer);
  const key = plot.columns[0]?.key;
  return key === undefined ? undefined : plot.rows.map((row) => row[key] ?? null);
}

function isModelField(field: string | undefined): boolean {
  return field === "model" || field === "models";
}

/** The reason a widget shows when its answer has no data. */
function emptyReason(query: WidgetQuery): string {
  const source = query.source;
  return query.filter
    ? `No ${source} in range match this widget's filter.`
    : `No ${source} in range.`;
}
