import { useQueryClient } from "@tanstack/react-query";
import { useNavigate } from "@tanstack/react-router";
import { useState } from "react";
import { $api } from "@/api/client";
import { fieldErrors, problemMessage, problemOf } from "@/api/problem";
import { projectRef } from "@/api/project-ref";
import { invalidatePrompt } from "@/api/queries-prompts";
import type { Schema } from "@/api/types";
import { CreatedBy } from "@/components/created-by";
import { DisabledReason } from "@/components/disabled-reason";
import { SaveVersionDialog } from "@/components/prompts/save-version-dialog";
import { TemplateEditor } from "@/components/prompts/template-editor";
import { UnsavedChangesGuard } from "@/components/settings/unsaved-changes";
import { RelativeTime } from "@/components/relative-time";
import { useProject } from "@/components/shell/project-context";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Label } from "@/components/ui/label";
import { Textarea } from "@/components/ui/textarea";
import { parseJsonField, prettyJson } from "@/lib/datasets";
import {
  asChat,
  type ChatEntry,
  fieldPositions,
  LABEL_FORBIDDEN_TEXT,
  type Position,
  type PromptKind,
  templateText,
  VIEWER_REASON,
  withPromptVersion,
} from "@/lib/prompts";

type PromptVersion = Schema<"PromptVersion">;
/** The editable parts of a version: the text or the chat messages, and the config as JSON text. */
type Draft = { text: string; chat: ChatEntry[]; config: string };

const FALLBACK = "Could not save the prompt.";
/** Field errors the editor shows under its own parts. */
const EDITOR_FIELD = /^(template|config)(\W|$)/;
/** Field errors the save dialog shows. */
const SAVE_FIELD = /^(commit_message|labels)(\W|$)/;

function draftOf(version: PromptVersion): Draft {
  return {
    text: version.type === "text" && typeof version.template === "string" ? version.template : "",
    chat: version.type === "chat" ? asChat(version.template) : [],
    config: prettyJson(version.config),
  };
}

/** What an edit changes, as text: two drafts with the same key have the same content. */
function draftKey(draft: Draft, type: PromptKind): string {
  return JSON.stringify([type === "text" ? draft.text : draft.chat, draft.config]);
}

/**
 * The template tab: the shown version read-only, and Edit, which changes the template and config
 * and saves them as a new version. The caller keys it by version, so an edit never carries over.
 */
export function TemplateTab({
  prompt,
  version,
  canManage,
  canProtect,
  protectedLabels,
}: {
  prompt: Schema<"Prompt">;
  version: PromptVersion;
  canManage: boolean;
  canProtect: boolean;
  protectedLabels: string[];
}) {
  const { org, project } = useProject();
  const queryClient = useQueryClient();
  const navigate = useNavigate();
  const create = $api.useMutation("post", "/api/v1/projects/{project}/prompts/{prompt}/versions");
  const [editing, setEditing] = useState(false);
  const [draft, setDraft] = useState<Draft>(() => draftOf(version));
  const [baseline, setBaseline] = useState("");
  const [errors, setErrors] = useState<Record<string, string>>({});
  const [positions, setPositions] = useState<Record<string, Position>>({});
  const [saveOpen, setSaveOpen] = useState(false);
  const [saveErrors, setSaveErrors] = useState<Record<string, string>>({});
  const [saveError, setSaveError] = useState<string | undefined>();

  if (!editing) {
    return (
      <div className="flex flex-col gap-4">
        <div className="flex justify-end">
          <DisabledReason reason={canManage ? undefined : VIEWER_REASON}>
            <Button
              type="button"
              variant="outline"
              size="sm"
              disabled={!canManage}
              onClick={() => {
                const next = draftOf(version);
                setDraft(next);
                setBaseline(draftKey(next, version.type));
                setErrors({});
                setPositions({});
                setEditing(true);
              }}
            >
              Edit
            </Button>
          </DisabledReason>
        </div>
        <TemplateView version={version} />
      </div>
    );
  }

  const changed = draftKey(draft, version.type) !== baseline;

  function openSave() {
    const config = parseJsonField(draft.config, { required: false, object: true });
    if (!config.ok) {
      setErrors({ config: config.message });
      setPositions({});
      return;
    }
    setErrors({});
    setPositions({});
    setSaveErrors({});
    setSaveError(undefined);
    setSaveOpen(true);
  }

  function save(input: { commitMessage: string | undefined; labels: string[] }) {
    const config = parseJsonField(draft.config, { required: false, object: true });
    if (!config.ok) {
      return;
    }
    const body: Schema<"CreateVersionRequest"> = {
      template: version.type === "text" ? draft.text : draft.chat,
      labels: input.labels,
    };
    if (config.value !== undefined) {
      body.config = config.value as Schema<"CreateVersionRequest">["config"];
    }
    if (input.commitMessage !== undefined) {
      body.commit_message = input.commitMessage;
    }
    setSaveErrors({});
    setSaveError(undefined);
    create.mutate(
      { params: { path: { project: projectRef(org, project), prompt: prompt.name } }, body },
      {
        onSuccess: () => {
          invalidatePrompt(queryClient, org, project, prompt.name);
          // The page then shows the latest version, which is the one just saved.
          setEditing(false);
          setSaveOpen(false);
          void navigate({
            to: "/$org/$project/prompts/$prompt",
            params: { org, project, prompt: prompt.name },
            search: (previous) => withPromptVersion(previous, undefined),
            state: true,
          });
        },
        onError: failed,
      },
    );
  }

  function failed(caught: unknown) {
    const fields = fieldErrors(caught);
    const editorErrors: Record<string, string> = {};
    const dialogErrors: Record<string, string> = {};
    for (const [field, message] of Object.entries(fields)) {
      if (EDITOR_FIELD.test(field)) {
        editorErrors[field] = message;
      } else if (SAVE_FIELD.test(field)) {
        dialogErrors[field] = message;
      }
    }
    if (Object.keys(editorErrors).length > 0) {
      // The template or the config is wrong: the dialog closes so the editor shows the problem.
      setErrors(editorErrors);
      setPositions(fieldPositions(caught));
      setSaveOpen(false);
      return;
    }
    setSaveErrors(dialogErrors);
    if (problemOf(caught)?.status === 403) {
      setSaveError(LABEL_FORBIDDEN_TEXT);
    } else if (Object.keys(dialogErrors).length === 0) {
      setSaveError(problemMessage(caught, FALLBACK));
    }
  }

  return (
    <div className="flex flex-col gap-4">
      <UnsavedChangesGuard dirty={changed} ignoreSearchChanges />
      {version.type === "text" ? (
        <TemplateEditor
          type="text"
          value={draft.text}
          errors={errors}
          positions={positions}
          onChange={(value) => {
            setDraft((current) => ({ ...current, text: value }));
          }}
        />
      ) : (
        <TemplateEditor
          type="chat"
          value={draft.chat}
          errors={errors}
          positions={positions}
          onChange={(value) => {
            setDraft((current) => ({ ...current, chat: value }));
          }}
        />
      )}
      <div className="flex flex-col gap-1.5">
        <Label htmlFor="prompt-version-config">Config</Label>
        <Textarea
          id="prompt-version-config"
          className="font-mono text-caption"
          spellCheck={false}
          rows={4}
          value={draft.config}
          aria-invalid={errors.config ? true : undefined}
          onChange={(event) => {
            const value = event.target.value;
            setDraft((current) => ({ ...current, config: value }));
          }}
        />
        {errors.config ? <p className="text-caption text-destructive">{errors.config}</p> : null}
      </div>
      <div className="flex flex-wrap gap-2">
        <Button
          type="button"
          variant="outline"
          onClick={() => {
            setEditing(false);
            setErrors({});
            setPositions({});
          }}
        >
          Cancel
        </Button>
        <Button type="button" onClick={openSave}>
          Save as new version…
        </Button>
      </div>
      <SaveVersionDialog
        open={saveOpen}
        onOpenChange={setSaveOpen}
        labels={prompt.labels.map((item) => item.label)}
        canProtect={canProtect}
        protectedLabels={protectedLabels}
        pending={create.isPending}
        errors={saveErrors}
        error={saveError}
        onSave={save}
      />
    </div>
  );
}

/** The shown version's template, variables, placeholders, config and author. */
function TemplateView({ version }: { version: PromptVersion }) {
  const entries = asChat(version.template);
  return (
    <div className="flex flex-col gap-4">
      {version.type === "text" ? (
        <pre
          aria-label="Template"
          className="rounded-md border bg-muted/30 p-3 font-mono text-body whitespace-pre-wrap break-words"
        >
          {templateText(version.template, "text")}
        </pre>
      ) : (
        <ol aria-label="Messages" className="flex flex-col gap-2">
          {entries.map((entry, index) =>
            "type" in entry ? (
              <li
                // The messages are rendered once, in order, so the position is a stable key.
                // biome-ignore lint/suspicious/noArrayIndexKey: the messages are rendered once, in order
                key={index}
                className="text-body text-muted-foreground"
              >
                {`Placeholder: ${entry.name}`}
              </li>
            ) : (
              <li
                // biome-ignore lint/suspicious/noArrayIndexKey: the messages are rendered once, in order
                key={index}
                className="flex flex-col gap-1 rounded-md border p-3"
              >
                <span className="text-caption text-muted-foreground">{entry.role}</span>
                <pre className="font-mono text-body whitespace-pre-wrap break-words">
                  {entry.content}
                </pre>
              </li>
            ),
          )}
        </ol>
      )}
      <dl className="grid gap-x-4 gap-y-2 text-body sm:grid-cols-[auto_1fr]">
        <dt className="text-muted-foreground">Variables</dt>
        <dd className="flex flex-wrap gap-1.5">
          {version.variables.length === 0 ? <span>None</span> : null}
          {version.variables.map((name) => (
            <Badge key={name} variant="outline">
              {name}
            </Badge>
          ))}
        </dd>
        <dt className="text-muted-foreground">Placeholders</dt>
        <dd className="flex flex-wrap gap-1.5">
          {version.placeholders.length === 0 ? <span>None</span> : null}
          {version.placeholders.map((name) => (
            <Badge key={name} variant="outline">
              {name}
            </Badge>
          ))}
        </dd>
        <dt className="text-muted-foreground">Config</dt>
        <dd>
          <pre aria-label="Config" className="font-mono text-caption whitespace-pre-wrap">
            {prettyJson(version.config)}
          </pre>
        </dd>
      </dl>
      <div className="flex flex-col gap-1 text-caption text-muted-foreground">
        {version.commit_message ? <p>{`Commit message: ${version.commit_message}`}</p> : null}
        <p>
          Saved <RelativeTime value={version.created_at} /> by{" "}
          <CreatedBy createdBy={version.created_by} />
        </p>
      </div>
    </div>
  );
}
