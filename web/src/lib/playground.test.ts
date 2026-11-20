import { HttpResponse, http } from "msw";
import { afterEach, expect, it, vi } from "vitest";
import { fieldErrors } from "@/api/problem";
import { setUnauthorizedHandler } from "@/api/session";
import type { Schema } from "@/api/types";
import {
  blockedMessages,
  type Column,
  type Draft,
  draftFromPromptVersion,
  draftFromSpan,
  draftVariables,
  droppedToolDefinitions,
  emptyDraft,
  loadDraft,
  type RunEvent,
  type RunRequest,
  readSse,
  runPlayground,
  saveDraft,
  toChatTemplate,
  toRunRequest,
} from "@/lib/playground";
import { server } from "@/mocks/node";
import { STORAGE_PREFIX } from "@/product";
import versionTwo from "../../../fixtures/api/responses/prompts/support-reply/versions/2.json";

const PROJECT = "acme/support-bot";
const CONNECTION = "01900000-0000-7000-8000-000000000011";
const CONNECTIONS = [{ id: CONNECTION, default_model: "gpt-4o" }];
const TRACE = "a1000000000000000000000000000001";
const SPAN = "b100000000000012";

const BODY: RunRequest = {
  connection_id: CONNECTION,
  model: "gpt-4o",
  messages: [{ role: "user", parts: [{ type: "text", content: "Hi" }] }],
  variables: {},
  placeholders: {},
  render_variables: true,
};

afterEach(() => {
  setUnauthorizedHandler(undefined);
  vi.restoreAllMocks();
});

function streamOf(chunks: (string | Uint8Array)[]): ReadableStream<Uint8Array> {
  const encoder = new TextEncoder();
  return new ReadableStream<Uint8Array>({
    start(controller) {
      for (const chunk of chunks) {
        controller.enqueue(typeof chunk === "string" ? encoder.encode(chunk) : chunk);
      }
      controller.close();
    },
  });
}

function sseResponse(chunks: string[]): Response {
  return new HttpResponse(streamOf(chunks), {
    headers: { "Content-Type": "text/event-stream" },
  });
}

async function collect(body: ReadableStream<Uint8Array>): Promise<{ event: string; data: string }[]> {
  const events: { event: string; data: string }[] = [];
  for await (const event of readSse(body)) {
    events.push(event);
  }
  return events;
}

function draftWith(entries: Draft["entries"]): Draft {
  return { ...emptyDraft(CONNECTION), entries };
}

function column(overrides: Partial<Column> = {}): Column {
  return {
    id: "01900000-0000-7000-8000-000000000021",
    connectionId: CONNECTION,
    model: "",
    temperature: "",
    maxTokens: "",
    ...overrides,
  };
}

function requestOf(draft: Draft, target: Column = column()): RunRequest {
  const result = toRunRequest(draft, target, CONNECTIONS);
  if (!result.ok) {
    throw new Error(JSON.stringify(result.errors));
  }
  return result.request;
}

function errorsOf(draft: Draft, target: Column = column()): Record<string, string> {
  const result = toRunRequest(draft, target, CONNECTIONS);
  if (result.ok) {
    throw new Error("expected the run to be refused");
  }
  return result.errors;
}

it("reads events split across chunks and skips comments and ping", async () => {
  const events = await collect(
    streamOf([
      "event: sta",
      'rt\ndata: {"trace_id":"t1"}\n\n: a comment\n\n',
      "event: ping\ndata: \n\n",
      'event: text\ndata: {"delta":"He"}\n\nevent: text\ndata: {"delta":"llo"}\n\n',
      "event: done\ndata: {}\n\n",
    ]),
  );
  expect(events).toEqual([
    { event: "start", data: '{"trace_id":"t1"}' },
    { event: "text", data: '{"delta":"He"}' },
    { event: "text", data: '{"delta":"llo"}' },
    { event: "done", data: "{}" },
  ]);
});

it("keeps a multi-byte character split across chunks", async () => {
  const bytes = new TextEncoder().encode('event: text\ndata: {"delta":"é"}\n\n');
  const split = bytes.indexOf(0xc3) + 1;
  const events = await collect(streamOf([bytes.slice(0, split), bytes.slice(split)]));
  expect(events).toEqual([{ event: "text", data: '{"delta":"é"}' }]);
});

it("reads CRLF lines, a CR split from its LF, two data lines and the default event name", async () => {
  const events = await collect(
    streamOf([
      "event: text\r\ndata: a\r\ndata: b\r\n\r\n",
      "event: text\r",
      "\ndata: c\r\n\r\n",
      "data: x\n\n",
    ]),
  );
  expect(events).toEqual([
    { event: "text", data: "a\nb" },
    { event: "text", data: "c" },
    { event: "message", data: "x" },
  ]);
});

it("drops a pending event that has no blank line at the end", async () => {
  const events = await collect(streamOf(["event: text\ndata: late\n"]));
  expect(events).toEqual([]);
});

it("posts the run and delivers its events in order", async () => {
  const seen: unknown[] = [];
  server.use(
    http.post("*/api/v1/projects/*/playground/runs", async ({ request }) => {
      seen.push({
        accept: request.headers.get("accept"),
        path: new URL(request.url).pathname,
        body: await request.json(),
      });
      return sseResponse([
        'event: start\ndata: {"trace_id":"t"}\n\n',
        'event: text\ndata: {"delta":"Hi"}\n\n',
        'event: done\ndata: {"finish_reason":"stop"}\n\n',
      ]);
    }),
  );
  const events: RunEvent[] = [];
  await runPlayground(PROJECT, BODY, {
    signal: new AbortController().signal,
    onEvent: (event) => {
      events.push(event);
    },
  });
  expect(events.map((event) => event.event)).toEqual(["start", "text", "done"]);
  expect(events[1]).toEqual({ event: "text", data: { delta: "Hi" } });
  expect(seen).toEqual([
    {
      accept: "text/event-stream",
      path: "/api/v1/projects/acme%2Fsupport-bot/playground/runs",
      body: BODY,
    },
  ]);
});

it("throws the parsed problem of a 400 answer", async () => {
  server.use(
    http.post("*/api/v1/projects/*/playground/runs", () =>
      HttpResponse.json(
        {
          status: 400,
          title: "Bad request",
          code: "bad_request",
          errors: [{ field: "messages[0].parts[0]", message: "absent is not set" }],
        },
        { status: 400 },
      ),
    ),
  );
  let caught: unknown;
  try {
    await runPlayground(PROJECT, BODY, {
      signal: new AbortController().signal,
      onEvent: () => undefined,
    });
  } catch (error) {
    caught = error;
  }
  expect(fieldErrors(caught)).toEqual({ "messages[0].parts[0]": "absent is not set" });
});

it("reports a 401 to the session handler and throws", async () => {
  const handler = vi.fn();
  setUnauthorizedHandler(handler);
  server.use(
    http.post("*/api/v1/projects/*/playground/runs", () =>
      HttpResponse.json({ status: 401, title: "Unauthorized", code: "unauthorized" }, { status: 401 }),
    ),
  );
  let caught: unknown;
  try {
    await runPlayground(PROJECT, BODY, {
      signal: new AbortController().signal,
      onEvent: () => undefined,
    });
  } catch (error) {
    caught = error;
  }
  expect(handler).toHaveBeenCalledWith("/api/v1/projects/acme%2Fsupport-bot/playground/runs");
  expect(caught).toMatchObject({ status: 401 });
});

it("resolves without an error when the signal aborts", async () => {
  const encoder = new TextEncoder();
  server.use(
    http.post(
      "*/api/v1/projects/*/playground/runs",
      () =>
        new HttpResponse(
          new ReadableStream<Uint8Array>({
            start(controller) {
              // The stream never closes: only the abort ends the run.
              controller.enqueue(encoder.encode('event: start\ndata: {"trace_id":"t"}\n\n'));
            },
          }),
          { headers: { "Content-Type": "text/event-stream" } },
        ),
    ),
  );
  const controller = new AbortController();
  const seen: string[] = [];
  await runPlayground(PROJECT, BODY, {
    signal: controller.signal,
    onEvent: (event) => {
      seen.push(event.event);
      controller.abort();
    },
  });
  expect(seen).toEqual(["start"]);
  expect(controller.signal.aborted).toBe(true);
});

it("skips unknown event names and data that is not JSON", async () => {
  server.use(
    http.post("*/api/v1/projects/*/playground/runs", () =>
      sseResponse([
        "event: mystery\ndata: {}\n\n",
        "event: text\ndata: not json\n\n",
        'event: text\ndata: {"delta":"ok"}\n\n',
      ]),
    ),
  );
  const events: RunEvent[] = [];
  await runPlayground(PROJECT, BODY, {
    signal: new AbortController().signal,
    onEvent: (event) => {
      events.push(event);
    },
  });
  expect(events).toEqual([{ event: "text", data: { delta: "ok" } }]);
});

it("takes the model from the connection and the numbers from the column", () => {
  const request = requestOf(emptyDraft(CONNECTION), column({ temperature: "0.5", maxTokens: "256" }));
  expect(request).toMatchObject({
    connection_id: CONNECTION,
    model: "gpt-4o",
    params: { temperature: 0.5, max_tokens: 256 },
    render_variables: true,
  });
  expect(request).not.toHaveProperty("tools");
  expect(request).not.toHaveProperty("tool_choice");
  expect(request).not.toHaveProperty("response_format");
});

it("refuses a temperature that is not a number and a bad completion cap", () => {
  const errors = errorsOf(emptyDraft(CONNECTION), column({ temperature: "abc", maxTokens: "0" }));
  expect(errors.temperature).toBe("Must be a number from 0 to 2");
  expect(errors.maxTokens).toBe("Must be a whole number from 1 to 65536");
});

it("asks for a model when neither the column nor the connection names one", () => {
  const result = toRunRequest(emptyDraft(CONNECTION), column(), [
    { id: CONNECTION, default_model: undefined },
  ]);
  expect(result.ok).toBe(false);
  if (!result.ok) {
    expect(result.errors.model).toBe("Choose a model");
  }
});

it("refuses tools that are not a JSON list of named tools", () => {
  const draft = { ...emptyDraft(CONNECTION), tools: "[{bad" };
  expect(errorsOf(draft).tools).toBe("Not valid JSON");
  const named = { ...emptyDraft(CONNECTION), tools: '[{"name":"bad name","parameters":{}}]' };
  expect(errorsOf(named).tools).toBe("Each tool needs a name of 1 to 64 letters, digits, _ or -");
});

it("sends tools and the tool choice, and the choice only with tools or none", () => {
  const withTools = {
    ...emptyDraft(CONNECTION),
    tools: '[{"name":"lookup","description":"Find it.","parameters":{"type":"object"}}]',
    toolChoice: "auto",
  };
  expect(requestOf(withTools)).toMatchObject({
    tools: [{ name: "lookup", description: "Find it.", parameters: { type: "object" } }],
    tool_choice: "auto",
  });
  const choiceWithoutTools = { ...emptyDraft(CONNECTION), toolChoice: "auto" };
  expect(requestOf(choiceWithoutTools)).not.toHaveProperty("tool_choice");
  const noneWithoutTools = { ...emptyDraft(CONNECTION), toolChoice: "none" };
  expect(requestOf(noneWithoutTools)).toMatchObject({ tool_choice: "none" });
});

it("parses the response format and refuses one without a schema", () => {
  const draft = {
    ...emptyDraft(CONNECTION),
    responseFormat: '{"name":"answer","schema":{"type":"object"},"strict":true}',
  };
  expect(requestOf(draft)).toMatchObject({
    response_format: { name: "answer", schema: { type: "object" }, strict: true },
  });
  const broken = { ...emptyDraft(CONNECTION), responseFormat: '{"name":"answer"}' };
  expect(errorsOf(broken).responseFormat).toContain("Must be an object");
});

it("parses a placeholder's JSON into messages, a blank one into none", () => {
  const draft: Draft = {
    ...draftWith([{ type: "placeholder", name: "history" }]),
    placeholders: {
      history: '[{"role":"user","parts":[{"type":"text","content":"earlier"}]}]',
    },
  };
  expect(requestOf(draft).placeholders).toEqual({
    history: [{ role: "user", parts: [{ type: "text", content: "earlier" }] }],
  });
  expect(requestOf({ ...draft, placeholders: { history: "  " } }).placeholders).toEqual({
    history: [],
  });
  expect(errorsOf({ ...draft, placeholders: { history: "{}" } })["placeholders.history"]).toBe(
    "Must be a JSON list of messages",
  );
});

it("refuses a message with media parts and names the message", () => {
  const draft = draftWith([
    {
      role: "user",
      parts: [
        { type: "text", content: "look" },
        { type: "blob", mime_type: "image/png", data: "AAAA" },
      ],
    },
  ]);
  expect(errorsOf(draft).messages).toBe(
    "Message 1 has blob parts, which the playground cannot send. Remove them to run.",
  );
});

it("sends the variables only when they are rendered", () => {
  const draft: Draft = {
    ...draftWith([{ role: "user", parts: [{ type: "text", content: "Hi {{name}}" }] }]),
    variables: { name: "Ada" },
  };
  expect(requestOf(draft)).toMatchObject({ variables: { name: "Ada" }, render_variables: true });
  expect(requestOf({ ...draft, renderVariables: false })).toMatchObject({
    variables: {},
    render_variables: false,
  });
});

it("lists the variables of the text parts in order of first use", () => {
  const draft = draftWith([
    { role: "system", parts: [{ type: "text", content: "{{a}} {{ b }} {{a}} {% x %}" }] },
    { type: "placeholder", name: "history" },
    { role: "user", parts: [{ type: "text", content: "{{ 1x }} {{ c_2 }}" }] },
  ]);
  expect(draftVariables(draft)).toEqual(["a", "b", "c_2"]);
  expect(draftVariables({ ...draft, renderVariables: false })).toEqual([]);
});

it("names the messages the server refuses", () => {
  const draft = draftWith([
    {
      role: "system",
      parts: [
        { type: "text", content: "Be kind." },
        { type: "reasoning", content: "kept, sent and not forwarded" },
      ],
    },
    { type: "placeholder", name: "history" },
    { role: "user", parts: [{ type: "uri", uri: "https://example.com/a.png" }] },
    { role: "user", parts: [{ type: "tool_call", id: "c1", name: "f" }] },
    { role: "tool", parts: [{ type: "text", content: "no" }] },
    { role: "assistant", parts: [{ type: "tool_call_response", id: "c2", response: 1 }] },
    { role: "tool", parts: [{ type: "tool_call_response", response: 1 }] },
    { role: "assistant", parts: [{ type: "tool_call", id: "c3", name: "f" }] },
  ]);
  expect(blockedMessages(draft)).toEqual([
    { index: 3, kinds: ["uri"] },
    { index: 4, kinds: ["tool_call"] },
    { index: 5, kinds: ["text"] },
    { index: 6, kinds: ["tool_call_response"] },
    { index: 7, kinds: ["tool_call_response"] },
  ]);
});

it("reads a prompt version as a draft", () => {
  const draft = draftFromPromptVersion(
    versionTwo as unknown as Schema<"PromptVersion">,
    "01900000-0000-7000-8000-000000000099",
  );
  expect(draft.entries).toEqual([
    {
      role: "system",
      parts: [
        {
          type: "text",
          content: "You are a friendly support agent for {{ product }}. Answer in {{ language }}.",
        },
      ],
    },
    { type: "placeholder", name: "history" },
    { role: "user", parts: [{ type: "text", content: "{{ question }}" }] },
  ]);
  expect(draft.placeholders).toEqual({ history: "" });
  expect(draft.columns).toEqual([
    {
      id: expect.any(String),
      connectionId: "01900000-0000-7000-8000-000000000099",
      model: "gpt-4o",
      temperature: "0.3",
      maxTokens: "512",
    },
  ]);
  expect(draft.source).toEqual({ prompt: { name: "support-reply", version: 2 } });
  expect(draft.renderVariables).toBe(true);
  expect(draft.tools).toBe("");
});

it("reads a span as a draft, converting its tool definitions", () => {
  const span = {
    trace_id: TRACE,
    span_id: SPAN,
    request_model: "claude-sonnet-4-5",
    model: "claude-sonnet-4-5",
    system_instructions: "Be kind.",
    input: { format: "messages", value: [{ role: "user", parts: [{ type: "text", content: "Hello" }] }] },
    tool_definitions: JSON.stringify([
      { type: "function", function: { name: "lookup", description: "Find it.", parameters: { type: "object" } } },
      { name: "weather", input_schema: { type: "object" } },
      { name: "broken" },
    ]),
    request_params: JSON.stringify({ temperature: 0.2, max_tokens: 300 }),
  } as unknown as Schema<"SpanDetail">;
  const draft = draftFromSpan(span, CONNECTION);
  expect(draft.entries).toEqual([
    { role: "system", parts: [{ type: "text", content: "Be kind." }] },
    { role: "user", parts: [{ type: "text", content: "Hello" }] },
  ]);
  expect(JSON.parse(draft.tools)).toEqual([
    { name: "lookup", description: "Find it.", parameters: { type: "object" } },
    { name: "weather", parameters: { type: "object" } },
  ]);
  expect(droppedToolDefinitions(span)).toBe(1);
  expect(draft.columns).toEqual([
    {
      id: expect.any(String),
      connectionId: CONNECTION,
      model: "claude-sonnet-4-5",
      temperature: "0.2",
      maxTokens: "300",
    },
  ]);
  expect(draft.renderVariables).toBe(false);
  expect(draft.source).toEqual({ span: { trace_id: TRACE, span_id: SPAN } });
});

it("converts a draft to a chat template or names the first message it cannot", () => {
  const withToolCall = draftWith([
    { role: "system", parts: [{ type: "text", content: "Be kind." }] },
    { role: "assistant", parts: [{ type: "tool_call", id: "c", name: "f" }] },
  ]);
  expect(toChatTemplate(withToolCall)).toEqual({ ok: false, blocked: 2 });

  const toolRole = draftWith([
    { role: "tool", parts: [{ type: "tool_call_response", id: "c", response: 1 }] },
  ]);
  expect(toChatTemplate(toolRole)).toEqual({ ok: false, blocked: 1 });

  const textOnly = draftWith([
    { role: "system", parts: [{ type: "text", content: "Be" }] },
    { type: "placeholder", name: "history" },
    {
      role: "user",
      parts: [
        { type: "text", content: "a" },
        { type: "text", content: "b" },
      ],
    },
  ]);
  expect(toChatTemplate(textOnly)).toEqual({
    ok: true,
    template: [
      { role: "system", content: "Be" },
      { type: "placeholder", name: "history" },
      { role: "user", content: "a\nb" },
    ],
  });

  // Reasoning parts are dropped from the template, not refused.
  const withReasoning = draftWith([
    {
      role: "assistant",
      parts: [
        { type: "reasoning", content: "thinking" },
        { type: "text", content: "Answer" },
      ],
    },
  ]);
  expect(toChatTemplate(withReasoning)).toEqual({
    ok: true,
    template: [{ role: "assistant", content: "Answer" }],
  });
});

it("makes an empty draft with a system and a user message and one column", () => {
  const draft = emptyDraft(CONNECTION);
  expect(draft.entries).toEqual([
    { role: "system", parts: [{ type: "text", content: "" }] },
    { role: "user", parts: [{ type: "text", content: "" }] },
  ]);
  expect(draft.columns).toEqual([
    { id: expect.any(String), connectionId: CONNECTION, model: "", temperature: "", maxTokens: "" },
  ]);
  expect(draft.renderVariables).toBe(true);
});

it("keeps a draft per project and keeps it in memory when storage fails", () => {
  const draft = emptyDraft("01900000-0000-7000-8000-000000000011");
  saveDraft("acme/round-trip", draft);
  expect(loadDraft("acme/round-trip")).toEqual(draft);
  expect(localStorage.getItem(`${STORAGE_PREFIX}playground.acme/round-trip`)).toBe(
    JSON.stringify(draft),
  );

  vi.spyOn(Storage.prototype, "setItem").mockImplementation(() => {
    throw new Error("quota");
  });
  vi.spyOn(Storage.prototype, "getItem").mockImplementation(() => {
    throw new Error("blocked");
  });
  const other = { ...draft, tools: "[]" };
  saveDraft("acme/blocked-storage", other);
  expect(loadDraft("acme/blocked-storage")).toEqual(other);
});

it("ignores unknown fields of a stored draft and refuses one that does not fit", () => {
  const key = `${STORAGE_PREFIX}playground.acme/stored`;
  const draft = emptyDraft("01900000-0000-7000-8000-000000000011");
  localStorage.setItem(key, JSON.stringify({ ...draft, extra: 1 }));
  expect(loadDraft("acme/stored")).toEqual(draft);

  localStorage.setItem(key, JSON.stringify({ ...draft, columns: [] }));
  expect(loadDraft("acme/stored")).toBeUndefined();
  localStorage.setItem(key, "not json");
  expect(loadDraft("acme/stored")).toBeUndefined();
  localStorage.removeItem(key);
});
