import { useQuery, useQueryClient } from "@tanstack/react-query";
import { Link } from "@tanstack/react-router";
import { useState } from "react";
import { $api } from "@/api/client";
import { problemMessage } from "@/api/problem";
import { projectRef } from "@/api/project-ref";
import { evaluatorQuery } from "@/api/queries-evaluation";
import {
  backfillsQuery,
  evalJobsKey,
  onlineRuleKey,
  onlineRuleStatsKey,
  onlineRuleStatsQuery,
  onlineRulesKey,
} from "@/api/queries-online-rules";
import { connectionsInit } from "@/api/queries-settings";
import type { Schema } from "@/api/types";
import { DisabledReason } from "@/components/disabled-reason";
import { FormError } from "@/components/form/form-error";
import { BackfillDialog } from "@/components/online-rules/backfill-dialog";
import { BackfillsTable } from "@/components/online-rules/backfills-table";
import { HourlyJobs } from "@/components/online-rules/hourly-jobs";
import { JobsTable } from "@/components/online-rules/jobs-table";
import { RecentFailures } from "@/components/online-rules/recent-failures";
import { RuleBanner } from "@/components/online-rules/rule-banner";
import { sampleText } from "@/components/online-rules/rule-summary";
import { ScoreDistribution } from "@/components/online-rules/score-distribution";
import { ScoreTrend } from "@/components/online-rules/score-summary";
import { SpendBar } from "@/components/online-rules/spend-bar";
import { useProject } from "@/components/shell/project-context";
import { Button } from "@/components/ui/button";
import { Skeleton } from "@/components/ui/skeleton";
import { formatCost } from "@/lib/format";
import { canManageEvaluation } from "@/lib/roles";

type Rule = Schema<"OnlineRule">;

const COUNT_LABELS = [
  ["done", "Done"],
  ["failed", "Failed"],
  ["queued", "Queued"],
  ["running", "Running"],
  ["skipped", "Skipped"],
] as const;

/**
 * Why the rule cannot run a backfill now, or undefined. `running` is whether one already runs; the
 * rules list does not load the backfills and passes false (the server refuses a second one).
 */
export function backfillReason(rule: Rule, running: boolean): string | undefined {
  if (!rule.enabled) {
    return "Enable the rule to run it on existing data.";
  }
  if (rule.paused_reason !== undefined) {
    return "Resume the rule to run it on existing data.";
  }
  return running ? "A backfill of this rule is running." : undefined;
}

/** Resumes a paused rule and refreshes what shows its state; the error shows under the button. */
export function ResumeButton({
  rule,
  size = "default",
}: {
  rule: Rule;
  size?: "default" | "xs" | "sm";
}) {
  const { org, project } = useProject();
  const queryClient = useQueryClient();
  const resume = $api.useMutation("post", "/api/v1/projects/{project}/online-rules/{id}/resume", {
    gcTime: 0,
  });

  return (
    <div className="flex flex-col items-end gap-1">
      <Button
        type="button"
        size={size}
        disabled={resume.isPending}
        onClick={() => {
          resume.mutate(
            { params: { path: { project: projectRef(org, project), id: rule.id } } },
            {
              onSuccess: () => {
                void queryClient.invalidateQueries({
                  queryKey: onlineRuleKey(org, project, rule.id),
                });
                void queryClient.invalidateQueries({ queryKey: onlineRulesKey(org, project) });
                void queryClient.invalidateQueries({ queryKey: onlineRuleStatsKey(org, project) });
                void queryClient.invalidateQueries({ queryKey: evalJobsKey(org, project) });
              },
            },
          );
        }}
      >
        Resume
      </Button>
      <FormError
        message={
          resume.error ? problemMessage(resume.error, "Could not resume the rule.") : undefined
        }
      />
    </div>
  );
}

/** A rule's content: the pause banner, details, spend, the last 24 hours, backfills, scores and jobs. Shared by the rule page and the rule panel. */
export function RuleDetail({ rule }: { rule: Rule }) {
  const { org, project, data } = useProject();
  const evaluatorId = rule.evaluator_id;
  const evaluator = useQuery({
    ...evaluatorQuery(org, project, evaluatorId),
    enabled: evaluatorId !== "",
  });
  // The connection's name is only for the banner; without it the banner still reads.
  const connections = $api.useQuery(
    "get",
    "/api/v1/orgs/{org}/llm-connections",
    connectionsInit(org),
  );
  const canManage = canManageEvaluation(data.role);
  const [backfillOpen, setBackfillOpen] = useState(false);
  // The same query as the table's, so the button sees a running backfill as soon as the table does.
  const backfills = useQuery(backfillsQuery(org, project, rule.id));

  const config: Record<string, unknown> = evaluator.data?.config ?? {};
  const connectionId = typeof config.connection_id === "string" ? config.connection_id : undefined;
  const connectionName = (connections.data?.data ?? []).find(
    (connection) => connection.id === connectionId,
  )?.name;

  const backfillRunning = (backfills.data?.data ?? []).some(
    (backfill) => backfill.status === "running",
  );
  const reason = backfillReason(rule, backfillRunning);
  // The list's stats query: one entry per rule, shared with the list page's cache.
  const stats = useQuery(onlineRuleStatsQuery(org, project));
  const ruleStats = stats.data?.data.find((entry) => entry.rule_id === rule.id);

  return (
    <div className="flex flex-col gap-6">
      <RuleBanner rule={rule} connectionName={connectionName} />

      <section aria-label="Details" className="flex flex-col gap-3">
        <h2 className="text-body font-medium">Details</h2>
        <dl className="grid grid-cols-[10rem_1fr] gap-x-4 gap-y-2 text-body">
          <dt className="text-muted-foreground">Evaluator</dt>
          <dd>
            {evaluator.data ? (
              <Link
                to="/$org/$project/evaluators/$evaluatorId"
                params={{ org, project, evaluatorId }}
                className="underline-offset-4 hover:underline"
              >
                {evaluator.data.name}
              </Link>
            ) : (
              "—"
            )}
          </dd>
          <dt className="text-muted-foreground">Target</dt>
          <dd>{rule.target === "trace" ? "Traces" : "Spans"}</dd>
          <dt className="text-muted-foreground">Filter</dt>
          <dd className="font-mono break-all">{rule.filter ?? "—"}</dd>
          <dt className="text-muted-foreground">Sample rate</dt>
          <dd className="tabular-nums">{sampleText(rule.sample_rate)}</dd>
          <dt className="text-muted-foreground">Variables</dt>
          <dd>
            {Object.keys(rule.variable_mapping).length === 0 ? (
              "—"
            ) : (
              <ul className="flex flex-col gap-1 font-mono text-caption">
                {Object.entries(rule.variable_mapping).map(([variable, expression]) => (
                  <li key={variable}>
                    {variable} ← {expression}
                  </li>
                ))}
              </ul>
            )}
          </dd>
        </dl>
      </section>

      {rule.spend !== undefined && rule.budget_usd !== undefined ? (
        <section aria-label="Spend" className="flex flex-col gap-2">
          <h2 className="text-body font-medium">Spend</h2>
          <SpendBar rule={rule} />
          {rule.spend.reserved_usd > 0 ? (
            <p className="text-caption text-muted-foreground">
              {`+${formatCost(rule.spend.reserved_usd)} reserved`}
            </p>
          ) : null}
        </section>
      ) : null}

      <section aria-label="Last 24 hours" className="flex flex-col gap-2">
        <h2 className="text-body font-medium">Last 24 hours</h2>
        <dl className="flex flex-wrap gap-6 text-body">
          {COUNT_LABELS.map(([key, label]) => (
            <div key={key} className="flex flex-col">
              <dt className="text-caption text-muted-foreground">{label}</dt>
              <dd className="text-title tabular-nums">{rule.jobs_last_24h[key]}</dd>
            </div>
          ))}
        </dl>
        {stats.isPending ? (
          <Skeleton className="h-12 w-full" />
        ) : ruleStats === undefined ? null : (
          <HourlyJobs jobs={ruleStats.jobs_hourly} className="h-12 w-full" />
        )}
        {rule.dropped_last_hour > 0 ? (
          <p className="text-caption text-muted-foreground">
            {`${rule.dropped_last_hour} matches dropped this hour by the 10 000 per hour cap`}
          </p>
        ) : null}
      </section>

      <RecentFailures org={org} project={project} ruleId={rule.id} />

      <section aria-label="Backfills" className="flex flex-col gap-3">
        <div className="flex flex-wrap items-center justify-between gap-2">
          <h2 className="text-body font-medium">Backfills</h2>
          {canManage ? (
            <DisabledReason reason={reason}>
              <Button
                type="button"
                variant="outline"
                disabled={reason !== undefined}
                onClick={() => {
                  setBackfillOpen(true);
                }}
              >
                Run on existing data
              </Button>
            </DisabledReason>
          ) : null}
        </div>
        <BackfillsTable org={org} project={project} ruleId={rule.id} />
        {canManage ? (
          <BackfillDialog open={backfillOpen} onOpenChange={setBackfillOpen} rule={rule} />
        ) : null}
      </section>

      {stats.isPending || ruleStats !== undefined ? (
        <section aria-label="Scores, last 24 hours" className="flex flex-col gap-2">
          <h2 className="text-body font-medium">Scores, last 24 hours</h2>
          {ruleStats === undefined ? (
            <Skeleton className="h-12 w-full" />
          ) : (
            <ScoreTrend scores={ruleStats.scores} />
          )}
        </section>
      ) : null}

      <section aria-label="Score distribution">
        <ScoreDistribution org={org} project={project} filters={{ rule_id: rule.id }} />
      </section>

      <section aria-label="Jobs" className="flex flex-col gap-3">
        <h2 className="text-body font-medium">Jobs</h2>
        <JobsTable org={org} project={project} filters={{ rule_id: rule.id }} />
      </section>
    </div>
  );
}
