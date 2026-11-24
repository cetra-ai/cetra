// Tests for the pure helpers of pi_extension.ts. Run: node --test crates/cli/src/agents/pi_extension.test.mjs
import assert from "node:assert/strict";
import { test } from "node:test";
import { finishReason, parseHeaders, toOpenAI } from "./pi_extension.ts";

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
