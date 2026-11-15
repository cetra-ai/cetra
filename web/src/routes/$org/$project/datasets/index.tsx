import { createFileRoute, useNavigate } from "@tanstack/react-router";
import { Loader2, Table2 } from "lucide-react";
import { useEffect, useState } from "react";
import { $api } from "@/api/client";
import { datasetsInit } from "@/api/queries-datasets";
import { DatasetDialog } from "@/components/datasets/dataset-dialog";
import { DisabledReason } from "@/components/disabled-reason";
import { EmptyState } from "@/components/empty-state";
import { useProject } from "@/components/shell/project-context";
import { TimeText } from "@/components/time-text";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Skeleton } from "@/components/ui/skeleton";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";
import { canManageDatasets } from "@/lib/roles";

export const Route = createFileRoute("/$org/$project/datasets/")({
  validateSearch: (search) => ({
    q: typeof search.q === "string" && search.q.trim() ? search.q.trim() : undefined,
  }),
  component: DatasetsPage,
});

const SEARCH_DEBOUNCE_MS = 300;

function DatasetsPage() {
  const { org, project, data } = useProject();
  const { q } = Route.useSearch();
  const navigate = useNavigate();
  const canManage = canManageDatasets(data.role);
  const [creating, setCreating] = useState(false);
  const [text, setText] = useState(q ?? "");

  // The search text reaches the URL after a pause, so typing does not request on every key.
  useEffect(() => {
    const timer = setTimeout(() => {
      const next = text.trim() || undefined;
      if (next !== q) {
        void navigate({
          to: "/$org/$project/datasets",
          params: { org, project },
          search: { q: next },
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
    },
  );

  if (query.error) {
    throw query.error;
  }

  const rows = query.data?.pages.flatMap((page) => page.data) ?? [];

  return (
    <div className="mx-auto flex w-full max-w-6xl flex-1 flex-col gap-6 overflow-auto px-4 py-6">
      <div className="flex items-start justify-between gap-4">
        <div className="flex flex-col gap-1">
          <h1 className="text-lg font-medium">Datasets</h1>
          <p className="text-xs text-muted-foreground">
            Versioned sets of inputs and expected outputs for experiments.
          </p>
        </div>
        <DisabledReason reason={canManage ? undefined : "Only members can create datasets"}>
          <Button
            type="button"
            disabled={!canManage}
            onClick={() => {
              setCreating(true);
            }}
          >
            New dataset
          </Button>
        </DisabledReason>
      </div>
      <Input
        aria-label="Search datasets"
        placeholder="Search by name"
        value={text}
        onChange={(event) => {
          setText(event.target.value);
        }}
        className="max-w-xs"
      />
      {query.isPending ? (
        <>
          <Skeleton className="h-8 w-full" />
          <Skeleton className="h-8 w-full" />
          <Skeleton className="h-8 w-full" />
        </>
      ) : rows.length === 0 ? (
        q === undefined ? (
          <EmptyState
            icon={Table2}
            title="No datasets"
            description="Create one, import a CSV or JSONL file, or add items from a trace."
          />
        ) : (
          <EmptyState icon={Table2} title="No dataset matches" />
        )
      ) : (
        <Table aria-label="Datasets">
          <TableHeader>
            <TableRow>
              <TableHead>Name</TableHead>
              <TableHead>Items</TableHead>
              <TableHead>Version</TableHead>
              <TableHead>Experiments</TableHead>
              <TableHead>Updated</TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            {rows.map((dataset) => (
              <TableRow key={dataset.id}>
                {/* J5.3 adds the link to the dataset page, which this sub-phase does not create. */}
                <TableCell>{dataset.name}</TableCell>
                <TableCell>{dataset.item_count}</TableCell>
                <TableCell>v{dataset.current_version}</TableCell>
                <TableCell>{dataset.experiment_count}</TableCell>
                <TableCell>
                  <TimeText value={dataset.updated_at} style="datetime" />
                </TableCell>
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
      <DatasetDialog
        open={creating}
        onOpenChange={setCreating}
        // The dialog invalidates the list itself; the new dataset's page is J5.3's to open.
        onSaved={() => undefined}
      />
    </div>
  );
}
