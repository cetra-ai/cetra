import { ALERT_BUCKETS, type AlertBucket } from "@/lib/alerts";
import { parseEnum } from "@/lib/search/common";

export type AlertsSearch = {
  /** Shows only the rules in this bucket. */
  state?: AlertBucket;
};

export function parseAlertsSearch(search: Record<string, unknown>): AlertsSearch {
  const state = parseEnum(search.state, ALERT_BUCKETS);
  return state ? { state } : {};
}
