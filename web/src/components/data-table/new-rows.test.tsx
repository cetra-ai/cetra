import { fireEvent, render, screen } from "@testing-library/react";
import { expect, it, vi } from "vitest";
import { countLabel, NewRowsPill } from "@/components/data-table/new-rows";

it("labels a count, singular for one, and marks a capped answer with +", () => {
  expect(countLabel(1, false, "new span", "new spans")).toBe("1 new span");
  expect(countLabel(3, false, "new span", "new spans")).toBe("3 new spans");
  expect(countLabel(50, true, "new span", "new spans")).toBe("50+ new spans");
  expect(countLabel(1, true, "new span", "new spans")).toBe("1+ new spans");
});

it("renders the pill as a button that calls onClick", () => {
  const onClick = vi.fn();
  render(<NewRowsPill label="2 new spans" onClick={onClick} />);
  fireEvent.click(screen.getByRole("button", { name: "2 new spans ↑" }));
  expect(onClick).toHaveBeenCalledTimes(1);
});
