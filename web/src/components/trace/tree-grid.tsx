// The plan requires a CSS grid with grid/row/gridcell roles so rows can be
// virtualized. The grid is the single tab stop; arrows move the selection.
// biome-ignore-all lint/a11y/useSemanticElements: virtualized CSS grid, not a table
// biome-ignore-all lint/a11y/useFocusableInteractive: one tab stop on the grid
// biome-ignore-all lint/a11y/useKeyWithClickEvents: arrows on the grid move the selection

import { useQueryClient } from "@tanstack/react-query";
import { useVirtualizer } from "@tanstack/react-virtual";
import { cn } from "cn";
import {
  type KeyboardEvent,
  type ReactNode,
  type PointerEvent as ReactPointerEvent,
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
} from "react";
import { spanDetailQuery } from "@/api/queries";
import type { Schema } from "@/api/types";
import { KindIcon } from "@/components/kind-icon";
import { useProject } from "@/components/shell/project-context";
import { StatusDot } from "@/components/status-dot";
import { useRowHeight } from "@/components/theme-provider";
import { SpanBar } from "@/components/trace/span-bar";
import { TreeToolbar } from "@/components/trace/tree-toolbar";
import { Badge } from "@/components/ui/badge";
import { TooltipProvider } from "@/components/ui/tooltip";
import { formatCost, formatDuration, formatTokens } from "@/lib/format";
import type { TraceMode } from "@/lib/search/traces";
import { matchesBinding } from "@/lib/shortcuts";
import {
  barGeometry,
  clampView,
  panBy,
  selfTimes,
  slowSpans,
  type TimeWindow,
  ticks,
  traceWindow,
  zoomAt,
  zoomToSpan,
} from "@/lib/timeline";
import {
  ancestorsOf,
  autoNameWidth,
  defaultCollapsed,
  filterRows,
  hasChildren,
  metricColumnWidth,
  subtreeEnds,
  type TreeGuide,
  treeGuides,
  visibleRows,
} from "@/lib/tree";
import { STORAGE_PREFIX } from "@/product";

/** A row must rest under the pointer this long before its span detail is prefetched. */
const HOVER_PREFETCH_MS = 100;
const OVERSCAN = 12;
// Tree mode: Name, then Duration, Tokens and Cost, each sized to its widest visible value.
const NAME_FLEX_COLUMN = "minmax(160px, 1fr)";
// Timeline mode has no metric columns: the bars take the room beside the Name column,
// which is sized apart (auto or dragged).
const TIMELINE_MIN_PX = 160;
// Timeline rows are 4px shorter than the density's, so the bars sit close together.
const TIMELINE_ROW_INSET_PX = 4;
// Tree layout: a 4px margin, then 16px per depth. Each row starts with a 16px toggle slot
// holding an 11px −/+ box 2px from its left; the tree lines run from a box's edges, never
// through it. An odd box leaves a 9px inside, so the 1px strokes of − and + sit dead centre,
// and the 1px tree line at x 7 meets the box's centre (7.5).
const INDENT_BASE_PX = 4;
const INDENT_PX = 16;
const TOGGLE_SLOT_PX = 16;
const TOGGLE_BOX_PX = 11;
const TOGGLE_BOX_LEFT_PX = 2;
// The kind icon starts after the slot and the row's 4px gap.
const ICON_OFFSET_PX = TOGGLE_SLOT_PX + 4;
// A tick label past this share of the axis sits left of its gridline, so it isn't cut.
const TICK_FLIP_PCT = 85;
const NAME_MIN_PX = 140;
const NAME_MAX_PX = 640;
const NAME_KEY_STEP_PX = 16;
const NAME_WIDTH_KEY = `${STORAGE_PREFIX}tree.name_width`;

function readNameWidth(): number | null {
  try {
    const value = Number(localStorage.getItem(NAME_WIDTH_KEY));
    return Number.isFinite(value) && value >= NAME_MIN_PX && value <= NAME_MAX_PX ? value : null;
  } catch {
    return null;
  }
}

function writeNameWidth(width: number | null): void {
  try {
    if (width === null) {
      localStorage.removeItem(NAME_WIDTH_KEY);
    } else {
      localStorage.setItem(NAME_WIDTH_KEY, String(Math.round(width)));
    }
  } catch {
    // Private mode and quota failures leave the in-memory width in place.
  }
}
const ZOOM_WHEEL = 1.2;
const PAN_WHEEL_PCT = 12;

const BAR: Record<Schema<"SpanKind">, string> = {
  llm: "bg-kind-llm",
  embedding: "bg-kind-embedding",
  tool: "bg-kind-tool",
  agent: "bg-kind-agent",
  chain: "bg-kind-chain",
  retriever: "bg-kind-retriever",
  reranker: "bg-kind-reranker",
  guardrail: "bg-kind-guardrail",
  evaluator: "bg-kind-evaluator",
  span: "bg-kind-span",
};

export function TreeGrid({
  spans,
  trace,
  selectedSpanId,
  onSelectSpan,
  mode,
  onMode,
  timeline,
  highlightedSpanIds,
}: {
  spans: readonly Schema<"TreeSpan">[];
  trace: Schema<"TraceSummary">;
  selectedSpanId?: string;
  onSelectSpan: (spanId: string) => void;
  mode: TraceMode;
  onMode: (mode: TraceMode) => void;
  /** The time-axis column, its zoom and drag-zoom. Off in Tree mode. */
  timeline: boolean;
  highlightedSpanIds?: ReadonlySet<string>;
}) {
  const scrollRef = useRef<HTMLDivElement>(null);
  const gridRef = useRef<HTMLDivElement>(null);
  const timelineRef = useRef<HTMLDivElement>(null);
  const [timelineWidth, setTimelineWidth] = useState(480);
  const [collapsed, setCollapsed] = useState(() => collapsedWithPath(spans, selectedSpanId));
  const [query, setQuery] = useState("");
  const [errorsOnly, setErrorsOnly] = useState(false);
  const [view, setView] = useState<TimeWindow | null>(null);
  const [needle, setNeedle] = useState<{ x: number; label: string } | null>(null);
  const [drag, setDrag] = useState<{ startPct: number; endPct: number } | null>(null);
  const dragRef = useRef<{ startPct: number; endPct: number } | null>(null);
  const { org, project } = useProject();
  const queryClient = useQueryClient();
  const hoverTimer = useRef<number | undefined>(undefined);

  useEffect(() => () => window.clearTimeout(hoverTimer.current), []);

  // Live-highlighted rows are skipped: their detail is still changing as spans arrive.
  function prefetchSpan(spanId: string) {
    if (highlightedSpanIds?.has(spanId)) {
      return;
    }
    void queryClient.prefetchQuery(spanDetailQuery(org, project, trace.trace_id, spanId));
  }

  function prefetchNeighbors(visiblePos: number) {
    for (const pos of [visiblePos - 1, visiblePos + 1]) {
      const index = rows[pos];
      const id = index === undefined ? undefined : spans[index]?.span.span_id;
      if (id !== undefined) {
        prefetchSpan(id);
      }
    }
  }

  useEffect(() => {
    const index = spans.findIndex((row) => row.span.span_id === selectedSpanId);
    if (index < 0) {
      return;
    }
    setCollapsed((current) => expandAncestors(spans, current, index));
  }, [selectedSpanId, spans]);

  useLayoutEffect(() => {
    const node = timelineRef.current;
    if (!timeline || !node) {
      return;
    }
    const update = () => {
      if (node.clientWidth > 0) {
        setTimelineWidth(node.clientWidth);
      }
    };
    update();
    const observer = new ResizeObserver(update);
    observer.observe(node);
    return () => observer.disconnect();
  }, [timeline]);

  const ends = useMemo(() => subtreeEnds(spans), [spans]);
  const timeWindow = useMemo(() => traceWindow(trace, spans), [trace, spans]);
  const zoom = view ?? timeWindow;
  const marks = useMemo(
    () => ticks(zoom, timeWindow.startNs, timelineWidth),
    [zoom, timeWindow.startNs, timelineWidth],
  );
  const self = useMemo(() => selfTimes(spans), [spans]);
  const slowIds = useMemo(
    () => slowSpans(spans, self, timeWindow.endNs - timeWindow.startNs),
    [spans, self, timeWindow],
  );
  const selectedIndex = spans.findIndex((row) => row.span.span_id === selectedSpanId);
  const needleQuery = query.trim().toLowerCase();

  const errorRows = useMemo(
    () =>
      errorsOnly ? filterRows(spans, ends, (index) => spans[index]?.span.status === "error") : null,
    [errorsOnly, spans, ends],
  );
  const errorSet = useMemo(() => (errorRows ? new Set(errorRows) : null), [errorRows]);

  const matchIndexes = useMemo(() => {
    if (!needleQuery) {
      return [];
    }
    const found: number[] = [];
    for (let index = 0; index < spans.length; index++) {
      if (errorSet && !errorSet.has(index)) {
        continue;
      }
      const span = spans[index]?.span;
      if (span && spanMatches(span, needleQuery)) {
        found.push(index);
      }
    }
    return found;
  }, [spans, needleQuery, errorSet]);
  const matchSet = useMemo(() => new Set(matchIndexes), [matchIndexes]);

  const displayCollapsed = useMemo(() => {
    const targets = errorRows ? [...errorRows, ...matchIndexes] : matchIndexes;
    let next = collapsed;
    for (const index of targets) {
      next = expandAncestors(spans, next, index);
    }
    return next;
  }, [collapsed, errorRows, matchIndexes, spans]);

  const rows = useMemo(() => {
    const visible = visibleRows(spans, ends, displayCollapsed);
    return errorSet ? visible.filter((index) => errorSet.has(index)) : visible;
  }, [spans, ends, displayCollapsed, errorSet]);
  const guides = useMemo(
    () => treeGuides(rows.map((index) => spans[index]?.depth ?? 0)),
    [rows, spans],
  );

  // The Name column fits the longest visible name until the user drags it.
  const [userNameWidth, setUserNameWidth] = useState<number | null>(readNameWidth);
  const autoWidth = useMemo(() => autoNameWidth(spans, rows), [spans, rows]);
  const nameWidth = userNameWidth ?? autoWidth;
  const metricWidths = useMemo(() => {
    const durations: string[] = [];
    const tokens: string[] = [];
    const costs: string[] = [];
    for (const index of rows) {
      const span = spans[index]?.span;
      if (!span) {
        continue;
      }
      durations.push(formatDuration(span.duration_ns));
      if (showsTokens(span.span_kind)) {
        tokens.push(formatTokens(span.total_tokens));
      }
      if (span.cost_total !== undefined) {
        costs.push(formatCost(span.cost_total));
      }
    }
    return [
      metricColumnWidth("Duration", durations),
      metricColumnWidth("Tokens", tokens),
      metricColumnWidth("Cost", costs),
    ];
  }, [spans, rows]);
  const template = timeline
    ? `${nameWidth}px minmax(${TIMELINE_MIN_PX}px, 1fr)`
    : `${NAME_FLEX_COLUMN} ${metricWidths.map((px) => `${px}px`).join(" ")}`;

  function clampName(width: number): number {
    const gridWidth = gridRef.current?.clientWidth;
    const room = gridWidth ? gridWidth - TIMELINE_MIN_PX : Number.POSITIVE_INFINITY;
    return Math.round(Math.max(NAME_MIN_PX, Math.min(NAME_MAX_PX, room, width)));
  }

  function onNameResizeDown(event: ReactPointerEvent<HTMLDivElement>) {
    event.preventDefault();
    event.stopPropagation();
    const handle = event.currentTarget;
    handle.setPointerCapture(event.pointerId);
    const startX = event.clientX;
    const startWidth = nameWidth;
    let latest = startWidth;
    const move = (moveEvent: PointerEvent) => {
      latest = clampName(startWidth + moveEvent.clientX - startX);
      setUserNameWidth(latest);
    };
    const stop = () => {
      handle.removeEventListener("pointermove", move);
      handle.removeEventListener("pointerup", stop);
      handle.removeEventListener("pointercancel", stop);
      writeNameWidth(latest);
    };
    handle.addEventListener("pointermove", move);
    handle.addEventListener("pointerup", stop);
    handle.addEventListener("pointercancel", stop);
  }

  function onNameResizeKey(event: KeyboardEvent<HTMLDivElement>) {
    if (event.key !== "ArrowLeft" && event.key !== "ArrowRight") {
      return;
    }
    event.preventDefault();
    event.stopPropagation();
    const next = clampName(nameWidth + (event.key === "ArrowRight" ? 1 : -1) * NAME_KEY_STEP_PX);
    setUserNameWidth(next);
    writeNameWidth(next);
  }

  // The header row is as tall as the density's row, and sticks at the top of the scroll area,
  // so the rows start below it and a row scrolled into view isn't left under it.
  const headerHeight = useRowHeight();
  const rowHeight = headerHeight - (timeline ? TIMELINE_ROW_INSET_PX : 0);
  const virtualizer = useVirtualizer({
    count: rows.length,
    getScrollElement: () => scrollRef.current,
    estimateSize: () => rowHeight,
    overscan: OVERSCAN,
    scrollMargin: headerHeight,
    scrollPaddingStart: headerHeight,
  });

  // A density change alters every row's height, so the measurements must be redone.
  // biome-ignore lint/correctness/useExhaustiveDependencies: rowHeight is the trigger; estimateSize reads it
  useEffect(() => {
    virtualizer.measure();
  }, [rowHeight, virtualizer]);

  useEffect(() => {
    const visibleIndex = rows.indexOf(selectedIndex);
    if (visibleIndex >= 0) {
      virtualizer.scrollToIndex(visibleIndex, { align: "auto" });
    }
  }, [rows, selectedIndex, virtualizer]);

  useEffect(() => {
    const node = gridRef.current;
    if (!node || !timeline) {
      return;
    }
    function onWheel(event: WheelEvent) {
      const pct = timelinePercent(timelineRef.current, event.clientX);
      if (pct === undefined) {
        return;
      }
      if (event.ctrlKey || event.metaKey) {
        event.preventDefault();
        const factor = event.deltaY < 0 ? ZOOM_WHEEL : 1 / ZOOM_WHEEL;
        setView((current) => zoomAt(timeWindow, current ?? timeWindow, pct, factor));
      } else if (event.shiftKey) {
        event.preventDefault();
        const delta = event.deltaX === 0 ? event.deltaY : event.deltaX;
        const sign = delta > 0 ? 1 : -1;
        setView((current) => panBy(timeWindow, current ?? timeWindow, sign * PAN_WHEEL_PCT));
      }
    }
    node.addEventListener("wheel", onWheel, { passive: false });
    return () => node.removeEventListener("wheel", onWheel);
  }, [timeWindow, timeline]);

  function toggle(spanId: string) {
    setCollapsed((current) => {
      const next = new Set(current);
      if (next.has(spanId)) {
        next.delete(spanId);
      } else {
        next.add(spanId);
      }
      return next;
    });
  }

  function selectAt(spanIndex: number) {
    const row = spans[spanIndex];
    if (!row) {
      return;
    }
    onSelectSpan(row.span.span_id);
  }

  function goMatch(delta: number) {
    if (matchIndexes.length === 0) {
      return;
    }
    const current = matchIndexes.indexOf(selectedIndex);
    const next =
      current < 0
        ? delta > 0
          ? 0
          : matchIndexes.length - 1
        : (current + delta + matchIndexes.length) % matchIndexes.length;
    const index = matchIndexes[next];
    if (index !== undefined) {
      selectAt(index);
    }
  }

  function onKeyDown(event: KeyboardEvent<HTMLDivElement>) {
    if (timeline && matchesBinding(event, "fit")) {
      event.preventDefault();
      setView(null);
      return;
    }
    if (rows.length === 0) {
      return;
    }
    const visiblePos = Math.max(0, rows.indexOf(selectedIndex));
    if (matchesBinding(event, "row-next")) {
      event.preventDefault();
      const next = Math.min(rows.length - 1, visiblePos + 1);
      selectAt(rows[next] ?? 0);
      prefetchNeighbors(next);
    } else if (matchesBinding(event, "row-prev")) {
      event.preventDefault();
      const prev = Math.max(0, visiblePos - 1);
      selectAt(rows[prev] ?? 0);
      prefetchNeighbors(prev);
    } else if (event.key === "ArrowRight") {
      event.preventDefault();
      const id = spans[selectedIndex]?.span.span_id;
      if (id && hasChildren(ends, selectedIndex) && displayCollapsed.has(id)) {
        toggle(id);
      }
    } else if (event.key === "ArrowLeft") {
      event.preventDefault();
      const id = spans[selectedIndex]?.span.span_id;
      if (id && hasChildren(ends, selectedIndex) && !displayCollapsed.has(id)) {
        toggle(id);
        return;
      }
      const parent = ancestorsOf(spans, selectedIndex).at(-1);
      if (parent !== undefined) {
        selectAt(parent);
      }
    } else if (event.key === "Enter") {
      event.preventDefault();
      const id = spans[selectedIndex]?.span.span_id;
      if (id && hasChildren(ends, selectedIndex)) {
        toggle(id);
      }
    }
  }

  function onPointerMove(event: ReactPointerEvent<HTMLDivElement>) {
    if (!timeline) {
      return;
    }
    const pct = timelinePercent(timelineRef.current, event.clientX);
    const host = gridRef.current?.getBoundingClientRect();
    if (pct === undefined || !host) {
      setNeedle(null);
      return;
    }
    const offset = zoom.startNs + (pct / 100) * (zoom.endNs - zoom.startNs) - timeWindow.startNs;
    setNeedle({ x: event.clientX - host.left, label: formatDuration(offset) });
  }

  function onHeaderDown(event: ReactPointerEvent<HTMLDivElement>) {
    if (event.button !== 0) {
      return;
    }
    const pct = timelinePercent(timelineRef.current, event.clientX);
    if (pct === undefined) {
      return;
    }
    event.preventDefault();
    const next = { startPct: pct, endPct: pct };
    dragRef.current = next;
    setDrag(next);
    try {
      event.currentTarget.setPointerCapture(event.pointerId);
    } catch {
      // jsdom has no pointer capture.
    }
  }

  function onHeaderMove(event: ReactPointerEvent<HTMLDivElement>) {
    const current = dragRef.current;
    if (!current) {
      return;
    }
    const pct = timelinePercent(timelineRef.current, event.clientX);
    if (pct === undefined) {
      return;
    }
    const next = { startPct: current.startPct, endPct: pct };
    dragRef.current = next;
    setDrag(next);
  }

  function onHeaderUp(event: ReactPointerEvent<HTMLDivElement>) {
    const current = dragRef.current;
    if (!current) {
      return;
    }
    const pct = timelinePercent(timelineRef.current, event.clientX) ?? current.endPct;
    const startPct = Math.min(current.startPct, pct);
    const endPct = Math.max(current.startPct, pct);
    dragRef.current = null;
    setDrag(null);
    if (endPct - startPct < 1) {
      return;
    }
    const width = zoom.endNs - zoom.startNs;
    setView(
      clampView(timeWindow, {
        startNs: zoom.startNs + (startPct / 100) * width,
        endNs: zoom.startNs + (endPct / 100) * width,
      }),
    );
  }

  const virtualRows = virtualizer.getVirtualItems();
  const matchPos = matchIndexes.indexOf(selectedIndex);
  const zoomLabel = `${formatDuration(zoom.startNs - timeWindow.startNs)}–${formatDuration(zoom.endNs - timeWindow.startNs)}`;
  const shownNeedle = timeline ? needle : null;

  return (
    <TooltipProvider>
      <div className="flex min-h-0 flex-1 flex-col">
        <TreeToolbar
          mode={mode}
          onMode={onMode}
          query={query}
          onQuery={setQuery}
          matchLabel={`${matchPos >= 0 ? matchPos + 1 : 0} / ${matchIndexes.length}`}
          onPrevious={() => goMatch(-1)}
          onNext={() => goMatch(1)}
          errorsOnly={errorsOnly}
          onErrorsOnly={setErrorsOnly}
          onExpandAll={() => setCollapsed(new Set())}
          onCollapseDepth={() => setCollapsed(collapsedToDepth(spans, ends))}
          onFit={timeline ? () => setView(null) : undefined}
          zoomLabel={timeline ? zoomLabel : undefined}
        />
        <div
          ref={gridRef}
          role="grid"
          aria-label="Trace"
          aria-rowcount={rows.length + 1}
          aria-colcount={timeline ? 2 : 4}
          tabIndex={0}
          className="relative flex min-h-0 flex-1 flex-col outline-none focus-visible:outline-2 focus-visible:outline-ring focus-visible:-outline-offset-2 focus-visible:outline-solid"
          onKeyDown={onKeyDown}
          onPointerMove={onPointerMove}
          onPointerLeave={() => setNeedle(null)}
        >
          {shownNeedle ? (
            <div
              className="pointer-events-none absolute top-0 bottom-0 z-10 w-px bg-foreground/70"
              style={{ left: shownNeedle.x }}
            >
              <span className="absolute top-0.5 left-1 text-caption whitespace-nowrap text-foreground normal-case">
                {shownNeedle.label}
              </span>
            </div>
          ) : null}
          <div
            ref={scrollRef}
            data-scroll-container
            className="relative min-h-0 flex-1 overflow-auto"
          >
            <div
              role="row"
              aria-rowindex={1}
              className="sticky top-0 z-20 grid h-row items-center border-b bg-background text-caption font-medium text-muted-foreground uppercase"
              style={{ gridTemplateColumns: template }}
            >
              <div role="columnheader" className="relative min-w-0 truncate px-2">
                Name
                {timeline ? (
                  <div
                    role="separator"
                    aria-orientation="vertical"
                    aria-label="Resize Name column"
                    aria-valuemin={NAME_MIN_PX}
                    aria-valuemax={NAME_MAX_PX}
                    aria-valuenow={Math.round(nameWidth)}
                    tabIndex={0}
                    title="Drag to resize, double-click to reset"
                    onPointerDown={onNameResizeDown}
                    onKeyDown={onNameResizeKey}
                    onClick={(event) => {
                      event.stopPropagation();
                    }}
                    onDoubleClick={() => {
                      setUserNameWidth(null);
                      writeNameWidth(null);
                    }}
                    className="absolute inset-y-0 -right-1 z-20 w-2 cursor-col-resize touch-none hover:bg-ring/40 focus-visible:bg-ring/60 focus-visible:outline-hidden"
                  />
                ) : null}
              </div>
              {timeline ? null : (
                <>
                  <div role="columnheader" className="px-2 text-right">
                    Duration
                  </div>
                  <div role="columnheader" className="px-2 text-right">
                    Tokens
                  </div>
                  <div role="columnheader" className="px-2 text-right">
                    Cost
                  </div>
                </>
              )}
              {timeline ? (
                <div
                  role="columnheader"
                  aria-label="Timeline"
                  className="relative h-full"
                  ref={timelineRef}
                  onPointerDown={onHeaderDown}
                  onPointerMove={onHeaderMove}
                  onPointerUp={onHeaderUp}
                >
                  <span className="sr-only">Timeline</span>
                  {drag ? (
                    <span
                      className="absolute top-0 h-full bg-primary/20"
                      style={{
                        left: `${Math.min(drag.startPct, drag.endPct)}%`,
                        width: `${Math.abs(drag.endPct - drag.startPct)}%`,
                      }}
                    />
                  ) : null}
                  {marks.map((mark) => {
                    const pct = markPct(mark.ns, zoom);
                    return (
                      <span
                        key={mark.ns}
                        aria-hidden="true"
                        data-tick
                        className={cn(
                          "absolute inset-y-0 flex items-center border-border text-caption whitespace-nowrap normal-case tabular-nums",
                          pct > TICK_FLIP_PCT ? "-translate-x-full border-r pr-1" : "border-l pl-1",
                        )}
                        style={{ left: `${pct}%` }}
                      >
                        {mark.label}
                      </span>
                    );
                  })}
                </div>
              ) : null}
            </div>

            <div className="relative" style={{ height: virtualizer.getTotalSize() }}>
              {virtualRows.map((item) => {
                const spanIndex = rows[item.index];
                const row = spanIndex === undefined ? undefined : spans[spanIndex];
                if (spanIndex === undefined || !row) {
                  return null;
                }
                const span = row.span;
                const guide = guides[item.index];
                const slow = slowIds.has(span.span_id);
                const selected = span.span_id === selectedSpanId;
                const expandable = hasChildren(ends, spanIndex);
                const isCollapsed = displayCollapsed.has(span.span_id);
                const bar = timeline ? barGeometry(span, zoom) : undefined;
                const showTokens = showsTokens(span.span_kind);
                const isMatch = matchSet.has(spanIndex);
                const dim = needleQuery.length > 0 && !isMatch;
                return (
                  <div
                    key={span.span_id}
                    role="row"
                    aria-rowindex={item.index + 2}
                    aria-selected={selected}
                    data-match={isMatch ? "true" : undefined}
                    className={cn(
                      "absolute top-0 grid w-full items-center",
                      // Timeline rows have gridlines instead of row borders.
                      !timeline && "border-b",
                      selected
                        ? "bg-selection shadow-[inset_2px_0_0_var(--primary)]"
                        : "hover:bg-muted",
                      dim && "opacity-40",
                      isMatch && !selected && "bg-primary/10",
                      highlightedSpanIds?.has(span.span_id) && "live-row",
                    )}
                    style={{
                      height: rowHeight,
                      transform: `translateY(${item.start - headerHeight}px)`,
                      gridTemplateColumns: template,
                    }}
                    onClick={() => {
                      gridRef.current?.focus();
                      onSelectSpan(span.span_id);
                    }}
                    onPointerEnter={() => {
                      window.clearTimeout(hoverTimer.current);
                      hoverTimer.current = window.setTimeout(
                        () => prefetchSpan(span.span_id),
                        HOVER_PREFETCH_MS,
                      );
                    }}
                    onPointerLeave={() => {
                      window.clearTimeout(hoverTimer.current);
                    }}
                  >
                    <div role="gridcell" className="relative h-full min-w-0">
                      {guide ? (
                        <TreeGuideLines guide={guide} depth={row.depth} expandable={expandable} />
                      ) : null}
                      <div
                        className="flex h-full min-w-0 items-center gap-[4px] pr-2"
                        style={{ paddingLeft: INDENT_BASE_PX + row.depth * INDENT_PX }}
                      >
                        {expandable ? (
                          <button
                            type="button"
                            tabIndex={-1}
                            aria-expanded={!isCollapsed}
                            aria-label={isCollapsed ? "Expand" : "Collapse"}
                            className="group inline-flex size-[16px] shrink-0 items-center"
                            onClick={(event) => {
                              event.stopPropagation();
                              toggle(span.span_id);
                            }}
                          >
                            <span
                              className="relative shrink-0 rounded-[2px] border border-muted-foreground/60 text-muted-foreground group-hover:border-foreground group-hover:text-foreground"
                              style={{
                                width: TOGGLE_BOX_PX,
                                height: TOGGLE_BOX_PX,
                                marginLeft: TOGGLE_BOX_LEFT_PX,
                              }}
                            >
                              <span className="absolute top-[4px] left-[2px] h-px w-[5px] bg-current" />
                              {isCollapsed ? (
                                <span className="absolute top-[2px] left-[4px] h-[5px] w-px bg-current" />
                              ) : null}
                            </span>
                          </button>
                        ) : (
                          <span className="inline-block size-[16px] shrink-0" />
                        )}
                        <KindIcon kind={span.span_kind} />
                        <span className="min-w-0 truncate">{highlightName(span.name, query)}</span>
                        {span.status === "error" ? <StatusDot status="error" /> : null}
                        {row.detached ? (
                          <Badge variant="secondary" className="h-4 px-1.5 text-caption">
                            detached
                          </Badge>
                        ) : null}
                      </div>
                    </div>
                    {timeline ? null : (
                      <>
                        <div
                          role="gridcell"
                          data-slow={slow ? "true" : undefined}
                          className={cn(
                            "num truncate px-2 text-caption",
                            slow && "font-medium text-status-warn",
                          )}
                        >
                          {formatDuration(span.duration_ns)}
                        </div>
                        <div role="gridcell" className="num truncate px-2 text-caption">
                          {showTokens ? formatTokens(span.total_tokens) : ""}
                        </div>
                        <div role="gridcell" className="num truncate px-2 text-caption">
                          {span.cost_total === undefined ? "" : formatCost(span.cost_total)}
                        </div>
                      </>
                    )}
                    {timeline ? (
                      <div role="gridcell" className="relative h-full">
                        {marks.map((mark) => (
                          <span
                            key={mark.ns}
                            aria-hidden="true"
                            data-gridline
                            className="pointer-events-none absolute inset-y-0 w-px bg-border"
                            style={{ left: `${markPct(mark.ns, zoom)}%` }}
                          />
                        ))}
                        {bar ? (
                          <SpanBar
                            span={span}
                            bar={bar}
                            colorClass={BAR[span.span_kind]}
                            timelineWidth={timelineWidth}
                            traceStartNs={timeWindow.startNs}
                            selfNs={self[spanIndex]}
                            onDoubleClick={() => {
                              setView(zoomToSpan(timeWindow, span));
                            }}
                          />
                        ) : null}
                      </div>
                    ) : null}
                  </div>
                );
              })}
            </div>
          </div>
        </div>
      </div>
    </TooltipProvider>
  );
}

/** Only model spans have tokens worth a cell. */
function showsTokens(kind: Schema<"SpanKind">): boolean {
  return kind === "llm" || kind === "embedding";
}

function markPct(ns: number, zoom: TimeWindow): number {
  return ((ns - zoom.startNs) / (zoom.endNs - zoom.startNs)) * 100;
}

/**
 * The elbow lines in a row's indent: each level's line runs down from its parent's −/+ box.
 * ├ and a tick for a child with later siblings, a rounded └ for the last child, a full line
 * for each ancestor level that continues, and a line down from an expanded parent's box.
 */
function TreeGuideLines({
  guide,
  depth,
  expandable,
}: {
  guide: TreeGuide;
  depth: number;
  /** The row has a −/+ box, collapsed or not. */
  expandable: boolean;
}) {
  const slotX = (level: number) => INDENT_BASE_PX + level * INDENT_PX;
  const lineX = (level: number) =>
    slotX(level - 1) + TOGGLE_BOX_LEFT_PX + Math.floor(TOGGLE_BOX_PX / 2);
  const elbowX = lineX(depth);
  // The tick ends at this row's box, or, for a leaf, runs through the empty slot to its icon.
  const tickEnd = expandable
    ? slotX(depth) + TOGGLE_BOX_LEFT_PX
    : slotX(depth) + ICON_OFFSET_PX - 2;
  return (
    <span aria-hidden="true" data-guide={guideShape(guide)} className="pointer-events-none">
      {guide.through.map((through, offset) =>
        through ? (
          <span
            // biome-ignore lint/suspicious/noArrayIndexKey: the index is the tree level
            key={offset}
            className="absolute inset-y-0 w-px bg-border"
            style={{ left: lineX(offset + 1) }}
          />
        ) : null,
      )}
      {guide.elbow === "mid" ? (
        <>
          <span className="absolute inset-y-0 w-px bg-border" style={{ left: elbowX }} />
          <span
            className="absolute top-1/2 h-px bg-border"
            style={{ left: elbowX, width: tickEnd - elbowX }}
          />
        </>
      ) : null}
      {guide.elbow === "last" ? (
        <span
          className="absolute top-0 h-[calc(50%+1px)] rounded-bl-[4px] border-b border-l border-border"
          style={{ left: elbowX, width: tickEnd - elbowX }}
        />
      ) : null}
      {guide.down ? (
        <span
          className="absolute bottom-0 w-px bg-border"
          style={{ left: lineX(depth + 1), height: `calc(50% - ${TOGGLE_BOX_PX / 2}px)` }}
        />
      ) : null}
    </span>
  );
}

/** A row's guides as text, e.g. "│ ├" or "  └┐", for tests and debugging. */
function guideShape(guide: TreeGuide): string {
  const levels = guide.through.map((through) => (through ? "│" : " ")).join(" ");
  const elbow = guide.elbow === "mid" ? "├" : guide.elbow === "last" ? "└" : "";
  return `${levels}${levels && elbow ? " " : ""}${elbow}${guide.down ? "┐" : ""}`;
}

function spanMatches(
  span: { name: string; span_id: string; model?: string },
  query: string,
): boolean {
  if (span.name.toLowerCase().includes(query)) {
    return true;
  }
  if (span.model?.toLowerCase().includes(query)) {
    return true;
  }
  return span.span_id.toLowerCase().startsWith(query);
}

function highlightName(name: string, query: string): ReactNode {
  const trimmed = query.trim();
  if (!trimmed) {
    return name;
  }
  const index = name.toLowerCase().indexOf(trimmed.toLowerCase());
  if (index < 0) {
    return name;
  }
  return (
    <>
      {name.slice(0, index)}
      <mark className="bg-primary/25 text-inherit">
        {name.slice(index, index + trimmed.length)}
      </mark>
      {name.slice(index + trimmed.length)}
    </>
  );
}

function timelinePercent(node: HTMLElement | null, clientX: number): number | undefined {
  if (!node) {
    return undefined;
  }
  const rect = node.getBoundingClientRect();
  if (rect.width <= 0 || clientX < rect.left || clientX > rect.right) {
    return undefined;
  }
  return ((clientX - rect.left) / rect.width) * 100;
}

function collapsedToDepth(spans: readonly Schema<"TreeSpan">[], ends: Int32Array): Set<string> {
  const collapsed = new Set<string>();
  for (let index = 0; index < spans.length; index++) {
    const row = spans[index];
    if (row && row.depth >= 1 && hasChildren(ends, index)) {
      collapsed.add(row.span.span_id);
    }
  }
  return collapsed;
}

function collapsedWithPath(
  spans: readonly Schema<"TreeSpan">[],
  spanId: string | undefined,
): Set<string> {
  const collapsed = defaultCollapsed(spans);
  const index = spans.findIndex((row) => row.span.span_id === spanId);
  if (index < 0) {
    return collapsed;
  }
  return expandAncestors(spans, collapsed, index);
}

function expandAncestors(
  spans: readonly Schema<"TreeSpan">[],
  collapsed: Set<string>,
  index: number,
): Set<string> {
  let changed = false;
  const next = new Set(collapsed);
  for (const ancestorIndex of ancestorsOf(spans, index)) {
    const ancestor = spans[ancestorIndex];
    if (ancestor && next.delete(ancestor.span.span_id)) {
      changed = true;
    }
  }
  return changed ? next : collapsed;
}
