import { useEffect, useRef, useState } from "react";
import type { ChartRow } from "@/lib/metrics";

// Bucket widths are in nanoseconds (`bucket_ns`); row times are in milliseconds.
const NS_PER_MS = 1_000_000;
// A press becomes a zoom only once the pointer has moved this far sideways; less is a click.
const DRAG_PX = 8;

/** The pointer fields a drag reads. Recharts passes the React mouse event with the chart state. */
type PointerLike = { button: number; clientX: number; nativeEvent?: unknown };
/** The Recharts mouse state: the index of the bucket under the pointer, as a string or a number. */
type ChartState = { activeTooltipIndex?: unknown };

type Press = {
  start: number;
  end: number;
  /** Where the press began, in client pixels. */
  x: number;
  /** The farthest the pointer has moved sideways from `x`. */
  reach: number;
  /** Whether the shade is shown: the drag is a zoom so far. */
  armed: boolean;
};

/**
 * Drag to zoom on a chart. A primary-button press (not a touch or pen pointer) starts a drag;
 * it zooms to the buckets it spans when the pointer moved at least 8 px sideways and the drag
 * covers two buckets or more. Any other release is an ordinary click. Escape, or leaving the
 * chart, cancels a drag in progress, and the click that follows is swallowed.
 */
export function useDragZoom({
  rows,
  bucketNs,
  onZoom,
}: {
  rows: readonly ChartRow[];
  bucketNs: number;
  onZoom?: (fromMs: number, toMs: number) => void;
}) {
  const press = useRef<Press | null>(null);
  // Set when a press zoomed or was cancelled: the click that ends it opens no bucket.
  const swallowClick = useRef(false);
  const [pressed, setPressed] = useState(false);
  const [shade, setShade] = useState<{ start: number; end: number } | null>(null);

  // Escape ends a press in progress, wherever the focus is.
  useEffect(() => {
    if (!pressed) {
      return;
    }
    function onEscape(event: KeyboardEvent) {
      if (event.key !== "Escape" || !press.current) {
        return;
      }
      press.current = null;
      setPressed(false);
      setShade(null);
      swallowClick.current = true;
    }
    document.addEventListener("keydown", onEscape);
    return () => document.removeEventListener("keydown", onEscape);
  }, [pressed]);

  function onMouseDown(state: ChartState, event: PointerLike) {
    swallowClick.current = false;
    if (!onZoom || event.button !== 0 || isTouchOrPen(event)) {
      return;
    }
    const index = indexOf(state);
    if (index === undefined) {
      return;
    }
    press.current = { start: index, end: index, x: event.clientX, reach: 0, armed: false };
    setPressed(true);
  }

  function onMouseMove(state: ChartState, event: PointerLike) {
    const current = press.current;
    if (!current) {
      return;
    }
    current.reach = Math.max(current.reach, Math.abs(event.clientX - current.x));
    const index = indexOf(state);
    let moved = false;
    if (index !== undefined && index !== current.end) {
      current.end = index;
      moved = true;
    }
    const armed = current.reach >= DRAG_PX && current.end !== current.start;
    if (moved || armed !== current.armed) {
      current.armed = armed;
      setShade(armed ? { start: current.start, end: current.end } : null);
    }
  }

  // Recharts delivers each event type on its own animation frame, so a quick drag can reach
  // mouse up before its last move: the release's own position and bucket count too.
  function onMouseUp(state?: ChartState, event?: PointerLike) {
    const current = press.current;
    press.current = null;
    setPressed(false);
    setShade(null);
    if (current && event) {
      current.reach = Math.max(current.reach, Math.abs(event.clientX - current.x));
      const index = state ? indexOf(state) : undefined;
      if (index !== undefined) {
        current.end = index;
      }
    }
    if (!current || !onZoom || current.reach < DRAG_PX) {
      return;
    }
    const range = zoomRange(rows, current.start, current.end, bucketNs);
    if (range) {
      swallowClick.current = true;
      onZoom(range.fromMs, range.toMs);
    }
  }

  function onMouseLeave() {
    if (!press.current) {
      return;
    }
    press.current = null;
    setPressed(false);
    setShade(null);
    swallowClick.current = true;
  }

  function onClickCapture(event: { stopPropagation: () => void }) {
    // The click that ends a zoom drag (or a cancelled one) must not open a bucket.
    if (swallowClick.current) {
      swallowClick.current = false;
      event.stopPropagation();
    }
  }

  return { shade, onMouseDown, onMouseMove, onMouseUp, onMouseLeave, onClickCapture };
}

/** Touch and pen presses do not drag: only the mouse does. */
function isTouchOrPen(event: PointerLike): boolean {
  const pointer = (event.nativeEvent as { pointerType?: unknown } | undefined)?.pointerType;
  return typeof pointer === "string" && pointer !== "mouse";
}

/**
 * The bucket index under the pointer, from a Recharts mouse state. Recharts 3 gives it as a
 * string (`"2"`); a number is accepted too.
 */
export function indexOf(state: { activeTooltipIndex?: unknown }): number | undefined {
  const raw = state.activeTooltipIndex;
  const value = typeof raw === "string" && raw !== "" ? Number(raw) : raw;
  return typeof value === "number" && Number.isInteger(value) && value >= 0 ? value : undefined;
}

/**
 * The range a drag from bucket index `a` to index `b` (either order) zooms to: from the first
 * bucket's start to the last bucket's end. A drag across fewer than two buckets zooms to nothing,
 * so it stays a click.
 */
export function zoomRange(
  rows: readonly ChartRow[],
  a: number,
  b: number,
  bucketNs: number,
): { fromMs: number; toMs: number } | undefined {
  const low = Math.min(a, b);
  const high = Math.max(a, b);
  if (high - low < 1) {
    return undefined;
  }
  const first = rows[low]?.time;
  const last = rows[high]?.time;
  if (first === undefined || last === undefined) {
    return undefined;
  }
  return { fromMs: first, toMs: last + bucketNs / NS_PER_MS };
}
