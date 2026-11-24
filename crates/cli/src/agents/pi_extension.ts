// Managed by cetra (cetra agents setup pi). Delete this file or run: cetra agents remove pi
// cetra's pi extension (D-IN-27, D-IN-28). It turns each pi agent run into GenAI spans and
// sends them to cetra as OTLP/HTTP JSON. It loads only when OTEL_EXPORTER_OTLP_ENDPOINT is
// set. It imports nothing: it uses the globals fetch, crypto, AbortSignal and process.env.
// Every handler swallows its errors, so a problem here never breaks pi.

const SCOPE = "cetra-pi";
const EXTENSION_VERSION = "1";
const FLUSH_TIMEOUT_MS = 5000;

const env = (name: string): string => (process.env[name] ?? "").trim();

const decode = (value: string): string => {
  try {
    return decodeURIComponent(value);
  } catch {
    return value;
  }
};

// OTEL_EXPORTER_OTLP_HEADERS: comma-separated key=value pairs. Each pair splits at its first
// "=". Values are percent-encoded; a value that does not decode stays raw. Pairs without "="
// (or with an empty key) are skipped.
export function parseHeaders(text: string): Record<string, string> {
  const out: Record<string, string> = {};
  for (const part of text.split(",")) {
    const i = part.indexOf("=");
    if (i < 0) continue;
    const key = part.slice(0, i).trim();
    if (key === "") continue;
    out[key] = decode(part.slice(i + 1).trim());
  }
  return out;
}

// pi's stop reasons as gen_ai.response.finish_reasons values. Any other reason is kept as is.
const FINISH_REASONS = new Map<string, string>([
  ["stop", "stop"],
  ["length", "length"],
  ["toolUse", "tool_calls"],
  ["error", "error"],
  ["aborted", "cancelled"],
]);

export function finishReason(piReason: string | undefined): string | undefined {
  if (!piReason) return undefined;
  return FINISH_REASONS.get(piReason) ?? piReason;
}

// The text parts of a message content (or the string content itself), joined by newlines.
const text = (content: any): string =>
  typeof content === "string"
    ? content
    : (Array.isArray(content) ? content : [])
        .filter((p: any) => p?.type === "text")
        .map((p: any) => p.text)
        .join("\n");

// A pi message as an OpenAI chat message, the shape gen_ai.input.messages and
// gen_ai.output.messages take. The content is a string or an array of parts. Roles other
// than these are skipped (null).
export function toOpenAI(message: any): any {
  if (!message || typeof message.role !== "string") return null;
  if (message.role === "assistant") {
    const out: any = { role: "assistant", content: text(message.content) };
    const parts: any[] = Array.isArray(message.content) ? message.content : [];
    const thinking = parts
      .filter((p: any) => p?.type === "thinking")
      .map((p: any) => p.thinking)
      .join("\n");
    if (thinking) out.reasoning_content = thinking;
    const calls = parts
      .filter((p: any) => p?.type === "toolCall")
      .map((p: any) => ({
        id: p.id,
        type: "function",
        function: { name: p.name, arguments: JSON.stringify(p.arguments ?? {}) },
      }));
    if (calls.length) out.tool_calls = calls;
    return out;
  }
  if (message.role === "toolResult") {
    return { role: "tool", tool_call_id: message.toolCallId, content: text(message.content) };
  }
  if (message.role === "system" || message.role === "user") {
    return { role: message.role, content: text(message.content) };
  }
  return null;
}

const hexId = (bytes: number): string => {
  const buffer = new Uint8Array(bytes);
  crypto.getRandomValues(buffer);
  return [...buffer].map((x) => x.toString(16).padStart(2, "0")).join("");
};

const now = (): bigint => BigInt(Date.now()) * 1000000n;

type Attr = { key: string; value: Record<string, unknown> };
const str = (key: string, value: unknown): Attr => ({ key, value: { stringValue: String(value) } });
const int = (key: string, value: number): Attr => ({ key, value: { intValue: String(value) } });
const json = (key: string, value: unknown): Attr => ({
  key,
  value: { stringValue: JSON.stringify(value) },
});

// Wraps a handler so that an error in it is swallowed.
const guard =
  (handler: (...args: any[]) => unknown) =>
  async (...args: any[]): Promise<void> => {
    try {
      await handler(...args);
    } catch {
      // Never break pi.
    }
  };

export default function cetraPi(pi: any): void {
  const base = env("OTEL_EXPORTER_OTLP_ENDPOINT").replace(/\/+$/, "");
  if (base === "") return;
  const endpoint = `${base}/v1/traces`;
  const headers = {
    ...parseHeaders(env("OTEL_EXPORTER_OTLP_HEADERS")),
    "content-type": "application/json",
  };
  const service = env("OTEL_SERVICE_NAME") || "pi";
  const capture = env("OTEL_INSTRUMENTATION_GENAI_CAPTURE_MESSAGE_CONTENT").toLowerCase() === "true";

  const spans: any[] = [];
  let traceId: string | null = null;
  let sessionId = "";
  let run: { id: string; start: bigint } | null = null;
  let requestStart: bigint | null = null;
  let inputMessages: any[] = [];
  const tools = new Map<string, { id: string; start: bigint; name: string; args: any }>();

  const record = (
    name: string,
    start: bigint,
    end: bigint,
    spanId: string,
    parent: string | null,
    attrs: Attr[],
    isError: boolean,
  ): void => {
    if (traceId === null) traceId = hexId(16);
    spans.push({
      traceId,
      spanId,
      parentSpanId: parent ?? undefined,
      name,
      kind: 1,
      startTimeUnixNano: start.toString(),
      endTimeUnixNano: end.toString(),
      attributes: attrs,
      status: isError ? { code: 2 } : undefined,
    });
  };

  // Sends the spans buffered so far in one POST. A failed POST is dropped.
  const flush = async (): Promise<void> => {
    if (spans.length === 0) return;
    const batch = spans.splice(0, spans.length);
    const body = {
      resourceSpans: [
        {
          resource: { attributes: [str("service.name", service)] },
          scopeSpans: [{ scope: { name: SCOPE, version: EXTENSION_VERSION }, spans: batch }],
        },
      ],
    };
    try {
      await fetch(endpoint, {
        method: "POST",
        headers,
        body: JSON.stringify(body),
        signal: AbortSignal.timeout(FLUSH_TIMEOUT_MS),
      });
    } catch {
      // Dropped silently: nothing goes to stdout or stderr.
    }
  };

  const conv = (): Attr => str("gen_ai.conversation.id", sessionId);
  const finish = (reason: string | undefined): Attr[] => {
    const value = finishReason(reason);
    return value === undefined
      ? []
      : [{ key: "gen_ai.response.finish_reasons", value: { arrayValue: { values: [{ stringValue: value }] } } }];
  };
  // Ends the run. A tool that started and never ended is dropped, not emitted, so unfinished
  // tools never pile up across runs.
  const closeRun = (): void => {
    tools.clear();
    if (!run) return;
    record(
      "invoke_agent",
      run.start,
      now(),
      run.id,
      null,
      [str("gen_ai.operation.name", "invoke_agent"), conv()],
      false,
    );
    run = null;
  };

  pi.on(
    "session_start",
    guard((_e: any, ctx: any) => {
      sessionId = ctx?.sessionManager?.getSessionId?.() ?? "";
    }),
  );
  pi.on(
    "context_with_system",
    guard((e: any) => {
      inputMessages = e?.messages ?? [];
    }),
  );
  pi.on(
    "before_provider_request",
    guard(() => {
      requestStart = now();
    }),
  );
  pi.on(
    "agent_start",
    guard((_e: any, ctx: any) => {
      sessionId = ctx?.sessionManager?.getSessionId?.() || sessionId;
      // One trace per prompt.
      traceId = hexId(16);
      run = { id: hexId(8), start: now() };
    }),
  );
  pi.on(
    "message_end",
    guard((e: any) => {
      const m = e?.message;
      if (!run || m?.role !== "assistant") return;
      const u = m.usage ?? {};
      const attrs: Attr[] = [
        str("gen_ai.operation.name", "chat"),
        str("gen_ai.provider.name", m.provider),
        str("gen_ai.request.model", m.model),
        str("gen_ai.response.model", m.responseModel ?? m.model),
        conv(),
        int("gen_ai.usage.input_tokens", u.input ?? 0),
        int("gen_ai.usage.output_tokens", u.output ?? 0),
        int("gen_ai.usage.cache_read.input_tokens", u.cacheRead ?? 0),
        int("gen_ai.usage.cache_write.input_tokens", u.cacheWrite ?? 0),
        int("gen_ai.usage.total_tokens", u.totalTokens ?? 0),
        ...(u.reasoning !== undefined ? [int("gen_ai.usage.reasoning.output_tokens", u.reasoning)] : []),
        ...finish(m.stopReason),
      ];
      if (capture) {
        const messages: any[] = Array.isArray(inputMessages) ? inputMessages : [];
        attrs.push(json("gen_ai.input.messages", messages.map(toOpenAI).filter(Boolean)));
        attrs.push(json("gen_ai.output.messages", [toOpenAI(m)].filter(Boolean)));
      }
      record(
        `chat ${m.model}`,
        requestStart ?? now(),
        now(),
        hexId(8),
        run.id,
        attrs,
        m.stopReason === "error",
      );
      requestStart = null;
    }),
  );
  pi.on(
    "tool_execution_start",
    guard((e: any) => {
      tools.set(e.toolCallId, { id: hexId(8), start: now(), name: e.toolName, args: e.args });
    }),
  );
  pi.on(
    "tool_execution_end",
    guard((e: any) => {
      const t = tools.get(e?.toolCallId);
      if (!t) return;
      tools.delete(e.toolCallId);
      const attrs: Attr[] = [
        str("gen_ai.operation.name", "execute_tool"),
        str("gen_ai.tool.name", e.toolName ?? t.name),
        str("gen_ai.tool.call.id", e.toolCallId),
        conv(),
      ];
      if (capture) {
        attrs.push(json("gen_ai.tool.call.arguments", t.args));
        attrs.push(json("gen_ai.tool.call.result", text(e.result?.content) || (e.result ?? "")));
      }
      record(
        `execute_tool ${e.toolName ?? t.name}`,
        t.start,
        now(),
        t.id,
        run?.id ?? null,
        attrs,
        e.isError === true,
      );
    }),
  );
  pi.on(
    "agent_end",
    guard(() => {
      // Not awaited: pi goes on at once, and the POST finishes in the background.
      closeRun();
      void flush();
    }),
  );
  pi.on(
    "session_shutdown",
    guard(async () => {
      // A run that never saw agent_end (crash or abort) is closed rather than lost.
      closeRun();
      await flush();
    }),
  );
}
