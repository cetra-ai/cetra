import { carryFilter, type ListKind } from "@/lib/facets";
import { FILTER_MAX, parseEnum, parseRfc3339, parseString } from "@/lib/search/common";
import { PRESETS, type Preset } from "@/lib/time-range";

/**
 * The list page last visited in this browser tab, per project (D8). A list page entered with an
 * empty search carries its filters and window from it.
 */
export type ListContext = {
  source: ListKind;
  range?: Preset;
  from?: string;
  to?: string;
  eval?: 1;
  exp?: 1;
  filter?: string;
};

/** The search a list page is entered with, carried from another list page (D8). */
export type CarriedSearch = {
  search: Record<string, unknown>;
  from: ListKind;
  dropped: number;
  /** Whether a filter conjunct was kept. */
  kept: boolean;
};

/** The window and flags of a list search: the part a "Clear" keeps the range of. */
export type RangeSearch = { range?: Preset; from?: string; to?: string };

const KINDS: readonly ListKind[] = ["traces", "spans", "sessions", "users"];

function storageKey(org: string, project: string): string {
  return `cetra.list-context.${org}/${project}`;
}

/** The list context of this tab, or undefined when there is none or storage is blocked. */
export function readListContext(org: string, project: string): ListContext | undefined {
  try {
    const raw = window.sessionStorage.getItem(storageKey(org, project));
    if (raw === null) {
      return undefined;
    }
    const parsed: unknown = JSON.parse(raw);
    if (typeof parsed !== "object" || parsed === null) {
      return undefined;
    }
    const record = parsed as Record<string, unknown>;
    if (!KINDS.includes(record.source as ListKind)) {
      return undefined;
    }
    // Each value is checked as the list pages check their own search: a stored value that the
    // route would drop must not reach it, or the entered list would wait for a search that never lands.
    const context: ListContext = { source: record.source as ListKind };
    const range = parseEnum(record.range, PRESETS);
    if (range !== undefined) {
      context.range = range;
    }
    const from = parseRfc3339(record.from);
    const to = parseRfc3339(record.to);
    if (from !== undefined && to !== undefined && Date.parse(from) < Date.parse(to)) {
      context.from = from;
      context.to = to;
    }
    if (record.eval === 1) {
      context.eval = 1;
    }
    if (record.exp === 1) {
      context.exp = 1;
    }
    const filter = parseString(record.filter, FILTER_MAX);
    if (filter !== undefined) {
      context.filter = filter;
    }
    return context;
  } catch {
    // Blocked or corrupt storage: no context, so nothing is carried.
    return undefined;
  }
}

/** Records the search of a list page as this tab's list context. Blocked storage is ignored. */
export function writeListContext(
  org: string,
  project: string,
  kind: ListKind,
  search: object,
): void {
  const record = search as Record<string, unknown>;
  const context: ListContext = { source: kind };
  if (typeof record.range === "string") {
    context.range = record.range as Preset;
  }
  if (typeof record.from === "string") {
    context.from = record.from;
  }
  if (typeof record.to === "string") {
    context.to = record.to;
  }
  if (record.eval === 1) {
    context.eval = 1;
  }
  if (record.exp === 1) {
    context.exp = 1;
  }
  if (typeof record.filter === "string" && record.filter.length > 0) {
    context.filter = record.filter;
  }
  try {
    window.sessionStorage.setItem(storageKey(org, project), JSON.stringify(context));
  } catch {
    // Blocked storage: the next list page has no context to carry.
  }
}

/**
 * The search `to` is entered with, carried from the context (D8): the window and flags always,
 * and the filter conjuncts that carry. Undefined when there is no other list's context, or when
 * nothing is carried and nothing is dropped. A search of `{}` with drops is returned as is: the
 * caller states the drops without navigating.
 */
export function carriedSearch(
  context: ListContext | undefined,
  to: ListKind,
): CarriedSearch | undefined {
  if (context === undefined || context.source === to) {
    return undefined;
  }
  let kept = "";
  let dropped = 0;
  try {
    ({ kept, dropped } = carryFilter(context.filter ?? "", context.source, to));
  } catch {
    // A filter that does not split carries nothing.
    dropped = 1;
  }
  const search: Record<string, unknown> = {
    ...(context.range !== undefined ? { range: context.range } : {}),
    ...(context.from !== undefined ? { from: context.from } : {}),
    ...(context.to !== undefined ? { to: context.to } : {}),
    ...(context.eval !== undefined ? { eval: context.eval } : {}),
    ...(context.exp !== undefined ? { exp: context.exp } : {}),
    ...(kept.length > 0 ? { filter: kept } : {}),
  };
  // Nothing to put in the URL and nothing dropped: there is nothing to carry or to say.
  if (Object.keys(search).length === 0 && dropped === 0) {
    return undefined;
  }
  return { search, from: context.source, dropped, kept: kept.length > 0 };
}

/** The window of a list search only: the search "Clear" leaves a list with (D8). */
export function rangeOnly(search: RangeSearch): RangeSearch {
  return {
    ...(search.range !== undefined ? { range: search.range } : {}),
    ...(search.from !== undefined ? { from: search.from } : {}),
    ...(search.to !== undefined ? { to: search.to } : {}),
  };
}
