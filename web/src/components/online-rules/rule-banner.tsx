import type { Schema } from "@/api/types";
import { formatCost } from "@/lib/format";

type Rule = Schema<"OnlineRule">;

/** The banner text of a paused rule, or `undefined` when the rule is not paused. */
export function pausedMessage(rule: Rule, connectionName: string | undefined): string | undefined {
  const detail = rule.paused_detail ? ` (${rule.paused_detail})` : "";
  switch (rule.paused_reason) {
    case undefined:
      return undefined;
    case "budget": {
      const monthly = rule.budget_period === "month";
      const period = monthly ? "monthly" : "daily";
      const turns = monthly ? "month" : "day";
      const budget = formatCost(rule.budget_usd);
      return `Paused: the ${period} budget of ${budget} is spent. The rule resumes when the ${turns} turns (UTC), or when you raise the budget.`;
    }
    case "errors":
      return `Paused: too many jobs failed${detail}. Fix the cause, then resume the rule.`;
    case "connection": {
      const subject = connectionName
        ? `the LLM connection ${connectionName}`
        : "the LLM connection";
      return `Paused: ${subject} is failing${detail}. Rules in other projects that use it are paused too. Fix it in Settings › LLM connections; a successful connection test resumes them.`;
    }
  }
}

export function RuleBanner({
  rule,
  connectionName,
}: {
  rule: Rule;
  connectionName: string | undefined;
}) {
  const message = pausedMessage(rule, connectionName);
  if (message === undefined) {
    return null;
  }
  return (
    <div
      role="alert"
      className="rounded-md border border-destructive/40 bg-destructive/5 px-4 py-3 text-sm"
    >
      {message}
    </div>
  );
}
