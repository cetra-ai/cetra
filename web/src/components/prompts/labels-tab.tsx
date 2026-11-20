import { History } from "lucide-react";
import { $api } from "@/api/client";
import { promptLabelEventsInit } from "@/api/queries-prompts";
import type { Schema } from "@/api/types";
import { CreatedBy } from "@/components/created-by";
import { EmptyState } from "@/components/empty-state";
import { LabelChip } from "@/components/prompts/label-chip";
import { RelativeTime } from "@/components/relative-time";
import { useProject } from "@/components/shell/project-context";
import { Button } from "@/components/ui/button";
import { Skeleton } from "@/components/ui/skeleton";

/** The history of the prompt's labels, newest first: each move of a label to or from a version. */
export function LabelsTab({ prompt }: { prompt: string }) {
  const { org, project } = useProject();
  const query = $api.useInfiniteQuery(
    "get",
    "/api/v1/projects/{project}/prompts/{prompt}/label-events",
    promptLabelEventsInit(org, project, prompt),
    {
      pageParamName: "cursor",
      initialPageParam: null,
      getNextPageParam: (page) => page.next_cursor ?? undefined,
    },
  );

  if (query.error) {
    throw query.error;
  }
  if (query.isPending) {
    return <Skeleton className="h-24 w-full" />;
  }
  const events = query.data?.pages.flatMap((page) => page.data) ?? [];
  if (events.length === 0) {
    return <EmptyState icon={History} title="No label has moved yet" />;
  }

  return (
    <div className="flex flex-col gap-3">
      <ol aria-label="Label history" className="flex flex-col divide-y">
        {events.map((event, index) => (
          <li
            // The history is append-only and rendered in order, so the position is a stable key.
            // biome-ignore lint/suspicious/noArrayIndexKey: the rows are rendered once, in order
            key={index}
            className="flex flex-wrap items-center gap-x-3 gap-y-1 py-2 text-body"
          >
            <LabelChip label={event.label} />
            <span className="num">{movement(event)}</span>
            <span className="text-muted-foreground">
              <CreatedBy createdBy={event.actor} />
            </span>
            <span className="text-muted-foreground">
              <RelativeTime value={event.at} />
            </span>
          </li>
        ))}
      </ol>
      {query.hasNextPage ? (
        <div>
          <Button
            type="button"
            variant="outline"
            size="sm"
            disabled={query.isFetchingNextPage}
            onClick={() => {
              void query.fetchNextPage();
            }}
          >
            Load more
          </Button>
        </div>
      ) : null}
    </div>
  );
}

/** What a label move did: `new → vN`, `vN → vM`, or `vN → removed`. */
function movement(event: Schema<"PromptLabelEvent">): string {
  if (event.to_version === undefined) {
    return event.from_version === undefined ? "removed" : `v${event.from_version} → removed`;
  }
  return event.from_version === undefined
    ? `new → v${event.to_version}`
    : `v${event.from_version} → v${event.to_version}`;
}
