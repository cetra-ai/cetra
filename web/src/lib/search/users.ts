import type { Schema } from "@/api/types";
import { quoteString } from "@/lib/dsl/dsl";
import {
  FILTER_MAX,
  parseEnum,
  parseFlag,
  parseRfc3339,
  parseString,
  parseViewId,
} from "@/lib/search/common";
import { PRESETS, type Preset, type RangeState, rangeFromSearch } from "@/lib/time-range";

const USER_SORTS = [
  "last_seen",
  "first_seen",
  "trace_count",
  "session_count",
  "traces_per_session",
  "total_tokens",
  "cost_total",
  "error_count",
  "error_rate",
] as const;
const SORT_ORDERS = ["asc", "desc"] as const;

export const USER_COLUMN_IDS = [
  "user",
  "activity",
  "traces",
  "sessions",
  "traces_per_session",
  "tokens",
  "cost",
  "errors",
  "error_rate",
  "metadata",
  "first_seen",
  "last_seen",
] as const;

export type UserColumnId = (typeof USER_COLUMN_IDS)[number];

const USER_COLUMN_ID_SET: ReadonlySet<string> = new Set(USER_COLUMN_IDS);

export type UsersSearch = {
  sort?: Schema<"UserSort">;
  order?: Schema<"SortOrder">;
  range?: Preset;
  from?: string;
  to?: string;
  eval?: 1;
  exp?: 1;
  cols?: string;
  filter?: string;
  view?: string;
  user?: string;
};

export const USER_PAGE_TABS = ["sessions", "traces"] as const;
export type UserPageTab = (typeof USER_PAGE_TABS)[number];

/** Search params of the user page. The list keeps its own URL. */
export type UserPageSearch = {
  range?: Preset;
  from?: string;
  to?: string;
  eval?: 1;
  exp?: 1;
  tab?: UserPageTab;
};

export const USER_PAGE_DEFAULT_RANGE: Preset = "30d";

/** The traces-view filter that selects one end user's traces (UP-5). */
export function userTraceFilter(userId: string): string {
  return `user_id = ${quoteString(userId)}`;
}

/** The page's range: the URL's, else the last 30 days (UP-2). */
export function userPageRange(search: UserPageSearch): RangeState {
  return search.range || (search.from && search.to)
    ? rangeFromSearch(search)
    : { kind: "preset", preset: USER_PAGE_DEFAULT_RANGE };
}

/** A range as URL params: the preset, or the absolute pair. Always explicit. */
export function rangeParams(state: RangeState): { range: Preset } | { from: string; to: string } {
  return state.kind === "preset" ? { range: state.preset } : { from: state.from, to: state.to };
}

export function isUserColumnId(id: string): boolean {
  return USER_COLUMN_ID_SET.has(id) || (id.startsWith("score:") && id.length > "score:".length);
}

export function parseUsersSearch(search: Record<string, unknown>): UsersSearch {
  const sort = parseEnum(search.sort, USER_SORTS);
  const order = parseEnum(search.order, SORT_ORDERS);
  const range = parseEnum(search.range, PRESETS);
  const from = parseRfc3339(search.from);
  const to = parseRfc3339(search.to);
  const absolute = from !== undefined && to !== undefined && Date.parse(from) < Date.parse(to);
  const evalOn = parseFlag(search.eval);
  const expOn = parseFlag(search.exp);
  const cols = parseCols(search.cols);
  const filter = parseString(search.filter, FILTER_MAX);
  const view = parseViewId(search.view);
  const user = parseString(search.user, 256);
  return {
    ...(user ? { user } : {}),
    ...(sort ? { sort } : {}),
    ...(order ? { order } : {}),
    ...(range ? { range } : {}),
    ...(absolute ? { from, to } : {}),
    ...(evalOn ? { eval: 1 } : {}),
    ...(expOn ? { exp: 1 } : {}),
    ...(cols ? { cols } : {}),
    ...(filter ? { filter } : {}),
    ...(view ? { view } : {}),
  };
}

export function parseUserPageSearch(search: Record<string, unknown>): UserPageSearch {
  const range = parseEnum(search.range, PRESETS);
  const from = parseRfc3339(search.from);
  const to = parseRfc3339(search.to);
  const absolute = from !== undefined && to !== undefined && Date.parse(from) < Date.parse(to);
  const evalOn = parseFlag(search.eval);
  const expOn = parseFlag(search.exp);
  const tab = parseEnum(search.tab, USER_PAGE_TABS);
  return {
    ...(range ? { range } : {}),
    ...(absolute ? { from, to } : {}),
    ...(evalOn ? { eval: 1 } : {}),
    ...(expOn ? { exp: 1 } : {}),
    ...(tab ? { tab } : {}),
  };
}

function parseCols(value: unknown): string | undefined {
  if (typeof value !== "string") {
    return undefined;
  }
  const ids: string[] = [];
  for (const part of value.split(",")) {
    const id = part.trim();
    if (!isUserColumnId(id) || ids.includes(id)) {
      continue;
    }
    ids.push(id);
  }
  return ids.length > 0 ? ids.join(",") : undefined;
}
