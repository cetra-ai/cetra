import { afterEach, describe, expect, it, vi } from "vitest";
import {
  carriedSearch,
  type ListContext,
  rangeOnly,
  readListContext,
  writeListContext,
} from "@/lib/list-context";

const KEY = "cetra.list-context.acme/support-bot";

afterEach(() => {
  vi.restoreAllMocks();
});

describe("carriedSearch", () => {
  it("carries nothing from the same list or without a context", () => {
    expect(carriedSearch({ source: "traces", filter: "user_id = 'a'" }, "traces")).toBeUndefined();
    expect(carriedSearch(undefined, "spans")).toBeUndefined();
  });

  it("keeps the window and the carried conjuncts, and counts the rest", () => {
    const context: ListContext = {
      source: "traces",
      range: "1h",
      eval: 1,
      filter: "name = 'refund' AND user_id = 'u-1' AND session_id = 's-1'",
    };
    expect(carriedSearch(context, "spans")).toEqual({
      search: {
        range: "1h",
        eval: 1,
        filter: "user_id = 'u-1' AND session_id = 's-1'",
      },
      from: "traces",
      dropped: 1,
      kept: true,
    });
  });

  it("carries the window alone without a kept filter", () => {
    expect(carriedSearch({ source: "spans", range: "7d", filter: "name = 'x'" }, "traces")).toEqual(
      {
        search: { range: "7d" },
        from: "spans",
        dropped: 1,
        kept: false,
      },
    );
  });

  it("reports drops with an empty search when nothing is kept", () => {
    expect(carriedSearch({ source: "spans", filter: "name = 'x'" }, "traces")).toEqual({
      search: {},
      from: "spans",
      dropped: 1,
      kept: false,
    });
  });

  it("carries nothing when nothing is kept and nothing is dropped", () => {
    expect(carriedSearch({ source: "spans", range: undefined }, "traces")).toBeUndefined();
  });
});

describe("list context storage", () => {
  it("reads back what a list page wrote", () => {
    writeListContext("acme", "support-bot", "sessions", {
      range: "24h",
      eval: 1,
      filter: "user_id = 'u-1'",
      sort: "cost_total",
    });
    expect(readListContext("acme", "support-bot")).toEqual({
      source: "sessions",
      range: "24h",
      eval: 1,
      filter: "user_id = 'u-1'",
    });
  });

  it("keeps each project's context apart", () => {
    writeListContext("acme", "support-bot", "traces", { filter: "user_id = 'u-1'" });
    expect(readListContext("acme", "other")).toBeUndefined();
  });

  it("drops a stored window whose from is not before its to", () => {
    window.sessionStorage.setItem(
      KEY,
      JSON.stringify({
        source: "traces",
        from: "2026-02-01T00:00:00Z",
        to: "2026-01-01T00:00:00Z",
      }),
    );
    expect(readListContext("acme", "support-bot")).toEqual({ source: "traces" });
    window.sessionStorage.setItem(
      KEY,
      JSON.stringify({
        source: "traces",
        from: "2026-01-01T00:00:00Z",
        to: "2026-02-01T00:00:00Z",
      }),
    );
    expect(readListContext("acme", "support-bot")).toEqual({
      source: "traces",
      from: "2026-01-01T00:00:00Z",
      to: "2026-02-01T00:00:00Z",
    });
  });

  it("drops a stored window that the list pages would not accept", () => {
    window.sessionStorage.setItem(
      KEY,
      JSON.stringify({ source: "traces", range: "1y", from: "2026-01-01", filter: "x = 1" }),
    );
    expect(readListContext("acme", "support-bot")).toEqual({ source: "traces", filter: "x = 1" });
  });

  it("ignores a stored value that is not a list context", () => {
    window.sessionStorage.setItem(KEY, "{not json");
    expect(readListContext("acme", "support-bot")).toBeUndefined();
    window.sessionStorage.setItem(KEY, JSON.stringify({ source: "dashboard" }));
    expect(readListContext("acme", "support-bot")).toBeUndefined();
  });

  it("does not throw when storage is blocked", () => {
    vi.spyOn(Storage.prototype, "getItem").mockImplementation(() => {
      throw new Error("blocked");
    });
    vi.spyOn(Storage.prototype, "setItem").mockImplementation(() => {
      throw new Error("blocked");
    });
    expect(readListContext("acme", "support-bot")).toBeUndefined();
    expect(() => writeListContext("acme", "support-bot", "traces", {})).not.toThrow();
  });
});

describe("rangeOnly", () => {
  it("keeps the window and nothing else", () => {
    expect(rangeOnly({ range: "1h", filter: "x", eval: 1 } as object)).toEqual({ range: "1h" });
    expect(rangeOnly({ from: "a", to: "b" })).toEqual({ from: "a", to: "b" });
  });
});
