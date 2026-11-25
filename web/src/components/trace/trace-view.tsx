import { keepPreviousData, useQuery, useQueryClient } from "@tanstack/react-query";
import { Link, useRouterState } from "@tanstack/react-router";
import { cn } from "cn";
import { ArrowLeft } from "lucide-react";
import { type ReactNode, useEffect, useRef, useState } from "react";
import { problemOf } from "@/api/problem";
import { projectRef } from "@/api/project-ref";
import { traceDetailQuery } from "@/api/queries";
import type { Schema } from "@/api/types";
import { AddToQueueButton } from "@/components/annotation-queues/add-to-queue-dialog";
import { useShortcut } from "@/components/command/use-shortcut";
import { AddToDatasetButton } from "@/components/datasets/add-to-dataset-dialog";
import { useProject } from "@/components/shell/project-context";
import { RunEvaluatorButton } from "@/components/trace/run-evaluator-dialog";
import { SpanDetail } from "@/components/trace/span-detail";
import { SplitPane } from "@/components/trace/split-pane";
import { TraceHeader } from "@/components/trace/trace-header";
import { TraceSummary } from "@/components/trace/trace-summary";
import { TranscriptView } from "@/components/trace/transcript-view";
import { TreeGrid } from "@/components/trace/tree-grid";
import { Button } from "@/components/ui/button";
import { Skeleton } from "@/components/ui/skeleton";
import { instantNs, nsBetween } from "@/lib/format";
import type { ListReturn } from "@/lib/list-return";
import type { TraceMode } from "@/lib/search/traces";
import { useStream } from "@/lib/sse";
import { insertSpan } from "@/lib/tree";
import { useDelayedFlag } from "@/lib/use-delayed-flag";
import { STORAGE_PREFIX } from "@/product";

const RECENT_MS = 10 * 60 * 1000;
const HIGHLIGHT_MS = 1_500;
const EMPTY_HIGHLIGHTS: ReadonlySet<string> = new Set();
/** The Transcript's Full width choice, a reading preference kept per browser. */
const WIDE_KEY = `${STORAGE_PREFIX}transcript-wide`;
const NEXT_MODE: Record<TraceMode, TraceMode> = {
  tree: "timeline",
  timeline: "transcript",
  transcript: "tree",
};

/** A trace that ended within the last 10 minutes still receives live spans. */
export function traceIsRecent(endTime: string, now: number): boolean {
  const end = Date.parse(endTime);
  return Number.isFinite(end) && now - end <= RECENT_MS;
}

export function TraceView({
  traceId,
  spanId,
  mode,
  onModeChange,
  onSelectSpan,
  onShowSummary,
  onAddFilter,
  onClose,
  onPrevious,
  onNext,
  layout,
  backLink,
  fullPageListReturn,
  stacked = false,
}: {
  traceId: string;
  spanId?: string;
  mode?: TraceMode;
  onModeChange?: (mode: TraceMode) => void;
  onSelectSpan: (spanId: string) => void;
  /** Goes back to the trace summary from a selected span in Transcript mode. The bar is hidden when missing. */
  onShowSummary?: () => void;
  onAddFilter?: (conjunct: string) => void;
  onClose?: () => void | Promise<void>;
  onPrevious?: () => void;
  onNext?: () => void;
  layout?: ReactNode;
  /** Shown before the trace name in the header. */
  backLink?: ReactNode;
  /** The list return the full-page link carries. See `TraceHeader`. */
  fullPageListReturn?: () => ListReturn | undefined;
  /** Tree on top and span detail below, for a narrow pane (the session page). */
  stacked?: boolean;
}) {
  const { org, project } = useProject();
  const detailOptions = traceDetailQuery(org, project, traceId);
  // The previous trace stays on screen, dimmed, while the next one loads, so switching does not flash.
  const trace = useQuery({ ...detailOptions, placeholderData: keepPreviousData });
  const previous = trace.isPlaceholderData;
  // The dim shows only for a slow switch. A fast or cached switch shows the new trace at once.
  const dimmed = useDelayedFlag(previous);
  // The trace whose switch was dimmed. Its arrival fades in; any other switch clears it.
  const [slowTraceId, setSlowTraceId] = useState<string | null>(null);
  const queryClient = useQueryClient();
  const listLive = useRouterState({
    select: (state) => {
      const search = state.location.search as { live?: unknown };
      return search?.live === 1 || search?.live === "1";
    },
  });
  const [storedMode, setStoredMode] = useState<TraceMode>("tree");
  const [stoppedId, setStoppedId] = useState<string | null>(null);
  const [wide, setWide] = useState(() => localStorage.getItem(WIDE_KEY) === "1");
  const [highlightState, setHighlightState] = useState<{
    traceId: string;
    ids: ReadonlySet<string>;
  }>({ traceId, ids: EMPTY_HIGHLIGHTS });
  const timers = useRef<number[]>([]);
  const stopped = stoppedId === traceId;
  const highlighted = highlightState.traceId === traceId ? highlightState.ids : EMPTY_HIGHLIGHTS;
  const active = onModeChange ? (mode ?? "tree") : storedMode;
  function chooseMode(next: TraceMode) {
    if (onModeChange) onModeChange(next);
    else setStoredMode(next);
  }
  function chooseWide(on: boolean) {
    setWide(on);
    if (on) localStorage.setItem(WIDE_KEY, "1");
    else localStorage.removeItem(WIDE_KEY);
  }

  useEffect(() => {
    setSlowTraceId((current) => (current === traceId ? current : null));
  }, [traceId]);

  useEffect(() => {
    if (dimmed) {
      setSlowTraceId(traceId);
    }
  }, [dimmed, traceId]);

  useEffect(() => {
    return () => {
      for (const timer of timers.current) {
        window.clearTimeout(timer);
      }
    };
  }, []);

  const detail = trace.data;
  const follow =
    detail !== undefined &&
    !previous &&
    !stopped &&
    (listLive || traceIsRecent(detail.trace.end_time, Date.now()));
  const queryKeyRef = useRef(detailOptions.queryKey);
  queryKeyRef.current = detailOptions.queryKey;

  function mark(spanIdToMark: string) {
    setHighlightState((current) => {
      const ids = current.traceId === traceId ? new Set(current.ids) : new Set<string>();
      ids.add(spanIdToMark);
      return { traceId, ids };
    });
    const timer = window.setTimeout(() => {
      setHighlightState((current) => {
        if (current.traceId !== traceId) {
          return current;
        }
        const ids = new Set(current.ids);
        ids.delete(spanIdToMark);
        return { traceId, ids };
      });
    }, HIGHLIGHT_MS);
    timers.current.push(timer);
  }

  const { status } = useStream(
    follow ? spanStreamUrl(projectRef(org, project), traceId) : undefined,
    (event) => {
      const key = queryKeyRef.current;
      if (event.type === "resync") {
        void queryClient.invalidateQueries({ queryKey: key, exact: true });
        return;
      }
      if (event.type === "trace") {
        queryClient.setQueryData<Schema<"TraceDetail">>(key, (current) =>
          current ? { ...current, trace: event.trace } : current,
        );
        return;
      }
      if (event.type !== "span") {
        return;
      }
      const current = queryClient.getQueryData<Schema<"TraceDetail">>(key);
      if (!current) {
        return;
      }
      const inserted = current.spans.every((row) => row.span.span_id !== event.span.span_id);
      queryClient.setQueryData<Schema<"TraceDetail">>(key, (data) => {
        if (!data) {
          return data;
        }
        const spans = insertSpan(data.spans, event.span);
        return { ...data, spans, trace: withSpanTotals(data.trace, spans) };
      });
      if (inserted) {
        mark(event.span.span_id);
      }
    },
  );

  useShortcut("toggle-mode", () => {
    chooseMode(NEXT_MODE[active]);
  });

  if (trace.isPending) {
    return (
      <div className="flex flex-col gap-2 p-3">
        <Skeleton className="h-4 w-1/2" />
        <Skeleton className="h-4 w-2/3" />
        <Skeleton className="h-4 w-1/3" />
      </div>
    );
  }
  if (trace.isError) {
    return (
      <p className="p-3 text-body">
        {problemOf(trace.error)?.title ?? "Could not load this trace"}
      </p>
    );
  }

  const loaded = trace.data;
  // Transcript mode shows the trace summary until a span is picked; the other modes always have a span.
  const summaryShown = active === "transcript" && spanId === undefined;
  const selected = summaryShown
    ? undefined
    : previous
      ? loaded.spans[0]?.span.span_id
      : (spanId ?? loaded.spans[0]?.span.span_id);
  const selectedRow = loaded.spans.find((row) => row.span.span_id === selected);
  const detached = selectedRow?.detached ?? false;
  const rootSpanId = loaded.spans.find((row) => row.span.parent_span_id === undefined)?.span
    .span_id;
  const selectedIsRoot = selectedRow !== undefined && selectedRow.span.parent_span_id === undefined;
  const showBar = active === "transcript" && onShowSummary !== undefined;
  const transcript = (
    <TranscriptView
      key={loaded.trace.trace_id}
      traceId={loaded.trace.trace_id}
      spans={loaded.spans}
      selectedSpanId={selected}
      onSelectSpan={onSelectSpan}
      mode={active}
      onMode={chooseMode}
      wide={wide}
      onWide={chooseWide}
    />
  );
  const spanDetail = selected ? (
    <SpanDetail
      traceId={loaded.trace.trace_id}
      spanId={selected}
      summary={selectedRow?.span}
      detached={detached}
      onAddFilter={onAddFilter}
      spans={loaded.spans}
    />
  ) : null;

  return (
    // Keyed by the loaded trace. A slow switch fades the new one in over the dimmed previous one.
    <div
      key={loaded.trace.trace_id}
      aria-busy={previous}
      className={cn(
        "flex min-h-0 flex-1 flex-col transition-opacity",
        slowTraceId === loaded.trace.trace_id && "trace-swap",
        previous && "pointer-events-none",
        dimmed && "opacity-60",
      )}
    >
      <TraceHeader
        trace={loaded.trace}
        spanId={selected}
        mode={active === "transcript" ? active : undefined}
        onClose={onClose}
        onPrevious={onPrevious}
        onNext={onNext}
        layout={layout}
        backLink={backLink}
        fullPageListReturn={fullPageListReturn}
        live={follow ? { status, onStop: () => setStoppedId(traceId) } : undefined}
        actions={
          <>
            <RunEvaluatorButton
              trace={loaded.trace}
              spanId={selectedRow ? selected : undefined}
              selectedIsRoot={selectedIsRoot}
            />
            <AddToDatasetButton
              traceId={loaded.trace.trace_id}
              rootSpanId={rootSpanId}
              spanId={selectedRow ? selected : undefined}
              selectedIsRoot={selectedIsRoot}
            />
            {selectedRow?.span.span_kind === "llm" ? (
              <Button asChild variant="outline" size="sm">
                <Link
                  to="/$org/$project/playground"
                  params={{ org, project }}
                  search={{ trace: loaded.trace.trace_id, span: selectedRow.span.span_id }}
                >
                  Open in playground
                </Link>
              </Button>
            ) : null}
            {selectedRow !== undefined && !selectedIsRoot ? (
              <AddToQueueButton
                target="span"
                items={[{ trace_id: loaded.trace.trace_id, span_id: selectedRow.span.span_id }]}
              />
            ) : (
              <AddToQueueButton target="trace" items={[{ trace_id: loaded.trace.trace_id }]} />
            )}
          </>
        }
      />
      {loaded.truncated ? (
        <div className="shrink-0 border-b bg-muted px-3 py-1 text-caption">
          Showing the first 20 000 spans
        </div>
      ) : null}
      <div className="relative min-h-0 flex-1">
        <div className="absolute inset-0">
          {active === "transcript" && wide ? (
            <div className="flex h-full min-h-0 flex-col">{transcript}</div>
          ) : (
            <SplitPane
              name={active === "timeline" ? "trace-timeline" : "trace"}
              mode={stacked ? "bottom" : "split"}
              still={stacked}
              start={
                active === "transcript" ? (
                  transcript
                ) : (
                  <TreeGrid
                    key={loaded.trace.trace_id}
                    spans={loaded.spans}
                    trace={loaded.trace}
                    selectedSpanId={selected}
                    onSelectSpan={onSelectSpan}
                    mode={active}
                    onMode={chooseMode}
                    timeline={active === "timeline"}
                    highlightedSpanIds={highlighted}
                  />
                )
              }
              end={
                summaryShown ? (
                  <TraceSummary
                    trace={loaded.trace}
                    spans={loaded.spans}
                    onSelectSpan={onSelectSpan}
                  />
                ) : showBar ? (
                  <div className="flex min-h-0 flex-1 flex-col">
                    <div className="flex items-center border-b px-4 py-1.5">
                      <Button
                        type="button"
                        variant="ghost"
                        size="xs"
                        className="text-body"
                        onClick={onShowSummary}
                      >
                        <ArrowLeft />
                        Trace summary
                      </Button>
                    </div>
                    {spanDetail}
                  </div>
                ) : (
                  spanDetail
                )
              }
            />
          )}
        </div>
      </div>
    </div>
  );
}

function spanStreamUrl(project: string, traceId: string): string {
  return `/api/v1/projects/${encodeURIComponent(project)}/traces/${encodeURIComponent(traceId)}/stream`;
}

/** Span count and duration follow the cached tree when a live span arrives without a trace event. */
function withSpanTotals(
  trace: Schema<"TraceSummary">,
  spans: readonly Schema<"TreeSpan">[],
): Schema<"TraceSummary"> {
  let endTime = trace.end_time;
  let endNs = instantNs(endTime);
  for (const row of spans) {
    const ns = instantNs(row.span.end_time);
    if (ns > endNs) {
      endNs = ns;
      endTime = row.span.end_time;
    }
  }
  return {
    ...trace,
    span_count: spans.length,
    duration_ns: Math.max(0, nsBetween(trace.start_time, endTime)),
    end_time: endTime,
  };
}
