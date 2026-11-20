import { $api, api } from "@/api/client";
import { projectRef } from "@/api/project-ref";
import type { Schema } from "@/api/types";

export function traceListOptions(
  org: string,
  project: string,
  query: {
    from: string;
    to: string;
    limit?: number;
    sort?: Schema<"TraceSort">;
    order?: Schema<"SortOrder">;
    search?: string;
    filter?: string;
    includeEval?: boolean;
    includeExperiments?: boolean;
  },
) {
  const path = { project: projectRef(org, project) };
  const listQuery = {
    limit: query.limit ?? 100,
    from: query.from,
    to: query.to,
    ...(query.sort ? { sort: query.sort } : {}),
    ...(query.order ? { order: query.order } : {}),
    ...(query.search ? { search: query.search } : {}),
    ...(query.filter ? { filter: query.filter } : {}),
    ...(query.includeEval ? { include_eval: true } : {}),
    ...(query.includeExperiments ? { include_experiments: true } : {}),
  };
  const { queryKey } = $api.queryOptions("get", "/api/v1/projects/{project}/traces", {
    params: { path, query: listQuery },
  });
  return {
    queryKey,
    initialPageParam: null as string | null,
    queryFn: async ({ pageParam, signal }: { pageParam: string | null; signal?: AbortSignal }) => {
      const { data, error } = await api.GET("/api/v1/projects/{project}/traces", {
        params: {
          path,
          query: { ...listQuery, ...(pageParam ? { cursor: pageParam } : {}) },
        },
        signal,
      });
      if (error) {
        throw error;
      }
      if (!data) {
        throw new Error("Empty trace list response");
      }
      return data;
    },
    getNextPageParam: (page: Schema<"TracePage">) => page.next_cursor ?? undefined,
  };
}

/**
 * The all-time probe that tells whether a project has any trace: one row, no window and no
 * filter, so eval and experiment traces are excluded as the list excludes them by default.
 */
export function traceProbeOptions(org: string, project: string) {
  const path = { project: projectRef(org, project) };
  const query = { limit: 1 };
  const { queryKey } = $api.queryOptions("get", "/api/v1/projects/{project}/traces", {
    params: { path, query },
  });
  return {
    queryKey,
    // A cached empty answer must not show onboarding after a trace has arrived.
    staleTime: 0,
    queryFn: async ({ signal }: { signal?: AbortSignal }) => {
      const { data, error } = await api.GET("/api/v1/projects/{project}/traces", {
        params: { path, query },
        signal,
      });
      if (error) {
        throw error;
      }
      if (!data) {
        throw new Error("Empty trace list response");
      }
      return data;
    },
  };
}
