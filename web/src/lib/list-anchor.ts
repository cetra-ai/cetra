import { useCallback, useEffect, useRef, useState } from "react";
import type { RowAnchor } from "@/lib/list-return";

/** The lists whose window is remembered on return. */
export type AnchoredList = "traces" | "spans" | "sessions" | "users";

/** How long a list's window ("now") is reused on return, from the anchor itself (D-C-23). */
export const ANCHOR_REUSE_MS = 15 * 60_000;

/** List queries keyed by a window stay fresh and cached as long as the window is reused. */
export const LIST_CACHE = { staleTime: ANCHOR_REUSE_MS, gcTime: ANCHOR_REUSE_MS } as const;

/** The new-rows check asks for at most this many rows; more reads "50+". */
export const NEW_ROWS_LIMIT = 50;

/**
 * What defines a list's window apart from the anchor, normalised by the page: `rangeKey` is the
 * page's existing `rangeKey` (`preset`, or `from\nto`, from `rangeFromSearch`), and `sort` and
 * `order` are the effective values with the page's defaults applied (so `?range=24h` and no
 * range, or `sort=start_time` and no sort, give the same key).
 */
export type WindowSearch = {
  rangeKey: string;
  filter: string | undefined;
  search: string | undefined;
  sort: string;
  order: string;
  eval: boolean;
  exp: boolean;
};

type Entry = { anchor: number; scroll?: RowAnchor };

const entries = new Map<string, Entry>();

/** Tests only: forget every remembered anchor. */
export function resetListAnchors(): void {
  entries.clear();
}

/** The store key: org, project, list and the window search. */
export function anchorKey(
  org: string,
  project: string,
  list: AnchoredList,
  search: WindowSearch,
): string {
  return `${org}/${project}/${list}\n${JSON.stringify([
    search.rangeKey,
    search.filter ?? null,
    search.search ?? null,
    search.sort,
    search.order,
    search.eval,
    search.exp,
  ])}`;
}

export type ListAnchor = {
  /** Epoch ms: the window's "now". */
  anchor: number;
  /** True when the anchor came from the store (a return), false when it was set now. */
  reused: boolean;
  /** Epoch ms when this anchor was looked up or set: the new-rows check's `to`. */
  lookedUpAt: number;
  /** The first visible row when the list was left, for a reused anchor. */
  scroll: RowAnchor | undefined;
  /** Sets a new anchor (now) for the current key: the refresh button, the pill, live on/off. */
  renew: () => void;
  /** Remembers the first visible row for the current key and anchor. */
  rememberScroll: (scroll: RowAnchor) => void;
};

type Lookup = {
  key: string;
  anchor: number;
  reused: boolean;
  lookedUpAt: number;
  scroll: RowAnchor | undefined;
};

/** Reads the store only: a lookup never writes, since React may call it twice in StrictMode. */
function lookup(key: string, now: number, fresh: boolean): Lookup {
  const entry = entries.get(key);
  if (!fresh && entry && now - entry.anchor < ANCHOR_REUSE_MS) {
    return { key, anchor: entry.anchor, reused: true, lookedUpAt: now, scroll: entry.scroll };
  }
  return { key, anchor: now, reused: false, lookedUpAt: now, scroll: undefined };
}

/**
 * The anchor of the list window `key`. While `fresh` is true the store is neither read nor
 * written (Traces live mode: live inserts rows into the cached pages, and the stream does not
 * replay what arrived while away), and the key's stored entry is deleted. A key change on a
 * mounted page looks the new key up again.
 */
export function useListAnchor(key: string, fresh: boolean): ListAnchor {
  const freshRef = useRef(fresh);
  freshRef.current = fresh;
  const [state, setState] = useState<Lookup>(() => lookup(key, Date.now(), fresh));
  // "Adjust state on prop change": a new key is looked up in this render, not the next one.
  let current = state;
  if (state.key !== key) {
    const next = lookup(key, Date.now(), fresh);
    setState(next);
    current = next;
  }

  useEffect(() => {
    if (fresh) {
      entries.delete(current.key);
      return;
    }
    const now = Date.now();
    for (const [storedKey, entry] of entries) {
      if (now - entry.anchor >= ANCHOR_REUSE_MS) {
        entries.delete(storedKey);
      }
    }
    // A stored scroll is kept when the anchor is the same.
    if (entries.get(current.key)?.anchor !== current.anchor) {
      entries.set(current.key, { anchor: current.anchor });
    }
  }, [current.key, current.anchor, fresh]);

  const renew = useCallback(() => {
    const now = Date.now();
    setState((prev) => ({
      key: prev.key,
      anchor: now,
      reused: false,
      lookedUpAt: now,
      scroll: undefined,
    }));
  }, []);

  const keyRef = useRef(current.key);
  const anchorRef = useRef(current.anchor);
  keyRef.current = current.key;
  anchorRef.current = current.anchor;

  const rememberScroll = useCallback((scroll: RowAnchor) => {
    if (freshRef.current) {
      return;
    }
    const entry = entries.get(keyRef.current);
    if (entry && entry.anchor === anchorRef.current) {
      entry.scroll = scroll;
    }
  }, []);

  return {
    anchor: current.anchor,
    reused: current.reused,
    lookedUpAt: current.lookedUpAt,
    scroll: current.scroll,
    renew,
    rememberScroll,
  };
}
