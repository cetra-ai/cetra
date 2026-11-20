import { useInfiniteQuery, useQuery } from "@tanstack/react-query";
import { useNavigate } from "@tanstack/react-router";
import { useState } from "react";
import { $api, api } from "@/api/client";
import { projectRef } from "@/api/project-ref";
import { meQuery } from "@/api/queries";
import type { Schema } from "@/api/types";
import { FormError } from "@/components/form/form-error";
import { RelativeTime } from "@/components/relative-time";
import { useProject } from "@/components/shell/project-context";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
  Sheet,
  SheetContent,
  SheetDescription,
  SheetHeader,
  SheetTitle,
} from "@/components/ui/sheet";
import { Skeleton } from "@/components/ui/skeleton";
import { formatCost } from "@/lib/format";

/** The traces list path: the cache prefix a finished run invalidates. */
export const RUNS_PATH = "/api/v1/projects/{project}/traces";
/** The runs listed reach this far back; the window is fixed when the panel opens. */
const WINDOW_DAYS = 90;
const DAY_MS = 24 * 60 * 60 * 1000;
const PAGE_SIZE = 50;

type TraceRow = Schema<"TraceSummary">;

/**
 * The user's recent playground runs of the project, newest first, with `Open` to reopen one. The
 * list is the traces of the reserved `cetra-playground` environment (D-V-22).
 */
export function RecentRuns({
  open,
  onOpenChange,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
}) {
  const me = useQuery(meQuery);
  const openRun = useOpenRun();
  return (
    <Sheet open={open} onOpenChange={onOpenChange}>
      <SheetContent className="w-full overflow-y-auto sm:max-w-md">
        <SheetHeader>
          <SheetTitle>Recent runs</SheetTitle>
          <SheetDescription>Your playground runs in this project, newest first.</SheetDescription>
        </SheetHeader>
        {me.isError ? <FormError message="Could not load your runs." /> : null}
        {open && me.data !== undefined ? (
          <RunList
            userId={me.data.user.id}
            onOpen={(traceId) => {
              onOpenChange(false);
              openRun(traceId);
            }}
          />
        ) : null}
        {open && me.data === undefined && !me.isError ? <Skeleton className="h-40 w-full" /> : null}
      </SheetContent>
    </Sheet>
  );
}

/** Opens a run on the playground page: `?run=<trace id>` (the page loads it). */
function useOpenRun(): (traceId: string) => void {
  const navigate = useNavigate();
  const { org, project } = useProject();
  return (traceId) => {
    void navigate({
      to: "/$org/$project/playground",
      params: { org, project },
      search: { run: traceId },
    });
  };
}

function RunList({ userId, onOpen }: { userId: string; onOpen: (traceId: string) => void }) {
  const { org, project } = useProject();
  // Fixed when the panel opens: the list is mounted again on each opening.
  const [range] = useState(() => {
    const to = Date.now();
    return {
      from: new Date(to - WINDOW_DAYS * DAY_MS).toISOString(),
      to: new Date(to).toISOString(),
    };
  });
  const path = { project: projectRef(org, project) };
  const query = {
    limit: PAGE_SIZE,
    from: range.from,
    to: range.to,
    filter: `environment = 'cetra-playground' AND metadata['cetra.playground.run_by'] = '${userId}'`,
  };
  const { queryKey } = $api.queryOptions(RUNS_PATH, { params: { path, query } });
  const runs = useInfiniteQuery({
    queryKey,
    initialPageParam: null as string | null,
    queryFn: async ({ pageParam, signal }: { pageParam: string | null; signal?: AbortSignal }) => {
      const { data, error } = await api.GET(RUNS_PATH, {
        params: {
          path,
          query: { ...query, ...(pageParam ? { cursor: pageParam } : {}) },
        },
        signal,
      });
      if (error) {
        throw error;
      }
      if (!data) {
        throw new Error("Empty trace list response");
      }
      return data;
    },
    getNextPageParam: (page: Schema<"TracePage">) => page.next_cursor ?? undefined,
    staleTime: 0,
  });

  if (runs.isError) {
    return <FormError message="Could not load your runs." />;
  }
  if (runs.isPending) {
    return <Skeleton className="h-40 w-full" />;
  }
  const rows: TraceRow[] = runs.data.pages.flatMap((page) => page.data);
  if (rows.length === 0) {
    return <p className="text-body text-muted-foreground">No playground runs yet.</p>;
  }
  return (
    <div className="flex flex-col gap-3">
      <ul className="flex flex-col divide-y">
        {rows.map((row) => (
          <li key={row.trace_id} className="flex items-center gap-3 py-2">
            <div className="flex min-w-0 flex-1 flex-col">
              <span className="truncate text-body">{row.models[0] ?? "No model"}</span>
              <span className="text-caption text-muted-foreground">
                <RelativeTime value={row.start_time} /> · {formatCost(row.cost_total)}
              </span>
            </div>
            {row.error_count > 0 ? <Badge variant="destructive">Error</Badge> : null}
            <Button
              type="button"
              variant="outline"
              size="sm"
              onClick={() => {
                onOpen(row.trace_id);
              }}
            >
              Open
            </Button>
          </li>
        ))}
      </ul>
      {runs.hasNextPage ? (
        <Button
          type="button"
          variant="outline"
          disabled={runs.isFetchingNextPage}
          onClick={() => {
            void runs.fetchNextPage();
          }}
        >
          Load more
        </Button>
      ) : null}
    </div>
  );
}
