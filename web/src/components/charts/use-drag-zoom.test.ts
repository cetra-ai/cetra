import { act, renderHook } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { indexOf, useDragZoom, zoomRange } from "@/components/charts/use-drag-zoom";
import type { ChartRow } from "@/lib/metrics";

const HOUR_MS = 3_600_000;
const BUCKET_NS = 3_600_000_000_000;
const ROWS: ChartRow[] = ["00", "01", "02"].map((hour) => ({
  time: Date.parse(`2026-11-17T${hour}:00:00Z`),
}));

function setup() {
  const onZoom = vi.fn();
  const hook = renderHook(() => useDragZoom({ rows: ROWS, bucketNs: BUCKET_NS, onZoom }));
  return {
    onZoom,
    result: hook.result,
    down: (index: string, clientX: number, button = 0, nativeEvent?: unknown) =>
      act(() => {
        hook.result.current.onMouseDown(
          { activeTooltipIndex: index },
          { button, clientX, nativeEvent },
        );
      }),
    move: (index: string, clientX: number) =>
      act(() => {
        hook.result.current.onMouseMove({ activeTooltipIndex: index }, { button: 0, clientX });
      }),
    up: (index?: string, clientX?: number) =>
      act(() => {
        if (index === undefined || clientX === undefined) {
          hook.result.current.onMouseUp();
        } else {
          hook.result.current.onMouseUp({ activeTooltipIndex: index }, { button: 0, clientX });
        }
      }),
    leave: () =>
      act(() => {
        hook.result.current.onMouseLeave();
      }),
    /** Whether the next click is swallowed (true) or passes through (false). */
    clickIsSwallowed: () => {
      const stopPropagation = vi.fn();
      act(() => {
        hook.result.current.onClickCapture({ stopPropagation });
      });
      return stopPropagation.mock.calls.length > 0;
    },
  };
}

describe("useDragZoom", () => {
  it("zooms when the release arrives before any move (Recharts delivers them on separate frames)", () => {
    const drag = setup();
    drag.down("0", 100);
    drag.up("2", 140);
    expect(drag.onZoom).toHaveBeenCalledWith(ROWS[0]?.time, (ROWS[2]?.time ?? 0) + HOUR_MS);
    expect(drag.clickIsSwallowed()).toBe(true);
  });

  it("zooms a drag of 12 px across three buckets, and swallows the click that ends it", () => {
    const drag = setup();
    drag.down("0", 100);
    drag.move("1", 106);
    drag.move("2", 112);
    expect(drag.result.current.shade).toEqual({ start: 0, end: 2 });
    drag.up();
    expect(drag.onZoom).toHaveBeenCalledWith(ROWS[0]?.time, (ROWS[2]?.time ?? 0) + HOUR_MS);
    expect(drag.result.current.shade).toBeNull();
    expect(drag.clickIsSwallowed()).toBe(true);
    expect(drag.clickIsSwallowed()).toBe(false);
  });

  it("does not zoom a drag of 4 px across two buckets, and the release is a click", () => {
    const drag = setup();
    drag.down("0", 100);
    drag.move("2", 104);
    expect(drag.result.current.shade).toBeNull();
    drag.up();
    expect(drag.onZoom).not.toHaveBeenCalled();
    expect(drag.clickIsSwallowed()).toBe(false);
  });

  it("does not zoom a long drag within one bucket", () => {
    const drag = setup();
    drag.down("1", 100);
    drag.move("1", 150);
    drag.up();
    expect(drag.onZoom).not.toHaveBeenCalled();
    expect(drag.clickIsSwallowed()).toBe(false);
  });

  it("ignores a press with a button other than the primary one", () => {
    const drag = setup();
    drag.down("0", 100, 2);
    drag.move("2", 160);
    drag.up();
    expect(drag.onZoom).not.toHaveBeenCalled();
    expect(drag.clickIsSwallowed()).toBe(false);
  });

  it("ignores a press from a touch or pen pointer", () => {
    const drag = setup();
    drag.down("0", 100, 0, { pointerType: "touch" });
    drag.move("2", 160);
    drag.up();
    expect(drag.onZoom).not.toHaveBeenCalled();
    drag.down("0", 100, 0, { pointerType: "pen" });
    drag.move("2", 160);
    drag.up();
    expect(drag.onZoom).not.toHaveBeenCalled();
  });

  it("cancels a drag on Escape and swallows the next click", () => {
    const drag = setup();
    drag.down("0", 100);
    drag.move("2", 112);
    expect(drag.result.current.shade).not.toBeNull();
    act(() => {
      document.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape" }));
    });
    expect(drag.result.current.shade).toBeNull();
    drag.up();
    expect(drag.onZoom).not.toHaveBeenCalled();
    expect(drag.clickIsSwallowed()).toBe(true);
  });

  it("cancels a drag when the pointer leaves the chart and swallows the next click", () => {
    const drag = setup();
    drag.down("0", 100);
    drag.move("2", 112);
    drag.leave();
    expect(drag.result.current.shade).toBeNull();
    drag.up();
    expect(drag.onZoom).not.toHaveBeenCalled();
    expect(drag.clickIsSwallowed()).toBe(true);
  });

  it("leaves a click alone when the pointer leaves outside a drag", () => {
    const drag = setup();
    drag.leave();
    expect(drag.clickIsSwallowed()).toBe(false);
  });
});

describe("indexOf", () => {
  it("reads Recharts 3's string index and a number", () => {
    expect(indexOf({ activeTooltipIndex: "2" })).toBe(2);
    expect(indexOf({ activeTooltipIndex: 3 })).toBe(3);
    expect(indexOf({ activeTooltipIndex: null })).toBeUndefined();
    expect(indexOf({ activeTooltipIndex: "" })).toBeUndefined();
    expect(indexOf({ activeTooltipIndex: "x" })).toBeUndefined();
  });
});

describe("zoomRange", () => {
  it("zooms a drag across buckets to the first start and the last end", () => {
    const first = ROWS[0]?.time ?? 0;
    // Dragging left to right and right to left give the same range: buckets 0 to 2 of 1 h each.
    const expected = { fromMs: first, toMs: first + 3 * HOUR_MS };
    expect(zoomRange(ROWS, 0, 2, BUCKET_NS)).toEqual(expected);
    expect(zoomRange(ROWS, 2, 0, BUCKET_NS)).toEqual(expected);
    // Two adjacent buckets already count as a drag.
    expect(zoomRange(ROWS, 1, 2, BUCKET_NS)).toEqual({
      fromMs: first + HOUR_MS,
      toMs: first + 3 * HOUR_MS,
    });
  });

  it("does not zoom a drag within one bucket, nor one over no rows", () => {
    expect(zoomRange(ROWS, 1, 1, BUCKET_NS)).toBeUndefined();
    expect(zoomRange(ROWS, 0, 9, BUCKET_NS)).toBeUndefined();
    expect(zoomRange([], 0, 1, BUCKET_NS)).toBeUndefined();
  });
});
