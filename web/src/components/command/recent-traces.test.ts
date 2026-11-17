import { expect, it } from "vitest";
import {
  forgetTrace,
  openTraceId,
  readRecent,
  rememberTrace,
} from "@/components/command/recent-traces";
import { rememberItem } from "@/components/shell/recent-items";
import { STORAGE_PREFIX } from "@/product";

it("keeps the last 10 traces per project, ids and names only", () => {
  for (let index = 0; index < 12; index += 1) {
    rememberTrace("acme", "support-bot", {
      id: `a10000000000000000000000000000${index.toString(16).padStart(2, "0")}`,
      name: `trace ${index}`,
    });
  }
  rememberTrace("acme", "playground", { id: "b".repeat(32), name: "other" });
  const recent = readRecent("acme", "support-bot");
  expect(recent).toHaveLength(10);
  expect(recent[0]).toEqual({
    id: "a100000000000000000000000000000b",
    name: "trace 11",
  });
  expect(readRecent("acme", "playground")).toEqual([{ id: "b".repeat(32), name: "other" }]);
  const stored = JSON.parse(localStorage.getItem(`${STORAGE_PREFIX}recent`) ?? "{}") as Record<
    string,
    Record<string, unknown>[]
  >;
  expect(Object.keys(stored["acme/support-bot"]?.[0] ?? {})).toEqual(["kind", "id", "label"]);
  expect(localStorage.getItem(`${STORAGE_PREFIX}recent-traces`)).toBeNull();
});

it("lists only traces, not other kinds of recent items", () => {
  rememberItem("acme", "support-bot", { kind: "evaluator", id: "e1", label: "Relevance" });
  rememberTrace("acme", "support-bot", { id: "e".repeat(32), name: "trace" });
  expect(readRecent("acme", "support-bot")).toEqual([{ id: "e".repeat(32), name: "trace" }]);
});

it("reads a trace id from the split or the full page", () => {
  expect(openTraceId("/acme/support-bot/traces", { trace: "a".repeat(32) })).toBe("a".repeat(32));
  expect(openTraceId(`/acme/support-bot/traces/${"b".repeat(32)}`, {})).toBe("b".repeat(32));
  expect(openTraceId("/acme/support-bot/spans", {})).toBeUndefined();
});

it("forgets a deleted trace in every project and keeps the rest", () => {
  const gone = "c".repeat(32);
  const kept = "d".repeat(32);
  rememberTrace("acme", "support-bot", { id: gone, name: "gone" });
  rememberTrace("acme", "support-bot", { id: kept, name: "kept" });
  rememberTrace("acme", "playground", { id: gone, name: "gone too" });
  forgetTrace(gone);
  expect(readRecent("acme", "support-bot")).toEqual([{ id: kept, name: "kept" }]);
  expect(readRecent("acme", "playground")).toEqual([]);
  const stored = JSON.parse(localStorage.getItem(`${STORAGE_PREFIX}recent`) ?? "{}") as Record<
    string,
    unknown
  >;
  expect(Object.keys(stored)).toEqual(["acme/support-bot"]);
});
