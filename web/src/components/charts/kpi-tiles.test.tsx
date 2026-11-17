import { render, screen } from "@testing-library/react";
import { expect, it } from "vitest";
import { KpiRowSkeleton } from "@/components/charts/kpi-tiles";

it("shows each KPI tile's label with a bar for its value", () => {
  const { container } = render(<KpiRowSkeleton />);
  for (const label of ["Traces", "Error rate", "p95 latency", "Cost", "Tokens"]) {
    expect(screen.getByText(label)).toBeInTheDocument();
  }
  expect(container.querySelectorAll('[data-slot="skeleton"]')).toHaveLength(5);
  expect(screen.queryByRole("button")).not.toBeInTheDocument();
});
