import { queryOptions, useQueryClient } from "@tanstack/react-query";
import type { IssueSort, IssueStateTab } from "@/lib/search/issues";
import { $api, api } from "./client";
import { projectRef } from "./project-ref";
import type { Schema } from "./types";

/** Prefix of the score name an issue occurrence is written under (D-IC-18). */
const ISSUE_SCORE_PREFIX = "issue:";

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

/** The score name an issue occurrence is written under (D-IC-18): `issue:<issue id>`. */
export function issueScoreName(issueId: string): string {
  return `${ISSUE_SCORE_PREFIX}${issueId}`;
}

/** The issue id of an issue score's name, or undefined for any other score. */
export function issueIdFromScoreName(name: string): string | undefined {
  return name.startsWith(ISSUE_SCORE_PREFIX) ? name.slice(ISSUE_SCORE_PREFIX.length) : undefined;
}

/** The most recent open issues, for the "Has issue" menu of the trace filter. */
export function openIssuesQuery(org: string, project: string) {
  return $api.queryOptions("get", "/api/v1/projects/{project}/issues", {
    params: { path: { project: projectRef(org, project) }, query: { state: "open", limit: 20 } },
  });
}

/**
 * The issues a trace has occurrences of: its `issue` scores, then the issues they name, in one
 * list. Merged issues are included, so their chip can show the merge.
 */
export function traceIssuesQuery(org: string, project: string, traceId: string) {
  const path = { project: projectRef(org, project) };
  return queryOptions({
    queryKey: ["trace-issues", org, project, traceId],
    queryFn: async ({ signal }): Promise<Schema<"Issue">[]> => {
      const scores = await api.GET("/api/v1/projects/{project}/scores", {
        params: { path, query: { trace_id: traceId, source: "issue", limit: 100 } },
        signal,
      });
      if (scores.error) {
        throw scores.error;
      }
      if (!scores.data) {
        throw new Error("Empty score list response");
      }
      const ids = scores.data.data
        .map((score) => issueIdFromScoreName(score.name))
        .filter((id): id is string => id !== undefined);
      if (ids.length === 0) {
        return [];
      }
      const issues = await api.GET("/api/v1/projects/{project}/issues", {
        params: {
          path,
          query: { state: "open,resolved,ignored,merged", ids: ids.join(","), limit: 100 },
        },
        signal,
      });
      if (issues.error) {
        throw issues.error;
      }
      if (!issues.data) {
        throw new Error("Empty issue list response");
      }
      return issues.data.data;
    },
  });
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

/** Prefix of every issue source list query of the project, for invalidation. */
export function issueSourcesKey(org: string, project: string) {
  return $api.queryOptions("get", "/api/v1/projects/{project}/issue-sources", {
    params: { path: { project: projectRef(org, project) } },
  }).queryKey;
}

/** One issue source. */
export function issueSourceQuery(org: string, project: string, sourceId: string) {
  return $api.queryOptions(
    "get",
    "/api/v1/projects/{project}/issue-sources/{id}",
    issueInit(org, project, sourceId),
  );
}

/** The key of one issue source's detail query, for invalidation. */
export function issueSourceKey(org: string, project: string, sourceId: string) {
  return issueSourceQuery(org, project, sourceId).queryKey;
}

/** One source's refits, newest first (the newest 50). */
export function issueRefitsQuery(org: string, project: string, sourceId: string) {
  return $api.queryOptions(
    "get",
    "/api/v1/projects/{project}/issue-sources/{id}/refits",
    issueInit(org, project, sourceId),
  );
}

/** The key of one source's refit list, for invalidation. */
export function issueRefitsKey(org: string, project: string, sourceId: string) {
  return issueRefitsQuery(org, project, sourceId).queryKey;
}

/** Asks for a refit of an LLM source now. Refreshes the source, the list and its refits. */
export function useRefitIssueSource(org: string, project: string) {
  const queryClient = useQueryClient();
  return $api.useMutation("post", "/api/v1/projects/{project}/issue-sources/{id}/refit", {
    onSuccess: (_source, variables) => {
      const sourceId = variables.params.path.id;
      void queryClient.invalidateQueries({ queryKey: issueSourceKey(org, project, sourceId) });
      void queryClient.invalidateQueries({ queryKey: issueSourcesKey(org, project) });
      void queryClient.invalidateQueries({ queryKey: issueRefitsKey(org, project, sourceId) });
    },
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

/** Creates an issue source. Refreshes the source list. */
export function useCreateIssueSource(org: string, project: string) {
  const queryClient = useQueryClient();
  return $api.useMutation("post", "/api/v1/projects/{project}/issue-sources", {
    onSuccess: () => {
      void queryClient.invalidateQueries({ queryKey: issueSourcesKey(org, project) });
    },
  });
}

/** Edits a source's name, filter or enabled flag. Refreshes the source and the list. */
export function usePatchIssueSource(org: string, project: string) {
  const queryClient = useQueryClient();
  return $api.useMutation("patch", "/api/v1/projects/{project}/issue-sources/{id}", {
    onSuccess: (_source, variables) => {
      const sourceId = variables.params.path.id;
      void queryClient.invalidateQueries({ queryKey: issueSourceKey(org, project, sourceId) });
      void queryClient.invalidateQueries({ queryKey: issueSourcesKey(org, project) });
    },
  });
}

/** Deletes a source and its issues. Refreshes the source list and the issues. */
export function useDeleteIssueSource(org: string, project: string) {
  const queryClient = useQueryClient();
  return $api.useMutation("delete", "/api/v1/projects/{project}/issue-sources/{id}", {
    onSuccess: (_result, variables) => {
      const sourceId = variables.params.path.id;
      void queryClient.invalidateQueries({ queryKey: issueSourceKey(org, project, sourceId) });
      void queryClient.invalidateQueries({ queryKey: issueSourcesKey(org, project) });
      void queryClient.invalidateQueries({ queryKey: issuesKey(org, project) });
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
