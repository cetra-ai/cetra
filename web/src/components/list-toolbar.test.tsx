import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import {
  createMemoryHistory,
  createRootRoute,
  createRoute,
  createRouter,
  Outlet,
  RouterProvider,
} from "@tanstack/react-router";
import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { type ReactNode, Suspense, useState } from "react";
import { expect, it } from "vitest";
import { ListToolbar } from "@/components/list-toolbar";
import { TopBarLayout } from "@/components/shell/top-bar-layout";
import { ThemeProvider } from "@/components/theme-provider";
import { TooltipProvider } from "@/components/ui/tooltip";

/** The toolbar under a `/$org` route, which the time range picker reads. */
async function renderToolbar(extra: {
  filter?: boolean;
  withToggle?: boolean;
  withPresets?: boolean;
  toolbar?: ReactNode;
}) {
  const toolbar = extra.toolbar ?? (
    <TooltipProvider>
      <ListToolbar
        range={{ kind: "preset", preset: "15m" }}
        onRange={() => undefined}
        onRefresh={() => undefined}
        evalOn={false}
        onEval={() => undefined}
        expOn={false}
        onExp={() => undefined}
        filter={extra.filter ? <input aria-label="Filter node" /> : undefined}
        filtersToggle={extra.withToggle ? <button type="button">Toggle node</button> : undefined}
        presets={extra.withPresets ? <button type="button">Preset node</button> : undefined}
      />
    </TooltipProvider>
  );
  const root = createRootRoute();
  const org = createRoute({ getParentRoute: () => root, path: "$org", component: () => toolbar });
  const router = createRouter({
    routeTree: root.addChildren([org]),
    history: createMemoryHistory({ initialEntries: ["/acme"] }),
  });
  render(
    <QueryClientProvider client={new QueryClient()}>
      <RouterProvider router={router} />
    </QueryClientProvider>,
  );
  await router.load();
}

it("renders no second row without a filter", async () => {
  await renderToolbar({ withToggle: true, withPresets: true });
  expect(await screen.findByRole("button", { name: "Refresh" })).toBeInTheDocument();
  expect(screen.queryByRole("button", { name: "Toggle node" })).not.toBeInTheDocument();
  expect(screen.queryByRole("button", { name: "Preset node" })).not.toBeInTheDocument();
});

it("puts the toggle, the presets and the filter in row 2", async () => {
  await renderToolbar({ filter: true, withToggle: true, withPresets: true });
  const toggle = await screen.findByRole("button", { name: "Toggle node" });
  const row = toggle.parentElement;
  expect(row).not.toBeNull();
  expect(screen.getByRole("button", { name: "Preset node" }).parentElement).toBe(row);
  expect(screen.getByRole("textbox", { name: "Filter node" }).closest("div")?.parentElement).toBe(
    row,
  );
  // Row 1 keeps the refresh button, not the filter.
  expect(screen.getByRole("button", { name: "Refresh" }).parentElement).not.toBe(row);
});

function Harness({ calls }: { calls: string[] }) {
  const [evalOn, setEvalOn] = useState(false);
  const [expOn, setExpOn] = useState(true);
  return (
    <TooltipProvider>
      <ListToolbar
        range={{ kind: "preset", preset: "15m" }}
        onRange={() => undefined}
        onRefresh={() => undefined}
        evalOn={evalOn}
        onEval={(on) => {
          calls.push(`eval:${on}`);
          setEvalOn(on);
        }}
        expOn={expOn}
        onExp={(on) => {
          calls.push(`exp:${on}`);
          setExpOn(on);
        }}
      />
    </TooltipProvider>
  );
}

it("shows how many trace kinds are included and toggles each from the Include menu", async () => {
  const calls: string[] = [];
  await renderToolbar({ toolbar: <Harness calls={calls} /> });
  const user = userEvent.setup();
  const trigger = await screen.findByRole("button", {
    name: "Include evaluation and experiment traces, 1 on",
  });
  expect(trigger).toHaveTextContent(/^Include · 1$/);

  await user.click(trigger);
  expect(await screen.findByRole("menuitemcheckbox", { name: "Experiment traces" })).toBeChecked();
  expect(screen.getByRole("menuitemcheckbox", { name: "Evaluation traces" })).not.toBeChecked();

  await user.click(screen.getByRole("menuitemcheckbox", { name: "Evaluation traces" }));
  // The menu stays open after a toggle.
  expect(screen.getByRole("menuitemcheckbox", { name: "Evaluation traces" })).toBeChecked();
  await waitFor(() => {
    expect(trigger).toHaveTextContent(/^Include · 2$/);
  });

  await user.click(screen.getByRole("menuitemcheckbox", { name: "Experiment traces" }));
  await waitFor(() => {
    expect(trigger).toHaveTextContent(/^Include · 1$/);
  });
  expect(calls).toEqual(["eval:true", "exp:false"]);
});

it("names the Include trigger without a count when nothing is included", async () => {
  await renderToolbar({
    toolbar: (
      <TooltipProvider>
        <ListToolbar
          range={{ kind: "preset", preset: "15m" }}
          onRange={() => undefined}
          onRefresh={() => undefined}
          evalOn={false}
          onEval={() => undefined}
          expOn={false}
          onExp={() => undefined}
        />
      </TooltipProvider>
    ),
  });
  const trigger = await screen.findByRole("button", {
    name: "Include evaluation and experiment traces",
  });
  expect(trigger).toHaveTextContent(/^Include$/);
});

/** Renders `page` on a project's traces route, inside the top bar layout, so the bar has a slot. */
async function renderInTopBar(page: ReactNode) {
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
    component: () => <Outlet />,
  });
  const traces = createRoute({
    getParentRoute: () => project,
    path: "traces",
    component: () => <TopBarLayout>{page}</TopBarLayout>,
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

it("renders the views menu in the top bar slot", async () => {
  await renderInTopBar(
    <TooltipProvider>
      <ListToolbar
        range={{ kind: "preset", preset: "15m" }}
        onRange={() => undefined}
        onRefresh={() => undefined}
        evalOn={false}
        onEval={() => undefined}
        expOn={false}
        onExp={() => undefined}
        views={<span>Views</span>}
      />
    </TooltipProvider>,
  );
  const crumb = await screen.findByRole("navigation", { name: "Breadcrumb" });
  expect(await within(crumb).findByText("Views")).toBeInTheDocument();
});
