import { useQuery } from "@tanstack/react-query";
import { createFileRoute, Link, notFound } from "@tanstack/react-router";
import { ChevronLeft } from "lucide-react";
import { problemOf } from "@/api/problem";
import { annotationQueueQuery } from "@/api/queries-annotation-queues";
import { ItemsTable } from "@/components/annotation-queues/items-table";
import { QueueProgress } from "@/components/annotation-queues/queue-progress";
import { QueueSettings } from "@/components/annotation-queues/queue-settings";
import { BACK_LINK_CLASS, BackLink } from "@/components/back-link";
import { DisabledReason } from "@/components/disabled-reason";
import { ITEM_TITLE_CLASS, PageToolbar, PageToolbarTitle } from "@/components/page-toolbar";
import { useProject } from "@/components/shell/project-context";
import { TopBarCrumb } from "@/components/shell/top-bar-slot";
import { Button } from "@/components/ui/button";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { parseQueueSearch, QUEUE_POLL_MS, targetLabel } from "@/lib/annotation-queues";
import { canReview } from "@/lib/roles";

export const Route = createFileRoute("/$org/$project/annotation-queues/$queueId/")({
  validateSearch: (search) => parseQueueSearch(search),
  loader: async ({ context, params }) => {
    try {
      await context.queryClient.ensureQueryData(
        annotationQueueQuery(params.org, params.project, params.queueId),
      );
    } catch (error) {
      if (problemOf(error)?.status === 404) {
        throw notFound();
      }
      throw error;
    }
  },
  component: QueuePage,
});

function QueuePage() {
  const { org, project, data } = useProject();
  const { queueId } = Route.useParams();
  const search = Route.useSearch();
  const navigate = Route.useNavigate();
  const queue = useQuery({
    ...annotationQueueQuery(org, project, queueId),
    refetchInterval: QUEUE_POLL_MS,
    refetchOnWindowFocus: true,
  });

  if (queue.isPending) {
    return null;
  }
  if (queue.isError) {
    throw queue.error;
  }

  const current = queue.data;
  const tab = search.tab ?? "items";
  const startable = canReview(data.role) && current.counts.pending > 0;
  const startReason = !canReview(data.role)
    ? "Only members can review items"
    : current.counts.pending === 0
      ? "No pending items to review"
      : undefined;

  function chooseTab(value: string) {
    const next = parseQueueSearch({ tab: value }).tab;
    if (next === undefined) {
      return;
    }
    void navigate({
      to: "/$org/$project/annotation-queues/$queueId",
      params: { org, project, queueId },
      search: (prev) => ({ ...prev, tab: next }),
      state: true,
    });
  }

  return (
    <div className="flex h-full min-h-0 w-full flex-1 flex-col">
      <PageToolbar
        actions={
          <>
            <QueueProgress counts={current.counts} />
            <DisabledReason reason={startReason}>
              {startable ? (
                <Button asChild size="sm">
                  <Link
                    to="/$org/$project/annotation-queues/$queueId/review/$itemId"
                    params={{ org, project, queueId, itemId: "next" }}
                  >
                    Start reviewing
                  </Link>
                </Button>
              ) : (
                <Button type="button" size="sm" disabled>
                  Start reviewing
                </Button>
              )}
            </DisabledReason>
          </>
        }
      >
        <PageToolbarTitle
          back={
            <BackLink
              fallback={
                <Link
                  to="/$org/$project/annotation-queues"
                  params={{ org, project }}
                  aria-label="Back to annotation queues"
                  className={BACK_LINK_CLASS}
                >
                  <ChevronLeft aria-hidden="true" className="size-3.5" />
                  Annotation queues
                </Link>
              }
            />
          }
          label="Annotation queue"
        >
          <h2 className={ITEM_TITLE_CLASS}>{current.name}</h2>
          <TopBarCrumb>
            <span className="max-w-64 truncate text-body font-medium">{current.name}</span>
          </TopBarCrumb>
        </PageToolbarTitle>
        <span className="text-body text-muted-foreground">{targetLabel(current.target)}</span>
      </PageToolbar>

      {current.description ? (
        <p className="px-3 pt-2 text-body text-muted-foreground">{current.description}</p>
      ) : null}

      <Tabs value={tab} onValueChange={chooseTab} className="flex min-h-0 flex-1 flex-col">
        <TabsList variant="line" className="h-8 shrink-0 px-3">
          <TabsTrigger value="items" className="text-body">
            Items
          </TabsTrigger>
          <TabsTrigger value="settings" className="text-body">
            Settings
          </TabsTrigger>
        </TabsList>
        <TabsContent value="items" className="min-h-0 flex-1 overflow-auto p-3">
          <ItemsTable
            queueId={queueId}
            status={search.status}
            onStatusChange={(next) => {
              void navigate({
                to: "/$org/$project/annotation-queues/$queueId",
                params: { org, project, queueId },
                search: (prev) => ({ ...prev, status: next }),
                replace: true,
                state: true,
              });
            }}
          />
        </TabsContent>
        <TabsContent value="settings" className="min-h-0 flex-1 overflow-auto p-3">
          <QueueSettings queue={current} />
        </TabsContent>
      </Tabs>
    </div>
  );
}
