/**
 * `Prompt` and `PromptNotAvailable` (D-V-14, D-V-15), in a leaf module: it
 * imports the template renderer, the logger and OTel only, so `context.ts`
 * and `prompts.ts` can both import it without a cycle.
 */

import { context, trace } from "@opentelemetry/api";

import { debug } from "./log.js";
import {
  type ChatEntry,
  type ChatMessage,
  chatNames,
  PromptCompileError,
  render,
  renderChat,
  variables,
} from "./template.js";

/**
 * The values of `Prompt.compile`. Strings, numbers and booleans fill
 * `{{ name }}` (a boolean is `true` or `false`; a finite number is its
 * shortest text, so `1.0` is `1`; NaN and infinities are a `TypeError`); a
 * list of `{role, content}` fills a placeholder.
 */
export type PromptValues = Readonly<
  Record<string, string | number | boolean | readonly ChatMessage[]>
>;

/** No prompt could be fetched, and the call gave no fallback (D-V-14). */
export class PromptNotAvailable extends Error {
  readonly prompt: string;
  readonly reason: string;

  constructor(prompt: string, reason: string) {
    super(`prompt ${prompt} is not available: ${reason}`);
    this.name = "PromptNotAvailable";
    this.prompt = prompt;
    this.reason = reason;
  }
}

/** The fields of a `Prompt`. Internal. */
export interface PromptFields {
  readonly name: string;
  readonly version: number | undefined;
  readonly config: Readonly<Record<string, unknown>>;
  readonly labels: readonly string[];
  readonly isFallback: boolean;
  /** A string is a text prompt; a list is a chat prompt. */
  readonly template: string | readonly ChatEntry[];
}

/** A prompt version from the registry, or a fallback (D-V-14). */
export class Prompt {
  readonly name: string;
  readonly type: "text" | "chat";
  readonly template: string | readonly ChatEntry[];
  /** `undefined` for a fallback. */
  readonly version: number | undefined;
  readonly config: Readonly<Record<string, unknown>>;
  readonly labels: readonly string[];
  readonly variables: readonly string[];
  readonly placeholders: readonly string[];
  readonly isFallback: boolean;

  /**
   * Throws `PromptCompileError` when the template is invalid.
   * @internal
   */
  constructor(fields: PromptFields) {
    this.name = fields.name;
    this.version = fields.version;
    this.config = Object.freeze({ ...fields.config });
    this.labels = Object.freeze([...fields.labels]);
    this.isFallback = fields.isFallback;
    if (typeof fields.template === "string") {
      this.type = "text";
      this.template = fields.template;
      this.variables = variables(fields.template);
      this.placeholders = [];
    } else {
      const names = chatNames(fields.template);
      this.type = "chat";
      this.template = Object.freeze([...fields.template]);
      this.variables = names.variables;
      this.placeholders = names.placeholders;
    }
  }

  /**
   * Renders the template with `values`. A missing variable or placeholder, or
   * an invalid template, throws `PromptCompileError` carrying the prompt name;
   * a value of another type throws `TypeError`. On success the current span,
   * when it records, gets `cetra.prompt.name` and `cetra.prompt.version`.
   */
  compile(values: PromptValues = {}): string | ChatMessage[] {
    const strings: [string, string][] = [];
    const lists: [string, readonly ChatMessage[]][] = [];
    for (const [key, value] of Object.entries(values)) {
      if (typeof value === "string") {
        strings.push([key, value]);
      } else if (typeof value === "number") {
        if (!Number.isFinite(value)) {
          throw new TypeError(`prompt value ${key}: not a finite number`);
        }
        strings.push([key, String(value)]);
      } else if (typeof value === "boolean") {
        strings.push([key, value ? "true" : "false"]);
      } else if (isMessageList(value)) {
        lists.push([key, value]);
      } else {
        throw new TypeError(`prompt value ${key}: unsupported type`);
      }
    }
    const stringValues = Object.fromEntries(strings);
    const listValues = Object.fromEntries(lists);
    let rendered: string | ChatMessage[];
    try {
      rendered =
        typeof this.template === "string"
          ? render(this.template, stringValues)
          : renderChat(this.template, stringValues, listValues);
    } catch (error) {
      if (error instanceof PromptCompileError) {
        throw new PromptCompileError(error.kind, {
          variable: error.variable,
          reason: error.reason,
          line: error.line,
          column: error.column,
          prompt: this.name,
        });
      }
      throw error;
    }
    this.#link();
    return rendered;
  }

  /** Stamps the prompt on the active span when it records. Never throws. */
  #link(): void {
    try {
      const span = trace.getSpan(context.active());
      if (span?.isRecording()) {
        span.setAttribute("cetra.prompt.name", this.name);
        if (this.version !== undefined) {
          span.setAttribute("cetra.prompt.version", this.version);
        }
      }
    } catch (error) {
      debug(`cetra: could not link the prompt (${errorName(error)})`);
    }
  }
}

/** A list of `{role, content}` string mappings: the value of a placeholder. */
function isMessageList(value: unknown): value is readonly ChatMessage[] {
  return (
    Array.isArray(value) &&
    value.every(
      (item: unknown) =>
        typeof item === "object" &&
        item !== null &&
        typeof (item as { role?: unknown }).role === "string" &&
        typeof (item as { content?: unknown }).content === "string",
    )
  );
}

function errorName(error: unknown): string {
  return error instanceof Error ? error.name : typeof error;
}
