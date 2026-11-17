import { useLocation } from "@tanstack/react-router";
import { useEffect, useState } from "react";
import { STORAGE_PREFIX } from "@/product";

export type ListName = "sessions" | "traces";

/** Where a detail page goes back to: the list's search, and the row it was opened from. */
export type ListReturn = {
  list: ListName;
  search: Record<string, unknown>;
  anchorId?: string;
  anchorOffset?: number;
  openedId?: string;
};

/** What a list restores on the way back: the scroll anchor, and the row to highlight. */
export type ListRestore = {
  anchorId?: string;
  anchorOffset?: number;
  openedId?: string;
};

/** The first visible row of a list, and how many px of it are scrolled past. */
export type RowAnchor = { rowId: string; offset: number };

declare module "@tanstack/react-router" {
  interface HistoryState {
    /** Set by a list when it opens a detail page. */
    listReturn?: ListReturn;
    /** Set by a detail page's back link, read by the list it returns to. */
    listRestore?: ListRestore;
  }
}

const HIGHLIGHT_MS = 1500;

export function listReturnKey(list: ListName): string {
  return `${STORAGE_PREFIX}list-return.${list}`;
}

/** Keeps the return for a reload or a page opened without its history state. */
export function rememberListReturn(ret: ListReturn): void {
  try {
    sessionStorage.setItem(listReturnKey(ret.list), JSON.stringify(ret));
  } catch {
    // Blocked storage leaves the history state as the only way back.
  }
}

export function readListReturn(list: ListName): ListReturn | undefined {
  try {
    const raw = sessionStorage.getItem(listReturnKey(list));
    if (!raw) {
      return undefined;
    }
    const value: unknown = JSON.parse(raw);
    return isListReturn(value, list) ? value : undefined;
  } catch {
    return undefined;
  }
}

function isListReturn(value: unknown, list: ListName): value is ListReturn {
  if (typeof value !== "object" || value === null) {
    return false;
  }
  const candidate = value as Partial<Record<keyof ListReturn, unknown>>;
  return (
    candidate.list === list &&
    typeof candidate.search === "object" &&
    candidate.search !== null &&
    (candidate.anchorId === undefined || typeof candidate.anchorId === "string") &&
    (candidate.anchorOffset === undefined || typeof candidate.anchorOffset === "number") &&
    (candidate.openedId === undefined || typeof candidate.openedId === "string")
  );
}

/** The return a list hands to the detail page it opens. */
export function listReturnFor(
  list: ListName,
  search: Record<string, unknown>,
  anchor: RowAnchor | undefined,
  openedId: string,
): ListReturn {
  return {
    list,
    search,
    ...(anchor ? { anchorId: anchor.rowId, anchorOffset: anchor.offset } : {}),
    openedId,
  };
}

/** The anchor a list restores, when its return state has one. */
export function anchorOf(restore: ListRestore | undefined): RowAnchor | undefined {
  if (restore?.anchorId === undefined || restore.anchorOffset === undefined) {
    return undefined;
  }
  return { rowId: restore.anchorId, offset: restore.anchorOffset };
}

/** The return of the detail page's list: the history state first, then the stored one. */
export function useListReturn(list: ListName): ListReturn | undefined {
  const fromState = useLocation().state.listReturn;
  if (fromState?.list === list) {
    return fromState;
  }
  return readListReturn(list);
}

/** Holds the id of the row a list opened for `HIGHLIGHT_MS` after it mounts with that id. */
export function useRestoredHighlight(openedId: string | undefined): ReadonlySet<string> {
  const [ids, setIds] = useState<ReadonlySet<string>>(() => new Set());
  useEffect(() => {
    if (openedId === undefined) {
      setIds(new Set());
      return;
    }
    setIds(new Set([openedId]));
    const timer = window.setTimeout(() => {
      setIds(new Set());
    }, HIGHLIGHT_MS);
    return () => {
      window.clearTimeout(timer);
    };
  }, [openedId]);
  return ids;
}
