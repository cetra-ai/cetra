import type { Schema } from "@/api/types";

export type AssistantMessage = Schema<"AssistantMessage">;
export type AssistantPart = Schema<"AssistantPart">;
export type AssistantStatus = Schema<"AssistantStatus">;
export type DraftKind = Schema<"AssistantDraftKind">;

/** The turn route's SSE events (spec 09 § Assistant). The OpenAPI does not describe them. */
export type TurnEvent =
  | { event: "start"; data: { turn_id: number; trace_id: string } }
  | { event: "text"; data: { delta: string } }
  | { event: "tool_call"; data: { id: string; name: string; arguments: unknown } }
  | {
      event: "tool_result";
      data: {
        id: string;
        ok: boolean;
        summary: { size_bytes: number; rows?: number; error_code?: string };
      };
    }
  | { event: "draft"; data: { id: string; kind: DraftKind; draft: unknown } }
  | {
      event: "done";
      data: {
        finish_reason: string;
        usage: { input: number; output: number };
        cost_usd: number | null;
        steps: number;
      };
    }
  | { event: "error"; data: { class: "retryable" | "pause" | "permanent"; message: string } };

type Json = Record<string, unknown>;

function isObject(value: unknown): value is Json {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function isString(value: unknown): value is string {
  return typeof value === "string";
}

function isNumber(value: unknown): value is number {
  return typeof value === "number" && Number.isFinite(value);
}

function isBoolean(value: unknown): value is boolean {
  return typeof value === "boolean";
}

function oneOf<T extends string>(value: unknown, values: readonly T[]): value is T {
  return typeof value === "string" && (values as readonly string[]).includes(value);
}

/**
 * An optional field: `undefined` when absent, the value when `check` passes, `null` when
 * present with the wrong type. Callers treat `null` as a wrong shape.
 */
function optional<T>(
  object: Json,
  key: string,
  check: (value: unknown) => value is T,
): T | undefined | null {
  const value = object[key];
  if (value === undefined) {
    return undefined;
  }
  return check(value) ? value : null;
}

/** Parses one SSE block's name and JSON data into a TurnEvent, or `null` (unknown name, bad JSON, wrong shape, `ping`). */
export function parseTurnEvent(name: string, data: string): TurnEvent | null {
  let value: unknown;
  try {
    value = JSON.parse(data);
  } catch {
    return null;
  }
  if (!isObject(value)) {
    return null;
  }
  switch (name) {
    case "start": {
      if (!isNumber(value.turn_id) || !isString(value.trace_id)) {
        return null;
      }
      return { event: "start", data: { turn_id: value.turn_id, trace_id: value.trace_id } };
    }
    case "text": {
      if (!isString(value.delta)) {
        return null;
      }
      return { event: "text", data: { delta: value.delta } };
    }
    case "tool_call": {
      if (!isString(value.id) || !isString(value.name) || !("arguments" in value)) {
        return null;
      }
      return {
        event: "tool_call",
        data: { id: value.id, name: value.name, arguments: value.arguments },
      };
    }
    case "tool_result": {
      const summary = value.summary;
      if (!isString(value.id) || !isBoolean(value.ok) || !isObject(summary)) {
        return null;
      }
      if (!isNumber(summary.size_bytes)) {
        return null;
      }
      const parsed: { size_bytes: number; rows?: number; error_code?: string } = {
        size_bytes: summary.size_bytes,
      };
      const rows = optional(summary, "rows", isNumber);
      if (rows === null) {
        return null;
      }
      if (rows !== undefined) {
        parsed.rows = rows;
      }
      const errorCode = optional(summary, "error_code", isString);
      if (errorCode === null) {
        return null;
      }
      if (errorCode !== undefined) {
        parsed.error_code = errorCode;
      }
      return { event: "tool_result", data: { id: value.id, ok: value.ok, summary: parsed } };
    }
    case "draft": {
      if (!isString(value.id) || !oneOf(value.kind, DRAFT_KINDS) || !("draft" in value)) {
        return null;
      }
      return { event: "draft", data: { id: value.id, kind: value.kind, draft: value.draft } };
    }
    case "done": {
      const usage = value.usage;
      if (
        !isString(value.finish_reason) ||
        !isObject(usage) ||
        !isNumber(usage.input) ||
        !isNumber(usage.output) ||
        !(value.cost_usd === null || isNumber(value.cost_usd)) ||
        !isNumber(value.steps)
      ) {
        return null;
      }
      return {
        event: "done",
        data: {
          finish_reason: value.finish_reason,
          usage: { input: usage.input, output: usage.output },
          cost_usd: value.cost_usd,
          steps: value.steps,
        },
      };
    }
    case "error": {
      if (!oneOf(value.class, ERROR_CLASSES) || !isString(value.message)) {
        return null;
      }
      return { event: "error", data: { class: value.class, message: value.message } };
    }
    default:
      return null;
  }
}

const DRAFT_KINDS: readonly DraftKind[] = [
  "filter",
  "sql",
  "dashboard",
  "evaluator",
  "online_rule",
];
const ERROR_CLASSES = ["retryable", "pause", "permanent"] as const;
const FILTER_VIEWS = ["traces", "spans", "sessions", "users"] as const;
const RANGES = ["15m", "1h", "6h", "24h", "7d", "30d"] as const;
const SCORE_DATA_TYPES = ["numeric", "boolean", "categorical", "text"] as const;

export type FilterDraft = {
  view: "traces" | "spans" | "sessions" | "users";
  filter?: string;
  search?: string;
  range?: "15m" | "1h" | "6h" | "24h" | "7d" | "30d";
  from?: string;
  to?: string;
  include_eval: boolean;
  include_experiments: boolean;
};
export type SqlDraft = {
  query: string;
  params: Record<string, unknown>;
  range?: string;
  from?: string;
  to?: string;
  columns: { name: string; type: string }[];
};
export type DashboardDraft = {
  dashboard_id?: string;
  version?: number;
  name: string;
  description?: string;
  definition: Schema<"Definition">;
};
export type EvaluatorDraft = {
  name: string;
  kind: "llm_judge" | "check";
  config: Record<string, unknown>;
  output_score: Schema<"OutputScoreBody">;
};
export type RuleDraft = {
  name: string;
  evaluator_id: string;
  target: "trace" | "span";
  filter?: string;
  variable_mapping: Record<string, string>;
  sample_rate: number;
  budget_usd?: number;
  budget_period?: "day" | "month";
  enabled: boolean;
};
export type Draft =
  | { kind: "filter"; draft: FilterDraft }
  | { kind: "sql"; draft: SqlDraft }
  | { kind: "dashboard"; draft: DashboardDraft }
  | { kind: "evaluator"; draft: EvaluatorDraft }
  | { kind: "online_rule"; draft: RuleDraft };

/** Runtime check of a draft value (the generated type is `Record<string, never>`). `null` when the shape is wrong. */
export function parseDraft(kind: DraftKind, value: unknown): Draft | null {
  if (!isObject(value)) {
    return null;
  }
  switch (kind) {
    case "filter":
      return parseFilter(value);
    case "sql":
      return parseSql(value);
    case "dashboard":
      return parseDashboard(value);
    case "evaluator":
      return parseEvaluator(value);
    case "online_rule":
      return parseRule(value);
  }
}

function parseFilter(value: Json): Draft | null {
  const filter = optional(value, "filter", isString);
  const search = optional(value, "search", isString);
  const range = optional(value, "range", (v): v is NonNullable<FilterDraft["range"]> =>
    oneOf(v, RANGES),
  );
  const from = optional(value, "from", isString);
  const to = optional(value, "to", isString);
  if (
    !oneOf(value.view, FILTER_VIEWS) ||
    filter === null ||
    search === null ||
    range === null ||
    from === null ||
    to === null ||
    !isBoolean(value.include_eval) ||
    !isBoolean(value.include_experiments)
  ) {
    return null;
  }
  const draft: FilterDraft = {
    view: value.view,
    include_eval: value.include_eval,
    include_experiments: value.include_experiments,
  };
  if (filter !== undefined) {
    draft.filter = filter;
  }
  if (search !== undefined) {
    draft.search = search;
  }
  if (range !== undefined) {
    draft.range = range;
  }
  if (from !== undefined) {
    draft.from = from;
  }
  if (to !== undefined) {
    draft.to = to;
  }
  return { kind: "filter", draft };
}

function parseSql(value: Json): Draft | null {
  const range = optional(value, "range", isString);
  const from = optional(value, "from", isString);
  const to = optional(value, "to", isString);
  const columns = value.columns;
  if (
    !isString(value.query) ||
    !isObject(value.params) ||
    range === null ||
    from === null ||
    to === null ||
    !Array.isArray(columns) ||
    !columns.every((column) => isObject(column) && isString(column.name) && isString(column.type))
  ) {
    return null;
  }
  const draft: SqlDraft = {
    query: value.query,
    params: value.params,
    columns: columns.map((column: Json) => ({
      name: column.name as string,
      type: column.type as string,
    })),
  };
  if (range !== undefined) {
    draft.range = range;
  }
  if (from !== undefined) {
    draft.from = from;
  }
  if (to !== undefined) {
    draft.to = to;
  }
  return { kind: "sql", draft };
}

function parseDashboard(value: Json): Draft | null {
  const dashboardId = optional(value, "dashboard_id", isString);
  const version = optional(value, "version", isNumber);
  const description = optional(value, "description", isString);
  const definition = value.definition;
  if (
    !isString(value.name) ||
    dashboardId === null ||
    version === null ||
    description === null ||
    !isObject(definition) ||
    !Array.isArray(definition.widgets)
  ) {
    return null;
  }
  const draft: DashboardDraft = {
    name: value.name,
    // Checked only to be an object with a `widgets` array; the dashboard page validates the rest.
    definition: definition as DashboardDraft["definition"],
  };
  if (dashboardId !== undefined) {
    draft.dashboard_id = dashboardId;
  }
  if (version !== undefined) {
    draft.version = version;
  }
  if (description !== undefined) {
    draft.description = description;
  }
  return { kind: "dashboard", draft };
}

function parseEvaluator(value: Json): Draft | null {
  const outputScore = value.output_score;
  if (
    !isString(value.name) ||
    !oneOf(value.kind, ["llm_judge", "check"] as const) ||
    !isObject(value.config) ||
    !isObject(outputScore) ||
    !isString(outputScore.name) ||
    !oneOf(outputScore.data_type, SCORE_DATA_TYPES)
  ) {
    return null;
  }
  const categories = optional(
    outputScore,
    "categories",
    (v): v is string[] => Array.isArray(v) && v.every(isString),
  );
  if (categories === null) {
    return null;
  }
  const scoreBody: Schema<"OutputScoreBody"> = {
    name: outputScore.name,
    data_type: outputScore.data_type,
  };
  if (categories !== undefined) {
    scoreBody.categories = categories;
  }
  return {
    kind: "evaluator",
    draft: { name: value.name, kind: value.kind, config: value.config, output_score: scoreBody },
  };
}

function parseRule(value: Json): Draft | null {
  const filter = optional(value, "filter", isString);
  const budgetUsd = optional(value, "budget_usd", isNumber);
  const budgetPeriod = optional(value, "budget_period", (v): v is "day" | "month" =>
    oneOf(v, ["day", "month"]),
  );
  const mapping = value.variable_mapping;
  if (
    !isString(value.name) ||
    !isString(value.evaluator_id) ||
    !oneOf(value.target, ["trace", "span"] as const) ||
    filter === null ||
    !isObject(mapping) ||
    !Object.values(mapping).every(isString) ||
    !isNumber(value.sample_rate) ||
    budgetUsd === null ||
    budgetPeriod === null ||
    !isBoolean(value.enabled)
  ) {
    return null;
  }
  const draft: RuleDraft = {
    name: value.name,
    evaluator_id: value.evaluator_id,
    target: value.target,
    variable_mapping: mapping as Record<string, string>,
    sample_rate: value.sample_rate,
    enabled: value.enabled,
  };
  if (filter !== undefined) {
    draft.filter = filter;
  }
  if (budgetUsd !== undefined) {
    draft.budget_usd = budgetUsd;
  }
  if (budgetPeriod !== undefined) {
    draft.budget_period = budgetPeriod;
  }
  return { kind: "online_rule", draft };
}
