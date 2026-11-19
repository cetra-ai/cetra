import { render, screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import { ScoreLine, scoreLineDomain, scoreLineSegments } from "./score-line";

describe("scoreLineDomain", () => {
  it("is 0 to 1 for pass rates", () => {
    expect(scoreLineDomain([0.9, 0.95], true)).toEqual([0, 1]);
  });

  it("is the values' range otherwise, widened when flat", () => {
    expect(scoreLineDomain([0.2, null, 0.8], false)).toEqual([0.2, 0.8]);
    expect(scoreLineDomain([3, 3], false)).toEqual([2.5, 3.5]);
    expect(scoreLineDomain([null], false)).toEqual([0, 1]);
  });
});

describe("scoreLineSegments", () => {
  it("breaks the line at hours without a value", () => {
    const segments = scoreLineSegments([0, 1, null, 0.5, null], [0, 1]);
    expect(segments).toEqual([
      [
        { x: 0.5, y: 1 },
        { x: 1.5, y: 0 },
      ],
      [{ x: 3.5, y: 0.5 }],
    ]);
  });
});

describe("ScoreLine", () => {
  it("labels the line and shows the range and span with the axis", () => {
    render(
      <ScoreLine
        values={[0.2, 0.8]}
        boolean={false}
        label="Average score per hour, last 24 h"
        axis
      />,
    );
    expect(screen.getByRole("img", { name: "Average score per hour, last 24 h" })).toBeVisible();
    expect(screen.getByText("0.80")).toBeVisible();
    expect(screen.getByText("0.20")).toBeVisible();
    expect(screen.getByText("24 h ago")).toBeVisible();
  });
});
