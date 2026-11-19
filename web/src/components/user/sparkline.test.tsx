import { render } from "@testing-library/react";
import { expect, it } from "vitest";
import { Sparkline } from "./sparkline";

it("draws one bar per non-zero value, scaled to the largest", () => {
  const { container } = render(<Sparkline values={[0, 2, 4]} label="Activity" />);
  const svg = container.querySelector("svg");
  expect(svg).toHaveAttribute("aria-label", "Activity");
  const rects = container.querySelectorAll("rect");
  expect(rects).toHaveLength(2);
  expect(rects[1]).toHaveAttribute("height", "1");
  expect(rects[1]).toHaveAttribute("y", "0");
});

it("renders no bars for empty values", () => {
  const { container } = render(<Sparkline values={[]} label="Activity" />);
  expect(container.querySelector("svg")).toBeInTheDocument();
  expect(container.querySelectorAll("rect")).toHaveLength(0);
});
