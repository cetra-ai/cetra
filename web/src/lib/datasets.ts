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
