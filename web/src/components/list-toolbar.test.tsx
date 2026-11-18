import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import {
  createMemoryHistory,
  createRootRoute,
  createRoute,
  createRouter,
  RouterProvider,
} from "@tanstack/react-router";
import { render, screen } from "@testing-library/react";
import { expect, it } from "vitest";
import { ListToolbar } from "@/components/list-toolbar";
import { TooltipProvider } from "@/components/ui/tooltip";

/** The toolbar under a `/$org` route, which the time range picker reads. */
async function renderToolbar(extra: {
  filter?: boolean;
  withToggle?: boolean;
  withPresets?: boolean;
}) {
  const toolbar = (
    <TooltipProvider>
      <ListToolbar
        title="Traces"
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
  expect(await screen.findByRole("heading", { name: "Traces" })).toBeInTheDocument();
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
  // Row 1 keeps the title, not the filter.
  expect(screen.getByRole("heading", { name: "Traces" }).parentElement).not.toBe(row);
});
