import { Link } from "@tanstack/react-router";
import { History, Loader2 } from "lucide-react";
import { $api } from "@/api/client";
import { datasetVersionsInit } from "@/api/queries-datasets";
import { EmptyState } from "@/components/empty-state";
import { IdText } from "@/components/id-text";
import { useProject } from "@/components/shell/project-context";
import { TimeText } from "@/components/time-text";
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

/** The version history of a dataset, newest first. A version opens the items of that version. */
export function VersionsTable({ datasetId }: { datasetId: string }) {
  const { org, project } = useProject();
  const query = $api.useInfiniteQuery(
    "get",
    "/api/v1/projects/{project}/datasets/{dataset}/versions",
    datasetVersionsInit(org, project, datasetId),
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

  if (query.isPending) {
    return (
      <div className="flex flex-col gap-2">
        <Skeleton className="h-8 w-full" />
        <Skeleton className="h-8 w-full" />
      </div>
    );
  }
  if (rows.length === 0) {
    return <EmptyState icon={History} title="No versions yet" />;
  }

  return (
    <div className="flex flex-col gap-4">
      <Table aria-label="Versions">
        <TableHeader>
          <TableRow>
            <TableHead>Version</TableHead>
            <TableHead>Description</TableHead>
            <TableHead>By</TableHead>
            <TableHead>Created</TableHead>
          </TableRow>
        </TableHeader>
        <TableBody>
          {rows.map((version) => (
            <TableRow key={version.version}>
              <TableCell>
                <Link
                  to="/$org/$project/datasets/$datasetId"
                  params={{ org, project, datasetId }}
                  search={{ tab: "items", version: version.version }}
                  className="font-mono underline-offset-4 hover:underline"
                >
                  {`v${version.version}`}
                </Link>
              </TableCell>
              <TableCell className="whitespace-normal">{version.description}</TableCell>
              <TableCell>
                <CreatedBy createdBy={version.created_by} />
              </TableCell>
              <TableCell>
                <TimeText value={version.created_at} style="datetime" />
              </TableCell>
            </TableRow>
          ))}
        </TableBody>
      </Table>
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

/** A user id, "API key" for a key (`key:<id>`), or a dash when the version has no author. */
function CreatedBy({ createdBy }: { createdBy: string | undefined }) {
  if (createdBy === undefined) {
    return "—";
  }
  if (createdBy.startsWith("key:")) {
    return "API key";
  }
  return <IdText value={createdBy} kind="user" />;
}
