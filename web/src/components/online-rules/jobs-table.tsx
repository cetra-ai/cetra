import { Link } from "@tanstack/react-router";
import { Loader2 } from "lucide-react";
import { $api } from "@/api/client";
import { evalJobsInit } from "@/api/queries-online-rules";
import type { Schema } from "@/api/types";
import { TimeText } from "@/components/time-text";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";
import { Tooltip, TooltipContent, TooltipProvider, TooltipTrigger } from "@/components/ui/tooltip";
import { formatCost, formatDuration, shortId } from "@/lib/format";

type Job = Schema<"EvalJob">;

export interface JobFilters {
  rule_id?: string;
  evaluator_id?: string;
  status?: Schema<"EvalJobStatus">;
  kind?: Schema<"EvalJobKind">;
}

/** The eval jobs of a project that match `filters`, newest first, with "Load more". */
export function JobsTable({
  org,
  project,
  filters,
}: {
  org: string;
  project: string;
  filters: JobFilters;
}) {
  const query = $api.useInfiniteQuery(
    "get",
    "/api/v1/projects/{project}/eval-jobs",
    evalJobsInit(org, project, filters),
    {
      pageParamName: "cursor",
      initialPageParam: null,
      getNextPageParam: (page) => page.next_cursor ?? undefined,
    },
  );
  if (query.error) {
    throw query.error;
  }
  if (query.isPending) {
    return <p className="text-xs text-muted-foreground">Loading jobs…</p>;
  }
  const jobs = query.data.pages.flatMap((page) => page.data);

  if (jobs.length === 0) {
    return <p className="text-xs text-muted-foreground">No jobs yet.</p>;
  }
  return (
    <div className="flex flex-col gap-3">
      <Table aria-label="Jobs">
        <TableHeader>
          <TableRow>
            <TableHead>Status</TableHead>
            <TableHead>Kind</TableHead>
            <TableHead>Target</TableHead>
            <TableHead>Attempts</TableHead>
            <TableHead>Error</TableHead>
            <TableHead>Cost</TableHead>
            <TableHead>Latency</TableHead>
            <TableHead>Created</TableHead>
            <TableHead>Judge trace</TableHead>
          </TableRow>
        </TableHeader>
        <TableBody>
          {jobs.map((job) => (
            <JobRow key={job.id} org={org} project={project} job={job} />
          ))}
        </TableBody>
      </Table>
      {query.hasNextPage ? (
        <Button
          type="button"
          variant="outline"
          className="w-fit"
          disabled={query.isFetchingNextPage}
          onClick={() => {
            void query.fetchNextPage();
          }}
        >
          {query.isFetchingNextPage ? <Loader2 className="animate-spin" /> : null}
          Load more
        </Button>
      ) : null}
    </div>
  );
}

function JobRow({ org, project, job }: { org: string; project: string; job: Job }) {
  return (
    <TableRow>
      <TableCell>
        <Badge variant="outline">{job.status}</Badge>
      </TableCell>
      <TableCell>{job.kind}</TableCell>
      <TableCell>
        <Link
          to="/$org/$project/traces/$traceId"
          params={{ org, project, traceId: job.trace_id }}
          search={job.span_id ? { span: job.span_id } : {}}
          className="font-mono underline-offset-4 hover:underline"
        >
          {job.span_id
            ? `${shortId(job.trace_id)} · ${shortId(job.span_id)}`
            : shortId(job.trace_id)}
        </Link>
      </TableCell>
      <TableCell className="tabular-nums">{job.attempts}</TableCell>
      <TableCell>
        {job.error ? (
          <div className="flex max-w-64 flex-col gap-0.5">
            {job.error_class ? (
              <span className="text-xs text-muted-foreground">{job.error_class}</span>
            ) : null}
            <TooltipProvider>
              <Tooltip>
                <TooltipTrigger asChild>
                  <span className="truncate">{job.error}</span>
                </TooltipTrigger>
                <TooltipContent>{job.error}</TooltipContent>
              </Tooltip>
            </TooltipProvider>
          </div>
        ) : (
          "—"
        )}
      </TableCell>
      <TableCell className="tabular-nums">{formatCost(job.cost_usd)}</TableCell>
      <TableCell className="tabular-nums">
        {job.latency_ms === undefined ? "—" : formatDuration(job.latency_ms * 1_000_000)}
      </TableCell>
      <TableCell>
        <TimeText value={job.created_at} style="datetime" />
      </TableCell>
      <TableCell>
        {job.judge_trace_id ? (
          <Link
            to="/$org/$project/traces/$traceId"
            params={{ org, project, traceId: job.judge_trace_id }}
            className="underline-offset-4 hover:underline"
          >
            View judge trace
          </Link>
        ) : (
          "—"
        )}
      </TableCell>
    </TableRow>
  );
}
