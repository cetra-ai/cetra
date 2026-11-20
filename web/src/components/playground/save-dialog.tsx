import { useQuery, useQueryClient } from "@tanstack/react-query";
import { Link } from "@tanstack/react-router";
import { type FormEvent, useId, useState } from "react";
import { $api } from "@/api/client";
import { fieldErrors, problemMessage, problemOf } from "@/api/problem";
import { projectRef } from "@/api/project-ref";
import { invalidatePrompt, promptSettingsQuery, promptsInit } from "@/api/queries-prompts";
import type { Schema } from "@/api/types";
import { DisabledReason } from "@/components/disabled-reason";
import { FormError } from "@/components/form/form-error";
import { SubmitButton } from "@/components/form/submit-button";
import { TextField } from "@/components/form/text-field";
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
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { RadioGroup, RadioGroupItem } from "@/components/ui/radio-group";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { type Draft, toChatTemplate, toRunRequest } from "@/lib/playground";
import {
  DEFAULT_PROTECTED_LABELS,
  LABEL_FORBIDDEN_TEXT,
  PROTECTED_REASON,
  isValidLabel,
  isValidPromptName,
} from "@/lib/prompts";
import { atLeast } from "@/lib/roles";

type Connections = readonly Schema<"LlmConnection">[];
type SavedVersion = Schema<"PromptVersion">;

const FALLBACK = "Could not save the prompt.";
const NAME_TAKEN_DETAIL = "a prompt with this name already exists";
const NAME_TAKEN_TEXT = "A prompt with this name already exists";
const NAME_RULE_TEXT =
  "Use letters, digits, _, . and -, starting with a letter or digit (200 characters at most).";
const LABEL_RULE_TEXT =
  'Use 1 to 64 lowercase letters, digits, _, . or -. "latest" and numbers alone are not labels.';
const TEXT_PROMPT_TEXT = "Text prompts take one text; save as a new prompt instead.";
const CONFIG_BLOCKED_TEXT = "Fix the settings of the first model to save.";
/** The fields a save can name: the commit message, the name and the labels. */
const SAVE_FIELD = /^(commit_message|name|labels)(\W|$)/;
/** The select's value for no prompt chosen (an empty string is not a select value). */
const NO_PROMPT = "__none__";

function useCreateVersion() {
  return $api.useMutation("post", "/api/v1/projects/{project}/prompts/{prompt}/versions");
}

function useCreatePrompt() {
  return $api.useMutation("post", "/api/v1/projects/{project}/prompts");
}

/**
 * The first model's settings as a prompt version's config (`model`, `connection_id`, `params`,
 * `tools`, `tool_choice`, `response_format`), or `undefined` when that model cannot run as it is.
 */
function configOf(draft: Draft, connections: Connections): Record<string, unknown> | undefined {
  const column = draft.columns[0];
  if (column === undefined) {
    return undefined;
  }
  const checked = toRunRequest(draft, column, connections);
  if (!checked.ok) {
    return undefined;
  }
  const { request } = checked;
  return {
    model: request.model,
    connection_id: request.connection_id,
    ...(request.params === undefined ? {} : { params: request.params }),
    ...(request.tools === undefined ? {} : { tools: request.tools }),
    ...(request.tool_choice === undefined ? {} : { tool_choice: request.tool_choice }),
    ...(request.response_format === undefined
      ? {}
      : { response_format: request.response_format }),
  };
}

/**
 * Saves the playground's messages and the first model's settings as a prompt version: a new
 * version of an existing chat prompt, or a new chat prompt (D-V-27). Text-only messages only.
 */
export function SaveDialog({
  open,
  onOpenChange,
  draft,
  connections,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  draft: Draft;
  connections: Connections;
}) {
  const createVersion = useCreateVersion();
  const createPrompt = useCreatePrompt();
  const pending = createVersion.isPending || createPrompt.isPending;
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
            The messages and the first model's settings become a prompt version: a new version of a
            chat prompt, or a new chat prompt. The labels you check move to the new version.
          </DialogDescription>
        </DialogHeader>
        {open ? (
          <SaveForm
            draft={draft}
            connections={connections}
            createVersion={createVersion}
            createPrompt={createPrompt}
          />
        ) : null}
      </DialogContent>
    </Dialog>
  );
}

function SaveForm({
  draft,
  connections,
  createVersion,
  createPrompt,
}: {
  draft: Draft;
  connections: Connections;
  createVersion: ReturnType<typeof useCreateVersion>;
  createPrompt: ReturnType<typeof useCreatePrompt>;
}) {
  const { org, project, data } = useProject();
  const queryClient = useQueryClient();
  const idPrefix = useId();
  const prompts = $api.useQuery(
    "get",
    "/api/v1/projects/{project}/prompts",
    promptsInit(org, project, {}),
  );
  const settings = useQuery(promptSettingsQuery(org, project));
  const protectedLabels = settings.data?.protected_labels ?? DEFAULT_PROTECTED_LABELS;
  const canProtect = atLeast(data.role, "admin");
  const productionLocked = protectedLabels.includes("production") && !canProtect;

  const [mode, setMode] = useState<"existing" | "new">("existing");
  const [promptName, setPromptName] = useState("");
  const [newName, setNewName] = useState("");
  const [message, setMessage] = useState("");
  const [checked, setChecked] = useState<string[]>([]);
  const [production, setProduction] = useState(true);
  const [newLabel, setNewLabel] = useState("");
  const [errors, setErrors] = useState<Record<string, string>>({});
  const [error, setError] = useState<string | undefined>();
  const [saved, setSaved] = useState<SavedVersion | undefined>();

  const promptRows = prompts.data?.data ?? [];
  const prompt = promptRows.find((item) => item.name === promptName);
  const labels = prompt?.labels.map((item) => item.label) ?? [];
  const template = toChatTemplate(draft);
  const config = configOf(draft, connections);
  const reason = !template.ok
    ? `Message ${template.blocked} has tool parts; a prompt version holds text messages only.`
    : config === undefined
      ? CONFIG_BLOCKED_TEXT
      : undefined;
  const pending = createVersion.isPending || createPrompt.isPending;

  // A failed save: the fields it names go under their controls, a protected label or any other
  // problem in the form's error line.
  function failed(caught: unknown, touchesProtected: boolean) {
    const next: Record<string, string> = {};
    for (const [field, text] of Object.entries(fieldErrors(caught))) {
      if (SAVE_FIELD.test(field)) {
        next[field] = text;
      }
    }
    const problem = problemOf(caught);
    if (problem?.status === 409 && problem.detail === NAME_TAKEN_DETAIL) {
      next.name = NAME_TAKEN_TEXT;
    }
    setErrors(next);
    if (problem?.status === 403 && touchesProtected) {
      setError(LABEL_FORBIDDEN_TEXT);
    } else if (Object.keys(next).length === 0) {
      setError(problemMessage(caught, FALLBACK));
    }
  }

  function submit(event: FormEvent) {
    event.preventDefault();
    setError(undefined);
    setErrors({});
    setSaved(undefined);
    const nextErrors: Record<string, string> = {};
    const trimmedName = newName.trim();
    if (mode === "existing") {
      if (prompt === undefined) {
        nextErrors.prompt = "Choose a prompt.";
      }
    } else if (trimmedName === "") {
      nextErrors.name = "Enter a name.";
    } else if (!isValidPromptName(trimmedName)) {
      nextErrors.name = NAME_RULE_TEXT;
    }
    const typedLabel = newLabel.trim();
    if (typedLabel !== "" && !isValidLabel(typedLabel)) {
      nextErrors.labels = LABEL_RULE_TEXT;
    } else if (typedLabel !== "" && protectedLabels.includes(typedLabel) && !canProtect) {
      nextErrors.labels = LABEL_FORBIDDEN_TEXT;
    }
    if (Object.keys(nextErrors).length > 0) {
      setErrors(nextErrors);
      return;
    }
    if (template.ok === false || config === undefined) {
      return;
    }
    const trimmedMessage = message.trim();
    const commit = trimmedMessage === "" ? {} : { commit_message: trimmedMessage };

    if (mode === "existing" && prompt !== undefined) {
      const labelSet = [...new Set([...checked, ...(typedLabel === "" ? [] : [typedLabel])])];
      const touchesProtected = labelSet.some((label) => protectedLabels.includes(label));
      createVersion.mutate(
        {
          params: { path: { project: projectRef(org, project), prompt: prompt.name } },
          body: { template: template.template, config, labels: labelSet, ...commit },
        },
        {
          onSuccess: (version) => {
            invalidatePrompt(queryClient, org, project, version.name);
            setSaved(version);
          },
          onError: (caught) => {
            failed(caught, touchesProtected);
          },
        },
      );
      return;
    }

    const labelSet = [
      ...new Set([
        ...(production && !productionLocked ? ["production"] : []),
        ...(typedLabel === "" ? [] : [typedLabel]),
      ]),
    ];
    const touchesProtected = labelSet.some((label) => protectedLabels.includes(label));
    createPrompt.mutate(
      {
        params: { path: { project: projectRef(org, project) } },
        body: {
          name: trimmedName,
          type: "chat",
          template: template.template,
          config,
          labels: labelSet,
          ...commit,
        },
      },
      {
        onSuccess: (version) => {
          invalidatePrompt(queryClient, org, project, version.name);
          setSaved(version);
        },
        onError: (caught) => {
          failed(caught, touchesProtected);
        },
      },
    );
  }

  const labelsError = errors.labels;

  return (
    <form onSubmit={submit} className="flex flex-col gap-4" noValidate>
      <RadioGroup
        aria-label="Save to"
        value={mode}
        onValueChange={(value) => {
          setMode(value as "existing" | "new");
          setErrors({});
          setError(undefined);
        }}
        className="flex gap-4"
      >
        <div className="flex items-center gap-2">
          <RadioGroupItem value="existing" id={`${idPrefix}-existing`} />
          <Label htmlFor={`${idPrefix}-existing`}>Existing prompt</Label>
        </div>
        <div className="flex items-center gap-2">
          <RadioGroupItem value="new" id={`${idPrefix}-new`} />
          <Label htmlFor={`${idPrefix}-new`}>New prompt</Label>
        </div>
      </RadioGroup>

      {mode === "existing" ? (
        <div className="flex flex-col gap-1.5">
          <Label htmlFor={`${idPrefix}-prompt`}>Prompt</Label>
          <Select
            value={promptName === "" ? NO_PROMPT : promptName}
            onValueChange={(value) => {
              setPromptName(value === NO_PROMPT ? "" : value);
              setChecked([]);
              setErrors({});
            }}
          >
            <SelectTrigger id={`${idPrefix}-prompt`} aria-invalid={errors.prompt ? true : undefined}>
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              <SelectItem value={NO_PROMPT} disabled>
                Choose a prompt
              </SelectItem>
              {promptRows.map((item) => (
                <SelectItem key={item.name} value={item.name} disabled={item.type !== "chat"}>
                  {item.type === "chat" ? item.name : `${item.name} (text)`}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
          {promptRows.some((item) => item.type !== "chat") ? (
            <p className="text-caption text-muted-foreground">{TEXT_PROMPT_TEXT}</p>
          ) : null}
          {errors.prompt ? <p className="text-caption text-destructive">{errors.prompt}</p> : null}
        </div>
      ) : (
        <TextField
          id={`${idPrefix}-name`}
          label="Name"
          value={newName}
          onChange={(value) => {
            setNewName(value);
          }}
          error={errors.name}
          description="Letters, digits, _, . and -. The name cannot be changed later."
          autoComplete="off"
          maxLength={200}
        />
      )}

      <TextField
        id={`${idPrefix}-message`}
        label="Commit message"
        value={message}
        onChange={(value) => {
          setMessage(value);
        }}
        error={errors.commit_message}
        maxLength={500}
      />

      <fieldset aria-label="Labels" className="m-0 flex min-w-0 flex-col gap-2 border-0 p-0">
        {mode === "existing"
          ? labels.map((label) => {
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
            })
          : (
              <DisabledReason reason={productionLocked ? PROTECTED_REASON : undefined}>
                <div className="flex items-center gap-2">
                  <Checkbox
                    id={`${idPrefix}-production`}
                    checked={production && !productionLocked}
                    disabled={productionLocked}
                    onCheckedChange={(next) => {
                      setProduction(next === true);
                    }}
                  />
                  <Label htmlFor={`${idPrefix}-production`}>Label as production</Label>
                </div>
              </DisabledReason>
            )}
        <div className="flex flex-col gap-1.5">
          <Label htmlFor={`${idPrefix}-new-label`}>New label</Label>
          <Input
            id={`${idPrefix}-new-label`}
            value={newLabel}
            autoComplete="off"
            spellCheck={false}
            className="h-7 text-body"
            aria-invalid={labelsError ? true : undefined}
            onChange={(event) => {
              setNewLabel(event.target.value);
            }}
          />
          <p className="text-caption text-muted-foreground">
            Optional. The label moves to the new version.
          </p>
          {labelsError ? <p className="text-caption text-destructive">{labelsError}</p> : null}
        </div>
      </fieldset>

      {reason ? <p className="text-caption text-destructive">{reason}</p> : null}
      <FormError message={error} />
      {saved ? (
        <p role="status" className="text-body">
          <span>{`Saved as ${saved.name} v${saved.version}.`}</span>{" "}
          <Link
            to="/$org/$project/prompts/$prompt"
            params={{ org, project, prompt: saved.name }}
            search={{ version: saved.version }}
            className="underline underline-offset-4"
          >
            Open the version
          </Link>
        </p>
      ) : null}
      <DialogFooter>
        <DisabledReason reason={reason}>
          <SubmitButton pending={pending} disabled={reason !== undefined}>
            Save version
          </SubmitButton>
        </DisabledReason>
      </DialogFooter>
    </form>
  );
}
