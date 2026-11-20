import { keepPreviousData } from "@tanstack/react-query";
import { createFileRoute, Link } from "@tanstack/react-router";
import { ClipboardCheck } from "lucide-react";
import { useEffect, useMemo, useState } from "react";
import { $api } from "@/api/client";
import { annotationQueuesInit } from "@/api/queries-annotation-queues";
import type { Schema } from "@/api/types";
import { QueueDialog } from "@/components/annotation-queues/queue-dialog";
import { QueueProgress } from "@/components/annotation-queues/queue-progress";
import { DataTable, type DataTableColumn } from "@/components/data-table/data-table";
import { DisabledReason } from "@/components/disabled-reason";
import { EmptyState } from "@/components/empty-state";
import { RelativeTime } from "@/components/relative-time";
import { useProject } from "@/components/shell/project-context";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Skeleton } from "@/components/ui/skeleton";
import { parseQueuesSearch, QUEUE_POLL_MS, targetLabel } from "@/lib/annotation-queues";
import { canManageQueues, canReview } from "@/lib/roles";

export const Route = createFileRoute("/$org/$project/annotation-queues/")({
  validateSearch: (search) => parseQueuesSearch(search),
  component: QueuesPage,
});

const SEARCH_DEBOUNCE_MS = 300;

type Queue = Schema<"AnnotationQueue">;

function QueuesPage() {
  const { org, project, data } = useProject();
  const search = Route.useSearch();
  const { q } = search;
  const navigate = Route.useNavigate();
  const canManage = canManageQueues(data.role);
  const [creating, setCreating] = useState(false);
  const [text, setText] = useState(q ?? "");

  // The search text follows the URL when it changes from elsewhere (back, another link).
  useEffect(() => {
    setText((current) => (current.trim() === (q ?? "") ? current : (q ?? "")));
  }, [q]);

  // The search text reaches the URL after a pause, so typing does not filter on every key.
  useEffect(() => {
    const timer = setTimeout(() => {
      const next = text.trim() || undefined;
      if (next !== q) {
        void navigate({
          to: "/$org/$project/annotation-queues",
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
    "/api/v1/projects/{project}/annotation-queues",
    annotationQueuesInit(org, project),
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

  const loaded = useMemo(() => query.data?.pages.flatMap((page) => page.data) ?? [], [query.data]);
  // The name filter applies to the loaded rows; the API has no name search.
  const rows = useMemo(() => {
    const needle = (q ?? "").toLowerCase();
    if (needle === "") {
      return loaded;
    }
    return loaded.filter((queue) => queue.name.toLowerCase().includes(needle));
  }, [loaded, q]);

  const columns: DataTableColumn<Queue>[] = [
    {
      id: "name",
      size: 240,
      header: "Name",
      enableSorting: false,
      cell: ({ row }) => <NameCell queue={row.original} />,
    },
    {
      id: "kind",
      size: 96,
      header: "Kind",
      enableSorting: false,
      cell: ({ row }) => targetLabel(row.original.target),
    },
    {
      id: "progress",
      size: 184,
      header: "Progress",
      enableSorting: false,
      cell: ({ row }) => <QueueProgress counts={row.original.counts} />,
    },
    {
      id: "pending",
      size: 88,
      header: "Pending",
      enableSorting: false,
      meta: { align: "right" },
      cell: ({ row }) => <span className="num">{row.original.counts.pending}</span>,
    },
    {
      id: "updated",
      size: 120,
      header: "Updated",
      enableSorting: false,
      cell: ({ row }) => <RelativeTime value={row.original.updated_at} />,
    },
    {
      id: "review",
      size: 168,
      header: "",
      enableSorting: false,
      meta: { align: "right" },
      cell: ({ row }) => <StartReviewingCell queue={row.original} />,
    },
  ];

  return (
    <div className="flex h-full min-h-0 w-full flex-1 flex-col">
      <div className="flex min-h-9 shrink-0 items-center gap-3 border-b px-3">
        <Input
          aria-label="Search annotation queues"
          placeholder="Search by name"
          value={text}
          onChange={(event) => {
            setText(event.target.value);
          }}
          className="h-7 max-w-xs text-body"
        />
        <div className="ml-auto">
          <DisabledReason reason={canManage ? undefined : "Only members can create queues"}>
            <Button
              type="button"
              size="sm"
              disabled={!canManage}
              onClick={() => {
                setCreating(true);
              }}
            >
              New queue
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
      ) : rows.length === 0 ? (
        q === undefined ? (
          <EmptyState
            icon={ClipboardCheck}
            title="No annotation queues yet"
            description="Create a queue, then add traces, spans or sessions from their lists to review them one at a time."
          />
        ) : (
          <EmptyState icon={ClipboardCheck} title="No queue matches" />
        )
      ) : (
        <div className="min-h-0 flex-1">
          <DataTable
            tableId="annotation-queues"
            ariaLabel="Annotation queues"
            columns={columns}
            rows={rows}
            getRowId={(row) => row.id}
            onActivate={(row) => {
              void navigate({
                to: "/$org/$project/annotation-queues/$queueId",
                params: { org, project, queueId: row.id },
              });
            }}
            hasNextPage={query.hasNextPage}
            isFetchingNextPage={query.isFetchingNextPage}
            fetchNextPage={() => {
              void query.fetchNextPage();
            }}
            emptyState={null}
          />
        </div>
      )}
      <QueueDialog
        open={creating}
        onOpenChange={setCreating}
        // The dialog invalidates the list itself; the new queue opens on its own page.
        onCreated={(created) => {
          void navigate({
            to: "/$org/$project/annotation-queues/$queueId",
            params: { org, project, queueId: created.id },
          });
        }}
      />
    </div>
  );
}

/** The queue's name, a link to its page. The click stops here so the row does not open it too. */
function NameCell({ queue }: { queue: Queue }) {
  const { org, project } = useProject();
  return (
    <Link
      to="/$org/$project/annotation-queues/$queueId"
      params={{ org, project, queueId: queue.id }}
      className="block truncate font-medium underline-offset-4 hover:underline"
      onClick={(event) => {
        event.stopPropagation();
      }}
    >
      {queue.name}
    </Link>
  );
}

/** Opens the queue at its next pending item. Disabled for viewers, and when none is pending. */
function StartReviewingCell({ queue }: { queue: Queue }) {
  const { org, project, data } = useProject();
  const reviewable = canReview(data.role);
  const reason = !reviewable
    ? "Only members can review items"
    : queue.counts.pending === 0
      ? "No pending items to review"
      : undefined;
  return (
    <DisabledReason reason={reason}>
      {reason === undefined ? (
        <Button asChild variant="outline" size="xs">
          <Link
            to="/$org/$project/annotation-queues/$queueId/review/$itemId"
            params={{ org, project, queueId: queue.id, itemId: "next" }}
            onClick={(event) => {
              event.stopPropagation();
            }}
          >
            Start reviewing
          </Link>
        </Button>
      ) : (
        <Button type="button" variant="outline" size="xs" disabled>
          Start reviewing
        </Button>
      )}
    </DisabledReason>
  );
}
