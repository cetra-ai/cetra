import {
  type ReactNode,
  type PointerEvent as ReactPointerEvent,
  useEffect,
  useRef,
  useState,
} from "react";
import { useGroupRef } from "react-resizable-panels";
import { ResizableHandle, ResizablePanel, ResizablePanelGroup } from "@/components/ui/resizable";
import { setTraceMaximized, useTraceMaximized } from "@/lib/list-layout";
import { STORAGE_PREFIX } from "@/product";

type SplitName =
  | "traces"
  | "trace"
  | "trace-timeline"
  | "session"
  | "users"
  | "alerts"
  | "experiments";

export type SplitMode = "split" | "bottom" | "drawer" | "page";

const PANES = {
  traces: { start: "traces-list", end: "traces-detail", startSize: 40 },
  trace: { start: "trace-tree", end: "trace-detail", startSize: 40 },
  "trace-timeline": { start: "trace-timeline-tree", end: "trace-timeline-detail", startSize: 60 },
  session: { start: "session-turns", end: "session-trace", startSize: 55 },
  users: { start: "users-list", end: "users-detail", startSize: 40 },
  alerts: { start: "alerts-list", end: "alerts-detail", startSize: 40 },
  experiments: { start: "experiments-list", end: "experiments-detail", startSize: 40 },
} as const;

type SplitPaneProps = {
  name: SplitName;
  mode?: SplitMode;
  open?: boolean;
  start: ReactNode;
  end: ReactNode;
  /** Shown above the trace in page mode. */
  filmstrip?: ReactNode;
  /** Bottom mode without the slide-in, for a split that is always open (the session's trace). */
  still?: boolean;
};

export function SplitPane({ mode = "split", ...props }: SplitPaneProps) {
  const maximized = useTraceMaximized();
  const open = props.open ?? true;
  const fills = open && maximized;

  // Switching layouts with a trace open closes the current layout, then opens the new one, both
  // with the faster animations. With no trace open the layout changes at once.
  const [shownMode, setShownMode] = useState(mode);
  const [phase, setPhase] = useState<"idle" | "leaving" | "entering">("idle");
  if (mode !== shownMode && phase === "idle") {
    if (open) {
      setPhase("leaving");
    } else {
      setShownMode(mode);
    }
  }
  useEffect(() => {
    if (phase === "idle") {
      return;
    }
    const timer = window.setTimeout(() => {
      if (phase === "leaving") {
        setShownMode(mode);
        setPhase("entering");
      } else {
        setPhase("idle");
      }
    }, slideMs(true));
    return () => {
      window.clearTimeout(timer);
    };
  }, [phase, mode]);

  useEffect(() => {
    if (!open) {
      setTraceMaximized(false);
    }
  }, [open]);

  // A mode change remounts the group, so the list remounts too (accepted: the list scroll resets).
  // A maximized trace is an overlay over the group, which keeps its layout underneath.
  return (
    <div data-fast={phase === "idle" ? undefined : ""} className="relative h-full min-h-0">
      <SplitGroup
        key={shownMode}
        mode={shownMode}
        {...props}
        open={phase === "leaving" ? false : props.open}
        fast={phase !== "idle"}
        end={fills ? null : props.end}
      />
      {fills ? (
        <section aria-label="Trace" className="absolute inset-0 z-40 flex flex-col bg-background">
          {props.end}
        </section>
      ) : null}
    </div>
  );
}

function SplitGroup({
  name,
  mode,
  open = true,
  start,
  end,
  filmstrip,
  still = false,
  fast = false,
}: SplitPaneProps & { mode: SplitMode; fast?: boolean }) {
  const panes = PANES[name];
  const slides = mode === "bottom" && !still;
  const groupRef = useGroupRef();
  // Bottom opens closed and slides the trace in, also when it mounts open (a layout switch).
  const [initial] = useState(() =>
    open && !slides ? (readSplit(name, mode) ?? fallback(name, mode)) : closedLayout(name),
  );

  // In bottom mode the trace slides over the list before the layout opens, and slides back down
  // before the list is given the space back, so the list stays visible underneath.
  const pageTree = (
    <>
      {filmstrip}
      <div className="relative min-h-0 flex-1">
        <div className="absolute inset-0 flex min-h-0 flex-col">{end}</div>
      </div>
    </>
  );
  const exit = useExit(open, mode === "page" ? pageTree : end, fast);
  const [sliding, setSliding] = useState(open && slides);
  // The slide waits two frames, so a heavy mount (a layout switch) cannot use up its 200ms before
  // the first paint; until then the trace waits just below the list.
  const [armed, setArmed] = useState(false);
  const [seenOpen, setSeenOpen] = useState(open);
  if (seenOpen !== open) {
    setSeenOpen(open);
    setSliding(open && slides);
    setArmed(false);
  }
  useEffect(() => {
    if (!sliding || armed) {
      return;
    }
    let second = 0;
    const first = window.requestAnimationFrame(() => {
      second = window.requestAnimationFrame(() => {
        setArmed(true);
      });
    });
    return () => {
      window.cancelAnimationFrame(first);
      window.cancelAnimationFrame(second);
    };
  }, [sliding, armed]);
  useEffect(() => {
    if (!sliding || !armed) {
      return;
    }
    const timer = window.setTimeout(() => {
      setSliding(false);
    }, slideMs(fast));
    return () => {
      window.clearTimeout(timer);
    };
  }, [sliding, armed, fast]);
  const layoutOpen = slides ? open && !sliding : open;
  const overlay = slides && (sliding || exit.closing);

  useEffect(() => {
    if (mode === "drawer" || mode === "page") {
      return;
    }
    const handle = groupRef.current;
    if (!handle) {
      return;
    }
    // Both panels stay mounted, so opening the detail does not remount the list.
    handle.setLayout(
      layoutOpen ? (readSplit(name, mode) ?? fallback(name, mode)) : closedLayout(name),
    );
  }, [groupRef, name, mode, layoutOpen]);

  if (mode === "page") {
    // The list stays mounted but hidden while a trace is open, so its rows and scroll survive.
    // On close it is shown at once and the page fades out over it.
    const hideList = exit.mounted && !exit.closing;
    return (
      <div className="relative h-full min-h-0">
        <div className={hideList ? "hidden" : "absolute inset-0 flex min-h-0 flex-col"}>
          {start}
        </div>
        {exit.mounted ? (
          <div
            className={`absolute inset-0 z-10 flex min-h-0 flex-col bg-background ${
              exit.closing ? "page-out" : "page-in"
            }`}
          >
            {exit.content}
          </div>
        ) : null}
      </div>
    );
  }

  if (mode === "drawer") {
    // Non-modal: no scrim and no focus trap, so the list stays usable.
    return (
      <div className="relative h-full min-h-0">
        <div className="absolute inset-0 flex min-h-0 flex-col">{start}</div>
        <DrawerHost name={name} open={open} fast={fast}>
          {end}
        </DrawerHost>
      </div>
    );
  }

  return (
    <ResizablePanelGroup
      id={`split-${name}`}
      groupRef={groupRef}
      orientation={mode === "bottom" ? "vertical" : "horizontal"}
      disabled={!layoutOpen}
      className="relative h-full min-h-0"
      defaultLayout={initial}
      onLayoutChanged={(_layout, meta) => {
        if (!layoutOpen || !meta.isUserInteraction || !meta.requestedLayout) {
          return;
        }
        writeSplit(name, mode, meta.requestedLayout);
      }}
    >
      <ResizablePanel
        id={panes.start}
        minSize={layoutOpen ? "15%" : "0%"}
        className="relative"
        style={{ overflow: "hidden" }}
      >
        <div className="absolute inset-0 flex min-h-0 flex-col">{start}</div>
      </ResizablePanel>
      {layoutOpen ? <ResizableHandle /> : null}
      <ResizablePanel
        id={panes.end}
        minSize={layoutOpen ? "15%" : "0%"}
        // While the trace is an overlay the group is its containing block, so the panel's
        // overflow clip does not hide it.
        className={overlay ? undefined : "relative"}
        style={{ overflow: "hidden" }}
      >
        <div
          className={
            overlay
              ? `absolute inset-x-0 bottom-0 z-10 flex min-h-0 flex-col border-t bg-background ${
                  exit.closing ? "slide-down-out" : armed ? "slide-up-in" : "slide-start"
                }`
              : "absolute inset-0 flex min-h-0 flex-col"
          }
          style={
            overlay
              ? { height: `${(readSplit(name, mode) ?? fallback(name, mode))[panes.end]}%` }
              : undefined
          }
        >
          {mode === "bottom" ? (exit.mounted ? exit.content : null) : open ? end : null}
        </div>
      </ResizablePanel>
    </ResizablePanelGroup>
  );
}

// The traces list takes less of the height than of the width when the trace is below it.
const BOTTOM_START_SIZE = 40;

// Matches the 200ms slide animations in styles.css, plus a frame of slack.
const EXIT_MS = 220;

// The faster slides used while switching layouts; they match the `[data-fast]` rules in styles.css.
const FAST_EXIT_MS = 140;

function slideMs(fast: boolean): number {
  const reduced = window.matchMedia?.("(prefers-reduced-motion: reduce)").matches ?? false;
  if (reduced) {
    return 0;
  }
  return fast ? FAST_EXIT_MS : EXIT_MS;
}

const DRAWER_DEFAULT = 70;
const DRAWER_MIN = 30;
const DRAWER_MAX = 95;
const DRAWER_KEY_STEP = 5;

function clampDrawer(percent: number): number {
  return Math.min(DRAWER_MAX, Math.max(DRAWER_MIN, percent));
}

// Keeps the drawer mounted while it slides out, showing the trace it had when it closed.
function DrawerHost({
  name,
  open,
  fast,
  children,
}: {
  name: SplitName;
  open: boolean;
  fast: boolean;
  children: ReactNode;
}) {
  const exit = useExit(open, children, fast);
  if (!exit.mounted) {
    return null;
  }
  return (
    <Drawer name={name} closing={exit.closing}>
      {exit.content}
    </Drawer>
  );
}

/**
 * Keeps `content` mounted for the slide-out after `open` turns false, showing what it had when it
 * closed. Reopening during the exit cancels it.
 */
function useExit(open: boolean, content: ReactNode, fast: boolean) {
  const [mounted, setMounted] = useState(open);
  const last = useRef(content);
  if (open) {
    last.current = content;
    if (!mounted) {
      setMounted(true);
    }
  }

  useEffect(() => {
    if (open || !mounted) {
      return;
    }
    const timer = window.setTimeout(() => {
      setMounted(false);
    }, slideMs(fast));
    return () => {
      window.clearTimeout(timer);
    };
  }, [open, mounted, fast]);

  return { mounted, closing: !open, content: last.current };
}

// The drawer's width is a percentage of the list area, dragged from its left edge.
function Drawer({
  name,
  closing,
  children,
}: {
  name: SplitName;
  closing: boolean;
  children: ReactNode;
}) {
  const [width, setWidth] = useState(() => readDrawer(name));
  const asideRef = useRef<HTMLElement>(null);

  function commit(next: number) {
    const clamped = clampDrawer(next);
    setWidth(clamped);
    writeDrawer(name, clamped);
  }

  function onPointerDown(event: ReactPointerEvent<HTMLDivElement>) {
    const container = asideRef.current?.parentElement;
    if (!container) {
      return;
    }
    event.preventDefault();
    const handle = event.currentTarget;
    handle.setPointerCapture(event.pointerId);
    const move = (moveEvent: PointerEvent) => {
      const box = container.getBoundingClientRect();
      setWidth(clampDrawer(((box.right - moveEvent.clientX) / box.width) * 100));
    };
    const stop = (upEvent: PointerEvent) => {
      handle.removeEventListener("pointermove", move);
      handle.removeEventListener("pointerup", stop);
      handle.removeEventListener("pointercancel", stop);
      const box = container.getBoundingClientRect();
      commit(((box.right - upEvent.clientX) / box.width) * 100);
    };
    handle.addEventListener("pointermove", move);
    handle.addEventListener("pointerup", stop);
    handle.addEventListener("pointercancel", stop);
  }

  return (
    <aside
      ref={asideRef}
      aria-label="Trace"
      style={{ width: `${width}%` }}
      className={`${closing ? "drawer-out" : "drawer-in"} absolute inset-y-0 right-0 z-20 flex min-w-[480px] max-w-full flex-col border-l bg-background shadow-xl`}
    >
      {/* biome-ignore lint/a11y/useSemanticElements: a focusable window splitter, not a thematic break */}
      <div
        role="separator"
        aria-orientation="vertical"
        aria-label="Resize trace"
        aria-valuemin={DRAWER_MIN}
        aria-valuemax={DRAWER_MAX}
        aria-valuenow={Math.round(width)}
        tabIndex={0}
        onPointerDown={onPointerDown}
        onKeyDown={(event) => {
          if (event.key === "ArrowLeft") {
            event.preventDefault();
            commit(width + DRAWER_KEY_STEP);
          } else if (event.key === "ArrowRight") {
            event.preventDefault();
            commit(width - DRAWER_KEY_STEP);
          }
        }}
        className="absolute inset-y-0 -left-1 z-30 w-2 cursor-col-resize touch-none hover:bg-ring/40 focus-visible:bg-ring/60 focus-visible:outline-hidden"
      />
      {children}
    </aside>
  );
}

function drawerKey(name: SplitName): string {
  return `${STORAGE_PREFIX}split.${name}.drawer`;
}

function readDrawer(name: SplitName): number {
  try {
    const value = Number(localStorage.getItem(drawerKey(name)));
    return Number.isFinite(value) && value >= DRAWER_MIN && value <= DRAWER_MAX
      ? value
      : DRAWER_DEFAULT;
  } catch {
    return DRAWER_DEFAULT;
  }
}

function writeDrawer(name: SplitName, percent: number): void {
  try {
    localStorage.setItem(drawerKey(name), String(percent));
  } catch {
    // Private mode and quota failures leave the in-memory width in place.
  }
}

function closedLayout(name: SplitName): Record<string, number> {
  const panes = PANES[name];
  return { [panes.start]: 100, [panes.end]: 0 };
}

function startSize(name: SplitName, mode: SplitMode): number {
  if (name === "traces" && mode === "bottom") {
    return BOTTOM_START_SIZE;
  }
  return PANES[name].startSize;
}

function fallback(name: SplitName, mode: SplitMode): Record<string, number> {
  const panes = PANES[name];
  const start = startSize(name, mode);
  return { [panes.start]: start, [panes.end]: 100 - start };
}

// Split keeps today's key so stored ratios survive; the other modes get their own.
function storageKey(name: SplitName, mode: SplitMode): string {
  const suffix = mode === "split" ? "" : `.${mode}`;
  return `${STORAGE_PREFIX}split.${name}${suffix}`;
}

function readSplit(name: SplitName, mode: SplitMode): Record<string, number> | undefined {
  const panes = PANES[name];
  try {
    const raw = localStorage.getItem(storageKey(name, mode));
    if (!raw) {
      return undefined;
    }
    const parsed: unknown = JSON.parse(raw);
    if (typeof parsed !== "object" || parsed === null) {
      return undefined;
    }
    const record = parsed as Record<string, unknown>;
    const start = record[panes.start];
    const end = record[panes.end];
    if (!isPercent(start) || !isPercent(end)) {
      return undefined;
    }
    return { [panes.start]: start, [panes.end]: end };
  } catch {
    return undefined;
  }
}

function writeSplit(name: SplitName, mode: SplitMode, layout: Record<string, number>): void {
  try {
    localStorage.setItem(storageKey(name, mode), JSON.stringify(layout));
  } catch {
    // Private mode and quota failures leave the in-memory ratio in place.
  }
}

function isPercent(value: unknown): value is number {
  return typeof value === "number" && Number.isFinite(value) && value > 0 && value < 100;
}
