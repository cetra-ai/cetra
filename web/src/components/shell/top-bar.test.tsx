import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import {
  createMemoryHistory,
  createRootRoute,
  createRoute,
  createRouter,
  Outlet,
  RouterProvider,
} from "@tanstack/react-router";
import { render, screen, within } from "@testing-library/react";
import { HttpResponse, http } from "msw";
import { type ReactNode, Suspense } from "react";
import { expect, it } from "vitest";
import { pageOf } from "@/components/shell/page-crumb";
import { TopBarLayout } from "@/components/shell/top-bar-layout";
import { TopBarCrumb } from "@/components/shell/top-bar-slot";
import { ThemeProvider } from "@/components/theme-provider";
import { server } from "@/mocks/node";
import { renderApp } from "@/test/render";
import me from "../../../../fixtures/api/static/me.json";

it("finds the page a project path is on", () => {
  expect(pageOf("/acme/support-bot/traces", "acme", "support-bot")?.label).toBe("Traces");
  expect(pageOf("/acme/support-bot/traces/abc", "acme", "support-bot")?.label).toBe("Traces");
  expect(pageOf("/acme/support-bot/dashboard/123", "acme", "support-bot")?.label).toBe("Dashboard");
  expect(pageOf("/acme/support-bot/alerts/new", "acme", "support-bot")?.label).toBe("Alerts");
});

it("finds no page for settings or a path that only shares a prefix", () => {
  expect(pageOf("/acme/support-bot/settings/general", "acme", "support-bot")).toBeUndefined();
  expect(pageOf("/acme/support-bot/tracesx", "acme", "support-bot")).toBeUndefined();
});

it("shows the breadcrumb with the page as the only heading", async () => {
  await renderApp("/acme/support-bot/traces");

  const crumb = await screen.findByRole("navigation", { name: "Breadcrumb" });
  expect(within(crumb).getByRole("button", { name: "Acme" })).toBeInTheDocument();
  expect(await within(crumb).findByRole("button", { name: "Support bot" })).toBeInTheDocument();
  expect(within(crumb).getByRole("heading", { level: 1, name: "Traces" })).toBeInTheDocument();
  expect(screen.getAllByRole("heading", { level: 1 })).toHaveLength(1);
});

it("shows no page heading on org pages", async () => {
  await renderApp("/acme");

  expect(await screen.findByRole("list", { name: "Projects" })).toBeInTheDocument();
  const crumb = screen.getByRole("navigation", { name: "Breadcrumb" });
  expect(within(crumb).queryByRole("heading")).not.toBeInTheDocument();
});

it("marks the demo org", async () => {
  server.use(
    http.get("/api/auth/me", () =>
      HttpResponse.json({
        ...me,
        orgs: me.orgs.map((org) => ({ ...org, demo: true })),
      }),
    ),
  );
  await renderApp("/acme/support-bot/traces");

  const trigger = await screen.findByRole("button", { name: "Acme" });
  expect(trigger).toHaveTextContent("Demo");
});

/** A project route whose traces page renders `child` inside the top bar layout. */
async function renderCrumb(child: ReactNode) {
  const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });
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
      <TopBarLayout>
        <Outlet />
      </TopBarLayout>
    ),
  });
  const traces = createRoute({
    getParentRoute: () => project,
    path: "traces",
    component: () => child,
  });
  const router = createRouter({
    routeTree: root.addChildren([org.addChildren([project.addChildren([traces])])]),
    history: createMemoryHistory({ initialEntries: ["/acme/support-bot/traces"] }),
  });
  render(
    <QueryClientProvider client={queryClient}>
      <ThemeProvider>
        <Suspense fallback={null}>
          <RouterProvider router={router} />
        </Suspense>
      </ThemeProvider>
    </QueryClientProvider>,
  );
  await router.load();
}

it("renders a page's crumb after the page", async () => {
  await renderCrumb(<TopBarCrumb>Item</TopBarCrumb>);

  const crumb = await screen.findByRole("navigation", { name: "Breadcrumb" });
  expect(await within(crumb).findByText("Item")).toBeInTheDocument();
});

it("TopBarCrumb renders nothing without the layout", () => {
  render(<TopBarCrumb>Item</TopBarCrumb>);
  expect(screen.queryByText("Item")).not.toBeInTheDocument();
});
