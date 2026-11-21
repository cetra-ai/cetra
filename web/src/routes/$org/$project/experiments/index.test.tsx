import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { HttpResponse, http } from "msw";
import { expect, it } from "vitest";
import type { Schema } from "@/api/types";
import { itemKey, scoreSummaryText } from "@/lib/experiments";
import { server } from "@/mocks/node";
import { STORAGE_PREFIX } from "@/product";
import { problemResponse } from "@/test/problem";
import { renderApp } from "@/test/render";
import datasetsFixture from "../../../../../../fixtures/api/responses/datasets.json";
import experimentFixture from "../../../../../../fixtures/api/static/experiment.json";
import comparisonFixture from "../../../../../../fixtures/api/static/experiment-comparison.json";
import resultsFixture from "../../../../../../fixtures/api/static/experiment-results.json";
import experimentsFixture from "../../../../../../fixtures/api/static/experiments.json";

const BASELINE_ID = "01900000-0000-7000-8000-000000000e01";
const CANDIDATE_ID = "01900000-0000-7000-8000-000000000e02";
const EXPERIMENTS_PATH = "/api/v1/projects/:project/experiments";
const EXPERIMENT_PATH = "/api/v1/projects/:project/experiments/:id";
const RESULTS_PATH = "/api/v1/projects/:project/experiments/:id/results";
const COMPARE_PATH = "/api/v1/projects/:project/experiments/compare";
const PANEL_URL = `/acme/support-bot/experiments?experiment=${BASELINE_ID}`;

function firstDataset() {
  const dataset = datasetsFixture.data[0];
  if (!dataset) {
    throw new Error("the datasets fixture has no dataset");
  }
  return dataset;
}

/** The experiment's GET with one answer. The list's batch `experiments/summaries` falls through. */
function experimentRoute(respond: () => Response) {
  return http.get(EXPERIMENT_PATH, ({ params }) =>
    String(params.id) === "summaries" ? undefined : respond(),
  );
}

function firstRow() {
  const row = resultsFixture.rows[0];
  if (!row) {
    throw new Error("the results fixture has no row");
  }
  return row;
}

/** The results answer with the fixture's summary, its experiment replaced by `experiment`. */
function resultsWith(
  experiment: Record<string, unknown>,
  rows: typeof resultsFixture.rows = resultsFixture.rows,
) {
  return {
    summary: {
      ...resultsFixture.summary,
      experiment: { ...resultsFixture.summary.experiment, ...experiment },
    },
    rows,
    next_cursor: null,
  };
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
  expect(
    screen.getAllByRole("heading", { level: 1 }).map((heading) => heading.textContent),
  ).toEqual(["Experiments"]);

  const table = await screen.findByRole("table", { name: "Experiments" });
  for (const fixture of experimentsFixture.data) {
    const row = rowOf(fixture.name);
    expect(within(row).getByRole("link", { name: fixture.name })).toHaveAttribute(
      "href",
      `/acme/support-bot/experiments?experiment=${fixture.id}`,
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

it("selecting the selected status chip again clears the filter", async () => {
  const user = userEvent.setup();
  const running = {
    ...fixtureExperiment(BASELINE_ID),
    name: "in-flight",
    status: "running" as const,
  };
  server.use(
    http.get(EXPERIMENTS_PATH, () =>
      HttpResponse.json({ data: [fixtureExperiment(CANDIDATE_ID), running], next_cursor: null }),
    ),
  );
  const { router } = await renderApp("/acme/support-bot/experiments");
  await screen.findByRole("table", { name: "Experiments" });

  await user.click(screen.getByRole("button", { name: "Completed 1" }));
  await waitFor(() => expect(router.state.location.href).toContain("status=completed"));
  expect(
    within(screen.getByRole("table", { name: "Experiments" })).queryByText("in-flight"),
  ).toBeNull();

  await user.click(screen.getByRole("button", { name: "Completed 1" }));
  await waitFor(() => expect(router.state.location.href).not.toContain("status="));
  expect(
    within(screen.getByRole("table", { name: "Experiments" })).getByText("in-flight"),
  ).toBeInTheDocument();
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
  await renderApp(PANEL_URL);

  const panel = await screen.findByRole("region", { name: "Experiment" });
  expect(await within(panel).findByText("baseline")).toBeInTheDocument();
  expect(await within(panel).findByRole("table", { name: "Results" })).toBeInTheDocument();
});

it("has no Open page link in the panel", async () => {
  await renderApp(PANEL_URL);

  const panel = await screen.findByRole("region", { name: "Experiment" });
  await within(panel).findByRole("table", { name: "Results" });
  expect(within(panel).queryByRole("link", { name: "Open page" })).toBeNull();
});

it("drops the errors switch when another experiment opens", async () => {
  const user = userEvent.setup();
  const { router } = await renderApp(`${PANEL_URL}&errors=1`);
  await screen.findByRole("region", { name: "Experiment" });
  await screen.findByRole("table", { name: "Experiments" });

  await user.click(within(rowOf("candidate")).getByText(/^\d+ \/ \d+$/));

  await waitFor(() => expect(router.state.location.href).toContain(`experiment=${CANDIDATE_ID}`));
  expect(decodeURIComponent(router.state.location.href)).not.toContain("errors=");
  await waitFor(() => expect(router.state.status).toBe("idle"));
});

it("clears the item from the URL when its sheet closes and keeps the errors switch", async () => {
  const user = userEvent.setup();
  const { router } = await renderApp(`${PANEL_URL}&errors=1&item=${itemKey(firstRow())}`);
  await screen.findByRole("dialog");

  await user.keyboard("{Escape}");

  await waitFor(() => expect(router.state.location.href).not.toContain("item="));
  expect(router.state.location.href).toContain("errors=1");
  await waitFor(() => expect(router.state.status).toBe("idle"));
});

it("filters to errors with the switch and keeps it in the URL", async () => {
  const user = userEvent.setup();
  const requested: (string | null)[] = [];
  server.use(
    http.get(RESULTS_PATH, ({ request }) => {
      requested.push(new URL(request.url).searchParams.get("errors_only"));
      return HttpResponse.json(resultsFixture);
    }),
  );
  const { router } = await renderApp(PANEL_URL);
  const panel = await screen.findByRole("region", { name: "Experiment" });
  await within(panel).findByRole("table", { name: "Results" });

  await user.click(within(panel).getByRole("switch", { name: "Errors only" }));

  await waitFor(() => expect(requested).toContain("true"));
  await waitFor(() => expect(router.state.location.href).toContain("errors=1"));
  await waitFor(() => expect(router.state.status).toBe("idle"));
});

it("filters to errors from a deep link with errors=1", async () => {
  const requested: (string | null)[] = [];
  server.use(
    http.get(RESULTS_PATH, ({ request }) => {
      requested.push(new URL(request.url).searchParams.get("errors_only"));
      return HttpResponse.json(resultsFixture);
    }),
  );

  const { router } = await renderApp(`${PANEL_URL}&errors=1`);

  const panel = await screen.findByRole("region", { name: "Experiment" });
  await within(panel).findByRole("table", { name: "Results" });
  expect(requested.at(-1)).toBe("true");
  expect(within(panel).getByRole("switch", { name: "Errors only" })).toBeChecked();
  await waitFor(() => expect(router.state.status).toBe("idle"));
});

it("opens an item from a deep link with item=", async () => {
  const { router } = await renderApp(`${PANEL_URL}&item=${itemKey(firstRow())}`);

  expect(await screen.findByRole("dialog")).toBeInTheDocument();
  await waitFor(() => expect(router.state.status).toBe("idle"));
});

it("shows the summary", async () => {
  await renderApp(PANEL_URL);

  const panel = await screen.findByRole("region", { name: "Experiment" });
  const list = await within(panel).findByRole("list", { name: "Summary" });
  const cards = within(list).getAllByRole("listitem");
  for (const score of resultsFixture.summary.scores) {
    const card = cards.find((candidate) => within(candidate).queryByText(score.name));
    expect(card).toBeDefined();
    expect(card).toHaveTextContent(scoreSummaryText(score as Schema<"ScoreSummary">));
  }
  const results = cards.find((card) => within(card).queryByText("Results"));
  expect(results).toHaveTextContent(
    `${resultsFixture.summary.result_count} of ${experimentFixture.item_count * experimentFixture.repetitions}`,
  );
});

it("lists the results with their scores", async () => {
  await renderApp(PANEL_URL);

  const panel = await screen.findByRole("region", { name: "Experiment" });
  const table = await within(panel).findByRole("table", { name: "Results" });
  const rows = within(table).getAllByRole("row").slice(1);
  expect(rows).toHaveLength(resultsFixture.rows.length);
  for (const score of resultsFixture.summary.scores) {
    expect(within(table).getByRole("columnheader", { name: score.name })).toBeInTheDocument();
  }
  for (const [index, fixture] of resultsFixture.rows.entries()) {
    const row = rows[index] as HTMLElement;
    if (fixture.trace_id) {
      expect(within(row).getByRole("link", { name: "trace" })).toHaveAttribute(
        "href",
        `/acme/support-bot/traces/${fixture.trace_id}`,
      );
    }
  }
});

it("shows the p50 alone when the p95 is absent", async () => {
  const { latency_p95_ns: _p95, ...summary } = resultsFixture.summary;
  server.use(http.get(RESULTS_PATH, () => HttpResponse.json({ ...resultsFixture, summary })));

  await renderApp(PANEL_URL);

  const panel = await screen.findByRole("region", { name: "Experiment" });
  const list = await within(panel).findByRole("list", { name: "Summary" });
  const latency = within(list)
    .getAllByRole("listitem")
    .find((card) => within(card).queryByText("Latency"));
  expect(latency).toHaveTextContent(/^Latencyp50 /);
  expect(latency).not.toHaveTextContent("p95");
  expect(latency).not.toHaveTextContent("mean");
});

it("answers not found when the results are not found", async () => {
  server.use(
    http.get(RESULTS_PATH, () => problemResponse(404, "not_found", { detail: "no such results" })),
  );

  await renderApp(PANEL_URL);

  expect(await screen.findByText("Page not found")).toBeInTheDocument();
});

it("says so in the panel when the experiment no longer exists", async () => {
  server.use(
    experimentRoute(() => problemResponse(404, "not_found", { detail: "no such experiment" })),
  );

  await renderApp(PANEL_URL);

  expect(await screen.findByText("This experiment no longer exists.")).toBeInTheDocument();
});

it("shows a failed run", async () => {
  const failed = { status: "failed" as const, error: "results upload failed" };
  server.use(
    experimentRoute(() => HttpResponse.json({ ...experimentFixture, ...failed })),
    http.get(RESULTS_PATH, () => HttpResponse.json(resultsWith(failed))),
  );

  await renderApp(PANEL_URL);

  expect(await screen.findByText("The run failed: results upload failed")).toBeInTheDocument();
});

it("shows the snippet while waiting", async () => {
  const running = { status: "running" };
  server.use(
    experimentRoute(() =>
      HttpResponse.json({ ...experimentFixture, status: "running", finished_at: undefined }),
    ),
    http.get(RESULTS_PATH, () => HttpResponse.json(resultsWith(running, []))),
  );

  await renderApp(PANEL_URL);

  expect(await screen.findByText("Waiting for results")).toBeInTheDocument();
  expect(screen.getByText(/cetra\.evaluate\(/, { selector: "code" })).toBeInTheDocument();
});

it("hides the repetition column for one repetition", async () => {
  await renderApp(PANEL_URL);
  const panel = await screen.findByRole("region", { name: "Experiment" });
  await within(panel).findByRole("table", { name: "Results" });
  expect(within(panel).queryByRole("columnheader", { name: "Rep" })).toBeNull();
});

it("shows the repetition column for several repetitions", async () => {
  server.use(
    experimentRoute(() => HttpResponse.json({ ...experimentFixture, repetitions: 3 })),
    http.get(RESULTS_PATH, () => HttpResponse.json(resultsWith({ repetitions: 3 }, [firstRow()]))),
  );

  await renderApp(PANEL_URL);

  const panel = await screen.findByRole("region", { name: "Experiment" });
  const table = await within(panel).findByRole("table", { name: "Results" });
  expect(within(table).getByRole("columnheader", { name: "Rep" })).toBeInTheDocument();
  const row = within(table).getAllByRole("row")[1] as HTMLElement;
  expect(within(row).getByText("1")).toBeInTheDocument();
});

it("compares with the previous run of the same dataset", async () => {
  server.use(
    http.get(EXPERIMENTS_PATH, () => HttpResponse.json(experimentsFixture)),
    experimentRoute(() => HttpResponse.json({ ...experimentFixture, id: CANDIDATE_ID })),
    http.get(RESULTS_PATH, () => HttpResponse.json(resultsFixture)),
  );
  await renderApp(`/acme/support-bot/experiments?experiment=${CANDIDATE_ID}`);

  const panel = await screen.findByRole("region", { name: "Experiment" });
  const link = await within(panel).findByRole("link", { name: "Compare with previous run" });
  const href = decodeURIComponent(link.getAttribute("href") ?? "");
  expect(href).toContain(`/experiments/compare`);
  expect(href).toContain(`ids=${BASELINE_ID},${CANDIDATE_ID}`);
});

it("hides the previous-run comparison for the oldest run", async () => {
  await renderApp(PANEL_URL);

  const panel = await screen.findByRole("region", { name: "Experiment" });
  await within(panel).findByRole("table", { name: "Results" });
  expect(within(panel).queryByRole("link", { name: "Compare with previous run" })).toBeNull();
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
