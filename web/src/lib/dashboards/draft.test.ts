import { describe, expect, it } from "vitest";
import {
  bottomOf,
  issueWidgetIndex,
  issueWithin,
  newWidgetId,
  nudgedLayout,
  sameDefinition,
  withLayouts,
} from "./draft";
import type { Widget } from "./types";

function chart(id: string, x: number, y: number, w: number, h: number): Widget {
  return {
    id,
    title: id,
    kind: "chart",
    chart: { type: "line" },
    query: { source: "traces", measures: [{ agg: "count" }] },
    layout: { x, y, w, h },
  };
}

describe("bottomOf", () => {
  it("is the first row below every widget", () => {
    expect(bottomOf([])).toBe(0);
    expect(bottomOf([chart("a", 0, 0, 6, 3), chart("b", 6, 1, 6, 7)])).toBe(8);
  });
});

describe("withLayouts", () => {
  it("returns the same list when no position moved", () => {
    const widgets = [chart("a", 0, 0, 6, 3)];
    expect(withLayouts(widgets, [{ i: "a", x: 0, y: 0, w: 6, h: 3 }])).toBe(widgets);
    expect(withLayouts(widgets, [{ i: "unknown", x: 1, y: 1, w: 1, h: 1 }])).toBe(widgets);
  });

  it("applies the new position of a moved widget only", () => {
    const widgets = [chart("a", 0, 0, 6, 3), chart("b", 6, 0, 6, 3)];
    const next = withLayouts(widgets, [
      { i: "a", x: 0, y: 2, w: 6, h: 3 },
      { i: "b", x: 6, y: 0, w: 6, h: 3 },
    ]);
    expect(next).not.toBe(widgets);
    expect(next[0]?.layout).toEqual({ x: 0, y: 2, w: 6, h: 3 });
    expect(next[1]).toBe(widgets[1]);
  });
});

describe("issue paths", () => {
  it("reads the widget index of a path", () => {
    expect(issueWidgetIndex("definition.widgets[3].query.measures[0].field")).toBe(3);
    expect(issueWidgetIndex("definition.widgets")).toBeUndefined();
    expect(issueWidgetIndex("name")).toBeUndefined();
  });

  it("drops the widget prefix for display", () => {
    expect(issueWithin("definition.widgets[3].query.measures[0].field")).toBe(
      "query.measures[0].field",
    );
    expect(issueWithin("definition.widgets[3].id")).toBe("id");
    expect(issueWithin("name")).toBe("name");
  });
});

describe("newWidgetId", () => {
  it("is a valid widget id, and a new one each time", () => {
    const id = newWidgetId();
    expect(id).toMatch(/^[A-Za-z0-9_-]{1,64}$/);
    expect(newWidgetId()).not.toBe(id);
  });
});

describe("sameDefinition", () => {
  it("compares the widgets", () => {
    const widgets = [chart("a", 0, 0, 6, 3)];
    expect(sameDefinition({ widgets }, { widgets: structuredClone(widgets) })).toBe(true);
    expect(sameDefinition({ widgets }, { widgets: [chart("a", 0, 1, 6, 3)] })).toBe(false);
  });

  it("does not count the order of keys or an option the editor leaves out as a change", () => {
    const stored: Widget = {
      id: "a",
      title: "a",
      layout: { x: 0, y: 0, w: 6, h: 3 },
      kind: "chart",
      chart: { type: "line" },
      query: {
        source: "traces",
        measures: [{ agg: "count" }],
        breakdown: { field: "models", limit: 5, other: true },
      },
    };
    const written: Widget = {
      id: "a",
      title: "a",
      kind: "chart",
      chart: { type: "line" },
      query: {
        source: "traces",
        measures: [{ agg: "count" }],
        breakdown: { field: "models", limit: 5 },
      },
      layout: { x: 0, y: 0, w: 6, h: 3 },
    };
    expect(sameDefinition({ widgets: [stored] }, { widgets: [written] })).toBe(true);
  });
});

describe("nudgedLayout", () => {
  const at = (x: number, y: number, w: number, h: number) => ({ x, y, w, h });

  it("moves and resizes by five columns across and one row down", () => {
    expect(nudgedLayout(at(10, 3, 20, 5), "up")).toEqual(at(10, 2, 20, 5));
    expect(nudgedLayout(at(10, 3, 20, 5), "down")).toEqual(at(10, 4, 20, 5));
    expect(nudgedLayout(at(10, 3, 20, 5), "left")).toEqual(at(5, 3, 20, 5));
    expect(nudgedLayout(at(10, 3, 20, 5), "right")).toEqual(at(15, 3, 20, 5));
    expect(nudgedLayout(at(10, 3, 20, 5), "wider")).toEqual(at(10, 3, 25, 5));
    expect(nudgedLayout(at(10, 3, 20, 5), "narrower")).toEqual(at(10, 3, 15, 5));
    expect(nudgedLayout(at(10, 3, 20, 5), "taller")).toEqual(at(10, 3, 20, 6));
    expect(nudgedLayout(at(10, 3, 20, 5), "shorter")).toEqual(at(10, 3, 20, 4));
  });

  it("stops a step across at the grid's edges", () => {
    expect(nudgedLayout(at(3, 0, 20, 1), "left")).toEqual(at(0, 0, 20, 1));
    expect(nudgedLayout(at(38, 0, 20, 1), "right")).toEqual(at(40, 0, 20, 1));
    expect(nudgedLayout(at(38, 0, 20, 1), "wider")).toEqual(at(38, 0, 22, 1));
    expect(nudgedLayout(at(0, 0, 3, 1), "narrower")).toEqual(at(0, 0, 1, 1));
  });

  it("refuses a step that leaves the grid's limits", () => {
    expect(nudgedLayout(at(0, 0, 4, 1), "up")).toBeUndefined();
    expect(nudgedLayout(at(0, 0, 4, 1), "left")).toBeUndefined();
    expect(nudgedLayout(at(40, 0, 20, 1), "right")).toBeUndefined();
    expect(nudgedLayout(at(40, 0, 20, 1), "wider")).toBeUndefined();
    expect(nudgedLayout(at(0, 0, 1, 1), "narrower")).toBeUndefined();
    expect(nudgedLayout(at(0, 0, 4, 40), "taller")).toBeUndefined();
    expect(nudgedLayout(at(0, 0, 4, 1), "shorter")).toBeUndefined();
    expect(nudgedLayout(at(0, 1000, 4, 1), "down")).toBeUndefined();
  });
});
