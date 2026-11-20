import type { Agg, WidgetSource } from "./types";

/**
 * The fields of each widget source, as spec 05 § Dashboard queries lists them. A bare map column
 * (`metadata`, `attributes`, `resource_attributes`) is not a field: only `column['key']` is.
 */

/** Longest map key, in characters (spec 05). */
const MAX_MAP_KEY = 256;

/** Columns counted as tokens or counts: they format as plain numbers. */
const COUNT_COLUMNS = [
  "input_tokens",
  "output_tokens",
  "cache_read_tokens",
  "cache_write_tokens",
  "reasoning_tokens",
  "total_tokens",
  "span_count",
  "error_count",
  "llm_count",
];

/** Identifier columns that `count_distinct` accepts besides the breakdown fields. */
const IDENTITY_FIELDS = ["trace_id", "session_id", "user_id"];

/** List fields: `count_distinct` refuses them. */
const LIST_FIELDS = ["tags", "models"];

type SourceFields = {
  numeric: readonly string[];
  breakdown: readonly string[];
  maps: readonly string[];
};

const SOURCES: Record<WidgetSource, SourceFields> = {
  traces: {
    numeric: [
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
    ],
    breakdown: [
      "name",
      "status",
      "environment",
      "release",
      "user_id",
      "session_id",
      "tags",
      "models",
    ],
    maps: ["metadata"],
  },
  spans: {
    numeric: [
      "duration_ns",
      "ttft_ns",
      "input_tokens",
      "output_tokens",
      "cache_read_tokens",
      "cache_write_tokens",
      "reasoning_tokens",
      "total_tokens",
      "cost_input",
      "cost_output",
      "cost_total",
    ],
    breakdown: [
      "name",
      "span_kind",
      "status",
      "service_name",
      "environment",
      "release",
      "provider",
      "model",
      "operation",
      "user_id",
      "tags",
    ],
    maps: ["metadata", "attributes", "resource_attributes"],
  },
  scores: {
    numeric: ["value_num"],
    breakdown: ["name", "source", "data_type", "target", "value_str"],
    maps: ["metadata"],
  },
};

/** Numeric fields the numeric aggregates (`sum`, `avg`, `min`, `max`, percentiles) take. */
export function numericFields(source: WidgetSource): readonly string[] {
  return SOURCES[source].numeric;
}

/** Plain breakdown fields. Map keys are typed as `column['key']`; see `isMapKeyField`. */
export function breakdownFields(source: WidgetSource): readonly string[] {
  return SOURCES[source].breakdown;
}

/** Fields `count_distinct` takes, besides any map key: the breakdown fields without the lists, and the identifiers. */
export function countDistinctFields(source: WidgetSource): readonly string[] {
  const { breakdown } = SOURCES[source];
  const plain = breakdown.filter((field) => !LIST_FIELDS.includes(field));
  return [...plain, ...IDENTITY_FIELDS.filter((field) => !plain.includes(field))];
}

/** The map columns of a source: `column['key']` is allowed on these. */
export function mapColumns(source: WidgetSource): readonly string[] {
  return SOURCES[source].maps;
}

/** The fields the editor offers for an aggregate. `count` and `rate` take none. */
export function fieldsForAgg(source: WidgetSource, agg: Agg): readonly string[] {
  switch (agg) {
    case "count":
    case "rate":
      return [];
    case "count_distinct":
      return countDistinctFields(source);
    default:
      return numericFields(source);
  }
}

/**
 * Whether `text` is `column['key']` for a map column of `source`. The key has 1 to 256
 * characters and no quote.
 */
export function isMapKeyField(source: WidgetSource, text: string): boolean {
  const match = /^([a-z_]+)\['([^']+)'\]$/.exec(text);
  if (!match) {
    return false;
  }
  const column = match[1] ?? "";
  const keyLength = Array.from(match[2] ?? "").length;
  return mapColumns(source).includes(column) && keyLength >= 1 && keyLength <= MAX_MAP_KEY;
}

/** Kind of a numeric field, for formatting: durations, costs, scores, counts or plain numbers. */
export function fieldKind(field: string): "duration" | "cost" | "count" | "score" | "number" {
  if (field === "duration_ns" || field === "ttft_ns") {
    return "duration";
  }
  if (field.startsWith("cost_")) {
    return "cost";
  }
  if (field === "value_num") {
    return "score";
  }
  if (COUNT_COLUMNS.includes(field)) {
    return "count";
  }
  return "number";
}
