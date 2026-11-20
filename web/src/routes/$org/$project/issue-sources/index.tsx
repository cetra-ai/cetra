import { useQuery } from "@tanstack/react-query";
import { createFileRoute, Link } from "@tanstack/react-router";
import { Plus, Radar } from "lucide-react";
import { issueSourcesQuery } from "@/api/queries-issues";
import { DisabledReason } from "@/components/disabled-reason";
import { EmptyState } from "@/components/empty-state";
import { SourceList } from "@/components/issue-sources/source-list";
import { useProject } from "@/components/shell/project-context";
import { Button } from "@/components/ui/button";
import { Skeleton } from "@/components/ui/skeleton";
import { canManageEvaluation } from "@/lib/roles";

export const Route = createFileRoute("/$org/$project/issue-sources/")({
  component: IssueSourcesPage,
});

const NO_PERMISSION = "Only members can manage issue sources";

function IssueSourcesPage() {
  const { org, project, data } = useProject();
  const canManage = canManageEvaluation(data.role);
  const sources = useQuery(issueSourcesQuery(org, project));

  if (sources.isError) {
    throw sources.error;
  }

  return (
    <div className="flex w-full flex-1 flex-col gap-6 overflow-auto px-4 py-6">
      <div className="flex flex-wrap items-center justify-between gap-4">
        <h2 className="text-title font-medium">Issue sources</h2>
        <DisabledReason reason={canManage ? undefined : NO_PERMISSION}>
          {canManage ? (
            <Button asChild>
              <Link to="/$org/$project/issue-sources/new" params={{ org, project }}>
                <Plus />
                New source
              </Link>
            </Button>
          ) : (
            <Button disabled>
              <Plus />
              New source
            </Button>
          )}
        </DisabledReason>
      </div>
      {sources.isPending ? (
        <Skeleton className="h-48 w-full" />
      ) : (
        <SourceList
          org={org}
          project={project}
          rows={sources.data.data}
          canManage={canManage}
          emptyState={
            <EmptyState
              icon={Radar}
              title="No issue sources yet"
              description="Add an error source to start finding issues."
            />
          }
        />
      )}
    </div>
  );
}
