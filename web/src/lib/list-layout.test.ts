import { act, renderHook } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { startAnalytics, type WireEvent } from "@/lib/analytics";
import {
  getListLayout,
  LIST_LAYOUT_STORAGE_KEY,
  setListLayout,
  useListLayout,
} from "@/lib/list-layout";

describe("list layout preference", () => {
  afterEach(() => {
    vi.restoreAllMocks();
    localStorage.clear();
    setListLayout("drawer");
  });

  it("a layout change is tracked once", () => {
    const sent: WireEvent[][] = [];
    const stop = startAnalytics({
      send: async (events) => {
        sent.push(events);
      },
      flushMs: 60_000,
    });
    try {
      setListLayout("bottom");
      setListLayout("bottom");
    } finally {
      stop();
    }
    expect(sent.flat()).toEqual([
      { name: "list_layout.changed", properties: { layout: "bottom" } },
    ]);
  });

  it("defaults to split", () => {
    localStorage.clear();
    expect(getListLayout()).toBe("drawer");
  });

  it("stores the choice in localStorage", () => {
    setListLayout("bottom");
    expect(getListLayout()).toBe("bottom");
    expect(localStorage.getItem(LIST_LAYOUT_STORAGE_KEY)).toBe("bottom");
  });

  it("reads a garbage stored value as split", () => {
    localStorage.setItem(LIST_LAYOUT_STORAGE_KEY, "nope");
    expect(getListLayout()).toBe("drawer");
  });

  it("re-renders a hook consumer when the layout changes", () => {
    const { result } = renderHook(() => useListLayout());
    expect(result.current).toBe("drawer");

    act(() => {
      setListLayout("bottom");
    });

    expect(result.current).toBe("bottom");
  });

  it("keeps the choice when storage throws", () => {
    vi.spyOn(Storage.prototype, "getItem").mockImplementation(() => {
      throw new Error("denied");
    });
    vi.spyOn(Storage.prototype, "setItem").mockImplementation(() => {
      throw new Error("denied");
    });
    expect(() => setListLayout("drawer")).not.toThrow();
    expect(getListLayout()).toBe("drawer");
  });
});
