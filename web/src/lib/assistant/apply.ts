import type { FilterDraft, SqlDraft as SqlDraftOf } from "@/lib/assistant/types";
import type { SqlDraft } from "@/lib/sql/draft";

/** The list route and search a filter draft opens. */
export function filterTarget(draft: FilterDraft): {
  to:
    | "/$org/$project/traces"
    | "/$org/$project/spans"
    | "/$org/$project/sessions"
    | "/$org/$project/users";
  search: Record<string, string | number>;
} {
  const search: Record<string, string | number> = {};
  if (draft.filter !== undefined) {
    search.filter = draft.filter;
  }
  // Sessions and users have no full-text search parameter: a draft's search is left out there.
  if (draft.search !== undefined && (draft.view === "traces" || draft.view === "spans")) {
    search.search = draft.search;
  }
  Object.assign(search, rangeSearchOf(draft));
  if (draft.include_eval) {
    search.eval = 1;
  }
  if (draft.include_experiments) {
    search.exp = 1;
  }
  const to = {
    traces: "/$org/$project/traces",
    spans: "/$org/$project/spans",
    sessions: "/$org/$project/sessions",
    users: "/$org/$project/users",
  } as const;
  return { to: to[draft.view], search };
}

/** The search keys of a draft's range: `range`, or `from` and `to` as RFC 3339 (a draft's epoch milliseconds are converted). */
export function rangeSearchOf(source: {
  range?: string;
  from?: string;
  to?: string;
}): Record<string, string> {
  if (source.range !== undefined) {
    return { range: source.range };
  }
  if (source.from !== undefined && source.to !== undefined) {
    return { from: isoOf(source.from), to: isoOf(source.to) };
  }
  return {};
}

/** An all-digit value is epoch milliseconds; anything else, or an unreadable number, is kept as given. */
function isoOf(value: string): string {
  if (!/^\d+$/.test(value)) {
    return value;
  }
  const date = new Date(Number(value));
  return Number.isNaN(date.getTime()) ? value : date.toISOString();
}

/** The SQL tab draft of a SQL draft: params as strings (JSON.stringify for non-strings). */
export function sqlTabDraft(draft: SqlDraftOf): SqlDraft {
  const params: Record<string, string> = {};
  for (const [name, value] of Object.entries(draft.params)) {
    params[name] = typeof value === "string" ? value : JSON.stringify(value);
  }
  return { query: draft.query, params };
}
