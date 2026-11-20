import { useQueryClient } from "@tanstack/react-query";
import { Link } from "@tanstack/react-router";
import { type FormEvent, useEffect, useState } from "react";
import { $api } from "@/api/client";
import { problemMessage, problemOf } from "@/api/problem";
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
  const add = useAddItems();
  const busy = add.isPending;
  const count = items.length;
  const noun = NOUNS[target];

  return (
    <Dialog
      open={open}
      onOpenChange={(next) => {
        if (!next && busy) {
          return;
        }
        onOpenChange(next);
      }}
    >
      <DialogContent
        className="max-h-[90vh] overflow-y-auto"
        onInteractOutside={(event) => {
          if (busy) {
            event.preventDefault();
          }
        }}
        onEscapeKeyDown={(event) => {
          if (busy) {
            event.preventDefault();
          }
        }}
      >
        <DialogHeader>
          <DialogTitle>{`Add ${count} ${count === 1 ? noun : `${noun}s`} to a queue`}</DialogTitle>
          <DialogDescription>
            Reviewers work through the queue in order. An item already in the queue is not added
            twice.
          </DialogDescription>
        </DialogHeader>
        {open ? (
          <AddToQueueForm
            target={target}
            items={items}
            add={add}
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

function AddToQueueForm({
  target,
  items,
  add,
  onAdded,
  onClose,
}: {
  target: QueueKind;
  items: readonly QueueItemTarget[];
  add: ReturnType<typeof useAddItems>;
  onAdded: (() => void) | undefined;
  onClose: () => void;
}) {
  const { org, project } = useProject();
  const ref = projectRef(org, project);
  const queryClient = useQueryClient();
  const { queues, query } = useProjectQueues();
  const [picked, setPicked] = useState<Queue | undefined>();
  const [newOpen, setNewOpen] = useState(false);
  const [formError, setFormError] = useState<string | undefined>();
  const [result, setResult] = useState<
    { queue: Queue; answer: Schema<"AddedQueueItems"> } | undefined
  >();

  const listed = queues.filter((queue) => queue.target === target);
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
      const answer = await add.mutateAsync({
        params: { path: { project: ref, id: picked.id } },
        body: { items: [...items] },
      });
      for (const key of annotationQueueWriteKeys(org, project, picked.id)) {
        void queryClient.invalidateQueries({ queryKey: key });
      }
      onAdded?.();
      setResult({ queue: picked, answer });
    } catch (caught) {
      setFormError(
        queueFullMessage(caught) ?? problemMessage(caught, "The items could not be added."),
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
                  <span className="font-mono">{itemLabel(items[row.index])}</span>
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
                {queue.name}
              </SelectItem>
            ))}
            <SelectItem value={NEW_QUEUE}>New queue…</SelectItem>
          </SelectContent>
        </Select>
        <FormError message={message} />
        <DialogFooter>
          <SubmitButton pending={add.isPending}>Add</SubmitButton>
        </DialogFooter>
      </form>
      {/* Outside the form: a submit of the create form would otherwise reach this form's handler. */}
      <QueueDialog
        open={newOpen}
        onOpenChange={setNewOpen}
        defaultTarget={target}
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
