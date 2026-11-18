import { OVERVIEW } from "./overview";
import type { Definition, Widget } from "./types";

/**
 * The user page's Usage: the Overview's numbers and charts, without its headings and without the
 * evaluation cost. Numbers keep their places; the charts follow them in reading order, two per row
 * (6 columns by 7 rows each), as the Overview lays out its charts.
 */

const CHART_WIDTH = 6;
const CHART_HEIGHT = 7;

const EXCLUDED = new Set(["ov-18"]);

function isChart(widget: Widget): boolean {
  return widget.kind === "chart";
}

function isNumber(widget: Widget): boolean {
  return widget.chart?.type === "number";
}

const NUMBERS = OVERVIEW.widgets.filter((widget) => isChart(widget) && isNumber(widget));
/** The row under the numbers: the charts start there, whatever height the numbers have. */
const NUMBERS_END = Math.max(...NUMBERS.map((widget) => widget.layout.y + widget.layout.h));

function charts(): Widget[] {
  const kept = OVERVIEW.widgets.filter(
    (widget) => isChart(widget) && !isNumber(widget) && !EXCLUDED.has(widget.id),
  );
  return kept.map((widget, index) => ({
    ...widget,
    layout: {
      x: (index % 2) * CHART_WIDTH,
      y: NUMBERS_END + Math.floor(index / 2) * CHART_HEIGHT,
      w: CHART_WIDTH,
      h: CHART_HEIGHT,
    },
  }));
}

/** The user page's widgets: the five numbers, then the charts. Not stored; not edited. */
export const USER_OVERVIEW: Definition = {
  widgets: [...NUMBERS, ...charts()],
};
