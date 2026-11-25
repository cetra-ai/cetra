import { Link } from "@tanstack/react-router";
import {
  ArrowDownToLine,
  ArrowUpFromLine,
  Coins,
  Flag,
  Play,
  SlidersHorizontal,
  Square,
  Timer,
  Trash2,
  Zap,
} from "lucide-react";
import { memo } from "react";
import type { Schema } from "@/api/types";
import { DisabledReason } from "@/components/disabled-reason";
import { isPlaceholder } from "@/components/playground/message-editor";
import { useProject } from "@/components/shell/project-context";
import { MetricChip } from "@/components/trace/metric-chip";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Popover, PopoverContent, PopoverTrigger } from "@/components/ui/popover";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { prettyJson } from "@/lib/datasets";
import { formatCost, formatDuration, formatTokens } from "@/lib/format";
import type { Column, Entry, Message, RunEvent } from "@/lib/playground";

type DoneData = Extract<RunEvent, { event: "done" }>["data"];
type StreamError = Extract<RunEvent, { event: "error" }>["data"];
type ToolCall = Extract<RunEvent, { event: "tool_call" }>["data"];

/** What one column shows: its stream so far, or why it did not start. */
export type RunState = {
  status: "idle" | "running" | "done" | "failed" | "stopped" | "incomplete";
  traceId?: string;
  text: string;
  toolCalls: ToolCall[];
  done?: DoneData;
  error?: StreamError;
  /** Why the run failed before or during the stream. */
  problem?: string;
  /** Server field errors of the last run. */
  errors: Record<string, string>;
};

export const IDLE_RUN: RunState = { status: "idle", text: "", toolCalls: [], errors: {} };

/** The run after one stream event. */
export function applyRunEvent(run: RunState, event: RunEvent): RunState {
  switch (event.event) {
    case "start":
      return { ...run, traceId: event.data.trace_id };
    case "text":
      return { ...run, text: run.text + event.data.delta };
    case "tool_call":
      return { ...run, toolCalls: [...run.toolCalls, event.data] };
    case "done":
      return { ...run, status: "done", done: event.data };
    case "error":
      return { ...run, status: "failed", error: event.data };
  }
}

/** A version the column's select offers. */
export type VersionOption = { version: number; labels: readonly string[] };

type RunColumnProps = {
  /** 1-based position, for the accessible names. */
  number: number;
  column: Column;
  run: RunState;
  connections: readonly Schema<"LlmConnection">[];
  /** Field errors that belong to this column. */
  errors: Record<string, string>;
  /** Why Run is disabled, if it is. */
  runReason: string | undefined;
  /** The source prompt's versions, newest first; `undefined` hides the select (no prompt source). */
  versions: readonly VersionOption[] | undefined;
  /** The pinned version's messages, once loaded; `undefined` for a draft column or while loading. */
  template: readonly Entry[] | undefined;
  /** Pins the column to a version, or back to the draft (`undefined`). */
  onPickVersion: (id: string, version: number | undefined) => void;
  /** The run-all shortcut's label (`⌘↵` or `Ctrl+Enter`), shown before the first run. */
  shortcut: string;
  canRemove: boolean;
  /** The callbacks take the column's id, so the parent can pass the same functions to every column. */
  onChange: (id: string, patch: Partial<Column>) => void;
  onRun: (id: string) => void;
  onStop: (id: string) => void;
  onRemove: (id: string) => void;
  /** Appends the run's answer to the draft's messages. */
  onAddAnswer: (message: Message) => void;
};

/** The line that says how a run ended, or `""` while it runs or before it starts. */
function endText(run: RunState): string {
  switch (run.status) {
    case "done":
      return "Run finished";
    case "stopped":
      return "Stopped";
    case "incomplete":
      return "The stream ended early.";
    case "failed":
      return run.problem ?? run.error?.message ?? "The run failed.";
    default:
      return "";
  }
}

/** The select's text for a version: `v2`, or `v2 · latest, staging`. */
export function versionLabel(option: VersionOption): string {
  return option.labels.length === 0
    ? `v${option.version}`
    : `v${option.version} · ${option.labels.join(", ")}`;
}

/** The text of a template message: its text parts, one per line (prompt versions hold text only). */
function templateText(message: Message): string {
  return message.parts
    .flatMap((part) =>
      part.type === "text" && typeof part.content === "string" ? [part.content] : [],
    )
    .join("\n");
}

/** The error fields the Parameters popover holds: the client's and the server's spellings. */
const PARAMETER_FIELDS = new Set(["temperature", "maxTokens", "max_tokens"]);

/** The Parameters button's text: "Parameters" while both are the provider's defaults. */
export function parametersSummary(column: Pick<Column, "temperature" | "maxTokens">): string {
  const parts: string[] = [];
  if (column.temperature.trim() !== "") {
    parts.push(`temp ${column.temperature.trim()}`);
  }
  if (column.maxTokens.trim() !== "") {
    parts.push(`max ${column.maxTokens.trim()}`);
  }
  return parts.length === 0 ? "Parameters" : parts.join(" · ");
}

/**
 * One model column: a header with its connection, model, parameters, Run or Stop, and remove; then
 * the streamed answer; then the run's metrics, trace link and "Add answer to conversation".
 */
export const RunColumn = memo(function RunColumn({
  number,
  column,
  run,
  connections,
  errors,
  runReason,
  versions,
  template,
  onPickVersion,
  shortcut,
  canRemove,
  onChange,
  onRun,
  onStop,
  onRemove,
  onAddAnswer,
}: RunColumnProps) {
  const { org, project } = useProject();
  const running = run.status === "running";
  const connection = connections.find((item) => item.id === column.connectionId);
  const output = run.done?.output;
  const id = column.id;
  const parameterError = Object.keys(errors).some((field) => PARAMETER_FIELDS.has(field));
  const ended = endText(run);
  const empty = run.status === "idle" && run.text === "" && run.toolCalls.length === 0;
  return (
    <section
      aria-label={`Model ${number}`}
      className="flex min-h-0 min-w-72 flex-1 flex-col rounded-md border"
    >
      <div className="flex flex-wrap items-center gap-1.5 border-b p-2">
        {versions === undefined ? null : (
          <Select
            value={column.version === undefined ? "draft" : String(column.version)}
            onValueChange={(value) => {
              onPickVersion(id, value === "draft" ? undefined : Number(value));
            }}
          >
            <SelectTrigger aria-label="Prompt version" className="w-44 min-w-0 shrink-0">
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              <SelectItem value="draft">Draft</SelectItem>
              {versions.map((option) => (
                <SelectItem key={option.version} value={String(option.version)}>
                  {versionLabel(option)}
                </SelectItem>
              ))}
              {column.version === undefined ||
              versions.some((option) => option.version === column.version) ? null : (
                // A pinned version the list does not offer (a newer page of versions): the trigger still shows it.
                <SelectItem value={String(column.version)}>{`v${column.version}`}</SelectItem>
              )}
            </SelectContent>
          </Select>
        )}
        <Select
          value={column.connectionId ?? ""}
          disabled={connections.length === 0}
          onValueChange={(connectionId) => {
            onChange(id, { connectionId });
          }}
        >
          <SelectTrigger aria-label="Connection" className="w-32 min-w-0 shrink-0">
            <SelectValue placeholder="Connection" />
          </SelectTrigger>
          <SelectContent>
            {connections.map((item) => (
              <SelectItem key={item.id} value={item.id}>
                {item.name}
              </SelectItem>
            ))}
          </SelectContent>
        </Select>
        <Input
          aria-label="Model"
          className="h-7 min-w-32 flex-1 font-mono text-body"
          placeholder={connection?.default_model ?? "Model"}
          value={column.model}
          onChange={(event) => {
            onChange(id, { model: event.target.value });
          }}
        />
        <Popover>
          <PopoverTrigger asChild>
            <Button
              type="button"
              variant="ghost"
              size="sm"
              aria-label={`Parameters of model ${number}`}
              className={parameterError ? "text-destructive" : "text-muted-foreground"}
            >
              <SlidersHorizontal aria-hidden="true" />
              {parametersSummary(column)}
            </Button>
          </PopoverTrigger>
          <PopoverContent align="end" className="w-60 gap-3">
            <ParameterField
              id={`${id}-temperature`}
              label="Temperature"
              inputMode="decimal"
              value={column.temperature}
              error={errors.temperature}
              onChange={(temperature) => {
                onChange(id, { temperature });
              }}
            />
            <ParameterField
              id={`${id}-max-tokens`}
              label="Max tokens"
              inputMode="numeric"
              value={column.maxTokens}
              error={errors.maxTokens ?? errors.max_tokens}
              onChange={(maxTokens) => {
                onChange(id, { maxTokens });
              }}
            />
            <p className="text-caption text-muted-foreground">
              Empty fields use the provider&apos;s default.
            </p>
          </PopoverContent>
        </Popover>
        <DisabledReason reason={running ? undefined : runReason}>
          {running ? (
            <Button
              type="button"
              variant="outline"
              size="sm"
              onClick={() => {
                onStop(id);
              }}
            >
              <Square aria-hidden="true" />
              Stop
            </Button>
          ) : (
            <Button
              type="button"
              size="sm"
              disabled={runReason !== undefined}
              onClick={() => {
                onRun(id);
              }}
            >
              <Play aria-hidden="true" />
              Run
            </Button>
          )}
        </DisabledReason>
        {canRemove ? (
          <Button
            type="button"
            variant="ghost"
            size="icon"
            aria-label={`Remove model ${number}`}
            onClick={() => {
              onRemove(id);
            }}
          >
            <Trash2 aria-hidden="true" />
          </Button>
        ) : null}
      </div>
      <div className="flex min-h-0 flex-1 flex-col gap-3 overflow-y-auto p-3">
        {template !== undefined && column.version !== undefined ? (
          <details open className="rounded-md border bg-muted/40">
            <summary className="cursor-pointer px-2 py-1 text-caption text-muted-foreground">
              Template of v{column.version}
            </summary>
            <ol
              aria-label={`Template of version ${column.version}`}
              className="flex flex-col gap-2 px-2 pb-2"
            >
              {template.map((entry, index) => (
                <li
                  // biome-ignore lint/suspicious/noArrayIndexKey: the template is fixed for the pinned version
                  key={index}
                  className="flex flex-col gap-1"
                >
                  {isPlaceholder(entry) ? (
                    <p className="text-caption text-muted-foreground">Placeholder {entry.name}</p>
                  ) : (
                    <>
                      <p className="text-caption text-muted-foreground">{entry.role}</p>
                      <pre className="font-mono text-body break-words whitespace-pre-wrap">
                        {templateText(entry)}
                      </pre>
                    </>
                  )}
                </li>
              ))}
            </ol>
          </details>
        ) : null}
        {connections.length === 0 ? (
          <p className="text-caption text-muted-foreground">
            Add an LLM connection in{" "}
            <Link
              to="/$org/settings/llm-connections"
              params={{ org }}
              className="underline underline-offset-4"
            >
              organization settings
            </Link>{" "}
            to run the playground.
          </p>
        ) : null}
        {Object.entries(errors).map(([field, message]) => (
          <p key={field} className="text-caption text-destructive">
            {message}
          </p>
        ))}
        {empty && connections.length > 0 ? (
          <p className="m-auto flex items-center gap-2 text-body text-muted-foreground">
            Run to see the answer
            <kbd className="rounded border bg-muted px-1.5 py-0.5 font-mono text-caption">
              {shortcut}
            </kbd>
          </p>
        ) : null}
        {run.text === "" ? null : (
          <pre className="font-mono text-body break-words whitespace-pre-wrap">{run.text}</pre>
        )}
        {run.toolCalls.map((call, index) => (
          <div
            // biome-ignore lint/suspicious/noArrayIndexKey: the calls are append-only and a provider may repeat an id
            key={`${call.id}-${index}`}
            className="flex flex-col gap-1"
          >
            <p className="text-caption text-muted-foreground">Tool call: {call.name}</p>
            <pre className="font-mono text-body break-all whitespace-pre-wrap">
              {prettyJson(call.arguments)}
            </pre>
          </div>
        ))}
        <div className={ended === "" ? "sr-only" : "flex items-center gap-2"}>
          {run.error ? <Badge variant="outline">{run.error.class}</Badge> : null}
          <p
            role="status"
            className={
              run.status === "failed"
                ? "text-body text-destructive"
                : "text-caption text-muted-foreground"
            }
          >
            {ended}
          </p>
        </div>
      </div>
      {run.done || run.traceId || output ? (
        <div className="flex flex-col gap-2 border-t p-2">
          {run.done ? (
            <div className="flex flex-wrap gap-1.5">
              <MetricChip icon={ArrowDownToLine} label="Input tokens">
                {formatTokens(run.done.usage?.input)}
              </MetricChip>
              <MetricChip icon={ArrowUpFromLine} label="Output tokens">
                {formatTokens(run.done.usage?.output)}
              </MetricChip>
              <MetricChip icon={Coins} label="Cost">
                {formatCost(run.done.cost_usd ?? undefined)}
              </MetricChip>
              <MetricChip icon={Timer} label="Latency">
                {formatDuration(run.done.latency_ms * 1_000_000)}
              </MetricChip>
              <MetricChip icon={Zap} label="Time to first token">
                {run.done.ttft_ms === null ? "—" : formatDuration(run.done.ttft_ms * 1_000_000)}
              </MetricChip>
              <MetricChip icon={Flag} label="Finish reason">
                {run.done.finish_reason ?? "—"}
              </MetricChip>
            </div>
          ) : null}
          <div className="flex flex-wrap items-center gap-3">
            {output ? (
              <Button
                type="button"
                variant="outline"
                size="sm"
                onClick={() => {
                  onAddAnswer({
                    role: output.role,
                    parts: output.parts.filter((part) => part.type !== "reasoning"),
                  });
                }}
              >
                Add answer to conversation
              </Button>
            ) : null}
            {run.traceId ? (
              <Link
                to="/$org/$project/traces/$traceId"
                params={{ org, project, traceId: run.traceId }}
                className="text-body underline underline-offset-4"
              >
                Open trace
              </Link>
            ) : null}
          </div>
        </div>
      ) : null}
    </section>
  );
});

/** A labelled parameter input with its error. */
function ParameterField({
  id,
  label,
  inputMode,
  value,
  error,
  onChange,
}: {
  id: string;
  label: string;
  inputMode: "decimal" | "numeric";
  value: string;
  error: string | undefined;
  onChange: (value: string) => void;
}) {
  return (
    <div className="flex flex-col gap-1.5">
      <Label htmlFor={id}>{label}</Label>
      <Input
        id={id}
        inputMode={inputMode}
        className="h-7 text-body"
        placeholder="Default"
        value={value}
        aria-invalid={error ? true : undefined}
        onChange={(event) => {
          onChange(event.target.value);
        }}
      />
    </div>
  );
}
