import { useQuery } from "@tanstack/react-query";
import { createFileRoute, Link, notFound } from "@tanstack/react-router";
import { problemOf } from "@/api/problem";
import { onlineRuleQuery } from "@/api/queries-online-rules";
import { ResumeButton, RuleDetail } from "@/components/online-rules/rule-detail";
import { RuleStatusPill } from "@/components/online-rules/rule-status";
import { ruleSummary } from "@/components/online-rules/rule-summary";
import { useProject } from "@/components/shell/project-context";
import { Button } from "@/components/ui/button";
import { TooltipProvider } from "@/components/ui/tooltip";
import { canManageEvaluation } from "@/lib/roles";

export const Route = createFileRoute("/$org/$project/online-rules/$ruleId/")({
  loader: async ({ context, params }) => {
    try {
      await context.queryClient.ensureQueryData(
        onlineRuleQuery(params.org, params.project, params.ruleId),
      );
    } catch (error) {
      if (problemOf(error)?.status === 404) {
        throw notFound();
      }
      throw error;
    }
  },
  component: OnlineRulePage,
});

function OnlineRulePage() {
  const { org, project, data } = useProject();
  const { ruleId } = Route.useParams();
  const detail = useQuery(onlineRuleQuery(org, project, ruleId));

  if (detail.isPending) {
    return null;
  }
  if (detail.isError) {
    throw detail.error;
  }
  const rule = detail.data;
  const canManage = canManageEvaluation(data.role);

  return (
    <TooltipProvider>
      <div className="mx-auto flex w-full max-w-5xl flex-1 flex-col gap-6 overflow-auto px-4 py-6">
        <Link
          to="/$org/$project/online-rules"
          params={{ org, project }}
          className="w-fit text-body text-muted-foreground underline-offset-4 hover:underline"
        >
          Back to online rules
        </Link>
        <header className="flex flex-wrap items-start justify-between gap-4">
          <div className="flex flex-col gap-2">
            <div className="flex flex-wrap items-center gap-3">
              <h1 className="text-heading font-medium">{rule.name}</h1>
              <RuleStatusPill rule={rule} />
            </div>
            <p className="font-mono text-body text-muted-foreground">{ruleSummary(rule)}</p>
          </div>
          {canManage ? (
            <div className="flex flex-wrap items-start gap-2">
              <Button asChild variant="outline">
                <Link
                  to="/$org/$project/online-rules/$ruleId/edit"
                  params={{ org, project, ruleId }}
                >
                  Edit
                </Link>
              </Button>
              {rule.paused_reason !== undefined ? <ResumeButton rule={rule} /> : null}
            </div>
          ) : null}
        </header>

        <RuleDetail rule={rule} />
      </div>
    </TooltipProvider>
  );
}
