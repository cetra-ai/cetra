import { parseEnum, parseString, SEARCH_MAX } from "@/lib/search/common";

/** The state tabs of the Issues list. `open` is the default. */
export const ISSUE_STATE_TABS = ["open", "resolved", "ignored", "all"] as const;
export type IssueStateTab = (typeof ISSUE_STATE_TABS)[number];

/** The sort orders of the Issues list, newest first. `last_seen` is the default. */
export const ISSUE_SORTS = ["last_seen", "first_seen"] as const;
export type IssueSort = (typeof ISSUE_SORTS)[number];

/** Longest source id kept in the URL. Ids are UUIDs, so this is generous. */
const SOURCE_MAX = 64;

export type IssuesSearch = {
  /** Shows only the issues in this state. Absent means `open`. */
  state?: IssueStateTab;
  /** Shows only the issues of this source. Absent means all sources. */
  source?: string;
  /** Keeps the issues whose title contains this text. */
  q?: string;
  /** Absent means `last_seen`. */
  sort?: IssueSort;
};

export function parseIssuesSearch(search: Record<string, unknown>): IssuesSearch {
  const state = parseEnum(search.state, ISSUE_STATE_TABS);
  const source = parseString(search.source, SOURCE_MAX);
  const q = parseString(search.q, SEARCH_MAX);
  const sort = parseEnum(search.sort, ISSUE_SORTS);
  return {
    ...(state ? { state } : {}),
    ...(source ? { source } : {}),
    ...(q ? { q } : {}),
    ...(sort ? { sort } : {}),
  };
}
