import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import {
  createMemoryHistory,
  createRootRoute,
  createRoute,
  createRouter,
  Outlet,
  RouterProvider,
} from "@tanstack/react-router";
import { act, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { HttpResponse, http } from "msw";
import { Suspense } from "react";
import { afterEach, expect, it, vi } from "vitest";
import { alertRulesQuery } from "@/api/queries-alerts";
import { server } from "@/mocks/node";
import { AlertBell } from "./alert-bell";

const ALERT_RULES = "*/api/v1/projects/:project/alert-rules";

function alertRule(id: string, name: string, enabled: boolean) {
  return {
    id,
    name,
    enabled,
    condition: { metric: "cost", window: "24h", comparator: "gt", threshold: 50 },
    channel_ids: [],
    cooldown_s: 3600,
    notify_while_firing: false,
    state: "firing",
    last_fired_at: "2026-11-18T11:50:00.000000000Z",
    created_at: "2026-01-01T09:00:00.000000000Z",
    updated_at: "2026-01-01T09:00:00.000000000Z",
  };
}

afterEach(() => {
  vi.useRealTimers();
});

/** Renders the bell in a project route, with an alerts route to navigate to. */
async function renderBell() {
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
    component: () => (
      <>
        <AlertBell org="acme" project="support-bot" />
        <Outlet />
      </>
    ),
  });
  const alerts = createRoute({
    getParentRoute: () => project,
    path: "alerts",
    component: () => <p>Alerts page</p>,
  });
  const router = createRouter({
    routeTree: root.addChildren([org.addChildren([project.addChildren([alerts])])]),
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
  return { router, queryClient, user: userEvent.setup() };
}

it("is absent when no enabled rule fires", async () => {
  server.use(
    http.get(ALERT_RULES, () =>
      HttpResponse.json({
        data: [alertRule("01900000-0000-7000-8000-000000000512", "Daily cost", false)],
        next_cursor: null,
      }),
    ),
  );
  const { queryClient } = await renderBell();

  await waitFor(() =>
    expect(queryClient.getQueryState(alertRulesQuery("acme", "support-bot").queryKey)?.status).toBe(
      "success",
    ),
  );
  expect(screen.queryByRole("button", { name: /alert/ })).not.toBeInTheDocument();
});

it("shows the count and lists the firing rules with links to them", async () => {
  server.use(
    http.get(ALERT_RULES, () =>
      HttpResponse.json({
        data: [
          alertRule("01900000-0000-7000-8000-000000000001", "Daily cost", true),
          alertRule("01900000-0000-7000-8000-000000000002", "Low helpfulness", true),
        ],
        next_cursor: null,
      }),
    ),
  );
  const { user } = await renderBell();

  await user.click(await screen.findByRole("button", { name: "2 alert rules firing" }));
  const dialog = await screen.findByRole("dialog");
  expect(within(dialog).getByText("Firing alerts")).toBeInTheDocument();
  const dailyCost = within(dialog).getByRole("link", { name: /Daily cost/ });
  expect(dailyCost).toHaveAttribute(
    "href",
    "/acme/support-bot/alerts?rule=01900000-0000-7000-8000-000000000001",
  );
  expect(dailyCost).toHaveTextContent("fired");
  expect(within(dialog).getByRole("link", { name: /Low helpfulness/ })).toHaveAttribute(
    "href",
    "/acme/support-bot/alerts?rule=01900000-0000-7000-8000-000000000002",
  );
});

it("the All alerts link goes to the alerts page and closes the popover", async () => {
  server.use(
    http.get(ALERT_RULES, () =>
      HttpResponse.json({
        data: [alertRule("01900000-0000-7000-8000-000000000001", "Daily cost", true)],
        next_cursor: null,
      }),
    ),
  );
  const { user, router } = await renderBell();

  await user.click(await screen.findByRole("button", { name: "1 alert rule firing" }));
  const dialog = await screen.findByRole("dialog");
  await user.click(within(dialog).getByRole("link", { name: "All alerts" }));

  expect(await screen.findByText("Alerts page")).toBeInTheDocument();
  expect(router.state.location.pathname).toBe("/acme/support-bot/alerts");
  expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
});

it("keeps showing the last known count when a refetch fails", async () => {
  server.use(
    http.get(ALERT_RULES, () =>
      HttpResponse.json({
        data: [alertRule("01900000-0000-7000-8000-000000000001", "Daily cost", true)],
        next_cursor: null,
      }),
    ),
  );
  vi.useFakeTimers({ shouldAdvanceTime: true });
  await renderBell();
  await screen.findByRole("button", { name: "1 alert rule firing" });

  let failures = 0;
  server.use(
    http.get(ALERT_RULES, () => {
      failures += 1;
      return HttpResponse.json({ error: "unavailable" }, { status: 500 });
    }),
  );
  // Past the 60 s refetch.
  await act(async () => {
    await vi.advanceTimersByTimeAsync(61_000);
  });

  expect(failures).toBeGreaterThan(0);
  expect(screen.getByRole("button", { name: "1 alert rule firing" })).toBeInTheDocument();
});
