import { type RefObject, useLayoutEffect, useRef, useState } from "react";
import { flushSync } from "react-dom";

/**
 * The width of the dashboard grid's container, in step with it frame by frame. It is measured
 * before the first paint (null until then, so the grid is not drawn at a guessed width), and each
 * later change is rendered inside the ResizeObserver callback, before the browser paints. The
 * library's own hook (`useContainerWidth`) applies a change one animation frame later, so during a
 * window resize or a zoom the cells kept their old widths for a frame and then corrected themselves.
 */
export function useGridWidth(): {
  width: number | null;
  containerRef: RefObject<HTMLDivElement | null>;
} {
  const containerRef = useRef<HTMLDivElement>(null);
  const [width, setWidth] = useState<number | null>(null);
  useLayoutEffect(() => {
    const node = containerRef.current;
    if (!node) return;
    setWidth(node.clientWidth);
    if (typeof ResizeObserver === "undefined") return;
    const observer = new ResizeObserver((entries) => {
      const next = Math.round(entries[0]?.contentRect.width ?? node.clientWidth);
      flushSync(() => setWidth((prev) => (prev === next ? prev : next)));
    });
    observer.observe(node);
    return () => observer.disconnect();
  }, []);
  return { width, containerRef };
}
