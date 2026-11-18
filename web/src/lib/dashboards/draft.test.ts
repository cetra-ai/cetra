import { describe, expect, it } from "vitest";
import {
  bottomOf,
  issueWidgetIndex,
  issueWithin,
  newWidgetId,
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
});
