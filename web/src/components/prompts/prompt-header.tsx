import { useQueryClient } from "@tanstack/react-query";
import { Link, useNavigate } from "@tanstack/react-router";
import { ChevronLeft } from "lucide-react";
import { useState } from "react";
import { $api } from "@/api/client";
import { problemMessage, problemOf } from "@/api/problem";
import { projectRef } from "@/api/project-ref";
import { invalidatePrompt, promptsKey } from "@/api/queries-prompts";
import type { Schema } from "@/api/types";
import { BACK_LINK_CLASS, BackLink } from "@/components/back-link";
import { ConfirmDialog } from "@/components/confirm-dialog";
import { DisabledReason } from "@/components/disabled-reason";
import { LabelChip } from "@/components/prompts/label-chip";
import { useProject } from "@/components/shell/project-context";
import { TopBarCrumb } from "@/components/shell/top-bar-slot";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { TypeToConfirmDialog } from "@/components/type-to-confirm-dialog";
import { LABEL_FORBIDDEN_TEXT, PROTECTED_REASON, VIEWER_REASON } from "@/lib/prompts";

/** The version picker's value for "the latest version". */
export const LATEST = "latest";
/** The picker's last option, which loads the next page of versions instead of choosing one. */
export const LOAD_MORE = "load-more";

const FALLBACK_DELETE = "Could not delete the prompt.";
const FALLBACK_REMOVE = "Could not remove the label.";

/** The message of a failed write: the fixed text for `403`, else the problem's own message. */
function writeMessage(error: unknown, forbidden: string, fallback: string): string | undefined {
  if (error === null || error === undefined) {
    return undefined;
  }
  return problemOf(error)?.status === 403 ? forbidden : problemMessage(error, fallback);
}

/**
 * The prompt page's header: back link, name, type, the version picker, the labels of the version
 * shown (removable), the description and the delete action.
 */
export function PromptHeader({
  prompt,
  shown,
  versions,
  hasNextPage,
  labels,
  canManage,
  canProtect,
  protectedLabels,
  onChooseVersion,
}: {
  prompt: Schema<"Prompt">;
  /** The version shown. */
  shown: number;
  /** The loaded versions, newest first. */
  versions: Schema<"PromptVersion">[];
  hasNextPage: boolean;
  /** The labels of the version shown, `latest` included. */
  labels: string[];
  canManage: boolean;
  canProtect: boolean;
  protectedLabels: string[];
  /** Called with `LATEST`, `LOAD_MORE` or a version number as text. */
  onChooseVersion: (value: string) => void;
}) {
  const { org, project } = useProject();
  const queryClient = useQueryClient();
  const navigate = useNavigate();
  const [removing, setRemoving] = useState<string | undefined>();
  const [deleting, setDeleting] = useState(false);
  const removeLabel = $api.useMutation(
    "delete",
    "/api/v1/projects/{project}/prompts/{prompt}/labels/{label}",
  );
  const deletePrompt = $api.useMutation("delete", "/api/v1/projects/{project}/prompts/{prompt}");

  const latest = prompt.latest_version;
  const pickerValue = shown === latest ? LATEST : String(shown);
  // A version shown from the URL that is not among the loaded pages still shows in the picker.
  const unloaded =
    shown !== latest && !versions.some((item) => item.version === shown) ? shown : undefined;

  function closeRemove() {
    setRemoving(undefined);
    removeLabel.reset();
  }

  function confirmRemove() {
    if (removing === undefined) {
      return;
    }
    removeLabel.mutate(
      {
        params: {
          path: { project: projectRef(org, project), prompt: prompt.name, label: removing },
        },
      },
      {
        onSuccess: () => {
          invalidatePrompt(queryClient, org, project, prompt.name);
          closeRemove();
        },
      },
    );
  }

  function confirmDelete() {
    deletePrompt.mutate(
      { params: { path: { project: projectRef(org, project), prompt: prompt.name } } },
      {
        onSuccess: () => {
          void queryClient.invalidateQueries({ queryKey: promptsKey(org, project) });
          void navigate({ to: "/$org/$project/prompts", params: { org, project } });
        },
      },
    );
  }

  return (
    <header className="flex min-h-9 flex-wrap items-center gap-2 border-b px-3 py-1.5">
      <nav aria-label="Prompt" className="flex min-w-0 items-center gap-1.5">
        <BackLink
          fallback={
            <Link
              to="/$org/$project/prompts"
              params={{ org, project }}
              aria-label="Back to prompts"
              className={BACK_LINK_CLASS}
            >
              <ChevronLeft aria-hidden="true" className="size-3.5" />
              Prompts
            </Link>
          }
        />
        <span aria-hidden="true" className="text-muted-foreground/60">
          /
        </span>
        <h2 className="truncate text-ui font-medium">{prompt.name}</h2>
        <TopBarCrumb>
          <span className="max-w-64 truncate text-body font-medium">{prompt.name}</span>
        </TopBarCrumb>
      </nav>
      <Badge variant="outline">{prompt.type === "chat" ? "Chat" : "Text"}</Badge>
      <div className="ml-auto flex flex-wrap items-center gap-1.5">
        <Select value={pickerValue} onValueChange={onChooseVersion}>
          <SelectTrigger aria-label="Version" size="sm" className="w-fit text-body">
            <SelectValue />
          </SelectTrigger>
          <SelectContent>
            <SelectItem value={LATEST}>{`Latest (v${latest})`}</SelectItem>
            {versions
              .filter((item) => item.version !== latest)
              .map((item) => (
                <SelectItem key={item.version} value={String(item.version)}>
                  {`v${item.version}`}
                </SelectItem>
              ))}
            {unloaded !== undefined ? (
              <SelectItem value={String(unloaded)}>{`v${unloaded}`}</SelectItem>
            ) : null}
            {hasNextPage ? <SelectItem value={LOAD_MORE}>Load more versions…</SelectItem> : null}
          </SelectContent>
        </Select>
        <DisabledReason reason={canManage ? undefined : VIEWER_REASON}>
          <Button
            type="button"
            variant="outline"
            size="sm"
            disabled={!canManage}
            onClick={() => {
              setDeleting(true);
            }}
          >
            Delete prompt
          </Button>
        </DisabledReason>
      </div>
      {prompt.description ? (
        <p className="w-full text-body text-muted-foreground">{prompt.description}</p>
      ) : null}
      <div role="group" aria-label={`Labels of v${shown}`} className="flex flex-wrap gap-1.5">
        {labels.map((label) =>
          label === "latest" ? (
            <Badge key={label} variant="outline">
              latest
            </Badge>
          ) : (
            <LabelChip
              key={label}
              label={label}
              protectedLabel={protectedLabels.includes(label)}
              canTouch={canManage && (canProtect || !protectedLabels.includes(label))}
              onRemove={() => {
                setRemoving(label);
              }}
            />
          ),
        )}
      </div>

      <ConfirmDialog
        open={removing !== undefined}
        onOpenChange={(open) => {
          if (!open) {
            closeRemove();
          }
        }}
        title={`Remove label ${removing ?? ""}?`}
        description={`The label stops pointing at v${shown}. The version itself is kept.`}
        confirmLabel="Remove label"
        destructive
        pending={removeLabel.isPending}
        error={writeMessage(removeLabel.error, LABEL_FORBIDDEN_TEXT, FALLBACK_REMOVE)}
        onConfirm={confirmRemove}
      />
      <TypeToConfirmDialog
        open={deleting}
        onOpenChange={(open) => {
          if (!open) {
            setDeleting(false);
            deletePrompt.reset();
          }
        }}
        title="Delete prompt?"
        description="The prompt, all its versions, its labels and their history are deleted. This cannot be undone."
        expected={prompt.name}
        inputLabel={`Type the prompt name "${prompt.name}" to confirm`}
        confirmLabel="Delete prompt"
        pending={deletePrompt.isPending}
        error={writeMessage(
          deletePrompt.error,
          // A prompt with a protected label is deleted by project admins only (D-V-10).
          PROTECTED_REASON,
          FALLBACK_DELETE,
        )}
        onConfirm={confirmDelete}
      />
    </header>
  );
}
