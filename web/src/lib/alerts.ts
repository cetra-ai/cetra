import type { Schema } from "@/api/types";
import { seriesPlot } from "@/lib/dashboards/plot";
import type { WidgetQuery } from "@/lib/dashboards/types";
import { serializeChip } from "@/lib/dsl/dsl";
import { type ChartRow, epochMs } from "@/lib/metrics";

const KIND_LABELS: Record<Schema<"ChannelKind">, string> = {
  slack: "Slack",
  webhook: "Webhook",
  email: "Email",
};

/** "Slack", "Webhook" or "Email". */
export function channelKindLabel(kind: Schema<"ChannelKind">): string {
  return KIND_LABELS[kind];
}

/** The buckets of the alert list, in display order. */
export const ALERT_BUCKETS = ["firing", "no_data", "ok", "disabled"] as const;
export type AlertBucket = (typeof ALERT_BUCKETS)[number];

/** The bucket a rule is listed under: a disabled rule is Disabled whatever its state. */
export function alertBucket(rule: Pick<Schema<"AlertRule">, "enabled" | "state">): AlertBucket {
  return rule.enabled ? rule.state : "disabled";
}

type Condition = Schema<"AlertCondition">;

const COMPARATOR_SYMBOLS: Record<Schema<"Comparator">, string> = {
  gt: ">",
  gte: "≥",
  lt: "<",
  lte: "≤",
};

/**
 * Where the preview draws its "Threshold" label: on the side of the line where the data sits
 * while the rule is not firing, at the left end. A rule that fires above the threshold has its
 * label above the line (Recharts' `insideBottomLeft` of a zero-height line), and below for one
 * that fires under it.
 */
export function thresholdLabelPosition(
  comparator: Schema<"Comparator">,
): "insideBottomLeft" | "insideTopLeft" {
  return comparator === "gt" || comparator === "gte" ? "insideBottomLeft" : "insideTopLeft";
}

/** ">", "≥", "<", "≤" for gt, gte, lt, lte. */
export function comparatorSymbol(comparator: Schema<"Comparator">): string {
  return COMPARATOR_SYMBOLS[comparator];
}

function isPassRate(condition: Condition): boolean {
  return condition.metric === "score" && condition.aggregation === "pass_rate";
}

/** Metric label: "Error rate", "Cost", "p95 latency" (+ " of LLM spans" for llm_span), "Score <name>",
 *  "Pass rate of <name>" or "Issue occurrences". */
export function metricLabel(condition: Condition): string {
  switch (condition.metric) {
    case "error_rate":
      return "Error rate";
    case "cost":
      return "Cost";
    case "latency_p95":
      return condition.target === "llm_span" ? "p95 latency of LLM spans" : "p95 latency";
    case "score":
      return isPassRate(condition)
        ? `Pass rate of ${condition.score_name ?? ""}`
        : `Score ${condition.score_name ?? ""}`;
    case "issue_occurrences":
      return "Issue occurrences";
  }
}

/** A value in the metric's unit: error_rate and pass_rate as percent with 1 decimal ("12.5%"),
 *  cost as "$" with 2 decimals, latency as "820 ms" (0 decimals), score avg with 3 decimals,
 *  issue occurrences as a whole count. `undefined` → "—". */
export function formatAlertValue(condition: Condition, value: number | undefined): string {
  if (value === undefined) {
    return "—";
  }
  switch (condition.metric) {
    case "error_rate":
      return `${(value * 100).toFixed(1)}%`;
    case "cost":
      return `$${value.toFixed(2)}`;
    case "latency_p95":
      return `${Math.round(value)} ms`;
    case "score":
      return isPassRate(condition) ? `${(value * 100).toFixed(1)}%` : value.toFixed(3);
    case "issue_occurrences":
      return String(Math.round(value));
  }
}

/** "Error rate > 5.0% over 5m" — label, comparator symbol, threshold via formatAlertValue, window. */
export function conditionSummary(condition: Condition): string {
  return `${metricLabel(condition)} ${comparatorSymbol(condition.comparator)} ${formatAlertValue(condition, condition.threshold)} over ${condition.window}`;
}

type Metric = Schema<"AlertMetric">;
type Aggregation = Schema<"ScoreAggregation">;
type Comparator = Schema<"Comparator">;
type Window = Schema<"AlertWindow">;

/** The editable form. Numbers are strings while typed. Rates are typed in percent. */
export type RuleFormValues = {
  name: string;
  metric: Metric;
  window: Window;
  filter: string;
  comparator: Comparator;
  /** Percent for error_rate and pass_rate, USD for cost, ms for latency, raw for score avg,
   *  a count for issue_occurrences. */
  threshold: string;
  minCount: string;
  target: Schema<"LatencyTarget">;
  scoreName: string;
  aggregation: Aggregation;
  /** issue_occurrences only: one issue (`issueId`) or every issue of one source (`sourceId`). */
  issueScope: "issue" | "source";
  issueId: string;
  sourceId: string;
  channelIds: string[];
  enabled: boolean;
  /** Minutes, 5–10080. */
  cooldownMinutes: string;
  notifyWhileFiring: boolean;
};

export type IssueScope = RuleFormValues["issueScope"];

export const DEFAULT_RULE_FORM: RuleFormValues = {
  name: "",
  metric: "error_rate",
  window: "5m",
  filter: "",
  comparator: "gt",
  threshold: "5",
  minCount: "20",
  target: "trace",
  scoreName: "",
  aggregation: "avg",
  issueScope: "issue",
  issueId: "",
  sourceId: "",
  channelIds: [],
  enabled: true,
  cooldownMinutes: "60",
  notifyWhileFiring: false,
};

/** Whether the threshold is typed in percent. */
export function thresholdInPercent(metric: Metric, aggregation: Aggregation): boolean {
  return metric === "error_rate" || (metric === "score" && aggregation === "pass_rate");
}

/** The unit shown after the threshold input: "%", "USD", "ms" or "" (score avg). */
export function thresholdUnit(metric: Metric, aggregation: Aggregation): string {
  switch (metric) {
    case "error_rate":
      return "%";
    case "cost":
      return "USD";
    case "latency_p95":
      return "ms";
    case "score":
      return thresholdInPercent(metric, aggregation) ? "%" : "";
    case "issue_occurrences":
      return "occurrences";
  }
}

/** The values a metric starts with when it is chosen (the form's reset on a metric change). */
export function metricDefaults(
  metric: Metric,
  aggregation: Aggregation,
): { threshold: string; comparator: Comparator; minCount: string } {
  switch (metric) {
    case "error_rate":
      return { threshold: "5", comparator: "gt", minCount: "20" };
    case "cost":
      return { threshold: "100", comparator: "gt", minCount: "20" };
    case "latency_p95":
      return { threshold: "5000", comparator: "gt", minCount: "20" };
    case "score":
      return aggregation === "pass_rate"
        ? { threshold: "90", comparator: "lt", minCount: "10" }
        : { threshold: "0.5", comparator: "lt", minCount: "10" };
    case "issue_occurrences":
      return { threshold: "0", comparator: "gt", minCount: DEFAULT_RULE_FORM.minCount };
  }
}

/** Form values from a stored rule (edit). Percent thresholds are multiplied by 100 and printed
 *  with `String(Math.round(value * 1e6) / 1e6)`; `cooldownMinutes` is `String(Math.round(cooldown_s / 60))`.
 *  Fields the metric does not use take the defaults of DEFAULT_RULE_FORM (`target` "trace",
 *  `scoreName` "", `aggregation` "avg", `minCount` "20" for cost and issue_occurrences); `filter` is
 *  the stored filter or "". An issue_occurrences rule with `source_id` has the source scope, one
 *  with `issue_id` (or neither) the issue scope. */
export function formFromRule(rule: Schema<"AlertRule">): RuleFormValues {
  const condition = rule.condition;
  const aggregation = condition.aggregation ?? DEFAULT_RULE_FORM.aggregation;
  const threshold = thresholdInPercent(condition.metric, aggregation)
    ? String(Math.round(condition.threshold * 100 * 1e6) / 1e6)
    : String(condition.threshold);
  return {
    name: rule.name,
    metric: condition.metric,
    window: condition.window,
    filter: condition.filter ?? "",
    comparator: condition.comparator,
    threshold,
    minCount:
      condition.metric === "cost" || condition.metric === "issue_occurrences"
        ? DEFAULT_RULE_FORM.minCount
        : String(condition.min_count ?? DEFAULT_RULE_FORM.minCount),
    target: condition.target ?? DEFAULT_RULE_FORM.target,
    scoreName: condition.score_name ?? DEFAULT_RULE_FORM.scoreName,
    aggregation,
    issueScope: condition.source_id !== undefined ? "source" : "issue",
    issueId: condition.issue_id ?? DEFAULT_RULE_FORM.issueId,
    sourceId: condition.source_id ?? DEFAULT_RULE_FORM.sourceId,
    channelIds: [...rule.channel_ids],
    enabled: rule.enabled,
    cooldownMinutes: String(Math.round(rule.cooldown_s / 60)),
    notifyWhileFiring: rule.notify_while_firing,
  };
}

/** The threshold in the unit the API stores: a percent becomes a fraction. */
function storedThreshold(form: RuleFormValues, value: number): number {
  return thresholdInPercent(form.metric, form.aggregation)
    ? Number((value / 100).toPrecision(12))
    : value;
}

function buildCondition(form: RuleFormValues, threshold: number, minCount: number): Condition {
  const condition: Condition = {
    metric: form.metric,
    window: form.window,
    comparator: form.comparator,
    threshold,
  };
  const filter = form.filter.trim();
  if (filter && form.metric !== "issue_occurrences") {
    condition.filter = filter;
  }
  if (form.metric !== "cost" && form.metric !== "issue_occurrences") {
    condition.min_count = minCount;
  }
  if (form.metric === "latency_p95") {
    condition.target = form.target;
  }
  if (form.metric === "score") {
    condition.score_name = form.scoreName.trim();
    condition.aggregation = form.aggregation;
  }
  if (form.metric === "issue_occurrences") {
    if (form.issueScope === "issue") {
      condition.issue_id = form.issueId.trim();
    } else {
      condition.source_id = form.sourceId.trim();
    }
  }
  return condition;
}

/** The condition to send, or field errors keyed like the server's (`condition.threshold`,
 *  `condition.min_count`, `condition.score_name`, `condition.window`, `condition.issue_id`,
 *  `condition.source_id`, `cooldown_s`). Only the fields of the metric are set: `min_count` not for
 *  cost or issue_occurrences, `target` only for latency, `score_name` and `aggregation` only for
 *  score, `issue_id` or `source_id` (by scope) only for issue_occurrences. An empty filter is
 *  omitted; an issue_occurrences rule has no filter. */
export function conditionFromForm(
  form: RuleFormValues,
):
  | { ok: true; condition: Condition; cooldownS: number }
  | { ok: false; errors: Record<string, string> } {
  const errors: Record<string, string> = {};
  const raw = form.threshold.trim();
  const number = Number(raw);
  const percent = thresholdInPercent(form.metric, form.aggregation);
  let threshold = 0;
  if (raw === "" || !Number.isFinite(number)) {
    errors["condition.threshold"] = "Enter a number.";
  } else if (percent && (number < 0 || number > 100)) {
    errors["condition.threshold"] = "Enter a percentage from 0 to 100.";
  } else if ((form.metric === "cost" || form.metric === "issue_occurrences") && number < 0) {
    errors["condition.threshold"] = "Enter 0 or more.";
  } else if (form.metric === "latency_p95" && number <= 0) {
    errors["condition.threshold"] = "Enter a duration above 0.";
  } else {
    threshold = storedThreshold(form, number);
  }

  let minCount = 0;
  if (form.metric !== "cost" && form.metric !== "issue_occurrences") {
    const text = form.minCount.trim();
    const parsed = Number(text);
    if (text !== "" && Number.isInteger(parsed) && parsed >= 1 && parsed <= 1_000_000) {
      minCount = parsed;
    } else {
      errors["condition.min_count"] = "Enter a whole number from 1 to 1,000,000.";
    }
  }

  if (form.metric === "score" && form.scoreName.trim() === "") {
    errors["condition.score_name"] = "Enter a score name.";
  }

  if (form.metric === "issue_occurrences") {
    if (form.window === "1m" || form.window === "5m") {
      errors["condition.window"] = "Issue occurrences need a window of 15 minutes or more.";
    }
    if (form.issueScope === "issue" && form.issueId.trim() === "") {
      errors["condition.issue_id"] = "Choose an issue.";
    }
    if (form.issueScope === "source" && form.sourceId.trim() === "") {
      errors["condition.source_id"] = "Choose a source.";
    }
  }

  let cooldownS = 3600;
  if (form.notifyWhileFiring) {
    const text = form.cooldownMinutes.trim();
    const minutes = Number(text);
    if (text !== "" && Number.isInteger(minutes) && minutes >= 5 && minutes <= 10_080) {
      cooldownS = minutes * 60;
    } else {
      errors.cooldown_s = "Enter 5 to 10,080 minutes.";
    }
  }

  if (Object.keys(errors).length > 0) {
    return { ok: false, errors };
  }
  return { ok: true, condition: buildCondition(form, threshold, minCount), cooldownS };
}

/** A full condition for labels and axes, also while the form is invalid: an unparsable
 *  threshold becomes 0, a blank score name stays blank. Never sent. */
export function previewCondition(form: RuleFormValues): Condition {
  const raw = form.threshold.trim();
  const number = Number(raw);
  const threshold = raw === "" || !Number.isFinite(number) ? 0 : storedThreshold(form, number);
  const minCount = Number.parseInt(form.minCount, 10);
  return buildCondition(form, threshold, Number.isFinite(minCount) ? minCount : 0);
}

/** The condition-related form fields of `a` and `b` differ, compared as strings after trimming.
 *  Metric, window, filter, comparator and threshold always count. minCount counts for every
 *  metric but cost and issue_occurrences. target counts only for latency_p95; scoreName and
 *  aggregation only for score. For issue_occurrences the filter and minCount are ignored and the
 *  scope counts (the scope, then the issue or source id of that scope). A field the metric does not
 *  use is ignored, so switching metric and back is not a change. Used to decide whether a PATCH
 *  sends `condition`. */
export function conditionChanged(a: RuleFormValues, b: RuleFormValues): boolean {
  if (
    a.metric !== b.metric ||
    a.window !== b.window ||
    a.comparator !== b.comparator ||
    a.threshold.trim() !== b.threshold.trim()
  ) {
    return true;
  }
  if (a.metric === "issue_occurrences") {
    if (a.issueScope !== b.issueScope) {
      return true;
    }
    return a.issueScope === "issue"
      ? a.issueId.trim() !== b.issueId.trim()
      : a.sourceId.trim() !== b.sourceId.trim();
  }
  if (a.filter.trim() !== b.filter.trim()) {
    return true;
  }
  if (a.metric !== "cost" && a.minCount.trim() !== b.minCount.trim()) {
    return true;
  }
  if (a.metric === "latency_p95") {
    return a.target !== b.target;
  }
  if (a.metric === "score") {
    return a.scoreName.trim() !== b.scoreName.trim() || a.aggregation !== b.aggregation;
  }
  return false;
}

const DAY_MS = 24 * 60 * 60 * 1000;
const NS_PER_MS = 1_000_000;

/** D-K-13: the bucket equal to the window, and the preview range. */
export function previewParams(
  window: Window,
  nowMs: number,
): { bucket: Schema<"MetricsBucket">; from: string; to: string } {
  const plan: Record<Window, { bucket: Schema<"MetricsBucket">; spanMs: number }> = {
    "1m": { bucket: "1m", spanMs: DAY_MS },
    "5m": { bucket: "5m", spanMs: DAY_MS },
    "15m": { bucket: "15m", spanMs: DAY_MS },
    "1h": { bucket: "1h", spanMs: DAY_MS },
    "6h": { bucket: "6h", spanMs: 3 * DAY_MS },
    "24h": { bucket: "1d", spanMs: 14 * DAY_MS },
  };
  const { bucket, spanMs } = plan[window];
  return {
    bucket,
    from: new Date(nowMs - spanMs).toISOString(),
    to: new Date(nowMs).toISOString(),
  };
}

/**
 * The chart rows of a preview answer: one per bucket with a gap where the bucket has no value, and
 * `all` the value in the condition's unit.
 */
export function previewRows(
  query: WidgetQuery,
  answer: Schema<"WidgetQueryResponse"> | undefined,
  divisor: number,
): ChartRow[] {
  if (answer?.shape !== "series") {
    return [];
  }
  const plot = seriesPlot(query, answer);
  const key = plot.columns[0]?.key;
  if (key === undefined) {
    return [];
  }
  return plot.rows.map((row) => {
    const value = row[key];
    return { time: row.time, all: typeof value === "number" ? value / divisor : null };
  });
}

/** The widget query that draws a rule's preview and trend (the trace error rate, cost, p95 latency
 *  or score average), with the divisor that turns its value into the condition's unit. Latency of
 *  llm spans have no preview, and issue occurrences are drawn from `issuePreviewRows` instead:
 *  both are `undefined` here. Score measures draw the average of the named score. */
export function previewQuery(
  condition: Pick<RuleFormValues, "metric" | "target" | "scoreName">,
): { query: WidgetQuery; divisor: number } | undefined {
  switch (condition.metric) {
    case "error_rate":
      return {
        query: { source: "traces", measures: [{ agg: "rate", where: "status = 'error'" }] },
        divisor: 1,
      };
    case "cost":
      return {
        query: { source: "traces", measures: [{ agg: "sum", field: "cost_total" }] },
        divisor: 1,
      };
    case "latency_p95":
      return condition.target === "trace"
        ? {
            query: { source: "traces", measures: [{ agg: "p95", field: "duration_ns" }] },
            divisor: NS_PER_MS,
          }
        : undefined;
    case "score":
      return {
        query: {
          source: "scores",
          filter: serializeChip({
            kind: "compare",
            field: { column: "name" },
            op: "=",
            value: { kind: "string", value: condition.scoreName.trim() },
          }),
          measures: [{ agg: "avg", field: "value_num" }],
        },
        divisor: 1,
      };
    case "issue_occurrences":
      return undefined;
  }
}

/** The scope and window of an issue occurrence preview, or why there is none: the form has no
 *  issue or source yet, or a window under 15 minutes. `undefined` for the other metrics. */
export function issuePreviewScope(
  form: Pick<RuleFormValues, "metric" | "issueScope" | "issueId" | "sourceId" | "window">,
):
  | { kind: "ready"; scope: { issue_id: string } | { source_id: string }; window: Window }
  | { kind: "missing"; message: string }
  | undefined {
  if (form.metric !== "issue_occurrences") {
    return undefined;
  }
  if (form.window === "1m" || form.window === "5m") {
    return {
      kind: "missing",
      message: "Issue occurrences need a window of 15 minutes or more.",
    };
  }
  if (form.issueScope === "issue") {
    const issueId = form.issueId.trim();
    return issueId === ""
      ? { kind: "missing", message: "Choose an issue to see the preview." }
      : { kind: "ready", scope: { issue_id: issueId }, window: form.window };
  }
  const sourceId = form.sourceId.trim();
  return sourceId === ""
    ? { kind: "missing", message: "Choose an issue source to see the preview." }
    : { kind: "ready", scope: { source_id: sourceId }, window: form.window };
}

/** The chart rows of an issue occurrence preview: one per point, `all` the count. */
export function issuePreviewRows(answer: Schema<"IssueAlertPreview"> | undefined): ChartRow[] {
  if (answer === undefined) {
    return [];
  }
  return answer.points.flatMap((point) => {
    const time = epochMs(point.time);
    return time === undefined ? [] : [{ time, all: point.count }];
  });
}
