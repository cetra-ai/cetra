import { useQueryClient } from "@tanstack/react-query";
import { $api } from "@/api/client";
import { problemMessage, problemOf } from "@/api/problem";
import { projectRef } from "@/api/project-ref";
import { onlineRuleKey, onlineRuleStatsKey, onlineRulesKey } from "@/api/queries-online-rules";
import type { Schema } from "@/api/types";
import { DisabledReason } from "@/components/disabled-reason";
import { FormError } from "@/components/form/form-error";
import { UpgradeButton } from "@/components/plan/upgrade-button";
import { useOrgEntitlements } from "@/components/plan/use-org-entitlements";
import { useProject } from "@/components/shell/project-context";
import { Switch } from "@/components/ui/switch";

type Rule = Schema<"OnlineRule">;

export const CANNOT_MANAGE_RULES = "Only members can manage online rules";

/** The rule's enabled flag, saved as it is toggled. A plan-limit refusal shows the upgrade. */
export function EnabledSwitch({ rule, canManage }: { rule: Rule; canManage: boolean }) {
  const { org, project } = useProject();
  const queryClient = useQueryClient();
  const upgradeUrl = useOrgEntitlements(org).upgradeUrl;
  const update = $api.useMutation("patch", "/api/v1/projects/{project}/online-rules/{id}");
  const problem = problemOf(update.error);
  const planLimited = problem?.status === 403 && problem.code === "plan_limit";

  return (
    <div className="flex flex-col items-start gap-1">
      <DisabledReason reason={canManage ? undefined : CANNOT_MANAGE_RULES}>
        <Switch
          aria-label={`Enable ${rule.name}`}
          checked={rule.enabled}
          disabled={!canManage || update.isPending}
          onCheckedChange={(enabled) => {
            update.mutate(
              {
                params: { path: { project: projectRef(org, project), id: rule.id } },
                body: { enabled },
              },
              {
                onSuccess: () => {
                  void queryClient.invalidateQueries({ queryKey: onlineRulesKey(org, project) });
                  void queryClient.invalidateQueries({
                    queryKey: onlineRuleStatsKey(org, project),
                  });
                  void queryClient.invalidateQueries({
                    queryKey: onlineRuleKey(org, project, rule.id),
                  });
                },
              },
            );
          }}
        />
      </DisabledReason>
      {update.error ? (
        <>
          <FormError message={problemMessage(update.error, "Could not update the rule.")} />
          {planLimited ? (
            <p className="text-caption">Upgrade your plan or disable another rule.</p>
          ) : null}
          {planLimited ? <UpgradeButton url={upgradeUrl} /> : null}
        </>
      ) : null}
    </div>
  );
}
