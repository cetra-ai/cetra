import { createFileRoute, Link } from "@tanstack/react-router";
import { cn } from "cn";
import { ChevronLeft } from "lucide-react";
import { BACK_LINK_CLASS, BackLink } from "@/components/back-link";
import { SourceForm } from "@/components/issue-sources/source-form";
import { ITEM_TITLE_CLASS, PageToolbar, PageToolbarTitle } from "@/components/page-toolbar";
import { useProject } from "@/components/shell/project-context";

export const Route = createFileRoute("/$org/$project/issue-sources/new")({
  component: NewIssueSourcePage,
});

function NewIssueSourcePage() {
  const { org, project } = useProject();
  return (
    <div className="flex h-full min-h-0 w-full flex-col">
      <PageToolbar>
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
          <h2 className={ITEM_TITLE_CLASS}>New source</h2>
        </PageToolbarTitle>
      </PageToolbar>
      <div className="min-h-0 flex-1 overflow-auto">
        <div className={cn("flex flex-col gap-6 px-3 py-3", "max-w-3xl")}>
          <SourceForm org={org} project={project} />
        </div>
      </div>
    </div>
  );
}
