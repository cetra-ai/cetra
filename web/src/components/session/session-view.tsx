import { useQuery } from "@tanstack/react-query";
import { Link, useNavigate } from "@tanstack/react-router";
import { cn } from "cn";
import {
  ChevronLeft,
  Clock,
  Coins,
  DollarSign,
  MessagesSquare,
  TriangleAlert,
} from "lucide-react";
import { useState } from "react";
import { problemOf } from "@/api/problem";
import { sessionQuery } from "@/api/queries";
import type { Schema } from "@/api/types";
import { AddToQueueButton } from "@/components/annotation-queues/add-to-queue-dialog";
import { QueuesChip } from "@/components/annotation-queues/queues-chip";
import { AnnotationForm } from "@/components/annotation/annotation-form";
import { BACK_LINK_CLASS, BackLink } from "@/components/back-link";
import { CopyButton } from "@/components/copy-button";
import { DisabledReason } from "@/components/disabled-reason";
import { ScoreBadge } from "@/components/score-badge";
import { badgeValue, SessionConversation } from "@/components/session/session-conversation";
import { useProject } from "@/components/shell/project-context";
import { TimeText } from "@/components/time-text";
import { MetricChip } from "@/components/trace/metric-chip";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
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
import { formatCost, formatTokens } from "@/lib/format";
import { useListReturn } from "@/lib/list-return";
import { canAnnotate } from "@/lib/roles";
import { parseSessionsSearch, type SessionPageSearch } from "@/lib/search/sessions";

export function SessionView({
  sessionId,
  search,
}: {
  sessionId: string;
  search: SessionPageSearch;
}) {
  const { org, project } = useProject();
  const navigate = useNavigate();
  const detail = useQuery(
    sessionQuery(org, project, sessionId, {
      includeEval: search.eval === 1,
      includeExperiments: search.exp === 1,
    }),
  );
  // The index of the turn to scroll to, or null. The conversation clears it once it has scrolled.
  const [scrollTarget, setScrollTarget] = useState<number | null>(null);

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
      <p className="p-3 text-body">
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
    setScrollTarget(failedIndex);
    selectTurn(turn.trace.trace_id);
  }

  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <SessionHeader
        session={session}
        onShowErrors={failedIndex >= 0 ? showFirstError : undefined}
      />
      {truncated ? (
        <div className="shrink-0 border-b bg-muted px-3 py-1 text-caption">
          Showing the first 1 000 turns
        </div>
      ) : null}
      <SessionConversation
        turns={turns}
        trace={search.trace}
        onSelectTrace={selectTurn}
        scrollTarget={scrollTarget}
        onScrolled={() => {
          setScrollTarget(null);
        }}
        onCloseTrace={() => {
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
  return (
    <header className="flex shrink-0 flex-col gap-2 border-b px-4 py-3">
      <div className="flex min-w-0 flex-wrap items-center gap-2">
        <BackLink
          fallback={
            <Link
              to="/$org/$project/sessions"
              params={{ org, project }}
              search={parseSessionsSearch(ret?.search ?? {})}
              state={(current) => ({
                ...current,
                listRestore: ret
                  ? {
                      anchorId: ret.anchorId,
                      anchorOffset: ret.anchorOffset,
                      openedId: ret.openedId,
                    }
                  : undefined,
              })}
              aria-label="Back to sessions"
              className={BACK_LINK_CLASS}
            >
              <ChevronLeft aria-hidden="true" className="size-3.5" />
              Sessions
            </Link>
          }
        />
        <span aria-hidden="true" className="text-muted-foreground/60">
          /
        </span>
        <span className="min-w-0 truncate text-title font-semibold select-text">
          {session.session_id}
        </span>
        <CopyButton value={session.session_id} label="session ID" />
        {session.user_id ? (
          <Link
            to="/$org/$project/users/$userId"
            params={{ org, project, userId: session.user_id }}
            className="focus-ring rounded-sm font-mono text-body underline-offset-2 hover:underline"
          >
            {session.user_id}
          </Link>
        ) : null}
        <div className="ml-auto flex shrink-0 items-center gap-2">
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
          <AddToQueueButton target="session" items={[{ session_id: session.session_id }]} />
        </div>
      </div>
      <div className="flex flex-wrap items-center gap-2 text-body text-muted-foreground">
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
            className="focus-ring inline-flex items-center gap-1 rounded-md border border-status-error/40 px-1.5 py-0.5 text-body text-status-error disabled:opacity-50"
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
              "inline-flex rounded-sm border px-1.5 text-caption",
              categoryClasses(tag).pill,
            )}
          >
            {tag}
          </span>
        ))}
        {[...session.scores]
          .sort((left, right) => left.name.localeCompare(right.name))
          .map((score) => (
            <Badge key={score.score_id} variant="secondary" className="h-4 px-1.5 text-caption">
              {score.name}{" "}
              <ScoreBadge name={score.name} value={score.value} text={badgeValue(score.value)} />
            </Badge>
          ))}
        <QueuesChip sessionId={session.session_id} />
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

