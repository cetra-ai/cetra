import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { HttpResponse, http } from "msw";
import { expect, it } from "vitest";
import { server } from "@/mocks/node";
import { STORAGE_PREFIX } from "@/product";
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

const DATASET_ID = "01900000-0000-7000-8000-000000000c01";
const TREND_KEY = `${STORAGE_PREFIX}experiments.trend`;

it("shows score badges with the change against the previous run", async () => {
  await renderApp("/acme/support-bot/experiments");
  await screen.findByRole("table", { name: "Experiments" });

  const row = rowOf("candidate");
  expect(await within(row).findByText("exact 33.3 %")).toBeInTheDocument();
  expect(within(row).getByText("quality 0.500")).toBeInTheDocument();
  expect(within(row).getByText("−33.3 pts")).toBeInTheDocument();
  expect(within(row).getByText("−0.167")).toBeInTheDocument();
  expect(within(rowOf("baseline")).queryByText("−33.3 pts")).toBeNull();
});

it("shows the metadata of each experiment as chips", async () => {
  await renderApp("/acme/support-bot/experiments");
  await screen.findByRole("table", { name: "Experiments" });

  const row = rowOf("candidate");
  expect(within(row).getByText("model gpt-5-mini")).toBeInTheDocument();
  expect(within(row).getByText("prompt v2")).toBeInTheDocument();
});

it("filters the rows by status from the summary counts", async () => {
  const user = userEvent.setup();
  const failed = { ...fixtureExperiment(BASELINE_ID), status: "failed" as const };
  server.use(
    http.get(EXPERIMENTS_PATH, () =>
      HttpResponse.json({ data: [fixtureExperiment(CANDIDATE_ID), failed], next_cursor: null }),
    ),
  );
  const { router } = await renderApp("/acme/support-bot/experiments");
  const table = await screen.findByRole("table", { name: "Experiments" });

  await user.click(screen.getByRole("button", { name: "Failed 1" }));

  await waitFor(() => expect(router.state.location.href).toContain("status=failed"));
  expect(within(table).queryByText("candidate")).toBeNull();
  expect(within(table).getByText("baseline")).toBeInTheDocument();
  expect(screen.getByRole("button", { name: "Failed 1" })).toHaveAttribute("aria-pressed", "true");
});

it("opens an experiment from a row click, but not from its link", async () => {
  const user = userEvent.setup();
  const { router } = await renderApp("/acme/support-bot/experiments");
  await screen.findByRole("table", { name: "Experiments" });

  await user.click(within(rowOf("candidate")).getByText(/^\d+ \/ \d+$/));

  await waitFor(() => expect(router.state.location.href).toContain(`experiment=${CANDIDATE_ID}`));
});

it("shows a trend for one dataset and remembers when it is closed", async () => {
  const user = userEvent.setup();
  await renderApp(`/acme/support-bot/experiments?dataset=${DATASET_ID}`);
  await screen.findByRole("table", { name: "Experiments" });

  const toggle = await screen.findByRole("button", { name: "Trend" });
  expect(toggle).toHaveAttribute("aria-expanded", "true");
  await user.click(toggle);
  expect(toggle).toHaveAttribute("aria-expanded", "false");
  expect(localStorage.getItem(TREND_KEY)).toBe("closed");
  localStorage.removeItem(TREND_KEY);
});

it("hides the trend without a dataset filter", async () => {
  await renderApp("/acme/support-bot/experiments");
  await screen.findByRole("table", { name: "Experiments" });

  expect(screen.queryByRole("button", { name: "Trend" })).toBeNull();
});

it("links the empty state to the datasets", async () => {
  server.use(http.get(EXPERIMENTS_PATH, () => HttpResponse.json({ data: [], next_cursor: null })));
  await renderApp("/acme/support-bot/experiments");

  expect(
    await screen.findByText(/An experiment runs your app on every item of a dataset/),
  ).toBeInTheDocument();
  expect(screen.getByRole("link", { name: "Open datasets" })).toHaveAttribute(
    "href",
    "/acme/support-bot/datasets",
  );
});

it("opens the panel from ?experiment= with the experiment in full", async () => {
  await renderApp(`/acme/support-bot/experiments?experiment=${BASELINE_ID}`);

  const panel = await screen.findByRole("region", { name: "Experiment" });
  expect(await within(panel).findByText("baseline")).toBeInTheDocument();
  expect(within(panel).getByRole("link", { name: "Open page" })).toHaveAttribute(
    "href",
    `/acme/support-bot/experiments/${BASELINE_ID}`,
  );
  expect(await within(panel).findByRole("table", { name: "Results" })).toBeInTheDocument();
});

it("closes the panel with its close button", async () => {
  const user = userEvent.setup();
  const { router } = await renderApp(`/acme/support-bot/experiments?experiment=${CANDIDATE_ID}`);

  const panel = await screen.findByRole("region", { name: "Experiment" });
  await user.click(within(panel).getByRole("button", { name: "Close" }));

  await waitFor(() => expect(router.state.location.href).not.toContain("experiment="));
});

it("opens the focused row with Enter and steps with the arrows while the panel is open", async () => {
  const user = userEvent.setup();
  const { router } = await renderApp("/acme/support-bot/experiments");

  await screen.findByRole("table", { name: "Experiments" });
  rowOf("candidate").focus();
  await user.keyboard("{Enter}");
  await waitFor(() => expect(router.state.location.href).toContain(`experiment=${CANDIDATE_ID}`));

  await screen.findByRole("region", { name: "Experiment" });
  rowOf("candidate").focus();
  await user.keyboard("{ArrowDown}");
  await waitFor(() => expect(router.state.location.href).toContain(`experiment=${BASELINE_ID}`));
});
