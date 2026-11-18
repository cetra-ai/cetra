import { uuidv7 } from "@/lib/uuid";
import { formOf, widgetOf } from "./editor";
import type { Definition, Layout, Widget } from "./types";

/** The grid's columns. A widget's `x + w` never passes this (spec 04 § Dashboards). */
export const COLUMNS = 12;
/** The tallest widget, in rows, and the lowest row a widget can start on (spec 04 § Dashboards). */
export const MAX_HEIGHT = 40;
export const MAX_Y = 1000;

/** One step of a widget's menu: a move by one cell, or a resize by one column or row. */
export type Nudge = "up" | "down" | "left" | "right" | "wider" | "narrower" | "taller" | "shorter";

/** Where `issue.field` names a widget: `definition.widgets[3].query…` gives 3. */
const WIDGET_PATH = /^definition\.widgets\[(\d+)\]/;

/** The first row below every widget, so a new widget goes under the existing ones. */
export function bottomOf(widgets: readonly Widget[]): number {
  return widgets.reduce((bottom, widget) => Math.max(bottom, widget.layout.y + widget.layout.h), 0);
}

/** A new id for a widget: `w-` and a UUID, which is 38 characters of `[A-Za-z0-9_-]`. */
export function newWidgetId(): string {
  return `w-${uuidv7()}`;
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

/**
 * The layout a widget takes after one step, or undefined when the step would leave the limits of
 * spec 04 § Dashboards (a step that cannot be taken is disabled in the menu).
 */
export function nudgedLayout(layout: Layout, step: Nudge): Layout | undefined {
  const { x, y, w, h } = layout;
  switch (step) {
    case "up":
      return y > 0 ? { x, y: y - 1, w, h } : undefined;
    case "down":
      return y < MAX_Y ? { x, y: y + 1, w, h } : undefined;
    case "left":
      return x > 0 ? { x: x - 1, y, w, h } : undefined;
    case "right":
      return x + w < COLUMNS ? { x: x + 1, y, w, h } : undefined;
    case "wider":
      return x + w < COLUMNS ? { x, y, w: w + 1, h } : undefined;
    case "narrower":
      return w > 1 ? { x, y, w: w - 1, h } : undefined;
    case "taller":
      return h < MAX_HEIGHT ? { x, y, w, h: h + 1 } : undefined;
    case "shorter":
      return h > 1 ? { x, y, w, h: h - 1 } : undefined;
  }
}

/**
 * Whether two definitions hold the same widgets. Each chart is compared as the editor writes it
 * (so a key order, or an option the editor would not write, is not a change), with its keys in a
 * fixed order.
 */
export function sameDefinition(left: Definition, right: Definition): boolean {
  return stableJson(canonical(left)) === stableJson(canonical(right));
}

function canonical(definition: Definition): unknown[] {
  return definition.widgets.map((widget) =>
    widget.kind === "chart" && widget.chart && widget.query
      ? widgetOf(formOf(widget), widget)
      : widget,
  );
}

/** JSON with the keys of every object sorted, so the order a value was written in does not show. */
function stableJson(value: unknown): string {
  return JSON.stringify(value, (_key, item: unknown) =>
    item !== null && typeof item === "object" && !Array.isArray(item)
      ? Object.fromEntries(
          Object.entries(item as Record<string, unknown>).sort(([a], [b]) => a.localeCompare(b)),
        )
      : item,
  );
}
