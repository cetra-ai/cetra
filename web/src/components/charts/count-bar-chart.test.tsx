import { render, screen, waitFor } from "@testing-library/react";
import { expect, it } from "vitest";
import { CountBarChart } from "@/components/charts/count-bar-chart";

it("draws one bar per non-empty bucket under the given name", async () => {
  const start = Date.parse("2026-11-17T10:00:00.000Z");
  const rows = [
    { time: start, count: 2 },
    { time: start + 60_000, count: null },
    { time: start + 120_000, count: 5 },
  ];
  render(<CountBarChart rows={rows} label="Traces" seriesLabel="Traces" />);
  const chart = screen.getByRole("img", { name: "Traces" });
  await waitFor(() => {
    expect(chart.querySelectorAll(".recharts-bar-rectangle")).toHaveLength(2);
  });
});
