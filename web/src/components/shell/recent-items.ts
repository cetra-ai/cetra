import { useMemo, useSyncExternalStore } from "react";
import { STORAGE_PREFIX } from "@/product";

export type RecentKind = "trace" | "session" | "evaluator" | "dataset" | "experiment";
export type RecentItem = { kind: RecentKind; id: string; label: string };

type Store = Record<string, RecentItem[]>;

const STORAGE_KEY = `${STORAGE_PREFIX}recent`;
const LIMIT = 20;
const KINDS: readonly RecentKind[] = ["trace", "session", "evaluator", "dataset", "experiment"];
const EMPTY: readonly RecentItem[] = [];

function projectKey(org: string, project: string): string {
  return `${org}/${project}`;
}

function validItem(item: unknown): RecentItem | undefined {
  if (
    !item ||
    typeof item !== "object" ||
    !("kind" in item) ||
    !("id" in item) ||
    !("label" in item)
  ) {
    return undefined;
  }
  const { kind, id, label } = item;
  const known = KINDS.find((candidate) => candidate === kind);
  if (
    known === undefined ||
    typeof id !== "string" ||
    typeof label !== "string" ||
    id.length === 0 ||
    label.length === 0
  ) {
    return undefined;
  }
  return { kind: known, id, label };
}

function parseStore(raw: string | null): Store {
  if (!raw) {
    return {};
  }
  try {
    const parsed: unknown = JSON.parse(raw);
    if (!parsed || typeof parsed !== "object" || Array.isArray(parsed)) {
      return {};
    }
    const store: Store = {};
    for (const [key, value] of Object.entries(parsed)) {
      if (!Array.isArray(value)) {
        continue;
      }
      const items = value.flatMap((item) => {
        const valid = validItem(item);
        return valid ? [valid] : [];
      });
      if (items.length > 0) {
        store[key] = items.slice(0, LIMIT);
      }
    }
    return store;
  } catch {
    return {};
  }
}

// The parsed store is cached per raw string, so reads return the same arrays until
// the stored value changes. useSyncExternalStore relies on that to avoid loops.
let cachedRaw: string | null = null;
let cachedStore: Store = {};

function readStore(): Store {
  let raw: string | null;
  try {
    raw = localStorage.getItem(STORAGE_KEY);
  } catch {
    raw = null;
  }
  if (raw !== cachedRaw) {
    cachedRaw = raw;
    cachedStore = parseStore(raw);
  }
  return cachedStore;
}

function writeStore(store: Store): void {
  const raw = JSON.stringify(store);
  try {
    localStorage.setItem(STORAGE_KEY, raw);
    cachedRaw = raw;
    cachedStore = store;
  } catch {
    // Private mode can throw. The recent list then lasts for this page only.
  }
  notify();
}

const listeners = new Set<() => void>();

function notify(): void {
  for (const listener of listeners) {
    listener();
  }
}

function onStorage(event: StorageEvent): void {
  // A null key means the whole storage was cleared, in another tab.
  if (event.key === STORAGE_KEY || event.key === null) {
    notify();
  }
}

function subscribe(listener: () => void): () => void {
  listeners.add(listener);
  if (listeners.size === 1) {
    window.addEventListener("storage", onStorage);
  }
  return () => {
    listeners.delete(listener);
    if (listeners.size === 0) {
      window.removeEventListener("storage", onStorage);
    }
  };
}

function projectItems(org: string, project: string): readonly RecentItem[] {
  return readStore()[projectKey(org, project)] ?? EMPTY;
}

/** Recently opened items of one project, newest first. */
export function readRecentItems(org: string, project: string): RecentItem[] {
  return [...projectItems(org, project)];
}

/**
 * Remembers an opened item at the front of its project's list. Re-opening an item moves it there.
 * While the name is still loading the label is the id: then a previously stored name is kept.
 */
export function rememberItem(org: string, project: string, item: RecentItem): void {
  const store: Store = { ...readStore() };
  const key = projectKey(org, project);
  const existing = store[key] ?? [];
  const same = (entry: RecentItem) => entry.kind === item.kind && entry.id === item.id;
  const previous = existing.find(same);
  const label = item.label === item.id && previous ? previous.label : item.label;
  store[key] = [
    { kind: item.kind, id: item.id, label },
    ...existing.filter((entry) => !same(entry)),
  ].slice(0, LIMIT);
  writeStore(store);
}

/** Drops an item that no longer exists from every project's list. */
export function forgetItem(kind: RecentKind, id: string): void {
  const store = readStore();
  const next: Store = {};
  for (const [key, items] of Object.entries(store)) {
    const remaining = items.filter((item) => item.kind !== kind || item.id !== id);
    if (remaining.length > 0) {
      next[key] = remaining;
    }
  }
  writeStore(next);
}

/** The project's recent items, newest first, re-rendering on changes in this tab and others. */
export function useRecentItems(org: string, project: string, limit: number): RecentItem[] {
  const items = useSyncExternalStore(subscribe, () => projectItems(org, project));
  return useMemo(() => items.slice(0, limit), [items, limit]);
}
