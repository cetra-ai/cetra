import { useVirtualizer } from "@tanstack/react-virtual";
import { cn } from "cn";
import { ChevronRight, MoreHorizontal, Wrench } from "lucide-react";
import {
  type KeyboardEvent,
  type MouseEvent,
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
} from "react";
import type { Schema } from "@/api/types";
import { KindIcon } from "@/components/kind-icon";
import { ChatView } from "@/components/payload/chat-view";
import { Markdown } from "@/components/payload/markdown";
import { PayloadView } from "@/components/payload/payload-view";
import { ShowMore } from "@/components/payload/show-more";
import { ScoreBadge } from "@/components/score-badge";
import { TurnStrip } from "@/components/session/turn-strip";
import { TurnToolbar } from "@/components/session/turn-toolbar";
import type { SessionSteps } from "@/components/session/use-session-steps";
import { useProject } from "@/components/shell/project-context";
import { TimeText } from "@/components/time-text";
import { SplitPane } from "@/components/trace/split-pane";
import { TraceView } from "@/components/trace/trace-view";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { formatCost, formatDuration, formatTokens } from "@/lib/format";
import {
  type Bubble,
  bubbleOf,
  chipSteps,
  filterCounts,
  gapNs,
  previewLine,
  rawText,
  type TurnDensity,
  type TurnFilter,
  turnMatches,
  turnText,
  useTurnDensity,
} from "@/lib/session-turns";
import { isTypingTarget, matchesBinding } from "@/lib/shortcuts";

const TURN_ESTIMATE = 96;
const COMPACT_ESTIMATE = 28;
const OVERSCAN = 6;
/** Characters of a turn's text shown in its bubble. The details view keeps the full content. */
const BUBBLE_LIMIT = 20_000;
/** Steps shown as chips on a collapsed turn; the rest count as "+N more". */
const MAX_STEP_CHIPS = 6;

/** A turn in the conversation: its 1-based position in the session and the turn before it. */
type ConversationEntry = {
  turn: Schema<"SessionTurn">;
  index: number;
  previous: Schema<"SessionTurn"> | undefined;
};

/**
 * The turn the list scrolls to once it is rendered. The keys use "auto" (the list moves only as far
 * as needed); the strip, the errors chip and the URL use "center".
 */
type PendingScroll = { traceId: string; align: "auto" | "center" };

/**
 * The session's turns beside the selected turn's trace. The session page drives it from the URL
 * (`trace`); the reviewer drives it from its own state. The turns are the session's `turns`, each
 * with its steps (D-C-28). The search, the filters and the density live here too.
 */
export function SessionConversation({
  turns,
  steps,
  trace,
  onSelectTrace,
  scrollTarget,
  onScrolled,
  onCloseTrace,
}: {
  turns: readonly Schema<"SessionTurn">[];
  steps: SessionSteps;
  /** The selected turn's trace id. */
  trace: string | undefined;
  onSelectTrace: (traceId: string) => void;
  /** The trace id of the turn to scroll to, or null. */
  scrollTarget: string | null;
  /** Called once the trace id in `scrollTarget` has been taken up. */
  onScrolled: () => void;
  /** Closes the trace. Omitted, the trace has no close button. */
  onCloseTrace?: () => void;
}) {
  const { ref } = useProject();
  const [span, setSpan] = useState<{ traceId?: string; spanId?: string }>({});
  const spanId = span.traceId === trace ? span.spanId : undefined;
  const [query, setQuery] = useState("");
  const [filters, setFilters] = useState<ReadonlySet<TurnFilter>>(new Set());
  // The turn the list scrolls to once that turn is rendered, or null.
  const [pendingScroll, setPendingScroll] = useState<PendingScroll | null>(null);
  const density = useTurnDensity();
  const stepsReady = steps.status === "ready";
  // The lower-cased text each turn is searched by, rebuilt only when the turns change.
  const searchTexts = useMemo(
    () => new Map(turns.map((turn) => [turn.trace.trace_id, turnText(turn).toLowerCase()])),
    [turns],
  );
  const { activeFilters, entries, visibleIds } = useMemo(() => {
    // "Uses tools" needs the steps: until they are ready the filter does not apply.
    const active: ReadonlySet<TurnFilter> = stepsReady
      ? filters
      : new Set([...filters].filter((filter) => filter !== "tools"));
    const needle = query.trim().toLowerCase();
    const matched: ConversationEntry[] = turns.flatMap((turn, offset) => {
      const traceId = turn.trace.trace_id;
      const textMatches = needle === "" || (searchTexts.get(traceId)?.includes(needle) ?? false);
      return textMatches && turnMatches(turn, steps.stepsOf(traceId), "", active)
        ? [{ turn, index: offset + 1, previous: offset > 0 ? turns[offset - 1] : undefined }]
        : [];
    });
    return {
      activeFilters: active,
      entries: matched,
      visibleIds: new Set(matched.map((entry) => entry.turn.trace.trace_id)),
    };
  }, [turns, searchTexts, steps.stepsOf, query, filters, stepsReady]);
  const counts = useMemo(() => filterCounts(turns, steps.stepsOf), [turns, steps.stepsOf]);

  // The effect below reads these through refs, so it runs only when its target changes.
  const visibleRef = useRef(visibleIds);
  visibleRef.current = visibleIds;
  const filtersRef = useRef(filters);
  filtersRef.current = filters;
  const onScrolledRef = useRef(onScrolled);
  onScrolledRef.current = onScrolled;

  useEffect(() => {
    if (scrollTarget === null) {
      return;
    }
    if (!visibleRef.current.has(scrollTarget)) {
      setQuery("");
      if (filtersRef.current.size > 0) {
        setFilters(new Set());
      }
    }
    setPendingScroll({ traceId: scrollTarget, align: "center" });
    onScrolledRef.current();
  }, [scrollTarget]);

  function selectStep(traceId: string, spanId: string) {
    setSpan({ traceId, spanId });
    if (traceId !== trace) {
      onSelectTrace(traceId);
    }
  }

  /**
   * Selects a turn from the strip or the keys, and scrolls the list to it. A turn the search or
   * the filters hide clears them first, so the selection brings it back.
   */
  const pick = useCallback(
    (traceId: string, align: PendingScroll["align"] = "center") => {
      if (!visibleIds.has(traceId)) {
        setQuery("");
        setFilters(new Set());
      }
      setPendingScroll({ traceId, align });
      onSelectTrace(traceId);
    },
    [onSelectTrace, visibleIds],
  );

  function toggleFilter(filter: TurnFilter) {
    setFilters((current) => {
      const next = new Set(current);
      if (next.has(filter)) {
        next.delete(filter);
      } else {
        next.add(filter);
      }
      return next;
    });
  }

  /**
   * j and k move the selection through the visible turns. Nothing selected (or the selected turn
   * hidden) starts at the first visible turn; the ends do not wrap. Keys from the turn menu, a
   * text field or another control inside the list are left alone.
   */
  function onKeyDown(event: KeyboardEvent<HTMLElement>) {
    const target = event.target;
    if (
      !(target instanceof Element) ||
      !event.currentTarget.contains(target) ||
      target.closest("[role='menu']") !== null ||
      isTypingTarget(target)
    ) {
      return;
    }
    let step = 0;
    if (matchesBinding(event, "row-next")) {
      step = 1;
    } else if (matchesBinding(event, "row-prev")) {
      step = -1;
    }
    if (step === 0) {
      return;
    }
    event.preventDefault();
    const selected = entries.findIndex((entry) => entry.turn.trace.trace_id === trace);
    const nextIndex =
      selected === -1 ? 0 : Math.min(Math.max(selected + step, 0), entries.length - 1);
    const entry = entries[nextIndex];
    if (entry) {
      pick(entry.turn.trace.trace_id, "auto");
    }
  }

  return (
    <div className="relative min-h-0 flex-1">
      <div className="absolute inset-0">
        <SplitPane
          name="session"
          start={
            <div className="flex min-h-0 flex-1 flex-col">
              <TurnToolbar
                query={query}
                onQuery={setQuery}
                filters={activeFilters}
                onToggleFilter={toggleFilter}
                counts={counts}
                stepsReady={stepsReady}
              />
              <TurnStrip turns={turns} visible={visibleIds} selectedTraceId={trace} onPick={pick} />
              <section
                aria-label="Conversation"
                // biome-ignore lint/a11y/noNoninteractiveTabindex: the conversation is one keyboard stop that takes j and k, like a list
                tabIndex={0}
                onKeyDown={onKeyDown}
                className="focus-ring flex min-h-0 flex-1 flex-col"
              >
                {entries.length === 0 && turns.length > 0 ? (
                  <p className="px-4 py-3 text-body text-muted-foreground">No turns match.</p>
                ) : (
                  <TurnList
                    entries={entries}
                    selectedTraceId={trace}
                    steps={steps}
                    density={density}
                    scrollTo={pendingScroll}
                    onScrolled={() => {
                      setPendingScroll(null);
                    }}
                    onSelect={onSelectTrace}
                    onSelectStep={selectStep}
                    projectRef={ref}
                  />
                )}
              </section>
            </div>
          }
          end={
            trace ? (
              <TraceView
                key={trace}
                stacked
                traceId={trace}
                spanId={spanId}
                onSelectSpan={(next) => setSpan({ traceId: trace, spanId: next })}
                onShowSummary={() => setSpan({ traceId: trace })}
                onClose={onCloseTrace}
              />
            ) : (
              <p className="p-3 text-caption text-muted-foreground">Select a turn</p>
            )
          }
        />
      </div>
    </div>
  );
}

function TurnList({
  entries,
  selectedTraceId,
  steps,
  density,
  scrollTo,
  onScrolled,
  onSelect,
  onSelectStep,
  projectRef,
}: {
  entries: readonly ConversationEntry[];
  selectedTraceId?: string;
  steps: SessionSteps;
  density: TurnDensity;
  /** Scrolls the list to this turn once it is rendered. */
  scrollTo: PendingScroll | null;
  /** Called once the list has scrolled to `scrollTo`. */
  onScrolled: () => void;
  onSelect: (traceId: string) => void;
  onSelectStep: (traceId: string, spanId: string) => void;
  projectRef: string;
}) {
  const scrollRef = useRef<HTMLDivElement>(null);
  // The expanded turns, keyed by trace id so the state survives virtualization.
  const [expanded, setExpanded] = useState<ReadonlySet<string>>(new Set());
  const virtualizer = useVirtualizer({
    count: entries.length,
    getScrollElement: () => scrollRef.current,
    // Keyed by trace id, so a turn keeps its measured size when the search or filters move it.
    getItemKey: (index) => entries[index]?.turn.trace.trace_id ?? index,
    estimateSize: () => (density === "compact" ? COMPACT_ESTIMATE : TURN_ESTIMATE),
    measureElement: (element) => {
      const height = element.getBoundingClientRect().height;
      return height > 0 ? height : TURN_ESTIMATE;
    },
    overscan: OVERSCAN,
  });

  // biome-ignore lint/correctness/useExhaustiveDependencies: a density change changes the row heights, so the rows are measured again
  useEffect(() => {
    virtualizer.measure();
  }, [density, virtualizer]);

  // Read through a ref, so the scroll runs when the target or the rendered list changes only.
  const onScrolledRef = useRef(onScrolled);
  onScrolledRef.current = onScrolled;
  useEffect(() => {
    if (scrollTo === null) {
      return;
    }
    const index = entries.findIndex((entry) => entry.turn.trace.trace_id === scrollTo.traceId);
    // Not rendered yet (the filters are clearing): wait for the next render.
    if (index >= 0) {
      virtualizer.scrollToIndex(index, { align: scrollTo.align });
      onScrolledRef.current();
    }
  }, [scrollTo, entries, virtualizer]);

  return (
    <div ref={scrollRef} data-scroll-container className="min-h-0 flex-1 overflow-auto">
      <div
        className="relative mx-auto w-full max-w-[760px]"
        style={{ height: virtualizer.getTotalSize() }}
      >
        {virtualizer.getVirtualItems().map((item) => {
          const entry = entries[item.index];
          if (!entry) {
            return null;
          }
          const { turn } = entry;
          const traceId = turn.trace.trace_id;
          return (
            <div
              key={traceId}
              data-index={item.index}
              ref={virtualizer.measureElement}
              className="absolute top-0 left-0 w-full"
              style={{ transform: `translateY(${item.start}px)` }}
            >
              {density === "compact" ? (
                <CompactTurn
                  index={entry.index}
                  turn={turn}
                  selected={traceId === selectedTraceId}
                  toolCount={toolStepCount(steps, traceId)}
                  onSelect={() => onSelect(traceId)}
                />
              ) : (
                <Turn
                  index={entry.index}
                  turn={turn}
                  previous={entry.previous}
                  selected={traceId === selectedTraceId}
                  expanded={expanded.has(traceId)}
                  onToggle={() => {
                    setExpanded((current) => {
                      const next = new Set(current);
                      if (next.has(traceId)) {
                        next.delete(traceId);
                      } else {
                        next.add(traceId);
                      }
                      return next;
                    });
                  }}
                  steps={steps}
                  onSelect={() => onSelect(traceId)}
                  onSelectStep={onSelectStep}
                  projectRef={projectRef}
                />
              )}
            </div>
          );
        })}
      </div>
    </div>
  );
}

/** The number of tool steps of a turn; 0 while the steps are not ready. */
function toolStepCount(steps: SessionSteps, traceId: string): number {
  return steps.stepsOf(traceId)?.filter((step) => step.span_kind === "tool").length ?? 0;
}

/** A click selects the turn, except on a control inside it and on a click that ends a text selection. */
function onTurnClick(event: MouseEvent<HTMLElement>, onSelect: () => void) {
  const target = event.target as HTMLElement;
  if (
    target.closest(
      "button, a, input, textarea, select, [role='button'], [role='menuitem'], [role='tab']",
    )
  ) {
    return;
  }
  if ((window.getSelection()?.toString() ?? "").length > 0) {
    return;
  }
  onSelect();
}

/** One line per turn in the compact density: index, name, the first line of each bubble, tools, duration. */
function CompactTurn({
  index,
  turn,
  selected,
  toolCount,
  onSelect,
}: {
  index: number;
  turn: Schema<"SessionTurn">;
  selected: boolean;
  toolCount: number;
  onSelect: () => void;
}) {
  const trace = turn.trace;
  const failed = trace.status === "error";
  const input = bubbleOf(turn.input, "user")?.text.split("\n")[0];
  const output = bubbleOf(turn.output, "assistant")?.text.split("\n")[0];
  return (
    // The whole line selects the turn; the name button is the keyboard path.
    // biome-ignore lint/a11y/noStaticElementInteractions: the name button is the keyboard equivalent.
    // biome-ignore lint/a11y/useKeyWithClickEvents: the name button is the keyboard equivalent.
    <div
      className={cn(
        "grid h-7 grid-cols-[2.5rem_9rem_minmax(0,1fr)_3.5rem_4rem] items-center gap-2 border-b px-4 text-body",
        selected
          ? "bg-selection shadow-[inset_2px_0_0_var(--primary)]"
          : cn("hover:bg-row-hover", failed && "shadow-[inset_2px_0_0_var(--status-error)]"),
      )}
      onClick={(event) => {
        onTurnClick(event, onSelect);
      }}
    >
      <span className="num text-muted-foreground">#{index}</span>
      <button
        type="button"
        className="focus-ring truncate rounded-sm text-left font-medium"
        aria-current={selected ? "true" : undefined}
        onClick={onSelect}
      >
        {trace.name}
      </button>
      <span className="min-w-0 truncate text-muted-foreground">
        {input}
        {input !== undefined && output !== undefined ? " → " : null}
        {output}
      </span>
      <span className="num flex items-center gap-1 text-muted-foreground">
        {toolCount > 0 ? (
          <>
            <Wrench aria-hidden="true" className="size-3" />
            {toolCount}
          </>
        ) : null}
      </span>
      <span className="num text-right text-muted-foreground">
        {formatDuration(trace.duration_ns)}
      </span>
    </div>
  );
}

function Turn({
  index,
  turn,
  previous,
  selected,
  expanded,
  onToggle,
  steps,
  onSelect,
  onSelectStep,
  projectRef,
}: {
  index: number;
  turn: Schema<"SessionTurn">;
  /** The turn before this one; its start time makes the gap. */
  previous: Schema<"SessionTurn"> | undefined;
  selected: boolean;
  expanded: boolean;
  onToggle: () => void;
  steps: SessionSteps;
  onSelect: () => void;
  onSelectStep: (traceId: string, spanId: string) => void;
  projectRef: string;
}) {
  const [details, setDetails] = useState(false);
  const trace = turn.trace;
  const traceId = trace.trace_id;
  const failed = trace.status === "error";
  const turnSteps = steps.stepsOf(traceId);
  const gap = gapNs(previous, turn);
  const input = rawText(turn.input);
  const output = rawText(turn.output);
  return (
    // The whole turn selects it; the meta-line button is the keyboard path. Clicks on controls
    // inside the turn, and clicks that end a text selection, are left alone.
    // biome-ignore lint/a11y/noStaticElementInteractions: the meta-line button is the keyboard equivalent.
    // biome-ignore lint/a11y/useKeyWithClickEvents: the meta-line button is the keyboard equivalent.
    <div
      className={cn(
        "group flex cursor-pointer flex-col gap-3 border-b px-4 py-3",
        selected
          ? "bg-selection shadow-[inset_2px_0_0_var(--primary)]"
          : cn("hover:bg-row-hover", failed && "shadow-[inset_2px_0_0_var(--status-error)]"),
      )}
      onClick={(event) => {
        onTurnClick(event, onSelect);
      }}
    >
      <div className="flex flex-wrap items-center gap-2 text-body text-muted-foreground">
        {turnSteps !== undefined && turnSteps.length > 0 ? (
          <Button
            type="button"
            variant="ghost"
            size="icon-xs"
            aria-expanded={expanded}
            aria-label={expanded ? "Hide steps" : "Show steps"}
            onClick={onToggle}
          >
            <ChevronRight aria-hidden="true" className={cn(expanded && "rotate-90")} />
          </Button>
        ) : (
          <span className="size-6 shrink-0" />
        )}
        <button
          type="button"
          // The meta line wraps when the column is narrow, so badges never run into the cost. Its
          // floor fits the time (a long name truncates instead); below it, the cost wraps instead.
          className="focus-ring flex min-w-36 flex-1 flex-wrap items-center gap-x-2 gap-y-1 rounded-sm text-left"
          aria-current={selected ? "true" : undefined}
          onClick={onSelect}
        >
          <span className="num shrink-0">#{index}</span>
          <span className="min-w-0 truncate font-medium text-foreground">{trace.name}</span>
          <span className="shrink-0 whitespace-nowrap">
            <TimeText value={trace.start_time} style="time" />
          </span>
          {gap !== undefined ? (
            <span className="num shrink-0 whitespace-nowrap">+{formatDuration(gap)}</span>
          ) : null}
          {[...trace.scores]
            .sort((left, right) => left.name.localeCompare(right.name))
            .map((score) => (
              <Badge key={score.score_id} variant="secondary" className="h-4 px-1.5 text-caption">
                {score.name}{" "}
                <ScoreBadge name={score.name} value={score.value} text={badgeValue(score.value)} />
              </Badge>
            ))}
          {failed ? (
            <span className="shrink-0 rounded-sm bg-status-error/12 px-1.5 text-caption font-medium text-status-error">
              Failed
            </span>
          ) : null}
        </button>
        <span className="num shrink-0 text-muted-foreground">
          {formatDuration(trace.duration_ns)}
          {trace.cost_total !== undefined ? ` · ${formatCost(trace.cost_total)}` : null}
        </span>
        <DropdownMenu>
          <DropdownMenuTrigger asChild>
            <Button
              type="button"
              variant="ghost"
              size="icon-xs"
              aria-label="Turn actions"
              className="opacity-0 group-hover:opacity-100 focus-visible:opacity-100 data-[state=open]:opacity-100"
            >
              <MoreHorizontal />
            </Button>
          </DropdownMenuTrigger>
          <DropdownMenuContent align="end">
            <DropdownMenuItem disabled={input === undefined} onSelect={() => copyText(input)}>
              Copy input
            </DropdownMenuItem>
            <DropdownMenuItem disabled={output === undefined} onSelect={() => copyText(output)}>
              Copy output
            </DropdownMenuItem>
            <DropdownMenuItem
              onSelect={() => {
                setDetails((current) => !current);
              }}
            >
              {details ? "Hide details" : "Show details"}
            </DropdownMenuItem>
          </DropdownMenuContent>
        </DropdownMenu>
      </div>
      {details ? (
        <TurnPayloads turn={turn} projectRef={projectRef} />
      ) : (
        <TurnBubbles
          turn={turn}
          turnSteps={turnSteps}
          status={steps.status}
          expanded={expanded}
          onSelectStep={(spanId) => onSelectStep(traceId, spanId)}
        />
      )}
    </div>
  );
}

function TurnBubbles({
  turn,
  turnSteps,
  status,
  expanded,
  onSelectStep,
}: {
  turn: Schema<"SessionTurn">;
  /** The turn's steps once loaded; undefined while pending or after an error. */
  turnSteps: readonly Schema<"SpanSummary">[] | undefined;
  status: SessionSteps["status"];
  expanded: boolean;
  onSelectStep: (spanId: string) => void;
}) {
  const input = bubbleOf(turn.input, "user");
  const output = bubbleOf(turn.output, "assistant");
  const steps = expanded ? (
    <ExpandedSteps steps={turnSteps} status={status} onSelectStep={onSelectStep} />
  ) : (
    <StepChips steps={turnSteps} />
  );
  if (turn.input === undefined && turn.output === undefined) {
    return (
      <div className="flex flex-col gap-2">
        <p className="text-body text-muted-foreground">No root input/output</p>
        {steps}
      </div>
    );
  }
  return (
    <div className="flex flex-col gap-2">
      {input ? <BubbleBox speaker="user" bubble={input} /> : null}
      {steps}
      {output ? <BubbleBox speaker="assistant" bubble={output} /> : null}
    </div>
  );
}

/** The turn's steps as chips on a collapsed turn; nothing while they load or when it has none. */
function StepChips({ steps }: { steps: readonly Schema<"SpanSummary">[] | undefined }) {
  const chips = steps === undefined ? [] : chipSteps(steps);
  if (chips.length === 0) {
    return null;
  }
  const shown = chips.slice(0, MAX_STEP_CHIPS);
  const rest = chips.length - shown.length;
  return (
    <div className="flex flex-wrap gap-1.5 pl-6">
      {shown.map((step) => (
        <StepChip key={step.span_id} step={step} />
      ))}
      {rest > 0 ? <span className="text-caption text-muted-foreground">+{rest} more</span> : null}
    </div>
  );
}

function StepChip({ step }: { step: Schema<"SpanSummary"> }) {
  const failed = step.status === "error";
  return (
    <span
      className={cn(
        "inline-flex items-center gap-1 rounded-md border bg-muted/40 px-1.5 text-caption",
        failed && "border-status-error/40 text-status-error",
      )}
    >
      <KindIcon kind={step.span_kind} />
      <span className="max-w-[20ch] truncate">{step.name}</span>
      <span className="num text-muted-foreground">{formatDuration(step.duration_ns)}</span>
      {failed && step.status_message !== undefined ? (
        <span className="max-w-[24ch] truncate" title={step.status_message}>
          {step.status_message}
        </span>
      ) : null}
    </span>
  );
}

/** The turn's steps in start order, one clickable row each. */
function ExpandedSteps({
  steps,
  status,
  onSelectStep,
}: {
  steps: readonly Schema<"SpanSummary">[] | undefined;
  status: SessionSteps["status"];
  onSelectStep: (spanId: string) => void;
}) {
  if (status === "pending") {
    return <p className="text-caption text-muted-foreground">Loading steps</p>;
  }
  if (status === "error") {
    return <p className="text-caption text-muted-foreground">Could not load the steps</p>;
  }
  if (steps === undefined || steps.length === 0) {
    return <p className="text-caption text-muted-foreground">No model or tool calls</p>;
  }
  return (
    <ul aria-label="Steps" className="ml-6 flex flex-col border-l pl-3">
      {steps.map((step) => (
        <li key={step.span_id}>
          <StepRow step={step} onSelect={() => onSelectStep(step.span_id)} />
        </li>
      ))}
    </ul>
  );
}

function StepRow({ step, onSelect }: { step: Schema<"SpanSummary">; onSelect: () => void }) {
  const preview = previewLine(step.input_preview);
  return (
    <button
      type="button"
      className="focus-ring flex w-full flex-col items-stretch rounded-sm px-1 py-0.5 text-left text-body hover:bg-row-hover"
      onClick={onSelect}
    >
      <div className="flex items-center gap-2">
        <KindIcon kind={step.span_kind} />
        <span className="truncate">{step.name}</span>
        {step.model !== undefined ? (
          <span className="text-muted-foreground">{step.model}</span>
        ) : null}
        {step.total_tokens !== undefined ? (
          <span className="num text-muted-foreground">
            {formatTokens(step.total_tokens)} tokens
          </span>
        ) : null}
        {step.status === "error" && step.status_message !== undefined ? (
          <span className="truncate text-status-error">{step.status_message}</span>
        ) : null}
        <span className="num ml-auto shrink-0 text-muted-foreground">
          {formatDuration(step.duration_ns)}
        </span>
      </div>
      {preview !== undefined ? (
        <div className="truncate pl-6 font-mono text-caption text-muted-foreground" title={preview}>
          {preview}
        </div>
      ) : null}
    </button>
  );
}

function BubbleBox({ speaker, bubble }: { speaker: "user" | "assistant"; bubble: Bubble }) {
  const truncated = bubble.text.length > BUBBLE_LIMIT;
  const shown = truncated ? bubble.text.slice(0, BUBBLE_LIMIT) : bubble.text;
  return (
    <div
      className={cn(
        "max-w-[85%] rounded-lg px-3 py-2 text-ui",
        speaker === "user" ? "ml-auto bg-muted" : "mr-auto border bg-card",
      )}
    >
      <ShowMore>
        {bubble.kind === "markdown" ? (
          <Markdown text={shown} />
        ) : (
          <pre className="font-mono text-body break-words whitespace-pre-wrap">{shown}</pre>
        )}
      </ShowMore>
      {truncated ? (
        <p className="mt-1 text-caption text-muted-foreground">Truncated, see details</p>
      ) : null}
    </div>
  );
}

function copyText(text: string | undefined) {
  if (text !== undefined) {
    void navigator.clipboard.writeText(text).catch(() => undefined);
  }
}

function TurnPayloads({ turn, projectRef }: { turn: Schema<"SessionTurn">; projectRef: string }) {
  if (turn.input === undefined && turn.output === undefined) {
    return <p className="text-caption text-muted-foreground">No root input/output</p>;
  }
  const input = messagesOf(turn.input);
  const output = messagesOf(turn.output);
  if (input && output) {
    return <ChatView input={input} output={output} projectRef={projectRef} />;
  }
  return (
    <div className="flex flex-col gap-2">
      <PayloadView payload={turn.input} label="Input" />
      <PayloadView payload={turn.output} label="Output" />
    </div>
  );
}

function messagesOf(payload: Schema<"Payload"> | undefined): Schema<"Message">[] | undefined {
  if (!payload) {
    return [];
  }
  if (payload.format === "messages") {
    return payload.value;
  }
  return undefined;
}

export function badgeValue(value: Schema<"ScoreValue">): string {
  if (value.data_type === "numeric") {
    return value.value.toFixed(3).replace(/\.?0+$/, "");
  }
  if (value.data_type === "boolean") {
    return value.value ? "✓" : "✗";
  }
  if (value.data_type === "categorical") {
    return value.value.label;
  }
  return value.value;
}
