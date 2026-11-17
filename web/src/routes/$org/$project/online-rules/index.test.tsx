import { screen, waitFor, within } from "@testing-library/react";
import { HttpResponse, http } from "msw";
import { expect, it } from "vitest";
import { server } from "@/mocks/node";
import { renderApp } from "@/test/render";
import onlineRuleStatsFixture from "../../../../../../fixtures/api/static/online-rule-stats.json";
import onlineRulesFixture from "../../../../../../fixtures/api/static/online-rules.json";
import projectFixture from "../../../../../../fixtures/api/static/project.json";

const PAGE = "/acme/support-bot/online-rules";
const ANSWERS_ID = "01900000-0000-7000-8000-000000000700";
const SPANS_ID = "01900000-0000-7000-8000-000000000701";
const PANEL = { name: "Online rule" };

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

  await user.hover(await screen.findByRole("heading", { name: "Online rules" }));

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

  await user.hover(await screen.findByRole("heading", { name: "Online rules" }));

  expect(await screen.findByText(/1 active rule in this organization\.$/)).toBeInTheDocument();
});

it("links each name to the rule in the panel, and the panel to the rule page", async () => {
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
  expect(within(panel).getByRole("link", { name: "Open page" })).toHaveAttribute(
    "href",
    `${PAGE}/${ANSWERS_ID}`,
  );
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

it("hides the management items from viewers", async () => {
  server.use(
    http.get("/api/v1/projects/:project", () =>
      HttpResponse.json({ ...projectFixture, role: "viewer" }),
    ),
  );

  const { user } = await renderApp(PAGE);
  const table = await screen.findByRole("table", { name: "Online rules" });
  await user.click(within(table).getByRole("button", { name: "Actions for Relevance on spans" }));

  expect(await screen.findByRole("menuitem", { name: "Open page" })).toBeInTheDocument();
  expect(screen.queryByRole("menuitem", { name: "Delete…" })).toBeNull();
  expect(screen.queryByRole("menuitem", { name: "Edit" })).toBeNull();
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
