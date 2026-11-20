import { useQuery, useQueryClient } from "@tanstack/react-query";
import { useNavigate } from "@tanstack/react-router";
import { type FormEvent, useState } from "react";
import { $api } from "@/api/client";
import { fieldErrors, problemMessage, problemOf } from "@/api/problem";
import { projectRef } from "@/api/project-ref";
import { invalidatePrompt, promptSettingsQuery, promptsKey } from "@/api/queries-prompts";
import type { Schema } from "@/api/types";
import { DisabledReason } from "@/components/disabled-reason";
import { FormError } from "@/components/form/form-error";
import { SubmitButton } from "@/components/form/submit-button";
import { TextField } from "@/components/form/text-field";
import {
  fieldMessage,
  TemplateEditor,
  withoutTemplate,
} from "@/components/prompts/template-editor";
import { useProject } from "@/components/shell/project-context";
import { Checkbox } from "@/components/ui/checkbox";
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
import { parseJsonField } from "@/lib/datasets";
import {
  type ChatEntry,
  DEFAULT_PROTECTED_LABELS,
  emptyChat,
  fieldPositions,
  isValidPromptName,
  type Position,
  type PromptKind,
} from "@/lib/prompts";
import { atLeast } from "@/lib/roles";

/** The API's detail for a name that is already taken in the project. */
const NAME_TAKEN_DETAIL = "a prompt with this name already exists";
const NAME_TAKEN_TEXT = "A prompt with this name already exists";
const PROTECTED_PRODUCTION_TEXT =
  'Only project admins can set the production label; uncheck "Label as production".';
const PROTECTED_REASON = "Only project admins can move protected labels";
const FALLBACK = "Could not save the prompt.";
/** Field errors the form renders under their own field. */
const PLACED_FIELD = /^(name|config|template|labels)(\W|$)/;

function useCreatePrompt() {
  return $api.useMutation("post", "/api/v1/projects/{project}/prompts");
}

/** The dialog that creates a prompt with its first version. Opened from the list's `?create=1`. */
export function NewPromptDialog({
  open,
  onOpenChange,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
}) {
  const create = useCreatePrompt();
  // A form the user has typed in is not closed by Escape or a click outside (the X still closes it).
  const [dirty, setDirty] = useState(false);
  const keepOpen = create.isPending || dirty;

  // Unmounting the form on close drops its state with it, so reopening starts clean.
  function close() {
    create.reset();
    setDirty(false);
    onOpenChange(false);
  }

  return (
    <Dialog
      open={open}
      onOpenChange={(next) => {
        if (!next && !create.isPending) {
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
          <DialogTitle>New prompt</DialogTitle>
          <DialogDescription>
            The first version is saved with this prompt. Later versions are added from its page.
          </DialogDescription>
        </DialogHeader>
        {open ? (
          <NewPromptForm
            create={create}
            onDirty={() => {
              setDirty(true);
            }}
          />
        ) : null}
      </DialogContent>
    </Dialog>
  );
}

function NewPromptForm({
  create,
  onDirty,
}: {
  create: ReturnType<typeof useCreatePrompt>;
  onDirty: () => void;
}) {
  const { org, project, data } = useProject();
  const queryClient = useQueryClient();
  const navigate = useNavigate();
  const settings = useQuery(promptSettingsQuery(org, project));
  const [name, setName] = useState("");
  const [type, setType] = useState<PromptKind>("text");
  const [text, setText] = useState("");
  const [chat, setChat] = useState<ChatEntry[]>(emptyChat);
  const [configText, setConfigText] = useState("");
  const [commitMessage, setCommitMessage] = useState("");
  const [production, setProduction] = useState(true);
  const [errors, setErrors] = useState<Record<string, string>>({});
  const [positions, setPositions] = useState<Record<string, Position>>({});
  const [error, setError] = useState<string | undefined>();

  // Below admin, a protected `production` cannot be set on create: the box is unchecked and off.
  const protectedLabels = settings.data?.protected_labels ?? DEFAULT_PROTECTED_LABELS;
  const productionProtected = protectedLabels.includes("production");
  const productionLocked = productionProtected && !atLeast(data.role, "admin");
  const configField = Object.keys(errors).find((field) => /^config(\W|$)/.test(field));
  const configMessage =
    configField === undefined ? undefined : fieldMessage(configField, errors, positions);

  function switchType(next: PromptKind) {
    if (next === type) {
      return;
    }
    setType(next);
    setText("");
    setChat(emptyChat());
    onDirty();
  }

  function onSubmit(event: FormEvent) {
    event.preventDefault();
    setError(undefined);
    setErrors({});
    setPositions({});
    const trimmedName = name.trim();
    const config = parseJsonField(configText, { required: false, object: true });
    const nextErrors: Record<string, string> = {};
    if (trimmedName === "") {
      nextErrors.name = "Enter a name.";
    } else if (!isValidPromptName(trimmedName)) {
      nextErrors.name =
        "Use letters, digits, _, . and -, starting with a letter or digit (200 characters at most).";
    }
    if (!config.ok) {
      nextErrors.config = config.message;
    }
    if (Object.keys(nextErrors).length > 0 || !config.ok) {
      setErrors(nextErrors);
      return;
    }

    const labels = production && !productionLocked ? ["production"] : [];
    const body: Schema<"CreatePromptRequest"> = {
      name: trimmedName,
      type,
      template: type === "text" ? text : chat,
      labels,
    };
    if (config.value !== undefined) {
      body.config = config.value as Schema<"CreatePromptRequest">["config"];
    }
    const message = commitMessage.trim();
    if (message !== "") {
      body.commit_message = message;
    }
    const touchesProtected = labels.some((label) => protectedLabels.includes(label));
    create.mutate(
      { params: { path: { project: projectRef(org, project) } }, body },
      {
        onSuccess: saved,
        onError: (caught) => {
          onError(caught, touchesProtected);
        },
      },
    );
  }

  function saved(prompt: Schema<"PromptVersion">) {
    void queryClient.invalidateQueries({ queryKey: promptsKey(org, project) });
    invalidatePrompt(queryClient, org, project, prompt.name);
    // The new prompt opens on its own page; the dialog goes with the list page it was opened from.
    void navigate({
      to: "/$org/$project/prompts/$prompt",
      params: { org, project, prompt: prompt.name },
    });
  }

  function onError(caught: unknown, touchesProtected: boolean) {
    const problem = problemOf(caught);
    const next: Record<string, string> = {};
    let placed = false;
    for (const [field, message] of Object.entries(fieldErrors(caught))) {
      if (PLACED_FIELD.test(field)) {
        next[field] = message;
        placed = true;
      }
    }
    if (problem?.status === 409 && problem.detail === NAME_TAKEN_DETAIL) {
      next.name = NAME_TAKEN_TEXT;
      placed = true;
    }
    setErrors(next);
    setPositions(fieldPositions(caught));
    if (problem?.status === 403 && touchesProtected) {
      setError(PROTECTED_PRODUCTION_TEXT);
    } else if (!placed) {
      setError(problemMessage(caught, FALLBACK));
    }
  }

  const labelsError = errors["labels[0]"] ?? errors.labels;
  // A template edit clears the template's errors, which describe the template as it was sent.
  const clearTemplateErrors = () => {
    setErrors(withoutTemplate);
    setPositions(withoutTemplate);
  };

  return (
    <form onSubmit={onSubmit} className="flex flex-col gap-4" noValidate>
      <TextField
        id="prompt-name"
        label="Name"
        value={name}
        onChange={(value) => {
          setName(value);
          onDirty();
        }}
        error={errors.name}
        description="Letters, digits, _, . and -. The name cannot be changed later."
        autoComplete="off"
        maxLength={200}
      />
      <div className="flex flex-col gap-2">
        <Label>Type</Label>
        <RadioGroup
          aria-label="Type"
          value={type}
          onValueChange={(value) => {
            switchType(value as PromptKind);
          }}
          className="flex gap-4"
        >
          <div className="flex items-center gap-2">
            <RadioGroupItem value="text" id="prompt-type-text" />
            <Label htmlFor="prompt-type-text">Text</Label>
          </div>
          <div className="flex items-center gap-2">
            <RadioGroupItem value="chat" id="prompt-type-chat" />
            <Label htmlFor="prompt-type-chat">Chat</Label>
          </div>
        </RadioGroup>
      </div>
      {type === "text" ? (
        <TemplateEditor
          type="text"
          value={text}
          onChange={(value) => {
            clearTemplateErrors();
            setText(value);
            onDirty();
          }}
          errors={errors}
          positions={positions}
        />
      ) : (
        <TemplateEditor
          type="chat"
          value={chat}
          onChange={(value) => {
            clearTemplateErrors();
            setChat(value);
            onDirty();
          }}
          errors={errors}
          positions={positions}
        />
      )}
      <div className="flex flex-col gap-1.5">
        <Label htmlFor="prompt-config">Config</Label>
        <Textarea
          id="prompt-config"
          className="font-mono text-caption"
          spellCheck={false}
          rows={4}
          value={configText}
          aria-invalid={configMessage ? true : undefined}
          aria-describedby="prompt-config-hint"
          onChange={(event) => {
            setConfigText(event.target.value);
            onDirty();
          }}
        />
        <p id="prompt-config-hint" className="text-caption text-muted-foreground">
          A JSON object with the model and its parameters. Leave empty for none.
        </p>
        {configMessage ? <p className="text-caption text-destructive">{configMessage}</p> : null}
      </div>
      <TextField
        id="prompt-commit-message"
        label="Commit message"
        value={commitMessage}
        onChange={(value) => {
          setCommitMessage(value);
          onDirty();
        }}
        maxLength={500}
      />
      <div className="flex flex-col gap-1.5">
        <DisabledReason reason={productionLocked ? PROTECTED_REASON : undefined}>
          <div className="flex items-center gap-2">
            <Checkbox
              id="prompt-label-production"
              checked={production && !productionLocked}
              disabled={productionLocked}
              onCheckedChange={(next) => {
                setProduction(next === true);
                onDirty();
              }}
            />
            <Label htmlFor="prompt-label-production">Label as production</Label>
          </div>
        </DisabledReason>
        {labelsError ? <p className="text-caption text-destructive">{labelsError}</p> : null}
      </div>
      <FormError message={error} />
      <DialogFooter>
        <SubmitButton pending={create.isPending}>Create prompt</SubmitButton>
      </DialogFooter>
    </form>
  );
}
