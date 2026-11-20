import { problemOf } from "@/api/problem";
import type { Schema } from "@/api/types";
import { stringContains } from "@/components/filter-bar/logic";
import { parseEnum, parseString } from "@/lib/search/common";

export type PromptKind = Schema<"PromptKind">;
export type PromptTab = "template" | "versions" | "labels" | "metrics";
export type Position = { line: number; column: number };
/** The search of a prompt page: the tab and the version (absent = the latest). */
export type PromptSearch = { tab?: PromptTab; version?: number };

/** Why a label or prompt control is off for a viewer. */
export const VIEWER_REASON = "Only members can edit prompts";
export const PROTECTED_REASON = "Only project admins can move protected labels";
/** The text when the server refuses a label move with `403` (D-V-10). */
export const LABEL_FORBIDDEN_TEXT = "Only project admins can move this label.";

const PROMPT_TABS: readonly PromptTab[] = ["template", "versions", "labels", "metrics"];
const PROMPT_NAME = /^[A-Za-z0-9][A-Za-z0-9_.-]{0,199}$/;
const LABEL_NAME = /^[a-z0-9][a-z0-9_.-]{0,63}$/;
const ALL_DIGITS = /^\d+$/;
const SEARCH_TEXT_MAX = 200;
const CHAT_ROLES = ["system", "user", "assistant"] as const;

export type ChatRole = (typeof CHAT_ROLES)[number];

/** One message of a chat template, or a placeholder that the SDK fills with a list of messages. */
export type ChatEntry = { role: ChatRole; content: string } | { type: "placeholder"; name: string };

export function parsePromptsSearch(search: Record<string, unknown>): {
  q?: string;
  label?: string;
  tag?: string;
  create?: true;
} {
  const q = parseString(search.q, SEARCH_TEXT_MAX);
  const label = parseString(search.label, SEARCH_TEXT_MAX);
  const tag = parseString(search.tag, SEARCH_TEXT_MAX);
  const create = search.create === 1 || search.create === "1" || search.create === true;
  return {
    ...(q !== undefined ? { q } : {}),
    ...(label !== undefined ? { label } : {}),
    ...(tag !== undefined ? { tag } : {}),
    ...(create ? { create: true as const } : {}),
  };
}

/** An integer of 1 or more, from a number or a string of digits; anything else is absent. */
function parseVersion(value: unknown): number | undefined {
  const number =
    typeof value === "number"
      ? value
      : typeof value === "string" && ALL_DIGITS.test(value)
        ? Number(value)
        : Number.NaN;
  return Number.isSafeInteger(number) && number >= 1 ? number : undefined;
}

export function parsePromptSearch(search: Record<string, unknown>): {
  tab?: PromptTab;
  version?: number;
} {
  const tab = parseEnum(search.tab, PROMPT_TABS);
  const version = parseVersion(search.version);
  return {
    ...(tab !== undefined ? { tab } : {}),
    ...(version !== undefined ? { version } : {}),
  };
}

/** The name rule of D-V-2: letters, digits, `_`, `.`, `-`, starting with a letter or digit. */
export function isValidPromptName(name: string): boolean {
  return PROMPT_NAME.test(name);
}

/** The label rule of D-V-8. `latest` is computed, so it is never a stored label. */
export function isValidLabel(label: string): boolean {
  return LABEL_NAME.test(label) && !ALL_DIGITS.test(label) && label !== "latest";
}

/** The chat messages of a template value. Entries that do not fit the shape are dropped. */
export function asChat(template: unknown): ChatEntry[] {
  if (!Array.isArray(template)) {
    return [];
  }
  const entries: ChatEntry[] = [];
  for (const item of template) {
    const entry = chatEntryOf(item);
    if (entry !== undefined) {
      entries.push(entry);
    }
  }
  return entries;
}

function chatEntryOf(item: unknown): ChatEntry | undefined {
  if (typeof item !== "object" || item === null) {
    return undefined;
  }
  const record = item as Record<string, unknown>;
  if (record.type === "placeholder") {
    return typeof record.name === "string" ? { type: "placeholder", name: record.name } : undefined;
  }
  if (CHAT_ROLES.includes(record.role as ChatRole) && typeof record.content === "string") {
    return { role: record.role as ChatRole, content: record.content };
  }
  return undefined;
}

/**
 * The template as the diff compares it. A text template is its string. A chat template is one
 * block per entry: `[role]` then the content, or `[placeholder: name]`; blocks are joined by a
 * newline.
 */
export function templateText(template: unknown, type: PromptKind): string {
  if (type === "text") {
    return typeof template === "string" ? template : "";
  }
  return asChat(template)
    .map((entry) =>
      "type" in entry ? `[placeholder: ${entry.name}]` : `[${entry.role}]\n${entry.content}`,
    )
    .join("\n");
}

/** The messages a new chat prompt starts with in the editor: a system and a user message. */
export function emptyChat(): ChatEntry[] {
  return [
    { role: "system", content: "" },
    { role: "user", content: "" },
  ];
}

/** The first position of each field of a problem, for the editor's line and column. */
export function fieldPositions(error: unknown): Record<string, Position> {
  const result: Record<string, Position> = {};
  for (const item of problemOf(error)?.errors ?? []) {
    const position = positionOf(item);
    if (position !== undefined && result[item.field] === undefined) {
      result[item.field] = position;
    }
  }
  return result;
}

function positionOf(value: unknown): Position | undefined {
  if (typeof value !== "object" || value === null) {
    return undefined;
  }
  const position = (value as { position?: unknown }).position;
  if (typeof position !== "object" || position === null) {
    return undefined;
  }
  const { line, column } = position as { line?: unknown; column?: unknown };
  return typeof line === "number" && typeof column === "number" ? { line, column } : undefined;
}

/** The protected labels until the project's settings load: `production` is protected by default. */
export const DEFAULT_PROTECTED_LABELS: string[] = ["production"];

/**
 * The label filter's choices: `latest` and `production` first, then the other labels sorted. They
 * are the labels given (the labels seen so far), the protected labels and the selected label,
 * without repeats.
 */
export function labelChoices(
  labelNames: readonly string[],
  protectedLabels: readonly string[],
  selected: string | undefined,
): string[] {
  const labels = new Set<string>([
    ...protectedLabels,
    ...labelNames,
    ...(selected !== undefined ? [selected] : []),
  ]);
  labels.delete("latest");
  labels.delete("production");
  return ["latest", "production", ...[...labels].sort()];
}

/** The tag filter's choices: the tags of the loaded rows and the selected tag, sorted, no repeats. */
export function tagChoices(
  rows: readonly { tags: readonly string[] }[],
  selected: string | undefined,
): string[] {
  const tags = new Set<string>([
    ...rows.flatMap((row) => row.tags),
    ...(selected !== undefined ? [selected] : []),
  ]);
  return [...tags].sort();
}

/** The search with the version set, or without `version` when it is undefined (the latest). */
export function withPromptVersion(search: PromptSearch, version: number | undefined): PromptSearch {
  const { version: _version, ...rest } = search;
  return version === undefined ? rest : { ...rest, version };
}

/** The search with the tab set. */
export function withPromptTab(search: PromptSearch, tab: PromptTab): PromptSearch {
  return { ...search, tab };
}

/** The windows of the Metrics tab; they match the traces list presets of the same names. */
export const METRICS_PRESETS = ["24h", "7d", "30d"] as const;
export type MetricsPreset = (typeof METRICS_PRESETS)[number];

const MINUTE_MS = 60_000;
const DAY_MS = 24 * 60 * MINUTE_MS;
const METRICS_WINDOW_MS: Record<MetricsPreset, number> = {
  "24h": DAY_MS,
  "7d": 7 * DAY_MS,
  "30d": 30 * DAY_MS,
};

/**
 * The metrics window of a preset: it ends at `nowMs` rounded down to the minute, so one render
 * after another asks for the same window. Both ends are RFC 3339 UTC.
 */
export function metricsWindow(preset: MetricsPreset, nowMs: number): { from: string; to: string } {
  const to = Math.floor(nowMs / MINUTE_MS) * MINUTE_MS;
  return {
    from: new Date(to - METRICS_WINDOW_MS[preset]).toISOString(),
    to: new Date(to).toISOString(),
  };
}

/** The traces list search that shows the spans linked to one version, over a metrics window. */
export function tracesSearchFor(
  name: string,
  version: number,
  preset: MetricsPreset,
): { filter: string; range: MetricsPreset } {
  return { filter: stringContains("prompts", `${name}@${version}`), range: preset };
}
