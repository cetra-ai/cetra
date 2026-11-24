// Tests for the pure helpers and the event handlers of pi_extension.ts.
// Run: node --test crates/cli/src/agents/pi_extension.test.mjs
import assert from "node:assert/strict";
import { test } from "node:test";
import cetraPi, { finishReason, parseHeaders, toOpenAI } from "./pi_extension.ts";

// Polls until predicate() holds, so a test never races the extension's unawaited flush.
const waitFor = async (predicate, ms = 2000) => {
  const deadline = Date.now() + ms;
  while (!predicate()) {
    if (Date.now() > deadline) throw new Error("timed out waiting for the export");
    await new Promise((resolve) => setTimeout(resolve, 5));
  }
};

test("the handlers send one trace per prompt", async () => {
  const endpoint = "http://cetra.test:4317";
  const previousEndpoint = process.env.OTEL_EXPORTER_OTLP_ENDPOINT;
  const previousCapture = process.env.OTEL_INSTRUMENTATION_GENAI_CAPTURE_MESSAGE_CONTENT;
  const previousFetch = globalThis.fetch;
  const posts = [];
  process.env.OTEL_EXPORTER_OTLP_ENDPOINT = endpoint;
  process.env.OTEL_INSTRUMENTATION_GENAI_CAPTURE_MESSAGE_CONTENT = "true";
  globalThis.fetch = async (url, init) => {
    posts.push({ url: String(url), body: JSON.parse(init.body) });
    return { ok: true };
  };
  try {
    const handlers = new Map();
    cetraPi({ on: (event, handler) => handlers.set(event, handler) });
    const call = (event, ...args) => handlers.get(event)(...args);
    const spansOf = (post) => post.body.resourceSpans[0].scopeSpans[0].spans;

    await call("session_start", {}, { sessionManager: { getSessionId: () => "sess-1" } });
    await call("agent_start", {}, {});
    await call("context_with_system", {
      messages: [{ role: "user", content: [{ type: "text", text: "read hello.txt" }] }],
    });
    await call("before_provider_request");
    await call("message_end", {
      message: {
        role: "assistant",
        model: "gpt-4o-mini",
        provider: "openai",
        usage: { input: 10, output: 5, totalTokens: 15 },
        stopReason: "toolUse",
        content: [
          { type: "toolCall", id: "call_1", name: "read", arguments: { path: "hello.txt" } },
        ],
      },
    });
    await call("tool_execution_start", {
      toolCallId: "call_1",
      toolName: "read",
      args: { path: "hello.txt" },
    });
    await call("tool_execution_end", {
      toolCallId: "call_1",
      toolName: "read",
      result: { content: [{ type: "text", text: "hello from cetra" }] },
    });
    await call("agent_end");
    await waitFor(() => posts.length === 1);

    assert.equal(posts.length, 1);
    assert.equal(posts[0].url, `${endpoint}/v1/traces`);
    const spans = spansOf(posts[0]);
    assert.deepEqual(
      spans.map((span) => span.name).sort(),
      ["chat gpt-4o-mini", "execute_tool read", "invoke_agent"],
    );
    assert.equal(new Set(spans.map((span) => span.traceId)).size, 1);
    const root = spans.find((span) => span.name === "invoke_agent");
    const chat = spans.find((span) => span.name === "chat gpt-4o-mini");
    const tool = spans.find((span) => span.name === "execute_tool read");
    assert.equal(chat.parentSpanId, root.spanId);
    assert.equal(tool.parentSpanId, root.spanId);

    // A second prompt is its own trace, with a string content on the answer.
    await call("agent_start", {}, {});
    await call("message_end", {
      message: { role: "assistant", model: "gpt-4o-mini", usage: {}, stopReason: "stop", content: "done" },
    });
    await call("agent_end");
    await waitFor(() => posts.length === 2);
    const second = spansOf(posts[1]);
    assert.equal(new Set(second.map((span) => span.traceId)).size, 1);
    assert.notEqual(second[0].traceId, spans[0].traceId);

    // Malformed events never throw, and a non-array messages payload is read as empty.
    await call("agent_start", {}, {});
    await assert.doesNotReject(call("context_with_system", { messages: "not an array" }));
    await assert.doesNotReject(call("message_end", {}));
    await assert.doesNotReject(call("message_end", null));
    await assert.doesNotReject(call("tool_execution_end", null));
    await call("agent_end");
    await waitFor(() => posts.length === 3);
  } finally {
    if (previousEndpoint === undefined) delete process.env.OTEL_EXPORTER_OTLP_ENDPOINT;
    else process.env.OTEL_EXPORTER_OTLP_ENDPOINT = previousEndpoint;
    if (previousCapture === undefined) delete process.env.OTEL_INSTRUMENTATION_GENAI_CAPTURE_MESSAGE_CONTENT;
    else process.env.OTEL_INSTRUMENTATION_GENAI_CAPTURE_MESSAGE_CONTENT = previousCapture;
    globalThis.fetch = previousFetch;
  }
});

test("parseHeaders decodes a percent-encoded bearer token", () => {
  assert.deepEqual(parseHeaders("Authorization=Bearer%20k"), { Authorization: "Bearer k" });
});

test("parseHeaders splits pairs on commas and decodes each value", () => {
  assert.deepEqual(parseHeaders("a=1, b=x%2Cy"), { a: "1", b: "x,y" });
});

test("parseHeaders keeps a value with a lone percent sign raw", () => {
  assert.deepEqual(parseHeaders("a=100%"), { a: "100%" });
});

test("parseHeaders keeps a plus sign as a plus sign", () => {
  assert.deepEqual(parseHeaders("a=x+y"), { a: "x+y" });
});

test("parseHeaders skips an entry without an equals sign", () => {
  assert.deepEqual(parseHeaders("novalue, a=1"), { a: "1" });
});

test("finishReason maps each pi stop reason", () => {
  assert.equal(finishReason("stop"), "stop");
  assert.equal(finishReason("length"), "length");
  assert.equal(finishReason("toolUse"), "tool_calls");
  assert.equal(finishReason("error"), "error");
  assert.equal(finishReason("aborted"), "cancelled");
});

test("finishReason keeps any other reason as it is", () => {
  assert.equal(finishReason("deferred"), "deferred");
  assert.equal(finishReason("constructor"), "constructor");
  assert.equal(finishReason(undefined), undefined);
});

test("toOpenAI maps an assistant message with a tool call", () => {
  const message = {
    role: "assistant",
    content: [
      { type: "text", text: "Let me check." },
      { type: "thinking", thinking: "read the file" },
      { type: "toolCall", id: "call_1", name: "read", arguments: { path: "a.txt" } },
    ],
  };
  assert.deepEqual(toOpenAI(message), {
    role: "assistant",
    content: "Let me check.",
    reasoning_content: "read the file",
    tool_calls: [
      {
        id: "call_1",
        type: "function",
        function: { name: "read", arguments: '{"path":"a.txt"}' },
      },
    ],
  });
});

test("toOpenAI maps a tool result", () => {
  const message = {
    role: "toolResult",
    toolCallId: "call_1",
    content: [{ type: "text", text: "file body" }],
  };
  assert.deepEqual(toOpenAI(message), {
    role: "tool",
    tool_call_id: "call_1",
    content: "file body",
  });
});

test("toOpenAI maps user and system messages and skips a custom role", () => {
  assert.deepEqual(toOpenAI({ role: "user", content: [{ type: "text", text: "hi" }] }), {
    role: "user",
    content: "hi",
  });
  assert.deepEqual(toOpenAI({ role: "system", content: "be brief" }), {
    role: "system",
    content: "be brief",
  });
  assert.equal(toOpenAI({ role: "custom", content: "x" }), null);
});
