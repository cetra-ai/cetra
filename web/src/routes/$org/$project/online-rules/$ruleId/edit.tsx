import { useQuery, useQueryClient } from "@tanstack/react-query";
import { createFileRoute, Link, notFound, useNavigate } from "@tanstack/react-router";
import { cn } from "cn";
import { ChevronLeft } from "lucide-react";
import { useState } from "react";

import { $api } from "@/api/client";
import { problemMessage, problemOf } from "@/api/problem";
import { projectRef } from "@/api/project-ref";
import { onlineRuleQuery, onlineRuleStatsKey, onlineRulesKey } from "@/api/queries-online-rules";
import { BACK_LINK_CLASS, BackLink } from "@/components/back-link";
import { ConfirmDialog } from "@/components/confirm-dialog";
import { DisabledReason } from "@/components/disabled-reason";
import { RuleForm } from "@/components/online-rules/rule-form";
import { ITEM_TITLE_CLASS, PageToolbar, PageToolbarTitle } from "@/components/page-toolbar";

import { useProject } from "@/components/shell/project-context";
import { Button } from "@/components/ui/button";
import { canManageEvaluation } from "@/lib/roles";

export const Route = createFileRoute("/$org/$project/online-rules/$ruleId/edit")({
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
  component: EditOnlineRulePage,
});

function EditOnlineRulePage() {
  const { org, project, data } = useProject();
  const { ruleId } = Route.useParams();
  const navigate = useNavigate();
  const queryClient = useQueryClient();
  const detail = useQuery(onlineRuleQuery(org, project, ruleId));
  const remove = $api.useMutation("delete", "/api/v1/projects/{project}/online-rules/{id}");
  const [confirmOpen, setConfirmOpen] = useState(false);
  const [deleteError, setDeleteError] = useState<string | undefined>();
  const canManage = canManageEvaluation(data.role);

  if (detail.isPending) {
    return null;
  }
  if (detail.isError) {
    throw detail.error;
  }
  const rule = detail.data;

  function onDelete() {
    setDeleteError(undefined);
    remove.mutate(
      { params: { path: { project: projectRef(org, project), id: rule.id } } },
      {
        onSuccess: () => {
          void queryClient.invalidateQueries({ queryKey: onlineRulesKey(org, project) });
          void queryClient.invalidateQueries({ queryKey: onlineRuleStatsKey(org, project) });
          void navigate({ to: "/$org/$project/online-rules", params: { org, project } });
        },
        onError: (caught) => {
          setDeleteError(problemMessage(caught, "Could not delete the rule."));
        },
      },
    );
  }

  return (
    <div className="flex h-full min-h-0 w-full flex-col">
      <PageToolbar
        actions={
          <DisabledReason reason={canManage ? undefined : "Only members can edit online rules"}>
            <Button
              type="button"
              variant="outline"
              size="sm"
              disabled={!canManage}
              onClick={() => {
                setDeleteError(undefined);
                setConfirmOpen(true);
              }}
            >
              Delete
            </Button>
          </DisabledReason>
        }
      >
        <PageToolbarTitle
          back={
            <BackLink
              fallback={
                <Link
                  to="/$org/$project/online-rules"
                  params={{ org, project }}
                  search={{ rule: rule.id }}
                  aria-label="Back to online rules"
                  className={BACK_LINK_CLASS}
                >
                  <ChevronLeft aria-hidden="true" className="size-3.5" />
                  Online rules
                </Link>
              }
            />
          }
          label="Online rule"
        >
          <h2 className={ITEM_TITLE_CLASS}>{rule.name}</h2>
        </PageToolbarTitle>
      </PageToolbar>
      <div className="min-h-0 flex-1 overflow-auto">
        <div className={cn("flex flex-col gap-6 px-3 py-3", "max-w-3xl")}>
          <RuleForm key={rule.id} org={org} project={project} rule={rule} />
          <ConfirmDialog
            open={confirmOpen}
            onOpenChange={setConfirmOpen}
            title={`Delete ${rule.name}?`}
            description="The rule stops running on new traces and spans. This cannot be undone."
            confirmLabel="Delete"
            destructive
            pending={remove.isPending}
            error={deleteError}
            onConfirm={onDelete}
          />
        </div>
      </div>
    </div>
  );
}
