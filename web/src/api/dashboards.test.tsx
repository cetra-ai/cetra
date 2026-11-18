import { QueryClient, QueryClientProvider, useQuery } from "@tanstack/react-query";
import { renderHook, waitFor } from "@testing-library/react";
import { HttpResponse, http } from "msw";
import type { ReactNode } from "react";
import { expect, it } from "vitest";
import { dashboardHandlers, seedDashboards } from "@/mocks/dashboards";
import { server } from "@/mocks/node";
import { problemResponse } from "@/test/problem";
import dashboardDocument from "../../../fixtures/api/static/dashboard.json";
import {
  dashboardsQuery,
  isStaleDashboard,
  useCreateDashboard,
  useDeleteDashboard,
  useUpdateDashboard,
  widgetQuery,
} from "./dashboards";

const ORG = "acme";
const PROJECT = "support-bot";
const ID = dashboardDocument.id;
const LIST = "/api/v1/projects/:project/dashboards";

function clientFor() {
  return new QueryClient({
    defaultOptions: { queries: { retry: false, gcTime: Infinity, staleTime: Infinity } },
  });
}

function wrapperFor(queryClient: QueryClient) {
  return ({ children }: { children: ReactNode }) => (
    <QueryClientProvider client={queryClient}>{children}</QueryClientProvider>
  );
}

it("reads every page of the list until next_cursor is null", async () => {
  const seen: (string | null)[] = [];
  server.use(
    http.get(LIST, ({ request }) => {
      const cursor = new URL(request.url).searchParams.get("cursor");
      seen.push(cursor);
      if (cursor === null) {
        return HttpResponse.json({ data: [seedDashboards()[0]], next_cursor: "p2" });
      }
      return HttpResponse.json({
        data: [
          { ...seedDashboards()[0], id: "01900000-0000-7000-8000-000000000f02", name: "Second" },
        ],
        next_cursor: null,
      });
    }),
  );
  const queryClient = clientFor();
  const { result } = renderHook(() => useQuery(dashboardsQuery(ORG, PROJECT)), {
    wrapper: wrapperFor(queryClient),
  });
  await waitFor(() => expect(result.current.isSuccess).toBe(true));
  expect(seen).toEqual([null, "p2"]);
  expect(result.current.data?.data.map((item) => item.name)).toEqual([
    "Support overview",
    "Second",
  ]);
});

it("creates a dashboard and refreshes the list", async () => {
  server.use(...dashboardHandlers(seedDashboards()));
  const queryClient = clientFor();
  const list = dashboardsQuery(ORG, PROJECT);
  // An observed query is refetched by invalidation; the cache holds the new list.
  renderHook(() => useQuery(list), { wrapper: wrapperFor(queryClient) });
  await waitFor(() => expect(queryClient.getQueryData(list.queryKey)).toBeDefined());
  const { result } = renderHook(() => useCreateDashboard(ORG, PROJECT), {
    wrapper: wrapperFor(queryClient),
  });
  await result.current.mutateAsync({ name: "Costs", definition: { widgets: [] } });
  const names = queryClient.getQueryData(list.queryKey);
  expect(names?.data.map((item) => item.name)).toContain("Costs");
});

it("sends the version it read and detects a 409 as stale", async () => {
  server.use(...dashboardHandlers(seedDashboards()));
  const { result } = renderHook(() => useUpdateDashboard(ORG, PROJECT, ID), {
    wrapper: wrapperFor(clientFor()),
  });
  await result.current.mutateAsync({ version: 3, name: "Renamed" });
  const error = await result.current
    .mutateAsync({ version: 3, name: "Again" })
    .catch((caught) => caught);
  expect(isStaleDashboard(error)).toBe(true);
});

it("reports a non-409 failure as not stale", async () => {
  server.use(
    http.patch(`/api/v1/projects/:project/dashboards/:id`, () =>
      problemResponse(400, "bad_request", { errors: [{ field: "name", message: "bad" }] }),
    ),
  );
  const { result } = renderHook(() => useUpdateDashboard(ORG, PROJECT, ID), {
    wrapper: wrapperFor(clientFor()),
  });
  const error = await result.current
    .mutateAsync({ version: 3, name: "" })
    .catch((caught) => caught);
  expect(isStaleDashboard(error)).toBe(false);
});

it("deletes a dashboard", async () => {
  server.use(...dashboardHandlers(seedDashboards()));
  const { result } = renderHook(() => useDeleteDashboard(ORG, PROJECT, ID), {
    wrapper: wrapperFor(clientFor()),
  });
  await result.current.mutateAsync();
  await expect(result.current.mutateAsync()).rejects.toBeDefined();
});

it("keeps a widget answer fresh for 10 s, the server's cache lifetime", () => {
  const options = widgetQuery(ORG, PROJECT, {
    from: "2026-01-01T00:00:00.000Z",
    to: "2026-01-02T00:00:00.000Z",
    shape: "total",
    query: { source: "traces", measures: [{ agg: "count" }] },
  });
  expect(options.staleTime).toBe(10_000);
});
