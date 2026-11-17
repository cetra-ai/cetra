import { keepPreviousData, useQuery } from "@tanstack/react-query";
import { Link } from "@tanstack/react-router";
import { X } from "lucide-react";
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
import type { Preset } from "@/lib/time-range";
import { userIdentity } from "@/lib/user-identity";

const RECENT_SESSIONS = 20;

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
  const user = row ?? fetched?.user;
  const identity = userIdentity(user?.metadata);
  const sessions = fetched?.sessions ?? [];
  const pageSearch = {
    ...(range ?? {}),
    ...(includeEval ? { eval: 1 as const } : {}),
    ...(includeExperiments ? { exp: 1 as const } : {}),
  };

  return (
    <section aria-label="User" className="flex h-full min-h-0 flex-col">
      <header className="flex shrink-0 items-center gap-2 border-b px-3 py-1.5 text-xs">
        {user && identity.name ? (
          <span className="flex min-w-0 items-baseline gap-1.5">
            <span className="truncate text-[13px] font-semibold">{identity.name}</span>
            <span className="truncate font-mono text-[11px] text-muted-foreground">{userId}</span>
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
      <div className="min-h-0 flex-1 overflow-auto p-3 text-xs">
        {user ? (
          <div className="flex flex-col gap-4">
            <div className="flex flex-col gap-2">
              {identity.email ? (
                <p className="text-[12px] text-muted-foreground">{identity.email}</p>
              ) : null}
              <UserChips user={user} />
              <p className="text-muted-foreground">
                <TimeText value={user.first_seen} style="datetime" />
                <span aria-hidden="true"> → </span>
                <TimeText value={user.last_seen} style="datetime" />
              </p>
            </div>
            <Section title="Activity">
              <ActivityChart values={user.activity} from={from} to={to} />
            </Section>
            <Section title="Scores">
              <UserScores scores={user.scores} />
            </Section>
            <Section title="Metadata">
              <UserMetadata metadata={user.metadata} />
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
                        className="focus-ring flex min-w-0 flex-col gap-0.5 rounded-sm underline-offset-2 hover:underline"
                      >
                        <span className="flex min-w-0 items-center gap-1.5">
                          {session.error_trace_count > 0 ? (
                            <span
                              aria-hidden="true"
                              title="Has errors"
                              className="size-1.5 shrink-0 rounded-full bg-status-error"
                            />
                          ) : null}
                          {session.first_input_preview ? (
                            <span className="min-w-0 truncate" title={session.first_input_preview}>
                              {session.first_input_preview}
                            </span>
                          ) : (
                            <span className="min-w-0 truncate font-mono">{session.session_id}</span>
                          )}
                          {session.error_trace_count > 0 ? (
                            <span className="sr-only"> (has errors)</span>
                          ) : null}
                        </span>
                        <span className="num flex items-center gap-1.5 text-[11px] text-muted-foreground">
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
