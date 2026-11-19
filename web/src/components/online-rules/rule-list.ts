import type { Schema } from "@/api/types";
import type { RuleSort } from "@/lib/search/online-rules";
import { RULE_BUCKETS, type RuleBucket, ruleBucket } from "./rule-status";
import { spendRatio } from "./spend-bar";

type Rule = Schema<"OnlineRule">;

/** Jobs of the last 24 hours in every status. */
export function jobTotal(rule: Rule): number {
  const jobs = rule.jobs_last_24h;
  return jobs.queued + jobs.running + jobs.done + jobs.failed + jobs.skipped;
}

export function countBuckets(rules: readonly Rule[]): Record<RuleBucket, number> {
  const counts: Record<RuleBucket, number> = { active: 0, paused: 0, disabled: 0 };
  for (const rule of rules) {
    counts[ruleBucket(rule)] += 1;
  }
  return counts;
}

/** Spend of the day-budget rules, or undefined when no rule has a daily budget. */
export function spendToday(rules: readonly Rule[]): number | undefined {
  const daily = rules.filter((rule) => rule.budget_period === "day");
  if (daily.length === 0) {
    return undefined;
  }
  return daily.reduce((sum, rule) => sum + (rule.spend?.spent_usd ?? 0), 0);
}

/** The left border of a row: red when paused for errors or the connection, amber for the budget. */
export function borderTone(rule: Rule): string {
  if (rule.paused_reason === "errors" || rule.paused_reason === "connection") {
    return "border-l-status-error";
  }
  if (rule.paused_reason === "budget") {
    return "border-l-status-warn";
  }
  return "border-l-transparent";
}

/** Keeps the rules in the bucket (when set) whose name, evaluator or filter contains the text. */
export function filterRules(
  rules: readonly Rule[],
  options: {
    status?: RuleBucket;
    q?: string;
    evaluatorName: (evaluatorId: string) => string | undefined;
  },
): Rule[] {
  const text = options.q?.toLowerCase();
  return rules.filter((rule) => {
    if (options.status !== undefined && ruleBucket(rule) !== options.status) {
      return false;
    }
    if (text === undefined) {
      return true;
    }
    return [rule.name, options.evaluatorName(rule.evaluator_id) ?? "", rule.filter ?? ""].some(
      (field) => field.toLowerCase().includes(text),
    );
  });
}

/**
 * Active, paused, disabled, then by name when no column is sorted. A column sort puts rules with
 * no value for it (no budget for spend, no score) last in both directions; the name breaks ties.
 * `scoreOf` is the rule's score value (see `scoreValue`).
 */
export function sortRules(
  rules: readonly Rule[],
  sort: RuleSort | undefined,
  dir: "asc" | "desc" = "asc",
  scoreOf: (rule: Rule) => number | undefined,
): Rule[] {
  const byName = (a: Rule, b: Rule) => a.name.localeCompare(b.name);
  if (sort === undefined) {
    return [...rules].sort(
      (a, b) =>
        RULE_BUCKETS.indexOf(ruleBucket(a)) - RULE_BUCKETS.indexOf(ruleBucket(b)) || byName(a, b),
    );
  }
  const sign = dir === "asc" ? 1 : -1;
  const value = (rule: Rule): number | string | undefined => {
    switch (sort) {
      case "name":
        return rule.name;
      case "status":
        return RULE_BUCKETS.indexOf(ruleBucket(rule));
      case "spend":
        return spendRatio(rule);
      case "jobs":
        return jobTotal(rule);
      case "score":
        return scoreOf(rule);
    }
  };
  return [...rules].sort((a, b) => {
    const va = value(a);
    const vb = value(b);
    if (va === undefined || vb === undefined) {
      if (va === vb) {
        return byName(a, b);
      }
      return va === undefined ? 1 : -1;
    }
    const raw =
      typeof va === "string" && typeof vb === "string"
        ? va.localeCompare(vb)
        : Number(va) - Number(vb);
    return raw !== 0 ? sign * raw : byName(a, b);
  });
}
