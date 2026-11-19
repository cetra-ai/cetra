import { screen, waitFor, within } from "@testing-library/react";
import { HttpResponse, http } from "msw";
import { expect, it } from "vitest";
import { server } from "@/mocks/node";
import { problemResponse } from "@/test/problem";
import { renderApp } from "@/test/render";
import scoresFixture from "../../../../../../fixtures/api/responses/scores.json";
import backfillsFixture from "../../../../../../fixtures/api/static/backfills.json";
import ruleFixture from "../../../../../../fixtures/api/static/online-rule.json";
import onlineRuleStatsFixture from "../../../../../../fixtures/api/static/online-rule-stats.json";
import onlineRulesFixture from "../../../../../../fixtures/api/static/online-rules.json";
import projectFixture from "../../../../../../fixtures/api/static/project.json";

const PAGE = "/acme/support-bot/online-rules";
const ANSWERS_ID = "01900000-0000-7000-8000-000000000700";
const SPANS_ID = "01900000-0000-7000-8000-000000000701";
const ANSWERS_URL = `${PAGE}?rule=${ANSWERS_ID}`;
const SPANS_URL = `${PAGE}?rule=${SPANS_ID}`;
const PANEL = { name: "Online rule" };
const NOW = "2026-01-01T11:00:00.000000000Z";

// Single-rule overrides also match the stats path, so the panel tests answer stats first.
const STATS = http.get("/api/v1/projects/:project/online-rules/stats", () =>
  HttpResponse.json(onlineRuleStatsFixture),
);

/** The rule fixture as the span rule, paused for errors. */
function pausedRule(overrides: Record<string, unknown>) {
  return {
    ...ruleFixture,
    id: SPANS_ID,
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

/** The table row whose name cell reads `name`. */
function rowOf(table: HTMLElement, name: string): HTMLElement {
  const row = within(table).getByText(name).closest("tr");
  if (!row) {
    throw new Error(`no row named ${name}`);
  }
  return row;
}

/** The names of the rows in the table, in order (the header row excluded). */
function rowNames(table: HTMLElement): string[] {
  return within(table)
    .getAllByRole("row")
    .slice(1)
    .map((row) => within(row).getAllByRole("link")[0]?.textContent ?? "");
}

it("lists the rules with their evaluator, status, sample, spend and jobs", async () => {
  await renderApp(PAGE);
  expect(
    screen.getAllByRole("heading", { level: 1 }).map((heading) => heading.textContent),
  ).toEqual(["Online rules"]);

  const table = await screen.findByRole("table", { name: "Online rules" });
  const answers = rowOf(table, "Relevance on answers");
  expect(within(answers).getByText("Relevance")).toBeInTheDocument();
  expect(within(answers).getByText("LLM judge")).toBeInTheDocument();
  expect(within(answers).getByText("Active")).toBeInTheDocument();
  expect(within(answers).getByText("Traces · status = 'ok' · 25 %")).toBeInTheDocument();
  expect(within(answers).getByText("$0.42 / $5.00 today")).toBeInTheDocument();
  expect(
    within(answers).getByRole("img", { name: "40 done, 3 failed, 1 running, 2 queued" }),
  ).toBeInTheDocument();
  expect(within(answers).getByText("3 failed")).toBeInTheDocument();

  const spans = rowOf(table, "Relevance on spans");
  expect(within(spans).getByText("Paused: errors")).toBeInTheDocument();
  expect(within(spans).getByText("Spans · all · 100 %")).toBeInTheDocument();
  expect(within(spans).getByText("—")).toBeInTheDocument();
  expect(within(spans).getByRole("img", { name: "12 done, 9 failed" })).toBeInTheDocument();
  expect(within(spans).getByText("9 failed")).toBeInTheDocument();
});

it("counts the active rules in the title's tooltip", async () => {
  const { user } = await renderApp(PAGE);

  await user.hover(await screen.findByRole("button", { name: "About online rules" }));

  expect(
    await screen.findByText(
      "A rule runs an evaluator on new traces or spans that match its filter. 2 active rules in this organization.",
    ),
  ).toBeInTheDocument();
});

it("says singular for one active rule", async () => {
  server.use(
    http.get("/api/v1/projects/:project/online-rules", () =>
      HttpResponse.json({ ...onlineRulesFixture, org_enabled_rules: 1 }),
    ),
  );

  const { user } = await renderApp(PAGE);

  await user.hover(await screen.findByRole("button", { name: "About online rules" }));

  expect(await screen.findByText(/1 active rule in this organization\.$/)).toBeInTheDocument();
});

it("links each name to the rule in the panel, and the panel has no page link", async () => {
  const { user } = await renderApp(PAGE);

  const table = await screen.findByRole("table", { name: "Online rules" });
  const link = within(table).getByRole("link", { name: "Relevance on answers" });
  expect(link).toHaveAttribute("href", `${PAGE}?rule=${ANSWERS_ID}`);
  expect(screen.getByRole("link", { name: "New rule" })).toHaveAttribute(
    "href",
    "/acme/support-bot/online-rules/new",
  );

  await user.click(link);
  const panel = await screen.findByRole("region", PANEL);
  expect(within(panel).queryByRole("link", { name: "Open page" })).toBeNull();
});

it("filters the rows by the status strip and clears the filter", async () => {
  const { router, user } = await renderApp(PAGE);

  const table = await screen.findByRole("table", { name: "Online rules" });
  expect(screen.getByRole("button", { name: /Active/ })).toHaveTextContent("1");
  expect(screen.getByRole("button", { name: /Paused/ })).toHaveTextContent("1");
  await user.click(screen.getByRole("button", { name: /Paused/ }));

  expect(router.state.location.search.status).toBe("paused");
  expect(rowNames(table)).toEqual(["Relevance on spans"]);

  await user.click(screen.getByRole("button", { name: /Paused/ }));
  expect(router.state.location.search.status).toBeUndefined();
  expect(rowNames(table)).toEqual(["Relevance on answers", "Relevance on spans"]);
});

it("filters the rows by the search text", async () => {
  const { user } = await renderApp(PAGE);

  const table = await screen.findByRole("table", { name: "Online rules" });
  await user.type(screen.getByRole("textbox", { name: "Search rules" }), "spans");

  expect(rowNames(table)).toEqual(["Relevance on spans"]);
});

it("shows No rules match. when nothing matches the search", async () => {
  const { user } = await renderApp(PAGE);

  await screen.findByRole("table", { name: "Online rules" });
  await user.type(screen.getByRole("textbox", { name: "Search rules" }), "zzz");

  expect(screen.getByText("No rules match.")).toBeInTheDocument();
});

it("sorts by name, and a second click reverses the order", async () => {
  const { router, user } = await renderApp(PAGE);

  const table = await screen.findByRole("table", { name: "Online rules" });
  expect(rowNames(table)).toEqual(["Relevance on answers", "Relevance on spans"]);

  await user.click(within(table).getByRole("button", { name: "Name" }));
  expect(router.state.location.search).toMatchObject({ sort: "name", dir: "asc" });
  expect(rowNames(table)).toEqual(["Relevance on answers", "Relevance on spans"]);

  await user.click(within(table).getByRole("button", { name: "Name" }));
  expect(router.state.location.search).toMatchObject({ sort: "name", dir: "desc" });
  expect(rowNames(table)).toEqual(["Relevance on spans", "Relevance on answers"]);
});

it("opens the rule in the panel when its row is clicked", async () => {
  const { router, user } = await renderApp(PAGE);

  const table = await screen.findByRole("table", { name: "Online rules" });
  await user.click(within(table).getByText("Spans · all · 100 %"));

  expect(router.state.location.search).toMatchObject({ rule: SPANS_ID });
  expect(await screen.findByRole("region", PANEL)).toBeInTheDocument();
});

it("sends the enabled flag when the switch is toggled, without opening the row", async () => {
  let body: unknown;
  server.use(
    http.patch("/api/v1/projects/:project/online-rules/:id", async ({ request }) => {
      body = await request.json();
      return HttpResponse.json({ ...onlineRulesFixture.data[0], enabled: false });
    }),
  );

  const { router, user } = await renderApp(PAGE);
  const table = await screen.findByRole("table", { name: "Online rules" });
  await user.click(within(table).getByRole("switch", { name: "Enable Relevance on answers" }));

  await waitFor(() => expect(body).toEqual({ enabled: false }));
  expect(router.state.location.search.rule).toBeUndefined();
});

it("deletes a rule from its actions menu after confirming", async () => {
  let deleted: string | undefined;
  server.use(
    http.delete("/api/v1/projects/:project/online-rules/:id", ({ params }) => {
      deleted = String(params.id);
      return new HttpResponse(null, { status: 204 });
    }),
  );

  const { user } = await renderApp(PAGE);
  const table = await screen.findByRole("table", { name: "Online rules" });
  await user.click(within(table).getByRole("button", { name: "Actions for Relevance on spans" }));
  await user.click(await screen.findByRole("menuitem", { name: "Delete…" }));

  const dialog = await screen.findByRole("alertdialog");
  await user.click(within(dialog).getByRole("button", { name: "Delete" }));

  await waitFor(() => expect(deleted).toBe(SPANS_ID));
});

it("gives viewers no actions menu", async () => {
  server.use(
    http.get("/api/v1/projects/:project", () =>
      HttpResponse.json({ ...projectFixture, role: "viewer" }),
    ),
  );

  await renderApp(PAGE);
  const table = await screen.findByRole("table", { name: "Online rules" });
  expect(within(table).getByText("Relevance on spans")).toBeInTheDocument();
  expect(within(table).queryByRole("button", { name: /^Actions for / })).toBeNull();
});

it("shows the empty state when the project has no rules", async () => {
  server.use(
    http.get("/api/v1/projects/:project/online-rules", () =>
      HttpResponse.json({ data: [], next_cursor: null, org_enabled_rules: 0 }),
    ),
  );

  await renderApp(PAGE);

  expect(await screen.findByText("No online rules yet")).toBeInTheDocument();
  expect(
    screen.getByText(
      "Score live traffic automatically: a rule runs an evaluator on new traces or spans that match its filter, on a sample, within a budget.",
    ),
  ).toBeInTheDocument();
  expect(screen.getByRole("link", { name: "New rule" })).toBeInTheDocument();
  expect(screen.queryByRole("table", { name: "Online rules" })).toBeNull();
});

it("asks for an evaluator first when the project has none", async () => {
  server.use(
    http.get("/api/v1/projects/:project/online-rules", () =>
      HttpResponse.json({ data: [], next_cursor: null, org_enabled_rules: 0 }),
    ),
    http.get("/api/v1/projects/:project/evaluators", () =>
      HttpResponse.json({ data: [], next_cursor: null }),
    ),
  );

  await renderApp(PAGE);

  expect(await screen.findByText("A rule needs an evaluator first.")).toBeInTheDocument();
  expect(screen.getByRole("link", { name: "Create an evaluator" })).toHaveAttribute(
    "href",
    "/acme/support-bot/evaluators/new",
  );
  expect(screen.queryByRole("link", { name: "New rule" })).toBeNull();
});

it("disables New rule for viewers, with the reason", async () => {
  server.use(
    http.get("/api/v1/projects/:project", () =>
      HttpResponse.json({ ...projectFixture, role: "viewer" }),
    ),
  );

  await renderApp(PAGE);

  await screen.findByRole("table", { name: "Online rules" });
  expect(screen.queryByRole("link", { name: "New rule" })).toBeNull();
  const button = screen.getByRole("button", { name: "New rule" });
  expect(button).toBeDisabled();
  expect(button.parentElement).toHaveAccessibleDescription("Only members can create online rules");
});

it("shows each rule's score summary and its hourly jobs from the stats", async () => {
  await renderApp(PAGE);

  const table = await screen.findByRole("table", { name: "Online rules" });
  const answers = rowOf(table, "Relevance on answers");
  expect(await within(answers).findByText("0.83")).toBeInTheDocument();
  expect(
    within(answers).getByRole("img", { name: "Average score per hour, last 24 h" }),
  ).toBeInTheDocument();
  expect(within(answers).getByText("36 scores · 24 h")).toBeInTheDocument();

  const spans = rowOf(table, "Relevance on spans");
  expect(within(spans).getByText("No scores")).toBeInTheDocument();

  expect(
    within(table).getAllByRole("img", { name: "Finished jobs per hour, last 24 h" }),
  ).toHaveLength(2);
});

it("sorts by score, and rules without a score go last in both directions", async () => {
  const stats = structuredClone(onlineRuleStatsFixture);
  const spans = stats.data[1];
  if (spans === undefined) {
    throw new Error("the stats fixture has no second rule");
  }
  spans.scores = { ...spans.scores, count: 5, average: 0.2 };
  server.use(
    http.get("/api/v1/projects/:project/online-rules/stats", () => HttpResponse.json(stats)),
  );

  const { router, user } = await renderApp(PAGE);
  const table = await screen.findByRole("table", { name: "Online rules" });
  await waitFor(() => expect(within(table).getByText("0.83")).toBeInTheDocument());

  await user.click(within(table).getByRole("button", { name: "Score" }));
  expect(router.state.location.search).toMatchObject({ sort: "score", dir: "desc" });
  expect(rowNames(table)).toEqual(["Relevance on answers", "Relevance on spans"]);

  await user.click(within(table).getByRole("button", { name: "Score" }));
  expect(router.state.location.search).toMatchObject({ sort: "score", dir: "asc" });
  expect(rowNames(table)).toEqual(["Relevance on spans", "Relevance on answers"]);
});

it("shows the rule, its spend and its counts in the panel", async () => {
  await renderApp(ANSWERS_URL);

  const panel = await screen.findByRole("region", PANEL);
  expect(await within(panel).findByText("Relevance on answers")).toBeInTheDocument();
  expect(within(panel).getByText("Active")).toBeInTheDocument();
  expect(within(panel).getByText("$0.42 / $5.00 today")).toBeInTheDocument();
  expect(within(panel).getByText("Traces · status = 'ok' · 25 %")).toBeInTheDocument();
  expect(within(panel).getByText("+$0.01 reserved")).toBeInTheDocument();
  expect(within(panel).getByRole("progressbar", { name: "Spend" })).toHaveAttribute(
    "aria-valuenow",
    "8",
  );

  const counts = within(panel).getByRole("region", { name: "Last 24 hours" });
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

it("shows the errors banner in the panel and resumes the rule", async () => {
  let resumed = false;
  server.use(
    STATS,
    http.get("/api/v1/projects/:project/online-rules/:id", () =>
      HttpResponse.json(resumed ? pausedRule({ paused_reason: undefined }) : pausedRule({})),
    ),
    http.post("/api/v1/projects/:project/online-rules/:id/resume", () => {
      resumed = true;
      return HttpResponse.json(pausedRule({ paused_reason: undefined }));
    }),
  );
  const { user } = await renderApp(SPANS_URL);

  const panel = await screen.findByRole("region", PANEL);
  const banner = await within(panel).findByRole("alert");
  expect(banner).toHaveTextContent(
    "Paused: too many jobs failed (error rate 60 %). Fix the cause, then resume the rule.",
  );
  await user.click(within(panel).getByRole("button", { name: "Resume" }));

  await waitFor(() => expect(within(panel).queryByRole("alert")).toBeNull());
  expect(within(panel).getByText("Active")).toBeInTheDocument();
  expect(resumed).toBe(true);
});

it("names the connection in the connection banner", async () => {
  server.use(
    STATS,
    http.get("/api/v1/projects/:project/online-rules/:id", () =>
      HttpResponse.json(
        pausedRule({
          id: ANSWERS_ID,
          name: "Relevance on answers",
          target: "trace",
          paused_reason: "connection",
          paused_detail: "401 unauthorized",
        }),
      ),
    ),
  );
  await renderApp(ANSWERS_URL);

  const panel = await screen.findByRole("region", PANEL);
  const banner = await within(panel).findByRole("alert");
  // The connection's name arrives with the connections list, after the banner's first render.
  await waitFor(() =>
    expect(banner).toHaveTextContent(
      "Paused: the LLM connection OpenAI is failing (401 unauthorized). Rules in other projects that use it are paused too.",
    ),
  );
});

it("says when a budget pause ends", async () => {
  server.use(
    STATS,
    http.get("/api/v1/projects/:project/online-rules/:id", () =>
      HttpResponse.json(
        pausedRule({
          id: ANSWERS_ID,
          name: "Relevance on answers",
          target: "trace",
          paused_reason: "budget",
          paused_detail: undefined,
          spend: { period_start: NOW, spent_usd: 5, reserved_usd: 0 },
        }),
      ),
    ),
  );
  await renderApp(ANSWERS_URL);

  const panel = await screen.findByRole("region", PANEL);
  expect(await within(panel).findByRole("alert")).toHaveTextContent(
    "Paused: the daily budget of $5.00 is spent. The rule resumes when the day turns (UTC), or when you raise the budget.",
  );
});

it("lists the panel's jobs with judge trace links", async () => {
  await renderApp(ANSWERS_URL);

  const panel = await screen.findByRole("region", PANEL);
  const jobs = await within(panel).findByRole("table", { name: "Jobs" });
  const done = within(jobs).getByRole("link", { name: "View judge trace" });
  expect(done).toHaveAttribute("href", "/acme/support-bot/traces/e1000000000000000000000000000002");
  expect(within(jobs).getByText("provider status 503")).toBeInTheDocument();
});

it("shows the score distribution of the rule's scores in the panel", async () => {
  const score = scoresFixture.data[0];
  const scores = [0.1, 0.5, 0.9].map((value, index) => ({
    ...score,
    score_id: `01900000-0000-7000-8000-00000000090${index}`,
    name: "relevance",
    value: { data_type: "numeric", value },
    source: "llm_judge",
    rule_id: ANSWERS_ID,
    deleted: false,
  }));
  server.use(
    http.get("/api/v1/projects/:project/scores", () =>
      HttpResponse.json({ data: scores, next_cursor: null }),
    ),
  );
  await renderApp(ANSWERS_URL);

  const panel = await screen.findByRole("region", PANEL);
  expect(
    await within(panel).findByText("Score distribution (latest 3 scores)"),
  ).toBeInTheDocument();
  expect(within(panel).getAllByLabelText(/: 1$/)).toHaveLength(3);
});

it("shows neither edit nor resume to a viewer in the panel", async () => {
  server.use(
    projectAs("viewer"),
    STATS,
    http.get("/api/v1/projects/:project/online-rules/:id", () => HttpResponse.json(pausedRule({}))),
  );
  await renderApp(SPANS_URL);

  const panel = await screen.findByRole("region", PANEL);
  expect(await within(panel).findByText("Relevance on spans")).toBeInTheDocument();
  expect(within(panel).queryByRole("link", { name: "Edit" })).toBeNull();
  expect(within(panel).queryByRole("button", { name: "Resume" })).toBeNull();
});

it("keeps the panel when the evaluator query fails", async () => {
  server.use(
    STATS,
    http.get("/api/v1/projects/:project/online-rules/:id", () => HttpResponse.json(pausedRule({}))),
    http.get("/api/v1/projects/:project/evaluators/:id", () => problemResponse(500, "internal")),
  );
  await renderApp(SPANS_URL);

  const panel = await screen.findByRole("region", PANEL);
  expect(await within(panel).findByText("Relevance on spans")).toBeInTheDocument();
  expect(within(panel).getByText("Evaluator").nextElementSibling).toHaveTextContent("—");
  expect(within(panel).getByRole("button", { name: "Resume" })).toBeInTheDocument();
});

it("keeps the panel and shows an alert when the scores query fails", async () => {
  server.use(
    STATS,
    http.get("/api/v1/projects/:project/online-rules/:id", () => HttpResponse.json(pausedRule({}))),
    http.get("/api/v1/projects/:project/scores", () => problemResponse(500, "internal")),
  );
  await renderApp(SPANS_URL);

  const panel = await screen.findByRole("region", PANEL);
  expect(await within(panel).findByText("Relevance on spans")).toBeInTheDocument();
  const section = within(panel).getByRole("region", { name: "Score distribution" });
  expect(await within(section).findByRole("alert")).toHaveTextContent("Could not load the scores.");
  expect(within(panel).getByRole("button", { name: "Resume" })).toBeInTheDocument();
});

it("keeps the panel and shows an alert when the eval jobs query fails", async () => {
  server.use(
    STATS,
    http.get("/api/v1/projects/:project/online-rules/:id", () => HttpResponse.json(pausedRule({}))),
    http.get("/api/v1/projects/:project/eval-jobs", () => problemResponse(500, "internal")),
  );
  await renderApp(SPANS_URL);

  const panel = await screen.findByRole("region", PANEL);
  expect(await within(panel).findByText("Relevance on spans")).toBeInTheDocument();
  const section = within(panel).getByRole("region", { name: "Jobs" });
  expect(await within(section).findByRole("alert")).toHaveTextContent("Could not load the jobs.");
  expect(within(panel).getByRole("button", { name: "Resume" })).toBeInTheDocument();
});

it("reloads the eval jobs in the panel after a resume", async () => {
  let jobReads = 0;
  let resumed = false;
  server.use(
    STATS,
    http.get("/api/v1/projects/:project/online-rules/:id", () =>
      HttpResponse.json(resumed ? pausedRule({ paused_reason: undefined }) : pausedRule({})),
    ),
    http.get("/api/v1/projects/:project/eval-jobs", () => {
      jobReads += 1;
      return HttpResponse.json({ data: [], next_cursor: null });
    }),
    http.post("/api/v1/projects/:project/online-rules/:id/resume", () => {
      resumed = true;
      return HttpResponse.json(pausedRule({ paused_reason: undefined }));
    }),
  );
  const { user } = await renderApp(SPANS_URL);

  const panel = await screen.findByRole("region", PANEL);
  await within(panel).findByText("No jobs yet.");
  const reads = jobReads;
  await user.click(within(panel).getByRole("button", { name: "Resume" }));

  await waitFor(() => expect(jobReads).toBeGreaterThan(reads));
});

it("lets a viewer see the panel's backfills without the button", async () => {
  server.use(projectAs("viewer"));
  await renderApp(ANSWERS_URL);

  const panel = await screen.findByRole("region", PANEL);
  expect(await within(panel).findByRole("table", { name: "Backfills" })).toBeInTheDocument();
  expect(within(panel).queryByRole("button", { name: "Run on existing data" })).toBeNull();
});

it("does not backfill a disabled rule in the panel", async () => {
  server.use(
    STATS,
    http.get("/api/v1/projects/:project/online-rules/:id", () =>
      HttpResponse.json({ ...ruleFixture, enabled: false }),
    ),
  );
  await renderApp(ANSWERS_URL);

  const panel = await screen.findByRole("region", PANEL);
  expect(await within(panel).findByRole("button", { name: "Run on existing data" })).toBeDisabled();
  expect(
    within(panel).getByText("Enable the rule to run it on existing data."),
  ).toBeInTheDocument();
});

it("does not backfill a paused rule in the panel", async () => {
  server.use(
    STATS,
    http.get("/api/v1/projects/:project/online-rules/:id", () =>
      HttpResponse.json({
        ...ruleFixture,
        paused_reason: "errors",
        paused_at: NOW,
        paused_detail: "error rate 60 %",
      }),
    ),
  );
  await renderApp(ANSWERS_URL);

  const panel = await screen.findByRole("region", PANEL);
  expect(await within(panel).findByRole("button", { name: "Run on existing data" })).toBeDisabled();
  expect(
    within(panel).getByText("Resume the rule to run it on existing data."),
  ).toBeInTheDocument();
});

it("does not start a backfill while one runs, in the panel", async () => {
  server.use(
    http.get("/api/v1/projects/:project/online-rules/:id/backfills", () =>
      HttpResponse.json({
        data: [{ ...backfillsFixture.data[0], status: "running", finished_at: undefined }],
        next_cursor: null,
      }),
    ),
  );
  await renderApp(ANSWERS_URL);

  const panel = await screen.findByRole("region", PANEL);
  await within(panel).findByRole("button", { name: "Run on existing data" });
  // The button is enabled until the backfill list arrives; the wrapper then remounts it, so query it again.
  await waitFor(() =>
    expect(within(panel).getByRole("button", { name: "Run on existing data" })).toBeDisabled(),
  );
  expect(within(panel).getByText("A backfill of this rule is running.")).toBeInTheDocument();
});

it("shows the panel's score trend and its hourly jobs for the last 24 hours", async () => {
  await renderApp(ANSWERS_URL);

  const panel = await screen.findByRole("region", PANEL);
  const scores = await within(panel).findByRole("region", { name: "Scores, last 24 hours" });
  expect(await within(scores).findByText("0.83")).toBeInTheDocument();
  expect(within(scores).getByText("36 scores · 24 h")).toBeInTheDocument();
  expect(
    within(scores).getByRole("img", { name: "Average score per hour, last 24 h" }),
  ).toBeInTheDocument();

  const counts = within(panel).getByRole("region", { name: "Last 24 hours" });
  expect(
    await within(counts).findByRole("img", { name: "Finished jobs per hour, last 24 h" }),
  ).toBeInTheDocument();
});
