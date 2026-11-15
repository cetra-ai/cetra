import type { Schema } from "@/api/types";

export type JsonParse = { ok: true; value: unknown } | { ok: false; message: string };

/**
 * Parses a JSON textarea. Empty (after trimming): `{ ok: true, value: undefined }` when not
 * required, else `{ ok: false, message: "Required" }`. Invalid: "Not valid JSON". With `object`,
 * a parsed value that is not a plain object (null, an array, a scalar) is
 * "Must be a JSON object".
 */
export function parseJsonField(
  text: string,
  options: { required: boolean; object?: boolean },
): JsonParse {
  const source = text.trim();
  if (source === "") {
    return options.required ? { ok: false, message: "Required" } : { ok: true, value: undefined };
  }
  let value: unknown;
  try {
    value = JSON.parse(source);
  } catch {
    return { ok: false, message: "Not valid JSON" };
  }
  if (options.object && (typeof value !== "object" || value === null || Array.isArray(value))) {
    return { ok: false, message: "Must be a JSON object" };
  }
  return { ok: true, value };
}

/** Whether two JSON values are the same text, the test the forms use to send only changed fields. */
export function sameJson(left: unknown, right: unknown): boolean {
  return JSON.stringify(left) === JSON.stringify(right);
}

/** `JSON.stringify(value, null, 2)`; `""` for `undefined`. */
export function prettyJson(value: unknown): string {
  return value === undefined ? "" : JSON.stringify(value, null, 2);
}

/** Compact JSON of `value` cut to `max` characters (default 120) with "…"; a string shows as itself; `undefined` is "—". */
export function previewJson(value: unknown, max = 120): string {
  if (value === undefined) {
    return "—";
  }
  const text = typeof value === "string" ? value : JSON.stringify(value);
  return text.length > max ? `${text.slice(0, max)}…` : text;
}

/** The JSON a span payload stands for (D-J-8): `messages` and `json` → their `value`, `text` → the string; none → `undefined`. */
export function payloadJson(payload: Schema<"Payload"> | undefined): unknown {
  return payload?.value;
}

/** The tabs of the dataset page. M6 adds "experiments". */
export type DatasetTab = "items" | "versions" | "settings";

const DATASET_TABS: readonly DatasetTab[] = ["items", "versions", "settings"];

/**
 * The dataset page's search params. An unknown `tab` is dropped; `version` is a non-negative
 * integer, given as a number or a string of digits, and anything else is dropped.
 */
export function parseDatasetSearch(search: Record<string, unknown>): {
  tab?: DatasetTab;
  version?: number;
} {
  const tab = DATASET_TABS.find((candidate) => candidate === search.tab);
  const version = parseVersion(search.version);
  return {
    ...(tab ? { tab } : {}),
    ...(version !== undefined ? { version } : {}),
  };
}

function parseVersion(value: unknown): number | undefined {
  let parsed: number | undefined;
  if (typeof value === "number") {
    parsed = value;
  } else if (typeof value === "string" && /^\d+$/.test(value)) {
    parsed = Number(value);
  }
  return parsed !== undefined && Number.isSafeInteger(parsed) && parsed >= 0 ? parsed : undefined;
}

export type ImportFormat = "csv" | "jsonl";

/** The largest file the import wizard takes (D-J-6). */
export const IMPORT_MAX_BYTES = 100 * 1024 * 1024;

/** The head of a file the preview reads (D-J-6). */
export const PREVIEW_BYTES = 1024 * 1024;

/** `csv` for `.csv`; `jsonl` for `.jsonl` and `.ndjson` (case-insensitive); else `undefined`. */
export function importFormatOf(fileName: string): ImportFormat | undefined {
  const name = fileName.toLowerCase();
  if (name.endsWith(".csv")) {
    return "csv";
  }
  if (name.endsWith(".jsonl") || name.endsWith(".ndjson")) {
    return "jsonl";
  }
  return undefined;
}

/** The import targets of D-J-6. */
export type ImportTarget =
  | "input"
  | `input.${string}`
  | "expected"
  | `expected.${string}`
  | `metadata.${string}`
  | "ignore";

/**
 * A column's default target: `input` → "input"; `expected` or `expected_output` → "expected";
 * `metadata` → "ignore"; any other column `c` → `input.c`. Names compare case-insensitively
 * after trimming.
 */
export function defaultTarget(column: string): ImportTarget {
  const name = column.trim().toLowerCase();
  if (name === "input") {
    return "input";
  }
  if (name === "expected" || name === "expected_output") {
    return "expected";
  }
  if (name === "metadata") {
    return "ignore";
  }
  return `input.${column}`;
}

/**
 * Problems with a mapping, in this order, each once: no column maps to the input; the whole
 * input and its keys both used; the same for the expected output; a target used by two columns;
 * a keyed target with an empty key.
 */
export function mappingProblems(rows: { column: string; target: ImportTarget }[]): string[] {
  const targets: string[] = rows.map((row) => row.target);
  const problems: string[] = [];
  const inputKeyed = targets.some((target) => target.startsWith("input."));
  const expectedKeyed = targets.some((target) => target.startsWith("expected."));
  if (!targets.includes("input") && !inputKeyed) {
    problems.push("Map at least one column to the input");
  }
  if (targets.includes("input") && inputKeyed) {
    problems.push("Map the whole input or input keys, not both");
  }
  if (targets.includes("expected") && expectedKeyed) {
    problems.push("Map the whole expected output or its keys, not both");
  }
  const counts = new Map<string, number>();
  for (const target of targets) {
    if (target !== "ignore") {
      counts.set(target, (counts.get(target) ?? 0) + 1);
    }
  }
  for (const [target, count] of counts) {
    if (count > 1) {
      problems.push(`Two columns map to ${target}`);
    }
  }
  for (const row of rows) {
    if (/^(input|expected|metadata)\.$/.test(row.target)) {
      problems.push(`${row.column}: the key is empty`);
    }
  }
  return problems;
}
