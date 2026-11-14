import { screen, within } from "@testing-library/react";
import { HttpResponse, http } from "msw";
import { expect, it } from "vitest";
import { server } from "@/mocks/node";
import { renderApp } from "@/test/render";
import onlineRulesFixture from "../../../../../../fixtures/api/static/online-rules.json";
import projectFixture from "../../../../../../fixtures/api/static/project.json";

/** The table row whose name cell reads `name`. */
function rowOf(table: HTMLElement, name: string): HTMLElement {
  const row = within(table).getByText(name).closest("tr");
  if (!row) {
    throw new Error(`no row named ${name}`);
  }
  return row;
}

it("lists the rules with their status, sample, spend and jobs", async () => {
  await renderApp("/acme/support-bot/online-rules");

  const table = await screen.findByRole("table", { name: "Online rules" });
  const answers = rowOf(table, "Relevance on answers");
  expect(within(answers).getByText("Relevance")).toBeInTheDocument();
  expect(within(answers).getByText("Traces")).toBeInTheDocument();
  expect(within(answers).getByText("Active")).toBeInTheDocument();
  expect(within(answers).getByText("25 %")).toBeInTheDocument();
  expect(within(answers).getByText("$0.42 of $5.00 today")).toBeInTheDocument();
  expect(
    within(answers).getByText("40 done · 3 failed · 2 queued · 1 running"),
  ).toBeInTheDocument();

  const spans = rowOf(table, "Relevance on spans");
  expect(within(spans).getByText("Spans")).toBeInTheDocument();
  expect(within(spans).getByText("Paused: errors")).toBeInTheDocument();
  expect(within(spans).getByText("100 %")).toBeInTheDocument();
  expect(within(spans).getByText("—")).toBeInTheDocument();
  expect(within(spans).getByText("12 done · 9 failed")).toBeInTheDocument();

  expect(screen.getByText("2 active rules in this organization")).toBeInTheDocument();
});

it("links each name to its editor", async () => {
  await renderApp("/acme/support-bot/online-rules");

  const table = await screen.findByRole("table", { name: "Online rules" });
  expect(within(table).getByRole("link", { name: "Relevance on answers" })).toHaveAttribute(
    "href",
    "/acme/support-bot/online-rules/01900000-0000-7000-8000-000000000700/edit",
  );
  expect(screen.getByRole("link", { name: "New rule" })).toHaveAttribute(
    "href",
    "/acme/support-bot/online-rules/new",
  );
});

it("says singular for one active rule", async () => {
  server.use(
    http.get("/api/v1/projects/:project/online-rules", () =>
      HttpResponse.json({ ...onlineRulesFixture, org_enabled_rules: 1 }),
    ),
  );

  await renderApp("/acme/support-bot/online-rules");

  expect(await screen.findByText("1 active rule in this organization")).toBeInTheDocument();
});

it("shows the empty state when the project has no rules", async () => {
  server.use(
    http.get("/api/v1/projects/:project/online-rules", () =>
      HttpResponse.json({ data: [], next_cursor: null, org_enabled_rules: 0 }),
    ),
  );

  await renderApp("/acme/support-bot/online-rules");

  expect(await screen.findByText("No online rules")).toBeInTheDocument();
  expect(
    screen.getByText("A rule runs an evaluator on the traces or spans that match its filter."),
  ).toBeInTheDocument();
  expect(screen.queryByRole("table", { name: "Online rules" })).toBeNull();
});

it("disables New rule for viewers, with the reason", async () => {
  server.use(
    http.get("/api/v1/projects/:project", () =>
      HttpResponse.json({ ...projectFixture, role: "viewer" }),
    ),
  );

  await renderApp("/acme/support-bot/online-rules");

  await screen.findByRole("table", { name: "Online rules" });
  expect(screen.queryByRole("link", { name: "New rule" })).toBeNull();
  const button = screen.getByRole("button", { name: "New rule" });
  expect(button).toBeDisabled();
  expect(button.parentElement).toHaveAccessibleDescription("Only members can create online rules");
});
