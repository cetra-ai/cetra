import { useCallback, useEffect, useState } from "react";
import { sqlTabDraft } from "@/lib/assistant/apply";
import { subscribeDrafts, takeDraft } from "@/lib/assistant/handoff";
import { DEFAULT_QUERY, isRecord, type SqlDraft } from "@/lib/sql/draft";
import { STORAGE_PREFIX } from "@/product";

/** The most query tabs one project keeps open. */
export const MAX_TABS = 20;

export type SqlTab = { id: string; number: number; draft: SqlDraft };

export type SqlTabsState = { tabs: SqlTab[]; active: string };

/** The tab state of `projectRef`, in localStorage. */
export function tabsKey(projectRef: string): string {
  return `${STORAGE_PREFIX}sql.tabs.${projectRef}`;
}

/**
 * A tab id, unique in this browser only: the ids never leave localStorage. `crypto.randomUUID`
 * is not used because it is missing over plain http (not a secure context).
 */
function newId(): string {
  return `${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 10)}`;
}

function newTab(number: number, draft: SqlDraft): SqlTab {
  return { id: newId(), number, draft };
}

/** One tab with the default query: the state of a project with nothing stored. */
function freshState(): SqlTabsState {
  const tab = newTab(1, { query: DEFAULT_QUERY, params: {} });
  return { tabs: [tab], active: tab.id };
}

/** A draft as stored, or undefined when it is not one. */
function parseDraft(value: unknown): SqlDraft | undefined {
  if (!isRecord(value) || typeof value.query !== "string") {
    return undefined;
  }
  const params: Record<string, string> = {};
  if (isRecord(value.params)) {
    for (const [name, param] of Object.entries(value.params)) {
      if (typeof param === "string") {
        params[name] = param;
      }
    }
  }
  const view = typeof value.view === "string" ? value.view : undefined;
  return view ? { query: value.query, params, view } : { query: value.query, params };
}

/** A tab as stored, or undefined when any of its fields is not valid. */
function parseTab(value: unknown): SqlTab | undefined {
  if (!isRecord(value)) {
    return undefined;
  }
  const { id, number, draft } = value;
  const parsed = parseDraft(draft);
  if (
    typeof id !== "string" ||
    id === "" ||
    typeof number !== "number" ||
    !Number.isSafeInteger(number) ||
    number < 1 ||
    parsed === undefined
  ) {
    return undefined;
  }
  return { id, number, draft: parsed };
}

/**
 * The tabs of `projectRef`. Invalid tabs and repeated ids are dropped, at most `MAX_TABS` are
 * kept, and when none is left the project starts with the default query. An `active` that is
 * not a kept tab falls back to the first one.
 */
export function readTabs(projectRef: string): SqlTabsState {
  let raw: unknown;
  try {
    const text = localStorage.getItem(tabsKey(projectRef));
    if (text === null) {
      return freshState();
    }
    raw = JSON.parse(text);
  } catch {
    return freshState();
  }
  if (!isRecord(raw) || !Array.isArray(raw.tabs)) {
    return freshState();
  }
  const seen = new Set<string>();
  const tabs: SqlTab[] = [];
  for (const item of raw.tabs) {
    const tab = parseTab(item);
    if (tab === undefined || seen.has(tab.id)) {
      continue;
    }
    seen.add(tab.id);
    tabs.push(tab);
    if (tabs.length === MAX_TABS) {
      break;
    }
  }
  const [first] = tabs;
  if (first === undefined) {
    return freshState();
  }
  const active = typeof raw.active === "string" && seen.has(raw.active) ? raw.active : first.id;
  return { tabs, active };
}

export function writeTabs(projectRef: string, state: SqlTabsState): void {
  try {
    localStorage.setItem(tabsKey(projectRef), JSON.stringify(state));
  } catch {
    // Private mode or a full quota: the tabs still live in memory for this page.
  }
}

/** The tab that is shown. A state always holds at least one tab. */
export function activeTabOf(state: SqlTabsState): SqlTab {
  const [first] = state.tabs;
  const active = state.tabs.find((tab) => tab.id === state.active) ?? first;
  return active ?? newTab(1, { query: DEFAULT_QUERY, params: {} });
}

/**
 * A new tab after the last one, with the next number, and active. Refused (the state comes back
 * unchanged) at `MAX_TABS`.
 */
export function addTab(
  state: SqlTabsState,
  draft: SqlDraft = { query: "", params: {} },
): SqlTabsState {
  if (state.tabs.length >= MAX_TABS) {
    return state;
  }
  const number = Math.max(0, ...state.tabs.map((tab) => tab.number)) + 1;
  const tab = newTab(number, draft);
  return { tabs: [...state.tabs, tab], active: tab.id };
}

/**
 * Removes a tab. Closing the active one activates its right neighbour, or the left one when it
 * was last. Closing the only tab leaves one fresh tab with the default query.
 */
export function closeTab(state: SqlTabsState, id: string): SqlTabsState {
  const index = state.tabs.findIndex((tab) => tab.id === id);
  if (index === -1) {
    return state;
  }
  const remaining = state.tabs.filter((tab) => tab.id !== id);
  const [first] = remaining;
  if (first === undefined) {
    const fresh = newTab(1, { query: DEFAULT_QUERY, params: {} });
    return { tabs: [fresh], active: fresh.id };
  }
  if (state.active !== id) {
    return { tabs: remaining, active: state.active };
  }
  const neighbour = state.tabs[index + 1] ?? state.tabs[index - 1];
  return { tabs: remaining, active: neighbour?.id ?? first.id };
}

/** Makes the tab `id` the active one. Unknown ids and the active tab leave the state unchanged. */
export function selectTab(state: SqlTabsState, id: string): SqlTabsState {
  if (state.active === id || !state.tabs.some((tab) => tab.id === id)) {
    return state;
  }
  return { tabs: state.tabs, active: id };
}

/** Changes the draft of tab `id`. An unknown id leaves the state unchanged. */
export function updateTabDraft(
  state: SqlTabsState,
  id: string,
  update: (previous: SqlDraft) => SqlDraft,
): SqlTabsState {
  if (!state.tabs.some((tab) => tab.id === id)) {
    return state;
  }
  return {
    tabs: state.tabs.map((tab) => (tab.id === id ? { ...tab, draft: update(tab.draft) } : tab)),
    active: state.active,
  };
}

/** The tab whose draft was loaded from the saved query `viewId`. */
export function findTabByView(state: SqlTabsState, viewId: string): SqlTab | undefined {
  return state.tabs.find((tab) => tab.draft.view === viewId);
}

/**
 * Shows the saved query `view`: the tab that holds it is selected, otherwise a new tab opens
 * with the draft. Refused at `MAX_TABS` like `addTab`.
 */
export function openViewTab(
  state: SqlTabsState,
  view: { id: string; draft: SqlDraft },
): SqlTabsState {
  const existing = findTabByView(state, view.id);
  if (existing) {
    return selectTab(state, existing.id);
  }
  if (reusesActive(state)) {
    return updateTabDraft(state, state.active, () => view.draft);
  }
  return addTab(state, view.draft);
}

/** Whether the active tab is blank and holds no saved query, so a saved query opens in it. */
function reusesActive(state: SqlTabsState): boolean {
  const active = activeTabOf(state);
  return active.draft.query.trim() === "" && active.draft.view === undefined;
}

/** Whether `openViewTab` can show the saved query `viewId` (it is held, reused, or a tab is free). */
export function canOpenView(state: SqlTabsState, viewId: string): boolean {
  return (
    findTabByView(state, viewId) !== undefined ||
    reusesActive(state) ||
    state.tabs.length < MAX_TABS
  );
}

/** The saved query's name when the tab holds a listed one, else `Query <number>`. */
export function tabTitle(
  tab: SqlTab,
  savedViews: readonly { id: string; name: string }[] | undefined,
): string {
  const view = tab.draft.view ? savedViews?.find((item) => item.id === tab.draft.view) : undefined;
  return view ? view.name : `Query ${tab.number}`;
}

export type SqlTabs = {
  state: SqlTabsState;
  activeTab: SqlTab;
  add: (draft?: SqlDraft) => void;
  close: (id: string) => void;
  select: (id: string) => void;
  updateDraft: (id: string, update: (previous: SqlDraft) => SqlDraft) => void;
  open: (view: { id: string; draft: SqlDraft }) => void;
};

/**
 * The query tabs of `projectRef`. Each transition is written through to localStorage. Every
 * change is applied to the latest state, so two updates in one event both apply.
 */
export function useSqlTabs(projectRef: string): SqlTabs {
  const [state, setState] = useState(() => readTabs(projectRef));
  const commit = useCallback(
    (change: (previous: SqlTabsState) => SqlTabsState) => {
      setState((previous) => {
        const next = change(previous);
        if (next === previous) {
          return previous;
        }
        writeTabs(projectRef, next);
        return next;
      });
    },
    [projectRef],
  );
  const add = useCallback((draft?: SqlDraft) => commit((s) => addTab(s, draft)), [commit]);
  const close = useCallback((id: string) => commit((s) => closeTab(s, id)), [commit]);
  const select = useCallback((id: string) => commit((s) => selectTab(s, id)), [commit]);
  const updateDraft = useCallback(
    (id: string, update: (previous: SqlDraft) => SqlDraft) =>
      commit((s) => updateTabDraft(s, id, update)),
    [commit],
  );
  const open = useCallback(
    (view: { id: string; draft: SqlDraft }) => commit((s) => openViewTab(s, view)),
    [commit],
  );
  // A SQL draft the assistant handed over opens in a new tab, here or on mount (D-AS-28).
  useEffect(() => {
    function takeSqlDraft() {
      const draft = takeDraft(projectRef, "sql");
      if (draft !== undefined) {
        add(sqlTabDraft(draft));
      }
    }
    takeSqlDraft();
    return subscribeDrafts(projectRef, takeSqlDraft);
  }, [projectRef, add]);
  return {
    state,
    activeTab: activeTabOf(state),
    add,
    close,
    select,
    updateDraft,
    open,
  };
}
