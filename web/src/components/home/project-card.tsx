import { Link, useNavigate } from "@tanstack/react-router";
import type { MouseEvent } from "react";
import type { Schema } from "@/api/types";
import { ActivitySparkline } from "@/components/home/activity-sparkline";
import { type ActivityState, activityCells } from "@/components/home/use-project-activity";
import { RelativeTime } from "@/components/relative-time";
import { Badge } from "@/components/ui/badge";
import { Skeleton } from "@/components/ui/skeleton";
import { roleLabel } from "@/lib/roles";

/** One project on the org home: its name, slug and role, and its last 24 hours (D-C-27). */
export function ProjectCard({
  org,
  project,
  activity,
}: {
  org: string;
  project: Schema<"Project">;
  activity: ActivityState;
}) {
  const navigate = useNavigate();
  const cells = activityCells(activity);

  /** A click on the card body opens the project (OH-7). A click on a link or button inside is theirs. */
  function openOnBodyClick(event: MouseEvent<HTMLElement>) {
    if (event.defaultPrevented || (event.target as Element).closest("a, button")) {
      return;
    }
    void navigate({ to: "/$org/$project/traces", params: { org, project: project.slug } });
  }

  return (
    // biome-ignore lint/a11y/useKeyWithClickEvents: the name link is the keyboard path; the click only extends it to the card body (OH-7)
    <article
      aria-label={project.name}
      onClick={openOnBodyClick}
      className="relative flex cursor-pointer flex-col gap-3 rounded-md border bg-card p-3 transition-colors hover:border-foreground/20 focus-within:border-foreground/20"
    >
      <div className="flex items-start justify-between gap-2">
        <div className="flex min-w-0 flex-col">
          <Link
            to="/$org/$project/traces"
            params={{ org, project: project.slug }}
            className="focus-ring truncate rounded-sm text-ui font-medium hover:underline after:absolute after:inset-0 after:rounded-md after:content-['']"
          >
            {project.name}
          </Link>
          <span className="truncate font-mono text-caption text-muted-foreground">
            {project.slug}
          </span>
        </div>
        {project.role ? <Badge variant="secondary">{roleLabel(project.role)}</Badge> : null}
      </div>
      <div className="h-8">
        {activity.state === "loading" ? <Skeleton className="h-8 w-full" /> : null}
        {activity.state === "ready" && activity.activity.traces > 0 ? (
          <ActivitySparkline from={activity.from} buckets={activity.activity.buckets} />
        ) : null}
        {activity.state === "ready" && activity.activity.traces === 0 ? (
          <p className="flex h-8 items-center text-caption text-muted-foreground">
            No traces in the last 24 hours
          </p>
        ) : null}
        {activity.state === "none" ? (
          <p className="flex h-8 items-center text-caption text-muted-foreground">
            Activity unavailable
          </p>
        ) : null}
      </div>
      <dl className="grid grid-cols-4 gap-2">
        <div className="flex min-w-0 flex-col">
          <dt className="text-caption text-muted-foreground">Traces</dt>
          <dd className="truncate text-body tabular-nums">
            {cells ? cells.traces : <Skeleton className="h-4 w-10" />}
          </dd>
        </div>
        <div className="flex min-w-0 flex-col">
          <dt className="text-caption text-muted-foreground">Error rate</dt>
          <dd className="truncate text-body tabular-nums">
            {cells ? cells.errorRate : <Skeleton className="h-4 w-10" />}
          </dd>
        </div>
        <div className="flex min-w-0 flex-col">
          <dt className="text-caption text-muted-foreground">Cost</dt>
          <dd className="truncate text-body tabular-nums">
            {cells ? cells.cost : <Skeleton className="h-4 w-10" />}
          </dd>
        </div>
        <div className="flex min-w-0 flex-col">
          <dt className="text-caption text-muted-foreground">Last trace</dt>
          <dd className="truncate text-body tabular-nums">
            {cells === undefined ? (
              <Skeleton className="h-4 w-10" />
            ) : cells.lastIngestAt ? (
              <RelativeTime value={cells.lastIngestAt} />
            ) : (
              "—"
            )}
          </dd>
        </div>
      </dl>
    </article>
  );
}
