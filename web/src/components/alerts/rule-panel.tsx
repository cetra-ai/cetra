import { useQuery } from "@tanstack/react-query";
import { Link } from "@tanstack/react-router";
import { X } from "lucide-react";
import type { ReactNode } from "react";
import { problemOf } from "@/api/problem";
import { alertRuleQuery } from "@/api/queries-alerts";
import type { Schema } from "@/api/types";
import { RuleDetail } from "@/components/alerts/rule-detail";
import { StateBadge } from "@/components/alerts/state-badge";
import { useProject } from "@/components/shell/project-context";
import { Button } from "@/components/ui/button";
import { Skeleton } from "@/components/ui/skeleton";

/**
 * The alert rule drawer beside the alerts list: header with the name and state, then the rule's
 * content (the same as the rule page). `layout` is the list's layout controls; `onDeleted` runs
 * after the rule is deleted. `row` is the rule as the list has it, shown until the fetch answers.
 */
export function RulePanel({
  ruleId,
  row,
  layout,
  onClose,
  onDeleted,
}: {
  ruleId: string;
  row?: Schema<"AlertRule">;
  layout?: ReactNode;
  onClose: () => void;
  onDeleted: () => void;
}) {
  const { org, project } = useProject();
  const query = useQuery(alertRuleQuery(org, project, ruleId));
  // A rule the list holds shows at once; once the fetch answers, its answer wins.
  const rule = query.data ?? (query.isPending ? row : undefined);
  const missing = problemOf(query.error)?.status === 404;

  return (
    <section aria-label="Alert rule" className="flex h-full min-h-0 flex-col">
      <header className="flex shrink-0 items-center gap-2 border-b px-3 py-1.5 text-xs">
        {rule ? (
          <span className="flex min-w-0 items-center gap-2">
            <span className="truncate text-[13px] font-semibold">{rule.name}</span>
            <StateBadge state={rule.state} enabled={rule.enabled} />
          </span>
        ) : null}
        <div className="ml-auto flex shrink-0 items-center gap-1">
          {layout}
          <Button variant="ghost" size="xs" asChild>
            <Link to="/$org/$project/alerts/$ruleId" params={{ org, project, ruleId }}>
              Open page
            </Link>
          </Button>
          <Button type="button" variant="ghost" size="icon-xs" aria-label="Close" onClick={onClose}>
            <X />
          </Button>
        </div>
      </header>
      <div className="min-h-0 flex-1 overflow-auto p-3 text-xs">
        {rule ? (
          <RuleDetail rule={rule} onDeleted={onDeleted} />
        ) : missing ? (
          <p className="text-muted-foreground">This rule no longer exists.</p>
        ) : query.isError ? (
          <p className="text-destructive">
            {problemOf(query.error)?.title ?? "Could not load this rule."}
          </p>
        ) : (
          <div className="flex flex-col gap-2">
            <Skeleton className="h-6 w-full" />
            <Skeleton className="h-6 w-full" />
            <Skeleton className="h-6 w-full" />
          </div>
        )}
      </div>
    </section>
  );
}
