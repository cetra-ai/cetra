import { fireEvent, screen, waitFor, within } from "@testing-library/react";
import type { UserEvent } from "@testing-library/user-event";
import { HttpResponse, http } from "msw";
import { beforeEach, expect, it, vi } from "vitest";
import { type Column, type Draft, emptyDraft, saveDraft } from "@/lib/playground";
import { server } from "@/mocks/node";
import { renderApp } from "@/test/render";
import versionOneFixture from "../../../../../../fixtures/api/responses/prompts/support-reply/versions/1.json";
import versionTwoFixture from "../../../../../../fixtures/api/responses/prompts/support-reply/versions/2.json";
import llmSpanFixture from "../../../../../../fixtures/api/responses/spans/a1000000000000000000000000000001/b100000000000012.json";
import traceFixture from "../../../../../../fixtures/api/responses/traces/a1000000000000000000000000000001.json";
import projectFixture from "../../../../../../fixtures/api/static/project.json";

const CONNECTION_ID = "01900000-0000-7000-8000-000000000400";
const TRACE_ID = "0af7651916cd43dd8448eb211c80319c";
const RUNS = "/api/v1/projects/:project/playground/runs";
const PAGE = "/acme/support-bot/playground";
const PROJECT_KEY = "acme/support-bot";
const FIXTURE_TRACE = "a1000000000000000000000000000001";
const LLM_SPAN = "b100000000000012";
const VERSION = "/api/v1/projects/:project/prompts/:prompt/versions/:version";
const TRACE_URL = "/api/v1/projects/:project/traces/:trace_id";
const SPAN_URL = "/api/v1/projects/:project/traces/:trace_id/spans/:span_id";
const SPAN_SYSTEM = "You are a support agent. Use tools to look up orders.";
const SPAN_QUESTION = "My order 1234 hasn't arrived.";

const DONE = {
  finish_reason: "stop",
  usage: { input: 12, output: 5, cache_read: 0, cache_write: 0, reasoning: 0 },
  cost_usd: 0.0021,
  latency_ms: 420,
  ttft_ms: 180,
  output: { role: "assistant", parts: [{ type: "text", content: "Hi there" }] },
};

type RunBody = {
  model: string;
  messages: unknown[];
  variables: Record<string, string>;
  placeholders: Record<string, unknown>;
  connection_id: string;
  render_variables: boolean;
  source?: { prompt?: { name: string; version: number } };
};

/** A complete answer: each event is `[name, data]`, and the stream ends. */
function answer(...events: [string, unknown][]) {
  const body = events
    .map(([name, data]) => `event: ${name}\ndata: ${JSON.stringify(data)}\n\n`)
    .join("");
  return new HttpResponse(body, { headers: { "Content-Type": "text/event-stream" } });
}

/** A stream that starts and then stays open until the request is cancelled. */
function openStream() {
  const encoder = new TextEncoder();
  const first = `event: start\ndata: ${JSON.stringify({ trace_id: TRACE_ID })}\n\n`;
  const stream = new ReadableStream<Uint8Array>({
    start(controller) {
      controller.enqueue(encoder.encode(first));
    },
  });
  return new HttpResponse(stream, { headers: { "Content-Type": "text/event-stream" } });
}

function projectAs(role: string) {
  return http.get("/api/v1/projects/:project", () =>
    HttpResponse.json({ ...projectFixture, role }),
  );
}

beforeEach(() => {
  localStorage.clear();
});

/**
 * The split pane (react-resizable-panels) cancels pointerdown in jsdom, which has no geometry, so
 * user.click never focuses a field and user.type loses its keys. Focus the field first, then type.
 */
async function typeIn(user: UserEvent, field: HTMLElement, text: string) {
  field.focus();
  await user.keyboard(text);
}

it("runs one column and streams the answer", async () => {
  const bodies: RunBody[] = [];
  server.use(
    http.post(RUNS, async ({ request }) => {
      bodies.push((await request.json()) as RunBody);
      return answer(
        ["start", { trace_id: TRACE_ID }],
        ["text", { delta: "Hi " }],
        ["text", { delta: "there" }],
        ["done", DONE],
      );
    }),
  );
  const { user } = await renderApp(PAGE);
  await typeIn(user, await screen.findByRole("textbox", { name: "Text of message 2" }), "Hello");
  await user.click(screen.getByRole("button", { name: "Run" }));

  expect(await screen.findByText("Hi there")).toBeInTheDocument();
  expect(bodies[0]).toMatchObject({ connection_id: CONNECTION_ID, model: "gpt-5-mini" });
  expect(bodies[0]?.messages).toEqual([
    { role: "system", parts: [{ type: "text", content: "" }] },
    { role: "user", parts: [{ type: "text", content: "Hello" }] },
  ]);
  expect(screen.getByText("$0.0021")).toBeInTheDocument();
  expect(screen.getByText("420ms")).toBeInTheDocument();
  expect(screen.getByRole("link", { name: "Open trace" })).toHaveAttribute(
    "href",
    `/acme/support-bot/traces/${TRACE_ID}`,
  );
});

it("run all sends one request per column", async () => {
  const models: string[] = [];
  server.use(
    http.post(RUNS, async ({ request }) => {
      const body = (await request.json()) as RunBody;
      models.push(body.model);
      return answer(["done", DONE]);
    }),
  );
  const { user } = await renderApp(PAGE);
  await user.click(await screen.findByRole("button", { name: "Add model" }));
  const [, second] = screen.getAllByRole("textbox", { name: "Model" });
  (second as HTMLElement).focus();
  await user.clear(second as HTMLElement);
  await user.keyboard("claude-x");
  await user.click(screen.getByRole("button", { name: "Run all" }));

  await waitFor(() => expect(models.length).toBe(2));
  expect([...models].sort()).toEqual(["claude-x", "gpt-5-mini"]);
});

it("variables are listed and sent", async () => {
  let body: RunBody | undefined;
  server.use(
    http.post(RUNS, async ({ request }) => {
      body = (await request.json()) as RunBody;
      return answer(["done", DONE]);
    }),
  );
  const { user } = await renderApp(PAGE);
  const text = await screen.findByRole("textbox", { name: "Text of message 2" });
  text.focus();
  // user-event reads braces as key codes, so the text is pasted.
  await user.paste("Make {{product}} great");
  await typeIn(user, await screen.findByRole("textbox", { name: "product" }), "Cetra");
  await user.click(screen.getByRole("button", { name: "Run" }));

  await waitFor(() => expect(body).toBeDefined());
  expect(body?.variables).toEqual({ product: "Cetra" });
});

it("a 400 field error lands on the message", async () => {
  server.use(
    http.post(RUNS, () =>
      HttpResponse.json(
        {
          type: "about:blank",
          title: "Invalid run",
          status: 400,
          code: "invalid_run",
          detail: "The run is not valid.",
          errors: [{ field: "messages[0].parts[0]", message: "Unsupported part type" }],
        },
        { status: 400 },
      ),
    ),
  );
  const { user } = await renderApp(PAGE);
  await user.click(await screen.findByRole("button", { name: "Run" }));

  const messages = within(screen.getByRole("list", { name: "Messages" }));
  const [first] = messages.getAllByRole("listitem");
  const firstMessage = within(first as HTMLElement);
  expect(await firstMessage.findByText("Unsupported part type")).toBeInTheDocument();
});

it("an error event is shown", async () => {
  server.use(
    http.post(RUNS, () =>
      answer(
        ["start", { trace_id: TRACE_ID }],
        ["error", { class: "pause", message: "Rate limited" }],
      ),
    ),
  );
  const { user } = await renderApp(PAGE);
  await user.click(await screen.findByRole("button", { name: "Run" }));

  expect(await screen.findByText("Rate limited")).toBeInTheDocument();
  expect(screen.getByText("pause")).toBeInTheDocument();
});

it("stop cancels", async () => {
  let signal: AbortSignal | undefined;
  server.use(
    http.post(RUNS, ({ request }) => {
      signal = request.signal;
      return openStream();
    }),
  );
  const { user } = await renderApp(PAGE);
  await user.click(await screen.findByRole("button", { name: "Run" }));
  await waitFor(() => expect(signal).toBeDefined());
  await user.click(screen.getByRole("button", { name: "Stop" }));

  await waitFor(() => expect(signal?.aborted).toBe(true));
  expect(await screen.findByText("Stopped")).toBeInTheDocument();
});

it("blocked parts disable running", async () => {
  const draft = emptyDraft(CONNECTION_ID);
  draft.entries[1] = {
    role: "user",
    parts: [{ type: "uri", uri: "https://example.com/picture.png" }],
  };
  saveDraft(PROJECT_KEY, draft);
  await renderApp(PAGE);

  const reason = "Message 2 has uri parts, which the playground cannot send. Remove them to run.";
  expect((await screen.findAllByText(reason)).length).toBeGreaterThan(0);
  expect(screen.getByRole("button", { name: "Run" })).toBeDisabled();
});

it("viewer cannot run", async () => {
  server.use(projectAs("viewer"));
  await renderApp(PAGE);

  expect(
    (await screen.findAllByText("Only members can run the playground")).length,
  ).toBeGreaterThan(0);
  expect(screen.getByRole("button", { name: "Run" })).toBeDisabled();
});

it("the draft survives a reload", async () => {
  const first = await renderApp(PAGE);
  await typeIn(
    first.user,
    await screen.findByRole("textbox", { name: "Text of message 2" }),
    "Keep me",
  );
  first.unmount();

  await renderApp(PAGE);
  expect(await screen.findByRole("textbox", { name: "Text of message 2" })).toHaveValue("Keep me");
});

it("opens a prompt version from the URL", async () => {
  server.use(http.get(VERSION, () => HttpResponse.json(versionTwoFixture)));
  const { router } = await renderApp(`${PAGE}?prompt=support-reply&version=2`);

  expect(await screen.findByText("From prompt support-reply v2")).toBeInTheDocument();
  expect(screen.getByRole("textbox", { name: "Text of message 1" })).toHaveValue(
    "You are a friendly support agent for {{ product }}. Answer in {{ language }}.",
  );
  expect(screen.getByRole("textbox", { name: "Name of placeholder 2" })).toHaveValue("history");
  expect(screen.getByRole("textbox", { name: "Text of message 3" })).toHaveValue("{{ question }}");
  expect(screen.getAllByRole("textbox", { name: "Model" })[0]).toHaveValue("gpt-4o");
  // The source keys leave the URL; the draft keeps its source.
  expect((router.state.location.search as Record<string, unknown>).prompt).toBeUndefined();
});

it("opens an LLM span from the URL", async () => {
  server.use(http.get(SPAN_URL, () => HttpResponse.json(llmSpanFixture)));
  const { router } = await renderApp(`${PAGE}?trace=${FIXTURE_TRACE}&span=${LLM_SPAN}`);

  expect(await screen.findByText("From a span")).toBeInTheDocument();
  expect(screen.getByRole("textbox", { name: "Text of message 1" })).toHaveValue(SPAN_SYSTEM);
  expect(screen.getByRole("textbox", { name: "Text of message 2" })).toHaveValue(SPAN_QUESTION);
  expect(screen.getAllByRole("textbox", { name: "Model" })[0]).toHaveValue("claude-sonnet-4-5");
  expect(
    screen.queryByText("This span was cut at ingest; the playground shows what was stored."),
  ).not.toBeInTheDocument();
  expect((router.state.location.search as Record<string, unknown>).span).toBeUndefined();
});

it("shows the banner when the span was cut at ingest", async () => {
  server.use(
    http.get(SPAN_URL, () => HttpResponse.json({ ...llmSpanFixture, truncated: ["cetra.input"] })),
  );
  await renderApp(`${PAGE}?trace=${FIXTURE_TRACE}&span=${LLM_SPAN}`);

  expect(
    await screen.findByText("This span was cut at ingest; the playground shows what was stored."),
  ).toBeInTheDocument();
});

it("reopens a past run from its LLM span", async () => {
  server.use(
    http.get(TRACE_URL, () =>
      HttpResponse.json({
        ...traceFixture,
        spans: traceFixture.spans.filter((row) => row.span.span_id === LLM_SPAN),
      }),
    ),
    http.get(SPAN_URL, () => HttpResponse.json(llmSpanFixture)),
  );
  await renderApp(`${PAGE}?run=${FIXTURE_TRACE}`);

  // The empty draft is shown first: wait for the run's text.
  await waitFor(() =>
    expect(screen.getByRole("textbox", { name: "Text of message 2" })).toHaveValue(SPAN_QUESTION),
  );
  expect(screen.getByRole("textbox", { name: "Text of message 1" })).toHaveValue(SPAN_SYSTEM);
});

it("removing a message keeps each parts editor with its own message", async () => {
  const draft = emptyDraft(CONNECTION_ID);
  draft.entries = [
    { role: "tool", parts: [{ type: "tool_call_response", id: "call-1", response: "first" }] },
    { role: "tool", parts: [{ type: "tool_call_response", id: "call-2", response: "second" }] },
  ];
  saveDraft(PROJECT_KEY, draft);
  const { user } = await renderApp(PAGE);
  await user.click(await screen.findByRole("button", { name: "Remove message 1" }));

  const parts = (await screen.findByRole("textbox", {
    name: "Parts of message 1",
  })) as HTMLTextAreaElement;
  expect(parts.value).toContain("call-2");
  expect(parts.value).not.toContain("call-1");
});

it("a placeholder named like an object property shows an empty text", async () => {
  const draft = emptyDraft(CONNECTION_ID);
  draft.entries = [{ type: "placeholder", name: "toString" }];
  saveDraft(PROJECT_KEY, draft);
  await renderApp(PAGE);

  expect(await screen.findByRole("textbox", { name: "Placeholder toString" })).toHaveValue("");
});

it("renaming a placeholder carries its text to the new name", async () => {
  const text = '[{"role":"user","parts":[{"type":"text","content":"earlier"}]}]';
  const draft = emptyDraft(CONNECTION_ID);
  draft.entries = [
    { role: "user", parts: [{ type: "text", content: "Hi" }] },
    { type: "placeholder", name: "history" },
  ];
  draft.placeholders = { history: text };
  saveDraft(PROJECT_KEY, draft);
  await renderApp(PAGE);

  fireEvent.change(await screen.findByRole("textbox", { name: "Name of placeholder 2" }), {
    target: { value: "chat" },
  });
  expect(await screen.findByRole("textbox", { name: "Placeholder chat" })).toHaveValue(text);
  expect(screen.queryByRole("textbox", { name: "Placeholder history" })).not.toBeInTheDocument();
});

it("a prompt asks before it replaces a draft with content, and cancel keeps the draft", async () => {
  server.use(http.get(VERSION, () => HttpResponse.json(versionTwoFixture)));
  const draft = emptyDraft(CONNECTION_ID);
  draft.entries[1] = { role: "user", parts: [{ type: "text", content: "Keep me" }] };
  saveDraft(PROJECT_KEY, draft);
  const { user } = await renderApp(`${PAGE}?prompt=support-reply&version=2`);

  expect(await screen.findByText("Replace the current draft?")).toBeInTheDocument();
  await user.click(screen.getByRole("button", { name: "Cancel" }));

  await waitFor(() =>
    expect(screen.queryByText("Replace the current draft?")).not.toBeInTheDocument(),
  );
  expect(screen.getByRole("textbox", { name: "Text of message 2" })).toHaveValue("Keep me");
  expect(screen.queryByText("From prompt support-reply v2")).not.toBeInTheDocument();
});

it("the variables switch sets render_variables in the run body", async () => {
  const bodies: RunBody[] = [];
  server.use(
    http.post(RUNS, async ({ request }) => {
      bodies.push((await request.json()) as RunBody);
      return answer(["done", DONE]);
    }),
  );
  const { user } = await renderApp(PAGE);
  const text = await screen.findByRole("textbox", { name: "Text of message 2" });
  text.focus();
  await user.paste("Make {{product}} great");
  await user.click(screen.getByRole("button", { name: "Run" }));
  expect(await screen.findByText("Run finished")).toBeInTheDocument();
  expect(bodies[0]?.render_variables).toBe(true);

  fireEvent.click(screen.getByRole("switch", { name: "Treat {{ }} as variables" }));
  await user.click(screen.getByRole("button", { name: "Run" }));
  await waitFor(() => expect(bodies).toHaveLength(2));
  expect(bodies[1]?.render_variables).toBe(false);
});

it("unmounting the page aborts a run that is still streaming", async () => {
  let signal: AbortSignal | undefined;
  server.use(
    http.post(RUNS, ({ request }) => {
      signal = request.signal;
      return openStream();
    }),
  );
  const { user, unmount } = await renderApp(PAGE);
  await user.click(await screen.findByRole("button", { name: "Run" }));
  await waitFor(() => expect(signal).toBeDefined());
  unmount();

  expect(signal?.aborted).toBe(true);
});

it("a run started after a stop leaves the earlier request aborted", async () => {
  const signals: AbortSignal[] = [];
  server.use(
    http.post(RUNS, ({ request }) => {
      signals.push(request.signal);
      return openStream();
    }),
  );
  const { user } = await renderApp(PAGE);
  await user.click(await screen.findByRole("button", { name: "Run" }));
  await waitFor(() => expect(signals).toHaveLength(1));
  await user.click(screen.getByRole("button", { name: "Stop" }));
  await screen.findByText("Stopped");
  await user.click(screen.getByRole("button", { name: "Run" }));

  await waitFor(() => expect(signals).toHaveLength(2));
  expect(signals[0]?.aborted).toBe(true);
  expect(signals[1]?.aborted).toBe(false);
});

it("a stream that breaks after start says the connection was lost", async () => {
  // The mock server answers whole responses, so this test hands the page a response whose stream
  // breaks after its start event.
  const original = window.fetch;
  const spy = vi.spyOn(window, "fetch").mockImplementation((input, init) => {
    const url = input instanceof Request ? input.url : String(input);
    if (!url.includes("/playground/runs")) {
      return original(input, init);
    }
    const encoder = new TextEncoder();
    const stream = new ReadableStream<Uint8Array>({
      start(controller) {
        controller.enqueue(
          encoder.encode(`event: start\ndata: ${JSON.stringify({ trace_id: TRACE_ID })}\n\n`),
        );
      },
      pull(controller) {
        controller.error(new Error("socket closed"));
      },
    });
    return Promise.resolve(
      new Response(stream, { status: 200, headers: { "Content-Type": "text/event-stream" } }),
    );
  });
  try {
    const { user } = await renderApp(PAGE);
    await user.click(await screen.findByRole("button", { name: "Run" }));

    expect(await screen.findByText("The connection was lost.")).toBeInTheDocument();
    expect(screen.queryByText("Could not start the run.")).not.toBeInTheDocument();
  } finally {
    spy.mockRestore();
  }
});

it("Ctrl+Enter in a message runs every model", async () => {
  const models: string[] = [];
  server.use(
    http.post(RUNS, async ({ request }) => {
      models.push(((await request.json()) as RunBody).model);
      return answer(["done", DONE]);
    }),
  );
  const { user } = await renderApp(PAGE);
  await user.click(await screen.findByRole("button", { name: "Add model" }));
  await typeIn(user, screen.getByRole("textbox", { name: "Text of message 2" }), "Hello");
  await user.keyboard("{Control>}{Enter}{/Control}");

  await waitFor(() => expect(models).toHaveLength(2));
});

it("an unrun column says how to run it", async () => {
  await renderApp(PAGE);
  const column = within(await screen.findByRole("region", { name: "Model 1" }));
  expect(column.getByText("Run to see the answer")).toBeInTheDocument();
});

it("the parameters popover sets the temperature and max tokens", async () => {
  let body: (RunBody & { params?: { temperature?: number; max_tokens?: number } }) | undefined;
  server.use(
    http.post(RUNS, async ({ request }) => {
      body = (await request.json()) as typeof body;
      return answer(["done", DONE]);
    }),
  );
  const { user } = await renderApp(PAGE);
  await user.click(await screen.findByRole("button", { name: "Parameters of model 1" }));
  await typeIn(user, await screen.findByRole("textbox", { name: "Temperature" }), "0.5");
  await typeIn(user, screen.getByRole("textbox", { name: "Max tokens" }), "200");
  await user.keyboard("{Escape}");
  expect(screen.getByRole("button", { name: "Parameters of model 1" })).toHaveTextContent(
    "temp 0.5 · max 200",
  );
  await user.click(screen.getByRole("button", { name: "Run" }));

  await waitFor(() => expect(body).toBeDefined());
  expect(body?.params).toEqual({ temperature: 0.5, max_tokens: 200 });
});

it("empty settings sections start folded and open on click", async () => {
  const { user } = await renderApp(PAGE);
  const tools = await screen.findByRole("button", { name: "Tools" });
  expect(tools).toHaveAttribute("aria-expanded", "false");
  expect(screen.queryByRole("textbox", { name: "Tools" })).not.toBeInTheDocument();
  expect(screen.queryByRole("heading", { name: "Placeholders" })).not.toBeInTheDocument();

  await user.click(tools);
  expect(screen.getByRole("textbox", { name: "Tools" })).toBeInTheDocument();
  expect(screen.getByRole("combobox", { name: "Tool choice" })).toBeInTheDocument();
});

/** A draft from `emptyDraft` whose source is `support-reply` v2, with these columns and messages. */
function promptDraft(columns: Column[], entries?: Draft["entries"]): Draft {
  const base = emptyDraft(CONNECTION_ID);
  return {
    ...base,
    entries: entries ?? base.entries,
    source: { prompt: { name: "support-reply", version: 2 } },
    columns,
  };
}

/** A column with no model or settings, pinned to `version` when it is given. */
function column(id: string, version?: number): Column {
  return {
    id,
    connectionId: CONNECTION_ID,
    model: "",
    temperature: "",
    maxTokens: "",
    ...(version === undefined ? {} : { version }),
  };
}

/**
 * Picks a version in a column's Prompt version select from the keyboard: the split pane cancels
 * pointer events in jsdom, so a click never picks an option. The option's first word is typed to
 * highlight it (`v1` for `v1 · production`, `Draft` for `Draft`).
 */
async function pickVersionIn(user: UserEvent, region: string, option: string) {
  const select = within(screen.getByRole("region", { name: region })).getByRole("combobox", {
    name: "Prompt version",
  });
  select.focus();
  await user.keyboard("{Enter}");
  await screen.findByRole("option", { name: option });
  await user.keyboard(option.split(" ")[0] ?? "");
  await user.keyboard("{Enter}");
  await waitFor(() =>
    expect(screen.queryByRole("option", { name: option })).not.toBeInTheDocument(),
  );
}

it("a prompt draft shows the version select with Draft and the versions", async () => {
  const { user } = await renderApp(`${PAGE}?prompt=support-reply&version=2`);
  const select = await screen.findByRole("combobox", { name: "Prompt version" });
  expect(select).toHaveTextContent("Draft");

  await user.click(select);
  expect(await screen.findByRole("option", { name: "v2 · latest, staging" })).toBeInTheDocument();
  expect(screen.getByRole("option", { name: "v1 · production" })).toBeInTheDocument();
});

it("a span draft shows no version select", async () => {
  server.use(http.get(SPAN_URL, () => HttpResponse.json(llmSpanFixture)));
  await renderApp(`${PAGE}?trace=${FIXTURE_TRACE}&span=${LLM_SPAN}`);

  expect(await screen.findByText("From a span")).toBeInTheDocument();
  expect(screen.queryByRole("combobox", { name: "Prompt version" })).not.toBeInTheDocument();
});

it("a pinned column shows the version's template and runs its messages", async () => {
  const bodies: RunBody[] = [];
  server.use(
    http.post(RUNS, async ({ request }) => {
      bodies.push((await request.json()) as RunBody);
      return answer(["done", DONE]);
    }),
  );
  const { user } = await renderApp(`${PAGE}?prompt=support-reply&version=2`);
  await typeIn(user, await screen.findByRole("textbox", { name: "product" }), "Cetra");
  await typeIn(user, screen.getByRole("textbox", { name: "question" }), "Where is it?");
  await user.click(screen.getByRole("button", { name: "Add model" }));
  await pickVersionIn(user, "Model 2", "v1 · production");

  expect(await screen.findByRole("list", { name: "Template of version 1" })).toBeInTheDocument();
  await user.click(screen.getByRole("button", { name: "Run all" }));
  await waitFor(() => expect(bodies).toHaveLength(2));

  const pinned = bodies.find((body) => body.source?.prompt?.version === 1);
  const draftRun = bodies.find((body) => body.source?.prompt?.version === 2);
  // The body carries the version's entries as they are: the server expands the placeholder.
  expect(pinned?.messages).toEqual([
    {
      role: "system",
      parts: [{ type: "text", content: "You are a support agent for {{ product }}." }],
    },
    { type: "placeholder", name: "history" },
    { role: "user", parts: [{ type: "text", content: "{{ question }}" }] },
  ]);
  expect(pinned?.placeholders).toEqual({ history: [] });
  expect(pinned?.variables).toMatchObject({ product: "Cetra", question: "Where is it?" });
  expect(pinned?.source).toEqual({ prompt: { name: "support-reply", version: 1 } });
  expect(draftRun?.variables).toMatchObject({ product: "Cetra", question: "Where is it?" });
});

it("picking a version applies its config to the column", async () => {
  server.use(
    http.get(VERSION, ({ params }) =>
      String(params.version) === "1"
        ? HttpResponse.json({
            ...versionOneFixture,
            config: { model: "gpt-4o-mini", params: { temperature: 0.9, max_tokens: 256 } },
          })
        : undefined,
    ),
  );
  const { user } = await renderApp(`${PAGE}?prompt=support-reply&version=2`);
  await screen.findByText("From prompt support-reply v2");
  await user.click(screen.getByRole("button", { name: "Add model" }));
  await pickVersionIn(user, "Model 2", "v1 · production");

  const model = within(screen.getByRole("region", { name: "Model 2" })).getByRole("textbox", {
    name: "Model",
  });
  await waitFor(() => expect(model).toHaveValue("gpt-4o-mini"));
  expect(screen.getByRole("button", { name: "Parameters of model 2" })).toHaveTextContent(
    "temp 0.9 · max 256",
  );
});

it("both columns show their tokens, cost and latency", async () => {
  server.use(
    http.post(RUNS, async ({ request }) => {
      const body = (await request.json()) as RunBody;
      return answer(
        body.source?.prompt?.version === 1
          ? [
              "done",
              {
                ...DONE,
                usage: { ...DONE.usage, input: 345 },
                cost_usd: 0.0042,
                latency_ms: 900,
              },
            ]
          : ["done", DONE],
      );
    }),
  );
  const { user } = await renderApp(`${PAGE}?prompt=support-reply&version=2`);
  await screen.findByText("From prompt support-reply v2");
  await user.click(screen.getByRole("button", { name: "Add model" }));
  await pickVersionIn(user, "Model 2", "v1 · production");
  await user.click(screen.getByRole("button", { name: "Run all" }));

  const first = within(await screen.findByRole("region", { name: "Model 1" }));
  const second = within(screen.getByRole("region", { name: "Model 2" }));
  expect(await first.findByText("$0.0021")).toBeInTheDocument();
  expect(await second.findByText("$0.0042")).toBeInTheDocument();
  expect(first.getByText("420ms")).toBeInTheDocument();
  expect(second.getByText("900ms")).toBeInTheDocument();
  expect(first.getByText("12")).toBeInTheDocument();
  expect(second.getByText("345")).toBeInTheDocument();
});

it("the settings list the pinned version's variables", async () => {
  const entries = emptyDraft(CONNECTION_ID).entries;
  entries[1] = { role: "user", parts: [{ type: "text", content: "{{ question }}" }] };
  saveDraft(PROJECT_KEY, promptDraft([column("c1", 1)], entries));
  await renderApp(PAGE);

  // Version 1 uses `product`, which the draft's own text does not.
  expect(await screen.findByRole("textbox", { name: "product" })).toBeInTheDocument();
  expect(screen.getByRole("textbox", { name: "question" })).toBeInTheDocument();
});

it("a pinned column's message error stays in the column", async () => {
  server.use(
    http.post(RUNS, () =>
      HttpResponse.json(
        {
          type: "about:blank",
          title: "Invalid run",
          status: 400,
          code: "invalid_run",
          detail: "The run is not valid.",
          errors: [{ field: "messages[0].parts[0]", message: "Unsupported part type" }],
        },
        { status: 400 },
      ),
    ),
  );
  saveDraft(PROJECT_KEY, promptDraft([column("c1", 1)]));
  const { user } = await renderApp(PAGE);
  const region = within(await screen.findByRole("region", { name: "Model 1" }));
  // The pinned version loads first: its Run is disabled until then.
  await region.findByRole("list", { name: "Template of version 1" });
  await user.click(region.getByRole("button", { name: "Run" }));

  expect(
    await within(screen.getByRole("region", { name: "Model 1" })).findByText(
      "Unsupported part type",
    ),
  ).toBeInTheDocument();
  expect(
    within(screen.getByRole("list", { name: "Messages" })).queryByText("Unsupported part type"),
  ).not.toBeInTheDocument();
});

it("a pinned column runs while the draft has a blocked message", async () => {
  const entries = emptyDraft(CONNECTION_ID).entries;
  entries[1] = { role: "user", parts: [{ type: "uri", uri: "https://example.com/picture.png" }] };
  saveDraft(PROJECT_KEY, promptDraft([column("c1"), column("c2", 1)], entries));
  await renderApp(PAGE);

  const draftRun = within(await screen.findByRole("region", { name: "Model 1" })).getByRole(
    "button",
    { name: "Run" },
  );
  const pinned = within(screen.getByRole("region", { name: "Model 2" }));
  await pinned.findByRole("list", { name: "Template of version 1" });
  const pinnedRun = pinned.getByRole("button", { name: "Run" });
  expect(draftRun).toBeDisabled();
  expect(pinnedRun).toBeEnabled();
});

it("a pinned version that fails to load disables its Run with the reason", async () => {
  server.use(
    http.get(VERSION, ({ params }) =>
      String(params.version) === "1"
        ? HttpResponse.json({ title: "Broken", status: 500 }, { status: 500 })
        : undefined,
    ),
  );
  saveDraft(PROJECT_KEY, promptDraft([column("c1"), column("c2", 1)]));
  await renderApp(PAGE);

  expect(await screen.findByText("Could not load version 1.")).toBeInTheDocument();
  expect(
    within(screen.getByRole("region", { name: "Model 2" })).getByRole("button", { name: "Run" }),
  ).toBeDisabled();
});

it("the pin survives a reload", async () => {
  const first = await renderApp(`${PAGE}?prompt=support-reply&version=2`);
  await screen.findByText("From prompt support-reply v2");
  await first.user.click(screen.getByRole("button", { name: "Add model" }));
  await pickVersionIn(first.user, "Model 2", "v1 · production");
  await screen.findByRole("list", { name: "Template of version 1" });
  first.unmount();

  await renderApp(PAGE);
  const select = within(await screen.findByRole("region", { name: "Model 2" })).getByRole(
    "combobox",
    { name: "Prompt version" },
  );
  // The select shows the bare `v1` until the version list loads.
  await waitFor(() => expect(select).toHaveTextContent("v1 · production"));
});

it("changing a column's version clears its answer", async () => {
  server.use(
    http.post(RUNS, () =>
      answer(["start", { trace_id: TRACE_ID }], ["text", { delta: "Hi there" }], ["done", DONE]),
    ),
  );
  saveDraft(PROJECT_KEY, promptDraft([column("c1", 1)]));
  const { user } = await renderApp(PAGE);
  const region = within(await screen.findByRole("region", { name: "Model 1" }));
  await region.findByRole("list", { name: "Template of version 1" });
  await user.click(region.getByRole("button", { name: "Run" }));
  expect(await region.findByText("Hi there")).toBeInTheDocument();

  await pickVersionIn(user, "Model 1", "v2 · latest, staging");
  await waitFor(() => expect(region.queryByText("Hi there")).not.toBeInTheDocument());
  expect(region.getByText("Run to see the answer")).toBeInTheDocument();
});

it("a pinned draft asks before a new source replaces it", async () => {
  saveDraft(PROJECT_KEY, promptDraft([column("c1", 1)]));
  await renderApp(`${PAGE}?prompt=support-reply&version=1`);

  expect(await screen.findByText("Replace the current draft?")).toBeInTheDocument();
});

it("?compare= opens two pinned columns", async () => {
  const { router } = await renderApp(`${PAGE}?prompt=support-reply&version=2&compare=1`);

  // The loaded draft replaces the columns, so each select is queried again until it shows.
  const selectIn = (region: string) =>
    within(screen.getByRole("region", { name: region })).getByRole("combobox", {
      name: "Prompt version",
    });
  await waitFor(() => expect(selectIn("Model 1")).toHaveTextContent("v2 · latest, staging"));
  await waitFor(() => expect(selectIn("Model 2")).toHaveTextContent("v1 · production"));
  expect((router.state.location.search as Record<string, unknown>).compare).toBeUndefined();
});

it("?compare= with a missing version opens the version alone", async () => {
  await renderApp(`${PAGE}?prompt=support-reply&version=2&compare=9`);

  expect(await screen.findByText("From prompt support-reply v2")).toBeInTheDocument();
  expect(await screen.findByRole("region", { name: "Model 1" })).toBeInTheDocument();
  expect(screen.queryByRole("region", { name: "Model 2" })).not.toBeInTheDocument();
});

it("picking a version while its column runs stops the run and clears it", async () => {
  let signal: AbortSignal | undefined;
  server.use(
    http.post(RUNS, ({ request }) => {
      signal = request.signal;
      return openStream();
    }),
  );
  saveDraft(PROJECT_KEY, promptDraft([column("c1"), column("c2")]));
  const { user } = await renderApp(PAGE);
  const region = within(await screen.findByRole("region", { name: "Model 2" }));
  await user.click(region.getByRole("button", { name: "Run" }));
  await waitFor(() => expect(signal).toBeDefined());

  await pickVersionIn(user, "Model 2", "v1 · production");

  await waitFor(() => expect(signal?.aborted).toBe(true));
  expect(await region.findByText("Run to see the answer")).toBeInTheDocument();
});

it("Run all runs the columns that can run", async () => {
  const bodies: RunBody[] = [];
  server.use(
    http.post(RUNS, async ({ request }) => {
      bodies.push((await request.json()) as RunBody);
      return answer(["done", DONE]);
    }),
    http.get(VERSION, ({ params }) =>
      String(params.version) === "1"
        ? HttpResponse.json({ title: "Broken", status: 500 }, { status: 500 })
        : undefined,
    ),
  );
  saveDraft(PROJECT_KEY, promptDraft([column("c1"), column("c2", 1)]));
  const { user } = await renderApp(PAGE);
  await screen.findByText("Could not load version 1.");
  const runAll = screen.getByRole("button", { name: "Run all" });
  expect(runAll).toBeEnabled();

  await user.click(runAll);

  const first = within(screen.getByRole("region", { name: "Model 1" }));
  expect(await first.findByText("$0.0021")).toBeInTheDocument();
  expect(bodies).toHaveLength(1);
  expect(bodies[0]?.source?.prompt?.version).toBe(2);
});

it("picking a loaded version applies its config without fetching it again", async () => {
  let fetches = 0;
  server.use(
    http.get(VERSION, ({ params }) => {
      if (String(params.version) !== "1") {
        return undefined;
      }
      fetches += 1;
      return HttpResponse.json({
        ...versionOneFixture,
        config: { model: "gpt-4o-mini", params: { temperature: 0.9, max_tokens: 256 } },
      });
    }),
  );
  const { user } = await renderApp(`${PAGE}?prompt=support-reply&version=2`);
  await screen.findByText("From prompt support-reply v2");
  await user.click(screen.getByRole("button", { name: "Add model" }));
  const modelOf = () =>
    within(screen.getByRole("region", { name: "Model 2" })).getByRole("textbox", {
      name: "Model",
    });
  await pickVersionIn(user, "Model 2", "v1 · production");
  await waitFor(() => expect(modelOf()).toHaveValue("gpt-4o-mini"));
  await pickVersionIn(user, "Model 2", "Draft");
  await pickVersionIn(user, "Model 2", "v1 · production");

  expect(modelOf()).toHaveValue("gpt-4o-mini");
  expect(fetches).toBe(1);
});
