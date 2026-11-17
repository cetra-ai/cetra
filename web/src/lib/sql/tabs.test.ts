import { act, renderHook } from "@testing-library/react";
import { beforeEach, expect, it, vi } from "vitest";
import { DEFAULT_QUERY } from "@/lib/sql/draft";
import {
  addTab,
  canOpenView,
  closeTab,
  findTabByView,
  MAX_TABS,
  openViewTab,
  readTabs,
  type SqlTab,
  type SqlTabsState,
  selectTab,
  tabsKey,
  tabTitle,
  updateTabDraft,
  useSqlTabs,
} from "@/lib/sql/tabs";

const REF = "acme/support-bot";
const KEY = tabsKey(REF);
const VIEW = "01900000-0000-7000-8000-000000000211";

beforeEach(() => {
  localStorage.clear();
});

function stored(value: unknown) {
  localStorage.setItem(KEY, typeof value === "string" ? value : JSON.stringify(value));
}

/** Tabs with the given queries, the first one active. Numbers 1, 2, 3, … */
function stateOf(...queries: string[]): SqlTabsState {
  let state: SqlTabsState = { tabs: [], active: "" };
  for (const query of queries) {
    state = addTab(state, { query, params: {} });
  }
  const [first] = state.tabs;
  return first ? { tabs: state.tabs, active: first.id } : state;
}

function at(state: SqlTabsState, index: number): SqlTab {
  const tab = state.tabs[index];
  if (tab === undefined) {
    throw new Error(`no tab at ${index}`);
  }
  return tab;
}

it("starts with one default tab when nothing is stored", () => {
  const state = readTabs(REF);
  expect(state.tabs).toHaveLength(1);
  expect(at(state, 0)).toMatchObject({ number: 1, draft: { query: DEFAULT_QUERY, params: {} } });
  expect(state.active).toBe(at(state, 0).id);
});

it("reads stored tabs and their active tab", () => {
  stored({
    tabs: [
      { id: "a", number: 1, draft: { query: "SELECT 1", params: {} } },
      { id: "b", number: 2, draft: { query: "SELECT 2", params: { n: "3" }, view: VIEW } },
    ],
    active: "b",
  });
  expect(readTabs(REF)).toEqual({
    tabs: [
      { id: "a", number: 1, draft: { query: "SELECT 1", params: {} } },
      { id: "b", number: 2, draft: { query: "SELECT 2", params: { n: "3" }, view: VIEW } },
    ],
    active: "b",
  });
});

it("reads one project's tabs without another's", () => {
  localStorage.setItem(
    tabsKey("acme/playground"),
    JSON.stringify({
      tabs: [{ id: "p", number: 1, draft: { query: "SELECT 'play'", params: {} } }],
      active: "p",
    }),
  );
  expect(at(readTabs(REF), 0).draft.query).toBe(DEFAULT_QUERY);
});

it("falls back to the default tab for unreadable JSON or a wrong shape", () => {
  for (const value of ["not json", "[]", '{"tabs":"x"}', '{"tabs":[]}', "null"]) {
    stored(value);
    expect(at(readTabs(REF), 0).draft.query).toBe(DEFAULT_QUERY);
  }
});

it("drops a tab with an invalid field and keeps the others", () => {
  stored({
    tabs: [
      { id: "ok", number: 1, draft: { query: "SELECT 1", params: {} } },
      { id: "", number: 2, draft: { query: "SELECT 2", params: {} } },
      { id: "no-number", number: "2", draft: { query: "SELECT 3", params: {} } },
      { id: "zero", number: 0, draft: { query: "SELECT 4", params: {} } },
      { id: "fraction", number: 1.5, draft: { query: "SELECT 5", params: {} } },
      { id: "no-draft", number: 6 },
      { id: "no-query", number: 7, draft: { params: {} } },
      { id: "query-number", number: 8, draft: { query: 3, params: {} } },
      "not a tab",
      null,
    ],
    active: "ok",
  });
  const state = readTabs(REF);
  expect(state.tabs.map((tab) => tab.id)).toEqual(["ok"]);
});

it("keeps the valid fields of a draft and drops the invalid ones", () => {
  stored({
    tabs: [
      {
        id: "a",
        number: 1,
        draft: { query: "Q", params: { a: "1", b: 2, c: "x" }, view: 5 },
      },
    ],
    active: "a",
  });
  expect(at(readTabs(REF), 0).draft).toEqual({ query: "Q", params: { a: "1", c: "x" } });
});

it("drops a repeated id after its first tab", () => {
  stored({
    tabs: [
      { id: "a", number: 1, draft: { query: "first", params: {} } },
      { id: "a", number: 2, draft: { query: "second", params: {} } },
    ],
    active: "a",
  });
  expect(readTabs(REF).tabs.map((tab) => tab.draft.query)).toEqual(["first"]);
});

it("keeps at most 20 tabs", () => {
  const tabs = Array.from({ length: 25 }, (_, index) => ({
    id: `t${index}`,
    number: index + 1,
    draft: { query: `SELECT ${index}`, params: {} },
  }));
  stored({ tabs, active: "t0" });
  const state = readTabs(REF);
  expect(state.tabs).toHaveLength(MAX_TABS);
  expect(state.tabs.at(-1)?.id).toBe("t19");
});

it("falls back to the first tab when the active id is not a kept tab", () => {
  stored({
    tabs: [
      { id: "a", number: 1, draft: { query: "A", params: {} } },
      { id: "b", number: 2, draft: { query: "B", params: {} } },
    ],
    active: "gone",
  });
  expect(readTabs(REF).active).toBe("a");
});

it("falls back to the first tab when active is not a string", () => {
  stored({
    tabs: [{ id: "a", number: 1, draft: { query: "A", params: {} } }],
    active: 3,
  });
  expect(readTabs(REF).active).toBe("a");
});

it("gives every fresh tab a new id", () => {
  const first = readTabs(REF).active;
  localStorage.clear();
  expect(readTabs(REF).active).not.toBe(first);
  expect(first).toMatch(/^[0-9a-z-]+$/);
});

it("addTab appends a tab numbered after the largest and makes it active", () => {
  const before = stateOf("A", "B");
  const after = addTab(before);
  expect(after.tabs).toHaveLength(3);
  expect(at(after, 2)).toMatchObject({ number: 3, draft: { query: "", params: {} } });
  expect(after.active).toBe(at(after, 2).id);
});

it("addTab numbers after the largest number left after closing", () => {
  const base = stateOf("A", "B", "C");
  const state = closeTab(base, at(base, 1).id);
  expect(at(addTab(state), 2).number).toBe(4);
});

it("addTab takes the draft it is given", () => {
  const after = addTab(stateOf("A"), { query: "SELECT 9", params: { n: "1" } });
  expect(at(after, 1).draft).toEqual({ query: "SELECT 9", params: { n: "1" } });
});

it("addTab is refused at 20 tabs", () => {
  let state = stateOf();
  while (state.tabs.length < MAX_TABS) {
    state = addTab(state);
  }
  expect(addTab(state)).toBe(state);
});

it("closeTab on the active tab activates its right neighbour", () => {
  const base = stateOf("A", "B", "C");
  const state = selectTab(base, at(base, 1).id);
  const closed = closeTab(state, at(state, 1).id);
  expect(closed.tabs.map((tab) => tab.draft.query)).toEqual(["A", "C"]);
  expect(closed.active).toBe(at(state, 2).id);
});

it("closeTab on the last tab, when active, activates its left neighbour", () => {
  const base = stateOf("A", "B", "C");
  const state = selectTab(base, at(base, 2).id);
  const closed = closeTab(state, at(state, 2).id);
  expect(closed.tabs.map((tab) => tab.draft.query)).toEqual(["A", "B"]);
  expect(closed.active).toBe(at(state, 1).id);
});

it("closeTab on another tab keeps the active one", () => {
  const state = stateOf("A", "B", "C");
  const closed = closeTab(state, at(state, 2).id);
  expect(closed.active).toBe(at(state, 0).id);
  expect(closed.tabs.map((tab) => tab.draft.query)).toEqual(["A", "B"]);
});

it("closeTab of the only tab leaves one fresh default tab", () => {
  const state = stateOf("A");
  const closed = closeTab(state, at(state, 0).id);
  expect(closed.tabs).toHaveLength(1);
  expect(at(closed, 0)).toMatchObject({ number: 1, draft: { query: DEFAULT_QUERY, params: {} } });
  expect(closed.active).toBe(at(closed, 0).id);
  expect(at(closed, 0).id).not.toBe(at(state, 0).id);
});

it("closeTab of an unknown id leaves the state unchanged", () => {
  const state = stateOf("A", "B");
  expect(closeTab(state, "nope")).toBe(state);
});

it("selectTab makes a tab active and ignores unknown ids", () => {
  const state = stateOf("A", "B");
  const selected = selectTab(state, at(state, 1).id);
  expect(selected.active).toBe(at(state, 1).id);
  expect(selectTab(state, "nope")).toBe(state);
  expect(selectTab(state, state.active)).toBe(state);
});

it("updateTabDraft changes only that tab's draft", () => {
  const state = stateOf("A", "B");
  const updated = updateTabDraft(state, at(state, 1).id, (draft) => ({
    ...draft,
    query: `${draft.query}!`,
  }));
  expect(updated.tabs.map((tab) => tab.draft.query)).toEqual(["A", "B!"]);
  expect(updated.active).toBe(state.active);
  expect(updateTabDraft(state, "nope", (draft) => draft)).toBe(state);
});

it("findTabByView finds the tab that holds the saved query", () => {
  const base = stateOf("A", "B");
  const state = updateTabDraft(base, at(base, 1).id, (draft) => ({ ...draft, view: VIEW }));
  expect(findTabByView(state, VIEW)?.draft.query).toBe("B");
  expect(findTabByView(state, "other")).toBeUndefined();
});

it("openViewTab selects the tab that holds the view, or adds one", () => {
  const state = stateOf("A");
  const opened = openViewTab(state, { id: VIEW, draft: { query: "V", params: {}, view: VIEW } });
  expect(opened.tabs).toHaveLength(2);
  expect(at(opened, 1).draft.view).toBe(VIEW);
  expect(opened.active).toBe(at(opened, 1).id);
  const moved = selectTab(opened, at(opened, 0).id);
  const again = openViewTab(moved, { id: VIEW, draft: { query: "other", params: {} } });
  expect(again.tabs).toHaveLength(2);
  expect(again.active).toBe(at(opened, 1).id);
});

it("openViewTab is refused at 20 tabs", () => {
  let state = stateOf();
  while (state.tabs.length < MAX_TABS) {
    state = addTab(state, { query: "SELECT 1", params: {} });
  }
  expect(openViewTab(state, { id: VIEW, draft: { query: "V", params: {}, view: VIEW } })).toBe(
    state,
  );
  expect(canOpenView(state, VIEW)).toBe(false);
});

it("openViewTab reuses a blank active tab that holds no saved query", () => {
  const state = stateOf("A", "");
  const active = selectTab(state, at(state, 1).id);
  const opened = openViewTab(active, { id: VIEW, draft: { query: "V", params: {}, view: VIEW } });
  expect(opened.tabs).toHaveLength(2);
  expect(opened.active).toBe(at(active, 1).id);
  expect(at(opened, 1).draft).toEqual({ query: "V", params: {}, view: VIEW });
});

it("openViewTab reuses a whitespace-only active tab", () => {
  const state = stateOf("   ");
  const opened = openViewTab(state, { id: VIEW, draft: { query: "V", params: {}, view: VIEW } });
  expect(opened.tabs).toHaveLength(1);
  expect(at(opened, 0).draft.view).toBe(VIEW);
});

it("openViewTab adds a tab when the active tab has a query or a view", () => {
  const filled = stateOf("A");
  expect(
    openViewTab(filled, { id: VIEW, draft: { query: "V", params: {}, view: VIEW } }).tabs,
  ).toHaveLength(2);
  const blank = stateOf("");
  const withView = updateTabDraft(blank, at(blank, 0).id, (draft) => ({
    ...draft,
    view: "other",
  }));
  expect(
    openViewTab(withView, { id: VIEW, draft: { query: "V", params: {}, view: VIEW } }).tabs,
  ).toHaveLength(2);
});

it("openViewTab selects the tab holding the view before reusing a blank one", () => {
  const base = stateOf("A", "");
  const held = updateTabDraft(base, at(base, 0).id, (draft) => ({ ...draft, view: VIEW }));
  const state = selectTab(held, at(held, 1).id);
  const opened = openViewTab(state, { id: VIEW, draft: { query: "V", params: {}, view: VIEW } });
  expect(opened.tabs).toHaveLength(2);
  expect(opened.active).toBe(at(held, 0).id);
});

it("canOpenView allows a blank active tab even at 20 tabs", () => {
  let state = stateOf();
  while (state.tabs.length < MAX_TABS) {
    state = addTab(state, { query: "SELECT 1", params: {} });
  }
  const blank = updateTabDraft(state, state.active, () => ({ query: " ", params: {} }));
  expect(canOpenView(blank, VIEW)).toBe(true);
});

it("tabTitle uses the saved query's name when the view is listed", () => {
  const tab: SqlTab = { id: "a", number: 4, draft: { query: "Q", params: {}, view: VIEW } };
  expect(tabTitle(tab, [{ id: VIEW, name: "Costs" }])).toBe("Costs");
  expect(tabTitle(tab, [])).toBe("Query 4");
  expect(tabTitle(tab, undefined)).toBe("Query 4");
});

it("tabTitle is Query <number> for a tab without a view", () => {
  const tab: SqlTab = { id: "a", number: 2, draft: { query: "Q", params: {} } };
  expect(tabTitle(tab, [{ id: VIEW, name: "Costs" }])).toBe("Query 2");
});

it("the hook writes each transition through to localStorage", () => {
  const { result } = renderHook(() => useSqlTabs(REF));
  act(() => {
    result.current.add({ query: "SELECT 2", params: {} });
  });
  expect(result.current.state.tabs).toHaveLength(2);
  expect(result.current.activeTab.draft.query).toBe("SELECT 2");
  const stored = JSON.parse(localStorage.getItem(KEY) ?? "null") as SqlTabsState;
  expect(stored.tabs).toHaveLength(2);
  expect(stored.active).toBe(result.current.activeTab.id);
});

it("the hook applies two draft updates in one act", () => {
  const { result } = renderHook(() => useSqlTabs(REF));
  const id = result.current.activeTab.id;
  act(() => {
    result.current.updateDraft(id, (draft) => ({ ...draft, query: `${draft.query}A` }));
    result.current.updateDraft(id, (draft) => ({ ...draft, query: `${draft.query}A` }));
  });
  expect(result.current.activeTab.draft.query).toBe(`${DEFAULT_QUERY}AA`);
});

it("the hook closes, selects and opens tabs", () => {
  const { result } = renderHook(() => useSqlTabs(REF));
  const first = result.current.activeTab.id;
  act(() => {
    result.current.open({ id: VIEW, draft: { query: "V", params: {}, view: VIEW } });
  });
  expect(result.current.activeTab.draft.view).toBe(VIEW);
  act(() => {
    result.current.select(first);
  });
  expect(result.current.activeTab.id).toBe(first);
  act(() => {
    result.current.close(first);
  });
  expect(result.current.activeTab.draft.view).toBe(VIEW);
});

it("the hook keeps the tabs when localStorage throws", () => {
  const { result } = renderHook(() => useSqlTabs(REF));
  const setItem = vi.spyOn(Storage.prototype, "setItem").mockImplementation(() => {
    throw new Error("quota");
  });
  try {
    act(() => {
      result.current.add();
    });
    expect(result.current.state.tabs).toHaveLength(2);
  } finally {
    setItem.mockRestore();
  }
});
