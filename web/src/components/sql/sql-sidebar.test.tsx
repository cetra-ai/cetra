import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { HttpResponse, http } from "msw";
import { afterEach, expect, it, vi } from "vitest";
import type { Schema } from "@/api/types";
import { ProjectProvider } from "@/components/shell/project-context";
import { SIDEBAR_TAB_KEY, SqlSidebar } from "@/components/sql/sql-sidebar";
import { server } from "@/mocks/node";
import { savedViewHandlers, seedSavedViews } from "@/mocks/saved-views";
import { ADA_ID, BOB_ID } from "@/test/fixtures";
import projectFixture from "../../../../fixtures/api/static/project.json";
import schemaFixture from "../../../../fixtures/api/static/schema.json";

const LATENCY = "01900000-0000-7000-8000-000000000212";
const REFUNDS = "01900000-0000-7000-8000-000000000213";

/** Two of Ada's queries (one private) and one shared by Bob, on top of the seeded views. */
const SQL_VIEWS = [
  ...seedSavedViews(),
  {
    id: LATENCY,
    kind: "sql",
    name: "Latency p95",
    state: { query: "SELECT 1", params: {}, range: "24h" },
    owner_user_id: ADA_ID,
    shared: false,
    is_default: false,
    created_at: "2026-01-02T09:00:00.000000000Z",
    updated_at: "2026-01-02T09:00:00.000000000Z",
  },
  {
    id: REFUNDS,
    kind: "sql",
    name: "Refund rate",
    state: { query: "SELECT 2", params: {}, range: "24h" },
    owner_user_id: BOB_ID,
    shared: true,
    is_default: false,
    created_at: "2026-01-03T09:00:00.000000000Z",
    updated_at: "2026-01-03T09:00:00.000000000Z",
  },
];

afterEach(() => {
  localStorage.clear();
});

function renderSidebar(viewId?: string) {
  const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  const onLoad = vi.fn();
  const onInsert = vi.fn();
  // The JSON fixture has the shape of the schema response.
  const schema = schemaFixture as Schema<"SchemaResponse">;
  const tree = (id: string | undefined) => (
    <QueryClientProvider client={queryClient}>
      <ProjectProvider
        value={{
          org: "acme",
          project: "support-bot",
          ref: "acme/support-bot",
          data: projectFixture as Schema<"Project">,
        }}
      >
        <SqlSidebar
          schema={schema}
          failed={false}
          onInsert={onInsert}
          viewId={id}
          onLoad={onLoad}
        />
      </ProjectProvider>
    </QueryClientProvider>
  );
  const view = render(tree(viewId));
  return {
    ...view,
    onLoad,
    onInsert,
    user: userEvent.setup(),
    rerenderWith: (id: string | undefined) => view.rerender(tree(id)),
  };
}

it("shows the schema tab by default", async () => {
  renderSidebar();
  expect(screen.getByRole("complementary", { name: "SQL sidebar" })).toBeInTheDocument();
  expect(screen.getByRole("tab", { name: "Schema" })).toHaveAttribute("aria-selected", "true");
  expect(await screen.findByRole("navigation", { name: "Schema" })).toBeInTheDocument();
  expect(screen.queryByRole("textbox", { name: "Filter saved queries" })).not.toBeInTheDocument();
});

it("lists mine and shared saved queries on the saved tab", async () => {
  server.use(...savedViewHandlers(SQL_VIEWS));
  const { user } = renderSidebar();
  await user.click(screen.getByRole("tab", { name: "Saved" }));

  expect(await screen.findByRole("heading", { name: "Mine" })).toBeInTheDocument();
  const mine = screen.getByRole("heading", { name: "Mine" }).parentElement;
  const shared = screen.getByRole("heading", { name: "Shared" }).parentElement;
  if (!mine || !shared) {
    throw new Error("the groups did not render");
  }
  expect(within(mine).getByRole("button", { name: "Cost per model per day" })).toBeInTheDocument();
  expect(within(mine).getByRole("button", { name: "Latency p95" })).toBeInTheDocument();
  expect(within(shared).getByRole("button", { name: "Refund rate" })).toBeInTheDocument();
  // Shared items carry the Users icon.
  expect(
    within(shared).getByRole("button", { name: "Refund rate" }).querySelector("svg"),
  ).not.toBeNull();
  expect(within(mine).getByRole("button", { name: "Latency p95" }).querySelector("svg")).toBeNull();
});

it("filters saved queries by name, ignoring case", async () => {
  server.use(...savedViewHandlers(SQL_VIEWS));
  const { user } = renderSidebar();
  await user.click(screen.getByRole("tab", { name: "Saved" }));
  await screen.findByRole("heading", { name: "Mine" });

  await user.type(screen.getByRole("textbox", { name: "Filter saved queries" }), "REFUND");
  expect(screen.getByRole("button", { name: "Refund rate" })).toBeInTheDocument();
  expect(screen.queryByRole("button", { name: "Latency p95" })).not.toBeInTheDocument();
  expect(screen.queryByRole("heading", { name: "Mine" })).not.toBeInTheDocument();

  await user.clear(screen.getByRole("textbox", { name: "Filter saved queries" }));
  await user.type(screen.getByRole("textbox", { name: "Filter saved queries" }), "zzz");
  expect(screen.getByText("No match")).toBeInTheDocument();
});

it("calls onLoad with the clicked query and marks the loaded one", async () => {
  server.use(...savedViewHandlers(SQL_VIEWS));
  const { user, onLoad, rerenderWith } = renderSidebar(REFUNDS);
  await user.click(screen.getByRole("tab", { name: "Saved" }));

  const loaded = await screen.findByRole("button", { name: "Refund rate" });
  expect(loaded).toHaveAttribute("aria-current", "true");
  expect(screen.getByRole("button", { name: "Latency p95" })).not.toHaveAttribute("aria-current");

  await user.click(screen.getByRole("button", { name: "Latency p95" }));
  expect(onLoad).toHaveBeenCalledTimes(1);
  expect(onLoad).toHaveBeenCalledWith(
    expect.objectContaining({ id: LATENCY, name: "Latency p95" }),
  );

  rerenderWith(LATENCY);
  expect(screen.getByRole("button", { name: "Latency p95" })).toHaveAttribute(
    "aria-current",
    "true",
  );
  expect(loaded).not.toHaveAttribute("aria-current");
});

it("shows the empty message when the project has no saved queries", async () => {
  server.use(...savedViewHandlers([]));
  const { user } = renderSidebar();
  await user.click(screen.getByRole("tab", { name: "Saved" }));
  expect(
    await screen.findByText("No saved queries yet. Save one with Saved queries → Save as…"),
  ).toBeInTheDocument();
});

it("shows an alert when the saved queries cannot be loaded", async () => {
  server.use(
    http.get("*/api/v1/projects/:project/saved-views", () =>
      HttpResponse.json(
        { type: "about:blank", title: "Error", status: 500 },
        { status: 500, headers: { "Content-Type": "application/problem+json" } },
      ),
    ),
  );
  const { user } = renderSidebar();
  await user.click(screen.getByRole("tab", { name: "Saved" }));
  expect(await screen.findByRole("alert")).toHaveTextContent("Saved queries could not be loaded.");
});

it("remembers the chosen tab across remounts", async () => {
  server.use(...savedViewHandlers(SQL_VIEWS));
  const first = renderSidebar();
  await first.user.click(screen.getByRole("tab", { name: "Saved" }));
  expect(localStorage.getItem(SIDEBAR_TAB_KEY)).toBe("saved");
  first.unmount();

  renderSidebar();
  expect(screen.getByRole("tab", { name: "Saved" })).toHaveAttribute("aria-selected", "true");
  expect(await screen.findByRole("textbox", { name: "Filter saved queries" })).toBeInTheDocument();
});

it("falls back to the schema tab for an unknown stored value", () => {
  localStorage.setItem(SIDEBAR_TAB_KEY, "bogus");
  renderSidebar();
  expect(screen.getByRole("tab", { name: "Schema" })).toHaveAttribute("aria-selected", "true");
});
