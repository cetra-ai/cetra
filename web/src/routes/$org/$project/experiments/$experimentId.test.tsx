import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { HttpResponse, http } from "msw";
import { expect, it } from "vitest";
import type { Schema } from "@/api/types";
import { scoreSummaryText } from "@/lib/experiments";
import { server } from "@/mocks/node";
import { problemResponse } from "@/test/problem";
import { renderApp } from "@/test/render";
import experimentFixture from "../../../../../../fixtures/api/static/experiment.json";
import resultsFixture from "../../../../../../fixtures/api/static/experiment-results.json";

const BASELINE_ID = "01900000-0000-7000-8000-000000000e01";
const EXPERIMENT_URL = `/acme/support-bot/experiments/${BASELINE_ID}`;
const EXPERIMENT_PATH = "/api/v1/projects/:project/experiments/:id";
const RESULTS_PATH = "/api/v1/projects/:project/experiments/:id/results";

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

it("shows the summary", async () => {
  await renderApp(EXPERIMENT_URL);

  expect(await screen.findByRole("heading", { name: experimentFixture.name })).toBeInTheDocument();
  const list = await screen.findByRole("list", { name: "Summary" });
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
  await renderApp(EXPERIMENT_URL);

  const table = await screen.findByRole("table", { name: "Results" });
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

it("filters to errors", async () => {
  const user = userEvent.setup();
  const requested: (string | null)[] = [];
  server.use(
    http.get(RESULTS_PATH, ({ request }) => {
      requested.push(new URL(request.url).searchParams.get("errors_only"));
      return HttpResponse.json(resultsFixture);
    }),
  );
  const { router } = await renderApp(EXPERIMENT_URL);
  await screen.findByRole("table", { name: "Results" });

  await user.click(screen.getByRole("switch", { name: "Errors only" }));

  await waitFor(() => expect(requested).toContain("true"));
  await waitFor(() => expect(router.state.location.href).toContain("errors=1"));
});

it("answers not found", async () => {
  server.use(
    http.get(EXPERIMENT_PATH, () =>
      problemResponse(404, "not_found", { detail: "no such experiment" }),
    ),
  );

  await renderApp(EXPERIMENT_URL);

  expect(await screen.findByText("Page not found")).toBeInTheDocument();
});

it("shows a failed run", async () => {
  const failed = { status: "failed" as const, error: "results upload failed" };
  server.use(
    http.get(EXPERIMENT_PATH, () => HttpResponse.json({ ...experimentFixture, ...failed })),
    http.get(RESULTS_PATH, () => HttpResponse.json(resultsWith(failed))),
  );

  await renderApp(EXPERIMENT_URL);

  expect(await screen.findByText("The run failed: results upload failed")).toBeInTheDocument();
});

it("shows the snippet while waiting", async () => {
  const running = { status: "running" };
  server.use(
    http.get(EXPERIMENT_PATH, () =>
      HttpResponse.json({ ...experimentFixture, status: "running", finished_at: undefined }),
    ),
    http.get(RESULTS_PATH, () => HttpResponse.json(resultsWith(running, []))),
  );

  await renderApp(EXPERIMENT_URL);

  expect(await screen.findByText("Waiting for results")).toBeInTheDocument();
  expect(screen.getByText(/llmobs\.evaluate\(/, { selector: "code" })).toBeInTheDocument();
});

it("hides the repetition column for one repetition", async () => {
  await renderApp(EXPERIMENT_URL);
  await screen.findByRole("table", { name: "Results" });
  expect(screen.queryByRole("columnheader", { name: "Rep" })).toBeNull();
});

it("shows the repetition column for several repetitions", async () => {
  server.use(
    http.get(EXPERIMENT_PATH, () => HttpResponse.json({ ...experimentFixture, repetitions: 3 })),
    http.get(RESULTS_PATH, () => HttpResponse.json(resultsWith({ repetitions: 3 }, [firstRow()]))),
  );

  await renderApp(EXPERIMENT_URL);

  const table = await screen.findByRole("table", { name: "Results" });
  expect(within(table).getByRole("columnheader", { name: "Rep" })).toBeInTheDocument();
  const row = within(table).getAllByRole("row")[1] as HTMLElement;
  expect(within(row).getByText("1")).toBeInTheDocument();
});
