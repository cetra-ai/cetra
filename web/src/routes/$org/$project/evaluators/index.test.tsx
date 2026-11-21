import { screen, waitFor, within } from "@testing-library/react";
import { HttpResponse, http } from "msw";
import { expect, it } from "vitest";
import { formatCost, formatDuration } from "@/lib/format";
import { server } from "@/mocks/node";
import { renderApp } from "@/test/render";
import evaluatorsFixture from "../../../../../../fixtures/api/static/evaluators.json";
import projectFixture from "../../../../../../fixtures/api/static/project.json";

/** The table row whose name cell reads `name`. */
function rowOf(table: HTMLElement, name: string): HTMLElement {
  const row = within(table).getByText(name).closest("tr");
  if (!row) {
    throw new Error(`no row named ${name}`);
  }
  return row;
}

it("lists evaluators with their kind, score and details", async () => {
  await renderApp("/acme/support-bot/evaluators");
  expect(
    screen.getAllByRole("heading", { level: 1 }).map((heading) => heading.textContent),
  ).toEqual(["Evaluators"]);

  const table = await screen.findByRole("table", { name: "Evaluators" });
  const row = rowOf(table, "Relevance");
  expect(within(row).getByText("LLM judge")).toBeInTheDocument();
  expect(within(row).getByText("relevance")).toBeInTheDocument();
  expect(within(row).getByText("numeric")).toBeInTheDocument();
  expect(within(row).getByText("gpt-5-mini")).toBeInTheDocument();
  expect(within(row).getByText("1")).toBeInTheDocument();
});

it("shows the empty state when the project has no evaluators", async () => {
  server.use(
    http.get("/api/v1/projects/:project/evaluators", () =>
      HttpResponse.json({ data: [], next_cursor: null }),
    ),
  );

  await renderApp("/acme/support-bot/evaluators");

  expect(await screen.findByText("No evaluators yet")).toBeInTheDocument();
  expect(screen.queryByRole("table", { name: "Evaluators" })).toBeNull();
});

it("links the name and the New button to the editor for members", async () => {
  await renderApp("/acme/support-bot/evaluators");

  const table = await screen.findByRole("table", { name: "Evaluators" });
  expect(within(table).getByRole("link", { name: "Relevance" })).toHaveAttribute(
    "href",
    "/acme/support-bot/evaluators/01900000-0000-7000-8000-000000000500",
  );
  expect(screen.getByRole("link", { name: "New evaluator" })).toHaveAttribute(
    "href",
    "/acme/support-bot/evaluators/new",
  );
});

it("shows the New button disabled to viewers, with the reason", async () => {
  server.use(
    http.get("/api/v1/projects/:project", () =>
      HttpResponse.json({ ...projectFixture, role: "viewer" }),
    ),
  );

  await renderApp("/acme/support-bot/evaluators");

  await screen.findByRole("table", { name: "Evaluators" });
  expect(screen.queryByRole("link", { name: "New evaluator" })).toBeNull();
  const button = screen.getByRole("button", { name: "New evaluator" });
  expect(button).toBeDisabled();
  expect(button.parentElement).toHaveAccessibleDescription("Only members can create evaluators");
});

it("shows a check's function and a webhook's host by kind", async () => {
  server.use(
    http.get("/api/v1/projects/:project/evaluators", () =>
      HttpResponse.json({
        data: [
          {
            ...evaluatorsFixture.data[0],
            id: "01900000-0000-7000-8000-000000000501",
            name: "Exact match",
            kind: "check",
            config: { check: "exact_match" },
          },
          {
            ...evaluatorsFixture.data[0],
            id: "01900000-0000-7000-8000-000000000502",
            name: "Hook",
            kind: "webhook",
            config: { url_host: "hooks.example.com", timeout_ms: 5000, header_names: [] },
            has_signing_secret: true,
            version: 3,
          },
        ],
        next_cursor: null,
      }),
    ),
  );

  await renderApp("/acme/support-bot/evaluators");

  const table = await screen.findByRole("table", { name: "Evaluators" });
  const check = rowOf(table, "Exact match");
  expect(within(check).getByText("Check")).toBeInTheDocument();
  expect(within(check).getByText("exact_match")).toBeInTheDocument();
  const hook = rowOf(table, "Hook");
  expect(within(hook).getByText("Webhook")).toBeInTheDocument();
  expect(within(hook).getByText("hooks.example.com")).toBeInTheDocument();
  expect(within(hook).getByText("3")).toBeInTheDocument();
});

it("shows each kind's icon and label, and the kind filter's labels", async () => {
  await renderApp("/acme/support-bot/evaluators");

  const table = await screen.findByRole("table", { name: "Evaluators" });
  const relevance = rowOf(table, "Relevance");
  expect(within(relevance).getByText("LLM judge")).toBeInTheDocument();
  expect(relevance.querySelector('[data-kind="llm_judge"]')).toHaveClass("text-violet-500");
  const check = rowOf(table, "Answer is JSON");
  expect(within(check).getByText("Check")).toBeInTheDocument();
  expect(check.querySelector('[data-kind="check"]')).toHaveClass("text-emerald-500");
  const webhook = rowOf(table, "Toxicity webhook");
  expect(within(webhook).getByText("Webhook")).toBeInTheDocument();
  expect(webhook.querySelector('[data-kind="webhook"]')).toHaveClass("text-sky-500");
  expect(within(webhook).getByText("moderation.example.com")).toBeInTheDocument();
});

it("filters the rows by name and keeps the search in the URL", async () => {
  const { user, router } = await renderApp("/acme/support-bot/evaluators");
  const table = await screen.findByRole("table", { name: "Evaluators" });

  await user.type(screen.getByRole("searchbox", { name: "Search evaluators" }), "JSON");

  await waitFor(() => expect(within(table).queryByText("Relevance")).toBeNull());
  expect(within(table).getByText("Answer is JSON")).toBeInTheDocument();
  expect(router.state.location.search).toMatchObject({ q: "JSON" });
});

it("filters the rows by kind and keeps the kind in the URL", async () => {
  const { user, router } = await renderApp("/acme/support-bot/evaluators");
  const table = await screen.findByRole("table", { name: "Evaluators" });

  await user.click(screen.getByRole("button", { name: /^Webhook/ }));

  await waitFor(() => expect(within(table).queryByText("Relevance")).toBeNull());
  expect(within(table).getByText("Toxicity webhook")).toBeInTheDocument();
  expect(screen.getByRole("button", { name: /^Webhook/ })).toHaveAttribute("aria-pressed", "true");
  expect(router.state.location.search).toMatchObject({ kind: "webhook" });
});

it("shows the kind chips with each kind's count, and no All chip", async () => {
  await renderApp("/acme/support-bot/evaluators");
  await screen.findByRole("table", { name: "Evaluators" });

  expect(screen.getByRole("region", { name: "Kind" })).toBeInTheDocument();
  expect(screen.getByRole("button", { name: "LLM judge 1" })).toBeInTheDocument();
  expect(screen.getByRole("button", { name: "Check 1" })).toBeInTheDocument();
  expect(screen.getByRole("button", { name: "Webhook 1" })).toBeInTheDocument();
  expect(screen.queryByRole("button", { name: "All" })).toBeNull();
});

it("selecting the selected kind chip again clears the filter", async () => {
  const { user, router } = await renderApp("/acme/support-bot/evaluators");
  const table = await screen.findByRole("table", { name: "Evaluators" });

  await user.click(screen.getByRole("button", { name: "Webhook 1" }));
  await waitFor(() => expect(within(table).queryByText("Relevance")).toBeNull());

  await user.click(screen.getByRole("button", { name: "Webhook 1" }));
  await waitFor(() => expect(within(table).getByText("Relevance")).toBeInTheDocument());
  expect(within(table).getByText("Answer is JSON")).toBeInTheDocument();
  expect(screen.getByRole("button", { name: "Webhook 1" })).toHaveAttribute("aria-pressed", "false");
  expect(router.state.location.search).toEqual({});
});

it("says when no evaluator matches, and clears the filters", async () => {
  const { user } = await renderApp("/acme/support-bot/evaluators");
  await screen.findByRole("table", { name: "Evaluators" });

  await user.type(screen.getByRole("searchbox", { name: "Search evaluators" }), "zzz");
  expect(await screen.findByText("No evaluators match")).toBeInTheDocument();

  await user.click(screen.getByRole("button", { name: "Clear filters" }));
  const table = await screen.findByRole("table", { name: "Evaluators" });
  expect(within(table).getByText("Relevance")).toBeInTheDocument();
  expect(screen.getByRole("searchbox", { name: "Search evaluators" })).toHaveValue("");
});

it("shows the rules that use each evaluator", async () => {
  await renderApp("/acme/support-bot/evaluators");

  const table = await screen.findByRole("table", { name: "Evaluators" });
  const relevance = rowOf(table, "Relevance");
  expect(within(relevance).getByText("2 rules")).toBeInTheDocument();
  // 40 + 2 + 1 + 3 done, failed, queued and running jobs of rule ...0700, and 12 + 9 of rule ...0701.
  expect(within(relevance).getByText("1 active · 1 paused · 67 jobs / 24h")).toBeInTheDocument();
  expect(within(rowOf(table, "Answer is JSON")).getByText("No rules")).toBeInTheDocument();
  expect(within(rowOf(table, "Toxicity webhook")).getByText("No rules")).toBeInTheDocument();
});

it("shows a dash in the Used by column when the rules cannot be loaded", async () => {
  server.use(
    http.get("/api/v1/projects/:project/online-rules", () =>
      HttpResponse.json({ error: "boom" }, { status: 500 }),
    ),
  );

  await renderApp("/acme/support-bot/evaluators");

  const table = await screen.findByRole("table", { name: "Evaluators" });
  expect(within(rowOf(table, "Relevance")).getByText("—")).toBeInTheDocument();
  expect(within(table).getByText("Relevance")).toBeInTheDocument();
});

it("links a template card to the editor with that template", async () => {
  await renderApp("/acme/support-bot/evaluators");

  expect(await screen.findByRole("heading", { name: "Start from a template" })).toBeInTheDocument();
  expect(screen.getByRole("link", { name: /^Hallucination/ })).toHaveAttribute(
    "href",
    "/acme/support-bot/evaluators/new?template=hallucination",
  );
});

it("hides the template gallery from viewers", async () => {
  server.use(
    http.get("/api/v1/projects/:project", () =>
      HttpResponse.json({ ...projectFixture, role: "viewer" }),
    ),
  );

  await renderApp("/acme/support-bot/evaluators");

  await screen.findByRole("table", { name: "Evaluators" });
  expect(screen.queryByRole("heading", { name: "Start from a template" })).toBeNull();
  expect(screen.queryByRole("link", { name: /^Hallucination/ })).toBeNull();
});

it("shows each evaluator's 7-day runs, errors, cost, latency and result", async () => {
  await renderApp("/acme/support-bot/evaluators");

  const table = await screen.findByRole("table", { name: "Evaluators" });
  for (const heading of ["Output", "Runs (7d)", "Errors", "Avg cost", "p50", "Result (7d)"]) {
    expect(within(table).getByRole("columnheader", { name: heading })).toBeInTheDocument();
  }
  const row = rowOf(table, "Relevance");
  expect(within(row).getByText("41")).toBeInTheDocument();
  expect(within(row).getByText("7.5%")).toBeInTheDocument();
  expect(within(row).getByText(formatCost(0.0016))).toBeInTheDocument();
  expect(within(row).getByText(formatDuration(820_000_000))).toBeInTheDocument();
  expect(within(row).getByText("0.82")).toBeInTheDocument();
  const webhook = rowOf(table, "Toxicity webhook");
  expect(within(webhook).getByText("0")).toBeInTheDocument();
  expect(within(webhook).getAllByText("—").length).toBeGreaterThanOrEqual(2);
});

it("shows a dash in the stats cells when the stats cannot be loaded", async () => {
  server.use(
    http.get("/api/v1/projects/:project/evaluators/stats", () =>
      HttpResponse.json({ error: "boom" }, { status: 500 }),
    ),
  );

  await renderApp("/acme/support-bot/evaluators");

  const table = await screen.findByRole("table", { name: "Evaluators" });
  const row = rowOf(table, "Relevance");
  expect(within(row).getByText("Relevance")).toBeInTheDocument();
  expect(within(row).getAllByText("—").length).toBeGreaterThanOrEqual(4);
  expect(within(table).queryByText("41")).toBeNull();
});

it("shows a dash, not a skeleton, for an evaluator the stats leave out", async () => {
  server.use(
    http.get("/api/v1/projects/:project/evaluators/stats", () =>
      HttpResponse.json({
        window: { from: "2025-12-26T00:00:00.000000000Z", to: "2026-01-01T12:00:00.000000000Z" },
        data: [],
      }),
    ),
  );

  await renderApp("/acme/support-bot/evaluators");

  const table = await screen.findByRole("table", { name: "Evaluators" });
  await within(table).findByText("2 rules");
  await waitFor(() => expect(table.querySelector('[data-slot="skeleton"]')).toBeNull());
  expect(within(rowOf(table, "Relevance")).getAllByText("—").length).toBeGreaterThanOrEqual(4);
});

it("opens after the online rules page, which reads the same evaluator list", async () => {
  const { router } = await renderApp("/acme/support-bot/online-rules");
  // The online rules list reads the evaluators to name each rule's evaluator.
  expect(await screen.findByText("Relevance on answers")).toBeInTheDocument();
  await router.navigate({
    to: "/$org/$project/evaluators",
    params: { org: "acme", project: "support-bot" },
  });
  expect(await screen.findByRole("table", { name: "Evaluators" })).toBeInTheDocument();
  expect(screen.queryByText("Something went wrong")).not.toBeInTheDocument();
  await waitFor(() => expect(router.state.status).toBe("idle"));
});

it("opens an evaluator when a row's other cells are clicked, with the way back", async () => {
  const { user, router } = await renderApp("/acme/support-bot/evaluators");

  const table = await screen.findByRole("table", { name: "Evaluators" });
  await user.click(within(rowOf(table, "Relevance")).getByText("LLM judge"));

  await waitFor(() => {
    expect(router.state.location.pathname).toBe(
      "/acme/support-bot/evaluators/01900000-0000-7000-8000-000000000500",
    );
  });
  expect(await screen.findByRole("link", { name: "Back to Evaluators" })).toBeInTheDocument();
  await waitFor(() => expect(router.state.status).toBe("idle"));
});

it("returns from a row click to the list with its search", async () => {
  const { user, router } = await renderApp("/acme/support-bot/evaluators?q=rel");

  const table = await screen.findByRole("table", { name: "Evaluators" });
  await user.click(within(rowOf(table, "Relevance")).getByText("LLM judge"));
  await user.click(await screen.findByRole("link", { name: "Back to Evaluators" }));

  await waitFor(() => {
    expect(router.state.location.pathname).toBe("/acme/support-bot/evaluators");
    expect(router.state.location.search).toEqual({ q: "rel" });
  });
  await waitFor(() => expect(router.state.status).toBe("idle"));
});

it("ignores a modified click on a row's other cell", async () => {
  const { user, router } = await renderApp("/acme/support-bot/evaluators");

  const table = await screen.findByRole("table", { name: "Evaluators" });
  await user.keyboard("{Control>}");
  await user.click(within(rowOf(table, "Relevance")).getByText("LLM judge"));
  await user.keyboard("{/Control}");

  expect(router.state.location.pathname).toBe("/acme/support-bot/evaluators");
  await waitFor(() => expect(router.state.status).toBe("idle"));
});
