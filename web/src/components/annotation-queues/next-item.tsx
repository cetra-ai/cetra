import { Link, useNavigate } from "@tanstack/react-router";
import { ClipboardCheck } from "lucide-react";
import { useCallback, useEffect, useRef, useState } from "react";
import { api } from "@/api/client";
import { problemMessage } from "@/api/problem";
import { EmptyState } from "@/components/empty-state";
import { FormError } from "@/components/form/form-error";
import { useProject } from "@/components/shell/project-context";
import { Button } from "@/components/ui/button";
import { Skeleton } from "@/components/ui/skeleton";
import { canReview } from "@/lib/roles";

const NEXT = "/api/v1/projects/{project}/annotation-queues/{id}/next";

/**
 * Moves the reviewer on from an item: navigates to `/review/next?after=<id>`, which claims the
 * next free item (D-AQ-5). Every move in the reviewer goes through this one URL.
 */
export function useGoToNext(queueId: string) {
  const { org, project } = useProject();
  const navigate = useNavigate();
  return (afterId: string) =>
    navigate({
      to: "/$org/$project/annotation-queues/$queueId/review/$itemId",
      params: { org, project, queueId, itemId: "next" },
      search: { after: afterId },
    });
}

/**
 * `/review/next`: claims the first free item after `after` once per `after` (StrictMode's
 * double effect does not claim twice, and neither does the location change of its own
 * replace navigation), then replaces itself with that item. When nothing is
 * free, it shows the end screen. Viewers claim nothing.
 */
export function NextItem({ queueId, after }: { queueId: string; after: string | undefined }) {
  const { org, project, ref, data } = useProject();
  const navigate = useNavigate();
  const member = canReview(data.role);
  const [outcome, setOutcome] = useState<
    { kind: "claiming" } | { kind: "empty" } | { kind: "error"; message: string }
  >({ kind: "claiming" });
  const claimedFor = useRef<{ after: string | undefined } | undefined>(undefined);

  const claim = useCallback(
    async (afterId: string | undefined) => {
      const {
        data: item,
        response,
        error,
      } = await api.POST(NEXT, {
        params: { path: { project: ref, id: queueId } },
        body: afterId === undefined ? {} : { after: afterId },
      });
      if (response.status === 204) {
        setOutcome({ kind: "empty" });
        return;
      }
      if (!response.ok || item === undefined) {
        setOutcome({ kind: "error", message: problemMessage(error, "Could not claim an item.") });
        return;
      }
      setOutcome({ kind: "claiming" });
      await navigate({
        to: "/$org/$project/annotation-queues/$queueId/review/$itemId",
        params: { org, project, queueId, itemId: item.id },
        replace: true,
      });
    },
    [navigate, org, project, queueId, ref],
  );

  useEffect(() => {
    if (!member || (claimedFor.current !== undefined && claimedFor.current.after === after)) {
      return;
    }
    claimedFor.current = { after };
    void claim(after);
  }, [after, claim, member]);

  const backLink = (
    <Link
      to="/$org/$project/annotation-queues/$queueId"
      params={{ org, project, queueId }}
      className="text-caption underline underline-offset-4"
    >
      Back to the queue
    </Link>
  );

  if (!member) {
    return (
      <div className="flex flex-col items-center gap-2 px-4 py-16 text-center">
        <p className="text-body text-muted-foreground">Only members can review.</p>
        {backLink}
      </div>
    );
  }
  if (outcome.kind === "claiming") {
    return (
      <div className="flex flex-col gap-2 p-3">
        <Skeleton className="h-4 w-1/2" />
        <Skeleton className="h-4 w-2/3" />
      </div>
    );
  }
  if (outcome.kind === "error") {
    return <FormError message={outcome.message} />;
  }
  return (
    <div className="flex flex-col items-center">
      <EmptyState
        icon={ClipboardCheck}
        title="Nothing left to review"
        description="All pending items are done or being reviewed by someone else."
      />
      <div className="flex gap-2">
        {backLink}
        <Button
          variant="outline"
          size="sm"
          onClick={() => {
            setOutcome({ kind: "claiming" });
            void claim(undefined);
          }}
        >
          Check again
        </Button>
      </div>
    </div>
  );
}
