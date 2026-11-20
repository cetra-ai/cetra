import type { Schema } from "@/api/types";
import { parseSearchText } from "@/lib/datasets";

/** The queue pages refetch this often while visible, and on focus (D-AQ-4). */
export const QUEUE_POLL_MS = 10_000;
/** The reviewer renews the item's 5-minute lease this often (D-AQ-4). */
export const LEASE_RENEW_MS = 60_000;
/** The API's conflict detail for a queue name that is already taken. */
export const QUEUE_NAME_TAKEN = "an annotation queue with this name exists";

export type QueuesSearch = { q?: string };
export type QueueSearch = {
  tab?: "items" | "settings";
  status?: "pending" | "completed" | "skipped";
};
export type ReviewSearch = { after?: string };

const QUEUE_TABS = ["items", "settings"] as const;
const QUEUE_STATUSES = ["pending", "completed", "skipped"] as const;

/** The queue list's search: the name filter. Only set keys are kept. */
export function parseQueuesSearch(search: Record<string, unknown>): QueuesSearch {
  const q = parseSearchText(search.q);
  return q !== undefined ? { q } : {};
}

/** The queue page's search: the tab and the item status filter. Only set keys are kept. */
export function parseQueueSearch(search: Record<string, unknown>): QueueSearch {
  const tab = QUEUE_TABS.find((candidate) => candidate === search.tab);
  const status = QUEUE_STATUSES.find((candidate) => candidate === search.status);
  return {
    ...(tab ? { tab } : {}),
    ...(status ? { status } : {}),
  };
}

/** The reviewer's search: the item the next move came from. Only a non-empty string is kept. */
export function parseReviewSearch(search: Record<string, unknown>): ReviewSearch {
  const after = typeof search.after === "string" && search.after !== "" ? search.after : undefined;
  return after !== undefined ? { after } : {};
}

/** The kind of item a queue holds, as the page names it. */
export function targetLabel(target: Schema<"QueueTarget">): "Traces" | "Spans" | "Sessions" {
  switch (target) {
    case "trace":
      return "Traces";
    case "span":
      return "Spans";
    case "session":
      return "Sessions";
  }
}

/** Reviewed items (completed or skipped) out of all items that are reviewed or pending. */
export function progressOf(counts: Schema<"QueueCounts">): { done: number; total: number } {
  const done = counts.completed + counts.skipped;
  return { done, total: done + counts.pending };
}
