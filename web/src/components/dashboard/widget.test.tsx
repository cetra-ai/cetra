import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { HttpResponse, http } from "msw";
import { describe, expect, it, vi } from "vitest";
import type { Schema } from "@/api/types";
import { TooltipProvider } from "@/components/ui/tooltip";
import type { Widget as WidgetModel, WidgetQuery } from "@/lib/dashboards/types";
import { server } from "@/mocks/node";
import { Widget, type WidgetContext } from "./widget";

type Body = Schema<"WidgetQueryRequest">;
type Answer = Schema<"WidgetQueryResponse">;

const QUERY_PATH = "/api/v1/projects/:project/dashboards/query";
const H1 = "2026-01-01T10:00:00.000000000Z";
const H2 = "2026-01-01T10:10:00.000000000Z";

const CONTEXT: WidgetContext = {
  org: "acme",
  project: "support-bot",
  from: "2026-01-01T10:00:00.000Z",
  to: "2026-01-01T11:00:00.000Z",
  includeEval: false,
  includeExperiments: false,
};

/** Answers every widget query with `respond`, and records the request bodies. */
function answer(respond: (body: Body) => Answer | Record<string, unknown>, status = 200) {
  const bodies: Body[] = [];
  server.use(
    http.post(QUERY_PATH, async ({ request }) => {
      const body = (await request.json()) as Body;
      bodies.push(body);
      return HttpResponse.json(respond(body), { status });
    }),
  );
  return bodies;
}

function seriesAnswer(series: Extract<Answer, { shape: "series" }>["series"]): Answer {
  return { shape: "series", bucket_ns: 600_000_000_000, series };
}

function totalAnswer(rows: Extract<Answer, { shape: "total" }>["rows"]): Answer {
  return { shape: "total", rows };
}

function widget(
  chart: NonNullable<WidgetModel["chart"]>,
  query: WidgetQuery,
  title = "Test",
): WidgetModel {
  return { id: "w1", title, kind: "chart", chart, query, layout: { x: 0, y: 0, w: 6, h: 7 } };
}

function renderWidget(
  model: WidgetModel,
  props: Partial<Parameters<typeof Widget>[0]> = {},
  context: WidgetContext = CONTEXT,
) {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={client}>
      <TooltipProvider>
        <Widget widget={model} context={context} {...props} />
      </TooltipProvider>
    </QueryClientProvider>,
  );
}

describe("time charts", () => {
  it("draws a line chart of the series answer, asking for the bucket of the range", async () => {
    const bodies = answer(() =>
      seriesAnswer([
        { measure: 0, other: false, points: [{ time: H1, value: 2, rows: 2 }] },
        { measure: 1, other: false, points: [{ time: H2, value: 3, rows: 3 }] },
      ]),
    );
    renderWidget(
      widget(
        { type: "line" },
        {
          source: "traces",
          measures: [
            { agg: "count", label: "Traces" },
            { agg: "p95", field: "duration_ns" },
          ],
        },
      ),
    );
    expect(
      await screen.findByRole("group", { name: /^Test\. Arrow keys move between time buckets/ }),
    ).toBeInTheDocument();
    expect(bodies[0]).toMatchObject({ shape: "series", bucket: "1m" });
    expect(bodies[0]?.query.source).toBe("traces");
  });

  it("fades a bar whose bucket has fewer rows than fade_below", async () => {
    answer(() =>
      seriesAnswer([
        {
          measure: 0,
          other: false,
          points: [
            { time: H1, value: 0.5, rows: 2 },
            { time: H2, value: 0.1, rows: 9 },
          ],
        },
      ]),
    );
    const { container } = renderWidget(
      widget(
        { type: "bar", fade_below: 5 },
        {
          source: "traces",
          measures: [{ agg: "rate", where: "status = 'error'" }],
        },
      ),
    );
    await screen.findByRole("group", { name: /^Test\./ });
    const opacities = [...container.querySelectorAll(".recharts-bar-rectangle rect")].map((rect) =>
      rect.getAttribute("fill-opacity"),
    );
    expect(opacities).toContain("0.35");
    expect(opacities).toContain(null);
  });

  it("splits a line into one small chart per group, sorted by name, at most six, then says how many more", async () => {
    // Seven groups in no particular order: the first six by name are drawn, the seventh is the "+1 more".
    answer(() =>
      seriesAnswer(
        ["g7", "g3", "g1", "g6", "g2", "g5", "g4"].map((group) => ({
          measure: 0,
          group,
          other: false,
          points: [{ time: H1, value: 1, rows: 1 }],
        })),
      ),
    );
    renderWidget(
      widget(
        { type: "line", split: true },
        {
          source: "scores",
          measures: [{ agg: "avg", field: "value_num" }],
          breakdown: { field: "name", limit: 6, other: false },
        },
      ),
    );
    await waitFor(() => {
      expect(screen.getAllByRole("group", { name: /^Test · g\d\. / })).toHaveLength(6);
    });
    const names = screen
      .getAllByRole("group", { name: /^Test · g\d\. / })
      .map((chart) => chart.getAttribute("aria-label")?.split(".")[0]);
    expect(names).toEqual([
      "Test · g1",
      "Test · g2",
      "Test · g3",
      "Test · g4",
      "Test · g5",
      "Test · g6",
    ]);
    expect(screen.getByText("+1 more")).toBeInTheDocument();
  });

  it("stacks the bars of every series of a stacked chart", async () => {
    answer(() =>
      seriesAnswer([
        { measure: 0, other: false, points: [{ time: H1, value: 2, rows: 2 }] },
        { measure: 1, other: false, points: [{ time: H1, value: 3, rows: 3 }] },
      ]),
    );
    const { container } = renderWidget(
      widget(
        { type: "bar", stacked: true },
        {
          source: "traces",
          measures: [
            { agg: "count", where: "status <> 'error'", label: "OK" },
            { agg: "count", where: "status = 'error'", label: "Errors" },
          ],
        },
      ),
    );
    await screen.findByRole("group", { name: /^Test\./ });
    const stacks = container.querySelectorAll(".recharts-bar");
    expect(stacks.length).toBe(2);
  });

  it("opens the bucket of a bar with its group's clause", async () => {
    answer(() =>
      seriesAnswer([
        { measure: 0, group: "alpha", other: false, points: [{ time: H1, value: 2, rows: 2 }] },
      ]),
    );
    const onBucket = vi.fn();
    renderWidget(
      widget(
        { type: "bar" },
        {
          source: "traces",
          measures: [{ agg: "count" }],
          breakdown: { field: "name" },
        },
      ),
      { onBucket },
    );
    const bucket = await waitFor(() => {
      const found = document.querySelector('[aria-label="Open bucket 2026-01-01T10:00:00.000Z"]');
      expect(found).toBeInstanceOf(Element);
      return found as HTMLElement;
    });
    fireEvent.click(bucket);
    expect(onBucket).toHaveBeenCalledWith({
      from: "2026-01-01T10:00:00.000Z",
      to: "2026-01-01T10:10:00.000Z",
      clause: "name = 'alpha'",
    });
  });

  it("opens a bucket of a two-group chart without a group, by click and by Enter", async () => {
    answer(() =>
      seriesAnswer([
        { measure: 0, group: "alpha", other: false, points: [{ time: H1, value: 2, rows: 2 }] },
        { measure: 0, group: "beta", other: false, points: [{ time: H1, value: 1, rows: 1 }] },
      ]),
    );
    const onBucket = vi.fn();
    renderWidget(
      widget(
        { type: "bar" },
        { source: "traces", measures: [{ agg: "count" }], breakdown: { field: "name" } },
      ),
      { onBucket },
    );
    const buckets = await waitFor(() => {
      const found = document.querySelectorAll(
        '[aria-label="Open bucket 2026-01-01T10:00:00.000Z"]',
      );
      expect(found.length).toBe(2);
      return [...found] as HTMLElement[];
    });
    fireEvent.click(buckets[0] as HTMLElement);
    expect(onBucket).toHaveBeenLastCalledWith({
      from: "2026-01-01T10:00:00.000Z",
      to: "2026-01-01T10:10:00.000Z",
    });
    const chart = screen.getByRole("group", { name: /^Test\. Arrow keys/ });
    chart.focus();
    fireEvent.keyDown(chart, { key: "ArrowRight" });
    fireEvent.keyDown(chart, { key: "Enter" });
    expect(onBucket).toHaveBeenLastCalledWith({
      from: "2026-01-01T10:00:00.000Z",
      to: "2026-01-01T10:10:00.000Z",
    });
  });

  it("opens an area's bucket without a group, even with one group", async () => {
    answer(() =>
      seriesAnswer([
        { measure: 0, group: "alpha", other: false, points: [{ time: H1, value: 2, rows: 2 }] },
      ]),
    );
    const onBucket = vi.fn();
    renderWidget(
      widget(
        { type: "area" },
        { source: "traces", measures: [{ agg: "count" }], breakdown: { field: "name" } },
      ),
      { onBucket },
    );
    const chart = await screen.findByRole("group", { name: /^Test\. Arrow keys/ });
    chart.focus();
    fireEvent.keyDown(chart, { key: "ArrowRight" });
    fireEvent.keyDown(chart, { key: "Enter" });
    expect(onBucket).toHaveBeenCalledWith({
      from: "2026-01-01T10:00:00.000Z",
      to: "2026-01-01T10:10:00.000Z",
    });
  });

  it("opens the bucket from the keyboard with Enter", async () => {
    answer(() =>
      seriesAnswer([{ measure: 0, other: false, points: [{ time: H1, value: 2, rows: 2 }] }]),
    );
    const onBucket = vi.fn();
    renderWidget(widget({ type: "line" }, { source: "traces", measures: [{ agg: "count" }] }), {
      onBucket,
    });
    const chart = await screen.findByRole("group", { name: /^Test\. Arrow keys/ });
    chart.focus();
    fireEvent.keyDown(chart, { key: "ArrowRight" });
    fireEvent.keyDown(chart, { key: "Enter" });
    expect(onBucket).toHaveBeenCalledWith(
      expect.objectContaining({ from: "2026-01-01T10:00:00.000Z" }),
    );
  });

  it("gives scores no bucket buttons, since they open no list", async () => {
    answer(() =>
      seriesAnswer([{ measure: 0, other: false, points: [{ time: H1, value: 0.5, rows: 1 }] }]),
    );
    const { container } = renderWidget(
      widget(
        { type: "line" },
        { source: "scores", measures: [{ agg: "avg", field: "value_num" }] },
      ),
    );
    await screen.findByRole("group", { name: /^Test\./ });
    expect(container.querySelectorAll('[aria-label^="Open bucket"]')).toHaveLength(0);
  });
});

describe("other widgets", () => {
  it("shows a number, its change against the previous period, and a sparkline", async () => {
    answer((body) =>
      body.shape === "total"
        ? totalAnswer([{ other: false, values: [10], previous: [5] }])
        : seriesAnswer([
            {
              measure: 0,
              other: false,
              points: [
                { time: H1, value: 1, rows: 1 },
                { time: H2, value: 3, rows: 1 },
              ],
            },
          ]),
    );
    const user = userEvent.setup();
    const { container } = renderWidget(
      widget(
        { type: "number" },
        { source: "traces", measures: [{ agg: "count", label: "Traces" }] },
      ),
    );
    expect(await screen.findByText("100%")).toBeInTheDocument();
    expect(screen.getByRole("figure", { name: "Test: 10" })).toBeInTheDocument();
    expect(container.querySelector("polyline")).not.toBeNull();
    await user.hover(screen.getByRole("button", { name: /100%/ }));
    expect(await screen.findByText("Previous period: 5")).toBeInTheDocument();
  });

  it("sends a number without the dashboard's split, and a chart with it", async () => {
    const bodies = answer((body) =>
      body.shape === "total"
        ? totalAnswer([{ other: false, values: [10], previous: [5] }])
        : seriesAnswer([{ measure: 0, other: false, points: [{ time: H1, value: 1, rows: 1 }] }]),
    );
    const context = { ...CONTEXT, groupBy: "model" as const };
    const { unmount } = renderWidget(
      widget({ type: "number" }, { source: "traces", measures: [{ agg: "count" }] }),
      {},
      context,
    );
    expect(await screen.findByText("100%")).toBeInTheDocument();
    await waitFor(() => expect(bodies.some((body) => body.shape === "series")).toBe(true));
    expect(bodies.every((body) => body.group_by === undefined)).toBe(true);
    expect(bodies.every((body) => body.compare === undefined || body.compare === true)).toBe(true);
    unmount();
    bodies.length = 0;
    renderWidget(
      widget({ type: "line" }, { source: "traces", measures: [{ agg: "count" }] }),
      {},
      context,
    );
    await waitFor(() => expect(bodies.length).toBeGreaterThan(0));
    expect(bodies[0]).toMatchObject({ group_by: "model" });
  });

  it("badges a rise from zero as new, and a large rise as a ratio", async () => {
    answer(() => totalAnswer([{ other: false, values: [4], previous: [0] }]));
    const { unmount } = renderWidget(
      widget({ type: "number" }, { source: "traces", measures: [{ agg: "count" }] }),
    );
    expect(await screen.findByText("new")).toBeInTheDocument();
    unmount();
    answer(() => totalAnswer([{ other: false, values: [1], previous: [0.1] }]));
    renderWidget(
      widget(
        { type: "number" },
        { source: "traces", measures: [{ agg: "sum", field: "cost_total" }] },
      ),
    );
    expect(await screen.findByText("×10")).toBeInTheDocument();
  });

  it("draws one horizontal bar per group", async () => {
    answer(() =>
      totalAnswer([
        { group: "alpha", other: false, values: [3] },
        { group: "beta", other: false, values: [1] },
      ]),
    );
    renderWidget(
      widget(
        { type: "bar_h" },
        { source: "traces", measures: [{ agg: "count" }], breakdown: { field: "name" } },
      ),
    );
    expect((await screen.findAllByText("alpha")).length).toBeGreaterThan(0);
    expect(screen.getAllByText("beta").length).toBeGreaterThan(0);
  });

  it("colours the other group's bar muted, and tolerates two measures with one label", async () => {
    answer(() =>
      totalAnswer([
        { group: "model-a", other: false, values: [3, 1] },
        { other: true, values: [4, 2] },
      ]),
    );
    const errors = vi.spyOn(console, "error").mockImplementation(() => undefined);
    try {
      const { container } = renderWidget(
        widget(
          { type: "bar_h" },
          {
            source: "spans",
            measures: [
              { agg: "count", label: "Calls" },
              { agg: "count", label: "Calls" },
            ],
            breakdown: { field: "model", other: true },
          },
        ),
      );
      await screen.findAllByText("Other");
      const fills = [...container.querySelectorAll(".recharts-bar-rectangle path")].map((bar) =>
        bar.getAttribute("fill"),
      );
      expect(fills).toContain("var(--muted-foreground)");
      expect(fills.filter((fill) => fill === "var(--muted-foreground)")).toHaveLength(1);
      expect(errors.mock.calls.some((call) => String(call[0]).includes("same key"))).toBe(false);
    } finally {
      errors.mockRestore();
    }
  });

  it("shares a table row over every row of the answer, the other group included", async () => {
    answer(() =>
      totalAnswer([
        { group: "model-a", other: false, values: [3] },
        { group: "model-b", other: false, values: [1] },
        { other: true, values: [4] },
      ]),
    );
    renderWidget(
      widget(
        { type: "table", share: true },
        {
          source: "spans",
          measures: [{ agg: "sum", field: "cost_total", label: "Cost" }],
          breakdown: { field: "model" },
        },
      ),
    );
    const row = await screen.findByRole("row", { name: /model-a/ });
    expect(row.querySelector("div[aria-hidden]")).toHaveStyle({ width: "37.5%" });
  });

  it("lists the groups of a table with a share bar, and opens a model's traces", async () => {
    answer(() =>
      totalAnswer([
        { group: "model-a", other: false, values: [3] },
        { group: "model-b", other: false, values: [1] },
      ]),
    );
    const onModel = vi.fn();
    renderWidget(
      widget(
        { type: "table", share: true },
        {
          source: "spans",
          measures: [{ agg: "sum", field: "cost_total", label: "Cost" }],
          breakdown: { field: "model" },
        },
      ),
      { onModel },
    );
    const row = await screen.findByRole("row", { name: /model-a/ });
    expect(row.querySelector("div[aria-hidden]")).toHaveStyle({ width: "75%" });
    fireEvent.click(screen.getByRole("button", { name: "Open traces of model-a" }));
    expect(onModel).toHaveBeenCalledWith("model-a");
  });

  it("shows a heading as a second-level title, with no card", () => {
    renderWidget({
      id: "h",
      title: "Volume",
      kind: "heading",
      layout: { x: 0, y: 0, w: 12, h: 1 },
    });
    expect(screen.getByRole("heading", { level: 2, name: "Volume" })).toBeInTheDocument();
  });

  it("says why a widget has no data, and names its filter", async () => {
    answer(() => seriesAnswer([]));
    const { unmount } = renderWidget(
      widget({ type: "line" }, { source: "traces", measures: [{ agg: "count" }] }),
    );
    expect(await screen.findByText("No traces in range.")).toBeInTheDocument();
    unmount();
    renderWidget(
      widget(
        { type: "line" },
        {
          source: "traces",
          filter: "status = 'error'",
          measures: [{ agg: "count" }],
        },
      ),
    );
    expect(
      await screen.findByText("No traces in range match this widget's filter."),
    ).toBeInTheDocument();
  });

  it("shows the error and retries the query", async () => {
    let calls = 0;
    server.use(
      http.post(QUERY_PATH, () => {
        calls += 1;
        if (calls === 1) {
          return HttpResponse.json(
            {
              type: "about:blank",
              title: "Query failed",
              status: 500,
              code: "query_failed",
              detail: "The query failed.",
            },
            { status: 500 },
          );
        }
        return HttpResponse.json(
          seriesAnswer([{ measure: 0, other: false, points: [{ time: H1, value: 2, rows: 2 }] }]),
        );
      }),
    );
    const user = userEvent.setup();
    renderWidget(widget({ type: "line" }, { source: "traces", measures: [{ agg: "count" }] }));
    expect(await screen.findByRole("alert")).toHaveTextContent("The query failed.");
    await user.click(screen.getByRole("button", { name: "Retry" }));
    expect(await screen.findByRole("group", { name: /^Test\./ })).toBeInTheDocument();
  });
});
