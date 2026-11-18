import { render as renderIn, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import type { ReactElement } from "react";
import { expect, it } from "vitest";
import type { Schema } from "@/api/types";
import { KPI_TONE, KpiRow, KpiRowSkeleton } from "@/components/charts/kpi-tiles";
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
