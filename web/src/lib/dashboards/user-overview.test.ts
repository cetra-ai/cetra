import { describe, expect, it } from "vitest";
import { USER_OVERVIEW } from "./user-overview";
import { validateDefinition } from "./validate";

describe("the user Usage", () => {
  it("is a valid definition whose queries all pass the server rules", () => {
    expect(validateDefinition(USER_OVERVIEW)).toEqual([]);
  });

  it("has the five numbers, then every Overview chart but the evaluation cost, with no headings", () => {
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
    ]);
    expect(USER_OVERVIEW.widgets.some((widget) => widget.kind === "heading")).toBe(false);
  });

  it("lays the charts out two per row, 6 columns by 7 rows, below the numbers", () => {
    const charts = USER_OVERVIEW.widgets.slice(5);
    expect(
      charts.map((widget) => [widget.layout.x, widget.layout.y, widget.layout.w, widget.layout.h]),
    ).toEqual([
      [0, 3, 6, 7],
      [6, 3, 6, 7],
      [0, 10, 6, 7],
      [6, 10, 6, 7],
      [0, 17, 6, 7],
      [6, 17, 6, 7],
      [0, 24, 6, 7],
      [6, 24, 6, 7],
      [0, 31, 6, 7],
    ]);
  });
});
