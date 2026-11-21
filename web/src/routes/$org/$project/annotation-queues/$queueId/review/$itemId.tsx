import { useQuery } from "@tanstack/react-query";
import { createFileRoute, Link, notFound } from "@tanstack/react-router";
import { ChevronLeft } from "lucide-react";
import { problemOf } from "@/api/problem";
import { annotationQueueQuery } from "@/api/queries-annotation-queues";
import { NextItem } from "@/components/annotation-queues/next-item";
import { Reviewer } from "@/components/annotation-queues/reviewer";
import { BACK_LINK_CLASS } from "@/components/back-link";
import { ITEM_TITLE_CLASS, PageToolbar, PageToolbarTitle } from "@/components/page-toolbar";
import { useProject } from "@/components/shell/project-context";
import { TopBarCrumb } from "@/components/shell/top-bar-slot";
import { parseReviewSearch } from "@/lib/annotation-queues";

export const Route = createFileRoute("/$org/$project/annotation-queues/$queueId/review/$itemId")({
  validateSearch: (search) => parseReviewSearch(search),
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
  component: ReviewPage,
});

/** `/review/next` claims an item; any other id is the item under review. */
function ReviewPage() {
  const { org, project } = useProject();
  const { queueId, itemId } = Route.useParams();
  const search = Route.useSearch();
  const queue = useQuery(annotationQueueQuery(org, project, queueId));
  const name = queue.data?.name ?? "Annotation queue";
  return (
    <div className="flex h-full min-h-0 flex-1 flex-col">
      <TopBarCrumb>
        <span className="text-body">Review</span>
      </TopBarCrumb>
      <PageToolbar>
        <PageToolbarTitle
          back={
            <Link
              to="/$org/$project/annotation-queues/$queueId"
              params={{ org, project, queueId }}
              aria-label={`Back to ${name}`}
              className={BACK_LINK_CLASS}
            >
              <ChevronLeft aria-hidden="true" className="size-3.5" />
              <span className="max-w-64 truncate">{name}</span>
            </Link>
          }
          label="Annotation queue"
        >
          <h2 className={ITEM_TITLE_CLASS}>Review</h2>
        </PageToolbarTitle>
      </PageToolbar>
      <div className="relative min-h-0 flex-1">
        <div className="absolute inset-0 flex min-h-0 flex-col">
          {itemId === "next" ? (
            <NextItem queueId={queueId} after={search.after} />
          ) : (
            <Reviewer key={itemId} queueId={queueId} itemId={itemId} />
          )}
        </div>
      </div>
    </div>
  );
}
