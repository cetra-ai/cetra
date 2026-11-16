import { act, screen, waitFor, within } from "@testing-library/react";
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
    expect(within(table).getAllByText(text).length).toBeGreaterThan(0);
    expect(within(table).getAllByText("versus baseline").length).toBeGreaterThan(0);
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
  expect(within(regressedRow as HTMLElement).getByText("Regressed")).toBeInTheDocument();
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

it("reads the threshold and the regressions flag the router parses as numbers", async () => {
  const requested: URLSearchParams[] = [];
  server.use(
    http.get(COMPARE_PATH, ({ request }) => {
      requested.push(new URL(request.url).searchParams);
      return HttpResponse.json(comparisonFixture);
    }),
  );

  await renderApp(`${COMPARE_URL}&threshold=0.25&regressions=1`);

  await screen.findByRole("table", { name: "Items" });
  expect(requested.at(-1)?.get("threshold")).toBe("0.25");
  expect(requested.at(-1)?.get("regressions_only")).toBe("true");
  expect(screen.getByRole("spinbutton", { name: "Regression threshold" })).toHaveValue(0.25);
});

it("updates the threshold field when the URL changes", async () => {
  const { router } = await renderApp(COMPARE_URL);
  const input = await screen.findByRole("spinbutton", { name: "Regression threshold" });

  await act(() =>
    router.navigate({
      to: "/$org/$project/experiments/compare",
      params: { org: "acme", project: "support-bot" },
      search: { ids: `${BASELINE_ID},${CANDIDATE_ID}`, threshold: 0.4 },
    }),
  );

  await waitFor(() => expect(input).toHaveValue(0.4));
});

it("says so when regressions-only has no rows and no next page", async () => {
  server.use(
    http.get(COMPARE_PATH, () =>
      HttpResponse.json({ ...comparisonFixture, rows: [], next_cursor: null }),
    ),
  );

  await renderApp(`${COMPARE_URL}&regressions=1`);

  expect(await screen.findByText("No regressions at this threshold")).toBeInTheDocument();
  expect(screen.queryByRole("button", { name: "Load more" })).toBeNull();
});

it("shows Load more instead of the empty message when a page has a cursor", async () => {
  server.use(
    http.get(COMPARE_PATH, () =>
      HttpResponse.json({ ...comparisonFixture, rows: [], next_cursor: "page-2" }),
    ),
  );

  await renderApp(`${COMPARE_URL}&regressions=1`);

  expect(await screen.findByRole("button", { name: "Load more" })).toBeInTheDocument();
  expect(screen.queryByText("No regressions at this threshold")).toBeNull();
});

it("marks an out-of-range threshold", async () => {
  const user = userEvent.setup();
  await renderApp(COMPARE_URL);
  const input = await screen.findByRole("spinbutton", { name: "Regression threshold" });

  await user.clear(input);
  await user.type(input, "2000");

  expect(input).toHaveAttribute("aria-invalid", "true");
  expect(screen.getByText("Between 0 and 1000")).toBeInTheDocument();
});

it("keeps the rows and shows the error when Load more fails", async () => {
  const user = userEvent.setup();
  server.use(
    http.get(COMPARE_PATH, ({ request }) => {
      if (new URL(request.url).searchParams.get("cursor")) {
        return problemResponse(500, "internal", { detail: "the next page broke" });
      }
      return HttpResponse.json({ ...comparisonFixture, next_cursor: "page-2" });
    }),
  );
  await renderApp(COMPARE_URL);
  await screen.findByRole("table", { name: "Items" });

  await user.click(screen.getByRole("button", { name: "Load more" }));

  expect(await screen.findByRole("alert")).toHaveTextContent("the next page broke");
  expect(screen.getByRole("table", { name: "Items" })).toBeInTheDocument();
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
  expect(within(row).getByText("No baseline output")).toBeInTheDocument();
  expect(within(row).getByText(/"answer": "No\./)).toBeInTheDocument();
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
