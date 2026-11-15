import { useQuery } from "@tanstack/react-query";
import { createFileRoute, Link, notFound } from "@tanstack/react-router";
import { useState } from "react";
import { $api } from "@/api/client";
import { problemOf } from "@/api/problem";
import { datasetQuery, datasetVersionsInit } from "@/api/queries-datasets";
import { AddItemDialog } from "@/components/datasets/add-item-dialog";
import { DatasetSettings } from "@/components/datasets/dataset-settings";
import { ItemsTable } from "@/components/datasets/items-table";
import { VersionsTable } from "@/components/datasets/versions-table";
import { DisabledReason } from "@/components/disabled-reason";
import { useProject } from "@/components/shell/project-context";
import { Button } from "@/components/ui/button";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { type DatasetTab, parseDatasetSearch } from "@/lib/datasets";
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
  const shown = search.version ?? latest;
  const atLatest = shown === latest;
  const tab = search.tab ?? "items";
  const editable = canManage && atLatest;
  const editReason = !canManage
    ? "Only members can edit datasets"
    : !atLatest
      ? "Switch to the latest version to edit"
      : undefined;
  const loadedVersions = versions.data?.pages.flatMap((page) => page.data) ?? [];
  const pickerValue =
    search.version === undefined || search.version === latest ? LATEST : String(search.version);
  // A version named in the URL but not loaded yet still shows in the picker.
  const unloadedVersion =
    search.version !== undefined &&
    search.version !== latest &&
    !loadedVersions.some((version) => version.version === search.version)
      ? search.version
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
    });
  }

  return (
    <div className="mx-auto flex w-full max-w-6xl flex-1 flex-col gap-6 overflow-auto px-4 py-6">
      <Link
        to="/$org/$project/datasets"
        params={{ org, project }}
        search={{ q: undefined }}
        className="w-fit rounded-sm text-xs text-muted-foreground underline-offset-4 outline-none hover:underline focus-visible:outline-2 focus-visible:outline-ring focus-visible:outline-offset-2 focus-visible:outline-solid"
      >
        Datasets
      </Link>
      <header className="flex flex-wrap items-start justify-between gap-4">
        <div className="flex flex-col gap-1">
          <h1 className="text-lg font-medium">{dataset.name}</h1>
          {dataset.description ? (
            <p className="text-xs text-muted-foreground">{dataset.description}</p>
          ) : null}
          <p className="text-xs text-muted-foreground">
            {`${dataset.item_count} items · version ${latest}`}
          </p>
        </div>
        <div className="flex flex-wrap gap-2">
          <DisabledReason reason={editReason}>
            <Button
              type="button"
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

      <div className="flex flex-wrap items-center gap-3">
        <Select value={pickerValue} onValueChange={chooseVersion}>
          <SelectTrigger aria-label="Version" className="w-fit">
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
      </div>

      {atLatest ? null : (
        <div
          role="status"
          className="flex flex-wrap items-center justify-between gap-2 rounded-md border px-3 py-2 text-sm"
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

      <Tabs value={tab} onValueChange={chooseTab} className="flex flex-col gap-4">
        <TabsList>
          <TabsTrigger value="items">Items</TabsTrigger>
          <TabsTrigger value="versions">Versions</TabsTrigger>
          <TabsTrigger value="settings">Settings</TabsTrigger>
        </TabsList>
        <TabsContent value="items">
          <ItemsTable datasetId={datasetId} version={shown} editable={editable} />
        </TabsContent>
        <TabsContent value="versions">
          <VersionsTable datasetId={datasetId} />
        </TabsContent>
        <TabsContent value="settings">
          <DatasetSettings dataset={dataset} />
        </TabsContent>
      </Tabs>

      <AddItemDialog datasetId={datasetId} open={adding} onOpenChange={setAdding} />
    </div>
  );
}

/** The search without `version`, and with `version` when one is given. */
function withVersion(
  search: { tab?: DatasetTab; version?: number },
  version: number | undefined,
): { tab?: DatasetTab; version?: number } {
  const { version: _version, ...rest } = search;
  return version === undefined ? rest : { ...rest, version };
}
