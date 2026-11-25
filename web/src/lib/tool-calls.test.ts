import { describe, expect, it } from "vitest";
import type { Schema } from "@/api/types";
import {
  foldedMessage,
  inlineArgs,
  inlineResult,
  matchToolSpans,
  toolLinks,
} from "@/lib/tool-calls";

const TRACE = "a1000000000000000000000000000001";
const ROOT = "b100000000000011";
const LLM_CALL = "b100000000000012";
const TOOL = "b100000000000013";
const LLM_RESULT = "b100000000000014";

function row(
  span: Partial<Schema<"SpanSummary">> & {
    span_id: string;
    name: string;
    start_time: string;
    end_time: string;
  },
): Schema<"TreeSpan"> {
  return {
    depth: 1,
    detached: false,
    span: {
      trace_id: TRACE,
      duration_ns: 0,
      metadata: {},
      span_kind: "tool",
      status: "ok",
      tags: [],
      ...span,
    },
  };
}

function message(role: string, parts: Schema<"Part">[]): Schema<"Message"> {
  return { role, parts };
}

function call(id: string, name: string, args: unknown = {}): Schema<"Part"> {
  return { type: "tool_call", id, name, arguments: args };
}

function response(id: string, value: unknown): Schema<"Part"> {
  return { type: "tool_call_response", id, response: value };
}

const SUPPORT_LLM_CALL = row({
  span_id: LLM_CALL,
  parent_span_id: ROOT,
  name: "chat gpt",
  span_kind: "llm",
  start_time: "2026-01-01T10:00:00.100000000Z",
  end_time: "2026-01-01T10:00:01.900000000Z",
}).span;

const SUPPORT_TOOL = row({
  span_id: TOOL,
  parent_span_id: ROOT,
  name: "get_order_status",
  start_time: "2026-01-01T10:00:01.950000000Z",
  end_time: "2026-01-01T10:00:02.300000000Z",
  duration_ns: 350_000_000,
});

const SUPPORT_LLM_RESULT = row({
  span_id: LLM_RESULT,
  parent_span_id: ROOT,
  name: "chat gpt",
  span_kind: "llm",
  start_time: "2026-01-01T10:00:02.350000000Z",
  end_time: "2026-01-01T10:00:03.000000000Z",
}).span;

const SUPPORT_TREE = [
  row({
    span_id: LLM_CALL,
    parent_span_id: ROOT,
    name: "chat gpt",
    span_kind: "llm",
    start_time: "2026-01-01T10:00:00.100000000Z",
    end_time: "2026-01-01T10:00:01.900000000Z",
  }),
  SUPPORT_TOOL,
  row({
    span_id: LLM_RESULT,
    parent_span_id: ROOT,
    name: "chat gpt",
    span_kind: "llm",
    start_time: "2026-01-01T10:00:02.350000000Z",
    end_time: "2026-01-01T10:00:03.000000000Z",
  }),
];

const SHOWN_CALL = call("call_1", "get_order_status", { order_id: "1234" });

describe("inlineArgs", () => {
  it("writes an object's entries without braces", () => {
    expect(inlineArgs({ order_id: "1234" })).toBe('order_id: "1234"');
    expect(inlineArgs({ day: "Friday", time: "morning" })).toBe('day: "Friday", time: "morning"');
  });

  it("parses a JSON string argument", () => {
    expect(inlineArgs('{"path":"a.py"}')).toBe('path: "a.py"');
  });

  it("shortens nested values past depth 2", () => {
    expect(inlineArgs({ a: { b: { c: 1 } } })).toBe("a: {b: {…}}");
    expect(inlineArgs({ list: [1, 2] })).toBe("list: [1, 2]");
  });

  it("is empty for missing, null, empty and empty-object arguments", () => {
    expect(inlineArgs(undefined)).toBe("");
    expect(inlineArgs(null)).toBe("");
    expect(inlineArgs({})).toBe("");
    expect(inlineArgs("")).toBe("");
  });

  it("quotes a non-JSON string", () => {
    expect(inlineArgs("ls -la")).toBe('"ls -la"');
  });

  it("shortens to 80 code points with an ellipsis", () => {
    const text = inlineArgs({ text: "x".repeat(200) });
    expect(Array.from(text)).toHaveLength(80);
    expect(text.endsWith("…")).toBe(true);
  });
});

describe("inlineResult", () => {
  it("renders an object result inline", () => {
    expect(inlineResult({ slots: ["9:30", "11:00"] })).toBe('{slots: ["9:30", "11:00"]}');
  });

  it("unquotes the first non-empty line of a string", () => {
    expect(inlineResult("\n  def f():\n    pass")).toBe("def f():");
    expect(inlineResult("48 passed in 3.2s")).toBe("48 passed in 3.2s");
  });

  it("renders scalars and undefined", () => {
    expect(inlineResult(true)).toBe("true");
    expect(inlineResult(undefined)).toBe("");
  });

  it("shortens long text by code points", () => {
    const text = inlineResult("é".repeat(100));
    expect(Array.from(text)).toHaveLength(80);
    expect(text.endsWith("…")).toBe(true);
  });

  it("keeps 50 emoji, which is within the limit", () => {
    expect(inlineResult("😀".repeat(50))).toBe("😀".repeat(50));
  });
});

describe("toolLinks", () => {
  it("links a shown call to its response", () => {
    const links = toolLinks([
      message("assistant", [SHOWN_CALL]),
      message("tool", [response("call_1", { status: "shipped" })]),
    ]);
    expect(links.responses.get("call_1")).toEqual({ status: "shipped" });
    expect(links.shownCalls.has("call_1")).toBe(true);
    expect(links.callNames.get("call_1")).toBe("get_order_status");
  });

  it("names a response whose call is only in context without linking it", () => {
    const links = toolLinks(
      [message("tool", [response("call_9", "ok")])],
      [message("assistant", [call("call_9", "lookup")])],
    );
    expect(links.responses.has("call_9")).toBe(false);
    expect(links.callNames.get("call_9")).toBe("lookup");
  });

  it("keeps a call without an id in neither map", () => {
    const links = toolLinks([message("assistant", [{ type: "tool_call", name: "x" }])]);
    expect(links.shownCalls.size).toBe(0);
    expect(links.callNames.size).toBe(0);
  });

  it("takes the first of two responses with one id", () => {
    const links = toolLinks([
      message("assistant", [SHOWN_CALL]),
      message("tool", [response("call_1", 1), response("call_1", 2)]),
    ]);
    expect(links.responses.get("call_1")).toBe(1);
  });

  it("shows a call without a string name in shownCalls only", () => {
    const links = toolLinks([message("assistant", [{ type: "tool_call", id: "call_2" }])]);
    expect(links.shownCalls.has("call_2")).toBe(true);
    expect(links.callNames.has("call_2")).toBe(false);
  });
});

describe("foldedMessage", () => {
  const links = toolLinks([
    message("assistant", [SHOWN_CALL]),
    message("tool", [response("call_1", "ok")]),
  ]);

  it("is true for a message of folded responses", () => {
    expect(foldedMessage(message("tool", [response("call_1", "ok")]), links)).toBe(true);
  });

  it("is false when one response's call is not shown", () => {
    const mixed = message("tool", [response("call_1", "ok"), response("call_9", "ok")]);
    expect(foldedMessage(mixed, links)).toBe(false);
  });

  it("is false for a message without parts", () => {
    expect(foldedMessage(message("tool", []), links)).toBe(false);
  });
});

describe("matchToolSpans", () => {
  it("matches the support-bot output call of the first LLM span", () => {
    const matches = matchToolSpans(
      SUPPORT_LLM_CALL,
      SUPPORT_TREE,
      [],
      [message("assistant", [SHOWN_CALL])],
    );
    expect(matches.get(SHOWN_CALL)).toStrictEqual({
      spanId: TOOL,
      durationNs: 350_000_000,
    });
  });

  it("matches the support-bot input call of the second LLM span", () => {
    const input = [message("assistant", [SHOWN_CALL]), message("tool", [response("call_1", "ok")])];
    const matches = matchToolSpans(SUPPORT_LLM_RESULT, SUPPORT_TREE, input, []);
    expect(matches.get(SHOWN_CALL)).toStrictEqual({
      spanId: TOOL,
      durationNs: 350_000_000,
    });
  });

  it("carries a failed span's status message, or Failed when there is none", () => {
    const timing = {
      parent_span_id: ROOT,
      name: "get_order_status",
      start_time: "2026-01-01T10:00:01.950000000Z",
      end_time: "2026-01-01T10:00:02.300000000Z",
      status: "error" as const,
    };
    const withMessage = matchToolSpans(
      SUPPORT_LLM_CALL,
      [row({ ...timing, span_id: TOOL, status_message: "1 failed, 47 passed" })],
      [],
      [message("assistant", [SHOWN_CALL])],
    );
    expect(withMessage.get(SHOWN_CALL)?.error).toBe("1 failed, 47 passed");

    const withoutMessage = matchToolSpans(
      SUPPORT_LLM_CALL,
      [row({ ...timing, span_id: TOOL })],
      [],
      [message("assistant", [SHOWN_CALL])],
    );
    expect(withoutMessage.get(SHOWN_CALL)?.error).toBe("Failed");
  });

  it("ignores a tool span with another parent", () => {
    const stray = row({
      span_id: TOOL,
      parent_span_id: "b1000000000000ff",
      name: "get_order_status",
      start_time: "2026-01-01T10:00:01.950000000Z",
      end_time: "2026-01-01T10:00:02.300000000Z",
    });
    const matches = matchToolSpans(
      SUPPORT_LLM_CALL,
      [stray],
      [],
      [message("assistant", [SHOWN_CALL])],
    );
    expect(matches.has(SHOWN_CALL)).toBe(false);
  });

  it("matches a child of the LLM span", () => {
    const child = row({
      span_id: TOOL,
      parent_span_id: LLM_CALL,
      name: "get_order_status",
      start_time: "2026-01-01T10:00:01.000000000Z",
      end_time: "2026-01-01T10:00:01.300000000Z",
      duration_ns: 300_000_000,
    });
    const matches = matchToolSpans(
      SUPPORT_LLM_CALL,
      [child],
      [],
      [message("assistant", [SHOWN_CALL])],
    );
    expect(matches.get(SHOWN_CALL)?.spanId).toBe(TOOL);
  });

  it("maps two output calls to the first two candidates in start order", () => {
    const first = call("call_a", "get_order_status");
    const second = call("call_b", "get_order_status");
    const candidate = (id: string, start: string, end: string) =>
      row({
        span_id: id,
        parent_span_id: ROOT,
        name: "get_order_status",
        start_time: start,
        end_time: end,
      });
    const tree = [
      candidate(
        "b100000000000021",
        "2026-01-01T10:00:03.000000000Z",
        "2026-01-01T10:00:03.100000000Z",
      ),
      candidate(
        "b100000000000022",
        "2026-01-01T10:00:02.000000000Z",
        "2026-01-01T10:00:02.100000000Z",
      ),
      candidate(
        "b100000000000023",
        "2026-01-01T10:00:04.000000000Z",
        "2026-01-01T10:00:04.100000000Z",
      ),
    ];
    const matches = matchToolSpans(
      SUPPORT_LLM_CALL,
      tree,
      [],
      [message("assistant", [first, second])],
    );
    expect(matches.get(first)?.spanId).toBe("b100000000000022");
    expect(matches.get(second)?.spanId).toBe("b100000000000021");
  });

  it("maps two input calls to the one earlier candidate on the right", () => {
    const earlier = call("call_a", "get_order_status");
    const later = call("call_b", "get_order_status");
    const input = [message("assistant", [earlier, later])];
    const matches = matchToolSpans(SUPPORT_LLM_RESULT, SUPPORT_TREE, input, []);
    expect(matches.has(earlier)).toBe(false);
    expect(matches.get(later)?.spanId).toBe(TOOL);
  });

  it("matches no input call whose only candidate ends after the LLM span starts", () => {
    const late = row({
      span_id: TOOL,
      parent_span_id: ROOT,
      name: "get_order_status",
      start_time: "2026-01-01T10:00:02.000000000Z",
      end_time: "2026-01-01T10:00:02.400000000Z",
    });
    const input = [message("assistant", [SHOWN_CALL])];
    const matches = matchToolSpans(SUPPORT_LLM_RESULT, [late], input, []);
    expect(matches.has(SHOWN_CALL)).toBe(false);
  });

  it("matches nothing when both spans have no parent", () => {
    const llm = row({
      span_id: LLM_CALL,
      name: "chat gpt",
      span_kind: "llm",
      start_time: "2026-01-01T10:00:00.100000000Z",
      end_time: "2026-01-01T10:00:01.900000000Z",
    }).span;
    const orphan = row({
      span_id: TOOL,
      name: "get_order_status",
      start_time: "2026-01-01T10:00:01.950000000Z",
      end_time: "2026-01-01T10:00:02.300000000Z",
    });
    const matches = matchToolSpans(llm, [orphan], [], [message("assistant", [SHOWN_CALL])]);
    expect(matches.has(SHOWN_CALL)).toBe(false);
  });

  it("matches a child of a parentless LLM span", () => {
    const llm = row({
      span_id: LLM_CALL,
      name: "chat gpt",
      span_kind: "llm",
      start_time: "2026-01-01T10:00:00.100000000Z",
      end_time: "2026-01-01T10:00:01.900000000Z",
    }).span;
    const child = row({
      span_id: TOOL,
      parent_span_id: LLM_CALL,
      name: "get_order_status",
      start_time: "2026-01-01T10:00:01.950000000Z",
      end_time: "2026-01-01T10:00:02.300000000Z",
      duration_ns: 350_000_000,
    });
    const matches = matchToolSpans(llm, [child], [], [message("assistant", [SHOWN_CALL])]);
    expect(matches.get(SHOWN_CALL)?.spanId).toBe(TOOL);
  });
});
