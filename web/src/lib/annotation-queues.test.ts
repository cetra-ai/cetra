import { describe, expect, it } from "vitest";
import {
  LEASE_RENEW_MS,
  parseQueueSearch,
  parseQueuesSearch,
  parseReviewSearch,
  progressOf,
  QUEUE_NAME_TAKEN,
  QUEUE_POLL_MS,
  targetLabel,
} from "@/lib/annotation-queues";

describe("parseQueuesSearch", () => {
  it("keeps a trimmed name filter and nothing else", () => {
    expect(parseQueuesSearch({ q: "  relevance " })).toEqual({ q: "relevance" });
    expect(parseQueuesSearch({ q: "   " })).toEqual({});
    expect(parseQueuesSearch({ q: 3, other: "x" })).toEqual({});
  });
});

describe("parseQueueSearch", () => {
  it("keeps a known tab and a known status", () => {
    expect(parseQueueSearch({ tab: "settings", status: "skipped" })).toEqual({
      tab: "settings",
      status: "skipped",
    });
  });

  it("drops unknown values", () => {
    expect(parseQueueSearch({ tab: "results", status: "done" })).toEqual({});
    expect(parseQueueSearch({})).toEqual({});
  });
});

describe("parseReviewSearch", () => {
  it("keeps a non-empty after id", () => {
    expect(parseReviewSearch({ after: "01900000-0000-7000-8000-000000000b01" })).toEqual({
      after: "01900000-0000-7000-8000-000000000b01",
    });
  });

  it("drops an empty or non-string after", () => {
    expect(parseReviewSearch({ after: "" })).toEqual({});
    expect(parseReviewSearch({ after: 7 })).toEqual({});
  });
});

describe("targetLabel", () => {
  it("names each kind", () => {
    expect(targetLabel("trace")).toBe("Traces");
    expect(targetLabel("span")).toBe("Spans");
    expect(targetLabel("session")).toBe("Sessions");
  });
});

describe("progressOf", () => {
  it("counts completed and skipped as done", () => {
    expect(progressOf({ pending: 12, completed: 4, skipped: 1 })).toEqual({ done: 5, total: 17 });
  });

  it("is empty for an empty queue", () => {
    expect(progressOf({ pending: 0, completed: 0, skipped: 0 })).toEqual({ done: 0, total: 0 });
  });
});

describe("constants", () => {
  it("are the plan's intervals and the API's conflict detail", () => {
    expect(QUEUE_POLL_MS).toBe(10_000);
    expect(LEASE_RENEW_MS).toBe(60_000);
    expect(QUEUE_NAME_TAKEN).toBe("an annotation queue with this name exists");
  });
});
