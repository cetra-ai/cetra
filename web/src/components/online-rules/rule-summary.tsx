import type { Schema } from "@/api/types";

type Rule = Schema<"OnlineRule">;

/** The sample rate as a percentage, e.g. "25 %". */
export function sampleText(rate: number): string {
  return `${Number((rate * 100).toFixed(2))} %`;
}

/** "Traces · status = 'ok' · 25 %"; the filter reads "all" when the rule has none. */
export function ruleSummary(rule: Rule): string {
  const target = rule.target === "trace" ? "Traces" : "Spans";
  const filter = rule.filter ? rule.filter : "all";
  return `${target} · ${filter} · ${sampleText(rule.sample_rate)}`;
}
