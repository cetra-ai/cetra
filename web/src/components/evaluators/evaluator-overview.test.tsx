import { screen, waitFor, within } from "@testing-library/react";
import { HttpResponse, http } from "msw";
import { expect, it } from "vitest";
import { formatCost, formatDuration } from "@/lib/format";
import { server } from "@/mocks/node";
import { renderApp } from "@/test/render";
import onlineRulesFixture from "../../../../fixtures/api/static/online-rules.json";

const EVALUATOR_URL = "/acme/support-bot/evaluators/01900000-0000-7000-8000-000000000500";
const EVALUATOR_ID = "01900000-0000-7000-8000-000000000500";
const OTHER_EVALUATOR_ID = "01900000-0000-7000-8000-000000000501";

/** The stat tile labelled `label`. */
function tileOf(label: string): HTMLElement {
  const tile = screen.getByText(label, { selector: "dt" }).closest("div");
  if (!tile) {
    throw new Error(`no tile ${label}`);
  }
  return tile;
}

it("lists the rules that use the evaluator, with totals in the tiles", async () => {
  await renderApp(EVALUATOR_URL);

  const table = await screen.findByRole("table", { name: "Rules using this evaluator" });
  const rows = within(table).getAllByRole("row").slice(1);
  expect(rows).toHaveLength(onlineRulesFixture.data.length);
  for (const rule of onlineRulesFixture.data) {
    expect(within(table).getByRole("link", { name: rule.name })).toHaveAttribute(
      "href",
      `/acme/support-bot/online-rules?rule=${rule.id}`,
    );
  }

  // 46 jobs on the first rule and 21 on the second, in the last 24 hours.
  expect(within(tileOf("Jobs / 24h")).getByText("67")).toBeInTheDocument();
  expect(within(tileOf("Online rules")).getByText("2")).toBeInTheDocument();
  expect(within(tileOf("Online rules")).getByText("1 active")).toBeInTheDocument();
  expect(within(tileOf("Spend")).getByText("$0.42")).toBeInTheDocument();
});

it("says so when no rule uses the evaluator, and offers a new rule to members", async () => {
  server.use(
    http.get("/api/v1/projects/:project/online-rules", () =>
      HttpResponse.json({
        data: onlineRulesFixture.data.map((rule) => ({
          ...rule,
          evaluator_id: OTHER_EVALUATOR_ID,
        })),
        next_cursor: null,
        org_enabled_rules: 2,
      }),
    ),
  );
  await renderApp(EVALUATOR_URL);

  expect(await screen.findByText("No online rule uses this evaluator.")).toBeInTheDocument();
  expect(screen.queryByRole("table", { name: "Rules using this evaluator" })).toBeNull();
  expect(screen.getByRole("link", { name: "Create a rule" })).toHaveAttribute(
    "href",
    "/acme/support-bot/online-rules/new",
  );
});

it("reads every page of rules before it sums them", async () => {
  const [first, ...rest] = onlineRulesFixture.data;
  if (first === undefined) {
    throw new Error("the fixture has no rules");
  }
  server.use(
    http.get("/api/v1/projects/:project/online-rules", ({ request }) => {
      const cursor = new URL(request.url).searchParams.get("cursor");
      return HttpResponse.json(
        cursor === null
          ? { data: [first], next_cursor: "page-2", org_enabled_rules: 2 }
          : { data: rest, next_cursor: null, org_enabled_rules: 2 },
      );
    }),
  );
  await renderApp(EVALUATOR_URL);

  expect(
    await screen.findByRole("link", { name: onlineRulesFixture.data[1]?.name ?? "" }),
  ).toBeInTheDocument();
  expect(within(tileOf("Jobs / 24h")).getByText("67")).toBeInTheDocument();
});

it("asks for the jobs and the scores of this evaluator", async () => {
  const requested: { path: string; evaluatorId: string | null }[] = [];
  server.use(
    http.get("/api/v1/projects/:project/eval-jobs", ({ request }) => {
      requested.push({
        path: "eval-jobs",
        evaluatorId: new URL(request.url).searchParams.get("evaluator_id"),
      });
      return HttpResponse.json({ data: [], next_cursor: null });
    }),
    http.get("/api/v1/projects/:project/scores", ({ request }) => {
      requested.push({
        path: "scores",
        evaluatorId: new URL(request.url).searchParams.get("evaluator_id"),
      });
      return HttpResponse.json({ data: [], next_cursor: null });
    }),
  );
  await renderApp(EVALUATOR_URL);

  await waitFor(() =>
    expect(requested).toEqual(
      expect.arrayContaining([
        { path: "eval-jobs", evaluatorId: EVALUATOR_ID },
        { path: "scores", evaluatorId: EVALUATOR_ID },
      ]),
    ),
  );
  expect(await screen.findByText("No jobs yet.")).toBeInTheDocument();
});

it("shows the last 7 days of the evaluator in tiles", async () => {
  await renderApp(EVALUATOR_URL);

  await screen.findByRole("table", { name: "Rules using this evaluator" });
  const runs = tileOf("Runs");
  expect(await within(runs).findByText("41")).toBeInTheDocument();
  expect(within(runs).getByText("37 done · 3 failed · 1 skipped")).toBeInTheDocument();
  expect(within(tileOf("Error rate")).getByText("7.5%")).toBeInTheDocument();
  const cost = tileOf("Judge cost");
  expect(within(cost).getByText(formatCost(0.0592))).toBeInTheDocument();
  expect(within(cost).getByText(`avg ${formatCost(0.0016)}`)).toBeInTheDocument();
  const latency = tileOf("Latency p50 / p95");
  expect(within(latency).getByText(formatDuration(820_000_000))).toBeInTheDocument();
  expect(within(latency).getByText(`p95 ${formatDuration(2_140_000_000)}`)).toBeInTheDocument();
  const result = tileOf("Result");
  expect(within(result).getByText("0.82")).toBeInTheDocument();
  expect(within(result).getByText("37 scores")).toBeInTheDocument();
});

it("lists the experiments with the evaluator's score", async () => {
  await renderApp(EVALUATOR_URL);

  const section = await screen.findByRole("region", { name: "Experiments" });
  expect(
    within(section).getByRole("heading", { name: "Experiments with a relevance score" }),
  ).toBeInTheDocument();
  expect(await within(section).findByRole("link", { name: "candidate" })).toHaveAttribute(
    "href",
    "/acme/support-bot/experiments?experiment=01900000-0000-7000-8000-000000000e02",
  );
  expect(within(section).getByRole("link", { name: "baseline" })).toHaveAttribute(
    "href",
    "/acme/support-bot/experiments?experiment=01900000-0000-7000-8000-000000000e01",
  );
  expect(within(section).queryByText(/more$/)).toBeNull();
});

it("says how many more experiments there are beyond the five listed", async () => {
  server.use(
    http.get("/api/v1/projects/:project/evaluators/stats", () =>
      HttpResponse.json({
        window: { from: "2025-12-26T00:00:00.000000000Z", to: "2026-01-01T12:00:00.000000000Z" },
        data: [
          {
            evaluator_id: EVALUATOR_ID,
            jobs: { done: 0, failed: 0, skipped: 0, cost_usd_total: 0, runs_daily: [] },
            scores: { data_type: "numeric", count: 0, daily: [] },
            experiments_with_score: {
              total: 7,
              recent: [
                {
                  id: "01900000-0000-7000-8000-000000000e02",
                  name: "candidate",
                  last_scored_at: "2026-01-01T09:30:00.000000000Z",
                },
                {
                  id: "01900000-0000-7000-8000-000000000e01",
                  name: "baseline",
                  last_scored_at: "2025-12-31T16:00:00.000000000Z",
                },
              ],
            },
          },
        ],
      }),
    ),
  );
  await renderApp(EVALUATOR_URL);

  const section = await screen.findByRole("region", { name: "Experiments" });
  expect(await within(section).findByText("+5 more")).toBeInTheDocument();
});

it("says when no experiment has the score", async () => {
  server.use(
    http.get("/api/v1/projects/:project/evaluators/stats", () =>
      HttpResponse.json({
        window: { from: "2025-12-26T00:00:00.000000000Z", to: "2026-01-01T12:00:00.000000000Z" },
        data: [
          {
            evaluator_id: EVALUATOR_ID,
            jobs: { done: 0, failed: 0, skipped: 0, cost_usd_total: 0, runs_daily: [] },
            scores: { data_type: "numeric", count: 0, daily: [] },
            experiments_with_score: { total: 0, recent: [] },
          },
        ],
      }),
    ),
  );
  await renderApp(EVALUATOR_URL);

  const section = await screen.findByRole("region", { name: "Experiments" });
  expect(
    await within(section).findByText("No experiment has a relevance score in the last 30 days."),
  ).toBeInTheDocument();
});

it("shows an alert when the experiments cannot be loaded", async () => {
  server.use(
    http.get("/api/v1/projects/:project/evaluators/stats", () =>
      HttpResponse.json({ error: "boom" }, { status: 500 }),
    ),
  );
  await renderApp(EVALUATOR_URL);

  const section = await screen.findByRole("region", { name: "Experiments" });
  expect(await within(section).findByRole("alert")).toHaveTextContent(
    "Could not load the experiments.",
  );
});

it("shows a skeleton in the experiments section until the stats answer", async () => {
  server.use(http.get("/api/v1/projects/:project/evaluators/stats", () => new Promise(() => {})));
  await renderApp(EVALUATOR_URL);

  const section = await screen.findByRole("region", { name: "Experiments" });
  expect(section.querySelector('[data-slot="skeleton"]')).not.toBeNull();
});

it("shows a dash in the tiles when the stats fail or leave the evaluator out", async () => {
  server.use(
    http.get("/api/v1/projects/:project/evaluators/stats", () =>
      HttpResponse.json({
        data: [],
        window: { from: "2025-12-26T00:00:00.000000000Z", to: "2026-01-01T12:00:00.000000000Z" },
      }),
    ),
  );
  await renderApp(EVALUATOR_URL);

  await screen.findByRole("table", { name: "Rules using this evaluator" });
  await waitFor(() => expect(within(tileOf("Runs")).getByText("—")).toBeInTheDocument());
  expect(within(tileOf("Error rate")).getByText("—")).toBeInTheDocument();
  expect(within(tileOf("Result")).getByText("—")).toBeInTheDocument();
});
