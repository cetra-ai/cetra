import { screen, waitFor, within } from "@testing-library/react";
import { HttpResponse, http } from "msw";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { retryQuery } from "@/api/client";
import { savedViewQuery } from "@/api/queries";
import { CLIPPED, NO_CHART } from "@/components/sql/result-chart";
import { DEFAULT_QUERY } from "@/lib/sql/draft";
import { EXPORT_ROLE_REASON } from "@/lib/sql/export";
import { formatSql } from "@/lib/sql/format-sql";
import { tabsKey } from "@/lib/sql/tabs";
import { server } from "@/mocks/node";
import { savedViewHandlers } from "@/mocks/saved-views";
import { ADA_ID, BOB_ID } from "@/test/fixtures";
import { renderApp } from "@/test/render";
import projectFixture from "../../../../../../fixtures/api/static/project.json";
import schemaFixture from "../../../../../../fixtures/api/static/schema.json";
import resultDocument from "../../../../../../fixtures/api/static/sql-result.json";

const PROBLEM_JSON = { "Content-Type": "application/problem+json" };

function single<T>(items: readonly T[]): T {
  expect(items).toHaveLength(1);
  const [item] = items;
  if (item === undefined) {
    throw new Error("expected one item");
  }
  return item;
}

/** The editor of the shown tab. Every tab keeps its editor mounted, so hidden ones are skipped. */
function editorContent(): HTMLElement {
  const content = Array.from(document.querySelectorAll(".cm-content")).find(
    (node) => node.closest("[hidden]") === null,
  );
  if (!(content instanceof HTMLElement)) {
    throw new Error("SQL editor did not mount");
  }
  return content;
}

type StoredTab = { query: string; params?: Record<string, string>; view?: string };

/** Stores the project's query tabs; the one at `active` (0-based) is shown. */
function storeTabs(tabs: StoredTab[], active = 0, ref = "acme/support-bot") {
  const stored = tabs.map((tab, index) => ({
    id: `tab-${index + 1}`,
    number: index + 1,
    draft: { params: {}, ...tab },
  }));
  localStorage.setItem(tabsKey(ref), JSON.stringify({ tabs: stored, active: `tab-${active + 1}` }));
}

function storeDraft(query: string, ref = "acme/support-bot") {
  storeTabs([{ query }], 0, ref);
}

function sidebar(): HTMLElement {
  return screen.getByRole("complementary", { name: "SQL sidebar" });
}

/** The query tabs of the strip, not the sidebar's or the result's tabs. */
function queryTabs(): HTMLElement[] {
  return within(screen.getByRole("tablist", { name: "Query tabs" })).getAllByRole("tab");
}

/** Opens the sidebar's Saved list. */
async function showSaved(user: { click: (element: Element) => Promise<void> }) {
  await user.click(within(sidebar()).getByRole("tab", { name: "Saved" }));
}

function captureSql(
  answer: () => Response | Promise<Response> = () => HttpResponse.json(resultDocument),
) {
  const bodies: Record<string, unknown>[] = [];
  const accepts: (string | null)[] = [];
  server.use(
    http.post("*/api/v1/projects/:project/sql", async ({ request }) => {
      accepts.push(request.headers.get("Accept"));
      bodies.push((await request.json()) as Record<string, unknown>);
      return answer();
    }),
  );
  return { bodies, accepts };
}

function problemResponse(problem: Record<string, unknown>, status: number) {
  return HttpResponse.json(
    { type: "about:blank", title: "Error", status, ...problem },
    { status, headers: PROBLEM_JSON },
  );
}

it("lists SQL in the sidebar and opens the console", async () => {
  const { router, user } = await renderApp("/acme/support-bot/traces");
  await user.click(screen.getByRole("link", { name: "SQL" }));
  await waitFor(() => {
    expect(router.state.location.pathname).toBe("/acme/support-bot/sql");
  });
  expect(await screen.findByRole("button", { name: "Schema" })).toBeInTheDocument();
  expect(screen.getByText("Run a query to see results")).toBeInTheDocument();
  expect(screen.getByRole("link", { name: "SQL" })).toHaveAttribute("aria-current", "page");
  expect(screen.getByRole("button", { name: "Run" })).toHaveAttribute("title", "Run (Ctrl↵)");
});

it("opens the console from the palette", async () => {
  const { router, user } = await renderApp("/acme/support-bot/traces");
  await screen.findByRole("grid", { name: "Traces" });
  await user.keyboard("{Meta>}k{/Meta}");
  const dialog = await screen.findByRole("dialog", { name: /Command palette/ });
  await user.click(within(dialog).getByRole("option", { name: "SQL" }));
  await waitFor(() => {
    expect(router.state.location.pathname).toBe("/acme/support-bot/sql");
  });
});

it("runs the draft over the picked range", async () => {
  const sql = captureSql();
  const { user } = await renderApp("/acme/support-bot/sql?range=1h");
  await user.click(screen.getByRole("button", { name: "Run" }));
  await waitFor(() => {
    const body = single(sql.bodies);
    expect(body.query).toBe(DEFAULT_QUERY);
    expect(Date.parse(String(body.to)) - Date.parse(String(body.from))).toBe(3_600_000);
    expect(Object.keys(body).sort()).toEqual(["from", "query", "to"]);
    expect(single(sql.accepts)).toBe("application/json");
  });
  const grid = await screen.findByRole("grid", { name: "Query results" });
  expect(
    within(grid)
      .getAllByRole("columnheader")
      .map((header) => header.textContent?.trim()),
  ).toEqual(resultDocument.columns.map((column) => column.name));
  await screen.findByText(new RegExp(`^${resultDocument.rows.length} rows? · \\d+ ms$`));
});

it("runs the stored draft with Ctrl+Enter", async () => {
  storeDraft("SELECT 1");
  const sql = captureSql();
  const { user } = await renderApp("/acme/support-bot/sql");
  const content = await waitFor(editorContent);
  content.focus();
  await user.keyboard("{Control>}{Enter}{/Control}");
  await waitFor(() => {
    expect(single(sql.bodies).query).toBe("SELECT 1");
  });
});

it("sends one request for two fast Ctrl+Enter presses", async () => {
  storeDraft("SELECT 1");
  let release = () => {};
  const gate = new Promise<void>((resolve) => {
    release = resolve;
  });
  const sql = captureSql(async () => {
    await gate;
    return HttpResponse.json(resultDocument);
  });
  await renderApp("/acme/support-bot/sql");
  const content = await waitFor(editorContent);
  content.focus();
  // Two keydowns on one stack, outside act: React has not re-rendered, so the
  // second press still sees `busy` false. Only the synchronous guard stops it.
  const press = () =>
    content.dispatchEvent(
      new KeyboardEvent("keydown", { key: "Enter", ctrlKey: true, bubbles: true }),
    );
  press();
  press();
  await waitFor(() => {
    expect(sql.bodies).toHaveLength(1);
  });
  await new Promise((resolve) => setTimeout(resolve, 50));
  expect(sql.bodies).toHaveLength(1);
  expect(await screen.findByText("Running…")).toBeInTheDocument();
  release();
  await screen.findByRole("grid", { name: "Query results" });
  // A run after the guard settled still works.
  const run = screen.getByRole("button", { name: "Run" });
  await waitFor(() => {
    expect(run).toBeEnabled();
  });
});

it("saves typing to the draft", async () => {
  storeDraft("");
  const { user } = await renderApp("/acme/support-bot/sql");
  const content = await waitFor(editorContent);
  await user.type(content, "SELECT 7");
  await waitFor(() => {
    expect(storedDraft()).toEqual({
      query: "SELECT 7",
      params: {},
    });
  });
});

it("restores the draft from localStorage", async () => {
  storeDraft("SELECT 42");
  await renderApp("/acme/support-bot/sql");
  await waitFor(() => {
    expect(editorContent().textContent).toBe("SELECT 42");
  });
});

it("keeps each project's tabs apart", async () => {
  storeDraft("SELECT 'bot'");
  storeDraft("SELECT 'play'", "acme/playground");
  const { router } = await renderApp("/acme/support-bot/sql");
  await waitFor(() => {
    expect(editorContent().textContent).toBe("SELECT 'bot'");
  });
  await router.navigate({
    to: "/$org/$project/sql",
    params: { org: "acme", project: "playground" },
  });
  await waitFor(() => {
    expect(editorContent().textContent).toBe("SELECT 'play'");
  });
});

it("does nothing for an empty query", async () => {
  storeDraft("   ");
  const sql = captureSql();
  const { user } = await renderApp("/acme/support-bot/sql");
  const run = screen.getByRole("button", { name: "Run" });
  expect(run).toBeDisabled();
  const content = await waitFor(editorContent);
  content.focus();
  await user.keyboard("{Control>}{Enter}{/Control}");
  await new Promise((resolve) => setTimeout(resolve, 50));
  expect(sql.bodies).toHaveLength(0);
  expect(run).toBeDisabled();
  expect(screen.queryByText("Running…")).toBeNull();
});

it("shows Running… and blocks a second run", async () => {
  let release = () => {};
  const gate = new Promise<void>((resolve) => {
    release = resolve;
  });
  captureSql(async () => {
    await gate;
    return HttpResponse.json(resultDocument);
  });
  const { user } = await renderApp("/acme/support-bot/sql");
  await user.click(screen.getByRole("button", { name: "Run" }));
  const status = await screen.findByRole("status");
  expect(status).toHaveTextContent("Running…");
  // While a run is pending the toolbar button is Cancel, so no second Run can start.
  expect(screen.queryByRole("button", { name: "Run" })).toBeNull();
  expect(screen.getByRole("button", { name: "Cancel" })).toBeEnabled();
  release();
  await screen.findByRole("grid", { name: "Query results" });
  expect(screen.queryByText("Running…")).toBeNull();
  expect(screen.getByRole("button", { name: "Run" })).toBeEnabled();
});

it("underlines the reported position", async () => {
  storeDraft("SELECT 1\nFROM nme");
  captureSql(() =>
    problemResponse(
      {
        code: "invalid_filter",
        detail: "invalid query",
        status: 400,
        title: "Bad Request",
        errors: [
          {
            field: "query",
            message: "table 'nme' not found",
            position: { line: 2, column: 6 },
          },
        ],
      },
      400,
    ),
  );
  const { user } = await renderApp("/acme/support-bot/sql");
  await user.click(screen.getByRole("button", { name: "Run" }));
  const alert = await screen.findByRole("alert");
  expect(alert).toHaveTextContent("table 'nme' not found");
  expect(alert.id).toMatch(/^sql-error-/);
  await waitFor(() => {
    expect(document.querySelector(".cm-sql-error")?.textContent).toBe("nme");
  });
  expect(editorContent()).toHaveAttribute("aria-invalid", "true");
  expect(editorContent()).toHaveAttribute("aria-describedby", alert.id);
  expect(screen.queryByRole("grid", { name: "Query results" })).toBeNull();
  editorContent().focus();
  await user.keyboard("{Control>}{End}{/Control}x");
  await waitFor(() => {
    expect(document.querySelector(".cm-sql-error")).toBeNull();
  });
  expect(editorContent()).toHaveAttribute("aria-invalid", "false");
});

it("shows an error without a position", async () => {
  storeDraft("SELECT nope()");
  captureSql(() =>
    problemResponse(
      {
        code: "invalid_filter",
        detail: "invalid query",
        errors: [{ field: "query", message: "unknown function nope" }],
      },
      400,
    ),
  );
  const { user } = await renderApp("/acme/support-bot/sql");
  await user.click(screen.getByRole("button", { name: "Run" }));
  expect(await screen.findByRole("alert")).toHaveTextContent("unknown function nope");
  expect(document.querySelector(".cm-sql-error")).toBeNull();
});

it("shows a timeout message", async () => {
  captureSql(() =>
    problemResponse(
      {
        code: "query_timeout",
        detail: "the query exceeded its time limit",
        title: "Unprocessable Entity",
      },
      422,
    ),
  );
  const { user } = await renderApp("/acme/support-bot/sql");
  await user.click(screen.getByRole("button", { name: "Run" }));
  expect(await screen.findByRole("alert")).toHaveTextContent("the query exceeded its time limit");
});

it("shows a generic message for a non-problem failure", async () => {
  captureSql(
    () => new HttpResponse("oops", { status: 500, headers: { "Content-Type": "text/plain" } }),
  );
  const { user } = await renderApp("/acme/support-bot/sql");
  await user.click(screen.getByRole("button", { name: "Run" }));
  expect(await screen.findByRole("alert")).toHaveTextContent("The query could not be run");
});

it("shows a generic message for an empty error body", async () => {
  captureSql(() => new HttpResponse(null, { status: 503 }));
  const { user } = await renderApp("/acme/support-bot/sql");
  await user.click(screen.getByRole("button", { name: "Run" }));
  expect(await screen.findByRole("alert")).toHaveTextContent("The query could not be run");
  expect(screen.queryByRole("grid", { name: "Query results" })).toBeNull();
});

it("replaces the result and clears the error on the next run", async () => {
  let release = () => {};
  const gate = new Promise<void>((resolve) => {
    release = resolve;
  });
  let calls = 0;
  captureSql(async () => {
    calls += 1;
    if (calls === 1) {
      return problemResponse(
        {
          code: "query_timeout",
          detail: "the query exceeded its time limit",
          title: "Unprocessable Entity",
        },
        422,
      );
    }
    await gate;
    return HttpResponse.json(resultDocument);
  });
  const { user } = await renderApp("/acme/support-bot/sql");
  await user.click(screen.getByRole("button", { name: "Run" }));
  await screen.findByRole("alert");
  await user.click(screen.getByRole("button", { name: "Run" }));
  expect(await screen.findByText("Running…")).toBeInTheDocument();
  expect(screen.queryByRole("alert")).toBeNull();
  expect(screen.queryByRole("grid", { name: "Query results" })).toBeNull();
  release();
  await screen.findByRole("grid", { name: "Query results" });
  expect(screen.queryByRole("alert")).toBeNull();
});

it("renders NULL and JSON cells", async () => {
  captureSql(() =>
    HttpResponse.json({
      columns: [
        { name: "a", type: "Utf8" },
        { name: "n", type: "Int64" },
        { name: "m", type: "Map(Utf8, Utf8)" },
      ],
      rows: [{ a: null, n: 3, m: { k: "v" } }],
      truncated: true,
    }),
  );
  const { user } = await renderApp("/acme/support-bot/sql");
  await user.click(screen.getByRole("button", { name: "Run" }));
  const grid = await screen.findByRole("grid", { name: "Query results" });
  expect(
    within(grid)
      .getAllByRole("gridcell")
      .map((cell) => cell.textContent),
  ).toEqual(["NULL", "3", '{"k":"v"}']);
  expect(await screen.findByText(/^1 row \(truncated\) · \d+ ms$/)).toBeInTheDocument();
});

it("changes the range in the URL", async () => {
  const { router, user } = await renderApp("/acme/support-bot/sql");
  await user.click(screen.getByRole("button", { name: "Last 24 hours" }));
  await user.click(await screen.findByRole("button", { name: "Last 7 days" }));
  await waitFor(() => {
    expect(router.state.location.search).toEqual({ range: "7d" });
  });
});

const COST_VIEW = "01900000-0000-7000-8000-000000000211";
const SHARED_VIEW = "01900000-0000-7000-8000-000000000212";
const SEED_QUERY =
  "SELECT model, date_trunc('day', start_time) AS day, sum(cost_total) AS cost FROM spans WHERE span_kind = 'llm' GROUP BY model, day ORDER BY day, cost DESC";

function sqlView(overrides: Record<string, unknown>) {
  return {
    id: SHARED_VIEW,
    kind: "sql",
    name: "Shared one",
    state: { query: "SELECT 1", params: {}, range: "24h" },
    owner_user_id: ADA_ID,
    shared: true,
    is_default: false,
    created_at: "2026-01-01T09:00:00.000000000Z",
    updated_at: "2026-01-01T09:00:00.000000000Z",
    ...overrides,
  };
}

function storeFullDraft(draft: { query: string; params?: Record<string, string>; view?: string }) {
  storeTabs([draft]);
}

/** The draft of the shown tab as stored. */
function storedDraft(): unknown {
  const stored = storedTabs();
  return stored.tabs.find((tab) => tab.id === stored.active)?.draft;
}

function storedTabs(): {
  tabs: { id: string; number: number; draft: unknown }[];
  active: string;
} {
  return JSON.parse(localStorage.getItem(tabsKey("acme/support-bot")) ?? "null");
}

function storedHistory(): Record<string, unknown>[] {
  return JSON.parse(localStorage.getItem("cetra.sql.history.acme/support-bot") ?? "[]");
}

function asRole(role: string) {
  server.use(
    http.get("*/api/v1/projects/:project", () => HttpResponse.json({ ...projectFixture, role })),
  );
}

function captureWrites(method: "POST" | "PATCH") {
  const bodies: unknown[] = [];
  const onStart = ({ request }: { request: Request }) => {
    if (request.method === method && request.url.includes("/saved-views")) {
      void request
        .clone()
        .json()
        .then((body) => bodies.push(body));
    }
  };
  server.events.on("request:start", onStart);
  return { bodies, stop: () => server.events.removeListener("request:start", onStart) };
}

function problemAnswer(status: number, code: string, detail: string) {
  return () =>
    HttpResponse.json(
      { type: "about:blank", title: "Error", status, code, detail },
      { status, headers: PROBLEM_JSON },
    );
}

it("sends typed parameters", async () => {
  storeFullDraft({ query: "SELECT {n:Int} AS n, {flag:Bool} AS b" });
  const sql = captureSql();
  const { user } = await renderApp("/acme/support-bot/sql");
  await user.type(await screen.findByLabelText("n (Int)"), "7");
  await user.selectOptions(screen.getByLabelText("flag (Bool)"), "true");
  await user.click(screen.getByRole("button", { name: "Run" }));
  await waitFor(() => {
    expect(single(sql.bodies).params).toEqual({ n: 7, flag: true });
  });
});

it("blocks a run with an invalid parameter, then clears the error", async () => {
  storeFullDraft({ query: "SELECT {n:Int} AS n, {flag:Bool} AS b" });
  const sql = captureSql();
  const { user } = await renderApp("/acme/support-bot/sql");
  await user.type(await screen.findByLabelText("n (Int)"), "x");
  await user.click(screen.getByRole("button", { name: "Run" }));
  await screen.findByText("Enter an integer");
  expect(screen.getByLabelText("n (Int)")).toHaveAttribute("aria-invalid", "true");
  await new Promise((resolve) => setTimeout(resolve, 50));
  expect(sql.bodies).toHaveLength(0);
  await user.type(screen.getByLabelText("n (Int)"), "{Backspace}1");
  expect(screen.queryByText("Enter an integer")).toBeNull();
});

it("clears a parameter error when its placeholder goes", async () => {
  storeFullDraft({ query: "SELECT {n:Int}" });
  const { user } = await renderApp("/acme/support-bot/sql");
  await user.click(await screen.findByRole("button", { name: "Run" }));
  await screen.findByText("Enter an integer");
  const content = await waitFor(editorContent);
  content.focus();
  await user.keyboard("{Control>}a{/Control}SELECT 1");
  await waitFor(() => {
    expect(screen.queryByText("Enter an integer")).toBeNull();
    expect(screen.queryByLabelText("n (Int)")).toBeNull();
  });
});

it("lists saved queries in the sidebar and opens one", async () => {
  const { router, user } = await renderApp("/acme/support-bot/sql?range=1h");
  await showSaved(user);
  await user.click(
    await within(sidebar()).findByRole("button", { name: "Cost per model per day" }),
  );
  await waitFor(() => {
    expect(router.state.location.search).toEqual({ range: "24h", view: COST_VIEW });
  });
  await waitFor(() => {
    expect(editorContent().textContent).toBe(SEED_QUERY);
  });
  await screen.findByRole("button", { name: /Cost per model per day/, expanded: false });
  expect(screen.queryByRole("status", { name: "Unsaved changes" })).toBeNull();
  await waitFor(() => {
    expect(storedDraft()).toMatchObject({ view: COST_VIEW });
  });
});

it("does not list another user's private query", async () => {
  server.use(
    ...savedViewHandlers([sqlView({ owner_user_id: BOB_ID, shared: false, name: "Bob's" })]),
  );
  const { user } = await renderApp("/acme/support-bot/sql");
  await showSaved(user);
  expect(await within(sidebar()).findByText(/No saved queries yet/)).toBeInTheDocument();
});

it("opens a ?view= link over another draft", async () => {
  storeDraft("SELECT 'old'");
  await renderApp(`/acme/support-bot/sql?view=${COST_VIEW}`);
  await waitFor(() => {
    expect(editorContent().textContent).toBe(SEED_QUERY);
  });
});

const PAGE_TWO = "01900000-0000-7000-8000-000000000250";
const LATE_ID = "01900000-0000-7000-8000-000000000251";

function lateView() {
  return sqlView({
    id: LATE_ID,
    name: "Late arrival",
    state: { query: "SELECT 'late'", params: {}, range: "24h" },
  });
}

it("loads a ?view= link that is on the second page of the list", async () => {
  const cursors: (string | null)[] = [];
  server.use(
    http.get("*/api/v1/projects/:project/saved-views", ({ request }) => {
      const cursor = new URL(request.url).searchParams.get("cursor");
      cursors.push(cursor);
      return HttpResponse.json(
        cursor === PAGE_TWO
          ? { data: [lateView()], next_cursor: null }
          : { data: [sqlView({})], next_cursor: PAGE_TWO },
      );
    }),
  );
  await renderApp(`/acme/support-bot/sql?view=${LATE_ID}`);
  await waitFor(() => {
    expect(editorContent().textContent).toBe("SELECT 'late'");
  });
  expect(await screen.findByRole("tab", { name: "Late arrival" })).toBeInTheDocument();
  expect(cursors[0]).toBeNull();
  expect(cursors).toContain(PAGE_TWO);
});

it("loads a ?view= link to a view that no list page holds", async () => {
  server.use(
    http.get("*/api/v1/projects/:project/saved-views", () =>
      HttpResponse.json({ data: [sqlView({})], next_cursor: null }),
    ),
    http.get("*/api/v1/projects/:project/saved-views/:id", ({ params }) =>
      params.id === LATE_ID
        ? HttpResponse.json(lateView())
        : problemResponse({ code: "not_found", detail: "Not found" }, 404),
    ),
  );
  await renderApp(`/acme/support-bot/sql?view=${LATE_ID}`);
  await waitFor(() => {
    expect(editorContent().textContent).toBe("SELECT 'late'");
  });
});

it("drops a ?view= link that cannot be read", async () => {
  storeDraft("SELECT 'kept'");
  server.use(
    http.get("*/api/v1/projects/:project/saved-views", () =>
      HttpResponse.json({ data: [sqlView({})], next_cursor: null }),
    ),
    http.get("*/api/v1/projects/:project/saved-views/:id", () =>
      problemResponse({ code: "not_found", detail: "Not found" }, 404),
    ),
  );
  const { router } = await renderApp(`/acme/support-bot/sql?view=${LATE_ID}`);
  await waitFor(() => {
    expect(router.state.location.search).toEqual({});
  });
  expect(editorContent().textContent).toBe("SELECT 'kept'");
});

it("reads a ?view= link once when the server answers 404", async () => {
  storeDraft("SELECT 'kept'");
  const reads: string[] = [];
  server.use(
    http.get("*/api/v1/projects/:project/saved-views", () =>
      HttpResponse.json({ data: [sqlView({})], next_cursor: null }),
    ),
    http.get("*/api/v1/projects/:project/saved-views/:id", () => {
      reads.push("read");
      return problemResponse({ code: "not_found", detail: "Not found" }, 404);
    }),
  );
  const { router } = await renderApp(`/acme/support-bot/sql?view=${LATE_ID}`, {
    retry: retryQuery,
  });
  await waitFor(() => {
    expect(router.state.location.search).toEqual({});
  });
  expect(reads).toHaveLength(1);
});

it("does not reopen a deleted view from the cache", async () => {
  const { router, user, queryClient } = await renderApp(`/acme/support-bot/sql?view=${COST_VIEW}`);
  await waitFor(() => {
    expect(router.state.location.search).toEqual({ range: "24h", view: COST_VIEW });
  });
  // A by-id read of the view, cached before the delete.
  queryClient.setQueryData(
    savedViewQuery("acme", "support-bot", COST_VIEW).queryKey,
    sqlView({ id: COST_VIEW, name: "Cost per model per day" }),
  );
  await user.click(
    await screen.findByRole("button", { name: /Cost per model per day/, expanded: false }),
  );
  await user.click(await screen.findByRole("menuitem", { name: "Delete" }));
  await user.click(screen.getByRole("button", { name: "Delete query" }));
  await waitFor(() => {
    expect(router.state.location.search).toEqual({ range: "24h" });
  });
  await router.navigate({
    to: "/$org/$project/sql",
    params: { org: "acme", project: "support-bot" },
    search: { view: COST_VIEW },
  });
  await waitFor(() => {
    expect(router.state.location.search).toEqual({});
  });
});

it("keeps unsaved edits of the same view on reload", async () => {
  storeFullDraft({ query: "SELECT 'edited'", view: COST_VIEW });
  await renderApp(`/acme/support-bot/sql?view=${COST_VIEW}&range=24h`);
  await screen.findByRole("status", { name: "Unsaved changes" });
  expect(editorContent().textContent).toBe("SELECT 'edited'");
});

it("shows no dot for a view link without range", async () => {
  await renderApp(`/acme/support-bot/sql?view=${COST_VIEW}`);
  await waitFor(() => {
    expect(editorContent().textContent).toBe(SEED_QUERY);
  });
  await screen.findByRole("button", { name: /Cost per model per day/, expanded: false });
  expect(screen.queryByRole("status", { name: "Unsaved changes" })).toBeNull();
});

it("shows no dot for a saved state with an extra unused param", async () => {
  server.use(
    ...savedViewHandlers([
      sqlView({
        shared: false,
        state: { query: "SELECT {n:Int}", params: { n: "5", stale: "x" }, range: "24h" },
      }),
    ]),
  );
  await renderApp(`/acme/support-bot/sql?view=${SHARED_VIEW}`);
  expect(await screen.findByLabelText("n (Int)")).toHaveValue("5");
  await screen.findByRole("button", { name: /Shared one/, expanded: false });
  expect(screen.queryByRole("status", { name: "Unsaved changes" })).toBeNull();
  await waitFor(() => {
    expect(storedDraft()).toEqual({
      query: "SELECT {n:Int}",
      params: { n: "5" },
      view: SHARED_VIEW,
    });
  });
});

it("navigating to another view loads its text, and back returns", async () => {
  server.use(
    ...savedViewHandlers([
      sqlView({ shared: false, name: "First" }),
      sqlView({
        id: COST_VIEW,
        shared: false,
        name: "Second",
        state: { query: "SELECT 2", params: {}, range: "24h" },
      }),
    ]),
  );
  const { router } = await renderApp(`/acme/support-bot/sql?view=${SHARED_VIEW}`);
  await waitFor(() => {
    expect(editorContent().textContent).toBe("SELECT 1");
  });
  await router.navigate({
    to: "/$org/$project/sql",
    params: { org: "acme", project: "support-bot" },
    search: { view: COST_VIEW },
  });
  await waitFor(() => {
    expect(editorContent().textContent).toBe("SELECT 2");
    expect(router.state.location.search).toEqual({ range: "24h", view: COST_VIEW });
  });
  router.history.back();
  await waitFor(() => {
    expect(editorContent().textContent).toBe("SELECT 1");
    expect(router.state.location.search).toEqual({ range: "24h", view: SHARED_VIEW });
  });
});

it("a range-only back and forward does not reload the view and keeps edits", async () => {
  const { router, user } = await renderApp(`/acme/support-bot/sql?view=${COST_VIEW}`);
  await waitFor(() => {
    expect(editorContent().textContent).toBe(SEED_QUERY);
  });
  editorContent().focus();
  await user.keyboard("{Control>}{End}{/Control} LIMIT 9");
  await router.navigate({
    to: "/$org/$project/sql",
    params: { org: "acme", project: "support-bot" },
    search: { range: "7d", view: COST_VIEW },
  });
  await waitFor(() => {
    expect(router.state.location.search).toEqual({ range: "7d", view: COST_VIEW });
  });
  router.history.back();
  await waitFor(() => {
    expect(router.state.location.search).toEqual({ range: "24h", view: COST_VIEW });
  });
  await new Promise((resolve) => setTimeout(resolve, 50));
  expect(editorContent().textContent).toBe(`${SEED_QUERY} LIMIT 9`);
  router.history.forward();
  await waitFor(() => {
    expect(router.state.location.search).toEqual({ range: "7d", view: COST_VIEW });
  });
  await new Promise((resolve) => setTimeout(resolve, 50));
  expect(editorContent().textContent).toBe(`${SEED_QUERY} LIMIT 9`);
});

it("drops an unknown ?view=", async () => {
  const { router } = await renderApp(
    "/acme/support-bot/sql?range=1h&view=01900000-0000-7000-8000-0000000002ff",
  );
  await waitFor(() => {
    expect(router.state.location.search).toEqual({ range: "1h" });
  });
});

it("keeps an unknown ?view= when the list is incomplete", async () => {
  server.use(
    http.get("*/api/v1/projects/:project/saved-views", () =>
      HttpResponse.json({ data: [], next_cursor: "more" }),
    ),
  );
  const { router } = await renderApp(
    "/acme/support-bot/sql?range=1h&view=01900000-0000-7000-8000-0000000002ff",
  );
  await screen.findByRole("button", { name: "Saved queries" });
  await new Promise((resolve) => setTimeout(resolve, 50));
  expect(router.state.location.search).toEqual({
    range: "1h",
    view: "01900000-0000-7000-8000-0000000002ff",
  });
});

it("keeps ?view= when the list fails", async () => {
  server.use(
    http.get("*/api/v1/projects/:project/saved-views", problemAnswer(503, "busy", "busy")),
  );
  storeDraft("SELECT 'mine'");
  const { router } = await renderApp(`/acme/support-bot/sql?view=${COST_VIEW}`);
  await screen.findByText("Saved queries could not be loaded.");
  expect(router.state.location.search).toEqual({ view: COST_VIEW });
  expect(editorContent().textContent).toBe("SELECT 'mine'");
});

it("saves the current query", async () => {
  const capture = captureWrites("POST");
  try {
    storeFullDraft({ query: "SELECT {a:Int}", params: { a: "1", old: "x" } });
    const { router, user } = await renderApp("/acme/support-bot/sql?range=6h");
    await user.click(await screen.findByRole("button", { name: "Saved queries" }));
    await user.click(screen.getByRole("menuitem", { name: "Save as…" }));
    await user.type(screen.getByLabelText("Name"), "Mine");
    await user.click(screen.getByRole("button", { name: "Save query" }));
    await waitFor(() => {
      expect(single(capture.bodies)).toEqual({
        kind: "sql",
        name: "Mine",
        state: { query: "SELECT {a:Int}", params: { a: "1" }, range: "6h" },
        shared: false,
        is_default: false,
      });
    });
    await waitFor(() => {
      expect(String(router.state.location.search.view)).toMatch(/^[0-9a-f-]{36}$/);
    });
    await screen.findByRole("button", { name: /Mine/, expanded: false });
  } finally {
    capture.stop();
  }
});

it("stores the default range when the URL has none", async () => {
  const capture = captureWrites("POST");
  try {
    const { user } = await renderApp("/acme/support-bot/sql");
    await user.click(await screen.findByRole("button", { name: "Saved queries" }));
    await user.click(screen.getByRole("menuitem", { name: "Save as…" }));
    await user.type(screen.getByLabelText("Name"), "Plain");
    await user.click(screen.getByRole("button", { name: "Save query" }));
    await waitFor(() => {
      expect(single(capture.bodies)).toMatchObject({
        state: { query: DEFAULT_QUERY, params: {}, range: "24h" },
      });
    });
  } finally {
    capture.stop();
  }
});

it("Save as on a loaded query keeps the text and switches the id", async () => {
  const { router, user } = await renderApp(`/acme/support-bot/sql?view=${COST_VIEW}`);
  await waitFor(() => {
    expect(editorContent().textContent).toBe(SEED_QUERY);
  });
  editorContent().focus();
  await user.keyboard("{Control>}{End}{/Control} LIMIT 9");
  await user.click(
    await screen.findByRole("button", { name: /Cost per model per day/, expanded: false }),
  );
  await user.click(screen.getByRole("menuitem", { name: "Save as…" }));
  await user.type(screen.getByLabelText("Name"), "Copy");
  await user.click(screen.getByRole("button", { name: "Save query" }));
  await waitFor(() => {
    const view = String(router.state.location.search.view);
    expect(view).toMatch(/^[0-9a-f-]{36}$/);
    expect(view).not.toBe(COST_VIEW);
  });
  expect(editorContent().textContent).toBe(`${SEED_QUERY} LIMIT 9`);
  await new Promise((resolve) => setTimeout(resolve, 50));
  expect(editorContent().textContent).toBe(`${SEED_QUERY} LIMIT 9`);
  await screen.findByRole("button", { name: /Copy/, expanded: false });
});

it("shows a refused save in the dialog and clears it on close", async () => {
  server.use(
    http.post(
      "*/api/v1/projects/:project/saved-views",
      problemAnswer(409, "conflict", "too many saved views"),
    ),
  );
  const { user } = await renderApp("/acme/support-bot/sql");
  await user.click(await screen.findByRole("button", { name: "Saved queries" }));
  await user.click(screen.getByRole("menuitem", { name: "Save as…" }));
  await user.type(screen.getByLabelText("Name"), "Mine");
  await user.click(screen.getByRole("button", { name: "Save query" }));
  expect(await screen.findByText("too many saved views")).toBeInTheDocument();
  expect(screen.getByRole("dialog", { name: "Save query" })).toBeInTheDocument();
  await user.click(screen.getByRole("button", { name: "Cancel" }));
  await user.click(screen.getByRole("button", { name: "Saved queries" }));
  expect(screen.queryByRole("alert")).toBeNull();
});

it("marks edits and saves over the loaded query", async () => {
  const capture = captureWrites("PATCH");
  try {
    const { user } = await renderApp(`/acme/support-bot/sql?view=${COST_VIEW}`);
    await waitFor(() => {
      expect(editorContent().textContent).toBe(SEED_QUERY);
    });
    editorContent().focus();
    await user.keyboard("{Control>}{End}{/Control} LIMIT 5");
    await screen.findByRole("status", { name: "Unsaved changes" });
    await user.click(
      screen.getByRole("button", { name: /Cost per model per day/, expanded: false }),
    );
    await user.click(await screen.findByRole("menuitem", { name: "Save" }));
    await waitFor(() => {
      expect(single(capture.bodies)).toEqual({
        state: { query: `${SEED_QUERY} LIMIT 5`, params: {}, range: "24h" },
      });
    });
    await waitFor(() => {
      expect(screen.queryByRole("status", { name: "Unsaved changes" })).toBeNull();
    });
  } finally {
    capture.stop();
  }
});

it("shows a failed save in the menu", async () => {
  const { user } = await renderApp(`/acme/support-bot/sql?view=${COST_VIEW}`);
  await waitFor(() => {
    expect(editorContent().textContent).toBe(SEED_QUERY);
  });
  editorContent().focus();
  await user.keyboard("{Control>}{End}{/Control} LIMIT 5");
  server.use(
    http.patch(
      "*/api/v1/projects/:project/saved-views/:id",
      problemAnswer(400, "bad_request", "state is too large"),
    ),
  );
  await user.click(
    await screen.findByRole("button", { name: /Cost per model per day/, expanded: false }),
  );
  await user.click(await screen.findByRole("menuitem", { name: "Save" }));
  expect(await within(screen.getByRole("menu")).findByRole("alert")).toHaveTextContent(
    "state is too large",
  );
});

it("discards changes", async () => {
  const { user } = await renderApp(`/acme/support-bot/sql?view=${COST_VIEW}`);
  await waitFor(() => {
    expect(editorContent().textContent).toBe(SEED_QUERY);
  });
  editorContent().focus();
  await user.keyboard("{Control>}{End}{/Control} LIMIT 5");
  await screen.findByRole("status", { name: "Unsaved changes" });
  await user.click(screen.getByRole("button", { name: /Cost per model per day/, expanded: false }));
  await user.click(await screen.findByRole("menuitem", { name: "Discard changes" }));
  await waitFor(() => {
    expect(editorContent().textContent).toBe(SEED_QUERY);
    expect(screen.queryByRole("status", { name: "Unsaved changes" })).toBeNull();
  });
});

it("renames the loaded query", async () => {
  const { user } = await renderApp(`/acme/support-bot/sql?view=${COST_VIEW}`);
  await user.click(
    await screen.findByRole("button", { name: /Cost per model per day/, expanded: false }),
  );
  await user.click(await screen.findByRole("menuitem", { name: "Rename…" }));
  await user.clear(screen.getByLabelText("Name"));
  await user.type(screen.getByLabelText("Name"), "Costs");
  await user.click(screen.getByRole("button", { name: "Rename" }));
  await screen.findByRole("button", { name: /Costs/, expanded: false });
});

it("shows a refused rename in the dialog", async () => {
  server.use(
    http.patch(
      "*/api/v1/projects/:project/saved-views/:id",
      problemAnswer(400, "bad_request", "name is too long"),
    ),
  );
  const { user } = await renderApp(`/acme/support-bot/sql?view=${COST_VIEW}`);
  await user.click(
    await screen.findByRole("button", { name: /Cost per model per day/, expanded: false }),
  );
  await user.click(await screen.findByRole("menuitem", { name: "Rename…" }));
  await user.clear(screen.getByLabelText("Name"));
  await user.type(screen.getByLabelText("Name"), "Costs");
  await user.click(screen.getByRole("button", { name: "Rename" }));
  expect(await screen.findByText("name is too long")).toBeInTheDocument();
  expect(screen.getByRole("dialog", { name: "Rename query" })).toBeInTheDocument();
});

it("shares a query as a member", async () => {
  asRole("member");
  const capture = captureWrites("PATCH");
  try {
    const { user } = await renderApp(`/acme/support-bot/sql?view=${COST_VIEW}`);
    await user.click(
      await screen.findByRole("button", { name: /Cost per model per day/, expanded: false }),
    );
    await user.click(await screen.findByRole("menuitem", { name: "Share with project" }));
    await waitFor(() => {
      expect(single(capture.bodies)).toEqual({ shared: true });
    });
  } finally {
    capture.stop();
  }
});

it("stops sharing an owned query", async () => {
  server.use(...savedViewHandlers([sqlView({})]));
  const capture = captureWrites("PATCH");
  try {
    const { user } = await renderApp(`/acme/support-bot/sql?view=${SHARED_VIEW}`);
    await user.click(await screen.findByRole("button", { name: /Shared one/, expanded: false }));
    await user.click(await screen.findByRole("menuitem", { name: "Stop sharing" }));
    await waitFor(() => {
      expect(single(capture.bodies)).toEqual({ shared: false });
    });
  } finally {
    capture.stop();
  }
});

it("lets an admin moderate someone else's shared query", async () => {
  server.use(...savedViewHandlers([sqlView({ owner_user_id: BOB_ID })]));
  const { user } = await renderApp(`/acme/support-bot/sql?view=${SHARED_VIEW}`);
  await user.click(await screen.findByRole("button", { name: /Shared one/, expanded: false }));
  expect(await screen.findByRole("menuitem", { name: "Stop sharing" })).toBeInTheDocument();
  expect(screen.getByRole("menuitem", { name: "Delete" })).toBeInTheDocument();
  expect(screen.queryByRole("menuitem", { name: "Save" })).toBeNull();
  expect(screen.queryByRole("menuitem", { name: "Rename…" })).toBeNull();
});

it("hides Stop sharing for an ownerless shared query", async () => {
  const orphan = sqlView({});
  delete (orphan as { owner_user_id?: string }).owner_user_id;
  server.use(...savedViewHandlers([orphan]));
  const { user } = await renderApp(`/acme/support-bot/sql?view=${SHARED_VIEW}`);
  await user.click(await screen.findByRole("button", { name: /Shared one/, expanded: false }));
  await screen.findByRole("menuitem", { name: "Delete" });
  expect(screen.queryByRole("menuitem", { name: "Stop sharing" })).toBeNull();
});

it("viewers cannot share", async () => {
  asRole("viewer");
  const { user } = await renderApp(`/acme/support-bot/sql?view=${COST_VIEW}`);
  await user.click(
    await screen.findByRole("button", { name: /Cost per model per day/, expanded: false }),
  );
  const share = await screen.findByRole("menuitem", { name: /^Share with project/ });
  expect(share).toHaveAttribute("aria-disabled", "true");
  expect(share).toHaveTextContent("Needs the member role");
  await user.keyboard("{Escape}");
  await user.click(screen.getByRole("button", { name: /Cost per model per day/, expanded: false }));
  await user.click(await screen.findByRole("menuitem", { name: "Save as…" }));
  expect(screen.getByRole("switch", { name: "Share with project" })).toBeDisabled();
});

it("a viewer cannot change their shared query but may delete it", async () => {
  asRole("viewer");
  server.use(...savedViewHandlers([sqlView({})]));
  const { user } = await renderApp(`/acme/support-bot/sql?view=${SHARED_VIEW}`);
  await user.click(await screen.findByRole("button", { name: /Shared one/, expanded: false }));
  await screen.findByRole("menuitem", { name: /^Rename…/ });
  const items = screen.getAllByRole("menuitem");
  const save = items.find(
    (item) => item.textContent?.startsWith("Save") && !item.textContent.startsWith("Save as"),
  );
  const rename = items.find((item) => item.textContent?.startsWith("Rename…"));
  expect(save).toBeDefined();
  expect(rename).toBeDefined();
  for (const item of [save, rename]) {
    expect(item).toHaveAttribute("aria-disabled", "true");
    expect(item).toHaveTextContent("Changing a shared query needs the member role");
  }
  expect(screen.getByRole("menuitem", { name: "Delete" })).not.toHaveAttribute(
    "aria-disabled",
    "true",
  );
  expect(screen.getByRole("menuitem", { name: "Stop sharing" })).not.toHaveAttribute(
    "aria-disabled",
    "true",
  );
});

it("deletes the loaded query", async () => {
  const { router, user } = await renderApp(`/acme/support-bot/sql?view=${COST_VIEW}`);
  await waitFor(() => {
    expect(router.state.location.search).toEqual({ range: "24h", view: COST_VIEW });
  });
  await user.click(
    await screen.findByRole("button", { name: /Cost per model per day/, expanded: false }),
  );
  await user.click(await screen.findByRole("menuitem", { name: "Delete" }));
  await user.click(screen.getByRole("button", { name: "Delete query" }));
  await waitFor(() => {
    expect(router.state.location.search).toEqual({ range: "24h" });
  });
  await user.click(await screen.findByRole("button", { name: "Saved queries" }));
  expect(await screen.findByRole("menuitem", { name: "Save as…" })).toBeInTheDocument();
  expect(screen.queryByRole("menuitem", { name: "Delete" })).toBeNull();
});

it("shows a refused delete in the dialog", async () => {
  server.use(
    http.delete(
      "*/api/v1/projects/:project/saved-views/:id",
      problemAnswer(409, "conflict", "cannot delete"),
    ),
  );
  const { user } = await renderApp(`/acme/support-bot/sql?view=${COST_VIEW}`);
  await user.click(
    await screen.findByRole("button", { name: /Cost per model per day/, expanded: false }),
  );
  await user.click(await screen.findByRole("menuitem", { name: "Delete" }));
  await user.click(screen.getByRole("button", { name: "Delete query" }));
  expect(await screen.findByText("cannot delete")).toBeInTheDocument();
  expect(screen.getByRole("dialog", { name: "Delete query" })).toBeInTheDocument();
});

it("saves and reloads an absolute range", async () => {
  const capture = captureWrites("POST");
  try {
    const { router, user } = await renderApp(
      "/acme/support-bot/sql?from=2026-01-01T00:00:00Z&to=2026-01-02T00:00:00Z",
    );
    await user.click(await screen.findByRole("button", { name: "Saved queries" }));
    await user.click(screen.getByRole("menuitem", { name: "Save as…" }));
    await user.type(screen.getByLabelText("Name"), "Day");
    await user.click(screen.getByRole("button", { name: "Save query" }));
    await waitFor(() => {
      expect(single(capture.bodies)).toMatchObject({
        state: {
          query: DEFAULT_QUERY,
          params: {},
          from: "2026-01-01T00:00:00Z",
          to: "2026-01-02T00:00:00Z",
        },
      });
    });
    await waitFor(() => {
      expect(router.state.location.search.view).toBeDefined();
    });
    const created = String(router.state.location.search.view);
    await router.navigate({
      to: "/$org/$project/sql",
      params: { org: "acme", project: "support-bot" },
      search: { range: "1h" },
    });
    await showSaved(user);
    await user.click(await within(sidebar()).findByRole("button", { name: "Day" }));
    await waitFor(() => {
      expect(router.state.location.search).toEqual({
        from: "2026-01-01T00:00:00Z",
        to: "2026-01-02T00:00:00Z",
        view: created,
      });
    });
  } finally {
    capture.stop();
  }
});

it("loads saved params and runs them with Ctrl+Enter", async () => {
  server.use(
    ...savedViewHandlers([
      sqlView({
        shared: false,
        state: { query: "SELECT {n:Int}", params: { n: "5" }, range: "24h" },
      }),
    ]),
  );
  const sql = captureSql();
  const { user } = await renderApp(`/acme/support-bot/sql?view=${SHARED_VIEW}`);
  expect(await screen.findByLabelText("n (Int)")).toHaveValue("5");
  editorContent().focus();
  await user.keyboard("{Control>}{Enter}{/Control}");
  await waitFor(() => {
    expect(single(sql.bodies).params).toEqual({ n: 5 });
  });
});

it("opening another query opens it in a new tab and keeps this one's edits", async () => {
  server.use(
    ...savedViewHandlers([
      sqlView({ shared: false }),
      sqlView({
        id: COST_VIEW,
        name: "Other",
        shared: false,
        state: { query: "SELECT 2", params: {}, range: "24h" },
      }),
    ]),
  );
  const { router, user } = await renderApp(`/acme/support-bot/sql?view=${SHARED_VIEW}`);
  await waitFor(() => {
    expect(editorContent().textContent).toBe("SELECT 1");
  });
  editorContent().focus();
  await user.keyboard("{Control>}{End}{/Control} + 1");
  await showSaved(user);
  await user.click(await within(sidebar()).findByRole("button", { name: "Other" }));
  await waitFor(() => {
    expect(editorContent().textContent).toBe("SELECT 2");
    expect(router.state.location.search.view).toBe(COST_VIEW);
  });
  await user.click(screen.getByRole("tab", { name: "Shared one" }));
  await waitFor(() => {
    expect(editorContent().textContent).toBe("SELECT 1 + 1");
    expect(router.state.location.search.view).toBe(SHARED_VIEW);
  });
});

it("an example clears the parameters and keeps the view", async () => {
  storeFullDraft({ query: "SELECT {n:Int}", params: { n: "3" }, view: COST_VIEW });
  const { user } = await renderApp(`/acme/support-bot/sql?view=${COST_VIEW}&range=24h`);
  await screen.findByLabelText("n (Int)");
  await user.click(screen.getByRole("button", { name: "Examples" }));
  await user.click(await screen.findByRole("menuitem", { name: "Users by spend" }));
  await waitFor(() => {
    expect(editorContent().textContent).toContain("ORDER BY cost DESC");
  });
  expect(screen.queryByLabelText("n (Int)")).toBeNull();
  await waitFor(() => {
    expect(storedDraft()).toMatchObject({ params: {}, view: COST_VIEW });
  });
  await screen.findByRole("status", { name: "Unsaved changes" });
  await new Promise((resolve) => setTimeout(resolve, 50));
  expect(editorContent().textContent).toContain("ORDER BY cost DESC");
});

it("a history pick keeps a stale ?view= from reloading", async () => {
  localStorage.setItem(
    "cetra.sql.history.acme/support-bot",
    JSON.stringify([{ query: "SELECT 9", params: {}, ranAt: "2026-01-01T00:00:00.000Z", rows: 1 }]),
  );
  const { user } = await renderApp(`/acme/support-bot/sql?view=${COST_VIEW}`);
  await waitFor(() => {
    expect(editorContent().textContent).toBe(SEED_QUERY);
  });
  await user.click(screen.getByRole("button", { name: "History" }));
  await user.click(await screen.findByRole("menuitem", { name: /^SELECT 9/ }));
  await waitFor(() => {
    expect(editorContent().textContent).toBe("SELECT 9");
  });
  await new Promise((resolve) => setTimeout(resolve, 50));
  expect(editorContent().textContent).toBe("SELECT 9");
});

it("records runs with only the used params and restores them", async () => {
  storeFullDraft({ query: "SELECT {n:Int}", params: { n: "4", stale: "x" } });
  captureSql();
  const { user } = await renderApp("/acme/support-bot/sql");
  await user.click(await screen.findByRole("button", { name: "Run" }));
  await screen.findByRole("grid", { name: "Query results" });
  await waitFor(() => {
    expect(storedHistory()[0]).toMatchObject({
      query: "SELECT {n:Int}",
      params: { n: "4" },
      rows: resultDocument.rows.length,
    });
  });
  const content = await waitFor(editorContent);
  content.focus();
  await user.keyboard("{Control>}a{/Control}SELECT 2");
  await user.click(screen.getByRole("button", { name: "History" }));
  await user.click(await screen.findByRole("menuitem", { name: /^SELECT \{n:Int\}/ }));
  await waitFor(() => {
    expect(editorContent().textContent).toBe("SELECT {n:Int}");
    expect(screen.getByLabelText("n (Int)")).toHaveValue("4");
  });
  await user.click(screen.getByRole("button", { name: "History" }));
  await user.click(await screen.findByRole("menuitem", { name: "Clear history" }));
  await user.click(screen.getByRole("button", { name: "History" }));
  expect(await screen.findByRole("menuitem", { name: "No runs yet" })).toBeInTheDocument();
});

it("keeps one history entry for a repeated run", async () => {
  storeDraft("SELECT 1");
  const sql = captureSql();
  const { user } = await renderApp("/acme/support-bot/sql");
  await user.click(screen.getByRole("button", { name: "Run" }));
  await screen.findByRole("grid", { name: "Query results" });
  await user.click(screen.getByRole("button", { name: "Run" }));
  await waitFor(() => {
    expect(sql.bodies).toHaveLength(2);
  });
  await waitFor(() => {
    expect(screen.getByRole("button", { name: "Run" })).toBeEnabled();
  });
  await waitFor(() => {
    expect(storedHistory()).toHaveLength(1);
  });
});

it("records a failed run", async () => {
  storeDraft("SELECT nope");
  captureSql(() =>
    problemResponse(
      {
        code: "invalid_filter",
        detail: "invalid query",
        errors: [{ field: "query", message: "unknown column nope" }],
      },
      400,
    ),
  );
  const { user } = await renderApp("/acme/support-bot/sql");
  await user.click(screen.getByRole("button", { name: "Run" }));
  await screen.findByRole("alert");
  await waitFor(() => {
    expect(storedHistory()[0]).not.toHaveProperty("rows");
  });
  await user.click(screen.getByRole("button", { name: "History" }));
  expect(await screen.findByRole("menuitem", { name: /^SELECT nope/ })).toHaveTextContent("failed");
});

it("works when localStorage throws", async () => {
  storeDraft("");
  const setItem = vi.spyOn(Storage.prototype, "setItem").mockImplementation(() => {
    throw new Error("quota");
  });
  try {
    const sql = captureSql();
    const { user } = await renderApp("/acme/support-bot/sql");
    const content = await waitFor(editorContent);
    await user.type(content, "SELECT 3");
    await user.click(screen.getByRole("button", { name: "Run" }));
    await waitFor(() => {
      expect(single(sql.bodies).query).toBe("SELECT 3");
    });
    await user.click(screen.getByRole("button", { name: "History" }));
    expect(await screen.findByRole("menuitem", { name: "No runs yet" })).toBeInTheDocument();
  } finally {
    setItem.mockRestore();
  }
});

/** `saveBlob` revokes after ~1 s; the restore must not put the stubs back before that. */
let restoreDownloads: (() => Promise<void>) | undefined;
afterEach(async () => {
  await restoreDownloads?.();
  restoreDownloads = undefined;
});

function captureDownloads() {
  const saved: { href: string; download: string; rel: string }[] = [];
  const revoked: string[] = [];
  const target = URL as unknown as Record<string, unknown>;
  const original = {
    createObjectURL: target.createObjectURL,
    revokeObjectURL: target.revokeObjectURL,
  };
  target.createObjectURL = () => "blob:test";
  target.revokeObjectURL = (url: string) => {
    revoked.push(url);
  };
  const click = vi.spyOn(HTMLAnchorElement.prototype, "click").mockImplementation(function (
    this: HTMLAnchorElement,
  ) {
    saved.push({ href: this.getAttribute("href") ?? "", download: this.download, rel: this.rel });
  });
  restoreDownloads = async () => {
    if (saved.length > 0) {
      await waitFor(() => {
        if (revoked.length < saved.length) {
          throw new Error(`revoke pending: ${revoked.length}/${saved.length}`);
        }
      });
    }
    click.mockRestore();
    for (const [key, value] of Object.entries(original)) {
      if (value === undefined) {
        delete target[key];
      } else {
        target[key] = value;
      }
    }
  };
  return { saved, revoked };
}

/** Watches `POST /sql` without answering it: the default mock still replies. */
function watchSql() {
  const seen: { accept: string | null; body: Record<string, unknown> }[] = [];
  server.use(
    http.post("*/api/v1/projects/:project/sql", async ({ request }) => {
      const body = (await request.clone().json()) as Record<string, unknown>;
      seen.push({ accept: request.headers.get("Accept"), body });
    }),
  );
  return seen;
}

async function exportAs(user: { click: (element: Element) => Promise<void> }, label: string) {
  await user.click(await screen.findByRole("button", { name: "Export" }));
  await user.click(await screen.findByRole("menuitem", { name: label }));
}

it("exports CSV with the run's body", async () => {
  const downloads = captureDownloads();
  const seen = watchSql();
  const { user } = await renderApp("/acme/support-bot/sql?range=1h");
  await exportAs(user, "CSV");
  await waitFor(() => {
    expect(downloads.saved).toEqual([
      { href: "blob:test", download: "query-20260101T100000Z.csv", rel: "noopener" },
    ]);
  });
  const request = single(seen);
  expect(request.accept).toBe("text/csv");
  expect(Object.keys(request.body).sort()).toEqual(["from", "query", "to"]);
  expect(Date.parse(String(request.body.to)) - Date.parse(String(request.body.from))).toBe(
    3_600_000,
  );
});

it("exports NDJSON and Parquet", async () => {
  const downloads = captureDownloads();
  const seen = watchSql();
  const { user } = await renderApp("/acme/support-bot/sql");
  await exportAs(user, "NDJSON");
  await waitFor(() => {
    expect(downloads.saved).toEqual([
      { href: "blob:test", download: "query-20260101T100000Z.ndjson", rel: "noopener" },
    ]);
  });
  await waitFor(() => {
    expect(screen.queryByText("Exporting…")).toBeNull();
  });
  await exportAs(user, "Parquet");
  await waitFor(() => {
    expect(downloads.saved).toEqual([
      { href: "blob:test", download: "query-20260101T100000Z.ndjson", rel: "noopener" },
      { href: "blob:test", download: "query-20260101T100000Z.parquet", rel: "noopener" },
    ]);
  });
  expect(seen.map((entry) => entry.accept)).toEqual([
    "application/x-ndjson",
    "application/vnd.apache.parquet",
  ]);
});

it("shows Exporting… and blocks Run", async () => {
  captureDownloads();
  let release = () => {};
  const gate = new Promise<void>((resolve) => {
    release = resolve;
  });
  server.use(
    http.post("*/api/v1/projects/:project/sql", async () => {
      await gate;
      return new HttpResponse("a\r\n", { headers: { "Content-Type": "text/csv" } });
    }),
  );
  const { user } = await renderApp("/acme/support-bot/sql");
  await exportAs(user, "CSV");
  expect(await screen.findByText("Exporting…")).toBeInTheDocument();
  expect(screen.getByRole("button", { name: "Run" })).toBeDisabled();
  await user.click(screen.getByRole("button", { name: "Export" }));
  const items = await screen.findAllByRole("menuitem");
  expect(items).toHaveLength(3);
  for (const item of items) {
    expect(item).toHaveAttribute("aria-disabled", "true");
  }
  await user.keyboard("{Escape}");
  release();
  await waitFor(() => {
    expect(screen.queryByText("Exporting…")).toBeNull();
    expect(screen.getByRole("button", { name: "Run" })).toBeEnabled();
  });
});

it("blocks export while a run is pending", async () => {
  let release = () => {};
  const gate = new Promise<void>((resolve) => {
    release = resolve;
  });
  captureSql(async () => {
    await gate;
    return HttpResponse.json(resultDocument);
  });
  const { user } = await renderApp("/acme/support-bot/sql");
  await user.click(screen.getByRole("button", { name: "Run" }));
  expect(await screen.findByText("Running…")).toBeInTheDocument();
  await user.click(screen.getByRole("button", { name: "Export" }));
  expect(await screen.findByRole("menuitem", { name: "CSV" })).toHaveAttribute(
    "aria-disabled",
    "true",
  );
  await user.keyboard("{Escape}");
  release();
  await screen.findByRole("grid", { name: "Query results" });
});

it("blocks an export with an invalid parameter", async () => {
  storeFullDraft({ query: "SELECT {n:Int}", params: { n: "x" } });
  const seen = watchSql();
  const { user } = await renderApp("/acme/support-bot/sql");
  await exportAs(user, "CSV");
  await screen.findByText("Enter an integer");
  await new Promise((resolve) => setTimeout(resolve, 50));
  expect(seen).toHaveLength(0);
});

it("disables export items on an empty query", async () => {
  storeDraft("  ");
  const { user } = await renderApp("/acme/support-bot/sql");
  await user.click(screen.getByRole("button", { name: "Export" }));
  expect(await screen.findByRole("menuitem", { name: "CSV" })).toHaveAttribute(
    "aria-disabled",
    "true",
  );
});

it("disables export for viewers", async () => {
  asRole("viewer");
  const { user } = await renderApp("/acme/support-bot/sql");
  const trigger = screen.getByRole("button", { name: "Export" });
  expect(trigger).toBeEnabled();
  await user.click(trigger);
  const item = await screen.findByRole("menuitem", { name: /^CSV/ });
  expect(item).toHaveAttribute("aria-disabled", "true");
  expect(item).toHaveTextContent(EXPORT_ROLE_REASON);
});

it("shows a refused export", async () => {
  const downloads = captureDownloads();
  server.use(
    http.post("*/api/v1/projects/:project/sql", () =>
      HttpResponse.json(
        {
          type: "about:blank",
          title: "Forbidden",
          status: 403,
          code: "forbidden",
          detail: "the export was refused",
        },
        { status: 403, headers: PROBLEM_JSON },
      ),
    ),
  );
  const { user } = await renderApp("/acme/support-bot/sql");
  await exportAs(user, "CSV");
  // The fixture caller is an owner, so a refusal is not explained as the member rule.
  expect(await screen.findByRole("alert")).toHaveTextContent("the export was refused");
  expect(screen.queryByText(EXPORT_ROLE_REASON)).toBeNull();
  expect(downloads.saved).toEqual([]);
});

it("reports a network failure", async () => {
  const downloads = captureDownloads();
  server.use(http.post("*/api/v1/projects/:project/sql", () => HttpResponse.error()));
  const { user } = await renderApp("/acme/support-bot/sql");
  await exportAs(user, "CSV");
  expect(await screen.findByRole("alert")).toHaveTextContent(
    "The export failed before it finished",
  );
  expect(downloads.saved).toEqual([]);
});

it("Run clears an export error", async () => {
  captureDownloads();
  server.use(
    http.post("*/api/v1/projects/:project/sql", () =>
      HttpResponse.json(
        {
          type: "about:blank",
          title: "Forbidden",
          status: 403,
          code: "forbidden",
          detail: "the export was refused",
        },
        { status: 403, headers: PROBLEM_JSON },
      ),
    ),
  );
  const { user } = await renderApp("/acme/support-bot/sql");
  await exportAs(user, "CSV");
  await screen.findByText("the export was refused");
  server.use(http.post("*/api/v1/projects/:project/sql", () => HttpResponse.json(resultDocument)));
  await user.click(screen.getByRole("button", { name: "Run" }));
  await waitFor(() => {
    expect(screen.queryByText("the export was refused")).toBeNull();
  });
  await screen.findByRole("grid", { name: "Query results" });
});

it("draws a line chart with one series per value and keeps the tab", async () => {
  captureSql(() =>
    HttpResponse.json({
      columns: [
        { name: "day", type: 'Timestamp(Nanosecond, Some("UTC"))' },
        { name: "model", type: "Utf8" },
        { name: "cost", type: "Float64" },
      ],
      rows: [
        { day: "2026-01-01T00:00:00.000000000Z", model: "a;}b{", cost: 1 },
        { day: "2026-01-02T00:00:00.000000000Z", model: "b", cost: 2 },
      ],
      truncated: false,
    }),
  );
  const { user } = await renderApp("/acme/support-bot/sql");
  await user.click(screen.getByRole("button", { name: "Run" }));
  await screen.findByRole("grid", { name: "Query results" });
  await user.click(screen.getByRole("tab", { name: "Chart" }));
  expect(
    await screen.findByRole("region", {
      name: "Line chart of cost by day, one series per model",
    }),
  ).toBeInTheDocument();
  await user.click(screen.getByRole("button", { name: "Run" }));
  await waitFor(() => {
    expect(screen.getByRole("tab", { name: "Chart" })).toHaveAttribute("aria-selected", "true");
  });
  expect(
    screen.getByRole("region", { name: "Line chart of cost by day, one series per model" }),
  ).toBeInTheDocument();
});

it("draws a bar chart", async () => {
  captureSql(() =>
    HttpResponse.json({
      columns: [
        { name: "name", type: "Utf8" },
        { name: "traces", type: "Int64" },
      ],
      rows: [
        { name: "a", traces: 1 },
        { name: "b", traces: 2 },
      ],
      truncated: false,
    }),
  );
  const { user } = await renderApp("/acme/support-bot/sql");
  await user.click(screen.getByRole("button", { name: "Run" }));
  await screen.findByRole("grid", { name: "Query results" });
  await user.click(screen.getByRole("tab", { name: "Chart" }));
  expect(
    await screen.findByRole("region", { name: "Bar chart of traces by name" }),
  ).toBeInTheDocument();
});

it("says when no chart fits", async () => {
  captureSql(() =>
    HttpResponse.json({
      columns: [{ name: "name", type: "Utf8" }],
      rows: [{ name: "a" }],
      truncated: false,
    }),
  );
  const { user } = await renderApp("/acme/support-bot/sql");
  await user.click(screen.getByRole("button", { name: "Run" }));
  await screen.findByRole("grid", { name: "Query results" });
  await user.click(screen.getByRole("tab", { name: "Chart" }));
  expect(await screen.findByText(NO_CHART)).toBeInTheDocument();
});

it("notes a clipped chart", async () => {
  captureSql(() =>
    HttpResponse.json({
      columns: [
        { name: "name", type: "Utf8" },
        { name: "a", type: "Int64" },
        { name: "b", type: "Int64" },
        { name: "c", type: "Int64" },
        { name: "d", type: "Int64" },
        { name: "e", type: "Int64" },
        { name: "f", type: "Int64" },
      ],
      rows: [{ name: "x", a: 1, b: 1, c: 1, d: 1, e: 1, f: 1 }],
      truncated: false,
    }),
  );
  const { user } = await renderApp("/acme/support-bot/sql");
  await user.click(screen.getByRole("button", { name: "Run" }));
  await screen.findByRole("grid", { name: "Query results" });
  await user.click(screen.getByRole("tab", { name: "Chart" }));
  expect(await screen.findByText(CLIPPED)).toBeInTheDocument();
});

it("inserts names from the schema browser", async () => {
  storeDraft("");
  const { user } = await renderApp("/acme/support-bot/sql");
  const traces = await screen.findByRole("list", { name: "traces columns" });
  await user.click(within(traces).getByRole("button", { name: "Insert cost_total" }));
  await waitFor(() => {
    expect(editorContent().textContent).toBe("cost_total");
  });
  await user.click(screen.getByRole("button", { name: "spans", expanded: false }));
  await user.click(
    within(screen.getByRole("list", { name: "spans columns" })).getByRole("button", {
      name: "Insert model",
    }),
  );
  await waitFor(() => {
    expect(editorContent().textContent).toBe("cost_total model");
  });
});

it("a schema insert after loading an example lands at the end", async () => {
  storeDraft("SELECT 1");
  const { user } = await renderApp("/acme/support-bot/sql");
  const content = await waitFor(editorContent);
  content.focus();
  await user.click(screen.getByRole("button", { name: "Examples" }));
  await user.click(await screen.findByRole("menuitem", { name: "Users by spend" }));
  await waitFor(() => {
    expect(editorContent().textContent).toContain("ORDER BY cost DESC");
  });
  const traces = screen.getByRole("list", { name: "traces columns" });
  await user.click(within(traces).getByRole("button", { name: "Insert cost_total" }));
  await waitFor(() => {
    expect(editorContent().textContent?.endsWith("cost_total")).toBe(true);
  });
});

it("lists SQL functions on demand", async () => {
  storeDraft("");
  const { user } = await renderApp("/acme/support-bot/sql");
  await screen.findByRole("button", { name: "Insert traces" });
  const functions = screen.getByRole("button", { name: "Functions", expanded: false });
  expect(screen.queryByRole("list", { name: "Functions" })).toBeNull();
  await user.click(functions);
  const list = screen.getByRole("list", { name: "Functions" });
  expect(
    within(list)
      .getAllByRole("button")
      .map((button) => button.getAttribute("aria-label")),
  ).toEqual(schemaFixture.functions.filter((fn) => fn.in_sql).map((fn) => `Insert ${fn.name}(`));
  await user.click(within(list).getByRole("button", { name: "Insert now(" }));
  await waitFor(() => {
    expect(editorContent().textContent).toBe("now(");
  });
});

it("shows Loading… until the schema arrives", async () => {
  let release = () => {};
  const gate = new Promise<void>((resolve) => {
    release = resolve;
  });
  server.use(
    http.get("*/api/v1/projects/:project/schema", async () => {
      await gate;
      return HttpResponse.json(schemaFixture);
    }),
  );
  await renderApp("/acme/support-bot/sql");
  expect(
    within(await screen.findByRole("navigation", { name: "Schema" })).getByText("Loading…"),
  ).toBeInTheDocument();
  release();
  await screen.findByRole("button", { name: "Insert traces" });
});

it("shows Schema unavailable when the schema fails", async () => {
  server.use(
    http.get("*/api/v1/projects/:project/schema", () =>
      HttpResponse.json(
        { type: "about:blank", title: "Service Unavailable", status: 503, code: "busy" },
        { status: 503, headers: PROBLEM_JSON },
      ),
    ),
  );
  await renderApp("/acme/support-bot/sql");
  expect(
    await within(screen.getByRole("navigation", { name: "Schema" })).findByText(
      "Schema unavailable",
    ),
  ).toBeInTheDocument();
});

it("hides the schema browser and remembers it", async () => {
  const { user } = await renderApp("/acme/support-bot/sql");
  await user.click(await screen.findByRole("button", { name: "Schema" }));
  expect(screen.queryByRole("navigation", { name: "Schema" })).toBeNull();
  expect(screen.getByRole("button", { name: "Schema" })).toHaveAttribute("aria-pressed", "false");
  expect(screen.queryByRole("complementary", { name: "SQL sidebar" })).toBeNull();
  expect(localStorage.getItem("cetra.sql.schema")).toBe("closed");
  await user.click(screen.getByRole("button", { name: "Schema" }));
  expect(await screen.findByRole("navigation", { name: "Schema" })).toBeInTheDocument();
  expect(screen.getByRole("button", { name: "Schema" })).toHaveAttribute("aria-pressed", "true");
  expect(localStorage.getItem("cetra.sql.schema")).toBe("open");
});

it("starts closed when it was closed", async () => {
  localStorage.setItem("cetra.sql.schema", "closed");
  await renderApp("/acme/support-bot/sql");
  expect(await screen.findByRole("button", { name: "Schema" })).toHaveAttribute(
    "aria-pressed",
    "false",
  );
  expect(screen.queryByRole("navigation", { name: "Schema" })).toBeNull();
});

/** The separator that resizes the panel which `startPanelId` names. */
function splitSeparator(startPanelId: string): Element | null {
  return document.querySelector(`[role="separator"][aria-controls="${startPanelId}"]`);
}

/**
 * jsdom lays nothing out, so every rect is zero and a press at (0, 0) is over every separator.
 * react-resizable-panels then focuses a separator on any press in the page, which steals the
 * editor's focus and closes open menus. Parking the separators away from the origin keeps the
 * press where it was made.
 */
beforeEach(() => {
  const real = HTMLElement.prototype.getBoundingClientRect;
  vi.spyOn(HTMLElement.prototype, "getBoundingClientRect").mockImplementation(function (
    this: HTMLElement,
  ) {
    return this.getAttribute("role") === "separator"
      ? new DOMRect(10_000, 10_000)
      : real.call(this);
  });
});

it("lists the examples on the empty state and loads one into the editor", async () => {
  const { user } = await renderApp("/acme/support-bot/sql");
  await waitFor(editorContent);
  expect(screen.getByText("Run a query to see results")).toBeInTheDocument();
  expect(screen.getByText("Or start from an example:")).toBeInTheDocument();
  // A card's name is followed by its query preview, so match the name as a prefix.
  await user.click(screen.getByRole("button", { name: /^Users by spend/ }));
  await waitFor(() => {
    expect(editorContent().textContent).toContain("ORDER BY cost DESC");
  });
});

it("renders the editor and results panels with a separator between them", async () => {
  await renderApp("/acme/support-bot/sql");
  await waitFor(editorContent);
  expect(splitSeparator("editor")).not.toBeNull();
  expect(splitSeparator("editor")?.getAttribute("aria-valuenow")).toBe("40");
});

it("restores a stored editor and results split", async () => {
  localStorage.setItem("cetra.sql.split", JSON.stringify({ editor: 70, results: 30 }));
  await renderApp("/acme/support-bot/sql");
  await waitFor(() => {
    expect(splitSeparator("editor")?.getAttribute("aria-valuenow")).toBe("70");
  });
});

it("cancels a running query without showing an error", async () => {
  storeDraft("SELECT 1");
  // Never answers: only the cancel ends the request.
  const sql = captureSql(() => new Promise<Response>(() => {}));
  const { user } = await renderApp("/acme/support-bot/sql");
  await user.click(screen.getByRole("button", { name: "Run" }));
  await user.click(await screen.findByRole("button", { name: "Cancel" }));
  expect(await screen.findByText("Query cancelled.")).toHaveAttribute("role", "status");
  expect(screen.queryByRole("alert")).toBeNull();
  expect(editorContent()).toHaveAttribute("aria-invalid", "false");
  expect(screen.getByRole("button", { name: "Run" })).toBeEnabled();
  expect(single(sql.bodies).query).toBe("SELECT 1");
  // A cancelled run is recorded in history like a failed one.
  expect(storedHistory()).toHaveLength(1);
});

it("a new tab starts empty and becomes the shown one", async () => {
  storeDraft("SELECT 1");
  const { user } = await renderApp("/acme/support-bot/sql");
  await waitFor(() => {
    expect(editorContent().textContent).toBe("SELECT 1");
  });
  await user.click(screen.getByRole("button", { name: "New query tab" }));
  expect(queryTabs()).toHaveLength(2);
  expect(screen.getByRole("tab", { name: "Query 2" })).toHaveAttribute("aria-selected", "true");
  expect(screen.getByRole("tab", { name: "Query 1" })).toHaveAttribute("aria-selected", "false");
  expect(screen.getByRole("button", { name: "Run" })).toBeDisabled();
});

it("switching tabs keeps each tab's editor text and its own result", async () => {
  storeDraft("SELECT 1");
  captureSql();
  const { user } = await renderApp("/acme/support-bot/sql");
  await user.click(screen.getByRole("button", { name: "Run" }));
  await screen.findByRole("grid", { name: "Query results" });
  await user.click(screen.getByRole("button", { name: "New query tab" }));
  expect(screen.queryByRole("grid", { name: "Query results" })).toBeNull();
  const content = await waitFor(editorContent);
  content.focus();
  await user.keyboard("SELECT 2");
  await user.click(screen.getByRole("tab", { name: "Query 1" }));
  await waitFor(() => {
    expect(editorContent().textContent).toBe("SELECT 1");
  });
  expect(screen.getByRole("grid", { name: "Query results" })).toBeInTheDocument();
  await user.click(screen.getByRole("tab", { name: "Query 2" }));
  await waitFor(() => {
    expect(editorContent().textContent).toBe("SELECT 2");
  });
  expect(screen.queryByRole("grid", { name: "Query results" })).toBeNull();
});

it("closing the shown tab shows its right neighbour", async () => {
  storeTabs([{ query: "SELECT 'a'" }, { query: "SELECT 'b'" }, { query: "SELECT 'c'" }], 1);
  const { user } = await renderApp("/acme/support-bot/sql");
  await waitFor(() => {
    expect(editorContent().textContent).toBe("SELECT 'b'");
  });
  await user.click(screen.getByRole("button", { name: "Close Query 2" }));
  await waitFor(() => {
    expect(editorContent().textContent).toBe("SELECT 'c'");
  });
  expect(queryTabs().map((tab) => tab.getAttribute("aria-label"))).toEqual(["Query 1", "Query 3"]);
  expect(screen.getByRole("tab", { name: "Query 3" })).toHaveAttribute("aria-selected", "true");
});

it("keeps the tabs and their text when the page remounts", async () => {
  const first = await renderApp("/acme/support-bot/sql");
  await first.user.click(screen.getByRole("button", { name: "New query tab" }));
  await first.user.type(editorContent(), "SELECT 9");
  await waitFor(() => {
    expect(storedDraft()).toEqual({ query: "SELECT 9", params: {} });
  });
  first.unmount();
  await renderApp("/acme/support-bot/sql");
  await waitFor(() => {
    expect(editorContent().textContent).toBe("SELECT 9");
  });
  expect(queryTabs()).toHaveLength(2);
});

it("opens a saved query from the sidebar in a new tab, and selects that tab when opened again", async () => {
  const { router, user } = await renderApp("/acme/support-bot/sql");
  await showSaved(user);
  await user.click(
    await within(sidebar()).findByRole("button", { name: "Cost per model per day" }),
  );
  await waitFor(() => {
    expect(editorContent().textContent).toBe(SEED_QUERY);
  });
  expect(queryTabs()).toHaveLength(2);
  await user.click(screen.getByRole("tab", { name: "Query 1" }));
  await waitFor(() => {
    expect(router.state.location.search.view).toBeUndefined();
  });
  await user.click(within(sidebar()).getByRole("button", { name: "Cost per model per day" }));
  await waitFor(() => {
    expect(router.state.location.search.view).toBe(COST_VIEW);
    expect(editorContent().textContent).toBe(SEED_QUERY);
  });
  expect(queryTabs()).toHaveLength(2);
  expect(screen.getByRole("tab", { name: "Cost per model per day" })).toHaveAttribute(
    "aria-selected",
    "true",
  );
});

it("a ?view= link opens its query in a new tab beside the others", async () => {
  storeDraft("SELECT 'kept'");
  const { router, user } = await renderApp(`/acme/support-bot/sql?view=${COST_VIEW}`);
  await waitFor(() => {
    expect(editorContent().textContent).toBe(SEED_QUERY);
  });
  expect(queryTabs()).toHaveLength(2);
  await user.click(screen.getByRole("tab", { name: "Query 1" }));
  await waitFor(() => {
    expect(editorContent().textContent).toBe("SELECT 'kept'");
    expect(router.state.location.search.view).toBeUndefined();
  });
});

it("selecting a tab moves ?view= to that tab's saved query", async () => {
  storeTabs([{ query: SEED_QUERY, view: COST_VIEW }, { query: "SELECT 'plain'" }], 1);
  const { router, user } = await renderApp("/acme/support-bot/sql");
  await user.click(await screen.findByRole("tab", { name: "Cost per model per day" }));
  await waitFor(() => {
    expect(router.state.location.search.view).toBe(COST_VIEW);
  });
  expect(editorContent().textContent).toBe(SEED_QUERY);
  await user.click(screen.getByRole("tab", { name: "Query 2" }));
  await waitFor(() => {
    expect(router.state.location.search.view).toBeUndefined();
    expect(editorContent().textContent).toBe("SELECT 'plain'");
  });
});

it("Format rewrites the editor text with the allowed functions", async () => {
  const text = "select name, count(*) from traces";
  const names = schemaFixture.functions.filter((fn) => fn.in_sql).map((fn) => fn.name);
  storeDraft(text);
  const { user } = await renderApp("/acme/support-bot/sql");
  await waitFor(() => {
    expect(editorContent().textContent).toBe(text);
  });
  await user.click(screen.getByRole("button", { name: "Format" }));
  await waitFor(() => {
    expect(editorContent().textContent).toBe(formatSql(text, names).replaceAll("\n", ""));
  });
});

it("Format is disabled for a blank query", async () => {
  storeDraft("   ");
  await renderApp("/acme/support-bot/sql");
  expect(await screen.findByRole("button", { name: "Format" })).toBeDisabled();
});

it("a tab whose query runs shows a spinner on its tab", async () => {
  storeDraft("SELECT 1");
  let release = () => {};
  const gate = new Promise<void>((resolve) => {
    release = resolve;
  });
  captureSql(async () => {
    await gate;
    return HttpResponse.json(resultDocument);
  });
  const { user } = await renderApp("/acme/support-bot/sql");
  await user.click(screen.getByRole("button", { name: "Run" }));
  await screen.findByText("Running…");
  await user.click(screen.getByRole("button", { name: "New query tab" }));
  expect(
    within(screen.getByRole("tab", { name: "Query 1" })).getByRole("img", { name: "Running" }),
  ).toBeInTheDocument();
  expect(
    within(screen.getByRole("tab", { name: "Query 2" })).queryByRole("img", { name: "Running" }),
  ).toBeNull();
  release();
  await user.click(screen.getByRole("tab", { name: "Query 1" }));
  await screen.findByRole("grid", { name: "Query results" });
  expect(screen.queryByRole("img", { name: "Running" })).toBeNull();
});

it("New query tab is disabled at 20 tabs", async () => {
  storeTabs(
    Array.from({ length: 20 }, (_, index) => ({ query: `SELECT ${index}` })),
    0,
  );
  await renderApp("/acme/support-bot/sql");
  expect(await screen.findByRole("button", { name: "New query tab" })).toBeDisabled();
  expect(queryTabs()).toHaveLength(20);
});

it("a saved query opened from the sidebar reuses a blank tab", async () => {
  storeDraft("");
  const { router, user } = await renderApp("/acme/support-bot/sql");
  // The placeholder shows in an empty editor, so only wait for the editor to mount.
  await waitFor(editorContent);
  await showSaved(user);
  await user.click(
    await within(sidebar()).findByRole("button", { name: "Cost per model per day" }),
  );
  await waitFor(() => {
    expect(editorContent().textContent).toBe(SEED_QUERY);
    expect(router.state.location.search.view).toBe(COST_VIEW);
  });
  expect(queryTabs()).toHaveLength(1);
});

it("a ?view= link reuses a blank tab", async () => {
  storeDraft("   ");
  await renderApp(`/acme/support-bot/sql?view=${COST_VIEW}`);
  await waitFor(() => {
    expect(editorContent().textContent).toBe(SEED_QUERY);
  });
  expect(queryTabs()).toHaveLength(1);
});
