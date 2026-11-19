import { describe, expect, it } from "vitest";
import { OVERVIEW } from "./overview";
import type { Definition, Widget } from "./types";
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

describe("the Overview", () => {
  it("is a valid definition whose queries all pass the server rules", () => {
    expect(validateDefinition(OVERVIEW)).toEqual([]);
  });

  it("lists the widgets in the order of spec 11, with no headings", () => {
    const titles = OVERVIEW.widgets.map((widget) => widget.title);
    expect(titles.slice(0, 5)).toEqual(["Traces", "Error rate", "p95 latency", "Cost", "Tokens"]);
    expect(OVERVIEW.widgets.some((widget) => widget.kind === "heading")).toBe(false);
    expect(titles.slice(-3)).toEqual([
      "Top models by cost",
      "Top users by cost",
      "Top trace names by cost",
    ]);
  });

  it("draws the two cost bars as horizontal bars of the 10 largest, by user and by trace name", () => {
    const byUser = OVERVIEW.widgets.find((widget) => widget.title === "Top users by cost");
    const byName = OVERVIEW.widgets.find((widget) => widget.title === "Top trace names by cost");
    expect(byUser?.chart?.type).toBe("bar_h");
    expect(byUser?.query?.breakdown).toEqual({ field: "user_id", limit: 10, other: false });
    expect(byUser?.query?.filter).toBe("user_id IS NOT NULL");
    expect(byName?.chart?.type).toBe("bar_h");
    expect(byName?.query?.breakdown).toEqual({ field: "name", limit: 10, other: false });
  });

  it("keeps the earlier widgets in place after the refactor", () => {
    expect(
      OVERVIEW.widgets
        .slice(0, 15)
        .map(({ id, title, layout }) => [id, title, layout.x, layout.y, layout.w, layout.h]),
    ).toEqual([
      ["ov-1", "Traces", 0, 0, 12, 4],
      ["ov-2", "Error rate", 12, 0, 12, 4],
      ["ov-3", "p95 latency", 24, 0, 12, 4],
      ["ov-4", "Cost", 36, 0, 12, 4],
      ["ov-5", "Tokens", 48, 0, 12, 4],
      ["ov-7", "Traces & errors", 0, 4, 30, 7],
      ["ov-8", "Tokens by type", 30, 4, 30, 7],
      ["ov-10", "Error rate", 0, 11, 30, 7],
      ["ov-11", "Scores", 30, 11, 30, 7],
      ["ov-13", "Latency", 0, 18, 30, 7],
      ["ov-14", "LLM latency by model", 30, 18, 30, 7],
      ["ov-15", "TTFT", 0, 25, 30, 7],
      ["ov-17", "Cost by model", 0, 32, 30, 7],
      ["ov-18", "Evaluation cost", 30, 32, 30, 7],
      ["ov-19", "Top models by cost", 0, 39, 30, 7],
    ]);
  });

  it("gives the numbers four rows and places no two widgets over each other", () => {
    const numbers = OVERVIEW.widgets.filter((widget) => widget.chart?.type === "number");
    expect(numbers.map((widget) => widget.layout.h)).toEqual([4, 4, 4, 4, 4]);
    // The five share the whole first row equally.
    expect(numbers.map((widget) => [widget.layout.x, widget.layout.w])).toEqual([
      [0, 12],
      [12, 12],
      [24, 12],
      [36, 12],
      [48, 12],
    ]);
    expect(overlapping(OVERVIEW)).toEqual([]);
  });

  it("gives every widget a unique id", () => {
    const ids = OVERVIEW.widgets.map((widget) => widget.id);
    expect(new Set(ids).size).toBe(ids.length);
  });
});
