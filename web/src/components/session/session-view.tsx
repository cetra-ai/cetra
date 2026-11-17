import { useQuery } from "@tanstack/react-query";
import { Link, useNavigate, useRouter } from "@tanstack/react-router";
import { useVirtualizer } from "@tanstack/react-virtual";
import { cn } from "cn";
import {
  ChevronLeft,
  Clock,
  Coins,
  DollarSign,
  MessagesSquare,
  MoreHorizontal,
  TriangleAlert,
  Wrench,
} from "lucide-react";
import { useEffect, useRef, useState } from "react";
import { problemOf } from "@/api/problem";
import { sessionQuery } from "@/api/queries";
import type { Schema } from "@/api/types";
import { AnnotationForm } from "@/components/annotation/annotation-form";
import { CopyButton } from "@/components/copy-button";
import { DisabledReason } from "@/components/disabled-reason";
import { ChatView } from "@/components/payload/chat-view";
import { Markdown } from "@/components/payload/markdown";
import { PayloadView } from "@/components/payload/payload-view";
import { ShowMore } from "@/components/payload/show-more";
import { ScoreBadge } from "@/components/score-badge";
import { useProject } from "@/components/shell/project-context";
import { StatusDot } from "@/components/status-dot";
import { TimeText } from "@/components/time-text";
import { MetricChip } from "@/components/trace/metric-chip";
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
import {
  Sheet,
  SheetContent,
  SheetDescription,
  SheetHeader,
  SheetTitle,
} from "@/components/ui/sheet";
import { Skeleton } from "@/components/ui/skeleton";
import { targetKey } from "@/lib/annotation";
import { categoryClasses } from "@/lib/colors";
import { formatCost, formatDuration, formatTokens } from "@/lib/format";
import { useBackTo, useListReturn } from "@/lib/list-return";
import { canAnnotate } from "@/lib/roles";
import { parseSessionsSearch, type SessionPageSearch } from "@/lib/search/sessions";

const TURN_ESTIMATE = 96;
const OVERSCAN = 6;
/** Characters of a turn's text shown in its bubble. The details view keeps the full content. */
const BUBBLE_LIMIT = 20_000;

export function SessionView({
  sessionId,
  search,
}: {
  sessionId: string;
  search: SessionPageSearch;
}) {
  const { org, project, ref } = useProject();
  const navigate = useNavigate();
  const detail = useQuery(
    sessionQuery(org, project, sessionId, {
      includeEval: search.eval === 1,
      includeExperiments: search.exp === 1,
    }),
  );
  const [span, setSpan] = useState<{ traceId?: string; spanId?: string }>({});
  const [scrollTarget, setScrollTarget] = useState<{ index: number }>();
  const traceId = search.trace;
  const spanId = span.traceId === traceId ? span.spanId : undefined;

  if (detail.isPending) {
    return (
      <div className="flex flex-col gap-2 p-3">
        <Skeleton className="h-4 w-1/2" />
        <Skeleton className="h-4 w-2/3" />
      </div>
    );
  }
  if (detail.isError) {
    return (
      <p className="p-3 text-sm">
        {problemOf(detail.error)?.title ?? "Could not load this session"}
      </p>
    );
  }

  const { session, turns, truncated } = detail.data;
  function selectTurn(next: string) {
    void navigate({
      to: "/$org/$project/sessions/$sessionId",
      params: { org, project, sessionId },
      search: (prev: SessionPageSearch) => ({ ...prev, trace: next }),
      replace: true,
      // Keeps the history state, which carries the way back to where the session was opened from.
      state: true,
    });
  }

  const failedIndex = turns.findIndex((turn) => turn.trace.status === "error");
  function showFirstError() {
    const turn = turns[failedIndex];
    if (!turn) {
      return;
    }
    setScrollTarget({ index: failedIndex });
    selectTurn(turn.trace.trace_id);
  }

  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <SessionHeader
        session={session}
        onShowErrors={failedIndex >= 0 ? showFirstError : undefined}
      />
      {truncated ? (
        <div className="shrink-0 border-b bg-muted px-3 py-1 text-xs">
          Showing the first 1 000 turns
        </div>
      ) : null}
      <div className="relative min-h-0 flex-1">
        <div className="absolute inset-0">
          <SplitPane
            name="session"
            start={
              <TurnList
                turns={turns}
                selectedTraceId={traceId}
                scrollTarget={scrollTarget}
                onSelect={selectTurn}
                projectRef={ref}
              />
            }
            end={
              traceId ? (
                <TraceView
                  key={traceId}
                  stacked
                  traceId={traceId}
                  spanId={spanId}
                  onSelectSpan={(next) => setSpan({ traceId, spanId: next })}
                  onShowSummary={() => setSpan({ traceId })}
                  onClose={() => {
                    return navigate({
                      to: "/$org/$project/sessions/$sessionId",
                      params: { org, project, sessionId },
                      search: (prev: SessionPageSearch) => {
                        const { trace: _trace, ...rest } = prev;
                        return rest;
                      },
                      replace: true,
                      state: true,
                    });
                  }}
                />
              ) : (
                <p className="p-3 text-xs text-muted-foreground">Select a turn</p>
              )
            }
          />
        </div>
      </div>
    </div>
  );
}

function SessionHeader({
  session,
  onShowErrors,
}: {
  session: Schema<"SessionSummary">;
  /** Selects the first failed turn. Absent when no turn failed. */
  onShowErrors?: () => void;
}) {
  const { org, project, data } = useProject();
  const [annotating, setAnnotating] = useState(false);
  const canAnnotateSession = canAnnotate(data.role);
  const ret = useListReturn("sessions");
  const backTo = useBackTo();
  const router = useRouter();
  return (
    <header className="flex shrink-0 flex-col gap-2 border-b px-4 py-3">
      <div className="flex min-w-0 flex-wrap items-center gap-2">
        {backTo ? (
          <a
            href={backTo.href}
            aria-label={`Back to ${backTo.label}`}
            onClick={(event) => {
              // Modified and non-primary clicks keep the browser's own behaviour, such as a new tab.
              if (
                event.button !== 0 ||
                event.metaKey ||
                event.ctrlKey ||
                event.shiftKey ||
                event.altKey
              ) {
                return;
              }
              event.preventDefault();
              router.history.push(backTo.href);
            }}
            className="focus-ring -ml-1 inline-flex items-center gap-0.5 rounded-sm px-1 text-[12px] text-muted-foreground hover:bg-row-hover hover:text-foreground"
          >
            <ChevronLeft aria-hidden="true" className="size-3.5" />
            {backTo.label}
          </a>
        ) : (
          <Link
            to="/$org/$project/sessions"
            params={{ org, project }}
            search={parseSessionsSearch(ret?.search ?? {})}
            state={(current) => ({
              ...current,
              listRestore: ret
                ? { anchorId: ret.anchorId, anchorOffset: ret.anchorOffset, openedId: ret.openedId }
                : undefined,
            })}
            aria-label="Back to sessions"
            className="focus-ring -ml-1 inline-flex items-center gap-0.5 rounded-sm px-1 text-[12px] text-muted-foreground hover:bg-row-hover hover:text-foreground"
          >
            <ChevronLeft aria-hidden="true" className="size-3.5" />
            Sessions
          </Link>
        )}
        <span aria-hidden="true" className="text-muted-foreground/60">
          /
        </span>
        <span className="min-w-0 truncate text-[15px] font-semibold select-text">
          {session.session_id}
        </span>
        <CopyButton value={session.session_id} label="session ID" />
        {session.user_id ? (
          <Link
            to="/$org/$project/users/$userId"
            params={{ org, project, userId: session.user_id }}
            className="focus-ring rounded-sm font-mono text-[12px] underline-offset-2 hover:underline"
          >
            {session.user_id}
          </Link>
        ) : null}
        <div className="ml-auto flex shrink-0 items-center">
          <DisabledReason reason={canAnnotateSession ? undefined : "Only members can annotate"}>
            <Button
              type="button"
              variant="outline"
              size="xs"
              disabled={!canAnnotateSession}
              onClick={() => {
                setAnnotating(true);
              }}
            >
              Annotate
            </Button>
          </DisabledReason>
        </div>
      </div>
      <div className="flex flex-wrap items-center gap-2 text-[12px] text-muted-foreground">
        <MetricChip icon={Clock} label="Time span">
          <TimeText value={session.start_time} style="datetime" />
          <span aria-hidden="true"> → </span>
          <TimeText value={session.end_time} style="datetime" />
        </MetricChip>
        <MetricChip icon={MessagesSquare} label="Turns">
          {session.trace_count} turns
        </MetricChip>
        {session.total_tokens !== undefined ? (
          <MetricChip icon={Coins} label="Tokens">
            {formatTokens(session.total_tokens)}
          </MetricChip>
        ) : null}
        {session.cost_total !== undefined ? (
          <MetricChip icon={DollarSign} label="Cost">
            {formatCost(session.cost_total)}
          </MetricChip>
        ) : null}
        {session.error_count > 0 ? (
          <button
            type="button"
            className="focus-ring inline-flex items-center gap-1 rounded-md border border-status-error/40 px-1.5 py-0.5 text-[12px] text-status-error disabled:opacity-50"
            disabled={!onShowErrors}
            onClick={onShowErrors}
          >
            <TriangleAlert aria-hidden="true" className="size-3.5" />
            <span className="num">{session.error_count} errors</span>
          </button>
        ) : null}
        {session.tags.map((tag) => (
          <span
            key={tag}
            className={cn(
              "inline-flex rounded-sm border px-1.5 text-[11px]",
              categoryClasses(tag).pill,
            )}
          >
            {tag}
          </span>
        ))}
        {[...session.scores]
          .sort((left, right) => left.name.localeCompare(right.name))
          .map((score) => (
            <Badge key={score.score_id} variant="secondary" className="h-4 px-1.5 text-[11px]">
              {score.name}{" "}
              <ScoreBadge name={score.name} value={score.value} text={badgeValue(score.value)} />
            </Badge>
          ))}
      </div>
      <Sheet open={annotating} onOpenChange={setAnnotating}>
        <SheetContent side="right" className="overflow-y-auto">
          <SheetHeader>
            <SheetTitle>Annotate session</SheetTitle>
            <SheetDescription>{session.session_id}</SheetDescription>
          </SheetHeader>
          <div className="px-4 pb-4">
            <AnnotationForm
              key={targetKey({ type: "session", sessionId: session.session_id })}
              target={{ type: "session", sessionId: session.session_id }}
              context={{ userId: session.user_id }}
            />
          </div>
        </SheetContent>
      </Sheet>
    </header>
  );
}

function TurnList({
  turns,
  selectedTraceId,
  scrollTarget,
  onSelect,
  projectRef,
}: {
  turns: readonly Schema<"SessionTurn">[];
  selectedTraceId?: string;
  /** Scrolls the list to this turn each time the object changes. */
  scrollTarget?: { index: number };
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
    if (scrollTarget) {
      virtualizer.scrollToIndex(scrollTarget.index, { align: "center" });
    }
  }, [scrollTarget, virtualizer]);

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
      <div className="flex items-center gap-2 text-[12px] text-muted-foreground">
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
              <Badge key={score.score_id} variant="secondary" className="h-4 px-1.5 text-[11px]">
                {score.name}{" "}
                <ScoreBadge name={score.name} value={score.value} text={badgeValue(score.value)} />
              </Badge>
            ))}
          <StatusDot status={trace.status} />
          {failed ? (
            <span className="shrink-0 rounded-sm bg-status-error/12 px-1.5 text-[11px] font-medium text-status-error">
              Failed
            </span>
          ) : null}
        </button>
        {tools > 0 ? (
          <span className="inline-flex shrink-0 items-center gap-1 text-[11px]">
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
    return <p className="text-[12px] text-muted-foreground">No root input/output</p>;
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
        "max-w-[85%] rounded-lg px-3 py-2 text-[13px]",
        speaker === "user" ? "ml-auto bg-muted" : "mr-auto border bg-card",
      )}
    >
      <ShowMore>
        {bubble.kind === "markdown" ? (
          <Markdown text={shown} />
        ) : (
          <pre className="font-mono text-[12px] break-words whitespace-pre-wrap">{shown}</pre>
        )}
      </ShowMore>
      {truncated ? (
        <p className="mt-1 text-[11px] text-muted-foreground">Truncated, see details</p>
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
    return <p className="text-xs text-muted-foreground">No root input/output</p>;
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

function badgeValue(value: Schema<"ScoreValue">): string {
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
