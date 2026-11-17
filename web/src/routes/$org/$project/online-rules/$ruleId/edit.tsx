import { useQuery, useQueryClient } from "@tanstack/react-query";
import { createFileRoute, notFound, useNavigate } from "@tanstack/react-router";
import { useState } from "react";
import { $api } from "@/api/client";
import { problemMessage, problemOf } from "@/api/problem";
import { projectRef } from "@/api/project-ref";
import { onlineRuleQuery, onlineRuleStatsKey, onlineRulesKey } from "@/api/queries-online-rules";
import { ConfirmDialog } from "@/components/confirm-dialog";
import { DisabledReason } from "@/components/disabled-reason";
import { RuleForm } from "@/components/online-rules/rule-form";
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
    <div className="mx-auto flex w-full max-w-3xl flex-1 flex-col gap-6 overflow-auto px-4 py-6">
      <div className="flex flex-wrap items-start justify-between gap-4">
        <h1 className="text-title font-medium">{rule.name}</h1>
        <DisabledReason reason={canManage ? undefined : "Only members can edit online rules"}>
          <Button
            type="button"
            variant="outline"
            disabled={!canManage}
            onClick={() => {
              setDeleteError(undefined);
              setConfirmOpen(true);
            }}
          >
            Delete
          </Button>
        </DisabledReason>
      </div>
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
  );
}
