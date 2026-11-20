import { isProblem } from "@/api/problem";
import { reportUnauthorized } from "@/api/session";
import type { Schema } from "@/api/types";
import { parseJsonField, prettyJson } from "@/lib/datasets";
import { asChat, type ChatEntry } from "@/lib/prompts";
import { uuidv7 } from "@/lib/uuid";
import { STORAGE_PREFIX } from "@/product";

export type Role = "system" | "user" | "assistant" | "tool";

/** A message part. Tool parts may lack an `id` here; the server refuses them without one. */
export type Part =
  | { type: "text"; content: string }
  | { type: "tool_call"; id?: string; name: string; arguments?: unknown }
  | { type: "tool_call_response"; id?: string; response: unknown }
  | { type: string; [key: string]: unknown };

export type Message = { role: Role; parts: Part[] };
export type Entry = Message | { type: "placeholder"; name: string };

export type Column = {
  id: string;
  connectionId: string | undefined;
  model: string;
  /** Kept as typed text; `""` = unset. */
  temperature: string;
  /** `""` = unset. */
  maxTokens: string;
};

export type Draft = {
  entries: Entry[];
  variables: Record<string, string>;
  /** JSON text of a `Message[]` per placeholder name. */
  placeholders: Record<string, string>;
  /** JSON text of `[{name, description?, parameters}]`; `""` = none. */
  tools: string;
  /** `""` | `auto` | `none` | `required` | a tool name. */
  toolChoice: string;
  /** JSON text of `{name, schema, strict?}`; `""` = none. */
  responseFormat: string;
  /** 1 to 4 columns. */
  columns: Column[];
  source?:
    | { prompt: { name: string; version: number } }
    | { span: { trace_id: string; span_id: string } };
  /** Whether `{{ name }}` in the text parts is rendered from `variables` (D-V-38). */
  renderVariables: boolean;
};

export type Usage = {
  input: number;
  output: number;
  cache_read: number;
  cache_write: number;
  reasoning: number;
};

export type RunEvent =
  | { event: "start"; data: { trace_id: string } }
  | { event: "text"; data: { delta: string } }
  | { event: "tool_call"; data: { id: string; name: string; arguments: unknown } }
  | {
      event: "done";
      data: {
        finish_reason: string | null;
        usage: Usage | null;
        cost_usd: number | null;
        latency_ms: number;
        ttft_ms: number | null;
        output: Message | null;
      };
    }
  | { event: "error"; data: { class: "retryable" | "pause" | "permanent"; message: string } };

/** A tool as the run request takes it. */
export type RequestTool = { name: string; description?: string; parameters: unknown };

/** The body of `POST …/playground/runs` (spec 09 § Playground). */
export type RunRequest = {
  connection_id: string;
  model: string;
  messages: Entry[];
  variables: Record<string, string>;
  placeholders: Record<string, Message[]>;
  render_variables: boolean;
  params?: { temperature?: number; max_tokens?: number };
  tools?: RequestTool[];
  tool_choice?: string;
  response_format?: { name: string; schema: unknown; strict?: boolean };
  source?: Draft["source"];
};

const ROLES: readonly Role[] = ["system", "user", "assistant", "tool"];
const TOOL_NAME = /^[A-Za-z0-9_-]{1,64}$/;
/** A `{{ … }}` tag; the inner text is checked by `VARIABLE_NAME`. */
const VARIABLE_TAG = /\{\{([^{}]*)\}\}/g;
const VARIABLE_NAME = /^[A-Za-z_][A-Za-z0-9_]*$/;
const WHOLE_NUMBER = /^\d+$/;
const TEMPERATURE_MAX = 2;
const MAX_TOKENS_MAX = 65_536;
const COLUMNS_MIN = 1;
const COLUMNS_MAX = 4;
const KNOWN_EVENTS: ReadonlySet<string> = new Set(["start", "text", "tool_call", "done", "error"]);

/** Reads `text/event-stream` bytes as events. `data` is the joined `data:` lines; comments and `ping` are skipped. */
export async function* readSse(
  body: ReadableStream<Uint8Array>,
): AsyncGenerator<{ event: string; data: string }> {
  const reader = body.getReader();
  const decoder = new TextDecoder();
  let pending = "";
  let eventName: string | undefined;
  let dataLines: string[] = [];
  try {
    for (;;) {
      const chunk = await reader.read();
      pending += decoder.decode(chunk.value, { stream: !chunk.done });
      const { lines, rest } = splitLines(pending, chunk.done);
      pending = rest;
      for (const line of lines) {
        if (line === "") {
          // A blank line ends the event. A pending event without one at the end is dropped.
          if (dataLines.length > 0 && eventName !== "ping") {
            yield { event: eventName ?? "message", data: dataLines.join("\n") };
          }
          eventName = undefined;
          dataLines = [];
        } else if (!line.startsWith(":")) {
          const colon = line.indexOf(":");
          const field = colon === -1 ? line : line.slice(0, colon);
          let text = colon === -1 ? "" : line.slice(colon + 1);
          if (text.startsWith(" ")) {
            text = text.slice(1);
          }
          if (field === "event") {
            eventName = text;
          } else if (field === "data") {
            dataLines.push(text);
          }
        }
      }
      if (chunk.done) {
        return;
      }
    }
  } finally {
    reader.cancel().catch(() => undefined);
  }
}

/**
 * Splits complete lines off `text`. A line ends at `\r\n`, `\n` or `\r`. A `\r` at the very end is
 * held back (unless `final`), since a `\n` may follow it in the next chunk.
 */
function splitLines(text: string, final: boolean): { lines: string[]; rest: string } {
  const lines: string[] = [];
  let start = 0;
  let index = 0;
  while (index < text.length) {
    const char = text[index];
    if (char === "\r" && index + 1 === text.length && !final) {
      break;
    }
    if (char === "\n" || char === "\r") {
      lines.push(text.slice(start, index));
      index += char === "\r" && text[index + 1] === "\n" ? 2 : 1;
      start = index;
    } else {
      index += 1;
    }
  }
  return { lines, rest: text.slice(start) };
}

function isAbortError(error: unknown): boolean {
  return (
    typeof error === "object" && error !== null && "name" in error && error.name === "AbortError"
  );
}

/**
 * POSTs one run. Before the stream: a non-2xx answer throws the parsed problem (a 401 also calls
 * `reportUnauthorized`). Then calls `onEvent` for each event until the stream ends. Aborting
 * `signal` cancels: the promise resolves without an error.
 */
export async function runPlayground(
  project: string,
  body: RunRequest,
  options: { signal: AbortSignal; onEvent: (event: RunEvent) => void },
): Promise<void> {
  const url = `${window.location.origin}/api/v1/projects/${encodeURIComponent(project)}/playground/runs`;
  try {
    const response = await fetch(url, {
      method: "POST",
      credentials: "same-origin",
      headers: { "Content-Type": "application/json", Accept: "text/event-stream" },
      body: JSON.stringify(body),
      signal: options.signal,
    });
    if (!response.ok) {
      if (response.status === 401) {
        reportUnauthorized(new URL(url).pathname);
      }
      let problem: unknown;
      try {
        problem = await response.json();
      } catch {
        problem = undefined;
      }
      if (isProblem(problem)) {
        throw problem;
      }
      throw new Error(`HTTP ${response.status}`);
    }
    if (response.body === null) {
      throw new Error("The run has no stream");
    }
    for await (const event of readSse(response.body)) {
      if (!KNOWN_EVENTS.has(event.event)) {
        continue;
      }
      let data: unknown;
      try {
        data = JSON.parse(event.data);
      } catch {
        continue;
      }
      options.onEvent({ event: event.event, data } as RunEvent);
    }
  } catch (error) {
    if (options.signal.aborted || isAbortError(error)) {
      return;
    }
    throw error;
  }
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function isRole(value: unknown): value is Role {
  return ROLES.some((role) => role === value);
}

function isPlaceholder(entry: Entry): entry is { type: "placeholder"; name: string } {
  return "type" in entry && entry.type === "placeholder";
}

function hasToolId(part: Part): boolean {
  return "id" in part && typeof part.id === "string" && part.id !== "";
}

/** The text of a `text` part, or `undefined` for any other part (or a text part without a string). */
function textOf(part: Part): string | undefined {
  return part.type === "text" && "content" in part && typeof part.content === "string"
    ? part.content
    : undefined;
}

function placeholderNames(entries: readonly Entry[]): string[] {
  const names: string[] = [];
  for (const entry of entries) {
    if (isPlaceholder(entry) && !names.includes(entry.name)) {
      names.push(entry.name);
    }
  }
  return names;
}

/** The kinds of part a message holds that the server refuses. Empty when the message is sendable. */
function refusedKinds(message: Message): string[] {
  const kinds: string[] = [];
  const refuse = (kind: string) => {
    if (!kinds.includes(kind)) {
      kinds.push(kind);
    }
  };
  for (const part of message.parts) {
    if (part.type === "text") {
      if (message.role === "tool") {
        refuse("text");
      }
    } else if (part.type === "reasoning") {
      // Kept in the draft and sent; the server accepts it and does not forward it.
    } else if (part.type === "tool_call") {
      if (message.role !== "assistant" || !hasToolId(part)) {
        refuse("tool_call");
      }
    } else if (part.type === "tool_call_response") {
      if (message.role !== "tool" || !hasToolId(part)) {
        refuse("tool_call_response");
      }
    } else {
      // Media (`uri`, `blob`) and unknown part types.
      refuse(part.type);
    }
  }
  return kinds;
}

/** `{{ name }}` names in order of first use across the text parts (same rule as the SDKs; invalid tags are skipped). */
export function draftVariables(draft: Draft): string[] {
  if (!draft.renderVariables) {
    return [];
  }
  const names: string[] = [];
  for (const entry of draft.entries) {
    if (isPlaceholder(entry)) {
      continue;
    }
    for (const part of entry.parts) {
      const content = textOf(part);
      if (content === undefined) {
        continue;
      }
      for (const match of content.matchAll(VARIABLE_TAG)) {
        const name = (match[1] ?? "").trim();
        if (VARIABLE_NAME.test(name) && !names.includes(name)) {
          names.push(name);
        }
      }
    }
  }
  return names;
}

/**
 * Messages (1-based numbers, counting every entry) holding parts the server refuses: media and
 * unknown parts, text in a `tool` message, a tool call outside an `assistant` message, a tool
 * response outside a `tool` message, and a tool part without an id. `kinds` names the part types.
 */
export function blockedMessages(draft: Draft): { index: number; kinds: string[] }[] {
  const blocked: { index: number; kinds: string[] }[] = [];
  draft.entries.forEach((entry, position) => {
    if (isPlaceholder(entry)) {
      return;
    }
    const kinds = refusedKinds(entry);
    if (kinds.length > 0) {
      blocked.push({ index: position + 1, kinds });
    }
  });
  return blocked;
}

function isMessageShape(value: unknown): value is Message {
  return isRecord(value) && typeof value.role === "string" && Array.isArray(value.parts);
}

/** The tools of the draft, or `[]`. A problem is recorded under `tools`. */
function requestTools(text: string, errors: Record<string, string>): RequestTool[] {
  const parsed = parseJsonField(text, { required: false });
  if (!parsed.ok) {
    errors.tools = parsed.message;
    return [];
  }
  if (parsed.value === undefined) {
    return [];
  }
  if (!Array.isArray(parsed.value)) {
    errors.tools = "Must be a JSON list of tools";
    return [];
  }
  const tools: RequestTool[] = [];
  for (const item of parsed.value) {
    const name: unknown = isRecord(item) ? item.name : undefined;
    if (!isRecord(item) || typeof name !== "string" || !TOOL_NAME.test(name)) {
      errors.tools = "Each tool needs a name of 1 to 64 letters, digits, _ or -";
      return [];
    }
    if (item.parameters === undefined) {
      errors.tools = "Each tool needs parameters";
      return [];
    }
    const description = item.description;
    tools.push({
      name,
      ...(typeof description === "string" ? { description } : {}),
      parameters: item.parameters,
    });
  }
  return tools;
}

/** The response format of the draft, or `undefined`. A problem is recorded under `responseFormat`. */
function requestResponseFormat(
  text: string,
  errors: Record<string, string>,
): RunRequest["response_format"] {
  const parsed = parseJsonField(text, { required: false, object: true });
  if (!parsed.ok) {
    errors.responseFormat = parsed.message;
    return undefined;
  }
  if (parsed.value === undefined) {
    return undefined;
  }
  const value = parsed.value;
  const name = isRecord(value) ? value.name : undefined;
  const strict = isRecord(value) ? value.strict : undefined;
  if (
    !isRecord(value) ||
    typeof name !== "string" ||
    !TOOL_NAME.test(name) ||
    value.schema === undefined ||
    (strict !== undefined && typeof strict !== "boolean")
  ) {
    errors.responseFormat =
      "Must be an object with a name of 1 to 64 letters, digits, _ or -, a schema and an optional strict flag";
    return undefined;
  }
  return {
    name,
    schema: value.schema,
    ...(strict === undefined ? {} : { strict }),
  };
}

/** The placeholders the draft's entries name, as messages. Problems are recorded under `placeholders.<name>`. */
function requestPlaceholders(
  draft: Draft,
  errors: Record<string, string>,
): Record<string, Message[]> {
  const result: Record<string, Message[]> = {};
  for (const name of placeholderNames(draft.entries)) {
    const text = draft.placeholders[name] ?? "";
    if (text.trim() === "") {
      result[name] = [];
      continue;
    }
    const parsed = parseJsonField(text, { required: false });
    if (!parsed.ok) {
      errors[`placeholders.${name}`] = parsed.message;
    } else if (!Array.isArray(parsed.value) || !parsed.value.every(isMessageShape)) {
      errors[`placeholders.${name}`] = "Must be a JSON list of messages";
    } else {
      result[name] = parsed.value;
    }
  }
  return result;
}

/**
 * Builds a column's request. Returns field errors instead when the draft cannot run as the column
 * is. Keys are unindexed: `model`, `connection`, `temperature`, `maxTokens`, `tools`,
 * `responseFormat`, `placeholders.<name>`, and `messages` (the first refused message).
 */
export function toRunRequest(
  draft: Draft,
  column: Column,
  connections: readonly Pick<Schema<"LlmConnection">, "id" | "default_model">[],
): { ok: true; request: RunRequest } | { ok: false; errors: Record<string, string> } {
  const errors: Record<string, string> = {};
  const connection = connections.find((item) => item.id === column.connectionId);
  if (connection === undefined) {
    errors.connection = "Choose a connection";
  }
  const model = column.model.trim() || connection?.default_model || "";
  if (model === "") {
    errors.model = "Choose a model";
  }

  let temperature: number | undefined;
  const temperatureText = column.temperature.trim();
  if (temperatureText !== "") {
    const value = Number(temperatureText);
    if (Number.isFinite(value) && value >= 0 && value <= TEMPERATURE_MAX) {
      temperature = value;
    } else {
      errors.temperature = "Must be a number from 0 to 2";
    }
  }
  let maxTokens: number | undefined;
  const maxTokensText = column.maxTokens.trim();
  if (maxTokensText !== "") {
    const value = Number(maxTokensText);
    if (WHOLE_NUMBER.test(maxTokensText) && value >= 1 && value <= MAX_TOKENS_MAX) {
      maxTokens = value;
    } else {
      errors.maxTokens = "Must be a whole number from 1 to 65536";
    }
  }

  const tools = requestTools(draft.tools, errors);
  const responseFormat = requestResponseFormat(draft.responseFormat, errors);
  const placeholders = requestPlaceholders(draft, errors);
  const blocked = blockedMessages(draft)[0];
  if (blocked !== undefined) {
    errors.messages = `Message ${blocked.index} has ${blocked.kinds.join(", ")} parts, which the playground cannot send. Remove them to run.`;
  }
  if (Object.keys(errors).length > 0) {
    return { ok: false, errors };
  }

  const variables: Record<string, string> = {};
  for (const name of draftVariables(draft)) {
    variables[name] = draft.variables[name] ?? "";
  }
  const params: NonNullable<RunRequest["params"]> = {
    ...(temperature === undefined ? {} : { temperature }),
    ...(maxTokens === undefined ? {} : { max_tokens: maxTokens }),
  };
  const toolChoice = draft.toolChoice;
  const sendToolChoice = toolChoice !== "" && (tools.length > 0 || toolChoice === "none");
  const request: RunRequest = {
    connection_id: column.connectionId ?? "",
    model,
    messages: draft.entries,
    variables,
    placeholders,
    render_variables: draft.renderVariables,
    ...(Object.keys(params).length > 0 ? { params } : {}),
    ...(tools.length > 0 ? { tools } : {}),
    ...(sendToolChoice ? { tool_choice: toolChoice } : {}),
    ...(responseFormat === undefined ? {} : { response_format: responseFormat }),
    ...(draft.source === undefined ? {} : { source: draft.source }),
  };
  return { ok: true, request };
}

/**
 * The text of a message's parts joined with a newline, or `undefined` when a part is not text.
 * Reasoning parts are dropped, not refused (Precision 4).
 */
function plainText(message: Message): string | undefined {
  const texts: string[] = [];
  for (const part of message.parts) {
    if (part.type === "reasoning") {
      continue;
    }
    const content = textOf(part);
    if (content === undefined) {
      return undefined;
    }
    texts.push(content);
  }
  return texts.join("\n");
}

/**
 * The draft's messages as a chat template for "Save as new version": text-only messages become
 * `{role, content}`, placeholders stay. A message with other parts, or with role `tool`, makes the
 * result `blocked`: the 1-based number of the first such entry.
 */
export function toChatTemplate(
  draft: Draft,
): { ok: true; template: ChatEntry[] } | { ok: false; blocked: number } {
  const template: ChatEntry[] = [];
  for (const [position, entry] of draft.entries.entries()) {
    if (isPlaceholder(entry)) {
      template.push({ type: "placeholder", name: entry.name });
      continue;
    }
    const role = entry.role;
    const content = plainText(entry);
    if (role === "tool" || content === undefined) {
      return { ok: false, blocked: position + 1 };
    }
    template.push({ role, content });
  }
  return { ok: true, template };
}

/** The tool definitions of a span: an array, or a JSON string of one (Precision 21). */
function toolDefinitionsOf(value: unknown): { tools: RequestTool[]; dropped: number } {
  let list: unknown = value;
  if (typeof value === "string") {
    try {
      list = JSON.parse(value);
    } catch {
      return { tools: [], dropped: 1 };
    }
  }
  if (list === undefined || list === null) {
    return { tools: [], dropped: 0 };
  }
  if (!Array.isArray(list)) {
    return { tools: [], dropped: 1 };
  }
  const tools: RequestTool[] = [];
  let dropped = 0;
  for (const item of list) {
    const tool = toolOf(item);
    if (tool === undefined) {
      dropped += 1;
    } else {
      tools.push(tool);
    }
  }
  return { tools, dropped };
}

/** A tool in `{type: "function", function: {…}}` or flat form, with `parameters`, `input_schema` or `inputSchema`. */
function toolOf(item: unknown): RequestTool | undefined {
  if (!isRecord(item)) {
    return undefined;
  }
  const flat = item.type === "function" && isRecord(item.function) ? item.function : item;
  const name = flat.name;
  if (typeof name !== "string" || name === "") {
    return undefined;
  }
  const parameters = [flat.parameters, flat.input_schema, flat.inputSchema].find(
    (candidate) => candidate !== undefined,
  );
  if (parameters === undefined) {
    return undefined;
  }
  const description = flat.description;
  return {
    name,
    ...(typeof description === "string" ? { description } : {}),
    parameters,
  };
}

/** How many of a span's tool definitions could not be converted to a draft tool (Precision 21). */
export function droppedToolDefinitions(span: Schema<"SpanDetail">): number {
  return toolDefinitionsOf(span.tool_definitions).dropped;
}

/** A prompt version as a draft (D-V-28). */
export function draftFromPromptVersion(
  version: Schema<"PromptVersion">,
  fallbackConnectionId: string | undefined,
): Draft {
  const config = version.config;
  const params: Record<string, unknown> = isRecord(config.params) ? config.params : {};
  const entries: Entry[] =
    version.type === "chat"
      ? asChat(version.template).map(
          (entry): Entry =>
            "type" in entry
              ? { type: "placeholder", name: entry.name }
              : textMessage(entry.role, entry.content),
        )
      : [textMessage("user", typeof version.template === "string" ? version.template : "")];
  const placeholders: Record<string, string> = {};
  for (const name of placeholderNames(entries)) {
    placeholders[name] = "";
  }
  const connectionId = typeof config.connection_id === "string" ? config.connection_id : undefined;
  return {
    entries,
    variables: {},
    placeholders,
    tools: prettyOrBlank(config.tools),
    toolChoice: typeof config.tool_choice === "string" ? config.tool_choice : "",
    responseFormat: prettyOrBlank(config.response_format),
    columns: [
      newColumn(
        connectionId ?? fallbackConnectionId,
        typeof config.model === "string" ? config.model : "",
        numberText(params.temperature),
        numberText(params.max_tokens),
      ),
    ],
    source: { prompt: { name: version.name, version: version.version } },
    renderVariables: true,
  };
}

/** A span as a draft (D-V-28): its system instructions, messages, tools and request parameters. */
export function draftFromSpan(
  span: Schema<"SpanDetail">,
  fallbackConnectionId: string | undefined,
): Draft {
  const entries: Entry[] = [];
  const system = systemText(span.system_instructions);
  if (system !== undefined) {
    entries.push(textMessage("system", system));
  }
  const input = span.input;
  if (input !== undefined) {
    if (input.format === "messages" && Array.isArray(input.value)) {
      for (const item of input.value) {
        entries.push(messageOf(item));
      }
    } else if (input.format === "text" && typeof input.value === "string") {
      entries.push(textMessage("user", input.value));
    } else if (input.format === "json") {
      entries.push(textMessage("user", JSON.stringify(input.value, null, 2)));
    }
  }
  const tools = toolDefinitionsOf(span.tool_definitions).tools;
  const requestParams = parsedObject(span.request_params);
  return {
    entries,
    variables: {},
    placeholders: {},
    tools: tools.length > 0 ? prettyJson(tools) : "",
    toolChoice: "",
    responseFormat: "",
    columns: [
      newColumn(
        fallbackConnectionId,
        span.request_model ?? span.model ?? "",
        numberText(requestParams.temperature),
        numberText(requestParams.max_tokens),
      ),
    ],
    source: { span: { trace_id: span.trace_id, span_id: span.span_id } },
    renderVariables: false,
  };
}

/** System instructions: a string as it is, a list of text parts joined by newlines, else JSON. */
function systemText(value: unknown): string | undefined {
  if (value === undefined || value === null || value === "") {
    return undefined;
  }
  if (typeof value === "string") {
    return value;
  }
  if (Array.isArray(value)) {
    const texts = value.flatMap((part) =>
      isRecord(part) && part.type === "text" && typeof part.content === "string"
        ? [part.content]
        : [],
    );
    if (texts.length === value.length && texts.length > 0) {
      return texts.join("\n");
    }
  }
  return JSON.stringify(value, null, 2);
}

/** A span's message as a draft message: an unknown role becomes `user`, and only object parts are kept. */
function messageOf(item: unknown): Message {
  const record: Record<string, unknown> = isRecord(item) ? item : {};
  const role: Role = isRole(record.role) ? record.role : "user";
  const parts = Array.isArray(record.parts)
    ? record.parts.filter((part): part is Part => isRecord(part) && typeof part.type === "string")
    : [];
  return { role, parts };
}

/** A JSON object given as an object or a JSON string; anything else is `{}`. */
function parsedObject(value: unknown): Record<string, unknown> {
  let parsed: unknown = value;
  if (typeof value === "string") {
    try {
      parsed = JSON.parse(value);
    } catch {
      return {};
    }
  }
  return isRecord(parsed) ? parsed : {};
}

function numberText(value: unknown): string {
  return typeof value === "number" && Number.isFinite(value) ? String(value) : "";
}

function prettyOrBlank(value: unknown): string {
  return value === undefined || value === null ? "" : prettyJson(value);
}

function textMessage(role: Role, content: string): Message {
  return { role, parts: [{ type: "text", content }] };
}

function newColumn(
  connectionId: string | undefined,
  model = "",
  temperature = "",
  maxTokens = "",
): Column {
  return { id: uuidv7(), connectionId, model, temperature, maxTokens };
}

/** An empty draft: a system and a user message, one column. */
export function emptyDraft(connectionId: string | undefined): Draft {
  return {
    entries: [textMessage("system", ""), textMessage("user", "")],
    variables: {},
    placeholders: {},
    tools: "",
    toolChoice: "",
    responseFormat: "",
    columns: [newColumn(connectionId)],
    renderVariables: true,
  };
}

// Drafts are kept per project in localStorage. When storage is blocked or full, a copy stays in
// memory for this page's life (pattern of recent-items.ts).
const memory = new Map<string, string>();

function draftKey(projectKey: string): string {
  return `${STORAGE_PREFIX}playground.${projectKey}`;
}

function readRaw(key: string): string | null {
  try {
    const stored = localStorage.getItem(key);
    if (stored !== null) {
      return stored;
    }
  } catch {
    // Storage can be blocked: the in-memory copy serves.
  }
  return memory.get(key) ?? null;
}

function writeRaw(key: string, raw: string): void {
  try {
    localStorage.setItem(key, raw);
    memory.delete(key);
  } catch {
    // Private mode or a full quota: the in-memory copy serves.
    memory.set(key, raw);
  }
}

/** The draft of a project, or `undefined` when none is stored or the stored value does not fit. */
export function loadDraft(projectKey: string): Draft | undefined {
  const raw = readRaw(draftKey(projectKey));
  if (raw === null) {
    return undefined;
  }
  try {
    return draftOf(JSON.parse(raw));
  } catch {
    return undefined;
  }
}

export function saveDraft(projectKey: string, draft: Draft): void {
  writeRaw(draftKey(projectKey), JSON.stringify(draft));
}

function draftOf(value: unknown): Draft | undefined {
  if (!isRecord(value)) {
    return undefined;
  }
  const { entries, variables, placeholders, tools, toolChoice, responseFormat, columns } = value;
  const renderVariables = value.renderVariables;
  if (!Array.isArray(entries) || !entries.every(isEntry)) {
    return undefined;
  }
  if (!isStringRecord(variables) || !isStringRecord(placeholders)) {
    return undefined;
  }
  if (
    typeof tools !== "string" ||
    typeof toolChoice !== "string" ||
    typeof responseFormat !== "string"
  ) {
    return undefined;
  }
  if (
    !Array.isArray(columns) ||
    columns.length < COLUMNS_MIN ||
    columns.length > COLUMNS_MAX ||
    !columns.every(isColumn)
  ) {
    return undefined;
  }
  if (renderVariables !== undefined && typeof renderVariables !== "boolean") {
    return undefined;
  }
  const source = sourceOf(value.source);
  return {
    entries,
    variables,
    placeholders,
    tools,
    toolChoice,
    responseFormat,
    columns,
    renderVariables: renderVariables ?? true,
    ...(source === undefined ? {} : { source }),
  };
}

function isEntry(value: unknown): value is Entry {
  if (!isRecord(value)) {
    return false;
  }
  if (value.type === "placeholder") {
    return typeof value.name === "string";
  }
  return isRole(value.role) && Array.isArray(value.parts) && value.parts.every(isRecord);
}

function isStringRecord(value: unknown): value is Record<string, string> {
  return isRecord(value) && Object.values(value).every((item) => typeof item === "string");
}

function isColumn(value: unknown): value is Column {
  return (
    isRecord(value) &&
    typeof value.id === "string" &&
    (value.connectionId === undefined || typeof value.connectionId === "string") &&
    typeof value.model === "string" &&
    typeof value.temperature === "string" &&
    typeof value.maxTokens === "string"
  );
}

/** The source of a stored draft; a malformed one is dropped, the rest of the draft is kept. */
function sourceOf(value: unknown): Draft["source"] {
  if (!isRecord(value)) {
    return undefined;
  }
  const prompt = value.prompt;
  if (isRecord(prompt) && typeof prompt.name === "string" && typeof prompt.version === "number") {
    return { prompt: { name: prompt.name, version: prompt.version } };
  }
  const span = value.span;
  if (isRecord(span) && typeof span.trace_id === "string" && typeof span.span_id === "string") {
    return { span: { trace_id: span.trace_id, span_id: span.span_id } };
  }
  return undefined;
}
