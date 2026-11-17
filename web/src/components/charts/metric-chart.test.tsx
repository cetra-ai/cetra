import { render, screen, within } from "@testing-library/react";
import { expect, it } from "vitest";
import { DashboardBodySkeleton } from "@/components/charts/metric-chart";

const TITLES = [
  "Traces & errors",
  "Error rate",
  "Latency",
  "LLM latency by model",
  "Cost by model",
  "Tokens by type",
  "TTFT",
  "Scores",
  "Evaluation cost",
  "Top models by cost",
];

it("shows every chart card as a busy region with its title", () => {
  render(<DashboardBodySkeleton />);
  expect(screen.getAllByRole("region")).toHaveLength(TITLES.length);
  for (const title of TITLES) {
    const region = screen.getByRole("region", { name: title });
    expect(region).toHaveAttribute("aria-busy", "true");
    expect(within(region).getByRole("heading", { name: title })).toBeInTheDocument();
  }
});
