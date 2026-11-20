/**
 * `withContext` and the context values it stores in the OTel context (D-F-12).
 *
 * The values live under one context key, so they follow the context manager
 * (`AsyncLocalStorage` on Node) into callbacks, promises and concurrent flows.
 * Nested blocks merge: inner session/user win, tags are the union (outer
 * first, no duplicates), metadata merges by key with the inner value winning.
 * Invalid options are dropped silently, with a debug log; user code runs
 * unchanged either way.
 */

import {
  type Context,
  context,
  createContextKey,
  propagation,
  ROOT_CONTEXT,
} from "@opentelemetry/api";

import { debug } from "./log.js";
import { applyMask } from "./mask.js";
import { Prompt } from "./prompt.js";

/** README § Names: one OTel context key for all context values. */
export const CONTEXT_KEY = createContextKey("cetra.context");

const BAGGAGE_SESSION_KEY = "cetra.session.id";
const BAGGAGE_USER_KEY = "cetra.user.id";
/** § Span attributes: not empty, only `[A-Za-z0-9_.-]`. */
const METADATA_KEY = /^[A-Za-z0-9_.-]+$/;

export interface ContextOptions {
  sessionId?: string;
  userId?: string;
  tags?: readonly string[];
  metadata?: Readonly<Record<string, unknown>>;
  /** Also set the session/user baggage entries for W3C propagation. */
  propagate?: boolean;
  /** Stamps `cetra.prompt.name` and, when the prompt has one, `cetra.prompt.version` (D-V-15). */
  prompt?: Prompt;
}

/** The experiment and dataset item of an `evaluate` run (D-J-16). Internal. */
export interface ExperimentLink {
  readonly experimentId: string;
  readonly datasetItemId: string;
}

/** The prompt of a context block: its name, and its version unless it is a fallback. */
export interface PromptLink {
  readonly name: string;
  readonly version: number | undefined;
}

export interface ContextValues {
  readonly sessionId: string | undefined;
  readonly userId: string | undefined;
  readonly tags: readonly string[];
  readonly metadata: Readonly<Record<string, unknown>>;
  readonly experiment: ExperimentLink | undefined;
  readonly prompt: PromptLink | undefined;
}

const EMPTY_VALUES: ContextValues = Object.freeze({
  sessionId: undefined,
  userId: undefined,
  tags: Object.freeze([]),
  metadata: Object.freeze({}) as Readonly<Record<string, unknown>>,
  experiment: undefined,
  prompt: undefined,
});

/**
 * A context holding only the experiment link, built on `ROOT_CONTEXT`: no
 * active span, no baggage and none of the caller's values, so the run's root
 * span starts a new trace. Internal (`evaluate`).
 */
export function experimentContext(experimentId: string, datasetItemId: string): Context {
  const values: ContextValues = Object.freeze({
    ...EMPTY_VALUES,
    experiment: Object.freeze({ experimentId, datasetItemId }),
  });
  return ROOT_CONTEXT.setValue(CONTEXT_KEY, values);
}

/** The context values of `ctx` (the active context when omitted). Internal. */
export function currentValues(ctx?: Context): ContextValues {
  return coerceValues((ctx ?? context.active()).getValue(CONTEXT_KEY));
}

/**
 * Run `fn` with the merged context values in scope. The return value (a
 * promise included) is returned unchanged; so are `fn`'s exceptions.
 */
export function withContext<T>(options: ContextOptions, fn: () => T): T {
  return context.with(contextFor(options), fn);
}

/**
 * The context to enter: the active context plus the merged values under
 * `CONTEXT_KEY` and, with `propagate`, our two baggage entries merged into
 * any existing baggage. Never throws; on failure the active context.
 */
function contextFor(options: ContextOptions): Context {
  try {
    const raw = options as Record<string, unknown>;
    const outer = currentValues();
    const inner = validOptions(raw);
    const merged: ContextValues = Object.freeze({
      sessionId: inner.sessionId ?? outer.sessionId,
      userId: inner.userId ?? outer.userId,
      tags: Object.freeze(unionTags(outer.tags, inner.tags)),
      metadata: Object.freeze({ ...outer.metadata, ...inner.metadata }),
      experiment: outer.experiment,
      prompt: inner.prompt ?? outer.prompt,
    });
    let ctx = context.active().setValue(CONTEXT_KEY, merged);
    if (raw.propagate) {
      let baggage = propagation.getBaggage(ctx) ?? propagation.createBaggage();
      if (merged.sessionId !== undefined) {
        baggage = baggage.setEntry(BAGGAGE_SESSION_KEY, { value: merged.sessionId });
      }
      if (merged.userId !== undefined) {
        baggage = baggage.setEntry(BAGGAGE_USER_KEY, { value: merged.userId });
      }
      ctx = propagation.setBaggage(ctx, baggage);
    }
    return ctx;
  } catch (error) {
    debug(`cetra: withContext failed: ${errorName(error)}`);
    return context.active();
  }
}

/** The valid parts of user-given options; invalid values are dropped silently. */
function validOptions(raw: Record<string, unknown>): {
  sessionId: string | undefined;
  userId: string | undefined;
  tags: string[];
  metadata: Record<string, unknown>;
  prompt: PromptLink | undefined;
} {
  const sessionId = validString(raw.sessionId, "sessionId");
  const userId = validString(raw.userId, "userId");
  const tags = validTags(raw.tags);
  const metadata: Record<string, unknown> = {};
  const source = raw.metadata;
  if (typeof source === "object" && source !== null && !Array.isArray(source)) {
    // Metadata values are masked at capture time, on entry (D-F-6).
    for (const [key, value] of Object.entries(source)) {
      metadata[key] = applyMask(value);
    }
  } else if (source !== undefined) {
    debug("cetra: withContext metadata option dropped");
  }
  return { sessionId, userId, tags, metadata, prompt: validPrompt(raw.prompt) };
}

/** The link of a `Prompt`; anything else is dropped with a debug log. */
function validPrompt(value: unknown): PromptLink | undefined {
  if (value === undefined) {
    return undefined;
  }
  if (value instanceof Prompt) {
    return Object.freeze({ name: value.name, version: value.version });
  }
  debug("cetra: withContext prompt option dropped");
  return undefined;
}

function validString(value: unknown, name: string): string | undefined {
  if (typeof value === "string") {
    return value;
  }
  if (value !== undefined) {
    debug(`cetra: withContext ${name} option dropped`);
  }
  return undefined;
}

/** A bare string is one tag; non-string items of an array are skipped. */
function validTags(value: unknown): string[] {
  if (typeof value === "string") {
    return [value];
  }
  if (!Array.isArray(value)) {
    if (value !== undefined) {
      debug("cetra: withContext tags option dropped");
    }
    return [];
  }
  return value.filter((tag): tag is string => typeof tag === "string");
}

/** The union of outer and inner tags: outer first, no duplicates. */
function unionTags(outer: readonly string[], inner: readonly string[]): string[] {
  const seen = new Set(outer);
  const tags = [...outer];
  for (const tag of inner) {
    if (!seen.has(tag)) {
      seen.add(tag);
      tags.push(tag);
    }
  }
  return tags;
}

/** Only what this module stored passes as is; anything else under the key is garbage. */
function coerceValues(raw: unknown): ContextValues {
  if (typeof raw !== "object" || raw === null || Array.isArray(raw)) {
    return EMPTY_VALUES;
  }
  const record = raw as Record<string, unknown>;
  const tags = Array.isArray(record.tags)
    ? record.tags.filter((tag): tag is string => typeof tag === "string")
    : [];
  const metadata =
    typeof record.metadata === "object" &&
    record.metadata !== null &&
    !Array.isArray(record.metadata)
      ? (record.metadata as Readonly<Record<string, unknown>>)
      : {};
  return {
    sessionId: typeof record.sessionId === "string" ? record.sessionId : undefined,
    userId: typeof record.userId === "string" ? record.userId : undefined,
    tags,
    metadata,
    experiment: coerceLink(record.experiment),
    prompt: coercePrompt(record.prompt),
  };
}

/** A prompt link with a string name; anything else is no link. */
function coercePrompt(raw: unknown): PromptLink | undefined {
  if (typeof raw !== "object" || raw === null) {
    return undefined;
  }
  const record = raw as Record<string, unknown>;
  if (typeof record.name !== "string") {
    return undefined;
  }
  return {
    name: record.name,
    version: typeof record.version === "number" ? record.version : undefined,
  };
}

/** An experiment link with two string ids; anything else is no link. */
function coerceLink(raw: unknown): ExperimentLink | undefined {
  if (typeof raw !== "object" || raw === null) {
    return undefined;
  }
  const record = raw as Record<string, unknown>;
  if (typeof record.experimentId !== "string" || typeof record.datasetItemId !== "string") {
    return undefined;
  }
  return { experimentId: record.experimentId, datasetItemId: record.datasetItemId };
}

const skippedMetadataKeys = new Set<string>();

/**
 * The attribute name a metadata key maps to (`cetra.metadata.<key>`), or
 * `undefined` when the key fails the rule (§ Span attributes). A failing key
 * is logged once at debug level, with its length only, never the key.
 */
export function metadataAttributeName(key: string): string | undefined {
  if (METADATA_KEY.test(key)) {
    return `cetra.metadata.${key}`;
  }
  if (!skippedMetadataKeys.has(key)) {
    skippedMetadataKeys.add(key);
    debug(`cetra: metadata key of length ${key.length} skipped`);
  }
  return undefined;
}

function errorName(error: unknown): string {
  return error instanceof Error ? error.name : typeof error;
}
