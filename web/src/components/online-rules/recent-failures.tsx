import { Link } from "@tanstack/react-router";
import { $api } from "@/api/client";
import { evalJobsInit } from "@/api/queries-online-rules";
import type { Schema } from "@/api/types";
import { TimeText } from "@/components/time-text";
import { Badge } from "@/components/ui/badge";
import { Skeleton } from "@/components/ui/skeleton";

type Job = Schema<"EvalJob">;

/** One distinct failure of a rule: the jobs that failed with the same class and message. */
export interface FailureGroup {
  errorClass: Job["error_class"];
  error: string | undefined;
  count: number;
  /** The newest job of the group, which the trace link points at. */
  newest: Job;
  /** When the newest job of the group ended (or was created, if it has no end). */
  lastSeen: string;
}

/** Milliseconds of an RFC 3339 timestamp, with the nanoseconds cut off (Date.parse reads milliseconds). */
function timeMs(value: string): number {
  return Date.parse(value.replace(/\.(\d{3})\d*Z$/, ".$1Z"));
}

/**
 * Groups the failed jobs by error class and message. Groups are ordered by count, then by the
 * newest failure. Jobs that did not fail are left out.
 */
export function failureGroups(jobs: Job[]): FailureGroup[] {
  const groups = new Map<string, FailureGroup>();
  for (const job of jobs) {
    if (job.status !== "failed") {
      continue;
    }
    const lastSeen = job.finished_at ?? job.created_at;
    const key = `${job.error_class ?? ""}\n${job.error ?? ""}`;
    const group = groups.get(key);
    if (group === undefined) {
      groups.set(key, {
        errorClass: job.error_class,
        error: job.error,
        count: 1,
        newest: job,
        lastSeen,
      });
      continue;
    }
    group.count += 1;
    if (timeMs(lastSeen) > timeMs(group.lastSeen)) {
      group.newest = job;
      group.lastSeen = lastSeen;
    }
  }
  return [...groups.values()].sort(
    (a, b) => b.count - a.count || timeMs(b.lastSeen) - timeMs(a.lastSeen),
  );
}

/** The rule's latest failed jobs, grouped by error. Renders nothing when there are none. */
export function RecentFailures({
  org,
  project,
  ruleId,
}: {
  org: string;
  project: string;
  ruleId: string;
}) {
  const query = $api.useQuery(
    "get",
    "/api/v1/projects/{project}/eval-jobs",
    evalJobsInit(org, project, { rule_id: ruleId, status: "failed" }),
  );
  if (query.isPending) {
    return <Skeleton className="h-16 w-full" />;
  }
  if (query.isError) {
    return (
      <section aria-label="Recent failures" className="flex flex-col gap-3">
        <h2 className="text-body font-medium">Recent failures</h2>
        <p role="alert" className="text-caption text-destructive">
          Could not load the failures.
        </p>
      </section>
    );
  }
  const groups = failureGroups(query.data.data);
  if (groups.length === 0) {
    return null;
  }
  return (
    <section aria-label="Recent failures" className="flex flex-col gap-3">
      <h2 className="text-body font-medium">
        Recent failures{" "}
        <span className="font-normal text-muted-foreground">(latest 50 failed jobs)</span>
      </h2>
      <ul className="flex flex-col divide-y rounded-md border">
        {groups.map((group) => (
          <FailureRow
            key={`${group.errorClass ?? ""}\n${group.error ?? ""}`}
            org={org}
            project={project}
            group={group}
          />
        ))}
      </ul>
    </section>
  );
}

function FailureRow({
  org,
  project,
  group,
}: {
  org: string;
  project: string;
  group: FailureGroup;
}) {
  const { newest } = group;
  return (
    <li className="flex flex-col gap-2 p-3">
      <div className="flex flex-wrap items-center gap-2 text-caption text-muted-foreground">
        <span className="font-medium text-foreground tabular-nums">{`${group.count}×`}</span>
        {group.errorClass ? <Badge variant="outline">{group.errorClass}</Badge> : null}
        <span>
          Last seen <TimeText value={group.lastSeen} style="datetime" />
        </span>
        <Link
          to="/$org/$project/traces/$traceId"
          params={{ org, project, traceId: newest.trace_id }}
          search={newest.span_id ? { span: newest.span_id } : {}}
          className="underline-offset-4 hover:underline"
        >
          View trace
        </Link>
      </div>
      <p className="font-mono text-body break-words whitespace-pre-wrap">
        {group.error ?? "No message"}
      </p>
    </li>
  );
}
