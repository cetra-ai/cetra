import { keepPreviousData, queryOptions, useMutation, useQueryClient } from "@tanstack/react-query";
import {
  type DashboardStore,
  dashboardKey,
  dashboardsKey,
  useDashboardStore,
} from "@/lib/dashboards/store";
import { $api } from "./client";
import { problemOf } from "./problem";
import { projectRef } from "./project-ref";
import type { Schema } from "./types";

/** Every dashboard of the project, without definitions. */
export function dashboardsQuery(org: string, project: string, store: DashboardStore) {
  return queryOptions({
    queryKey: dashboardsKey(org, project, store.local),
    queryFn: () => store.list(),
  });
}

/** One dashboard with its definition. */
export function dashboardQuery(org: string, project: string, id: string, store: DashboardStore) {
  return queryOptions({
    queryKey: dashboardKey(org, project, id, store.local),
    queryFn: () => store.get(id),
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

/**
 * Refreshes the list and, when given, one dashboard. The keys without "local" are prefixes of the
 * browser's keys too, so both stores are refreshed.
 */
function useInvalidateDashboards(org: string, project: string, id?: string) {
  const queryClient = useQueryClient();
  return async (): Promise<void> => {
    await queryClient.invalidateQueries({ queryKey: dashboardsKey(org, project, false) });
    if (id !== undefined) {
      await queryClient.invalidateQueries({ queryKey: dashboardKey(org, project, id, false) });
    }
  };
}

/** Creates a dashboard. Its body is `{name, description?, definition}`. */
export function useCreateDashboard(org: string, project: string) {
  const { store } = useDashboardStore(org, project);
  const invalidate = useInvalidateDashboards(org, project);
  return useMutation({
    mutationFn: (body: Parameters<DashboardStore["create"]>[0]) => store.create(body),
    onSuccess: invalidate,
  });
}

/**
 * Updates a dashboard. The body names the version the caller read; on the API a stale one fails
 * with a `409`, which `isStaleDashboard` detects.
 */
export function useUpdateDashboard(org: string, project: string, id: string) {
  const { store } = useDashboardStore(org, project);
  const invalidate = useInvalidateDashboards(org, project, id);
  return useMutation({
    mutationFn: (body: Parameters<DashboardStore["update"]>[1]) => store.update(id, body),
    onSuccess: invalidate,
  });
}

/** Deletes a dashboard. */
export function useDeleteDashboard(org: string, project: string, id: string) {
  const { store } = useDashboardStore(org, project);
  const invalidate = useInvalidateDashboards(org, project, id);
  return useMutation({
    mutationFn: () => store.remove(id),
    onSuccess: invalidate,
  });
}
