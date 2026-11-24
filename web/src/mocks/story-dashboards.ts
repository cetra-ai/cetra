import type { Dashboard, Widget, WidgetQuery } from "@/lib/dashboards/types";

/**
 * The homepage stories' dashboards (design §12, M14), one per story. Each widget carries its
 * story's tag in `query.filter`, since a stored dashboard has no filter of its own; the scores
 * widget cannot (scores have no tags). The widget titles are the ones the assistant's draft
 * cards show. The browser mock stores them next to the fixture dashboard (`browser.ts`); the
 * large mock answers every widget.
 */

/** A fixture user (`fixtures/api/static/dashboard.json`). */
const AUTHOR = "01900000-0000-7000-8000-000000000100";
const CREATED_AT = "2026-11-10T09:00:00.000000000Z";

export const STORY_DASHBOARD_IDS = {
  answerQuality: "01900000-0000-7000-8000-000000570401",
  codeAgentHealth: "01900000-0000-7000-8000-000000570402",
  voiceLatency: "01900000-0000-7000-8000-000000570403",
  invoiceExtraction: "01900000-0000-7000-8000-000000570404",
} as const;

/** The 2x2 grid of a story dashboard: widgets 0 and 1 on the first row, 2 and 3 on the second. */
const SLOTS = [
  { x: 0, y: 0, w: 30, h: 6 },
  { x: 30, y: 0, w: 30, h: 6 },
  { x: 0, y: 6, w: 30, h: 6 },
  { x: 30, y: 6, w: 30, h: 6 },
] as const;

function chart(
  slot: 0 | 1 | 2 | 3,
  id: string,
  title: string,
  type: "line" | "bar" | "bar_h" | "number",
  query: WidgetQuery,
): Widget {
  return { id, title, layout: { ...SLOTS[slot] }, kind: "chart", chart: { type }, query };
}

function dashboard(id: string, name: string, description: string, widgets: Widget[]): Dashboard {
  return {
    id,
    name,
    description,
    definition: { widgets },
    version: 1,
    created_by: AUTHOR,
    updated_by: AUTHOR,
    created_at: CREATED_AT,
    updated_at: CREATED_AT,
  };
}

const EMAIL = "tags @> ['email']";
const RETRY = "tags @> ['retry']";
const VOICE = "tags @> ['voice']";
const BILLING = "tags @> ['billing']";

/** The story dashboards, in story order. Fresh objects on every call. */
export function storyDashboards(): Dashboard[] {
  return [
    dashboard(
      STORY_DASHBOARD_IDS.answerQuality,
      "Answer quality",
      "Support answers: quality, volume and cost.",
      [
        chart(0, "correctness-by-day", "Correctness by day", "line", {
          source: "scores",
          filter: "name = 'correctness'",
          measures: [{ agg: "avg", field: "value_num", label: "Correctness" }],
        }),
        chart(1, "answers-per-day", "Answers per day", "bar", {
          source: "traces",
          filter: EMAIL,
          measures: [{ agg: "count", label: "Answers" }],
        }),
        chart(2, "cost-per-answer", "Cost per answer", "number", {
          source: "traces",
          filter: EMAIL,
          measures: [{ agg: "avg", field: "cost_total", label: "Cost per answer" }],
        }),
        chart(3, "answers-by-model", "Answers by model", "bar_h", {
          source: "traces",
          filter: EMAIL,
          measures: [{ agg: "count", label: "Answers" }],
          breakdown: { field: "models", limit: 5 },
        }),
      ],
    ),
    dashboard(
      STORY_DASHBOARD_IDS.codeAgentHealth,
      "Code agent health",
      "Failed runs, cost and duration of the coding agent.",
      [
        chart(0, "failed-runs-per-day", "Failed runs per day", "bar", {
          source: "traces",
          filter: RETRY,
          measures: [{ agg: "count", where: "status = 'error'", label: "Failed runs" }],
        }),
        chart(1, "error-rate", "Error rate", "number", {
          source: "traces",
          filter: RETRY,
          measures: [{ agg: "rate", where: "error_count > 0", label: "Error rate" }],
        }),
        chart(2, "cost-per-run", "Cost per run", "line", {
          source: "traces",
          filter: RETRY,
          measures: [{ agg: "avg", field: "cost_total", label: "Cost per run" }],
        }),
        chart(3, "p95-duration", "p95 duration", "number", {
          source: "traces",
          filter: RETRY,
          measures: [{ agg: "p95", field: "duration_ns", label: "p95 duration" }],
        }),
      ],
    ),
    dashboard(
      STORY_DASHBOARD_IDS.voiceLatency,
      "Voice latency",
      "How long callers wait for a reply.",
      [
        chart(0, "p95-reply-time", "p95 reply time", "number", {
          source: "traces",
          filter: VOICE,
          measures: [{ agg: "p95", field: "duration_ns", label: "p95 reply time" }],
        }),
        chart(1, "reply-time-by-day", "Reply time by day", "line", {
          source: "traces",
          filter: VOICE,
          measures: [{ agg: "p95", field: "duration_ns", label: "p95 reply time" }],
        }),
        chart(2, "calls-over-3-s", "Calls over 3 s", "number", {
          source: "traces",
          filter: VOICE,
          measures: [{ agg: "rate", where: "duration_ns > 3000000000", label: "Over 3 s" }],
        }),
        chart(3, "calls-per-day", "Calls per day", "bar", {
          source: "traces",
          filter: VOICE,
          measures: [{ agg: "count", label: "Calls" }],
        }),
      ],
    ),
    dashboard(
      STORY_DASHBOARD_IDS.invoiceExtraction,
      "Invoice extraction",
      "Invoices read, schema-check failures and cost.",
      [
        chart(0, "invoices-per-day", "Invoices per day", "bar", {
          source: "traces",
          filter: BILLING,
          measures: [{ agg: "count", label: "Invoices" }],
        }),
        chart(1, "schema-check-failures", "Schema check failures", "number", {
          source: "traces",
          filter: BILLING,
          measures: [{ agg: "rate", where: "status = 'error'", label: "Failures" }],
        }),
        chart(2, "cost-per-invoice", "Cost per invoice", "line", {
          source: "traces",
          filter: BILLING,
          measures: [{ agg: "avg", field: "cost_total", label: "Cost per invoice" }],
        }),
        chart(3, "invoices-by-model", "Invoices by model", "bar_h", {
          source: "traces",
          filter: BILLING,
          measures: [{ agg: "count", label: "Invoices" }],
          breakdown: { field: "models", limit: 5 },
        }),
      ],
    ),
  ];
}
