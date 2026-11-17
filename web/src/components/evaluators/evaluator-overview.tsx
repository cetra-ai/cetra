import { useQuery } from "@tanstack/react-query";
import { Link } from "@tanstack/react-router";
import { useEffect } from "react";
import { $api } from "@/api/client";
import { evaluatorStatsQuery } from "@/api/queries-evaluation";
import { onlineRulesInit } from "@/api/queries-online-rules";
import type { Schema } from "@/api/types";
import { errorRate, resultText } from "@/components/evaluators/stats-cells";
import { JobsTable } from "@/components/online-rules/jobs-table";
import { ruleStatusLabel } from "@/components/online-rules/rule-form";
import { ScoreDistribution } from "@/components/online-rules/score-distribution";
import { useProject } from "@/components/shell/project-context";
import { TimeText } from "@/components/time-text";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Skeleton } from "@/components/ui/skeleton";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";
import { formatCost, formatDuration } from "@/lib/format";
import { canManageEvaluation } from "@/lib/roles";

type Evaluator = Schema<"Evaluator">;
type Rule = Schema<"OnlineRule">;
type JobCounts = Schema<"JobCounts">;

function sampleText(rate: number): string {
  return `${Number((rate * 100).toFixed(2))} %`;
}

/** "40 done · 3 failed", counting only the statuses that have jobs; "—" when there are none. */
function jobsText(counts: JobCounts): string {
  const parts: string[] = [];
  for (const status of ["done", "failed", "queued", "running", "skipped"] as const) {
    if (counts[status] > 0) {
      parts.push(`${counts[status]} ${status}`);
    }
  }
  return parts.length > 0 ? parts.join(" · ") : "—";
}

function jobsTotal(counts: JobCounts): number {
  return counts.queued + counts.running + counts.done + counts.failed + counts.skipped;
}

/**
 * The evaluator's page overview: its online rules, the score distribution of its scores and its
 * recent jobs. The rules are the project's rules that use this evaluator.
 */
export function EvaluatorOverview({
  org,
  project,
  evaluator,
}: {
  org: string;
  project: string;
  evaluator: Evaluator;
}) {
  const { data } = useProject();
  const canManage = canManageEvaluation(data.role);
  const rulesQuery = $api.useInfiniteQuery(
    "get",
    "/api/v1/projects/{project}/online-rules",
    onlineRulesInit(org, project),
    {
      pageParamName: "cursor",
      initialPageParam: null,
      getNextPageParam: (page) => page.next_cursor ?? undefined,
    },
  );
  const statsQuery = useQuery(evaluatorStatsQuery(org, project, [evaluator.id]));
  const stats = statsQuery.data?.data.find((entry) => entry.evaluator_id === evaluator.id);
  const rate = stats ? errorRate(stats.jobs) : undefined;
  const { hasNextPage, isFetchingNextPage, fetchNextPage } = rulesQuery;
  // The tiles sum every rule, so all pages are read before they are shown.
  useEffect(() => {
    if (hasNextPage && !isFetchingNextPage) {
      void fetchNextPage();
    }
  }, [hasNextPage, isFetchingNextPage, fetchNextPage]);

  if (rulesQuery.error) {
    throw rulesQuery.error;
  }
  const rules: Rule[] = (rulesQuery.data?.pages ?? [])
    .flatMap((page) => page.data)
    .filter((rule) => rule.evaluator_id === evaluator.id);
  const loading = rulesQuery.isPending || hasNextPage === true;
  const active = rules.filter((rule) => rule.enabled && rule.paused_reason == null).length;
  const jobs = rules.reduce((sum, rule) => sum + jobsTotal(rule.jobs_last_24h), 0);
  const spend = rules.reduce((sum, rule) => sum + (rule.spend?.spent_usd ?? 0), 0);

  return (
    <div className="flex flex-col gap-8">
      <dl className="grid grid-cols-3 gap-3">
        <Tile
          label="Online rules"
          value={loading ? "—" : String(rules.length)}
          sub={loading ? undefined : `${active} active`}
        />
        <Tile label="Jobs / 24h" value={loading ? "—" : String(jobs)} />
        <Tile
          label="Spend"
          value={loading ? "—" : formatCost(spend)}
          sub={loading ? undefined : "this budget period"}
        />
      </dl>

      <section aria-label="Last 7 days" className="flex flex-col gap-3">
        <h2 className="text-body font-medium">Last 7 days</h2>
        <dl className="grid grid-cols-2 gap-3 sm:grid-cols-5">
          <Tile
            label="Runs"
            value={stats ? String(stats.jobs.done + stats.jobs.failed + stats.jobs.skipped) : "—"}
            sub={
              stats
                ? `${stats.jobs.done} done · ${stats.jobs.failed} failed · ${stats.jobs.skipped} skipped`
                : undefined
            }
          />
          <Tile
            label="Error rate"
            value={rate === undefined ? "—" : `${(rate * 100).toFixed(1)}%`}
          />
          <Tile
            label="Judge cost"
            value={stats ? formatCost(stats.jobs.cost_usd_total) : "—"}
            sub={
              stats && stats.jobs.cost_usd_avg !== undefined
                ? `avg ${formatCost(stats.jobs.cost_usd_avg)}`
                : undefined
            }
          />
          <Tile
            label="Latency p50 / p95"
            value={
              stats && stats.jobs.latency_p50_ms !== undefined
                ? formatDuration(stats.jobs.latency_p50_ms * 1_000_000)
                : "—"
            }
            sub={
              stats && stats.jobs.latency_p95_ms !== undefined
                ? `p95 ${formatDuration(stats.jobs.latency_p95_ms * 1_000_000)}`
                : undefined
            }
          />
          <Tile
            label="Result"
            value={stats ? resultText(stats.scores) : "—"}
            sub={stats ? `${stats.scores.count} scores` : undefined}
          />
        </dl>
      </section>

      <section aria-label="Online rules" className="flex flex-col gap-3">
        <h2 className="text-body font-medium">Online rules</h2>
        {loading ? (
          <Skeleton className="h-8 w-full" />
        ) : rules.length === 0 ? (
          <div className="flex flex-wrap items-center gap-3">
            <p className="text-caption text-muted-foreground">
              No online rule uses this evaluator.
            </p>
            {canManage ? (
              <Button asChild variant="outline" size="sm">
                <Link to="/$org/$project/online-rules/new" params={{ org, project }}>
                  Create a rule
                </Link>
              </Button>
            ) : null}
          </div>
        ) : (
          <Table aria-label="Rules using this evaluator">
            <TableHeader>
              <TableRow>
                <TableHead>Name</TableHead>
                <TableHead>Status</TableHead>
                <TableHead>Target</TableHead>
                <TableHead>Sample</TableHead>
                <TableHead>Jobs, last 24 h</TableHead>
                <TableHead>Spend</TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {rules.map((rule) => (
                <TableRow key={rule.id}>
                  <TableCell>
                    <Link
                      to="/$org/$project/online-rules/$ruleId"
                      params={{ org, project, ruleId: rule.id }}
                      className="rounded-sm underline-offset-4 outline-none hover:underline focus-visible:outline-2 focus-visible:outline-ring focus-visible:outline-offset-2 focus-visible:outline-solid"
                    >
                      {rule.name}
                    </Link>
                  </TableCell>
                  <TableCell>
                    <Badge variant="outline">{ruleStatusLabel(rule)}</Badge>
                  </TableCell>
                  <TableCell>{rule.target === "trace" ? "Traces" : "Spans"}</TableCell>
                  <TableCell>{sampleText(rule.sample_rate)}</TableCell>
                  <TableCell>{jobsText(rule.jobs_last_24h)}</TableCell>
                  <TableCell>{formatCost(rule.spend?.spent_usd ?? 0)}</TableCell>
                </TableRow>
              ))}
            </TableBody>
          </Table>
        )}
      </section>

      <section aria-label="Score distribution">
        <ScoreDistribution org={org} project={project} filters={{ evaluator_id: evaluator.id }} />
      </section>

      <section aria-label="Recent jobs" className="flex flex-col gap-3">
        <h2 className="text-body font-medium">Recent jobs</h2>
        <JobsTable org={org} project={project} filters={{ evaluator_id: evaluator.id }} />
      </section>

      <section aria-label="Experiments" className="flex flex-col gap-3">
        <h2 className="text-body font-medium">
          Experiments with a{" "}
          <code className="font-mono text-body">{evaluator.output_score.name}</code> score
        </h2>
        <ExperimentsWithScore
          org={org}
          project={project}
          name={evaluator.output_score.name}
          pending={statsQuery.isPending}
          failed={statsQuery.isError}
          experiments={stats?.experiments_with_score}
        />
      </section>
    </div>
  );
}

function ExperimentsWithScore({
  org,
  project,
  name,
  pending,
  failed,
  experiments,
}: {
  org: string;
  project: string;
  name: string;
  pending: boolean;
  failed: boolean;
  experiments?: Schema<"ExperimentsWithScore">;
}) {
  if (pending) {
    return <Skeleton className="h-8 w-full" />;
  }
  if (failed) {
    return (
      <p role="alert" className="text-body text-destructive">
        Could not load the experiments.
      </p>
    );
  }
  if (experiments === undefined || experiments.total === 0) {
    return (
      <p className="text-body text-muted-foreground">
        No experiment has a {name} score in the last 30 days.
      </p>
    );
  }
  return (
    <>
      <ul className="flex flex-col gap-1">
        {experiments.recent.map((row) => (
          <li key={row.id} className="flex items-center gap-3 text-ui">
            <Link
              to="/$org/$project/experiments/$experimentId"
              params={{ org, project, experimentId: row.id }}
              className="underline-offset-4 hover:underline"
            >
              {row.name}
            </Link>
            <TimeText value={row.last_scored_at} style="datetime" />
          </li>
        ))}
      </ul>
      {experiments.total > experiments.recent.length ? (
        <p className="text-body text-muted-foreground">
          +{experiments.total - experiments.recent.length} more
        </p>
      ) : null}
    </>
  );
}

function Tile({ label, value, sub }: { label: string; value: string; sub?: string }) {
  return (
    <div className="flex flex-col gap-0.5 rounded-md border px-3 py-2">
      <dt className="text-caption text-muted-foreground">{label}</dt>
      <dd className="text-ui font-medium tabular-nums">{value}</dd>
      {sub !== undefined ? <dd className="text-caption text-muted-foreground">{sub}</dd> : null}
    </div>
  );
}
