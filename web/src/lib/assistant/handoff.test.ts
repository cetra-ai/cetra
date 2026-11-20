import { describe, expect, it, vi } from "vitest";
import { handDraft, peekDraft, subscribeDrafts, takeDraft } from "@/lib/assistant/handoff";
import type { SqlDraft } from "@/lib/assistant/types";

const sql = (query: string): SqlDraft => ({ query, params: {}, columns: [] });

describe("hand-off store", () => {
  it("takes a draft once", () => {
    handDraft("acme/take-once", { kind: "sql", draft: sql("SELECT 1") });
    expect(takeDraft("acme/take-once", "sql")).toEqual(sql("SELECT 1"));
    expect(takeDraft("acme/take-once", "sql")).toBeUndefined();
  });

  it("peeks without taking", () => {
    handDraft("acme/peek", { kind: "sql", draft: sql("SELECT 2") });
    expect(peekDraft("acme/peek", "sql")).toEqual(sql("SELECT 2"));
    expect(takeDraft("acme/peek", "sql")).toEqual(sql("SELECT 2"));
  });

  it("replaces an untaken draft of the same kind", () => {
    handDraft("acme/replace", { kind: "sql", draft: sql("first") });
    handDraft("acme/replace", { kind: "sql", draft: sql("second") });
    expect(takeDraft("acme/replace", "sql")?.query).toBe("second");
  });

  it("keeps drafts of other kinds apart", () => {
    handDraft("acme/kinds", { kind: "sql", draft: sql("kept") });
    handDraft("acme/kinds", {
      kind: "evaluator",
      draft: {
        name: "Judge",
        kind: "check",
        config: {},
        output_score: { name: "ok", data_type: "boolean" },
      },
    });
    expect(takeDraft("acme/kinds", "evaluator")?.name).toBe("Judge");
    expect(takeDraft("acme/kinds", "sql")?.query).toBe("kept");
  });

  it("isolates projects", () => {
    handDraft("acme/one", { kind: "sql", draft: sql("one") });
    expect(takeDraft("acme/two", "sql")).toBeUndefined();
    expect(takeDraft("acme/one", "sql")?.query).toBe("one");
  });

  it("calls the listeners after each hand-off, until unsubscribed", () => {
    const listener = vi.fn();
    const unsubscribe = subscribeDrafts("acme/listen", listener);
    handDraft("acme/listen", { kind: "sql", draft: sql("a") });
    expect(listener).toHaveBeenCalledTimes(1);
    unsubscribe();
    handDraft("acme/listen", { kind: "sql", draft: sql("b") });
    expect(listener).toHaveBeenCalledTimes(1);
  });

  it("does not call a listener of another project", () => {
    const listener = vi.fn();
    const unsubscribe = subscribeDrafts("acme/listen-a", listener);
    handDraft("acme/listen-b", { kind: "sql", draft: sql("b") });
    expect(listener).not.toHaveBeenCalled();
    unsubscribe();
  });
});
