import { useQueryClient } from "@tanstack/react-query";
import { type FormEvent, useId, useState } from "react";
import { $api } from "@/api/client";
import { fieldErrors, problemMessage, problemOf } from "@/api/problem";
import { projectRef } from "@/api/project-ref";
import { invalidatePrompt } from "@/api/queries-prompts";
import { DisabledReason } from "@/components/disabled-reason";
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
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { isValidLabel, LABEL_FORBIDDEN_TEXT, PROTECTED_REASON } from "@/lib/prompts";

const FALLBACK = "Could not save the prompt.";
const LABEL_RULE_TEXT =
  'Use 1 to 64 lowercase letters, digits, _, . or -. "latest" and numbers alone are not labels.';

/**
 * Moves a label to a version (D-V-8). `labels` are the suggestions. A protected label is moved by
 * project admins only (D-V-10): below admin, Save is off with the reason.
 */
export function SetLabelDialog({
  open,
  onOpenChange,
  prompt,
  version,
  labels,
  protectedLabels,
  canProtect,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  prompt: string;
  /** The version the label moves to; undefined while closed. */
  version: number | undefined;
  labels: string[];
  protectedLabels: string[];
  canProtect: boolean;
}) {
  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>Set label</DialogTitle>
          <DialogDescription>
            {version === undefined
              ? ""
              : `The label points at v${version}. A label that already exists moves from its version.`}
          </DialogDescription>
        </DialogHeader>
        {open && version !== undefined ? (
          <SetLabelForm
            prompt={prompt}
            version={version}
            labels={labels}
            protectedLabels={protectedLabels}
            canProtect={canProtect}
            onDone={() => {
              onOpenChange(false);
            }}
          />
        ) : null}
      </DialogContent>
    </Dialog>
  );
}

function SetLabelForm({
  prompt,
  version,
  labels,
  protectedLabels,
  canProtect,
  onDone,
}: {
  prompt: string;
  version: number;
  labels: string[];
  protectedLabels: string[];
  canProtect: boolean;
  onDone: () => void;
}) {
  const { org, project } = useProject();
  const queryClient = useQueryClient();
  const idPrefix = useId();
  const put = $api.useMutation("put", "/api/v1/projects/{project}/prompts/{prompt}/labels/{label}");
  const [label, setLabel] = useState("");
  const [localError, setLocalError] = useState<string | undefined>();
  const typed = label.trim();
  const locked = typed !== "" && protectedLabels.includes(typed) && !canProtect;
  const serverErrors = put.error ? fieldErrors(put.error) : {};
  const forbidden = put.error ? problemOf(put.error)?.status === 403 : false;
  const fieldMessage = localError ?? serverErrors.label;
  const formMessage =
    localError === undefined && fieldMessage === undefined && put.error
      ? forbidden
        ? LABEL_FORBIDDEN_TEXT
        : problemMessage(put.error, FALLBACK)
      : undefined;

  function submit(event: FormEvent) {
    event.preventDefault();
    if (locked) {
      return;
    }
    if (!isValidLabel(typed)) {
      setLocalError(LABEL_RULE_TEXT);
      return;
    }
    setLocalError(undefined);
    put.mutate(
      {
        params: {
          path: { project: projectRef(org, project), prompt, label: typed },
        },
        body: { version },
      },
      {
        onSuccess: () => {
          invalidatePrompt(queryClient, org, project, prompt);
          onDone();
        },
      },
    );
  }

  const listId = `${idPrefix}-labels`;
  return (
    <form onSubmit={submit} className="flex flex-col gap-4" noValidate>
      <div className="flex flex-col gap-1.5">
        <Label htmlFor={`${idPrefix}-name`}>Label</Label>
        <Input
          id={`${idPrefix}-name`}
          list={listId}
          value={label}
          autoComplete="off"
          spellCheck={false}
          aria-invalid={fieldMessage ? true : undefined}
          onChange={(event) => {
            setLabel(event.target.value);
            setLocalError(undefined);
            put.reset();
          }}
        />
        <datalist id={listId}>
          {[...new Set([...labels, ...protectedLabels])].map((item) => (
            <option key={item} value={item} />
          ))}
        </datalist>
        {fieldMessage ? <p className="text-caption text-destructive">{fieldMessage}</p> : null}
      </div>
      <FormError message={formMessage} />
      <DialogFooter>
        <DisabledReason reason={locked ? PROTECTED_REASON : undefined}>
          <SubmitButton pending={put.isPending} disabled={locked || typed === ""}>
            Save
          </SubmitButton>
        </DisabledReason>
      </DialogFooter>
    </form>
  );
}
