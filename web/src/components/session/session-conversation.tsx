import { useVirtualizer } from "@tanstack/react-virtual";
import { cn } from "cn";
import { ChevronRight, MoreHorizontal } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import type { Schema } from "@/api/types";
import { KindIcon } from "@/components/kind-icon";
import { ChatView } from "@/components/payload/chat-view";
import { Markdown } from "@/components/payload/markdown";
import { PayloadView } from "@/components/payload/payload-view";
import { ShowMore } from "@/components/payload/show-more";
import { ScoreBadge } from "@/components/score-badge";
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
import { type Bubble, bubbleOf, chipSteps, gapNs, previewLine, rawText } from "@/lib/session-turns";

const TURN_ESTIMATE = 96;
const OVERSCAN = 6;
/** Characters of a turn's text shown in its bubble. The details view keeps the full content. */
const BUBBLE_LIMIT = 20_000;
/** Steps shown as chips on a collapsed turn; the rest count as "+N more". */
const MAX_STEP_CHIPS = 6;

/**
 * The session's turns beside the selected turn's trace. The session page drives it from the URL
 * (`trace`); the reviewer drives it from its own state. The turns are the session's `turns`, each
 * with its steps (D-C-28).
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
  /** Called once the list has scrolled to `scrollTarget`. */
  onScrolled: () => void;
  /** Closes the trace. Omitted, the trace has no close button. */
  onCloseTrace?: () => void;
}) {
  const { ref } = useProject();
  const [span, setSpan] = useState<{ traceId?: string; spanId?: string }>({});
  const spanId = span.traceId === trace ? span.spanId : undefined;

  function selectStep(traceId: string, spanId: string) {
    setSpan({ traceId, spanId });
    if (traceId !== trace) {
      onSelectTrace(traceId);
    }
  }

  return (
    <div className="relative min-h-0 flex-1">
      <div className="absolute inset-0">
        <SplitPane
          name="session"
          start={
            <TurnList
              turns={turns}
              selectedTraceId={trace}
              steps={steps}
              scrollTo={scrollTarget}
              onScrolled={onScrolled}
              onSelect={onSelectTrace}
              onSelectStep={selectStep}
              projectRef={ref}
            />
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
  turns,
  selectedTraceId,
  steps,
  scrollTo,
  onScrolled,
  onSelect,
  onSelectStep,
  projectRef,
}: {
  turns: readonly Schema<"SessionTurn">[];
  selectedTraceId?: string;
  steps: SessionSteps;
  /** Scrolls the list to this trace id's turn when it is set. */
  scrollTo: string | null;
  onScrolled: () => void;
  onSelect: (traceId: string) => void;
  onSelectStep: (traceId: string, spanId: string) => void;
  projectRef: string;
}) {
  const scrollRef = useRef<HTMLDivElement>(null);
  // The expanded turns, keyed by trace id so the state survives virtualization.
  const [expanded, setExpanded] = useState<ReadonlySet<string>>(new Set());
  const virtualizer = useVirtualizer({
    count: turns.length,
    getScrollElement: () => scrollRef.current,
    estimateSize: () => TURN_ESTIMATE,
    measureElement: (element) => {
      const height = element.getBoundingClientRect().height;
      return height > 0 ? height : TURN_ESTIMATE;
    },
    overscan: OVERSCAN,
  });

  useEffect(() => {
    if (scrollTo !== null) {
      const index = turns.findIndex((turn) => turn.trace.trace_id === scrollTo);
      if (index >= 0) {
        virtualizer.scrollToIndex(index, { align: "center" });
      }
      onScrolled();
    }
  }, [scrollTo, turns, virtualizer, onScrolled]);

  return (
    <section aria-label="Conversation" className="flex min-h-0 flex-1 flex-col">
      <div ref={scrollRef} data-scroll-container className="min-h-0 flex-1 overflow-auto">
        <div
          className="relative mx-auto w-full max-w-[760px]"
          style={{ height: virtualizer.getTotalSize() }}
        >
          {virtualizer.getVirtualItems().map((item) => {
            const turn = turns[item.index];
            if (!turn) {
              return null;
            }
            const traceId = turn.trace.trace_id;
            const previous = item.index > 0 ? turns[item.index - 1] : undefined;
            return (
              <div
                key={traceId}
                data-index={item.index}
                ref={virtualizer.measureElement}
                className="absolute top-0 left-0 w-full"
                style={{ transform: `translateY(${item.start}px)` }}
              >
                <Turn
                  index={item.index + 1}
                  turn={turn}
                  previous={previous}
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
              </div>
            );
          })}
        </div>
      </div>
    </section>
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
      }}
    >
      <div className="flex items-center gap-2 text-body text-muted-foreground">
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
          className="focus-ring flex min-w-0 flex-1 items-center gap-2 rounded-sm text-left"
          aria-current={selected ? "true" : undefined}
          onClick={onSelect}
        >
          <span className="num shrink-0">#{index}</span>
          <span className="min-w-0 truncate font-medium text-foreground">{trace.name}</span>
          <TimeText value={trace.start_time} style="time" />
          {gap !== undefined ? <span className="num">+{formatDuration(gap)}</span> : null}
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
