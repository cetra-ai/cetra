// cetra M7 spike: OpenCode plugin prototype. Throwaway, not shipped, no imports.
// One default export serves both builds: OpenCode v2 calls setup(context), OpenCode v1 calls server(input).
// Spans: invoke_agent per turn, chat <model> per model call, execute_tool <name> per tool call.
// The endpoint comes from CETRA_OTLP_ENDPOINT, not OTEL_EXPORTER_OTLP_ENDPOINT: OpenCode v2.3.0
// exports its own spans whenever OTEL_EXPORTER_OTLP_ENDPOINT is set (m7-opencode-plugin.md § 5).

const SCOPE = "cetra-opencode";
const FLUSH_TIMEOUT_MS = 5000;

const env = (name: string): string => (process.env[name] ?? "").trim();
const decode = (value: string): string => {
  try {
    return decodeURIComponent(value);
  } catch {
    return value;
  }
};
const endpoint = (): string => {
  const base = env("CETRA_OTLP_ENDPOINT").replace(/\/+$/, "");
  return base ? base + "/v1/traces" : "";
};
// Same header rule as the pi extension: comma-separated pairs, split at the first "=", values percent-decoded.
const headers = (): Record<string, string> => {
  const out: Record<string, string> = { "content-type": "application/json" };
  for (const part of env("CETRA_OTLP_HEADERS").split(",")) {
    const i = part.indexOf("=");
    if (i > 0) out[part.slice(0, i).trim()] = decode(part.slice(i + 1).trim());
  }
  return out;
};
const capture = (): boolean => /^(1|true|yes)$/i.test(env("OTEL_INSTRUMENTATION_GENAI_CAPTURE_MESSAGE_CONTENT"));

type Attr = { key: string; value: Record<string, unknown> };
const str = (key: string, value: unknown): Attr => ({ key, value: { stringValue: String(value) } });
const int = (key: string, value: number): Attr => ({ key, value: { intValue: String(Math.trunc(value)) } });
const json = (key: string, value: unknown): Attr => ({ key, value: { stringValue: JSON.stringify(value) } });
const hex = (bytes: number): string => {
  const b = new Uint8Array(bytes);
  crypto.getRandomValues(b);
  return [...b].map((x) => x.toString(16).padStart(2, "0")).join("");
};
const nanos = (ms: number): string => String(BigInt(Math.round(ms)) * 1000000n);

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
const ended: Span[] = [];
const newSpan = (name: string, parent: Span | undefined, start: number, attrs: Attr[]): Span => ({
  traceId: parent?.traceId ?? hex(16),
  spanId: hex(8),
  parentSpanId: parent?.spanId,
  name,
  start,
  attrs,
});
const end = (span: Span, at: number, error?: string): void => {
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
// Sends everything that ended so far. Errors are swallowed: a dead endpoint never breaks OpenCode.
const flush = async (): Promise<void> => {
  const url = endpoint();
  const batch = ended.splice(0);
  if (!url || batch.length === 0) return;
  const body = {
    resourceSpans: [
      {
        resource: { attributes: [str("service.name", env("OTEL_SERVICE_NAME") || "opencode")] },
        scopeSpans: [{ scope: { name: SCOPE }, spans: batch.map(toOtlp) }],
      },
    ],
  };
  try {
    await fetch(url, { method: "POST", headers: headers(), body: JSON.stringify(body), signal: AbortSignal.timeout(FLUSH_TIMEOUT_MS) });
  } catch {
    // ignored on purpose
  }
};

// Shared state, keyed by OpenCode ids. Both builds drive the same functions.
const turns = new Map<string, Span>();
const steps = new Map<string, { span: Span; text: string[]; calls: unknown[] }>();
const tools = new Map<string, Span>();
const requests = new Map<string, string>();

const textOf = (content: any): string =>
  (Array.isArray(content) ? content : []).map((p: any) => (typeof p?.text === "string" ? p.text : JSON.stringify(p))).join("\n");

const turnStart = (sessionID: string, at: number): void => {
  if (turns.has(sessionID)) return;
  turns.set(sessionID, newSpan("invoke_agent", undefined, at, [str("gen_ai.operation.name", "invoke_agent"), str("gen_ai.conversation.id", sessionID)]));
};
const turnEnd = (sessionID: string, at: number, error?: string): void => {
  const span = turns.get(sessionID);
  if (!span) return;
  for (const [id, step] of steps) if (step.span.traceId === span.traceId) { end(step.span, at, error); steps.delete(id); }
  for (const [id, tool] of tools) if (tool.traceId === span.traceId) { end(tool, at, error); tools.delete(id); }
  turns.delete(sessionID);
  end(span, at, error);
  void flush();
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
  if (tokens) {
    // gen_ai.usage.input_tokens is cache-inclusive in cetra (usage.rs); OpenCode reports cache apart from input.
    const cache = (tokens.cache?.read ?? 0) + (tokens.cache?.write ?? 0);
    a.push(int("gen_ai.usage.input_tokens", (tokens.input ?? 0) + cache), int("gen_ai.usage.output_tokens", tokens.output ?? 0));
    a.push(int("gen_ai.usage.reasoning.output_tokens", tokens.reasoning ?? 0));
    a.push(int("gen_ai.usage.cache_read.input_tokens", tokens.cache?.read ?? 0), int("gen_ai.usage.cache_creation.input_tokens", tokens.cache?.write ?? 0));
  }
  if (finish) a.push(json("gen_ai.response.finish_reasons", [finish]));
  if (capture() && (step.text.length > 0 || step.calls.length > 0))
    a.push(json("gen_ai.output.messages", [{ role: "assistant", content: step.text.join(""), ...(step.calls.length ? { tool_calls: step.calls } : {}) }]));
  end(step.span, at, error);
};
const toolStart = (sessionID: string, callID: string, name: string, args: unknown, at: number): void => {
  const attrs = [
    str("gen_ai.operation.name", "execute_tool"),
    str("gen_ai.tool.name", name),
    str("gen_ai.tool.call.id", callID),
    str("gen_ai.conversation.id", sessionID),
  ];
  if (capture()) attrs.push(json("gen_ai.tool.call.arguments", args ?? {}));
  tools.set(callID, newSpan(`execute_tool ${name}`, turns.get(sessionID), at, attrs));
};
const toolEnd = (callID: string, at: number, result: unknown, error?: string): void => {
  const span = tools.get(callID);
  if (!span) return;
  tools.delete(callID);
  if (capture() && result !== undefined) span.attrs.push(str("gen_ai.tool.call.result", typeof result === "string" ? result : JSON.stringify(result)));
  end(span, at, error);
};

// OpenCode v2 (promise plugin API, @opencode/plugin 2.3.0: session.hook, event.subscribe).
export async function setupV2(ctx: any): Promise<() => Promise<void>> {
  const names = new Map<string, string>();
  await ctx.session.hook("context", (event: any) => {
    const system = (event.system ?? []).map((part: any) => ({ role: "system", content: part.text ?? JSON.stringify(part) }));
    if (capture()) requests.set(event.sessionID, JSON.stringify([...system, ...event.messages]));
  });
  const types = [
    "session.created", "session.deleted", "session.execution.started", "session.execution.succeeded",
    "session.execution.failed", "session.execution.interrupted", "session.step.started", "session.step.ended",
    "session.step.failed", "session.text.ended", "session.tool.input.started", "session.tool.called",
    "session.tool.success", "session.tool.failed",
  ];
  void (async () => {
    for await (const event of ctx.event.subscribe({ types })) {
      try {
        const d = event.data;
        const at = event.created;
        switch (event.type) {
          case "session.execution.started": turnStart(d.sessionID, at); break;
          case "session.execution.succeeded": turnEnd(d.sessionID, at); break;
          case "session.execution.failed": turnEnd(d.sessionID, at, d.error?.message ?? "failed"); break;
          case "session.execution.interrupted": turnEnd(d.sessionID, at, `interrupted: ${d.reason}`); break;
          case "session.deleted": turnEnd(d.sessionID, at, "session deleted"); break;
          case "session.step.started":
            turnStart(d.sessionID, at);
            chatSpan(d.sessionID, d.assistantMessageID, d.model.providerID, d.model.id, d.started ?? at);
            if (capture()) steps.get(d.assistantMessageID)?.span.attrs.push(json("gen_ai.input.messages", JSON.parse(requests.get(d.sessionID) ?? "[]")));
            requests.delete(d.sessionID);
            break;
          case "session.text.ended": steps.get(d.assistantMessageID)?.text.push(d.text); break;
          case "session.step.ended": chatEnd(d.assistantMessageID, at, d.tokens, d.finish); break;
          case "session.step.failed": chatEnd(d.assistantMessageID, at, d.tokens, undefined, d.error?.message ?? "step failed"); break;
          case "session.tool.input.started": names.set(d.id, d.name); break;
          case "session.tool.called": {
            const name = names.get(d.id) ?? "tool";
            steps.get(d.assistantMessageID)?.calls.push({ id: d.id, type: "function", function: { name, arguments: JSON.stringify(d.input) } });
            toolStart(d.sessionID, d.id, name, d.input, at);
            break;
          }
          case "session.tool.success": toolEnd(d.id, at, textOf(d.content)); break;
          case "session.tool.failed": toolEnd(d.id, at, textOf(d.content), d.error?.message ?? "tool failed"); break;
        }
      } catch {
        // one bad event never stops the stream
      }
    }
  })().catch(() => undefined);
  return async () => {
    await flush();
  };
}

// OpenCode v1 (@opencode-ai/plugin 1.19.6 Hooks: chat.message, event, tool.execute.before/after).
export async function serverV1(_input: any): Promise<Record<string, unknown>> {
  const textParts = new Map<string, { messageID: string; text: string }>();
  const done = new Set<string>();
  return {
    "chat.message": async (input: any) => {
      turnStart(input.sessionID, Date.now());
    },
    event: async ({ event }: any) => {
      const p = event.properties;
      try {
        switch (event.type) {
          case "message.updated": {
            const m = p.info;
            if (m.role !== "assistant" || !m.time?.completed || done.has(m.id)) break;
            done.add(m.id);
            turnStart(m.sessionID, m.time.created);
            const span = newSpan(`chat ${m.modelID}`, turns.get(m.sessionID), m.time.created, [
              str("gen_ai.operation.name", "chat"),
              str("gen_ai.provider.name", m.providerID),
              str("gen_ai.request.model", m.modelID),
              str("gen_ai.conversation.id", m.sessionID),
            ]);
            const text = [...textParts.values()].filter((t) => t.messageID === m.id).map((t) => t.text).join("");
            if (capture() && text) span.attrs.push(json("gen_ai.output.messages", [{ role: "assistant", content: text }]));
            span.attrs.push(int("gen_ai.usage.input_tokens", m.tokens.input + m.tokens.cache.read + m.tokens.cache.write), int("gen_ai.usage.output_tokens", m.tokens.output));
            span.attrs.push(int("gen_ai.usage.reasoning.output_tokens", m.tokens.reasoning));
            span.attrs.push(int("gen_ai.usage.cache_read.input_tokens", m.tokens.cache.read), int("gen_ai.usage.cache_creation.input_tokens", m.tokens.cache.write));
            if (m.finish) span.attrs.push(json("gen_ai.response.finish_reasons", [m.finish]));
            end(span, m.time.completed, m.error?.name);
            break;
          }
          case "message.part.updated": {
            const part = p.part;
            if (part.type === "text") textParts.set(part.id, { messageID: part.messageID, text: part.text });
            if (part.type === "tool" && part.state.status === "error") toolEnd(part.callID, Date.now(), undefined, part.state.error);
            break;
          }
          case "session.idle":
            turnEnd(p.sessionID, Date.now());
            break;
          case "session.deleted":
            turnEnd(p.info.id, Date.now(), "session deleted");
            break;
        }
      } catch {
        // swallowed on purpose
      }
    },
    "tool.execute.before": async (input: any, output: any) => {
      toolStart(input.sessionID, input.callID, input.tool, output.args, Date.now());
    },
    "tool.execute.after": async (input: any, output: any) => {
      toolEnd(input.callID, Date.now(), output.output);
    },
  };
}

export default {
  id: "cetra-opencode",
  setup: setupV2,
  server: serverV1,
};
