import "react-grid-layout/css/styles.css";
import { type ReactNode, useMemo } from "react";
import { GridLayout, type Layout, useContainerWidth } from "react-grid-layout";
import { LazyMount } from "@/components/charts/lazy-mount";
import { Skeleton } from "@/components/ui/skeleton";
import { useIsMobile } from "@/hooks/use-mobile";
import { COLUMNS } from "@/lib/dashboards/draft";
import type { Widget as WidgetModel } from "@/lib/dashboards/types";

/** Height of one row in pixels. */
const ROW_HEIGHT = 40;
/** The gap between widgets, in pixels, both ways. */
const GAP = 12;
/** The charts drawn at once, in reading order; the rest mount when scrolled near. */
const EAGER_CHARTS = 4;
/** The height of a chart's placeholder: a chart card's minimum height. */
const CHART_HEIGHT = 220;

const GRID_CONFIG = {
  cols: COLUMNS,
  rowHeight: ROW_HEIGHT,
  margin: [GAP, GAP],
  containerPadding: [0, 0],
} as const;

/**
 * The widgets on the 12-column grid. Outside edit mode they are static. In edit mode they can be
 * dragged and resized, and `onLayoutChange` reports the new positions. Below 768 px they stack in
 * one column in `(y, x)` order, and nothing can be moved.
 */
export function WidgetGrid({
  widgets,
  editing,
  onLayoutChange,
  renderCell,
}: {
  widgets: readonly WidgetModel[];
  editing: boolean;
  onLayoutChange?: (layout: Layout) => void;
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
      renderCell={renderCell}
    />
  );
}

function FreeWidgets({
  widgets,
  editing,
  onLayoutChange,
  renderCell,
}: {
  widgets: readonly WidgetModel[];
  editing: boolean;
  onLayoutChange?: (layout: Layout) => void;
  renderCell: (widget: WidgetModel) => ReactNode;
}) {
  const { width, containerRef } = useContainerWidth();
  const layout = useMemo(
    () => widgets.map((widget) => ({ i: widget.id, ...widget.layout })),
    [widgets],
  );
  const dragConfig = useMemo(() => ({ enabled: editing, bounded: true }), [editing]);
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
      >
        {widgets.map((widget) => (
          <div key={widget.id} className="relative h-full min-w-0">
            {widget.kind === "chart" ? (
              // A chart fetches its answer when it is near the viewport, so a long dashboard does
              // not query every widget at once.
              <LazyMount
                eager={eager.has(widget.id)}
                minHeight={CHART_HEIGHT}
                fallback={
                  <Skeleton className="h-full w-full" style={{ minHeight: CHART_HEIGHT }} />
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
