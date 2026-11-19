import { useQueries } from "@tanstack/react-query";
import { experimentSummariesQuery } from "@/api/queries-experiments";
import type { Schema } from "@/api/types";
import { useProject } from "@/components/shell/project-context";
import { isLiveExperiment, LIVE_POLL_MS, summaryChunks } from "@/lib/experiments";

/** Where one experiment's summary stands. */
export type SummaryState =
  | { state: "loading" }
  | { state: "ready"; summary: Schema<"ExperimentSummary"> }
  | { state: "skipped" }
  | { state: "missing" };

/**
 * The summaries of `rows` (newest first), loaded in chunks of 25 with `useQueries`. A chunk with a
 * running experiment refetches every `LIVE_POLL_MS`. Components that ask with the same rows share
 * the same queries. Returns the lookup by experiment id: `missing` is an unknown or hidden id, or a
 * failed chunk.
 */
export function useExperimentSummaries(
  rows: readonly Schema<"Experiment">[],
): (id: string) => SummaryState {
  const { org, project } = useProject();
  const chunks = summaryChunks(rows);
  const queries = useQueries({
    queries: chunks.map((chunk) => {
      const now = Date.now();
      return {
        ...experimentSummariesQuery(
          org,
          project,
          chunk.map((row) => row.id),
        ),
        refetchInterval: chunk.some((row) => isLiveExperiment(row, now)) ? LIVE_POLL_MS : false,
      };
    }),
  });

  const ready = new Map<string, Schema<"ExperimentSummary">>();
  const skipped = new Set<string>();
  const loading = new Set<string>();
  chunks.forEach((chunk, index) => {
    const query = queries[index];
    if (query?.data !== undefined) {
      for (const summary of query.data.data) {
        ready.set(summary.experiment.id, summary);
      }
      for (const id of query.data.skipped) {
        skipped.add(id);
      }
    } else if (query?.isPending) {
      for (const row of chunk) {
        loading.add(row.id);
      }
    }
  });

  return (id) => {
    const summary = ready.get(id);
    if (summary !== undefined) {
      return { state: "ready", summary };
    }
    if (skipped.has(id)) {
      return { state: "skipped" };
    }
    return loading.has(id) ? { state: "loading" } : { state: "missing" };
  };
}
