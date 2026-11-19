import "react-grid-layout/css/styles.css";
import {
  type CSSProperties,
  type ReactNode,
  useEffect,
  useMemo,
  useReducer,
  useRef,
  useState,
} from "react";
import {
  type EventCallback,
  GridLayout,
  type Layout,
  moveElement,
  verticalCompactor,
} from "react-grid-layout";
import { LayoutKey } from "@/components/charts/layout-key";
import { LazyMount } from "@/components/charts/lazy-mount";
import { Skeleton } from "@/components/ui/skeleton";
import { useIsMobile } from "@/hooks/use-mobile";
import { COLUMNS, MAX_HEIGHT, type Nudge, nudgedLayout, withLayouts } from "@/lib/dashboards/draft";
import type { Widget as WidgetModel } from "@/lib/dashboards/types";
import { useGridWidth } from "./grid-width";
import { DRAG_HANDLE } from "./widget";

/** Height of one row in pixels. */
const ROW_HEIGHT = 40;
/** The gap between widgets, in pixels, both ways. */
const GAP = 12;
/** The charts drawn at once, in reading order; the rest mount when scrolled near. */
const EAGER_CHARTS = 4;

const GRID_CONFIG = {
  cols: COLUMNS,
  rowHeight: ROW_HEIGHT,
  margin: [GAP, GAP],
  containerPadding: [0, 0],
} as const;

/** The height in pixels of a widget `rows` rows tall: its cell, gaps included. */
function cellHeight(rows: number): number {
  return rows * ROW_HEIGHT + (rows - 1) * GAP;
}

/** How long the width must hold still before the charts start to redraw at it, in milliseconds. */
export const SETTLE_MS = 150;
/** How long the widgets slide into place after a drop or a resize, in milliseconds. */
const SETTLING_MS = 250;
/** The class of the grid while the widgets slide into place after a drop or a resize. */
export const SETTLING = "dashboard-grid-settling";

/** The width in pixels of a widget `columns` wide on a grid `gridWidth` wide (react-grid-layout's). */
function cellWidth(gridWidth: number, columns: number): number {
  const column = (gridWidth - GAP * (COLUMNS - 1)) / COLUMNS;
  return Math.round(column * columns + Math.max(0, columns - 1) * GAP);
}

/**
 * The grid width each widget's chart is drawn at. After the measured width holds still for
 * `SETTLE_MS`, the widgets drawn at another width are redrawn one at a time in reading order: those
 * on screen one per frame, then those below the fold whenever the browser is idle. Redrawing every
 * chart at once froze the page for a fifth of a second after each step of a window resize or a
 * zoom. A new change of width cancels the rest of the queue and starts it again once the width
 * settles. The first measurement applies to all at once; a widget added later starts at the current
 * width.
 */
function useDrawnWidths(
  widgets: readonly WidgetModel[],
  width: number,
  measured: boolean,
  container: { readonly current: HTMLElement | null },
): (id: string) => number | null {
  const [drawn, setDrawn] = useState<{ base: number | null; byId: Record<string, number> }>({
    base: null,
    byId: {},
  });
  const current = useRef(drawn);
  current.current = drawn;
  useEffect(() => {
    if (!measured) return;
    if (current.current.base === null) {
      setDrawn({ base: width, byId: {} });
      return;
    }
    const at = (id: string) => current.current.byId[id] ?? current.current.base;
    const stale = widgets.filter((widget) => at(widget.id) !== width);
    if (stale.length === 0) return;
    let cancel: (() => void) | undefined;
    const timer = setTimeout(() => {
      const { seen, unseen } = redrawOrder(stale, container.current);
      const redraw = (id: string) =>
        setDrawn((prev) => ({ base: prev.base, byId: { ...prev.byId, [id]: width } }));
      // On screen: one per frame. Below the fold: one each time the browser is idle.
      const next = () => {
        const id = seen.shift();
        if (id !== undefined) {
          redraw(id);
          const frame = requestAnimationFrame(next);
          cancel = () => cancelAnimationFrame(frame);
          return;
        }
        const later = unseen.shift();
        if (later === undefined) return;
        redraw(later);
        cancel = whenIdle(next);
      };
      next();
    }, SETTLE_MS);
    return () => {
      clearTimeout(timer);
      cancel?.();
    };
  }, [widgets, width, measured, container]);
  return (id) => drawn.byId[id] ?? drawn.base;
}

/** Runs `task` when the browser is idle (after a short delay where it cannot tell); the result cancels it. */
function whenIdle(task: () => void): () => void {
  if (typeof window.requestIdleCallback === "function") {
    const handle = window.requestIdleCallback(task, { timeout: 1000 });
    return () => window.cancelIdleCallback(handle);
  }
  const timer = setTimeout(task, 50);
  return () => clearTimeout(timer);
}

/** The widgets' ids in redraw order, those on screen and the rest, each by `(y, x)`. */
function redrawOrder(
  widgets: readonly WidgetModel[],
  container: HTMLElement | null,
): { seen: string[]; unseen: string[] } {
  const onScreen = (id: string) => {
    const cell = container?.querySelector<HTMLElement>(`[data-widget-id="${CSS.escape(id)}"]`);
    if (!cell) return false;
    const box = cell.getBoundingClientRect();
    return box.bottom > 0 && box.top < window.innerHeight;
  };
  const ordered = [...widgets].sort(
    (left, right) => left.layout.y - right.layout.y || left.layout.x - right.layout.x,
  );
  return {
    seen: ordered.filter((widget) => onScreen(widget.id)).map((widget) => widget.id),
    unseen: ordered.filter((widget) => !onScreen(widget.id)).map((widget) => widget.id),
  };
}

/** The class of a cell's body: the grid's styles lift it while it is dragged. */
export const CELL_BODY = "dashboard-cell";

/** A cell being resized, and its body's size in pixels when the resize began. */
type Frozen = { id: string; width: number; height: number };

/**
 * Widgets are dragged by their title bar; a control inside it keeps its own clicks. A dragged
 * widget follows the pointer anywhere, even past the grid's edges; where it lands stays on the grid.
 */
const DRAG_CONFIG = {
  bounded: false,
  handle: `.${DRAG_HANDLE}`,
  cancel: "button, a, input",
} as const;

/**
 * The grid's items: a widget's position and size, with the limits of spec 04 § Dashboards so that
 * a drag or a resize cannot produce a layout the server refuses.
 */
export function layoutItems(widgets: readonly WidgetModel[]): Layout {
  return widgets.map((widget) => ({
    i: widget.id,
    x: widget.layout.x,
    y: widget.layout.y,
    w: widget.layout.w,
    h: widget.layout.h,
    minW: 1,
    maxW: COLUMNS,
    minH: 1,
    maxH: MAX_HEIGHT,
  }));
}

/** The widgets packed upwards, as the grid packs them when it is shown in edit mode. */
export function compactWidgets(widgets: WidgetModel[]): WidgetModel[] {
  return withLayouts(widgets, verticalCompactor.compact(layoutItems(widgets), COLUMNS));
}

/**
 * The widgets after one menu step on the widget at `index`: a move is placed the way a drag drops
 * it (the widgets in its way move down), a resize sets its size, and the result is packed upwards.
 * Returns `widgets` itself when the step is not allowed.
 */
export function nudgeWidgets(widgets: WidgetModel[], index: number, step: Nudge): WidgetModel[] {
  const widget = widgets[index];
  const next = widget ? nudgedLayout(widget.layout, step) : undefined;
  const items = layoutItems(widgets);
  const item = items[index];
  if (next === undefined || item === undefined) {
    return widgets;
  }
  if (step === "up" || step === "down" || step === "left" || step === "right") {
    const placed = moveElement(items, item, next.x, next.y, true, false, "vertical", COLUMNS);
    return withLayouts(widgets, verticalCompactor.compact(placed, COLUMNS));
  }
  item.w = next.w;
  item.h = next.h;
  return withLayouts(widgets, verticalCompactor.compact(items, COLUMNS));
}

/**
 * The widgets on the 12-column grid. Outside edit mode they are static. In edit mode they can be
 * dragged and resized, and `onLayoutChange` reports the new positions. `onDragStop` and
 * `onResizeStop` report a finished move with its final layout. A cell being resized keeps its
 * content at its starting size until the resize ends. Below 768 px they stack in one
 * column in `(y, x)` order, and nothing can be moved.
 */
export function WidgetGrid({
  widgets,
  editing,
  onLayoutChange,
  onDragStop,
  onResizeStop,
  renderCell,
}: {
  widgets: readonly WidgetModel[];
  editing: boolean;
  onLayoutChange?: (layout: Layout) => void;
  /** A finished drag, with the final layout. */
  onDragStop?: (layout: Layout) => void;
  /** A finished resize, with the final layout. */
  onResizeStop?: (layout: Layout) => void;
  /** The content of one widget's cell. */
  renderCell: (widget: WidgetModel) => ReactNode;
}) {
  const narrow = useIsMobile();
  if (narrow) {
    return <StackedWidgets widgets={widgets} renderCell={renderCell} />;
  }
  return (
    <FreeWidgets
      widgets={widgets}
      editing={editing}
      onLayoutChange={onLayoutChange}
      onDragStop={onDragStop}
      onResizeStop={onResizeStop}
      renderCell={renderCell}
    />
  );
}

function FreeWidgets({
  widgets,
  editing,
  onLayoutChange,
  onDragStop,
  onResizeStop,
  renderCell,
}: {
  widgets: readonly WidgetModel[];
  editing: boolean;
  onLayoutChange?: (layout: Layout) => void;
  onDragStop?: (layout: Layout) => void;
  onResizeStop?: (layout: Layout) => void;
  renderCell: (widget: WidgetModel) => ReactNode;
}) {
  const { width, containerRef } = useGridWidth();
  const mounted = width !== null;
  const drawnAt = useDrawnWidths(widgets, width ?? 0, mounted, containerRef);
  // After a drop or a resize the widgets slide into place; a width change moves them at once.
  const [settling, setSettling] = useState(false);
  useEffect(() => {
    if (!settling) return;
    const timer = setTimeout(() => setSettling(false), SETTLING_MS);
    return () => clearTimeout(timer);
  }, [settling]);
  const layout = useMemo(() => layoutItems(widgets), [widgets]);
  const dragConfig = useMemo(() => ({ ...DRAG_CONFIG, enabled: editing }), [editing]);
  const resizeConfig = useMemo(() => ({ enabled: editing }), [editing]);
  const eager = useMemo(() => eagerCharts(widgets), [widgets]);
  // While a cell is resized its body keeps the size it had, clipped by the cell, so its chart does
  // not redraw on every pointer move; it redraws once, at its new size, when the resize ends.
  const [frozen, setFrozen] = useState<Frozen | null>(null);
  // react-grid-layout 2.3.0 keeps a dragged or resized item's pixel position in a ref, and its
  // memoized item re-renders only when its cell or its children change: alone, the item would jump
  // from cell to cell. New children on every move make it follow the pointer pixel by pixel; the
  // widgets themselves are memoized, so their charts do not redraw.
  const [, followPointer] = useReducer((moves: number) => moves + 1, 0);
  const onResizeStart: EventCallback = (_layout, item, _new, _placeholder, _event, element) => {
    if (item && element) {
      setFrozen({ id: item.i, width: element.offsetWidth, height: element.offsetHeight });
    }
  };
  const endResize = (layout: Layout) => {
    setFrozen(null);
    setSettling(true);
    onResizeStop?.(layout);
  };
  const endDrag = (layout: Layout) => {
    setSettling(true);
    onDragStop?.(layout);
  };
  /**
   * A cell body's style. A resized cell keeps its starting size. While the grid's width changes,
   * every body keeps the width it was drawn at and is stretched across to its cell, so no chart
   * measures a new size until the width settles.
   */
  const bodyStyle = (widget: WidgetModel): CSSProperties => {
    if (frozen?.id === widget.id) return { width: frozen.width, height: frozen.height };
    const drawnWidth = drawnAt(widget.id);
    if (width !== null && drawnWidth !== null && drawnWidth !== width) {
      const drawn = cellWidth(drawnWidth, widget.layout.w);
      return {
        width: drawn,
        height: "100%",
        transform: `scaleX(${cellWidth(width, widget.layout.w) / drawn})`,
        transformOrigin: "0 0",
      };
    }
    return { width: "100%", height: "100%" };
  };
  /** The cell's layout key: the width its charts are drawn at, and whether its own resize holds it. */
  const layoutKeyOf = (widget: WidgetModel): string =>
    `${drawnAt(widget.id) ?? "unmeasured"}:${frozen?.id === widget.id ? "held" : "free"}`;
  return (
    <div ref={containerRef} className={settling ? SETTLING : undefined}>
      {width === null ? null : (
        <GridLayout
          width={width}
          layout={layout}
          gridConfig={GRID_CONFIG}
          dragConfig={dragConfig}
          resizeConfig={resizeConfig}
          onLayoutChange={onLayoutChange}
          onDrag={() => followPointer()}
          onDragStop={endDrag}
          onResize={() => followPointer()}
          onResizeStart={onResizeStart}
          onResizeStop={endResize}
        >
          {widgets.map((widget) => (
            <div key={widget.id} data-widget-id={widget.id} className="relative h-full min-w-0">
              <div className={CELL_BODY} style={bodyStyle(widget)}>
                {/* A new key in the render that resizes the body: its charts measure it then. */}
                <LayoutKey.Provider value={layoutKeyOf(widget)}>
                  {widget.kind === "chart" ? (
                    // A chart fetches its answer when it is near the viewport, so a long dashboard does
                    // not query every widget at once. Its placeholder is its cell's height.
                    <LazyMount
                      eager={eager.has(widget.id)}
                      minHeight={cellHeight(widget.layout.h)}
                      fallback={
                        <Skeleton
                          className="h-full w-full"
                          style={{ minHeight: cellHeight(widget.layout.h) }}
                        />
                      }
                    >
                      {renderCell(widget)}
                    </LazyMount>
                  ) : (
                    renderCell(widget)
                  )}
                </LayoutKey.Provider>
              </div>
            </div>
          ))}
        </GridLayout>
      )}
    </div>
  );
}

/** The ids of the first charts in reading order (by `y`, then `x`): these mount at once. */
function eagerCharts(widgets: readonly WidgetModel[]): Set<string> {
  const charts = widgets
    .filter((widget) => widget.kind === "chart")
    .sort((left, right) => left.layout.y - right.layout.y || left.layout.x - right.layout.x);
  return new Set(charts.slice(0, EAGER_CHARTS).map((widget) => widget.id));
}

function StackedWidgets({
  widgets,
  renderCell,
}: {
  widgets: readonly WidgetModel[];
  renderCell: (widget: WidgetModel) => ReactNode;
}) {
  const ordered = [...widgets].sort(
    (left, right) => left.layout.y - right.layout.y || left.layout.x - right.layout.x,
  );
  return (
    <div className="grid grid-cols-1 gap-3">
      {ordered.map((widget) => (
        <div key={widget.id} className="relative min-w-0">
          {renderCell(widget)}
        </div>
      ))}
    </div>
  );
}
