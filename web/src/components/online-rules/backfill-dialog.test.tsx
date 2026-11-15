import { screen, waitFor, within } from "@testing-library/react";
import { HttpResponse, http } from "msw";
import { expect, it } from "vitest";
import { server } from "@/mocks/node";
import { problemResponse } from "@/test/problem";
import { renderApp } from "@/test/render";
import backfillFixture from "../../../../fixtures/api/static/backfill.json";
import estimateFixture from "../../../../fixtures/api/static/backfill-estimate.json";
import backfillsFixture from "../../../../fixtures/api/static/backfills.json";

const RULE_URL = "/acme/support-bot/online-rules/01900000-0000-7000-8000-000000000700";
const ESTIMATE = "/api/v1/projects/:project/online-rules/:id/backfills/estimate";
const START = "/api/v1/projects/:project/online-rules/:id/backfills";
const LIST = "/api/v1/projects/:project/online-rules/:id/backfills";
const DAY_MS = 86_400_000;

type Body = { from: string; to: string; max_targets: number; sample_rate: number };

/** Answers the estimate and start requests, and records their bodies and the list reads. */
function recordRequests(options: { estimate?: () => Response; start?: () => Response } = {}) {
  const estimates: Body[] = [];
  const starts: Body[] = [];
  let lists = 0;
  server.use(
    http.post(ESTIMATE, async ({ request }) => {
      estimates.push((await request.json()) as Body);
      return options.estimate?.() ?? HttpResponse.json(estimateFixture);
    }),
    http.post(START, async ({ request }) => {
      starts.push((await request.json()) as Body);
      return options.start?.() ?? HttpResponse.json(backfillFixture, { status: 202 });
    }),
    http.get(LIST, () => {
      lists += 1;
      return HttpResponse.json(backfillsFixture);
    }),
  );
  return { estimates, starts, lists: () => lists };
}

/** Opens "Run on existing data" on the rule page, as an admin. */
async function openDialog() {
  const { user } = await renderApp(RULE_URL);
  await user.click(await screen.findByRole("button", { name: "Run on existing data" }));
  const dialog = await screen.findByRole("dialog", { name: "Run on existing data" });
  return { user, dialog };
}

it("estimates, then runs the same body and closes", async () => {
  const requests = recordRequests();
  const { user, dialog } = await openDialog();

  await user.click(within(dialog).getByRole("button", { name: "Estimate" }));
  const status = await within(dialog).findByRole("status");
  expect(status).toHaveTextContent("More than 1,000 traces match; the newest 1,000 are used.");
  expect(status).toHaveTextContent("Up to 250 would be evaluated.");
  expect(status).toHaveTextContent("Estimated cost: $0.31");

  const listsBefore = requests.lists();
  await user.click(within(dialog).getByRole("button", { name: "Run backfill" }));

  await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
  expect(requests.starts).toEqual(requests.estimates);
  const body = requests.starts[0] as Body;
  expect(body.max_targets).toBe(1000);
  expect(body.sample_rate).toBe(0.25);
  expect(Date.parse(body.to) - Date.parse(body.from)).toBe(7 * DAY_MS);
  await waitFor(() => expect(requests.lists()).toBeGreaterThan(listsBefore));
});

it("needs a fresh estimate after a change", async () => {
  recordRequests();
  const { user, dialog } = await openDialog();

  await user.click(within(dialog).getByRole("button", { name: "Estimate" }));
  await within(dialog).findByText("Up to 250 would be evaluated.");

  const max = within(dialog).getByLabelText("Max targets");
  await user.clear(max);
  await user.type(max, "500");

  expect(within(dialog).getByRole("button", { name: "Run backfill" })).toBeDisabled();
  expect(within(dialog).queryByText("Up to 250 would be evaluated.")).toBeNull();
});

it("validates the inputs before it estimates", async () => {
  const requests = recordRequests();
  const { user, dialog } = await openDialog();

  const max = within(dialog).getByLabelText("Max targets");
  await user.clear(max);
  await user.type(max, "0");
  const sample = within(dialog).getByLabelText("Sample rate (%)");
  await user.clear(sample);
  await user.type(sample, "0");
  await user.click(within(dialog).getByRole("button", { name: "Estimate" }));

  expect(
    await within(dialog).findByText("Enter a whole number from 1 to 10,000."),
  ).toBeInTheDocument();
  expect(
    within(dialog).getByText("Enter a percentage above 0 and at most 100."),
  ).toBeInTheDocument();
  expect(requests.estimates).toHaveLength(0);
});

it("says when the judge model has no price", async () => {
  recordRequests({
    estimate: () => HttpResponse.json({ ...estimateFixture, estimated_cost_usd: undefined }),
  });
  const { user, dialog } = await openDialog();

  await user.click(within(dialog).getByRole("button", { name: "Estimate" }));

  expect(
    await within(dialog).findByText(
      "The judge model has no price, so the cost cannot be estimated.",
    ),
  ).toBeInTheDocument();
});

it("shows a conflict and stays open", async () => {
  recordRequests({
    start: () =>
      problemResponse(409, "conflict", {
        detail: "a backfill of this rule is already running",
      }),
  });
  const { user, dialog } = await openDialog();

  await user.click(within(dialog).getByRole("button", { name: "Estimate" }));
  await within(dialog).findByText("Up to 250 would be evaluated.");
  await user.click(within(dialog).getByRole("button", { name: "Run backfill" }));

  expect(
    await within(dialog).findByText("a backfill of this rule is already running"),
  ).toBeInTheDocument();
  expect(screen.getByRole("dialog", { name: "Run on existing data" })).toBeInTheDocument();
});

it("shows the server's field errors under their fields", async () => {
  recordRequests({
    estimate: () =>
      problemResponse(400, "bad_request", {
        errors: [
          { field: "max_targets", message: "max_targets must be 1 to 10000" },
          { field: "sample_rate", message: "sample_rate must be above 0 and at most 1" },
        ],
      }),
  });
  const { user, dialog } = await openDialog();

  await user.click(within(dialog).getByRole("button", { name: "Estimate" }));

  expect(await within(dialog).findByText("max_targets must be 1 to 10000")).toBeInTheDocument();
  expect(within(dialog).getByText("sample_rate must be above 0 and at most 1")).toBeInTheDocument();
  expect(within(dialog).getByLabelText("Max targets")).toHaveAttribute("aria-invalid", "true");
});

it("shows the server's window error under the window selector", async () => {
  recordRequests({
    estimate: () =>
      problemResponse(400, "bad_request", {
        errors: [{ field: "to", message: "to must be at least 5 minutes ago" }],
      }),
  });
  const { user, dialog } = await openDialog();

  await user.click(within(dialog).getByRole("button", { name: "Estimate" }));

  expect(await within(dialog).findByText("to must be at least 5 minutes ago")).toBeInTheDocument();
  expect(within(dialog).getByLabelText("Window")).toHaveAttribute("aria-invalid", "true");
});

it("shows a failed estimate that has no field errors", async () => {
  recordRequests({
    estimate: () => problemResponse(500, "internal", {}),
  });
  const { user, dialog } = await openDialog();

  await user.click(within(dialog).getByRole("button", { name: "Estimate" }));

  expect(await within(dialog).findByText("Internal Server Error")).toBeInTheDocument();
  expect(within(dialog).queryByText(/would be evaluated/)).toBeNull();
});
