import { useInfiniteQuery } from "@tanstack/react-query";
import { createFileRoute, useLocation, useRouter } from "@tanstack/react-router";
import { MessagesSquare } from "lucide-react";
import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import { problemOf } from "@/api/problem";
import { sessionListOptions } from "@/api/queries";
import type { Schema } from "@/api/types";
import { AddMatchingDialog } from "@/components/annotation-queues/add-matching-dialog";
import {
  AddToQueueDialog,
  addToQueueReason,
  type QueueItemTarget,
} from "@/components/annotation-queues/add-to-queue-dialog";
import { ColumnChooser } from "@/components/data-table/column-chooser";
import {
  lockedColumnIds,
  readColumnChoice,
  sameColumnIds,
  visibleColumnIds,
  writeColumnChoice,
} from "@/components/data-table/column-layout";
import { DataTable } from "@/components/data-table/data-table";
import { countLabel, NewRowsPill, useNewRows } from "@/components/data-table/new-rows";
import { DisabledReason } from "@/components/disabled-reason";
import { EmptyState } from "@/components/empty-state";
import { CarriedNotice } from "@/components/facets/carried-notice";
import {
  FacetPanel,
  type FacetScope,
  FiltersToggle,
  useFacetsOpen,
} from "@/components/facets/facet-panel";
import { PresetChips } from "@/components/facets/preset-chips";
import { FilterBar } from "@/components/filter-bar/filter-bar";
import { filterMessage } from "@/components/filter-bar/logic";
import { ListToolbar } from "@/components/list-toolbar";
import { useDefaultSavedView } from "@/components/saved-views/default-view";
import { SavedViewsMenu } from "@/components/saved-views/saved-views-menu";
import { SessionsStartedChart } from "@/components/session/sessions-started-chart";
import { SessionsSummary } from "@/components/session/sessions-summary";
import {
  orderedSessionColumns,
  sessionColumnSpecs,
  sessionScoreNames,
} from "@/components/session-columns";
import { useProject } from "@/components/shell/project-context";
import { Button } from "@/components/ui/button";
import { TooltipProvider } from "@/components/ui/tooltip";
import { appendFilter } from "@/lib/dsl/dsl";
import {
  anchorKey,
  keepSameProject,
  LIST_CACHE,
  NEW_ROWS_LIMIT,
  useListAnchor,
} from "@/lib/list-anchor";
import { rangeOnly, writeListContext } from "@/lib/list-context";
import {
  anchorOf,
  listReturnFor,
  markReturnPoint,
  type RowAnchor,
  rememberListReturn,
  useRestoredHighlight,
} from "@/lib/list-return";
import { parseSessionsSearch, type SessionsSearch } from "@/lib/search/sessions";
import { type RangeState, rangeFromSearch, resolveRange } from "@/lib/time-range";

const TABLE_ID = "sessions";

const SORT_COLUMN: Record<Schema<"SessionSort">, string> = {
  start_time: "start",
  end_time: "last",
  trace_count: "traces",
  cost_total: "cost",
};

const COLUMN_SORT: Record<string, Schema<"SessionSort">> = {
  start: "start_time",
  last: "end_time",
  traces: "trace_count",
  cost: "cost_total",
};

const NO_SESSIONS: ReadonlySet<string> = new Set();

export const Route = createFileRoute("/$org/$project/sessions/")({
  validateSearch: (search) => parseSessionsSearch(search),
  component: SessionsPage,
});

function SessionsPage() {
  const { org, project, data } = useProject();
  const search = Route.useSearch();
  const navigate = Route.useNavigate();
  const router = useRouter();
  const applyView = useCallback(
    (next: Record<string, unknown>) => {
      void navigate({
        to: "/$org/$project/sessions",
        params: { org, project },
        search: () => parseSessionsSearch(next),
      });
    },
    [navigate, org, project],
  );
  const applyDefault = useCallback(
    (next: Record<string, unknown>) => {
      void navigate({
        to: "/$org/$project/sessions",
        params: { org, project },
        search: () => parseSessionsSearch(next),
        replace: true,
      });
    },
    [navigate, org, project],
  );
  const { hold, carried } = useDefaultSavedView("sessions", search, applyDefault, "sessions");
  useEffect(() => {
    if (!hold) {
      writeListContext(org, project, "sessions", search);
    }
  }, [hold, org, project, search]);
  function clearCarried() {
    void navigate({
      to: "/$org/$project/sessions",
      params: { org, project },
      search: (prev) => rangeOnly(prev),
    });
  }
  const [facetsOpen, setFacetsOpen] = useFacetsOpen();
  const listRestore = useLocation().state.listRestore;
  const restoreAnchor = useMemo(() => anchorOf(listRestore), [listRestore]);
  const restoredHighlight = useRestoredHighlight(listRestore?.openedId);
  const range = rangeFromSearch(search);
  const rangeKey = range.kind === "preset" ? range.preset : `${range.from}\n${range.to}`;
  const windowKey = anchorKey(org, project, "sessions", {
    rangeKey,
    filter: search.filter,
    search: undefined,
    sort: search.sort ?? "start_time",
    order: search.order ?? "desc",
    eval: search.eval === 1,
    exp: search.exp === 1,
  });
  const listAnchor = useListAnchor(windowKey, false);
  const anchor = listAnchor.anchor;
  const windowId = `${windowKey}\n${anchor}`;
  const [restoreAtMount] = useState(() => (listAnchor.reused ? listAnchor.scroll : undefined));
  const lastAnchor = useRef<RowAnchor | undefined>(undefined);
  const onScrollAnchor = useCallback(
    (scroll: RowAnchor) => {
      lastAnchor.current = scroll;
      listAnchor.rememberScroll(scroll);
    },
    [listAnchor.rememberScroll],
  );
  const resolved = resolveRange(range, anchor);
  /** The list's window and flags: the facet panel, its presets and the filter bar fetch with it. */
  const facetScope: FacetScope = {
    from: resolved.from,
    to: resolved.to,
    includeEval: search.eval === 1,
    includeExperiments: search.exp === 1,
  };
  // Checked sessions belong to the list they were checked in: a new filter, range or
  // eval/experiment flag clears them.
  const selectionKey = JSON.stringify([search.filter, rangeKey, search.eval, search.exp]);
  const [selection, setSelection] = useState<{ key: string; ids: ReadonlySet<string> }>({
    key: selectionKey,
    ids: NO_SESSIONS,
  });
  const checkedIds = selection.key === selectionKey ? selection.ids : NO_SESSIONS;
  const queueReason = addToQueueReason(data.role, checkedIds.size);
  const matchingQueueReason = addToQueueReason(data.role, 0);
  const [queueOpen, setQueueOpen] = useState(false);
  const [queueItems, setQueueItems] = useState<readonly QueueItemTarget[]>([]);
  const [matchOpen, setMatchOpen] = useState(false);
  const listOptions = sessionListOptions(org, project, {
    from: resolved.from,
    to: resolved.to,
    sort: search.sort,
    order: search.order,
    filter: search.filter,
    includeEval: search.eval === 1,
    includeExperiments: search.exp === 1,
  });
  const list = useInfiniteQuery({
    ...listOptions,
    ...LIST_CACHE,
    placeholderData: keepSameProject(listOptions.queryKey),
    enabled: !hold,
  });
  // Sessions that arrived since the anchor: asked once after a return to a remembered window.
  const newRows = useNewRows(
    sessionListOptions(org, project, {
      from: new Date(anchor).toISOString(),
      to: new Date(listAnchor.lookedUpAt).toISOString(),
      filter: search.filter,
      includeEval: search.eval === 1,
      includeExperiments: search.exp === 1,
      limit: NEW_ROWS_LIMIT,
    }),
    listAnchor.reused &&
      range.kind === "preset" &&
      !hold &&
      list.isSuccess &&
      !list.isPlaceholderData,
  );
  const fetchNextPage = useCallback(() => {
    void list.fetchNextPage();
  }, [list.fetchNextPage]);
  const rows = useMemo(() => list.data?.pages.flatMap((page) => page.data) ?? [], [list.data]);
  const specs = useMemo(() => sessionColumnSpecs(sessionScoreNames(rows)), [rows]);
  const lockedIds = useMemo(() => lockedColumnIds(specs), [specs]);
  const defaults = useMemo(() => visibleColumnIds(specs, undefined), [specs]);
  const visible = useMemo(() => {
    const fromUrl = search.cols?.split(",");
    return visibleColumnIds(specs, fromUrl ?? readColumnChoice(TABLE_ID));
  }, [search.cols, specs]);
  const onFilter = useCallback(
    (conjunct: string) => {
      void navigate({
        to: "/$org/$project/sessions",
        params: { org, project },
        search: (prev) => withFilter(prev, appendFilter(prev.filter ?? "", conjunct)),
      });
    },
    [navigate, org, project],
  );
  const tableColumns = useMemo(
    () => orderedSessionColumns(rows, visible, onFilter),
    [rows, visible, onFilter],
  );

  function onColumns(ids: string[]) {
    const choice = sameColumnIds(ids, defaults) ? undefined : ids;
    writeColumnChoice(TABLE_ID, choice);
    void navigate({
      to: "/$org/$project/sessions",
      params: { org, project },
      search: (prev) => withCols(prev, choice?.join(",")),
    });
  }

  const problem = list.isError ? problemOf(list.error) : undefined;
  const filterError = problem?.code === "invalid_filter" ? problem : undefined;
  if (problem && !filterError) {
    throw list.error;
  }

  // A new window (a filter change, refresh or the pill) starts at the top, where its rows show.
  const seenWindow = useRef(windowId);
  useLayoutEffect(() => {
    if (seenWindow.current === windowId) {
      return;
    }
    seenWindow.current = windowId;
    const node = document.querySelector<HTMLElement>(
      '[aria-label="Sessions"] [data-scroll-container]',
    );
    if (node && node.scrollTop !== 0) {
      node.scrollTop = 0;
      node.dispatchEvent(new Event("scroll"));
    }
  }, [windowId]);

  return (
    <div className="flex h-full min-h-0 flex-1 flex-col">
      <TooltipProvider>
        <ListToolbar
          idPrefix="sessions"
          range={range}
          onRange={(next) => {
            const nextKey = next.kind === "preset" ? next.preset : `${next.from}\n${next.to}`;
            if (nextKey === rangeKey) {
              listAnchor.renew();
            }
            void navigate({
              to: "/$org/$project/sessions",
              params: { org, project },
              search: (prev) => withRange(prev, next),
            });
          }}
          onRefresh={listAnchor.renew}
          updatedAt={anchor}
          evalOn={search.eval === 1}
          onEval={(on) => {
            void navigate({
              to: "/$org/$project/sessions",
              params: { org, project },
              search: (prev) => withFlag(prev, "eval", on),
            });
          }}
          expOn={search.exp === 1}
          onExp={(on) => {
            void navigate({
              to: "/$org/$project/sessions",
              params: { org, project },
              search: (prev) => withFlag(prev, "exp", on),
            });
          }}
          notice={
            carried ? (
              <CarriedNotice
                from={carried.from}
                dropped={carried.dropped}
                kept={carried.kept}
                onClear={clearCarried}
              />
            ) : null
          }
          filter={
            <FilterBar
              view="sessions"
              value={search.filter ?? ""}
              error={filterError}
              scope={facetScope}
              onApply={(text) => {
                void navigate({
                  to: "/$org/$project/sessions",
                  params: { org, project },
                  search: (prev) => withFilter(prev, text),
                });
              }}
            />
          }
          filtersToggle={
            <FiltersToggle
              open={facetsOpen}
              onOpenChange={setFacetsOpen}
              filter={search.filter ?? ""}
            />
          }
          presets={
            <PresetChips
              view="sessions"
              filter={search.filter ?? ""}
              onFilter={(text) => {
                void navigate({
                  to: "/$org/$project/sessions",
                  params: { org, project },
                  search: (prev) => withFilter(prev, text),
                });
              }}
              scope={facetScope}
            />
          }
          columns={<ColumnChooser columns={specs} visible={visible} onVisible={onColumns} />}
          views={<SavedViewsMenu kind="sessions" search={search} onApply={applyView} />}
          actions={
            <>
              {checkedIds.size > 0 ? (
                <DisabledReason reason={queueReason}>
                  <Button
                    type="button"
                    variant="outline"
                    size="sm"
                    disabled={queueReason !== undefined}
                    onClick={() => {
                      setQueueItems(
                        [...checkedIds].map((sessionId) => ({ session_id: sessionId })),
                      );
                      setQueueOpen(true);
                    }}
                  >
                    {`Add ${checkedIds.size} to queue`}
                  </Button>
                </DisabledReason>
              ) : null}
              <DisabledReason reason={matchingQueueReason}>
                <Button
                  type="button"
                  variant="outline"
                  size="sm"
                  disabled={matchingQueueReason !== undefined}
                  onClick={() => {
                    setMatchOpen(true);
                  }}
                >
                  Add matching to queue…
                </Button>
              </DisabledReason>
            </>
          }
          loaded={list.data ? rows.length : undefined}
        />
        <AddToQueueDialog
          open={queueOpen}
          onOpenChange={setQueueOpen}
          target="session"
          items={queueItems}
          onAdded={() => {
            setSelection({ key: selectionKey, ids: NO_SESSIONS });
          }}
        />
        <AddMatchingDialog
          open={matchOpen}
          onOpenChange={setMatchOpen}
          target="session"
          selection={{
            filter: search.filter,
            from: resolved.from,
            to: resolved.to,
            includeExperiments: search.exp === 1,
          }}
        />
        <div className="flex min-h-0 flex-1">
          {facetsOpen && !hold ? (
            <FacetPanel
              view="sessions"
              filter={search.filter ?? ""}
              onFilter={(text) => {
                void navigate({
                  to: "/$org/$project/sessions",
                  params: { org, project },
                  search: (prev) => withFilter(prev, text),
                });
              }}
              scope={facetScope}
            />
          ) : null}
          <div className="flex min-w-0 flex-1 flex-col">
            {filterError ? (
              <div className="mx-auto flex max-w-md flex-col gap-2 px-4 py-16">
                <h2 className="text-body font-medium">{filterError.title}</h2>
                <p className="text-body text-destructive">{filterMessage(filterError)}</p>
              </div>
            ) : !hold && list.data && !(list.isPlaceholderData && rows.length === 0) ? (
              <>
                <SessionsSummary sessions={rows} more={list.hasNextPage} />
                <div className="border-b px-3 py-2">
                  <SessionsStartedChart sessions={rows} from={resolved.from} to={resolved.to} />
                </div>
                <DataTable
                  tableId="sessions"
                  ariaLabel="Sessions"
                  onReorder={onColumns}
                  lockedColumnIds={lockedIds}
                  columns={tableColumns}
                  rows={rows}
                  getRowId={(row) => row.session_id}
                  checkedIds={checkedIds}
                  onCheckedChange={(ids) => {
                    setSelection({ key: selectionKey, ids });
                  }}
                  onScrollAnchor={list.isPlaceholderData ? undefined : onScrollAnchor}
                  restoreAnchor={restoreAnchor ?? restoreAtMount}
                  highlightedRowIds={restoredHighlight}
                  stale={list.isPlaceholderData}
                  notice={
                    newRows && newRows.count > 0 ? (
                      <NewRowsPill
                        label={countLabel(
                          newRows.count,
                          newRows.more,
                          "session with new activity",
                          "sessions with new activity",
                        )}
                        onClick={listAnchor.renew}
                      />
                    ) : null
                  }
                  onActivate={(row) => {
                    const listReturn = listReturnFor(
                      "sessions",
                      search,
                      lastAnchor.current,
                      row.session_id,
                    );
                    rememberListReturn(listReturn);
                    markReturnPoint(router.history, listReturn);
                    void navigate({
                      to: "/$org/$project/sessions/$sessionId",
                      params: { org, project, sessionId: row.session_id },
                      search: pageFlags(search),
                      state: (current) => ({ ...current, listReturn }),
                    });
                  }}
                  hasNextPage={list.hasNextPage && !list.isPlaceholderData}
                  isFetchingNextPage={list.isFetchingNextPage}
                  fetchNextPage={fetchNextPage}
                  sorting={{
                    id: SORT_COLUMN[search.sort ?? "start_time"],
                    desc: (search.order ?? "desc") === "desc",
                  }}
                  onSortingChange={(next) => {
                    void navigate({
                      to: "/$org/$project/sessions",
                      params: { org, project },
                      search: (prev) => withSort(prev, next),
                    });
                  }}
                  emptyState={<EmptyState icon={MessagesSquare} title="No sessions" />}
                />
              </>
            ) : null}
          </div>
        </div>
      </TooltipProvider>
    </div>
  );
}

function pageFlags(search: SessionsSearch): { eval?: 1; exp?: 1 } {
  return {
    ...(search.eval ? { eval: 1 as const } : {}),
    ...(search.exp ? { exp: 1 as const } : {}),
  };
}

function withRange(search: SessionsSearch, state: RangeState): SessionsSearch {
  const { range: _range, from: _from, to: _to, ...rest } = search;
  if (state.kind === "preset") {
    return { ...rest, range: state.preset };
  }
  return { ...rest, from: state.from, to: state.to };
}

function withFilter(search: SessionsSearch, value: string): SessionsSearch {
  const { filter: _filter, ...rest } = search;
  const trimmed = value.trim();
  return trimmed.length > 0 ? { ...rest, filter: trimmed } : rest;
}

function withFlag(search: SessionsSearch, key: "eval" | "exp", on: boolean): SessionsSearch {
  const { [key]: _flag, ...rest } = search;
  return on ? { ...rest, [key]: 1 } : rest;
}

function withCols(search: SessionsSearch, cols: string | undefined): SessionsSearch {
  const { cols: _cols, ...rest } = search;
  return cols ? { ...rest, cols } : rest;
}

function withSort(
  search: SessionsSearch,
  next: { id: string; desc: boolean } | null,
): SessionsSearch {
  const { sort: _sort, order: _order, ...rest } = search;
  if (!next) {
    return rest;
  }
  const sort = COLUMN_SORT[next.id];
  if (!sort) {
    return rest;
  }
  if (sort === "start_time" && next.desc) {
    return rest;
  }
  return { ...rest, sort, order: next.desc ? "desc" : "asc" };
}
