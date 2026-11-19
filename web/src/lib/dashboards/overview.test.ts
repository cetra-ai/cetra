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

  it("lists the headings and widgets in the order of spec 11", () => {
    const titles = OVERVIEW.widgets.map((widget) => widget.title);
    expect(titles.slice(0, 5)).toEqual(["Traces", "Error rate", "p95 latency", "Cost", "Tokens"]);
    expect(titles.filter((_, index) => OVERVIEW.widgets[index]?.kind === "heading")).toEqual([
      "Volume",
      "Quality",
      "Latency",
      "Cost",
    ]);
    expect(titles.at(-1)).toBe("Top models by cost");
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
