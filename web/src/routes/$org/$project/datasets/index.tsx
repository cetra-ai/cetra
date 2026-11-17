import { keepPreviousData, useQueries } from "@tanstack/react-query";
import { createFileRoute } from "@tanstack/react-router";
import { Table2 } from "lucide-react";
import { useCallback, useEffect, useMemo, useState } from "react";
import { $api } from "@/api/client";
import { datasetTrendsInit } from "@/api/queries-dataset-trends";
import { datasetsInit } from "@/api/queries-datasets";
import { useShortcut } from "@/components/command/use-shortcut";
import { DataTable } from "@/components/data-table/data-table";
import { DATASET_COLUMNS } from "@/components/datasets/dataset-columns";
import { DatasetDialog } from "@/components/datasets/dataset-dialog";
import { DatasetPanel } from "@/components/datasets/dataset-panel";
import { collectTrends, DatasetTrendsContext } from "@/components/datasets/dataset-trend";
import { DatasetsListEmpty } from "@/components/datasets/datasets-empty";
import { DatasetsSummary } from "@/components/datasets/datasets-summary";
import { DisabledReason } from "@/components/disabled-reason";
import { EmptyState } from "@/components/empty-state";
import { TraceLayoutControls } from "@/components/layout-switcher";
import { useProject } from "@/components/shell/project-context";
import { Filmstrip } from "@/components/trace/filmstrip";
import { SplitPane } from "@/components/trace/split-pane";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Skeleton } from "@/components/ui/skeleton";
import { getTraceMaximized, setTraceMaximized, useListLayout } from "@/lib/list-layout";
import { useBackState } from "@/lib/list-return";
import { canManageDatasets } from "@/lib/roles";

export const Route = createFileRoute("/$org/$project/datasets/")({
  validateSearch: (search) => ({
    q: typeof search.q === "string" && search.q.trim() ? search.q.trim() : undefined,
    dataset: typeof search.dataset === "string" && search.dataset ? search.dataset : undefined,
  }),
  component: DatasetsPage,
});

const SEARCH_DEBOUNCE_MS = 300;

function DatasetsPage() {
  const { org, project, data } = useProject();
  const search = Route.useSearch();
  const { q, dataset: openId } = search;
  const navigate = Route.useNavigate();
  const backState = useBackState("Datasets");
  const layout = useListLayout();
  const canManage = canManageDatasets(data.role);
  const [creating, setCreating] = useState(false);
  const [text, setText] = useState(q ?? "");

  // The search text follows the URL when it changes from elsewhere (back, another link).
  useEffect(() => {
    setText((current) => (current.trim() === (q ?? "") ? current : (q ?? "")));
  }, [q]);

  // The search text reaches the URL after a pause, so typing does not request on every key.
  useEffect(() => {
    const timer = setTimeout(() => {
      const next = text.trim() || undefined;
      if (next !== q) {
        void navigate({
          to: "/$org/$project/datasets",
          params: { org, project },
          search: (prev) => ({ ...prev, q: next }),
          replace: true,
        });
      }
    }, SEARCH_DEBOUNCE_MS);
    return () => clearTimeout(timer);
  }, [text, q, navigate, org, project]);

  const query = $api.useInfiniteQuery(
    "get",
    "/api/v1/projects/{project}/datasets",
    datasetsInit(org, project, q),
    {
      pageParamName: "cursor",
      initialPageParam: null,
      getNextPageParam: (page) => page.next_cursor ?? undefined,
      placeholderData: keepPreviousData,
    },
  );

  if (query.error) {
    throw query.error;
  }

  const rows = useMemo(() => query.data?.pages.flatMap((page) => page.data) ?? [], [query.data]);
  // One trends request per loaded page, keyed on the page's dataset ids.
  const pageIds = useMemo(
    () => query.data?.pages.map((page) => page.data.map((row) => row.id)) ?? [],
    [query.data],
  );
  const trendAnswers = useQueries({
    queries: pageIds.map((ids) => ({
      ...$api.queryOptions(
        "get",
        "/api/v1/projects/{project}/dataset-trends",
        datasetTrendsInit(org, project, ids),
      ),
      enabled: ids.length > 0,
    })),
  });
  const trends = collectTrends(
    pageIds.map((ids, index) => ({
      ids,
      answer: { data: trendAnswers[index]?.data, isError: trendAnswers[index]?.isError ?? false },
    })),
  );
  const fetchNextPage = useCallback(() => {
    void query.fetchNextPage();
  }, [query.fetchNextPage]);

  function openDataset(id: string, replace = false) {
    void navigate({
      to: "/$org/$project/datasets",
      params: { org, project },
      search: (prev) => ({ ...prev, dataset: id }),
      replace,
    });
  }

  function closeDataset() {
    void navigate({
      to: "/$org/$project/datasets",
      params: { org, project },
      search: (prev) => ({ ...prev, dataset: undefined }),
    });
  }

  useShortcut("dismiss", () => {
    if (getTraceMaximized()) {
      setTraceMaximized(false);
      return;
    }
    if (openId === undefined) {
      return;
    }
    closeDataset();
  });

  return (
    <div className="relative h-full min-h-0 flex-1">
      <div className="absolute inset-0">
        <SplitPane
          name="datasets"
          mode={layout}
          open={openId !== undefined}
          filmstrip={
            <Filmstrip
              items={rows.map((row) => ({
                id: row.id,
                label: row.name,
                detail: `${row.item_count} items`,
                status: "ok" as const,
              }))}
              selectedId={openId}
              onSelect={(id) => {
                openDataset(id);
              }}
              onBack={closeDataset}
            />
          }
          start={
            <div className="flex h-full min-h-0 flex-col">
              <div className="flex min-h-9 shrink-0 items-center gap-3 border-b px-3">
                <h1 className="text-ui font-medium">Datasets</h1>
                <Input
                  aria-label="Search datasets"
                  placeholder="Search by name"
                  value={text}
                  onChange={(event) => {
                    setText(event.target.value);
                  }}
                  className="h-7 max-w-xs text-body"
                />
                <div className="ml-auto">
                  <DisabledReason
                    reason={canManage ? undefined : "Only members can create datasets"}
                  >
                    <Button
                      type="button"
                      size="sm"
                      disabled={!canManage}
                      onClick={() => {
                        setCreating(true);
                      }}
                    >
                      New dataset
                    </Button>
                  </DisabledReason>
                </div>
              </div>
              {query.isPending ? (
                <div className="flex flex-col gap-2 p-3">
                  <Skeleton className="h-7 w-full" />
                  <Skeleton className="h-7 w-full" />
                  <Skeleton className="h-7 w-full" />
                </div>
              ) : (
                <>
                  <DatasetsSummary datasets={rows} more={query.hasNextPage} />
                  {rows.length === 0 ? (
                    q === undefined ? (
                      <DatasetsListEmpty
                        canCreate={canManage}
                        onCreate={() => {
                          setCreating(true);
                        }}
                      />
                    ) : (
                      <EmptyState icon={Table2} title="No dataset matches" />
                    )
                  ) : (
                    <DatasetTrendsContext value={trends}>
                      <DataTable
                        tableId="datasets"
                        ariaLabel="Datasets"
                        columns={DATASET_COLUMNS}
                        rows={rows}
                        getRowId={(row) => row.id}
                        selectedRowId={openId}
                        onActivate={(row) => {
                          openDataset(row.id);
                        }}
                        onSelectionMove={(row) => {
                          openDataset(row.id, true);
                        }}
                        hasNextPage={query.hasNextPage}
                        isFetchingNextPage={query.isFetchingNextPage}
                        fetchNextPage={fetchNextPage}
                        emptyState={null}
                      />
                    </DatasetTrendsContext>
                  )}
                </>
              )}
            </div>
          }
          end={
            openId !== undefined ? (
              <DatasetPanel
                datasetId={openId}
                row={rows.find((row) => row.id === openId)}
                layout={<TraceLayoutControls />}
                onClose={closeDataset}
              />
            ) : null
          }
        />
      </div>
      <DatasetDialog
        open={creating}
        onOpenChange={setCreating}
        // The dialog invalidates the list itself; the new dataset opens on its own page.
        onSaved={(saved) => {
          void navigate({
            to: "/$org/$project/datasets/$datasetId",
            params: { org, project, datasetId: saved.id },
            state: backState,
          });
        }}
      />
    </div>
  );
}
