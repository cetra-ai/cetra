import { Link } from "@tanstack/react-router";
import { ArrowDownToLine, ArrowUpFromLine, Coins, Flag, Timer, Trash2, Zap } from "lucide-react";
import type { Schema } from "@/api/types";
import { DisabledReason } from "@/components/disabled-reason";
import { useProject } from "@/components/shell/project-context";
import { MetricChip } from "@/components/trace/metric-chip";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { prettyJson } from "@/lib/datasets";
import { formatCost, formatDuration, formatTokens } from "@/lib/format";
import type { Column, Message, RunEvent } from "@/lib/playground";

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
  canRemove: boolean;
  onChange: (patch: Partial<Column>) => void;
  onRun: () => void;
  onStop: () => void;
  onRemove: () => void;
  /** Appends the run's answer to the draft's messages. */
  onAddAnswer: (message: Message) => void;
};

/** One model column: its connection, model and parameters, Run or Stop, and the streamed answer. */
export function RunColumn({
  number,
  column,
  run,
  connections,
  errors,
  runReason,
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
  return (
    <section
      aria-label={`Model ${number}`}
      className="flex min-w-72 flex-1 flex-col gap-3 rounded-md border p-3"
    >
      <div className="flex items-center gap-2">
        <Select
          value={column.connectionId ?? ""}
          disabled={connections.length === 0}
          onValueChange={(connectionId) => {
            onChange({ connectionId });
          }}
        >
          <SelectTrigger aria-label="Connection" className="min-w-0 flex-1">
            <SelectValue placeholder="Choose a connection" />
          </SelectTrigger>
          <SelectContent>
            {connections.map((item) => (
              <SelectItem key={item.id} value={item.id}>
                {item.name}
              </SelectItem>
            ))}
          </SelectContent>
        </Select>
        {canRemove ? (
          <Button
            type="button"
            variant="ghost"
            size="icon"
            aria-label={`Remove model ${number}`}
            onClick={onRemove}
          >
            <Trash2 aria-hidden="true" />
          </Button>
        ) : null}
      </div>
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
      <div className="flex flex-wrap gap-2">
        <Input
          aria-label="Model"
          className="h-7 min-w-40 flex-1 font-mono text-body"
          placeholder={connection?.default_model ?? ""}
          value={column.model}
          onChange={(event) => {
            onChange({ model: event.target.value });
          }}
        />
        <Input
          aria-label="Temperature"
          inputMode="decimal"
          className="h-7 w-24 text-body"
          placeholder="Default"
          value={column.temperature}
          onChange={(event) => {
            onChange({ temperature: event.target.value });
          }}
        />
        <Input
          aria-label="Max tokens"
          inputMode="numeric"
          className="h-7 w-28 text-body"
          placeholder="Default"
          value={column.maxTokens}
          onChange={(event) => {
            onChange({ maxTokens: event.target.value });
          }}
        />
      </div>
      <div className="flex items-center gap-2">
        <DisabledReason reason={running ? undefined : runReason}>
          {running ? (
            <Button type="button" variant="outline" size="sm" onClick={onStop}>
              Stop
            </Button>
          ) : (
            <Button type="button" size="sm" disabled={runReason !== undefined} onClick={onRun}>
              Run
            </Button>
          )}
        </DisabledReason>
      </div>
      {Object.entries(errors).map(([field, message]) => (
        <p key={field} className="text-caption text-destructive">
          {message}
        </p>
      ))}
      {run.problem ? <p className="text-body text-destructive">{run.problem}</p> : null}
      {run.error ? (
        <div className="flex items-center gap-2">
          <Badge variant="outline">{run.error.class}</Badge>
          <p className="text-body text-destructive">{run.error.message}</p>
        </div>
      ) : null}
      {run.status === "stopped" ? (
        <p className="text-caption text-muted-foreground">Stopped</p>
      ) : null}
      {run.status === "incomplete" ? (
        <p className="text-caption text-muted-foreground">The stream ended early.</p>
      ) : null}
      {run.traceId ? (
        <Link
          to="/$org/$project/traces/$traceId"
          params={{ org, project, traceId: run.traceId }}
          className="w-fit text-body underline underline-offset-4"
        >
          Open trace
        </Link>
      ) : null}
      <pre aria-live="polite" className="font-mono text-body break-words whitespace-pre-wrap">
        {run.text}
      </pre>
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
      {output ? (
        <Button
          type="button"
          variant="outline"
          size="sm"
          className="w-fit"
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
    </section>
  );
}
