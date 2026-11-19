import { type RouterHistory, useLocation, useMatch } from "@tanstack/react-router";
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

/** The page a detail page's back link returns to, when it was not opened from its list. */
export type BackTo = { label: string; href: string };

declare module "@tanstack/react-router" {
  interface HistoryState {
    /** Set by a list when it opens a detail page. */
    listReturn?: ListReturn;
    /** Set by a detail page's back link, read by the list it returns to. */
    listRestore?: ListRestore;
    /** Set by a page that opens a detail page from somewhere other than its list: the way back. */
    backTo?: BackTo;
  }
}

/** The history state of the current location, with the augmented fields. */
type LocationState = ReturnType<typeof useLocation>["state"];

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

/**
 * Marks the list's own history entry with where the user was, just before it opens a detail
 * page, so the browser's Back button restores the list as the back link does.
 */
export function markReturnPoint(history: RouterHistory, ret: ListReturn): void {
  const { href, state } = history.location;
  history.replace(href, {
    ...state,
    listRestore: {
      ...(ret.anchorId !== undefined ? { anchorId: ret.anchorId } : {}),
      ...(ret.anchorOffset !== undefined ? { anchorOffset: ret.anchorOffset } : {}),
      ...(ret.openedId !== undefined ? { openedId: ret.openedId } : {}),
    },
  });
  // The browser history batches changes: without a flush, the push that opens the detail page
  // replaces this one and the list's entry never gets the state.
  history.flush();
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

/**
 * The state a link to a detail page passes: the way back to the current location, labelled
 * `label`. Use it as the link's `state` prop.
 */
export function useBackState(label: string): (current: LocationState) => LocationState {
  const href = useLocation().href;
  return (current) => ({ ...current, backTo: { label, href } });
}

/** The way back a detail page was opened with, if any. */
export function useBackTo(): BackTo | undefined {
  return useLocation().state.backTo;
}

/**
 * Whether the page shown is `sessionId`'s own session page. A link from that page to itself must
 * keep the way back it was opened with, not replace it with a link to itself.
 */
export function useOnSessionPage(sessionId: string): boolean {
  const match = useMatch({ from: "/$org/$project/sessions/$sessionId", shouldThrow: false });
  return match?.params.sessionId === sessionId;
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
