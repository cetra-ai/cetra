import { describe, expect, it } from "vitest";
import type { Definition, Widget } from "./types";
import { USER_OVERVIEW } from "./user-overview";
import { validateDefinition } from "./validate";

/** Whether two widgets' cells share a point: their rectangles in grid units overlap. */
function overlap(left: Widget, right: Widget): boolean {
  const a = left.layout;
  const b = right.layout;
  return a.x < b.x + b.w && b.x < a.x + a.w && a.y < b.y + b.h && b.y < a.y + a.h;
}

function overlapping(definition: Definition): [string, string][] {
  const pairs: [string, string][] = [];
  definition.widgets.forEach((widget, index) => {
    for (const other of definition.widgets.slice(index + 1)) {
      if (overlap(widget, other)) pairs.push([widget.title, other.title]);
    }
  });
  return pairs;
}

describe("the user Usage", () => {
  it("is a valid definition whose queries all pass the server rules", () => {
    expect(validateDefinition(USER_OVERVIEW)).toEqual([]);
  });

  it("has the five numbers, then every Overview chart but the evaluation cost and the top users, with no headings", () => {
    const titles = USER_OVERVIEW.widgets.map((widget) => widget.title);
    expect(titles).toEqual([
      "Traces",
      "Error rate",
      "p95 latency",
      "Cost",
      "Tokens",
      "Traces & errors",
      "Tokens by type",
      "Error rate",
      "Scores",
      "Latency",
      "LLM latency by model",
      "TTFT",
      "Cost by model",
      "Top models by cost",
      "Top trace names by cost",
    ]);
    expect(USER_OVERVIEW.widgets.some((widget) => widget.kind === "heading")).toBe(false);
  });

  it("lays the charts out two per row, half the width by 7 rows, below the numbers", () => {
    const charts = USER_OVERVIEW.widgets.slice(5);
    expect(
      charts.map((widget) => [widget.layout.x, widget.layout.y, widget.layout.w, widget.layout.h]),
    ).toEqual([
      [0, 4, 30, 7],
      [30, 4, 30, 7],
      [0, 11, 30, 7],
      [30, 11, 30, 7],
      [0, 18, 30, 7],
      [30, 18, 30, 7],
      [0, 25, 30, 7],
      [30, 25, 30, 7],
      [0, 32, 30, 7],
      [30, 32, 30, 7],
    ]);
  });

  it("starts the charts under the four-row numbers and places no two widgets over each other", () => {
    expect(USER_OVERVIEW.widgets.slice(0, 5).every((widget) => widget.layout.h === 4)).toBe(true);
    expect(overlapping(USER_OVERVIEW)).toEqual([]);
  });
});
