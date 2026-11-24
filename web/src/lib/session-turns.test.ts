import { afterEach, describe, expect, it, vi } from "vitest";
import type { Schema } from "@/api/types";
import {
  chipSteps,
  chunk,
  DEFAULT_TURN_DENSITY,
  filterCounts,
  gapNs,
  getTurnDensity,
  groupSteps,
  nextFailed,
  previewLine,
  setTurnDensity,
  stepsFilter,
  stepsWindow,
  stripWeight,
  turnMatches,
} from "@/lib/session-turns";

const FIRST = "a1000000000000000000000000000001";
const SECOND = "a1000000000000000000000000000002";
const THIRD = "a1000000000000000000000000000003";
const FOURTH = "a1000000000000000000000000000004";

function traceOf(overrides: Partial<Schema<"TraceSummary">> = {}): Schema<"TraceSummary"> {
  return {
    trace_id: FIRST,
    name: "support-agent",
    start_time: "2026-01-01T10:00:00.000000000Z",
    end_time: "2026-01-01T10:00:02.000000000Z",
    duration_ns: 2_000_000_000,
    error_count: 0,
    has_root: true,
    llm_count: 1,
    span_count: 3,
    metadata: {},
    models: [],
    scores: [],
    status: "ok",
    tags: [],
    ...overrides,
  };
}

function turnOf(overrides: Partial<Schema<"SessionTurn">> = {}): Schema<"SessionTurn"> {
  return {
    trace: traceOf(),
    input: { format: "text", value: "Where is my order?" },
    output: { format: "text", value: "It ships tomorrow." },
    ...overrides,
  };
}

function spanOf(overrides: Partial<Schema<"SpanSummary">> = {}): Schema<"SpanSummary"> {
  return {
    trace_id: FIRST,
    span_id: "b2000000000000000000000000000002",
    parent_span_id: "b2000000000000000000000000000001",
    name: "get_order_status",
    span_kind: "tool",
    start_time: "2026-01-01T10:00:00.500000000Z",
    end_time: "2026-01-01T10:00:01.000000000Z",
    duration_ns: 500_000_000,
    status: "ok",
    metadata: {},
    tags: [],
    ...overrides,
  };
}

const SCORE: Schema<"ScoreBrief"> = {
  name: "resolved",
  score_id: "01900000-0000-7000-8000-000000000340",
  source: "human",
  value: { data_type: "boolean", value: true },
};

describe("stepsFilter", () => {
  it("quotes each trace id and lists the four kinds", () => {
    expect(stepsFilter([FIRST, SECOND])).toBe(
      "trace_id IN ('a1000000000000000000000000000001', 'a1000000000000000000000000000002') AND span_kind IN ('llm', 'tool', 'retriever', 'guardrail')",
    );
  });

  it("of 100 ids fits the 4096-character filter limit", () => {
    const ids = Array.from({ length: 100 }, (_, index) => `a1${String(index).padStart(30, "0")}`);
    const filter = stepsFilter(ids);
    expect(filter).toHaveLength(3671);
    expect(filter.length).toBeLessThanOrEqual(4096);
  });
});

describe("chunk", () => {
  it("splits into slices of the size", () => {
    expect(chunk([], 3)).toEqual([]);
    expect(chunk([1, 2, 3, 4, 5], 2)).toEqual([
      [1, 2],
      [3, 4],
      [5],
    ]);
    expect(chunk([1, 2], 5)).toEqual([[1, 2]]);
  });
});

describe("stepsWindow", () => {
  it("starts at or before the start and ends 1 ms after the end", () => {
    const start = "2026-01-01T10:00:00.123456789Z";
    const end = "2026-01-01T10:05:30.987654321Z";
    const range = stepsWindow(start, end);
    expect(range.from).toBe("2026-01-01T10:00:00.123Z");
    expect(range.to).toBe("2026-01-01T10:05:30.988Z");
    expect(Date.parse(range.from)).toBeLessThanOrEqual(Date.parse(start));
    expect(Date.parse(range.to)).toBeGreaterThan(Date.parse(end));
  });
});

describe("groupSteps", () => {
  it("drops roots and other kinds, sorts by start time, and keeps a duplicate once", () => {
    const grouped = groupSteps([
      spanOf({ span_id: "b0000000000000000000000000000000", parent_span_id: undefined }),
      spanOf({
        span_id: "b3000000000000000000000000000003",
        start_time: "2026-01-01T10:00:03.000000000Z",
      }),
      // A duplicate span id: the first copy stays, however late the second claims to start.
      spanOf({
        span_id: "b3000000000000000000000000000003",
        start_time: "2026-01-01T10:00:09.000000000Z",
      }),
      spanOf({ span_kind: "agent", span_id: "b4000000000000000000000000000004" }),
      // A start-time tie is broken by span id: …06 sorts before …07.
      spanOf({
        span_id: "b0000000000000000000000000000007",
        start_time: "2026-01-01T10:00:05.000000000Z",
      }),
      spanOf({
        span_kind: "llm",
        span_id: "b0000000000000000000000000000006",
        start_time: "2026-01-01T10:00:05.000000000Z",
      }),
      spanOf({
        trace_id: SECOND,
        span_kind: "retriever",
        span_id: "b5000000000000000000000000000005",
      }),
    ]);
    expect([...grouped.keys()]).toEqual([FIRST, SECOND]);
    expect(
      grouped.get(FIRST)?.map((step) => [step.span_id, step.span_kind]),
    ).toEqual([
      ["b3000000000000000000000000000003", "tool"],
      ["b0000000000000000000000000000006", "llm"],
      ["b0000000000000000000000000000007", "tool"],
    ]);
    expect(grouped.get(SECOND)?.map((step) => step.span_kind)).toEqual(["retriever"]);
  });
});

describe("chipSteps", () => {
  it("keeps tool, retriever and guardrail steps", () => {
    const steps = [
      spanOf({ span_kind: "llm" }),
      spanOf({ span_kind: "tool" }),
      spanOf({ span_kind: "retriever" }),
      spanOf({ span_kind: "guardrail" }),
      spanOf({ span_kind: "agent" }),
    ];
    expect(chipSteps(steps).map((step) => step.span_kind)).toEqual([
      "tool",
      "retriever",
      "guardrail",
    ]);
  });
});

describe("previewLine", () => {
  it("collapses whitespace and drops a blank preview", () => {
    expect(previewLine(undefined)).toBeUndefined();
    expect(previewLine('{\n  "order": 1234\n}\n')).toBe('{ "order": 1234 }');
    expect(previewLine("  \n\t ")).toBeUndefined();
  });
});

describe("turnMatches", () => {
  const turn = turnOf({
    trace: traceOf({ name: "Support-agent" }),
    input: { format: "text", value: "My ORDER 1234" },
    output: { format: "text", value: "It ships tomorrow." },
  });

  it("searches the name and both bubbles, ignoring case and spaces", () => {
    expect(turnMatches(turn, undefined, "", new Set())).toBe(true);
    expect(turnMatches(turn, undefined, "  order  ", new Set())).toBe(true);
    expect(turnMatches(turn, undefined, "SHIPS", new Set())).toBe(true);
    expect(turnMatches(turn, undefined, "support", new Set())).toBe(true);
    expect(turnMatches(turn, undefined, "elephant", new Set())).toBe(false);
  });

  it("requires every selected filter", () => {
    const failedAndScored = turnOf({
      trace: traceOf({ status: "error", scores: [SCORE] }),
    });
    expect(turnMatches(failedAndScored, undefined, "", new Set(["failed", "scored"]))).toBe(true);
    expect(
      turnMatches(
        turnOf({ trace: traceOf({ status: "error" }) }),
        undefined,
        "",
        new Set(["failed", "scored"]),
      ),
    ).toBe(false);
    expect(
      turnMatches(
        turnOf({ trace: traceOf({ scores: [SCORE] }) }),
        undefined,
        "",
        new Set(["failed", "scored"]),
      ),
    ).toBe(false);
  });

  it("never matches the tools filter while steps are unknown", () => {
    expect(turnMatches(turn, undefined, "", new Set(["tools"]))).toBe(false);
    expect(turnMatches(turn, [], "", new Set(["tools"]))).toBe(false);
    expect(turnMatches(turn, [spanOf({ span_kind: "llm" })], "", new Set(["tools"]))).toBe(false);
    expect(turnMatches(turn, [spanOf({ span_kind: "tool" })], "", new Set(["tools"]))).toBe(true);
  });
});

describe("filterCounts", () => {
  it("counts each filter on its own", () => {
    const turns = [
      turnOf({ trace: traceOf({ trace_id: FIRST, status: "error" }) }),
      turnOf({ trace: traceOf({ trace_id: SECOND, scores: [SCORE] }) }),
      turnOf({ trace: traceOf({ trace_id: THIRD }) }),
    ];
    const stepsOf = (traceId: string) =>
      traceId === THIRD ? [spanOf({ trace_id: THIRD, span_kind: "tool" })] : [];
    expect(filterCounts(turns, stepsOf)).toEqual({ failed: 1, scored: 1, tools: 1 });
    expect(filterCounts(turns, () => undefined)).toEqual({ failed: 1, scored: 1, tools: 0 });
  });
});

describe("gapNs", () => {
  it("is undefined for the first turn and for a turn that starts earlier", () => {
    const first = turnOf({ trace: traceOf({ trace_id: FIRST }) });
    const second = turnOf({
      trace: traceOf({ trace_id: SECOND, start_time: "2026-01-01T10:00:01.500000000Z" }),
    });
    const earlier = turnOf({
      trace: traceOf({ trace_id: THIRD, start_time: "2026-01-01T09:59:00.000000000Z" }),
    });
    expect(gapNs(undefined, first)).toBeUndefined();
    expect(gapNs(first, second)).toBe(1_500_000_000);
    expect(gapNs(second, earlier)).toBeUndefined();
  });
});

describe("nextFailed", () => {
  const turns = [
    turnOf({ trace: traceOf({ trace_id: FIRST }) }),
    turnOf({ trace: traceOf({ trace_id: SECOND, status: "error" }) }),
    turnOf({ trace: traceOf({ trace_id: THIRD }) }),
  ];

  it("wraps, starts from the selection, and returns -1 without failures", () => {
    expect(nextFailed(turns, undefined)).toBe(1);
    expect(nextFailed(turns, FIRST)).toBe(1);
    expect(nextFailed(turns, SECOND)).toBe(1);
    // The failed turn after index 1 is none, so the search wraps to the start.
    const four = [...turns, turnOf({ trace: traceOf({ trace_id: FOURTH, status: "error" }) })];
    expect(nextFailed(four, SECOND)).toBe(3);
    expect(nextFailed(four, FOURTH)).toBe(1);
    // An unknown id reads as no selection: the first failed turn.
    expect(nextFailed(turns, "unknown")).toBe(1);
    // The selected turn being the only failed one finds itself again.
    expect(nextFailed(turns.slice(0, 2), SECOND)).toBe(1);
    const noFailures = [
      turnOf({ trace: traceOf({ trace_id: FIRST }) }),
      turnOf({ trace: traceOf({ trace_id: SECOND }) }),
    ];
    expect(nextFailed(noFailures, SECOND)).toBe(-1);
  });
});

describe("stripWeight", () => {
  it("is at least 1", () => {
    expect(stripWeight(turnOf({ trace: traceOf({ duration_ns: 0 }) }))).toBe(1);
    expect(stripWeight(turnOf({ trace: traceOf({ duration_ns: 499_999 }) }))).toBe(1);
    expect(stripWeight(turnOf({ trace: traceOf({ duration_ns: 2_000_000_000 }) }))).toBe(2000);
  });
});

describe("turn density", () => {
  afterEach(() => {
    vi.restoreAllMocks();
    setTurnDensity(DEFAULT_TURN_DENSITY);
    localStorage.clear();
  });

  it("keeps the choice when storage throws", () => {
    vi.spyOn(Storage.prototype, "getItem").mockImplementation(() => {
      throw new Error("denied");
    });
    vi.spyOn(Storage.prototype, "setItem").mockImplementation(() => {
      throw new Error("denied");
    });

    expect(() => {
      setTurnDensity("compact");
    }).not.toThrow();
    expect(getTurnDensity()).toBe("compact");
  });
});
