import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import {
  createMemoryHistory,
  createRootRoute,
  createRouter,
  RouterProvider,
} from "@tanstack/react-router";
import { render, screen, waitFor, within } from "@testing-library/react";
import { HttpResponse, http } from "msw";
import { expect, it } from "vitest";
import { projectRef } from "@/api/project-ref";
import type { Schema } from "@/api/types";
import { ProjectProvider } from "@/components/shell/project-context";
import { server } from "@/mocks/node";
import projectFixture from "../../../../fixtures/api/static/project.json";
import { DatasetPanel } from "./dataset-panel";

const ORG = "acme";
const PROJECT = "support-bot";
const PROJECT_PATH = "/api/v1/projects/:project";
const DATASET_ID = "01900000-0000-7000-8000-000000000c01";

const dataset: Schema<"Dataset"> = {
  id: DATASET_ID,
  name: "faq",
  description: "Support questions and expected answers",
  input_schema: {},
  current_version: 2,
  item_count: 7,
  experiment_count: 2,
  created_at: "2026-01-01T00:00:00.000000000Z",
  updated_at: "2026-01-01T00:00:00.000000000Z",
};

function items(count: number): Schema<"DatasetItem">[] {
  return Array.from({ length: count }, (_, index) => ({
    id: `01900000-0000-7000-8000-0000000000${String(index + 10).padStart(2, "0")}`,
    revision_id: `01900000-0000-7000-8000-0000000001${String(index + 10).padStart(2, "0")}`,
    version: 1,
    input: { question: `Question ${index + 1}` },
    metadata: {},
    created_at: "2026-01-01T00:00:00.000000000Z",
  }));
}

const experiments: Schema<"Experiment">[] = [
  {
    id: "01900000-0000-7000-8000-000000000e01",
    name: "prompt v3",
    dataset_name: "faq",
    dataset_version: 2,
    item_count: 7,
    metadata: {},
    repetitions: 2,
    result_count: 14,
    status: "completed",
    created_at: "2026-01-01T00:00:00.000000000Z",
  },
  {
    id: "01900000-0000-7000-8000-000000000e02",
    name: "gpt-4.1 baseline",
    dataset_name: "faq",
    dataset_version: 1,
    item_count: 7,
    metadata: {},
    repetitions: 1,
    result_count: 3,
    status: "failed",
    error: "provider timeout",
    created_at: "2026-01-01T00:00:00.000000000Z",
    finished_at: "2026-01-01T00:05:00.000000000Z",
  },
];

const versions: Schema<"DatasetVersion">[] = [
  {
    version: 2,
    description: "edit item 01900000-0000-7000-8000-000000000d02",
    created_at: "2026-01-01T00:00:00.000000000Z",
    added: 0,
    edited: 1,
    deleted: 0,
  },
  {
    version: 1,
    description: "add 3 items",
    created_at: "2026-01-01T00:00:00.000000000Z",
    added: 3,
    edited: 0,
    deleted: 0,
  },
];

/** The first `limit` rows of a list request, as the server pages them. */
function limited<T>(rows: T[], request: Request): T[] {
  const limit = Number(new URL(request.url).searchParams.get("limit") ?? rows.length);
  return rows.slice(0, limit);
}

/** Answers the four reads of the panel with the given dataset, items, experiments and versions. */
function answerWith(answer: { dataset?: Response; items?: Schema<"DatasetItem">[] } = {}) {
  server.use(
    http.get(
      `${PROJECT_PATH}/datasets/:dataset`,
      () => answer.dataset ?? HttpResponse.json(dataset),
    ),
    http.get(`${PROJECT_PATH}/datasets/:dataset/items`, ({ request }) =>
      HttpResponse.json({ data: limited(answer.items ?? items(7), request), next_cursor: null }),
    ),
    http.get(`${PROJECT_PATH}/datasets/:dataset/versions`, ({ request }) =>
      HttpResponse.json({ data: limited(versions, request), next_cursor: null }),
    ),
    http.get(`${PROJECT_PATH}/experiments`, ({ request }) =>
      HttpResponse.json({ data: limited(experiments, request), next_cursor: null }),
    ),
  );
}

/** Renders the panel inside the project context, with a root route for its links. */
async function renderPanel(options: { row?: Schema<"Dataset">; onClose?: () => void } = {}) {
  const queryClient = new QueryClient({
    defaultOptions: { queries: { retry: false, gcTime: Infinity, staleTime: Infinity } },
  });
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
        <DatasetPanel
          datasetId={DATASET_ID}
          row={options.row}
          layout={<span>Layout controls</span>}
          onClose={options.onClose ?? (() => undefined)}
        />
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
}

it("shows the dataset's facts, description and layout controls", async () => {
  answerWith();
  await renderPanel();

  const panel = await screen.findByRole("region", { name: "Dataset" });
  await waitFor(() => {
    expect(within(panel).getByText("faq")).toBeInTheDocument();
  });
  const header = panel.querySelector("header") as HTMLElement;
  expect(within(header).getByText("Layout controls")).toBeInTheDocument();
  expect(within(panel).getByText("Support questions and expected answers")).toBeInTheDocument();
  const facts = within(panel.querySelector("dl") as HTMLElement);
  expect(facts.getByText("Items").nextElementSibling).toHaveTextContent("7");
  expect(facts.getByText("Version").nextElementSibling).toHaveTextContent("v2");
  expect(facts.getByText("Experiments").nextElementSibling).toHaveTextContent("2");
  expect(facts.getByText("Input schema").nextElementSibling).toHaveTextContent("set");
  expect(facts.getByText("Expected schema").nextElementSibling).toHaveTextContent("none");
});

it("shows the first five items, as previews", async () => {
  answerWith();
  await renderPanel();

  const panel = await screen.findByRole("region", { name: "Dataset" });
  expect(await within(panel).findByText("question: Question 1")).toBeInTheDocument();
  expect(within(panel).getByText("question: Question 5")).toBeInTheDocument();
  expect(within(panel).getByRole("link", { name: "question: Question 1" })).toHaveAttribute(
    "href",
    `/acme/support-bot/datasets/${DATASET_ID}?item=01900000-0000-7000-8000-000000000010`,
  );
  expect(within(panel).queryByText("question: Question 6")).toBeNull();
  expect(within(panel).getByRole("link", { name: "View all 7 items" })).toHaveAttribute(
    "href",
    `/acme/support-bot/datasets/${DATASET_ID}?tab=items`,
  );
});

it("shows the first experiments with their status and results, and the run button", async () => {
  answerWith();
  await renderPanel();

  const panel = await screen.findByRole("region", { name: "Dataset" });
  const link = await within(panel).findByRole("link", { name: "prompt v3" });
  expect(link).toHaveAttribute(
    "href",
    "/acme/support-bot/experiments/01900000-0000-7000-8000-000000000e01",
  );
  expect(within(panel).getByText("Completed")).toBeInTheDocument();
  expect(within(panel).getByText("Failed")).toBeInTheDocument();
  expect(within(panel).getByText("14/7 results")).toBeInTheDocument();
  expect(within(panel).getByRole("button", { name: "Run experiment" })).toBeInTheDocument();
});

it("shows the recent versions with their descriptions", async () => {
  answerWith();
  await renderPanel();

  const panel = await screen.findByRole("region", { name: "Dataset" });
  const heading = await within(panel).findByRole("heading", { name: "Recent versions" });
  const recent = within(heading.parentElement as HTMLElement);
  expect(await recent.findByText("add 3 items")).toBeInTheDocument();
  expect(recent.getByText("v2")).toBeInTheDocument();
});

it("links Open page to the dataset page", async () => {
  answerWith();
  await renderPanel();

  const panel = await screen.findByRole("region", { name: "Dataset" });
  expect(await within(panel).findByRole("link", { name: "Open page" })).toHaveAttribute(
    "href",
    `/acme/support-bot/datasets/${DATASET_ID}`,
  );
});

it("shows the list row until the fetch answers, then says when the dataset is gone", async () => {
  answerWith({
    dataset: HttpResponse.json(
      { type: "about:blank", title: "Not Found", status: 404, code: "not_found" },
      { status: 404, headers: { "Content-Type": "application/problem+json" } },
    ),
  });
  await renderPanel({ row: dataset });

  const panel = await screen.findByRole("region", { name: "Dataset" });
  expect(within(panel).getByText("faq")).toBeInTheDocument();
  expect(await within(panel).findByText("This dataset no longer exists.")).toBeInTheDocument();
});

it("shows the score trend of the experiments, each point linking to its experiment", async () => {
  answerWith();
  server.use(
    http.get(`${PROJECT_PATH}/dataset-trends`, () =>
      HttpResponse.json({
        data: [
          {
            dataset_id: DATASET_ID,
            experiments: [
              {
                id: experiments[0]?.id,
                name: "prompt v3",
                status: "completed",
                created_at: "2026-01-01T00:00:00.000000000Z",
                result_count: 14,
                error_count: 0,
                scores: [{ name: "accuracy", data_type: "numeric", mean: 0.873, count: 14 }],
              },
              {
                id: experiments[1]?.id,
                name: "gpt-4.1 baseline",
                status: "failed",
                created_at: "2026-01-01T00:00:00.000000000Z",
                result_count: 3,
                error_count: 1,
                scores: [],
              },
            ],
          },
        ],
        scores_unavailable: false,
      }),
    ),
  );
  await renderPanel();

  const panel = await screen.findByRole("region", { name: "Dataset" });
  const link = await within(panel).findByRole("link", { name: "prompt v3: 0.87" });
  expect(link).toHaveAttribute("href", `/acme/support-bot/experiments/${experiments[0]?.id}`);
  expect(within(panel).getByText("0.87")).toBeInTheDocument();
  expect(
    within(panel).getByRole("img", { name: "accuracy over the last experiments" }),
  ).toBeInTheDocument();
  expect(
    within(panel).getByRole("img", { name: "Error rate over the last experiments" }),
  ).toBeInTheDocument();
});
