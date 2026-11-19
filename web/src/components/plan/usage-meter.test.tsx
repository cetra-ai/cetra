import { cleanup, render, screen } from "@testing-library/react";
import { expect, it } from "vitest";
import { UsageMeter, usagePercent } from "./usage-meter";

it("computes the percent against the included value, or the cap without one", () => {
  expect(usagePercent(50, 200, 300)).toBe(25);
  expect(usagePercent(50, undefined, 200)).toBe(25);
});

it("is undefined without a base or with a base of 0", () => {
  expect(usagePercent(50)).toBeUndefined();
  expect(usagePercent(50, 0, 100)).toBeUndefined();
  expect(usagePercent(50, undefined, 0)).toBeUndefined();
});

it("is not capped at 100", () => {
  expect(usagePercent(250, 200)).toBe(125);
});

function barClassAt(units: number) {
  render(
    <UsageMeter units={units} resetsAt="2026-02-01T00:00:00Z" included={200} cap={undefined} />,
  );
  const classes = screen.getByRole("meter").firstElementChild?.className ?? "";
  cleanup();
  return classes;
}

it("colours the bar by threshold: primary, then warn at 80 %, then error at 100 %", () => {
  expect(barClassAt(100)).toContain("bg-primary");
  expect(barClassAt(160)).toContain("bg-status-warn");
  expect(barClassAt(200)).toContain("bg-status-error");
});

it("shows the compact text with the percent, and no reset or cap lines", () => {
  render(
    <UsageMeter
      units={1240}
      resetsAt="2026-02-01T00:00:00Z"
      included={200000}
      cap={250000}
      variant="compact"
    />,
  );

  expect(screen.getByText("1,240 of 200,000 units · 1%")).toBeInTheDocument();
  expect(screen.queryByText(/Resets on/)).toBeNull();
  expect(screen.queryByText(/Ingest pauses/)).toBeNull();
  expect(screen.getByRole("meter")).toHaveClass("h-1.5");
});

it("shows only the units text when there is no base", () => {
  render(
    <UsageMeter
      units={1240}
      resetsAt="2026-02-01T00:00:00Z"
      included={undefined}
      cap={undefined}
      variant="compact"
    />,
  );

  expect(screen.getByText("1,240 units")).toBeInTheDocument();
  expect(screen.queryByRole("meter")).toBeNull();
});
