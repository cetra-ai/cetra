import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { HttpResponse, http } from "msw";
import { expect, it } from "vitest";
import { server } from "@/mocks/node";
import { renderApp } from "@/test/render";
import datasetsFixture from "../../../../../../fixtures/api/responses/datasets.json";
import comparisonFixture from "../../../../../../fixtures/api/static/experiment-comparison.json";
import experimentsFixture from "../../../../../../fixtures/api/static/experiments.json";

const BASELINE_ID = "01900000-0000-7000-8000-000000000e01";
const CANDIDATE_ID = "01900000-0000-7000-8000-000000000e02";
const EXPERIMENTS_PATH = "/api/v1/projects/:project/experiments";
const COMPARE_PATH = "/api/v1/projects/:project/experiments/compare";

function firstDataset() {
  const dataset = datasetsFixture.data[0];
  if (!dataset) {
    throw new Error("the datasets fixture has no dataset");
  }
  return dataset;
}

function fixtureExperiment(id: string) {
  const experiment = experimentsFixture.data.find((candidate) => candidate.id === id);
  if (!experiment) {
    throw new Error(`the experiments fixture has no ${id}`);
  }
  return experiment;
}

function rowOf(name: string): HTMLElement {
  const table = screen.getByRole("table", { name: "Experiments" });
  const row = within(table).getByText(name).closest("tr");
  if (!row) {
    throw new Error(`no row named ${name}`);
  }
  return row;
}

it("lists the experiments", async () => {
  await renderApp("/acme/support-bot/experiments");

  const table = await screen.findByRole("table", { name: "Experiments" });
  for (const fixture of experimentsFixture.data) {
    const row = rowOf(fixture.name);
    expect(within(row).getByRole("link", { name: fixture.name })).toHaveAttribute(
      "href",
      `/acme/support-bot/experiments/${fixture.id}`,
    );
    expect(
      within(row).getByText(`${fixture.dataset_name} v${fixture.dataset_version}`),
    ).toBeInTheDocument();
    expect(within(row).getByText("Completed")).toBeInTheDocument();
    expect(
      within(row).getByText(
        `${fixture.result_count} / ${fixture.item_count * fixture.repetitions}`,
      ),
    ).toBeInTheDocument();
  }
  expect(within(table).getAllByRole("row").slice(1)).toHaveLength(experimentsFixture.data.length);
});

it("filters by dataset", async () => {
  const user = userEvent.setup();
  const requested: (string | null)[] = [];
  server.use(
    http.get(EXPERIMENTS_PATH, ({ request }) => {
      requested.push(new URL(request.url).searchParams.get("dataset"));
      return HttpResponse.json(experimentsFixture);
    }),
  );
  const { router } = await renderApp("/acme/support-bot/experiments");
  await screen.findByRole("table", { name: "Experiments" });

  await user.click(screen.getByRole("combobox", { name: "Dataset" }));
  await user.click(await screen.findByRole("option", { name: firstDataset().name }));

  await waitFor(() => expect(requested).toContain(firstDataset().id));
  await waitFor(() => expect(router.state.location.href).toContain(`dataset=${firstDataset().id}`));
});

it("takes the first pick as the baseline and enables Compare for two of one dataset", async () => {
  const user = userEvent.setup();
  await renderApp("/acme/support-bot/experiments");
  await screen.findByRole("table", { name: "Experiments" });

  await user.click(screen.getByRole("checkbox", { name: "Select candidate" }));
  expect(screen.getByText("Baseline: candidate")).toBeInTheDocument();
  expect(screen.getByRole("button", { name: "Compare" })).toBeDisabled();
  await user.click(screen.getByRole("checkbox", { name: "Select baseline" }));
  expect(screen.getByText("Baseline: candidate")).toBeInTheDocument();
  expect(screen.getByRole("button", { name: "Compare" })).toBeEnabled();
});

it("needs two experiments of one dataset", async () => {
  const user = userEvent.setup();
  const first = fixtureExperiment(CANDIDATE_ID);
  server.use(
    http.get(EXPERIMENTS_PATH, () =>
      HttpResponse.json({
        data: [
          first,
          { ...fixtureExperiment(BASELINE_ID), dataset_id: "01900000-0000-7000-8000-00000000cc99" },
        ],
        next_cursor: null,
      }),
    ),
  );
  await renderApp("/acme/support-bot/experiments");
  await screen.findByRole("table", { name: "Experiments" });

  await user.click(screen.getByRole("checkbox", { name: "Select candidate" }));
  await user.click(screen.getByRole("checkbox", { name: "Select baseline" }));

  const button = screen.getByRole("button", { name: "Compare" });
  expect(button).toBeDisabled();
  expect(button.parentElement).toHaveAccessibleDescription("Pick experiments of the same dataset");
});

it("allows at most four picks", async () => {
  const user = userEvent.setup();
  const base = fixtureExperiment(BASELINE_ID);
  const experiments = ["a", "b", "c", "d", "e"].map((letter, index) => ({
    ...base,
    id: `01900000-0000-7000-8000-00000000000${index}`,
    name: `run-${letter}`,
  }));
  server.use(
    http.get(EXPERIMENTS_PATH, () => HttpResponse.json({ data: experiments, next_cursor: null })),
  );
  await renderApp("/acme/support-bot/experiments");
  await screen.findByRole("table", { name: "Experiments" });

  for (const letter of ["a", "b", "c", "d"]) {
    await user.click(screen.getByRole("checkbox", { name: `Select run-${letter}` }));
  }

  expect(screen.getByRole("checkbox", { name: "Select run-e" })).toBeDisabled();
});

it("shows a deleted dataset without a link", async () => {
  server.use(
    http.get(EXPERIMENTS_PATH, () =>
      HttpResponse.json({
        data: [{ ...fixtureExperiment(BASELINE_ID), dataset_id: undefined }],
        next_cursor: null,
      }),
    ),
  );
  await renderApp("/acme/support-bot/experiments");
  await screen.findByRole("table", { name: "Experiments" });

  const row = rowOf("baseline");
  expect(within(row).getByText("(deleted)")).toBeInTheDocument();
  expect(within(row).queryByRole("link", { name: /faq/ })).toBeNull();
});

it("shows the snippet when the project has no experiments", async () => {
  server.use(http.get(EXPERIMENTS_PATH, () => HttpResponse.json({ data: [], next_cursor: null })));
  await renderApp("/acme/support-bot/experiments");

  expect(await screen.findByText("No experiments")).toBeInTheDocument();
  expect(screen.getByText(/cetra\.evaluate\(/, { selector: "code" })).toBeInTheDocument();
});

it("deletes an experiment after confirmation", async () => {
  const user = userEvent.setup();
  const deleted: string[] = [];
  server.use(
    http.delete(`${EXPERIMENTS_PATH}/:id`, ({ params }) => {
      deleted.push(String(params.id));
      return new HttpResponse(null, { status: 204 });
    }),
  );
  await renderApp("/acme/support-bot/experiments");
  await screen.findByRole("table", { name: "Experiments" });

  await user.click(screen.getByRole("button", { name: "Actions of baseline" }));
  await user.click(await screen.findByRole("menuitem", { name: "Delete" }));
  const dialog = await screen.findByRole("alertdialog");
  await user.click(within(dialog).getByRole("button", { name: "Delete" }));

  await waitFor(() => expect(deleted).toEqual([BASELINE_ID]));
});

it("opens the comparison with the picks in order", async () => {
  const user = userEvent.setup();
  const requested: (string | null)[] = [];
  server.use(
    http.get(COMPARE_PATH, ({ request }) => {
      requested.push(new URL(request.url).searchParams.get("ids"));
      return HttpResponse.json(comparisonFixture);
    }),
  );
  const { router } = await renderApp("/acme/support-bot/experiments");
  await screen.findByRole("table", { name: "Experiments" });

  await user.click(screen.getByRole("checkbox", { name: "Select candidate" }));
  await user.click(screen.getByRole("checkbox", { name: "Select baseline" }));
  await user.click(screen.getByRole("button", { name: "Compare" }));

  await waitFor(() => expect(router.state.location.pathname).toMatch(/\/experiments\/compare$/));
  const href = decodeURIComponent(router.state.location.href);
  expect(href).toContain(`ids=${CANDIDATE_ID},${BASELINE_ID}`);
  await waitFor(() => expect(requested).toContain(`${CANDIDATE_ID},${BASELINE_ID}`));
});
