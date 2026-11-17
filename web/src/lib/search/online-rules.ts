import { RULE_BUCKETS, type RuleBucket } from "@/components/online-rules/rule-status";
import { parseEnum, parseString } from "@/lib/search/common";

export const RULE_SORTS = ["name", "status", "spend", "jobs", "score"] as const;
export type RuleSort = (typeof RULE_SORTS)[number];

export type OnlineRulesSearch = {
  /** The rule open in the panel beside the list. */
  rule?: string;
  /** Shows only the rules in this bucket. */
  status?: RuleBucket;
  /** Client-side search over name, evaluator and filter. */
  q?: string;
  sort?: RuleSort;
  dir?: "asc" | "desc";
};

export function parseOnlineRulesSearch(search: Record<string, unknown>): OnlineRulesSearch {
  const rule = parseString(search.rule, 256);
  const status = parseEnum(search.status, RULE_BUCKETS);
  const q = parseString(search.q, 200);
  const sort = parseEnum(search.sort, RULE_SORTS);
  const dir = parseEnum(search.dir, ["asc", "desc"] as const);
  return {
    ...(rule ? { rule } : {}),
    ...(status ? { status } : {}),
    ...(q ? { q } : {}),
    ...(sort ? { sort } : {}),
    ...(dir ? { dir } : {}),
  };
}
