/** Templates of "New dashboard" (D-C-21): their widgets are copied into the new dashboard. */
import { CHART_HEIGHT, chartAt, ERROR, HALF, LLM, numberAt, query } from "./preset";
import type { Definition } from "./types";

export type TemplateId = "cost" | "latency" | "quality";

export type DashboardTemplate = {
  id: TemplateId;
  name: string;
  /** One line under the name in the dialog. */
  description: string;
  definition: Definition;
};

const QUARTER = 15; // four tiles in the first row
const THIRD = 20; // three tiles in the first row
const LLM_TTFT = "span_kind = 'llm' AND ttft_ns IS NOT NULL";

export const TEMPLATES: readonly DashboardTemplate[] = [
  {
    id: "cost",
    name: "Cost",
    description: "Cost by model, environment, user and trace name, and p95 cost.",
    definition: {
      widgets: [
        numberAt(
          "cost-1",
          "Total cost",
          0,
          QUARTER,
          query("traces", [{ agg: "sum", field: "cost_total" }]),
        ),
        numberAt(
          "cost-2",
          "Avg cost per trace",
          QUARTER,
          QUARTER,
          query("traces", [{ agg: "avg", field: "cost_total" }]),
        ),
        numberAt(
          "cost-3",
          "LLM calls",
          2 * QUARTER,
          QUARTER,
          query("spans", [{ agg: "count" }], { filter: "span_kind = 'llm'" }),
        ),
        numberAt(
          "cost-4",
          "Tokens",
          3 * QUARTER,
          QUARTER,
          query("traces", [{ agg: "sum", field: "total_tokens" }]),
        ),
        chartAt(
          "cost-5",
          "Cost over time by model",
          0,
          4,
          { type: "area", stacked: true },
          query("spans", [{ agg: "sum", field: "cost_total", label: "Cost" }], {
            filter: LLM,
            breakdown: { field: "model", limit: 5, other: true },
          }),
        ),
        chartAt(
          "cost-6",
          "Cost by environment",
          HALF,
          4,
          { type: "bar_h" },
          query("traces", [{ agg: "sum", field: "cost_total", label: "Cost" }], {
            breakdown: { field: "environment", limit: 10, other: false },
          }),
        ),
        chartAt(
          "cost-7",
          "Top users by cost",
          0,
          4 + CHART_HEIGHT,
          { type: "bar_h" },
          query("traces", [{ agg: "sum", field: "cost_total", label: "Cost" }], {
            filter: "user_id IS NOT NULL",
            breakdown: { field: "user_id", limit: 20, other: false },
          }),
        ),
        chartAt(
          "cost-8",
          "Top trace names by cost",
          HALF,
          4 + CHART_HEIGHT,
          { type: "bar_h" },
          query("traces", [{ agg: "sum", field: "cost_total", label: "Cost" }], {
            breakdown: { field: "name", limit: 20, other: false },
          }),
        ),
        chartAt(
          "cost-9",
          "Top span names by cost",
          0,
          4 + 2 * CHART_HEIGHT,
          { type: "bar_h" },
          query("spans", [{ agg: "sum", field: "cost_total", label: "Cost" }], {
            filter: "cost_total IS NOT NULL",
            breakdown: { field: "name", limit: 20, other: false },
          }),
        ),
        chartAt(
          "cost-10",
          "p95 cost per trace by name",
          HALF,
          4 + 2 * CHART_HEIGHT,
          { type: "line" },
          query("traces", [{ agg: "p95", field: "cost_total", label: "p95" }], {
            breakdown: { field: "name", limit: 5, other: false },
          }),
        ),
        chartAt(
          "cost-11",
          "p95 cost per LLM call",
          0,
          4 + 3 * CHART_HEIGHT,
          { type: "line" },
          query(
            "spans",
            [
              { agg: "p95", field: "cost_input", label: "Input" },
              { agg: "p95", field: "cost_output", label: "Output" },
            ],
            { filter: LLM },
          ),
        ),
      ],
    },
  },
  {
    id: "latency",
    name: "Latency",
    description: "Trace and LLM latency percentiles, slowest traces and time to first token.",
    definition: {
      widgets: [
        numberAt(
          "lat-1",
          "p50 trace duration",
          0,
          THIRD,
          query("traces", [{ agg: "p50", field: "duration_ns" }]),
        ),
        numberAt(
          "lat-2",
          "p95 trace duration",
          THIRD,
          THIRD,
          query("traces", [{ agg: "p95", field: "duration_ns" }]),
        ),
        numberAt(
          "lat-3",
          "p95 TTFT",
          2 * THIRD,
          THIRD,
          query("spans", [{ agg: "p95", field: "ttft_ns" }], { filter: LLM_TTFT }),
        ),
        chartAt(
          "lat-4",
          "p95 duration by trace name",
          0,
          4,
          { type: "line" },
          query("traces", [{ agg: "p95", field: "duration_ns", label: "p95" }], {
            breakdown: { field: "name", limit: 5, other: false },
          }),
        ),
        chartAt(
          "lat-5",
          "Slowest trace names",
          HALF,
          4,
          { type: "bar_h" },
          query("traces", [{ agg: "p95", field: "duration_ns", label: "p95" }], {
            breakdown: { field: "name", limit: 10, other: false },
          }),
        ),
        chartAt(
          "lat-6",
          "LLM latency by model",
          0,
          4 + CHART_HEIGHT,
          { type: "line" },
          query("spans", [{ agg: "p95", field: "duration_ns", label: "p95" }], {
            filter: LLM,
            breakdown: { field: "model", limit: 5, other: false },
          }),
        ),
        chartAt(
          "lat-7",
          "TTFT by model",
          HALF,
          4 + CHART_HEIGHT,
          { type: "line" },
          query("spans", [{ agg: "p95", field: "ttft_ns", label: "p95" }], {
            filter: `${LLM} AND ttft_ns IS NOT NULL`,
            breakdown: { field: "model", limit: 5, other: false },
          }),
        ),
      ],
    },
  },
  {
    id: "quality",
    name: "Quality",
    description: "Error rates by trace and span name, and scores over time.",
    definition: {
      widgets: [
        numberAt(
          "qual-1",
          "Error rate",
          0,
          THIRD,
          query("traces", [{ agg: "rate", where: ERROR, label: "Error rate" }]),
        ),
        numberAt(
          "qual-2",
          "Errors",
          THIRD,
          THIRD,
          query("traces", [{ agg: "count", where: ERROR, label: "Errors" }]),
        ),
        numberAt("qual-3", "Traces", 2 * THIRD, THIRD, query("traces", [{ agg: "count" }])),
        chartAt(
          "qual-4",
          "Error rate over time",
          0,
          4,
          { type: "line" },
          query("traces", [{ agg: "rate", where: ERROR, label: "Error rate" }]),
        ),
        chartAt(
          "qual-5",
          "Error rate by trace name",
          HALF,
          4,
          { type: "bar_h" },
          query("traces", [{ agg: "rate", where: ERROR, label: "Error rate" }], {
            breakdown: { field: "name", limit: 10, other: false },
          }),
        ),
        chartAt(
          "qual-6",
          "Top failing span names",
          0,
          4 + CHART_HEIGHT,
          { type: "bar_h" },
          query("spans", [{ agg: "count", label: "Errors" }], {
            filter: ERROR,
            breakdown: { field: "name", limit: 10, other: false },
          }),
        ),
        chartAt(
          "qual-7",
          "Scores over time",
          HALF,
          4 + CHART_HEIGHT,
          { type: "line" },
          query("scores", [{ agg: "avg", field: "value_num" }], {
            filter: "value_num IS NOT NULL",
            breakdown: { field: "name", limit: 6, other: false },
          }),
        ),
      ],
    },
  },
];
