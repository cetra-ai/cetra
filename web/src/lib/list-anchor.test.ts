import { act, renderHook } from "@testing-library/react";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import {
  ANCHOR_REUSE_MS,
  anchorKey,
  resetListAnchors,
  useListAnchor,
  type WindowSearch,
} from "@/lib/list-anchor";

const MINUTE = 60_000;
const T0 = Date.parse("2026-11-20T10:00:00.000Z");

const SEARCH: WindowSearch = {
  rangeKey: "24h",
  filter: undefined,
  search: undefined,
  sort: "start_time",
  order: "desc",
  eval: false,
  exp: false,
};
const KEY = anchorKey("acme", "shop", "spans", SEARCH);
const OTHER_KEY = anchorKey("acme", "shop", "spans", { ...SEARCH, filter: "model = 'x'" });

/** Moves the fake clock to `ms` after T0. */
function at(ms: number): void {
  vi.setSystemTime(T0 + ms);
}

beforeEach(() => {
  vi.useFakeTimers({ toFake: ["Date"] });
  at(0);
});

afterEach(() => {
  vi.useRealTimers();
  resetListAnchors();
});

it("keys a window by org, project, list and each search field", () => {
  const variants = [
    anchorKey("other", "shop", "spans", SEARCH),
    anchorKey("acme", "other", "spans", SEARCH),
    anchorKey("acme", "shop", "traces", SEARCH),
    anchorKey("acme", "shop", "spans", { ...SEARCH, rangeKey: "1h" }),
    anchorKey("acme", "shop", "spans", { ...SEARCH, filter: "x" }),
    anchorKey("acme", "shop", "spans", { ...SEARCH, search: "x" }),
    anchorKey("acme", "shop", "spans", { ...SEARCH, sort: "cost" }),
    anchorKey("acme", "shop", "spans", { ...SEARCH, order: "asc" }),
    anchorKey("acme", "shop", "spans", { ...SEARCH, eval: true }),
    anchorKey("acme", "shop", "spans", { ...SEARCH, exp: true }),
  ];
  expect(new Set([KEY, ...variants]).size).toBe(variants.length + 1);
  expect(anchorKey("acme", "shop", "spans", { ...SEARCH })).toBe(KEY);
});

it("gives a first mount a new anchor at now", () => {
  const { result } = renderHook(() => useListAnchor(KEY, false));
  expect(result.current.reused).toBe(false);
  expect(result.current.anchor).toBe(T0);
  expect(result.current.scroll).toBeUndefined();
});

it("reuses the anchor when the same key is mounted again within 15 minutes", () => {
  const first = renderHook(() => useListAnchor(KEY, false));
  first.unmount();
  at(14 * MINUTE + 59_000);
  const second = renderHook(() => useListAnchor(KEY, false));
  expect(second.result.current.reused).toBe(true);
  expect(second.result.current.anchor).toBe(T0);
});

it("does not reuse the anchor exactly 15 minutes after it was set", () => {
  const first = renderHook(() => useListAnchor(KEY, false));
  first.unmount();
  at(ANCHOR_REUSE_MS);
  const second = renderHook(() => useListAnchor(KEY, false));
  expect(second.result.current.reused).toBe(false);
  expect(second.result.current.anchor).toBe(T0 + ANCHOR_REUSE_MS);
});

it("fresh ignores and deletes a stored anchor, and stores nothing", () => {
  const first = renderHook(() => useListAnchor(KEY, false));
  first.unmount();
  at(MINUTE);
  const live = renderHook(() => useListAnchor(KEY, true));
  expect(live.result.current.reused).toBe(false);
  expect(live.result.current.anchor).toBe(T0 + MINUTE);
  act(() => {
    live.result.current.rememberScroll({ rowId: "row-3", offset: 4 });
  });
  live.unmount();
  at(2 * MINUTE);
  const back = renderHook(() => useListAnchor(KEY, false));
  expect(back.result.current.reused).toBe(false);
  expect(back.result.current.anchor).toBe(T0 + 2 * MINUTE);
  expect(back.result.current.scroll).toBeUndefined();
});

it("a key change while fresh does not reuse the new key's stored anchor", () => {
  const stored = renderHook(() => useListAnchor(OTHER_KEY, false));
  stored.unmount();
  at(MINUTE);
  const mounted = renderHook(
    ({ key, fresh }: { key: string; fresh: boolean }) => useListAnchor(key, fresh),
    { initialProps: { key: KEY, fresh: false } },
  );
  mounted.rerender({ key: OTHER_KEY, fresh: true });
  expect(mounted.result.current.reused).toBe(false);
  expect(mounted.result.current.anchor).toBe(T0 + MINUTE);
});

it("prunes entries older than 15 minutes when another key mounts", () => {
  const a = renderHook(() => useListAnchor(KEY, false));
  a.unmount();
  at(ANCHOR_REUSE_MS);
  const b = renderHook(() => useListAnchor(OTHER_KEY, false));
  b.unmount();
  // The entry of KEY is pruned, so its lookup finds nothing to reuse.
  const again = renderHook(() => useListAnchor(KEY, false));
  expect(again.result.current.reused).toBe(false);
  expect(again.result.current.anchor).toBe(T0 + ANCHOR_REUSE_MS);
});

it("reports the lookup time, which is not the anchor's time when reused", () => {
  const first = renderHook(() => useListAnchor(KEY, false));
  first.unmount();
  at(5 * MINUTE);
  const second = renderHook(() => useListAnchor(KEY, false));
  expect(second.result.current.anchor).toBe(T0);
  expect(second.result.current.lookedUpAt).toBe(T0 + 5 * MINUTE);
});

it("a key change on a mounted page looks the new key up again", () => {
  const stored = renderHook(() => useListAnchor(OTHER_KEY, false));
  stored.unmount();
  at(MINUTE);
  const mounted = renderHook(({ key }: { key: string }) => useListAnchor(key, false), {
    initialProps: { key: KEY },
  });
  expect(mounted.result.current.anchor).toBe(T0 + MINUTE);
  mounted.rerender({ key: OTHER_KEY });
  expect(mounted.result.current.reused).toBe(true);
  expect(mounted.result.current.anchor).toBe(T0);
});

it("renew sets the anchor to now, and the next mount reuses the renewed anchor", () => {
  const { result, unmount } = renderHook(() => useListAnchor(KEY, false));
  at(5 * MINUTE);
  act(() => {
    result.current.renew();
  });
  expect(result.current.reused).toBe(false);
  expect(result.current.anchor).toBe(T0 + 5 * MINUTE);
  unmount();
  at(6 * MINUTE);
  const next = renderHook(() => useListAnchor(KEY, false));
  expect(next.result.current.reused).toBe(true);
  expect(next.result.current.anchor).toBe(T0 + 5 * MINUTE);
});

it("returns a remembered scroll on the next mount with the same anchor, and not after renew", () => {
  const first = renderHook(() => useListAnchor(KEY, false));
  act(() => {
    first.result.current.rememberScroll({ rowId: "row-9", offset: 12 });
  });
  first.unmount();
  const second = renderHook(() => useListAnchor(KEY, false));
  expect(second.result.current.scroll).toEqual({ rowId: "row-9", offset: 12 });
  second.unmount();

  at(MINUTE);
  const renewed = renderHook(() => useListAnchor(KEY, false));
  act(() => {
    renewed.result.current.renew();
  });
  renewed.unmount();
  const afterRenew = renderHook(() => useListAnchor(KEY, false));
  expect(afterRenew.result.current.reused).toBe(true);
  expect(afterRenew.result.current.scroll).toBeUndefined();
});

it("resetListAnchors forgets every remembered anchor", () => {
  const first = renderHook(() => useListAnchor(KEY, false));
  first.unmount();
  resetListAnchors();
  at(MINUTE);
  const second = renderHook(() => useListAnchor(KEY, false));
  expect(second.result.current.reused).toBe(false);
  expect(second.result.current.anchor).toBe(T0 + MINUTE);
});
