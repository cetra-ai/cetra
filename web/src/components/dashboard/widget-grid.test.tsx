import { act, render, screen, within } from "@testing-library/react";
import { useContext } from "react";
import type { GridLayoutProps } from "react-grid-layout";
import { afterEach, describe, expect, it, vi } from "vitest";
import { LayoutKey } from "@/components/charts/layout-key";
import type { Widget as WidgetModel } from "@/lib/dashboards/types";
import {
  CELL_BODY,
  compactWidgets,
  layoutItems,
  nudgeWidgets,
  SETTLE_MS,
  SETTLING,
  WidgetGrid,
} from "./widget-grid";

/**
 * The props the grid last rendered react-grid-layout with; the grid itself is the real one. The
 * container's measured width is `grid.width` (jsdom lays nothing out).
 */
const grid = vi.hoisted(() => ({
  last: undefined as GridLayoutProps | undefined,
  width: 1248,
}));

vi.mock("react-grid-layout", async (importOriginal) => {
  const actual = await importOriginal<typeof import("react-grid-layout")>();
  function CapturedGridLayout(props: GridLayoutProps) {
    grid.last = props;
    return <actual.GridLayout {...props} />;
  }
  return { ...actual, GridLayout: CapturedGridLayout };
});

// jsdom lays nothing out: the container's measured width is `grid.width`.
vi.mock("./grid-width", async () => {
  const { useRef } = await import("react");
  return {
    useGridWidth: () => ({ width: grid.width, containerRef: useRef<HTMLDivElement>(null) }),
  };
});

/** Records the layout key each cell gives its content. */
function KeyProbe({ id, keys }: { id: string; keys: Map<string, string> }) {
  keys.set(id, useContext(LayoutKey));
  return <span>{id}</span>;
}

function widget(id: string, x: number, y: number, w: number, h: number): WidgetModel {
  return {
    id,
    title: id,
    kind: "heading",
    layout: { x, y, w, h },
  };
}

const widgets = [
  widget("wide", 0, 0, 60, 1),
  widget("tall", 0, 1, 30, 3),
  widget("side", 30, 1, 30, 3),
];

function setViewportWidth(width: number) {
  Object.defineProperty(window, "innerWidth", { configurable: true, writable: true, value: width });
}

afterEach(() => {
  setViewportWidth(1024);
  grid.width = 1248;
  vi.useRealTimers();
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

describe("the grid's drag", () => {
  it("lets a dragged widget follow the pointer past the grid's edges", () => {
    render(<WidgetGrid widgets={widgets} editing renderCell={(item) => <span>{item.id}</span>} />);
    expect(grid.last?.dragConfig).toMatchObject({ enabled: true, bounded: false });
  });

  it("renders new cells on every drag and resize move, so the item follows the pointer", () => {
    render(<WidgetGrid widgets={widgets} editing renderCell={(item) => <span>{item.id}</span>} />);
    const layout = layoutItems(widgets);
    const tall = layout.find((item) => item.i === "tall") ?? null;
    const before = grid.last?.children;
    act(() => grid.last?.onDrag?.(layout, tall, tall, tall, new Event("mousemove"), null));
    const afterDrag = grid.last?.children;
    expect(afterDrag).not.toBe(before);
    act(() => grid.last?.onResize?.(layout, tall, tall, tall, new Event("mousemove"), null));
    expect(grid.last?.children).not.toBe(afterDrag);
  });
});

describe("the grid's finished moves", () => {
  it("forwards onDragStop and onResizeStop to react-grid-layout", () => {
    const onDragStop = vi.fn();
    const onResizeStop = vi.fn();
    render(
      <WidgetGrid
        widgets={widgets}
        editing
        onDragStop={onDragStop}
        onResizeStop={onResizeStop}
        renderCell={(item) => <span>{item.id}</span>}
      />,
    );
    const layout = layoutItems(widgets);
    act(() => grid.last?.onDragStop?.(layout, null, null, null, new Event("mouseup"), null));
    expect(onDragStop).toHaveBeenCalledWith(layout);
    act(() => grid.last?.onResizeStop?.(layout, null, null, null, new Event("mouseup"), null));
    expect(onResizeStop).toHaveBeenCalledWith(layout);
  });

  it("keeps a resized cell's content at its starting size until the resize ends", () => {
    const { container } = render(
      <WidgetGrid widgets={widgets} editing renderCell={(item) => <span>{item.id}</span>} />,
    );
    const body = (id: string) => screen.getByText(id).closest<HTMLElement>(`.${CELL_BODY}`);
    expect(body("tall")?.style.width).toBe("100%");
    const element = cells(container)[1];
    if (!element) throw new Error("the tall cell is missing");
    Object.defineProperty(element, "offsetWidth", { configurable: true, value: 480 });
    Object.defineProperty(element, "offsetHeight", { configurable: true, value: 144 });
    const layout = layoutItems(widgets);
    const tall = layout.find((item) => item.i === "tall") ?? null;
    const start = new Event("mousedown");

    act(() => grid.last?.onResizeStart?.(layout, tall, tall, null, start, element));
    expect(body("tall")?.style.width).toBe("480px");
    expect(body("tall")?.style.height).toBe("144px");
    // The others follow their cells.
    expect(body("side")?.style.width).toBe("100%");

    act(() => grid.last?.onResizeStop?.(layout, tall, tall, null, new Event("mouseup"), element));
    expect(body("tall")?.style.width).toBe("100%");
    expect(body("tall")?.style.height).toBe("100%");
  });
});

describe("a change of the grid's width", () => {
  const FRAME = 16;
  /** Every cell is on screen, except those named: jsdom lays nothing out. */
  const placeCells = (below: string[] = []) =>
    vi.spyOn(HTMLElement.prototype, "getBoundingClientRect").mockImplementation(function (
      this: HTMLElement,
    ) {
      const id = this.dataset.widgetId;
      const top = id !== undefined && below.includes(id) ? 5000 : 100;
      return new DOMRect(0, top, 100, 100);
    });
  afterEach(() => {
    vi.restoreAllMocks();
  });
  const fakeFrames = () =>
    vi.useFakeTimers({
      toFake: ["setTimeout", "clearTimeout", "requestAnimationFrame", "cancelAnimationFrame"],
    });
  const view = () => (
    <WidgetGrid widgets={widgets} editing renderCell={(item) => <span>{item.id}</span>} />
  );
  const body = (id: string) => screen.getByText(id).closest<HTMLElement>(`.${CELL_BODY}`);

  // 1248 px: 60 columns of 9 px with 59 gaps of 12 px. A 30-column cell is 618 px wide.
  it("stretches the drawn charts to their cells while the width changes", () => {
    fakeFrames();
    placeCells();
    const { rerender } = render(view());
    expect(body("tall")?.style.width).toBe("100%");

    // 1308 px: columns of 10 px, so a 30-column cell is 648 px wide.
    grid.width = 1308;
    rerender(view());
    expect(body("tall")?.style.width).toBe("618px");
    const stretch = Number(body("tall")?.style.transform.match(/^scaleX\(([\d.]+)\)$/)?.[1]);
    expect(stretch).toBeCloseTo(648 / 618, 4);
    act(() => vi.advanceTimersByTime(SETTLE_MS - 1));
    expect(body("tall")?.style.width).toBe("618px");
  });

  it("then redraws them one per frame in reading order", () => {
    fakeFrames();
    placeCells();
    const { rerender } = render(view());
    grid.width = 1308;
    rerender(view());

    // The first, the full-width heading, at once; then one more per frame.
    act(() => vi.advanceTimersByTime(SETTLE_MS));
    expect(body("wide")?.style.width).toBe("100%");
    expect(body("tall")?.style.width).toBe("618px");
    expect(body("side")?.style.width).toBe("618px");
    act(() => vi.advanceTimersByTime(FRAME));
    expect(body("tall")?.style.width).toBe("100%");
    expect(body("tall")?.style.transform).toBe("");
    expect(body("side")?.style.width).toBe("618px");
    act(() => vi.advanceTimersByTime(FRAME));
    expect(body("side")?.style.width).toBe("100%");
  });

  it("waits for the last of several quick changes", () => {
    fakeFrames();
    placeCells();
    const { rerender } = render(view());
    for (const width of [1270, 1290, 1308]) {
      grid.width = width;
      rerender(view());
      act(() => vi.advanceTimersByTime(SETTLE_MS - 10));
    }
    expect(body("wide")?.style.width).not.toBe("100%");
    act(() => vi.advanceTimersByTime(10 + 2 * FRAME));
    expect(body("side")?.style.width).toBe("100%");
  });

  it("redraws the charts below the fold after those on screen, when the browser is idle", () => {
    fakeFrames();
    placeCells(["tall"]);
    const { rerender } = render(view());
    grid.width = 1308;
    rerender(view());

    act(() => vi.advanceTimersByTime(SETTLE_MS));
    expect(body("wide")?.style.width).toBe("100%");
    act(() => vi.advanceTimersByTime(FRAME));
    // "side" is on screen and goes before "tall", which is not.
    expect(body("side")?.style.width).toBe("100%");
    expect(body("tall")?.style.width).toBe("618px");
    // jsdom has no requestIdleCallback: idle falls back to a short delay.
    act(() => vi.advanceTimersByTime(50));
    expect(body("tall")?.style.width).toBe("100%");
  });

  it("gives a cell a new layout key only in the render that redraws it", () => {
    fakeFrames();
    placeCells();
    const keys = new Map<string, string>();
    const keyed = () => (
      <WidgetGrid
        widgets={widgets}
        editing
        renderCell={(item) => <KeyProbe id={item.id} keys={keys} />}
      />
    );
    const { rerender } = render(keyed());
    const first = keys.get("tall");
    grid.width = 1308;
    rerender(keyed());
    // Stretched, not redrawn: the charts keep their size, so their key stays.
    expect(keys.get("tall")).toBe(first);
    act(() => vi.advanceTimersByTime(SETTLE_MS + FRAME));
    expect(keys.get("tall")).not.toBe(first);
  });

  it("stops redrawing at a width that changed again, and starts over once it settles", () => {
    fakeFrames();
    placeCells();
    const { rerender } = render(view());
    grid.width = 1308;
    rerender(view());
    act(() => vi.advanceTimersByTime(SETTLE_MS));
    expect(body("wide")?.style.width).toBe("100%");

    grid.width = 1248;
    rerender(view());
    // "wide" was drawn at 1308 and is stretched back; "tall" was never redrawn: it fits again.
    expect(body("wide")?.style.width).not.toBe("100%");
    expect(body("tall")?.style.width).toBe("100%");
    act(() => vi.advanceTimersByTime(4 * FRAME));
    expect(body("wide")?.style.width).not.toBe("100%");
    act(() => vi.advanceTimersByTime(SETTLE_MS));
    expect(body("wide")?.style.width).toBe("100%");
  });
});

describe("the grid after a move", () => {
  it("lets the widgets slide into place for a moment after a drop or a resize", () => {
    vi.useFakeTimers();
    const { container } = render(
      <WidgetGrid widgets={widgets} editing renderCell={(item) => <span>{item.id}</span>} />,
    );
    const settling = () => container.querySelector(`.${SETTLING}`) !== null;
    expect(settling()).toBe(false);
    const layout = layoutItems(widgets);
    act(() => grid.last?.onDragStop?.(layout, null, null, null, new Event("mouseup"), null));
    expect(settling()).toBe(true);
    act(() => vi.advanceTimersByTime(250));
    expect(settling()).toBe(false);
    act(() => grid.last?.onResizeStop?.(layout, null, null, null, new Event("mouseup"), null));
    expect(settling()).toBe(true);
  });
});

describe("the grid's layout", () => {
  it("limits every item to the sizes the server accepts", () => {
    for (const item of layoutItems(widgets)) {
      expect(item).toMatchObject({ minW: 1, maxW: 60, minH: 1, maxH: 40 });
    }
  });

  it("packs widgets upwards, as the grid does when it is shown", () => {
    const gapped = [widget("top", 0, 0, 60, 1), widget("low", 0, 4, 30, 3)];
    expect(compactWidgets(gapped).map((item) => item.layout.y)).toEqual([0, 1]);
  });
});

describe("a menu step", () => {
  it("resizes the widget and packs the rest", () => {
    const next = nudgeWidgets(widgets, 1, "taller");
    expect(next[1]?.layout).toEqual({ x: 0, y: 1, w: 30, h: 4 });
    expect(next[0]).toBe(widgets[0]);
  });

  it("moves a widget into the place of another, which moves down", () => {
    const next = nudgeWidgets(widgets, 1, "right");
    expect(next[1]?.layout).toEqual({ x: 5, y: 1, w: 30, h: 3 });
    expect(next[2]?.layout.y).toBeGreaterThan(1);
  });

  it("returns the same widgets when the step is not allowed", () => {
    expect(nudgeWidgets(widgets, 0, "up")).toBe(widgets);
  });
});
