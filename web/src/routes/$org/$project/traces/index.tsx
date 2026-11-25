import { useInfiniteQuery, useQuery, useQueryClient } from "@tanstack/react-query";
import { createFileRoute, useLocation, useRouter } from "@tanstack/react-router";
import { Trash2, Waypoints } from "lucide-react";
import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import { problemOf } from "@/api/problem";
import { projectRef } from "@/api/project-ref";
import { spanDetailQuery, traceDetailQuery, traceTotalsQuery } from "@/api/queries";
import type { Schema } from "@/api/types";
import { AddMatchingDialog } from "@/components/annotation-queues/add-matching-dialog";
import {
  AddToQueueDialog,
  addToQueueReason,
  type QueueItemTarget,
} from "@/components/annotation-queues/add-to-queue-dialog";
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
import { countLabel, NewRowsPill, useNewRows } from "@/components/data-table/new-rows";
import { traceListOptions, traceProbeOptions } from "@/components/data-table/use-infinite-list";
import { useLiveTraces } from "@/components/data-table/use-live-traces";
import {
  AddTracesToDatasetDialog,
  addTracesReason,
} from "@/components/datasets/add-traces-to-dataset-dialog";
import { DeleteMatchingDialog } from "@/components/deletions/delete-matching-dialog";
import { DisabledReason } from "@/components/disabled-reason";
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
import { TraceLayoutControls } from "@/components/layout-switcher";
import { ListToolbar } from "@/components/list-toolbar";
import { LiveToggle } from "@/components/live-toggle";
import { OnboardingPanel } from "@/components/onboarding/panel";
import { onboardingPollMs } from "@/components/onboarding/poll";
import { onboardingShown, tracesNeedOnboarding } from "@/components/onboarding/state";
import { useDefaultSavedView } from "@/components/saved-views/default-view";
import { SavedViewsMenu } from "@/components/saved-views/saved-views-menu";
import { useProject } from "@/components/shell/project-context";
import { Filmstrip } from "@/components/trace/filmstrip";
import { SplitPane } from "@/components/trace/split-pane";
import { selectionTotals, TraceTotalsLine, totalsOf } from "@/components/trace/trace-totals";
import { TraceView } from "@/components/trace/trace-view";
import { TracesHistogram } from "@/components/trace/traces-histogram";
import {
  orderedTraceColumns,
  scoreNames,
  traceColumnSpecs,
  traceRowTone,
} from "@/components/trace-columns";
import { Button } from "@/components/ui/button";
import { Skeleton } from "@/components/ui/skeleton";
import { TooltipProvider } from "@/components/ui/tooltip";
import { appendFilter } from "@/lib/dsl/dsl";
import { formatDuration } from "@/lib/format";
import {
  anchorKey,
  keepSameProject,
  LIST_CACHE,
  NEW_ROWS_LIMIT,
  useListAnchor,
} from "@/lib/list-anchor";
import { rangeOnly, writeListContext } from "@/lib/list-context";
import { getTraceMaximized, setTraceMaximized, useListLayout } from "@/lib/list-layout";
import {
  anchorOf,
  type ListReturn,
  listReturnFor,
  markReturnPoint,
  type RowAnchor,
  rememberListReturn,
  useRestoredHighlight,
} from "@/lib/list-return";
import { atLeast } from "@/lib/roles";
import {
  parseTracesSearch,
  type TraceMode,
  type TracesSearch,
  traceLiveReason,
} from "@/lib/search/traces";
import { traceStreamUrl } from "@/lib/sse";
import { type RangeState, rangeFromSearch, resolveRange } from "@/lib/time-range";

const TABLE_ID = "traces";
const NO_TRACES: ReadonlySet<string> = new Set();

const SORT_COLUMN: Record<Schema<"TraceSort">, string> = {
  start_time: "start",
  duration: "duration",
  cost_total: "cost",
  total_tokens: "tokens",
};

const COLUMN_SORT: Record<string, Schema<"TraceSort">> = {
  start: "start_time",
  duration: "duration",
  cost: "cost_total",
  tokens: "total_tokens",
};

export const Route = createFileRoute("/$org/$project/traces/")({
  validateSearch: (search) => parseTracesSearch(search),
  component: TracesPage,
});

function TracesPage() {
  const { org, project, data } = useProject();
  const canDelete = atLeast(data.role, "admin");
  const search = Route.useSearch();
  const navigate = Route.useNavigate();
  const router = useRouter();
  const [deleteOpen, setDeleteOpen] = useState(false);
  const [addOpen, setAddOpen] = useState(false);
  const [addIds, setAddIds] = useState<string[]>([]);
  const [queueOpen, setQueueOpen] = useState(false);
  const [queueItems, setQueueItems] = useState<readonly QueueItemTarget[]>([]);
  const [matchOpen, setMatchOpen] = useState(false);
  const applyView = useCallback(
    (next: Record<string, unknown>) => {
      void navigate({
        to: "/$org/$project/traces",
        params: { org, project },
        search: () => parseTracesSearch(next),
      });
    },
    [navigate, org, project],
  );
  const applyDefault = useCallback(
    (next: Record<string, unknown>) => {
      void navigate({
        to: "/$org/$project/traces",
        params: { org, project },
        search: () => parseTracesSearch(next),
        replace: true,
      });
    },
    [navigate, org, project],
  );
  const { hold, carried } = useDefaultSavedView("traces", search, applyDefault, "traces");
  useEffect(() => {
    if (!hold) {
      writeListContext(org, project, "traces", search);
    }
  }, [hold, org, project, search]);
  function clearCarried() {
    void navigate({
      to: "/$org/$project/traces",
      params: { org, project },
      search: (prev) => rangeOnly(prev),
    });
  }
  const [facetsOpen, setFacetsOpen] = useFacetsOpen();
  const range = rangeFromSearch(search);
  const rangeKey = range.kind === "preset" ? range.preset : `${range.from}\n${range.to}`;
  const windowKey = anchorKey(org, project, "traces", {
    rangeKey,
    filter: search.filter,
    search: search.search,
    sort: search.sort ?? "start_time",
    order: search.order ?? "desc",
    eval: search.eval === 1,
    exp: search.exp === 1,
  });
  const listAnchor = useListAnchor(windowKey, search.live === 1);
  const anchor = listAnchor.anchor;
  const windowId = `${windowKey}\n${anchor}`;
  const [restoreAtMount] = useState(() => (listAnchor.reused ? listAnchor.scroll : undefined));
  const resolved = resolveRange(range, anchor);
  /** The list's window and flags: the facet panel, its presets and the filter bar fetch with it. */
  const facetScope: FacetScope = {
    from: resolved.from,
    to: resolved.to,
    includeEval: search.eval === 1,
    includeExperiments: search.exp === 1,
    search: search.search,
  };
  // Checked traces belong to the list they were checked in: a new filter, search, range or
  // eval/experiment flag clears them.
  const selectionKey = JSON.stringify([
    search.filter,
    search.search,
    rangeKey,
    search.eval,
    search.exp,
  ]);
  const [selection, setSelection] = useState<{ key: string; ids: ReadonlySet<string> }>({
    key: selectionKey,
    ids: NO_TRACES,
  });
  const checkedIds = selection.key === selectionKey ? selection.ids : NO_TRACES;
  const addReason = addTracesReason(data.role, checkedIds.size);
  const queueReason = addToQueueReason(data.role, checkedIds.size);
  const matchingQueueReason = addToQueueReason(data.role, 0);
  const listOptions = traceListOptions(org, project, {
    from: resolved.from,
    to: resolved.to,
    sort: search.sort,
    order: search.order,
    search: search.search,
    filter: search.filter,
    includeEval: search.eval === 1,
    includeExperiments: search.exp === 1,
  });
  const queryClient = useQueryClient();
  const needsOnboarding = tracesNeedOnboarding(search, range);
  const probe = traceProbeOptions(org, project);
  const list = useInfiniteQuery({
    ...listOptions,
    ...LIST_CACHE,
    placeholderData: keepSameProject(listOptions.queryKey),
    enabled: !hold,
    refetchInterval: (query) => {
      if (hold || !needsOnboarding) {
        return false;
      }
      const data = query.state.data;
      if (!data || data.pages.some((page) => page.data.length > 0)) {
        return false;
      }
      const probeState = queryClient.getQueryState<Schema<"TracePage">>(probe.queryKey);
      const probeRows = probeState?.data?.data.length;
      const probeFailed = probeState?.status === "error";
      return onboardingShown(needsOnboarding, probeRows, probeFailed) ? onboardingPollMs() : false;
    },
  });
  const fetchNextPage = useCallback(() => {
    void list.fetchNextPage();
  }, [list.fetchNextPage]);
  const rows = useMemo(() => list.data?.pages.flatMap((page) => page.data) ?? [], [list.data]);
  const selectedTotals = useMemo(
    () => (checkedIds.size > 0 ? selectionTotals(rows, checkedIds) : undefined),
    [rows, checkedIds],
  );
  const specs = useMemo(() => traceColumnSpecs(scoreNames(rows)), [rows]);
  const lockedIds = useMemo(() => lockedColumnIds(specs), [specs]);
  const defaults = useMemo(() => visibleColumnIds(specs, undefined), [specs]);
  const visible = useMemo(() => {
    const fromUrl = search.cols?.split(",");
    return visibleColumnIds(specs, fromUrl ?? readColumnChoice(TABLE_ID));
  }, [search.cols, specs]);
  const onFilter = useCallback(
    (conjunct: string) => {
      void navigate({
        to: "/$org/$project/traces",
        params: { org, project },
        search: (prev) => withFilter(prev, appendFilter(prev.filter ?? "", conjunct)),
      });
    },
    [navigate, org, project],
  );
  const tableColumns = useMemo(
    () => orderedTraceColumns(rows, visible, onFilter),
    [rows, visible, onFilter],
  );
  function onColumns(ids: string[]) {
    const choice = sameColumnIds(ids, defaults) ? undefined : ids;
    writeColumnChoice(TABLE_ID, choice);
    void navigate({
      to: "/$org/$project/traces",
      params: { org, project },
      search: (prev) => withCols(prev, choice?.join(",")),
    });
  }
  function changeRange(next: RangeState) {
    const nextKey = next.kind === "preset" ? next.preset : `${next.from}\n${next.to}`;
    if (nextKey === rangeKey) {
      listAnchor.renew();
    }
    void navigate({
      to: "/$org/$project/traces",
      params: { org, project },
      search: (prev) => {
        const ranged = withRange(prev, next);
        return traceLiveReason(ranged) ? withoutLive(ranged) : ranged;
      },
    });
  }
  const layout = useListLayout();
  const listRestore = useLocation().state.listRestore;
  const restoreAnchor = useMemo(() => anchorOf(listRestore), [listRestore]);
  const restoredHighlight = useRestoredHighlight(listRestore?.openedId);
  const lastAnchor = useRef<RowAnchor | undefined>(undefined);
  const onScrollAnchor = useCallback(
    (anchor: RowAnchor) => {
      lastAnchor.current = anchor;
      listAnchor.rememberScroll(anchor);
    },
    [listAnchor.rememberScroll],
  );
  // The full-page link reads this when it is followed, so the list's position is current.
  function captureFullPageReturn(): ListReturn | undefined {
    if (search.trace === undefined) {
      return undefined;
    }
    const ret = listReturnFor("traces", search, lastAnchor.current, search.trace);
    rememberListReturn(ret);
    markReturnPoint(router.history, ret);
    return ret;
  }
  const traceOpen = search.trace !== undefined;
  const listScroll = useRef(0);
  const wasOpen = useRef(traceOpen);

  useLayoutEffect(() => {
    const node = document.querySelector<HTMLElement>(
      '[aria-label="Traces"] [data-scroll-container]',
    );
    if (node && wasOpen.current !== traceOpen) {
      node.scrollTop = listScroll.current;
      node.dispatchEvent(new Event("scroll"));
      wasOpen.current = traceOpen;
    }
    return () => {
      if (node) {
        listScroll.current = node.scrollTop;
      }
    };
  }, [traceOpen]);

  useShortcut("dismiss", () => {
    if (getTraceMaximized()) {
      setTraceMaximized(false);
      return;
    }
    if (!search.trace) {
      return;
    }
    void navigate({
      to: "/$org/$project/traces",
      params: { org, project },
      search: (prev) => withoutTrace(prev),
    });
  });

  // Previous and next follow the loaded rows in display order; no button at either end of the loaded list.
  const openIndex = search.trace ? rows.findIndex((row) => row.trace_id === search.trace) : -1;
  const previousRow = openIndex > 0 ? rows[openIndex - 1] : undefined;
  const nextRow = openIndex >= 0 && openIndex < rows.length - 1 ? rows[openIndex + 1] : undefined;
  function openTrace(traceId: string) {
    void navigate({
      to: "/$org/$project/traces",
      params: { org, project },
      search: (prev) => ({ ...withoutSpan(prev), trace: traceId }),
    });
  }
  // Warms a trace and its root span, which the panel opens on, so opening it shows no loading state.
  const prefetchTrace = useCallback(
    (row: Schema<"TraceSummary">) => {
      void queryClient.prefetchQuery(traceDetailQuery(org, project, row.trace_id));
      if (row.root_span_id) {
        void queryClient.prefetchQuery(
          spanDetailQuery(org, project, row.trace_id, row.root_span_id),
        );
      }
    },
    [org, project, queryClient],
  );
  // Warms the open trace's neighbours once per open trace, so Previous and Next open at once.
  const prefetchedFor = useRef<string | undefined>(undefined);
  useEffect(() => {
    if (!search.trace) {
      prefetchedFor.current = undefined;
      return;
    }
    if (openIndex < 0 || prefetchedFor.current === search.trace) {
      return;
    }
    prefetchedFor.current = search.trace;
    for (const row of [previousRow, nextRow]) {
      if (row !== undefined) {
        prefetchTrace(row);
      }
    }
  }, [search.trace, openIndex, previousRow, nextRow, prefetchTrace]);

  const problem = list.isError ? problemOf(list.error) : undefined;
  const filterError = problem?.code === "invalid_filter" ? problem : undefined;
  const listEmpty =
    !hold && list.data !== undefined && !list.isPlaceholderData && rows.length === 0;
  const projectProbe = useQuery({ ...probe, enabled: listEmpty && needsOnboarding });
  const probeRows = projectProbe.data?.data.length;
  // A cached empty answer is not trusted while it refetches: a trace may have arrived since.
  const probing =
    listEmpty &&
    needsOnboarding &&
    (projectProbe.isPending || (projectProbe.isFetching && probeRows === 0));
  const onboarding =
    listEmpty && !probing && onboardingShown(needsOnboarding, probeRows, projectProbe.isError);
  const totals = useQuery({
    ...traceTotalsQuery(org, project, {
      from: resolved.from,
      to: resolved.to,
      filter: search.filter,
      search: search.search,
      includeEval: search.eval === 1,
      includeExperiments: search.exp === 1,
    }),
    ...LIST_CACHE,
    enabled: !hold && !filterError && !probing && !onboarding && rows.length > 0,
  });
  const reason = traceLiveReason(search);
  const liveOn = search.live === 1 && reason === undefined && !hold;
  const live = useLiveTraces({
    url:
      liveOn && (list.isSuccess || list.isRefetchError) && !list.isPlaceholderData
        ? traceStreamUrl(projectRef(org, project), {
            filter: search.filter,
            search: search.search,
            includeEval: search.eval === 1,
            includeExperiments: search.exp === 1,
          })
        : undefined,
    queryKey: listOptions.queryKey,
    selectedId: search.trace,
    ready: !hold && list.data !== undefined && !filterError && !onboarding && !probing,
  });
  // Traces that arrived since the anchor: asked once after a return to a remembered window.
  const newRows = useNewRows(
    traceListOptions(org, project, {
      from: new Date(anchor).toISOString(),
      to: new Date(listAnchor.lookedUpAt).toISOString(),
      search: search.search,
      filter: search.filter,
      includeEval: search.eval === 1,
      includeExperiments: search.exp === 1,
      limit: NEW_ROWS_LIMIT,
    }),
    listAnchor.reused &&
      range.kind === "preset" &&
      !liveOn &&
      !hold &&
      list.isSuccess &&
      !list.isPlaceholderData &&
      !onboarding &&
      !probing,
  );
  // A new window (a search change, refresh or the pill) starts at the top, where its rows show.
  const seenWindow = useRef(windowId);
  useLayoutEffect(() => {
    if (seenWindow.current === windowId) {
      return;
    }
    seenWindow.current = windowId;
    const node = document.querySelector<HTMLElement>(
      '[aria-label="Traces"] [data-scroll-container]',
    );
    if (node && node.scrollTop !== 0) {
      node.scrollTop = 0;
      node.dispatchEvent(new Event("scroll"));
    }
  }, [windowId]);
  const highlighted = useMemo(
    () =>
      restoredHighlight.size === 0
        ? live.highlighted
        : new Set([...live.highlighted, ...restoredHighlight]),
    [live.highlighted, restoredHighlight],
  );
  useEffect(() => {
    if (search.live !== 1 || reason === undefined) {
      return;
    }
    void navigate({
      to: "/$org/$project/traces",
      params: { org, project },
      search: (prev) => withoutLive(prev),
      replace: true,
    });
  }, [search.live, reason, navigate, org, project]);
  if (problem && !filterError) {
    throw list.error;
  }
  function clearConstraints() {
    void navigate({
      to: "/$org/$project/traces",
      params: { org, project },
      search: (prev) => withoutConstraints(prev),
    });
  }

  return (
    <div className="relative h-full min-h-0 flex-1">
      <div className="absolute inset-0">
        <SplitPane
          name="traces"
          mode={layout}
          filmstrip={
            <Filmstrip
              items={rows.map((row) => ({
                id: row.trace_id,
                label: row.name,
                detail: formatDuration(row.duration_ns),
                status: row.status,
              }))}
              selectedId={search.trace}
              onSelect={(id) => {
                void navigate({
                  to: "/$org/$project/traces",
                  params: { org, project },
                  search: (prev) => ({ ...withoutSpan(prev), trace: id }),
                });
              }}
              onBack={() => {
                void navigate({
                  to: "/$org/$project/traces",
                  params: { org, project },
                  search: (prev) => withoutTrace(prev),
                });
              }}
            />
          }
          open={traceOpen}
          start={
            <div className="flex h-full min-h-0 flex-col">
              <TooltipProvider>
                <ListToolbar
                  range={range}
                  onRange={changeRange}
                  onRefresh={listAnchor.renew}
                  updatedAt={anchor}
                  live={
                    <LiveToggle
                      pressed={liveOn}
                      disabled={reason !== undefined}
                      reason={reason}
                      status={live.status}
                      onPressedChange={(on) => {
                        void navigate({
                          to: "/$org/$project/traces",
                          params: { org, project },
                          search: (prev) => withLive(prev, on),
                        });
                      }}
                    />
                  }
                  search={search.search ?? ""}
                  onSearch={(value) => {
                    void navigate({
                      to: "/$org/$project/traces",
                      params: { org, project },
                      search: (prev) => withSearch(prev, value),
                    });
                  }}
                  evalOn={search.eval === 1}
                  onEval={(on) => {
                    void navigate({
                      to: "/$org/$project/traces",
                      params: { org, project },
                      search: (prev) => withFlag(prev, "eval", on),
                    });
                  }}
                  expOn={search.exp === 1}
                  onExp={(on) => {
                    void navigate({
                      to: "/$org/$project/traces",
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
                      view="traces"
                      value={search.filter ?? ""}
                      error={filterError}
                      scope={facetScope}
                      onApply={(text) => {
                        void navigate({
                          to: "/$org/$project/traces",
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
                      hold={hold}
                      view="traces"
                      filter={search.filter ?? ""}
                      onFilter={(text) => {
                        void navigate({
                          to: "/$org/$project/traces",
                          params: { org, project },
                          search: (prev) => withFilter(prev, text),
                        });
                      }}
                      scope={facetScope}
                    />
                  }
                  columns={
                    <ColumnChooser columns={specs} visible={visible} onVisible={onColumns} />
                  }
                  views={<SavedViewsMenu kind="traces" search={search} onApply={applyView} />}
                  actions={
                    <>
                      {checkedIds.size > 0 ? (
                        <DisabledReason reason={addReason}>
                          <Button
                            type="button"
                            variant="outline"
                            size="sm"
                            disabled={addReason !== undefined}
                            onClick={() => {
                              setAddIds([...checkedIds]);
                              setAddOpen(true);
                            }}
                          >
                            {`Add ${checkedIds.size} to dataset`}
                          </Button>
                        </DisabledReason>
                      ) : null}
                      {checkedIds.size > 0 ? (
                        <DisabledReason reason={queueReason}>
                          <Button
                            type="button"
                            variant="outline"
                            size="sm"
                            disabled={queueReason !== undefined}
                            onClick={() => {
                              setQueueItems(
                                [...checkedIds].map((traceId) => ({ trace_id: traceId })),
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
                      <DisabledReason
                        reason={canDelete ? undefined : "Only project admins can delete traces"}
                      >
                        <Button
                          type="button"
                          variant="outline"
                          size="sm"
                          disabled={!canDelete}
                          onClick={() => {
                            setDeleteOpen(true);
                          }}
                        >
                          <Trash2 />
                          Delete matching…
                        </Button>
                      </DisabledReason>
                    </>
                  }
                  loaded={list.data ? rows.length : undefined}
                />
                <DeleteMatchingDialog
                  open={deleteOpen}
                  onOpenChange={setDeleteOpen}
                  selection={{
                    filter: search.filter,
                    search: search.search,
                    from: resolved.from,
                    to: resolved.to,
                    includeEval: search.eval === 1,
                    includeExperiments: search.exp === 1,
                  }}
                />
                <AddTracesToDatasetDialog
                  open={addOpen}
                  onOpenChange={setAddOpen}
                  traceIds={addIds}
                  onAdded={() => {
                    setSelection({ key: selectionKey, ids: NO_TRACES });
                  }}
                />
                <AddToQueueDialog
                  open={queueOpen}
                  onOpenChange={setQueueOpen}
                  target="trace"
                  items={queueItems}
                  onAdded={() => {
                    setSelection({ key: selectionKey, ids: NO_TRACES });
                  }}
                />
                <AddMatchingDialog
                  open={matchOpen}
                  onOpenChange={setMatchOpen}
                  target="trace"
                  selection={{
                    filter: search.filter,
                    search: search.search,
                    from: resolved.from,
                    to: resolved.to,
                    includeExperiments: search.exp === 1,
                  }}
                />
                <div className="flex min-h-0 flex-1">
                  {facetsOpen && !hold && !onboarding ? (
                    <FacetPanel
                      view="traces"
                      filter={search.filter ?? ""}
                      onFilter={(text) => {
                        void navigate({
                          to: "/$org/$project/traces",
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
                    ) : probing ? (
                      <div className="flex flex-col gap-2 p-4">
                        <Skeleton className="h-8 w-full" />
                        <Skeleton className="h-8 w-full" />
                        <Skeleton className="h-8 w-full" />
                      </div>
                    ) : onboarding ? (
                      <div className="min-h-0 flex-1 overflow-auto">
                        <OnboardingPanel origin={window.location.origin} />
                      </div>
                    ) : !hold &&
                      list.data &&
                      !probing &&
                      !(list.isPlaceholderData && rows.length === 0) ? (
                      <>
                        <TracesHistogram
                          org={org}
                          project={project}
                          from={resolved.from}
                          to={resolved.to}
                          filter={search.filter}
                          includeEval={search.eval === 1}
                          includeExperiments={search.exp === 1}
                          onRange={(fromIso, toIso) => {
                            changeRange({ kind: "absolute", from: fromIso, to: toIso });
                          }}
                        />
                        <DataTable
                          tableId="traces"
                          ariaLabel="Traces"
                          rowTone={traceRowTone}
                          onReorder={onColumns}
                          lockedColumnIds={lockedIds}
                          columns={tableColumns}
                          rows={rows}
                          getRowId={(row) => row.trace_id}
                          selectedRowId={search.trace}
                          onActivate={(row) => {
                            void navigate({
                              to: "/$org/$project/traces",
                              params: { org, project },
                              search: (prev) => ({ ...withoutSpan(prev), trace: row.trace_id }),
                            });
                          }}
                          onRowHover={prefetchTrace}
                          checkedIds={checkedIds}
                          onCheckedChange={(ids) => {
                            setSelection({ key: selectionKey, ids });
                          }}
                          onScrollAnchor={list.isPlaceholderData ? undefined : onScrollAnchor}
                          restoreAnchor={restoreAnchor ?? restoreAtMount}
                          onSelectionMove={(row) => {
                            void navigate({
                              to: "/$org/$project/traces",
                              params: { org, project },
                              search: (prev) => ({ ...withoutSpan(prev), trace: row.trace_id }),
                              replace: true,
                            });
                          }}
                          stale={list.isPlaceholderData}
                          hasNextPage={list.hasNextPage && !list.isPlaceholderData}
                          isFetchingNextPage={list.isFetchingNextPage}
                          fetchNextPage={fetchNextPage}
                          sorting={{
                            id: SORT_COLUMN[search.sort ?? "start_time"],
                            desc: (search.order ?? "desc") === "desc",
                          }}
                          onSortingChange={(next) => {
                            void navigate({
                              to: "/$org/$project/traces",
                              params: { org, project },
                              search: (prev) => {
                                const sorted = withSort(prev, next);
                                return traceLiveReason(sorted) ? withoutLive(sorted) : sorted;
                              },
                            });
                          }}
                          highlightedRowIds={highlighted}
                          notice={
                            live.pending > 0 ? (
                              <NewRowsPill
                                label={countLabel(
                                  live.pending,
                                  live.overflowed,
                                  "new trace",
                                  "new traces",
                                )}
                                onClick={live.reveal}
                              />
                            ) : newRows && newRows.count > 0 ? (
                              <NewRowsPill
                                label={countLabel(
                                  newRows.count,
                                  newRows.more,
                                  "new trace",
                                  "new traces",
                                )}
                                onClick={listAnchor.renew}
                              />
                            ) : null
                          }
                          emptyState={
                            needsOnboarding ? (
                              <div className="flex flex-col items-center gap-3 px-4 py-16 text-center">
                                <Waypoints className="size-8 text-muted-foreground" />
                                <h2 className="text-title font-medium">
                                  No traces in this time range.
                                </h2>
                              </div>
                            ) : (
                              <div className="flex flex-col items-center gap-3 px-4 py-16 text-center">
                                <Waypoints className="size-8 text-muted-foreground" />
                                <h2 className="text-title font-medium">No traces match</h2>
                                <Button
                                  type="button"
                                  variant="outline"
                                  size="sm"
                                  onClick={clearConstraints}
                                >
                                  Clear filters
                                </Button>
                              </div>
                            )
                          }
                        />
                        {rows.length > 0 ? (
                          <TraceTotalsLine
                            selected={selectedTotals}
                            list={totals.data ? totalsOf(totals.data) : undefined}
                            listStatus={totals.status}
                          />
                        ) : null}
                      </>
                    ) : null}
                  </div>
                </div>
              </TooltipProvider>
            </div>
          }
          end={
            search.trace ? (
              <TraceView
                traceId={search.trace}
                spanId={search.span}
                mode={search.mode}
                onModeChange={(next) => {
                  void navigate({
                    to: "/$org/$project/traces",
                    params: { org, project },
                    search: (prev) => withMode(prev, next),
                    replace: true,
                  });
                }}
                onAddFilter={onFilter}
                fullPageListReturn={captureFullPageReturn}
                onPrevious={previousRow ? () => openTrace(previousRow.trace_id) : undefined}
                onNext={nextRow ? () => openTrace(nextRow.trace_id) : undefined}
                layout={<TraceLayoutControls />}
                onClose={() => {
                  return navigate({
                    to: "/$org/$project/traces",
                    params: { org, project },
                    search: (prev) => withoutTrace(prev),
                  });
                }}
                onSelectSpan={(spanId) => {
                  void navigate({
                    to: "/$org/$project/traces",
                    params: { org, project },
                    search: (prev) => ({ ...prev, span: spanId }),
                    replace: true,
                  });
                }}
                onShowSummary={() => {
                  void navigate({
                    to: "/$org/$project/traces",
                    params: { org, project },
                    search: (prev) => withoutSpan(prev),
                    replace: true,
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

function withRange(search: TracesSearch, state: RangeState): TracesSearch {
  const { range: _range, from: _from, to: _to, ...rest } = search;
  if (state.kind === "preset") {
    return { ...rest, range: state.preset };
  }
  return { ...rest, from: state.from, to: state.to };
}

function withSearch(search: TracesSearch, value: string): TracesSearch {
  const { search: _search, ...rest } = search;
  const trimmed = value.trim();
  return trimmed.length > 0 ? { ...rest, search: trimmed } : rest;
}

function withFilter(search: TracesSearch, value: string): TracesSearch {
  const { filter: _filter, ...rest } = search;
  const trimmed = value.trim();
  return trimmed.length > 0 ? { ...rest, filter: trimmed } : rest;
}

function withLive(search: TracesSearch, on: boolean): TracesSearch {
  const rest = withoutLive(search);
  return on ? { ...rest, live: 1 } : rest;
}

function withoutLive(search: TracesSearch): TracesSearch {
  const { live: _live, ...rest } = search;
  return rest;
}

function withFlag(search: TracesSearch, key: "eval" | "exp", on: boolean): TracesSearch {
  const { [key]: _flag, ...rest } = search;
  return on ? { ...rest, [key]: 1 } : rest;
}

function withCols(search: TracesSearch, cols: string | undefined): TracesSearch {
  const { cols: _cols, ...rest } = search;
  return cols ? { ...rest, cols } : rest;
}

function withSort(search: TracesSearch, next: { id: string; desc: boolean } | null): TracesSearch {
  const { sort: _sort, order: _order, ...rest } = search;
  if (!next) {
    return rest;
  }
  const sort = COLUMN_SORT[next.id];
  if (!sort) {
    return rest;
  }
  return { ...rest, sort, order: next.desc ? "desc" : "asc" };
}

function withoutSpan(search: TracesSearch): TracesSearch {
  const { span: _span, ...rest } = search;
  return rest;
}

function withoutConstraints(search: TracesSearch): TracesSearch {
  const {
    filter: _filter,
    search: _query,
    eval: _eval,
    exp: _exp,
    from: _from,
    to: _to,
    view: _view,
    ...rest
  } = search;
  return rest;
}

function withoutTrace(search: TracesSearch): TracesSearch {
  const { trace: _trace, span: _span, ...rest } = search;
  return rest;
}

function withMode(search: TracesSearch, mode: TraceMode): TracesSearch {
  const { mode: _mode, ...rest } = search;
  return mode === "tree" ? rest : { ...rest, mode };
}
