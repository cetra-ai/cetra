import { type HttpHandler, HttpResponse, http } from "msw";
import type { Schema } from "@/api/types";
import { type Chip, type ChipValue, type Field, parseChip, splitConjuncts } from "@/lib/dsl/dsl";
import {
  bucketWidth,
  type Dataset,
  percentile,
  rfc3339,
  scoreNumber,
  visibleTraces,
} from "@/mocks/generated/dataset";

const PROJECT = "/api/v1/projects/:project";
const MS_PER_NS = 1_000_000;
/** The most groups a dashboard's split keeps (spec 05 § Dashboard queries). */
const DASHBOARD_LIMIT = 20;

type Body = Schema<"WidgetQueryRequest">;
type Query = Schema<"WidgetQueryBody">;
type Measure = Schema<"MeasureBody">;
type Source = Schema<"WidgetSourceBody">;
type Trace = Schema<"TraceSummary">;
type Cell = string | number | boolean | null | string[];
/** One source row. `__time` is the start of its trace, in epoch milliseconds. */
type Row = Record<string, Cell>;
type Predicate = (row: Row) => boolean;

/**
 * The columns each source's rows carry. A filter names only these: anything else (a map key, a
 * score function, a column the mock does not model) is refused with `invalid_filter`.
 */
const COLUMNS: Record<Source, ReadonlySet<string>> = {
  traces: new Set([
    "trace_id",
    "name",
    "status",
    "environment",
    "release",
    "user_id",
    "session_id",
    "models",
    "tags",
    "duration_ns",
    "span_count",
    "error_count",
    "llm_count",
    "input_tokens",
    "output_tokens",
    "cache_read_tokens",
    "cache_write_tokens",
    "reasoning_tokens",
    "total_tokens",
    "cost_total",
  ]),
  spans: new Set([
    "trace_id",
    "span_kind",
    "model",
    "name",
    "status",
    "environment",
    "release",
    "user_id",
    "session_id",
    "tags",
    "duration_ns",
    "ttft_ns",
    "input_tokens",
    "output_tokens",
    "total_tokens",
    "cache_read_tokens",
    "cache_write_tokens",
    "reasoning_tokens",
    "cost_total",
    "cost_input",
    "cost_output",
    "service_name",
    "provider",
    "operation",
  ]),
  scores: new Set(["name", "value_num", "data_type", "target"]),
};

/** List columns: a row counts under each of its values. */
const LIST_COLUMNS: ReadonlySet<string> = new Set(["models", "tags"]);

/** A filter or a field the mock cannot evaluate. The path names where it was written. */
class Unsupported extends Error {
  constructor(readonly path: string) {
    super("unsupported");
  }
}

/**
 * The widget query endpoint (spec 05 § Dashboard queries) over the generated dataset, so that the
 * dashboard runs in the large mock. A traces widget reads the visible traces, a spans widget one
 * row per (trace, model) and a scores widget one row per score. Spans carry the trace's tokens and
 * cost split equally across its models, and its duration divided by the number of models; there
 * is no time to first token, and no cache tokens. So the TTFT widget is empty here, and the token
 * and LLM latency widgets show these approximations.
 */
export function widgetQueryHandler(dataset: Dataset): HttpHandler {
  return http.post(`${PROJECT}/dashboards/query`, async ({ request }) => {
    const body = (await request.json()) as Body;
    try {
      return HttpResponse.json(answer(dataset, body));
    } catch (error) {
      if (error instanceof Unsupported) {
        return unsupported(error.path);
      }
      throw error;
    }
  });
}

function unsupported(path: string): Response {
  return HttpResponse.json(
    {
      type: "about:blank",
      title: "Bad Request",
      status: 400,
      code: "invalid_filter",
      detail: "The mock cannot evaluate this filter.",
      errors: [
        {
          field: path,
          message: "The mock cannot evaluate this filter.",
          position: { line: 1, column: 1 },
        },
      ],
    },
    { status: 400, headers: { "Content-Type": "application/problem+json" } },
  );
}

function answer(dataset: Dataset, body: Body) {
  const query = body.query;
  const from = Date.parse(body.from);
  const to = Date.parse(body.to);
  const previousFrom = from - (to - from);
  const start = body.compare ? previousFrom : from;
  // A filter that names `environment` lifts the evaluation exclusion (spec 05 § Default exclusions).
  const texts = [body.filter, query.filter, ...query.measures.map((measure) => measure.where)];
  const names = texts.some((text) => text !== undefined && /\benvironment\b/i.test(text));
  const visible = visibleTraces(
    dataset.traces,
    body.include_eval === true || names,
    body.include_experiments === true,
  );
  const dashboardFilter = body.filter ? compile(body.filter, COLUMNS.traces, "filter") : undefined;
  const traces = visible.filter((trace) => {
    const startsAt = Date.parse(trace.start_time);
    if (startsAt < start || startsAt >= to) {
      return false;
    }
    return dashboardFilter === undefined || dashboardFilter(traceRow(trace));
  });
  const rows = sourceRows(query.source, traces).filter((row) => {
    const time = row.__time as number;
    return time >= start && time < to;
  });
  const widgetFilter = query.filter
    ? compile(query.filter, COLUMNS[query.source], "query.filter")
    : undefined;
  const scoped = widgetFilter ? rows.filter(widgetFilter) : rows;
  const measures = query.measures.map((measure, index) => ({
    measure,
    where: measure.where
      ? compile(measure.where, COLUMNS[query.source], `query.measures[${index}].where`)
      : undefined,
  }));
  for (const { measure } of measures) {
    checkField(measure, query.source);
  }
  const current = scoped.filter((row) => (row.__time as number) >= from);
  const previous = scoped.filter((row) => (row.__time as number) < from);
  const groups = groupsOf(body, query, current);

  if (body.shape === "total") {
    return {
      shape: "total" as const,
      rows: groups.map((group) => ({
        ...(group.group === undefined ? {} : { group: group.group }),
        other: group.other,
        values: measures.map(({ measure, where }) =>
          aggregate(measure, where, current.filter(group.match)),
        ),
        ...(body.compare
          ? {
              previous: measures.map(({ measure, where }) =>
                aggregate(measure, where, previous.filter(group.match)),
              ),
            }
          : {}),
      })),
    };
  }
  const width = bucketWidth(body.bucket, from, to);
  const series = groups.flatMap((group) =>
    measures.map(({ measure, where }, index) => {
      const inGroup = current.filter(group.match);
      const byBucket = new Map<number, Row[]>();
      for (const row of inGroup) {
        const start = bucketStart(row.__time as number, width);
        byBucket.set(start, [...(byBucket.get(start) ?? []), row]);
      }
      const points = [...byBucket.keys()]
        .sort((left, right) => left - right)
        .map((start) => {
          const bucketRows = byBucket.get(start) ?? [];
          const value = aggregate(measure, where, bucketRows);
          return {
            time: rfc3339(start),
            rows: bucketRows.length,
            ...(value === undefined ? {} : { value }),
          };
        });
      return {
        measure: index,
        ...(group.group === undefined ? {} : { group: group.group }),
        other: group.other,
        points,
      };
    }),
  );
  return { shape: "series" as const, bucket_ns: width * MS_PER_NS, series };
}

/** The traces row of one trace. */
function traceRow(trace: Trace): Row {
  return {
    trace_id: trace.trace_id,
    name: trace.name ?? null,
    status: trace.status ?? null,
    environment: trace.environment ?? null,
    release: trace.release ?? null,
    user_id: trace.user_id ?? null,
    session_id: trace.session_id ?? null,
    models: trace.models,
    tags: trace.tags,
    duration_ns: trace.duration_ns,
    span_count: trace.span_count,
    error_count: trace.error_count,
    llm_count: trace.llm_count,
    input_tokens: trace.input_tokens ?? null,
    output_tokens: trace.output_tokens ?? null,
    cache_read_tokens: trace.cache_read_tokens ?? null,
    cache_write_tokens: trace.cache_write_tokens ?? null,
    reasoning_tokens: trace.reasoning_tokens ?? null,
    total_tokens: trace.total_tokens ?? null,
    cost_total: trace.cost_total ?? null,
    __time: Date.parse(trace.start_time),
  };
}

/** One spans row per model of the trace, with the trace's tokens and cost split equally. */
function spanRows(trace: Trace): Row[] {
  const count = trace.models.length;
  const share = (value: number | undefined): number | null =>
    value === undefined ? null : value / count;
  return trace.models.map((model) => ({
    trace_id: trace.trace_id,
    span_kind: "llm",
    model,
    name: null,
    status: null,
    environment: trace.environment ?? null,
    release: trace.release ?? null,
    user_id: trace.user_id ?? null,
    session_id: trace.session_id ?? null,
    tags: trace.tags,
    duration_ns: trace.duration_ns / count,
    ttft_ns: null,
    input_tokens: share(trace.input_tokens),
    output_tokens: share(trace.output_tokens),
    total_tokens: share(trace.total_tokens),
    cache_read_tokens: null,
    cache_write_tokens: null,
    reasoning_tokens: null,
    cost_total: share(trace.cost_total),
    cost_input: null,
    cost_output: null,
    service_name: null,
    provider: null,
    operation: null,
    __time: Date.parse(trace.start_time),
  }));
}

/** One scores row per score of the trace, at the trace's start. */
function scoreRows(trace: Trace): Row[] {
  return trace.scores.map((score) => ({
    name: score.name,
    value_num: scoreNumber(score) ?? null,
    data_type: score.value.data_type,
    target: "trace",
    __time: Date.parse(trace.start_time),
  }));
}

function sourceRows(source: Source, traces: readonly Trace[]): Row[] {
  if (source === "traces") {
    return traces.map(traceRow);
  }
  if (source === "spans") {
    return traces.flatMap(spanRows);
  }
  return traces.flatMap(scoreRows);
}

/** Compiles a filter (spec 05 DSL) into a predicate over rows. Anything it does not model is refused. */
function compile(text: string, columns: ReadonlySet<string>, path: string): Predicate {
  const predicates = splitConjuncts(text).map((conjunct) =>
    predicateOf(parseChip(conjunct.text), columns, path),
  );
  return (row) => predicates.every((predicate) => predicate(row));
}

/**
 * A predicate over traces for `filter`, or undefined when the mock cannot evaluate it. The trace
 * list uses it (M13), with the same columns as a traces widget.
 */
export function tracePredicate(filter: string): ((trace: Trace) => boolean) | undefined {
  try {
    const predicate = compile(filter, COLUMNS.traces, "filter");
    return (trace) => predicate(traceRow(trace));
  } catch (error) {
    if (error instanceof Unsupported) {
      return undefined;
    }
    throw error;
  }
}

function predicateOf(chip: Chip, columns: ReadonlySet<string>, path: string): Predicate {
  switch (chip.kind) {
    case "compare": {
      const column = plainColumn(chip.field, columns, path);
      return comparison(column, chip.op, chip.value, path);
    }
    case "null": {
      const column = plainColumn(chip.field, columns, path);
      return (row) => {
        const isNull = row[column] === null || row[column] === undefined;
        return chip.negated ? !isNull : isNull;
      };
    }
    case "in": {
      const column = plainColumn(chip.field, columns, path);
      const values = chip.values.map((value) => scalar(value, path));
      return (row) => {
        const cell = row[column];
        if (cell === null || cell === undefined) {
          return false;
        }
        const hit = Array.isArray(cell)
          ? values.some((value) => cell.includes(String(value)))
          : values.some((value) => cell === value);
        return chip.negated ? !hit : hit;
      };
    }
    case "contains": {
      const column = plainColumn(chip.field, columns, path);
      if (!LIST_COLUMNS.has(column)) {
        throw new Unsupported(path);
      }
      return (row) => {
        const cell = row[column];
        return Array.isArray(cell) && chip.values.every((value) => cell.includes(value));
      };
    }
    default:
      throw new Unsupported(path);
  }
}

function plainColumn(field: Field, columns: ReadonlySet<string>, path: string): string {
  if (!("column" in field) || "key" in field || !columns.has(field.column)) {
    throw new Unsupported(path);
  }
  return field.column;
}

function comparison(column: string, op: string, value: ChipValue, path: string): Predicate {
  const target = scalar(value, path);
  const tests: Record<string, (left: string | number, right: string | number) => boolean> = {
    "=": (left, right) => left === right,
    "!=": (left, right) => left !== right,
    "<>": (left, right) => left !== right,
    "<": (left, right) => left < right,
    "<=": (left, right) => left <= right,
    ">": (left, right) => left > right,
    ">=": (left, right) => left >= right,
  };
  const test = tests[op];
  if (!test) {
    throw new Unsupported(path);
  }
  return (row) => {
    const cell = row[column];
    if (cell === null || cell === undefined || Array.isArray(cell) || typeof cell === "boolean") {
      return false;
    }
    if (typeof cell !== typeof target) {
      return false;
    }
    return test(cell, target);
  };
}

function scalar(value: ChipValue, path: string): string | number {
  if (value.kind === "boolean") {
    throw new Unsupported(path);
  }
  return value.value;
}

/** A measure's field, when it has one, must be a column of its source. */
function checkField(measure: Measure, source: Source): void {
  if (measure.field !== undefined && !COLUMNS[source].has(measure.field)) {
    throw new Unsupported("query.measures.field");
  }
}

/** The values of a row under a field: each list item, or the one value, or none when null. */
function valuesOf(row: Row, field: string): (string | number)[] {
  const cell = row[field];
  if (Array.isArray(cell)) {
    return cell;
  }
  if (cell === null || cell === undefined || typeof cell === "boolean") {
    return [];
  }
  return [cell];
}

function numberOf(row: Row, field: string | undefined): number | undefined {
  if (field === undefined) {
    return undefined;
  }
  const cell = row[field];
  return typeof cell === "number" ? cell : undefined;
}

/**
 * One measure over rows, as the spec aggregates it: `count` and `rate` over all rows, the others
 * over the numeric values. `undefined` when there is nothing to aggregate (a gap).
 */
function aggregate(
  measure: Measure,
  where: Predicate | undefined,
  rows: readonly Row[],
): number | undefined {
  const matched = where ? rows.filter(where) : rows;
  switch (measure.agg) {
    case "count":
      return matched.length;
    case "count_distinct": {
      const field = measure.field ?? "";
      return new Set(matched.flatMap((row) => valuesOf(row, field))).size;
    }
    case "rate":
      return rows.length === 0 ? undefined : matched.length / rows.length;
    default: {
      const numbers = matched
        .map((row) => numberOf(row, measure.field))
        .filter((value): value is number => value !== undefined);
      if (numbers.length === 0) {
        return undefined;
      }
      return numeric(measure.agg, numbers);
    }
  }
}

function numeric(agg: Measure["agg"], numbers: readonly number[]): number | undefined {
  const sum = numbers.reduce((total, value) => total + value, 0);
  switch (agg) {
    case "sum":
      return sum;
    case "avg":
      return sum / numbers.length;
    case "min":
      return Math.min(...numbers);
    case "max":
      return Math.max(...numbers);
    case "p50":
      return percentile(numbers, 50);
    case "p75":
      return percentile(numbers, 75);
    case "p90":
      return percentile(numbers, 90);
    case "p95":
      return percentile(numbers, 95);
    case "p99":
      return percentile(numbers, 99);
    default:
      return undefined;
  }
}

/** A group of rows: `match` tells whether a row belongs to it. */
type GroupSpec = { group?: string; other: boolean; match: (row: Row) => boolean };

/**
 * The groups of a query. Without a split there is one group of every row. A breakdown keeps its
 * `limit` values (default 5) and `other` (default true) is one more group of the rows having any
 * other value. The dashboard's split (`group_by`) is a breakdown with `limit: 20, other: false`,
 * and is ignored when the query has its own breakdown, and for scores. The kept values are the
 * `limit` values with the largest first measure over the rows, nulls last, then by value
 * ascending (spec 05 § Dashboard queries).
 */
function groupsOf(body: Body, query: Query, rows: readonly Row[]): GroupSpec[] {
  const breakdown = query.breakdown;
  const dashboardSplit =
    body.group_by && query.source !== "scores"
      ? groupColumn(body.group_by, query.source)
      : undefined;
  const field = breakdown?.field ?? dashboardSplit;
  if (field === undefined) {
    return [{ other: false, match: () => true }];
  }
  if (breakdown && !COLUMNS[query.source].has(field)) {
    throw new Unsupported("query.breakdown");
  }
  const limit = breakdown ? (breakdown.limit ?? 5) : DASHBOARD_LIMIT;
  const other = breakdown ? (breakdown.other ?? true) : false;
  const values = [...new Set(rows.flatMap((row) => valuesOf(row, field).map(String)))];
  const first = query.measures[0];
  const firstWhere = first?.where
    ? compile(first.where, COLUMNS[query.source], "query.measures[0].where")
    : undefined;
  const ranked = values
    .map((value) => {
      const inValue = rows.filter((row) =>
        valuesOf(row, field).some((item) => String(item) === value),
      );
      const total = first ? aggregate(first, firstWhere, inValue) : undefined;
      return { value, total: total ?? Number.NEGATIVE_INFINITY };
    })
    .sort((left, right) => right.total - left.total || left.value.localeCompare(right.value));
  const kept = ranked.slice(0, limit).map((item) => item.value);
  const keptSet = new Set(kept);
  const specs: GroupSpec[] = kept.map((value) => ({
    group: value,
    other: false,
    match: (row: Row) => valuesOf(row, field).some((item) => String(item) === value),
  }));
  if (other) {
    specs.push({
      other: true,
      match: (row: Row) => valuesOf(row, field).some((item) => !keptSet.has(String(item))),
    });
  }
  return specs;
}

/** The column a dashboard split reads: a trace counts under each of its `models`. */
function groupColumn(groupBy: string, source: Source): string {
  if (groupBy === "model" && source === "traces") {
    return "models";
  }
  return groupBy;
}

function bucketStart(time: number, width: number): number {
  return Math.floor(time / width) * width;
}
