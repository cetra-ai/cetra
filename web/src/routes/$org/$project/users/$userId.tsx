import { keepPreviousData, useQuery, useQueryClient } from "@tanstack/react-query";
import { createFileRoute, Link } from "@tanstack/react-router";
import { ChevronLeft, Clock, LayoutDashboard, MessagesSquare, Trash2, Users } from "lucide-react";
import { useEffect, useMemo, useRef, useState } from "react";
import { problemOf } from "@/api/problem";
import { userQuery } from "@/api/queries";
import type { Schema } from "@/api/types";
import { CopyButton } from "@/components/copy-button";
import { visibleColumnIds } from "@/components/data-table/column-layout";
import { DataTable } from "@/components/data-table/data-table";
import { UserDeletionDialog } from "@/components/deletions/user-deletion-form";
import { DisabledReason } from "@/components/disabled-reason";
import { EmptyState } from "@/components/empty-state";
import {
  orderedSessionColumns,
  sessionColumnSpecs,
  sessionScoreNames,
} from "@/components/session-columns";
import { useProject } from "@/components/shell/project-context";
import { TimeRangePicker } from "@/components/time-range/time-range-picker";
import { TimeText } from "@/components/time-text";
import { MetricChip } from "@/components/trace/metric-chip";
import { Button } from "@/components/ui/button";
import { Skeleton } from "@/components/ui/skeleton";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { TooltipProvider } from "@/components/ui/tooltip";
import { PlanPill, UserChips } from "@/components/user/user-chips";
import { UserOverview } from "@/components/user/user-overview";
import { UserTraces } from "@/components/user/user-traces";
import { invalidateProjectReads } from "@/lib/deletions";
import { useBackState } from "@/lib/list-return";
import { atLeast } from "@/lib/roles";
import {
  parseUserPageSearch,
  rangeParams,
  type UserPageSearch,
  userPageRange,
  userTraceFilter,
} from "@/lib/search/users";
import { type RangeState, resolveRange } from "@/lib/time-range";
import { userIdentity } from "@/lib/user-identity";

export const Route = createFileRoute("/$org/$project/users/$userId")({
  validateSearch: (search) => parseUserPageSearch(search),
  component: UserPage,
});

function UserPage() {
  const { org, project } = useProject();
  const { userId } = Route.useParams();
  const search = Route.useSearch();
  const navigate = Route.useNavigate();
  const range = userPageRange(search);
  const rangeKey = rangeKeyOf(range);
  const [anchor, setAnchor] = useState(() => Date.now());
  const seenRange = useRef(rangeKey);
  // Back and forward move the range without the picker: reset the anchor from the URL.
  useEffect(() => {
    if (seenRange.current === rangeKey) {
      return;
    }
    seenRange.current = rangeKey;
    setAnchor(Date.now());
  }, [rangeKey]);
  const resolved = resolveRange(range, anchor);
  const includeEval = search.eval === 1;
  const includeExperiments = search.exp === 1;
  const detail = useQuery({
    ...userQuery(org, project, userId, {
      from: resolved.from,
      to: resolved.to,
      includeEval,
      includeExperiments,
    }),
    placeholderData: keepPreviousData,
  });
  // Placeholder data may belong to the previously opened user: only trust it for this one.
  const fetched = detail.data?.user.user_id === userId ? detail.data : undefined;
  const problem = detail.isError ? problemOf(detail.error) : undefined;
  const missing = problem?.status === 404;
  const backState = useBackState(userIdentity(fetched?.user.metadata).name ?? userId);
  if (detail.isError && !missing) {
    throw detail.error;
  }
  const sessions = fetched?.sessions ?? [];
  const sessionColumns = useMemo(() => {
    const specs = sessionColumnSpecs(sessionScoreNames(sessions));
    return orderedSessionColumns(
      sessions,
      visibleColumnIds(specs, undefined).filter((id) => id !== "user"),
    );
  }, [sessions]);

  function navigateSearch(next: (prev: UserPageSearch) => UserPageSearch, replace = false) {
    void navigate({
      to: "/$org/$project/users/$userId",
      params: { org, project, userId },
      search: next,
      replace,
    });
  }

  function changeRange(next: RangeState) {
    // As the list does: reset the anchor now, so the new range resolves against it on the first
    // render instead of firing its queries twice.
    seenRange.current = rangeKeyOf(next);
    setAnchor(Date.now());
    navigateSearch((prev) => withRange(prev, next));
  }

  if (missing) {
    return (
      <div className="flex h-full min-h-0 flex-1 flex-col">
        <UserHeader userId={userId} range={range} search={search} onRangeChange={changeRange} />
        <EmptyState icon={Users} title="No traces from this user in this range" />
      </div>
    );
  }

  if (!fetched) {
    return (
      <div className="flex h-full min-h-0 flex-1 flex-col">
        <UserHeader
          userId={userId}
          range={range}
          search={search}
          onRangeChange={changeRange}
          loading
        />
      </div>
    );
  }

  const { user } = fetched;
  const tab = search.tab ?? "overview";

  return (
    <div className="flex h-full min-h-0 flex-1 flex-col">
      <UserHeader
        userId={userId}
        user={user}
        range={range}
        search={search}
        onRangeChange={changeRange}
      />
      <TooltipProvider>
        <Tabs
          value={tab}
          onValueChange={(value) => {
            navigateSearch((prev) => {
              const { tab: _tab, ...rest } = prev;
              return value === "sessions" || value === "traces" ? { ...rest, tab: value } : rest;
            }, true);
          }}
          className="flex min-h-0 flex-1 flex-col"
        >
          <div className="shrink-0 px-4 pt-2">
            <TabsList>
              <TabsTrigger value="overview">Overview</TabsTrigger>
              <TabsTrigger value="sessions">
                Sessions ({sessions.length === 100 ? "100+" : sessions.length})
              </TabsTrigger>
              <TabsTrigger value="traces">Traces</TabsTrigger>
            </TabsList>
          </div>
          <TabsContent value="overview" className="mt-0 flex min-h-0 flex-1 flex-col">
            <UserOverview
              user={user}
              from={resolved.from}
              to={resolved.to}
              includeEval={includeEval}
              includeExperiments={includeExperiments}
            />
          </TabsContent>
          <TabsContent value="sessions" className="mt-0 flex min-h-0 flex-1 flex-col">
            <DataTable
              tableId="user-sessions"
              ariaLabel="Sessions"
              rowTone={(row) => (row.error_trace_count > 0 ? "error" : undefined)}
              columns={sessionColumns}
              rows={sessions}
              getRowId={(row) => row.session_id}
              onActivate={(row) => {
                void navigate({
                  to: "/$org/$project/sessions/$sessionId",
                  params: { org, project, sessionId: row.session_id },
                  search: pageFlags(search),
                  state: backState,
                });
              }}
              hasNextPage={false}
              isFetchingNextPage={false}
              fetchNextPage={() => undefined}
              emptyState={<EmptyState icon={MessagesSquare} title="No sessions" />}
            />
          </TabsContent>
          <TabsContent value="traces" className="mt-0 flex min-h-0 flex-1 flex-col">
            <UserTraces
              userId={userId}
              from={resolved.from}
              to={resolved.to}
              includeEval={includeEval}
              includeExperiments={includeExperiments}
            />
          </TabsContent>
        </Tabs>
      </TooltipProvider>
    </div>
  );
}

/**
 * The page header: the back link, identity, range picker, dashboard link and delete. `user` is
 * absent while loading and when the user has no trace in the range (UP-3); `loading` shows a
 * skeleton in place of the totals.
 */
function UserHeader({
  userId,
  user,
  range,
  search,
  onRangeChange,
  loading = false,
}: {
  userId: string;
  user?: Schema<"EndUserSummary">;
  range: RangeState;
  search: UserPageSearch;
  onRangeChange: (next: RangeState) => void;
  loading?: boolean;
}) {
  const { org, project, data } = useProject();
  const navigate = Route.useNavigate();
  const queryClient = useQueryClient();
  const [deleteOpen, setDeleteOpen] = useState(false);
  const canDelete = atLeast(data.role, "admin");
  const identity = userIdentity(user?.metadata);

  async function afterDelete() {
    // Navigate first: this page's query would refetch a 404 for the deleted user.
    await navigate({ to: "/$org/$project/settings/data", params: { org, project } });
    await invalidateProjectReads(queryClient);
  }

  return (
    <header className="flex shrink-0 flex-col gap-2 border-b px-4 py-3">
      <div className="flex min-w-0 flex-wrap items-center gap-2">
        <Link
          to="/$org/$project/users"
          params={{ org, project }}
          search={{ ...rangeParams(range), ...pageFlags(search) }}
          aria-label="Back to users"
          className="focus-ring -ml-1 inline-flex items-center gap-0.5 rounded-sm px-1 text-body text-muted-foreground hover:bg-row-hover hover:text-foreground"
        >
          <ChevronLeft aria-hidden="true" className="size-3.5" />
          Users
        </Link>
        <span aria-hidden="true" className="text-muted-foreground/60">
          /
        </span>
        {identity.name ? (
          <>
            <span className="min-w-0 truncate text-title font-semibold select-text">
              {identity.name}
            </span>
            <span className="font-mono text-body text-muted-foreground select-text">{userId}</span>
          </>
        ) : (
          <span className="min-w-0 truncate text-title font-semibold select-text">{userId}</span>
        )}
        <CopyButton value={userId} label="user ID" />
        {identity.plan ? <PlanPill plan={identity.plan} /> : null}
        {identity.email ? (
          <span className="text-body text-muted-foreground select-text">{identity.email}</span>
        ) : null}
        <div className="ml-auto flex shrink-0 items-center gap-2">
          <TimeRangePicker state={range} onChange={onRangeChange} />
          <Button variant="ghost" size="xs" asChild>
            <Link
              to="/$org/$project/dashboard"
              params={{ org, project }}
              search={{
                ...rangeParams(range),
                filter: userTraceFilter(userId),
                ...pageFlags(search),
              }}
            >
              <LayoutDashboard />
              Dashboard
            </Link>
          </Button>
          <DisabledReason reason={canDelete ? undefined : "Only project admins can delete data"}>
            <Button
              type="button"
              variant="outline"
              size="xs"
              disabled={!canDelete}
              onClick={() => {
                setDeleteOpen(true);
              }}
            >
              <Trash2 />
              Delete user data…
            </Button>
          </DisabledReason>
        </div>
      </div>
      <UserDeletionDialog
        userId={userId}
        open={deleteOpen}
        onOpenChange={setDeleteOpen}
        onDone={() => {
          void afterDelete();
        }}
      />
      {loading ? (
        <Skeleton className="h-6 w-64" />
      ) : user ? (
        <div className="flex flex-wrap items-center gap-2 text-body text-muted-foreground">
          <MetricChip icon={Clock} label="Time span">
            <TimeText value={user.first_seen} style="datetime" />
            <span aria-hidden="true"> → </span>
            <TimeText value={user.last_seen} style="datetime" />
          </MetricChip>
          <UserChips user={user} />
        </div>
      ) : null}
    </header>
  );
}

function rangeKeyOf(state: RangeState): string {
  return state.kind === "preset" ? state.preset : `${state.from}\n${state.to}`;
}

function withRange(search: UserPageSearch, state: RangeState): UserPageSearch {
  const { range: _range, from: _from, to: _to, ...rest } = search;
  return { ...rest, ...rangeParams(state) };
}

function pageFlags(search: UserPageSearch): { eval?: 1; exp?: 1 } {
  return {
    ...(search.eval ? { eval: 1 as const } : {}),
    ...(search.exp ? { exp: 1 as const } : {}),
  };
}
