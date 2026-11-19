import { fireEvent, screen, waitFor, within } from "@testing-library/react";
import { HttpResponse, http } from "msw";
import { expect, it } from "vitest";
import type { Schema } from "@/api/types";
import type { Dashboard } from "@/lib/dashboards/types";
import { dashboardHandlers, seedDashboards } from "@/mocks/dashboards";
import { server } from "@/mocks/node";
import { renderApp } from "@/test/render";
import { recordRequests } from "@/test/requests";
import overviewDocument from "../../../../../../fixtures/api/static/dashboard-overview.json";

const DASHBOARD = "/acme/support-bot/dashboard";

const NUMBERS = ["Traces", "Error rate", "p95 latency", "Cost", "Tokens"];
const CHARTS = [
  "Traces & errors",
  "Tokens by type",
  "Error rate",
  "Scores",
  "Latency",
  "LLM latency by model",
  "TTFT",
  "Cost by model",
  "Evaluation cost",
  "Top models by cost",
];

/** Records the bodies of the widget queries the page sends. */
function listenQueries() {
  const bodies: Schema<"WidgetQueryRequest">[] = [];
  const onStart = ({ request }: { request: Request }) => {
    if (new URL(request.url).pathname.endsWith("/dashboards/query")) {
      void request
        .clone()
        .json()
        .then((body) => bodies.push(body as Schema<"WidgetQueryRequest">));
    }
  };
  server.events.on("request:start", onStart);
  return {
    bodies,
    stop() {
      server.events.removeListener("request:start", onStart);
    },
  };
}

it("renders the five numbers and every chart, with no section headings", async () => {
  const queries = listenQueries();
  try {
    await renderApp(DASHBOARD);
    expect(
      screen.getAllByRole("heading", { level: 1 }).map((heading) => heading.textContent),
    ).toEqual(["Dashboard"]);
    await screen.findByRole("region", { name: "Traces & errors" });
    expect(screen.queryAllByRole("heading", { level: 2 })).toEqual([]);
    for (const title of NUMBERS) {
      expect(screen.getAllByRole("region", { name: title }).length).toBeGreaterThan(0);
    }
    for (const title of CHARTS) {
      expect(screen.getAllByRole("region", { name: title }).length).toBeGreaterThan(0);
    }
    expect(await screen.findAllByText("5")).not.toHaveLength(0);
    await waitFor(() => {
      expect(queries.bodies.some((body) => body.bucket === "30m")).toBe(true);
    });
    expect(queries.bodies.every((body) => body.group_by === undefined)).toBe(true);
  } finally {
    queries.stop();
  }
});

it("has no sections to collapse", async () => {
  await renderApp(DASHBOARD);
  await screen.findByRole("region", { name: "Traces & errors" });
  expect(localStorage.getItem("cetra.dashboard.collapsed.acme/support-bot")).toBeNull();
});

it("refetches when the range changes", async () => {
  const queries = listenQueries();
  try {
    const { router } = await renderApp(DASHBOARD);
    await screen.findByRole("region", { name: "Traces & errors" });
    const before = queries.bodies.at(-1)?.from;
    fireEvent.click(screen.getByRole("button", { name: "Last 24 hours" }));
    fireEvent.click(await screen.findByRole("button", { name: "Last 1 hour" }));
    await waitFor(() => {
      const latest = queries.bodies.at(-1);
      expect(latest?.from).toBeTruthy();
      expect(latest?.from).not.toBe(before);
      const span = Date.parse(latest?.to ?? "") - Date.parse(latest?.from ?? "");
      expect(Math.abs(span - 3_600_000)).toBeLessThan(5_000);
    });
    expect(router.state.location.search).toMatchObject({ range: "1h" });
  } finally {
    queries.stop();
  }
});

it("sends group_by with the charts when grouping by model, but not with the numbers", async () => {
  const queries = listenQueries();
  try {
    const { router } = await renderApp(DASHBOARD);
    await screen.findByRole("region", { name: "Traces & errors" });
    fireEvent.change(screen.getByRole("combobox", { name: "Group by" }), {
      target: { value: "model" },
    });
    await waitFor(() => {
      expect(queries.bodies.some((body) => body.group_by === "model")).toBe(true);
    });
    // A number is the only widget that asks for the previous period (`compare`): it ignores the split.
    const numbers = queries.bodies.filter((body) => body.compare === true);
    expect(numbers.length).toBeGreaterThan(0);
    expect(numbers.every((body) => body.group_by === undefined)).toBe(true);
    expect(router.state.location.search).toMatchObject({ group: "model" });
  } finally {
    queries.stop();
  }
});

it("opens the spans of a bucket with the widget's filter and group, not the dashboard's filter", async () => {
  // One model in the answer, so the bucket's group is that model.
  server.use(
    http.post("/api/v1/projects/:project/dashboards/query", async ({ request }) => {
      const body = (await request.json()) as Schema<"WidgetQueryRequest">;
      if (body.shape !== "series" || body.query.source !== "spans") {
        return undefined;
      }
      return HttpResponse.json({
        shape: "series",
        bucket_ns: 600_000_000_000,
        series: [
          {
            measure: 0,
            group: "gpt-4.1",
            other: false,
            points: [{ time: "2026-01-01T10:00:00.000000000Z", value: 1, rows: 1 }],
          },
        ],
      });
    }),
  );
  const { router } = await renderApp(
    "/acme/support-bot/dashboard?filter=status%20%3D%20'error'&group=model&eval=1",
  );
  const card = await screen.findByRole("region", { name: "TTFT" });
  const chart = await within(card).findByRole("group", { name: /^TTFT\./ });
  chart.focus();
  fireEvent.keyDown(chart, { key: "ArrowRight" });
  fireEvent.keyDown(chart, { key: "Enter" });
  await waitFor(() => {
    expect(router.state.location.pathname).toContain("/spans");
  });
  expect(router.state.location.search).toEqual({
    from: "2026-01-01T10:00:00.000Z",
    to: "2026-01-01T10:10:00.000Z",
    filter: "span_kind = 'llm' AND ttft_ns IS NOT NULL AND model = 'gpt-4.1'",
  });
});

it("shows the message of an invalid dashboard filter in place of the widgets", async () => {
  // The server refuses the dashboard's filter; the ranking query carries it, so the page shows why.
  server.use(
    http.post("/api/v1/projects/:project/dashboards/query", async ({ request }) => {
      const body = (await request.json()) as Schema<"WidgetQueryRequest">;
      if (body.filter === undefined) {
        return undefined;
      }
      return HttpResponse.json(
        {
          type: "about:blank",
          title: "Bad Request",
          status: 400,
          code: "invalid_filter",
          detail: "The filter is not valid.",
          errors: [
            {
              field: "filter",
              message: "Unknown field in the dashboard filter.",
              position: { line: 1, column: 1 },
            },
          ],
        },
        { status: 400, headers: { "Content-Type": "application/problem+json" } },
      );
    }),
  );
  await renderApp("/acme/support-bot/dashboard?filter=status%20%3D%20'error'");
  expect(await screen.findByRole("heading", { level: 2, name: "Bad Request" })).toBeInTheDocument();
  expect(screen.getAllByText("Unknown field in the dashboard filter.").length).toBeGreaterThan(0);
  expect(screen.queryByRole("region", { name: "Traces & errors" })).not.toBeInTheDocument();
});

it("shows the stored Overview, read from /dashboards/overview, when the project has one", async () => {
  server.use(...dashboardHandlers(seedDashboards(), overviewDocument as unknown as Dashboard));
  const requests = recordRequests();
  try {
    await renderApp(DASHBOARD);
    expect(await screen.findByRole("heading", { level: 2, name: "Traffic" })).toBeInTheDocument();
    expect(screen.queryByRole("region", { name: "Traces & errors" })).not.toBeInTheDocument();
    expect(
      requests.calls.some(
        (call) => call.method === "GET" && call.path.endsWith("/dashboards/overview"),
      ),
    ).toBe(true);
  } finally {
    requests.stop();
  }
});
