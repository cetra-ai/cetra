import { useSyncExternalStore } from "react";
import { track } from "@/lib/analytics";
import { STORAGE_PREFIX } from "@/product";

export type ListLayout = "drawer" | "bottom" | "page";

export const DEFAULT_LIST_LAYOUT: ListLayout = "drawer";

export const LIST_LAYOUTS: readonly ListLayout[] = ["drawer", "bottom", "page"];

export const LIST_LAYOUT_STORAGE_KEY = `${STORAGE_PREFIX}list_layout`;

const listeners = new Set<() => void>();
let fallback: ListLayout | null = null;

function isListLayout(value: string | null): value is ListLayout {
  return LIST_LAYOUTS.some((layout) => layout === value);
}

function readStored(): ListLayout {
  try {
    const value = localStorage.getItem(LIST_LAYOUT_STORAGE_KEY);
    if (isListLayout(value)) {
      return value;
    }
    return fallback ?? DEFAULT_LIST_LAYOUT;
  } catch {
    return fallback ?? DEFAULT_LIST_LAYOUT;
  }
}

function emit(): void {
  for (const listener of listeners) {
    listener();
  }
}

export function getListLayout(): ListLayout {
  return readStored();
}

export function setListLayout(next: ListLayout): void {
  const changed = next !== getListLayout();
  fallback = next;
  try {
    localStorage.setItem(LIST_LAYOUT_STORAGE_KEY, next);
  } catch {
    // Private mode can throw. The choice still applies for this page.
  }
  emit();
  if (changed) {
    track({ name: "list_layout.changed", properties: { layout: next } });
  }
}

function subscribe(listener: () => void): () => void {
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
  };
}

export function useListLayout(): ListLayout {
  return useSyncExternalStore(subscribe, getListLayout, () => DEFAULT_LIST_LAYOUT);
}

// Whether the open trace fills the whole list area. Not persisted: a reload shows the layout again.
let maximized = false;

export function getTraceMaximized(): boolean {
  return maximized;
}

export function setTraceMaximized(next: boolean): void {
  if (maximized === next) {
    return;
  }
  maximized = next;
  emit();
}

export function useTraceMaximized(): boolean {
  return useSyncExternalStore(subscribe, getTraceMaximized, () => false);
}
