import { ALERT_BUCKETS, type AlertBucket } from "@/lib/alerts";
import { parseEnum, parseString } from "@/lib/search/common";

export type AlertsSearch = {
  /** Shows only the rules in this bucket. */
  state?: AlertBucket;
  /** The rule open in the panel beside the list. */
  rule?: string;
};

export function parseAlertsSearch(search: Record<string, unknown>): AlertsSearch {
  const state = parseEnum(search.state, ALERT_BUCKETS);
  const rule = parseString(search.rule, 256);
  return {
    ...(state ? { state } : {}),
    ...(rule ? { rule } : {}),
  };
}

/** The prefilled rule form of "Create alert" on an issue page or a source. */
export type NewRuleSearch = {
  metric?: "issue_occurrences";
  issue_id?: string;
  source_id?: string;
};

export function parseNewRuleSearch(search: Record<string, unknown>): NewRuleSearch {
  const metric = search.metric === "issue_occurrences" ? "issue_occurrences" : undefined;
  const issueId = parseString(search.issue_id, 256);
  const sourceId = parseString(search.source_id, 256);
  return {
    ...(metric ? { metric } : {}),
    ...(issueId ? { issue_id: issueId } : {}),
    ...(sourceId ? { source_id: sourceId } : {}),
  };
}
