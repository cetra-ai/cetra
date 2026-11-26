import type { Schema } from "@/api/types";
import { serializeChip } from "@/lib/dsl/dsl";
import { type ChartRow, epochMs, groupColorIndex, PALETTE } from "@/lib/metrics";
import { type MeasureKind, measureKind, measureLabel } from "./format";
import type { Measure, WidgetQuery, WidgetSource } from "./types";

type SeriesAnswer = Schema<"WidgetSeriesResponse">;
type TotalAnswer = Schema<"WidgetTotalResponse">;
type Groups = Schema<"MetricsGroupBy">;

/** One drawn series: a measure, or a measure of one breakdown group. */
export type PlotColumn = {
  key: string;
  label: string;
  kind: MeasureKind;
  color: string;
  /**
   * The breakdown group of the series: `undefined` without a split, `null` for the group of every
   * other value (it has no filter of its own).
   */
  group: string | null | undefined;
};

/** The time chart of a series answer: one row per bucket, one column per series. */
export type SeriesPlot = {
  columns: PlotColumn[];
  rows: ChartRow[];
  bucketMs: number;
};

/** One row of a total answer, one value per measure. */
export type TableRow = {
  group: string | null | undefined;
  other: boolean;
  values: (number | null)[];
  previous?: (number | null)[];
  /** The row's share of the first measure's total, from 0 to 1. Only with `share`. */
  share?: number;
};

/** A total answer laid out as a table: one column per measure. */
export type TablePlot = {
  columns: { label: string; kind: MeasureKind }[];
  rows: TableRow[];
};

/** Above this many buckets the rows are not filled between the answer's points. */
const MAX_BUCKETS = 10_000;
const NS_PER_MS = 1_000_000;
const ERROR_WHERE = "status = 'error'";
/** Breakdown fields whose groups are models: they keep the model colours. */
const MODEL_FIELDS: readonly string[] = ["model", "models"];
/** List fields: a row counts under each of its values, so a group is a `contains` clause. */
const LIST_FIELDS: readonly string[] = ["models", "tags"];

/** The field a series is split by: the query's breakdown, else the dashboard's group. */
export function splitField(query: WidgetQuery, groupBy: Groups | undefined): string | undefined {
  return query.breakdown?.field ?? groupBy;
}

/** Aggregates whose value over no rows is 0: a bucket without rows plots at 0 for them. */
const ADDITIVE: ReadonlySet<string> = new Set(["count", "count_distinct", "sum"]);

/**
 * The series of a time chart. The colour of an error measure (`where` is exactly
 * `status = 'error'`) is the error colour. A model keeps the colour of its place in `ranking`
 * (the models by cost); without a ranking, the order of the answer's groups. Other series take
 * the palette in column order.
 */
export function seriesPlot(
  query: WidgetQuery,
  answer: SeriesAnswer,
  options: { ranking?: readonly string[]; groupBy?: Groups } = {},
): SeriesPlot {
  const groups = answerGroups(answer);
  const colors = groupColorIndex(options.ranking ?? groups, groups);
  const field = splitField(query, options.groupBy);
  const byModel = field !== undefined && MODEL_FIELDS.includes(field);
  const columns: PlotColumn[] = [];
  const pointMaps = new Map<string, Map<number, { value?: number; rows: number }>>();
  const additive = new Set<string>();
  for (const series of answer.series) {
    const measure = query.measures[series.measure];
    if (!measure) {
      continue;
    }
    const key = columnKey(series.measure, series.group, series.other);
    if (ADDITIVE.has(measure.agg)) {
      additive.add(key);
    }
    const group = series.other ? null : series.group;
    const color = colorOf(measure, group, byModel, colors, columns.length);
    columns.push({
      key,
      label: columnLabel(measure, group, query.measures.length === 1),
      kind: measureKind(measure),
      color,
      group,
    });
    const byTime = new Map<number, { value?: number; rows: number }>();
    for (const point of series.points) {
      const time = epochMs(point.time);
      if (time !== undefined) {
        byTime.set(time, { value: point.value, rows: point.rows });
      }
    }
    pointMaps.set(key, byTime);
  }
  const bucketMs = answer.bucket_ns / NS_PER_MS;
  const rows = [...timesOf(answer, bucketMs)]
    .sort((left, right) => left - right)
    .map((time) => {
      const row: ChartRow = { time };
      for (const column of columns) {
        const point = pointMaps.get(column.key)?.get(time);
        // A bucket without rows has no point (spec 05): a count or sum over no rows is 0, so it is drawn at 0
        // rather than as a gap, and a stacked chart stays whole. Other aggregates have no value there.
        const empty = point === undefined && additive.has(column.key);
        row[column.key] = empty ? 0 : (point?.value ?? null);
        row[`${column.key}__rows`] = empty ? 0 : (point?.rows ?? null);
      }
      return row;
    });
  return { columns, rows, bucketMs };
}

/** The table of a total answer, with each row's share of the first measure when `share` is set. */
export function totalTable(query: WidgetQuery, answer: TotalAnswer, share: boolean): TablePlot {
  const columns = query.measures.map((measure) => ({
    label: measureLabel(measure),
    kind: measureKind(measure),
  }));
  const total = answer.rows.reduce((sum, row) => sum + (row.values[0] ?? 0), 0);
  const rows: TableRow[] = answer.rows.map((row) => {
    const first = row.values[0] ?? 0;
    return {
      group: row.other ? null : row.group,
      other: row.other,
      values: row.values,
      ...(row.previous ? { previous: row.previous } : {}),
      ...(share ? { share: total > 0 ? first / total : 0 } : {}),
    };
  });
  return { columns, rows };
}

/**
 * The filter clause that keeps the rows of one group: `contains` for a list field, `=` otherwise.
 * On traces, a `model` split is the `models` list (a trace counts under each of its models).
 */
export function groupClause(source: WidgetSource, field: string, value: string): string {
  const column = field === "model" && source === "traces" ? "models" : field;
  if (LIST_FIELDS.includes(column)) {
    return serializeChip({ kind: "contains", field: { column }, values: [value] });
  }
  return serializeChip({
    kind: "compare",
    field: { column },
    op: "=",
    value: { kind: "string", value },
  });
}

/** Whether a measure is an error count: its `where` is exactly `status = 'error'`. */
export function isErrorMeasure(measure: Measure): boolean {
  return (
    measure.where !== undefined &&
    measure.where.trim().replace(/\s+/g, " ").toLowerCase() === ERROR_WHERE
  );
}

function answerGroups(answer: SeriesAnswer): string[] {
  const groups: string[] = [];
  for (const series of answer.series) {
    if (series.group !== undefined && !groups.includes(series.group)) {
      groups.push(series.group);
    }
  }
  return groups;
}

function columnKey(measure: number, group: string | undefined, other: boolean): string {
  if (other) {
    return `m${measure}:\u001fother`;
  }
  return group === undefined ? `m${measure}` : `m${measure}:${group}`;
}

/**
 * The label of a series. With one measure the group alone labels it (the measure is in the title);
 * with several, "<measure> · <group>".
 */
function columnLabel(
  measure: Measure,
  group: string | null | undefined,
  singleMeasure: boolean,
): string {
  const name = measureLabel(measure);
  if (group === undefined) {
    return name;
  }
  const groupName = group === null ? "Other" : group;
  return singleMeasure ? groupName : `${name} · ${groupName}`;
}

function colorOf(
  measure: Measure,
  group: string | null | undefined,
  byModel: boolean,
  colors: ReadonlyMap<string, number>,
  position: number,
): string {
  if (isErrorMeasure(measure)) {
    return "var(--chart-error)";
  }
  if (group === null) {
    return "var(--muted-foreground)";
  }
  if (group !== undefined && byModel) {
    return PALETTE[(colors.get(group) ?? 0) % PALETTE.length] ?? PALETTE[0];
  }
  return PALETTE[position % PALETTE.length] ?? PALETTE[0];
}

/**
 * Every bucket start from the first to the last point, in steps of the bucket width, so that a
 * bucket without a point is a gap rather than missing from the axis.
 */
function timesOf(answer: SeriesAnswer, bucketMs: number): Set<number> {
  const times = new Set<number>();
  for (const series of answer.series) {
    for (const point of series.points) {
      const time = epochMs(point.time);
      if (time !== undefined) {
        times.add(time);
      }
    }
  }
  const ordered = [...times].sort((left, right) => left - right);
  const start = ordered[0];
  const end = ordered.at(-1);
  if (start !== undefined && end !== undefined && bucketMs > 0) {
    const count = Math.round((end - start) / bucketMs);
    if (count > 0 && count <= MAX_BUCKETS) {
      for (let index = 0; index <= count; index += 1) {
        times.add(start + index * bucketMs);
      }
    }
  }
  return times;
}
