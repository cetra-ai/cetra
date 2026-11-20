import { useQuery, useQueryClient } from "@tanstack/react-query";
import { type FormEvent, useState } from "react";
import { $api } from "@/api/client";
import { fieldErrors, problemMessage } from "@/api/problem";
import { projectRef } from "@/api/project-ref";
import { promptSettingsKey, promptSettingsQuery } from "@/api/queries-prompts";
import { FormError } from "@/components/form/form-error";
import { SubmitButton } from "@/components/form/submit-button";
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
import { Textarea } from "@/components/ui/textarea";

const FALLBACK = "Could not save the protected labels.";

/**
 * Edits the project's protected labels, one per line (D-V-10). Project admins only; the list page
 * disables the button that opens it below admin.
 */
export function ProtectedLabelsDialog({
  open,
  onOpenChange,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
}) {
  const [dirty, setDirty] = useState(false);
  const save = useSave();
  const keepOpen = save.isPending || dirty;

  function close() {
    save.reset();
    setDirty(false);
    onOpenChange(false);
  }

  return (
    <Dialog
      open={open}
      onOpenChange={(next) => {
        if (!next && !save.isPending) {
          close();
        }
      }}
    >
      <DialogContent
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
          <DialogTitle>Protected labels</DialogTitle>
          <DialogDescription>
            Only project admins can move these labels. Other members see them locked.
          </DialogDescription>
        </DialogHeader>
        {open ? (
          <ProtectedLabelsForm
            save={save}
            onDirty={() => {
              setDirty(true);
            }}
            onSaved={close}
          />
        ) : null}
      </DialogContent>
    </Dialog>
  );
}

function ProtectedLabelsForm({
  save,
  onDirty,
  onSaved,
}: {
  save: ReturnType<typeof useSave>;
  onDirty: () => void;
  onSaved: () => void;
}) {
  const { org, project } = useProject();
  const queryClient = useQueryClient();
  const settings = useQuery(promptSettingsQuery(org, project));
  // The text starts as the saved labels and is only replaced once the user types.
  const [text, setText] = useState<string | undefined>(undefined);
  const [errors, setErrors] = useState<Record<string, string>>({});
  const [error, setError] = useState<string | undefined>();
  const shown = text ?? (settings.data?.protected_labels ?? []).join("\n");

  function onSubmit(event: FormEvent) {
    event.preventDefault();
    setErrors({});
    setError(undefined);
    const protectedLabels = shown
      .split("\n")
      .map((line) => line.trim())
      .filter((line) => line !== "");
    save.mutate(
      {
        params: { path: { project: projectRef(org, project) } },
        body: { protected_labels: protectedLabels },
      },
      {
        onSuccess: () => {
          void queryClient.invalidateQueries({ queryKey: promptSettingsKey(org, project) });
          onSaved();
        },
        onError: (caught) => {
          const fields = fieldErrors(caught);
          const placed: Record<string, string> = {};
          for (const [field, message] of Object.entries(fields)) {
            if (field.startsWith("protected_labels")) {
              placed[field] = message;
            }
          }
          setErrors(placed);
          if (Object.keys(placed).length === 0) {
            setError(problemMessage(caught, FALLBACK));
          }
        },
      },
    );
  }

  const fieldError = Object.values(errors)[0];

  return (
    <form onSubmit={onSubmit} className="flex flex-col gap-4" noValidate>
      <div className="flex flex-col gap-1.5">
        <Label htmlFor="protected-labels">Labels</Label>
        <Textarea
          id="protected-labels"
          aria-label="Protected labels"
          className="font-mono text-body"
          spellCheck={false}
          rows={5}
          value={shown}
          disabled={settings.isPending}
          aria-invalid={fieldError ? true : undefined}
          aria-describedby="protected-labels-hint"
          onChange={(event) => {
            setText(event.target.value);
            onDirty();
          }}
        />
        <p id="protected-labels-hint" className="text-caption text-muted-foreground">
          One label per line, at most 20. The default is production.
        </p>
        {fieldError ? <p className="text-caption text-destructive">{fieldError}</p> : null}
      </div>
      <FormError message={error} />
      <DialogFooter>
        <SubmitButton pending={save.isPending} disabled={settings.isPending}>
          Save
        </SubmitButton>
      </DialogFooter>
    </form>
  );
}

function useSave() {
  return $api.useMutation("put", "/api/v1/projects/{project}/prompt-settings");
}
