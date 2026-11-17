import { render, screen, within } from "@testing-library/react";
import { expect, it } from "vitest";
import type { Schema } from "@/api/types";
import { UsersSummary } from "./users-summary";

const kpis: Schema<"Kpi">[] = [
  { metric: "traces", value: 12, previous: 10 },
  { metric: "cost_total", value: 0.5 },
];

it("shows the loaded user count with a plus when more pages exist", () => {
  render(<UsersSummary kpis={kpis} users={40} more />);
  const summary = screen.getByRole("region", { name: "Summary" });
  expect(within(summary).getByText("Users")).toBeInTheDocument();
  expect(within(summary).getByText("40+")).toBeInTheDocument();
});

it("formats the KPI values and shows a dash for missing ones", () => {
  render(<UsersSummary kpis={kpis} users={3} more={false} />);
  const summary = screen.getByRole("region", { name: "Summary" });
  expect(within(summary).getByText("3")).toBeInTheDocument();
  expect(within(summary).getByText("12")).toBeInTheDocument();
  expect(within(summary).getAllByText("—")).toHaveLength(3);
});

it("shows dashes for every KPI while the metrics are not loaded", () => {
  render(<UsersSummary kpis={undefined} users={0} more={false} />);
  const summary = screen.getByRole("region", { name: "Summary" });
  expect(within(summary).getAllByText("—")).toHaveLength(5);
});
