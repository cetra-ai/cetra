import { createFileRoute, useNavigate } from "@tanstack/react-router";
import { RuleForm } from "@/components/alerts/rule-form";
import { Forbidden } from "@/components/forbidden";
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
    <div className="mx-auto flex w-full max-w-5xl flex-1 flex-col gap-6 overflow-auto px-4 py-6">
      <h2 className="text-title font-medium">New alert rule</h2>
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
  );
}
