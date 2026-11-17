import { useQueryClient } from "@tanstack/react-query";
import { Link } from "@tanstack/react-router";
import { MoreHorizontal } from "lucide-react";
import { useState } from "react";
import { $api } from "@/api/client";
import { problemMessage } from "@/api/problem";
import { projectRef } from "@/api/project-ref";
import { onlineRuleStatsKey, onlineRulesKey } from "@/api/queries-online-rules";
import type { Schema } from "@/api/types";
import { ConfirmDialog } from "@/components/confirm-dialog";
import { BackfillDialog } from "@/components/online-rules/backfill-dialog";
import { backfillReason } from "@/components/online-rules/rule-detail";
import { useProject } from "@/components/shell/project-context";
import { Button } from "@/components/ui/button";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";

type Rule = Schema<"OnlineRule">;

/**
 * The row's actions: open, edit, run on existing data and delete. Management items show for
 * members only. `onDeleted` runs after the rule is deleted, so the list can close its panel.
 */
export function RuleActions({
  rule,
  canManage,
  onDeleted,
}: {
  rule: Rule;
  canManage: boolean;
  onDeleted: () => void;
}) {
  const { org, project } = useProject();
  const queryClient = useQueryClient();
  const [backfillOpen, setBackfillOpen] = useState(false);
  const [deleteOpen, setDeleteOpen] = useState(false);
  const remove = $api.useMutation("delete", "/api/v1/projects/{project}/online-rules/{id}");
  // The list does not load the backfills, so a running one is refused by the server instead.
  const reason = backfillReason(rule, false);

  return (
    <>
      <DropdownMenu>
        <DropdownMenuTrigger asChild>
          <Button variant="ghost" size="icon-xs" aria-label={`Actions for ${rule.name}`}>
            <MoreHorizontal />
          </Button>
        </DropdownMenuTrigger>
        <DropdownMenuContent align="end">
          <DropdownMenuItem asChild>
            <Link
              to="/$org/$project/online-rules/$ruleId"
              params={{ org, project, ruleId: rule.id }}
            >
              Open page
            </Link>
          </DropdownMenuItem>
          {canManage ? (
            <>
              <DropdownMenuItem asChild>
                <Link
                  to="/$org/$project/online-rules/$ruleId/edit"
                  params={{ org, project, ruleId: rule.id }}
                >
                  Edit
                </Link>
              </DropdownMenuItem>
              <DropdownMenuItem
                disabled={reason !== undefined}
                title={reason}
                onSelect={() => {
                  setBackfillOpen(true);
                }}
              >
                Run on existing data
              </DropdownMenuItem>
              <DropdownMenuItem
                className="text-destructive"
                onSelect={() => {
                  setDeleteOpen(true);
                }}
              >
                Delete…
              </DropdownMenuItem>
            </>
          ) : null}
        </DropdownMenuContent>
      </DropdownMenu>
      {canManage ? (
        <>
          <BackfillDialog open={backfillOpen} onOpenChange={setBackfillOpen} rule={rule} />
          <ConfirmDialog
            open={deleteOpen}
            onOpenChange={setDeleteOpen}
            title="Delete online rule?"
            description="Its jobs and spend history are deleted too."
            confirmLabel="Delete"
            destructive
            pending={remove.isPending}
            error={
              remove.error ? problemMessage(remove.error, "Could not delete the rule.") : undefined
            }
            onConfirm={() => {
              remove.mutate(
                { params: { path: { project: projectRef(org, project), id: rule.id } } },
                {
                  onSuccess: () => {
                    setDeleteOpen(false);
                    remove.reset();
                    void queryClient.invalidateQueries({ queryKey: onlineRulesKey(org, project) });
                    void queryClient.invalidateQueries({
                      queryKey: onlineRuleStatsKey(org, project),
                    });
                    onDeleted();
                  },
                },
              );
            }}
          />
        </>
      ) : null}
    </>
  );
}
