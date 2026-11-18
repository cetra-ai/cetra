import { describe, expect, it } from "vitest";
import { OVERVIEW } from "./overview";
import { validateDefinition } from "./validate";

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

  it("gives every widget a unique id", () => {
    const ids = OVERVIEW.widgets.map((widget) => widget.id);
    expect(new Set(ids).size).toBe(ids.length);
  });
});
