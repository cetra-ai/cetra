import { screen, waitFor, within } from "@testing-library/react";
import { HttpResponse, http } from "msw";
import { expect, it } from "vitest";
import { server } from "@/mocks/node";
import { problemResponse } from "@/test/problem";
import { renderApp } from "@/test/render";
import scoresFixture from "../../../../../../../fixtures/api/responses/scores.json";
import ruleFixture from "../../../../../../../fixtures/api/static/online-rule.json";
import projectFixture from "../../../../../../../fixtures/api/static/project.json";

const RULE_ID = "01900000-0000-7000-8000-000000000700";
const SPAN_RULE_ID = "01900000-0000-7000-8000-000000000701";
const RULE_URL = `/acme/support-bot/online-rules/${RULE_ID}`;
const SPAN_RULE_URL = `/acme/support-bot/online-rules/${SPAN_RULE_ID}`;
const NOW = "2026-01-01T11:00:00.000000000Z";

/** The rule fixture with the second rule's pause: the span rule paused for errors. */
function pausedRule(overrides: Record<string, unknown>) {
  return {
    ...ruleFixture,
    id: SPAN_RULE_ID,
    name: "Relevance on spans",
    target: "span",
    sample_rate: 1,
    paused_reason: "errors",
    paused_at: NOW,
    paused_detail: "error rate 60 %",
    ...overrides,
  };
}

function projectAs(role: string) {
  return http.get("/api/v1/projects/:project", () =>
    HttpResponse.json({ ...projectFixture, role }),
  );
}

it("shows the rule, its spend and its counts", async () => {
  await renderApp(RULE_URL);

  expect(await screen.findByRole("heading", { name: "Relevance on answers" })).toBeInTheDocument();
  expect(screen.getByText("Active")).toBeInTheDocument();
  expect(screen.getByText("$0.42 spent of $5.00 today")).toBeInTheDocument();
  expect(screen.getByText("+$0.01 reserved")).toBeInTheDocument();
  expect(screen.getByRole("progressbar", { name: "Spend" })).toHaveAttribute("aria-valuenow", "8");

  const counts = screen.getByRole("region", { name: "Last 24 hours" });
  for (const [label, value] of [
    ["Done", "40"],
    ["Failed", "3"],
    ["Queued", "2"],
    ["Running", "1"],
    ["Skipped", "0"],
  ] as const) {
    expect(within(counts).getByText(label).nextElementSibling).toHaveTextContent(value);
  }
});

it("shows the errors banner and resumes the rule", async () => {
  let resumed = false;
  server.use(
    http.get("/api/v1/projects/:project/online-rules/:id", () =>
      HttpResponse.json(resumed ? pausedRule({ paused_reason: undefined }) : pausedRule({})),
    ),
    http.post("/api/v1/projects/:project/online-rules/:id/resume", () => {
      resumed = true;
      return HttpResponse.json(pausedRule({ paused_reason: undefined }));
    }),
  );
  const { user } = await renderApp(SPAN_RULE_URL);

  const banner = await screen.findByRole("alert");
  expect(banner).toHaveTextContent(
    "Paused: too many jobs failed (error rate 60 %). Fix the cause, then resume the rule.",
  );
  await user.click(screen.getByRole("button", { name: "Resume" }));

  await waitFor(() => expect(screen.queryByRole("alert")).toBeNull());
  expect(screen.getByText("Active")).toBeInTheDocument();
  expect(resumed).toBe(true);
});

it("names the connection in the connection banner", async () => {
  server.use(
    http.get("/api/v1/projects/:project/online-rules/:id", () =>
      HttpResponse.json(
        pausedRule({
          id: RULE_ID,
          name: "Relevance on answers",
          target: "trace",
          paused_reason: "connection",
          paused_detail: "401 unauthorized",
        }),
      ),
    ),
  );
  await renderApp(RULE_URL);

  const banner = await screen.findByRole("alert");
  // The connection's name arrives with the connections list, after the banner's first render.
  await waitFor(() =>
    expect(banner).toHaveTextContent(
      "Paused: the LLM connection OpenAI is failing (401 unauthorized). Rules in other projects that use it are paused too.",
    ),
  );
});

it("says when a budget pause ends", async () => {
  server.use(
    http.get("/api/v1/projects/:project/online-rules/:id", () =>
      HttpResponse.json(
        pausedRule({
          id: RULE_ID,
          name: "Relevance on answers",
          target: "trace",
          paused_reason: "budget",
          paused_detail: undefined,
          spend: { period_start: NOW, spent_usd: 5, reserved_usd: 0 },
        }),
      ),
    ),
  );
  await renderApp(RULE_URL);

  expect(await screen.findByRole("alert")).toHaveTextContent(
    "Paused: the daily budget of $5.00 is spent. The rule resumes when the day turns (UTC), or when you raise the budget.",
  );
});

it("lists the jobs with judge trace links", async () => {
  await renderApp(RULE_URL);

  const jobs = await screen.findByRole("table", { name: "Jobs" });
  const done = within(jobs).getByRole("link", { name: "View judge trace" });
  expect(done).toHaveAttribute("href", "/acme/support-bot/traces/e1000000000000000000000000000002");
  expect(within(jobs).getByText("provider status 503")).toBeInTheDocument();
});

it("shows the score distribution of the rule's scores", async () => {
  const score = scoresFixture.data[0];
  const scores = [0.1, 0.5, 0.9].map((value, index) => ({
    ...score,
    score_id: `01900000-0000-7000-8000-00000000090${index}`,
    name: "relevance",
    value: { data_type: "numeric", value },
    source: "llm_judge",
    rule_id: RULE_ID,
    deleted: false,
  }));
  server.use(
    http.get("/api/v1/projects/:project/scores", () =>
      HttpResponse.json({ data: scores, next_cursor: null }),
    ),
  );
  await renderApp(RULE_URL);

  expect(await screen.findByText("Score distribution (latest 3 scores)")).toBeInTheDocument();
  expect(screen.getAllByLabelText(/: 1$/)).toHaveLength(3);
});

it("shows neither edit nor resume to a viewer", async () => {
  server.use(
    projectAs("viewer"),
    http.get("/api/v1/projects/:project/online-rules/:id", () => HttpResponse.json(pausedRule({}))),
  );
  await renderApp(SPAN_RULE_URL);

  expect(await screen.findByRole("heading", { name: "Relevance on spans" })).toBeInTheDocument();
  expect(screen.queryByRole("link", { name: "Edit" })).toBeNull();
  expect(screen.queryByRole("button", { name: "Resume" })).toBeNull();
});

it("shows not found for an unknown rule", async () => {
  server.use(
    http.get("/api/v1/projects/:project/online-rules/:id", () => problemResponse(404, "not_found")),
  );
  await renderApp(RULE_URL);

  expect(await screen.findByText("Page not found")).toBeInTheDocument();
});
