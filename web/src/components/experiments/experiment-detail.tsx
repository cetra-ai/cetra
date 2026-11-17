import { keepPreviousData, useQuery } from "@tanstack/react-query";
import { Link } from "@tanstack/react-router";
import { FlaskConical, Loader2 } from "lucide-react";
import { useCallback } from "react";
import { $api } from "@/api/client";
import { problemMessage, problemOf } from "@/api/problem";
import { experimentQuery, experimentResultsInit, experimentsInit } from "@/api/queries-experiments";
import { EmptyState } from "@/components/empty-state";
import { EvaluateSnippet } from "@/components/experiments/evaluate-snippet";
import { StatusBadge } from "@/components/experiments/experiments-table";
import { ItemSheet } from "@/components/experiments/item-view";
import { ResultsTable } from "@/components/experiments/results-table";
import { ScoreSummaryCards } from "@/components/experiments/score-summary";
import { NotFound } from "@/components/not-found";
import { JsonView } from "@/components/payload/json-view";
import { useProject } from "@/components/shell/project-context";
import { TimeText } from "@/components/time-text";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Label } from "@/components/ui/label";
import { Skeleton } from "@/components/ui/skeleton";
import { Switch } from "@/components/ui/switch";
import {
  isLiveExperiment,
  itemKey,
  LIVE_POLL_MS,
  metadataTags,
  previousRuns,
} from "@/lib/experiments";
import { useBackState } from "@/lib/list-return";

/**
 * One experiment in full: its header, summary, and results, with the item view of an open row.
 * The list's panel renders it. `compact` leaves out the title (the panel's header has it). The
 * errors switch and the open item (`item`, an `itemKey`) belong to the caller, which keeps them
 * in the list's URL.
 */
export function ExperimentDetail({
  experimentId,
  compact = false,
  errorsOnly,
  onErrorsOnlyChange,
  item,
  onItemChange,
}: {
  experimentId: string;
  compact?: boolean;
  errorsOnly: boolean;
  onErrorsOnlyChange: (errorsOnly: boolean) => void;
  item: string | undefined;
  onItemChange: (item: string | undefined) => void;
}) {
  const { org, project } = useProject();
  const backState = useBackState("Experiments");
  // A running experiment is polled until it finishes (or is a day old).
  const experimentData = useQuery({
    ...experimentQuery(org, project, experimentId),
    refetchInterval: (state) =>
      isLiveExperiment(state.state.data, Date.now()) ? LIVE_POLL_MS : false,
  });
  const query = $api.useInfiniteQuery(
    "get",
    "/api/v1/projects/{project}/experiments/{id}/results",
    experimentResultsInit(org, project, experimentId, errorsOnly),
    {
      pageParamName: "cursor",
      initialPageParam: null,
      getNextPageParam: (page) => page.next_cursor ?? undefined,
      placeholderData: keepPreviousData,
      refetchInterval: isLiveExperiment(experimentData.data, Date.now()) ? LIVE_POLL_MS : false,
    },
  );

  // Every page repeats the summary, so the first one holds it.
  const summary = query.data?.pages[0]?.summary;
  const experiment = summary?.experiment ?? experimentData.data;
  const rows = query.data?.pages.flatMap((page) => page.rows) ?? [];
  const openIndex = rows.findIndex((row) => itemKey(row) === item);
  const openRow = openIndex < 0 ? undefined : rows[openIndex];
  const stepItem = useCallback(
    (delta: 1 | -1) => {
      const next = openIndex < 0 ? undefined : rows[openIndex + delta];
      if (next !== undefined) {
        onItemChange(itemKey(next));
      }
    },
    [openIndex, rows, onItemChange],
  );

  // The dataset's runs, for the previous run of this one. Only a run of a live dataset has them.
  const datasetId = experiment?.dataset_id;
  const runs = $api.useQuery(
    "get",
    "/api/v1/projects/{project}/experiments",
    experimentsInit(org, project, datasetId),
    { enabled: datasetId !== undefined },
  );
  const previousId = previousRuns(runs.data?.data ?? []).get(experimentId);

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

  if (experiment === undefined) {
    return (
      <div className="flex w-full flex-col gap-6">
        <Skeleton className="h-8 w-64" />
        <Skeleton className="h-24 w-full" />
      </div>
    );
  }

  const datasetLabel = `${experiment.dataset_name} v${experiment.dataset_version}`;
  const hasMetadata = Object.keys(experiment.metadata).length > 0;
  const tags = metadataTags(experiment.metadata);

  return (
    <div className={compact ? "flex w-full flex-col gap-5" : "flex w-full flex-col gap-6"}>
      <header className="flex flex-col gap-2">
        {compact ? null : <h1 className="text-title font-medium">{experiment.name}</h1>}
        {experiment.description ? (
          <p className="text-body text-muted-foreground">{experiment.description}</p>
        ) : null}
        <div className="flex flex-wrap items-center gap-x-4 gap-y-1 text-ui">
          {experiment.dataset_id !== undefined ? (
            <Link
              to="/$org/$project/datasets/$datasetId"
              params={{ org, project, datasetId: experiment.dataset_id }}
              search={{ version: experiment.dataset_version }}
              state={backState}
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
          {previousId === undefined ? null : (
            <Button variant="outline" size="xs" asChild>
              <Link
                to="/$org/$project/experiments/compare"
                params={{ org, project }}
                search={{ ids: `${previousId},${experimentId}` }}
              >
                Compare with previous run
              </Link>
            </Button>
          )}
        </div>
        {experiment.status === "failed" ? (
          <div role="alert" className="rounded-md border border-status-error px-3 py-2 text-ui">
            {`The run failed: ${experiment.error ?? "no error was recorded"}`}
          </div>
        ) : null}
        {tags.length === 0 ? null : (
          <ul aria-label="Metadata tags" className="flex flex-wrap gap-1">
            {tags.map(([key, text]) => (
              <li key={key}>
                <Badge variant="outline" className="text-caption font-normal">
                  <span className="text-muted-foreground">{key}</span>
                  {text}
                </Badge>
              </li>
            ))}
          </ul>
        )}
        {hasMetadata ? (
          <details className="text-ui">
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
            checked={errorsOnly}
            onCheckedChange={(checked) => {
              onErrorsOnlyChange(checked);
            }}
          />
          <Label htmlFor="errors-only">Errors only</Label>
        </div>
        {query.isPending ? (
          <Skeleton className="h-8 w-full" />
        ) : rows.length === 0 && query.hasNextPage ? null : rows.length === 0 ? (
          errorsOnly ? (
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
            activeKey={item}
            onOpen={(row) => {
              onItemChange(itemKey(row));
            }}
          />
        )}
        {inlineFailure === undefined ? null : (
          <p role="alert" className="text-ui text-status-error">
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

      <ItemSheet
        row={openRow}
        onStep={stepItem}
        onClose={() => {
          onItemChange(undefined);
        }}
      />
    </div>
  );
}
