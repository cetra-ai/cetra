import { parseEnum, parseString } from "./common";

export const AUDIT_RANGES = ["24h", "7d", "30d", "90d", "all"] as const;
export type AuditRange = (typeof AUDIT_RANGES)[number];

const OUTCOMES = ["success", "failure"] as const;
const TEXT_MAX = 200;
const DAY_MS = 86_400_000;

const RANGE_MS: Record<Exclude<AuditRange, "all">, number> = {
  "24h": DAY_MS,
  "7d": 7 * DAY_MS,
  "30d": 30 * DAY_MS,
  "90d": 90 * DAY_MS,
};

/** URL state of the audit log pages. `period` is always set; the rest are omitted when absent. */
export type AuditSearch = {
  period: AuditRange;
  action?: string;
  actor?: string;
  target?: string;
  outcome?: (typeof OUTCOMES)[number];
  open?: string;
};

/** Which log a view reads: one org's, or the instance's. */
export type AuditScope = { org: string } | "instance";

/** The query the audit list and export routes take, without paging. */
export type AuditQuery = {
  from?: string;
  action?: string;
  actor_id?: string;
  target_id?: string;
  outcome?: (typeof OUTCOMES)[number];
};

/** Parses the audit log URL params. An unknown period is the default, `7d`; invalid values are dropped. */
export function parseAuditSearch(search: Record<string, unknown>): AuditSearch {
  const period = parseEnum(search.period, AUDIT_RANGES) ?? "7d";
  const action = parseString(search.action, TEXT_MAX);
  const actor = parseString(search.actor, TEXT_MAX);
  const target = parseString(search.target, TEXT_MAX);
  const outcome = parseEnum(search.outcome, OUTCOMES);
  const open = parseString(search.open, TEXT_MAX);
  return {
    period,
    ...(action ? { action } : {}),
    ...(actor ? { actor } : {}),
    ...(target ? { target } : {}),
    ...(outcome ? { outcome } : {}),
    ...(open ? { open } : {}),
  };
}

/** The API query for `search`: `from` is `now` minus the period, as RFC 3339; `all` has no `from`. */
export function auditQuery(search: AuditSearch, now: Date): AuditQuery {
  const query: AuditQuery = {};
  if (search.period !== "all") {
    query.from = new Date(now.getTime() - RANGE_MS[search.period]).toISOString();
  }
  if (search.action) {
    query.action = search.action;
  }
  if (search.actor) {
    query.actor_id = search.actor;
  }
  if (search.target) {
    query.target_id = search.target;
  }
  if (search.outcome) {
    query.outcome = search.outcome;
  }
  return query;
}
