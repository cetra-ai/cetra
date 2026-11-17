import { cn } from "cn";
import { type ReactNode, useEffect, useRef, useState } from "react";

/** Clips content taller than `maxHeight` behind a fade, with a button that expands it. */
export function ShowMore({
  children,
  maxHeight = 480,
}: {
  children: ReactNode;
  maxHeight?: number;
}) {
  const ref = useRef<HTMLDivElement>(null);
  const [height, setHeight] = useState(0);
  const [expanded, setExpanded] = useState(false);

  useEffect(() => {
    const element = ref.current;
    if (!element) {
      return;
    }
    const measure = () => {
      // A container with no width (a collapsed or hidden pane) wraps text one letter per line;
      // its height says nothing about the content, so it is ignored.
      if (element.clientWidth === 0) {
        return;
      }
      setHeight(element.scrollHeight);
    };
    measure();
    const observer = new ResizeObserver(measure);
    observer.observe(element);
    return () => {
      observer.disconnect();
    };
  }, []);

  const overflowing = height > maxHeight;
  const clipped = overflowing && !expanded;
  return (
    <div className="flex min-w-0 flex-col gap-1">
      <div
        ref={ref}
        className={cn("relative", clipped && "overflow-hidden")}
        style={clipped ? { maxHeight } : undefined}
      >
        {children}
        {clipped ? (
          <div className="pointer-events-none absolute inset-x-0 bottom-0 h-12 bg-linear-to-t from-card to-transparent" />
        ) : null}
      </div>
      {overflowing ? (
        <button
          type="button"
          className="self-start text-[12px] text-primary"
          aria-expanded={expanded}
          onClick={() => {
            setExpanded((current) => !current);
          }}
        >
          {expanded ? "Show less" : "Show more"}
        </button>
      ) : null}
    </div>
  );
}
