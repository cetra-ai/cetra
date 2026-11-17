import { keepPreviousData, useQuery } from "@tanstack/react-query";
import { Link } from "@tanstack/react-router";
import { cn } from "cn";
import { X } from "lucide-react";
import type { ReactNode } from "react";
import { problemOf } from "@/api/problem";
import { userQuery } from "@/api/queries";
import type { Schema } from "@/api/types";
import { IdText } from "@/components/id-text";
import { useProject } from "@/components/shell/project-context";
import { TimeText } from "@/components/time-text";
import { Button } from "@/components/ui/button";
import { Skeleton } from "@/components/ui/skeleton";
import { Sparkline } from "@/components/user/sparkline";
import { decodeValue, scoreText } from "@/components/user-columns";
import { formatCost, formatTokens } from "@/lib/format";

const RECENT_SESSIONS = 20;

/** The user drawer beside the users list: totals, activity, scores, metadata and recent sessions. */
export function UserPanel({
  userId,
  row,
  includeEval,
  includeExperiments,
  layout,
  onClose,
}: {
  userId: string;
  row?: Schema<"EndUserSummary">;
  includeEval: boolean;
  includeExperiments: boolean;
  layout?: ReactNode;
  onClose: () => void;
}) {
  const { org, project } = useProject();
  const detail = useQuery({
    ...userQuery(org, project, userId, { includeEval, includeExperiments }),
    placeholderData: keepPreviousData,
  });
  // Placeholder data may belong to the previously selected user: only trust it for this one.
  const fetched = detail.data?.user.user_id === userId ? detail.data : undefined;
  const user = row ?? fetched?.user;
  const sessions = fetched?.sessions ?? [];
  const pageSearch = {
    ...(includeEval ? { eval: 1 as const } : {}),
    ...(includeExperiments ? { exp: 1 as const } : {}),
  };

  return (
    <section aria-label="User" className="flex h-full min-h-0 flex-col">
      <header className="flex shrink-0 items-center gap-2 border-b px-3 py-1.5 text-xs">
        <IdText value={userId} kind="user" />
        <div className="ml-auto flex shrink-0 items-center gap-1">
          {layout}
          <Button variant="ghost" size="xs" asChild>
            <Link
              to="/$org/$project/users/$userId"
              params={{ org, project, userId }}
              search={pageSearch}
            >
              Open page
            </Link>
          </Button>
          <Button type="button" variant="ghost" size="icon-xs" aria-label="Close" onClick={onClose}>
            <X />
          </Button>
        </div>
      </header>
      <div className="min-h-0 flex-1 overflow-auto p-3 text-xs">
        {user ? (
          <div className="flex flex-col gap-4">
            <Section title="Totals">
              <dl className="grid grid-cols-2 gap-x-4 gap-y-1">
                <Stat label="Traces" value={String(user.trace_count)} />
                <Stat label="Sessions" value={String(user.session_count)} />
                <Stat
                  label="Traces / session"
                  value={
                    user.session_count > 0
                      ? (user.trace_count / user.session_count).toFixed(1)
                      : "—"
                  }
                />
                <Stat
                  label="Error rate"
                  value={
                    user.trace_count > 0
                      ? `${Math.round((user.error_trace_count / user.trace_count) * 100)}%`
                      : "—"
                  }
                />
                <Stat label="Tokens" value={formatTokens(user.total_tokens)} />
                <Stat label="Cost" value={formatCost(user.cost_total)} />
              </dl>
              <p className="text-muted-foreground">
                <TimeText value={user.first_seen} style="datetime" />
                <span aria-hidden="true"> → </span>
                <TimeText value={user.last_seen} style="datetime" />
              </p>
            </Section>
            <Section title="Activity">
              <Sparkline values={user.activity} label="Activity" className="h-12 w-full" />
            </Section>
            <Section title="Scores">
              {user.scores.length === 0 ? (
                <p className="text-muted-foreground">No scores</p>
              ) : (
                <ul className="flex flex-col gap-1">
                  {user.scores.map((score) => (
                    <li key={`${score.name}:${score.data_type}`} className="flex gap-2">
                      <span className="min-w-0 truncate">{score.name}</span>
                      <span className="num">{scoreText(score)}</span>
                      <span className="num text-muted-foreground">{score.count}</span>
                    </li>
                  ))}
                </ul>
              )}
            </Section>
            <Section title="Metadata">
              {Object.keys(user.metadata ?? {}).length === 0 ? (
                <p className="text-muted-foreground">No metadata</p>
              ) : (
                <dl className="grid grid-cols-[auto_1fr] gap-x-4 gap-y-1">
                  {Object.entries(user.metadata ?? {}).map(([key, value]) => (
                    <div key={key} className="contents">
                      <dt className="text-muted-foreground">{key}</dt>
                      <dd className="min-w-0 break-words">{decodeValue(value)}</dd>
                    </div>
                  ))}
                </dl>
              )}
            </Section>
            <Section title="Recent sessions">
              {sessions.length === 0 ? (
                <p className="text-muted-foreground">No sessions</p>
              ) : (
                <ul className="flex flex-col gap-1">
                  {sessions.slice(0, RECENT_SESSIONS).map((session) => (
                    <li key={session.session_id}>
                      <Link
                        to="/$org/$project/sessions/$sessionId"
                        params={{ org, project, sessionId: session.session_id }}
                        className={cn(
                          "focus-ring flex items-center gap-3 rounded-sm underline-offset-2 hover:underline",
                        )}
                      >
                        <TimeText value={session.start_time} style="datetime" />
                        <span className="num text-muted-foreground">
                          {session.trace_count} traces
                        </span>
                        <span className="num">{formatCost(session.cost_total)}</span>
                      </Link>
                    </li>
                  ))}
                </ul>
              )}
            </Section>
          </div>
        ) : detail.isError ? (
          <p className="text-destructive">
            {problemOf(detail.error)?.title ?? "Could not load this user."}
          </p>
        ) : (
          <div className="flex flex-col gap-2">
            <Skeleton className="h-6 w-full" />
            <Skeleton className="h-6 w-full" />
            <Skeleton className="h-6 w-full" />
          </div>
        )}
      </div>
    </section>
  );
}

function Section({ title, children }: { title: string; children: ReactNode }) {
  return (
    <section className="flex flex-col gap-1.5">
      <h3 className="text-[10.5px] text-muted-foreground uppercase">{title}</h3>
      {children}
    </section>
  );
}

function Stat({ label, value }: { label: string; value: string }) {
  return (
    <>
      <dt className="text-muted-foreground">{label}</dt>
      <dd className="num text-right">{value}</dd>
    </>
  );
}
