/**
 * Plain `{{ name }}` prompt templates (D-V-3, D-V-5): the rules of
 * `fixtures/prompts`, the same as `crates/eval/src/plain_template.rs` and the
 * Python SDK's `_template.py`. Substitution only, no logic.
 */

/** Names minijinja reads as literals or globals; a variable cannot use them (D-V-3). */
export const RESERVED: ReadonlySet<string> = new Set([
  "true",
  "false",
  "none",
  "True",
  "False",
  "None",
  "and",
  "or",
  "not",
  "in",
  "is",
  "if",
  "else",
  "loop",
  "self",
  "range",
  "dict",
  "namespace",
  "debug",
]);

const NAME = /^[A-Za-z_][A-Za-z0-9_]*$/;

export type ChatMessage = { readonly role: string; readonly content: string };
export type ChatEntry = ChatMessage | { readonly type: "placeholder"; readonly name: string };

/**
 * A template is invalid, or a variable or placeholder has no value.
 *
 * `kind` is `invalid`, `missing_variable` or `missing_placeholder`. `variable`
 * is the missing name; `reason`, `line` and `column` describe an invalid
 * template. The message never quotes the template.
 */
export class PromptCompileError extends Error {
  readonly kind: "invalid" | "missing_variable" | "missing_placeholder";
  readonly variable: string | undefined;
  readonly reason: string | undefined;
  readonly line: number | undefined;
  readonly column: number | undefined;

  constructor(
    kind: "invalid" | "missing_variable" | "missing_placeholder",
    details: {
      variable?: string;
      reason?: string;
      line?: number;
      column?: number;
      prompt?: string;
    },
  ) {
    super(compileMessage(kind, details));
    this.name = "PromptCompileError";
    this.kind = kind;
    this.variable = details.variable;
    this.reason = details.reason;
    this.line = details.line;
    this.column = details.column;
  }
}

function compileMessage(
  kind: "invalid" | "missing_variable" | "missing_placeholder",
  details: { variable?: string; reason?: string; line?: number; column?: number; prompt?: string },
): string {
  let detail: string;
  if (kind === "missing_variable") {
    detail = `missing variable ${details.variable}`;
  } else if (kind === "missing_placeholder") {
    detail = `missing placeholder ${details.variable}`;
  } else if (details.line === undefined) {
    // A chat entry has no position: only its reason is known.
    detail = `invalid template: ${details.reason}`;
  } else {
    detail = `invalid template at line ${details.line}, column ${details.column}: ${details.reason}`;
  }
  return details.prompt === undefined ? detail : `prompt ${details.prompt}: ${detail}`;
}

type Segment =
  | { readonly kind: "text"; readonly text: string }
  | { readonly kind: "var"; readonly name: string };

function invalid(line: number, column: number, reason: string): PromptCompileError {
  return new PromptCompileError("invalid", { line, column, reason });
}

/** Removes leading and trailing spaces and tabs only (not a regex: linear on any input). */
function trimTags(text: string): string {
  let start = 0;
  let end = text.length;
  while (start < end && (text[start] === " " || text[start] === "\t")) {
    start += 1;
  }
  while (end > start && (text[end - 1] === " " || text[end - 1] === "\t")) {
    end -= 1;
  }
  return text.slice(start, end);
}

/**
 * Splits `source` into text and variables. Adjacent text is one segment; empty
 * text segments are not produced. Columns count code points.
 */
function scan(source: string): Segment[] {
  const segments: Segment[] = [];
  // Index (UTF-16 unit) where the pending literal text starts.
  let textStart = 0;
  let line = 1;
  let column = 1;
  let index = 0;
  while (index < source.length) {
    if (source[index] === "{") {
      const next = source[index + 1];
      if (next === "%" || next === "#") {
        throw invalid(line, column, "block");
      }
      if (next === "{") {
        const innerStart = index + 2;
        const relativeEnd = source.indexOf("}}", innerStart);
        if (relativeEnd < 0) {
          throw invalid(line, column, "unclosed");
        }
        const name = trimTags(source.slice(innerStart, relativeEnd));
        if (!NAME.test(name)) {
          throw invalid(line, column, "not_a_name");
        }
        if (RESERVED.has(name)) {
          throw invalid(line, column, "reserved");
        }
        if (textStart < index) {
          segments.push({ kind: "text", text: source.slice(textStart, index) });
        }
        segments.push({ kind: "var", name });
        // A valid tag holds only name characters, spaces and tabs: ASCII, no
        // newline, so every unit it spans advances the column by one.
        const after = relativeEnd + 2;
        column += after - index;
        textStart = after;
        index = after;
        continue;
      }
    }
    if (source[index] === "\n") {
      line += 1;
      column = 1;
      index += 1;
    } else {
      // One column per code point: a surrogate pair is two units.
      const width = (source.codePointAt(index) ?? 0) > 0xffff ? 2 : 1;
      column += 1;
      index += width;
    }
  }
  if (textStart < source.length) {
    segments.push({ kind: "text", text: source.slice(textStart) });
  }
  return segments;
}

/** The first variable of `segments` with no value, in template order, is an error. */
function checkVariables(
  segments: readonly Segment[],
  values: Readonly<Record<string, string>>,
): void {
  for (const segment of segments) {
    if (segment.kind === "var" && !Object.hasOwn(values, segment.name)) {
      throw new PromptCompileError("missing_variable", { variable: segment.name });
    }
  }
}

/** Joins the segments; values are inserted verbatim. Call `checkVariables` first. */
function fill(segments: readonly Segment[], values: Readonly<Record<string, string>>): string {
  let out = "";
  for (const segment of segments) {
    out += segment.kind === "text" ? segment.text : (values[segment.name] ?? "");
  }
  return out;
}

/** The variable names, in order of first appearance, without duplicates. */
export function variables(source: string): string[] {
  const seen = new Set<string>();
  for (const segment of scan(source)) {
    if (segment.kind === "var") {
      seen.add(segment.name);
    }
  }
  return [...seen];
}

/**
 * Renders `source`: each variable is replaced by its value, inserted verbatim
 * (never scanned again). Values not used by the template are ignored.
 */
export function render(source: string, values: Readonly<Record<string, string>>): string {
  const segments = scan(source);
  checkVariables(segments, values);
  return fill(segments, values);
}

type Entry =
  | { readonly kind: "message"; readonly role: string; readonly segments: Segment[] }
  | { readonly kind: "placeholder"; readonly name: string };

/**
 * Classifies one chat entry. A placeholder has `type === "placeholder"` and a
 * string `name`; any other entry must have string `role` and `content`.
 */
function classify(entry: unknown): Entry {
  if (typeof entry !== "object" || entry === null) {
    throw new PromptCompileError("invalid", { reason: "not_a_message" });
  }
  const fields = entry as { readonly [key: string]: unknown };
  if (fields.type === "placeholder" && typeof fields.name === "string") {
    // The tag-name rule of precision 24: trimmed of spaces and tabs, then an ASCII name.
    const name = trimTags(fields.name);
    if (!NAME.test(name)) {
      throw new PromptCompileError("invalid", { reason: "not_a_name" });
    }
    return { kind: "placeholder", name };
  }
  const role = fields.role;
  const content = fields.content;
  if (typeof role !== "string" || typeof content !== "string") {
    throw new PromptCompileError("invalid", { reason: "not_a_message" });
  }
  return { kind: "message", role, segments: scan(content) };
}

/** The variable and placeholder names of a chat template, each in order of first use. */
export function chatNames(template: readonly ChatEntry[]): {
  variables: string[];
  placeholders: string[];
} {
  const names = new Set<string>();
  const placeholders = new Set<string>();
  for (const entry of template.map(classify)) {
    if (entry.kind === "placeholder") {
      placeholders.add(entry.name);
    } else {
      for (const segment of entry.segments) {
        if (segment.kind === "var") {
          names.add(segment.name);
        }
      }
    }
  }
  return { variables: [...names], placeholders: [...placeholders] };
}

/**
 * Renders a chat template. Checks run in this order, each raising the first
 * failure: every entry is valid; every variable of every message has a value;
 * every placeholder is given. Messages render to `{role, content}`; a
 * placeholder becomes copies of the given messages, which are not rendered.
 */
export function renderChat(
  template: readonly ChatEntry[],
  values: Readonly<Record<string, string>>,
  placeholders: Readonly<Record<string, readonly ChatMessage[]>>,
): ChatMessage[] {
  const entries = template.map(classify);
  for (const entry of entries) {
    if (entry.kind === "message") {
      checkVariables(entry.segments, values);
    }
  }
  for (const entry of entries) {
    if (entry.kind === "placeholder" && !Object.hasOwn(placeholders, entry.name)) {
      throw new PromptCompileError("missing_placeholder", { variable: entry.name });
    }
  }
  const out: ChatMessage[] = [];
  for (const entry of entries) {
    if (entry.kind === "message") {
      out.push({ role: entry.role, content: fill(entry.segments, values) });
    } else {
      for (const message of placeholders[entry.name] ?? []) {
        out.push({ ...message });
      }
    }
  }
  return out;
}
