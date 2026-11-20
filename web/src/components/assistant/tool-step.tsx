import { useNavigate } from "@tanstack/react-router";
import { CircleCheck, CircleX, Loader } from "lucide-react";
import { projectRef } from "@/api/project-ref";
import { fenced, sqlTabsFullReason } from "@/components/assistant/draft-card";
import { DisabledReason } from "@/components/disabled-reason";
import { Markdown } from "@/components/payload/markdown";
import { useProject } from "@/components/shell/project-context";
import { Button } from "@/components/ui/button";
import { handDraft } from "@/lib/assistant/handoff";
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

/** A field of the tool's arguments: `query` (text) or `params` (an object), or `undefined`. */
function argOf(args: unknown, key: string): unknown {
  if (typeof args !== "object" || args === null || Array.isArray(args)) {
    return undefined;
  }
  return (args as Record<string, unknown>)[key];
}

/** One tool call, collapsed by default: its label, its result summary, and the arguments. */
export function ToolStep({ step }: { step: Step }) {
  const { org, project } = useProject();
  const navigate = useNavigate();
  const label = LABELS[step.name] ?? step.name;
  const rawQuery = step.name === "query_sql" ? argOf(step.arguments, "query") : undefined;
  const query = typeof rawQuery === "string" ? rawQuery : undefined;
  const rawParams = argOf(step.arguments, "params");
  const params: Record<string, unknown> =
    typeof rawParams === "object" && rawParams !== null && !Array.isArray(rawParams)
      ? (rawParams as Record<string, unknown>)
      : {};
  const ref = projectRef(org, project);
  const sqlReason = sqlTabsFullReason(ref);
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
        {query !== undefined ? (
          <div className="flex justify-end">
            <DisabledReason reason={sqlReason}>
              <Button
                type="button"
                variant="outline"
                size="sm"
                disabled={sqlReason !== undefined}
                onClick={() => {
                  handDraft(ref, { kind: "sql", draft: { query, params, columns: [] } });
                  void navigate({ to: "/$org/$project/sql", params: { org, project } });
                }}
              >
                Open in SQL console
              </Button>
            </DisabledReason>
          </div>
        ) : null}
      </div>
    </details>
  );
}
