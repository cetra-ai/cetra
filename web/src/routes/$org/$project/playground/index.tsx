import { type QueryClient, useQueryClient } from "@tanstack/react-query";
import { createFileRoute } from "@tanstack/react-router";
import { useCallback, useEffect, useRef, useState } from "react";
import { $api } from "@/api/client";
import { fieldErrors, problemMessage, problemOf } from "@/api/problem";
import { spanDetailQuery, traceDetailQuery } from "@/api/queries";
import { promptQuery, promptVersionQuery } from "@/api/queries-prompts";
import { connectionsInit } from "@/api/queries-settings";
import type { Schema } from "@/api/types";
import { ConfirmDialog } from "@/components/confirm-dialog";
import { DisabledReason } from "@/components/disabled-reason";
import { FormError } from "@/components/form/form-error";
import { MessageEditor } from "@/components/playground/message-editor";
import { RecentRuns, RUNS_PATH } from "@/components/playground/recent-runs";
import {
  applyRunEvent,
  IDLE_RUN,
  RunColumn,
  type RunState,
} from "@/components/playground/run-column";
import { SaveDialog } from "@/components/playground/save-dialog";
import { SettingsPanel } from "@/components/playground/settings-panel";
import { useProject } from "@/components/shell/project-context";
import { Button } from "@/components/ui/button";
import { ResizableHandle, ResizablePanel, ResizablePanelGroup } from "@/components/ui/resizable";
import { Skeleton } from "@/components/ui/skeleton";
import {
  blockedMessages,
  type Column,
  type Draft,
  draftFromPromptVersion,
  draftFromSpan,
  type Entry,
  emptyDraft,
  loadDraft,
  type Message,
  runPlayground,
  saveDraft,
  toRunRequest,
} from "@/lib/playground";
import { VIEWER_REASON as PROMPT_EDIT_REASON } from "@/lib/prompts";
import { canManagePrompts, canRunPlayground } from "@/lib/roles";
import { hexId } from "@/lib/shortcuts";
import { uuidv7 } from "@/lib/uuid";

type Connection = Schema<"LlmConnection">;

/** The search params of the page. `prompt`/`version`, `trace`/`span` are loaded in a later step. */
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

const RUN_OPEN_FAILED = "Could not open the run.";
/** The run's metadata key that names where its draft came from (D-V-22). */
const RUN_SOURCE_KEY = "cetra.playground.source";
const PROMPT_SOURCE = /^prompt:(.+)@(\d+)$/;
const SPAN_SOURCE = /^span:([0-9a-f]{32}):([0-9a-f]{16})$/;

/**
 * A run's source from its metadata: `prompt:<name>@<version>` or `span:<trace>:<span>`. Any other
 * value is no source.
 */
function sourceOfRun(value: string | undefined): Draft["source"] {
  const prompt = value === undefined ? null : PROMPT_SOURCE.exec(value);
  if (prompt !== null) {
    const [, name = "", digits = ""] = prompt;
    const version = Number(digits);
    return name !== "" && Number.isSafeInteger(version) && version >= 1
      ? { prompt: { name, version } }
      : undefined;
  }
  const span = value === undefined ? null : SPAN_SOURCE.exec(value);
  if (span !== null) {
    const [, traceId = "", spanId = ""] = span;
    return { span: { trace_id: traceId, span_id: spanId } };
  }
  return undefined;
}

/**
 * A past run's span as a draft (D-V-38). The column's connection is the one whose adapter matches
 * the span's provider, else the first. The draft's source is the run's own source, not this span.
 */
function draftFromRun(span: Schema<"SpanDetail">, connections: readonly Connection[]): Draft {
  const adapter = span.provider === "gcp.gemini" ? "gemini" : span.provider;
  const connection = connections.find((item) => item.adapter === adapter) ?? connections[0];
  const { source: _spanSource, ...draft } = draftFromSpan(span, connection?.id);
  const source = sourceOfRun(span.metadata[RUN_SOURCE_KEY]);
  return source === undefined ? draft : { ...draft, source };
}

/** A draft with no text, variables, placeholders or settings: replacing it asks nothing. */
function isBlankDraft(draft: Draft): boolean {
  return (
    draft.entries.every(
      (entry) =>
        "parts" in entry &&
        entry.parts.every(
          (part) => part.type === "text" && "content" in part && part.content === "",
        ),
    ) &&
    Object.keys(draft.variables).length === 0 &&
    Object.keys(draft.placeholders).length === 0 &&
    draft.tools === "" &&
    draft.toolChoice === "" &&
    draft.responseFormat === "" &&
    draft.columns.length === 1 &&
    draft.columns.every(
      (column) => column.model === "" && column.temperature === "" && column.maxTokens === "",
    )
  );
}

/** A draft that replaces the page's, with whether its span was cut at ingest (the banner). */
type Loaded = { draft: Draft; cut: boolean };

/** Loads a past run: its trace's `llm` span, then that span's detail, as a draft (D-V-38). */
async function loadRun(
  queryClient: QueryClient,
  org: string,
  project: string,
  traceId: string,
  connections: readonly Connection[],
): Promise<Loaded> {
  const trace = await queryClient.fetchQuery(traceDetailQuery(org, project, traceId));
  const llm = trace.spans.find((row) => row.span.span_kind === "llm");
  if (llm === undefined) {
    throw new Error("The run has no LLM span.");
  }
  const span = await queryClient.fetchQuery(
    spanDetailQuery(org, project, traceId, llm.span.span_id),
  );
  return { draft: draftFromRun(span, connections), cut: span.truncated.length > 0 };
}

/**
 * A prompt version as a draft. A version that does not exist gives the latest one instead, and a
 * prompt that does not exist is an error.
 */
async function loadPrompt(
  queryClient: QueryClient,
  org: string,
  project: string,
  name: string,
  version: number | undefined,
  fallbackConnectionId: string | undefined,
): Promise<Loaded> {
  if (version !== undefined) {
    try {
      const found = await queryClient.fetchQuery(promptVersionQuery(org, project, name, version));
      return { draft: draftFromPromptVersion(found, fallbackConnectionId), cut: false };
    } catch (error) {
      if (problemOf(error)?.status !== 404) {
        throw error;
      }
    }
  }
  const prompt = await queryClient.fetchQuery(promptQuery(org, project, name));
  const latest = await queryClient.fetchQuery(
    promptVersionQuery(org, project, name, prompt.latest_version),
  );
  return { draft: draftFromPromptVersion(latest, fallbackConnectionId), cut: false };
}

/** A LLM span as a draft, with the first org connection (D-V-28). */
async function loadSpan(
  queryClient: QueryClient,
  org: string,
  project: string,
  traceId: string,
  spanId: string,
  fallbackConnectionId: string | undefined,
): Promise<Loaded> {
  const span = await queryClient.fetchQuery(spanDetailQuery(org, project, traceId, spanId));
  return {
    draft: draftFromSpan(span, fallbackConnectionId),
    cut: span.truncated.length > 0,
  };
}

/**
 * The source the URL names, as one key: `prompt:<name>@<version>` or `span:<trace>:<span>`. A prompt
 * wins over a span. `undefined` when the URL names no source.
 */
function urlSourceKey(
  prompt: string | undefined,
  version: number | undefined,
  trace: string | undefined,
  span: string | undefined,
): string | undefined {
  if (prompt !== undefined) {
    return `prompt:${prompt}@${version ?? ""}`;
  }
  if (trace !== undefined && span !== undefined) {
    return `span:${trace}:${span}`;
  }
  return undefined;
}

const SOURCE_OPEN_FAILED = "Could not open the source.";

/** The line that says where the draft came from, or `undefined` for a draft from scratch. */
function sourceText(source: Draft["source"]): string | undefined {
  if (source === undefined) {
    return undefined;
  }
  return "prompt" in source
    ? `From prompt ${source.prompt.name} v${source.prompt.version}`
    : "From a span";
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
  const { org, project, data } = useProject();
  const queryClient = useQueryClient();
  const navigate = Route.useNavigate();
  const search = Route.useSearch();
  const canManage = canManagePrompts(data.role);
  const [saveOpen, setSaveOpen] = useState(false);
  const [recentOpen, setRecentOpen] = useState(false);
  // The status text of a source being opened ("Opening the run…").
  const [opening, setOpening] = useState<string | undefined>();
  const [openError, setOpenError] = useState<string | undefined>();
  // A source loaded over a draft with content waits here for the user's yes to replace it.
  const [replacing, setReplacing] = useState<Loaded | undefined>();
  // Whether the draft's span was cut at ingest (the banner).
  const [cut, setCut] = useState(false);
  const loadedRun = useRef<string | undefined>(undefined);
  const loadedSource = useRef<string | undefined>(undefined);
  const draftRef = useRef(draft);
  // False after unmount: a source that finishes loading then does not navigate back to the page.
  const mounted = useRef(true);

  useEffect(() => {
    draftRef.current = draft;
  }, [draft]);

  useEffect(() => {
    mounted.current = true;
    return () => {
      mounted.current = false;
    };
  }, []);

  // Replaces the draft with a loaded source. The runs of the replaced columns are aborted first:
  // their results belong to columns that are gone.
  const replaceWith = useCallback((loaded: Loaded) => {
    for (const controller of controllers.current.values()) {
      controller.abort();
    }
    controllers.current.clear();
    setRuns({});
    setCut(loaded.cut);
    setDraft(loaded.draft);
  }, []);

  // A loaded source replaces an empty draft at once, and a draft with content after the user's yes.
  const applyLoaded = useCallback(
    (loaded: Loaded) => {
      if (isBlankDraft(draftRef.current)) {
        replaceWith(loaded);
      } else {
        setReplacing(loaded);
      }
    },
    [replaceWith],
  );

  // `?run=` opens a past run once. The guard keeps StrictMode and the removal of `run` from the URL
  // from loading it again; removing it keeps the draft's source (D-V-38).
  useEffect(() => {
    const runId = search.run;
    if (runId === undefined) {
      loadedRun.current = undefined;
      return;
    }
    if (loadedRun.current === runId) {
      return;
    }
    loadedRun.current = runId;
    setOpening("Opening the run…");
    setOpenError(undefined);
    void loadRun(queryClient, org, project, runId, connections)
      .then(applyLoaded, (error: unknown) => {
        setOpenError(problemMessage(error, RUN_OPEN_FAILED));
      })
      .finally(() => {
        setOpening(undefined);
        if (mounted.current) {
          void navigate({
            search: (previous) => ({ ...previous, run: undefined }),
            replace: true,
          });
        }
      });
  }, [search.run, queryClient, org, project, connections, navigate, applyLoaded]);

  // `?prompt=` (with `version`) or `?trace=&span=` opens that source once, like `?run=` (D-V-28).
  // The source keys are then removed from the URL; the draft keeps its source.
  const { prompt: promptName, version: promptVersion, trace: traceId, span: spanId } = search;
  const sourceKey = urlSourceKey(promptName, promptVersion, traceId, spanId);
  useEffect(() => {
    if (sourceKey === undefined) {
      loadedSource.current = undefined;
      return;
    }
    if (loadedSource.current === sourceKey) {
      return;
    }
    const fallbackId = connections[0]?.id;
    let load: Promise<Loaded>;
    let status: string;
    if (promptName !== undefined) {
      status = "Opening the prompt…";
      load = loadPrompt(queryClient, org, project, promptName, promptVersion, fallbackId);
    } else if (traceId !== undefined && spanId !== undefined) {
      status = "Opening the span…";
      load = loadSpan(queryClient, org, project, traceId, spanId, fallbackId);
    } else {
      return;
    }
    loadedSource.current = sourceKey;
    setOpening(status);
    setOpenError(undefined);
    void load
      .then(applyLoaded, (error: unknown) => {
        setOpenError(problemMessage(error, SOURCE_OPEN_FAILED));
      })
      .finally(() => {
        setOpening(undefined);
        if (mounted.current) {
          void navigate({
            search: (previous) => ({
              ...previous,
              prompt: undefined,
              version: undefined,
              trace: undefined,
              span: undefined,
            }),
            replace: true,
          });
        }
      });
  }, [
    sourceKey,
    promptName,
    promptVersion,
    traceId,
    spanId,
    queryClient,
    org,
    project,
    connections,
    navigate,
    applyLoaded,
  ]);

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
        // A finished run is a trace now: the recent runs list is stale.
        if (event.event === "done" || event.event === "error") {
          void queryClient.invalidateQueries({
            predicate: (query) => query.queryKey[1] === RUNS_PATH,
          });
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
          <DisabledReason reason={canManage ? undefined : PROMPT_EDIT_REASON}>
            <Button
              type="button"
              variant="outline"
              disabled={!canManage}
              onClick={() => {
                setSaveOpen(true);
              }}
            >
              Save as new version…
            </Button>
          </DisabledReason>
          <Button
            type="button"
            variant="outline"
            onClick={() => {
              setRecentOpen(true);
            }}
          >
            Recent runs
          </Button>
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
      {sourceText(draft.source) === undefined ? null : (
        <p className="text-caption text-muted-foreground">{sourceText(draft.source)}</p>
      )}
      {cut ? (
        <p className="border-b bg-muted px-3 py-1 text-caption">
          This span was cut at ingest; the playground shows what was stored.
        </p>
      ) : null}
      {opening === undefined ? null : (
        <p role="status" className="text-caption text-muted-foreground">
          {opening}
        </p>
      )}
      <FormError message={openError} />
      <ResizablePanelGroup
        id="playground-split"
        orientation="horizontal"
        className="min-h-0 flex-1"
      >
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
      <SaveDialog
        open={saveOpen}
        onOpenChange={setSaveOpen}
        draft={draft}
        connections={connections}
      />
      <RecentRuns open={recentOpen} onOpenChange={setRecentOpen} />
      <ConfirmDialog
        open={replacing !== undefined}
        onOpenChange={(next) => {
          if (!next) {
            setReplacing(undefined);
          }
        }}
        title="Replace the current draft?"
        description="The messages and settings on this page are replaced by the ones being opened."
        confirmLabel="Replace draft"
        pending={false}
        onConfirm={() => {
          if (replacing !== undefined) {
            replaceWith(replacing);
          }
          setReplacing(undefined);
        }}
      />
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
