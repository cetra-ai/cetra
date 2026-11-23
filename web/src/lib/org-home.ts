import { useSyncExternalStore } from "react";
import { STORAGE_PREFIX } from "@/product";

/** How the org home shows its projects (D-C-27). */
export type ProjectView = "cards" | "table";

export const PROJECT_VIEWS: readonly ProjectView[] = ["cards", "table"];

export const DEFAULT_PROJECT_VIEW: ProjectView = "cards";

/** `cetra.org_home_view`: one preference per browser, for every org. */
export const PROJECT_VIEW_STORAGE_KEY = `${STORAGE_PREFIX}org_home_view`;

/** One hour in milliseconds: the width of an activity bucket. */
export const HOUR_MS = 3_600_000;

const listeners = new Set<() => void>();
let fallback: ProjectView | null = null;

function isProjectView(value: string | null): value is ProjectView {
  return PROJECT_VIEWS.some((view) => view === value);
}

function readStored(): ProjectView {
  try {
    const value = localStorage.getItem(PROJECT_VIEW_STORAGE_KEY);
    return isProjectView(value) ? value : DEFAULT_PROJECT_VIEW;
  } catch {
    // Only when storage throws (private mode) does the in-memory choice stand in for it.
    return fallback ?? DEFAULT_PROJECT_VIEW;
  }
}

function emit(): void {
  for (const listener of listeners) {
    listener();
  }
}

export function getProjectView(): ProjectView {
  return readStored();
}

export function setProjectView(next: ProjectView): void {
  fallback = next;
  try {
    localStorage.setItem(PROJECT_VIEW_STORAGE_KEY, next);
  } catch {
    // Private mode can throw. The choice still applies for this page.
  }
  emit();
}

function subscribe(listener: () => void): () => void {
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
  };
}

export function useProjectView(): ProjectView {
  return useSyncExternalStore(subscribe, getProjectView, () => DEFAULT_PROJECT_VIEW);
}

/**
 * The projects whose name or slug contains `query`, ignoring case and the query's surrounding
 * spaces, in their order. A blank query keeps them all.
 */
export function filterProjects<T extends { name: string; slug: string }>(
  projects: readonly T[],
  query: string,
): T[] {
  const needle = query.trim().toLowerCase();
  if (needle === "") {
    return [...projects];
  }
  return projects.filter(
    (project) =>
      project.name.toLowerCase().includes(needle) || project.slug.toLowerCase().includes(needle),
  );
}

/** `errors / traces`, or undefined when there are no traces. */
export function errorRate(traces: number, errors: number): number | undefined {
  return traces > 0 ? errors / traces : undefined;
}
