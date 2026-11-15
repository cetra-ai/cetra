import { keepPreviousData, useQuery } from "@tanstack/react-query";
import { createFileRoute, Link, notFound, useNavigate } from "@tanstack/react-router";
import { FlaskConical, Loader2 } from "lucide-react";
import { $api } from "@/api/client";
import { problemMessage, problemOf } from "@/api/problem";
import { experimentQuery, experimentResultsInit } from "@/api/queries-experiments";
import { EmptyState } from "@/components/empty-state";
import { EvaluateSnippet } from "@/components/experiments/evaluate-snippet";
import { StatusBadge } from "@/components/experiments/experiments-table";
import { ResultsTable } from "@/components/experiments/results-table";
import { ScoreSummaryCards } from "@/components/experiments/score-summary";
import { NotFound } from "@/components/not-found";
import { JsonView } from "@/components/payload/json-view";
import { useProject } from "@/components/shell/project-context";
import { TimeText } from "@/components/time-text";
import { Button } from "@/components/ui/button";
import { Label } from "@/components/ui/label";
import { Skeleton } from "@/components/ui/skeleton";
import { Switch } from "@/components/ui/switch";
import { isLiveExperiment, LIVE_POLL_MS, parseExperimentSearch } from "@/lib/experiments";

export const Route = createFileRoute("/$org/$project/experiments/$experimentId")({
  validateSearch: (search) => parseExperimentSearch(search),
  loader: async ({ context, params }) => {
    try {
      await context.queryClient.ensureQueryData(
        experimentQuery(params.org, params.project, params.experimentId),
      );
    } catch (error) {
      if (problemOf(error)?.status === 404) {
        throw notFound();
      }
      throw error;
    }
  },
  component: ExperimentPage,
});

function ExperimentPage() {
  const { org, project } = useProject();
  const { experimentId } = Route.useParams();
  const search = Route.useSearch();
  const navigate = useNavigate();
  // A running experiment is polled until it finishes (or is a day old).
  const experimentData = useQuery({
    ...experimentQuery(org, project, experimentId),
    refetchInterval: (state) =>
      isLiveExperiment(state.state.data, Date.now()) ? LIVE_POLL_MS : false,
  });
  const query = $api.useInfiniteQuery(
    "get",
    "/api/v1/projects/{project}/experiments/{id}/results",
    experimentResultsInit(org, project, experimentId, search.errors === 1),
    {
      pageParamName: "cursor",
      initialPageParam: null,
      getNextPageParam: (page) => page.next_cursor ?? undefined,
      placeholderData: keepPreviousData,
      refetchInterval: isLiveExperiment(experimentData.data, Date.now()) ? LIVE_POLL_MS : false,
    },
  );

  // A failure with no data yet is the page's error; after data, a failed refetch or "Load more"
  // is shown inline. A results 404 is the experiment not found.
  if (experimentData.error && experimentData.data === undefined) {
    throw experimentData.error;
  }
  if (query.error && query.data === undefined) {
    if (problemOf(query.error)?.status === 404) {
      // A component cannot throw notFound() to the router, so it renders the same page.
      return <NotFound />;
    }
    throw query.error;
  }
  const inlineFailure = query.data === undefined ? undefined : (query.error ?? undefined);

  // Every page repeats the summary, so the first one holds it.
  const summary = query.data?.pages[0]?.summary;
  const experiment = summary?.experiment ?? experimentData.data;
  const rows = query.data?.pages.flatMap((page) => page.rows) ?? [];

  if (experiment === undefined) {
    return (
      <div className="mx-auto flex w-full max-w-6xl flex-1 flex-col gap-6 px-4 py-6">
        <Skeleton className="h-8 w-64" />
        <Skeleton className="h-24 w-full" />
      </div>
    );
  }

  const datasetLabel = `${experiment.dataset_name} v${experiment.dataset_version}`;
  const hasMetadata = Object.keys(experiment.metadata).length > 0;

  return (
    <div className="mx-auto flex w-full max-w-6xl flex-1 flex-col gap-6 overflow-auto px-4 py-6">
      <Link
        to="/$org/$project/experiments"
        params={{ org, project }}
        search={{}}
        className="w-fit rounded-sm text-xs text-muted-foreground underline-offset-4 outline-none hover:underline focus-visible:outline-2 focus-visible:outline-ring focus-visible:outline-offset-2 focus-visible:outline-solid"
      >
        Experiments
      </Link>
      <header className="flex flex-col gap-2">
        <h1 className="text-lg font-medium">{experiment.name}</h1>
        {experiment.description ? (
          <p className="text-xs text-muted-foreground">{experiment.description}</p>
        ) : null}
        <div className="flex flex-wrap items-center gap-x-4 gap-y-1 text-sm">
          {experiment.dataset_id !== undefined ? (
            <Link
              to="/$org/$project/datasets/$datasetId"
              params={{ org, project, datasetId: experiment.dataset_id }}
              search={{ version: experiment.dataset_version }}
              className="rounded-sm underline-offset-4 outline-none hover:underline focus-visible:outline-2 focus-visible:outline-ring focus-visible:outline-offset-2 focus-visible:outline-solid"
            >
              {datasetLabel}
            </Link>
          ) : (
            <span>
              {datasetLabel} <span className="text-muted-foreground">(deleted)</span>
            </span>
          )}
          <StatusBadge experiment={experiment} />
          <span className="text-muted-foreground">
            Created <TimeText value={experiment.created_at} style="datetime" />
          </span>
          {experiment.finished_at !== undefined ? (
            <span className="text-muted-foreground">
              Finished <TimeText value={experiment.finished_at} style="datetime" />
            </span>
          ) : null}
          {experiment.repetitions > 1 ? (
            <span className="text-muted-foreground">{`${experiment.repetitions} repetitions`}</span>
          ) : null}
        </div>
        {experiment.status === "failed" ? (
          <div role="alert" className="rounded-md border border-status-error px-3 py-2 text-sm">
            {`The run failed: ${experiment.error ?? "no error was recorded"}`}
          </div>
        ) : null}
        {hasMetadata ? (
          <details className="text-sm">
            <summary className="w-fit cursor-pointer text-muted-foreground">Metadata</summary>
            <div className="mt-2">
              <JsonView value={experiment.metadata} />
            </div>
          </details>
        ) : null}
      </header>

      {summary === undefined ? (
        <Skeleton className="h-24 w-full" />
      ) : (
        <ScoreSummaryCards summary={summary} />
      )}

      <section className="flex flex-col gap-3">
        <div className="flex items-center gap-2">
          <Switch
            id="errors-only"
            checked={search.errors === 1}
            onCheckedChange={(checked) => {
              void navigate({
                to: "/$org/$project/experiments/$experimentId",
                params: { org, project, experimentId },
                search: (prev) => ({ ...prev, errors: checked ? 1 : undefined }),
              });
            }}
          />
          <Label htmlFor="errors-only">Errors only</Label>
        </div>
        {query.isPending ? (
          <Skeleton className="h-8 w-full" />
        ) : rows.length === 0 && query.hasNextPage ? null : rows.length === 0 ? (
          search.errors === 1 ? (
            <EmptyState icon={FlaskConical} title="No errors" />
          ) : experiment.status === "running" ? (
            <div className="flex flex-col gap-4">
              <EmptyState icon={FlaskConical} title="Waiting for results" />
              <EvaluateSnippet datasetName={experiment.dataset_name} />
            </div>
          ) : (
            <EmptyState icon={FlaskConical} title="No results" />
          )
        ) : (
          <ResultsTable
            rows={rows}
            scoreNames={summary?.scores.map((score) => score.name) ?? []}
            repetitions={experiment.repetitions}
          />
        )}
        {inlineFailure === undefined ? null : (
          <p role="alert" className="text-sm text-status-error">
            {problemMessage(inlineFailure, "The results could not be loaded")}
          </p>
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
      </section>
    </div>
  );
}
