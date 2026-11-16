import { keepPreviousData } from "@tanstack/react-query";
import { createFileRoute, useNavigate } from "@tanstack/react-router";
import { FlaskConical, Loader2 } from "lucide-react";
import { useState } from "react";
import { $api } from "@/api/client";
import { problemMessage } from "@/api/problem";
import { datasetsInit } from "@/api/queries-datasets";
import { experimentsInit } from "@/api/queries-experiments";
import type { Schema } from "@/api/types";
import { EmptyState } from "@/components/empty-state";
import { EvaluateSnippet } from "@/components/experiments/evaluate-snippet";
import { CompareSelection, ExperimentsTable } from "@/components/experiments/experiments-table";
import { useProject } from "@/components/shell/project-context";
import { Button } from "@/components/ui/button";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { Skeleton } from "@/components/ui/skeleton";
import { parseExperimentsSearch } from "@/lib/experiments";

export const Route = createFileRoute("/$org/$project/experiments/")({
  validateSearch: (search) => parseExperimentsSearch(search),
  component: ExperimentsPage,
});

/** The filter's value for "every dataset". */
const ALL = "all";

function ExperimentsPage() {
  const { org, project } = useProject();
  const { dataset } = Route.useSearch();
  const navigate = useNavigate();
  // Picked ids in the order they were picked; the first is the baseline.
  const [selected, setSelected] = useState<string[]>([]);
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

  return (
    <div className="mx-auto flex w-full max-w-6xl flex-1 flex-col gap-6 overflow-auto px-4 py-6">
      <div className="flex flex-col gap-1">
        <h1 className="text-lg font-medium">Experiments</h1>
        <p className="text-xs text-muted-foreground">
          Runs of your app on a dataset, scored by the SDK. Pick two to four to compare.
        </p>
      </div>
      <div className="flex flex-wrap items-center gap-3">
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
          <SelectTrigger aria-label="Dataset" className="w-fit min-w-48">
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
      </div>
      <CompareSelection experiments={selectedRows} />
      {query.isPending ? (
        <>
          <Skeleton className="h-8 w-full" />
          <Skeleton className="h-8 w-full" />
          <Skeleton className="h-8 w-full" />
        </>
      ) : rows.length === 0 ? (
        <div className="flex flex-col gap-4">
          <EmptyState
            icon={FlaskConical}
            title="No experiments"
            description="Run one from your code with evaluate():"
          />
          <EvaluateSnippet datasetName={datasetName} />
        </div>
      ) : (
        <ExperimentsTable rows={rows} selected={selected} onSelectedChange={setSelected} />
      )}
      {nextPageFailure === undefined ? null : (
        <p role="alert" className="text-sm text-status-error">
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
  );
}
