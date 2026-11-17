import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import {
  createMemoryHistory,
  createRootRoute,
  createRouter,
  RouterProvider,
} from "@tanstack/react-router";
import { render, screen } from "@testing-library/react";
import { expect, it } from "vitest";
import { projectRef } from "@/api/project-ref";
import type { Schema } from "@/api/types";
import { KIND_LABELS } from "@/components/evaluators/kind";
import { ProjectProvider } from "@/components/shell/project-context";
import { ThemeProvider } from "@/components/theme-provider";
import evaluatorFixture from "../../../../fixtures/api/static/evaluator.json";
import projectFixture from "../../../../fixtures/api/static/project.json";
import { EvaluatorCell } from "./evaluator-cell";

const ORG = "acme";
const PROJECT = "support-bot";
const EVALUATOR_ID = "01900000-0000-7000-8000-000000000500";

/** The cell needs the router for its link and the project context for the org and project. */
async function renderCell(props: { evaluator: Schema<"Evaluator"> | undefined; pending: boolean }) {
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
        <EvaluatorCell evaluatorId={EVALUATOR_ID} {...props} />
      </ProjectProvider>
    ),
  });
  const router = createRouter({
    routeTree: rootRoute,
    history: createMemoryHistory({ initialEntries: ["/"] }),
  });
  render(
    <QueryClientProvider client={new QueryClient()}>
      <ThemeProvider>
        <RouterProvider router={router} />
      </ThemeProvider>
    </QueryClientProvider>,
  );
  await router.load();
}

it("shows a skeleton while the evaluator loads", async () => {
  await renderCell({ evaluator: undefined, pending: true });

  expect(document.querySelector('[data-slot="skeleton"]')).not.toBeNull();
  expect(screen.queryByRole("link")).toBeNull();
});

it("links the evaluator with its name and kind", async () => {
  const evaluator: Schema<"Evaluator"> = {
    ...evaluatorFixture,
    kind: "llm_judge",
    config: {},
    output_score: { ...evaluatorFixture.output_score, data_type: "numeric" },
  };
  await renderCell({ evaluator, pending: false });

  const link = await screen.findByRole("link", { name: /Relevance/ });
  expect(link).toHaveAttribute("href", `/acme/support-bot/evaluators/${EVALUATOR_ID}`);
  expect(link).toHaveTextContent(KIND_LABELS[evaluator.kind]);
});

it("says the evaluator is unknown when it is not found", async () => {
  await renderCell({ evaluator: undefined, pending: false });

  expect(screen.getByText("Unknown evaluator")).toHaveAttribute("title", EVALUATOR_ID);
});
