import { act, renderHook, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import {
  forgetItem,
  readRecentItems,
  rememberItem,
  useRecentItems,
} from "@/components/shell/recent-items";
import { STORAGE_PREFIX } from "@/product";
import { renderApp } from "@/test/render";

const KEY = `${STORAGE_PREFIX}recent`;
const TRACE = "a".repeat(32);
const EVALUATOR = "01900000-0000-7000-8000-000000000500";

beforeEach(() => {
  localStorage.clear();
});

afterEach(() => {
  vi.restoreAllMocks();
});

it("lists items newest first, one entry per kind and id, and per project", () => {
  rememberItem("acme", "support-bot", { kind: "trace", id: TRACE, label: "first" });
  rememberItem("acme", "support-bot", { kind: "evaluator", id: "e1", label: "Relevance" });
  rememberItem("acme", "support-bot", { kind: "trace", id: TRACE, label: "second" });
  // The same id under another kind is a separate entry.
  rememberItem("acme", "support-bot", { kind: "session", id: "e1", label: "e1" });
  rememberItem("acme", "playground", { kind: "dataset", id: "d1", label: "faq" });

  expect(readRecentItems("acme", "support-bot")).toEqual([
    { kind: "session", id: "e1", label: "e1" },
    { kind: "trace", id: TRACE, label: "second" },
    { kind: "evaluator", id: "e1", label: "Relevance" },
  ]);
  expect(readRecentItems("acme", "playground")).toEqual([
    { kind: "dataset", id: "d1", label: "faq" },
  ]);
});

it("keeps the 20 newest items per project", () => {
  for (let index = 0; index < 22; index += 1) {
    rememberItem("acme", "support-bot", {
      kind: "experiment",
      id: `e${index}`,
      label: `run ${index}`,
    });
  }
  const recent = readRecentItems("acme", "support-bot");
  expect(recent).toHaveLength(20);
  expect(recent[0]?.id).toBe("e21");
  expect(recent[19]?.id).toBe("e2");
});

it("keeps the previous name when a later open only has the id", () => {
  const evaluator = { kind: "evaluator" as const, id: "e1" };
  rememberItem("acme", "support-bot", { ...evaluator, label: "Relevance" });
  rememberItem("acme", "support-bot", { ...evaluator, label: "e1" });
  expect(readRecentItems("acme", "support-bot")).toEqual([
    { kind: "evaluator", id: "e1", label: "Relevance" },
  ]);
  rememberItem("acme", "support-bot", { ...evaluator, label: "Tone" });
  expect(readRecentItems("acme", "support-bot")).toEqual([
    { kind: "evaluator", id: "e1", label: "Tone" },
  ]);
});

it("forgets an item in every project and keeps the rest", () => {
  rememberItem("acme", "support-bot", { kind: "trace", id: TRACE, label: "gone" });
  rememberItem("acme", "support-bot", { kind: "evaluator", id: "e1", label: "kept" });
  rememberItem("acme", "playground", { kind: "trace", id: TRACE, label: "gone too" });
  forgetItem("trace", TRACE);
  expect(readRecentItems("acme", "support-bot")).toEqual([
    { kind: "evaluator", id: "e1", label: "kept" },
  ]);
  expect(readRecentItems("acme", "playground")).toEqual([]);
  expect(Object.keys(JSON.parse(localStorage.getItem(KEY) ?? "{}"))).toEqual(["acme/support-bot"]);
});

it("ignores malformed entries and unknown kinds on read", () => {
  localStorage.setItem(
    KEY,
    JSON.stringify({
      "acme/support-bot": [
        { kind: "evaluator", id: "e1", label: "ok" },
        { kind: "widget", id: "x", label: "unknown kind" },
        { kind: "trace", id: "", label: "empty id" },
        { kind: "trace", id: 3, label: "number id" },
        { kind: "trace", id: "t1" },
        null,
        "text",
      ],
      "acme/other": "not a list",
    }),
  );
  expect(readRecentItems("acme", "support-bot")).toEqual([
    { kind: "evaluator", id: "e1", label: "ok" },
  ]);
  expect(readRecentItems("acme", "other")).toEqual([]);
});

it("reads an empty list for a stored value that is not an object", () => {
  localStorage.setItem(KEY, "{not json");
  expect(readRecentItems("acme", "support-bot")).toEqual([]);
  localStorage.setItem(KEY, JSON.stringify(["trace"]));
  expect(readRecentItems("acme", "support-bot")).toEqual([]);
});

it("reads empty and does not throw when localStorage throws", () => {
  vi.spyOn(Storage.prototype, "getItem").mockImplementation(() => {
    throw new Error("denied");
  });
  vi.spyOn(Storage.prototype, "setItem").mockImplementation(() => {
    throw new Error("full");
  });
  expect(readRecentItems("acme", "support-bot")).toEqual([]);
  expect(() =>
    rememberItem("acme", "support-bot", { kind: "trace", id: TRACE, label: "t" }),
  ).not.toThrow();
  expect(() => forgetItem("trace", TRACE)).not.toThrow();
});

it("re-renders on a change in this tab and in another tab, and keeps the snapshot stable", () => {
  const view = renderHook(() => useRecentItems("acme", "support-bot", 2));
  expect(view.result.current).toEqual([]);

  act(() => {
    rememberItem("acme", "support-bot", { kind: "evaluator", id: "e1", label: "Relevance" });
  });
  expect(view.result.current).toEqual([{ kind: "evaluator", id: "e1", label: "Relevance" }]);

  act(() => {
    rememberItem("acme", "support-bot", { kind: "dataset", id: "d1", label: "faq" });
    rememberItem("acme", "support-bot", { kind: "experiment", id: "x1", label: "baseline" });
  });
  expect(view.result.current.map((item) => item.id)).toEqual(["x1", "d1"]);

  const before = view.result.current;
  view.rerender();
  expect(view.result.current).toBe(before);

  localStorage.setItem(
    KEY,
    JSON.stringify({
      "acme/support-bot": [{ kind: "session", id: "sess-001", label: "sess-001" }],
    }),
  );
  act(() => {
    window.dispatchEvent(new StorageEvent("storage", { key: KEY }));
  });
  expect(view.result.current).toEqual([{ kind: "session", id: "sess-001", label: "sess-001" }]);
});

it("records an opened evaluator with its name", async () => {
  await renderApp(`/acme/support-bot/evaluators/${EVALUATOR}`);
  await waitFor(() =>
    expect(readRecentItems("acme", "support-bot")).toContainEqual({
      kind: "evaluator",
      id: EVALUATOR,
      label: "Relevance",
    }),
  );
});
