import { COLUMNS } from "./draft";
import { chartAt, ERROR, HALF, LLM, numberAt, query } from "./preset";
import type { Definition } from "./types";

/**
 * The built-in Overview (spec 11 § Dashboard): fixed widgets in this order, with their queries.
 * Its five numbers share the first row equally; below them, its charts take half the width by
 * 7 rows, two to a row. The Overview is not stored; it is not edited.
 */

/** The five number tiles share the first row. */
const TILE = COLUMNS / 5;

export const OVERVIEW: Definition = {
  widgets: [
    numberAt("ov-1", "Traces", 0 * TILE, TILE, query("traces", [{ agg: "count" }])),
    numberAt(
      "ov-2",
      "Error rate",
      1 * TILE,
      TILE,
      query("traces", [{ agg: "rate", where: ERROR, label: "Error rate" }]),
    ),
    numberAt(
      "ov-3",
      "p95 latency",
      2 * TILE,
      TILE,
      query("traces", [{ agg: "p95", field: "duration_ns" }]),
    ),
    numberAt(
      "ov-4",
      "Cost",
      3 * TILE,
      TILE,
      query("traces", [{ agg: "sum", field: "cost_total" }]),
    ),
    numberAt(
      "ov-5",
      "Tokens",
      4 * TILE,
      TILE,
      query("traces", [{ agg: "sum", field: "total_tokens" }]),
    ),

    chartAt(
      "ov-7",
      "Traces & errors",
      0,
      4,
      { type: "bar", stacked: true },
      query("traces", [
        { agg: "count", where: "status <> 'error'", label: "OK" },
        { agg: "count", where: ERROR, label: "Errors" },
      ]),
    ),
    chartAt(
      "ov-8",
      "Tokens by type",
      HALF,
      4,
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

    chartAt(
      "ov-10",
      "Error rate",
      0,
      11,
      { type: "bar", fade_below: 5 },
      query("traces", [{ agg: "rate", where: ERROR, label: "Error rate" }]),
    ),
    chartAt(
      "ov-11",
      "Scores",
      HALF,
      11,
      { type: "line", split: true },
      query("scores", [{ agg: "avg", field: "value_num" }], {
        filter: "value_num IS NOT NULL",
        breakdown: { field: "name", limit: 6, other: false },
      }),
    ),

    chartAt(
      "ov-13",
      "Latency",
      0,
      18,
      { type: "line" },
      query("traces", [
        { agg: "p50", field: "duration_ns", label: "p50" },
        { agg: "p95", field: "duration_ns", label: "p95" },
        { agg: "p99", field: "duration_ns", label: "p99" },
      ]),
    ),
    chartAt(
      "ov-14",
      "LLM latency by model",
      HALF,
      18,
      { type: "line" },
      query("spans", [{ agg: "p95", field: "duration_ns", label: "p95" }], {
        filter: LLM,
        breakdown: { field: "model", limit: 5, other: false },
      }),
    ),
    chartAt(
      "ov-15",
      "TTFT",
      0,
      25,
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

    chartAt(
      "ov-17",
      "Cost by model",
      0,
      32,
      { type: "area", stacked: true },
      query("spans", [{ agg: "sum", field: "cost_total", label: "Cost" }], {
        filter: LLM,
        breakdown: { field: "model", limit: 3, other: true },
      }),
    ),
    chartAt(
      "ov-18",
      "Evaluation cost",
      HALF,
      32,
      { type: "bar" },
      query("traces", [{ agg: "sum", field: "cost_total", label: "Evaluation cost" }], {
        filter: "environment = 'cetra-eval'",
      }),
    ),
    chartAt(
      "ov-19",
      "Top models by cost",
      0,
      39,
      { type: "table", share: true },
      query(
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
    ),
    chartAt(
      "ov-20",
      "Top users by cost",
      HALF,
      39,
      { type: "bar_h" },
      query("traces", [{ agg: "sum", field: "cost_total", label: "Cost" }], {
        filter: "user_id IS NOT NULL",
        breakdown: { field: "user_id", limit: 10, other: false },
      }),
    ),
    chartAt(
      "ov-21",
      "Top trace names by cost",
      0,
      46,
      { type: "bar_h" },
      query("traces", [{ agg: "sum", field: "cost_total", label: "Cost" }], {
        breakdown: { field: "name", limit: 10, other: false },
      }),
    ),
  ],
};
