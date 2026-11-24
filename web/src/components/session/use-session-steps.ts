import { useQueries, type UseQueryResult } from "@tanstack/react-query";
import { useCallback } from "react";
import { spanListOptions } from "@/api/queries";
import type { Schema } from "@/api/types";
import {
  chunk,
  groupSteps,
  STEPS_CHUNK,
  STEPS_MAX_PAGES,
  STEPS_PAGE,
  stepsFilter,
  stepsWindow,
} from "@/lib/session-turns";

export type SessionSteps = {
  /** "pending" until every request answered once; "error" when one failed. */
  status: "pending" | "error" | "ready";
  /** A turn's steps once ready (an empty list when it has none); undefined before, or after an error. */
  stepsOf: (traceId: string) => readonly Schema<"SpanSummary">[] | undefined;
  /** A request stopped after STEPS_MAX_PAGES pages: some steps are not shown. */
  truncated: boolean;
};

/** One steps request: the spans of up to STEPS_CHUNK turns, read page by page up to STEPS_MAX_PAGES. */
export function stepsChunkQuery(
  org: string,
  project: string,
  query: {
    from: string;
    to: string;
    traceIds: readonly string[];
    includeEval: boolean;
    includeExperiments: boolean;
  },
): {
  queryKey: readonly unknown[];
  queryFn: (context: {
    signal?: AbortSignal;
  }) => Promise<{ spans: Schema<"SpanSummary">[]; truncated: boolean }>;
} {
  const base = spanListOptions(org, project, {
    from: query.from,
    to: query.to,
    limit: STEPS_PAGE,
    sort: "start_time",
    order: "asc",
    filter: stepsFilter(query.traceIds),
    includeEval: query.includeEval,
    includeExperiments: query.includeExperiments,
  });
  return {
    queryKey: [...base.queryKey, "session-steps"],
    queryFn: async ({ signal }) => {
      const spans: Schema<"SpanSummary">[] = [];
      let cursor: string | null = null;
      for (let page = 0; page < STEPS_MAX_PAGES; page += 1) {
        const result = await base.queryFn({ pageParam: cursor, signal });
        spans.push(...result.data);
        cursor = result.next_cursor ?? null;
        if (cursor === null) {
          break;
        }
      }
      return { spans, truncated: cursor !== null };
    },
  };
}

function combineSteps(
  results: readonly UseQueryResult<{ spans: Schema<"SpanSummary">[]; truncated: boolean }>[],
): {
  status: SessionSteps["status"];
  grouped: Map<string, Schema<"SpanSummary">[]>;
  truncated: boolean;
} {
  const status: SessionSteps["status"] = results.some((result) => result.isError)
    ? "error"
    : results.some((result) => result.isPending)
      ? "pending"
      : "ready";
  return {
    status,
    grouped:
      status === "ready"
        ? groupSteps(results.flatMap((result) => result.data?.spans ?? []))
        : new Map(),
    truncated: results.some((result) => result.data?.truncated === true),
  };
}

/**
 * The steps of a session's turns: one request per STEPS_CHUNK turns, asking for the step kinds of
 * those traces' spans over the session's window (D-C-28). `stepsOf` is undefined until ready.
 */
export function useSessionSteps(args: {
  org: string;
  project: string;
  session: Pick<Schema<"SessionSummary">, "start_time" | "end_time">;
  turns: readonly Schema<"SessionTurn">[];
  includeEval: boolean;
  includeExperiments: boolean;
}): SessionSteps {
  const { org, project, session, turns, includeEval, includeExperiments } = args;
  const range = stepsWindow(session.start_time, session.end_time);
  const chunks = chunk(
    turns.map((turn) => turn.trace.trace_id),
    STEPS_CHUNK,
  );
  const combined = useQueries({
    queries: chunks.map((traceIds) => ({
      ...stepsChunkQuery(org, project, {
        from: range.from,
        to: range.to,
        traceIds,
        includeEval,
        includeExperiments,
      }),
      staleTime: 30_000,
    })),
    combine: combineSteps,
  });
  const stepsOf = useCallback(
    (id: string) => (combined.status === "ready" ? (combined.grouped.get(id) ?? []) : undefined),
    [combined.status, combined.grouped],
  );
  return { status: combined.status, stepsOf, truncated: combined.truncated };
}
