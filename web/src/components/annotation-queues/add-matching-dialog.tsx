import { useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { $api } from "@/api/client";
import { problemMessage, problemOf } from "@/api/problem";
import { annotationQueueWriteKeys } from "@/api/queries-annotation-queues";
import type { Schema } from "@/api/types";
import {
  type QueueKind,
  queueFullMessage,
  useProjectQueues,
} from "@/components/annotation-queues/add-to-queue-dialog";
import { FormError } from "@/components/form/form-error";
import { useProject } from "@/components/shell/project-context";
import { Button } from "@/components/ui/button";
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

type Queue = Schema<"AnnotationQueue">;
type Matching = Schema<"QueueMatchingPreview">;

/** The list's selection: its filter, search and window. Only the kind's own fields are sent. */
export type MatchingSelection = {
  filter?: string;
  search?: string;
  from: string; // RFC 3339
  to: string; // RFC 3339
  includeExperiments: boolean;
};

const NOUNS: Record<QueueKind, string> = { trace: "trace", span: "span", session: "session" };
const PREVIEW = "/api/v1/projects/{project}/annotation-queues/{id}/items/matching/preview";
const ADD = "/api/v1/projects/{project}/annotation-queues/{id}/items/matching";
const CAPPED = "Only the first 10 000 are added. Narrow the filter to add the rest.";

function useAddMatching() {
  return $api.useMutation("post", ADD);
}

/**
 * Counts the items a list selection matches in one queue of its kind, then adds them to it
 * after a preview. A session queue never sends a search, and evaluation traces are never matched.
 */
export function AddMatchingDialog({
  open,
  onOpenChange,
  target,
  selection,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  target: QueueKind;
  selection: MatchingSelection;
}) {
  const add = useAddMatching();
  const busy = add.isPending;
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
          <DialogTitle>{`Add matching ${noun}s to a queue`}</DialogTitle>
          <DialogDescription>
            Items already in the queue are not added twice. The filter and time range of the list
            decide what matches.
          </DialogDescription>
        </DialogHeader>
        {open ? (
          <MatchingForm
            target={target}
            selection={selection}
            add={add}
            onClose={() => {
              onOpenChange(false);
            }}
          />
        ) : null}
      </DialogContent>
    </Dialog>
  );
}

function MatchingForm({
  target,
  selection,
  add,
  onClose,
}: {
  target: QueueKind;
  selection: MatchingSelection;
  add: ReturnType<typeof useAddMatching>;
  onClose: () => void;
}) {
  const { org, project, ref } = useProject();
  const queryClient = useQueryClient();
  const { queues, query } = useProjectQueues();
  const preview = $api.useMutation("post", PREVIEW);
  const [picked, setPicked] = useState<Queue | undefined>();
  const [error, setError] = useState<string | undefined>();
  const [result, setResult] = useState<Schema<"QueueMatchingAdded"> | undefined>();
  const noun = NOUNS[target];
  const plural = `${noun}s`;
  const body = matchingBody(target, selection);
  const listed = queues.filter((queue) => queue.target === target);

  // A new choice counts again. The mutation keeps only the latest answer, so an older one never shows.
  function pickQueue(value: string) {
    const next = listed.find((queue) => queue.id === value);
    if (next === undefined) {
      return;
    }
    setPicked(next);
    setError(undefined);
    preview.mutate({ params: { path: { project: ref, id: next.id } }, body });
  }

  function confirm() {
    if (picked === undefined) {
      return;
    }
    const queueId = picked.id;
    setError(undefined);
    add.mutate(
      { params: { path: { project: ref, id: queueId } }, body },
      {
        onSuccess: (answer) => {
          for (const key of annotationQueueWriteKeys(org, project, queueId)) {
            void queryClient.invalidateQueries({ queryKey: key });
          }
          setResult(answer);
        },
        onError: (caught) => {
          setError(messageOf(caught, "The items could not be added."));
        },
      },
    );
  }

  if (result !== undefined) {
    return (
      <div className="flex flex-col gap-4">
        <p className="text-ui">
          {`Added ${result.added}, ${result.existing} already there, ${result.blocked} blocked by a deletion.`}
        </p>
        <DialogFooter>
          <Button type="button" variant="outline" onClick={onClose}>
            Close
          </Button>
        </DialogFooter>
      </div>
    );
  }

  const counted: Matching | undefined = preview.data;
  const previewError = preview.isError
    ? messageOf(preview.error, "Could not count the matching items.")
    : undefined;
  let description: string;
  if (picked === undefined) {
    description = `Choose a queue to count the matching ${plural}.`;
  } else if (preview.isPending || preview.isIdle) {
    description = `Counting matching ${plural}…`;
  } else if (counted !== undefined && counted.matched === 0) {
    description = `No ${noun} matches. Nothing to add.`;
  } else if (counted !== undefined) {
    const matched = counted.matched.toLocaleString("en-US");
    const single = counted.matched === 1;
    description = `${matched} ${single ? noun : plural} ${single ? "matches" : "match"}.`;
  } else {
    description = "";
  }
  const showAdd =
    picked !== undefined && !preview.isPending && counted !== undefined && counted.matched > 0;
  const count = counted?.matched.toLocaleString("en-US") ?? "";

  return (
    <div className="flex flex-col gap-4">
      <Select value={picked?.id ?? ""} onValueChange={pickQueue}>
        <SelectTrigger aria-label="Queue" className="w-full">
          <SelectValue placeholder="Choose a queue" />
        </SelectTrigger>
        <SelectContent>
          {listed.map((queue) => (
            <SelectItem key={queue.id} value={queue.id}>
              {queue.name}
            </SelectItem>
          ))}
        </SelectContent>
      </Select>
      {query.isError ? <FormError message="The queues could not be loaded." /> : null}
      <p aria-live="polite" className="text-ui">
        {description}
      </p>
      {counted?.capped ? <p className="text-body text-muted-foreground">{CAPPED}</p> : null}
      <FormError message={error ?? previewError} />
      <DialogFooter>
        {showAdd ? (
          <Button type="button" disabled={add.isPending} onClick={confirm}>
            {`Add ${count} to queue`}
          </Button>
        ) : null}
      </DialogFooter>
    </div>
  );
}

/** The list's window and filter for the matching routes. Blank text is left out. */
function matchingBody(target: QueueKind, selection: MatchingSelection) {
  const filter = selection.filter?.trim();
  const search = target === "session" ? undefined : selection.search?.trim();
  return {
    ...(filter ? { filter } : {}),
    ...(search ? { search } : {}),
    from: selection.from,
    to: selection.to,
    include_experiments: selection.includeExperiments,
  };
}

function messageOf(caught: unknown, fallback: string): string {
  if (problemOf(caught)?.code === "invalid_filter") {
    return "The filter is not valid. Fix it on the list first.";
  }
  return queueFullMessage(caught) ?? problemMessage(caught, fallback);
}
