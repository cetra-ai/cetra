import { useInfiniteQuery } from "@tanstack/react-query";
import { useNavigate } from "@tanstack/react-router";
import { ListTree } from "lucide-react";
import { useMemo } from "react";
import { visibleColumnIds } from "@/components/data-table/column-layout";
import { DataTable } from "@/components/data-table/data-table";
import { traceListOptions } from "@/components/data-table/use-infinite-list";
import { EmptyState } from "@/components/empty-state";
import { useProject } from "@/components/shell/project-context";
import {
  orderedTraceColumns,
  scoreNames,
  traceColumnSpecs,
  traceRowTone,
} from "@/components/trace-columns";
import { Skeleton } from "@/components/ui/skeleton";
import { userTraceFilter } from "@/lib/search/users";

/** The user page's Traces tab (UP6): the traces of one user, infinite scroll, opens a trace. */
export function UserTraces({
  userId,
  from,
  to,
  includeEval,
  includeExperiments,
}: {
  userId: string;
  from: string;
  to: string;
  includeEval: boolean;
  includeExperiments: boolean;
}) {
  const { org, project } = useProject();
  const navigate = useNavigate();
  const list = useInfiniteQuery(
    traceListOptions(org, project, {
      from,
      to,
      filter: userTraceFilter(userId),
      includeEval,
      includeExperiments,
    }),
  );
  const rows = useMemo(() => list.data?.pages.flatMap((page) => page.data) ?? [], [list.data]);
  const columns = useMemo(() => {
    const specs = traceColumnSpecs(scoreNames(rows));
    const visible = visibleColumnIds(specs, undefined).filter((id) => id !== "user");
    return orderedTraceColumns(rows, visible);
  }, [rows]);

  if (list.isError) {
    throw list.error;
  }
  // Until the first page arrives, an empty table would read as "No traces".
  if (list.isPending) {
    return (
      <div aria-busy="true" className="flex flex-col gap-2 p-3">
        <Skeleton className="h-6 w-full" />
        <Skeleton className="h-6 w-full" />
        <Skeleton className="h-6 w-full" />
      </div>
    );
  }

  return (
    <DataTable
      tableId="user-traces"
      ariaLabel="Traces"
      rowTone={traceRowTone}
      columns={columns}
      rows={rows}
      getRowId={(row) => row.trace_id}
      onActivate={(row) => {
        void navigate({
          to: "/$org/$project/traces/$traceId",
          params: { org, project, traceId: row.trace_id },
        });
      }}
      hasNextPage={list.hasNextPage}
      isFetchingNextPage={list.isFetchingNextPage}
      fetchNextPage={() => {
        void list.fetchNextPage();
      }}
      emptyState={<EmptyState icon={ListTree} title="No traces" />}
    />
  );
}
