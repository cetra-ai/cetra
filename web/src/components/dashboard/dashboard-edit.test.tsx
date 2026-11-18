import { act, fireEvent, screen, waitFor, within } from "@testing-library/react";
import { HttpResponse, http } from "msw";
import type { GridLayoutProps } from "react-grid-layout";
import { expect, it, vi } from "vitest";
import type { Dashboard, Definition } from "@/lib/dashboards/types";
import { dashboardHandlers, seedDashboards } from "@/mocks/dashboards";
import { server } from "@/mocks/node";
import { problemResponse } from "@/test/problem";
import { renderApp } from "@/test/render";
import { recordRequests } from "@/test/requests";
import dashboardDocument from "../../../../fixtures/api/static/dashboard.json";
import overviewDocument from "../../../../fixtures/api/static/dashboard-overview.json";
import staleDocument from "../../../../fixtures/api/static/dashboard-stale.json";
import projectDocument from "../../../../fixtures/api/static/project.json";
import { AUTOSAVE_DELAY_MS } from "./use-layout-autosave";

/** The props the grid last rendered react-grid-layout with; the grid itself is the real one. */
const grid = vi.hoisted(() => ({ last: undefined as GridLayoutProps | undefined }));

vi.mock("react-grid-layout", async (importOriginal) => {
  const actual = await importOriginal<typeof import("react-grid-layout")>();
  function CapturedGridLayout(props: GridLayoutProps) {
    grid.last = props;
    return <actual.GridLayout {...props} />;
  }
  return { ...actual, GridLayout: CapturedGridLayout };
});

const DASHBOARD_ID = dashboardDocument.id;
const CUSTOM = `/acme/support-bot/dashboard/${DASHBOARD_ID}`;
const ONE = "/api/v1/projects/:project/dashboards/:id";
const STORED = dashboardDocument.definition as Definition;
const ERROR_RATE = "Error rate";
const REQUESTS = "Requests by model";
const SLOWEST = "Slowest operations";
const OVERVIEW_URL = "/acme/support-bot/dashboard";
const OVERVIEW_PATH = "/api/v1/projects/:project/dashboards/overview";
/** The Overview the project has stored: the fixture's copy, at version 3. */
const STORED_OVERVIEW = overviewDocument as unknown as Dashboard;

/** The PATCH body the page sent, once it has been sent. */
async function savedBody(requests: ReturnType<typeof recordRequests>) {
  await waitFor(() => {
    expect(requests.calls.find((call) => call.method === "PATCH")?.body).toBeDefined();
  });
  return requests.calls.find((call) => call.method === "PATCH")?.body;
}

/** The PATCH bodies sent so far, once at least `count` have been sent (a save waits 800 ms). */
async function patchBodies(requests: ReturnType<typeof recordRequests>, count: number) {
  await waitFor(
    () => {
      expect(requests.calls.filter((call) => call.method === "PATCH")).toHaveLength(count);
    },
    { timeout: 3000 },
  );
  return requests.calls
    .filter((call) => call.method === "PATCH")
    .map((call) => call.body as { version: number; definition: Definition });
}

/**
 * Reports a finished drag of the widget titled `title` to column `x`, the way react-grid-layout
 * does when the mouse is released. jsdom lays nothing out, so the drop is reported directly.
 */
function dropWidget(title: string, x: number) {
  const id = STORED.widgets.find((widget) => widget.title === title)?.id;
  const next = (grid.last?.layout ?? []).map((item) => (item.i === id ? { ...item, x } : item));
  act(() => {
    grid.last?.onDragStop?.(next, null, null, null, new Event("mouseup"), null);
  });
}

/** Opens the custom dashboard in edit mode. */
async function editing(url = CUSTOM) {
  const rendered = await renderApp(url);
  await rendered.user.click(await screen.findByRole("button", { name: "Edit" }));
  return rendered;
}

it("adds a widget through the editor and saves the definition", async () => {
  const requests = recordRequests();
  try {
    const { user } = await editing();
    await user.click(screen.getByRole("button", { name: "Add widget" }));
    await user.type(screen.getByLabelText("Title"), "Latency p95");
    await user.selectOptions(screen.getByRole("combobox", { name: "Source" }), "spans");
    await user.selectOptions(screen.getByRole("combobox", { name: "Aggregate" }), "p95");
    await user.selectOptions(screen.getByRole("combobox", { name: "Field" }), "duration_ns");
    await user.click(screen.getByRole("checkbox", { name: "Break down by a field" }));
    await user.selectOptions(screen.getByRole("combobox", { name: "Breakdown field" }), "model");
    await user.click(screen.getByRole("button", { name: "Apply" }));
    expect(screen.getByRole("region", { name: "Latency p95" })).toBeInTheDocument();

    await user.click(screen.getByRole("button", { name: "Save" }));
    expect(await savedBody(requests)).toEqual({
      version: 3,
      definition: {
        widgets: [
          ...STORED.widgets,
          {
            id: expect.stringMatching(/^w-/),
            title: "Latency p95",
            kind: "chart",
            chart: { type: "line" },
            query: {
              source: "spans",
              measures: [{ agg: "p95", field: "duration_ns" }],
              breakdown: { field: "model", limit: 5 },
            },
            layout: { x: 0, y: 12, w: 6, h: 7 },
          },
        ],
      },
    });
  } finally {
    requests.stop();
  }
});

it("adds a heading across the full width and saves it", async () => {
  const requests = recordRequests();
  try {
    const { user } = await editing();
    await user.click(screen.getByRole("button", { name: "Add heading" }));
    await user.type(screen.getByLabelText("Heading"), "Latency");
    await user.click(screen.getByRole("button", { name: "Apply" }));
    expect(await screen.findByRole("heading", { level: 2, name: "Latency" })).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Save" }));
    const body = (await savedBody(requests)) as { definition: Definition };
    expect(body.definition.widgets.at(-1)).toEqual({
      id: expect.stringMatching(/^w-/),
      title: "Latency",
      kind: "heading",
      layout: { x: 0, y: 12, w: 12, h: 1 },
    });
  } finally {
    requests.stop();
  }
});

it("removes a widget and saves without it", async () => {
  const requests = recordRequests();
  try {
    const { user } = await editing();
    await user.click(screen.getByRole("button", { name: `Actions for ${ERROR_RATE}` }));
    await user.click(await screen.findByRole("menuitem", { name: "Remove" }));
    expect(screen.queryByRole("region", { name: ERROR_RATE })).not.toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Save" }));
    const body = (await savedBody(requests)) as { version: number; definition: Definition };
    expect(body.version).toBe(3);
    expect(body.definition.widgets.map((widget) => widget.title)).not.toContain(ERROR_RATE);
    expect(body.definition.widgets).toHaveLength(STORED.widgets.length - 1);
  } finally {
    requests.stop();
  }
});

it("drops the edits on Cancel and sends nothing", async () => {
  const requests = recordRequests();
  try {
    const { user } = await editing();
    await user.click(screen.getByRole("button", { name: `Actions for ${ERROR_RATE}` }));
    await user.click(await screen.findByRole("menuitem", { name: "Remove" }));
    await user.click(screen.getByRole("button", { name: "Cancel" }));
    expect(await screen.findByRole("region", { name: ERROR_RATE })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Edit" })).toBeInTheDocument();
    expect(requests.calls.some((call) => call.method === "PATCH")).toBe(false);
  } finally {
    requests.stop();
  }
});

it("blocks a save the rules refuse, with the message on its widget", async () => {
  const bad = structuredClone(dashboardDocument) as unknown as Dashboard;
  const slowest = bad.definition.widgets.find((widget) => widget.title === SLOWEST);
  if (slowest?.query?.measures[0]) {
    slowest.query.measures[0].field = "gone_column";
  }
  server.use(...dashboardHandlers([bad]));
  const requests = recordRequests();
  try {
    const { user } = await editing();
    await user.click(screen.getByRole("button", { name: "Save" }));
    const cell = screen.getByRole("region", { name: SLOWEST }).parentElement as HTMLElement;
    expect(
      await within(cell).findByText("query.measures[0].field: not a numeric field of this source"),
    ).toBeInTheDocument();
    expect(requests.calls.some((call) => call.method === "PATCH")).toBe(false);
  } finally {
    requests.stop();
  }
});

it("shows a server 400 next to the widget its path names", async () => {
  server.use(
    http.patch(ONE, () =>
      problemResponse(400, "bad_request", {
        errors: [
          {
            field: "definition.widgets[1].query.measures[0].field",
            message: "not a numeric field of this source",
          },
        ],
      }),
    ),
  );
  const { user } = await editing();
  await user.click(screen.getByRole("button", { name: "Add heading" }));
  await user.type(screen.getByLabelText("Heading"), "Latency");
  await user.click(screen.getByRole("button", { name: "Apply" }));
  await user.click(screen.getByRole("button", { name: "Save" }));
  const cell = screen.getByRole("region", { name: REQUESTS }).parentElement as HTMLElement;
  expect(
    await within(cell).findByText("query.measures[0].field: not a numeric field of this source"),
  ).toBeInTheDocument();
});

it("asks to reload on a 409, and keeps the edits on Keep editing", async () => {
  server.use(
    http.patch(ONE, () =>
      HttpResponse.json(staleDocument, {
        status: 409,
        headers: { "Content-Type": "application/problem+json" },
      }),
    ),
  );
  const { user } = await editing();
  await user.click(screen.getByRole("button", { name: `Actions for ${ERROR_RATE}` }));
  await user.click(await screen.findByRole("menuitem", { name: "Remove" }));
  await user.click(screen.getByRole("button", { name: "Save" }));
  const dialog = await screen.findByRole("alertdialog");
  expect(within(dialog).getByText("Someone else changed this dashboard.")).toBeInTheDocument();

  await user.click(within(dialog).getByRole("button", { name: "Keep editing" }));
  await waitFor(() => expect(screen.queryByRole("alertdialog")).not.toBeInTheDocument());
  expect(screen.getByRole("button", { name: "Save" })).toBeInTheDocument();
  expect(screen.queryByRole("region", { name: ERROR_RATE })).not.toBeInTheDocument();

  await user.click(screen.getByRole("button", { name: "Save" }));
  await user.click(
    within(await screen.findByRole("alertdialog")).getByRole("button", { name: "Reload" }),
  );
  expect(await screen.findByRole("button", { name: "Edit" })).toBeInTheDocument();
  expect(await screen.findByRole("region", { name: ERROR_RATE })).toBeInTheDocument();
});

it("asks before leaving with unsaved changes", async () => {
  const { router, user } = await editing();
  await user.click(screen.getByRole("button", { name: `Actions for ${ERROR_RATE}` }));
  await user.click(await screen.findByRole("menuitem", { name: "Remove" }));
  await user.click(screen.getByRole("link", { name: "Traces" }));
  const dialog = await screen.findByRole("alertdialog");
  expect(within(dialog).getByText("Discard unsaved changes?")).toBeInTheDocument();
  await user.click(within(dialog).getByRole("button", { name: "Keep editing" }));
  expect(router.state.location.pathname).toBe(CUSTOM);
  expect(screen.queryByRole("region", { name: ERROR_RATE })).not.toBeInTheDocument();

  await user.click(screen.getByRole("link", { name: "Traces" }));
  await user.click(
    within(await screen.findByRole("alertdialog")).getByRole("button", { name: "Discard" }),
  );
  await waitFor(() => expect(router.state.location.pathname).toBe("/acme/support-bot/traces"));
});

it("offers only the fields of the chosen aggregate", async () => {
  const { user } = await editing();
  await user.click(screen.getByRole("button", { name: "Add widget" }));
  // A count takes no field.
  expect(screen.getByRole("combobox", { name: "Aggregate" })).toHaveValue("count");
  expect(screen.queryByRole("combobox", { name: "Field" })).not.toBeInTheDocument();

  await user.selectOptions(screen.getByRole("combobox", { name: "Aggregate" }), "p95");
  const numeric = within(screen.getByRole("combobox", { name: "Field" }))
    .getAllByRole("option")
    .map((option) => option.textContent);
  expect(numeric).toContain("duration_ns");
  expect(numeric).not.toContain("name");

  await user.selectOptions(screen.getByRole("combobox", { name: "Aggregate" }), "count_distinct");
  const distinct = within(screen.getByRole("combobox", { name: "Field" }))
    .getAllByRole("option")
    .map((option) => option.textContent);
  expect(distinct).toContain("trace_id");
  expect(distinct).not.toContain("tags");
  expect(distinct).not.toContain("duration_ns");

  await user.selectOptions(screen.getByRole("combobox", { name: "Aggregate" }), "rate");
  expect(screen.queryByRole("combobox", { name: "Field" })).not.toBeInTheDocument();
});

it("offers the chart options of the chosen type only", async () => {
  const { user } = await editing();
  await user.click(screen.getByRole("button", { name: "Add widget" }));
  // A line splits, and does not stack or fade.
  expect(screen.getByRole("checkbox", { name: /One small chart per group/ })).toBeInTheDocument();
  expect(screen.queryByRole("checkbox", { name: "Stack the series" })).not.toBeInTheDocument();

  await user.selectOptions(screen.getByRole("combobox", { name: "Chart type" }), "bar");
  expect(screen.getByRole("checkbox", { name: "Stack the series" })).toBeInTheDocument();
  expect(screen.getByRole("spinbutton", { name: /Fade buckets/ })).toBeInTheDocument();
  expect(
    screen.queryByRole("checkbox", { name: /One small chart per group/ }),
  ).not.toBeInTheDocument();

  await user.selectOptions(screen.getByRole("combobox", { name: "Chart type" }), "table");
  expect(screen.getByRole("checkbox", { name: /Show each row's share/ })).toBeInTheDocument();
  expect(screen.queryByRole("checkbox", { name: "Stack the series" })).not.toBeInTheDocument();
});

it("saves a distinct count of a metadata key typed by hand", async () => {
  const requests = recordRequests();
  try {
    const { user } = await editing();
    await user.click(screen.getByRole("button", { name: "Add widget" }));
    await user.type(screen.getByLabelText("Title"), "Tiers");
    await user.selectOptions(screen.getByRole("combobox", { name: "Aggregate" }), "count_distinct");
    fireEvent.change(screen.getByLabelText("Map key"), { target: { value: "metadata['tier']" } });
    await user.click(screen.getByRole("button", { name: "Apply" }));
    await user.click(screen.getByRole("button", { name: "Save" }));
    const body = (await savedBody(requests)) as { definition: Definition };
    expect(body.definition.widgets.at(-1)?.query?.measures).toEqual([
      { agg: "count_distinct", field: "metadata['tier']" },
    ]);
  } finally {
    requests.stop();
  }
});

it("previews the widget while it is edited", async () => {
  const requests = recordRequests();
  try {
    const { user } = await editing();
    await user.click(screen.getByRole("button", { name: "Add widget" }));
    await user.type(screen.getByLabelText("Title"), "Preview probe");
    expect(await screen.findByRole("region", { name: "Preview probe" })).toBeInTheDocument();
    await waitFor(() => {
      expect(
        requests.calls.some(
          (call) => call.method === "POST" && call.path.endsWith("/dashboards/query"),
        ),
      ).toBe(true);
    });
  } finally {
    requests.stop();
  }
});

/** A copy of the fixture dashboard with `change` applied to its widgets. */
function dashboardWith(change: (widgets: Definition["widgets"]) => Definition["widgets"]) {
  const copy = structuredClone(dashboardDocument) as unknown as Dashboard;
  copy.definition = { widgets: change(structuredClone(copy.definition.widgets)) };
  return copy;
}

/** Replaces `window.matchMedia` for the mobile query, so a test can narrow the window. */
function narrowable() {
  const original = window.matchMedia;
  const listeners = new Set<() => void>();
  window.matchMedia = ((query: string) => {
    if (!query.includes("767")) {
      return original(query);
    }
    return {
      matches: false,
      media: query,
      onchange: null,
      addEventListener: (_type: string, listener: () => void) => listeners.add(listener),
      removeEventListener: (_type: string, listener: () => void) => listeners.delete(listener),
      addListener: () => undefined,
      removeListener: () => undefined,
      dispatchEvent: () => false,
    };
  }) as unknown as typeof window.matchMedia;
  return {
    narrow() {
      Object.defineProperty(window, "innerWidth", { configurable: true, value: 500 });
      for (const listener of listeners) {
        listener();
      }
    },
    restore() {
      window.matchMedia = original;
      Object.defineProperty(window, "innerWidth", { configurable: true, value: 1024 });
    },
  };
}

it("keeps the draft when only the search changes, and asks when the dashboard changes", async () => {
  const { router, user } = await editing();
  await user.click(screen.getByRole("button", { name: `Actions for ${ERROR_RATE}` }));
  await user.click(await screen.findByRole("menuitem", { name: "Remove" }));

  await user.selectOptions(screen.getByRole("combobox", { name: "Group by" }), "model");
  expect(screen.queryByRole("alertdialog")).not.toBeInTheDocument();
  expect(router.state.location.search).toMatchObject({ group: "model" });
  expect(screen.getByRole("button", { name: "Save" })).toBeInTheDocument();

  await user.click(screen.getByRole("button", { name: "Switch dashboard" }));
  await user.click(await screen.findByRole("menuitem", { name: "Overview" }));
  const dialog = await screen.findByRole("alertdialog");
  expect(within(dialog).getByText("Discard unsaved changes?")).toBeInTheDocument();
  await user.click(within(dialog).getByRole("button", { name: "Keep editing" }));
  expect(router.state.location.pathname).toBe(CUSTOM);
});

it("opens nothing from a chart in edit mode", async () => {
  const { router, user } = await editing();
  const card = await screen.findByRole("region", { name: REQUESTS });
  const chart = await within(card).findByRole("group", { name: /^Requests by model\./ });
  chart.focus();
  fireEvent.keyDown(chart, { key: "ArrowRight" });
  fireEvent.keyDown(chart, { key: "Enter" });
  const bucket = card.querySelector<HTMLElement>('[aria-label^="Open bucket"]');
  if (bucket) {
    await user.click(bucket);
  }
  expect(router.state.location.pathname).toBe(CUSTOM);
  expect(screen.getByRole("button", { name: "Save" })).toBeInTheDocument();
});

it("shows a save the server refuses without details at the top", async () => {
  server.use(
    http.patch(ONE, () =>
      problemResponse(400, "bad_request", { detail: "The dashboard is too large." }),
    ),
  );
  const { user } = await editing();
  await user.click(screen.getByRole("button", { name: "Add heading" }));
  await user.type(screen.getByLabelText("Heading"), "Latency");
  await user.click(screen.getByRole("button", { name: "Apply" }));
  await user.click(screen.getByRole("button", { name: "Save" }));
  const alert = await screen.findByRole("alert");
  expect(within(alert).getByText("The dashboard is too large.")).toBeInTheDocument();
});

it("disables the adds and the copy at the widget limit", async () => {
  const headings = Array.from({ length: 50 }, (_, index) => ({
    id: `heading-${index}`,
    title: `Heading ${index}`,
    kind: "heading" as const,
    layout: { x: 0, y: index, w: 12, h: 1 },
  }));
  server.use(...dashboardHandlers([dashboardWith(() => headings)]));
  const { user } = await editing();
  expect(screen.getByRole("button", { name: "Add widget" })).toBeDisabled();
  expect(screen.getByRole("button", { name: "Add heading" })).toBeDisabled();
  await user.click(screen.getByRole("button", { name: "Actions for Heading 0" }));
  expect(await screen.findByRole("menuitem", { name: "Duplicate" })).toHaveAttribute(
    "aria-disabled",
    "true",
  );
});

it("shows the preview only once the title has settled", async () => {
  const { user } = await editing();
  await user.click(screen.getByRole("button", { name: "Add widget" }));
  await user.type(screen.getByLabelText("Title"), "First");
  expect(await screen.findByRole("region", { name: "First" })).toBeInTheDocument();
  await user.type(screen.getByLabelText("Title"), " more");
  expect(screen.queryByRole("region", { name: "First" })).not.toBeInTheDocument();
  expect(await screen.findByRole("region", { name: "First more" })).toBeInTheDocument();
});

it("does not apply the widget when Enter is pressed in its filter bar", async () => {
  const { user } = await editing();
  await user.click(screen.getByRole("button", { name: "Add widget" }));
  await user.click(screen.getByRole("button", { name: "+ Filter" }));
  await user.type(await screen.findByRole("textbox", { name: "Value" }), "error{Enter}");
  // The chip popover closes; the editor stays open with its controls.
  expect(screen.getByRole("heading", { name: "New widget" })).toBeInTheDocument();
  expect(screen.getByRole("button", { name: "Apply" })).toBeInTheDocument();
});

it("moves a widget from its menu", async () => {
  const requests = recordRequests();
  try {
    const { user } = await editing();
    const card = screen.getByRole("region", { name: REQUESTS });
    // The menu is in the widget's title bar, not over the chart.
    await user.click(within(card).getByRole("button", { name: `Actions for ${REQUESTS}` }));
    await user.click(await screen.findByRole("menuitem", { name: "Move right" }));
    await user.click(screen.getByRole("button", { name: "Save" }));
    const body = (await savedBody(requests)) as { definition: Definition };
    expect(body.definition.widgets.find((widget) => widget.title === REQUESTS)?.layout).toEqual({
      x: 1,
      y: 1,
      w: 8,
      h: 6,
    });
  } finally {
    requests.stop();
  }
});

it("resizes a widget from its menu, and refuses a step past the grid", async () => {
  const requests = recordRequests();
  try {
    const { user } = await editing();
    await user.click(screen.getByRole("button", { name: `Actions for ${ERROR_RATE}` }));
    // Error rate ends at the grid's right edge, so it cannot grow wider.
    expect(await screen.findByRole("menuitem", { name: "Wider" })).toHaveAttribute(
      "aria-disabled",
      "true",
    );
    await user.click(screen.getByRole("menuitem", { name: "Narrower" }));
    await user.click(screen.getByRole("button", { name: `Actions for ${ERROR_RATE}` }));
    await user.click(await screen.findByRole("menuitem", { name: "Taller" }));
    await user.click(screen.getByRole("button", { name: "Save" }));
    const body = (await savedBody(requests)) as { definition: Definition };
    expect(body.definition.widgets.find((widget) => widget.title === ERROR_RATE)?.layout).toEqual({
      x: 8,
      y: 1,
      w: 3,
      h: 4,
    });
  } finally {
    requests.stop();
  }
});

it("names an untitled widget by its place in reading order", async () => {
  const doc = dashboardWith((widgets) =>
    widgets.map((widget) => (widget.title === ERROR_RATE ? { ...widget, title: "" } : widget)),
  );
  server.use(...dashboardHandlers([doc]));
  await editing();
  // Reading order: the heading, Requests by model, the untitled widget (1, 2, 3).
  expect(await screen.findByRole("button", { name: "Actions for widget 3" })).toBeInTheDocument();
});

it("clears a widget's filter and conditions when its source changes", async () => {
  const doc = dashboardWith((widgets) =>
    widgets.map((widget) =>
      widget.title === ERROR_RATE && widget.query
        ? { ...widget, query: { ...widget.query, filter: "status = 'error'" } }
        : widget,
    ),
  );
  server.use(...dashboardHandlers([doc]));
  const { user } = await editing();
  await user.click(screen.getByRole("button", { name: `Actions for ${ERROR_RATE}` }));
  await user.click(await screen.findByRole("menuitem", { name: "Edit" }));
  const dialog = await screen.findByRole("dialog");
  expect(within(dialog).getByText("status = 'error'")).toBeInTheDocument();
  expect(within(dialog).getByLabelText(/Condition/)).toHaveValue("error_count > 0");

  await user.selectOptions(within(dialog).getByRole("combobox", { name: "Source" }), "spans");
  expect(within(dialog).queryByText("status = 'error'")).not.toBeInTheDocument();
  expect(within(dialog).getByLabelText(/Condition/)).toHaveValue("");
  await user.click(within(dialog).getByRole("button", { name: "Cancel" }));

  await user.click(screen.getByRole("button", { name: `Actions for ${REQUESTS}` }));
  await user.click(await screen.findByRole("menuitem", { name: "Edit" }));
  const breakdown = await screen.findByRole("dialog");
  await user.selectOptions(within(breakdown).getByRole("combobox", { name: "Source" }), "spans");
  expect(within(breakdown).getByRole("combobox", { name: "Breakdown field" })).toHaveValue("");
});

it("leaves the draft clean when a widget is applied unchanged", async () => {
  const { router, user } = await editing();
  await user.click(screen.getByRole("button", { name: `Actions for ${REQUESTS}` }));
  await user.click(await screen.findByRole("menuitem", { name: "Edit" }));
  await user.click(await screen.findByRole("button", { name: "Apply" }));
  await user.click(screen.getByRole("link", { name: "Traces" }));
  expect(screen.queryByRole("alertdialog")).not.toBeInTheDocument();
  expect(router.state.location.pathname).toBe("/acme/support-bot/traces");
});

it("keeps a draft guarded while the grid is hidden by a narrow window", async () => {
  const viewport = narrowable();
  try {
    const { router, user } = await editing();
    await user.click(screen.getByRole("button", { name: `Actions for ${ERROR_RATE}` }));
    await user.click(await screen.findByRole("menuitem", { name: "Remove" }));
    act(() => viewport.narrow());
    await waitFor(() =>
      expect(screen.queryByRole("button", { name: "Save" })).not.toBeInTheDocument(),
    );
    await user.click(screen.getByRole("button", { name: "Switch dashboard" }));
    await user.click(await screen.findByRole("menuitem", { name: "Overview" }));
    expect(await screen.findByRole("alertdialog")).toBeInTheDocument();
    expect(router.state.location.pathname).toBe(CUSTOM);
  } finally {
    viewport.restore();
  }
});

it("locks the grid and its controls while a save is pending", async () => {
  let release: () => void = () => undefined;
  const gate = new Promise<void>((resolve) => {
    release = resolve;
  });
  server.use(
    http.patch(ONE, async () => {
      await gate;
      return problemResponse(500, "internal", { detail: "Try again later." });
    }),
  );
  const { user } = await editing();
  await user.click(screen.getByRole("button", { name: `Actions for ${ERROR_RATE}` }));
  await user.click(await screen.findByRole("menuitem", { name: "Remove" }));
  await user.click(screen.getByRole("button", { name: "Save" }));

  await waitFor(() => expect(screen.getByRole("button", { name: "Save" })).toBeDisabled());
  expect(screen.getByRole("button", { name: "Add widget" })).toBeDisabled();
  expect(screen.getByRole("button", { name: "Add heading" })).toBeDisabled();
  expect(screen.getByRole("button", { name: "Cancel" })).toBeDisabled();
  expect(screen.getByRole("button", { name: `Actions for ${REQUESTS}` })).toBeDisabled();

  release();
  expect(await screen.findByText("Try again later.")).toBeInTheDocument();
  expect(screen.getByRole("button", { name: "Save" })).toBeEnabled();
});

it("does not ask to save a dashboard whose stored layout the grid packs upwards", async () => {
  const gapped = dashboardWith((widgets) =>
    widgets.map((widget) =>
      widget.id === "slowest-operations"
        ? { ...widget, layout: { ...widget.layout, y: 9 } }
        : widget,
    ),
  );
  server.use(...dashboardHandlers([gapped]));
  const { router, user } = await editing();
  await user.click(screen.getByRole("link", { name: "Traces" }));
  await waitFor(() => expect(router.state.location.pathname).toBe("/acme/support-bot/traces"));
  expect(screen.queryByRole("alertdialog")).not.toBeInTheDocument();
});

it("a member's grid moves outside edit mode, and a drop saves the layout after the quiet time", async () => {
  const requests = recordRequests();
  try {
    const { router, user } = await renderApp(CUSTOM);
    await screen.findByRole("region", { name: REQUESTS });
    // The grid is movable (react-draggable items) and no resize handle is hidden.
    expect(document.querySelectorAll(".react-draggable").length).toBeGreaterThan(0);
    expect(document.querySelectorAll(".react-resizable-hide")).toHaveLength(0);

    dropWidget(REQUESTS, 1);
    const [saved] = await patchBodies(requests, 1);
    expect(saved?.definition.widgets.find((widget) => widget.title === REQUESTS)?.layout).toEqual({
      x: 1,
      y: 1,
      w: 8,
      h: 6,
    });
    expect(router.state.location.pathname).toBe(CUSTOM);
    expect(await screen.findByText("Layout saved.")).toBeInTheDocument();

    await user.click(screen.getByRole("button", { name: "Undo" }));
    const [, undone] = await patchBodies(requests, 2);
    expect(undone?.definition.widgets.find((widget) => widget.title === REQUESTS)?.layout).toEqual({
      x: 0,
      y: 1,
      w: 8,
      h: 6,
    });
  } finally {
    requests.stop();
  }
});

it("a resize saves the new size", async () => {
  const requests = recordRequests();
  try {
    await renderApp(CUSTOM);
    await screen.findByRole("region", { name: REQUESTS });
    const id = STORED.widgets.find((widget) => widget.title === REQUESTS)?.id;
    const next = (grid.last?.layout ?? []).map((item) =>
      item.i === id ? { ...item, w: 6 } : item,
    );
    act(() => {
      grid.last?.onResizeStop?.(next, null, null, null, new Event("mouseup"), null);
    });
    const [saved] = await patchBodies(requests, 1);
    expect(saved?.definition.widgets.find((widget) => widget.title === REQUESTS)?.layout.w).toBe(6);
  } finally {
    requests.stop();
  }
});

it("viewing a dashboard whose stored layout is gapped saves nothing", async () => {
  const gapped = dashboardWith((widgets) =>
    widgets.map((widget) =>
      widget.id === "slowest-operations"
        ? { ...widget, layout: { ...widget.layout, y: 9 } }
        : widget,
    ),
  );
  server.use(...dashboardHandlers([gapped]));
  const requests = recordRequests();
  try {
    await renderApp(CUSTOM);
    await screen.findByRole("region", { name: SLOWEST });
    await new Promise((resolve) => setTimeout(resolve, AUTOSAVE_DELAY_MS + 300));
    expect(requests.calls.some((call) => call.method === "PATCH")).toBe(false);
  } finally {
    requests.stop();
  }
});

it("a viewer's grid stays static and has no Edit", async () => {
  server.use(
    http.get("/api/v1/projects/:project", () =>
      HttpResponse.json({ ...projectDocument, role: "viewer" }),
    ),
  );
  await renderApp(CUSTOM);
  await screen.findByRole("region", { name: REQUESTS });
  expect(screen.queryByRole("button", { name: "Edit" })).not.toBeInTheDocument();
  expect(document.querySelectorAll(".react-draggable")).toHaveLength(0);
  expect(grid.last?.onDragStop).toBeUndefined();
});

it("a bucket click still opens its list outside edit mode for a member", async () => {
  const { router } = await renderApp(CUSTOM);
  const card = await screen.findByRole("region", { name: REQUESTS });
  const chart = await within(card).findByRole("group", { name: /^Requests by model\./ });
  chart.focus();
  fireEvent.keyDown(chart, { key: "ArrowRight" });
  fireEvent.keyDown(chart, { key: "Enter" });
  await waitFor(() => expect(router.state.location.pathname).not.toBe(CUSTOM));
  expect(router.state.location.pathname).toMatch(/\/(traces|spans)$/);
});

it("Edit then Save hides the Undo bar", async () => {
  const requests = recordRequests();
  try {
    const { user } = await renderApp(CUSTOM);
    await screen.findByRole("region", { name: REQUESTS });
    dropWidget(REQUESTS, 1);
    await patchBodies(requests, 1);
    expect(await screen.findByRole("button", { name: "Undo" })).toBeInTheDocument();

    await user.click(screen.getByRole("button", { name: "Edit" }));
    await user.click(screen.getByRole("button", { name: "Save" }));
    await patchBodies(requests, 2);
    expect(screen.queryByRole("button", { name: "Undo" })).not.toBeInTheDocument();
    expect(screen.queryByText("Layout saved.")).not.toBeInTheDocument();
  } finally {
    requests.stop();
  }
});

it("Edit is disabled while a layout save is pending", async () => {
  let release: () => void = () => undefined;
  const gate = new Promise<void>((resolve) => {
    release = resolve;
  });
  server.use(
    http.patch(ONE, async () => {
      await gate;
      return problemResponse(500, "internal", { detail: "Try again later." });
    }),
  );
  const { user } = await renderApp(CUSTOM);
  await screen.findByRole("region", { name: REQUESTS });
  dropWidget(REQUESTS, 1);

  expect(await screen.findByText("Saving layout…")).toBeInTheDocument();
  expect(screen.getByRole("button", { name: "Edit" })).toBeDisabled();

  release();
  expect(
    await screen.findByText("Try again later. The layout change was not saved."),
  ).toBeInTheDocument();
  expect(screen.getByRole("button", { name: "Edit" })).toBeEnabled();
  await user.click(screen.getByRole("button", { name: "Edit" }));
  expect(screen.getByRole("button", { name: "Save" })).toBeInTheDocument();
});

/** The PUT body the page sent to the Overview, once it has been sent. */
async function overviewSaveBody(requests: ReturnType<typeof recordRequests>) {
  await waitFor(() => {
    expect(requests.calls.find((call) => call.method === "PUT")?.body).toBeDefined();
  });
  return requests.calls.find((call) => call.method === "PUT")?.body as {
    version?: number;
    definition: Definition;
  };
}

it("a member edits the preset Overview, and its first save stores it with no version", async () => {
  const requests = recordRequests();
  try {
    const { user } = await renderApp(OVERVIEW_URL);
    await user.click(await screen.findByRole("button", { name: "Edit" }));
    await user.click(screen.getByRole("button", { name: "Add heading" }));
    await user.type(screen.getByLabelText("Heading"), "Costs");
    await user.click(screen.getByRole("button", { name: "Apply" }));
    await user.click(screen.getByRole("button", { name: "Save" }));

    const body = await overviewSaveBody(requests);
    expect(body).not.toHaveProperty("version");
    expect(body.definition.widgets.at(-1)).toMatchObject({ title: "Costs", kind: "heading" });
    expect(await screen.findByRole("heading", { level: 2, name: "Costs" })).toBeInTheDocument();
  } finally {
    requests.stop();
  }
});

it("a member saves the stored Overview at the version it read", async () => {
  server.use(...dashboardHandlers(seedDashboards(), STORED_OVERVIEW));
  const requests = recordRequests();
  try {
    const { user } = await renderApp(OVERVIEW_URL);
    await user.click(await screen.findByRole("button", { name: "Edit" }));
    await user.click(screen.getByRole("button", { name: "Add heading" }));
    await user.type(screen.getByLabelText("Heading"), "Costs");
    await user.click(screen.getByRole("button", { name: "Apply" }));
    await user.click(screen.getByRole("button", { name: "Save" }));

    const body = await overviewSaveBody(requests);
    expect(body.version).toBe(3);
    expect(body.definition.widgets).toHaveLength(STORED_OVERVIEW.definition.widgets.length + 1);
  } finally {
    requests.stop();
  }
});

it("a 409 on the Overview offers a reload, and Reload fetches the stored Overview again", async () => {
  server.use(
    http.put(OVERVIEW_PATH, () =>
      problemResponse(409, "conflict", { detail: "the dashboard changed; current version is 4" }),
    ),
    ...dashboardHandlers(seedDashboards(), STORED_OVERVIEW),
  );
  const requests = recordRequests();
  try {
    const { user } = await renderApp(OVERVIEW_URL);
    await user.click(await screen.findByRole("button", { name: "Edit" }));
    await user.click(screen.getByRole("button", { name: "Add heading" }));
    await user.type(screen.getByLabelText("Heading"), "Costs");
    await user.click(screen.getByRole("button", { name: "Apply" }));
    await user.click(screen.getByRole("button", { name: "Save" }));

    expect(await screen.findByRole("alertdialog")).toBeInTheDocument();
    const reads = () =>
      requests.calls.filter(
        (call) => call.method === "GET" && call.path.endsWith("/dashboards/overview"),
      ).length;
    const before = reads();
    await user.click(screen.getByRole("button", { name: "Reload" }));
    await waitFor(() => expect(reads()).toBeGreaterThan(before));
    await waitFor(() => expect(screen.queryByRole("alertdialog")).not.toBeInTheDocument());
  } finally {
    requests.stop();
  }
});

it("Reset to default shows only with a stored copy, asks first, and brings the preset back", async () => {
  const bare = await renderApp(OVERVIEW_URL);
  await bare.user.click(await screen.findByRole("button", { name: "Dashboard actions" }));
  expect(screen.queryByRole("menuitem", { name: "Reset to default" })).not.toBeInTheDocument();
  bare.unmount();

  server.use(...dashboardHandlers(seedDashboards(), STORED_OVERVIEW));
  const requests = recordRequests();
  try {
    const { user } = await renderApp(OVERVIEW_URL);
    await user.click(await screen.findByRole("button", { name: "Dashboard actions" }));
    expect(screen.queryByRole("menuitem", { name: "Rename" })).not.toBeInTheDocument();
    expect(screen.queryByRole("menuitem", { name: "Delete" })).not.toBeInTheDocument();
    await user.click(screen.getByRole("menuitem", { name: "Reset to default" }));

    const dialog = await screen.findByRole("dialog");
    expect(within(dialog).getByText("Reset the Overview?")).toBeInTheDocument();
    expect(requests.calls.some((call) => call.method === "DELETE")).toBe(false);
    await user.click(within(dialog).getByRole("button", { name: "Reset" }));

    expect(await screen.findByRole("heading", { level: 2, name: "Volume" })).toBeInTheDocument();
    expect(requests.calls.some((call) => call.method === "DELETE")).toBe(true);
  } finally {
    requests.stop();
  }
});

it("Reset after an autosaved move hides the Undo bar", async () => {
  server.use(...dashboardHandlers(seedDashboards(), STORED_OVERVIEW));
  const requests = recordRequests();
  try {
    const { user } = await renderApp(OVERVIEW_URL);
    await screen.findByRole("region", { name: REQUESTS });
    dropWidget(REQUESTS, 1);
    await overviewSaveBody(requests);
    expect(await screen.findByRole("button", { name: "Undo" })).toBeInTheDocument();

    await user.click(screen.getByRole("button", { name: "Dashboard actions" }));
    await user.click(screen.getByRole("menuitem", { name: "Reset to default" }));
    const dialog = await screen.findByRole("dialog");
    await user.click(within(dialog).getByRole("button", { name: "Reset" }));

    expect(await screen.findByRole("heading", { level: 2, name: "Volume" })).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Undo" })).not.toBeInTheDocument();
  } finally {
    requests.stop();
  }
});

it("a viewer sees the stored Overview with no controls", async () => {
  server.use(
    http.get("/api/v1/projects/:project", () =>
      HttpResponse.json({ ...projectDocument, role: "viewer" }),
    ),
    ...dashboardHandlers(seedDashboards(), STORED_OVERVIEW),
  );
  await renderApp(OVERVIEW_URL);
  expect(await screen.findByRole("heading", { level: 2, name: "Traffic" })).toBeInTheDocument();
  expect(screen.queryByRole("button", { name: "Edit" })).not.toBeInTheDocument();
  expect(screen.queryByRole("button", { name: "Dashboard actions" })).not.toBeInTheDocument();
});

it("viewing the preset Overview saves nothing", async () => {
  const requests = recordRequests();
  try {
    await renderApp(OVERVIEW_URL);
    await screen.findByRole("heading", { level: 2, name: "Volume" });
    await new Promise((resolve) => setTimeout(resolve, AUTOSAVE_DELAY_MS + 300));
    expect(requests.calls.some((call) => call.method === "PUT")).toBe(false);
  } finally {
    requests.stop();
  }
});
