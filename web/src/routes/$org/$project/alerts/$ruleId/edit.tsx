import { useSuspenseQuery } from "@tanstack/react-query";
import { createFileRoute, Link, notFound, useNavigate } from "@tanstack/react-router";
import { cn } from "cn";
import { ChevronLeft } from "lucide-react";
import { problemOf } from "@/api/problem";
import { alertRuleQuery } from "@/api/queries-alerts";
import { RuleForm } from "@/components/alerts/rule-form";
import { BACK_LINK_CLASS, BackLink } from "@/components/back-link";
import { Forbidden } from "@/components/forbidden";
import { ITEM_TITLE_CLASS, PageToolbar, PageToolbarTitle } from "@/components/page-toolbar";
import { useProject } from "@/components/shell/project-context";
import { canManageAlerts } from "@/lib/roles";

export const Route = createFileRoute("/$org/$project/alerts/$ruleId/edit")({
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
  component: EditRulePage,
});

function EditRulePage() {
  const { ruleId } = Route.useParams();
  const { org, project, data } = useProject();
  const navigate = useNavigate();
  const { data: rule } = useSuspenseQuery(alertRuleQuery(org, project, ruleId));

  if (!canManageAlerts(data.role)) {
    return <Forbidden description="Only project members and above can edit alert rules." />;
  }

  const toRule = () => {
    void navigate({
      to: "/$org/$project/alerts",
      params: { org, project },
      search: { rule: rule.id },
    });
  };

  return (
    <div className="flex h-full min-h-0 w-full flex-col">
      <PageToolbar>
        <PageToolbarTitle
          back={
            <BackLink
              fallback={
                <Link
                  to="/$org/$project/alerts"
                  params={{ org, project }}
                  search={{ rule: rule.id }}
                  aria-label="Back to alerts"
                  className={BACK_LINK_CLASS}
                >
                  <ChevronLeft aria-hidden="true" className="size-3.5" />
                  Alerts
                </Link>
              }
            />
          }
          label="Alert rule"
        >
          <h2 className={ITEM_TITLE_CLASS}>{rule.name}</h2>
        </PageToolbarTitle>
      </PageToolbar>
      <div className="min-h-0 flex-1 overflow-auto">
        <div className={cn("flex flex-col gap-6 px-3 py-3", "max-w-5xl")}>
          <RuleForm org={org} project={project} rule={rule} onSaved={toRule} onCancel={toRule} />
        </div>
      </div>
    </div>
  );
}
