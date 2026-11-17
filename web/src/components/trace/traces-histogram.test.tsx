import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { cleanup, render, screen, waitFor } from "@testing-library/react";
import { HttpResponse, http } from "msw";
import { afterEach, expect, it, vi } from "vitest";
import type { Schema } from "@/api/types";
import {
  histogramRows,
  selectionRange,
  TracesHistogram,
} from "@/components/trace/traces-histogram";
import { server } from "@/mocks/node";

const MINUTE_NS = 60 * 1_000_000_000;
const T0 = "2026-11-17T10:00:00.000Z";
const T1 = "2026-11-17T10:01:00.000Z";
const T2 = "2026-11-17T10:02:00.000Z";
const MS0 = Date.parse(T0);
const MS1 = Date.parse(T1);
const MS2 = Date.parse(T2);

function response(series: Schema<"MetricSeries">[]): Schema<"MetricsResponse"> {
  return {
    bucket_ns: MINUTE_NS,
    from: T0,
    to: "2026-11-17T10:03:00.000Z",
    kpis: [],
    series,
    top_models: [],
  };
}

const TRACES_AND_ERRORS: Schema<"MetricSeries">[] = [
  {
    metric: "traces",
    points: [
      { time: T0, value: 3 },
      { time: T1, value: 2 },
      { time: T2, value: 5 },
    ],
  },
  {
    metric: "errors",
    points: [
      { time: T0, value: 1 },
      { time: T2, value: 7 },
    ],
  },
];

afterEach(() => {
  cleanup();
});

it("computes ok as traces minus errors, never below 0, with missing points as 0", () => {
  expect(histogramRows(response(TRACES_AND_ERRORS))).toEqual([
    { time: MS0, ok: 2, errors: 1 },
    { time: MS1, ok: 2, errors: 0 },
    { time: MS2, ok: 0, errors: 7 },
  ]);
});

it("maps a drag between two bucket starts to the range from the earlier start to the later end", () => {
  expect(selectionRange(MS0, MS2, MINUTE_NS)).toEqual({
    from: T0,
    to: "2026-11-17T10:03:00.000Z",
  });
  expect(selectionRange(MS2, MS0, MINUTE_NS)).toEqual({
    from: T0,
    to: "2026-11-17T10:03:00.000Z",
  });
});

it("selects nothing when the drag stays on one bucket", () => {
  expect(selectionRange(MS1, MS1, MINUTE_NS)).toBeUndefined();
});

function renderChart(onRange = vi.fn()) {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false, gcTime: Infinity, staleTime: Infinity } },
  });
  const view = render(
    <QueryClientProvider client={client}>
      <TracesHistogram
        org="acme"
        project="support-bot"
        from={T0}
        to="2026-11-17T10:03:00.000Z"
        includeEval={false}
        includeExperiments={false}
        onRange={onRange}
      />
    </QueryClientProvider>,
  );
  return { client, onRange, ...view };
}

it("renders the chart from the metrics response", async () => {
  server.use(http.get("*/metrics", () => HttpResponse.json(response(TRACES_AND_ERRORS))));
  renderChart();
  const chart = await screen.findByRole("img", { name: "Traces over time" });
  await waitFor(() => {
    expect(chart.querySelectorAll(".recharts-bar-rectangle").length).toBeGreaterThan(0);
  });
});

it("renders an empty 64px box while loading", () => {
  server.use(
    http.get("*/metrics", async () => {
      await new Promise(() => undefined);
      return HttpResponse.json(response([]));
    }),
  );
  renderChart();
  const chart = screen.getByRole("img", { name: "Traces over time" });
  expect(chart).toHaveClass("h-16");
  expect(chart).toBeEmptyDOMElement();
});

it("renders nothing when the metrics request fails", async () => {
  server.use(
    http.get("*/metrics", () =>
      HttpResponse.json(
        { type: "about:blank", title: "Bad Request", status: 400, code: "invalid_filter" },
        { status: 400, headers: { "Content-Type": "application/problem+json" } },
      ),
    ),
  );
  const { client } = renderChart();
  await waitFor(() => {
    const state = client.getQueryCache().getAll()[0]?.state.status;
    expect(state).toBe("error");
  });
  expect(screen.queryByRole("img", { name: "Traces over time" })).not.toBeInTheDocument();
});
