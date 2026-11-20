import { screen, waitFor, within } from "@testing-library/react";
import { HttpResponse, http } from "msw";
import { afterEach, beforeEach, expect, it } from "vitest";
import { setCurrentConversation } from "@/lib/assistant/panel";
import { server } from "@/mocks/node";
import { typeMessage } from "@/test/assistant";
import { renderApp } from "@/test/render";
import { turnStream } from "@/test/turn-stream";
import conversation from "../../../../fixtures/api/responses/assistant-conversations/01900000-0000-7000-8000-000000001a01.json";

const DESKTOP_WIDTH = window.innerWidth;
const REF = "acme/support-bot";
const CONVERSATION_ID = "01900000-0000-7000-8000-000000001a01";
const ITEM = "/api/v1/projects/:project/assistant/conversations/:id";
const TURNS = "/api/v1/projects/:project/assistant/conversations/:id/turns";
const FEEDBACK = "/api/v1/projects/:project/assistant/conversations/:id/messages/:seq/feedback";
const TRACE = "0af7651916cd43dd8448eb211c80319c";
const FILTER = "status = 'error'";
const TRACES_URL = "/acme/support-bot/traces?filter=status%20%3D%20%27error%27";
type User = Awaited<ReturnType<typeof renderApp>>["user"];
const STAMP = "2026-01-02T00:00:00.000000000Z";

/** The messages the server stores after a turn, added after the fixture's four. */
let stored: unknown[] = [];
let reads = 0;

function storedTurn(
  userText: string,
  replies: { role: "assistant" | "tool"; parts: unknown[]; status?: string }[],
): unknown[] {
  return [
    {
      seq: 5,
      role: "user",
      parts: [{ type: "text", content: userText }],
      status: "complete",
      created_at: STAMP,
    },
    ...replies.map((reply, index) => ({
      seq: 6 + index,
      role: reply.role,
      parts: reply.parts,
      status: reply.status ?? "complete",
      turn_trace_id: TRACE,
      created_at: STAMP,
    })),
  ];
}

const START = { event: "start", data: { turn_id: 5, trace_id: TRACE } };
const DONE = {
  event: "done",
  data: { finish_reason: "stop", usage: { input: 10, output: 20 }, cost_usd: null, steps: 1 },
};

beforeEach(() => {
  window.innerWidth = 1280;
  stored = [];
  reads = 0;
  setCurrentConversation(REF, CONVERSATION_ID);
  server.use(
    http.get(ITEM, () => {
      reads += 1;
      const base = conversation.messages as unknown[];
      return HttpResponse.json({
        ...conversation,
        message_count: base.length + stored.length,
        messages: [...base, ...stored],
      });
    }),
  );
});

afterEach(() => {
  window.innerWidth = DESKTOP_WIDTH;
  localStorage.clear();
});

/** Opens the panel from the top bar and returns its region. */
async function openPanel(user: User) {
  await user.click(await screen.findByRole("button", { name: "Assistant" }));
  return screen.findByRole("region", { name: "Assistant" });
}

async function ask(user: User, region: HTMLElement, text: string) {
  await typeMessage(user, region, text);
  await user.click(within(region).getByRole("button", { name: "Send" }));
}

it("streams_an_answer", async () => {
  const bodies: unknown[] = [];
  server.use(
    http.post(TURNS, async ({ request }) => {
      bodies.push(await request.json());
      stored = storedTurn("Which traces failed?", [
        {
          role: "assistant",
          parts: [
            {
              type: "tool_call",
              id: "call_9",
              name: "search_traces",
              arguments: { filter: FILTER },
            },
          ],
        },
        {
          role: "tool",
          parts: [
            {
              type: "tool_result",
              id: "call_9",
              name: "search_traces",
              ok: true,
              size_bytes: 812,
              rows: 3,
              stubbed: false,
            },
          ],
        },
        { role: "assistant", parts: [{ type: "text", content: "Three traces failed." }] },
      ]);
      return turnStream([
        START,
        { event: "text", data: { delta: "Searching. " } },
        {
          event: "tool_call",
          data: { id: "call_9", name: "search_traces", arguments: { filter: FILTER } },
        },
        {
          event: "tool_result",
          data: { id: "call_9", ok: true, summary: { size_bytes: 812, rows: 3 } },
        },
        { event: "text", data: { delta: "Three traces failed." } },
        DONE,
      ]);
    }),
  );
  const { user } = await renderApp(TRACES_URL);
  const region = await openPanel(user);

  await ask(user, region, "Which traces failed?");

  expect(await within(region).findByText("Three traces failed.")).toBeInTheDocument();
  expect(within(region).getByText("Searched traces")).toBeInTheDocument();
  expect(bodies[0]).toMatchObject({
    message: "Which traces failed?",
    context: { page: "traces", filter: FILTER },
  });
  await waitFor(() => {
    expect(reads).toBeGreaterThan(1);
  });
});

it("removed_chip_is_not_sent", async () => {
  const bodies: unknown[] = [];
  server.use(
    http.post(TURNS, async ({ request }) => {
      bodies.push(await request.json());
      return turnStream([START, { event: "text", data: { delta: "Done." } }, DONE]);
    }),
  );
  const { user } = await renderApp(TRACES_URL);
  const region = await openPanel(user);

  await user.click(await within(region).findByRole("button", { name: "Remove filter" }));
  expect(within(region).queryByRole("button", { name: "Remove filter" })).not.toBeInTheDocument();
  await ask(user, region, "Why?");

  await within(region).findByText("Done.");
  const body = bodies[0] as { context: Record<string, unknown> };
  expect(body.context).toMatchObject({ page: "traces" });
  expect(body.context).not.toHaveProperty("filter");
});

it("stop_aborts", async () => {
  let release: () => void = () => undefined;
  const hold = new Promise<void>((resolve) => {
    release = resolve;
  });
  let signal: AbortSignal | undefined;
  server.use(
    http.post(TURNS, ({ request }) => {
      signal = request.signal;
      stored = storedTurn("A long question", [
        { role: "assistant", parts: [{ type: "text", content: "Partial" }], status: "cancelled" },
      ]);
      return turnStream([START, { event: "text", data: { delta: "Partial" } }], { hold });
    }),
  );
  const { user } = await renderApp(TRACES_URL);
  const region = await openPanel(user);

  await ask(user, region, "A long question");
  await within(region).findByText("Partial");
  await user.click(within(region).getByRole("button", { name: "Stop" }));

  await waitFor(() => {
    expect(signal?.aborted).toBe(true);
  });
  expect(await within(region).findByText("Stopped")).toBeInTheDocument();
  release();
});

it("error_event_shows_retry", async () => {
  const bodies: { message: string }[] = [];
  server.use(
    http.post(TURNS, async ({ request }) => {
      const body = (await request.json()) as { message: string };
      bodies.push(body);
      if (bodies.length === 1) {
        return turnStream([
          START,
          {
            event: "error",
            data: { class: "retryable", message: "The model is busy. Try again." },
          },
        ]);
      }
      stored = storedTurn("Why?", [
        { role: "assistant", parts: [{ type: "text", content: "Here is the answer." }] },
      ]);
      return turnStream([START, { event: "text", data: { delta: "Here is the answer." } }, DONE]);
    }),
  );
  const { user } = await renderApp(TRACES_URL);
  const region = await openPanel(user);

  await ask(user, region, "Why?");
  expect(await within(region).findByText("The model is busy. Try again.")).toBeInTheDocument();
  await user.click(within(region).getByRole("button", { name: "Retry" }));

  expect(await within(region).findByText("Here is the answer.")).toBeInTheDocument();
  expect(bodies).toHaveLength(2);
  expect(bodies[1]?.message).toBe("Why?");
});

it("busy_409_shows_detail", async () => {
  server.use(
    http.post(TURNS, () =>
      HttpResponse.json(
        {
          status: 409,
          title: "Conflict",
          code: "conflict",
          detail: "a turn is already running in this conversation",
        },
        { status: 409 },
      ),
    ),
  );
  const { user } = await renderApp(TRACES_URL);
  const region = await openPanel(user);

  await ask(user, region, "Again?");

  expect(
    await within(region).findByText("a turn is already running in this conversation"),
  ).toBeInTheDocument();
  expect(within(region).getByRole("textbox", { name: "Message the assistant" })).toHaveValue(
    "Again?",
  );
});

it("rate_limited_429", async () => {
  server.use(
    http.post(TURNS, () =>
      HttpResponse.json(
        { status: 429, title: "Too many requests", code: "rate_limited" },
        { status: 429, headers: { "Retry-After": "12" } },
      ),
    ),
  );
  const { user } = await renderApp(TRACES_URL);
  const region = await openPanel(user);

  await ask(user, region, "Once more");

  expect(
    await within(region).findByText("Too many questions. Try again in 12 seconds."),
  ).toBeInTheDocument();
});

it("thumbs_up_sends_feedback", async () => {
  const puts: { path: string; body: unknown }[] = [];
  server.use(
    http.put(FEEDBACK, async ({ request }) => {
      puts.push({ path: new URL(request.url).pathname, body: await request.json() });
      return new HttpResponse(null, { status: 204 });
    }),
  );
  const { user } = await renderApp(TRACES_URL);
  const region = await openPanel(user);

  await user.click(await within(region).findByRole("button", { name: "Thumbs up" }));

  await waitFor(() => {
    expect(puts).toHaveLength(1);
  });
  expect(puts[0]?.path).toMatch(/\/messages\/4\/feedback$/);
  expect(puts[0]?.body).toEqual({ feedback: "up" });
});

it("links_and_images", async () => {
  const answer =
    "![x](https://evil.example/x.png)\n\n" +
    `[trace a1](/acme/support-bot/traces/${TRACE})\n\n` +
    "[site](https://example.com)";
  server.use(
    http.post(TURNS, () => {
      stored = storedTurn("Show me", [
        { role: "assistant", parts: [{ type: "text", content: answer }] },
      ]);
      return turnStream([START, { event: "text", data: { delta: answer } }, DONE]);
    }),
  );
  const { user } = await renderApp(TRACES_URL);
  const region = await openPanel(user);

  await ask(user, region, "Show me");

  const inApp = await within(region).findByRole("link", { name: "trace a1" });
  expect(inApp).toHaveAttribute("href", `/acme/support-bot/traces/${TRACE}`);
  expect(region.querySelector("img")).toBeNull();
  expect(within(region).getByRole("link", { name: "site" })).toHaveAttribute(
    "rel",
    "noopener noreferrer",
  );
  expect(within(region).getByText("https://example.com")).toBeInTheDocument();
});
