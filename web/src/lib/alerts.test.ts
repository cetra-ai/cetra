import { expect, it } from "vitest";
import type { Schema } from "@/api/types";
import rulesDocument from "../../../fixtures/api/static/alert-rules.json";
import {
  channelKindLabel,
  conditionChanged,
  conditionFromForm,
  conditionSummary,
  DEFAULT_RULE_FORM,
  formatAlertValue,
  formFromRule,
  issuePreviewRows,
  issuePreviewScope,
  metricDefaults,
  metricLabel,
  previewCondition,
  previewParams,
  previewQuery,
  previewRows,
  type RuleFormValues,
  thresholdInPercent,
  thresholdUnit,
} from "./alerts";
import { parseNewRuleSearch } from "./search/alerts";

type Condition = Schema<"AlertCondition">;

const HIGH_ERROR_RATE: Condition = {
  metric: "error_rate",
  window: "5m",
  filter: "environment = 'production'",
  comparator: "gt",
  threshold: 0.05,
  min_count: 20,
};
const LOW_HELPFULNESS: Condition = {
  metric: "score",
  window: "1h",
  comparator: "lt",
  threshold: 0.7,
  min_count: 10,
  score_name: "helpfulness",
  aggregation: "avg",
};
const DAILY_COST: Condition = { metric: "cost", window: "24h", comparator: "gt", threshold: 50 };
const LLM_LATENCY: Condition = {
  metric: "latency_p95",
  window: "15m",
  target: "llm_span",
  comparator: "gte",
  threshold: 8000,
};
const PASS_RATE: Condition = {
  metric: "score",
  window: "1h",
  comparator: "lt",
  threshold: 0.9,
  score_name: "ok",
  aggregation: "pass_rate",
};

it("labels channel kinds", () => {
  expect(channelKindLabel("slack")).toBe("Slack");
  expect(channelKindLabel("webhook")).toBe("Webhook");
  expect(channelKindLabel("email")).toBe("Email");
});

it("formats a value in its metric's unit", () => {
  expect(formatAlertValue(HIGH_ERROR_RATE, 0.125)).toBe("12.5%");
  expect(formatAlertValue(LOW_HELPFULNESS, 0.7)).toBe("0.700");
  expect(formatAlertValue(DAILY_COST, 1.5)).toBe("$1.50");
  expect(formatAlertValue(LLM_LATENCY, 820.4)).toBe("820 ms");
  expect(formatAlertValue(PASS_RATE, 0.9)).toBe("90.0%");
});

it("shows a missing value as a dash", () => {
  expect(formatAlertValue(HIGH_ERROR_RATE, undefined)).toBe("—");
});

it("summarises an error rate condition", () => {
  expect(conditionSummary(HIGH_ERROR_RATE)).toBe("Error rate > 5.0% over 5m");
});

it("summarises a score condition", () => {
  expect(conditionSummary(LOW_HELPFULNESS)).toBe("Score helpfulness < 0.700 over 1h");
});

it("summarises a cost condition", () => {
  expect(conditionSummary(DAILY_COST)).toBe("Cost > $50.00 over 24h");
});

it("summarises an LLM span latency condition", () => {
  expect(conditionSummary(LLM_LATENCY)).toBe("p95 latency of LLM spans ≥ 8000 ms over 15m");
});

it("summarises a pass rate condition", () => {
  expect(conditionSummary(PASS_RATE)).toBe("Pass rate of ok < 90.0% over 1h");
});

const rules = rulesDocument.data as Schema<"AlertRule">[];

function formWith(patch: Partial<RuleFormValues>): RuleFormValues {
  return { ...DEFAULT_RULE_FORM, ...patch };
}

function conditionOf(form: RuleFormValues): Condition {
  const result = conditionFromForm(form);
  if (!result.ok) {
    throw new Error(`unexpected errors: ${JSON.stringify(result.errors)}`);
  }
  return result.condition;
}

function errorsOf(form: RuleFormValues): Record<string, string> {
  const result = conditionFromForm(form);
  if (result.ok) {
    throw new Error("expected errors");
  }
  return result.errors;
}

it("knows which metrics are typed in percent and their units", () => {
  expect(thresholdInPercent("error_rate", "avg")).toBe(true);
  expect(thresholdInPercent("score", "pass_rate")).toBe(true);
  expect(thresholdInPercent("score", "avg")).toBe(false);
  expect(thresholdInPercent("cost", "avg")).toBe(false);
  expect(thresholdUnit("error_rate", "avg")).toBe("%");
  expect(thresholdUnit("score", "pass_rate")).toBe("%");
  expect(thresholdUnit("cost", "avg")).toBe("USD");
  expect(thresholdUnit("latency_p95", "avg")).toBe("ms");
  expect(thresholdUnit("score", "avg")).toBe("");
});

it("builds the default error rate condition exactly", () => {
  const result = conditionFromForm(DEFAULT_RULE_FORM);
  expect(result.ok).toBe(true);
  if (!result.ok) {
    return;
  }
  expect(JSON.stringify(result.condition)).toBe(
    '{"metric":"error_rate","window":"5m","comparator":"gt","threshold":0.05,"min_count":20}',
  );
  expect(result.cooldownS).toBe(3600);
});

it("converts a typed percentage to a fraction without float noise", () => {
  expect(conditionOf(formWith({ threshold: "7" })).threshold).toBe(0.07);
});

it("types a stored error rate threshold as a percentage", () => {
  const rule = rules[1];
  if (!rule) {
    throw new Error("fixture rule missing");
  }
  const form = formFromRule({
    ...rule,
    condition: { ...rule.condition, metric: "error_rate", threshold: 0.07 },
  });
  expect(form.threshold).toBe("7");
});

it("rejects a blank threshold as not a number", () => {
  expect(errorsOf(formWith({ threshold: "  " }))).toEqual({
    "condition.threshold": "Enter a number.",
  });
});

it("accepts 0 and 100 as percentage bounds", () => {
  expect(conditionOf(formWith({ threshold: "0" })).threshold).toBe(0);
  expect(conditionOf(formWith({ threshold: "100" })).threshold).toBe(1);
});

it("omits the filter when it is blank and sends it trimmed otherwise", () => {
  expect(conditionOf(formWith({ filter: "   " })).filter).toBeUndefined();
  expect(conditionOf(formWith({ filter: " error_count > 0 " })).filter).toBe("error_count > 0");
});

it("sets only the fields of each metric", () => {
  expect(conditionOf(formWith({ metric: "cost", threshold: "50" }))).toEqual({
    metric: "cost",
    window: "5m",
    comparator: "gt",
    threshold: 50,
  });
  expect(
    conditionOf(formWith({ metric: "latency_p95", threshold: "8000", target: "llm_span" })),
  ).toEqual({
    metric: "latency_p95",
    window: "5m",
    comparator: "gt",
    threshold: 8000,
    min_count: 20,
    target: "llm_span",
  });
  expect(
    conditionOf(
      formWith({
        metric: "score",
        aggregation: "pass_rate",
        comparator: "lt",
        threshold: "90",
        minCount: "10",
        scoreName: "ok",
      }),
    ),
  ).toEqual({
    metric: "score",
    window: "5m",
    comparator: "lt",
    threshold: 0.9,
    min_count: 10,
    score_name: "ok",
    aggregation: "pass_rate",
  });
});

it("reports the threshold, minimum samples, score name and reminder errors", () => {
  expect(errorsOf(formWith({ threshold: "" }))).toEqual({
    "condition.threshold": "Enter a number.",
  });
  expect(errorsOf(formWith({ threshold: "abc" }))["condition.threshold"]).toBe("Enter a number.");
  expect(errorsOf(formWith({ threshold: "120" }))["condition.threshold"]).toBe(
    "Enter a percentage from 0 to 100.",
  );
  expect(errorsOf(formWith({ metric: "cost", threshold: "-1" }))["condition.threshold"]).toBe(
    "Enter 0 or more.",
  );
  expect(errorsOf(formWith({ metric: "latency_p95", threshold: "0" }))["condition.threshold"]).toBe(
    "Enter a duration above 0.",
  );
  expect(errorsOf(formWith({ minCount: "0" }))["condition.min_count"]).toBe(
    "Enter a whole number from 1 to 1,000,000.",
  );
  expect(errorsOf(formWith({ metric: "score", scoreName: "  " }))["condition.score_name"]).toBe(
    "Enter a score name.",
  );
  expect(errorsOf(formWith({ notifyWhileFiring: true, cooldownMinutes: "3" }))).toEqual({
    cooldown_s: "Enter 5 to 10,080 minutes.",
  });
});

it("sends the reminder interval in seconds when reminders are on", () => {
  const result = conditionFromForm(formWith({ notifyWhileFiring: true, cooldownMinutes: "120" }));
  expect(result.ok && result.cooldownS).toBe(7200);
});

it("reads a stored rule into the form", () => {
  const rule = rules[1];
  if (!rule) {
    throw new Error("fixture rule missing");
  }
  const form = formFromRule(rule);
  expect(form.threshold).toBe("5");
  expect(form.filter).toBe("environment = 'production'");
  expect(form.cooldownMinutes).toBe("60");
  expect(form.channelIds).toEqual(rule.channel_ids);
});

it("round-trips every stored rule through the form", () => {
  for (const rule of rules) {
    expect(conditionOf(formFromRule(rule))).toEqual(rule.condition);
    expect(conditionChanged(formFromRule(rule), formFromRule(rule))).toBe(false);
  }
});

it("sees a changed threshold as a changed condition", () => {
  const rule = rules[1];
  if (!rule) {
    throw new Error("fixture rule missing");
  }
  const base = formFromRule(rule);
  expect(conditionChanged(base, { ...base, name: "Renamed" })).toBe(false);
  expect(conditionChanged(base, { ...base, threshold: "10" })).toBe(true);
});

it("ignores the hidden score, target and aggregation of an error-rate form", () => {
  const base = formWith({
    metric: "error_rate",
    scoreName: "helpfulness",
    target: "llm_span",
    aggregation: "pass_rate",
  });
  expect(
    conditionChanged(base, { ...base, scoreName: "accuracy", target: "trace", aggregation: "avg" }),
  ).toBe(false);
});

it("sees a changed score name on a score form", () => {
  const base = formWith({
    metric: "score",
    comparator: "lt",
    threshold: "0.7",
    scoreName: "helpfulness",
    aggregation: "avg",
  });
  expect(conditionChanged(base, { ...base, scoreName: "accuracy" })).toBe(true);
});

it("ignores the hidden minCount of a cost form", () => {
  const base = formWith({ metric: "cost", threshold: "50", minCount: "20" });
  expect(conditionChanged(base, { ...base, minCount: "5" })).toBe(false);
});

it("sees the fields a metric uses, and a metric switch, as changes", () => {
  const latency = formWith({ metric: "latency_p95", threshold: "5000", target: "trace" });
  expect(conditionChanged(latency, { ...latency, target: "llm_span" })).toBe(true);
  const score = formWith({
    metric: "score",
    comparator: "lt",
    threshold: "0.7",
    scoreName: "helpfulness",
    aggregation: "avg",
  });
  expect(conditionChanged(score, { ...score, aggregation: "pass_rate" })).toBe(true);
  expect(conditionChanged(latency, { ...latency, metric: "error_rate" })).toBe(true);
});

it("previews over buckets as wide as the window", () => {
  const now = Date.parse("2026-01-04T00:00:00Z");
  expect(previewParams("6h", now)).toEqual({
    bucket: "6h",
    from: "2026-01-01T00:00:00.000Z",
    to: "2026-01-04T00:00:00.000Z",
  });
  expect(previewParams("24h", now)).toEqual({
    bucket: "1d",
    from: "2025-12-21T00:00:00.000Z",
    to: "2026-01-04T00:00:00.000Z",
  });
  expect(previewParams("5m", now)).toEqual({
    bucket: "5m",
    from: "2026-01-03T00:00:00.000Z",
    to: "2026-01-04T00:00:00.000Z",
  });
  expect(previewParams("1m", now).bucket).toBe("1m");
  expect(previewParams("15m", now).bucket).toBe("15m");
  expect(previewParams("1h", now).bucket).toBe("1h");
});

it("previews a pass rate score as the average of the score", () => {
  expect(
    previewQuery(formWith({ metric: "score", aggregation: "pass_rate", scoreName: " ok " })),
  ).toEqual({
    query: {
      source: "scores",
      filter: "name = 'ok'",
      measures: [{ agg: "avg", field: "value_num" }],
    },
    divisor: 1,
  });
});

it("picks the widget query to preview", () => {
  expect(previewQuery({ metric: "error_rate", target: "trace", scoreName: "" })).toEqual({
    query: { source: "traces", measures: [{ agg: "rate", where: "status = 'error'" }] },
    divisor: 1,
  });
  expect(previewQuery({ metric: "cost", target: "trace", scoreName: "" })).toEqual({
    query: { source: "traces", measures: [{ agg: "sum", field: "cost_total" }] },
    divisor: 1,
  });
  expect(previewQuery({ metric: "latency_p95", target: "trace", scoreName: "" })).toEqual({
    query: { source: "traces", measures: [{ agg: "p95", field: "duration_ns" }] },
    divisor: 1_000_000,
  });
  expect(previewQuery({ metric: "latency_p95", target: "llm_span", scoreName: "" })).toBe(
    undefined,
  );
  expect(previewQuery({ metric: "score", target: "trace", scoreName: "ok" })).toEqual({
    query: {
      source: "scores",
      filter: "name = 'ok'",
      measures: [{ agg: "avg", field: "value_num" }],
    },
    divisor: 1,
  });
});

it("turns an answer into preview rows in the condition's unit", () => {
  const query = {
    source: "traces" as const,
    measures: [{ agg: "p95" as const, field: "duration_ns" }],
  };
  const answer = {
    shape: "series" as const,
    bucket_ns: 60_000_000_000,
    series: [
      {
        measure: 0,
        group: undefined,
        other: false,
        points: [
          { time: "2026-01-03T00:00:00Z", value: 2_000_000_000, rows: 3 },
          { time: "2026-01-03T00:02:00Z", value: 500_000_000, rows: 1 },
        ],
      },
    ],
  };
  expect(previewRows(query, answer, 1_000_000)).toEqual([
    { time: Date.parse("2026-01-03T00:00:00Z"), all: 2000 },
    { time: Date.parse("2026-01-03T00:01:00Z"), all: null },
    { time: Date.parse("2026-01-03T00:02:00Z"), all: 500 },
  ]);
  expect(previewRows(query, undefined, 1)).toEqual([]);
});

it("previews a zero threshold while the form is invalid", () => {
  const condition = previewCondition(formWith({ threshold: "abc" }));
  expect(condition.threshold).toBe(0);
  expect(condition.metric).toBe("error_rate");
});

const ISSUE_OCCURRENCES: Condition = {
  metric: "issue_occurrences",
  window: "1h",
  comparator: "gt",
  threshold: 0,
  issue_id: "0190a6c4-2f1e-7000-8000-000000000001",
};

it("labels, formats and units an issue occurrences condition", () => {
  expect(metricLabel(ISSUE_OCCURRENCES)).toBe("Issue occurrences");
  expect(conditionSummary(ISSUE_OCCURRENCES)).toBe("Issue occurrences > 0 over 1h");
  expect(formatAlertValue(ISSUE_OCCURRENCES, 12)).toBe("12");
  expect(formatAlertValue(ISSUE_OCCURRENCES, 12.4)).toBe("12");
  expect(thresholdUnit("issue_occurrences", "avg")).toBe("occurrences");
  expect(thresholdInPercent("issue_occurrences", "avg")).toBe(false);
});

it("defaults an issue occurrences rule to more than 0", () => {
  expect(metricDefaults("issue_occurrences", "avg")).toEqual({
    threshold: "0",
    comparator: "gt",
    minCount: "20",
  });
});

it("builds an issue scope condition without filter and min count", () => {
  const form = formWith({
    metric: "issue_occurrences",
    window: "1h",
    threshold: "0",
    filter: "environment = 'production'",
    minCount: "7",
    issueScope: "issue",
    issueId: " 0190a6c4-2f1e-7000-8000-000000000001 ",
  });
  expect(conditionOf(form)).toEqual({
    metric: "issue_occurrences",
    window: "1h",
    comparator: "gt",
    threshold: 0,
    issue_id: "0190a6c4-2f1e-7000-8000-000000000001",
  });
});

it("builds a source scope condition", () => {
  const form = formWith({
    metric: "issue_occurrences",
    window: "1h",
    threshold: "3",
    issueScope: "source",
    sourceId: " src-1 ",
    issueId: "ignored",
  });
  expect(conditionOf(form)).toEqual({
    metric: "issue_occurrences",
    window: "1h",
    comparator: "gt",
    threshold: 3,
    source_id: "src-1",
  });
});

it("refuses 1m and 5m windows for issue occurrences only", () => {
  const issue = { metric: "issue_occurrences" as const, issueId: "i" };
  for (const window of ["1m", "5m"] as const) {
    expect(errorsOf(formWith({ ...issue, window }))).toEqual({
      "condition.window": "Issue occurrences need a window of 15 minutes or more.",
    });
  }
  expect(conditionOf(formWith({ ...issue, window: "15m" })).window).toBe("15m");
  expect(conditionOf(formWith({ ...issue, window: "1h" })).window).toBe("1h");
  expect(conditionOf(formWith({ metric: "error_rate", window: "1m" })).window).toBe("1m");
});

it("validates the threshold, the scope ids and the min count of issue occurrences", () => {
  expect(
    errorsOf(
      formWith({ metric: "issue_occurrences", window: "1h", issueId: "i", threshold: "-1" }),
    ),
  ).toEqual({ "condition.threshold": "Enter 0 or more." });
  expect(
    errorsOf(
      formWith({ metric: "issue_occurrences", window: "1h", issueScope: "issue", issueId: " " }),
    ),
  ).toEqual({ "condition.issue_id": "Choose an issue." });
  expect(
    errorsOf(
      formWith({ metric: "issue_occurrences", window: "1h", issueScope: "source", sourceId: "" }),
    ),
  ).toEqual({ "condition.source_id": "Choose a source." });
  expect(
    conditionOf(
      formWith({
        metric: "issue_occurrences",
        window: "1h",
        issueId: "i",
        threshold: "2.5",
        minCount: "abc",
      }),
    ).threshold,
  ).toBe(2.5);
});

it("round-trips an issue rule and a source rule through the form", () => {
  const base = rules[0];
  if (!base) {
    throw new Error("fixture rule missing");
  }
  const issueCondition: Condition = { ...ISSUE_OCCURRENCES, threshold: 2 };
  const sourceCondition: Condition = {
    metric: "issue_occurrences",
    window: "1h",
    comparator: "gte",
    threshold: 5,
    source_id: "src-1",
  };
  for (const condition of [issueCondition, sourceCondition]) {
    const form = formFromRule({ ...base, condition });
    expect(conditionOf(form)).toEqual(condition);
  }
  expect(formFromRule({ ...base, condition: sourceCondition }).issueScope).toBe("source");
  expect(formFromRule({ ...base, condition: issueCondition }).issueScope).toBe("issue");
  expect(formFromRule({ ...base, condition: issueCondition }).issueId).toBe(
    issueCondition.issue_id,
  );
});

it("changes an issue rule only by its scope, not by its filter or min count", () => {
  const base = formWith({
    metric: "issue_occurrences",
    window: "1h",
    issueId: "a",
    threshold: "0",
  });
  expect(conditionChanged(base, { ...base, filter: "environment = 'x'" })).toBe(false);
  expect(conditionChanged(base, { ...base, minCount: "3" })).toBe(false);
  expect(conditionChanged(base, { ...base, issueId: "b" })).toBe(true);
  expect(conditionChanged(base, { ...base, issueScope: "source", sourceId: "a" })).toBe(true);
  const source = { ...base, issueScope: "source" as const, sourceId: "s" };
  expect(conditionChanged(source, { ...source, issueId: "b" })).toBe(false);
  expect(conditionChanged(source, { ...source, sourceId: "t" })).toBe(true);
});

it("counts the scope of an issue rule when switching metric and back is not a change", () => {
  const original = formWith({
    metric: "score",
    window: "1h",
    comparator: "lt",
    threshold: "0.7",
    scoreName: "helpfulness",
    aggregation: "avg",
  });
  const issue = {
    ...original,
    metric: "issue_occurrences" as const,
    threshold: "0",
    comparator: "gt" as const,
    issueId: "x",
  };
  const back = { ...issue, metric: "score" as const, comparator: "lt" as const, threshold: "0.7" };
  expect(conditionChanged(original, back)).toBe(false);
});

it("has no widget query for issue occurrences", () => {
  expect(previewQuery({ metric: "issue_occurrences", target: "trace", scoreName: "" })).toBe(
    undefined,
  );
});

it("scopes the issue occurrence preview to the chosen issue or source", () => {
  expect(issuePreviewScope(formWith({ metric: "error_rate" }))).toBeUndefined();
  expect(
    issuePreviewScope(formWith({ metric: "issue_occurrences", window: "5m", issueId: "i" })),
  ).toEqual({
    kind: "missing",
    message: "Issue occurrences need a window of 15 minutes or more.",
  });
  expect(
    issuePreviewScope(formWith({ metric: "issue_occurrences", window: "1h", issueId: "  " })),
  ).toEqual({ kind: "missing", message: "Choose an issue to see the preview." });
  expect(
    issuePreviewScope(
      formWith({ metric: "issue_occurrences", window: "1h", issueScope: "source", sourceId: "" }),
    ),
  ).toEqual({ kind: "missing", message: "Choose an issue source to see the preview." });
  expect(
    issuePreviewScope(formWith({ metric: "issue_occurrences", window: "1h", issueId: " i-1 " })),
  ).toEqual({ kind: "ready", scope: { issue_id: "i-1" }, window: "1h" });
  expect(
    issuePreviewScope(
      formWith({
        metric: "issue_occurrences",
        window: "15m",
        issueScope: "source",
        sourceId: " s-1 ",
      }),
    ),
  ).toEqual({ kind: "ready", scope: { source_id: "s-1" }, window: "15m" });
});

it("turns an issue occurrence answer into rows, zeros included and unparsable times dropped", () => {
  expect(issuePreviewRows(undefined)).toEqual([]);
  expect(
    issuePreviewRows({
      from: "2026-01-01T00:00:00Z",
      to: "2026-01-01T03:00:00Z",
      window: "1h",
      points: [
        { time: "2026-01-01T00:00:00Z", count: 2 },
        { time: "2026-01-01T01:00:00Z", count: 0 },
        { time: "2026-01-01T02:00:00Z", count: 5 },
        { time: "not a time", count: 9 },
      ],
    }),
  ).toEqual([
    { time: Date.parse("2026-01-01T00:00:00Z"), all: 2 },
    { time: Date.parse("2026-01-01T01:00:00Z"), all: 0 },
    { time: Date.parse("2026-01-01T02:00:00Z"), all: 5 },
  ]);
});

it("parses the prefilled new rule search", () => {
  expect(
    parseNewRuleSearch({ metric: "issue_occurrences", issue_id: " abc ", source_id: "s" }),
  ).toEqual({ metric: "issue_occurrences", issue_id: "abc", source_id: "s" });
  expect(parseNewRuleSearch({ metric: "cost", issue_id: "", source_id: 3 })).toEqual({});
  expect(parseNewRuleSearch({})).toEqual({});
});
