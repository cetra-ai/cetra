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

/**
 * Compact JSON of `value` cut to `max` characters (default 120) with "…"; a string shows as itself;
 * `undefined` is "—". Serializing stops once the output is past `max`, so a huge value costs
 * about `max` characters of work, not its full size.
 */
export function previewJson(value: unknown, max = 120): string {
  if (value === undefined) {
    return "—";
  }
  const text = typeof value === "string" ? value : boundedJson(value, max);
  return text.length > max ? `${text.slice(0, max)}…` : text;
}

/**
 * The compact JSON of `value`, stopped once it is longer than `limit` characters: the result has
 * at least `limit + 1` characters when the full JSON is that long, and is the full JSON otherwise.
 * Follows `JSON.stringify`: `toJSON` is honoured, undefined, functions and symbols are skipped in
 * objects and written as `null` in arrays.
 */
function boundedJson(value: unknown, limit: number): string {
  const parts: string[] = [];
  let size = 0;
  // Returns false once the output is past the limit, which stops every loop below.
  function write(piece: string): boolean {
    parts.push(piece);
    size += piece.length;
    return size <= limit;
  }
  function visit(input: unknown): boolean {
    let current = input;
    if (typeof current === "object" && current !== null && "toJSON" in current) {
      const toJSON = (current as { toJSON: unknown }).toJSON;
      if (typeof toJSON === "function") {
        current = toJSON.call(current);
      }
    }
    if (typeof current === "string") {
      return write(JSON.stringify(current));
    }
    if (typeof current !== "object" || current === null) {
      const piece = JSON.stringify(current);
      return write(piece === undefined ? "null" : piece);
    }
    if (Array.isArray(current)) {
      if (!write("[")) {
        return false;
      }
      for (let index = 0; index < current.length; index++) {
        if (index > 0 && !write(",")) {
          return false;
        }
        if (!visit(current[index])) {
          return false;
        }
      }
      return write("]");
    }
    if (!write("{")) {
      return false;
    }
    let first = true;
    for (const [key, member] of Object.entries(current)) {
      if (member === undefined || typeof member === "function" || typeof member === "symbol") {
        continue;
      }
      if (!first && !write(",")) {
        return false;
      }
      first = false;
      if (!write(`${JSON.stringify(key)}:`) || !visit(member)) {
        return false;
      }
    }
    return write("}");
  }
  visit(value);
  return parts.join("");
}

/** The tabs of the dataset page. M6 adds "experiments". */
export type DatasetTab = "items" | "versions" | "experiments" | "settings";

const DATASET_TABS: readonly DatasetTab[] = ["items", "versions", "experiments", "settings"];

/** The dataset page's search params (see `parseDatasetSearch`). */
export type DatasetSearch = {
  tab?: DatasetTab;
  version?: number;
  /** The open item's id. */
  item?: string;
  /** The item search text, trimmed (D-J-22). */
  q?: string;
};

/** The longest item search, in characters (D-J-22). */
export const MAX_SEARCH_CHARS = 200;

/**
 * The dataset page's search params. An unknown `tab` is dropped; `version` is a non-negative
 * integer, given as a number or a string of digits, and anything else is dropped; `item` is a
 * non-empty string, and anything else is dropped; `q` is trimmed and kept when it is 1 to 200
 * characters, and dropped otherwise.
 */
export function parseDatasetSearch(search: Record<string, unknown>): DatasetSearch {
  const tab = DATASET_TABS.find((candidate) => candidate === search.tab);
  const version = parseVersion(search.version);
  const item = typeof search.item === "string" && search.item !== "" ? search.item : undefined;
  const q = parseSearchText(search.q);
  return {
    ...(tab ? { tab } : {}),
    ...(version !== undefined ? { version } : {}),
    ...(item !== undefined ? { item } : {}),
    ...(q !== undefined ? { q } : {}),
  };
}

/** The trimmed text when it is 1 to 200 characters (code points, as the server counts), else undefined. */
export function parseSearchText(value: unknown): string | undefined {
  if (typeof value !== "string") {
    return undefined;
  }
  const text = value.trim();
  const length = [...text].length;
  return length >= 1 && length <= MAX_SEARCH_CHARS ? text : undefined;
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

/** The longest key of a mapped target, in characters (the server's rule). */
export const MAX_KEY_CHARS = 200;

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
 * a keyed target whose key is blank or longer than 200 characters.
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
    const keyed = /^(?:input|expected|metadata)\.(.*)$/s.exec(row.target);
    if (keyed === null) {
      continue;
    }
    const key = keyed[1] ?? "";
    if (key.trim() === "") {
      problems.push(`${row.column}: the key is empty`);
    } else if ([...key].length > MAX_KEY_CHARS) {
      problems.push(`${row.column}: the key is longer than ${MAX_KEY_CHARS} characters`);
    }
  }
  return problems;
}

const FIELD_SEPARATOR = " · ";

/**
 * The lines of `text` joined into one, whitespace collapsed, with the Markdown markers that start
 * a line (headings, quotes, list bullets) removed, so a Markdown prompt previews by its words.
 */
function oneLine(text: string): string {
  return text
    .split(/\r?\n/)
    .map((line) => line.replace(/^\s*(?:#{1,6}\s+|>\s*|[-*+]\s+|\d+[.)]\s+)/, "").trim())
    .filter((line) => line.length > 0)
    .join(" ")
    .replace(/\s+/g, " ")
    .trim();
}

/** `text` cut to at most `max` characters, the last one being "…" when it is cut. */
function cutText(text: string, max: number): string {
  return text.length > max ? `${text.slice(0, Math.max(0, max - 1))}…` : text;
}

type Message = Record<string, unknown> & { role: string };

/** A non-empty array whose every element is an object with a string `role`. */
function isMessages(value: unknown[]): value is Message[] {
  return (
    value.length > 0 &&
    value.every(
      (item) =>
        typeof item === "object" &&
        item !== null &&
        !Array.isArray(item) &&
        typeof (item as { role?: unknown }).role === "string",
    )
  );
}

/** The content of the last user message, else of the last message with string content; none gives undefined. */
function messagesContent(messages: Message[]): string | undefined {
  for (let index = messages.length - 1; index >= 0; index--) {
    const message = messages[index];
    if (message?.role === "user" && typeof message.content === "string") {
      return message.content;
    }
  }
  for (let index = messages.length - 1; index >= 0; index--) {
    const content = messages[index]?.content;
    if (typeof content === "string") {
      return content;
    }
  }
  return undefined;
}

/** A value inside a top-level object: a string is its first line (quoted when empty or containing " · "). */
function fieldText(value: unknown): string {
  if (value === undefined) {
    return "—";
  }
  if (typeof value === "string") {
    const line = oneLine(value);
    return line === "" || line.includes(FIELD_SEPARATOR) ? `"${line}"` : line;
  }
  if (value === null || typeof value !== "object") {
    return JSON.stringify(value);
  }
  if (Array.isArray(value)) {
    if (isMessages(value)) {
      const content = messagesContent(value);
      return content === undefined ? `[${value.length}]` : fieldText(content);
    }
    return `[${value.length}]`;
  }
  return Object.keys(value).length === 0 ? "{}" : "{…}";
}

/** A top-level value, before the cut. */
function previewTop(value: unknown): string {
  if (value === undefined || value === null) {
    return "—";
  }
  if (typeof value === "string") {
    const line = oneLine(value);
    return line === "" ? "—" : line;
  }
  if (typeof value !== "object") {
    return JSON.stringify(value);
  }
  if (Array.isArray(value)) {
    if (isMessages(value)) {
      const content = messagesContent(value);
      return content === undefined ? `[${value.length}]` : previewTop(content);
    }
    if (value.length === 0) {
      return "[0]";
    }
    return `[${value.length}] ${previewTop(value[0])}`;
  }
  const fields = Object.entries(value).filter(([, member]) => member !== undefined);
  if (fields.length === 0) {
    return "{}";
  }
  return fields.map(([key, member]) => `${key}: ${fieldText(member)}`).join(FIELD_SEPARATOR);
}

/**
 * A one-line preview of a dataset item's value, at most `max` characters (the "…" counts).
 * A string shows its first line; an object shows `key: value` pairs joined by " · " (only the top
 * level is expanded: nested objects are `{…}`, nested arrays `[n]`); a chat array (objects with a
 * string `role`) shows the content of its last user message, the last string otherwise, else `[n]`;
 * another array shows `[n]` and its first element; null and undefined show "—".
 */
export function previewFields(value: unknown, max = 160): string {
  return cutText(previewTop(value), max);
}

/**
 * The short form of a dataset item or revision id: its last 8 hex digits. UUIDv7 ids made in the
 * same millisecond (an import, a batch add) share their first 8, so `shortId` would repeat.
 */
export function itemShortId(id: string): string {
  return id.replace(/-/g, "").slice(-8);
}
