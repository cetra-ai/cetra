import { Link } from "@tanstack/react-router";
import { Loader2, Table2 } from "lucide-react";
import { useState } from "react";
import { $api } from "@/api/client";
import { datasetItemsInit } from "@/api/queries-datasets";
import type { Schema } from "@/api/types";
import { ItemSheet } from "@/components/datasets/item-sheet";
import { EmptyState } from "@/components/empty-state";
import { useProject } from "@/components/shell/project-context";
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
import { previewJson } from "@/lib/datasets";
import { shortId } from "@/lib/format";

/** The items of one version of a dataset, a page at a time, with the item sheet. */
export function ItemsTable({
  datasetId,
  version,
  editable,
}: {
  datasetId: string;
  version: number;
  /** Whether the item sheet can edit: the latest version, and the user may manage datasets. */
  editable: boolean;
}) {
  const { org, project } = useProject();
  const [openItem, setOpenItem] = useState<Schema<"DatasetItem">>();
  const query = $api.useInfiniteQuery(
    "get",
    "/api/v1/projects/{project}/datasets/{dataset}/items",
    datasetItemsInit(org, project, datasetId, version),
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
    <div className="flex flex-col gap-4">
      {query.isPending ? (
        <>
          <Skeleton className="h-8 w-full" />
          <Skeleton className="h-8 w-full" />
          <Skeleton className="h-8 w-full" />
        </>
      ) : rows.length === 0 ? (
        <EmptyState
          icon={Table2}
          title="No items"
          description="Add items one by one, import a file, or use Add to dataset on a trace."
        />
      ) : (
        <Table aria-label="Items">
          <TableHeader>
            <TableRow>
              <TableHead>Item</TableHead>
              <TableHead>Input</TableHead>
              <TableHead>Expected output</TableHead>
              <TableHead>Metadata</TableHead>
              <TableHead>Source</TableHead>
              <TableHead>Added in</TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            {rows.map((item) => (
              <TableRow key={item.revision_id}>
                <TableCell>
                  <Button
                    type="button"
                    variant="link"
                    size="xs"
                    aria-label={`Open item ${item.id}`}
                    className="font-mono"
                    onClick={() => {
                      setOpenItem(item);
                    }}
                  >
                    {shortId(item.id)}
                  </Button>
                </TableCell>
                <TableCell className="max-w-xs truncate font-mono text-xs">
                  {previewJson(item.input)}
                </TableCell>
                <TableCell className="max-w-xs truncate font-mono text-xs">
                  {previewJson(item.expected_output)}
                </TableCell>
                <TableCell className="max-w-xs truncate font-mono text-xs">
                  {Object.keys(item.metadata).length === 0 ? "—" : previewJson(item.metadata)}
                </TableCell>
                <TableCell>
                  {item.source_trace_id !== undefined ? (
                    <Link
                      to="/$org/$project/traces/$traceId"
                      params={{ org, project, traceId: item.source_trace_id }}
                      search={
                        item.source_span_id !== undefined ? { span: item.source_span_id } : {}
                      }
                      className="underline-offset-4 hover:underline"
                    >
                      trace
                    </Link>
                  ) : (
                    "—"
                  )}
                </TableCell>
                <TableCell className="font-mono text-xs">{`v${item.version}`}</TableCell>
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
      <ItemSheet
        datasetId={datasetId}
        item={openItem}
        editable={editable}
        onOpenChange={(open) => {
          if (!open) {
            setOpenItem(undefined);
          }
        }}
      />
    </div>
  );
}
