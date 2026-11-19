import { render, screen } from "@testing-library/react";
import { expect, it } from "vitest";
import { BarCell } from "./bar-cell";

it("sizes the bar as a share of the maximum", () => {
  const { container } = render(
    <BarCell value={5} max={10}>
      five
    </BarCell>,
  );
  expect(container.querySelector("[data-bar]")).toHaveStyle({ width: "50%" });
  expect(screen.getByText("five")).toBeInTheDocument();
});

it("draws no bar for a missing value", () => {
  const { container } = render(
    <BarCell value={undefined} max={10}>
      none
    </BarCell>,
  );
  expect(container.querySelector("[data-bar]")).toHaveStyle({ width: "0%" });
});
