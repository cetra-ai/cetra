import { createFileRoute, Link } from "@tanstack/react-router";
import { ListChecks, Loader2 } from "lucide-react";
import { $api } from "@/api/client";
import { evaluatorsInit } from "@/api/queries-evaluation";
import { onlineRulesInit } from "@/api/queries-online-rules";
import type { Schema } from "@/api/types";
import { DisabledReason } from "@/components/disabled-reason";
import { EmptyState } from "@/components/empty-state";
import { IdText } from "@/components/id-text";
import { ruleStatusLabel } from "@/components/online-rules/rule-form";
import { useProject } from "@/components/shell/project-context";
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
import { formatCost } from "@/lib/format";
import { canManageEvaluation } from "@/lib/roles";

type Rule = Schema<"OnlineRule">;
type JobCounts = Schema<"JobCounts">;

export const Route = createFileRoute("/$org/$project/online-rules/")({
  component: OnlineRulesPage,
});

const JOB_LABELS: [keyof JobCounts, string][] = [
  ["done", "done"],
  ["failed", "failed"],
  ["queued", "queued"],
  ["running", "running"],
  ["skipped", "skipped"],
];

/** "40 done · 3 failed", counting only the statuses that have jobs; "—" when there are none. */
function jobsText(counts: JobCounts): string {
  const parts = JOB_LABELS.filter(([status]) => counts[status] > 0).map(
    ([status, label]) => `${counts[status]} ${label}`,
  );
  return parts.length > 0 ? parts.join(" · ") : "—";
}

/** "$0.42 of $5.00 today", or "—" for a rule without a budget. */
function spendText(rule: Rule): string {
  if (rule.budget_usd === undefined || rule.budget_period === undefined) {
    return "—";
  }
  const period = rule.budget_period === "day" ? "today" : "this month";
  return `${formatCost(rule.spend?.spent_usd ?? 0)} of ${formatCost(rule.budget_usd)} ${period}`;
}

function sampleText(rate: number): string {
  return `${Number((rate * 100).toFixed(2))} %`;
}

function OnlineRulesPage() {
  const { org, project, data } = useProject();
  const canManage = canManageEvaluation(data.role);
  const query = $api.useInfiniteQuery(
    "get",
    "/api/v1/projects/{project}/online-rules",
    onlineRulesInit(org, project),
    {
      pageParamName: "cursor",
      initialPageParam: null,
      getNextPageParam: (page) => page.next_cursor ?? undefined,
    },
  );
  const evaluators = $api.useQuery(
    "get",
    "/api/v1/projects/{project}/evaluators",
    evaluatorsInit(org, project),
  );

  if (query.error) {
    throw query.error;
  }

  const pages = query.data?.pages ?? [];
  const rows = pages.flatMap((page) => page.data);
  const activeRules = pages[0]?.org_enabled_rules;
  const evaluatorNames = new Map(
    (evaluators.data?.data ?? []).map((evaluator) => [evaluator.id, evaluator.name]),
  );

  return (
    <div className="mx-auto flex w-full max-w-6xl flex-1 flex-col gap-6 overflow-auto px-4 py-6">
      <div className="flex items-start justify-between gap-4">
        <div className="flex flex-col gap-1">
          <h1 className="text-lg font-medium">Online rules</h1>
          {activeRules !== undefined ? (
            <p className="text-xs text-muted-foreground">
              {activeRules === 1
                ? "1 active rule in this organization"
                : `${activeRules} active rules in this organization`}
            </p>
          ) : null}
        </div>
        <DisabledReason reason={canManage ? undefined : "Only members can create online rules"}>
          {canManage ? (
            <Button asChild>
              <Link to="/$org/$project/online-rules/new" params={{ org, project }}>
                New rule
              </Link>
            </Button>
          ) : (
            <Button disabled>New rule</Button>
          )}
        </DisabledReason>
      </div>
      {query.isPending ? (
        <>
          <Skeleton className="h-8 w-full" />
          <Skeleton className="h-8 w-full" />
          <Skeleton className="h-8 w-full" />
        </>
      ) : rows.length === 0 ? (
        <EmptyState
          icon={ListChecks}
          title="No online rules"
          description="A rule runs an evaluator on the traces or spans that match its filter."
        />
      ) : (
        <Table aria-label="Online rules">
          <TableHeader>
            <TableRow>
              <TableHead>Name</TableHead>
              <TableHead>Evaluator</TableHead>
              <TableHead>Target</TableHead>
              <TableHead>Status</TableHead>
              <TableHead>Sample</TableHead>
              <TableHead>Spend</TableHead>
              <TableHead>Last 24 h</TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            {rows.map((rule) => (
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
                  {evaluatorNames.get(rule.evaluator_id) ?? (
                    <IdText value={rule.evaluator_id} kind="evaluator" />
                  )}
                </TableCell>
                <TableCell>{rule.target === "trace" ? "Traces" : "Spans"}</TableCell>
                <TableCell>
                  <Badge variant="outline">{ruleStatusLabel(rule)}</Badge>
                </TableCell>
                <TableCell>{sampleText(rule.sample_rate)}</TableCell>
                <TableCell>{spendText(rule)}</TableCell>
                <TableCell>{jobsText(rule.jobs_last_24h)}</TableCell>
              </TableRow>
            ))}
          </TableBody>
        </Table>
      )}
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
