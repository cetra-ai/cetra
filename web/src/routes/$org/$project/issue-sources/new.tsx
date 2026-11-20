import { createFileRoute, Link } from "@tanstack/react-router";
import { ChevronLeft } from "lucide-react";
import { BACK_LINK_CLASS, BackLink } from "@/components/back-link";
import { SourceForm } from "@/components/issue-sources/source-form";
import { useProject } from "@/components/shell/project-context";

export const Route = createFileRoute("/$org/$project/issue-sources/new")({
  component: NewIssueSourcePage,
});

function NewIssueSourcePage() {
  const { org, project } = useProject();
  return (
    <div className="mx-auto flex w-full max-w-3xl flex-1 flex-col gap-6 overflow-auto px-4 py-6">
      <div className="flex min-w-0 flex-wrap items-center gap-2">
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
        <span aria-hidden="true" className="text-muted-foreground/60">
          /
        </span>
        <h2 className="text-title font-medium">New source</h2>
      </div>
      <SourceForm org={org} project={project} />
    </div>
  );
}
