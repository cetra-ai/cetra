import { createFileRoute } from "@tanstack/react-router";
import { useCallback, useEffect, useRef, useState } from "react";
import { $api } from "@/api/client";
import { fieldErrors, problemMessage } from "@/api/problem";
import { connectionsInit } from "@/api/queries-settings";
import type { Schema } from "@/api/types";
import { DisabledReason } from "@/components/disabled-reason";
import { FormError } from "@/components/form/form-error";
import { MessageEditor } from "@/components/playground/message-editor";
import {
  IDLE_RUN,
  RunColumn,
  type RunState,
  applyRunEvent,
} from "@/components/playground/run-column";
import { SettingsPanel } from "@/components/playground/settings-panel";
import { useProject } from "@/components/shell/project-context";
import { Button } from "@/components/ui/button";
import { ResizableHandle, ResizablePanel, ResizablePanelGroup } from "@/components/ui/resizable";
import { Skeleton } from "@/components/ui/skeleton";
import {
  type Column,
  type Draft,
  type Entry,
  type Message,
  blockedMessages,
  emptyDraft,
  loadDraft,
  runPlayground,
  saveDraft,
  toRunRequest,
} from "@/lib/playground";
import { canRunPlayground } from "@/lib/roles";
import { hexId } from "@/lib/shortcuts";
import { uuidv7 } from "@/lib/uuid";

type Connection = Schema<"LlmConnection">;

/** The search params of the page. The sources they name are loaded in a later step. */
type PlaygroundSearch = {
  prompt?: string;
  version?: number;
  trace?: string;
  span?: string;
  run?: string;
};

const COLUMN_LIMIT = 4;
const SAVE_DELAY_MS = 500;
const SEARCH_TEXT_MAX = 200;
const VIEWER_REASON = "Only members can run the playground";
const NO_CONNECTION_REASON =
  "Add an LLM connection in organization settings to run the playground.";
/** Field errors the page shows above the columns: the whole request, or its message list. */
const FORM_FIELDS: ReadonlySet<string> = new Set(["messages", "request"]);
const SETTINGS_FIELD =
  /^(variables|placeholders|tools|tool_choice|toolChoice|responseFormat|response_format)(\.|$)/;

export const Route = createFileRoute("/$org/$project/playground/")({
  validateSearch: (search) => parsePlaygroundSearch(search),
  component: PlaygroundPage,
});

function parsePlaygroundSearch(search: Record<string, unknown>): PlaygroundSearch {
  return {
    prompt: searchText(search.prompt),
    version: searchVersion(search.version),
    trace: typeof search.trace === "string" ? hexId(search.trace, 32) : undefined,
    span: typeof search.span === "string" ? hexId(search.span, 16) : undefined,
    run: typeof search.run === "string" ? hexId(search.run, 32) : undefined,
  };
}

function searchText(value: unknown): string | undefined {
  return typeof value === "string" && value !== "" && value.length <= SEARCH_TEXT_MAX
    ? value
    : undefined;
}

function searchVersion(value: unknown): number | undefined {
  const parsed =
    typeof value === "number"
      ? value
      : typeof value === "string" && /^\d+$/.test(value)
        ? Number(value)
        : undefined;
  return parsed !== undefined && Number.isSafeInteger(parsed) && parsed >= 1 ? parsed : undefined;
}

function PlaygroundPage() {
  const { org, ref, data } = useProject();
  const connections = $api.useQuery(
    "get",
    "/api/v1/orgs/{org}/llm-connections",
    connectionsInit(org),
  );
  if (connections.isError) {
    return <FormError message="Could not load the LLM connections." />;
  }
  if (!connections.isSuccess) {
    return <Skeleton className="h-64 w-full" />;
  }
  return (
    <PlaygroundWorkspace
      key={ref}
      projectKey={ref}
      canRun={canRunPlayground(data.role)}
      connections={connections.data?.data ?? []}
    />
  );
}

/** The draft stored for this project, with connections that no longer exist reset to the first. */
function initialDraft(key: string, connections: readonly Connection[]): Draft {
  const firstId = connections[0]?.id;
  const stored = loadDraft(key);
  if (stored === undefined) {
    return emptyDraft(firstId);
  }
  return {
    ...stored,
    columns: stored.columns.map((column) => ({
      ...column,
      connectionId: connections.some((item) => item.id === column.connectionId)
        ? column.connectionId
        : firstId,
    })),
  };
}

function messageIndexOf(field: string): number | undefined {
  const match = /^messages\[(\d+)\]/.exec(field);
  return match ? Number(match[1]) : undefined;
}

function blockedText(item: { index: number; kinds: string[] }): string {
  return `Message ${item.index} has ${item.kinds.join(", ")} parts, which the playground cannot send. Remove them to run.`;
}

/** The first error of each field across the sources, for the fields `keep` accepts. */
function mergeFirst(
  sources: Record<string, string>[],
  keep: (field: string) => boolean,
): Record<string, string> {
  const merged: Record<string, string> = {};
  for (const source of sources) {
    for (const [field, message] of Object.entries(source)) {
      if (keep(field) && merged[field] === undefined) {
        merged[field] = message;
      }
    }
  }
  return merged;
}

function PlaygroundWorkspace({
  projectKey,
  canRun,
  connections,
}: {
  /** `org/project`: the run path and the draft's storage key. */
  projectKey: string;
  canRun: boolean;
  connections: readonly Connection[];
}) {
  const [draft, setDraft] = useState<Draft>(() => initialDraft(projectKey, connections));
  const [runs, setRuns] = useState<Record<string, RunState>>({});
  const controllers = useRef(new Map<string, AbortController>());
  const pendingSave = useRef<Draft | undefined>(undefined);

  // The draft is saved 500 ms after the last change, and at unmount and page hide.
  const flush = useCallback(() => {
    const next = pendingSave.current;
    if (next !== undefined) {
      pendingSave.current = undefined;
      saveDraft(projectKey, next);
    }
  }, [projectKey]);

  useEffect(() => {
    pendingSave.current = draft;
    const timer = window.setTimeout(flush, SAVE_DELAY_MS);
    return () => {
      window.clearTimeout(timer);
    };
  }, [draft, flush]);

  useEffect(() => {
    window.addEventListener("pagehide", flush);
    return () => {
      window.removeEventListener("pagehide", flush);
      flush();
    };
  }, [flush]);

  // Leaving the page cancels every run.
  useEffect(() => {
    const active = controllers.current;
    return () => {
      for (const controller of active.values()) {
        controller.abort();
      }
    };
  }, []);

  const blocked = blockedMessages(draft);
  const checks = draft.columns.map((column) => toRunRequest(draft, column, connections));
  const running = Object.values(runs).some((run) => run.status === "running");
  const runReason = runBlockReason(canRun, blocked, connections.length);

  // Problems: the refused parts and server errors under their messages; the settings' under them;
  // the whole request above the columns; the rest under each column.
  const editorErrors: Record<number, string> = {};
  for (const item of blocked) {
    editorErrors[item.index - 1] = blockedText(item);
  }
  for (const run of Object.values(runs)) {
    for (const [field, message] of Object.entries(run.errors)) {
      const index = messageIndexOf(field);
      if (index !== undefined && editorErrors[index] === undefined) {
        editorErrors[index] = message;
      }
    }
  }
  const checkErrors = checks.map((check) => (check.ok ? {} : check.errors));
  const runErrors = Object.values(runs).map((run) => run.errors);
  const settingsErrors = mergeFirst([...checkErrors, ...runErrors], (field) =>
    SETTINGS_FIELD.test(field),
  );
  const formErrors = mergeFirst(runErrors, (field) => FORM_FIELDS.has(field));
  const formError = formErrors.messages ?? formErrors.request;

  function updateDraft(patch: Partial<Draft>) {
    setDraft((previous) => ({ ...previous, ...patch }));
  }

  function updateColumn(id: string, patch: Partial<Column>) {
    setDraft((previous) => ({
      ...previous,
      columns: previous.columns.map((column) =>
        column.id === id ? { ...column, ...patch } : column,
      ),
    }));
  }

  function addAnswer(message: Message) {
    setDraft((previous) => ({ ...previous, entries: [...previous.entries, message] }));
  }

  function addColumn() {
    const column: Column = {
      id: uuidv7(),
      connectionId: draft.columns[draft.columns.length - 1]?.connectionId ?? connections[0]?.id,
      model: "",
      temperature: "",
      maxTokens: "",
    };
    setDraft((previous) =>
      previous.columns.length >= COLUMN_LIMIT
        ? previous
        : { ...previous, columns: [...previous.columns, column] },
    );
  }

  function removeColumn(id: string) {
    controllers.current.get(id)?.abort();
    controllers.current.delete(id);
    setDraft((previous) => ({
      ...previous,
      columns: previous.columns.filter((column) => column.id !== id),
    }));
    setRuns((previous) =>
      Object.fromEntries(Object.entries(previous).filter(([key]) => key !== id)),
    );
  }

  function setRun(id: string, update: (run: RunState) => RunState) {
    setRuns((previous) => ({ ...previous, [id]: update(previous[id] ?? IDLE_RUN) }));
  }

  function startRun(column: Column) {
    const checked = toRunRequest(draft, column, connections);
    if (!checked.ok) {
      // The problems are already shown under the controls.
      return;
    }
    controllers.current.get(column.id)?.abort();
    const controller = new AbortController();
    controllers.current.set(column.id, controller);
    const isCurrent = () => controllers.current.get(column.id) === controller;
    setRun(column.id, () => ({ ...IDLE_RUN, status: "running" }));
    runPlayground(projectKey, checked.request, {
      signal: controller.signal,
      onEvent: (event) => {
        if (isCurrent()) {
          setRun(column.id, (run) => applyRunEvent(run, event));
        }
      },
    }).then(
      () => {
        if (!isCurrent()) {
          return;
        }
        controllers.current.delete(column.id);
        setRun(column.id, (run) =>
          run.status !== "running"
            ? run
            : { ...run, status: controller.signal.aborted ? "stopped" : "incomplete" },
        );
      },
      (error: unknown) => {
        if (!isCurrent()) {
          return;
        }
        controllers.current.delete(column.id);
        setRun(column.id, (run) => ({
          ...run,
          status: "failed",
          problem: problemMessage(error, "Could not start the run."),
          errors: fieldErrors(error),
        }));
      },
    );
  }

  function runAll() {
    for (const column of draft.columns) {
      startRun(column);
    }
  }

  function stopAll() {
    for (const controller of controllers.current.values()) {
      controller.abort();
    }
  }

  return (
    <div className="flex h-full min-h-0 flex-col gap-3">
      <div className="flex flex-wrap items-center gap-2">
        <h1 className="text-heading font-medium">Playground</h1>
        <div className="ml-auto flex flex-wrap items-center gap-2">
          {running ? (
            <Button type="button" variant="outline" onClick={stopAll}>
              Stop all
            </Button>
          ) : (
            <DisabledReason reason={runReason}>
              <Button type="button" disabled={runReason !== undefined} onClick={runAll}>
                Run all
              </Button>
            </DisabledReason>
          )}
          <Button
            type="button"
            variant="outline"
            disabled={draft.columns.length >= COLUMN_LIMIT}
            onClick={addColumn}
          >
            Add model
          </Button>
        </div>
      </div>
      <FormError message={formError} />
      <ResizablePanelGroup id="playground-split" orientation="horizontal" className="min-h-0 flex-1">
        <ResizablePanel id="playground-editor" defaultSize="40%" minSize="25%" className="min-h-0">
          <div className="flex h-full min-h-0 flex-col gap-6 overflow-y-auto pr-3">
            <MessageEditor
              entries={draft.entries}
              errors={editorErrors}
              onChange={(entries: Entry[]) => {
                updateDraft({ entries });
              }}
            />
            <SettingsPanel draft={draft} errors={settingsErrors} onChange={updateDraft} />
          </div>
        </ResizablePanel>
        <ResizableHandle withHandle />
        <ResizablePanel id="playground-columns" minSize="30%" className="min-h-0">
          <div className="flex h-full min-h-0 gap-3 overflow-x-auto pb-2">
            {draft.columns.map((column, index) => (
              <RunColumn
                key={column.id}
                number={index + 1}
                column={column}
                run={runs[column.id] ?? IDLE_RUN}
                connections={connections}
                errors={mergeFirst(
                  [checkErrors[index] ?? {}, runs[column.id]?.errors ?? {}],
                  (field) =>
                    !SETTINGS_FIELD.test(field) &&
                    !FORM_FIELDS.has(field) &&
                    messageIndexOf(field) === undefined,
                )}
                runReason={runReason}
                canRemove={draft.columns.length > 1}
                onChange={(patch) => {
                  updateColumn(column.id, patch);
                }}
                onRun={() => {
                  startRun(column);
                }}
                onStop={() => {
                  controllers.current.get(column.id)?.abort();
                }}
                onRemove={() => {
                  removeColumn(column.id);
                }}
                onAddAnswer={addAnswer}
              />
            ))}
          </div>
        </ResizablePanel>
      </ResizablePanelGroup>
    </div>
  );
}

/** Why every Run is disabled, or `undefined`: the viewer, a blocked message, or no connection. */
function runBlockReason(
  canRun: boolean,
  blocked: { index: number; kinds: string[] }[],
  connectionCount: number,
): string | undefined {
  if (!canRun) {
    return VIEWER_REASON;
  }
  const first = blocked[0];
  if (first !== undefined) {
    return blockedText(first);
  }
  return connectionCount === 0 ? NO_CONNECTION_REASON : undefined;
}
