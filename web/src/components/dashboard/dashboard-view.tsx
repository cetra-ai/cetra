import { useQuery, useQueryClient } from "@tanstack/react-query";
import { Link, useNavigate } from "@tanstack/react-router";
import { type ReactNode, useState } from "react";
import { dashboardQuery, WIDGET_QUERY_PATH } from "@/api/dashboards";
import { problemMessage, problemOf } from "@/api/problem";
import { FilterBar } from "@/components/filter-bar/filter-bar";
import { filterMessage } from "@/components/filter-bar/logic";
import { ListToolbar } from "@/components/list-toolbar";
import { useProject } from "@/components/shell/project-context";
import { Skeleton } from "@/components/ui/skeleton";
import { TooltipProvider } from "@/components/ui/tooltip";
import type { Widget as WidgetModel } from "@/lib/dashboards/types";
import { hasTopLevelOr, serializeChip } from "@/lib/dsl/dsl";
import type { DashboardSearch } from "@/lib/search/dashboard";
import { type RangeState, rangeFromSearch, resolveRange } from "@/lib/time-range";
import { DashboardBody } from "./dashboard-body";
import { useModelRanking } from "./use-model-ranking";
import { type BucketTarget, Widget, type WidgetContext } from "./widget";

/** The window's end is rounded down to the minute, so visits within one minute share a cache key. */
function minuteAnchor(now = Date.now()): number {
  return Math.floor(now / 60_000) * 60_000;
}

/**
 * The dashboard for the given URL search: the Overview, or the custom dashboard `dashboardId`.
 * The project layout renders it and keeps it mounted (hidden) on other project pages; the
 * dashboard routes themselves render nothing. `active` is whether it is the one on screen.
 */
export function DashboardView({
  search,
  dashboardId,
  active,
}: {
  search: DashboardSearch;
  dashboardId?: string;
  active: boolean;
}) {
  const { org, project } = useProject();
  const navigate = useNavigate();
  const queryClient = useQueryClient();
  const dashboard = useQuery({
    ...dashboardQuery(org, project, dashboardId ?? ""),
    enabled: dashboardId !== undefined,
  });
  const range = rangeFromSearch(search);
  const rangeKey = range.kind === "preset" ? range.preset : `${range.from}\n${range.to}`;
  const [anchor, setAnchor] = useState(() => minuteAnchor());
  const [seenRange, setSeenRange] = useState(rangeKey);
  if (seenRange !== rangeKey) {
    setSeenRange(rangeKey);
    setAnchor(minuteAnchor());
  }
  const resolved = resolveRange(range, anchor);
  const base: WidgetContext = {
    org,
    project,
    from: resolved.from,
    to: resolved.to,
    ...(search.filter ? { filter: search.filter } : {}),
    ...(search.group ? { groupBy: search.group } : {}),
    includeEval: search.eval === 1,
    includeExperiments: search.exp === 1,
  };
  const ranking = useModelRanking(base);
  // A dashboard filter the server refuses is shown in the filter bar; the widgets then stay blank.
  const rankingProblem = problemOf(ranking.error);
  const filterError = rankingProblem?.code === "invalid_filter" ? rankingProblem : undefined;
  const context: WidgetContext = { ...base, ranking: ranking.ranking };

  /** Changes the URL search of the open dashboard. Its id stays, so the switch keeps the dashboard. */
  function goDashboard(updater: (prev: DashboardSearch) => DashboardSearch) {
    if (dashboardId === undefined) {
      void navigate({ to: "/$org/$project/dashboard", params: { org, project }, search: updater });
      return;
    }
    void navigate({
      to: "/$org/$project/dashboard/$dashboardId",
      params: { org, project, dashboardId },
      search: updater,
    });
  }

  /** Shows another window: the toolbar's range picker and a zoom drag both come here. */
  function changeRange(next: RangeState) {
    setAnchor(minuteAnchor());
    goDashboard((prev) => withRange(prev, next));
  }

  function zoomTo(fromMs: number, toMs: number) {
    changeRange({
      kind: "absolute",
      from: new Date(fromMs).toISOString(),
      to: new Date(toMs).toISOString(),
    });
  }

  /**
   * Opens the list of a clicked bucket. A traces widget opens the traces with the dashboard's
   * filter; a spans widget opens the spans with its own filter only, since the dashboard's filter
   * is a traces expression. Both get the widget's filter and the bucket's group.
   */
  function openBucket(widget: WidgetModel, target: BucketTarget) {
    const source = widget.query?.source;
    const filter = andAll([
      source === "traces" ? search.filter : undefined,
      widget.query?.filter,
      target.clause,
    ]);
    if (source === "spans") {
      void navigate({
        to: "/$org/$project/spans",
        params: { org, project },
        search: { from: target.from, to: target.to, ...(filter ? { filter } : {}) },
      });
      return;
    }
    void navigate({
      to: "/$org/$project/traces",
      params: { org, project },
      search: {
        from: target.from,
        to: target.to,
        ...(filter ? { filter } : {}),
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
        filter: andAll([search.filter, clause]) ?? "",
        ...(search.eval ? { eval: 1 as const } : {}),
        ...(search.exp ? { exp: 1 as const } : {}),
      },
    });
  }

  /**
   * One widget. In edit mode (`edit` given) nothing opens or zooms, so a click on a chart does not
   * take the user away from the draft; `edit.actions` goes in the widget's title bar.
   */
  const renderWidget = (widget: WidgetModel, edit?: { actions: ReactNode }) => {
    const clickable = edit === undefined && widget.query?.source !== "scores";
    return (
      <Widget
        widget={widget}
        context={context}
        actions={edit?.actions}
        onBucket={clickable ? (target) => openBucket(widget, target) : undefined}
        onZoom={clickable ? zoomTo : undefined}
        onModel={edit === undefined ? openModel : undefined}
      />
    );
  };

  let content: ReactNode;
  if (filterError) {
    content = (
      <div className="mx-auto flex max-w-md flex-col gap-2 px-4 py-16">
        <h2 className="text-body font-medium">{filterError.title}</h2>
        <p className="text-destructive text-body">{filterMessage(filterError)}</p>
      </div>
    );
  } else if (dashboardId === undefined || dashboard.data) {
    content = (
      <DashboardBody
        dashboard={dashboard.data}
        active={active}
        context={context}
        renderWidget={renderWidget}
      />
    );
  } else if (problemOf(dashboard.error)?.status === 404) {
    content = (
      <div className="mx-auto flex max-w-md flex-col gap-2 px-4 py-16">
        <h2 className="text-body font-medium">Dashboard not found</h2>
        <p className="text-muted-foreground text-body">
          It may have been deleted. Open the Overview instead.
        </p>
        <Link
          to="/$org/$project/dashboard"
          params={{ org, project }}
          search={(prev) => prev}
          className="text-primary text-body underline-offset-4 hover:underline"
        >
          Go to the Overview
        </Link>
      </div>
    );
  } else if (dashboard.error) {
    content = (
      <p role="alert" className="text-destructive text-body">
        {problemMessage(dashboard.error, "Could not load the dashboard.")}
      </p>
    );
  } else {
    content = <Skeleton className="h-64" />;
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
              void queryClient.invalidateQueries({
                predicate: (query) => query.queryKey[1] === WIDGET_QUERY_PATH,
              });
            } else {
              setAnchor(next);
            }
          }}
          updatedAt={filterError ? undefined : ranking.updatedAt}
          evalOn={search.eval === 1}
          onEval={(on) => goDashboard((prev) => withFlag(prev, "eval", on))}
          expOn={search.exp === 1}
          onExp={(on) => goDashboard((prev) => withFlag(prev, "exp", on))}
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
                  onApply={(text) => goDashboard((prev) => withFilter(prev, text))}
                />
              </div>
              <GroupBy
                value={search.group}
                onChange={(group) => goDashboard((prev) => withGroup(prev, group))}
              />
            </div>
          }
        />
        <div className="min-h-0 flex-1 overflow-auto p-3">{content}</div>
      </TooltipProvider>
    </div>
  );
}

/** The conjuncts of a filter, each parenthesized when it has a top-level OR; empty parts are left out. */
function andAll(parts: readonly (string | undefined)[]): string | undefined {
  const kept = parts
    .map((part) => part?.trim() ?? "")
    .filter((part) => part.length > 0)
    .map((part) => (hasTopLevelOr(part) ? `(${part})` : part));
  return kept.length > 0 ? kept.join(" AND ") : undefined;
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

function withGroup(search: DashboardSearch, group: DashboardSearch["group"]): DashboardSearch {
  const { group: _group, ...rest } = search;
  return group ? { ...rest, group } : rest;
}
