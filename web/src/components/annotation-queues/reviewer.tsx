import { useQuery, useQueryClient } from "@tanstack/react-query";
import { notFound, useRouter } from "@tanstack/react-router";
import { Check, RotateCcw, SkipForward } from "lucide-react";
import { type ReactNode, useCallback, useEffect, useRef, useState } from "react";
import { $api } from "@/api/client";
import { problemMessage, problemOf } from "@/api/problem";
import { meQuery } from "@/api/queries";
import {
  annotationQueueItemQuery,
  annotationQueueQuery,
  annotationQueueWriteKeys,
} from "@/api/queries-annotation-queues";
import type { Schema } from "@/api/types";
import { AnnotationForm } from "@/components/annotation/annotation-form";
import { ConfirmDialog } from "@/components/confirm-dialog";
import { CreatedBy } from "@/components/created-by";
import { FormError } from "@/components/form/form-error";
import { Markdown } from "@/components/payload/markdown";
import { RelativeTime } from "@/components/relative-time";
import { useProject } from "@/components/shell/project-context";
import { SplitPane } from "@/components/trace/split-pane";
import { Button } from "@/components/ui/button";
import { Skeleton } from "@/components/ui/skeleton";
import type { AnnotationSaveHandle, AnnotationTarget } from "@/lib/annotation";
import { QUEUE_POLL_MS } from "@/lib/annotation-queues";
import { canManageQueues, canReview } from "@/lib/roles";
import { useGoToNext } from "./next-item";
import { QueueProgress } from "./queue-progress";
import { ReviewerTarget, useTargetDetail } from "./reviewer-target";
import { SkipDialog } from "./skip-dialog";
import { canLease, useLease } from "./use-lease";

type Item = Schema<"AnnotationQueueItem">;
type Kind = Schema<"QueueTarget">;

/** What the window's Enter handler reads: refs keep it from seeing a stale render. */
type ShortcutState = {
  member: boolean;
  busy: boolean;
  blocked: boolean;
  formReady: boolean;
  pending: boolean;
  skipOpen: boolean;
  discardOpen: boolean;
  complete: () => Promise<void>;
  next: () => void;
};

/** The annotation target of an item, as the form scores it. */
function targetOf(item: Item, kind: Kind): AnnotationTarget | undefined {
  if (kind === "session") {
    return item.session_id === undefined
      ? undefined
      : { type: "session", sessionId: item.session_id };
  }
  if (item.trace_id === undefined) {
    return undefined;
  }
  if (kind === "span") {
    return item.span_id === undefined
      ? undefined
      : { type: "span", traceId: item.trace_id, spanId: item.span_id };
  }
  return { type: "trace", traceId: item.trace_id };
}

/**
 * The reviewer: one item of a queue, its target beside the form, and the moves between items
 * (D-AQ-4, D-AQ-5). The form is hidden while another reviewer holds the item or the lease is lost.
 */
export function Reviewer({ queueId, itemId }: { queueId: string; itemId: string }) {
  const { org, project, ref, data } = useProject();
  const queryClient = useQueryClient();
  const router = useRouter();
  const goToNext = useGoToNext(queueId);
  const me = useQuery(meQuery);
  const queue = useQuery({
    ...annotationQueueQuery(org, project, queueId),
    refetchInterval: QUEUE_POLL_MS,
    refetchOnWindowFocus: true,
  });
  const item = useQuery({
    ...annotationQueueItemQuery(org, project, queueId, itemId),
    refetchInterval: QUEUE_POLL_MS,
    refetchOnWindowFocus: true,
  });
  const complete = $api.useMutation(
    "post",
    "/api/v1/projects/{project}/annotation-queues/{id}/items/{item_id}/complete",
  );
  const reopen = $api.useMutation(
    "post",
    "/api/v1/projects/{project}/annotation-queues/{id}/items/{item_id}/reopen",
  );

  const member = canReview(data.role);
  const meId = me.data?.user.id;
  const it = item.data;
  const kind = queue.data?.target;
  const detail = useTargetDetail(kind, it);
  const leaseOn = it !== undefined && meId !== undefined && member && canLease(it, meId);
  const lease = useLease(org, project, queueId, it, leaseOn);
  // Another reviewer's live lease. Unknown until the caller is loaded, so nothing is blocked then.
  const otherHolder =
    it !== undefined && meId !== undefined && it.status === "pending" && !canLease(it, meId)
      ? it.leased_by_user_id
      : undefined;
  const blocked = lease.state === "lost" || otherHolder !== undefined;

  const pending = it?.status === "pending";
  const saveRef = useRef<AnnotationSaveHandle | null>(null);
  const [busy, setBusy] = useState(false);
  const [formReady, setFormReady] = useState(false);
  const [actionError, setActionError] = useState<string | undefined>();
  const [skipOpen, setSkipOpen] = useState(false);
  const [discardOpen, setDiscardOpen] = useState(false);
  // Set at once by each action, so a second click or shortcut before the re-render does nothing.
  const inFlight = useRef(false);
  const onFormReady = useCallback(() => {
    setFormReady(true);
  }, []);

  async function refreshQueue() {
    await Promise.all(
      annotationQueueWriteKeys(org, project, queueId).map((key) =>
        queryClient.invalidateQueries({ queryKey: key }),
      ),
    );
  }

  // Saves the changed rows of a completed or skipped item, which stays as it is (D-AQ-5).
  // A failed save shows the form's own error.
  async function saveEdits() {
    if (inFlight.current || busy || !formReady) {
      return;
    }
    inFlight.current = true;
    setBusy(true);
    setActionError(undefined);
    try {
      if (saveRef.current) {
        await saveRef.current.save();
      }
    } finally {
      inFlight.current = false;
      setBusy(false);
    }
  }

  // Saves the changed rows first, then completes the item (D-AQ-5). A failed save completes nothing.
  async function completeAndNext() {
    if (inFlight.current || busy || blocked || !formReady) {
      return;
    }
    inFlight.current = true;
    setBusy(true);
    setActionError(undefined);
    try {
      const saved = saveRef.current ? await saveRef.current.save() : true;
      if (!saved) {
        return;
      }
      await complete.mutateAsync({
        params: { path: { project: ref, id: queueId, item_id: itemId } },
      });
      await refreshQueue();
      await goToNext(itemId);
    } catch (caught) {
      setActionError(problemMessage(caught, "Could not complete this item."));
    } finally {
      inFlight.current = false;
      setBusy(false);
    }
  }

  // Moves on without saving or changing the item. Unsaved edits ask first.
  function next() {
    if (inFlight.current || busy) {
      return;
    }
    if (saveRef.current?.isDirty()) {
      setDiscardOpen(true);
      return;
    }
    inFlight.current = true;
    void goToNext(itemId).finally(() => {
      inFlight.current = false;
    });
  }

  async function reopenItem() {
    if (inFlight.current) {
      return;
    }
    inFlight.current = true;
    setBusy(true);
    setActionError(undefined);
    try {
      await reopen.mutateAsync({
        params: { path: { project: ref, id: queueId, item_id: itemId } },
      });
      await refreshQueue();
    } catch (caught) {
      setActionError(problemMessage(caught, "Could not reopen this item."));
    } finally {
      inFlight.current = false;
      setBusy(false);
    }
  }

  const latest = useRef<ShortcutState | undefined>(undefined);
  useEffect(() => {
    latest.current = {
      member,
      busy,
      blocked,
      formReady,
      pending,
      skipOpen,
      discardOpen,
      complete: completeAndNext,
      next,
    };
  });
  // Ctrl/Cmd+Enter completes and moves on; with Shift it only moves on. Nothing runs while busy,
  // while a dialog is open, or for viewers.
  useEffect(() => {
    function onKeyDown(event: KeyboardEvent) {
      if (event.key !== "Enter" || !(event.metaKey || event.ctrlKey)) {
        return;
      }
      if (event.isComposing || event.repeat) {
        return;
      }
      const now = latest.current;
      if (!now || !now.member || now.busy || now.skipOpen || now.discardOpen) {
        return;
      }
      if (event.shiftKey) {
        event.preventDefault();
        now.next();
        return;
      }
      if (!now.pending || !now.formReady || now.blocked) {
        return;
      }
      event.preventDefault();
      void now.complete();
    }
    window.addEventListener("keydown", onKeyDown);
    return () => {
      window.removeEventListener("keydown", onKeyDown);
    };
  }, []);

  if (queue.isError || item.isError) {
    const error = queue.isError ? queue.error : item.error;
    if (problemOf(error)?.status === 404) {
      throw notFound();
    }
    return <p className="p-3 text-body">{problemMessage(error, "Could not load this item")}</p>;
  }
  if (it === undefined || queue.data === undefined || kind === undefined) {
    return (
      <div className="flex flex-col gap-2 p-3">
        <Skeleton className="h-4 w-1/2" />
        <Skeleton className="h-4 w-2/3" />
      </div>
    );
  }
  const q = queue.data;
  const target = targetOf(it, kind);

  let status: ReactNode = null;
  if (it.status === "completed") {
    status = (
      <p className="text-caption">
        Completed by <CreatedBy createdBy={it.done_by_user_id} />
        {it.done_at !== undefined ? (
          <>
            {" "}
            <RelativeTime value={it.done_at} />
          </>
        ) : null}
      </p>
    );
  } else if (it.status === "skipped") {
    status = (
      <p className="text-caption">
        Skipped by <CreatedBy createdBy={it.done_by_user_id} />
        {it.skip_reason ? ` — ${it.skip_reason}` : null}
      </p>
    );
  } else if (otherHolder !== undefined) {
    status = (
      <p className="text-caption">
        Being reviewed by <CreatedBy createdBy={otherHolder} />
      </p>
    );
  }

  let form: ReactNode = null;
  if (!member) {
    form = null;
  } else if (blocked) {
    form = (
      <p className="text-body">
        {lease.state === "lost"
          ? "Another reviewer took this item."
          : "This item is being reviewed by someone else."}
      </p>
    );
  } else if (detail.status === "pending") {
    form = (
      <div className="flex flex-col gap-2">
        <Skeleton className="h-4 w-2/3" />
        <Skeleton className="h-4 w-full" />
      </div>
    );
  } else if (detail.status === "ready" && target !== undefined) {
    form = (
      <AnnotationForm
        key={it.id}
        target={target}
        context={{ sessionId: detail.sessionId, userId: detail.userId, queueId }}
        definitionIds={q.definition_ids}
        saveRef={saveRef}
        hideSave
        onReady={onFormReady}
      />
    );
  }

  const panel = (
    <section aria-label="Review" className="flex h-full min-h-0 flex-col gap-3 overflow-y-auto p-3">
      <QueueProgress counts={q.counts} />
      {q.instructions ? (
        <details open>
          <summary className="cursor-pointer text-ui">Instructions</summary>
          <Markdown text={q.instructions} />
        </details>
      ) : null}
      {status}
      {member ? null : (
        <p className="text-caption text-muted-foreground">Only members can review.</p>
      )}
      {form}
      <FormError message={actionError} />
      <div className="flex flex-wrap gap-2">
        {member && pending ? (
          <Button
            disabled={busy || blocked || !formReady}
            onClick={() => {
              void completeAndNext();
            }}
          >
            <Check />
            Complete and next
          </Button>
        ) : null}
        <Button variant="outline" disabled={busy} onClick={next}>
          Next
        </Button>
        {member && pending ? (
          <Button
            variant="outline"
            disabled={busy || blocked}
            onClick={() => {
              setSkipOpen(true);
            }}
          >
            <SkipForward />
            Skip…
          </Button>
        ) : null}
        <Button
          variant="outline"
          disabled={busy}
          onClick={() => {
            router.history.back();
          }}
        >
          Previous
        </Button>
        {member && !pending ? (
          <Button
            variant="outline"
            disabled={busy || !formReady}
            onClick={() => {
              void saveEdits();
            }}
          >
            Save
          </Button>
        ) : null}
        {member && !pending ? (
          <Button
            variant="outline"
            disabled={busy}
            onClick={() => {
              void reopenItem();
            }}
          >
            <RotateCcw />
            Reopen
          </Button>
        ) : null}
      </div>
      {member ? (
        <SkipDialog open={skipOpen} onOpenChange={setSkipOpen} queueId={queueId} itemId={itemId} />
      ) : null}
      <ConfirmDialog
        open={discardOpen}
        onOpenChange={setDiscardOpen}
        title="Discard your unsaved annotations?"
        description="Your unsaved changes to this item are lost."
        confirmLabel="Discard and next"
        destructive
        pending={false}
        onConfirm={() => {
          setDiscardOpen(false);
          void goToNext(itemId);
        }}
      />
    </section>
  );

  return (
    <div className="relative h-full min-h-0 flex-1">
      <div className="absolute inset-0">
        <SplitPane
          name="annotation-review"
          start={
            <ReviewerTarget
              queueId={queueId}
              item={it}
              kind={kind}
              detail={detail}
              canManage={canManageQueues(data.role)}
            />
          }
          end={panel}
        />
      </div>
    </div>
  );
}
