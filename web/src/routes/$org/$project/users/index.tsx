import { useInfiniteQuery, useQuery } from "@tanstack/react-query";
import { createFileRoute } from "@tanstack/react-router";
import { Users } from "lucide-react";
import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import { problemOf } from "@/api/problem";
import { metricsQuery, userListOptions } from "@/api/queries";
import type { Schema } from "@/api/types";
import { useShortcut } from "@/components/command/use-shortcut";
import { ColumnChooser } from "@/components/data-table/column-chooser";
import {
  lockedColumnIds,
  readColumnChoice,
  sameColumnIds,
  visibleColumnIds,
  writeColumnChoice,
} from "@/components/data-table/column-layout";
import { DataTable } from "@/components/data-table/data-table";
import { EmptyState } from "@/components/empty-state";
import { FilterBar } from "@/components/filter-bar/filter-bar";
import { filterMessage } from "@/components/filter-bar/logic";
import { TraceLayoutControls } from "@/components/layout-switcher";
import { ListToolbar } from "@/components/list-toolbar";
import { useDefaultSavedView } from "@/components/saved-views/default-view";
import { SavedViewsMenu } from "@/components/saved-views/saved-views-menu";
import { useProject } from "@/components/shell/project-context";
import { Filmstrip } from "@/components/trace/filmstrip";
import { SplitPane } from "@/components/trace/split-pane";
import { TooltipProvider } from "@/components/ui/tooltip";
import { UserPanel } from "@/components/user/user-panel";
import { UsersSummary } from "@/components/user/users-summary";
import { orderedUserColumns, userColumnSpecs, userScoreNames } from "@/components/user-columns";
import { formatCost } from "@/lib/format";
import { getTraceMaximized, setTraceMaximized, useListLayout } from "@/lib/list-layout";
import { parseUsersSearch, type UsersSearch } from "@/lib/search/users";
import { type RangeState, rangeFromSearch, resolveRange } from "@/lib/time-range";

const TABLE_ID = "users";
const FILTER_PLACEHOLDER = "Filter traces, e.g. environment = 'production'";

const SORT_COLUMN: Record<Schema<"UserSort">, string> = {
  last_seen: "last_seen",
  first_seen: "first_seen",
  trace_count: "traces",
  session_count: "sessions",
  traces_per_session: "traces_per_session",
  total_tokens: "tokens",
  cost_total: "cost",
  error_count: "errors",
  error_rate: "error_rate",
};

const COLUMN_SORT: Record<string, Schema<"UserSort">> = {
  last_seen: "last_seen",
  first_seen: "first_seen",
  traces: "trace_count",
  sessions: "session_count",
  traces_per_session: "traces_per_session",
  tokens: "total_tokens",
  cost: "cost_total",
  errors: "error_count",
  error_rate: "error_rate",
};

export const Route = createFileRoute("/$org/$project/users/")({
  validateSearch: (search) => parseUsersSearch(search),
  component: UsersPage,
});

function UsersPage() {
  const { org, project } = useProject();
  const search = Route.useSearch();
  const navigate = Route.useNavigate();
  const applyView = useCallback(
    (next: Record<string, unknown>) => {
      void navigate({
        to: "/$org/$project/users",
        params: { org, project },
        search: () => parseUsersSearch(next),
      });
    },
    [navigate, org, project],
  );
  const applyDefault = useCallback(
    (next: Record<string, unknown>) => {
      void navigate({
        to: "/$org/$project/users",
        params: { org, project },
        search: () => parseUsersSearch(next),
        replace: true,
      });
    },
    [navigate, org, project],
  );
  const { hold } = useDefaultSavedView("users", search, applyDefault);
  const range = rangeFromSearch(search);
  const rangeKey = range.kind === "preset" ? range.preset : `${range.from}\n${range.to}`;
  const [anchor, setAnchor] = useState(() => Date.now());
  const seenRange = useRef(rangeKey);
  useEffect(() => {
    if (seenRange.current === rangeKey) {
      return;
    }
    seenRange.current = rangeKey;
    setAnchor(Date.now());
  }, [rangeKey]);
  const resolved = resolveRange(range, anchor);
  const list = useInfiniteQuery({
    ...userListOptions(org, project, {
      from: resolved.from,
      to: resolved.to,
      sort: search.sort,
      order: search.order,
      filter: search.filter,
      includeEval: search.eval === 1,
      includeExperiments: search.exp === 1,
    }),
    enabled: !hold,
  });
  const problem = list.isError ? problemOf(list.error) : undefined;
  const filterError = problem?.code === "invalid_filter" ? problem : undefined;
  const metrics = useQuery({
    ...metricsQuery(org, project, {
      from: resolved.from,
      to: resolved.to,
      filter: search.filter,
      includeEval: search.eval === 1,
      includeExperiments: search.exp === 1,
    }),
    enabled: !hold && !filterError,
  });
  const fetchNextPage = useCallback(() => {
    void list.fetchNextPage();
  }, [list.fetchNextPage]);
  const rows = useMemo(() => list.data?.pages.flatMap((page) => page.data) ?? [], [list.data]);
  const specs = useMemo(() => userColumnSpecs(userScoreNames(rows)), [rows]);
  const lockedIds = useMemo(() => lockedColumnIds(specs), [specs]);
  const defaults = useMemo(() => visibleColumnIds(specs, undefined), [specs]);
  const visible = useMemo(() => {
    const fromUrl = search.cols?.split(",");
    return visibleColumnIds(specs, fromUrl ?? readColumnChoice(TABLE_ID));
  }, [search.cols, specs]);
  const tableColumns = useMemo(() => orderedUserColumns(rows, visible), [rows, visible]);

  function onColumns(ids: string[]) {
    const choice = sameColumnIds(ids, defaults) ? undefined : ids;
    writeColumnChoice(TABLE_ID, choice);
    void navigate({
      to: "/$org/$project/users",
      params: { org, project },
      search: (prev) => withCols(prev, choice?.join(",")),
    });
  }

  const layout = useListLayout();
  const userOpen = search.user !== undefined;
  const listScroll = useRef(0);
  const wasOpen = useRef(userOpen);

  useLayoutEffect(() => {
    const node = document.querySelector<HTMLElement>(
      '[aria-label="Users"] [data-scroll-container]',
    );
    if (node && wasOpen.current !== userOpen) {
      node.scrollTop = listScroll.current;
      node.dispatchEvent(new Event("scroll"));
      wasOpen.current = userOpen;
    }
    return () => {
      if (node) {
        listScroll.current = node.scrollTop;
      }
    };
  }, [userOpen]);

  useShortcut("dismiss", () => {
    if (getTraceMaximized()) {
      setTraceMaximized(false);
      return;
    }
    if (!search.user) {
      return;
    }
    void navigate({
      to: "/$org/$project/users",
      params: { org, project },
      search: (prev) => withoutUser(prev),
    });
  });

  if (problem && !filterError) {
    throw list.error;
  }

  return (
    <div className="relative h-full min-h-0 flex-1">
      <div className="absolute inset-0">
        <SplitPane
          name="users"
          mode={layout}
          filmstrip={
            <Filmstrip
              items={rows.map((row) => ({
                id: row.user_id,
                label: row.user_id,
                detail: formatCost(row.cost_total),
                status: row.error_trace_count > 0 ? "error" : "ok",
              }))}
              selectedId={search.user}
              onSelect={(id) => {
                void navigate({
                  to: "/$org/$project/users",
                  params: { org, project },
                  search: (prev) => withUser(prev, id),
                });
              }}
              onBack={() => {
                void navigate({
                  to: "/$org/$project/users",
                  params: { org, project },
                  search: (prev) => withoutUser(prev),
                });
              }}
            />
          }
          open={userOpen}
          start={
            <div className="flex h-full min-h-0 flex-col">
              <TooltipProvider>
                <ListToolbar
                  title="Users"
                  idPrefix="users"
                  range={range}
                  onRange={(next) => {
                    seenRange.current =
                      next.kind === "preset" ? next.preset : `${next.from}\n${next.to}`;
                    setAnchor(Date.now());
                    void navigate({
                      to: "/$org/$project/users",
                      params: { org, project },
                      search: (prev) => withRange(prev, next),
                    });
                  }}
                  onRefresh={() => setAnchor(Date.now())}
                  evalOn={search.eval === 1}
                  onEval={(on) => {
                    void navigate({
                      to: "/$org/$project/users",
                      params: { org, project },
                      search: (prev) => withFlag(prev, "eval", on),
                    });
                  }}
                  expOn={search.exp === 1}
                  onExp={(on) => {
                    void navigate({
                      to: "/$org/$project/users",
                      params: { org, project },
                      search: (prev) => withFlag(prev, "exp", on),
                    });
                  }}
                  filter={
                    <FilterBar
                      view="traces"
                      placeholder={FILTER_PLACEHOLDER}
                      value={search.filter ?? ""}
                      error={filterError}
                      onApply={(text) => {
                        void navigate({
                          to: "/$org/$project/users",
                          params: { org, project },
                          search: (prev) => withFilter(prev, text),
                        });
                      }}
                    />
                  }
                  columns={
                    <ColumnChooser columns={specs} visible={visible} onVisible={onColumns} />
                  }
                  views={<SavedViewsMenu kind="users" search={search} onApply={applyView} />}
                  loaded={list.data ? rows.length : undefined}
                />
                {filterError ? (
                  <div className="mx-auto flex max-w-md flex-col gap-2 px-4 py-16">
                    <h2 className="text-sm font-medium">{filterError.title}</h2>
                    <p className="text-sm text-destructive">{filterMessage(filterError)}</p>
                  </div>
                ) : !hold && list.data ? (
                  <>
                    <UsersSummary
                      kpis={metrics.data?.kpis}
                      users={rows.length}
                      more={list.hasNextPage}
                    />
                    <DataTable
                      tableId="users"
                      ariaLabel="Users"
                      onReorder={onColumns}
                      lockedColumnIds={lockedIds}
                      columns={tableColumns}
                      rows={rows}
                      getRowId={(row) => row.user_id}
                      selectedRowId={search.user}
                      onActivate={(row) => {
                        void navigate({
                          to: "/$org/$project/users",
                          params: { org, project },
                          search: (prev) => withUser(prev, row.user_id),
                        });
                      }}
                      onSelectionMove={(row) => {
                        void navigate({
                          to: "/$org/$project/users",
                          params: { org, project },
                          search: (prev) => withUser(prev, row.user_id),
                          replace: true,
                        });
                      }}
                      hasNextPage={list.hasNextPage}
                      isFetchingNextPage={list.isFetchingNextPage}
                      fetchNextPage={fetchNextPage}
                      sorting={{
                        id: SORT_COLUMN[search.sort ?? "last_seen"],
                        desc: (search.order ?? "desc") === "desc",
                      }}
                      onSortingChange={(next) => {
                        void navigate({
                          to: "/$org/$project/users",
                          params: { org, project },
                          search: (prev) => withSort(prev, next),
                        });
                      }}
                      emptyState={<EmptyState icon={Users} title="No users" />}
                    />
                  </>
                ) : null}
              </TooltipProvider>
            </div>
          }
          end={
            search.user ? (
              <UserPanel
                userId={search.user}
                row={rows.find((row) => row.user_id === search.user)}
                includeEval={search.eval === 1}
                includeExperiments={search.exp === 1}
                layout={<TraceLayoutControls />}
                onClose={() => {
                  void navigate({
                    to: "/$org/$project/users",
                    params: { org, project },
                    search: (prev) => withoutUser(prev),
                  });
                }}
              />
            ) : null
          }
        />
      </div>
    </div>
  );
}

function withRange(search: UsersSearch, state: RangeState): UsersSearch {
  const { range: _range, from: _from, to: _to, ...rest } = search;
  if (state.kind === "preset") {
    return { ...rest, range: state.preset };
  }
  return { ...rest, from: state.from, to: state.to };
}

function withFilter(search: UsersSearch, value: string): UsersSearch {
  const { filter: _filter, ...rest } = search;
  const trimmed = value.trim();
  return trimmed.length > 0 ? { ...rest, filter: trimmed } : rest;
}

function withFlag(search: UsersSearch, key: "eval" | "exp", on: boolean): UsersSearch {
  const { [key]: _flag, ...rest } = search;
  return on ? { ...rest, [key]: 1 } : rest;
}

function withCols(search: UsersSearch, cols: string | undefined): UsersSearch {
  const { cols: _cols, ...rest } = search;
  return cols ? { ...rest, cols } : rest;
}

function withUser(search: UsersSearch, userId: string): UsersSearch {
  return { ...search, user: userId };
}

function withoutUser(search: UsersSearch): UsersSearch {
  const { user: _user, ...rest } = search;
  return rest;
}

function withSort(search: UsersSearch, next: { id: string; desc: boolean } | null): UsersSearch {
  const { sort: _sort, order: _order, ...rest } = search;
  if (!next) {
    return rest;
  }
  const sort = COLUMN_SORT[next.id];
  if (!sort) {
    return rest;
  }
  if (sort === "last_seen" && next.desc) {
    return rest;
  }
  return { ...rest, sort, order: next.desc ? "desc" : "asc" };
}
