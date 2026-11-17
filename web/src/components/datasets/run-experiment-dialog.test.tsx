import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import {
  createMemoryHistory,
  createRootRoute,
  createRouter,
  RouterProvider,
} from "@tanstack/react-router";
import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { expect, it } from "vitest";
import { projectRef } from "@/api/project-ref";
import { ProjectProvider } from "@/components/shell/project-context";
import projectFixture from "../../../../fixtures/api/static/project.json";
import { RunExperimentDialog } from "./run-experiment-dialog";

const ORG = "acme";
const PROJECT = "support-bot";
const DATASET_ID = "01900000-0000-7000-8000-000000000c01";

/** Renders the dialog inside the project context, with a root route for its link. */
async function renderDialog(datasetName: string) {
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
        <RunExperimentDialog datasetId={DATASET_ID} datasetName={datasetName} />
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

it("opens a dialog with the code for the dataset", async () => {
  const { user } = await renderDialog("faq");

  await user.click(screen.getByRole("button", { name: "Run experiment" }));

  const dialog = await screen.findByRole("dialog", { name: "Run an experiment on faq" });
  expect(within(dialog).getByText(/Experiments run from your code/)).toBeInTheDocument();
  const code = dialog.querySelector("pre code")?.textContent ?? "";
  expect(code).toContain('dataset="faq"');
});

it("links to the experiments of the dataset", async () => {
  const { user } = await renderDialog("faq");

  await user.click(screen.getByRole("button", { name: "Run experiment" }));

  const link = await screen.findByRole("link", { name: "Experiments of this dataset" });
  expect(link).toHaveAttribute("href", `/acme/support-bot/datasets/${DATASET_ID}?tab=experiments`);
});
