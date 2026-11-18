import { render as renderIn, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import type { ReactElement } from "react";
import { expect, it, vi } from "vitest";
import type { Schema } from "@/api/types";
import { KPI_TONE, KpiRow, KpiRowSkeleton, kpiTrend } from "@/components/charts/kpi-tiles";
import { TooltipProvider } from "@/components/ui/tooltip";

/** The tiles' tooltips need the provider the app mounts at its root. */
function render(ui: ReactElement) {
  return renderIn(<TooltipProvider>{ui}</TooltipProvider>);
}

const kpis: Schema<"Kpi">[] = [
  { metric: "traces", value: 120, previous: 100 },
  { metric: "error_rate", value: 0.2, previous: 0.1 },
  { metric: "latency_p95", value: 80, previous: 100 },
  { metric: "cost_total", value: 2, previous: 1 },
  { metric: "total_tokens", value: 500, previous: 1000 },
];

const tile = (name: RegExp) => screen.getByRole("button", { name });

it("shows each KPI tile's label with a bar for its value", () => {
  const { container } = render(<KpiRowSkeleton />);
  for (const label of ["Traces", "Error rate", "p95 latency", "Cost", "Tokens"]) {
    expect(screen.getByText(label)).toBeInTheDocument();
  }
  expect(container.querySelectorAll('[data-slot="skeleton"]')).toHaveLength(5);
  expect(screen.queryByRole("button")).not.toBeInTheDocument();
});

it("sets the tone per KPI: traces and tokens are neutral, error rate, latency and cost are up-bad", () => {
  expect(KPI_TONE).toEqual({
    traces: "neutral",
    error_rate: "up-bad",
    latency_p95: "up-bad",
    cost_total: "up-bad",
    total_tokens: "neutral",
  });
});

it("colours each delta by its tone: neutral muted, a bad rise red, a good fall green", () => {
  render(<KpiRow kpis={kpis} />);
  expect(within(tile(/^Traces:/)).getByText("20%")).toHaveClass("text-muted-foreground");
  expect(within(tile(/^Tokens:/)).getByText("50%")).toHaveClass("text-muted-foreground");
  expect(within(tile(/^Error rate:/)).getByText("100%")).toHaveClass("text-destructive");
  expect(within(tile(/^Cost:/)).getByText("100%")).toHaveClass("text-destructive");
  expect(within(tile(/^p95 latency:/)).getByText("20%")).toHaveClass("text-chart-2");
});

it("shows a rise from zero as `new`, with no arrow, in the colour of a rise", () => {
  render(<KpiRow kpis={[{ metric: "error_rate", value: 0.02, previous: 0 }]} />);
  const badge = within(tile(/^Error rate:/)).getByText("new");
  expect(badge).toHaveClass("text-destructive");
  expect(badge.querySelector("svg")).toBeNull();
});

it("shows a change of 500% or more as a ratio with the arrow", () => {
  const { unmount } = render(<KpiRow kpis={[{ metric: "traces", value: 70, previous: 10 }]} />);
  expect(within(tile(/^Traces:/)).getByText("×7")).toBeInTheDocument();
  expect(
    within(tile(/^Traces:/))
      .getByText("×7")
      .querySelector("svg"),
  ).not.toBeNull();
  unmount();

  const twelve = render(<KpiRow kpis={[{ metric: "traces", value: 120, previous: 10 }]} />);
  expect(within(tile(/^Traces:/)).getByText("×12")).toBeInTheDocument();
  twelve.unmount();

  const six = render(<KpiRow kpis={[{ metric: "traces", value: 60, previous: 10 }]} />);
  expect(within(tile(/^Traces:/)).getByText("×6")).toBeInTheDocument();
  six.unmount();
});

it("keeps the percentage below 500%", () => {
  render(<KpiRow kpis={[{ metric: "traces", value: 40, previous: 10 }]} />);
  expect(within(tile(/^Traces:/)).getByText("300%")).toBeInTheDocument();
});

it("shows the previous period on hover", async () => {
  const user = userEvent.setup();
  render(<KpiRow kpis={kpis} />);
  await user.hover(tile(/^Traces:/));
  expect(await screen.findByRole("tooltip")).toHaveTextContent("Previous period: 100");
});

it("says there is no data for the previous period when it is missing", async () => {
  const user = userEvent.setup();
  render(<KpiRow kpis={[{ metric: "cost_total", value: 2 }]} />);
  await user.hover(tile(/^Cost:/));
  expect(await screen.findByRole("tooltip")).toHaveTextContent("Previous period: no data");
});

it("draws a sparkline for a tile with two or more values, and gaps break the line", () => {
  const { container } = render(
    <KpiRow kpis={kpis} trend={{ traces: [1, 2, null, 3, 4], error_rate: [0.1] }} />,
  );
  const traces = tile(/^Traces:/);
  const svg = traces.querySelector("svg[aria-hidden='true']");
  expect(svg).not.toBeNull();
  expect(traces.querySelectorAll("polyline")).toHaveLength(2);
  expect(tile(/^Error rate:/).querySelectorAll("polyline")).toHaveLength(0);
  expect(container.querySelectorAll("polyline")).toHaveLength(2);
});

it("draws no sparkline for a tile without a trend", () => {
  const { container } = render(<KpiRow kpis={kpis} trend={{ traces: [1, 2] }} />);
  expect(tile(/^Cost:/).querySelectorAll("polyline")).toHaveLength(0);
  expect(container.querySelectorAll("polyline")).toHaveLength(1);
});

it("calls onSelect with the metric when a tile is clicked", async () => {
  const user = userEvent.setup();
  const onSelect = vi.fn();
  render(<KpiRow kpis={kpis} onSelect={onSelect} />);
  await user.click(tile(/^Cost:/));
  expect(onSelect).toHaveBeenCalledWith("cost_total");
});

const at = (minute: number) => new Date(Date.UTC(2026, 10, 17, 0, minute)).toISOString();
const point = (minute: number, value?: number) =>
  value === undefined ? { time: at(minute) } : { time: at(minute), value };

function response(series: Schema<"MetricSeries">[]): Schema<"MetricsResponse"> {
  return {
    bucket_ns: 60_000_000_000,
    from: at(0),
    to: at(3),
    kpis: [],
    series,
    top_models: [],
  };
}

it("builds the traces and cost trends from ungrouped series", () => {
  const trend = kpiTrend(
    response([
      { metric: "traces", points: [point(0, 3), point(1)] },
      { metric: "cost", points: [point(0, 0.75), point(1, 1)] },
    ]),
  );
  expect(trend.traces).toEqual([3, null]);
  expect(trend.cost_total).toEqual([0.75, 1]);
});

it("builds the error rate and p95 trends from their first series", () => {
  const trend = kpiTrend(
    response([
      { metric: "error_rate", points: [point(0, 0.1), point(1)] },
      { metric: "latency_p95", points: [point(0, 2e9), point(1, 3e9)] },
    ]),
  );
  expect(trend.error_rate).toEqual([0.1, null]);
  expect(trend.latency_p95).toEqual([2e9, 3e9]);
});

it("draws no sparklines when a trend series is grouped by model", () => {
  expect(
    kpiTrend(
      response([
        { metric: "traces", group: "a", points: [point(0, 3), point(1, 4)] },
        { metric: "traces", group: "b", points: [point(0, 1), point(1, 1)] },
      ]),
    ),
  ).toEqual({});
  expect(
    kpiTrend(
      response([
        { metric: "cost", group: "a", points: [point(0, 0.5), point(1, 1)] },
        { metric: "cost", group: "b", points: [point(0, 0.25), point(1)] },
      ]),
    ),
  ).toEqual({});
  expect(
    kpiTrend(
      response([{ metric: "error_rate", group: "other", points: [point(0, 0.9), point(1, 0.8)] }]),
    ),
  ).toEqual({});
});

it("sums the four token metrics per bucket and gives null where none is present", () => {
  const trend = kpiTrend(
    response([
      { metric: "input_tokens", points: [point(0, 10), point(1, 5), point(2)] },
      { metric: "output_tokens", points: [point(0, 1), point(1), point(2)] },
      { metric: "cache_read_tokens", points: [point(0), point(1, 2), point(2)] },
      { metric: "cache_write_tokens", points: [point(0), point(1), point(2)] },
    ]),
  );
  expect(trend.total_tokens).toEqual([11, 7, null]);
});
