/** Templates of "New dashboard" (D-C-21): their widgets are copied into the new dashboard. */
import { CHART_HEIGHT, chartAt, ERROR, HALF, LLM, LLM_TTFT, numberAt, query } from "./preset";
import type { Definition } from "./types";

export type TemplateId = "cost" | "latency" | "quality" | "coding_agents";

export type DashboardTemplate = {
  id: TemplateId;
  name: string;
  /** One line under the name in the dialog. */
  description: string;
  definition: Definition;
};

const QUARTER = 15; // four tiles in the first row
const THIRD = 20; // three tiles in the first row
const FIFTH = 12; // five tiles in the first row
/** Spans and traces of coding agents (D-CA-4). */
const AGENT = "metadata['coding_agent'] IS NOT NULL";

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
  {
    id: "coding_agents",
    name: "Coding agents",
    description:
      "Cost, sessions and tool use of coding agents (Claude Code, Codex, Gemini CLI, Copilot) by person, repository and branch. Cost is API-equivalent cost: what the tokens would cost at API prices, whatever plan pays for them.",
    definition: {
      widgets: [
        numberAt(
          "agents-1",
          "API-equivalent cost",
          0,
          FIFTH,
          query("traces", [{ agg: "sum", field: "cost_total" }], { filter: AGENT }),
        ),
        numberAt(
          "agents-2",
          "Sessions",
          FIFTH,
          FIFTH,
          query("traces", [{ agg: "count_distinct", field: "session_id" }], { filter: AGENT }),
        ),
        numberAt(
          "agents-3",
          "People",
          2 * FIFTH,
          FIFTH,
          query("traces", [{ agg: "count_distinct", field: "user_id" }], { filter: AGENT }),
        ),
        numberAt(
          "agents-4",
          "Turns",
          3 * FIFTH,
          FIFTH,
          query("traces", [{ agg: "count" }], { filter: AGENT }),
        ),
        numberAt(
          "agents-5",
          "Tokens",
          4 * FIFTH,
          FIFTH,
          query("traces", [{ agg: "sum", field: "total_tokens" }], { filter: AGENT }),
        ),
        chartAt(
          "agents-6",
          "API-equivalent cost by agent",
          0,
          4,
          { type: "area", stacked: true },
          query("traces", [{ agg: "sum", field: "cost_total", label: "API-equivalent cost" }], {
            filter: AGENT,
            breakdown: { field: "metadata['coding_agent']", limit: 5, other: true },
          }),
        ),
        chartAt(
          "agents-7",
          "API-equivalent cost by model",
          HALF,
          4,
          { type: "area", stacked: true },
          query("spans", [{ agg: "sum", field: "cost_total", label: "API-equivalent cost" }], {
            filter: `${LLM} AND ${AGENT}`,
            breakdown: { field: "model", limit: 5, other: true },
          }),
        ),
        chartAt(
          "agents-8",
          "Top people by API-equivalent cost",
          0,
          4 + CHART_HEIGHT,
          { type: "bar_h" },
          query("traces", [{ agg: "sum", field: "cost_total", label: "API-equivalent cost" }], {
            filter: `${AGENT} AND user_id IS NOT NULL`,
            breakdown: { field: "user_id", limit: 10, other: false },
          }),
        ),
        chartAt(
          "agents-9",
          "Top repositories by API-equivalent cost",
          HALF,
          4 + CHART_HEIGHT,
          { type: "bar_h" },
          query("traces", [{ agg: "sum", field: "cost_total", label: "API-equivalent cost" }], {
            filter: `${AGENT} AND metadata['repository'] IS NOT NULL`,
            breakdown: { field: "metadata['repository']", limit: 10, other: false },
          }),
        ),
        chartAt(
          "agents-10",
          "Top branches by API-equivalent cost",
          0,
          4 + 2 * CHART_HEIGHT,
          { type: "bar_h" },
          query("traces", [{ agg: "sum", field: "cost_total", label: "API-equivalent cost" }], {
            filter: `${AGENT} AND metadata['branch'] IS NOT NULL`,
            breakdown: { field: "metadata['branch']", limit: 10, other: false },
          }),
        ),
        chartAt(
          "agents-11",
          "Top sessions by API-equivalent cost",
          HALF,
          4 + 2 * CHART_HEIGHT,
          { type: "bar_h" },
          query("traces", [{ agg: "sum", field: "cost_total", label: "API-equivalent cost" }], {
            filter: `${AGENT} AND session_id IS NOT NULL`,
            breakdown: { field: "session_id", limit: 10, other: false },
          }),
        ),
        chartAt(
          "agents-12",
          "Tool calls by tool",
          0,
          4 + 3 * CHART_HEIGHT,
          { type: "table" },
          query(
            "spans",
            [
              { agg: "count", label: "Calls" },
              { agg: "rate", where: "status = 'error'", label: "Error rate" },
              { agg: "p95", field: "duration_ns", label: "p95 duration" },
            ],
            {
              filter: `span_kind = 'tool' AND ${AGENT}`,
              breakdown: { field: "name", limit: 20, other: true },
            },
          ),
        ),
        chartAt(
          "agents-13",
          "API-equivalent cost per turn by agent",
          HALF,
          4 + 3 * CHART_HEIGHT,
          { type: "table" },
          query(
            "traces",
            [
              { agg: "p50", field: "cost_total", label: "p50 API-equivalent cost" },
              { agg: "p95", field: "cost_total", label: "p95 API-equivalent cost" },
            ],
            {
              filter: AGENT,
              breakdown: { field: "metadata['coding_agent']", limit: 5, other: false },
            },
          ),
        ),
      ],
    },
  },
];
