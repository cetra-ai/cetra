import { cn } from "cn";
import { Loader2, Plus, X } from "lucide-react";
import { type KeyboardEvent, useRef } from "react";
import type { Schema } from "@/api/types";
import { Button } from "@/components/ui/button";
import { MAX_TABS, type SqlTab, tabTitle } from "@/lib/sql/tabs";

/** The strip of query tabs: select, close (button or middle click), and add. */
export function QueryTabs({
  tabs,
  activeId,
  savedViews,
  running,
  onSelect,
  onClose,
  onAdd,
}: {
  tabs: readonly SqlTab[];
  activeId: string;
  /** The listed saved queries, which name the tabs that hold one. */
  savedViews: readonly Schema<"SavedView">[] | undefined;
  /** The ids of the tabs whose query runs. */
  running: ReadonlySet<string>;
  onSelect: (id: string) => void;
  onClose: (id: string) => void;
  onAdd: () => void;
}) {
  const elements = useRef(new Map<string, HTMLDivElement>());

  /** Arrow Left and Right move to the neighbouring tab, which is selected and focused. */
  function moveFocus(event: KeyboardEvent<HTMLDivElement>, index: number) {
    if (event.key !== "ArrowLeft" && event.key !== "ArrowRight") {
      return;
    }
    event.preventDefault();
    const step = event.key === "ArrowRight" ? 1 : -1;
    const next = tabs[(index + step + tabs.length) % tabs.length];
    if (next) {
      onSelect(next.id);
      elements.current.get(next.id)?.focus();
    }
  }

  return (
    <div
      role="tablist"
      aria-label="Query tabs"
      className="flex min-w-0 flex-1 items-center gap-1 overflow-x-auto"
    >
      {tabs.map((tab, index) => {
        const title = tabTitle(tab, savedViews);
        const isActive = tab.id === activeId;
        return (
          <div
            key={tab.id}
            ref={(node) => {
              if (node) {
                elements.current.set(tab.id, node);
              } else {
                elements.current.delete(tab.id);
              }
            }}
            role="tab"
            aria-selected={isActive}
            aria-label={title}
            title={title}
            tabIndex={isActive ? 0 : -1}
            className={cn(
              "group flex h-7 max-w-48 shrink-0 cursor-pointer items-center gap-1.5 rounded-md px-2 text-[12px]",
              isActive
                ? "bg-muted font-medium text-foreground"
                : "text-muted-foreground hover:bg-muted/60",
            )}
            onClick={() => onSelect(tab.id)}
            onAuxClick={(event) => {
              if (event.button === 1) {
                event.preventDefault();
                onClose(tab.id);
              }
            }}
            onKeyDown={(event) => moveFocus(event, index)}
          >
            <span className="min-w-0 truncate">{title}</span>
            {running.has(tab.id) ? (
              <Loader2 role="img" aria-label="Running" className="size-3 shrink-0 animate-spin" />
            ) : null}
            <button
              type="button"
              aria-label={`Close ${title}`}
              className={cn(
                "shrink-0 rounded p-0.5 hover:bg-background",
                isActive ? "" : "opacity-0 focus-visible:opacity-100 group-hover:opacity-100",
              )}
              onClick={(event) => {
                event.stopPropagation();
                onClose(tab.id);
              }}
            >
              <X className="size-3" />
            </button>
          </div>
        );
      })}
      <Button
        type="button"
        variant="ghost"
        size="icon-xs"
        aria-label="New query tab"
        disabled={tabs.length >= MAX_TABS}
        onClick={onAdd}
      >
        <Plus />
      </Button>
    </div>
  );
}
