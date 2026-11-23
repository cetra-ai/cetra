import { Link } from "@tanstack/react-router";
import type { Schema } from "@/api/types";
import { type ActivityState, activityCells } from "@/components/home/use-project-activity";
import { RelativeTime } from "@/components/relative-time";
import { Badge } from "@/components/ui/badge";
import { Skeleton } from "@/components/ui/skeleton";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";
import { formatRetention } from "@/lib/format";
import { roleLabel } from "@/lib/roles";

/** The projects as a table: the list's columns and the last 24 hours (OH-8). */
export function ProjectTable({
  org,
  rows,
  activity,
}: {
  org: string;
  rows: readonly Schema<"Project">[];
  activity: (id: string) => ActivityState;
}) {
  return (
    <Table aria-label="Projects">
      <TableHeader>
        <TableRow>
          <TableHead>Name</TableHead>
          <TableHead>Slug</TableHead>
          <TableHead>Your role</TableHead>
          <TableHead>Retention</TableHead>
          <TableHead className="text-right">Traces</TableHead>
          <TableHead className="text-right">Error rate</TableHead>
          <TableHead className="text-right">Cost</TableHead>
          <TableHead className="text-right">Last trace</TableHead>
        </TableRow>
      </TableHeader>
      <TableBody>
        {rows.map((project) => {
          const cells = activityCells(activity(project.id));
          return (
            <TableRow key={project.id}>
              <TableCell>
                <Link
                  to="/$org/$project/traces"
                  params={{ org, project: project.slug }}
                  className="focus-ring rounded-sm hover:underline"
                >
                  {project.name}
                </Link>
              </TableCell>
              <TableCell className="font-mono">{project.slug}</TableCell>
              <TableCell>
                {project.role ? <Badge variant="secondary">{roleLabel(project.role)}</Badge> : null}
              </TableCell>
              <TableCell>{formatRetention(project.retention_days)}</TableCell>
              <TableCell className="text-right tabular-nums">
                {cells ? cells.traces : <Skeleton className="ml-auto h-4 w-10" />}
              </TableCell>
              <TableCell className="text-right tabular-nums">
                {cells ? cells.errorRate : <Skeleton className="ml-auto h-4 w-10" />}
              </TableCell>
              <TableCell className="text-right tabular-nums">
                {cells ? cells.cost : <Skeleton className="ml-auto h-4 w-10" />}
              </TableCell>
              <TableCell className="text-right tabular-nums">
                {cells === undefined ? (
                  <Skeleton className="ml-auto h-4 w-10" />
                ) : cells.lastIngestAt ? (
                  <RelativeTime value={cells.lastIngestAt} />
                ) : (
                  "—"
                )}
              </TableCell>
            </TableRow>
          );
        })}
      </TableBody>
    </Table>
  );
}
