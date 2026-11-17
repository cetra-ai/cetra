import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import {
  createMemoryHistory,
  createRootRoute,
  createRouter,
  RouterProvider,
} from "@tanstack/react-router";
import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import type { ReactNode } from "react";
import { expect, it, vi } from "vitest";
import { projectRef } from "@/api/project-ref";
import { ProjectProvider } from "@/components/shell/project-context";
import { TooltipProvider } from "@/components/ui/tooltip";
import projectFixture from "../../../../fixtures/api/static/project.json";
import { DatasetItemsEmpty, DatasetsListEmpty } from "./datasets-empty";

const ORG = "acme";
const PROJECT = "support-bot";

/** Renders the components inside the project context, with a root route for their links. */
async function renderInProject(ui: ReactNode) {
  const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  const rootRoute = createRootRoute({
    component: () => (
      <ProjectProvider
        value={{
          org: ORG,
          project: PROJECT,
          ref: projectRef(ORG, PROJECT),
          data: { ...projectFixture, role: "admin" },
        }}
      >
        <TooltipProvider>{ui}</TooltipProvider>
      </ProjectProvider>
    ),
  });
  const router = createRouter({
    routeTree: rootRoute,
    history: createMemoryHistory({ initialEntries: ["/"] }),
  });
  render(
    <QueryClientProvider client={queryClient}>
      <RouterProvider router={router} />
    </QueryClientProvider>,
  );
  await router.load();
  return { user: userEvent.setup() };
}

it("shows the three ways to create a dataset", async () => {
  await renderInProject(<DatasetsListEmpty onCreate={vi.fn()} />);

  expect(await screen.findByRole("heading", { name: "Create your first dataset" })).toBeVisible();
  expect(screen.getByRole("heading", { name: "Create" })).toBeVisible();
  expect(screen.getByRole("heading", { name: "From a trace" })).toBeVisible();
  expect(screen.getByRole("heading", { name: "From code" })).toBeVisible();
  expect(screen.getByText(/cetra\.Client\(\)/)).toBeInTheDocument();
  expect(screen.getByRole("link", { name: "Go to traces" })).toHaveAttribute(
    "href",
    `/${ORG}/${PROJECT}/traces`,
  );
});

it("calls onCreate from the list empty state", async () => {
  const onCreate = vi.fn();
  const { user } = await renderInProject(<DatasetsListEmpty onCreate={onCreate} />);

  await user.click(await screen.findByRole("button", { name: "Create dataset" }));

  expect(onCreate).toHaveBeenCalledTimes(1);
});

it("disables Create dataset for viewers, with the reason", async () => {
  await renderInProject(<DatasetsListEmpty onCreate={vi.fn()} canCreate={false} />);

  const button = await screen.findByRole("button", { name: "Create dataset" });
  expect(button).toBeDisabled();
  expect(button.parentElement).toHaveAccessibleDescription("Only members can create datasets");
});

it("calls onAddItem and onImport from the items empty state", async () => {
  const onAddItem = vi.fn();
  const onImport = vi.fn();
  const { user } = await renderInProject(
    <DatasetItemsEmpty onAddItem={onAddItem} onImport={onImport} />,
  );

  expect(await screen.findByRole("heading", { name: "Import a file" })).toBeVisible();
  await user.click(screen.getByRole("button", { name: "Add item" }));
  await user.click(screen.getByRole("button", { name: "Import a file" }));

  expect(onAddItem).toHaveBeenCalledTimes(1);
  expect(onImport).toHaveBeenCalledTimes(1);
  expect(screen.getByRole("heading", { name: "From a trace" })).toBeVisible();
});
