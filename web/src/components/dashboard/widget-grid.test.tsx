import { render, screen, within } from "@testing-library/react";
import { afterEach, describe, expect, it } from "vitest";
import type { Widget as WidgetModel } from "@/lib/dashboards/types";
import { compactWidgets, layoutItems, nudgeWidgets, WidgetGrid } from "./widget-grid";

function widget(id: string, x: number, y: number, w: number, h: number): WidgetModel {
  return {
    id,
    title: id,
    kind: "heading",
    layout: { x, y, w, h },
  };
}

const widgets = [
  widget("wide", 0, 0, 12, 1),
  widget("tall", 0, 1, 6, 3),
  widget("side", 6, 1, 6, 3),
];

function setViewportWidth(width: number) {
  Object.defineProperty(window, "innerWidth", { configurable: true, writable: true, value: width });
}

afterEach(() => {
  setViewportWidth(1024);
});

function cells(container: HTMLElement) {
  return Array.from(container.querySelectorAll<HTMLElement>(".react-grid-item"));
}

describe("the grid", () => {
  it("is static outside edit mode", () => {
    const { container } = render(
      <WidgetGrid
        widgets={widgets}
        editing={false}
        renderCell={(item) => <span>{item.id}</span>}
      />,
    );
    expect(cells(container)).toHaveLength(3);
    // The library keeps the handles and hides them, and turns dragging off.
    const handles = container.querySelectorAll(".react-resizable-handle");
    expect(handles).toHaveLength(3);
    expect(container.querySelectorAll(".react-resizable-hide")).toHaveLength(3);
  });

  it("offers resize handles in edit mode", () => {
    const { container } = render(
      <WidgetGrid widgets={widgets} editing renderCell={(item) => <span>{item.id}</span>} />,
    );
    expect(container.querySelectorAll(".react-resizable-handle")).toHaveLength(3);
    expect(container.querySelectorAll(".react-resizable-hide")).toHaveLength(0);
  });

  it("gives a widget the height of its rows", () => {
    const { container } = render(
      <WidgetGrid
        widgets={widgets}
        editing={false}
        renderCell={(item) => <span>{item.id}</span>}
      />,
    );
    const tall = cells(container).find((cell) => within(cell).queryByText("tall"));
    // Three rows of 40 px and two gaps of 12 px.
    expect(tall?.style.height).toBe("144px");
  });
});

describe("below 768 px", () => {
  it("stacks the widgets in one column, in (y, x) order, and cannot be edited", () => {
    setViewportWidth(500);
    const { container } = render(
      <WidgetGrid
        widgets={[widgets[2] as WidgetModel, widgets[0] as WidgetModel, widgets[1] as WidgetModel]}
        editing
        renderCell={(item) => <span>{item.id}</span>}
      />,
    );
    expect(container.querySelector(".react-grid-layout")).toBeNull();
    expect(container.querySelectorAll(".react-resizable-handle")).toHaveLength(0);
    const order = Array.from(container.querySelectorAll("span")).map((node) => node.textContent);
    expect(order).toEqual(["wide", "tall", "side"]);
    expect(screen.getByText("side")).toBeInTheDocument();
  });
});

describe("charts below the fold", () => {
  const original = globalThis.IntersectionObserver;

  afterEach(() => {
    globalThis.IntersectionObserver = original;
  });

  /** Reports nothing as visible: a chart mounts only when a test says it is near the viewport. */
  function offscreen() {
    globalThis.IntersectionObserver = class {
      observe() {}
      unobserve() {}
      disconnect() {}
      takeRecords() {
        return [];
      }
    } as unknown as typeof IntersectionObserver;
  }

  function chart(id: string, x: number, y: number): WidgetModel {
    return {
      id,
      title: id,
      kind: "chart",
      chart: { type: "line" },
      query: undefined,
      layout: { x, y, w: 6, h: 7 },
    } as WidgetModel;
  }

  it("mounts the first four charts in reading order at once, and the rest when scrolled near", () => {
    offscreen();
    const charts = [
      chart("c6", 6, 14),
      chart("c5", 0, 14),
      chart("c4", 6, 7),
      chart("c3", 0, 7),
      chart("c2", 6, 0),
      chart("c1", 0, 0),
    ];
    render(
      <WidgetGrid
        widgets={[widget("h", 0, 21, 12, 1), ...charts]}
        editing={false}
        renderCell={(item) => <span>{item.id}</span>}
      />,
    );
    for (const id of ["c1", "c2", "c3", "c4"]) {
      expect(screen.getByText(id)).toBeInTheDocument();
    }
    for (const id of ["c5", "c6"]) {
      expect(screen.queryByText(id)).not.toBeInTheDocument();
    }
    // A heading costs no query, so it mounts at once wherever it is.
    expect(screen.getByText("h")).toBeInTheDocument();
  });
});

describe("the grid's layout", () => {
  it("limits every item to the sizes the server accepts", () => {
    for (const item of layoutItems(widgets)) {
      expect(item).toMatchObject({ minW: 1, maxW: 12, minH: 1, maxH: 40 });
    }
  });

  it("packs widgets upwards, as the grid does when it is shown", () => {
    const gapped = [widget("top", 0, 0, 12, 1), widget("low", 0, 4, 6, 3)];
    expect(compactWidgets(gapped).map((item) => item.layout.y)).toEqual([0, 1]);
  });
});

describe("a menu step", () => {
  it("resizes the widget and packs the rest", () => {
    const next = nudgeWidgets(widgets, 1, "taller");
    expect(next[1]?.layout).toEqual({ x: 0, y: 1, w: 6, h: 4 });
    expect(next[0]).toBe(widgets[0]);
  });

  it("moves a widget into the place of another, which moves down", () => {
    const next = nudgeWidgets(widgets, 1, "right");
    expect(next[1]?.layout).toEqual({ x: 1, y: 1, w: 6, h: 3 });
    expect(next[2]?.layout.y).toBeGreaterThan(1);
  });

  it("returns the same widgets when the step is not allowed", () => {
    expect(nudgeWidgets(widgets, 0, "up")).toBe(widgets);
  });
});
