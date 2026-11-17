import { useSuspenseQuery } from "@tanstack/react-query";
import { createFileRoute, Link, notFound, useNavigate } from "@tanstack/react-router";
import { problemOf } from "@/api/problem";
import { alertRuleQuery } from "@/api/queries-alerts";
import { RuleDetail } from "@/components/alerts/rule-detail";
import { StateBadge } from "@/components/alerts/state-badge";
import { useProject } from "@/components/shell/project-context";

export const Route = createFileRoute("/$org/$project/alerts/$ruleId/")({
  loader: async ({ context, params }) => {
    try {
      await context.queryClient.ensureQueryData(
        alertRuleQuery(params.org, params.project, params.ruleId),
      );
    } catch (error) {
      if (problemOf(error)?.status === 404) {
        throw notFound();
      }
      throw error;
    }
  },
  component: AlertRulePage,
});

function AlertRulePage() {
  const { ruleId } = Route.useParams();
  const { org, project } = useProject();
  const navigate = useNavigate();
  const { data: rule } = useSuspenseQuery(alertRuleQuery(org, project, ruleId));

  return (
    <div className="mx-auto flex w-full max-w-5xl flex-1 flex-col gap-6 overflow-auto px-4 py-6">
      <div className="flex flex-col gap-2">
        <Link
          to="/$org/$project/alerts"
          params={{ org, project }}
          className="w-fit text-sm text-muted-foreground underline-offset-4 hover:underline"
        >
          Back to alerts
        </Link>
        <div className="flex flex-wrap items-center gap-3">
          <h1 className="text-lg font-medium">{rule.name}</h1>
          <StateBadge state={rule.state} enabled={rule.enabled} />
        </div>
      </div>
      <RuleDetail
        rule={rule}
        onDeleted={() => {
          void navigate({ to: "/$org/$project/alerts", params: { org, project } });
        }}
      />
    </div>
  );
}
