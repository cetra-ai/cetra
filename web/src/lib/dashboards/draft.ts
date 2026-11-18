import type { Definition, Layout, Widget } from "./types";

/** The grid's columns. A widget's `x + w` never passes this (spec 04 § Dashboards). */
export const COLUMNS = 12;

/** Where `issue.field` names a widget: `definition.widgets[3].query…` gives 3. */
const WIDGET_PATH = /^definition\.widgets\[(\d+)\]/;

/** The first row below every widget, so a new widget goes under the existing ones. */
export function bottomOf(widgets: readonly Widget[]): number {
  return widgets.reduce((bottom, widget) => Math.max(bottom, widget.layout.y + widget.layout.h), 0);
}

/** A new id for a widget: `w-` and a UUID, which is 38 characters of `[A-Za-z0-9_-]`. */
export function newWidgetId(): string {
  return `w-${crypto.randomUUID()}`;
}

/** The widget index an issue path names, or undefined for a dashboard-level path. */
export function issueWidgetIndex(field: string): number | undefined {
  const match = WIDGET_PATH.exec(field);
  return match ? Number(match[1]) : undefined;
}

/**
 * The path with the widget prefix removed, for showing under a widget: `query.measures[0].field`.
 * A path that names no widget is returned as it is.
 */
export function issueWithin(field: string): string {
  const match = WIDGET_PATH.exec(field);
  return match ? field.slice(match[0].length).replace(/^\./, "") : field;
}

/**
 * Applies the positions the grid reports. Returns `widgets` itself when no position moved, so a
 * grid that reports its layout again does not start a new render.
 */
export function withLayouts(
  widgets: Widget[],
  layout: readonly (Layout & { i: string })[],
): Widget[] {
  const placed = new Map(layout.map((item) => [item.i, item]));
  let changed = false;
  const next = widgets.map((widget) => {
    const item = placed.get(widget.id);
    if (
      item === undefined ||
      (item.x === widget.layout.x &&
        item.y === widget.layout.y &&
        item.w === widget.layout.w &&
        item.h === widget.layout.h)
    ) {
      return widget;
    }
    changed = true;
    return { ...widget, layout: { x: item.x, y: item.y, w: item.w, h: item.h } };
  });
  return changed ? next : widgets;
}

/** Whether two definitions hold the same widgets (the JSON of both is compared). */
export function sameDefinition(left: Definition, right: Definition): boolean {
  return JSON.stringify(left) === JSON.stringify(right);
}
