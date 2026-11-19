import { render, screen, within } from "@testing-library/react";
import { expect, it } from "vitest";
import { type UsersKpi, UsersSummary, usersKpis } from "./users-summary";

const kpis: UsersKpi[] = [
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

it("shows each KPI's change against the previous period, with the previous value on hover", () => {
  render(
    <UsersSummary
      kpis={[{ metric: "traces", value: 120, previous: 100 }]}
      users={3}
      more={false}
    />,
  );
  const summary = screen.getByRole("region", { name: "Summary" });
  expect(within(summary).getByText("20%")).toBeInTheDocument();
  expect(within(summary).getByText("Traces").parentElement).toHaveAttribute(
    "title",
    "Previous: 100",
  );
  expect(within(summary).getByText("Users").parentElement).not.toHaveAttribute("title");
});

it("shows dashes for every KPI while the metrics are not loaded", () => {
  render(<UsersSummary kpis={undefined} users={0} more={false} />);
  const summary = screen.getByRole("region", { name: "Summary" });
  expect(within(summary).getAllByText("—")).toHaveLength(5);
});

it("reads the headline numbers from the total answer, in the list's order", () => {
  expect(
    usersKpis({
      shape: "total",
      rows: [
        {
          group: undefined,
          other: false,
          values: [12, 0.25, 800_000_000, 1.5, 4000],
          previous: [10, null, 900_000_000, 1, 3000],
        },
      ],
    }),
  ).toEqual([
    { metric: "traces", value: 12, previous: 10 },
    { metric: "error_rate", value: 0.25, previous: undefined },
    { metric: "latency_p95", value: 800_000_000, previous: 900_000_000 },
    { metric: "cost_total", value: 1.5, previous: 1 },
    { metric: "total_tokens", value: 4000, previous: 3000 },
  ]);
  expect(usersKpis(undefined)).toBeUndefined();
});
