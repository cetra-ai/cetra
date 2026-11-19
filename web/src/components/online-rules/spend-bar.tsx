import { cn } from "cn";
import type { Schema } from "@/api/types";
import { formatCost } from "@/lib/format";

type Rule = Schema<"OnlineRule">;

/** Spent over budget for the current period, or undefined without a budget (or a zero one). */
export function spendRatio(rule: Rule): number | undefined {
  if (rule.budget_usd === undefined || rule.budget_usd === 0) {
    return undefined;
  }
  return (rule.spend?.spent_usd ?? 0) / rule.budget_usd;
}

/** "$0.42 / $5.00 today" over a thin bar: primary, then warn from 80 %, error at 100 %. */
export function SpendBar({ rule }: { rule: Rule }) {
  if (rule.budget_usd === undefined || rule.budget_period === undefined) {
    return <span className="text-muted-foreground">—</span>;
  }
  const spent = rule.spend?.spent_usd ?? 0;
  const ratio = spendRatio(rule) ?? 0;
  const fill = Math.min(100, ratio * 100);
  const period = rule.budget_period === "day" ? "today" : "this month";
  const tone = ratio >= 1 ? "bg-status-error" : ratio >= 0.8 ? "bg-status-warn" : "bg-primary";
  return (
    <div className="flex min-w-32 flex-col gap-1">
      <span className="text-body tabular-nums">
        {`${formatCost(spent)} / ${formatCost(rule.budget_usd)} ${period}`}
      </span>
      <div
        role="progressbar"
        aria-label="Spend"
        aria-valuemin={0}
        aria-valuemax={100}
        aria-valuenow={Math.round(fill)}
        className="h-1 overflow-hidden rounded-full bg-muted"
      >
        <div className={cn("h-full rounded-full", tone)} style={{ width: `${fill}%` }} />
      </div>
    </div>
  );
}
