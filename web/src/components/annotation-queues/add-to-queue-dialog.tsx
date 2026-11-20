import { useQueryClient } from "@tanstack/react-query";
import { Link } from "@tanstack/react-router";
import { type FormEvent, useEffect, useState } from "react";
import { $api } from "@/api/client";
import { fieldErrors, problemMessage, problemOf } from "@/api/problem";
import { projectRef } from "@/api/project-ref";
import { annotationQueuesInit, annotationQueueWriteKeys } from "@/api/queries-annotation-queues";
import type { Schema } from "@/api/types";
import { QueueDialog } from "@/components/annotation-queues/queue-dialog";
import { DisabledReason } from "@/components/disabled-reason";
import { FormError } from "@/components/form/form-error";
import { SubmitButton } from "@/components/form/submit-button";
import { useProject } from "@/components/shell/project-context";
import { Button, buttonVariants } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { shortId } from "@/lib/format";
import { canManageQueues, type OrgRole } from "@/lib/roles";

/** The most items one add sends. The server takes 1 to 1 000 (D-AQ-8). */
export const ADD_ITEMS_MAX = 1_000;

export type QueueKind = Schema<"QueueTarget">;
export type QueueItemTarget = Schema<"QueueItemTarget">;
type Queue = Schema<"AnnotationQueue">;

/** Select value of the "New queue…" option. Queue ids are UUIDs, so it cannot clash. */
const NEW_QUEUE = "new";
const QUEUES = "/api/v1/projects/{project}/annotation-queues";
const ITEMS = "/api/v1/projects/{project}/annotation-queues/{id}/items";

const NOUNS: Record<QueueKind, string> = { trace: "trace", span: "span", session: "session" };

/** Why the page cannot add `count` checked or selected items to a queue; undefined when it can. */
export function addToQueueReason(role: OrgRole | undefined, count: number): string | undefined {
  if (!canManageQueues(role)) {
    return "Only members can add to queues";
  }
  if (count > ADD_ITEMS_MAX) {
    return "Select at most 1 000 rows";
  }
  return undefined;
}

/**
 * The message for a 422 `too_many_items` (a full queue), or undefined for any other error. The
 * add dialogs show it in place of the problem's own text.
 */
export function queueFullMessage(caught: unknown): string | undefined {
  if (problemOf(caught)?.code === "too_many_items") {
    return "The queue is full (100 000 items).";
  }
  return undefined;
}

/**
 * Every queue of the project, all pages, through the list's own infinite init. The list page
 * reads the same key, so a plain query here would read its cache in the wrong shape. Pages are
 * fetched one after another until the last, so queues past the first page are listed too.
 */
export function useProjectQueues() {
  const { org, project } = useProject();
  const query = $api.useInfiniteQuery("get", QUEUES, annotationQueuesInit(org, project), {
    pageParamName: "cursor",
    initialPageParam: null,
    getNextPageParam: (page) => page.next_cursor ?? undefined,
  });
  const { hasNextPage, isFetchingNextPage, fetchNextPage } = query;
  useEffect(() => {
    if (hasNextPage && !isFetchingNextPage) {
      void fetchNextPage();
    }
  }, [hasNextPage, isFetchingNextPage, fetchNextPage]);
  const queues = (query.data?.pages ?? []).flatMap((page) => page.data);
  return { queues, query };
}

function useAddItems() {
  return $api.useMutation("post", ITEMS);
}

/** The "Add to queue" button of a trace, span or session page, with its dialog. */
export function AddToQueueButton({
  target,
  items,
}: {
  target: QueueKind;
  items: readonly QueueItemTarget[];
}) {
  const { data } = useProject();
  const [open, setOpen] = useState(false);
  const reason = addToQueueReason(data.role, items.length);
  return (
    <>
      <DisabledReason reason={reason}>
        <Button
          type="button"
          variant="outline"
          size="xs"
          disabled={reason !== undefined}
          onClick={() => {
            setOpen(true);
          }}
        >
          Add to queue
        </Button>
      </DisabledReason>
      <AddToQueueDialog open={open} onOpenChange={setOpen} target={target} items={items} />
    </>
  );
}

export interface QueueTargetDialogProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  title: string;
  description: string;
  /** The kinds of queue the picker lists. One kind: options show the name only; several: `name · kind`. */
  kinds: readonly QueueKind[];
  /** The kind "New queue…" starts with. */
  defaultKind: QueueKind;
  /** Adds to the queue `queueId`; resolves with the server's answer. */
  add: (queueId: string) => Promise<Schema<"AddedQueueItems">>;
  /** True while that request runs (the caller's mutation `isPending`). */
  adding: boolean;
  /** What a `skipped` row's `index` names in the result list. */
  skippedLabel: (index: number) => string;
  /** Called once the items are added. */
  onAdded?: () => void;
}

/** The dialog shell of every add-to-queue dialog: the picker, the add result and the busy guards. */
export function QueueTargetDialog({
  open,
  onOpenChange,
  title,
  description,
  kinds,
  defaultKind,
  add,
  adding,
  skippedLabel,
  onAdded,
}: QueueTargetDialogProps) {
  return (
    <Dialog
      open={open}
      onOpenChange={(next) => {
        if (!next && adding) {
          return;
        }
        onOpenChange(next);
      }}
    >
      <DialogContent
        className="max-h-[90vh] overflow-y-auto"
        onInteractOutside={(event) => {
          if (adding) {
            event.preventDefault();
          }
        }}
        onEscapeKeyDown={(event) => {
          if (adding) {
            event.preventDefault();
          }
        }}
      >
        <DialogHeader>
          <DialogTitle>{title}</DialogTitle>
          <DialogDescription>{description}</DialogDescription>
        </DialogHeader>
        {open ? (
          <QueueTargetForm
            kinds={kinds}
            defaultKind={defaultKind}
            add={add}
            adding={adding}
            skippedLabel={skippedLabel}
            onAdded={onAdded}
            onClose={() => {
              onOpenChange(false);
            }}
          />
        ) : null}
      </DialogContent>
    </Dialog>
  );
}

/** Adds the items to one queue of the same kind, or to a new one created here (D-AQ-8). */
export function AddToQueueDialog({
  open,
  onOpenChange,
  target,
  items,
  onAdded,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  target: QueueKind;
  items: readonly QueueItemTarget[];
  /** Called once the items are added. */
  onAdded?: () => void;
}) {
  const { org, project } = useProject();
  const ref = projectRef(org, project);
  const add = useAddItems();
  const count = items.length;
  const noun = NOUNS[target];

  return (
    <QueueTargetDialog
      open={open}
      onOpenChange={onOpenChange}
      title={`Add ${count} ${count === 1 ? noun : `${noun}s`} to a queue`}
      description="Reviewers work through the queue in order. An item already in the queue is not added twice."
      kinds={[target]}
      defaultKind={target}
      add={(queueId) =>
        add.mutateAsync({
          params: { path: { project: ref, id: queueId } },
          body: { items: [...items] },
        })
      }
      adding={add.isPending}
      skippedLabel={(index) => itemLabel(items[index])}
      onAdded={onAdded}
    />
  );
}

/** Sends the newest occurrences of an issue to a queue of any kind (D-IC-27). */
export function SendIssueToQueueDialog({
  open,
  onOpenChange,
  issueId,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  /** The issue whose newest occurrences are sent. */
  issueId: string;
}) {
  const { org, project } = useProject();
  const ref = projectRef(org, project);
  const send = $api.useMutation("post", "/api/v1/projects/{project}/issues/{id}/queue-items");

  return (
    <QueueTargetDialog
      open={open}
      onOpenChange={onOpenChange}
      title="Send to a queue"
      description="Adds the issue's newest occurrences (up to 100) to a queue. A trace queue gets their traces, a span queue the spans, a session queue the sessions, each once. An item already in the queue is not added twice."
      kinds={["trace", "span", "session"]}
      defaultKind="trace"
      add={(queueId) =>
        send.mutateAsync({
          params: { path: { project: ref, id: issueId } },
          body: { queue: queueId },
        })
      }
      adding={send.isPending}
      skippedLabel={(index) => `Occurrence ${index + 1}`}
    />
  );
}

function QueueTargetForm({
  kinds,
  defaultKind,
  add,
  adding,
  skippedLabel,
  onAdded,
  onClose,
}: {
  kinds: readonly QueueKind[];
  defaultKind: QueueKind;
  add: QueueTargetDialogProps["add"];
  adding: boolean;
  skippedLabel: (index: number) => string;
  onAdded: (() => void) | undefined;
  onClose: () => void;
}) {
  const { org, project } = useProject();
  const queryClient = useQueryClient();
  const { queues, query } = useProjectQueues();
  const [picked, setPicked] = useState<Queue | undefined>();
  const [newOpen, setNewOpen] = useState(false);
  const [formError, setFormError] = useState<string | undefined>();
  const [result, setResult] = useState<
    { queue: Queue; answer: Schema<"AddedQueueItems"> } | undefined
  >();

  const listed = queues.filter((queue) => kinds.includes(queue.target));
  // The queue chosen or created here stays listed, even before the refetch shows it.
  const options =
    picked !== undefined && !listed.some((queue) => queue.id === picked.id)
      ? [...listed, picked]
      : listed;

  function pickQueue(value: string) {
    setFormError(undefined);
    if (value === NEW_QUEUE) {
      setNewOpen(true);
      return;
    }
    const next = listed.find((queue) => queue.id === value);
    if (next !== undefined) {
      setPicked(next);
    }
  }

  async function onSubmit(event: FormEvent) {
    event.preventDefault();
    setFormError(undefined);
    if (picked === undefined) {
      setFormError("Choose a queue.");
      return;
    }
    try {
      const answer = await add(picked.id);
      for (const key of annotationQueueWriteKeys(org, project, picked.id)) {
        void queryClient.invalidateQueries({ queryKey: key });
      }
      onAdded?.();
      setResult({ queue: picked, answer });
    } catch (caught) {
      setFormError(
        queueFullMessage(caught) ??
          Object.values(fieldErrors(caught))[0] ??
          problemMessage(caught, "The items could not be added."),
      );
    }
  }

  if (result !== undefined) {
    const skipped = result.answer.skipped;
    return (
      <div className="flex flex-col gap-4">
        <p className="text-ui">{`Added ${result.answer.added} to ${result.queue.name}.`}</p>
        {result.answer.existing > 0 ? (
          <p className="text-ui text-muted-foreground">
            {`${result.answer.existing} already in the queue.`}
          </p>
        ) : null}
        {skipped.length > 0 ? (
          <div className="flex flex-col gap-2">
            <p className="text-ui font-medium">{`${skipped.length} not added`}</p>
            <ul className="flex flex-col gap-1 text-body">
              {skipped.map((row) => (
                <li key={row.index} className="flex gap-2">
                  <span className="font-mono">{skippedLabel(row.index)}</span>
                  <span className="text-muted-foreground">{row.message}</span>
                </li>
              ))}
            </ul>
          </div>
        ) : null}
        <DialogFooter>
          <Button type="button" variant="outline" onClick={onClose}>
            Close
          </Button>
          <Link
            to="/$org/$project/annotation-queues/$queueId"
            params={{ org, project, queueId: result.queue.id }}
            className={buttonVariants({ variant: "default" })}
          >
            Open queue
          </Link>
        </DialogFooter>
      </div>
    );
  }

  const message = formError ?? (query.isError ? "The queues could not be loaded." : undefined);

  return (
    <>
      <form noValidate className="flex flex-col gap-4" onSubmit={onSubmit}>
        <Select value={picked?.id ?? ""} onValueChange={pickQueue}>
          <SelectTrigger aria-label="Queue" className="w-full">
            <SelectValue placeholder="Choose a queue" />
          </SelectTrigger>
          <SelectContent>
            {options.map((queue) => (
              <SelectItem key={queue.id} value={queue.id}>
                {kinds.length === 1 ? queue.name : `${queue.name} · ${queue.target}`}
              </SelectItem>
            ))}
            <SelectItem value={NEW_QUEUE}>New queue…</SelectItem>
          </SelectContent>
        </Select>
        <FormError message={message} />
        <DialogFooter>
          <SubmitButton pending={adding}>Add</SubmitButton>
        </DialogFooter>
      </form>
      {/* Outside the form: a submit of the create form would otherwise reach this form's handler. */}
      <QueueDialog
        open={newOpen}
        onOpenChange={setNewOpen}
        defaultTarget={defaultKind}
        onCreated={(created) => {
          setPicked(created);
        }}
      />
    </>
  );
}

/** The short id of the item a skipped row names. */
function itemLabel(item: QueueItemTarget | undefined): string {
  return shortId(item?.span_id ?? item?.trace_id ?? item?.session_id ?? "");
}
