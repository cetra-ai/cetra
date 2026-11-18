import { describe, expect, it } from "vitest";
import dashboardDocument from "../../../../fixtures/api/static/dashboard.json";
import { blankForm, formOf, widgetOf, withSource } from "./editor";
import type { Definition, Widget } from "./types";

const definition = dashboardDocument.definition as Definition;

function widgetNamed(title: string): Widget {
  const widget = definition.widgets.find((item) => item.title === title);
  if (!widget) {
    throw new Error(`no fixture widget ${title}`);
  }
  return widget;
}

describe("formOf and widgetOf", () => {
  it("round-trips a stored widget unchanged", () => {
    for (const widget of definition.widgets) {
      if (widget.kind === "chart") {
        expect(widgetOf(formOf(widget), widget)).toEqual(widget);
      }
    }
  });

  it("writes only the options of the chart type", () => {
    const base: Widget = { ...widgetNamed("Requests by model") };
    const form = { ...formOf(base), type: "line" as const, stacked: true, fadeBelow: "4" };
    const widget = widgetOf(form, base);
    expect(widget.chart).toEqual({ type: "line" });

    const bar = widgetOf(
      { ...form, type: "bar", stacked: true, split: true, fadeBelow: "4", share: true },
      base,
    );
    expect(bar.chart).toEqual({ type: "bar", stacked: true, fade_below: 4 });
  });

  it("writes a map key typed for count_distinct as the field", () => {
    const base: Widget = {
      id: "w-1",
      title: "",
      kind: "chart",
      layout: { x: 0, y: 0, w: 6, h: 7 },
    };
    const form = {
      ...blankForm(),
      measures: [
        {
          rowId: "r",
          agg: "count_distinct" as const,
          field: "trace_id",
          mapKey: "metadata['tier']",
          where: "",
          label: "",
        },
      ],
    };
    expect(widgetOf(form, base).query?.measures).toEqual([
      { agg: "count_distinct", field: "metadata['tier']" },
    ]);
  });

  it("sends a number that does not parse as 0, which the rules refuse", () => {
    const base: Widget = {
      id: "w-1",
      title: "",
      kind: "chart",
      layout: { x: 0, y: 0, w: 6, h: 7 },
    };
    const form = { ...blankForm(), breakdown: true, breakdownField: "model", limit: "x" };
    expect(widgetOf(form, base).query?.breakdown).toEqual({ field: "model", limit: 0 });
  });

  it("keeps the layout and id of the base widget", () => {
    const base = widgetNamed("Error rate");
    expect(widgetOf(formOf(base), base).layout).toEqual(base.layout);
    expect(widgetOf(formOf(base), base).id).toBe(base.id);
  });
});

describe("withSource", () => {
  it("drops a breakdown field the new source does not have", () => {
    const form = { ...blankForm(), breakdown: true, breakdownField: "model" };
    expect(withSource(form, "scores").breakdownField).toBe("");
    expect(withSource(form, "spans").breakdownField).toBe("model");
  });

  it("moves a measure field the new source lacks to its first field", () => {
    const form = {
      ...blankForm(),
      measures: [
        { rowId: "r", agg: "p95" as const, field: "cost_total", mapKey: "", where: "", label: "" },
      ],
    };
    expect(withSource(form, "scores").measures[0]?.field).toBe("value_num");
    expect(withSource(form, "traces").measures[0]?.field).toBe("cost_total");
  });

  it("clears the filter and each measure's condition, which were written over the old source", () => {
    const form = {
      ...blankForm(),
      filter: "status = 'error'",
      measures: [
        {
          rowId: "r",
          agg: "rate" as const,
          field: "",
          mapKey: "",
          where: "error_count > 0",
          label: "",
        },
      ],
    };
    const next = withSource(form, "spans");
    expect(next.filter).toBe("");
    expect(next.measures[0]?.where).toBe("");
  });

  it("clears a map key whose column the new source does not have", () => {
    const form = {
      ...blankForm(),
      measures: [
        {
          rowId: "r",
          agg: "count_distinct" as const,
          field: "trace_id",
          mapKey: "attributes['tier']",
          where: "",
          label: "",
        },
      ],
      breakdownMapKey: "attributes['tier']",
    };
    const toSpans = withSource(form, "spans");
    expect(toSpans.measures[0]?.mapKey).toBe("attributes['tier']");
    expect(toSpans.breakdownMapKey).toBe("attributes['tier']");
    const toTraces = withSource(form, "traces");
    expect(toTraces.measures[0]?.mapKey).toBe("");
    expect(toTraces.breakdownMapKey).toBe("");
  });
});
