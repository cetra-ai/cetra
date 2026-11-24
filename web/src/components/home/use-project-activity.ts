import { keepPreviousData, useQueries } from "@tanstack/react-query";
import { projectActivityQuery } from "@/api/queries";
import type { Schema } from "@/api/types";
import { formatCost } from "@/lib/format";
import { formatCount, formatPercent } from "@/lib/metrics";
import { errorRate } from "@/lib/org-home";

/** How long an activity answer is fresh, and how often the open page refetches it (OH-6). */
export const ACTIVITY_STALE_MS = 30_000;
export const ACTIVITY_REFETCH_MS = 60_000;

/** Where one project's activity stands. `none`: unavailable, failed, or left out of the answer. */
export type ActivityState =
  | { state: "loading" }
  | { state: "ready"; from: string; activity: Schema<"ProjectActivity"> }
  | { state: "none" };

/** A project's activity as text, for a card or a table row. */
export type ActivityCells = {
  traces: string;
  errorRate: string;
  cost: string;
  /** RFC 3339; absent when unknown (shown as "—"). */
  lastIngestAt?: string;
};

/**
 * The activity of the loaded projects, one request per page of the project list (OH-3).
 * Returns the lookup by project id.
 */
export function useProjectActivity(
  org: string,
  pages: readonly (readonly Schema<"Project">[])[],
): (id: string) => ActivityState {
  const chunks = pages.filter((page) => page.length > 0);
  const queries = useQueries({
    queries: chunks.map((chunk) => ({
      ...projectActivityQuery(
        org,
        chunk.map((project) => project.id),
      ),
      staleTime: ACTIVITY_STALE_MS,
      placeholderData: keepPreviousData,
      refetchInterval: ACTIVITY_REFETCH_MS,
    })),
  });

  const ready = new Map<string, { from: string; activity: Schema<"ProjectActivity"> }>();
  const loading = new Set<string>();
  chunks.forEach((chunk, index) => {
    const query = queries[index];
    if (query?.data !== undefined) {
      for (const activity of query.data.data) {
        ready.set(activity.project_id, { from: query.data.from, activity });
      }
    } else if (query?.isPending) {
      for (const project of chunk) {
        loading.add(project.id);
      }
    }
  });

  return (id) => {
    const found = ready.get(id);
    if (found !== undefined) {
      return { state: "ready", from: found.from, activity: found.activity };
    }
    return loading.has(id) ? { state: "loading" } : { state: "none" };
  };
}

/**
 * The text of a project's activity: `undefined` while loading (the caller shows skeletons),
 * dashes when the activity is `none`, else the formatted numbers.
 */
export function activityCells(state: ActivityState): ActivityCells | undefined {
  if (state.state === "loading") {
    return undefined;
  }
  if (state.state === "none") {
    return { traces: "—", errorRate: "—", cost: "—" };
  }
  const { activity } = state;
  const rate = errorRate(activity.traces, activity.errors);
  return {
    traces: formatCount(activity.traces),
    errorRate: rate === undefined ? "—" : formatPercent(rate),
    cost: formatCost(activity.cost_usd),
    ...(activity.last_ingest_at !== undefined ? { lastIngestAt: activity.last_ingest_at } : {}),
  };
}
