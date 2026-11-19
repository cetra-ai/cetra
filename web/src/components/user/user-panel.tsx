import { keepPreviousData, useQuery } from "@tanstack/react-query";
import { Link } from "@tanstack/react-router";
import { cn } from "cn";
import { Activity, Gauge, type LucideIcon, MessagesSquare, Tags, X } from "lucide-react";
import type { ReactNode } from "react";
import { problemOf } from "@/api/problem";
import { userQuery } from "@/api/queries";
import type { Schema } from "@/api/types";
import { IdText } from "@/components/id-text";
import { RelativeTime } from "@/components/relative-time";
import { useProject } from "@/components/shell/project-context";
import { TimeText } from "@/components/time-text";
import { Button } from "@/components/ui/button";
import { Skeleton } from "@/components/ui/skeleton";
import { ActivityChart } from "@/components/user/activity-chart";
import { PlanPill, UserChips, UserMetadata, UserScores } from "@/components/user/user-chips";
import { formatCost, formatDuration } from "@/lib/format";
import { useBackState } from "@/lib/list-return";
import type { Preset } from "@/lib/time-range";
import { userIdentity } from "@/lib/user-identity";

const RECENT_SESSIONS = 10;

/**
 * The user drawer beside the users list: header with identity, metric chips, activity, scores,
 * metadata and recent sessions. `from` and `to` are the list's resolved range; `range` is the
 * list's URL range params, carried to the user page by "Open page".
 */
export function UserPanel({
  userId,
  row,
  from,
  to,
  range,
  includeEval,
  includeExperiments,
  layout,
  onClose,
}: {
  userId: string;
  row?: Schema<"EndUserSummary">;
  from: string;
  to: string;
  range?: { range?: Preset; from?: string; to?: string };
  includeEval: boolean;
  includeExperiments: boolean;
  layout?: ReactNode;
  onClose: () => void;
}) {
  const { org, project } = useProject();
  const detail = useQuery({
    ...userQuery(org, project, userId, { from, to, includeEval, includeExperiments }),
    placeholderData: keepPreviousData,
  });
  // Placeholder data may belong to the previously selected user: only trust it for this one.
  const fetched = detail.data?.user.user_id === userId ? detail.data : undefined;
  // The user's all-time detail: Recent sessions and the total session count ignore the range.
  const allTime = useQuery({
    ...userQuery(org, project, userId, { includeEval, includeExperiments }),
    placeholderData: keepPreviousData,
  });
  const allTimeFetched = allTime.data?.user.user_id === userId ? allTime.data : undefined;
  const user = row ?? fetched?.user;
  const identity = userIdentity(user?.metadata);
  const backState = useBackState(identity.name ?? userId);
  const sessions = allTimeFetched?.sessions ?? [];
  const shownSessions = sessions.slice(0, RECENT_SESSIONS);
  const pageSearch = {
    ...(range ?? {}),
    ...(includeEval ? { eval: 1 as const } : {}),
    ...(includeExperiments ? { exp: 1 as const } : {}),
  };

  return (
    <section aria-label="User" className="flex h-full min-h-0 flex-col">
      <header className="flex shrink-0 items-center gap-2 border-b px-3 py-1.5 text-body">
        {user && identity.name ? (
          <span className="flex min-w-0 items-baseline gap-1.5">
            <span className="truncate text-ui font-semibold">{identity.name}</span>
            <span className="truncate font-mono text-caption text-muted-foreground">{userId}</span>
          </span>
        ) : (
          <IdText value={userId} kind="user" />
        )}
        {identity.plan ? <PlanPill plan={identity.plan} /> : null}
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
      <div className="@container min-h-0 flex-1 overflow-auto p-3 text-body">
        {user ? (
          <div className="flex flex-col gap-3">
            <div className="flex flex-col gap-2">
              {identity.email ? (
                <p className="text-body text-muted-foreground">{identity.email}</p>
              ) : null}
              <UserChips user={user} totalSessions={allTimeFetched?.user.session_count} />
              <p className="text-muted-foreground">
                <TimeText value={user.first_seen} style="datetime" />
                <span aria-hidden="true"> → </span>
                <TimeText value={user.last_seen} style="datetime" />
              </p>
            </div>
            <Section title="Activity" icon={Activity}>
              <ActivityChart values={user.activity} from={from} to={to} />
            </Section>
            <div className="grid grid-cols-1 gap-3 @[700px]:grid-cols-2">
              <Section title="Scores" icon={Gauge}>
                <UserScores scores={user.scores} />
              </Section>
              <Section title="Metadata" icon={Tags}>
                <UserMetadata metadata={user.metadata} />
              </Section>
            </div>
            <Section title="Recent sessions" icon={MessagesSquare}>
              {allTimeFetched === undefined && allTime.isError ? (
                <p className="text-muted-foreground">
                  {problemOf(allTime.error)?.status === 404
                    ? "No traces from this user"
                    : "Could not load the sessions."}
                </p>
              ) : allTimeFetched === undefined ? (
                <Skeleton className="h-10 w-full" />
              ) : sessions.length === 0 ? (
                <p className="text-muted-foreground">No sessions</p>
              ) : (
                <>
                  <ul className="flex flex-col gap-1">
                    {shownSessions.map((session) => (
                      <li key={session.session_id}>
                        <Link
                          to="/$org/$project/sessions/$sessionId"
                          params={{ org, project, sessionId: session.session_id }}
                          state={backState}
                          className={cn(
                            "focus-ring flex min-w-0 flex-col gap-0.5 rounded-md border-l-2 px-2 py-1.5 hover:bg-row-hover",
                            session.error_trace_count > 0
                              ? "border-status-error"
                              : "border-transparent",
                          )}
                        >
                          <span className="flex min-w-0 items-center gap-1.5 text-body">
                            <span
                              aria-hidden="true"
                              title={session.error_trace_count > 0 ? "Has errors" : undefined}
                              className={cn(
                                "size-1.5 shrink-0 rounded-full",
                                session.error_trace_count > 0
                                  ? "bg-status-error"
                                  : "bg-[var(--chart-1)]",
                              )}
                            />
                            {session.first_input_preview ? (
                              <span
                                className="min-w-0 truncate"
                                title={session.first_input_preview}
                              >
                                {session.first_input_preview}
                              </span>
                            ) : (
                              <span className="min-w-0 truncate font-mono">
                                {session.session_id}
                              </span>
                            )}
                            {session.error_trace_count > 0 ? (
                              <span className="sr-only"> (has errors)</span>
                            ) : null}
                          </span>
                          <span className="num flex items-center gap-1.5 text-caption text-muted-foreground">
                            <RelativeTime value={session.start_time} />
                            <span aria-hidden="true">·</span>
                            <span>
                              {session.trace_count} {session.trace_count === 1 ? "trace" : "traces"}
                            </span>
                            <span aria-hidden="true">·</span>
                            <span>{formatCost(session.cost_total)}</span>
                            <span aria-hidden="true">·</span>
                            <span>{formatDuration(session.duration_ns)}</span>
                          </span>
                        </Link>
                      </li>
                    ))}
                  </ul>
                  {allTimeFetched.user.session_count > shownSessions.length ? (
                    <Link
                      to="/$org/$project/users/$userId"
                      params={{ org, project, userId }}
                      search={{ ...pageSearch, tab: "sessions" as const }}
                      className="self-start text-body text-muted-foreground hover:text-foreground"
                    >
                      All sessions →
                    </Link>
                  ) : null}
                </>
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

function Section({
  title,
  icon: Icon,
  children,
}: {
  title: string;
  icon: LucideIcon;
  children: ReactNode;
}) {
  return (
    <section className="flex flex-col gap-2 rounded-md border bg-surface p-3">
      <div className="flex items-center gap-1.5">
        <Icon aria-hidden="true" className="size-3.5 text-muted-foreground" />
        <h3 className="text-caption font-medium tracking-wide text-muted-foreground uppercase">
          {title}
        </h3>
      </div>
      {children}
    </section>
  );
}
