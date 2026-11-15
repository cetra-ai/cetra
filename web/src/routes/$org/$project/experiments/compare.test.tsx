import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { HttpResponse, http } from "msw";
import { expect, it } from "vitest";
import type { Schema } from "@/api/types";
import { deltaText } from "@/lib/experiments";
import { server } from "@/mocks/node";
import { problemResponse } from "@/test/problem";
import { renderApp } from "@/test/render";
import comparisonFixture from "../../../../../../fixtures/api/static/experiment-comparison.json";

const BASELINE_ID = "01900000-0000-7000-8000-000000000e01";
const CANDIDATE_ID = "01900000-0000-7000-8000-000000000e02";
const COMPARE_URL = `/acme/support-bot/experiments/compare?ids=${BASELINE_ID},${CANDIDATE_ID}`;
const COMPARE_PATH = "/api/v1/projects/:project/experiments/compare";

function fixtureRow(index: number) {
  const row = comparisonFixture.rows[index];
  if (!row) {
    throw new Error(`the comparison fixture has no row ${index}`);
  }
  return row;
}

function candidateScore(name: string) {
  const score = comparisonFixture.experiments[1]?.scores.find(
    (candidate) => candidate.name === name,
  );
  if (!score) {
    throw new Error(`the comparison fixture has no candidate score ${name}`);
  }
  return score;
}

it("shows the scores with deltas", async () => {
  await renderApp(COMPARE_URL);

  const table = await screen.findByRole("table", { name: "Scores" });
  expect(within(table).getByText("Baseline")).toBeInTheDocument();
  expect(within(table).getByRole("link", { name: "candidate" })).toBeInTheDocument();
  for (const [name, entry] of Object.entries(comparisonFixture.deltas[0]?.scores ?? {})) {
    const score = candidateScore(name);
    const text = deltaText(entry.delta, score.data_type as Schema<"ScoreSummary">["data_type"]);
    expect(within(table).getByLabelText(`${text} versus baseline`)).toHaveTextContent(text);
    expect(within(table).getAllByText(`n = ${entry.n_compared}`).length).toBeGreaterThan(0);
  }
});

it("highlights regressed items", async () => {
  await renderApp(COMPARE_URL);

  const table = await screen.findByRole("table", { name: "Items" });
  const regressedRow = within(table)
    .getByText(/Can I export my traces/)
    .closest("tr");
  const plainRow = within(table)
    .getByText(/Which plans include SSO/)
    .closest("tr");
  expect(regressedRow).toHaveAttribute("data-regressed", "true");
  expect(within(regressedRow as HTMLElement).getByLabelText("Regressed")).toBeInTheDocument();
  expect(plainRow).not.toHaveAttribute("data-regressed");
});

it("passes the threshold and the regressions filter", async () => {
  const user = userEvent.setup();
  const requested: URLSearchParams[] = [];
  server.use(
    http.get(COMPARE_PATH, ({ request }) => {
      requested.push(new URL(request.url).searchParams);
      return HttpResponse.json(comparisonFixture);
    }),
  );
  const { router } = await renderApp(COMPARE_URL);
  await screen.findByRole("table", { name: "Items" });

  const input = screen.getByRole("spinbutton", { name: "Regression threshold" });
  await user.clear(input);
  await user.type(input, "0.25");
  await user.click(screen.getByRole("switch", { name: "Regressions only" }));

  await waitFor(() =>
    expect(
      requested.some(
        (params) => params.get("threshold") === "0.25" && params.get("regressions_only") === "true",
      ),
    ).toBe(true),
  );
  await waitFor(() => expect(router.state.location.href).toContain("threshold=0.25"));
  expect(router.state.location.href).toContain("regressions=1");
});

it("shows the version warning", async () => {
  const warning = `experiments use different dataset versions: ${BASELINE_ID} at version 1, ${CANDIDATE_ID} at version 2`;
  server.use(
    http.get(COMPARE_PATH, () => HttpResponse.json({ ...comparisonFixture, warnings: [warning] })),
  );

  await renderApp(COMPARE_URL);

  expect(await screen.findByText(warning)).toHaveAttribute("role", "status");
});

it("shows the item-set warnings", async () => {
  const left = `${CANDIDATE_ID}: 1 items are left out of the deltas (missing or failed in one of the two)`;
  const missing = "1 items have no result in the baseline";
  server.use(
    http.get(COMPARE_PATH, () =>
      HttpResponse.json({ ...comparisonFixture, warnings: [missing, left] }),
    ),
  );

  await renderApp(COMPARE_URL);

  expect(await screen.findByText(left)).toHaveAttribute("role", "status");
  expect(screen.getByText(missing)).toHaveAttribute("role", "status");
});

it("matches cells by experiment id", async () => {
  // Only the candidate has a cell for this item; it is listed first to rule out matching by position.
  const candidateCell = fixtureRow(1).cells.find((cell) => cell.experiment_id === CANDIDATE_ID);
  const candidateOnly = { ...fixtureRow(1), cells: candidateCell ? [candidateCell] : [] };
  server.use(
    http.get(COMPARE_PATH, () =>
      HttpResponse.json({ ...comparisonFixture, rows: [candidateOnly] }),
    ),
  );

  await renderApp(COMPARE_URL);

  const table = await screen.findByRole("table", { name: "Items" });
  const row = within(table)
    .getByText(/Can I export my traces/)
    .closest("tr") as HTMLElement;
  expect(within(row).getByText("Not run")).toBeInTheDocument();
  expect(within(row).getByText(/"answer": "No\./, { selector: "ins" })).toBeInTheDocument();
});

it("explains a refused comparison", async () => {
  server.use(
    http.get(COMPARE_PATH, () =>
      problemResponse(400, "invalid_request", {
        detail: "the experiments belong to different datasets",
      }),
    ),
  );

  await renderApp(COMPARE_URL);

  expect(
    await screen.findByText("the experiments belong to different datasets"),
  ).toBeInTheDocument();
  expect(
    screen
      .getAllByRole("link", { name: "Experiments" })
      .some((link) => link.getAttribute("href") === "/acme/support-bot/experiments"),
  ).toBe(true);
});

it("explains a hidden experiment", async () => {
  server.use(
    http.get(COMPARE_PATH, () =>
      problemResponse(404, "not_found", { detail: "no such experiment" }),
    ),
  );

  await renderApp(COMPARE_URL);

  expect(
    await screen.findByText(
      "One of these experiments was not found. It may have been deleted or be outside your plan's data window.",
    ),
  ).toBeInTheDocument();
});

it("asks for a selection without ids", async () => {
  await renderApp("/acme/support-bot/experiments/compare");

  expect(await screen.findByText("Pick experiments to compare")).toBeInTheDocument();
  expect(screen.getByRole("link", { name: "Go to Experiments" })).toHaveAttribute(
    "href",
    "/acme/support-bot/experiments",
  );
});

it("is not the experiment page", async () => {
  await renderApp(COMPARE_URL);

  expect(await screen.findByRole("heading", { name: "Compare experiments" })).toBeInTheDocument();
});
