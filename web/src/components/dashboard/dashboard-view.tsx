import { useQuery, useQueryClient } from "@tanstack/react-query";
import { useNavigate } from "@tanstack/react-router";
import { useEffect, useRef, useState } from "react";
import { problemOf } from "@/api/problem";
import { metricsQuery } from "@/api/queries";
import type { Schema } from "@/api/types";
import { KpiRow, KpiRowSkeleton, kpiTrend } from "@/components/charts/kpi-tiles";
import {
  chartElementId,
  DashboardBody,
  DashboardBodySkeleton,
  sectionOfChart,
} from "@/components/charts/metric-chart";
import { FilterBar } from "@/components/filter-bar/filter-bar";
import { filterMessage } from "@/components/filter-bar/logic";
import { ListToolbar } from "@/components/list-toolbar";
import { useProject } from "@/components/shell/project-context";
import { TooltipProvider } from "@/components/ui/tooltip";
import { appendFilter, serializeChip } from "@/lib/dsl/dsl";
import type { KpiMetric } from "@/lib/metrics";
import { bucketRange } from "@/lib/metrics";
import type { DashboardSearch } from "@/lib/search/dashboard";
import { type RangeState, rangeFromSearch, resolveRange } from "@/lib/time-range";
import { STORAGE_PREFIX } from "@/product";

const METRICS_PATH = "/api/v1/projects/{project}/metrics";

/** The chart card each KPI tile scrolls to, by its title. */
const KPI_CHART: Record<KpiMetric, string> = {
  traces: "Traces & errors",
  error_rate: "Error rate",
  latency_p95: "Latency",
  cost_total: "Cost by model",
  total_tokens: "Tokens by type",
};

/** The window's end is rounded down to the minute, so visits within one minute share a cache key. */
function minuteAnchor(now = Date.now()): number {
  return Math.floor(now / 60_000) * 60_000;
}

/**
 * A metrics query key without `from` and `to`. Keys with the same value show the same dashboard
 * for different windows. Keys of other queries give undefined.
 */
function metricsBase(queryKey: readonly unknown[]): string | undefined {
  const init = queryKey[2] as
    | { params: { path: unknown; query: Record<string, unknown> } }
    | undefined;
  if (queryKey[0] !== "get" || queryKey[1] !== METRICS_PATH || !init) {
    return undefined;
  }
  const { from: _from, to: _to, ...query } = init.params.query;
  return JSON.stringify([init.params.path, query]);
}

/**
 * The dashboard for the given URL search. The project layout renders it and keeps it mounted
 * (hidden) on other project pages; the dashboard route itself renders nothing.
 */
export function DashboardView({ search }: { search: DashboardSearch }) {
  const { org, project } = useProject();
  const navigate = useNavigate({ from: "/$org/$project/dashboard/" });
  const queryClient = useQueryClient();
  const range = rangeFromSearch(search);
  const rangeKey = range.kind === "preset" ? range.preset : `${range.from}\n${range.to}`;
  const scrollRef = useRef<HTMLDivElement>(null);
  // The collapsed sections, per project. The state records the key it was read for, so a
  // project change reads that project's sections.
  const collapsedKey = `${STORAGE_PREFIX}dashboard.collapsed.${org}/${project}`;
  const [collapsedState, setCollapsedState] = useState(() => ({
    key: collapsedKey,
    names: readCollapsed(collapsedKey),
  }));
  const collapsed =
    collapsedState.key === collapsedKey ? collapsedState.names : readCollapsed(collapsedKey);
  // A KPI tile whose card sits in a collapsed section expands it first; the card is scrolled to
  // once it is rendered (the effect below).
  const pendingScroll = useRef<string | undefined>(undefined);
  useEffect(() => {
    const id = pendingScroll.current;
    if (id === undefined) {
      return;
    }
    pendingScroll.current = undefined;
    scrollToCard(id);
  });
  const sections = {
    collapsed,
    onToggle: (title: string) => {
      const next = new Set(collapsed);
      if (next.has(title)) {
        next.delete(title);
      } else {
        next.add(title);
      }
      writeCollapsed(collapsedKey, next);
      setCollapsedState({ key: collapsedKey, names: next });
    },
  };
  const [anchor, setAnchor] = useState(() => minuteAnchor());
  const seenRange = useRef(rangeKey);
  useEffect(() => {
    if (seenRange.current === rangeKey) {
      return;
    }
    seenRange.current = rangeKey;
    setAnchor(minuteAnchor());
  }, [rangeKey]);
  const resolved = resolveRange(range, anchor);
  const options = metricsQuery(org, project, {
    from: resolved.from,
    to: resolved.to,
    filter: search.filter,
    groupBy: search.group,
    includeEval: search.eval === 1,
    includeExperiments: search.exp === 1,
  });
  const base = metricsBase(options.queryKey);
  // The most recently updated cached result for the same query over another window.
  function latestCached(): { data: Schema<"MetricsResponse">; dataUpdatedAt: number } | undefined {
    if (base === undefined) {
      return undefined;
    }
    let latest: { data: Schema<"MetricsResponse">; dataUpdatedAt: number } | undefined;
    const entries = queryClient.getQueryCache().findAll({
      predicate: (query) => query.state.data !== undefined && metricsBase(query.queryKey) === base,
    });
    for (const entry of entries) {
      if (latest === undefined || entry.state.dataUpdatedAt > latest.dataUpdatedAt) {
        latest = {
          data: entry.state.data as Schema<"MetricsResponse">,
          dataUpdatedAt: entry.state.dataUpdatedAt,
        };
      }
    }
    return latest;
  }
  const metrics = useQuery({
    ...options,
    staleTime: 10_000,
    placeholderData: (previous) => previous ?? latestCached()?.data,
  });
  const problem = metrics.isError ? problemOf(metrics.error) : undefined;
  const filterError = problem?.code === "invalid_filter" ? problem : undefined;
  if (problem && !filterError) {
    throw metrics.error;
  }
  const data = filterError ? undefined : metrics.data;
  // A placeholder is the newest cached entry (or this observer's previous result, which is that
  // same entry in practice); the query's own result carries its own timestamp.
  const shownAt = metrics.isPlaceholderData ? latestCached()?.dataUpdatedAt : metrics.dataUpdatedAt;
  const updatedAt = data && shownAt ? shownAt : undefined;

  /** Shows another window: the toolbar's range picker and a zoom drag both come here. */
  function changeRange(next: RangeState) {
    seenRange.current = next.kind === "preset" ? next.preset : `${next.from}\n${next.to}`;
    setAnchor(minuteAnchor());
    void navigate({
      to: "/$org/$project/dashboard",
      params: { org, project },
      search: (prev) => withRange(prev, next),
    });
  }

  function zoomTo(fromMs: number, toMs: number) {
    changeRange({
      kind: "absolute",
      from: new Date(fromMs).toISOString(),
      to: new Date(toMs).toISOString(),
    });
  }

  /** Scrolls to a chart card, within this dashboard (the page may hold others). */
  function scrollToCard(id: string) {
    const card = scrollRef.current?.querySelector(`#${id}`);
    card?.scrollIntoView({ block: "start", behavior: "smooth" });
  }

  /** Scrolls to the chart card of a KPI tile, expanding its section first when it is collapsed. */
  function scrollToChart(metric: KpiMetric) {
    const title = KPI_CHART[metric];
    const id = chartElementId(title);
    const section = sectionOfChart(title);
    if (section !== undefined && collapsed.has(section)) {
      pendingScroll.current = id;
      sections.onToggle(section);
      return;
    }
    scrollToCard(id);
  }

  function openBucket(timeMs: number) {
    if (!data) {
      return;
    }
    const bucket = bucketRange(timeMs, data.bucket_ns);
    void navigate({
      to: "/$org/$project/traces",
      params: { org, project },
      search: {
        from: bucket.from,
        to: bucket.to,
        ...(search.filter ? { filter: search.filter } : {}),
        ...(search.eval ? { eval: 1 as const } : {}),
        ...(search.exp ? { exp: 1 as const } : {}),
      },
    });
  }

  /** The traces of one model: the same range and flags, with the facet's `models` clause added. */
  function openModel(model: string) {
    const clause = serializeChip({
      kind: "contains",
      field: { column: "models" },
      values: [model],
    });
    void navigate({
      to: "/$org/$project/traces",
      params: { org, project },
      search: {
        ...(range.kind === "preset" ? { range: range.preset } : { from: range.from, to: range.to }),
        filter: appendFilter(search.filter ?? "", clause),
        ...(search.eval ? { eval: 1 as const } : {}),
        ...(search.exp ? { exp: 1 as const } : {}),
      },
    });
  }

  return (
    <div className="flex h-full min-h-0 flex-1 flex-col">
      <TooltipProvider>
        <ListToolbar
          title="Dashboard"
          idPrefix="dashboard"
          range={range}
          onRange={changeRange}
          onRefresh={() => {
            const next = minuteAnchor();
            if (next === anchor) {
              void metrics.refetch();
            } else {
              setAnchor(next);
            }
          }}
          updatedAt={updatedAt}
          evalOn={search.eval === 1}
          onEval={(on) => {
            void navigate({
              to: "/$org/$project/dashboard",
              params: { org, project },
              search: (prev) => withFlag(prev, "eval", on),
            });
          }}
          expOn={search.exp === 1}
          onExp={(on) => {
            void navigate({
              to: "/$org/$project/dashboard",
              params: { org, project },
              search: (prev) => withFlag(prev, "exp", on),
            });
          }}
          filter={
            <div className="flex min-w-0 items-center gap-2">
              <div className="min-w-0 flex-1">
                <FilterBar
                  view="traces"
                  value={search.filter ?? ""}
                  error={filterError}
                  scope={{
                    from: resolved.from,
                    to: resolved.to,
                    includeEval: search.eval === 1,
                    includeExperiments: search.exp === 1,
                  }}
                  onApply={(text) => {
                    void navigate({
                      to: "/$org/$project/dashboard",
                      params: { org, project },
                      search: (prev) => withFilter(prev, text),
                    });
                  }}
                />
              </div>
              <GroupBy
                value={search.group}
                onChange={(group) => {
                  void navigate({
                    to: "/$org/$project/dashboard",
                    params: { org, project },
                    search: (prev) => withGroup(prev, group),
                  });
                }}
              />
            </div>
          }
        />
        <div ref={scrollRef} className="min-h-0 flex-1 overflow-auto p-3">
          {filterError ? (
            <div className="mx-auto flex max-w-md flex-col gap-2 px-4 py-16">
              <h2 className="text-body font-medium">{filterError.title}</h2>
              <p className="text-destructive text-body">{filterMessage(filterError)}</p>
            </div>
          ) : data ? (
            <>
              <KpiRow kpis={data.kpis} trend={kpiTrend(data)} onSelect={scrollToChart} />
              <DashboardBody
                response={data}
                onBucket={openBucket}
                onZoom={zoomTo}
                onModel={openModel}
                sections={sections}
              />
            </>
          ) : (
            <div role="status" aria-label="Loading dashboard">
              <KpiRowSkeleton />
              <DashboardBodySkeleton sections={sections} />
            </div>
          )}
        </div>
      </TooltipProvider>
    </div>
  );
}

function GroupBy({
  value,
  onChange,
}: {
  value: DashboardSearch["group"];
  onChange: (group: DashboardSearch["group"]) => void;
}) {
  return (
    <label className="flex shrink-0 items-center gap-1.5 text-muted-foreground text-caption">
      <span id="dashboard-group-label">Group by</span>
      <select
        aria-labelledby="dashboard-group-label"
        className="focus-ring h-6 rounded-md border border-border bg-background px-1.5 text-foreground text-caption"
        value={value ?? ""}
        onChange={(event) => {
          const next = event.target.value;
          onChange(
            next === "model" || next === "environment" || next === "release" ? next : undefined,
          );
        }}
      >
        <option value="">None</option>
        <option value="model">Model</option>
        <option value="environment">Environment</option>
        <option value="release">Release</option>
      </select>
    </label>
  );
}

function withRange(search: DashboardSearch, state: RangeState): DashboardSearch {
  const { range: _range, from: _from, to: _to, ...rest } = search;
  if (state.kind === "preset") {
    return { ...rest, range: state.preset };
  }
  return { ...rest, from: state.from, to: state.to };
}

function withFilter(search: DashboardSearch, value: string): DashboardSearch {
  const { filter: _filter, ...rest } = search;
  const trimmed = value.trim();
  return trimmed.length > 0 ? { ...rest, filter: trimmed } : rest;
}

function withFlag(search: DashboardSearch, key: "eval" | "exp", on: boolean): DashboardSearch {
  const { [key]: _flag, ...rest } = search;
  return on ? { ...rest, [key]: 1 } : rest;
}

/** The collapsed section titles stored for a project; none when storage is unavailable or invalid. */
function readCollapsed(key: string): Set<string> {
  try {
    const stored: unknown = JSON.parse(localStorage.getItem(key) ?? "[]");
    if (!Array.isArray(stored)) {
      return new Set();
    }
    return new Set(stored.filter((item): item is string => typeof item === "string"));
  } catch {
    return new Set();
  }
}

/** Stores the collapsed sections of a project; without storage the choice lasts for this visit. */
function writeCollapsed(key: string, names: ReadonlySet<string>) {
  try {
    localStorage.setItem(key, JSON.stringify([...names]));
  } catch {
    // Storage is unavailable (private window, blocked site data): the state is kept in memory.
  }
}

function withGroup(search: DashboardSearch, group: DashboardSearch["group"]): DashboardSearch {
  const { group: _group, ...rest } = search;
  return group ? { ...rest, group } : rest;
}
