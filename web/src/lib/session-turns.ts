import { useSyncExternalStore } from "react";
import type { Schema } from "@/api/types";
import { quoteString } from "@/lib/dsl/dsl";
import { STORAGE_PREFIX } from "@/product";

/** The span kinds that count as a turn's steps (D-C-28). */
export const STEP_KINDS = [
  "llm",
  "tool",
  "retriever",
  "guardrail",
] as const satisfies readonly Schema<"SpanKind">[];

/** The step kinds shown as chips on a collapsed turn. LLM calls show only in the expanded list. */
export const CHIP_KINDS: ReadonlySet<Schema<"SpanKind">> = new Set([
  "tool",
  "retriever",
  "guardrail",
]);

/**
 * Turns per steps request. A filter is at most 4096 characters (spec 05 § Limits, D95): each quoted
 * id with its separator takes 36, so 100 ids make a 3671-character filter.
 */
export const STEPS_CHUNK = 100;

/** Steps per page of a steps request. */
export const STEPS_PAGE = 1000;

/** Pages read per steps request before giving up on the rest. */
export const STEPS_MAX_PAGES = 5;

export type Bubble = { kind: "markdown" | "json"; text: string };

export type TurnFilter = "failed" | "scored" | "tools";

export const TURN_FILTERS: readonly { id: TurnFilter; label: string }[] = [
  { id: "failed", label: "Failed" },
  { id: "scored", label: "Scored" },
  { id: "tools", label: "Uses tools" },
];

export type TurnDensity = "chat" | "compact";

export const DEFAULT_TURN_DENSITY: TurnDensity = "chat";

/** `cetra.session_density`: one choice per browser. */
export const TURN_DENSITY_STORAGE_KEY = `${STORAGE_PREFIX}session_density`;

const STEP_KIND_SET: ReadonlySet<Schema<"SpanKind">> = new Set(STEP_KINDS);

/** The bubble text of one side of a turn: no bubble when the side has no text. */
export function bubbleOf(
  payload: Schema<"Payload"> | undefined,
  role: "user" | "assistant",
): Bubble | undefined {
  if (!payload) {
    return undefined;
  }
  if (payload.format === "text") {
    return nonEmpty({ kind: "markdown", text: payload.value });
  }
  if (payload.format === "json") {
    return nonEmpty({ kind: "json", text: JSON.stringify(payload.value) ?? "" });
  }
  const matching = payload.value.filter((message) => message.role === role);
  const message = matching[matching.length - 1];
  if (!message) {
    return undefined;
  }
  const text = message.parts
    .flatMap((part) =>
      part.type === "text" && typeof part.content === "string" ? [part.content] : [],
    )
    .join("\n\n");
  return nonEmpty({ kind: "markdown", text });
}

function nonEmpty(bubble: Bubble): Bubble | undefined {
  return bubble.text.trim() === "" ? undefined : bubble;
}

/** A payload as the raw copy has it: the text itself, or compact JSON. */
export function rawText(payload: Schema<"Payload"> | undefined): string | undefined {
  if (!payload) {
    return undefined;
  }
  return payload.format === "text" ? payload.value : (JSON.stringify(payload.value) ?? "");
}

/** The `/spans` filter that asks for the steps of `traceIds` (D-C-28). */
export function stepsFilter(traceIds: readonly string[]): string {
  const ids = traceIds.map((id) => quoteString(id)).join(", ");
  const kinds = STEP_KINDS.map((kind) => quoteString(kind)).join(", ");
  return `trace_id IN (${ids}) AND span_kind IN (${kinds})`;
}

/** Consecutive slices of `size` (the last may be shorter); `[]` for no items. */
export function chunk<T>(items: readonly T[], size: number): T[][] {
  const chunks: T[][] = [];
  for (let start = 0; start < items.length; start += size) {
    chunks.push(items.slice(start, start + size));
  }
  return chunks;
}

/**
 * The steps request's window: from the session's start to just after its end. `Date.parse` drops
 * the sub-millisecond digits, so `from` is at or before the start and `to` is after the end.
 */
export function stepsWindow(start: string, end: string): { from: string; to: string } {
  return {
    from: new Date(Date.parse(start)).toISOString(),
    to: new Date(Date.parse(end) + 1).toISOString(),
  };
}

/**
 * The spans of each trace id, in start order. A root span is the turn itself, not a step (a root
 * with a remote parent counts as one: accepted). A span already seen under its span id is kept
 * once, the first copy.
 */
export function groupSteps(
  spans: readonly Schema<"SpanSummary">[],
): Map<string, Schema<"SpanSummary">[]> {
  const grouped = new Map<string, Schema<"SpanSummary">[]>();
  for (const span of spans) {
    if (span.parent_span_id === undefined || !STEP_KIND_SET.has(span.span_kind)) {
      continue;
    }
    const steps = grouped.get(span.trace_id) ?? [];
    if (steps.some((step) => step.span_id === span.span_id)) {
      continue;
    }
    steps.push(span);
    grouped.set(span.trace_id, steps);
  }
  for (const steps of grouped.values()) {
    steps.sort(
      (left, right) =>
        left.start_time.localeCompare(right.start_time) ||
        left.span_id.localeCompare(right.span_id),
    );
  }
  return grouped;
}

/** The steps shown as chips on a collapsed turn: the tool, retriever and guardrail ones, in order. */
export function chipSteps(steps: readonly Schema<"SpanSummary">[]): Schema<"SpanSummary">[] {
  return steps.filter((step) => CHIP_KINDS.has(step.span_kind));
}

/** `preview` on one line; undefined when it is undefined or only whitespace. */
export function previewLine(preview: string | undefined): string | undefined {
  if (preview === undefined) {
    return undefined;
  }
  const line = preview.replace(/\s+/g, " ").trim();
  return line === "" ? undefined : line;
}

/** A turn's searchable text: its name and both bubbles. */
export function turnText(turn: Schema<"SessionTurn">): string {
  return [
    turn.trace.name,
    bubbleOf(turn.input, "user")?.text,
    bubbleOf(turn.output, "assistant")?.text,
  ]
    .filter((part): part is string => part !== undefined)
    .join("\n");
}

/** Whether a turn passes `query` and every filter in `filters` (`tools` needs the loaded steps). */
export function turnMatches(
  turn: Schema<"SessionTurn">,
  steps: readonly Schema<"SpanSummary">[] | undefined,
  query: string,
  filters: ReadonlySet<TurnFilter>,
): boolean {
  const needle = query.trim().toLowerCase();
  if (needle !== "" && !turnText(turn).toLowerCase().includes(needle)) {
    return false;
  }
  if (filters.has("failed") && turn.trace.status !== "error") {
    return false;
  }
  if (filters.has("scored") && turn.trace.scores.length === 0) {
    return false;
  }
  if (
    filters.has("tools") &&
    !(steps !== undefined && steps.some((step) => step.span_kind === "tool"))
  ) {
    return false;
  }
  return true;
}

/** How many turns pass each filter on its own, with a blank query. */
export function filterCounts(
  turns: readonly Schema<"SessionTurn">[],
  stepsOf: (traceId: string) => readonly Schema<"SpanSummary">[] | undefined,
): Record<TurnFilter, number> {
  const counts: Record<TurnFilter, number> = { failed: 0, scored: 0, tools: 0 };
  for (const filter of TURN_FILTERS) {
    counts[filter.id] = turns.filter((turn) =>
      turnMatches(turn, stepsOf(turn.trace.trace_id), "", new Set([filter.id])),
    ).length;
  }
  return counts;
}

/** Nanoseconds from the previous turn's start to this one's; undefined when it is not a gap. */
export function gapNs(
  previous: Schema<"SessionTurn"> | undefined,
  turn: Schema<"SessionTurn">,
): number | undefined {
  if (previous === undefined) {
    return undefined;
  }
  const gap =
    (Date.parse(turn.trace.start_time) - Date.parse(previous.trace.start_time)) * 1_000_000;
  return gap >= 0 && Number.isFinite(gap) ? gap : undefined;
}

/**
 * The index of the next failed turn after the selected one, wrapping to the start; the first
 * failed turn when none (or an unknown id) is selected; -1 when no turn failed. A selected turn
 * that is the only failed one finds itself again.
 */
export function nextFailed(
  turns: readonly Schema<"SessionTurn">[],
  selectedTraceId: string | undefined,
): number {
  const selected =
    selectedTraceId === undefined
      ? -1
      : turns.findIndex((turn) => turn.trace.trace_id === selectedTraceId);
  if (selected === -1) {
    return turns.findIndex((turn) => turn.trace.status === "error");
  }
  for (let step = 1; step <= turns.length; step += 1) {
    const index = (selected + step) % turns.length;
    const turn = turns[index];
    if (turn?.trace.status === "error") {
      return index;
    }
  }
  return -1;
}

/** The turn's overview cell weight: its duration in milliseconds, at least 1. */
export function stripWeight(turn: Schema<"SessionTurn">): number {
  return Math.max(1, Math.round(turn.trace.duration_ns / 1_000_000));
}

const TURN_DENSITIES: readonly TurnDensity[] = ["chat", "compact"];

const listeners = new Set<() => void>();
let fallback: TurnDensity | null = null;

function isTurnDensity(value: string | null): value is TurnDensity {
  return TURN_DENSITIES.some((density) => density === value);
}

function readStored(): TurnDensity {
  try {
    const value = localStorage.getItem(TURN_DENSITY_STORAGE_KEY);
    return isTurnDensity(value) ? value : DEFAULT_TURN_DENSITY;
  } catch {
    // Only when storage throws (private mode) does the in-memory choice stand in for it.
    return fallback ?? DEFAULT_TURN_DENSITY;
  }
}

function emit(): void {
  for (const listener of listeners) {
    listener();
  }
}

export function getTurnDensity(): TurnDensity {
  return readStored();
}

export function setTurnDensity(next: TurnDensity): void {
  fallback = next;
  try {
    localStorage.setItem(TURN_DENSITY_STORAGE_KEY, next);
  } catch {
    // Private mode can throw. The choice still applies for this page.
  }
  emit();
}

function subscribe(listener: () => void): () => void {
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
  };
}

export function useTurnDensity(): TurnDensity {
  return useSyncExternalStore(subscribe, getTurnDensity, () => DEFAULT_TURN_DENSITY);
}
