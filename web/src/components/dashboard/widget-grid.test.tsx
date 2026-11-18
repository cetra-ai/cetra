import { render, screen, within } from "@testing-library/react";
import { afterEach, describe, expect, it } from "vitest";
import type { Widget as WidgetModel } from "@/lib/dashboards/types";
import { WidgetGrid } from "./widget-grid";

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
