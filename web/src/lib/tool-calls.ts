import type { Schema } from "@/api/types";
import { nsBetween } from "@/lib/format";

/** The tool span a tool call ran as (D-C-32). */
export type ToolSpanMatch = {
  spanId: string;
  durationNs: number;
  /** The span's status message, or "Failed", when its status is `error`. */
  error?: string;
};

export type ToolLinks = {
  /** Call id → tool name, over the shown messages and their context. */
  callNames: Map<string, string>;
  /** Call id → response, for each response in the shown messages whose call is also shown. */
  responses: Map<string, unknown>;
  /**
   * Ids that pair one shown call with one shown response: that response is folded into its call.
   * An id used by two calls or two responses folds nothing, so no response is ever hidden.
   */
  folded: Set<string>;
  /** The matched tool span of a `tool_call` part, by part identity. */
  spans: Map<Schema<"Part">, ToolSpanMatch>;
};

/** Not exported: only this module uses it. */
const INLINE_MAX = 80;

function parseObjectOrArray(text: string): object | undefined {
  try {
    const parsed: unknown = JSON.parse(text);
    if (typeof parsed === "object" && parsed !== null) {
      return parsed;
    }
    return undefined;
  } catch {
    return undefined;
  }
}

function isPlainObject(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function firstLine(text: string): string {
  for (const line of text.split("\n")) {
    const trimmed = line.trim();
    if (trimmed !== "") {
      return trimmed;
    }
  }
  return "";
}

function shorten(text: string): string {
  const chars = Array.from(text);
  if (chars.length > INLINE_MAX) {
    return `${chars.slice(0, INLINE_MAX - 1).join("")}…`;
  }
  return text;
}

function inline(value: unknown, depth: number): string {
  let subject = value;
  if (depth === 0 && typeof subject === "string") {
    subject = parseObjectOrArray(subject) ?? subject;
  }
  if (typeof subject === "string") {
    return JSON.stringify(firstLine(subject));
  }
  if (typeof subject === "number" || typeof subject === "boolean") {
    return String(subject);
  }
  if (subject === null) {
    return "null";
  }
  if (subject === undefined) {
    return "";
  }
  if (Array.isArray(subject)) {
    if (subject.length === 0) {
      return "[]";
    }
    if (depth >= 2) {
      return "[…]";
    }
    return `[${subject.map((item) => inline(item, depth + 1)).join(", ")}]`;
  }
  if (isPlainObject(subject)) {
    const entries = Object.entries(subject);
    if (entries.length === 0) {
      return "{}";
    }
    if (depth >= 2) {
      return "{…}";
    }
    return `{${entries.map(([key, item]) => `${key}: ${inline(item, depth + 1)}`).join(", ")}}`;
  }
  return String(subject);
}

/** Arguments as `key: value, …` (no outer braces), shortened to INLINE_MAX. */
export function inlineArgs(value: unknown): string {
  const subject = typeof value === "string" ? (parseObjectOrArray(value) ?? value) : value;
  let text: string;
  if (subject === undefined || subject === null || subject === "") {
    text = "";
  } else if (isPlainObject(subject)) {
    text = Object.entries(subject)
      .map(([key, item]) => `${key}: ${inline(item, 1)}`)
      .join(", ");
  } else {
    text = inline(subject, 0);
  }
  return shorten(text);
}

/** A result: a string's first non-empty line unquoted, else the inline value, shortened to INLINE_MAX. */
export function inlineResult(value: unknown): string {
  let text: string;
  if (value === undefined) {
    text = "";
  } else if (typeof value === "string" && parseObjectOrArray(value) === undefined) {
    text = firstLine(value);
  } else {
    text = inline(value, 0);
  }
  return shorten(text);
}

/**
 * A tool span's arguments. Some instrumentations record the input as `{name, arguments}` (with an
 * optional `id`); when `name` is the span's tool name, the arguments are `arguments`.
 */
export function toolArguments(value: unknown, name: string): unknown {
  const subject = typeof value === "string" ? (parseObjectOrArray(value) ?? value) : value;
  if (
    isPlainObject(subject) &&
    subject.name === name &&
    "arguments" in subject &&
    Object.keys(subject).every((key) => key === "name" || key === "arguments" || key === "id")
  ) {
    return subject.arguments;
  }
  return value;
}

function isToolCall(part: Schema<"Part">): part is Schema<"ToolCallPart"> {
  return part.type === "tool_call";
}

function isToolResponse(part: Schema<"Part">): part is Schema<"ToolCallResponsePart"> {
  return part.type === "tool_call_response";
}

function toolCalls(messages: readonly Schema<"Message">[]): Schema<"ToolCallPart">[] {
  const calls: Schema<"ToolCallPart">[] = [];
  for (const message of messages) {
    for (const part of message.parts) {
      if (isToolCall(part)) {
        calls.push(part);
      }
    }
  }
  return calls;
}

/**
 * Links tool calls and responses within `messages` (the shown ones). `context` messages (earlier
 * turns, not shown) only add call names. `spans` is the result of matchToolSpans, or empty.
 */
export function toolLinks(
  messages: readonly Schema<"Message">[],
  context: readonly Schema<"Message">[] = [],
  spans: Map<Schema<"Part">, ToolSpanMatch> = new Map(),
): ToolLinks {
  const callNames = new Map<string, string>();
  const responses = new Map<string, unknown>();
  const folded = new Set<string>();

  const callCounts = new Map<string, number>();
  for (const call of toolCalls(messages)) {
    if (typeof call.id === "string" && call.id !== "") {
      callCounts.set(call.id, (callCounts.get(call.id) ?? 0) + 1);
      if (typeof call.name === "string" && !callNames.has(call.id)) {
        callNames.set(call.id, call.name);
      }
    }
  }
  for (const call of toolCalls(context)) {
    if (
      typeof call.id === "string" &&
      call.id !== "" &&
      typeof call.name === "string" &&
      !callNames.has(call.id)
    ) {
      callNames.set(call.id, call.name);
    }
  }
  const responseCounts = new Map<string, number>();
  for (const message of messages) {
    for (const part of message.parts) {
      if (isToolResponse(part) && typeof part.id === "string" && callCounts.get(part.id) === 1) {
        responseCounts.set(part.id, (responseCounts.get(part.id) ?? 0) + 1);
        responses.set(part.id, part.response);
      }
    }
  }
  for (const [id, count] of responseCounts) {
    if (count === 1) {
      folded.add(id);
    } else {
      responses.delete(id);
    }
  }
  return { callNames, responses, folded, spans };
}

/** True when the message has parts and every part is a response folded into a shown call. */
export function foldedMessage(message: Schema<"Message">, links: ToolLinks): boolean {
  return (
    message.parts.length > 0 &&
    message.parts.every(
      (part) => isToolResponse(part) && typeof part.id === "string" && links.folded.has(part.id),
    )
  );
}

function callsByName(messages: readonly Schema<"Message">[]): Map<string, Schema<"Part">[]> {
  const byName = new Map<string, Schema<"Part">[]>();
  for (const call of toolCalls(messages)) {
    if (typeof call.name !== "string") {
      continue;
    }
    const list = byName.get(call.name) ?? [];
    list.push(call);
    byName.set(call.name, list);
  }
  return byName;
}

function matchOf(span: Schema<"SpanSummary">): ToolSpanMatch {
  const match: ToolSpanMatch = { spanId: span.span_id, durationNs: span.duration_ns };
  if (span.status === "error") {
    match.error = span.status_message || "Failed";
  }
  return match;
}

/** D-C-32's matching rule. `llm` is the LLM span; `spans` is the trace's tree. */
export function matchToolSpans(
  llm: Schema<"SpanSummary">,
  spans: readonly Schema<"TreeSpan">[],
  input: readonly Schema<"Message">[],
  output: readonly Schema<"Message">[],
): Map<Schema<"Part">, ToolSpanMatch> {
  const parents = new Set<string>([llm.span_id]);
  if (llm.parent_span_id !== undefined) {
    parents.add(llm.parent_span_id);
  }
  const candidates: Schema<"SpanSummary">[] = [];
  for (const row of spans) {
    const span = row.span;
    if (
      span.span_kind === "tool" &&
      span.parent_span_id !== undefined &&
      parents.has(span.parent_span_id)
    ) {
      candidates.push(span);
    }
  }
  candidates.sort((x, y) => nsBetween(y.start_time, x.start_time));

  const inputCalls = callsByName(input);
  const outputCalls = callsByName(output);
  const names = new Set<string>([...inputCalls.keys(), ...outputCalls.keys()]);
  const result = new Map<Schema<"Part">, ToolSpanMatch>();

  for (const name of names) {
    const named = candidates.filter((span) => span.name === name);

    // Calls made before L: the last k candidates ending by L.start, right-aligned.
    const before = named.filter((span) => nsBetween(span.end_time, llm.start_time) >= 0);
    const earlier = inputCalls.get(name) ?? [];
    const kBefore = Math.min(earlier.length, before.length);
    for (let i = 0; i < kBefore; i++) {
      const call = earlier[earlier.length - kBefore + i];
      const span = before[before.length - kBefore + i];
      if (call !== undefined && span !== undefined) {
        result.set(call, matchOf(span));
      }
    }

    // Calls L asks for: the first k candidates starting at or after L.start, left-aligned.
    const after = named.filter((span) => nsBetween(llm.start_time, span.start_time) >= 0);
    const later = outputCalls.get(name) ?? [];
    const kAfter = Math.min(later.length, after.length);
    for (let i = 0; i < kAfter; i++) {
      const call = later[i];
      const span = after[i];
      if (call !== undefined && span !== undefined) {
        result.set(call, matchOf(span));
      }
    }
  }
  return result;
}
