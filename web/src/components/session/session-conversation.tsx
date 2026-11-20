import { useVirtualizer } from "@tanstack/react-virtual";
import { cn } from "cn";
import { MoreHorizontal, Wrench } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import type { Schema } from "@/api/types";
import { ChatView } from "@/components/payload/chat-view";
import { Markdown } from "@/components/payload/markdown";
import { PayloadView } from "@/components/payload/payload-view";
import { ShowMore } from "@/components/payload/show-more";
import { ScoreBadge } from "@/components/score-badge";
import { useProject } from "@/components/shell/project-context";
import { StatusDot } from "@/components/status-dot";
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
import { formatCost, formatDuration } from "@/lib/format";

const TURN_ESTIMATE = 96;
const OVERSCAN = 6;
/** Characters of a turn's text shown in its bubble. The details view keeps the full content. */
const BUBBLE_LIMIT = 20_000;

/**
 * The session's turns beside the selected turn's trace. The session page drives it from the URL
 * (`trace`); the reviewer drives it from its own state. The turns are the session's `turns`.
 */
export function SessionConversation({
  turns,
  trace,
  onSelectTrace,
  scrollTarget,
  onScrolled,
  onCloseTrace,
}: {
  turns: readonly Schema<"SessionTurn">[];
  /** The selected turn's trace id. */
  trace: string | undefined;
  onSelectTrace: (traceId: string) => void;
  /** The turn index to scroll to, or null. */
  scrollTarget: number | null;
  /** Called once the list has scrolled to `scrollTarget`. */
  onScrolled: () => void;
  /** Closes the trace. Omitted, the trace has no close button. */
  onCloseTrace?: () => void;
}) {
  const { ref } = useProject();
  const [span, setSpan] = useState<{ traceId?: string; spanId?: string }>({});
  const spanId = span.traceId === trace ? span.spanId : undefined;

  return (
    <div className="relative min-h-0 flex-1">
      <div className="absolute inset-0">
        <SplitPane
          name="session"
          start={
            <TurnList
              turns={turns}
              selectedTraceId={trace}
              scrollTarget={scrollTarget}
              onScrolled={onScrolled}
              onSelect={onSelectTrace}
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
  scrollTarget,
  onScrolled,
  onSelect,
  projectRef,
}: {
  turns: readonly Schema<"SessionTurn">[];
  selectedTraceId?: string;
  /** Scrolls the list to this turn index when it is set. */
  scrollTarget: number | null;
  onScrolled: () => void;
  onSelect: (traceId: string) => void;
  projectRef: string;
}) {
  const scrollRef = useRef<HTMLDivElement>(null);
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
    if (scrollTarget !== null) {
      virtualizer.scrollToIndex(scrollTarget, { align: "center" });
      onScrolled();
    }
  }, [scrollTarget, virtualizer, onScrolled]);

  return (
    <section aria-label="Conversation" className="flex min-h-0 flex-1 flex-col">
      <div ref={scrollRef} data-scroll-container className="min-h-0 flex-1 overflow-auto">
        <div className="relative w-full" style={{ height: virtualizer.getTotalSize() }}>
          {virtualizer.getVirtualItems().map((item) => {
            const turn = turns[item.index];
            if (!turn) {
              return null;
            }
            const traceId = turn.trace.trace_id;
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
                  selected={traceId === selectedTraceId}
                  onSelect={() => onSelect(traceId)}
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
  selected,
  onSelect,
  projectRef,
}: {
  index: number;
  turn: Schema<"SessionTurn">;
  selected: boolean;
  onSelect: () => void;
  projectRef: string;
}) {
  const [details, setDetails] = useState(false);
  const trace = turn.trace;
  const failed = trace.status === "error";
  const tools = toolCallCount(turn);
  const input = rawText(turn.input);
  const output = rawText(turn.output);
  return (
    // The whole turn selects it; the meta-line button is the keyboard path. Clicks on controls
    // inside the turn, and clicks that end a text selection, are left alone.
    // biome-ignore lint/a11y/noStaticElementInteractions: the meta-line button is the keyboard equivalent.
    // biome-ignore lint/a11y/useKeyWithClickEvents: the meta-line button is the keyboard equivalent.
    <div
      className={cn(
        "flex cursor-pointer flex-col gap-2 border-b px-3 py-2",
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
        <button
          type="button"
          className="focus-ring flex min-w-0 flex-1 items-center gap-2 rounded-sm text-left"
          aria-current={selected ? "true" : undefined}
          onClick={onSelect}
        >
          <span className="num shrink-0">#{index}</span>
          <TimeText value={trace.start_time} style="time" />
          <span className="min-w-0 truncate font-medium text-foreground">{trace.name}</span>
          <span className="num shrink-0">{formatDuration(trace.duration_ns)}</span>
          {trace.cost_total !== undefined ? (
            <span className="num shrink-0">{formatCost(trace.cost_total)}</span>
          ) : null}
          {[...trace.scores]
            .sort((left, right) => left.name.localeCompare(right.name))
            .map((score) => (
              <Badge key={score.score_id} variant="secondary" className="h-4 px-1.5 text-caption">
                {score.name}{" "}
                <ScoreBadge name={score.name} value={score.value} text={badgeValue(score.value)} />
              </Badge>
            ))}
          <StatusDot status={trace.status} />
          {failed ? (
            <span className="shrink-0 rounded-sm bg-status-error/12 px-1.5 text-caption font-medium text-status-error">
              Failed
            </span>
          ) : null}
        </button>
        {tools > 0 ? (
          <span className="inline-flex shrink-0 items-center gap-1 text-caption">
            <Wrench aria-hidden="true" className="size-3" />
            <span className="num">{tools} tool calls</span>
          </span>
        ) : null}
        <DropdownMenu>
          <DropdownMenuTrigger asChild>
            <Button type="button" variant="ghost" size="icon-xs" aria-label="Turn actions">
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
      {details ? <TurnPayloads turn={turn} projectRef={projectRef} /> : <TurnBubbles turn={turn} />}
    </div>
  );
}

function TurnBubbles({ turn }: { turn: Schema<"SessionTurn"> }) {
  if (turn.input === undefined && turn.output === undefined) {
    return <p className="text-body text-muted-foreground">No root input/output</p>;
  }
  const input = bubbleOf(turn.input, "user");
  const output = bubbleOf(turn.output, "assistant");
  return (
    <div className="flex flex-col gap-2">
      {input ? <BubbleBox speaker="user" bubble={input} /> : null}
      {output ? <BubbleBox speaker="assistant" bubble={output} /> : null}
    </div>
  );
}

type Bubble = { kind: "markdown" | "json"; text: string };

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

/** The bubble text of one side of a turn: no bubble when the side has no text. */
function bubbleOf(
  payload: Schema<"Payload"> | undefined,
  role: "user" | "assistant",
): Bubble | undefined {
  if (!payload) {
    return undefined;
  }
  if (payload.format === "text") {
    return nonEmpty({ kind: "markdown", text: payload.value });
  }
  if (payload.format === "json") {
    return nonEmpty({ kind: "json", text: JSON.stringify(payload.value) ?? "" });
  }
  const matching = payload.value.filter((message) => message.role === role);
  const message = matching[matching.length - 1];
  if (!message) {
    return undefined;
  }
  const text = message.parts
    .flatMap((part) =>
      part.type === "text" && typeof part.content === "string" ? [part.content] : [],
    )
    .join("\n\n");
  return nonEmpty({ kind: "markdown", text });
}

function nonEmpty(bubble: Bubble): Bubble | undefined {
  return bubble.text.trim() === "" ? undefined : bubble;
}

/** A payload as the raw copy has it: the text itself, or compact JSON. */
function rawText(payload: Schema<"Payload"> | undefined): string | undefined {
  if (!payload) {
    return undefined;
  }
  return payload.format === "text" ? payload.value : (JSON.stringify(payload.value) ?? "");
}

function copyText(text: string | undefined) {
  if (text !== undefined) {
    void navigator.clipboard.writeText(text).catch(() => undefined);
  }
}

function toolCallCount(turn: Schema<"SessionTurn">): number {
  let count = 0;
  for (const payload of [turn.input, turn.output]) {
    if (payload?.format !== "messages") {
      continue;
    }
    for (const message of payload.value) {
      for (const part of message.parts) {
        if (part.type === "tool_call") {
          count += 1;
        }
      }
    }
  }
  return count;
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
