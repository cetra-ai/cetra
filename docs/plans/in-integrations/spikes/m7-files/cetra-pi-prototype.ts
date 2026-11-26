// cetra m7 spike: pi extension emitting GenAI semconv spans as OTLP/HTTP JSON.
// Throwaway prototype, ~110 lines, zero imports (globals only, so jiti needs nothing).
// Spans: one invoke_agent per run, one `chat <model>` per assistant message,
// one `execute_tool <name>` per tool call. PI_FLUSH=shutdown flushes only at
// session_shutdown (spike: does pi await async shutdown handlers?).
const SCOPE = "cetra-pi-extension";
const FLUSH_MS = 5000;
const env = (n: string) => (process.env[n] ?? "").trim();
const endpoint = () => {
  const explicit = env("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT");
  if (explicit) return explicit;
  const base = env("OTEL_EXPORTER_OTLP_ENDPOINT").replace(/\/+$/, "");
  return base ? base + "/v1/traces" : "";
};
// OTEL_EXPORTER_OTLP_HEADERS values are percent-encoded per the OTel spec.
// decodeURIComponent throws on a lone "%", so a bad sequence stays raw.
const decode = (v: string) => { try { return decodeURIComponent(v); } catch { return v; } };
const outHeaders = () => {
  const out: Record<string, string> = { "content-type": "application/json" };
  for (const part of env("OTEL_EXPORTER_OTLP_HEADERS").split(",")) {
    const i = part.indexOf("=");
    if (i > 0) out[part.slice(0, i).trim()] = decode(part.slice(i + 1).trim());
  }
  return out;
};
const capture = () => /^(1|true|yes)$/i.test(env("OTEL_INSTRUMENTATION_GENAI_CAPTURE_MESSAGE_CONTENT"));
const flushAt = () => (env("PI_FLUSH") === "shutdown" ? "shutdown" : "boundaries");

const now = () => BigInt(Date.now()) * 1000000n;
const hexId = (n: number) => {
  const b = new Uint8Array(n);
  crypto.getRandomValues(b);
  return [...b].map((x) => x.toString(16).padStart(2, "0")).join("");
};

type Attr = { key: string; value: Record<string, unknown> };
const str = (k: string, v: unknown): Attr => ({ key: k, value: { stringValue: String(v) } });
const int = (k: string, v: number): Attr => ({ key: k, value: { intValue: String(v) } });
const json = (k: string, v: unknown): Attr => ({ key: k, value: { stringValue: JSON.stringify(v) } });

const STOP: Record<string, string> = { stop: "stop", length: "length", toolUse: "tool_calls", error: "error", aborted: "aborted", deferred: "deferred" };

const text = (content: unknown): string =>
  typeof content === "string" ? content
    : (Array.isArray(content) ? content : []).filter((p: any) => p?.type === "text").map((p: any) => p.text).join("\n");

// pi messages -> OpenAI chat format (what gen_ai.*.messages expects).
const toOpenAI = (m: any) => {
  if (!m || typeof m.role !== "string") return null;
  if (m.role === "assistant") {
    const out: any = { role: "assistant", content: text(m.content) };
    const thinking = (m.content ?? []).filter((p: any) => p?.type === "thinking").map((p: any) => p.thinking).join("\n");
    if (thinking) out.reasoning_content = thinking;
    const calls = (m.content ?? []).filter((p: any) => p?.type === "toolCall")
      .map((p: any) => ({ id: p.id, type: "function", function: { name: p.name, arguments: JSON.stringify(p.arguments ?? {}) } }));
    if (calls.length) out.tool_calls = calls;
    return out;
  }
  if (m.role === "toolResult") return { role: "tool", tool_call_id: m.toolCallId, content: text(m.content) };
  if (m.role === "system" || m.role === "user") return { role: m.role, content: text(m.content) };
  return null; // custom roles: not model input shapes we map
};

const spans: any[] = [];
let traceId: string | null = null;
let sessionId = "";
let run: { id: string; start: bigint } | null = null;
let requestStart: bigint | null = null;
let inputMessages: any[] = [];
const tools = new Map<string, { id: string; start: bigint; name: string; args: any }>();

const record = (name: string, start: bigint, end: bigint, spanId: string, parent: string | null, attrs: Attr[], isError: boolean) => {
  if (traceId === null) traceId = hexId(16);
  spans.push({
    traceId, spanId, parentSpanId: parent ?? undefined, name, kind: 1,
    startTimeUnixNano: start.toString(), endTimeUnixNano: end.toString(),
    attributes: attrs, status: isError ? { code: 2 } : undefined,
  });
};

let flushing = false;
const flush = async () => {
  if (flushing || spans.length === 0 || !endpoint()) return;
  flushing = true;
  const batch = spans.splice(0, spans.length);
  const body = {
    resourceSpans: [{
      resource: { attributes: [str("service.name", env("OTEL_SERVICE_NAME") || "pi")] },
      scopeSpans: [{ scope: { name: SCOPE, version: "0.1.0" }, spans: batch }],
    }],
  };
  try {
    await fetch(endpoint(), { method: "POST", headers: outHeaders(), body: JSON.stringify(body), signal: AbortSignal.timeout(FLUSH_MS) });
  } catch { /* never hold pi's exit open on a dead endpoint */ }
  flushing = false;
};

const conv = (): Attr => str("gen_ai.conversation.id", sessionId);
const finish = (reason: string | undefined): Attr[] =>
  reason && STOP[reason] ? [{ key: "gen_ai.response.finish_reasons", value: { arrayValue: { values: [{ stringValue: STOP[reason] }] } } }] : [];

export default (pi: any) => {
  pi.on("session_start", (_e: any, ctx: any) => {
    sessionId = ctx?.sessionManager?.getSessionId?.() ?? "";
  });
  pi.on("context_with_system", (e: any) => { inputMessages = e?.messages ?? []; });
  pi.on("before_provider_request", () => { requestStart = now(); });
  pi.on("agent_start", (_e: any, ctx: any) => {
    sessionId = ctx?.sessionManager?.getSessionId?.() || sessionId;
    run = { id: hexId(8), start: now() };
  });
  pi.on("message_end", (e: any) => {
    const m = e?.message;
    if (!run || m?.role !== "assistant") return;
    const u = m.usage ?? {};
    const attrs = [
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
    if (capture()) {
      attrs.push(json("gen_ai.input.messages", inputMessages.map(toOpenAI).filter(Boolean)));
      attrs.push(json("gen_ai.output.messages", [toOpenAI(m)].filter(Boolean)));
    }
    record(`chat ${m.model}`, requestStart ?? now(), now(), hexId(8), run.id, attrs, m.stopReason === "error");
    requestStart = null;
  });
  pi.on("tool_execution_start", (e: any) => {
    tools.set(e.toolCallId, { id: hexId(8), start: now(), name: e.toolName, args: e.args });
  });
  pi.on("tool_execution_end", (e: any) => {
    const t = tools.get(e?.toolCallId);
    if (!t) return;
    tools.delete(e.toolCallId);
    const attrs = [
      str("gen_ai.operation.name", "execute_tool"),
      str("gen_ai.tool.name", e.toolName ?? t.name),
      str("gen_ai.tool.call.id", e.toolCallId),
      conv(),
    ];
    if (capture()) {
      attrs.push(json("gen_ai.tool.call.arguments", t.args));
      attrs.push(json("gen_ai.tool.call.result", text(e.result?.content) || (e.result ?? "")));
    }
    record(`execute_tool ${e.toolName ?? t.name}`, t.start, now(), t.id, run?.id ?? null, attrs, e.isError === true);
  });
  pi.on("agent_end", async () => {
    if (!run) return;
    record("invoke_agent", run.start, now(), run.id, null, [str("gen_ai.operation.name", "invoke_agent"), conv()], false);
    run = null;
    if (flushAt() === "boundaries") await flush();
  });
  pi.on("agent_settled", async () => { if (flushAt() === "boundaries") await flush(); });
  pi.on("session_shutdown", async () => {
    if (run) { // a run that never saw agent_end (crash/abort): close it rather than lose it
      record("invoke_agent", run.start, now(), run.id, null, [str("gen_ai.operation.name", "invoke_agent"), conv()], false);
      run = null;
    }
    await flush();
  });
};
