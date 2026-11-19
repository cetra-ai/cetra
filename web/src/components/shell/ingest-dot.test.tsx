import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import {
  createMemoryHistory,
  createRootRoute,
  createRoute,
  createRouter,
  Outlet,
  RouterProvider,
} from "@tanstack/react-router";
import { act, render, screen } from "@testing-library/react";
import { HttpResponse, http } from "msw";
import { Suspense } from "react";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { server } from "@/mocks/node";
import { IngestDot } from "./ingest-dot";

const NOW = Date.parse("2026-11-18T12:00:00.000Z");
const PROJECT = "*/api/v1/projects/:project";

beforeEach(() => {
  vi.useFakeTimers({ shouldAdvanceTime: true });
  vi.setSystemTime(NOW);
});

afterEach(() => {
  vi.useRealTimers();
});

/** Renders the dot in a project route, with the project answer the test sets. */
async function renderDot(lastIngestAt: string | undefined) {
  server.use(
    http.get(PROJECT, () =>
      HttpResponse.json({
        id: "01900000-0000-7000-8000-000000000001",
        name: "support-bot",
        ...(lastIngestAt === undefined ? {} : { last_ingest_at: lastIngestAt }),
      }),
    ),
  );
  const queryClient = new QueryClient({
    defaultOptions: { queries: { retry: false, gcTime: Infinity, staleTime: Infinity } },
  });
  const root = createRootRoute({ component: () => <Outlet /> });
  const org = createRoute({
    getParentRoute: () => root,
    path: "$org",
    component: () => <Outlet />,
  });
  const project = createRoute({
    getParentRoute: () => org,
    path: "$project",
    component: () => <IngestDot org="acme" project="support-bot" />,
  });
  const traces = createRoute({
    getParentRoute: () => project,
    path: "traces",
    component: () => null,
  });
  const router = createRouter({
    routeTree: root.addChildren([org.addChildren([project.addChildren([traces])])]),
    history: createMemoryHistory({ initialEntries: ["/acme/support-bot"] }),
  });
  render(
    <QueryClientProvider client={queryClient}>
      <Suspense fallback={null}>
        <RouterProvider router={router} />
      </Suspense>
    </QueryClientProvider>,
  );
  await router.load();
}

it("is green and says when, when spans arrived in the last 5 minutes", async () => {
  await renderDot(new Date(NOW - 2 * 60_000).toISOString());

  const link = await screen.findByRole("link", {
    name: "Receiving spans · last 2m ago",
  });
  expect(link).toHaveAttribute("href", "/acme/support-bot/traces");
  expect(link.querySelector("span")).toHaveClass("bg-emerald-500");
});

it("is grey after 5 minutes", async () => {
  await renderDot(new Date(NOW - 6 * 60_000).toISOString());

  const link = await screen.findByRole("link", {
    name: "No spans in the last 5 minutes · last 6m ago",
  });
  expect(link.querySelector("span")).toHaveClass("bg-muted-foreground/40");
});

it("says no spans since the start when the field is absent", async () => {
  await renderDot(undefined);

  expect(
    await screen.findByRole("link", { name: "No spans received since the server started" }),
  ).toBeInTheDocument();
});

it("turns grey without a refetch as time passes", async () => {
  await renderDot(new Date(NOW - 4 * 60_000).toISOString());
  await screen.findByRole("link", { name: "Receiving spans · last 4m ago" });

  // Past the five-minute window: the 15 s clock redraws the dot.
  await act(async () => {
    await vi.advanceTimersByTimeAsync(75_000);
  });

  expect(
    screen.getByRole("link", { name: "No spans in the last 5 minutes · last 5m ago" }),
  ).toBeInTheDocument();
});

it("keeps showing the last known state when a refetch fails", async () => {
  await renderDot(new Date(NOW - 2 * 60_000).toISOString());
  await screen.findByRole("link", { name: "Receiving spans · last 2m ago" });

  let failures = 0;
  server.use(
    http.get(PROJECT, () => {
      failures += 1;
      return HttpResponse.json({ error: "unavailable" }, { status: 500 });
    }),
  );
  // Past the 30 s refetch.
  await act(async () => {
    await vi.advanceTimersByTimeAsync(31_000);
  });

  expect(failures).toBeGreaterThan(0);
  expect(screen.getByRole("link", { name: /spans/ })).toBeInTheDocument();
});
