import { forgetItem, readRecentItems, rememberItem } from "@/components/shell/recent-items";

export type RecentTrace = { id: string; name: string };

const LIMIT = 10;

/** Traces recently opened in a project, newest first. Reads the shared recent-items store. */
export function readRecent(org: string, project: string): RecentTrace[] {
  return readRecentItems(org, project)
    .filter((item) => item.kind === "trace")
    .slice(0, LIMIT)
    .map((item) => ({ id: item.id, name: item.label }));
}

/** Remembers an opened trace. Stores the id and name only. */
export function rememberTrace(org: string, project: string, trace: RecentTrace): void {
  rememberItem(org, project, { kind: "trace", id: trace.id, label: trace.name });
}

/** Drops a deleted trace from every project's list. */
export function forgetTrace(traceId: string): void {
  forgetItem("trace", traceId);
}

/** Trace id currently open in the split (`?trace=`) or on the full page. */
export function openTraceId(pathname: string, search: unknown): string | undefined {
  const fromPath = /\/traces\/([0-9a-f]{32})$/.exec(pathname)?.[1];
  if (fromPath) {
    return fromPath;
  }
  if (!search || typeof search !== "object" || !("trace" in search)) {
    return undefined;
  }
  const trace = search.trace;
  return typeof trace === "string" && /^[0-9a-f]{32}$/.test(trace) ? trace : undefined;
}
