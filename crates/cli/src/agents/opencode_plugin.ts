// Managed by cetra (cetra agents setup opencode). Delete this file or run: cetra agents remove opencode
// cetra's OpenCode plugin (D-IN-29). It turns each OpenCode turn into GenAI spans and sends
// them to cetra as OTLP/HTTP JSON. It reads its endpoint, headers and content switch from
// the config file `cetra agents setup opencode` writes beside it, never from the
// environment: OpenCode's own exporter starts exporting as soon as the standard OTel
// endpoint variables are set (m7-opencode-plugin.md § 6.2). Without a config the plugin
// registers nothing and OpenCode runs unchanged. It imports only node:fs; it uses the
// globals fetch, crypto, AbortSignal and process.env (for the config path). Every handler
// swallows its errors, so a problem here never breaks OpenCode. One default export serves
// both builds: OpenCode v2 calls setup(context), OpenCode v1 calls server(input). Spans:
// invoke_agent per turn, chat <model> per model call, execute_tool <name> per tool call.
import { readFileSync } from "node:fs";

const SCOPE = "cetra-opencode";
const PLUGIN_VERSION = "1";
const FLUSH_TIMEOUT_MS = 5000;

type Attr = {
  key: string;
  value: { stringValue?: string; intValue?: string; arrayValue?: { values: { stringValue: string }[] } };
};
export type Config = { endpoint: string; headers: Record<string, string>; captureContent: boolean };

/** `<XDG_CONFIG_HOME or HOME/.config>/opencode/cetra-opencode.json`, or null when neither variable is set and non-empty. */
export function configPath(env: Record<string, string | undefined>): string | null {
  const xdg = env.XDG_CONFIG_HOME;
  const home = env.HOME;
  const base =
    xdg !== undefined && xdg !== "" ? xdg : home !== undefined && home !== "" ? home + "/.config" : null;
  return base === null ? null : base + "/opencode/cetra-opencode.json";
}

/** null unless `text` is a JSON object whose `endpoint`, trimmed, is a non-empty string. */
export function parseConfig(text: string): Config | null {
  let root: unknown;
  try {
    root = JSON.parse(text);
  } catch {
    return null;
  }
  if (root === null || typeof root !== "object" || Array.isArray(root)) return null;
  const raw = root as Record<string, unknown>;
  const endpoint = typeof raw.endpoint === "string" ? raw.endpoint.trim().replace(/\/+$/, "") : "";
  if (endpoint === "") return null;
  const headers: Record<string, string> = {};
  if (raw.headers !== null && typeof raw.headers === "object" && !Array.isArray(raw.headers)) {
    for (const [key, value] of Object.entries(raw.headers)) {
      if (typeof value === "string") headers[key] = value;
    }
  }
  return { endpoint, headers, captureContent: raw.capture_content === true };
}

// A token count OpenCode may report as something else than a finite number reads as 0.
const count = (value: unknown): number =>
  typeof value === "number" && Number.isFinite(value) ? Math.trunc(value) : 0;

/** OpenCode's token counts as GenAI attributes (input includes cache). */
export function usageAttributes(
  tokens: { input?: number; output?: number; reasoning?: number; cache?: { read?: number; write?: number } } | undefined,
): Attr[] {
  if (!tokens) return [];
  const read = count(tokens.cache?.read);
  const write = count(tokens.cache?.write);
  const int = (key: string, value: number): Attr => ({ key, value: { intValue: String(value) } });
  return [
    int("gen_ai.usage.input_tokens", count(tokens.input) + read + write),
    int("gen_ai.usage.output_tokens", count(tokens.output)),
    int("gen_ai.usage.reasoning.output_tokens", count(tokens.reasoning)),
    int("gen_ai.usage.cache_read.input_tokens", read),
    int("gen_ai.usage.cache_creation.input_tokens", write),
  ];
}

// OpenCode's dashed finish values as gen_ai.response.finish_reasons values.
const FINISH_REASONS = new Map<string, string>([
  ["tool-calls", "tool_calls"],
  ["content-filter", "content_filter"],
]);

/** `tool-calls` → `tool_calls`, `content-filter` → `content_filter`; other strings unchanged; undefined → undefined. */
export function finishReason(raw: string | undefined): string | undefined {
  if (!raw) return undefined;
  return FINISH_REASONS.get(raw) ?? raw;
}

// The config `cetra agents setup opencode` wrote. It is read when OpenCode loads the plugin
// (in setup or server), never at module load: importing this file reads nothing. null when
// the path, the file or its content is missing: the plugin then registers nothing.
let config: Config | null = null;

const loadConfig = (): Config | null => {
  const path = configPath(process.env);
  if (path === null) return null;
  try {
    return parseConfig(readFileSync(path, "utf8"));
  } catch {
    return null;
  }
};

const str = (key: string, value: unknown): Attr => ({ key, value: { stringValue: String(value) } });
const json = (key: string, value: unknown): Attr => ({ key, value: { stringValue: JSON.stringify(value) } });

// A non-empty plain identifier: the only form of an error's class name that may leave the process
// with captureContent off.
const IDENTIFIER = /^[A-Za-z_][A-Za-z0-9_.]*$/;

// The status message for an error OpenCode reported, or undefined when there is none. With
// captureContent on, the error's own text is kept: its message, or the string itself. With it off,
// no error text leaves the process: the error's class name when that is a plain identifier, else
// the fixed string the handler picks ("tool failed", "model call failed", "interrupted", …).
const safeError = (error: unknown, fixed: string): string | undefined => {
  if (!error) return undefined;
  if (config?.captureContent) {
    if (typeof error === "string") return error;
    const message = (error as { message?: unknown }).message;
    return typeof message === "string" && message !== "" ? message : fixed;
  }
  if (typeof error === "string") return IDENTIFIER.test(error) ? error : fixed;
  const name = (error as { name?: unknown }).name;
  return typeof name === "string" && IDENTIFIER.test(name) ? name : fixed;
};

// gen_ai.response.finish_reasons is a string array, as GenAI semconv types it.
const finishReasons = (raw: string | undefined): Attr[] => {
  const value = finishReason(raw);
  return value === undefined
    ? []
    : [{ key: "gen_ai.response.finish_reasons", value: { arrayValue: { values: [{ stringValue: value }] } } }];
};

const hex = (bytes: number): string => {
  const b = new Uint8Array(bytes);
  crypto.getRandomValues(b);
  return [...b].map((x) => x.toString(16).padStart(2, "0")).join("");
};
const nanos = (ms: number): string => {
  const rounded = Number.isFinite(ms) ? Math.round(ms) : 0;
  return String(BigInt(rounded) * 1000000n);
};

type Span = {
  traceId: string;
  spanId: string;
  parentSpanId?: string;
  name: string;
  start: number;
  end?: number;
  attrs: Attr[];
  error?: string;
};
// Spans that ended and wait for the next flush. A session that never ends a turn (a
// long-running TUI) would grow it without bound, so it is capped: when it already
// holds ENDED_CAP spans, the next ended span flushes it first, unawaited.
const ended: Span[] = [];
const ENDED_CAP = 2000;
const newSpan = (name: string, parent: Span | undefined, start: number, attrs: Attr[]): Span => ({
  traceId: parent?.traceId ?? hex(16),
  spanId: hex(8),
  parentSpanId: parent?.spanId,
  name,
  start,
  attrs,
});
const end = (span: Span, at: number, error?: string): void => {
  if (ended.length >= ENDED_CAP) void flush();
  span.end = at;
  span.error = error;
  ended.push(span);
};

const toOtlp = (s: Span) => ({
  traceId: s.traceId,
  spanId: s.spanId,
  ...(s.parentSpanId ? { parentSpanId: s.parentSpanId } : {}),
  name: s.name,
  kind: 1,
  startTimeUnixNano: nanos(s.start),
  endTimeUnixNano: nanos(s.end ?? s.start),
  attributes: s.attrs,
  status: s.error ? { code: 2, message: s.error } : { code: 1 },
});

// Sends everything that ended so far in one POST. A failed POST is dropped silently; nothing
// is written to stdout or stderr.
const flush = async (): Promise<void> => {
  if (config === null) return;
  const batch = ended.splice(0);
  if (batch.length === 0) return;
  try {
    const body = {
      resourceSpans: [
        {
          resource: { attributes: [str("service.name", "opencode")] },
          scopeSpans: [{ scope: { name: SCOPE, version: PLUGIN_VERSION }, spans: batch.map(toOtlp) }],
        },
      ],
    };
    await fetch(config.endpoint + "/v1/traces", {
      method: "POST",
      headers: { "content-type": "application/json", ...config.headers },
      body: JSON.stringify(body),
      signal: AbortSignal.timeout(FLUSH_TIMEOUT_MS),
    });
  } catch {
    // dropped on purpose
  }
};

// Shared state, keyed by OpenCode ids. Both builds drive the same functions. A span that
// started and never ended is closed with its turn and taken out of the maps, so unfinished
// work never piles up across turns. `names` (v2's tool-input names, kept for a tool call
// that may never come) and `textParts` (v1's streamed text, kept for a message that may
// never complete) carry their session so a turn's end clears their entries too.
const turns = new Map<string, Span>();
const steps = new Map<string, { span: Span; text: string[]; calls: unknown[] }>();
const tools = new Map<string, Span>();
const requests = new Map<string, string>();
const names = new Map<string, { session: unknown; name: string }>();
const textParts = new Map<string, { messageID: unknown; session: unknown; text: string }>();

// The text parts of a tool result content (or the string itself), joined by newlines.
const textOf = (content: any): string =>
  (Array.isArray(content) ? content : [])
    .map((p: any) => (typeof p?.text === "string" ? p.text : JSON.stringify(p)))
    .join("\n");

// Starts a turn only for a non-empty string session id: an event or hook input without
// one is not a turn, and keying a span by anything else would only leak state.
const turnStart = (sessionID: unknown, at: number): void => {
  if (typeof sessionID !== "string" || sessionID === "") return;
  if (turns.has(sessionID)) return;
  turns.set(
    sessionID,
    newSpan("invoke_agent", undefined, at, [
      str("gen_ai.operation.name", "invoke_agent"),
      str("gen_ai.conversation.id", sessionID),
    ]),
  );
};
// Ends a turn and flushes it. The flush is awaited only by v1's `session.idle` and
// `session.deleted`, which have no disposer OpenCode would wait for; everywhere else
// the POST finishes in the background.
const turnEnd = async (sessionID: string, at: number, error?: string): Promise<void> => {
  const span = turns.get(sessionID);
  if (!span) return;
  for (const [id, step] of steps) {
    if (step.span.traceId === span.traceId) {
      end(step.span, at, error);
      steps.delete(id);
    }
  }
  for (const [id, tool] of tools) {
    if (tool.traceId === span.traceId) {
      end(tool, at, error);
      tools.delete(id);
    }
  }
  for (const [id, name] of names) {
    if (name.session === sessionID) names.delete(id);
  }
  for (const [id, part] of textParts) {
    if (part.session === sessionID) textParts.delete(id);
  }
  turns.delete(sessionID);
  requests.delete(sessionID);
  end(span, at, error);
  await flush();
};
const chatSpan = (sessionID: string, messageID: string, providerID: string, modelID: string, at: number): void => {
  const attrs = [
    str("gen_ai.operation.name", "chat"),
    str("gen_ai.provider.name", providerID),
    str("gen_ai.request.model", modelID),
    str("gen_ai.conversation.id", sessionID),
  ];
  const span = newSpan(`chat ${modelID}`, turns.get(sessionID), at, attrs);
  steps.set(messageID, { span, text: [], calls: [] });
};
const chatEnd = (messageID: string, at: number, tokens: any, finish: string | undefined, error?: string): void => {
  const step = steps.get(messageID);
  if (!step) return;
  steps.delete(messageID);
  const a = step.span.attrs;
  // gen_ai.usage.input_tokens is cache-inclusive in cetra (usage.rs); OpenCode reports cache
  // apart from input.
  a.push(...usageAttributes(tokens));
  a.push(...finishReasons(finish));
  if (config?.captureContent && (step.text.length > 0 || step.calls.length > 0)) {
    a.push(
      json("gen_ai.output.messages", [
        { role: "assistant", content: step.text.join(""), ...(step.calls.length ? { tool_calls: step.calls } : {}) },
      ]),
    );
  }
  end(step.span, at, error);
};
const toolStart = (sessionID: string, callID: string, name: string, args: unknown, at: number): void => {
  const attrs = [
    str("gen_ai.operation.name", "execute_tool"),
    str("gen_ai.tool.name", name),
    str("gen_ai.tool.call.id", callID),
    str("gen_ai.conversation.id", sessionID),
  ];
  if (config?.captureContent) attrs.push(json("gen_ai.tool.call.arguments", args ?? {}));
  tools.set(callID, newSpan(`execute_tool ${name}`, turns.get(sessionID), at, attrs));
};
const toolEnd = (callID: string, at: number, result: unknown, error?: string): void => {
  const span = tools.get(callID);
  if (!span) return;
  tools.delete(callID);
  if (config?.captureContent && result !== undefined) {
    span.attrs.push(str("gen_ai.tool.call.result", typeof result === "string" ? result : JSON.stringify(result)));
  }
  end(span, at, error);
};

// OpenCode v2 (promise plugin API, @opencode/plugin 2.3.0: session.hook, event.subscribe). The
// whole body is guarded: whatever fails, setup resolves to a no-op disposer and OpenCode runs
// unchanged.
async function setup(ctx: any): Promise<() => Promise<void>> {
  try {
    config = loadConfig();
    if (config === null) return async () => {};
    await ctx.session.hook("context", (event: any) => {
      try {
        const system = (Array.isArray(event?.system) ? event.system : []).map((part: any) => ({
          role: "system",
          content: part?.text ?? JSON.stringify(part),
        }));
        if (config?.captureContent && Array.isArray(event?.messages)) {
          requests.set(event.sessionID, JSON.stringify([...system, ...event.messages]));
        }
      } catch {
        // never break OpenCode
      }
    });
    const types = [
      "session.created",
      "session.deleted",
      "session.execution.started",
      "session.execution.succeeded",
      "session.execution.failed",
      "session.execution.interrupted",
      "session.step.started",
      "session.step.ended",
      "session.step.failed",
      "session.text.ended",
      "session.tool.input.started",
      "session.tool.called",
      "session.tool.success",
      "session.tool.failed",
    ];
    void (async () => {
      for await (const event of ctx.event.subscribe({ types })) {
        try {
          const d = event.data;
          const at = event.created;
          switch (event.type) {
            case "session.execution.started":
              turnStart(d.sessionID, at);
              break;
            case "session.execution.succeeded":
              void turnEnd(d.sessionID, at);
              break;
            case "session.execution.failed":
              void turnEnd(d.sessionID, at, safeError(d.error, "failed"));
              break;
            case "session.execution.interrupted":
              // The interrupt reason is free text: with content off it never leaves the process.
              void turnEnd(
                d.sessionID,
                at,
                config?.captureContent ? `interrupted: ${d.reason}` : "interrupted",
              );
              break;
            case "session.deleted":
              void turnEnd(d.sessionID, at, "session deleted");
              break;
            case "session.step.started":
              turnStart(d.sessionID, at);
              chatSpan(d.sessionID, d.assistantMessageID, d.model.providerID, d.model.id, d.started ?? at);
              if (config?.captureContent) {
                steps.get(d.assistantMessageID)?.span.attrs.push(
                  json("gen_ai.input.messages", JSON.parse(requests.get(d.sessionID) ?? "[]")),
                );
              }
              requests.delete(d.sessionID);
              break;
            case "session.text.ended":
              steps.get(d.assistantMessageID)?.text.push(d.text);
              break;
            case "session.step.ended":
              chatEnd(d.assistantMessageID, at, d.tokens, d.finish);
              break;
            case "session.step.failed":
              chatEnd(d.assistantMessageID, at, d.tokens, undefined, safeError(d.error, "model call failed"));
              break;
            case "session.tool.input.started":
              names.set(d.id, { session: d.sessionID, name: d.name });
              break;
            case "session.tool.called": {
              const name = names.get(d.id)?.name ?? "tool";
              steps.get(d.assistantMessageID)?.calls.push({
                id: d.id,
                type: "function",
                function: { name, arguments: JSON.stringify(d.input) },
              });
              toolStart(d.sessionID, d.id, name, d.input, at);
              break;
            }
            case "session.tool.success":
              toolEnd(d.id, at, textOf(d.content));
              names.delete(d.id);
              break;
            case "session.tool.failed":
              toolEnd(d.id, at, textOf(d.content), safeError(d.error, "tool failed"));
              names.delete(d.id);
              break;
          }
        } catch {
          // one bad event never stops the stream
        }
      }
    })().catch(() => undefined);
    return async () => {
      await flush();
    };
  } catch {
    // OpenCode must not fail to start because of us.
    return async () => {};
  }
}

// OpenCode v1 (@opencode-ai/plugin 1.19.6 Hooks: chat.message, event, tool.execute.before/after).
// As setup, the whole body is guarded: on any error the plugin registers nothing.
async function server(_input: any): Promise<Record<string, unknown>> {
  try {
    config = loadConfig();
    if (config === null) return {};
    // The message ids already recorded, with the session each belongs to so a turn's end can
    // clear its own entries.
    const done = new Map<string, string>();
    // Clears the `done` entries of one session, at its turn's end.
    const forget = (sessionID: unknown): void => {
      for (const [id, session] of done) {
        if (session === sessionID) done.delete(id);
      }
    };
    return {
      "chat.message": async (input: any) => {
        try {
          turnStart(input?.sessionID, Date.now());
        } catch {
          // never break OpenCode
        }
      },
      event: async ({ event }: any) => {
        const p = event?.properties;
        try {
          switch (event.type) {
            case "message.updated": {
              const m = p?.info;
              if (m?.role !== "assistant" || !m.time?.completed || done.has(m.id)) break;
              // A model call attaches to the current turn of its session, the one `chat.message`
              // started: `message.updated` never starts a turn, and with no current turn nothing
              // is recorded for it.
              const turn = turns.get(m.sessionID);
              if (!turn) break;
              done.set(m.id, m.sessionID);
              const span = newSpan(`chat ${m.modelID}`, turn, m.time.created, [
                str("gen_ai.operation.name", "chat"),
                str("gen_ai.provider.name", m.providerID),
                str("gen_ai.request.model", m.modelID),
                str("gen_ai.conversation.id", m.sessionID),
              ]);
              const text = [...textParts.values()].filter((t) => t.messageID === m.id).map((t) => t.text).join("");
              // The parts of a finished message are not read again, so they never pile up.
              for (const [id, part] of textParts) {
                if (part.messageID === m.id) textParts.delete(id);
              }
              if (config?.captureContent && text) {
                span.attrs.push(json("gen_ai.output.messages", [{ role: "assistant", content: text }]));
              }
              span.attrs.push(...usageAttributes(m.tokens));
              span.attrs.push(...finishReasons(m.finish));
              end(span, m.time.completed, safeError(m.error, "model call failed"));
              break;
            }
            case "message.part.updated": {
              const part = p?.part;
              if (part?.type === "text")
                textParts.set(part.id, { messageID: part.messageID, session: part.sessionID, text: part.text });
              if (part?.type === "tool" && part.state?.status === "error") {
                toolEnd(part.callID, Date.now(), undefined, safeError(part.state.error, "tool failed"));
              }
              break;
            }
            case "session.idle":
              // The turn's flush is awaited here: v1 has no disposer OpenCode would wait for, and
              // a one-shot `opencode run` may exit before an unawaited POST finishes (v2's turn
              // end stays unawaited: its disposer awaits).
              forget(p?.sessionID);
              await turnEnd(p?.sessionID, Date.now());
              break;
            case "session.deleted":
              // Awaited as `session.idle` above, for the same reason: the process may
              // be on its way out.
              forget(p?.info?.id);
              await turnEnd(p?.info?.id, Date.now(), "session deleted");
              break;
          }
        } catch {
          // swallowed on purpose
        }
      },
      "tool.execute.before": async (input: any, output: any) => {
        try {
          toolStart(input?.sessionID, input?.callID, input?.tool, output?.args, Date.now());
        } catch {
          // never break OpenCode
        }
      },
      "tool.execute.after": async (input: any, output: any) => {
        try {
          toolEnd(input?.callID, Date.now(), output?.output);
        } catch {
          // never break OpenCode
        }
      },
    };
  } catch {
    // OpenCode must not fail to start because of us.
    return {};
  }
}

export default { id: "cetra-opencode", setup, server };
