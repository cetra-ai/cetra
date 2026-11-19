import { render, screen, within } from "@testing-library/react";
import { expect, it } from "vitest";
import type { Schema } from "@/api/types";
import { DatasetsSummary } from "./datasets-summary";

function dataset(overrides: Partial<Schema<"Dataset">>): Schema<"Dataset"> {
  return {
    id: "01900000-0000-7000-8000-000000000c01",
    name: "faq",
    current_version: 1,
    item_count: 0,
    experiment_count: 0,
    created_at: "2026-01-01T00:00:00.000000000Z",
    updated_at: "2026-01-01T00:00:00.000000000Z",
    ...overrides,
  };
}

it("sums the loaded items and experiments", () => {
  render(
    <DatasetsSummary
      datasets={[
        dataset({ item_count: 3, experiment_count: 1 }),
        dataset({ id: "01900000-0000-7000-8000-000000000c02", item_count: 5, experiment_count: 2 }),
      ]}
      more={false}
    />,
  );
  const summary = screen.getByRole("region", { name: "Summary" });
  expect(within(summary).getByText("Datasets").parentElement).toHaveTextContent("Datasets 2");
  expect(within(summary).getByText("Items").parentElement).toHaveTextContent("Items 8");
  expect(within(summary).getByText("Experiments").parentElement).toHaveTextContent("Experiments 3");
});

it("marks the totals with a plus when another page exists", () => {
  render(<DatasetsSummary datasets={[dataset({ item_count: 3 })]} more />);
  const summary = screen.getByRole("region", { name: "Summary" });
  expect(within(summary).getByText("Datasets").parentElement).toHaveTextContent("Datasets 1+");
  expect(within(summary).getByText("Items").parentElement).toHaveTextContent("Items 3+");
});

it("shows the newest update time as the last updated time", () => {
  const { container } = render(
    <DatasetsSummary
      datasets={[
        dataset({ updated_at: "2026-01-01T00:00:00.000000000Z" }),
        dataset({
          id: "01900000-0000-7000-8000-000000000c02",
          updated_at: "2026-03-01T00:00:00.000000000Z",
        }),
      ]}
      more={false}
    />,
  );
  expect(container.querySelector("time")).toHaveAttribute(
    "datetime",
    "2026-03-01T00:00:00.000000000Z",
  );
});

it("shows a dash for the last update when there are no datasets", () => {
  render(<DatasetsSummary datasets={[]} more={false} />);
  const summary = screen.getByRole("region", { name: "Summary" });
  expect(within(summary).getByText("Last updated").parentElement).toHaveTextContent(
    "Last updated —",
  );
});
