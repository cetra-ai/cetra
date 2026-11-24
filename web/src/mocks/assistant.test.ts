import { beforeEach, describe, expect, it } from "vitest";
import {
  type AssistantBubble,
  applyEvent,
  type Bubble,
  bubblesOf,
} from "@/lib/assistant/transcript";
import {
  type AssistantMessage,
  parseDraft,
  parseTurnEvent,
  type TurnEvent,
} from "@/lib/assistant/types";
import { assistantHandlers, resetAssistantMock, STORIES, scriptFor } from "@/mocks/assistant";
import { createDataset } from "@/mocks/generated/dataset";
import { server } from "@/mocks/node";

const BASE = `${window.location.origin}/api/v1/projects/acme%2Fsupport-bot/assistant`;
const NEW_ID = "01900000-0000-7000-8000-000000001a02";
const OTHER_ID = "01900000-0000-7000-8000-000000001a01";
const DEFAULT_TRACE_ID = "0af7651916cd43dd8448eb211c80319c";
const DEFAULT_TITLE = "Which trace errors spiked?";

type StoredDetail = {
  id: string;
  title: string;
  message_count: number;
  messages: AssistantMessage[];
};

beforeEach(() => {
  resetAssistantMock();
  server.use(...assistantHandlers(0));
});

async function createChat(): Promise<void> {
  const response = await fetch(`${BASE}/conversations`, { method: "POST" });
  expect(response.status).toBe(201);
}

/** Runs one turn and returns the events the stream sent, in order. */
async function ask(
  id: string,
  message: string,
  context?: Record<string, unknown>,
): Promise<TurnEvent[]> {
  const response = await fetch(`${BASE}/conversations/${id}/turns`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ message, context }),
  });
  const text = await response.text();
  const events: TurnEvent[] = [];
  for (const block of text.split("\n\n")) {
    if (block.trim() === "" || block.startsWith(": open")) {
      continue;
    }
    const lines = block.split("\n");
    const name = lines.find((line) => line.startsWith("event: "))?.slice("event: ".length) ?? "";
    const data = lines.find((line) => line.startsWith("data: "))?.slice("data: ".length) ?? "";
    const event = parseTurnEvent(name, data);
    expect(event).not.toBeNull();
    if (event !== null) {
      events.push(event);
    }
  }
  return events;
}

async function detail(id: string): Promise<StoredDetail> {
  const response = await fetch(`${BASE}/conversations/${id}`);
  return (await response.json()) as StoredDetail;
}

/** The events of one kind, typed. */
function eventsNamed<N extends TurnEvent["event"]>(events: TurnEvent[], name: N) {
  return events.filter((event): event is Extract<TurnEvent, { event: N }> => event.event === name);
}

/** The bubble a live turn builds from its events, as the panel shows it before the reload. */
function streamedBubble(events: TurnEvent[]): AssistantBubble {
  return events.reduce<AssistantBubble>((bubble, event) => applyEvent(bubble, event), {
    role: "assistant",
    seqs: [],
    blocks: [],
    status: "streaming",
  });
}

/** A bubble with what the stored form and the streamed form must agree on. */
function comparable(bubble: AssistantBubble) {
  return {
    status: bubble.status,
    traceId: bubble.traceId,
    blocks: bubble.blocks.map((block) =>
      block.type === "step" && block.step.result !== undefined
        ? {
            ...block,
            step: {
              ...block.step,
              result: {
                ok: block.step.result.ok,
                size_bytes: block.step.result.size_bytes,
                rows: block.step.result.rows,
              },
            },
          }
        : block,
    ),
  };
}

function assistantAt(bubbles: Bubble[], index: number): AssistantBubble {
  const bubble = bubbles[index];
  if (bubble?.role !== "assistant") {
    throw new Error(`bubble ${index} is not an assistant bubble`);
  }
  return bubble;
}

function firstStory() {
  const [first] = STORIES;
  if (first === undefined) {
    throw new Error("no stories");
  }
  return first;
}

describe("assistant story conversations", () => {
  it("picks each story's script from the question, trimmed and in any case", async () => {
    for (const story of STORIES) {
      for (const [index, turn] of story.turns.entries()) {
        await createChat();
        const exact = await ask(NEW_ID, turn.question);
        await createChat();
        const loose = await ask(NEW_ID, `  ${turn.question.toUpperCase()}  `);

        expect(loose).toEqual(exact);
        expect(eventsNamed(exact, "start")[0]?.data.trace_id).toBe(story.heroTraceId);
        const call = eventsNamed(exact, "tool_call")[0];
        expect(call?.data.name).toBe(index === 0 ? "search_traces" : "draft_dashboard");
        if (index === 1) {
          expect(eventsNamed(exact, "draft")[0]?.data.kind).toBe("dashboard");
        }
        expect(exact.at(-1)).toMatchObject({ event: "done", data: { finish_reason: "stop" } });
      }
    }
  });

  it("keeps today's script for any other question", async () => {
    await createChat();
    const events = await ask(NEW_ID, "hello");

    expect(events[0]).toEqual({
      event: "start",
      data: { turn_id: 1, trace_id: DEFAULT_TRACE_ID },
    });
    expect(eventsNamed(events, "tool_call").map((event) => event.data.name)).toEqual([
      "search_traces",
      "draft_filter",
    ]);
    expect(eventsNamed(events, "draft")[0]?.data.kind).toBe("filter");
    const texts = eventsNamed(events, "text");
    expect(texts.at(-1)?.data.delta.startsWith("Three traces failed in the last 24 hours")).toBe(
      true,
    );

    const stored = await detail(NEW_ID);
    expect(stored.title).toBe(DEFAULT_TITLE);
    expect(stored.message_count).toBe(6);
    expect(stored.messages.map((message) => message.seq)).toEqual([1, 2, 3, 4, 5, 6]);
    const [question] = stored.messages;
    expect(question?.parts).toEqual([
      { type: "text", content: "Which trace errors spiked in the last 24 hours?" },
    ]);
    expect(question?.context).toEqual({
      page: "traces",
      filter: "status = 'error'",
      range: "24h",
    });

    expect(scriptFor("").title).toBe(DEFAULT_TITLE);
  });

  it.each(STORIES)("stores both turns of $name as they were streamed", async (story) => {
    await createChat();
    const [turn1, turn2] = story.turns;
    const first = await ask(NEW_ID, turn1.question, { page: "traces", range: "24h" });
    const second = await ask(NEW_ID, turn2.question, { page: "dashboard" });

    expect(eventsNamed(first, "start")[0]?.data.turn_id).toBe(1);
    expect(eventsNamed(second, "start")[0]?.data.turn_id).toBe(2);

    const stored = await detail(NEW_ID);
    expect(stored.title).toBe(turn1.question);
    expect(stored.message_count).toBe(8);
    expect(stored.messages.map((message) => message.seq)).toEqual([1, 2, 3, 4, 5, 6, 7, 8]);
    expect(stored.messages.map((message) => message.role)).toEqual([
      "user",
      "assistant",
      "tool",
      "assistant",
      "user",
      "assistant",
      "tool",
      "assistant",
    ]);
    expect(stored.messages[0]?.parts).toEqual([{ type: "text", content: turn1.question }]);
    expect(stored.messages[0]?.context).toEqual({ page: "traces", range: "24h" });
    expect(stored.messages[4]?.parts).toEqual([{ type: "text", content: turn2.question }]);
    expect(stored.messages[4]?.context).toEqual({ page: "dashboard" });

    const bubbles = bubblesOf(stored.messages);
    expect(bubbles.map((bubble) => bubble.role)).toEqual([
      "user",
      "assistant",
      "user",
      "assistant",
    ]);
    expect(bubbles[0]).toMatchObject({ role: "user", text: turn1.question });
    expect(bubbles[2]).toMatchObject({ role: "user", text: turn2.question });
    expect(comparable(assistantAt(bubbles, 1))).toEqual(comparable(streamedBubble(first)));
    expect(comparable(assistantAt(bubbles, 3))).toEqual(comparable(streamedBubble(second)));
    expect(assistantAt(bubbles, 1).traceId).toBe(story.heroTraceId);
    expect(assistantAt(bubbles, 3).status).toBe("complete");

    const draftEvent = eventsNamed(second, "draft")[0];
    const storedDraft = assistantAt(bubbles, 3).blocks.find((block) => block.type === "draft");
    expect(draftEvent).toBeDefined();
    expect(storedDraft).toMatchObject({ type: "draft", draft: draftEvent?.data.draft });
  });

  it("names only traces that exist", async () => {
    const ids = new Set(
      createDataset({ seed: 1, now: Date.parse("2026-11-19T12:00:00Z"), traces: 50 }).traces.map(
        (trace) => trace.trace_id,
      ),
    );
    const expectedRows: Record<string, number> = {
      support: 6,
      coding: 1,
      voice: 12,
      extraction: 3,
    };

    for (const story of STORIES) {
      expect(ids.has(story.heroTraceId)).toBe(true);

      const stored = await detail(story.conversationId);
      const search = stored.messages[2]?.parts[0] as Record<string, unknown> | undefined;
      const rows = (search?.result as { rows: { trace_id: string }[] } | undefined)?.rows ?? [];
      expect(rows.length).toBe(expectedRows[story.name]);
      for (const row of rows) {
        expect(ids.has(row.trace_id)).toBe(true);
      }
      expect(search).toMatchObject({ rows: expectedRows[story.name] });
      expect(rows.some((row) => row.trace_id === story.heroTraceId)).toBe(story.name !== "support");

      const events = await ask(NEW_ID, story.turns[0].question);
      expect(eventsNamed(events, "tool_result")[0]?.data.summary.rows).toBe(
        expectedRows[story.name],
      );
      await createChat();

      for (const message of stored.messages) {
        for (const part of message.parts) {
          if (part.type !== "text") {
            continue;
          }
          for (const match of part.content.matchAll(/\b[0-9a-f]{32}\b/g)) {
            expect(ids.has(match[0])).toBe(true);
          }
        }
      }

      const abbreviated = story.turns[0].answer.match(/a0000000…(\d{4})/);
      if (story.name === "support") {
        expect(abbreviated).toBeNull();
      } else {
        expect(abbreviated?.[1]).toBe(story.heroTraceId.slice(-4));
        expect(story.heroTraceId.startsWith("a0000000")).toBe(true);
      }
    }
  });

  it("lists the four story chats after a new one and before the fixture chat", async () => {
    const storyIds = STORIES.map((story) => story.conversationId);

    const before = (await (await fetch(`${BASE}/conversations`)).json()) as {
      data: Record<string, unknown>[];
    };
    expect(before.data.map((item) => item.id)).toEqual([...storyIds, OTHER_ID]);
    expect(before.data.slice(0, 4).map((item) => item.title)).toEqual(
      STORIES.map((story) => story.turns[0].question),
    );
    expect(before.data.slice(0, 4).map((item) => item.message_count)).toEqual([8, 8, 8, 8]);
    for (const item of before.data) {
      expect(item).not.toHaveProperty("messages");
    }

    await createChat();
    const empty = (await (await fetch(`${BASE}/conversations`)).json()) as {
      data: Record<string, unknown>[];
    };
    expect(empty.data[0]).toMatchObject({ id: NEW_ID, title: "", message_count: 0 });

    await ask(NEW_ID, firstStory().turns[0].question);
    const after = (await (await fetch(`${BASE}/conversations`)).json()) as {
      data: Record<string, unknown>[];
    };
    expect(after.data[0]).toMatchObject({
      id: NEW_ID,
      title: firstStory().turns[0].question,
      message_count: 4,
    });
  });

  it("opens a stored story chat directly", async () => {
    for (const story of STORIES) {
      const stored = await detail(story.conversationId);
      expect(stored.message_count).toBe(8);
      expect(stored.messages).toHaveLength(8);

      const bubbles = bubblesOf(stored.messages);
      expect(bubbles.map((bubble) => bubble.role)).toEqual([
        "user",
        "assistant",
        "user",
        "assistant",
      ]);
      expect(bubbles[0]).toMatchObject({ role: "user", text: story.turns[0].question });

      const last = assistantAt(bubbles, 3);
      expect(last.traceId).toBe(story.heroTraceId);
      expect(last.blocks).toContainEqual(
        expect.objectContaining({ type: "draft", kind: "dashboard" }),
      );
    }

    const missing = await fetch(`${BASE}/conversations/01900000-0000-7000-8000-00000000ffff`);
    expect(missing.status).toBe(404);
  });

  it("drafts dashboards the assistant card accepts", async () => {
    const expected: Record<
      string,
      { name: string; description: string; widgets: string[]; ids: string[] }
    > = {
      support: {
        name: "Answer quality",
        description: "Support answers: quality, volume and cost.",
        widgets: ["Correctness by day", "Answers per day", "Cost per answer", "Answers by model"],
        ids: ["correctness-by-day", "answers-per-day", "cost-per-answer", "answers-by-model"],
      },
      coding: {
        name: "Code agent health",
        description: "Failed runs, cost and duration of the coding agent.",
        widgets: ["Failed runs per day", "Error rate", "Cost per run", "p95 duration"],
        ids: ["failed-runs-per-day", "error-rate", "cost-per-run", "p95-duration"],
      },
      voice: {
        name: "Voice latency",
        description: "How long callers wait for a reply.",
        widgets: ["p95 reply time", "Reply time by day", "Calls over 3 s", "Calls per day"],
        ids: ["p95-reply-time", "reply-time-by-day", "calls-over-3-s", "calls-per-day"],
      },
      extraction: {
        name: "Invoice extraction",
        description: "Invoices read, schema-check failures and cost.",
        widgets: [
          "Invoices per day",
          "Schema check failures",
          "Cost per invoice",
          "Invoices by model",
        ],
        ids: ["invoices-per-day", "schema-check-failures", "cost-per-invoice", "invoices-by-model"],
      },
    };

    for (const story of STORIES) {
      const draft = story.turns[1].draft?.draft;
      expect(draft).toBeDefined();
      const parsed = parseDraft("dashboard", draft);
      expect(parsed).not.toBeNull();
      if (parsed?.kind !== "dashboard") {
        throw new Error("draft did not parse as a dashboard");
      }
      const want = expected[story.name];
      expect(want).toBeDefined();
      if (want === undefined) {
        continue;
      }
      expect(parsed.draft.name).toBe(want.name);
      expect(parsed.draft.description).toBe(want.description);
      const widgets = parsed.draft.definition.widgets;
      expect(widgets.map((widget) => widget.title)).toEqual(want.widgets);
      expect(widgets.map((widget) => widget.id)).toEqual(want.ids);
      expect(new Set(widgets.map((widget) => widget.id)).size).toBe(widgets.length);
      for (const widget of widgets) {
        expect(widget.layout.x + widget.layout.w).toBeLessThanOrEqual(60);
      }
    }
  });

  it("a new chat starts empty", async () => {
    await createChat();
    await ask(NEW_ID, firstStory().turns[0].question);
    await createChat();

    const stored = await detail(NEW_ID);
    expect(stored.messages).toEqual([]);
    expect(stored.message_count).toBe(0);
  });

  it("does not store a turn sent to another chat", async () => {
    const story = firstStory();
    await createChat();
    const events = await ask(story.conversationId, "hello");

    expect(events.length).toBeGreaterThan(0);
    expect((await detail(NEW_ID)).messages).toEqual([]);
    expect((await detail(story.conversationId)).message_count).toBe(8);
  });
});
