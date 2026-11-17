import { useQuery } from "@tanstack/react-query";
import { Link } from "@tanstack/react-router";
import { X } from "lucide-react";
import type { ReactNode } from "react";
import { problemOf } from "@/api/problem";
import { onlineRuleQuery } from "@/api/queries-online-rules";
import type { Schema } from "@/api/types";
import { ResumeButton, RuleDetail } from "@/components/online-rules/rule-detail";
import { RuleStatusPill } from "@/components/online-rules/rule-status";
import { ruleSummary } from "@/components/online-rules/rule-summary";
import { useProject } from "@/components/shell/project-context";
import { Button } from "@/components/ui/button";
import { Skeleton } from "@/components/ui/skeleton";
import { canManageEvaluation } from "@/lib/roles";

/**
 * The online rule drawer beside the rules list: header with the name and status, then the rule's
 * content (the same as the rule page). `layout` is the list's layout controls. `row` is the rule as
 * the list has it, shown until the fetch answers.
 */
export function RulePanel({
  ruleId,
  row,
  layout,
  onClose,
}: {
  ruleId: string;
  row?: Schema<"OnlineRule">;
  layout?: ReactNode;
  onClose: () => void;
}) {
  const { org, project, data } = useProject();
  const query = useQuery(onlineRuleQuery(org, project, ruleId));
  // A rule the list holds shows at once; once the fetch answers, its answer wins.
  const rule = query.data ?? (query.isPending ? row : undefined);
  const missing = problemOf(query.error)?.status === 404;
  const canManage = canManageEvaluation(data.role);

  return (
    <section aria-label="Online rule" className="flex h-full min-h-0 flex-col">
      <header className="flex shrink-0 items-center gap-2 border-b px-3 py-1.5 text-caption">
        {rule ? (
          <span className="flex min-w-0 items-center gap-2">
            <span className="truncate text-ui font-semibold">{rule.name}</span>
            <RuleStatusPill rule={rule} />
          </span>
        ) : null}
        <div className="ml-auto flex shrink-0 items-center gap-1">
          {layout}
          {rule && canManage && rule.paused_reason !== undefined ? (
            <ResumeButton rule={rule} size="xs" />
          ) : null}
          {canManage ? (
            <Button variant="ghost" size="xs" asChild>
              <Link to="/$org/$project/online-rules/$ruleId/edit" params={{ org, project, ruleId }}>
                Edit
              </Link>
            </Button>
          ) : null}
          <Button variant="ghost" size="xs" asChild>
            <Link to="/$org/$project/online-rules/$ruleId" params={{ org, project, ruleId }}>
              Open page
            </Link>
          </Button>
          <Button type="button" variant="ghost" size="icon-xs" aria-label="Close" onClick={onClose}>
            <X />
          </Button>
        </div>
      </header>
      <div className="min-h-0 flex-1 overflow-auto p-3 text-body">
        {rule ? (
          <div className="flex flex-col gap-4">
            <p className="font-mono text-caption text-muted-foreground">{ruleSummary(rule)}</p>
            <RuleDetail rule={rule} />
          </div>
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
