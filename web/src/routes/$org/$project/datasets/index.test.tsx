import { act, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { HttpResponse, http } from "msw";
import { expect, it } from "vitest";
import type { Schema } from "@/api/types";
import { server } from "@/mocks/node";
import { renderApp } from "@/test/render";
import datasetsFixture from "../../../../../../fixtures/api/responses/datasets.json";
import projectFixture from "../../../../../../fixtures/api/static/project.json";

function firstDataset() {
  const dataset = datasetsFixture.data[0];
  if (!dataset) {
    throw new Error("the datasets fixture has no dataset");
  }
  return dataset;
}

it("lists the datasets with their items, version and experiments", async () => {
  const dataset = firstDataset();
  await renderApp("/acme/support-bot/datasets");

  const grid = await screen.findByRole("grid", { name: "Datasets" });
  const row = within(grid).getByText(dataset.name).closest('[role="row"]');
  if (!(row instanceof HTMLElement)) {
    throw new Error(`no row named ${dataset.name}`);
  }
  expect(within(row).getByText(String(dataset.item_count))).toBeInTheDocument();
  expect(within(row).getByText(`v${dataset.current_version}`)).toBeInTheDocument();
  expect(within(row).getByText(String(dataset.experiment_count))).toBeInTheDocument();
  expect(within(row).getByText(dataset.description ?? "")).toBeInTheDocument();
});

it("shows the summary strip with the sums of the loaded datasets", async () => {
  const dataset = firstDataset();
  await renderApp("/acme/support-bot/datasets");

  await screen.findByRole("grid", { name: "Datasets" });
  const summary = screen.getByRole("region", { name: "Summary" });
  expect(within(summary).getByText("Datasets").parentElement).toHaveTextContent("Datasets 1");
  expect(within(summary).getByText("Items").parentElement).toHaveTextContent(
    `Items ${dataset.item_count}`,
  );
  expect(within(summary).getByText("Experiments").parentElement).toHaveTextContent(
    `Experiments ${dataset.experiment_count}`,
  );
});

it("opens the dataset panel when a row is clicked, through the dataset search param", async () => {
  const user = userEvent.setup();
  const dataset = firstDataset();
  const { router } = await renderApp("/acme/support-bot/datasets");

  const grid = await screen.findByRole("grid", { name: "Datasets" });
  await user.click(within(grid).getByText(dataset.name));

  await waitFor(() => expect(router.state.location.search).toMatchObject({ dataset: dataset.id }));
  expect(await screen.findByRole("region", { name: "Dataset" })).toBeInTheDocument();
});

it("shows the how-to empty state when the project has no datasets", async () => {
  server.use(
    http.get("/api/v1/projects/:project/datasets", () =>
      HttpResponse.json({ data: [], next_cursor: null }),
    ),
  );

  await renderApp("/acme/support-bot/datasets");

  expect(await screen.findByText("Create your first dataset")).toBeInTheDocument();
  expect(screen.getByRole("button", { name: "Create dataset" })).toBeInTheDocument();
  expect(screen.queryByRole("grid", { name: "Datasets" })).toBeNull();
});

it("searches by name, sending the query after a pause", async () => {
  const user = userEvent.setup();
  const queries: (string | null)[] = [];
  server.use(
    http.get("/api/v1/projects/:project/datasets", ({ request }) => {
      queries.push(new URL(request.url).searchParams.get("q"));
      return HttpResponse.json(datasetsFixture);
    }),
  );

  await renderApp("/acme/support-bot/datasets");
  await screen.findByRole("grid", { name: "Datasets" });
  await user.type(screen.getByRole("textbox", { name: "Search datasets" }), "faq");

  await waitFor(() => expect(queries).toContain("faq"));
});

it("opens a new dataset after creating it", async () => {
  const user = userEvent.setup();
  server.use(
    http.post("/api/v1/projects/:project/datasets", () =>
      HttpResponse.json({ ...firstDataset(), name: "golden" }, { status: 201 }),
    ),
  );

  const { router } = await renderApp("/acme/support-bot/datasets");
  await screen.findByRole("grid", { name: "Datasets" });
  await user.click(screen.getByRole("button", { name: "New dataset" }));
  await user.type(screen.getByRole("textbox", { name: "Name" }), "golden");
  await user.click(screen.getByRole("button", { name: "Create dataset" }));

  await waitFor(() =>
    expect(router.state.location.pathname).toBe(`/acme/support-bot/datasets/${firstDataset().id}`),
  );
});

it("disables New dataset for viewers, with the reason", async () => {
  server.use(
    http.get("/api/v1/projects/:project", () =>
      HttpResponse.json({ ...projectFixture, role: "viewer" }),
    ),
  );

  await renderApp("/acme/support-bot/datasets");

  await screen.findByRole("grid", { name: "Datasets" });
  const button = screen.getByRole("button", { name: "New dataset" });
  expect(button).toBeDisabled();
  expect(button.parentElement).toHaveAccessibleDescription("Only members can create datasets");
});

it("follows the search text when the URL changes", async () => {
  const { router } = await renderApp("/acme/support-bot/datasets?q=faq");

  const search = await screen.findByRole("textbox", { name: "Search datasets" });
  expect(search).toHaveValue("faq");
  await act(async () => {
    await router.navigate({
      to: "/$org/$project/datasets",
      params: { org: "acme", project: "support-bot" },
      search: { q: "golden", dataset: undefined },
    });
  });

  await waitFor(() => expect(search).toHaveValue("golden"));
});

const TREND_PATH = "/api/v1/projects/:project/dataset-trends";

function experimentTrend(
  id: string,
  name: string,
  scores: Schema<"ExperimentScoreMean">[],
): Schema<"ExperimentTrend"> {
  return {
    id,
    name,
    status: "completed",
    created_at: "2026-01-01T00:00:00.000000000Z",
    result_count: 10,
    error_count: 1,
    scores,
  };
}

it("shows the last experiment, its latest score and the score over the experiments", async () => {
  const dataset = firstDataset();
  server.use(
    http.get(TREND_PATH, () =>
      HttpResponse.json({
        data: [
          {
            dataset_id: dataset.id,
            experiments: [
              experimentTrend("01900000-0000-7000-8000-000000000e02", "prompt v3", [
                { name: "accuracy", data_type: "numeric", mean: 0.873, count: 10 },
              ]),
              experimentTrend("01900000-0000-7000-8000-000000000e01", "prompt v2", [
                { name: "accuracy", data_type: "numeric", mean: 0.5, count: 10 },
              ]),
            ],
          },
        ],
        scores_unavailable: false,
      }),
    ),
  );

  await renderApp("/acme/support-bot/datasets");

  const grid = await screen.findByRole("grid", { name: "Datasets" });
  const row = within(grid).getByText(dataset.name).closest('[role="row"]');
  if (!(row instanceof HTMLElement)) {
    throw new Error(`no row named ${dataset.name}`);
  }
  expect(await within(row).findByText("0.87")).toBeInTheDocument();
  expect(within(row).getByText("prompt v3")).toBeInTheDocument();
  expect(
    within(row).getByRole("img", { name: "accuracy over the last experiments" }),
  ).toBeInTheDocument();
});

it("shows a dash with the reason when the scores are unavailable", async () => {
  const user = userEvent.setup();
  const dataset = firstDataset();
  server.use(
    http.get(TREND_PATH, () =>
      HttpResponse.json({
        data: [
          {
            dataset_id: dataset.id,
            experiments: [experimentTrend("01900000-0000-7000-8000-000000000e01", "prompt v2", [])],
          },
        ],
        scores_unavailable: true,
      }),
    ),
  );

  await renderApp("/acme/support-bot/datasets");

  const grid = await screen.findByRole("grid", { name: "Datasets" });
  const row = within(grid).getByText(dataset.name).closest('[role="row"]');
  if (!(row instanceof HTMLElement)) {
    throw new Error(`no row named ${dataset.name}`);
  }
  const dash = await within(row).findByText("—");
  await user.hover(dash);
  expect((await screen.findAllByText("Scores are unavailable right now")).length).toBeGreaterThan(
    0,
  );
});

it("asks for the trends of the datasets of each loaded page, by their ids", async () => {
  const dataset = firstDataset();
  const requested: (string | null)[] = [];
  server.use(
    http.get(TREND_PATH, ({ request }) => {
      requested.push(new URL(request.url).searchParams.get("ids"));
      return HttpResponse.json({
        data: [{ dataset_id: dataset.id, experiments: [] }],
        scores_unavailable: false,
      });
    }),
  );

  await renderApp("/acme/support-bot/datasets");

  await screen.findByRole("grid", { name: "Datasets" });
  await waitFor(() => expect(requested).toContain(dataset.id));
});
