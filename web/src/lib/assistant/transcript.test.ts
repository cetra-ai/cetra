import { describe, expect, it } from "vitest";
import {
  type AssistantBubble,
  applyEvent,
  type Bubble,
  bubblesOf,
} from "@/lib/assistant/transcript";
import type { AssistantMessage } from "@/lib/assistant/types";
import conversation from "../../../../fixtures/api/responses/assistant-conversations/01900000-0000-7000-8000-000000001a01.json";

const TRACE = "0af7651916cd43dd8448eb211c80319c";

function assistantOf(bubbles: Bubble[]): AssistantBubble {
  const bubble = bubbles.find((item) => item.role === "assistant");
  if (bubble?.role !== "assistant") {
    throw new Error("no assistant bubble");
  }
  return bubble;
}

function message(value: unknown): AssistantMessage {
  return value as AssistantMessage;
}

const empty: AssistantBubble = { role: "assistant", seqs: [], blocks: [], status: "streaming" };

describe("bubblesOf", () => {
  it("builds one user bubble and one assistant bubble from the fixture conversation", () => {
    const bubbles = bubblesOf(conversation.messages as unknown as AssistantMessage[]);
    expect(bubbles).toHaveLength(2);
    expect(bubbles[0]).toMatchObject({
      role: "user",
      seq: 1,
      text: "Why did checkout errors spike yesterday?",
      context: { page: "traces", filter: "status = 'error'", range: "24h" },
    });
    const answer = assistantOf(bubbles);
    expect(answer.seqs).toEqual([2, 3, 4]);
    expect(answer.status).toBe("complete");
    expect(answer.feedbackSeq).toBe(4);
    expect(answer.traceId).toBe(TRACE);
    expect(answer.blocks.map((block) => block.type)).toEqual(["step", "text", "draft"]);
  });

  it("fills a step with its stored result", () => {
    const answer = assistantOf(bubblesOf(conversation.messages as unknown as AssistantMessage[]));
    const step = answer.blocks[0];
    expect(step).toMatchObject({
      type: "step",
      step: {
        id: "call_1",
        name: "query_sql",
        result: { ok: true, size_bytes: 120, rows: 1, stubbed: false },
      },
    });
  });

  it("marks a stubbed result and keeps no rows when the result has none", () => {
    const bubbles = bubblesOf([
      message({
        seq: 1,
        role: "user",
        parts: [{ type: "text", content: "q" }],
        status: "complete",
        created_at: "",
      }),
      message({
        seq: 2,
        role: "assistant",
        parts: [{ type: "tool_call", id: "c", name: "get_trace", arguments: { trace_id: TRACE } }],
        status: "complete",
        created_at: "",
      }),
      message({
        seq: 3,
        role: "tool",
        parts: [
          {
            type: "tool_result",
            id: "c",
            name: "get_trace",
            ok: true,
            size_bytes: 9,
            stubbed: true,
          },
        ],
        status: "complete",
        created_at: "",
      }),
    ]);
    const step = assistantOf(bubbles).blocks[0];
    expect(step).toEqual({
      type: "step",
      step: {
        id: "c",
        name: "get_trace",
        arguments: { trace_id: TRACE },
        result: { ok: true, size_bytes: 9, stubbed: true },
      },
    });
  });

  it("places a draft block after its step", () => {
    const answer = assistantOf(bubblesOf(conversation.messages as unknown as AssistantMessage[]));
    const kinds = answer.blocks.map((block) => block.type);
    expect(kinds.indexOf("draft")).toBeGreaterThan(kinds.indexOf("step"));
  });

  it("takes the status and feedback from the last assistant message", () => {
    const bubbles = bubblesOf([
      message({
        seq: 1,
        role: "user",
        parts: [{ type: "text", content: "q" }],
        status: "complete",
        created_at: "",
      }),
      message({
        seq: 2,
        role: "assistant",
        parts: [{ type: "text", content: "partial" }],
        status: "cancelled",
        feedback: "down",
        created_at: "",
      }),
    ]);
    expect(assistantOf(bubbles)).toMatchObject({
      status: "cancelled",
      feedback: "down",
      feedbackSeq: 2,
    });
  });
});

describe("applyEvent", () => {
  it("appends text deltas to one text block", () => {
    const once = applyEvent(empty, { event: "text", data: { delta: "Hello " } });
    const twice = applyEvent(once, { event: "text", data: { delta: "world" } });
    expect(twice.blocks).toEqual([{ type: "text", text: "Hello world" }]);
  });

  it("opens a new text block after a step", () => {
    const bubble = applyEvent(empty, { event: "text", data: { delta: "a" } });
    const withStep = applyEvent(bubble, {
      event: "tool_call",
      data: { id: "c1", name: "search_traces", arguments: {} },
    });
    const after = applyEvent(withStep, { event: "text", data: { delta: "b" } });
    expect(after.blocks.map((block) => block.type)).toEqual(["text", "step", "text"]);
  });

  it("adds a step on tool_call and fills it on tool_result", () => {
    const called = applyEvent(empty, {
      event: "tool_call",
      data: { id: "c1", name: "search_traces", arguments: { filter: "x" } },
    });
    const done = applyEvent(called, {
      event: "tool_result",
      data: { id: "c1", ok: false, summary: { size_bytes: 0, error_code: "bad_filter" } },
    });
    expect(done.blocks).toEqual([
      {
        type: "step",
        step: {
          id: "c1",
          name: "search_traces",
          arguments: { filter: "x" },
          result: { ok: false, size_bytes: 0, error_code: "bad_filter" },
        },
      },
    ]);
  });

  it("ignores a tool_result with an unknown id", () => {
    const result = applyEvent(empty, {
      event: "tool_result",
      data: { id: "missing", ok: true, summary: { size_bytes: 1 } },
    });
    expect(result.blocks).toEqual([]);
  });

  it("adds a draft block after the step", () => {
    const called = applyEvent(empty, {
      event: "tool_call",
      data: { id: "c1", name: "draft_filter", arguments: {} },
    });
    const drafted = applyEvent(called, {
      event: "draft",
      data: { id: "draft_c1", kind: "filter", draft: { view: "traces" } },
    });
    expect(drafted.blocks.map((block) => block.type)).toEqual(["step", "draft"]);
  });

  it("sets the trace id on start", () => {
    expect(
      applyEvent(empty, { event: "start", data: { turn_id: 1, trace_id: TRACE } }).traceId,
    ).toBe(TRACE);
  });

  it("completes on done and fails on error", () => {
    const done = applyEvent(empty, {
      event: "done",
      data: { finish_reason: "stop", usage: { input: 1, output: 2 }, cost_usd: null, steps: 1 },
    });
    expect(done.status).toBe("complete");
    const failed = applyEvent(empty, { event: "error", data: { class: "pause", message: "no" } });
    expect(failed.status).toBe("failed");
  });

  it("does not change the bubble it is given", () => {
    applyEvent(empty, { event: "text", data: { delta: "x" } });
    expect(empty.blocks).toEqual([]);
  });
});
