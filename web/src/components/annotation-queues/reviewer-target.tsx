import { useQuery, useQueryClient } from "@tanstack/react-query";
import { FileQuestion } from "lucide-react";
import { useState } from "react";
import { $api } from "@/api/client";
import { problemMessage, problemOf } from "@/api/problem";
import { sessionQuery, traceDetailQuery } from "@/api/queries";
import { annotationQueueWriteKeys } from "@/api/queries-annotation-queues";
import type { Schema } from "@/api/types";
import { EmptyState } from "@/components/empty-state";
import { FormError } from "@/components/form/form-error";
import { SessionConversation } from "@/components/session/session-conversation";
import { useSessionSteps } from "@/components/session/use-session-steps";
import { useProject } from "@/components/shell/project-context";
import { TraceView } from "@/components/trace/trace-view";
import { Button } from "@/components/ui/button";
import { Skeleton } from "@/components/ui/skeleton";
import { useGoToNext } from "./next-item";

type Item = Schema<"AnnotationQueueItem">;
type Kind = Schema<"QueueTarget">;

/**
 * What the reviewer shows of the item's target. `missing` is a 404 (or an item without its id):
 * the target is gone. `ready` carries the ids the form copies onto each score (D-I-18, D-I-37).
 */
export type TargetDetail =
  | { status: "pending" }
  | { status: "missing" }
  | { status: "error"; message: string }
  | {
      status: "ready";
      traceId?: string;
      sessionId?: string;
      userId?: string;
      session?: Schema<"SessionSummary">;
      turns?: readonly Schema<"SessionTurn">[];
    };

const NO_SCROLL = () => undefined;

/**
 * Loads the item's target: the trace (trace and span items) or the session (session items). The
 * queries are the ones the viewers use, so the cache is shared.
 */
export function useTargetDetail(kind: Kind | undefined, item: Item | undefined): TargetDetail {
  const { org, project } = useProject();
  const isSession = kind === "session";
  const traceId = item?.trace_id;
  const sessionId = item?.session_id;
  const trace = useQuery({
    ...traceDetailQuery(org, project, traceId ?? ""),
    enabled: kind !== undefined && !isSession && traceId !== undefined,
  });
  const session = useQuery({
    ...sessionQuery(org, project, sessionId ?? "", {}),
    enabled: isSession && sessionId !== undefined,
  });

  if (kind === undefined || item === undefined) {
    return { status: "pending" };
  }
  if (isSession) {
    if (sessionId === undefined) {
      return { status: "missing" };
    }
    if (session.isPending) {
      return { status: "pending" };
    }
    if (session.isError) {
      return failure(session.error, "Could not load this session");
    }
    return {
      status: "ready",
      sessionId,
      userId: session.data.session.user_id,
      session: session.data.session,
      turns: session.data.turns,
    };
  }
  if (traceId === undefined) {
    return { status: "missing" };
  }
  if (trace.isPending) {
    return { status: "pending" };
  }
  if (trace.isError) {
    return failure(trace.error, "Could not load this trace");
  }
  return {
    status: "ready",
    traceId,
    sessionId: trace.data.trace.session_id,
    userId: trace.data.trace.user_id,
  };
}

function failure(error: unknown, fallback: string): TargetDetail {
  if (problemOf(error)?.status === 404) {
    return { status: "missing" };
  }
  return { status: "error", message: problemMessage(error, fallback) };
}

/**
 * The left side of the reviewer: the item's trace or span, or its session conversation. A target
 * the server no longer has shows the not-available state, with Remove for managers.
 */
export function ReviewerTarget({
  queueId,
  item,
  kind,
  detail,
  canManage,
}: {
  queueId: string;
  item: Item;
  kind: Kind;
  detail: TargetDetail;
  canManage: boolean;
}) {
  const { org, project, ref } = useProject();
  const queryClient = useQueryClient();
  const goToNext = useGoToNext(queueId);
  const remove = $api.useMutation(
    "delete",
    "/api/v1/projects/{project}/annotation-queues/{id}/items/{item_id}",
  );
  const [spanId, setSpanId] = useState<string | undefined>(item.span_id);
  const [traceId, setTraceId] = useState<string | undefined>();
  const [removeError, setRemoveError] = useState<string | undefined>();

  if (detail.status === "pending") {
    return (
      <div className="flex flex-col gap-2 p-3">
        <Skeleton className="h-4 w-1/2" />
        <Skeleton className="h-4 w-2/3" />
      </div>
    );
  }
  if (detail.status === "error") {
    return <p className="p-3 text-body">{detail.message}</p>;
  }
  if (detail.status === "missing") {
    const label = kind === "session" ? "session" : "trace";
    return (
      <div className="flex flex-col items-center">
        <EmptyState icon={FileQuestion} title={`This ${label} is no longer available`} />
        {canManage ? (
          <Button
            variant="outline"
            size="sm"
            disabled={remove.isPending}
            onClick={() => {
              setRemoveError(undefined);
              void (async () => {
                try {
                  await remove.mutateAsync({
                    params: { path: { project: ref, id: queueId, item_id: item.id } },
                  });
                } catch (caught) {
                  setRemoveError(problemMessage(caught, "Could not remove this item."));
                  return;
                }
                await Promise.all(
                  annotationQueueWriteKeys(org, project, queueId).map((key) =>
                    queryClient.invalidateQueries({ queryKey: key }),
                  ),
                );
                await goToNext(item.id);
              })();
            }}
          >
            Remove from queue
          </Button>
        ) : null}
        <FormError message={removeError} />
      </div>
    );
  }

  if (kind === "session") {
    if (detail.session === undefined) {
      return <p className="p-3 text-body">Could not load this session</p>;
    }
    return (
      <ReviewerSession
        session={detail.session}
        turns={detail.turns ?? []}
        traceId={traceId}
        onSelectTrace={setTraceId}
      />
    );
  }
  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <TraceView
        key={detail.traceId}
        traceId={detail.traceId ?? ""}
        spanId={spanId}
        onSelectSpan={setSpanId}
      />
    </div>
  );
}

/** The reviewer's session conversation, with each turn's steps (D-C-28). */
function ReviewerSession({
  session,
  turns,
  traceId,
  onSelectTrace,
}: {
  session: Schema<"SessionSummary">;
  turns: readonly Schema<"SessionTurn">[];
  traceId: string | undefined;
  onSelectTrace: (traceId: string) => void;
}) {
  const { org, project } = useProject();
  const steps = useSessionSteps({
    org,
    project,
    session,
    turns,
    includeEval: false,
    includeExperiments: false,
  });
  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <SessionConversation
        turns={turns}
        steps={steps}
        trace={traceId}
        onSelectTrace={onSelectTrace}
        scrollTarget={null}
        onScrolled={NO_SCROLL}
      />
    </div>
  );
}
