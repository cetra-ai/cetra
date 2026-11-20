import { screen, waitFor, within } from "@testing-library/react";
import { HttpResponse, http } from "msw";
import { beforeEach, expect, it } from "vitest";
import { server } from "@/mocks/node";
import { emptyDraft, saveDraft } from "@/lib/playground";
import { renderApp } from "@/test/render";
import llmSpanFixture from "../../../../../../fixtures/api/responses/spans/a1000000000000000000000000000001/b100000000000012.json";
import traceFixture from "../../../../../../fixtures/api/responses/traces/a1000000000000000000000000000001.json";
import versionTwoFixture from "../../../../../../fixtures/api/responses/prompts/support-reply/versions/2.json";
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
  connection_id: string;
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
  await user.type(
    await screen.findByRole("textbox", { name: "Text of message 2" }),
    "Hello",
  );
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
  await user.type(second as HTMLElement, "claude-x");
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
  await user.click(text);
  // user-event reads braces as key codes, so the text is pasted.
  await user.paste("Make {{product}} great");
  await user.type(await screen.findByRole("textbox", { name: "product" }), "Cetra");
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
  await first.user.type(
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

  expect(await screen.findByRole("textbox", { name: "Text of message 2" })).toHaveValue(
    SPAN_QUESTION,
  );
  expect(screen.getByRole("textbox", { name: "Text of message 1" })).toHaveValue(SPAN_SYSTEM);
});
