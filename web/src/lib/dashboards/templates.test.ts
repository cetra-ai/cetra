import { expect, it } from "vitest";
import { COLUMNS } from "./draft";
import { TEMPLATES } from "./templates";
import type { Widget } from "./types";
import { validateDefinition } from "./validate";

function overlaps(a: Widget, b: Widget): boolean {
  const left = a.layout;
  const right = b.layout;
  return (
    left.x < right.x + right.w &&
    right.x < left.x + left.w &&
    left.y < right.y + right.h &&
    right.y < left.y + left.h
  );
}

it("every template passes the dashboard's own validation", () => {
  for (const template of TEMPLATES) {
    expect(validateDefinition(template.definition)).toEqual([]);
  }
});

it("templates' widgets have unique ids and do not overlap", () => {
  for (const template of TEMPLATES) {
    const widgets = template.definition.widgets;
    const ids = widgets.map((widget) => widget.id);
    expect(new Set(ids).size, template.id).toBe(ids.length);
    for (let i = 0; i < widgets.length; i += 1) {
      for (let j = i + 1; j < widgets.length; j += 1) {
        const a = widgets[i];
        const b = widgets[j];
        if (a && b) {
          expect(overlaps(a, b), `${template.id}: ${a.id} and ${b.id}`).toBe(false);
        }
      }
    }
  }
});

it("the ids and names are Cost, Latency and Quality, in that order", () => {
  expect(TEMPLATES.map((template) => template.id)).toEqual(["cost", "latency", "quality"]);
  expect(TEMPLATES.map((template) => template.name)).toEqual(["Cost", "Latency", "Quality"]);
});

it("each number row fills the width", () => {
  for (const template of TEMPLATES) {
    const tiles = template.definition.widgets.filter((widget) => widget.chart?.type === "number");
    expect(
      tiles.reduce((sum, widget) => sum + widget.layout.w, 0),
      template.id,
    ).toBe(COLUMNS);
  }
});
