import type { CSSProperties, ReactNode } from "react";
import type { Widget as WidgetModel } from "@/lib/dashboards/types";

/** The grid placement of a widget: columns and rows of the 12-column grid. */
export function WidgetGrid({
  widgets,
  render,
}: {
  widgets: readonly WidgetModel[];
  render: (widget: WidgetModel) => ReactNode;
}) {
  // Widgets are in reading order, so the single column below 768 px reads top to bottom.
  const ordered = [...widgets].sort(
    (left, right) => left.layout.y - right.layout.y || left.layout.x - right.layout.x,
  );
  return (
    <div className="grid grid-cols-1 gap-3 md:auto-rows-[40px] md:grid-cols-12">
      {ordered.map((widget) => (
        <div
          key={widget.id}
          className="min-w-0 md:[grid-column:var(--widget-column)] md:[grid-row:var(--widget-row)]"
          style={
            {
              "--widget-column": `${widget.layout.x + 1} / span ${widget.layout.w}`,
              "--widget-row": `${widget.layout.y + 1} / span ${widget.layout.h}`,
            } as CSSProperties
          }
        >
          {render(widget)}
        </div>
      ))}
    </div>
  );
}
