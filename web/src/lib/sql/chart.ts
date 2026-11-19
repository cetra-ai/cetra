import type { Schema } from "@/api/types";
import { isDateType, isNumericType, isTextType, isTimestampType } from "@/lib/sql/types";

export const CHART_MAX_POINTS = 1000;
export const CHART_MAX_SERIES = 5;
/** Key of the x value in each point; series keys are column names or pivot values. */
export const X_KEY = "__x";

export type ChartPoint = Record<string, string | number | null>;

type XKind = ChartSpec["x"]["kind"];

export type ChartSpec = {
  kind: "line" | "bar";
  /** The x column and how its values read. */
  x: { name: string; kind: "timestamp" | "date" | "text" | "number" };
  /** Series keys, in order. */
  series: string[];
  /** Set when the series are the values of a text column and `measure` is the one numeric column. */
  pivot?: { column: string; measure: string };
  data: ChartPoint[];
  /** True when rows or series were dropped. */
  clipped: boolean;
};

/** What a chart shows: chosen by the user, or the automatic choice from `autoChartConfig`. */
export type ChartConfig = {
  kind: "line" | "bar";
  /** The x column. */
  x: string;
  /** The y columns, in series order: numeric, safe names, at most CHART_MAX_SERIES. */
  y: string[];
  /**
   * A text column whose values become the series (one per value, at most CHART_MAX_SERIES).
   * Only applies with exactly one y. With several y it is ignored and the chart has one series
   * per y.
   */
  splitBy: string | null;
};

/** Keys that would not round-trip as plain data properties. */
const UNSAFE_KEYS = new Set(["__proto__", X_KEY]);

function numberOf(value: unknown): number | null {
  if (typeof value === "number" && Number.isFinite(value)) {
    return value;
  }
  if (typeof value === "string" && value.trim().length > 0 && Number.isFinite(Number(value))) {
    return Number(value);
  }
  return null;
}

/** How a column reads as x, or null when a chart cannot use it. */
function xKindOf(type: string): XKind | null {
  if (isTimestampType(type)) {
    return "timestamp";
  }
  if (isDateType(type)) {
    return "date";
  }
  if (isTextType(type)) {
    return "text";
  }
  if (isNumericType(type)) {
    return "number";
  }
  return null;
}

/** The x value of a row: a string for time and text, a number (or null) for number. */
function xValueOf(row: Record<string, unknown>, name: string, kind: XKind): string | number | null {
  const raw = row[name];
  return kind === "number" ? numberOf(raw) : String(raw ?? "");
}

/**
 * Sort key of a point's x: milliseconds for time, the value for number. Unparseable or missing
 * values sort last.
 */
function sortKey(point: ChartPoint, kind: XKind): number {
  const value = point[X_KEY];
  if (kind === "number") {
    return typeof value === "number" ? value : Number.POSITIVE_INFINITY;
  }
  const time = Date.parse(String(value));
  return Number.isNaN(time) ? Number.POSITIVE_INFINITY : time;
}

function sortPoints(data: ChartPoint[], kind: XKind): void {
  data.sort((left, right) => {
    const a = sortKey(left, kind);
    const b = sortKey(right, kind);
    return a === b ? 0 : a < b ? -1 : 1;
  });
}

/**
 * The columns a chart can use: x (timestamp, date, text or numeric), y (numeric, safe names),
 * and splitBy (text).
 */
export function chartColumns(result: Schema<"SqlQueryResult">): {
  x: string[];
  y: string[];
  splitBy: string[];
} {
  const x: string[] = [];
  const y: string[] = [];
  const splitBy: string[] = [];
  for (const column of result.columns) {
    if (xKindOf(column.type) !== null) {
      x.push(column.name);
    }
    if (isNumericType(column.type) && !UNSAFE_KEYS.has(column.name)) {
      y.push(column.name);
    }
    if (isTextType(column.type)) {
      splitBy.push(column.name);
    }
  }
  return { x, y, splitBy };
}

/**
 * The automatic choice. x: the first timestamp or date column (line chart), else the first text
 * column (bar chart). y: the numeric columns other than x, safe names, at most 5. Split by: a
 * line chart with exactly one numeric column and another text column pivots on the first such
 * column (spec 11). No x or no numeric column: null.
 */
export function autoChartConfig(result: Schema<"SqlQueryResult">): ChartConfig | null {
  const columns = result.columns;
  const time = columns.find((column) => isTimestampType(column.type) || isDateType(column.type));
  const text = columns.find((column) => isTextType(column.type));
  const xColumn = time ?? text;
  if (!xColumn) {
    return null;
  }
  const numericColumns = columns.filter(
    (column) => column.name !== xColumn.name && isNumericType(column.type),
  );
  const numeric = numericColumns.filter((column) => !UNSAFE_KEYS.has(column.name));
  const measure = numeric[0];
  if (!measure) {
    return null;
  }
  const kind = time ? "line" : "bar";
  // The pivot needs exactly one numeric column among all of them, not among the safe ones.
  const pivotColumn =
    kind === "line" && numericColumns.length === 1
      ? columns.find((column) => column.name !== xColumn.name && isTextType(column.type))
      : undefined;
  if (pivotColumn) {
    return { kind, x: xColumn.name, y: [measure.name], splitBy: pivotColumn.name };
  }
  return {
    kind,
    x: xColumn.name,
    y: numeric.slice(0, CHART_MAX_SERIES).map((column) => column.name),
    splitBy: null,
  };
}

/**
 * The chart for `config`, or null when `x` is not a column with a usable type, `y` is empty or
 * names a column that is not a safe numeric column other than x, or `splitBy` names a column
 * that is not a text column other than x.
 *
 * Points: the first 1,000 rows. A line chart is sorted by x (time, or number; text keeps row
 * order). With `splitBy` and exactly one y the chart pivots: one series per value, at most 5,
 * first seen first. Rows, pivot values or y beyond the limits set `clipped`.
 *
 * `clipped` also follows the result, as the automatic chart always did: it is set when the
 * result has an unsafe numeric column or more than 5 safe numeric columns besides x, even when
 * the config charts fewer of them.
 */
export function buildChart(
  result: Schema<"SqlQueryResult">,
  config: ChartConfig,
): ChartSpec | null {
  const columns = result.columns;
  const xColumn = columns.find((column) => column.name === config.x);
  const xKind = xColumn ? xKindOf(xColumn.type) : null;
  if (!xColumn || xKind === null || config.y.length === 0) {
    return null;
  }
  const numeric = columns.filter(
    (column) => column.name !== xColumn.name && isNumericType(column.type),
  );
  const chartable = numeric
    .filter((column) => !UNSAFE_KEYS.has(column.name))
    .map((column) => column.name);
  if (
    !config.y.every((name) => chartable.includes(name)) ||
    new Set(config.y).size !== config.y.length
  ) {
    return null;
  }
  const split =
    config.splitBy === null
      ? null
      : (columns.find(
          (column) =>
            column.name === config.splitBy &&
            column.name !== xColumn.name &&
            isTextType(column.type),
        ) ?? null);
  if (config.splitBy !== null && split === null) {
    return null;
  }

  const rows = (result.rows as Record<string, unknown>[]).slice(0, CHART_MAX_POINTS);
  let clipped =
    result.rows.length > CHART_MAX_POINTS ||
    numeric.length > chartable.length ||
    chartable.length > CHART_MAX_SERIES ||
    config.y.length > CHART_MAX_SERIES;
  const x = { name: xColumn.name, kind: xKind };
  const sortable = config.kind === "line" && xKind !== "text";
  const [measure] = config.y;

  if (split && measure !== undefined && config.y.length === 1) {
    const keys: string[] = [];
    const byX = new Map<string, ChartPoint>();
    for (const row of rows) {
      const xValue = xValueOf(row, xColumn.name, xKind);
      const raw = row[split.name];
      const key = raw === null || raw === undefined ? "NULL" : String(raw);
      if (UNSAFE_KEYS.has(key)) {
        clipped = true;
        continue;
      }
      if (!keys.includes(key)) {
        if (keys.length >= CHART_MAX_SERIES) {
          clipped = true;
          continue;
        }
        keys.push(key);
      }
      const groupKey = String(xValue);
      const point: ChartPoint = byX.get(groupKey) ?? { [X_KEY]: xValue };
      point[key] = numberOf(row[measure]);
      byX.set(groupKey, point);
    }
    const data = [...byX.values()];
    if (sortable) {
      sortPoints(data, xKind);
    }
    return {
      kind: config.kind,
      x,
      series: keys,
      pivot: { column: split.name, measure },
      data,
      clipped,
    };
  }

  const series = config.y.slice(0, CHART_MAX_SERIES);
  const data = rows.map((row) => {
    const point: ChartPoint = { [X_KEY]: xValueOf(row, xColumn.name, xKind) };
    for (const name of series) {
      point[name] = numberOf(row[name]);
    }
    return point;
  });
  if (sortable) {
    sortPoints(data, xKind);
  }
  return { kind: config.kind, x, series, data, clipped };
}

/** The automatic chart: `buildChart` over `autoChartConfig`. */
export function chartSpec(result: Schema<"SqlQueryResult">): ChartSpec | null {
  const config = autoChartConfig(result);
  return config ? buildChart(result, config) : null;
}

/** The chart's accessible name. */
export function chartLabel(spec: ChartSpec): string {
  const kind = spec.kind === "line" ? "Line chart" : "Bar chart";
  if (spec.pivot) {
    return `${kind} of ${spec.pivot.measure} by ${spec.x.name}, one series per ${spec.pivot.column}`;
  }
  return `${kind} of ${spec.series.join(", ")} by ${spec.x.name}`;
}
