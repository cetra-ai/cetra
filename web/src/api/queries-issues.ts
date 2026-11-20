import { useQueryClient } from "@tanstack/react-query";
import type { IssueSort, IssueStateTab } from "@/lib/search/issues";
import { $api, api } from "./client";
import { projectRef } from "./project-ref";
import type { Schema } from "./types";

/** The filters of the Issues list. `from` and `to` fix the window of `count` and `users`. */
export type IssueListFilters = {
  state: IssueStateTab;
  source?: string;
  q?: string;
  sort: IssueSort;
  from: string;
  to: string;
};

/** Prefix of every issue list query of the project, for invalidation. */
export function issuesKey(org: string, project: string) {
  return $api.queryOptions("get", "/api/v1/projects/{project}/issues", {
    params: { path: { project: projectRef(org, project) } },
  }).queryKey;
}

/** The issues of the project, page by page. Every reader of the list uses this one shape. */
export function issuesInfiniteQuery(org: string, project: string, filters: IssueListFilters) {
  const path = { project: projectRef(org, project) };
  const listQuery = {
    limit: 50,
    state: filters.state,
    sort: filters.sort,
    from: filters.from,
    to: filters.to,
    ...(filters.source ? { source_id: filters.source } : {}),
    ...(filters.q ? { q: filters.q } : {}),
  };
  const { queryKey } = $api.queryOptions("get", "/api/v1/projects/{project}/issues", {
    params: { path, query: listQuery },
  });
  return {
    queryKey,
    initialPageParam: null as string | null,
    queryFn: async ({ pageParam, signal }: { pageParam: string | null; signal?: AbortSignal }) => {
      const { data, error } = await api.GET("/api/v1/projects/{project}/issues", {
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
        throw new Error("Empty issue list response");
      }
      return data;
    },
    getNextPageParam: (page: Schema<"IssuePage">) => page.next_cursor ?? undefined,
  };
}

export function issueInit(org: string, project: string, issueId: string) {
  return { params: { path: { project: projectRef(org, project), id: issueId } } };
}

export function issueQuery(org: string, project: string, issueId: string) {
  return $api.queryOptions(
    "get",
    "/api/v1/projects/{project}/issues/{id}",
    issueInit(org, project, issueId),
  );
}

/** The key of one issue's detail query, for invalidation. */
export function issueKey(org: string, project: string, issueId: string) {
  return issueQuery(org, project, issueId).queryKey;
}

/** An issue's occurrences per day over `window`, and its newest examples. */
export function issueOccurrencesQuery(
  org: string,
  project: string,
  issueId: string,
  window: { from: string; to: string },
) {
  return $api.queryOptions("get", "/api/v1/projects/{project}/issues/{id}/occurrences", {
    params: {
      path: { project: projectRef(org, project), id: issueId },
      query: { from: window.from, to: window.to },
    },
  });
}

/** An issue's history, newest first. */
export function issueEventsQuery(org: string, project: string, issueId: string) {
  return $api.queryOptions(
    "get",
    "/api/v1/projects/{project}/issues/{id}/events",
    issueInit(org, project, issueId),
  );
}

/** The key of one issue's history query, for invalidation. */
export function issueEventsKey(org: string, project: string, issueId: string) {
  return issueEventsQuery(org, project, issueId).queryKey;
}

/** The project's issue sources, for the source filter. */
export function issueSourcesQuery(org: string, project: string) {
  return $api.queryOptions("get", "/api/v1/projects/{project}/issue-sources", {
    params: { path: { project: projectRef(org, project) }, query: { limit: 100 } },
  });
}

/** Edits an issue's title, description or state. Refreshes the issue, the list and its history. */
export function usePatchIssue(org: string, project: string) {
  const queryClient = useQueryClient();
  return $api.useMutation("patch", "/api/v1/projects/{project}/issues/{id}", {
    onSuccess: (_issue, variables) => {
      const issueId = variables.params.path.id;
      void queryClient.invalidateQueries({ queryKey: issueKey(org, project, issueId) });
      void queryClient.invalidateQueries({ queryKey: issuesKey(org, project) });
      void queryClient.invalidateQueries({ queryKey: issueEventsKey(org, project, issueId) });
    },
  });
}

/**
 * Merges an issue into another one of its source. Refreshes both issues, the list and the
 * merged issue's history.
 */
export function useMergeIssue(org: string, project: string) {
  const queryClient = useQueryClient();
  return $api.useMutation("post", "/api/v1/projects/{project}/issues/{id}/merge", {
    onSuccess: (_issue, variables) => {
      const issueId = variables.params.path.id;
      void queryClient.invalidateQueries({ queryKey: issueKey(org, project, issueId) });
      void queryClient.invalidateQueries({
        queryKey: issueKey(org, project, variables.body.into),
      });
      void queryClient.invalidateQueries({ queryKey: issuesKey(org, project) });
      void queryClient.invalidateQueries({ queryKey: issueEventsKey(org, project, issueId) });
    },
  });
}
