import { useQuery } from "@tanstack/react-query";
import { createFileRoute, Link, notFound } from "@tanstack/react-router";
import { ChevronLeft } from "lucide-react";
import { $api } from "@/api/client";
import { problemMessage, problemOf } from "@/api/problem";
import { projectRef } from "@/api/project-ref";
import { issueRefitsQuery, issueSourceQuery, useRefitIssueSource } from "@/api/queries-issues";
import { connectionsInit } from "@/api/queries-settings";
import type { Schema } from "@/api/types";
import { BACK_LINK_CLASS, BackLink } from "@/components/back-link";
import { DisabledReason } from "@/components/disabled-reason";
import { FormError } from "@/components/form/form-error";
import { KIND_LABELS } from "@/components/issue-sources/source-list";
import { useProject } from "@/components/shell/project-context";
import { TimeText } from "@/components/time-text";
import { Button } from "@/components/ui/button";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";
import { formatCost, formatDuration, nsBetween } from "@/lib/format";
import { canManageEvaluation } from "@/lib/roles";

type Source = Schema<"IssueSource">;
type RefitPage = Schema<"IssueRefitPage">;

const NO_PERMISSION = "Only members can manage issue sources";
const RECENT_REFIT = "A refit started less than 10 minutes ago.";
const BUDGET_TURN: Record<Schema<"BudgetPeriod">, string> = { day: "day", month: "month" };

export const Route = createFileRoute("/$org/$project/issue-sources/$sourceId/")({
  loader: async ({ context, params }) => {
    try {
      await context.queryClient.ensureQueryData(
        issueSourceQuery(params.org, params.project, params.sourceId),
      );
    } catch (error) {
      if (problemOf(error)?.status === 404) {
        throw notFound();
      }
      throw error;
    }
  },
  component: IssueSourcePage,
});

/** The source's state as a label: disabled, paused with its reason, or active. */
function sourceStateLabel(source: Pick<Source, "enabled" | "paused_reason">): string {
  if (!source.enabled) {
    return "Disabled";
  }
  switch (source.paused_reason) {
    case undefined:
      return "Active";
    case "budget":
      return "Paused: budget";
    case "connection":
      return "Paused: connection";
    default:
      return `Paused: ${source.paused_reason}`;
  }
}

/** The banner text of a paused source, or undefined when it is not paused. */
function sourcePausedMessage(source: Source): string | undefined {
  const detail = source.paused_detail ? ` (${source.paused_detail})` : "";
  switch (source.paused_reason) {
    case undefined:
      return undefined;
    case "budget": {
      const period = source.budget_period ?? "day";
      const budget = formatCost(source.budget_usd);
      return `Paused: the ${period} budget of ${budget} is spent. The source resumes when the ${BUDGET_TURN[period]} turns (UTC), or when you raise the budget.`;
    }
    case "connection":
      return `Paused: the LLM connection is failing${detail}. Fix it in Settings › LLM connections; a successful connection test resumes this source.`;
    default:
      return `Paused: ${source.paused_reason}${detail}. Fix the cause, then edit the source.`;
  }
}

/** Refetch every 5 s while a refit runs, else not at all. */
function refitRefetchInterval(data: RefitPage | undefined): number | false {
  return data?.data.some((refit) => refit.finished_at === undefined) ? 5000 : false;
}

/** The refit's duration, or "Running" while it runs. */
function refitDuration(refit: Schema<"IssueRefit">): string {
  if (refit.finished_at === undefined) {
    return "Running";
  }
  return formatDuration(nsBetween(refit.started_at, refit.finished_at));
}

function IssueSourcePage() {
  const { org, project, data } = useProject();
  const { sourceId } = Route.useParams();
  const detail = useQuery(issueSourceQuery(org, project, sourceId));
  const canManage = canManageEvaluation(data.role);

  if (detail.isPending) {
    return null;
  }
  if (detail.isError) {
    throw detail.error;
  }
  const source = detail.data;
  const llm = source.kind !== "error";

  return (
    <div className="mx-auto flex w-full max-w-3xl flex-1 flex-col gap-6 overflow-auto px-4 py-6">
      <div className="flex flex-wrap items-start justify-between gap-4">
        <div className="flex min-w-0 flex-wrap items-center gap-2">
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
          <span aria-hidden="true" className="text-muted-foreground/60">
            /
          </span>
          <h2 className="text-title font-medium">{source.name}</h2>
        </div>
        <Button asChild variant="outline">
          <Link
            to="/$org/$project/issue-sources/$sourceId/edit"
            params={{ org, project, sourceId: source.id }}
          >
            Edit
          </Link>
        </Button>
      </div>

      {sourcePausedMessage(source) ? (
        <div
          role="alert"
          className="rounded-md border border-destructive/40 bg-destructive/5 px-4 py-3 text-body"
        >
          {sourcePausedMessage(source)}
        </div>
      ) : null}

      <SourceDetails source={source} org={org} />

      {llm ? (
        <>
          <section aria-label="Spend" className="flex flex-col gap-2">
            <h2 className="text-body font-medium">Spend</h2>
            <SpendText source={source} />
          </section>

          <section aria-label="Candidates, last 24 hours" className="flex flex-col gap-2">
            <h2 className="text-body font-medium">Candidates, last 24 hours</h2>
            {source.candidates_24h ? (
              <dl className="flex flex-wrap gap-6 text-body">
                {(
                  [
                    ["queued", "Queued"],
                    ["pending", "Pending"],
                    ["assigned", "Assigned"],
                    ["dropped", "Dropped"],
                    ["failed", "Failed"],
                    ["skipped", "Skipped"],
                  ] as const
                ).map(([key, label]) => (
                  <div key={key} className="flex flex-col">
                    <dt className="text-caption text-muted-foreground">{label}</dt>
                    <dd className="text-title tabular-nums">{source.candidates_24h?.[key]}</dd>
                  </div>
                ))}
              </dl>
            ) : null}
          </section>

          <RefitsSection org={org} project={project} source={source} canManage={canManage} />
        </>
      ) : null}
    </div>
  );
}

/** The source's settings, read-only. The embedding fields are fixed after create. */
function SourceDetails({ source, org }: { source: Source; org: string }) {
  const connections = $api.useQuery(
    "get",
    "/api/v1/orgs/{org}/llm-connections",
    connectionsInit(org),
  );
  const connectionList = connections.data?.data ?? [];
  const nameOf = (id: string | undefined) =>
    connectionList.find((connection) => connection.id === id)?.name ?? "—";
  const llm = source.kind !== "error";
  return (
    <section aria-label="Details" className="flex flex-col gap-3">
      <h2 className="text-body font-medium">Details</h2>
      <dl className="grid grid-cols-[10rem_1fr] gap-x-4 gap-y-2 text-body">
        <dt className="text-muted-foreground">Kind</dt>
        <dd>{KIND_LABELS[source.kind]}</dd>
        <dt className="text-muted-foreground">State</dt>
        <dd>{sourceStateLabel(source)}</dd>
        <dt className="text-muted-foreground">Filter</dt>
        <dd className="font-mono break-all">{source.filter ?? "—"}</dd>
        {llm ? (
          <>
            {source.score_names ? (
              <>
                <dt className="text-muted-foreground">Score names</dt>
                <dd>{source.score_names.length > 0 ? source.score_names.join(", ") : "—"}</dd>
              </>
            ) : null}
            {source.feedback_sources ? (
              <>
                <dt className="text-muted-foreground">Feedback sources</dt>
                <dd>{source.feedback_sources.join(", ")}</dd>
              </>
            ) : null}
            <dt className="text-muted-foreground">Chat model</dt>
            <dd>
              {source.chat_model ?? "—"}{" "}
              <span className="text-caption text-muted-foreground">
                on {nameOf(source.chat_connection_id)}
              </span>
            </dd>
            <dt className="text-muted-foreground">Embedding model</dt>
            <dd>
              {source.embed_model ?? "—"}{" "}
              <span className="text-caption text-muted-foreground">
                on {nameOf(source.embed_connection_id)}
              </span>
            </dd>
            <dt className="text-muted-foreground">Sample rate</dt>
            <dd className="tabular-nums">
              {source.sample_rate === undefined
                ? "—"
                : `${Number((source.sample_rate * 100).toFixed(2))}%`}
            </dd>
            <dt className="text-muted-foreground">Daily cap</dt>
            <dd className="tabular-nums">{source.daily_cap ?? "—"}</dd>
            <dt className="text-muted-foreground">Join floor</dt>
            <dd className="tabular-nums">{source.floor ?? "—"}</dd>
            <dt className="text-muted-foreground">Minimum issue size</dt>
            <dd className="tabular-nums">{source.min_issue_size ?? "—"}</dd>
            <dt className="text-muted-foreground">Next refit</dt>
            <dd>
              {source.next_refit_at ? (
                <TimeText value={source.next_refit_at} style="datetime" />
              ) : (
                "—"
              )}
            </dd>
          </>
        ) : null}
      </dl>
    </section>
  );
}

/** Today's spend against the budget. A monthly budget is compared with today's spend too. */
function SpendText({ source }: { source: Source }) {
  if (source.budget_usd === undefined || source.budget_period === undefined) {
    return (
      <span className="text-body text-muted-foreground">
        No budget. Spent today: {formatCost(source.spent_today_usd)}
      </span>
    );
  }
  const period = source.budget_period === "day" ? "per day" : "per month";
  return (
    <span className="text-body tabular-nums">
      {`${formatCost(source.spent_today_usd)} spent today of ${formatCost(source.budget_usd)} ${period}`}
    </span>
  );
}

/** The refit history, newest first, and "Refit now". */
function RefitsSection({
  org,
  project,
  source,
  canManage,
}: {
  org: string;
  project: string;
  source: Source;
  canManage: boolean;
}) {
  const refits = useQuery({
    ...issueRefitsQuery(org, project, source.id),
    refetchInterval: (state) => refitRefetchInterval(state.state.data),
  });
  const refit = useRefitIssueSource(org, project);
  const ref = projectRef(org, project);
  const refitProblem = problemOf(refit.error);
  const refitError = refit.error
    ? refitProblem?.status === 409
      ? (refitProblem.detail ?? RECENT_REFIT)
      : problemMessage(refit.error, "Could not start the refit.")
    : undefined;

  return (
    <section aria-label="Refits" className="flex flex-col gap-3">
      <div className="flex flex-wrap items-center justify-between gap-2">
        <h2 className="text-body font-medium">Refits</h2>
        <DisabledReason reason={canManage ? undefined : NO_PERMISSION}>
          <Button
            type="button"
            variant="outline"
            disabled={!canManage || refit.isPending}
            onClick={() => {
              refit.mutate({ params: { path: { project: ref, id: source.id } } });
            }}
          >
            Refit now
          </Button>
        </DisabledReason>
      </div>
      <FormError message={refitError} />
      {refits.isError ? (
        <p role="alert" className="text-caption text-destructive">
          The refits could not be loaded.
        </p>
      ) : refits.isPending ? (
        <p className="text-caption text-muted-foreground">Loading refits…</p>
      ) : refits.data.data.length === 0 ? (
        <p className="text-caption text-muted-foreground">No refits yet.</p>
      ) : (
        <Table aria-label="Refit history">
          <TableHeader>
            <TableRow>
              <TableHead>Started</TableHead>
              <TableHead>Duration</TableHead>
              <TableHead>Pending</TableHead>
              <TableHead>Clusters</TableHead>
              <TableHead>Created</TableHead>
              <TableHead>Joined</TableHead>
              <TableHead>Outliers</TableHead>
              <TableHead>Cost</TableHead>
              <TableHead>Error</TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            {refits.data.data.map((item) => (
              <TableRow key={item.id}>
                <TableCell>
                  <TimeText value={item.started_at} style="datetime" />
                </TableCell>
                <TableCell className="tabular-nums">{refitDuration(item)}</TableCell>
                <TableCell className="tabular-nums">{item.pending}</TableCell>
                <TableCell className="tabular-nums">{item.clusters}</TableCell>
                <TableCell className="tabular-nums">{item.created}</TableCell>
                <TableCell className="tabular-nums">{item.joined}</TableCell>
                <TableCell className="tabular-nums">{item.outliers}</TableCell>
                <TableCell className="tabular-nums">{formatCost(item.cost_usd)}</TableCell>
                <TableCell>{item.error_class ?? "—"}</TableCell>
              </TableRow>
            ))}
          </TableBody>
        </Table>
      )}
    </section>
  );
}
