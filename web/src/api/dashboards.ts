import { keepPreviousData, queryOptions, useMutation, useQueryClient } from "@tanstack/react-query";
import { $api, api } from "./client";
import { problemOf } from "./problem";
import { projectRef } from "./project-ref";
import type { Schema } from "./types";

/** Page size of the dashboard list. The server allows up to 100. */
const DASHBOARD_PAGE = 100;
/** A list that repeats a cursor is an error, not a loop. */
const DASHBOARD_MAX_PAGES = 10;

/**
 * Every dashboard of the project, without definitions, read page by page until `next_cursor` is
 * null. A project holds at most 100 dashboards, so the result is one complete list.
 */
export function dashboardsQuery(org: string, project: string) {
  const { queryKey } = $api.queryOptions("get", "/api/v1/projects/{project}/dashboards", {
    params: { path: { project: projectRef(org, project) }, query: { limit: DASHBOARD_PAGE } },
  });
  return queryOptions({
    queryKey,
    queryFn: async ({ signal }) => {
      const dashboards: Schema<"DashboardSummary">[] = [];
      const seen = new Set<string>();
      let cursor: string | undefined;
      for (let page = 1; page <= DASHBOARD_MAX_PAGES; page += 1) {
        const { data, error } = await api.GET("/api/v1/projects/{project}/dashboards", {
          params: {
            path: { project: projectRef(org, project) },
            query: { limit: DASHBOARD_PAGE, cursor },
          },
          signal,
        });
        if (error) {
          throw error;
        }
        if (!data) {
          throw new Error("Empty dashboard list response");
        }
        dashboards.push(...data.data);
        cursor = data.next_cursor ?? undefined;
        if (cursor === undefined) {
          return { data: dashboards, next_cursor: null };
        }
        if (seen.has(cursor)) {
          throw new Error("Dashboard list repeated a page cursor");
        }
        seen.add(cursor);
      }
      throw new Error("Dashboard list has too many pages");
    },
  });
}

/** One dashboard with its definition. */
export function dashboardQuery(org: string, project: string, id: string) {
  return $api.queryOptions("get", "/api/v1/projects/{project}/dashboards/{id}", {
    params: { path: { project: projectRef(org, project), id } },
  });
}

/** The path of the widget query endpoint (spec 05 § Dashboard queries). */
export const WIDGET_QUERY_PATH = "/api/v1/projects/{project}/dashboards/query" as const;

/** How long a widget answer is fresh: the server's cache lifetime. */
const WIDGET_STALE_MS = 10_000;

/**
 * One widget's answer. The previous answer stays on screen while a new one (another window or
 * filter) is loading, so a range change does not blank the chart. An answer is fresh for 10 s,
 * as the server caches it for as long (spec 05 § Dashboard queries).
 */
export function widgetQuery(org: string, project: string, body: Schema<"WidgetQueryRequest">) {
  return $api.queryOptions(
    "post",
    WIDGET_QUERY_PATH,
    { params: { path: { project: projectRef(org, project) } }, body },
    { placeholderData: keepPreviousData, staleTime: WIDGET_STALE_MS },
  );
}

/** Whether a mutation failed because the dashboard changed since it was read (`409`). */
export function isStaleDashboard(error: unknown): boolean {
  return problemOf(error)?.status === 409;
}

function useInvalidateDashboards(org: string, project: string, id?: string) {
  const queryClient = useQueryClient();
  return async (): Promise<void> => {
    await queryClient.invalidateQueries({ queryKey: dashboardsQuery(org, project).queryKey });
    if (id !== undefined) {
      await queryClient.invalidateQueries({ queryKey: dashboardQuery(org, project, id).queryKey });
    }
  };
}

/** Creates a dashboard. Its body is `{name, description?, definition}`. */
export function useCreateDashboard(org: string, project: string) {
  const invalidate = useInvalidateDashboards(org, project);
  return useMutation({
    mutationFn: async (body: Schema<"CreateDashboard">): Promise<Schema<"Dashboard">> => {
      const { data, error } = await api.POST("/api/v1/projects/{project}/dashboards", {
        params: { path: { project: projectRef(org, project) } },
        body,
      });
      if (error) {
        throw error;
      }
      if (!data) {
        throw new Error("Empty dashboard response");
      }
      return data;
    },
    onSuccess: invalidate,
  });
}

/**
 * Updates a dashboard. The body names the version the caller read; a stale one fails with a
 * `409`, which `isStaleDashboard` detects.
 */
export function useUpdateDashboard(org: string, project: string, id: string) {
  const invalidate = useInvalidateDashboards(org, project, id);
  return useMutation({
    mutationFn: async (body: Schema<"UpdateDashboard">): Promise<Schema<"Dashboard">> => {
      const { data, error } = await api.PATCH("/api/v1/projects/{project}/dashboards/{id}", {
        params: { path: { project: projectRef(org, project), id } },
        body,
      });
      if (error) {
        throw error;
      }
      if (!data) {
        throw new Error("Empty dashboard response");
      }
      return data;
    },
    onSuccess: invalidate,
  });
}

/** Deletes a dashboard. */
export function useDeleteDashboard(org: string, project: string, id: string) {
  const invalidate = useInvalidateDashboards(org, project, id);
  return useMutation({
    mutationFn: async (): Promise<void> => {
      const { error } = await api.DELETE("/api/v1/projects/{project}/dashboards/{id}", {
        params: { path: { project: projectRef(org, project), id } },
      });
      if (error) {
        throw error;
      }
    },
    onSuccess: invalidate,
  });
}
