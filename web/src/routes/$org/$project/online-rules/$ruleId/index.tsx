import { useQuery, useQueryClient } from "@tanstack/react-query";
import { createFileRoute, Link, notFound } from "@tanstack/react-router";
import { useState } from "react";
import { $api } from "@/api/client";
import { problemMessage, problemOf } from "@/api/problem";
import { projectRef } from "@/api/project-ref";
import { evaluatorQuery } from "@/api/queries-evaluation";
import {
  backfillsQuery,
  evalJobsKey,
  onlineRuleKey,
  onlineRuleQuery,
  onlineRulesKey,
} from "@/api/queries-online-rules";
import { connectionsInit } from "@/api/queries-settings";
import { DisabledReason } from "@/components/disabled-reason";
import { BackfillDialog } from "@/components/online-rules/backfill-dialog";
import { BackfillsTable } from "@/components/online-rules/backfills-table";
import { JobsTable } from "@/components/online-rules/jobs-table";
import { RuleBanner } from "@/components/online-rules/rule-banner";
import { ruleStatusLabel } from "@/components/online-rules/rule-form";
import { ScoreDistribution } from "@/components/online-rules/score-distribution";
import { useProject } from "@/components/shell/project-context";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { formatCost } from "@/lib/format";
import { useBackState } from "@/lib/list-return";
import { canManageEvaluation } from "@/lib/roles";

export const Route = createFileRoute("/$org/$project/online-rules/$ruleId/")({
  loader: async ({ context, params }) => {
    try {
      await context.queryClient.ensureQueryData(
        onlineRuleQuery(params.org, params.project, params.ruleId),
      );
    } catch (error) {
      if (problemOf(error)?.status === 404) {
        throw notFound();
      }
      throw error;
    }
  },
  component: OnlineRulePage,
});

const COUNT_LABELS = [
  ["done", "Done"],
  ["failed", "Failed"],
  ["queued", "Queued"],
  ["running", "Running"],
  ["skipped", "Skipped"],
] as const;

function sampleText(rate: number): string {
  return `${Number((rate * 100).toFixed(2))} %`;
}

function OnlineRulePage() {
  const { org, project, data } = useProject();
  const { ruleId } = Route.useParams();
  const queryClient = useQueryClient();
  const detail = useQuery(onlineRuleQuery(org, project, ruleId));
  const evaluatorId = detail.data?.evaluator_id ?? "";
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
  const resume = $api.useMutation("post", "/api/v1/projects/{project}/online-rules/{id}/resume", {
    gcTime: 0,
  });
  const canManage = canManageEvaluation(data.role);
  const [backfillOpen, setBackfillOpen] = useState(false);
  // The same query as the table's, so the button sees a running backfill as soon as the table does.
  const backfills = useQuery(backfillsQuery(org, project, ruleId));
  // The rule's name is in the loaded detail; the link is only drawn once it has loaded.
  const backState = useBackState(detail.data?.name ?? "");

  if (detail.isPending) {
    return null;
  }
  if (detail.isError) {
    throw detail.error;
  }
  const rule = detail.data;
  const config: Record<string, unknown> = evaluator.data?.config ?? {};
  const connectionId = typeof config.connection_id === "string" ? config.connection_id : undefined;
  const connectionName = (connections.data?.data ?? []).find(
    (connection) => connection.id === connectionId,
  )?.name;

  function onResume() {
    resume.mutate(
      { params: { path: { project: projectRef(org, project), id: rule.id } } },
      {
        onSuccess: () => {
          void queryClient.invalidateQueries({ queryKey: onlineRuleKey(org, project, rule.id) });
          void queryClient.invalidateQueries({ queryKey: onlineRulesKey(org, project) });
          void queryClient.invalidateQueries({ queryKey: evalJobsKey(org, project) });
        },
      },
    );
  }

  const backfillRunning = (backfills.data?.data ?? []).some(
    (backfill) => backfill.status === "running",
  );
  const backfillReason = !rule.enabled
    ? "Enable the rule to run it on existing data."
    : rule.paused_reason !== undefined
      ? "Resume the rule to run it on existing data."
      : backfillRunning
        ? "A backfill of this rule is running."
        : undefined;

  const period = rule.budget_period === "month" ? "this month" : "today";
  const spent = rule.spend?.spent_usd ?? 0;
  const budget = rule.budget_usd ?? 0;
  const fill = budget > 0 ? Math.min(100, (spent / budget) * 100) : 0;

  return (
    <div className="mx-auto flex w-full max-w-4xl flex-1 flex-col gap-6 overflow-auto px-4 py-6">
      <header className="flex flex-wrap items-start justify-between gap-4">
        <div className="flex flex-col gap-2">
          <h1 className="text-title font-medium">{rule.name}</h1>
          <div>
            <Badge variant="outline">{ruleStatusLabel(rule)}</Badge>
          </div>
        </div>
        {canManage ? (
          <div className="flex flex-wrap gap-2">
            <Button asChild variant="outline">
              <Link to="/$org/$project/online-rules/$ruleId/edit" params={{ org, project, ruleId }}>
                Edit
              </Link>
            </Button>
            {rule.paused_reason !== undefined ? (
              <Button type="button" disabled={resume.isPending} onClick={onResume}>
                Resume
              </Button>
            ) : null}
          </div>
        ) : null}
      </header>
      {resume.error ? (
        <p role="alert" className="text-body text-destructive">
          {problemMessage(resume.error, "Could not resume the rule.")}
        </p>
      ) : null}

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
                state={backState}
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
          <dt className="text-muted-foreground">Enabled</dt>
          <dd>{rule.enabled ? "Yes" : "No"}</dd>
        </dl>
      </section>

      {rule.spend !== undefined && rule.budget_usd !== undefined ? (
        <section aria-label="Spend" className="flex flex-col gap-2">
          <h2 className="text-body font-medium">Spend</h2>
          <p className="text-body">
            {`${formatCost(spent)} spent of ${formatCost(rule.budget_usd)} ${period}`}
          </p>
          {rule.spend.reserved_usd > 0 ? (
            <p className="text-caption text-muted-foreground">
              {`+${formatCost(rule.spend.reserved_usd)} reserved`}
            </p>
          ) : null}
          <div
            role="progressbar"
            aria-label="Spend"
            aria-valuemin={0}
            aria-valuemax={100}
            aria-valuenow={Math.round(fill)}
            className="h-2 rounded-full bg-muted"
          >
            <div className="h-full rounded-full bg-primary" style={{ width: `${fill}%` }} />
          </div>
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
        {rule.dropped_last_hour > 0 ? (
          <p className="text-caption text-muted-foreground">
            {`${rule.dropped_last_hour} matches dropped this hour by the 10 000 per hour cap`}
          </p>
        ) : null}
      </section>

      <section aria-label="Backfills" className="flex flex-col gap-3">
        <div className="flex flex-wrap items-center justify-between gap-2">
          <h2 className="text-body font-medium">Backfills</h2>
          {canManage ? (
            <DisabledReason reason={backfillReason}>
              <Button
                type="button"
                variant="outline"
                disabled={backfillReason !== undefined}
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
