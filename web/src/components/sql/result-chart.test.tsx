import { cleanup, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, expect, it } from "vitest";
import type { Schema } from "@/api/types";
import { SqlResultChart } from "@/components/sql/result-chart";

const TS = "timestamp";

function result(
  columns: { name: string; type: string }[],
  rows: Record<string, unknown>[],
): Schema<"SqlQueryResult"> {
  return { columns, rows: rows as Schema<"SqlQueryResult">["rows"], truncated: false };
}

const MODELS = result(
  [
    { name: "day", type: TS },
    { name: "model", type: "string" },
    { name: "cost", type: "float" },
  ],
  [
    { day: "2026-01-01T00:00:00.000000000Z", model: "a", cost: 1 },
    { day: "2026-01-02T00:00:00.000000000Z", model: "b", cost: 2 },
  ],
);

afterEach(() => {
  cleanup();
});

it("starts from the automatic choice and follows Type and X", async () => {
  const user = userEvent.setup();
  render(<SqlResultChart result={MODELS} />);
  expect(
    screen.getByRole("region", { name: "Line chart of cost by day, one series per model" }),
  ).toBeInTheDocument();

  await user.click(screen.getByRole("button", { name: "Bar" }));
  expect(
    screen.getByRole("region", { name: "Bar chart of cost by day, one series per model" }),
  ).toBeInTheDocument();

  await user.click(screen.getByRole("combobox", { name: "X column" }));
  await user.click(await screen.findByRole("option", { name: "model" }));
  // The split-by column was the new x, so it is cleared; the one y stays.
  expect(screen.getByRole("region", { name: "Bar chart of cost by model" })).toBeInTheDocument();
});

it("keeps the choice for the same result and resets it for a new one", async () => {
  const user = userEvent.setup();
  const { rerender } = render(<SqlResultChart result={MODELS} />);
  await user.click(screen.getByRole("button", { name: "Bar" }));
  expect(screen.getByRole("region", { name: /^Bar chart/ })).toBeInTheDocument();

  const next = result(MODELS.columns, [...(MODELS.rows as Record<string, unknown>[])]);
  rerender(<SqlResultChart result={next} />);
  expect(
    screen.getByRole("region", { name: "Line chart of cost by day, one series per model" }),
  ).toBeInTheDocument();
});

it("shows the no-chart message for a result without a usable column", () => {
  render(<SqlResultChart result={result([{ name: "flag", type: "boolean" }], [{ flag: true }])} />);
  expect(
    screen.getByText(
      "No chart for this result: it needs a time or text column and a numeric column.",
    ),
  ).toBeInTheDocument();
});
