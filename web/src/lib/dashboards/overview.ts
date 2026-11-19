import { COLUMNS } from "./draft";
import type { Chart, Definition, Measure, Widget, WidgetQuery, WidgetSource } from "./types";

/**
 * The built-in Overview (spec 11 § Dashboard): fixed widgets in this order, with their queries.
 * Its five numbers share the first row equally, its headings take the full width, its charts half
 * the width by 7 rows. The Overview is not stored; it is not edited.
 */

const CHART_HEIGHT = 7;
const HALF = COLUMNS / 2;
/** The five number tiles share the first row. */
const TILE = COLUMNS / 5;

function heading(id: string, title: string, y: number): Widget {
  return { id, title, kind: "heading", layout: { x: 0, y, w: COLUMNS, h: 1 } };
}

/** A number tile in slot `slot` (0 to 4) of the first row. */
function number(id: string, title: string, slot: number, query: WidgetQuery): Widget {
  return {
    id,
    title,
    kind: "chart",
    chart: { type: "number" },
    query,
    layout: { x: slot * TILE, y: 0, w: TILE, h: 4 },
  };
}

function chart(
  id: string,
  title: string,
  x: number,
  y: number,
  settings: Chart,
  query: WidgetQuery,
): Widget {
  return {
    id,
    title,
    kind: "chart",
    chart: settings,
    query,
    layout: { x, y, w: HALF, h: CHART_HEIGHT },
  };
}

/** A query over one source: its measures, an optional filter and split. */
function query(
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

const LLM = "span_kind = 'llm' AND model IS NOT NULL";
const ERROR = "status = 'error'";

export const OVERVIEW: Definition = {
  widgets: [
    number("ov-1", "Traces", 0, query("traces", [{ agg: "count" }])),
    number(
      "ov-2",
      "Error rate",
      1,
      query("traces", [{ agg: "rate", where: ERROR, label: "Error rate" }]),
    ),
    number("ov-3", "p95 latency", 2, query("traces", [{ agg: "p95", field: "duration_ns" }])),
    number("ov-4", "Cost", 3, query("traces", [{ agg: "sum", field: "cost_total" }])),
    number("ov-5", "Tokens", 4, query("traces", [{ agg: "sum", field: "total_tokens" }])),

    heading("ov-6", "Volume", 4),
    chart(
      "ov-7",
      "Traces & errors",
      0,
      5,
      { type: "bar", stacked: true },
      query("traces", [
        { agg: "count", where: "status <> 'error'", label: "OK" },
        { agg: "count", where: ERROR, label: "Errors" },
      ]),
    ),
    chart(
      "ov-8",
      "Tokens by type",
      HALF,
      5,
      { type: "bar", stacked: true },
      query(
        "spans",
        [
          { agg: "sum", field: "input_tokens", label: "Input" },
          { agg: "sum", field: "output_tokens", label: "Output" },
          { agg: "sum", field: "cache_read_tokens", label: "Cache read" },
          { agg: "sum", field: "cache_write_tokens", label: "Cache write" },
        ],
        { filter: "span_kind IN ('llm', 'embedding')" },
      ),
    ),

    heading("ov-9", "Quality", 12),
    chart(
      "ov-10",
      "Error rate",
      0,
      13,
      { type: "bar", fade_below: 5 },
      query("traces", [{ agg: "rate", where: ERROR, label: "Error rate" }]),
    ),
    chart(
      "ov-11",
      "Scores",
      HALF,
      13,
      { type: "line", split: true },
      query("scores", [{ agg: "avg", field: "value_num" }], {
        filter: "value_num IS NOT NULL",
        breakdown: { field: "name", limit: 6, other: false },
      }),
    ),

    heading("ov-12", "Latency", 20),
    chart(
      "ov-13",
      "Latency",
      0,
      21,
      { type: "line" },
      query("traces", [
        { agg: "p50", field: "duration_ns", label: "p50" },
        { agg: "p95", field: "duration_ns", label: "p95" },
        { agg: "p99", field: "duration_ns", label: "p99" },
      ]),
    ),
    chart(
      "ov-14",
      "LLM latency by model",
      HALF,
      21,
      { type: "line" },
      query("spans", [{ agg: "p95", field: "duration_ns", label: "p95" }], {
        filter: LLM,
        breakdown: { field: "model", limit: 5, other: false },
      }),
    ),
    chart(
      "ov-15",
      "TTFT",
      0,
      28,
      { type: "line" },
      query(
        "spans",
        [
          { agg: "p50", field: "ttft_ns", label: "p50" },
          { agg: "p95", field: "ttft_ns", label: "p95" },
        ],
        { filter: "span_kind = 'llm' AND ttft_ns IS NOT NULL" },
      ),
    ),

    heading("ov-16", "Cost", 35),
    chart(
      "ov-17",
      "Cost by model",
      0,
      36,
      { type: "area", stacked: true },
      query("spans", [{ agg: "sum", field: "cost_total", label: "Cost" }], {
        filter: LLM,
        breakdown: { field: "model", limit: 3, other: true },
      }),
    ),
    chart(
      "ov-18",
      "Evaluation cost",
      HALF,
      36,
      { type: "bar" },
      query("traces", [{ agg: "sum", field: "cost_total", label: "Evaluation cost" }], {
        filter: "environment = 'cetra-eval'",
      }),
    ),
    {
      id: "ov-19",
      title: "Top models by cost",
      kind: "chart",
      chart: { type: "table", share: true },
      query: query(
        "spans",
        [
          { agg: "sum", field: "cost_total", label: "Cost" },
          { agg: "count", label: "LLM calls" },
          { agg: "sum", field: "total_tokens", label: "Tokens" },
        ],
        {
          filter: LLM,
          breakdown: { field: "model", limit: 10, other: false },
        },
      ),
      layout: { x: 0, y: 43, w: HALF, h: CHART_HEIGHT },
    },
  ],
};
