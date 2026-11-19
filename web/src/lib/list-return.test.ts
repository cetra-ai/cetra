import type { RouterHistory } from "@tanstack/react-router";
import { renderHook } from "@testing-library/react";
import { beforeEach, expect, it, vi } from "vitest";
import {
  type ListReturn,
  listReturnKey,
  markReturnPoint,
  readListReturn,
  rememberListReturn,
  useListReturn,
} from "@/lib/list-return";

const location = vi.hoisted(() => ({ state: {} as Record<string, unknown> }));

vi.mock("@tanstack/react-router", async (importOriginal) => {
  const actual = await importOriginal<typeof import("@tanstack/react-router")>();
  return { ...actual, useLocation: () => ({ state: location.state }) };
});

const stored: ListReturn = {
  list: "sessions",
  search: { sort: "cost_total", order: "asc" },
  anchorId: "sess-002",
  anchorOffset: 4,
  openedId: "sess-003",
};

const fromState: ListReturn = { ...stored, search: { sort: "start_time" }, openedId: "sess-009" };

beforeEach(() => {
  sessionStorage.clear();
  location.state = {};
});

it("prefers the return in the history state over the stored one", () => {
  rememberListReturn(stored);
  location.state = { listReturn: fromState };
  const { result } = renderHook(() => useListReturn("sessions"));
  expect(result.current).toEqual(fromState);
});

it("falls back to the stored return when the history state has none", () => {
  rememberListReturn(stored);
  const { result } = renderHook(() => useListReturn("sessions"));
  expect(result.current).toEqual(stored);
});

it("returns undefined when neither source has a return", () => {
  const { result } = renderHook(() => useListReturn("sessions"));
  expect(result.current).toBeUndefined();
});

it("ignores a history return that belongs to another list", () => {
  location.state = { listReturn: { ...fromState, list: "traces" } };
  const { result } = renderHook(() => useListReturn("sessions"));
  expect(result.current).toBeUndefined();
});

it("keeps each list's stored return under its own key", () => {
  rememberListReturn(stored);
  expect(listReturnKey("sessions")).toBe("cetra.list-return.sessions");
  expect(readListReturn("sessions")).toEqual(stored);
  expect(readListReturn("traces")).toBeUndefined();
});

it("treats a corrupt stored return as absent", () => {
  sessionStorage.setItem(listReturnKey("traces"), "{not json");
  expect(readListReturn("traces")).toBeUndefined();
  sessionStorage.setItem(listReturnKey("traces"), JSON.stringify({ list: "traces" }));
  expect(readListReturn("traces")).toBeUndefined();
});

it("marks the list's own entry and flushes it before the detail page is pushed", () => {
  const calls: string[] = [];
  const history = {
    location: { href: "/acme/support-bot/sessions?sort=cost_total", state: { __TSR_index: 0 } },
    replace: (href: string, state: unknown) => {
      calls.push(`replace ${href} ${JSON.stringify(state)}`);
    },
    flush: () => {
      calls.push("flush");
    },
  } as unknown as RouterHistory;
  markReturnPoint(history, {
    list: "sessions",
    search: { sort: "cost_total" },
    anchorId: "sess-0044",
    anchorOffset: 4,
    openedId: "sess-0114",
  });
  expect(calls).toEqual([
    'replace /acme/support-bot/sessions?sort=cost_total {"__TSR_index":0,"listRestore":{"anchorId":"sess-0044","anchorOffset":4,"openedId":"sess-0114"}}',
    "flush",
  ]);
});
