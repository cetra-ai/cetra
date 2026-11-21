import { createFileRoute, Link, useNavigate } from "@tanstack/react-router";
import { cn } from "cn";
import { ChevronLeft } from "lucide-react";
import { RuleForm } from "@/components/alerts/rule-form";
import { BACK_LINK_CLASS, BackLink } from "@/components/back-link";
import { Forbidden } from "@/components/forbidden";
import { ITEM_TITLE_CLASS, PageToolbar, PageToolbarTitle } from "@/components/page-toolbar";
import { useProject } from "@/components/shell/project-context";
import type { RuleFormValues } from "@/lib/alerts";
import { canManageAlerts } from "@/lib/roles";
import { parseNewRuleSearch } from "@/lib/search/alerts";

export const Route = createFileRoute("/$org/$project/alerts/new")({
  validateSearch: (search) => parseNewRuleSearch(search),
  component: NewRulePage,
});

function NewRulePage() {
  const { org, project, data } = useProject();
  const search = Route.useSearch();
  const navigate = useNavigate();

  if (!canManageAlerts(data.role)) {
    return <Forbidden description="Only project members and above can create alert rules." />;
  }

  // "Create alert" of an issue page or a source prefills an issue occurrences rule (D-IC-23).
  const initial: Partial<RuleFormValues> | undefined =
    search.metric === "issue_occurrences"
      ? {
          metric: "issue_occurrences",
          issueScope: search.source_id !== undefined ? "source" : "issue",
          issueId: search.issue_id ?? "",
          sourceId: search.source_id ?? "",
          window: "1h",
          comparator: "gt",
          threshold: "0",
        }
      : undefined;

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
          <h2 className={ITEM_TITLE_CLASS}>New alert rule</h2>
        </PageToolbarTitle>
      </PageToolbar>
      <div className="min-h-0 flex-1 overflow-auto">
        <div className={cn("flex flex-col gap-6 px-3 py-3", "max-w-5xl")}>
          <RuleForm
            org={org}
            project={project}
            initial={initial}
            onSaved={(rule) => {
              void navigate({
                to: "/$org/$project/alerts",
                params: { org, project },
                search: { rule: rule.id },
              });
            }}
            onCancel={() => {
              void navigate({ to: "/$org/$project/alerts", params: { org, project } });
            }}
          />
        </div>
      </div>
    </div>
  );
}
