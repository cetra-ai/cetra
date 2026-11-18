import "react-grid-layout/css/styles.css";
import { type ReactNode, useMemo } from "react";
import {
  GridLayout,
  type Layout,
  moveElement,
  useContainerWidth,
  verticalCompactor,
} from "react-grid-layout";
import { LazyMount } from "@/components/charts/lazy-mount";
import { Skeleton } from "@/components/ui/skeleton";
import { useIsMobile } from "@/hooks/use-mobile";
import { COLUMNS, MAX_HEIGHT, type Nudge, nudgedLayout, withLayouts } from "@/lib/dashboards/draft";
import type { Widget as WidgetModel } from "@/lib/dashboards/types";
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

/** Widgets are dragged by their title bar; a control inside it keeps its own clicks. */
const DRAG_CONFIG = {
  bounded: true,
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
 * `onResizeStop` report a finished move with its final layout. Below 768 px they stack in one
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
  const { width, containerRef } = useContainerWidth();
  const layout = useMemo(() => layoutItems(widgets), [widgets]);
  const dragConfig = useMemo(() => ({ ...DRAG_CONFIG, enabled: editing }), [editing]);
  const resizeConfig = useMemo(() => ({ enabled: editing }), [editing]);
  const eager = useMemo(() => eagerCharts(widgets), [widgets]);
  return (
    <div ref={containerRef}>
      <GridLayout
        width={width}
        layout={layout}
        gridConfig={GRID_CONFIG}
        dragConfig={dragConfig}
        resizeConfig={resizeConfig}
        onLayoutChange={onLayoutChange}
        onDragStop={onDragStop}
        onResizeStop={onResizeStop}
      >
        {widgets.map((widget) => (
          <div key={widget.id} className="relative h-full min-w-0">
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
          </div>
        ))}
      </GridLayout>
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
