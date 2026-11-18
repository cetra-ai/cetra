import { fireEvent, screen, waitFor, within } from "@testing-library/react";
import { HttpResponse, http } from "msw";
import { expect, it } from "vitest";
import type { Dashboard, Definition } from "@/lib/dashboards/types";
import { dashboardHandlers } from "@/mocks/dashboards";
import { server } from "@/mocks/node";
import { problemResponse } from "@/test/problem";
import { renderApp } from "@/test/render";
import { recordRequests } from "@/test/requests";
import dashboardDocument from "../../../../fixtures/api/static/dashboard.json";
import staleDocument from "../../../../fixtures/api/static/dashboard-stale.json";

const DASHBOARD_ID = dashboardDocument.id;
const CUSTOM = `/acme/support-bot/dashboard/${DASHBOARD_ID}`;
const ONE = "/api/v1/projects/:project/dashboards/:id";
const STORED = dashboardDocument.definition as Definition;
const ERROR_RATE = "Error rate";
const REQUESTS = "Requests by model";
const SLOWEST = "Slowest operations";

/** The PATCH body the page sent, once it has been sent. */
async function savedBody(requests: ReturnType<typeof recordRequests>) {
  await waitFor(() => {
    expect(requests.calls.find((call) => call.method === "PATCH")?.body).toBeDefined();
  });
  return requests.calls.find((call) => call.method === "PATCH")?.body;
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
