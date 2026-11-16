// The plan requires a CSS grid with grid/row/gridcell roles so rows can be
// virtualized. The grid is the single tab stop; arrows move the selection.
// biome-ignore-all lint/a11y/useSemanticElements: virtualized CSS grid, not a table
// biome-ignore-all lint/a11y/useFocusableInteractive: one tab stop on the grid
// biome-ignore-all lint/a11y/useKeyWithClickEvents: arrows on the grid move the selection

import { useVirtualizer } from "@tanstack/react-virtual";
import { cn } from "cn";
import { ChevronDown, ChevronRight } from "lucide-react";
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
import type { Schema } from "@/api/types";
import { KindIcon } from "@/components/kind-icon";
import { StatusDot } from "@/components/status-dot";
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
  subtreeEnds,
  visibleRows,
} from "@/lib/tree";
import { STORAGE_PREFIX } from "@/product";

const ROW_HEIGHT = 28;
const LIVE_ROW_CSS = `
@keyframes live-row {
  from { box-shadow: inset 0 0 0 999px color-mix(in oklch, var(--primary) 28%, transparent); }
  to { box-shadow: inset 0 0 0 999px transparent; }
}
.live-row { animation: live-row 1.5s ease-out; }
@media (prefers-reduced-motion: reduce) {
  .live-row { animation: none; }
}
`;
const OVERSCAN = 12;
// Duration, Tokens, Cost and Timeline; the Name column is sized apart (auto or dragged).
const OTHER_COLUMNS = "72px 64px 72px minmax(160px, 1fr)";
const OTHER_COLUMNS_MIN_PX = 72 + 64 + 72 + 160;
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
  highlightedSpanIds,
}: {
  spans: readonly Schema<"TreeSpan">[];
  trace: Schema<"TraceSummary">;
  selectedSpanId?: string;
  onSelectSpan: (spanId: string) => void;
  mode: TraceMode;
  onMode: (mode: TraceMode) => void;
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

  useEffect(() => {
    const index = spans.findIndex((row) => row.span.span_id === selectedSpanId);
    if (index < 0) {
      return;
    }
    setCollapsed((current) => expandAncestors(spans, current, index));
  }, [selectedSpanId, spans]);

  useLayoutEffect(() => {
    const node = timelineRef.current;
    if (!node) {
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
  }, []);

  const ends = useMemo(() => subtreeEnds(spans), [spans]);
  const timeWindow = useMemo(() => traceWindow(trace, spans), [trace, spans]);
  const zoom = view ?? timeWindow;
  const marks = useMemo(() => ticks(zoom, timelineWidth), [zoom, timelineWidth]);
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

  // The Name column fits the longest visible name until the user drags it.
  const [userNameWidth, setUserNameWidth] = useState<number | null>(readNameWidth);
  const autoWidth = useMemo(() => autoNameWidth(spans, rows), [spans, rows]);
  const nameWidth = userNameWidth ?? autoWidth;
  const template = `${nameWidth}px ${OTHER_COLUMNS}`;

  function clampName(width: number): number {
    const gridWidth = gridRef.current?.clientWidth;
    const room = gridWidth ? gridWidth - OTHER_COLUMNS_MIN_PX : Number.POSITIVE_INFINITY;
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

  const virtualizer = useVirtualizer({
    count: rows.length,
    getScrollElement: () => scrollRef.current,
    estimateSize: () => ROW_HEIGHT,
    overscan: OVERSCAN,
  });

  useEffect(() => {
    const visibleIndex = rows.indexOf(selectedIndex);
    if (visibleIndex >= 0) {
      virtualizer.scrollToIndex(visibleIndex, { align: "auto" });
    }
  }, [rows, selectedIndex, virtualizer]);

  useEffect(() => {
    const node = gridRef.current;
    if (!node) {
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
  }, [timeWindow]);

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
    if (matchesBinding(event, "fit")) {
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
      selectAt(rows[Math.min(rows.length - 1, visiblePos + 1)] ?? 0);
    } else if (matchesBinding(event, "row-prev")) {
      event.preventDefault();
      selectAt(rows[Math.max(0, visiblePos - 1)] ?? 0);
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
          onFit={() => setView(null)}
          zoomLabel={zoomLabel}
        />
        <div
          ref={gridRef}
          role="grid"
          aria-label="Trace"
          aria-rowcount={rows.length + 1}
          aria-colcount={5}
          tabIndex={0}
          className="relative flex min-h-0 flex-1 flex-col outline-none focus-visible:outline-2 focus-visible:outline-ring focus-visible:-outline-offset-2 focus-visible:outline-solid"
          onKeyDown={onKeyDown}
          onPointerMove={onPointerMove}
          onPointerLeave={() => setNeedle(null)}
        >
          <style>{LIVE_ROW_CSS}</style>
          {needle ? (
            <div
              className="pointer-events-none absolute top-0 bottom-0 z-10 w-px bg-foreground/70"
              style={{ left: needle.x }}
            >
              <span className="absolute top-0.5 left-1 text-[10px] whitespace-nowrap text-foreground normal-case">
                {needle.label}
              </span>
            </div>
          ) : null}
          <div
            role="row"
            aria-rowindex={1}
            className="grid h-row shrink-0 items-center border-b bg-background text-[11px] font-medium text-muted-foreground uppercase"
            style={{ gridTemplateColumns: template }}
          >
            <div role="columnheader" className="relative min-w-0 truncate px-2">
              Name
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
            </div>
            <div role="columnheader" className="px-2 text-right">
              Duration
            </div>
            <div role="columnheader" className="px-2 text-right">
              Tokens
            </div>
            <div role="columnheader" className="px-2 text-right">
              Cost
            </div>
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
              {marks.map((mark) => (
                <span
                  key={mark.ns}
                  aria-hidden="true"
                  className="absolute top-1/2 -translate-y-1/2 text-[10px] normal-case"
                  style={{
                    left: `${((mark.ns - zoom.startNs) / (zoom.endNs - zoom.startNs)) * 100}%`,
                  }}
                >
                  {mark.label}
                </span>
              ))}
            </div>
          </div>
          <div
            ref={scrollRef}
            data-scroll-container
            className="relative min-h-0 flex-1 overflow-auto"
          >
            <div className="relative" style={{ height: virtualizer.getTotalSize() }}>
              {virtualRows.map((item) => {
                const spanIndex = rows[item.index];
                const row = spanIndex === undefined ? undefined : spans[spanIndex];
                if (spanIndex === undefined || !row) {
                  return null;
                }
                const span = row.span;
                const selected = span.span_id === selectedSpanId;
                const expandable = hasChildren(ends, spanIndex);
                const isCollapsed = displayCollapsed.has(span.span_id);
                const bar = barGeometry(span, zoom);
                const showTokens = span.span_kind === "llm" || span.span_kind === "embedding";
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
                      "absolute top-0 grid w-full items-center border-b",
                      selected
                        ? "bg-selection shadow-[inset_2px_0_0_var(--primary)]"
                        : "hover:bg-muted",
                      dim && "opacity-40",
                      isMatch && !selected && "bg-primary/10",
                      highlightedSpanIds?.has(span.span_id) && "live-row",
                    )}
                    style={{
                      height: ROW_HEIGHT,
                      transform: `translateY(${item.start}px)`,
                      gridTemplateColumns: template,
                    }}
                    onClick={() => {
                      gridRef.current?.focus();
                      onSelectSpan(span.span_id);
                    }}
                  >
                    <div role="gridcell" className="min-w-0">
                      <div
                        className="flex min-w-0 items-center gap-1 pr-2"
                        style={{ paddingLeft: row.depth * 12 + 4 }}
                      >
                        {expandable ? (
                          <button
                            type="button"
                            tabIndex={-1}
                            aria-expanded={!isCollapsed}
                            aria-label={isCollapsed ? "Expand" : "Collapse"}
                            className="inline-flex size-4 shrink-0 items-center justify-center"
                            onClick={(event) => {
                              event.stopPropagation();
                              toggle(span.span_id);
                            }}
                          >
                            {isCollapsed ? (
                              <ChevronRight className="size-3" />
                            ) : (
                              <ChevronDown className="size-3" />
                            )}
                          </button>
                        ) : (
                          <span className="inline-block size-4 shrink-0" />
                        )}
                        <KindIcon kind={span.span_kind} />
                        <span className="min-w-0 truncate">{highlightName(span.name, query)}</span>
                        {span.status === "error" ? <StatusDot status="error" /> : null}
                        {row.detached ? (
                          <Badge variant="secondary" className="h-4 px-1.5 text-[10px]">
                            detached
                          </Badge>
                        ) : null}
                      </div>
                    </div>
                    <div role="gridcell" className="num truncate px-2 text-xs">
                      {formatDuration(span.duration_ns)}
                    </div>
                    <div role="gridcell" className="num truncate px-2 text-xs">
                      {showTokens ? formatTokens(span.total_tokens) : ""}
                    </div>
                    <div role="gridcell" className="num truncate px-2 text-xs">
                      {span.cost_total === undefined ? "" : formatCost(span.cost_total)}
                    </div>
                    <div role="gridcell" className="relative h-full">
                      {bar ? (
                        <SpanBar
                          span={span}
                          bar={bar}
                          colorClass={BAR[span.span_kind]}
                          timelineWidth={timelineWidth}
                          traceStartNs={timeWindow.startNs}
                          onDoubleClick={() => {
                            setView(zoomToSpan(timeWindow, span));
                          }}
                        />
                      ) : null}
                    </div>
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
