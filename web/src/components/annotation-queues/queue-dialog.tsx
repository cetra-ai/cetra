import { useQueryClient } from "@tanstack/react-query";
import { type FormEvent, useState } from "react";
import { $api } from "@/api/client";
import { fieldErrors, problemMessage, problemOf } from "@/api/problem";
import { projectRef } from "@/api/project-ref";
import { annotationQueueWriteKeys } from "@/api/queries-annotation-queues";
import type { Schema } from "@/api/types";
import { DefinitionsPicker } from "@/components/annotation-queues/definitions-picker";
import { FormError } from "@/components/form/form-error";
import { SubmitButton } from "@/components/form/submit-button";
import { TextField } from "@/components/form/text-field";
import { UpgradeButton } from "@/components/plan/upgrade-button";
import { useOrgEntitlements } from "@/components/plan/use-org-entitlements";
import { useProject } from "@/components/shell/project-context";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Label } from "@/components/ui/label";
import { RadioGroup, RadioGroupItem } from "@/components/ui/radio-group";
import { Textarea } from "@/components/ui/textarea";
import { QUEUE_NAME_TAKEN, targetLabel } from "@/lib/annotation-queues";

type Queue = Schema<"AnnotationQueue">;
type QueueTarget = Schema<"QueueTarget">;

const TARGETS: readonly QueueTarget[] = ["trace", "span", "session"];
const FALLBACK = "The queue could not be saved";
const STALE = "Someone else changed this queue. Close and open it again.";

function useCreateQueue() {
  return $api.useMutation("post", "/api/v1/projects/{project}/annotation-queues");
}

function usePatchQueue() {
  return $api.useMutation("patch", "/api/v1/projects/{project}/annotation-queues/{id}");
}

/**
 * Creates a queue, or edits `queue` when it is given. A form the user has typed in is not closed
 * by Escape or a click outside; the X still closes it.
 */
export function QueueDialog({
  open,
  onOpenChange,
  queue,
  defaultTarget,
  onCreated,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  /** Absent: create. Present: edit this queue. Its kind cannot change. */
  queue?: Queue;
  /** The kind a new queue starts with. */
  defaultTarget?: QueueTarget;
  /** Called with the new queue after a create. */
  onCreated?: (queue: Queue) => void;
}) {
  const create = useCreateQueue();
  const patch = usePatchQueue();
  const pending = queue === undefined ? create.isPending : patch.isPending;
  const [dirty, setDirty] = useState(false);
  const keepOpen = pending || dirty;

  // Unmounting the form on close drops its state with it, so reopening starts clean.
  function close() {
    create.reset();
    patch.reset();
    setDirty(false);
    onOpenChange(false);
  }

  return (
    <Dialog
      open={open}
      onOpenChange={(next) => {
        if (!next && !pending) {
          close();
        }
      }}
    >
      <DialogContent
        className="max-h-[90vh] overflow-y-auto"
        onInteractOutside={(event) => {
          if (keepOpen) {
            event.preventDefault();
          }
        }}
        onEscapeKeyDown={(event) => {
          if (keepOpen) {
            event.preventDefault();
          }
        }}
      >
        <DialogHeader>
          <DialogTitle>{queue ? "Edit queue" : "New annotation queue"}</DialogTitle>
          <DialogDescription>
            Reviewers work through the queue's items in order, with its definitions.
          </DialogDescription>
        </DialogHeader>
        {open ? (
          <QueueForm
            queue={queue}
            defaultTarget={defaultTarget}
            create={create}
            patch={patch}
            onCreated={onCreated}
            onDirty={() => {
              setDirty(true);
            }}
            onDone={close}
          />
        ) : null}
      </DialogContent>
    </Dialog>
  );
}

function QueueForm({
  queue,
  defaultTarget,
  create,
  patch,
  onCreated,
  onDirty,
  onDone,
}: {
  queue: Queue | undefined;
  defaultTarget: QueueTarget | undefined;
  create: ReturnType<typeof useCreateQueue>;
  patch: ReturnType<typeof usePatchQueue>;
  onCreated: ((queue: Queue) => void) | undefined;
  onDirty: () => void;
  onDone: () => void;
}) {
  const { org, project } = useProject();
  const queryClient = useQueryClient();
  const upgradeUrl = useOrgEntitlements(org).upgradeUrl;
  const [name, setName] = useState(queue?.name ?? "");
  const [description, setDescription] = useState(queue?.description ?? "");
  const [instructions, setInstructions] = useState(queue?.instructions ?? "");
  const [kind, setKind] = useState<QueueTarget>(queue?.target ?? defaultTarget ?? "trace");
  const [definitionIds, setDefinitionIds] = useState<string[]>([...(queue?.definition_ids ?? [])]);
  const [errors, setErrors] = useState<Record<string, string>>({});
  const [error, setError] = useState<string | undefined>();
  const pending = queue === undefined ? create.isPending : patch.isPending;
  const planLimited =
    problemOf(queue === undefined ? create.error : patch.error)?.code === "plan_limit";

  function onSubmit(event: FormEvent) {
    event.preventDefault();
    setError(undefined);
    setErrors({});
    const trimmedName = name.trim();
    const nextErrors: Record<string, string> = {};
    if (trimmedName === "") {
      nextErrors.name = "Enter a name.";
    }
    if (definitionIds.length === 0) {
      nextErrors.definition_ids = "Choose at least one score definition.";
    }
    if (Object.keys(nextErrors).length > 0) {
      setErrors(nextErrors);
      return;
    }
    const trimmedDescription = description.trim();
    const trimmedInstructions = instructions.trim();

    if (queue === undefined) {
      const body: Schema<"CreateAnnotationQueue"> = {
        name: trimmedName,
        target: kind,
        definition_ids: definitionIds,
      };
      if (trimmedDescription !== "") {
        body.description = trimmedDescription;
      }
      if (trimmedInstructions !== "") {
        body.instructions = trimmedInstructions;
      }
      create.mutate(
        { params: { path: { project: projectRef(org, project) } }, body },
        { onSuccess: saved, onError },
      );
      return;
    }

    // Only changed fields are sent, with the version the change is based on.
    // A cleared text is sent as "".
    const body: Schema<"UpdateAnnotationQueue"> = { version: queue.version };
    if (trimmedName !== queue.name) {
      body.name = trimmedName;
    }
    if (trimmedDescription !== (queue.description ?? "")) {
      body.description = trimmedDescription;
    }
    if (trimmedInstructions !== (queue.instructions ?? "")) {
      body.instructions = trimmedInstructions;
    }
    if (!sameIds(definitionIds, queue.definition_ids)) {
      body.definition_ids = definitionIds;
    }
    if (Object.keys(body).length === 1) {
      onDone();
      return;
    }
    patch.mutate(
      { params: { path: { project: projectRef(org, project), id: queue.id } }, body },
      { onSuccess: saved, onError },
    );
  }

  function saved(result: Queue) {
    for (const key of annotationQueueWriteKeys(org, project, queue?.id)) {
      void queryClient.invalidateQueries({ queryKey: key });
    }
    if (queue === undefined) {
      onCreated?.(result);
    }
    onDone();
  }

  function onError(caught: unknown) {
    const problem = problemOf(caught);
    const placed: Record<string, string> = {};
    for (const [field, message] of Object.entries(fieldErrors(caught))) {
      if (field === "name" || field === "description" || field === "instructions") {
        placed[field] = message;
      } else if (field.startsWith("definition_ids")) {
        placed.definition_ids = message;
      }
    }
    const nameTaken = problem?.status === 409 && problem.detail === QUEUE_NAME_TAKEN;
    const stale = problem?.status === 409 && queue !== undefined && !nameTaken;
    if (nameTaken) {
      placed.name = "A queue with this name exists.";
    }
    setErrors(placed);
    if (stale) {
      setError(STALE);
    } else if (Object.keys(placed).length === 0) {
      setError(problemMessage(caught, FALLBACK));
    }
  }

  return (
    <form onSubmit={onSubmit} className="flex flex-col gap-4" noValidate>
      <TextField
        id="queue-name"
        label="Name"
        value={name}
        onChange={(value) => {
          setName(value);
          onDirty();
        }}
        error={errors.name}
        autoComplete="off"
        maxLength={100}
      />
      {queue === undefined ? (
        <div className="flex flex-col gap-1.5">
          <span className="text-caption font-medium">Kind</span>
          <RadioGroup
            aria-label="Kind"
            value={kind}
            onValueChange={(value) => {
              const next = TARGETS.find((target) => target === value);
              if (next !== undefined) {
                setKind(next);
                onDirty();
              }
            }}
            className="flex flex-wrap gap-4"
          >
            {TARGETS.map((target) => (
              <div key={target} className="flex items-center gap-2">
                <RadioGroupItem value={target} id={`queue-kind-${target}`} />
                <Label htmlFor={`queue-kind-${target}`}>{targetLabel(target)}</Label>
              </div>
            ))}
          </RadioGroup>
        </div>
      ) : (
        <div className="flex flex-col gap-1.5">
          <span className="text-caption font-medium">Kind</span>
          <p className="text-body">{targetLabel(queue.target)}</p>
        </div>
      )}
      <div className="flex flex-col gap-1.5">
        <Label htmlFor="queue-description">Description</Label>
        <Textarea
          id="queue-description"
          value={description}
          maxLength={1000}
          rows={2}
          aria-invalid={errors.description ? true : undefined}
          onChange={(event) => {
            setDescription(event.target.value);
            onDirty();
          }}
        />
        {errors.description ? (
          <p className="text-caption text-destructive">{errors.description}</p>
        ) : null}
      </div>
      <div className="flex flex-col gap-1.5">
        <Label htmlFor="queue-instructions">Instructions</Label>
        <Textarea
          id="queue-instructions"
          value={instructions}
          maxLength={10000}
          rows={6}
          aria-describedby="queue-instructions-hint"
          aria-invalid={errors.instructions ? true : undefined}
          onChange={(event) => {
            setInstructions(event.target.value);
            onDirty();
          }}
        />
        <p id="queue-instructions-hint" className="text-caption text-muted-foreground">
          Markdown. Reviewers see it above the form.
        </p>
        {errors.instructions ? (
          <p className="text-caption text-destructive">{errors.instructions}</p>
        ) : null}
      </div>
      <div className="flex flex-col gap-1.5">
        <span className="text-caption font-medium">Definitions</span>
        <DefinitionsPicker
          value={definitionIds}
          onChange={(next) => {
            setDefinitionIds(next);
            onDirty();
          }}
        />
        {errors.definition_ids ? (
          <p className="text-caption text-destructive">{errors.definition_ids}</p>
        ) : null}
      </div>
      <FormError message={error} />
      {planLimited ? <UpgradeButton url={upgradeUrl} /> : null}
      <DialogFooter>
        <SubmitButton pending={pending}>
          {queue === undefined ? "Create queue" : "Save"}
        </SubmitButton>
      </DialogFooter>
    </form>
  );
}

function sameIds(left: readonly string[], right: readonly string[]): boolean {
  return left.length === right.length && left.every((id, index) => id === right[index]);
}
