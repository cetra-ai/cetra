import { COLUMNS } from "./draft";
import type { Chart, Measure, Widget, WidgetQuery, WidgetSource } from "./types";

/** Building blocks of the built-in dashboards: the Overview and the templates (D-C-21). */

/** Rows of a chart. */
export const CHART_HEIGHT = 7;
/** Rows of a number tile. */
export const NUMBER_HEIGHT = 4;
/** Half the grid's width. */
export const HALF = COLUMNS / 2;
/** LLM spans with a model. */
export const LLM = "span_kind = 'llm' AND model IS NOT NULL";
export const ERROR = "status = 'error'";

/** A number tile `w` columns wide at column `x` of the first row. */
export function numberAt(id: string, title: string, x: number, w: number, q: WidgetQuery): Widget {
  return {
    id,
    title,
    kind: "chart",
    chart: { type: "number" },
    query: q,
    layout: { x, y: 0, w, h: NUMBER_HEIGHT },
  };
}

/** A chart half the width and `CHART_HEIGHT` rows tall, at column `x` and row `y`. */
export function chartAt(
  id: string,
  title: string,
  x: number,
  y: number,
  settings: Chart,
  q: WidgetQuery,
): Widget {
  return {
    id,
    title,
    kind: "chart",
    chart: settings,
    query: q,
    layout: { x, y, w: HALF, h: CHART_HEIGHT },
  };
}

/** A query over one source: its measures, an optional filter and breakdown. */
export function query(
  source: WidgetSource,
  measures: Measure[],
  options: {
    filter?: string;
    breakdown?: WidgetQuery["breakdown"];
  } = {},
): WidgetQuery {
  return {
    source,
    measures,
    ...(options.filter ? { filter: options.filter } : {}),
    ...(options.breakdown ? { breakdown: options.breakdown } : {}),
  };
}
