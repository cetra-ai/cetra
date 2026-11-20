import { createFileRoute, notFound } from "@tanstack/react-router";
import { problemOf } from "@/api/problem";
import { annotationQueueQuery } from "@/api/queries-annotation-queues";
import { NextItem } from "@/components/annotation-queues/next-item";
import { Reviewer } from "@/components/annotation-queues/reviewer";
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
  const { queueId, itemId } = Route.useParams();
  const search = Route.useSearch();
  return (
    <div className="relative h-full min-h-0 flex-1">
      <TopBarCrumb>
        <span className="text-body">Review</span>
      </TopBarCrumb>
      <div className="absolute inset-0 flex min-h-0 flex-col">
        {itemId === "next" ? (
          <NextItem queueId={queueId} after={search.after} />
        ) : (
          <Reviewer key={itemId} queueId={queueId} itemId={itemId} />
        )}
      </div>
    </div>
  );
}
