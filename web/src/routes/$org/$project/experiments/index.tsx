import { keepPreviousData } from "@tanstack/react-query";
import { createFileRoute, Link, useNavigate } from "@tanstack/react-router";
import { FlaskConical, Loader2 } from "lucide-react";
import { useLayoutEffect, useRef, useState } from "react";
import { $api } from "@/api/client";
import { problemMessage } from "@/api/problem";
import { datasetsInit } from "@/api/queries-datasets";
import { experimentsInit } from "@/api/queries-experiments";
import type { Schema } from "@/api/types";
import { useShortcut } from "@/components/command/use-shortcut";
import { EmptyState } from "@/components/empty-state";
import { EvaluateSnippet } from "@/components/experiments/evaluate-snippet";
import { ExperimentPanel } from "@/components/experiments/experiment-panel";
import { CompareSelection, ExperimentsTable } from "@/components/experiments/experiments-table";
import { ExperimentsTrend, type TrendRun } from "@/components/experiments/experiments-trend";
import { useExperimentSummaries } from "@/components/experiments/use-experiment-summaries";
import { FilterChips } from "@/components/filter-chips";
import { TraceLayoutControls } from "@/components/layout-switcher";
import { PageToolbar } from "@/components/page-toolbar";
import { useProject } from "@/components/shell/project-context";
import { Filmstrip, type FilmstripItem } from "@/components/trace/filmstrip";
import { SplitPane } from "@/components/trace/split-pane";
import { Button } from "@/components/ui/button";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { Skeleton } from "@/components/ui/skeleton";
import { type ExperimentStatus, parseExperimentsSearch } from "@/lib/experiments";
import { getTraceMaximized, setTraceMaximized, useListLayout } from "@/lib/list-layout";

export const Route = createFileRoute("/$org/$project/experiments/")({
  validateSearch: (search) => parseExperimentsSearch(search),
  component: ExperimentsPage,
});

/** The filter's value for "every dataset". */
const ALL = "all";
/** The newest runs the trend plots. */
const TREND_RUNS = 25;

const STATUS_COUNTS: { status: ExperimentStatus; label: string }[] = [
  { status: "running", label: "Running" },
  { status: "completed", label: "Completed" },
  { status: "failed", label: "Failed" },
];

const STATUS_LABEL: Record<ExperimentStatus, string> = {
  running: "Running",
  completed: "Completed",
  failed: "Failed",
};

/** The filmstrip's dot for a run: a failure is an error, a completed run is ok, the rest unset. */
function filmstripStatus(status: ExperimentStatus): Schema<"SpanStatus"> {
  if (status === "failed") {
    return "error";
  }
  return status === "completed" ? "ok" : "unset";
}

function ExperimentsPage() {
  const { org, project } = useProject();
  const { dataset, status, experiment, errors, item } = Route.useSearch();
  const navigate = useNavigate();
  const layout = useListLayout();
  // Picked ids in the order they were picked; the first is the baseline.
  const [selected, setSelected] = useState<string[]>([]);
  const panelOpen = experiment !== undefined;
  const bodyRef = useRef<HTMLDivElement>(null);
  const listScroll = useRef(0);
  const wasOpen = useRef(panelOpen);

  // The list keeps its scroll while the panel opens and closes.
  useLayoutEffect(() => {
    const node = bodyRef.current;
    if (node && wasOpen.current !== panelOpen) {
      node.scrollTop = listScroll.current;
      node.dispatchEvent(new Event("scroll"));
      wasOpen.current = panelOpen;
    }
    return () => {
      if (node) {
        listScroll.current = node.scrollTop;
      }
    };
  }, [panelOpen]);

  const datasets = $api.useQuery(
    "get",
    "/api/v1/projects/{project}/datasets",
    datasetsInit(org, project),
  );
  const query = $api.useInfiniteQuery(
    "get",
    "/api/v1/projects/{project}/experiments",
    experimentsInit(org, project, dataset),
    {
      pageParamName: "cursor",
      initialPageParam: null,
      getNextPageParam: (page) => page.next_cursor ?? undefined,
      placeholderData: keepPreviousData,
    },
  );
  const summaryOf = useExperimentSummaries(query.data?.pages.flatMap((page) => page.data) ?? []);

  // A failure with no rows yet is the page's error; after rows, a failed "Load more" is inline.
  if (query.error && query.data === undefined) {
    throw query.error;
  }
  const nextPageFailure = query.data === undefined ? undefined : (query.error ?? undefined);

  const rows = query.data?.pages.flatMap((page) => page.data) ?? [];
  const selectedRows = selected
    .map((id) => rows.find((row) => row.id === id))
    .filter((row): row is Schema<"Experiment"> => row !== undefined);
  const datasetName = dataset
    ? datasets.data?.data.find((candidate) => candidate.id === dataset)?.name
    : undefined;
  const shownRows = status === undefined ? rows : rows.filter((row) => row.status === status);
  const shownCount = shownRows.length;
  const openRow = rows.find((row) => row.id === experiment);
  const trendRuns: TrendRun[] = [];
  if (dataset !== undefined) {
    for (const row of rows) {
      const found = summaryOf(row.id);
      if (row.dataset_id === dataset && found.state === "ready") {
        trendRuns.unshift({ experiment: row, summary: found.summary });
      }
    }
  }
  const trend = trendRuns.slice(-TREND_RUNS);

  useShortcut("dismiss", () => {
    if (getTraceMaximized()) {
      setTraceMaximized(false);
      return;
    }
    // An open item sheet is dismissed by its own close (Escape), not the panel.
    if (experiment === undefined || document.querySelector('[data-slot="sheet-content"]')) {
      return;
    }
    closeExperiment();
  });

  /**
   * Keeps the list's filters and opens one experiment in the panel (`?experiment=`). The errors
   * switch and the open item belong to the experiment, so they stay only when it is the same one.
   */
  function openExperiment(id: string, replace = false) {
    void navigate({
      to: "/$org/$project/experiments",
      params: { org, project },
      search: {
        ...(dataset === undefined ? {} : { dataset }),
        ...(status === undefined ? {} : { status }),
        experiment: id,
        ...(id === experiment ? { errors, item } : {}),
      },
      replace,
    });
  }

  /** Writes the open experiment's errors switch and item to the URL, replacing the entry. */
  function setPanelState(
    experimentId: string,
    next: { errors: 1 | undefined; item: string | undefined },
  ) {
    void navigate({
      to: "/$org/$project/experiments",
      params: { org, project },
      search: {
        ...(dataset === undefined ? {} : { dataset }),
        ...(status === undefined ? {} : { status }),
        experiment: experimentId,
        ...(next.errors === undefined ? {} : { errors: next.errors }),
        ...(next.item === undefined ? {} : { item: next.item }),
      },
      replace: true,
    });
  }

  /** Closes the panel and keeps the list's filters. */
  function closeExperiment() {
    void navigate({
      to: "/$org/$project/experiments",
      params: { org, project },
      search: {
        ...(dataset === undefined ? {} : { dataset }),
        ...(status === undefined ? {} : { status }),
      },
    });
  }

  /** Sets `?status=`; the active status is cleared by picking it again. */
  function setStatus(next: ExperimentStatus | undefined) {
    void navigate({
      to: "/$org/$project/experiments",
      params: { org, project },
      search: {
        ...(dataset === undefined ? {} : { dataset }),
        ...(next === undefined ? {} : { status: next }),
      },
    });
  }

  const filmstripItems: FilmstripItem[] = shownRows.map((row) => ({
    id: row.id,
    label: row.name,
    detail: STATUS_LABEL[row.status],
    status: filmstripStatus(row.status),
  }));

  const start = (
    <div className="flex h-full min-h-0 w-full flex-col">
      <PageToolbar actions={<CompareSelection experiments={selectedRows} />}>
        <FilterChips
          label="Status"
          chips={STATUS_COUNTS.map((entry) => ({
            value: entry.status,
            label: entry.label,
            count: rows.filter((row) => row.status === entry.status).length,
          }))}
          active={status}
          onSelect={setStatus}
        />
        <Select
          value={dataset ?? ALL}
          onValueChange={(value) => {
            // A selection holds experiments of one dataset, so a new filter clears it.
            setSelected([]);
            void navigate({
              to: "/$org/$project/experiments",
              params: { org, project },
              search: value === ALL ? {} : { dataset: value },
            });
          }}
        >
          <SelectTrigger aria-label="Dataset" className="h-7 w-fit min-w-44 text-body">
            <SelectValue />
          </SelectTrigger>
          <SelectContent>
            <SelectItem value={ALL}>All datasets</SelectItem>
            {(datasets.data?.data ?? []).map((candidate) => (
              <SelectItem key={candidate.id} value={candidate.id}>
                {candidate.name}
              </SelectItem>
            ))}
          </SelectContent>
        </Select>
      </PageToolbar>
      <div
        ref={bodyRef}
        data-scroll-container
        className="flex min-h-0 flex-1 flex-col gap-4 overflow-auto px-3 py-3"
      >
        {query.isPending ? (
          <>
            <Skeleton className="h-8 w-full" />
            <Skeleton className="h-8 w-full" />
            <Skeleton className="h-8 w-full" />
          </>
        ) : rows.length === 0 ? (
          <div className="mx-auto flex w-full max-w-3xl flex-col gap-4">
            <EmptyState
              icon={FlaskConical}
              title="No experiments"
              description="An experiment runs your app on every item of a dataset, scores each output, and lets you compare runs."
            />
            <p className="text-center text-body text-muted-foreground">
              Run one from your code with evaluate():
            </p>
            <EvaluateSnippet datasetName={datasetName} />
            <div className="flex justify-center">
              <Button variant="outline" asChild>
                <Link
                  to="/$org/$project/datasets"
                  params={{ org, project }}
                  search={{ q: undefined, dataset: undefined }}
                >
                  Open datasets
                </Link>
              </Button>
            </div>
          </div>
        ) : (
          <>
            {dataset !== undefined && trend.length >= 2 ? (
              <ExperimentsTrend runs={trend} onOpen={openExperiment} />
            ) : null}
            <ExperimentsTable
              rows={rows}
              status={status}
              selected={selected}
              onSelectedChange={setSelected}
              onOpen={openExperiment}
              openId={experiment}
            />
            {shownCount === 0 ? (
              <p className="text-ui text-muted-foreground">No experiments with this status.</p>
            ) : null}
          </>
        )}
        {nextPageFailure === undefined ? null : (
          <p role="alert" className="text-ui text-status-error">
            {problemMessage(nextPageFailure, "The next page could not be loaded")}
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
      </div>
    </div>
  );

  return (
    <div className="relative h-full min-h-0 flex-1">
      <div className="absolute inset-0">
        <SplitPane
          name="experiments"
          mode={layout}
          open={panelOpen}
          filmstrip={
            <Filmstrip
              items={filmstripItems}
              selectedId={experiment}
              onSelect={(id) => {
                openExperiment(id);
              }}
              onBack={closeExperiment}
            />
          }
          start={start}
          end={
            experiment === undefined ? null : (
              <ExperimentPanel
                key={experiment}
                experimentId={experiment}
                row={openRow}
                layout={<TraceLayoutControls />}
                errorsOnly={errors === 1}
                onErrorsOnlyChange={(checked) => {
                  setPanelState(experiment, { errors: checked ? 1 : undefined, item });
                }}
                item={item}
                onItemChange={(next) => {
                  setPanelState(experiment, { errors: errors === 1 ? 1 : undefined, item: next });
                }}
                onClose={closeExperiment}
              />
            )
          }
        />
      </div>
    </div>
  );
}
