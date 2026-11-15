import type { QueryClient } from "@tanstack/react-query";
import { problemOf } from "@/api/problem";
import { scoresQuery } from "@/api/queries";
import type { Schema } from "@/api/types";

type Score = Schema<"Score">;
type ScoreDefinition = Schema<"ScoreDefinition">;
type ScoreValue = Schema<"ScoreValue">;

/** What an annotation is attached to. */
export type AnnotationTarget =
  | { type: "trace"; traceId: string }
  | { type: "span"; traceId: string; spanId: string }
  | { type: "session"; sessionId: string };

/** Ids copied onto the score, as evaluator scores copy them (D-I-18, D-I-37). */
export type AnnotationContext = { sessionId?: string; userId?: string };

/** One row of the form as typed. `value` is "" when unset; see `draftOf` for each type's text. */
export type Draft = { value: string; reason: string };

export const EMPTY_DRAFT: Draft = { value: "", reason: "" };

/** The form is keyed by it, so a new target starts from a clean form. */
export function targetKey(target: AnnotationTarget): string {
  switch (target.type) {
    case "trace":
      return `trace:${target.traceId}`;
    case "span":
      return `span:${target.traceId}:${target.spanId}`;
    case "session":
      return `session:${target.sessionId}`;
  }
}

/** The `target` of a score write for this annotation target. */
export function scoreTarget(target: AnnotationTarget): Schema<"ScoreTarget"> {
  switch (target.type) {
    case "trace":
      return { type: "trace", trace_id: target.traceId };
    case "span":
      return { type: "span", trace_id: target.traceId, span_id: target.spanId };
    case "session":
      return { type: "session", session_id: target.sessionId };
  }
}

/** The score list of the target. The trace and span forms are the Scores tab's, so both share the cache. */
export function scoreListQuery(org: string, project: string, target: AnnotationTarget) {
  switch (target.type) {
    case "trace":
      return scoresQuery(org, project, { trace_id: target.traceId, target: "trace", limit: 1000 });
    case "span":
      return scoresQuery(org, project, {
        trace_id: target.traceId,
        span_id: target.spanId,
        target: "span",
        limit: 1000,
      });
    case "session":
      return scoresQuery(org, project, {
        session_id: target.sessionId,
        target: "session",
        limit: 1000,
      });
  }
}

/** True when the score is attached to the target: same kind, same ids. */
export function onTarget(score: Score, target: AnnotationTarget): boolean {
  const attached = score.target;
  switch (target.type) {
    case "trace":
      return attached.type === "trace" && attached.trace_id === target.traceId;
    case "span":
      return (
        attached.type === "span" &&
        attached.trace_id === target.traceId &&
        attached.span_id === target.spanId
      );
    case "session":
      return attached.type === "session" && attached.session_id === target.sessionId;
  }
}

/**
 * The caller's live `human` scores on the target, by name. When two share a name,
 * the one updated last wins.
 */
export function ownScores(
  scores: readonly Score[],
  target: AnnotationTarget,
  userId: string,
): Map<string, Score> {
  const result = new Map<string, Score>();
  for (const score of scores) {
    if (
      score.deleted ||
      score.source !== "human" ||
      score.author_user_id !== userId ||
      !onTarget(score, target)
    ) {
      continue;
    }
    const current = result.get(score.name);
    if (current === undefined || score.updated_at.localeCompare(current.updated_at) > 0) {
      result.set(score.name, score);
    }
  }
  return result;
}

/** The form row of a saved score. Empty when there is none, or its type is not the definition's. */
export function draftOf(score: Score | undefined, definition: ScoreDefinition): Draft {
  if (score === undefined || score.value.data_type !== definition.data_type) {
    return EMPTY_DRAFT;
  }
  return { value: valueText(score.value), reason: score.reason ?? "" };
}

function valueText(value: ScoreValue): string {
  switch (value.data_type) {
    case "numeric":
      return String(value.value);
    case "boolean":
      return value.value ? "true" : "false";
    case "categorical":
      return value.value.label;
    case "text":
      return value.value;
  }
}

/** True when the row has a value. */
export function isSet(draft: Draft): boolean {
  return draft.value.trim() !== "";
}

/** True when the value or the trimmed reason differs from the saved row. */
export function isDirty(draft: Draft, saved: Draft): boolean {
  return draft.value !== saved.value || draft.reason.trim() !== saved.reason.trim();
}

/** The message for a row that cannot be saved, or `undefined`. An unset row is never an error. */
export function draftError(draft: Draft, definition: ScoreDefinition): string | undefined {
  if (!isSet(draft)) {
    return undefined;
  }
  switch (definition.data_type) {
    case "numeric":
      return numericError(draft.value, definition);
    case "boolean":
      return draft.value === "true" || draft.value === "false" ? undefined : "Choose Yes or No.";
    case "categorical":
      return definition.categories.some((category) => category.label === draft.value)
        ? undefined
        : "Choose one of the categories.";
    case "text":
      return undefined;
  }
}

/** A plain decimal number: no hex, binary or octal forms, which `Number` would also accept. */
const DECIMAL = /^[+-]?(\d+\.?\d*|\.\d+)(e[+-]?\d+)?$/i;

function numericError(text: string, definition: ScoreDefinition): string | undefined {
  const value = Number(text);
  if (!DECIMAL.test(text.trim()) || !Number.isFinite(value)) {
    return "Enter a number.";
  }
  const min = definition.min_value;
  const max = definition.max_value;
  if (min !== undefined && max !== undefined) {
    return value < min || value > max ? `Enter a number from ${min} to ${max}.` : undefined;
  }
  if (min !== undefined && value < min) {
    return `Enter a number of at least ${min}.`;
  }
  if (max !== undefined && value > max) {
    return `Enter a number of at most ${max}.`;
  }
  return undefined;
}

/** The value of a set, valid row. A categorical value carries its category's number when it has one. */
function valueOfDraft(definition: ScoreDefinition, draft: Draft): ScoreValue {
  switch (definition.data_type) {
    case "numeric":
      return { data_type: "numeric", value: Number(draft.value) };
    case "boolean":
      return { data_type: "boolean", value: draft.value === "true" };
    case "categorical": {
      const category = definition.categories.find((item) => item.label === draft.value);
      return {
        data_type: "categorical",
        value: {
          label: draft.value,
          ...(category?.value !== undefined ? { value: category.value } : {}),
        },
      };
    }
    case "text":
      return { data_type: "text", value: draft.value.trim() };
  }
}

/**
 * The score item of a set, valid row. It never carries `score_id` or `source`: the server
 * derives the id per annotator and stores `human` for a session (D-G-28, D-G-27).
 */
export function scoreItem(
  definition: ScoreDefinition,
  draft: Draft,
  target: AnnotationTarget,
  context: AnnotationContext,
): Schema<"ScoreCreate"> {
  const item: Schema<"ScoreCreate"> = {
    target: scoreTarget(target),
    name: definition.name,
    value: valueOfDraft(definition, draft),
  };
  const reason = draft.reason.trim();
  if (reason !== "") {
    item.reason = reason;
  }
  if (target.type !== "session" && context.sessionId !== undefined) {
    item.session_id = context.sessionId;
  }
  if (context.userId !== undefined) {
    item.user_id = context.userId;
  }
  return item;
}

/**
 * The first message of each row of a `400` batch, keyed by the definition id sent at that
 * index (`scores[i].field`). Other errors give `{}`.
 */
export function rowErrors(error: unknown, sentIds: readonly string[]): Record<string, string> {
  const result: Record<string, string> = {};
  for (const item of problemOf(error)?.errors ?? []) {
    const match = /^scores\[(\d+)\]\./.exec(item.field);
    const id = match ? sentIds[Number(match[1])] : undefined;
    if (id !== undefined && result[id] === undefined) {
      result[id] = item.message;
    }
  }
  return result;
}

/** The reads that show scores: score lists, and the trace and session lists and details. */
export const SCORE_READ_PATHS: readonly string[] = [
  "/api/v1/projects/{project}/scores",
  "/api/v1/projects/{project}/traces",
  "/api/v1/projects/{project}/traces/{trace_id}",
  "/api/v1/projects/{project}/sessions",
  "/api/v1/projects/{project}/sessions/{session_id}",
];

/** Marks the score reads stale after a save or a clear. */
export function invalidateScoreReads(queryClient: QueryClient): Promise<void> {
  return queryClient.invalidateQueries({
    predicate: (query) => {
      const path = query.queryKey[1];
      return typeof path === "string" && SCORE_READ_PATHS.includes(path);
    },
  });
}
