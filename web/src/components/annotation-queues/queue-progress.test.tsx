import { render, screen } from "@testing-library/react";
import { expect, it } from "vitest";
import { QueueProgress } from "@/components/annotation-queues/queue-progress";

it("shows the reviewed items out of all reviewable ones", () => {
  render(<QueueProgress counts={{ pending: 12, completed: 4, skipped: 1 }} />);

  expect(screen.getByText("5 of 17 done")).toBeInTheDocument();
  const bar = screen.getByRole("progressbar", { name: "Reviewed" });
  expect(bar).toHaveAttribute("aria-valuenow", "5");
  expect(bar).toHaveAttribute("aria-valuemax", "17");
  expect(bar).toHaveAttribute("aria-valuemin", "0");
});

it("shows an empty queue as nothing done", () => {
  render(<QueueProgress counts={{ pending: 0, completed: 0, skipped: 0 }} />);

  expect(screen.getByText("0 of 0 done")).toBeInTheDocument();
  expect(screen.getByRole("progressbar", { name: "Reviewed" })).toHaveAttribute(
    "aria-valuenow",
    "0",
  );
});
