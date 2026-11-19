import { render, screen } from "@testing-library/react";
import { expect, it } from "vitest";
import { TooltipProvider } from "@/components/ui/tooltip";
import { JobsBar } from "./jobs-bar";

const COUNTS = { done: 40, failed: 3, running: 1, queued: 2, skipped: 0 };

function renderBar(counts: typeof COUNTS) {
  return render(
    <TooltipProvider>
      <JobsBar counts={counts} />
    </TooltipProvider>,
  );
}

it("says no jobs when there are none", () => {
  renderBar({ done: 0, failed: 0, running: 0, queued: 0, skipped: 0 });
  expect(screen.getByText("No jobs")).toBeInTheDocument();
  expect(screen.queryByRole("img")).toBeNull();
});

it("shows the total and the failures", () => {
  renderBar(COUNTS);
  expect(screen.getByText("3 failed")).toHaveClass("text-status-error");
  expect(screen.getByRole("img")).toHaveAttribute(
    "aria-label",
    "40 done, 3 failed, 1 running, 2 queued",
  );
});

it("draws one segment per status that has jobs", () => {
  const { container } = renderBar(COUNTS);
  const segments = container.querySelectorAll("[data-status]");
  expect(Array.from(segments, (segment) => segment.getAttribute("data-status"))).toEqual([
    "done",
    "failed",
    "running",
    "queued",
  ]);
});

it("omits the failure text when nothing failed", () => {
  renderBar({ ...COUNTS, failed: 0 });
  expect(screen.queryByText(/failed/)).toBeNull();
});
