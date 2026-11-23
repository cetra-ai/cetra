import { createFileRoute, Link } from "@tanstack/react-router";
import { FolderOpen, Loader2, Plus, Settings } from "lucide-react";
import { useState } from "react";
import { $api } from "@/api/client";
import { projectListInit, projectListOptions } from "@/api/queries";
import { DisabledReason } from "@/components/disabled-reason";
import { EmptyState } from "@/components/empty-state";
import { ProjectCard } from "@/components/home/project-card";
import { ProjectTable } from "@/components/home/project-table";
import { UsageStrip } from "@/components/home/usage-strip";
import { useProjectActivity } from "@/components/home/use-project-activity";
import { ProjectViewSwitch } from "@/components/home/view-switch";
import { NewProjectDialog } from "@/components/settings/new-project-dialog";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Skeleton } from "@/components/ui/skeleton";
import { filterProjects, useProjectView } from "@/lib/org-home";
import { atLeast } from "@/lib/roles";

export const Route = createFileRoute("/$org/")({
  loader: ({ context, params }) =>
    context.queryClient.ensureInfiniteQueryData(projectListOptions(params.org)),
  pendingMs: 0,
  pendingComponent: ProjectsPending,
  component: OrgHome,
});

function OrgHome() {
  const { org } = Route.useParams();
  const [creating, setCreating] = useState(false);
  const [search, setSearch] = useState("");
  const view = useProjectView();
  const { data: me } = $api.useSuspenseQuery("get", "/api/auth/me");
  const membership = me.orgs.find((item) => item.slug === org);
  const projects = $api.useInfiniteQuery(
    "get",
    "/api/v1/orgs/{org}/projects",
    projectListInit(org),
    {
      pageParamName: "cursor",
      initialPageParam: null,
      getNextPageParam: (page) => page.next_cursor ?? undefined,
    },
  );
  const activity = useProjectActivity(org, projects.data?.pages.map((page) => page.data) ?? []);

  if (projects.error) {
    throw projects.error;
  }
  if (projects.isPending) {
    return <ProjectsPending />;
  }

  const rows = projects.data.pages.flatMap((page) => page.data);
  const shown = filterProjects(rows, search);

  return (
    <main className="mx-auto flex w-full max-w-5xl flex-1 flex-col gap-6 overflow-auto px-4 py-6">
      <div className="flex items-start justify-between gap-4">
        <h1 className="text-heading font-medium">{membership?.name ?? org}</h1>
        <div className="flex shrink-0 items-center gap-2">
          <DisabledReason
            reason={
              atLeast(membership?.role, "admin")
                ? undefined
                : "Only admins and owners can create projects"
            }
          >
            <Button
              type="button"
              size="sm"
              disabled={!atLeast(membership?.role, "admin")}
              onClick={() => setCreating(true)}
            >
              <Plus />
              New project
            </Button>
          </DisabledReason>
          <Button asChild variant="outline" size="sm">
            <Link to="/$org/settings/general" params={{ org }}>
              <Settings />
              Settings
            </Link>
          </Button>
        </div>
      </div>
      <UsageStrip org={org} />
      <section className="flex flex-col gap-3">
        <div className="flex flex-wrap items-center gap-2">
          <h2 className="text-title font-medium">Projects</h2>
          <span className="text-body text-muted-foreground">Last 24 hours</span>
          {rows.length > 0 ? (
            <div className="ml-auto flex items-center gap-2">
              <Input
                aria-label="Search projects"
                placeholder="Search by name or slug"
                value={search}
                onChange={(event) => setSearch(event.target.value)}
                className="h-7 w-56 text-body"
              />
              <ProjectViewSwitch />
            </div>
          ) : null}
        </div>
        {rows.length === 0 ? (
          <EmptyState
            icon={FolderOpen}
            title="No projects yet"
            description="Create a project to start sending traces."
          />
        ) : shown.length === 0 ? (
          <p className="text-body text-muted-foreground">No projects match the search.</p>
        ) : view === "table" ? (
          <ProjectTable org={org} rows={shown} activity={activity} />
        ) : (
          <ul
            aria-label="Projects"
            className="grid grid-cols-1 gap-3 sm:grid-cols-2 lg:grid-cols-3"
          >
            {shown.map((project) => (
              <li key={project.id}>
                <ProjectCard org={org} project={project} activity={activity(project.id)} />
              </li>
            ))}
          </ul>
        )}
        {projects.hasNextPage ? (
          <Button
            type="button"
            variant="outline"
            className="w-fit"
            disabled={projects.isFetchingNextPage}
            onClick={() => {
              void projects.fetchNextPage();
            }}
          >
            {projects.isFetchingNextPage ? <Loader2 className="animate-spin" /> : null}
            Load more
          </Button>
        ) : null}
      </section>
      <NewProjectDialog org={org} open={creating} onOpenChange={setCreating} />
    </main>
  );
}

function ProjectsPending() {
  return (
    <main className="mx-auto flex w-full max-w-5xl flex-1 flex-col gap-3 overflow-auto px-4 py-6">
      <Skeleton className="h-8 w-40" />
      <Skeleton className="h-20 w-full" />
      <div className="grid grid-cols-1 gap-3 sm:grid-cols-2 lg:grid-cols-3">
        <Skeleton className="h-36 w-full" />
        <Skeleton className="h-36 w-full" />
        <Skeleton className="h-36 w-full" />
      </div>
    </main>
  );
}
