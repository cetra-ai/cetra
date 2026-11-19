import { useQuery } from "@tanstack/react-query";
import { createFileRoute, Link, notFound } from "@tanstack/react-router";
import { ChevronLeft, Clock, FlaskConical, History, Loader2, Rows3 } from "lucide-react";
import { useEffect, useState } from "react";
import { $api } from "@/api/client";
import { problemOf } from "@/api/problem";
import { datasetQuery, datasetVersionsInit } from "@/api/queries-datasets";
import { experimentsInit } from "@/api/queries-experiments";
import type { Schema } from "@/api/types";
import { BACK_LINK_CLASS, BackLink } from "@/components/back-link";
import { useShortcut } from "@/components/command/use-shortcut";
import { AddItemDialog } from "@/components/datasets/add-item-dialog";
import { DatasetSettings } from "@/components/datasets/dataset-settings";
import { ExportMenu } from "@/components/datasets/export-menu";
import { ImportDialog } from "@/components/datasets/import-dialog";
import { ItemsTable } from "@/components/datasets/items-table";
import { RunExperimentDialog } from "@/components/datasets/run-experiment-dialog";
import { VersionsTable } from "@/components/datasets/versions-table";
import { DisabledReason } from "@/components/disabled-reason";
import { EmptyState } from "@/components/empty-state";
import { EvaluateSnippet } from "@/components/experiments/evaluate-snippet";
import { CompareSelection, ExperimentsTable } from "@/components/experiments/experiments-table";
import { RelativeTime } from "@/components/relative-time";
import { useProject } from "@/components/shell/project-context";
import { TopBarCrumb } from "@/components/shell/top-bar-slot";
import { MetricChip } from "@/components/trace/metric-chip";
import { Button } from "@/components/ui/button";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { Skeleton } from "@/components/ui/skeleton";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { type DatasetSearch, parseDatasetSearch } from "@/lib/datasets";
import { getTraceMaximized, setTraceMaximized } from "@/lib/list-layout";
import { canManageDatasets } from "@/lib/roles";

export const Route = createFileRoute("/$org/$project/datasets/$datasetId")({
  validateSearch: (search) => parseDatasetSearch(search),
  loader: async ({ context, params }) => {
    try {
      await context.queryClient.ensureQueryData(
        datasetQuery(params.org, params.project, params.datasetId),
      );
    } catch (error) {
      if (problemOf(error)?.status === 404) {
        throw notFound();
      }
      throw error;
    }
  },
  component: DatasetPage,
});

/** The version picker's value for "the latest version". */
const LATEST = "latest";
/** The picker's last option, which loads the next page of versions instead of choosing one. */
const LOAD_MORE = "load-more";

function DatasetPage() {
  const { org, project, data } = useProject();
  const { datasetId } = Route.useParams();
  const search = Route.useSearch();
  const navigate = Route.useNavigate();
  const canManage = canManageDatasets(data.role);
  const detail = useQuery(datasetQuery(org, project, datasetId));
  const versions = $api.useInfiniteQuery(
    "get",
    "/api/v1/projects/{project}/datasets/{dataset}/versions",
    datasetVersionsInit(org, project, datasetId),
    {
      pageParamName: "cursor",
      initialPageParam: null,
      getNextPageParam: (page) => page.next_cursor ?? undefined,
    },
  );
  const [adding, setAdding] = useState(false);
  const [importing, setImporting] = useState(false);
  // The item panel's form has typed-in changes: Escape does not close the panel then.
  const [itemDirty, setItemDirty] = useState(false);
  useShortcut("dismiss", () => {
    if (getTraceMaximized()) {
      setTraceMaximized(false);
      return;
    }
    if (!search.item || itemDirty) {
      return;
    }
    void navigate({
      to: "/$org/$project/datasets/$datasetId",
      params: { org, project, datasetId },
      search: (prev) => withItem(prev, undefined),
      // Keeps the history state, which carries the way back to where the dataset was opened from.
      state: true,
    });
  });
  const latestVersion = detail.data?.current_version;
  // A version at or above the latest one is the latest version, so the URL drops it.
  const redundant =
    search.version !== undefined && latestVersion !== undefined && search.version >= latestVersion;
  useEffect(() => {
    if (redundant) {
      void navigate({
        to: "/$org/$project/datasets/$datasetId",
        params: { org, project, datasetId },
        search: (prev) => withVersion(prev, undefined),
        replace: true,
        state: true,
      });
    }
  }, [redundant, navigate, org, project, datasetId]);

  if (detail.isPending) {
    return null;
  }
  if (detail.isError) {
    throw detail.error;
  }
  if (versions.error) {
    throw versions.error;
  }

  const dataset = detail.data;
  const latest = dataset.current_version;
  // A version named in the URL below the latest one is read; otherwise the latest is shown.
  const pinned =
    search.version !== undefined && search.version < latest ? search.version : undefined;
  const shown = pinned ?? latest;
  const atLatest = shown === latest;
  const tab = search.tab ?? "items";
  const editable = canManage && atLatest;
  const editReason = !canManage
    ? "Only members can edit datasets"
    : !atLatest
      ? "Switch to the latest version to edit"
      : undefined;
  const loadedVersions = versions.data?.pages.flatMap((page) => page.data) ?? [];
  const pickerValue = pinned === undefined ? LATEST : String(pinned);
  // A version named in the URL but not loaded yet still shows in the picker.
  const unloadedVersion =
    pinned !== undefined && !loadedVersions.some((version) => version.version === pinned)
      ? pinned
      : undefined;

  function chooseVersion(value: string) {
    if (value === LOAD_MORE) {
      void versions.fetchNextPage();
      return;
    }
    const next = value === LATEST ? undefined : Number(value);
    void navigate({
      to: "/$org/$project/datasets/$datasetId",
      params: { org, project, datasetId },
      search: (prev) => withVersion(prev, next),
      state: true,
    });
  }

  function chooseTab(value: string) {
    const next = parseDatasetSearch({ tab: value }).tab;
    if (next === undefined) {
      return;
    }
    void navigate({
      to: "/$org/$project/datasets/$datasetId",
      params: { org, project, datasetId },
      search: (prev) => ({ ...prev, tab: next }),
      state: true,
    });
  }

  return (
    <div className="flex h-full min-h-0 w-full flex-1 flex-col">
      <header className="flex min-h-9 flex-wrap items-center gap-2 border-b px-3 py-1.5">
        <nav aria-label="Dataset" className="flex min-w-0 items-center gap-1.5">
          <BackLink
            fallback={
              <Link
                to="/$org/$project/datasets"
                params={{ org, project }}
                search={{ q: undefined, dataset: dataset.id }}
                aria-label="Back to datasets"
                className={BACK_LINK_CLASS}
              >
                <ChevronLeft aria-hidden="true" className="size-3.5" />
                Datasets
              </Link>
            }
          />
          <span aria-hidden="true" className="text-muted-foreground/60">
            /
          </span>
          <h2 className="truncate text-ui font-medium">{dataset.name}</h2>
          <TopBarCrumb>
            <span className="max-w-64 truncate text-body font-medium">{dataset.name}</span>
          </TopBarCrumb>
        </nav>
        <div className="flex flex-wrap items-center gap-1.5">
          <MetricChip icon={Rows3} label="Items">
            {dataset.item_count}
          </MetricChip>
          <MetricChip icon={History} label="Version">
            {`v${latest}`}
          </MetricChip>
          <MetricChip icon={FlaskConical} label="Experiments">
            {dataset.experiment_count}
          </MetricChip>
          <MetricChip icon={Clock} label="Updated">
            <RelativeTime value={dataset.updated_at} />
          </MetricChip>
        </div>
        <div className="ml-auto flex flex-wrap items-center gap-1.5">
          <Select value={pickerValue} onValueChange={chooseVersion}>
            <SelectTrigger aria-label="Version" size="sm" className="w-fit text-body">
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              <SelectItem value={LATEST}>{`Latest (v${latest})`}</SelectItem>
              {loadedVersions
                .filter((version) => version.version !== latest)
                .map((version) => (
                  <SelectItem key={version.version} value={String(version.version)}>
                    {`v${version.version} — ${version.description}`}
                  </SelectItem>
                ))}
              {unloadedVersion !== undefined ? (
                <SelectItem value={String(unloadedVersion)}>{`v${unloadedVersion}`}</SelectItem>
              ) : null}
              {versions.hasNextPage ? (
                <SelectItem value={LOAD_MORE}>Load more versions…</SelectItem>
              ) : null}
            </SelectContent>
          </Select>
          <RunExperimentDialog datasetId={datasetId} datasetName={dataset.name} />
          <DisabledReason reason={editReason}>
            <Button
              type="button"
              variant="outline"
              size="sm"
              disabled={editReason !== undefined}
              onClick={() => {
                setImporting(true);
              }}
            >
              Import
            </Button>
          </DisabledReason>
          <ExportMenu datasetId={datasetId} version={shown} />
          <DisabledReason reason={editReason}>
            <Button
              type="button"
              size="sm"
              disabled={editReason !== undefined}
              onClick={() => {
                setAdding(true);
              }}
            >
              Add item
            </Button>
          </DisabledReason>
        </div>
      </header>

      {dataset.description ? (
        <p className="px-3 pt-2 text-body text-muted-foreground">{dataset.description}</p>
      ) : null}

      {atLatest ? null : (
        <div
          role="status"
          className="flex flex-wrap items-center justify-between gap-2 border-b bg-muted/40 px-3 py-1 text-body"
        >
          <p>{`You are viewing version ${shown}. Items are read-only.`}</p>
          <Button
            type="button"
            variant="outline"
            size="xs"
            onClick={() => {
              chooseVersion(LATEST);
            }}
          >
            Go to latest
          </Button>
        </div>
      )}

      <Tabs value={tab} onValueChange={chooseTab} className="flex min-h-0 flex-1 flex-col">
        <TabsList variant="line" className="h-8 shrink-0 px-3">
          <TabsTrigger value="items" className="text-body">
            Items
          </TabsTrigger>
          <TabsTrigger value="versions" className="text-body">
            Versions
          </TabsTrigger>
          <TabsTrigger value="experiments" className="text-body">
            Experiments
          </TabsTrigger>
          <TabsTrigger value="settings" className="text-body">
            Settings
          </TabsTrigger>
        </TabsList>
        <TabsContent
          value="items"
          className="flex min-h-0 flex-1 flex-col overflow-hidden px-3 pt-3 pb-3"
        >
          {/* Keyed by version: the selection and the unsaved edits belong to the version shown. */}
          <ItemsTable
            key={shown}
            datasetId={datasetId}
            version={shown}
            latest={atLatest}
            editable={editable}
            openItemId={search.item}
            onOpenItem={(id) => {
              void navigate({
                to: "/$org/$project/datasets/$datasetId",
                params: { org, project, datasetId },
                search: (prev) => withItem(prev, id),
                state: true,
              });
            }}
            onItemDirtyChange={setItemDirty}
            search={search.q}
            onSearch={(text) => {
              void navigate({
                to: "/$org/$project/datasets/$datasetId",
                params: { org, project, datasetId },
                search: (prev) => withQuery(prev, text),
                replace: true,
                state: true,
              });
            }}
            onAddItem={() => {
              setAdding(true);
            }}
            onImport={() => {
              setImporting(true);
            }}
          />
        </TabsContent>
        <TabsContent value="versions" className="min-h-0 flex-1 overflow-auto px-3 pt-3 pb-3">
          <VersionsTable datasetId={datasetId} editable={editable} />
        </TabsContent>
        <TabsContent value="experiments" className="min-h-0 flex-1 overflow-auto px-3 pt-3 pb-3">
          <ExperimentsTab
            datasetId={datasetId}
            datasetName={dataset.name}
            open={tab === "experiments"}
          />
        </TabsContent>
        <TabsContent value="settings" className="min-h-0 flex-1 overflow-auto px-3 pt-3 pb-3">
          <DatasetSettings dataset={dataset} />
        </TabsContent>
      </Tabs>

      <AddItemDialog datasetId={datasetId} open={adding} onOpenChange={setAdding} />
      <ImportDialog datasetId={datasetId} open={importing} onOpenChange={setImporting} />
    </div>
  );
}

/** The dataset's experiments: the list (no dataset column), the comparison pick and a link to the list. */
function ExperimentsTab({
  datasetId,
  datasetName,
  open,
}: {
  datasetId: string;
  datasetName: string;
  /** The tab is open; the list is requested only then. */
  open: boolean;
}) {
  const { org, project } = useProject();
  // Picked ids in the order they were picked; the first is the baseline.
  const [selected, setSelected] = useState<string[]>([]);
  const query = $api.useInfiniteQuery(
    "get",
    "/api/v1/projects/{project}/experiments",
    experimentsInit(org, project, datasetId),
    {
      pageParamName: "cursor",
      initialPageParam: null,
      getNextPageParam: (page) => page.next_cursor ?? undefined,
      enabled: open,
    },
  );

  if (query.error) {
    throw query.error;
  }

  const rows = query.data?.pages.flatMap((page) => page.data) ?? [];
  const selectedRows = selected
    .map((id) => rows.find((row) => row.id === id))
    .filter((row): row is Schema<"Experiment"> => row !== undefined);

  return (
    <div className="flex flex-col gap-4">
      <div className="flex flex-wrap items-center justify-between gap-2">
        <Link
          to="/$org/$project/experiments"
          params={{ org, project }}
          search={{ dataset: datasetId }}
          className="w-fit rounded-sm text-body text-muted-foreground underline-offset-4 outline-none hover:underline focus-visible:outline-2 focus-visible:outline-ring focus-visible:outline-offset-2 focus-visible:outline-solid"
        >
          Open in Experiments
        </Link>
      </div>
      <CompareSelection experiments={selectedRows} />
      {query.isPending ? (
        <Skeleton className="h-8 w-full" />
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
        <ExperimentsTable
          rows={rows}
          selected={selected}
          onSelectedChange={setSelected}
          hideDataset
        />
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

/** The search without `version`, and with `version` when one is given. */
function withVersion(search: DatasetSearch, version: number | undefined): DatasetSearch {
  const { version: _version, ...rest } = search;
  return version === undefined ? rest : { ...rest, version };
}

/** The search with the item search text set, or without `q` when `text` is undefined. */
function withQuery(search: DatasetSearch, text: string | undefined): DatasetSearch {
  const { q: _q, ...rest } = search;
  return text === undefined ? rest : { ...rest, q: text };
}

/** The search with the open item set, or without `item` when `id` is undefined. */
function withItem(search: DatasetSearch, id: string | undefined): DatasetSearch {
  const { item: _item, ...rest } = search;
  return id === undefined ? rest : { ...rest, item: id };
}
