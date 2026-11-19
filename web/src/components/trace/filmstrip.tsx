import { cn } from "cn";
import { ChevronLeft, ChevronRight, List } from "lucide-react";
import { useEffect, useRef } from "react";
import type { Schema } from "@/api/types";
import { StatusDot } from "@/components/status-dot";
import { Button } from "@/components/ui/button";

export type FilmstripItem = {
  id: string;
  label: string;
  detail: string;
  status: Schema<"SpanStatus">;
};

/** A strip of the list's loaded rows above a full-page trace, for stepping between them. */
export function Filmstrip({
  items,
  selectedId,
  onSelect,
  onBack,
}: {
  items: readonly FilmstripItem[];
  selectedId?: string;
  onSelect: (id: string) => void;
  onBack: () => void;
}) {
  const stripRef = useRef<HTMLDivElement>(null);
  const selectedRef = useRef<HTMLButtonElement>(null);
  const index = items.findIndex((item) => item.id === selectedId);
  const previous = index > 0 ? items[index - 1] : undefined;
  const next = index >= 0 ? items[index + 1] : undefined;

  // A mouse wheel only scrolls vertically, so it scrolls the strip sideways instead. This needs a
  // native listener: React's wheel handlers are passive and cannot cancel the page scroll.
  useEffect(() => {
    const strip = stripRef.current;
    if (!strip) {
      return;
    }
    function onWheel(event: WheelEvent) {
      if (!strip || Math.abs(event.deltaY) <= Math.abs(event.deltaX)) {
        return;
      }
      event.preventDefault();
      strip.scrollLeft += event.deltaY;
    }
    strip.addEventListener("wheel", onWheel, { passive: false });
    return () => {
      strip.removeEventListener("wheel", onWheel);
    };
  }, []);

  // biome-ignore lint/correctness/useExhaustiveDependencies: scroll the chip into view when the selection changes
  useEffect(() => {
    // Scroll the strip only: scrollIntoView would also move the page's clipped ancestors.
    const strip = stripRef.current;
    const chip = selectedRef.current;
    if (strip && chip) {
      strip.scrollLeft = chip.offsetLeft - (strip.clientWidth - chip.offsetWidth) / 2;
    }
  }, [selectedId]);

  return (
    <nav aria-label="Rows" className="flex h-9 shrink-0 items-center gap-1 border-b px-2">
      <Button type="button" variant="ghost" size="xs" onClick={onBack}>
        <List />
        List
      </Button>
      <Button
        type="button"
        variant="ghost"
        size="icon-xs"
        aria-label="Previous"
        disabled={previous === undefined}
        onClick={() => {
          if (previous) {
            onSelect(previous.id);
          }
        }}
      >
        <ChevronLeft />
      </Button>
      <Button
        type="button"
        variant="ghost"
        size="icon-xs"
        aria-label="Next"
        disabled={next === undefined}
        onClick={() => {
          if (next) {
            onSelect(next.id);
          }
        }}
      >
        <ChevronRight />
      </Button>
      <div
        ref={stripRef}
        className="relative flex min-w-0 flex-1 items-center gap-1 overflow-x-auto"
      >
        {items.map((item) => {
          const selected = item.id === selectedId;
          return (
            <button
              key={item.id}
              ref={selected ? selectedRef : undefined}
              type="button"
              aria-current={selected ? "true" : undefined}
              onClick={() => {
                onSelect(item.id);
              }}
              className={cn(
                "inline-flex h-6 shrink-0 items-center gap-1.5 rounded-md border px-2 text-caption hover:bg-muted",
                selected && "border-ring bg-muted",
              )}
            >
              <StatusDot status={item.status} />
              <span className="max-w-32 truncate">{item.label}</span>
              <span className="num text-muted-foreground">{item.detail}</span>
            </button>
          );
        })}
      </div>
    </nav>
  );
}
