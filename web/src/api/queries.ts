import {
  keepPreviousData,
  type QueryClient,
  queryOptions,
  useQuery,
  useQueryClient,
} from "@tanstack/react-query";
import { $api, api } from "./client";
import { askFacet, type FacetShared } from "./facet-batch";
import { projectRef } from "./project-ref";
import type { Schema } from "./types";

export const meQuery = $api.queryOptions("get", "/api/auth/me");

export function projectListInit(org: string) {
  return {
    params: {
      path: { org },
      query: { limit: 50 },
    },
  };
}

export function projectListOptions(org: string) {
  const { queryKey } = $api.queryOptions(
    "get",
    "/api/v1/orgs/{org}/projects",
    projectListInit(org),
  );
  return {
    queryKey,
    initialPageParam: null as string | null,
    queryFn: async ({ pageParam, signal }: { pageParam: string | null; signal?: AbortSignal }) => {
      const { data, error } = await api.GET("/api/v1/orgs/{org}/projects", {
        params: {
          path: { org },
          query: { limit: 50, cursor: pageParam ?? undefined },
        },
        signal,
      });
      if (error) {
        throw error;
      }
      if (!data) {
        throw new Error("Empty project list response");
      }
      return data;
    },
    getNextPageParam: (page: Schema<"ProjectPage">) => page.next_cursor ?? undefined,
  };
}

export function projectInit(org: string, project: string) {
  return {
    params: {
      path: { project: projectRef(org, project) },
    },
  };
}

export function projectQuery(org: string, project: string) {
  return $api.queryOptions("get", "/api/v1/projects/{project}", projectInit(org, project));
}

export function traceDetailQuery(org: string, project: string, traceId: string) {
  return $api.queryOptions("get", "/api/v1/projects/{project}/traces/{trace_id}", {
    params: {
      path: { project: projectRef(org, project), trace_id: traceId },
    },
  });
}

export function schemaQuery(org: string, project: string) {
  return {
    ...$api.queryOptions("get", "/api/v1/projects/{project}/schema", {
      params: { path: { project: projectRef(org, project) } },
    }),
    staleTime: Number.POSITIVE_INFINITY,
  };
}

export function spanDetailQuery(org: string, project: string, traceId: string, spanId: string) {
  return $api.queryOptions("get", "/api/v1/projects/{project}/traces/{trace_id}/spans/{span_id}", {
    params: {
      path: {
        project: projectRef(org, project),
        trace_id: traceId,
        span_id: spanId,
      },
    },
  });
}

export function spanListOptions(
  org: string,
  project: string,
  query: {
    from: string;
    to: string;
    sort?: Schema<"SpanSort">;
    order?: Schema<"SortOrder">;
    search?: string;
    filter?: string;
    includeEval?: boolean;
    includeExperiments?: boolean;
  },
) {
  const path = { project: projectRef(org, project) };
  const listQuery = {
    limit: 100,
    from: query.from,
    to: query.to,
    ...(query.sort ? { sort: query.sort } : {}),
    ...(query.order ? { order: query.order } : {}),
    ...(query.search ? { search: query.search } : {}),
    ...(query.filter ? { filter: query.filter } : {}),
    ...(query.includeEval ? { include_eval: true } : {}),
    ...(query.includeExperiments ? { include_experiments: true } : {}),
  };
  const { queryKey } = $api.queryOptions("get", "/api/v1/projects/{project}/spans", {
    params: { path, query: listQuery },
  });
  return {
    queryKey,
    initialPageParam: null as string | null,
    queryFn: async ({ pageParam, signal }: { pageParam: string | null; signal?: AbortSignal }) => {
      const { data, error } = await api.GET("/api/v1/projects/{project}/spans", {
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
        throw new Error("Empty span list response");
      }
      return data;
    },
    getNextPageParam: (page: Schema<"SpanPage">) => page.next_cursor ?? undefined,
  };
}

export function scoresQuery(
  org: string,
  project: string,
  query: {
    trace_id?: string;
    span_id?: string;
    session_id?: string;
    rule_id?: string;
    evaluator_id?: string;
    target?: Schema<"ScoreTargetKind">;
    limit?: number;
  },
) {
  return $api.queryOptions("get", "/api/v1/projects/{project}/scores", {
    params: {
      path: { project: projectRef(org, project) },
      query,
    },
  });
}

export function sessionListOptions(
  org: string,
  project: string,
  query: {
    from: string;
    to: string;
    sort?: Schema<"SessionSort">;
    order?: Schema<"SortOrder">;
    filter?: string;
    includeEval?: boolean;
    includeExperiments?: boolean;
  },
) {
  const path = { project: projectRef(org, project) };
  const listQuery = {
    limit: 100,
    from: query.from,
    to: query.to,
    ...(query.sort ? { sort: query.sort } : {}),
    ...(query.order ? { order: query.order } : {}),
    ...(query.filter ? { filter: query.filter } : {}),
    ...(query.includeEval ? { include_eval: true } : {}),
    ...(query.includeExperiments ? { include_experiments: true } : {}),
  };
  const { queryKey } = $api.queryOptions("get", "/api/v1/projects/{project}/sessions", {
    params: { path, query: listQuery },
  });
  return {
    queryKey,
    initialPageParam: null as string | null,
    queryFn: async ({ pageParam, signal }: { pageParam: string | null; signal?: AbortSignal }) => {
      const { data, error } = await api.GET("/api/v1/projects/{project}/sessions", {
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
        throw new Error("Empty session list response");
      }
      return data;
    },
    getNextPageParam: (page: Schema<"SessionPage">) => page.next_cursor ?? undefined,
  };
}

export function sessionQuery(
  org: string,
  project: string,
  sessionId: string,
  flags: { includeEval?: boolean; includeExperiments?: boolean },
) {
  return $api.queryOptions("get", "/api/v1/projects/{project}/sessions/{session_id}", {
    params: {
      path: { project: projectRef(org, project), session_id: sessionId },
      query: {
        ...(flags.includeEval ? { include_eval: true } : {}),
        ...(flags.includeExperiments ? { include_experiments: true } : {}),
      },
    },
  });
}

export function userListOptions(
  org: string,
  project: string,
  query: {
    from: string;
    to: string;
    sort?: Schema<"UserSort">;
    order?: Schema<"SortOrder">;
    filter?: string;
    includeEval?: boolean;
    includeExperiments?: boolean;
  },
) {
  const path = { project: projectRef(org, project) };
  const listQuery = {
    limit: 100,
    from: query.from,
    to: query.to,
    ...(query.sort ? { sort: query.sort } : {}),
    ...(query.order ? { order: query.order } : {}),
    ...(query.filter ? { filter: query.filter } : {}),
    ...(query.includeEval ? { include_eval: true } : {}),
    ...(query.includeExperiments ? { include_experiments: true } : {}),
  };
  const { queryKey } = $api.queryOptions("get", "/api/v1/projects/{project}/users", {
    params: { path, query: listQuery },
  });
  return {
    queryKey,
    initialPageParam: null as string | null,
    queryFn: async ({ pageParam, signal }: { pageParam: string | null; signal?: AbortSignal }) => {
      const { data, error } = await api.GET("/api/v1/projects/{project}/users", {
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
        throw new Error("Empty user list response");
      }
      return data;
    },
    getNextPageParam: (page: Schema<"EndUserPage">) => page.next_cursor ?? undefined,
  };
}

/** The users with the highest cost in a list's scope (UP-7). */
export function topUsersQuery(
  org: string,
  project: string,
  query: {
    from: string;
    to: string;
    filter?: string;
    includeEval?: boolean;
    includeExperiments?: boolean;
    limit: number;
  },
) {
  return $api.queryOptions("get", "/api/v1/projects/{project}/users", {
    params: {
      path: { project: projectRef(org, project) },
      query: {
        limit: query.limit,
        from: query.from,
        to: query.to,
        sort: "cost_total",
        order: "desc",
        ...(query.filter ? { filter: query.filter } : {}),
        ...(query.includeEval ? { include_eval: true } : {}),
        ...(query.includeExperiments ? { include_experiments: true } : {}),
      },
    },
  });
}

export function userQuery(
  org: string,
  project: string,
  userId: string,
  flags: { from?: string; to?: string; includeEval?: boolean; includeExperiments?: boolean },
) {
  return $api.queryOptions("get", "/api/v1/projects/{project}/users/{user_id}", {
    params: {
      path: { project: projectRef(org, project), user_id: userId },
      query: {
        ...(flags.from ? { from: flags.from } : {}),
        ...(flags.to ? { to: flags.to } : {}),
        ...(flags.includeEval ? { include_eval: true } : {}),
        ...(flags.includeExperiments ? { include_experiments: true } : {}),
      },
    },
  });
}

/**
 * One facet of a list (D1). Each field has its own cache entry, kept 30 seconds; fields asked
 * together go out as one `POST /facets` (D-C-23). The server keeps none.
 */
export type FacetAsk = {
  view: Schema<"FacetView">;
  field: string;
  from: string;
  to: string;
  filter?: string;
  search?: string;
  includeEval?: boolean;
  includeExperiments?: boolean;
  q?: string;
  limit?: number;
};

/** The shared part of a facet ask, as the request body spells it, with empty parts left out. */
export function facetShared(query: FacetAsk): FacetShared {
  return {
    view: query.view,
    from: query.from,
    to: query.to,
    ...(query.filter ? { filter: query.filter } : {}),
    ...(query.search ? { search: query.search } : {}),
    ...(query.includeEval ? { include_eval: true } : {}),
    ...(query.includeExperiments ? { include_experiments: true } : {}),
  };
}

/** The field part of a facet ask, with empty parts left out. */
export function facetField(query: FacetAsk): Schema<"FacetFieldRequest"> {
  return {
    field: query.field,
    ...(query.q ? { q: query.q } : {}),
    ...(query.limit === undefined ? {} : { limit: query.limit }),
  };
}

export function facetQuery(org: string, project: string, query: FacetAsk) {
  const ref = projectRef(org, project);
  const shared = facetShared(query);
  const field = facetField(query);
  return queryOptions({
    queryKey: ["facets", ref, shared, field] as const,
    queryFn: () => askFacet(ref, shared, field),
    staleTime: 30_000,
    // Keeps the previous answer on screen while a filter, q or limit change refetches.
    placeholderData: keepPreviousData,
  });
}

/**
 * A facet the panel shows. A list fixes "now" when it opens, so coming back to it asks for a
 * slightly later window that is not cached yet. Until that answer arrives, this shows the
 * latest cached answer to the same question over a window of the same length, instead of a
 * loading state.
 */
export function useFacetQuery(
  org: string,
  project: string,
  query: FacetAsk,
  { enabled = true }: { enabled?: boolean } = {},
) {
  const client = useQueryClient();
  const options = facetQuery(org, project, query);
  return useQuery({
    ...options,
    enabled,
    placeholderData: (previous) => previous ?? latestFacetAnswer(client, options.queryKey),
  });
}

type FacetKey = ReturnType<typeof facetQuery>["queryKey"];
type FacetAnswer = Schema<"Facet">;

function windowLength(from: string | undefined, to: string | undefined): number | undefined {
  return from === undefined || to === undefined ? undefined : Date.parse(to) - Date.parse(from);
}

/** The newest cached answer whose key differs from `key` only by a window of the same length. */
export function latestFacetAnswer(client: QueryClient, key: FacetKey): FacetAnswer | undefined {
  const [, ref, shared, field] = key;
  const { from, to, ...rest } = shared;
  const length = windowLength(from, to);
  if (length === undefined) {
    return undefined;
  }
  const same = JSON.stringify([rest, field]);
  let best: { at: number; data: FacetAnswer } | undefined;
  for (const cached of client.getQueryCache().findAll({ queryKey: ["facets", ref] })) {
    const data = cached.state.data as FacetAnswer | undefined;
    if (data === undefined) {
      continue;
    }
    const [, , cachedShared, cachedField] = cached.queryKey as FacetKey;
    const { from: cachedFrom, to: cachedTo, ...cachedRest } = cachedShared;
    if (
      windowLength(cachedFrom, cachedTo) !== length ||
      JSON.stringify([cachedRest, cachedField]) !== same
    ) {
      continue;
    }
    if (best === undefined || cached.state.dataUpdatedAt > best.at) {
      best = { at: cached.state.dataUpdatedAt, data };
    }
  }
  return best?.data;
}

export function savedViewsQuery(org: string, project: string, kind: Schema<"SavedViewKind">) {
  return $api.queryOptions("get", "/api/v1/projects/{project}/saved-views", {
    params: {
      path: { project: projectRef(org, project) },
      query: { kind },
    },
  });
}

/** Page size for saved-view lists. The server allows up to 1,000 per page. */
const SAVED_VIEW_PAGE = 200;
/** Most pages one saved SQL query list may take. A list past this is an error, not a hang. */
const SAVED_QUERY_MAX_PAGES = 1_000;

/**
 * Every saved SQL query the caller can see, read page by page until `next_cursor` is null.
 * The per-owner cap (D-H-6, 1,000) does not bound this list: shared views of other members
 * count too. The result has `next_cursor: null`, so callers see one complete list.
 * A cursor that repeats, or more than `SAVED_QUERY_MAX_PAGES` pages, throws.
 */
export function savedQueriesQuery(org: string, project: string) {
  const { queryKey } = $api.queryOptions("get", "/api/v1/projects/{project}/saved-views", {
    params: {
      path: { project: projectRef(org, project) },
      query: { kind: "sql", limit: SAVED_VIEW_PAGE },
    },
  });
  return queryOptions({
    queryKey,
    queryFn: async ({ signal }) => {
      const views: Schema<"SavedView">[] = [];
      const seen = new Set<string>();
      let cursor: string | undefined;
      for (let page = 1; ; page += 1) {
        const { data, error } = await api.GET("/api/v1/projects/{project}/saved-views", {
          params: {
            path: { project: projectRef(org, project) },
            query: { kind: "sql", limit: SAVED_VIEW_PAGE, cursor },
          },
          signal,
        });
        if (error) {
          throw error;
        }
        if (!data) {
          throw new Error("Empty saved view list response");
        }
        views.push(...data.data);
        cursor = data.next_cursor ?? undefined;
        if (cursor === undefined) {
          return { data: views, next_cursor: null };
        }
        if (seen.has(cursor)) {
          throw new Error("Saved view list repeated a page cursor");
        }
        if (page >= SAVED_QUERY_MAX_PAGES) {
          throw new Error("Saved view list has too many pages");
        }
        seen.add(cursor);
      }
    },
  });
}

/** One saved view by id, for a `?view=` link that is not in the list (GET by id, D-H-13). */
export function savedViewQuery(org: string, project: string, id: string) {
  return $api.queryOptions("get", "/api/v1/projects/{project}/saved-views/{id}", {
    params: { path: { project: projectRef(org, project), id } },
  });
}
