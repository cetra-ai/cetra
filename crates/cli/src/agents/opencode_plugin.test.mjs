// Tests for the pure helpers and the handlers of opencode_plugin.ts. Importing the plugin reads
// no file: it reads its config only when OpenCode calls setup or server. The handler tests drive
// setup and server against a temp config and a recording fetch, restoring both afterwards; the
// event names and payload shapes are the ones the plugin itself reads.
// Run: node --test crates/cli/src/agents/opencode_plugin.test.mjs
import assert from "node:assert/strict";
import { mkdtempSync, mkdirSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import cetraOpenCode, { configPath, finishReason, parseConfig, usageAttributes } from "./opencode_plugin.ts";

// Polls until predicate() holds, so a test never races the plugin's unawaited v2 flush.
const waitFor = async (predicate, ms = 2000) => {
  const deadline = Date.now() + ms;
  while (!predicate()) {
    if (Date.now() > deadline) throw new Error("timed out waiting for the export");
    await new Promise((resolve) => setTimeout(resolve, 5));
  }
};

const ENDPOINT = "http://cetra.test:4317";

// Runs `run` with the plugin's config in a temp XDG_CONFIG_HOME (unless `configExists` is false)
// and `fetch` replaced by a recorder that collects every POST's parsed body.
const withPlugin = async ({ captureContent, configExists = true }, run) => {
  const dir = mkdtempSync(join(tmpdir(), "cetra-opencode-test-"));
  const previousXdg = process.env.XDG_CONFIG_HOME;
  const previousFetch = globalThis.fetch;
  const posts = [];
  process.env.XDG_CONFIG_HOME = dir;
  if (configExists) {
    mkdirSync(join(dir, "opencode"), { recursive: true });
    writeFileSync(
      join(dir, "opencode", "cetra-opencode.json"),
      JSON.stringify({
        managed_by: "cetra agents setup opencode",
        endpoint: ENDPOINT,
        headers: { Authorization: "Bearer k" },
        capture_content: captureContent,
      }),
    );
  }
  globalThis.fetch = async (url, init) => {
    posts.push({ url: String(url), body: JSON.parse(init.body) });
    return { ok: true };
  };
  try {
    await run(posts);
  } finally {
    if (previousXdg === undefined) delete process.env.XDG_CONFIG_HOME;
    else process.env.XDG_CONFIG_HOME = previousXdg;
    globalThis.fetch = previousFetch;
    rmSync(dir, { recursive: true, force: true });
  }
};

// A fake v2 plugin context: it records the hooks OpenCode would register and hands the events
// the test pushes to the plugin's subscription.
const fakeCtx = () => {
  const hooks = new Map();
  const subscriptions = [];
  const queue = [];
  let wake = null;
  const ctx = {
    session: { hook: (name, handler) => hooks.set(name, handler) },
    event: {
      subscribe: (options) => {
        subscriptions.push(options);
        return (async function* () {
          for (;;) {
            while (queue.length) yield queue.shift();
            await new Promise((resolve) => {
              wake = resolve;
            });
          }
        })();
      },
    },
  };
  return { ctx, hooks, subscriptions, push: (event) => { queue.push(event); wake?.(); } };
};

const spansOf = (post) => post.body.resourceSpans[0].scopeSpans[0].spans;
const usageOf = (span) =>
  Object.fromEntries(
    span.attributes.filter((a) => a.key.startsWith("gen_ai.usage.")).map((a) => [a.key, a.value.intValue]),
  );

// Asserts the shape both builds give one happy turn: one POST, one trace, the chat and the tool
// under invoke_agent, and the token counts on the chat span.
const assertOneTurn = (posts, toolName) => {
  assert.equal(posts.length, 1);
  assert.equal(posts[0].url, `${ENDPOINT}/v1/traces`);
  const spans = spansOf(posts[0]);
  assert.deepEqual(
    spans.map((span) => span.name).sort(),
    [`chat gpt-4o-mini`, `execute_tool ${toolName}`, "invoke_agent"].sort(),
  );
  assert.equal(new Set(spans.map((span) => span.traceId)).size, 1);
  const root = spans.find((span) => span.name === "invoke_agent");
  const chat = spans.find((span) => span.name === "chat gpt-4o-mini");
  const tool = spans.find((span) => span.name === `execute_tool ${toolName}`);
  assert.equal(chat.parentSpanId, root.spanId);
  assert.equal(tool.parentSpanId, root.spanId);
  assert.equal(usageOf(chat)["gen_ai.usage.input_tokens"], "82");
  assert.equal(usageOf(chat)["gen_ai.usage.output_tokens"], "17");
  return { root, chat, tool };
};

// Drives setup (v2) through one turn with a tool call, up to (and excluding) the turn's end
// event, which each test pushes itself.
const driveV2Turn = async (hooks, push, sessionID, toolEndEvent) => {
  await hooks.get("context")({
    sessionID,
    system: [],
    messages: [{ role: "user", content: [{ type: "text", text: "read hello.txt" }] }],
  });
  push({ type: "session.execution.started", data: { sessionID }, created: 1000 });
  push({
    type: "session.step.started",
    data: { sessionID, assistantMessageID: `${sessionID}-m1`, model: { providerID: "openai", id: "gpt-4o-mini" } },
    created: 1001,
  });
  push({
    type: "session.text.ended",
    data: { assistantMessageID: `${sessionID}-m1`, text: "Checking." },
    created: 1002,
  });
  push({ type: "session.tool.input.started", data: { id: `${sessionID}-call`, name: "shell" }, created: 1002 });
  push({
    type: "session.tool.called",
    data: {
      id: `${sessionID}-call`,
      assistantMessageID: `${sessionID}-m1`,
      sessionID,
      input: { command: "cat hello.txt" },
    },
    created: 1003,
  });
  push({
    type: "session.step.ended",
    data: { assistantMessageID: `${sessionID}-m1`, tokens: { input: 82, output: 17 }, finish: "tool_calls" },
    created: 1004,
  });
  push(toolEndEvent);
};

test("the v2 plugin sends one trace per turn", async () => {
  await withPlugin({ captureContent: true }, async (posts) => {
    const { ctx, hooks, subscriptions, push } = fakeCtx();
    const dispose = await cetraOpenCode.setup(ctx);
    assert.equal(typeof dispose, "function");
    assert.equal(hooks.size, 1);
    assert.ok(hooks.has("context"));
    assert.equal(subscriptions.length, 1);
    await driveV2Turn(hooks, push, "s1", {
      type: "session.tool.success",
      data: { id: "s1-call", content: [{ text: "hello from cetra" }] },
      created: 1005,
    });
    push({ type: "session.execution.succeeded", data: { sessionID: "s1" }, created: 1006 });
    await waitFor(() => posts.length === 1);
    const { chat } = assertOneTurn(posts, "shell");
    // Content capture on: the input the context hook saw rides the chat span.
    assert.ok(chat.attributes.some((a) => a.key === "gen_ai.input.messages"));
    await dispose();
    assert.equal(posts.length, 1);
  });
});

test("the v2 plugin replaces error text with fixed strings when content is off", async () => {
  await withPlugin({ captureContent: false }, async (posts) => {
    const { ctx, hooks, push } = fakeCtx();
    await cetraOpenCode.setup(ctx);
    await driveV2Turn(hooks, push, "s2", {
      // An error whose class name is a plain identifier: the class name leaves, the message does not.
      type: "session.tool.failed",
      data: { id: "s2-call", content: [], error: { name: "ShellError", message: "cat: no such file" } },
      created: 1005,
    });
    // A model call that fails without a class name gets the handler's fixed string.
    push({
      type: "session.step.started",
      data: { sessionID: "s2", assistantMessageID: "s2-m2", model: { providerID: "openai", id: "gpt-4o-mini" } },
      created: 1007,
    });
    push({
      type: "session.step.failed",
      data: { assistantMessageID: "s2-m2", tokens: { input: 9, output: 0 }, error: { message: "rate limited" } },
      created: 1008,
    });
    push({
      type: "session.execution.failed",
      data: { sessionID: "s2", error: { message: "the agent crashed" } },
      created: 1009,
    });
    await waitFor(() => posts.length === 1);
    const spans = spansOf(posts[0]);
    const tool = spans.find((span) => span.name === "execute_tool shell");
    assert.equal(tool.status.code, 2);
    assert.equal(tool.status.message, "ShellError");
    const chats = spans.filter((span) => span.name === "chat gpt-4o-mini");
    assert.equal(chats[1].status.message, "model call failed");
    const root = spans.find((span) => span.name === "invoke_agent");
    assert.equal(root.status.message, "failed");
    assert.ok(!JSON.stringify(posts[0].body).includes("no such file"));
    assert.ok(!JSON.stringify(posts[0].body).includes("rate limited"));
    assert.ok(!JSON.stringify(posts[0].body).includes("the agent crashed"));
  });
});

test("the v2 plugin keeps the error message with content on", async () => {
  await withPlugin({ captureContent: true }, async (posts) => {
    const { ctx, hooks, push } = fakeCtx();
    await cetraOpenCode.setup(ctx);
    await driveV2Turn(hooks, push, "s3", {
      type: "session.tool.failed",
      data: { id: "s3-call", content: [], error: { message: "cat: no such file" } },
      created: 1005,
    });
    push({ type: "session.execution.succeeded", data: { sessionID: "s3" }, created: 1007 });
    await waitFor(() => posts.length === 1);
    const tool = spansOf(posts[0]).find((span) => span.name === "execute_tool shell");
    assert.equal(tool.status.code, 2);
    assert.equal(tool.status.message, "cat: no such file");
  });
});

test("a v2 setup that throws resolves to a no-op disposer", async () => {
  await withPlugin({ captureContent: true }, async (posts) => {
    const ctx = {
      session: {
        hook: () => {
          throw new Error("no hooks for you");
        },
      },
      event: { subscribe: () => {} },
    };
    const dispose = await cetraOpenCode.setup(ctx);
    assert.equal(typeof dispose, "function");
    await dispose();
    assert.equal(posts.length, 0);
  });
});

test("without a config file nothing is registered and nothing is sent", async () => {
  await withPlugin({ captureContent: true, configExists: false }, async (posts) => {
    const { ctx, hooks, subscriptions } = fakeCtx();
    const dispose = await cetraOpenCode.setup(ctx);
    assert.equal(typeof dispose, "function");
    assert.equal(hooks.size, 0);
    assert.equal(subscriptions.length, 0);
    assert.deepEqual(await cetraOpenCode.server({}), {});
    await dispose();
    assert.equal(posts.length, 0);
  });
});

test("the v1 plugin sends one trace per turn", async () => {
  await withPlugin({ captureContent: true }, async (posts) => {
    const handlers = await cetraOpenCode.server({});
    assert.deepEqual(
      Object.keys(handlers).sort(),
      ["chat.message", "event", "tool.execute.after", "tool.execute.before"],
    );
    await handlers["chat.message"]({ sessionID: "v1s" });
    await handlers.event({
      event: {
        type: "message.part.updated",
        properties: { part: { type: "text", id: "p1", messageID: "m1", text: "Let me check." } },
      },
    });
    await handlers["tool.execute.before"](
      { sessionID: "v1s", callID: "call_1", tool: "bash" },
      { args: { command: "cat hello.txt" } },
    );
    await handlers["tool.execute.after"]({ callID: "call_1" }, { output: "hello from cetra" });
    await handlers.event({
      event: {
        type: "message.updated",
        properties: {
          info: {
            id: "m1",
            role: "assistant",
            sessionID: "v1s",
            providerID: "openai",
            modelID: "gpt-4o-mini",
            time: { created: 3000, completed: 3005 },
            tokens: { input: 82, output: 17 },
            finish: "stop",
          },
        },
      },
    });
    // v1's session.idle awaits the turn's flush, so the POST is there once the handler resolves.
    await handlers.event({ event: { type: "session.idle", properties: { sessionID: "v1s" } } });
    const { chat } = assertOneTurn(posts, "bash");
    assert.ok(chat.attributes.some((a) => a.key === "gen_ai.output.messages"));
  });
});

test("the v1 plugin replaces error text with fixed strings when content is off", async () => {
  await withPlugin({ captureContent: false }, async (posts) => {
    const handlers = await cetraOpenCode.server({});
    await handlers["chat.message"]({ sessionID: "v1e" });
    await handlers["tool.execute.before"](
      { sessionID: "v1e", callID: "call_e", tool: "bash" },
      { args: { command: "cat missing.txt" } },
    );
    // A string error that is not a plain identifier becomes the handler's fixed string.
    await handlers.event({
      event: {
        type: "message.part.updated",
        properties: { part: { type: "tool", callID: "call_e", state: { status: "error", error: "command not found" } } },
      },
    });
    await handlers.event({
      event: {
        type: "message.updated",
        properties: {
          info: {
            id: "me",
            role: "assistant",
            sessionID: "v1e",
            providerID: "openai",
            modelID: "gpt-4o-mini",
            time: { created: 4000, completed: 4005 },
            tokens: { input: 9, output: 0 },
            error: { name: "APIError", message: "rate limited" },
          },
        },
      },
    });
    await handlers.event({ event: { type: "session.idle", properties: { sessionID: "v1e" } } });
    assert.equal(posts.length, 1);
    const spans = spansOf(posts[0]);
    const tool = spans.find((span) => span.name === "execute_tool bash");
    assert.equal(tool.status.code, 2);
    assert.equal(tool.status.message, "tool failed");
    const chat = spans.find((span) => span.name === "chat gpt-4o-mini");
    assert.equal(chat.status.message, "APIError");
    assert.ok(!JSON.stringify(posts[0].body).includes("command not found"));
    assert.ok(!JSON.stringify(posts[0].body).includes("rate limited"));
  });
});

test("a v1 model call outside any turn is not recorded", async () => {
  await withPlugin({ captureContent: true }, async (posts) => {
    const handlers = await cetraOpenCode.server({});
    // message.updated alone does not start a turn: only chat.message (v1) does.
    await handlers.event({
      event: {
        type: "message.updated",
        properties: {
          info: {
            id: "orphan",
            role: "assistant",
            sessionID: "no-turn",
            providerID: "openai",
            modelID: "gpt-4o-mini",
            time: { created: 5000, completed: 5005 },
            tokens: { input: 1, output: 1 },
          },
        },
      },
    });
    // The session going idle flushes nothing, because no turn was ever started.
    await handlers.event({ event: { type: "session.idle", properties: { sessionID: "no-turn" } } });
    assert.equal(posts.length, 0);
  });
});

test("a missing sessionID starts no turn in either build", async () => {
  await withPlugin({ captureContent: true }, async (posts) => {
    // v1: chat.message without a sessionID neither throws nor starts a turn, and the
    // idle event that follows flushes nothing.
    const handlers = await cetraOpenCode.server({});
    await handlers["chat.message"]({});
    await handlers.event({ event: { type: "session.idle", properties: {} } });
    assert.equal(posts.length, 0);

    // v2: session.step.started without a sessionID neither throws nor starts a turn;
    // nothing it opened is ever flushed, because no turn ever ends.
    const { ctx, hooks, push } = fakeCtx();
    const dispose = await cetraOpenCode.setup(ctx);
    await hooks.get("context")({ messages: [] });
    push({
      type: "session.step.started",
      data: { assistantMessageID: "m-no-session", model: { providerID: "openai", id: "gpt-4o-mini" } },
      created: 1000,
    });
    await dispose();
    assert.equal(posts.length, 0);
  });
});

test("the buffer of ended spans is flushed at two thousand", async () => {
  await withPlugin({ captureContent: true }, async (posts) => {
    const { ctx, push } = fakeCtx();
    await cetraOpenCode.setup(ctx);
    // 2001 model calls without a turn end: the 2001st span finds a full buffer and
    // flushes it, so exactly one POST leaves, with the first 2000 spans.
    for (let i = 0; i < 2001; i++) {
      push({
        type: "session.step.started",
        data: {
          sessionID: "cap",
          assistantMessageID: `m${i}`,
          model: { providerID: "openai", id: "gpt-4o-mini" },
        },
        created: 1000 + i,
      });
      push({
        type: "session.step.ended",
        data: { assistantMessageID: `m${i}`, tokens: { input: 1, output: 1 }, finish: "stop" },
        created: 1001 + i,
      });
    }
    await waitFor(() => posts.length === 1);
    assert.equal(spansOf(posts[0]).length, 2000);
  });
});

test("configPath takes the config from XDG_CONFIG_HOME", () => {
  assert.equal(configPath({ XDG_CONFIG_HOME: "/x" }), "/x/opencode/cetra-opencode.json");
});

test("configPath falls back to HOME/.config when XDG_CONFIG_HOME is empty", () => {
  assert.equal(
    configPath({ XDG_CONFIG_HOME: "", HOME: "/h" }),
    "/h/.config/opencode/cetra-opencode.json",
  );
});

test("configPath is null when neither variable is set", () => {
  assert.equal(configPath({}), null);
  assert.equal(configPath({ XDG_CONFIG_HOME: "", HOME: "" }), null);
});

test("parseConfig reads the object setup writes", () => {
  const written = JSON.stringify({
    managed_by: "cetra agents setup opencode",
    endpoint: "https://cetra.example.com",
    headers: { Authorization: "Bearer k" },
    capture_content: true,
  });
  assert.deepEqual(parseConfig(written), {
    endpoint: "https://cetra.example.com",
    headers: { Authorization: "Bearer k" },
    captureContent: true,
  });
});

test("parseConfig is null without an endpoint", () => {
  assert.equal(parseConfig(JSON.stringify({ headers: { Authorization: "Bearer k" } })), null);
});

test("parseConfig is null when the endpoint is blank", () => {
  assert.equal(parseConfig(JSON.stringify({ endpoint: "  " })), null);
});

test("parseConfig is null for text that is not JSON", () => {
  assert.equal(parseConfig("not json"), null);
});

test("parseConfig is null for a JSON array", () => {
  assert.equal(parseConfig("[]"), null);
});

test("parseConfig drops a header that is not a string", () => {
  const parsed = parseConfig(
    JSON.stringify({ endpoint: "https://x", headers: { Authorization: "Bearer k", retries: 3 } }),
  );
  assert.deepEqual(parsed, { endpoint: "https://x", headers: { Authorization: "Bearer k" }, captureContent: false });
});

test("parseConfig reads a missing headers object as empty", () => {
  assert.deepEqual(parseConfig(JSON.stringify({ endpoint: "https://x" })).headers, {});
});

test("parseConfig is false for a string capture_content", () => {
  const parsed = parseConfig(JSON.stringify({ endpoint: "https://x", capture_content: "true" }));
  assert.equal(parsed.captureContent, false);
});

test("parseConfig drops the trailing slash of the endpoint", () => {
  assert.equal(parseConfig(JSON.stringify({ endpoint: "https://x/" })).endpoint, "https://x");
});

test("usageAttributes adds the cache to the input and reports it apart", () => {
  const attrs = usageAttributes({ input: 100, output: 20, reasoning: 5, cache: { read: 30, write: 10 } });
  assert.deepEqual(attrs, [
    { key: "gen_ai.usage.input_tokens", value: { intValue: "140" } },
    { key: "gen_ai.usage.output_tokens", value: { intValue: "20" } },
    { key: "gen_ai.usage.reasoning.output_tokens", value: { intValue: "5" } },
    { key: "gen_ai.usage.cache_read.input_tokens", value: { intValue: "30" } },
    { key: "gen_ai.usage.cache_creation.input_tokens", value: { intValue: "10" } },
  ]);
});

test("usageAttributes reads a missing count as zero", () => {
  const attrs = usageAttributes({ input: 100, output: 20 });
  assert.deepEqual(
    attrs.map((attr) => attr.value.intValue),
    ["100", "20", "0", "0", "0"],
  );
});

test("usageAttributes is empty without tokens", () => {
  assert.deepEqual(usageAttributes(undefined), []);
});

test("finishReason maps the two dashed values", () => {
  assert.equal(finishReason("tool-calls"), "tool_calls");
  assert.equal(finishReason("content-filter"), "content_filter");
});

test("finishReason keeps any other value and drops undefined", () => {
  assert.equal(finishReason("stop"), "stop");
  assert.equal(finishReason("other"), "other");
  assert.equal(finishReason(undefined), undefined);
});
