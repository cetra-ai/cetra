import { keepPreviousData, useQueryClient } from "@tanstack/react-query";
import { Link } from "@tanstack/react-router";
import { ListChecks } from "lucide-react";
import { useState } from "react";
import { $api } from "@/api/client";
import { problemMessage } from "@/api/problem";
import { projectRef } from "@/api/project-ref";
import {
  annotationQueueItemsInit,
  annotationQueueWriteKeys,
} from "@/api/queries-annotation-queues";
import type { Schema } from "@/api/types";
import { ConfirmDialog } from "@/components/confirm-dialog";
import { CreatedBy } from "@/components/created-by";
import { DataTable, type DataTableColumn } from "@/components/data-table/data-table";
import { DisabledReason } from "@/components/disabled-reason";
import { EmptyState } from "@/components/empty-state";
import { FormError } from "@/components/form/form-error";
import { IdText } from "@/components/id-text";
import { RelativeTime } from "@/components/relative-time";
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
import { parseQueueSearch, QUEUE_POLL_MS } from "@/lib/annotation-queues";
import { canManageQueues, canReview } from "@/lib/roles";

type Item = Schema<"AnnotationQueueItem">;
type Status = Schema<"QueueItemStatus">;

const ALL = "all";
const STATUS_LABEL: Record<Status, string> = {
  pending: "Pending",
  completed: "Completed",
  skipped: "Skipped",
};
const FALLBACK_REOPEN = "The item could not be reopened";
const FALLBACK_REMOVE = "The item could not be removed";

/**
 * The items of one queue, a page at a time, with a status filter. Pending items can be opened for
 * review; completed and skipped ones can be reopened. A member can remove any item. `status` is the
 * page's status search param, and `onStatusChange` applies a new one.
 */
export function ItemsTable({
  queueId,
  status,
  onStatusChange,
}: {
  queueId: string;
  status: Status | undefined;
  onStatusChange: (next: Status | undefined) => void;
}) {
  const { org, project, data } = useProject();
  const queryClient = useQueryClient();
  const reviewable = canReview(data.role);
  const manageable = canManageQueues(data.role);
  const [removing, setRemoving] = useState<Item | undefined>(undefined);
  const [failure, setFailure] = useState<string | undefined>(undefined);
  const reopen = $api.useMutation(
    "post",
    "/api/v1/projects/{project}/annotation-queues/{id}/items/{item_id}/reopen",
  );
  const remove = $api.useMutation(
    "delete",
    "/api/v1/projects/{project}/annotation-queues/{id}/items/{item_id}",
  );
  const query = $api.useInfiniteQuery(
    "get",
    "/api/v1/projects/{project}/annotation-queues/{id}/items",
    annotationQueueItemsInit(org, project, queueId, status),
    {
      pageParamName: "cursor",
      initialPageParam: null,
      getNextPageParam: (page) => page.next_cursor ?? undefined,
      placeholderData: keepPreviousData,
      refetchInterval: QUEUE_POLL_MS,
      refetchOnWindowFocus: true,
    },
  );

  if (query.error) {
    throw query.error;
  }

  const rows = query.data?.pages.flatMap((page) => page.data) ?? [];

  function invalidate() {
    for (const key of annotationQueueWriteKeys(org, project, queueId)) {
      void queryClient.invalidateQueries({ queryKey: key });
    }
  }

  function reopenItem(item: Item) {
    setFailure(undefined);
    reopen.mutate(
      { params: { path: { project: projectRef(org, project), id: queueId, item_id: item.id } } },
      {
        onSuccess: invalidate,
        onError: (error) => {
          setFailure(problemMessage(error, FALLBACK_REOPEN));
        },
      },
    );
  }

  function removeItem(item: Item) {
    remove.mutate(
      { params: { path: { project: projectRef(org, project), id: queueId, item_id: item.id } } },
      {
        onSuccess: () => {
          setRemoving(undefined);
          invalidate();
        },
      },
    );
  }

  const columns: DataTableColumn<Item>[] = [
    {
      id: "target",
      size: 240,
      header: "Target",
      enableSorting: false,
      cell: ({ row }) => <TargetCell item={row.original} />,
    },
    {
      id: "status",
      size: 112,
      header: "Status",
      enableSorting: false,
      cell: ({ row }) => STATUS_LABEL[row.original.status],
    },
    {
      id: "reviewing",
      size: 144,
      header: "Reviewing",
      enableSorting: false,
      cell: ({ row }) => <CreatedBy createdBy={row.original.leased_by_user_id} />,
    },
    {
      id: "done-by",
      size: 144,
      header: "Done by",
      enableSorting: false,
      cell: ({ row }) => <CreatedBy createdBy={row.original.done_by_user_id} />,
    },
    {
      id: "done-at",
      size: 120,
      header: "Done",
      enableSorting: false,
      cell: ({ row }) =>
        row.original.done_at !== undefined ? <RelativeTime value={row.original.done_at} /> : "—",
    },
    {
      id: "skip-reason",
      size: 200,
      header: "Skip reason",
      enableSorting: false,
      meta: { grow: true },
      cell: ({ row }) => row.original.skip_reason ?? "—",
    },
    {
      id: "added",
      size: 120,
      header: "Added",
      enableSorting: false,
      cell: ({ row }) => <RelativeTime value={row.original.created_at} />,
    },
    {
      id: "actions",
      size: 232,
      header: "",
      enableSorting: false,
      meta: { align: "right" },
      cell: ({ row }) => (
        <ItemActions
          queueId={queueId}
          item={row.original}
          reviewable={reviewable}
          manageable={manageable}
          reopenPending={reopen.isPending}
          onReopen={() => {
            reopenItem(row.original);
          }}
          onRemove={() => {
            setRemoving(row.original);
          }}
        />
      ),
    },
  ];

  return (
    <div className="flex min-h-0 flex-col gap-3">
      <div className="flex items-center gap-2">
        <Select
          value={status ?? ALL}
          onValueChange={(value) => {
            onStatusChange(parseQueueSearch({ status: value }).status);
          }}
        >
          <SelectTrigger aria-label="Status" size="sm" className="w-fit text-body">
            <SelectValue />
          </SelectTrigger>
          <SelectContent>
            <SelectItem value={ALL}>All</SelectItem>
            <SelectItem value="pending">Pending</SelectItem>
            <SelectItem value="completed">Completed</SelectItem>
            <SelectItem value="skipped">Skipped</SelectItem>
          </SelectContent>
        </Select>
      </div>
      <FormError message={failure} />
      {query.isPending ? (
        <div className="flex flex-col gap-2">
          <Skeleton className="h-7 w-full" />
          <Skeleton className="h-7 w-full" />
        </div>
      ) : rows.length === 0 ? (
        <EmptyState
          icon={ListChecks}
          title={status === undefined ? "No items yet" : "No items with this status"}
          description={
            status === undefined
              ? "Add traces, spans or sessions from their lists with Add to queue."
              : undefined
          }
        />
      ) : (
        <DataTable
          tableId="annotation-queue-items"
          ariaLabel="Queue items"
          columns={columns}
          rows={rows}
          getRowId={(row) => row.id}
          // A row click does nothing: the Open button is the way into the review.
          onActivate={() => undefined}
          hasNextPage={query.hasNextPage}
          isFetchingNextPage={query.isFetchingNextPage}
          fetchNextPage={() => {
            void query.fetchNextPage();
          }}
          emptyState={null}
        />
      )}
      <ConfirmDialog
        open={removing !== undefined}
        onOpenChange={(open) => {
          if (!open) {
            setRemoving(undefined);
            remove.reset();
          }
        }}
        title="Remove item"
        description="Removes the item from the queue. Its scores are kept."
        confirmLabel="Remove"
        destructive
        pending={remove.isPending}
        error={remove.error ? problemMessage(remove.error, FALLBACK_REMOVE) : undefined}
        onConfirm={() => {
          if (removing) {
            removeItem(removing);
          }
        }}
      />
    </div>
  );
}

/** The item's target: a link to its trace (and span) or its session, with the id beside it. */
function TargetCell({ item }: { item: Item }) {
  const { org, project } = useProject();
  if (item.session_id !== undefined) {
    return (
      <span className="inline-flex items-center gap-2">
        <Link
          to="/$org/$project/sessions/$sessionId"
          params={{ org, project, sessionId: item.session_id }}
          className="text-body underline-offset-4 hover:underline"
        >
          Session
        </Link>
        <IdText value={item.session_id} kind="Session" />
      </span>
    );
  }
  if (item.trace_id === undefined) {
    return "—";
  }
  const traceId = item.trace_id;
  const spanId = item.span_id;
  return (
    <span className="inline-flex items-center gap-2">
      <Link
        to="/$org/$project/traces/$traceId"
        params={{ org, project, traceId }}
        search={spanId !== undefined ? { span: spanId } : {}}
        className="text-body underline-offset-4 hover:underline"
      >
        {spanId !== undefined ? "Span" : "Trace"}
      </Link>
      <IdText value={spanId ?? traceId} kind={spanId !== undefined ? "Span" : "Trace"} />
    </span>
  );
}

/** The row's actions: Open (any reviewer), Reopen (completed or skipped) and Remove (members). */
function ItemActions({
  queueId,
  item,
  reviewable,
  manageable,
  reopenPending,
  onReopen,
  onRemove,
}: {
  queueId: string;
  item: Item;
  reviewable: boolean;
  manageable: boolean;
  reopenPending: boolean;
  onReopen: () => void;
  onRemove: () => void;
}) {
  const { org, project } = useProject();
  const reviewReason = reviewable ? undefined : "Only members can review items";
  const manageReason = manageable ? undefined : "Only members can remove items";
  return (
    <div className="flex items-center justify-end gap-1.5">
      <Button asChild variant="outline" size="xs">
        <Link
          to="/$org/$project/annotation-queues/$queueId/review/$itemId"
          params={{ org, project, queueId, itemId: item.id }}
        >
          Open
        </Link>
      </Button>
      {item.status === "pending" ? null : (
        <DisabledReason reason={reviewReason}>
          <Button
            type="button"
            variant="ghost"
            size="xs"
            disabled={!reviewable || reopenPending}
            onClick={onReopen}
          >
            Reopen
          </Button>
        </DisabledReason>
      )}
      <DisabledReason reason={manageReason}>
        <Button type="button" variant="ghost" size="xs" disabled={!manageable} onClick={onRemove}>
          Remove
        </Button>
      </DisabledReason>
    </div>
  );
}
