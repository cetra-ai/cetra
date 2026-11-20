import { Link } from "@tanstack/react-router";
import { useState } from "react";
import { problemMessage } from "@/api/problem";
import { projectRef } from "@/api/project-ref";
import { usePatchIssue } from "@/api/queries-issues";
import type { Schema } from "@/api/types";
import { FormError } from "@/components/form/form-error";
import { MergeDialog } from "@/components/issues/merge-dialog";
import { Button } from "@/components/ui/button";
import { serializeChip } from "@/lib/dsl/dsl";

/** Most issues one "View traces" filter names: the issue and up to seven it absorbed (D-IC-16). */
export const MAX_TRACE_ISSUES = 8;

/**
 * The traces filter of an issue and the issues merged into it, and the note that says when the
 * filter leaves some out. `has_score` names are the occurrence score names (`issue:<id>`).
 */
export function groupTraceFilter(issue: Schema<"Issue">): { filter: string; note?: string } {
  const names = [issue.score_name, ...issue.merged_from.map((id) => `issue:${id}`)].slice(
    0,
    MAX_TRACE_ISSUES,
  );
  const filter = names
    .map((name) => serializeChip({ kind: "has_score", name, negated: false }))
    .join(" OR ");
  const group = issue.merged_from.length + 1;
  return group > MAX_TRACE_ISSUES
    ? {
        filter,
        note: `Showing traces of ${MAX_TRACE_ISSUES} of the ${group} issues in this group.`,
      }
    : { filter };
}

/**
 * The buttons of an issue that is not merged: its state (members and above only), "Merge into…"
 * (members and above only), and "View traces" (everyone).
 */
export function IssueActions({
  org,
  project,
  issue,
  canEdit,
}: {
  org: string;
  project: string;
  issue: Schema<"Issue">;
  canEdit: boolean;
}) {
  const patch = usePatchIssue(org, project);
  const [mergeOpen, setMergeOpen] = useState(false);
  const [error, setError] = useState<string | undefined>();
  if (issue.state === "merged") {
    return null;
  }
  const { filter, note } = groupTraceFilter(issue);

  function setState(state: Schema<"IssueState">) {
    setError(undefined);
    patch.mutate(
      {
        params: { path: { project: projectRef(org, project), id: issue.id } },
        body: { state },
      },
      {
        onError: (caught) => {
          setError(problemMessage(caught, "Could not change the state."));
        },
      },
    );
  }

  return (
    <div className="flex flex-col gap-2">
      <div className="flex flex-wrap items-center gap-2">
        {canEdit && issue.state === "open" ? (
          <>
            <Button type="button" disabled={patch.isPending} onClick={() => setState("resolved")}>
              Resolve
            </Button>
            <Button
              type="button"
              variant="outline"
              disabled={patch.isPending}
              onClick={() => setState("ignored")}
            >
              Ignore
            </Button>
          </>
        ) : null}
        {canEdit && issue.state !== "open" ? (
          <Button
            type="button"
            variant="outline"
            disabled={patch.isPending}
            onClick={() => setState("open")}
          >
            Reopen
          </Button>
        ) : null}
        {canEdit ? (
          <Button type="button" variant="outline" onClick={() => setMergeOpen(true)}>
            Merge into…
          </Button>
        ) : null}
        <Button asChild variant="outline">
          <Link to="/$org/$project/traces" params={{ org, project }} search={{ filter }}>
            View traces
          </Link>
        </Button>
      </div>
      {note ? <p className="text-caption text-muted-foreground">{note}</p> : null}
      <FormError message={error} />
      {canEdit ? (
        <MergeDialog
          org={org}
          project={project}
          issue={issue}
          open={mergeOpen}
          onOpenChange={setMergeOpen}
        />
      ) : null}
    </div>
  );
}
