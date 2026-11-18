import { useQuery } from "@tanstack/react-query";
import { PanelLeft } from "lucide-react";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { retryQuery } from "@/api/client";
import { savedQueriesQuery, savedViewQuery, schemaQuery } from "@/api/queries";
import type { Schema } from "@/api/types";
import { useProject } from "@/components/shell/project-context";
import { QueryTabs } from "@/components/sql/query-tabs";
import { SqlSidebar } from "@/components/sql/sql-sidebar";
import { SqlTabBody } from "@/components/sql/sql-tab-body";
import { TimeRangePicker } from "@/components/time-range/time-range-picker";
import { Button } from "@/components/ui/button";
import { TooltipProvider } from "@/components/ui/tooltip";
import { type SqlSearch, withoutView, withSqlRange } from "@/lib/search/sql";
import type { SqlDraft } from "@/lib/sql/draft";
import { usedParams } from "@/lib/sql/params";
import { parseSavedState, searchOfSavedState } from "@/lib/sql/saved-state";
import { readSchemaPanelOpen, writeSchemaPanelOpen } from "@/lib/sql/schema-panel";
import { canOpenView, findTabByView, useSqlTabs } from "@/lib/sql/tabs";
import { rangeFromSearch } from "@/lib/time-range";

/** The draft a saved query opens with: its text, the parameters it uses, and its id. */
function draftOf(view: Schema<"SavedView">): SqlDraft {
  const state = parseSavedState(view.state);
  return { query: state.query, params: usedParams(state.query, state.params), view: view.id };
}

export function SqlConsole({
  search,
  onSearch,
}: {
  search: SqlSearch;
  /**
   * Changes the URL search. Always given the function form, like `navigate({ search })`.
   * `replace` replaces the history entry instead of pushing one.
   */
  onSearch: (update: (previous: SqlSearch) => SqlSearch, options?: { replace?: boolean }) => void;
}) {
  const { org, project, ref } = useProject();
  const schema = useQuery(schemaQuery(org, project));
  const tabs = useSqlTabs(ref);
  const { activeTab } = tabs;
  const [schemaOpen, setSchemaOpen] = useState(readSchemaPanelOpen);
  const [insert, setInsert] = useState<{ text: string; seq: number; tabId: string } | undefined>();
  /** The ids of the tabs whose query runs. */
  const [running, setRunning] = useState<ReadonlySet<string>>(() => new Set());
  const range = rangeFromSearch(search);
  const functionNames = useMemo(
    () => schema.data?.functions.filter((fn) => fn.in_sql).map((fn) => fn.name) ?? [],
    [schema.data],
  );
  const savedQueries = useQuery(savedQueriesQuery(org, project));
  const listed = savedQueries.data?.data;
  const listedView = search.view ? listed?.find((item) => item.id === search.view) : undefined;
  /** A `?view=` id the loaded list does not hold (shared after it loaded): read by id. */
  const byId = useQuery({
    ...savedViewQuery(org, project, search.view ?? ""),
    enabled: savedQueries.isSuccess && Boolean(search.view) && listedView === undefined,
    // A 4xx (missing, private, malformed) will not change on retry: drop the link at once.
    retry: retryQuery,
  });
  const fetchedView = byId.data?.id === search.view ? byId.data : undefined;
  /** The `?view=` id the effect below has already handled. */
  const handledView = useRef<string | undefined>(undefined);
  /** The active tab the URL was last moved to. The first one is the page's own URL. */
  const syncedTab = useRef(activeTab.id);

  /** Reloads a saved query into the tab `tabId` ("Discard changes"), with its range. */
  function loadInto(tabId: string, view: Schema<"SavedView">) {
    const state = parseSavedState(view.state);
    tabs.updateDraft(tabId, () => draftOf(view));
    onSearch((previous) => searchOfSavedState(state, previous, view.id), { replace: true });
  }

  /**
   * Shows a saved query from the sidebar: the URL takes its id and range, and the `?view=`
   * effect below opens it in a tab or selects the tab that holds it.
   */
  function openFromList(view: Schema<"SavedView">) {
    const state = parseSavedState(view.state);
    onSearch((previous) => searchOfSavedState(state, previous, view.id), { replace: true });
  }

  /** The tab `tabId`'s draft was saved as a new query. */
  function savedInto(tabId: string, view: Schema<"SavedView">) {
    tabs.updateDraft(tabId, (previous) => ({ ...previous, view: view.id }));
    onSearch((previous) => ({ ...previous, view: view.id }), { replace: true });
  }

  /** The saved query of tab `tabId` was deleted. */
  function clearedIn(tabId: string) {
    tabs.updateDraft(tabId, ({ view: _view, ...rest }) => rest);
    onSearch(withoutView, { replace: true });
  }

  /** Drops a `?view=` link that cannot be read or cannot get a tab. */
  function dropLink() {
    onSearch(withoutView, { replace: true });
  }

  // biome-ignore lint/correctness/useExhaustiveDependencies: tabs and dropLink are recreated every render; handledView is a ref
  useEffect(() => {
    const id = search.view;
    if (!id) {
      handledView.current = undefined;
      return;
    }
    if (!savedQueries.isSuccess) {
      return;
    }
    const view = listedView ?? fetchedView;
    if (!view) {
      handledView.current = undefined;
      // Neither listed nor readable by id (missing, private, or the read failed): drop the link.
      if (byId.isError) {
        dropLink();
      }
      return;
    }
    // A list refetch that lands before the URL catches up (Save as) must not reopen the view.
    if (handledView.current === id) {
      return;
    }
    handledView.current = id;
    const held = findTabByView(tabs.state, id) !== undefined;
    if (!canOpenView(tabs.state, id)) {
      dropLink();
      return;
    }
    tabs.open({ id, draft: draftOf(view) });
    if (!held) {
      // A query opened from a link takes its range, as a query loaded into the page always did.
      const state = parseSavedState(view.state);
      onSearch((previous) => searchOfSavedState(state, previous, id), { replace: true });
    }
  }, [search.view, savedQueries.isSuccess, listedView, fetchedView, byId.isError]);

  // A switch of the active tab moves the URL to that tab's saved query (or away from one).
  // biome-ignore lint/correctness/useExhaustiveDependencies: only a change of the active tab moves the URL; dropLink is recreated every render
  useEffect(() => {
    if (syncedTab.current === activeTab.id) {
      return;
    }
    syncedTab.current = activeTab.id;
    const viewId = activeTab.draft.view;
    onSearch((previous) => (viewId ? { ...previous, view: viewId } : withoutView(previous)), {
      replace: true,
    });
  }, [activeTab.id]);

  const onRunningChange = useCallback((tabId: string, isRunning: boolean) => {
    setRunning((previous) => {
      if (previous.has(tabId) === isRunning) {
        return previous;
      }
      const next = new Set(previous);
      if (isRunning) {
        next.add(tabId);
      } else {
        next.delete(tabId);
      }
      return next;
    });
  }, []);

  return (
    <TooltipProvider>
      <div className="flex h-full min-h-0 flex-1 flex-col">
        <div className="flex min-h-9 shrink-0 items-center gap-2 border-b px-3">
          <Button
            type="button"
            variant="ghost"
            size="icon-xs"
            aria-label="Schema"
            aria-pressed={schemaOpen}
            onClick={() => {
              const next = !schemaOpen;
              setSchemaOpen(next);
              writeSchemaPanelOpen(next);
            }}
          >
            <PanelLeft />
          </Button>
          <QueryTabs
            tabs={tabs.state.tabs}
            activeId={activeTab.id}
            savedViews={listed}
            running={running}
            onSelect={tabs.select}
            onClose={tabs.close}
            onAdd={() => tabs.add()}
          />
          <TimeRangePicker
            state={range}
            onChange={(next) => onSearch((previous) => withSqlRange(previous, next))}
          />
        </div>
        {savedQueries.isError ? (
          <p role="alert" className="px-3 py-1 text-destructive text-caption">
            Saved queries could not be loaded.
          </p>
        ) : null}
        <div className="flex min-h-0 flex-1">
          {schemaOpen ? (
            <SqlSidebar
              schema={schema.data}
              failed={schema.isError}
              onInsert={(text) =>
                setInsert((previous) => ({
                  text,
                  seq: (previous?.seq ?? 0) + 1,
                  tabId: activeTab.id,
                }))
              }
              viewId={activeTab.draft.view}
              onLoad={openFromList}
            />
          ) : null}
          <div className="flex min-h-0 flex-1 flex-col">
            {tabs.state.tabs.map((tab) => (
              <SqlTabBody
                key={tab.id}
                tab={tab}
                active={tab.id === activeTab.id}
                schema={schema.data}
                functionNames={functionNames}
                range={range}
                search={search}
                insert={insert?.tabId === tab.id ? insert : undefined}
                updateDraft={(update) => tabs.updateDraft(tab.id, update)}
                onLoadView={(view) => loadInto(tab.id, view)}
                onSavedView={(view) => savedInto(tab.id, view)}
                onClearedView={() => clearedIn(tab.id)}
                onRunningChange={onRunningChange}
              />
            ))}
          </div>
        </div>
      </div>
    </TooltipProvider>
  );
}
