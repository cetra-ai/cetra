import { CircleCheck, CircleX, Loader } from "lucide-react";
import { Markdown } from "@/components/payload/markdown";
import type { Step } from "@/lib/assistant/transcript";

/** The label of each tool (`crates/server/src/assistant/tools/mod.rs` `specs()`). */
const LABELS: Record<string, string> = {
  get_schema: "Read the schema",
  query_sql: "Ran SQL",
  list_saved_views: "Listed saved views",
  search_traces: "Searched traces",
  get_trace: "Read a trace",
  get_span: "Read a span",
  list_sessions: "Listed sessions",
  get_session: "Read a session",
  list_scores: "Listed scores",
  list_score_definitions: "Listed score definitions",
  get_project_settings: "Read the project settings",
  list_alert_rules: "Listed alert rules",
  list_evaluators: "Listed evaluators",
  get_evaluator: "Read an evaluator",
  list_online_rules: "Listed online rules",
  get_online_rule: "Read an online rule",
  list_dashboards: "Listed dashboards",
  get_dashboard: "Read a dashboard",
  query_metrics: "Queried metrics",
  draft_filter: "Drafted a filter",
  draft_sql: "Drafted SQL",
  draft_dashboard: "Drafted a dashboard",
  draft_evaluator: "Drafted an evaluator",
  draft_online_rule: "Drafted an online rule",
};

function summaryOf(result: Step["result"]): string {
  if (result === undefined) {
    return "running";
  }
  if (!result.ok) {
    return result.error_code !== undefined ? `failed: ${result.error_code}` : "failed";
  }
  if (result.stubbed === true) {
    return "result expired";
  }
  if (result.rows !== undefined) {
    return result.rows === 1 ? "1 row" : `${result.rows} rows`;
  }
  return `${result.size_bytes} bytes`;
}

function StatusIcon({ result }: { result: Step["result"] }) {
  if (result === undefined) {
    return <Loader aria-hidden="true" className="size-4 animate-spin text-muted-foreground" />;
  }
  if (!result.ok) {
    return <CircleX aria-hidden="true" className="size-4 text-destructive" />;
  }
  return <CircleCheck aria-hidden="true" className="size-4 text-muted-foreground" />;
}

/** A fenced code block whose fence is longer than any backtick run in `body`. */
function fenced(language: string, body: string): string {
  const longest = Math.max(2, ...(body.match(/`+/g) ?? []).map((run) => run.length));
  const fence = "`".repeat(longest + 1);
  return `${fence}${language}\n${body}\n${fence}`;
}

function sqlOf(args: unknown): string | undefined {
  if (typeof args !== "object" || args === null || Array.isArray(args)) {
    return undefined;
  }
  const query = (args as Record<string, unknown>).query;
  return typeof query === "string" ? query : undefined;
}

/** One tool call, collapsed by default: its label, its result summary, and the arguments. */
export function ToolStep({ step }: { step: Step }) {
  const label = LABELS[step.name] ?? step.name;
  const query = step.name === "query_sql" ? sqlOf(step.arguments) : undefined;
  return (
    <details className="rounded-md border bg-surface text-body">
      <summary className="flex cursor-pointer items-center gap-2 px-2.5 py-1.5">
        <StatusIcon result={step.result} />
        <span className="font-medium">{label}</span>
        <span className="text-muted-foreground">{summaryOf(step.result)}</span>
      </summary>
      <div className="flex flex-col gap-2 border-t p-2.5">
        <Markdown text={fenced("json", JSON.stringify(step.arguments ?? null, null, 2))} />
        {query !== undefined ? <Markdown text={fenced("sql", query)} /> : null}
      </div>
    </details>
  );
}
