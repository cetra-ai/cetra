import { type FormEvent, useId, useState } from "react";
import { DisabledReason } from "@/components/disabled-reason";
import { FormError } from "@/components/form/form-error";
import { SubmitButton } from "@/components/form/submit-button";
import { TextField } from "@/components/form/text-field";
import { Checkbox } from "@/components/ui/checkbox";
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

const LABEL_RULE_TEXT =
  'Use 1 to 64 lowercase letters, digits, _, . or -. "latest" and numbers alone are not labels.';

/**
 * Asks for the commit message and the labels to move to a new version. The save itself is the
 * caller's: `onSave` gets the message and the labels. Errors of the commit message and the labels
 * come in `errors` by field.
 */
export function SaveVersionDialog({
  open,
  onOpenChange,
  labels,
  canProtect,
  protectedLabels,
  pending,
  errors,
  error,
  onSave,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  /** The labels the prompt has now, `latest` excluded. */
  labels: string[];
  canProtect: boolean;
  protectedLabels: string[];
  pending: boolean;
  errors: Record<string, string>;
  error: string | undefined;
  onSave: (input: { commitMessage: string | undefined; labels: string[] }) => void;
}) {
  return (
    <Dialog
      open={open}
      onOpenChange={(next) => {
        if (!pending) {
          onOpenChange(next);
        }
      }}
    >
      <DialogContent className="max-h-[90vh] overflow-y-auto">
        <DialogHeader>
          <DialogTitle>Save as new version</DialogTitle>
          <DialogDescription>
            The template and config become the next version. The labels you check move to it.
          </DialogDescription>
        </DialogHeader>
        <SaveVersionForm
          labels={labels}
          canProtect={canProtect}
          protectedLabels={protectedLabels}
          pending={pending}
          errors={errors}
          error={error}
          onSave={onSave}
        />
      </DialogContent>
    </Dialog>
  );
}

function SaveVersionForm({
  labels,
  canProtect,
  protectedLabels,
  pending,
  errors,
  error,
  onSave,
}: {
  labels: string[];
  canProtect: boolean;
  protectedLabels: string[];
  pending: boolean;
  errors: Record<string, string>;
  error: string | undefined;
  onSave: (input: { commitMessage: string | undefined; labels: string[] }) => void;
}) {
  const idPrefix = useId();
  const [message, setMessage] = useState("");
  const [checked, setChecked] = useState<string[]>([]);
  const [newLabel, setNewLabel] = useState("");
  const [newLabelError, setNewLabelError] = useState<string | undefined>();
  const labelsMessage = newLabelError ?? errors.labels ?? errors["labels[0]"];

  function submit(event: FormEvent) {
    event.preventDefault();
    const typed = newLabel.trim();
    if (typed !== "" && !isValidLabel(typed)) {
      setNewLabelError(LABEL_RULE_TEXT);
      return;
    }
    if (typed !== "" && protectedLabels.includes(typed) && !canProtect) {
      setNewLabelError(LABEL_FORBIDDEN_TEXT);
      return;
    }
    setNewLabelError(undefined);
    const trimmed = message.trim();
    onSave({
      commitMessage: trimmed === "" ? undefined : trimmed,
      labels: [...new Set([...checked, ...(typed !== "" ? [typed] : [])])],
    });
  }

  return (
    <form onSubmit={submit} className="flex flex-col gap-4" noValidate>
      <TextField
        id={`${idPrefix}-message`}
        label="Commit message"
        value={message}
        onChange={setMessage}
        error={errors.commit_message}
        maxLength={500}
      />
      <div role="group" aria-label="Labels" className="flex flex-col gap-2">
        {labels.map((label) => {
          const locked = protectedLabels.includes(label) && !canProtect;
          const id = `${idPrefix}-label-${label}`;
          return (
            <DisabledReason key={label} reason={locked ? PROTECTED_REASON : undefined}>
              <div className="flex items-center gap-2">
                <Checkbox
                  id={id}
                  checked={checked.includes(label)}
                  disabled={locked}
                  onCheckedChange={(next) => {
                    setChecked((current) =>
                      next === true
                        ? [...current.filter((item) => item !== label), label]
                        : current.filter((item) => item !== label),
                    );
                  }}
                />
                <Label htmlFor={id}>{label}</Label>
              </div>
            </DisabledReason>
          );
        })}
        <div className="flex flex-col gap-1.5">
          <Label htmlFor={`${idPrefix}-new-label`}>New label</Label>
          <Input
            id={`${idPrefix}-new-label`}
            value={newLabel}
            autoComplete="off"
            spellCheck={false}
            className="h-7 text-body"
            aria-invalid={labelsMessage ? true : undefined}
            onChange={(event) => {
              setNewLabel(event.target.value);
            }}
          />
          <p className="text-caption text-muted-foreground">
            Optional. The label moves to the new version.
          </p>
          {labelsMessage ? <p className="text-caption text-destructive">{labelsMessage}</p> : null}
        </div>
      </div>
      <FormError message={error} />
      <DialogFooter>
        <SubmitButton pending={pending}>Save version</SubmitButton>
      </DialogFooter>
    </form>
  );
}
