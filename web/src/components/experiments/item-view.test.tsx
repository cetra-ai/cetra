import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { HttpResponse, http } from "msw";
import { expect, it } from "vitest";
import { server } from "@/mocks/node";
import { renderApp } from "@/test/render";
import resultsFixture from "../../../../fixtures/api/static/experiment-results.json";

const BASELINE_ID = "01900000-0000-7000-8000-000000000e01";
const EXPERIMENT_URL = `/acme/support-bot/experiments?experiment=${BASELINE_ID}`;
const RESULTS_PATH = "/api/v1/projects/:project/experiments/:id/results";

function rowAt(index: number) {
  const row = resultsFixture.rows[index];
  if (!row) {
    throw new Error(`the results fixture has no row ${index}`);
  }
  return row;
}

/** The results answer with the fixture's summary and the given rows. */
function resultsWith(rows: unknown[]) {
  return { summary: resultsFixture.summary, rows, next_cursor: null };
}

function keyOf(row: { dataset_item_id: string; repetition: number }) {
  return `${row.dataset_item_id}:${row.repetition}`;
}

it("renders a text field of the output as Markdown", async () => {
  server.use(
    http.get(RESULTS_PATH, () =>
      HttpResponse.json(resultsWith([{ ...rowAt(0), output: { answer: "**Bold** answer" } }])),
    ),
  );
  await renderApp(`${EXPERIMENT_URL}&item=${keyOf(rowAt(0))}`);

  const dialog = await screen.findByRole("dialog");
  const output = within(dialog).getByRole("region", { name: "Output" });
  expect(within(output).getByText("Bold").tagName).toBe("STRONG");
});

it("renders an object with several fields as JSON", async () => {
  server.use(
    http.get(RESULTS_PATH, () =>
      HttpResponse.json(
        resultsWith([{ ...rowAt(0), output: { answer: "Plain", confidence: 0.9 } }]),
      ),
    ),
  );
  await renderApp(`${EXPERIMENT_URL}&item=${keyOf(rowAt(0))}`);

  const dialog = await screen.findByRole("dialog");
  const output = within(dialog).getByRole("region", { name: "Output" });
  expect(within(output).getByText("confidence")).toBeInTheDocument();
  expect(within(output).getByText("Pretty", { selector: "button" })).toHaveAttribute(
    "aria-pressed",
    "true",
  );
  await userEvent.setup().click(within(output).getByRole("button", { name: "JSON" }));
  expect(within(output).getByRole("button", { name: "JSON" })).toHaveAttribute(
    "aria-pressed",
    "true",
  );
});

it("links the item to its trace", async () => {
  await renderApp(`${EXPERIMENT_URL}&item=${keyOf(rowAt(0))}`);

  const dialog = await screen.findByRole("dialog");
  expect(within(dialog).getByRole("link", { name: "Open trace" })).toHaveAttribute(
    "href",
    `/acme/support-bot/traces/${rowAt(0).trace_id}`,
  );
});

it("steps to the next row with the arrow keys", async () => {
  const user = userEvent.setup();
  const { router } = await renderApp(`${EXPERIMENT_URL}&item=${keyOf(rowAt(0))}`);

  const dialog = await screen.findByRole("dialog");
  expect(within(dialog).getByText(rowAt(0).dataset_item_id)).toBeInTheDocument();
  await user.keyboard("{ArrowDown}");

  await waitFor(() =>
    expect(new URL(router.state.location.href, "http://app.test").searchParams.get("item")).toBe(
      keyOf(rowAt(1)),
    ),
  );
});
