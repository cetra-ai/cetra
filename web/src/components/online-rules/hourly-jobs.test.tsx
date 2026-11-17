import { render, screen } from "@testing-library/react";
import { expect, it } from "vitest";
import type { Schema } from "@/api/types";
import { HourlyJobs } from "./hourly-jobs";

type RuleJobsHour = Schema<"RuleJobsHour">;

function hour(index: number, counts: Partial<Pick<RuleJobsHour, "done" | "failed" | "skipped">>) {
  return {
    time: `2026-01-01T${String(index).padStart(2, "0")}:00:00.000000000Z`,
    done: 0,
    failed: 0,
    skipped: 0,
    ...counts,
  };
}

it("names the chart and draws one column per hour", () => {
  const jobs = Array.from({ length: 24 }, (_, index) => hour(index, { done: 1 }));
  const { container } = render(<HourlyJobs jobs={jobs} />);

  expect(
    screen.getByRole("img", { name: "Finished jobs per hour, last 24 h" }),
  ).toBeInTheDocument();
  expect(container.querySelectorAll("[role=img] > div")).toHaveLength(24);
});

it("scales each hour's column to the busiest hour and splits it by status", () => {
  // Totals 4 and 8: the first column is half the height of the busiest one.
  const jobs = [hour(0, { done: 2, failed: 2 }), hour(1, { done: 4, skipped: 4 })];
  const { container } = render(<HourlyJobs jobs={jobs} />);

  const columns = Array.from(container.querySelectorAll("[role=img] > div > div"));
  expect(columns.map((column) => column.getAttribute("style"))).toEqual([
    "height: 50%;",
    "height: 100%;",
  ]);
  const segments = Array.from(container.querySelectorAll("[data-status]"));
  expect(
    segments.map((segment) => [segment.getAttribute("data-status"), segment.getAttribute("style")]),
  ).toEqual([
    ["done", "height: 50%;"],
    ["failed", "height: 50%;"],
    ["done", "height: 50%;"],
    ["skipped", "height: 50%;"],
  ]);
});

it("draws no segments when no hour has a finished job", () => {
  const jobs = Array.from({ length: 24 }, (_, index) => hour(index, {}));
  const { container } = render(<HourlyJobs jobs={jobs} />);

  expect(container.querySelectorAll("[data-status]")).toHaveLength(0);
});
