import { useQuery } from "@tanstack/react-query";
import { createFileRoute, Link, notFound, useNavigate } from "@tanstack/react-router";
import { cn } from "cn";
import { ChevronLeft } from "lucide-react";
import { useState } from "react";
import { problemMessage, problemOf } from "@/api/problem";
import { projectRef } from "@/api/project-ref";
import { issueSourceQuery, useDeleteIssueSource } from "@/api/queries-issues";
import { BACK_LINK_CLASS, BackLink } from "@/components/back-link";
import { ConfirmDialog } from "@/components/confirm-dialog";
import { DisabledReason } from "@/components/disabled-reason";
import { SourceForm } from "@/components/issue-sources/source-form";
import { openIssuesText } from "@/components/issue-sources/source-list";
import { ITEM_TITLE_CLASS, PageToolbar, PageToolbarTitle } from "@/components/page-toolbar";
import { useProject } from "@/components/shell/project-context";
import { Button } from "@/components/ui/button";
import { canManageEvaluation } from "@/lib/roles";

export const Route = createFileRoute("/$org/$project/issue-sources/$sourceId/edit")({
  loader: async ({ context, params }) => {
    try {
      await context.queryClient.ensureQueryData(
        issueSourceQuery(params.org, params.project, params.sourceId),
      );
    } catch (error) {
      if (problemOf(error)?.status === 404) {
        throw notFound();
      }
      throw error;
    }
  },
  component: EditIssueSourcePage,
});

const NO_PERMISSION = "Only members can manage issue sources";

function EditIssueSourcePage() {
  const { org, project, data } = useProject();
  const { sourceId } = Route.useParams();
  const navigate = useNavigate();
  const detail = useQuery(issueSourceQuery(org, project, sourceId));
  const remove = useDeleteIssueSource(org, project);
  const [confirmOpen, setConfirmOpen] = useState(false);
  const [deleteError, setDeleteError] = useState<string | undefined>();
  const canManage = canManageEvaluation(data.role);

  if (detail.isPending) {
    return null;
  }
  if (detail.isError) {
    throw detail.error;
  }
  const source = detail.data;

  function onDelete() {
    setDeleteError(undefined);
    remove.mutate(
      { params: { path: { project: projectRef(org, project), id: source.id } } },
      {
        onSuccess: () => {
          void navigate({ to: "/$org/$project/issue-sources", params: { org, project } });
        },
        onError: (caught) => {
          setDeleteError(problemMessage(caught, "Could not delete the source."));
        },
      },
    );
  }

  return (
    <div className="flex h-full min-h-0 w-full flex-col">
      <PageToolbar
        actions={
          <DisabledReason reason={canManage ? undefined : NO_PERMISSION}>
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
                  to="/$org/$project/issue-sources"
                  params={{ org, project }}
                  aria-label="Back to issue sources"
                  className={BACK_LINK_CLASS}
                >
                  <ChevronLeft aria-hidden="true" className="size-3.5" />
                  Issue sources
                </Link>
              }
            />
          }
          label="Issue source"
        >
          <h2 className={ITEM_TITLE_CLASS}>{source.name}</h2>
        </PageToolbarTitle>
      </PageToolbar>
      <div className="min-h-0 flex-1 overflow-auto">
        <div className={cn("flex flex-col gap-6 px-3 py-3", "max-w-3xl")}>
          <SourceForm key={source.id} org={org} project={project} source={source} />
          <ConfirmDialog
            open={confirmOpen}
            onOpenChange={setConfirmOpen}
            title={`Delete ${source.name}?`}
            description={`Delete this source and its ${openIssuesText(source.open_issues)}? Their occurrence scores stay on the traces.`}
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
