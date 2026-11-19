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
