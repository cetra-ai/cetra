# Lists come back from cache, newer rows behind a pill

Plan for Haiku implementers, one phase at a time, in the worktree
`/home/sebastien/travaux/llm-observer/.claude/worktrees/list-cache-return` (branch
`claude/list-cache-return`). Read the whole of your phase before editing.

Today the Traces, Spans, Sessions and Users lists fix "now" (the **anchor**) when they mount, with
millisecond precision, and ask for `from = anchor − preset`, `to = anchor`. Coming back to a list
gives a new URL, which misses the TanStack Query cache: the rows are fetched again and the page
flashes. This plan remembers each list's anchor per project and per list search for 15 minutes, so
a return asks for the same window and is served from the cache, with the scroll position restored.
After such a return, one cheap request asks what arrived after the anchor and shows "N new spans ↑"
(the Traces live-mode pill); a click, or the refresh button, loads a new window. A really new window
(changed range, filter, search, sort or flags, an expired anchor, refresh) keeps the previous rows
on screen, dimmed, until its answer arrives.

**Prerequisites:** none beyond `origin/main` at the worktree's base.

**Read first:**
- `AGENTS.md` (repo rules).
- `docs/spec/11-ui.md` § Pages → Facet sidebar (last sentence), Traces (list) → Live mode, Spans,
  Sessions, Users.
- `web/src/routes/$org/$project/spans/index.tsx` (the simplest list), then
  `traces/index.tsx`, `sessions/index.tsx`, `users/index.tsx` under the same folder.
- `web/src/components/data-table/data-table.tsx`: props, the `restoreAnchor` effect (lines ~197–230),
  the `notice` slot (~737), the `role="grid"` element (~588).
- `web/src/components/data-table/use-infinite-list.ts` (`traceListOptions`),
  `web/src/api/queries.ts` (`spanListOptions`, `sessionListOptions`, `userListOptions` only).
- `web/src/lib/list-return.ts` (`RowAnchor`), `web/src/lib/time-range.ts` (`resolveRange`).
- `web/src/components/list-toolbar.tsx` (`updatedAt`, `onRefresh`).
- `web/src/test/render.tsx`, `web/src/test/requests.ts`, `web/src/test/setup.ts`.

## What it is, and what it is not

When it's done:
- Leaving Traces, Spans, Sessions or Users and coming back within 15 minutes of the list's anchor,
  with the same search (range, filter, search text, sort, order, eval/exp flags), sends **no list
  request**: same rows, same order, same scroll position (the first visible row).
- The toolbar shows "Updated Ns ago" / "Updated N min ago" (the anchor's age) beside refresh.
- After such a return, when the range is a preset and Traces live mode is off, one request asks the
  list endpoint for `[anchor, now)` with `limit=50`. If it has rows, a pill shows at the top of the
  table: "N new traces ↑" / "N new spans ↑" / "N sessions with new activity ↑" / "N users with new
  activity ↑" ("50+" when the answer has a `next_cursor`; singular for 1). A click on it, or
  refresh, sets a new anchor.
- A new window (any search change, refresh, the pill, an anchor older than 15 minutes) shows the
  previous rows dimmed (`aria-busy`, 60% opacity) until the new first page arrives, instead of an
  empty table.

Out of scope:
- The facet panel and filter bar queries (`web/src/components/facets/`, facet code in
  `web/src/api/queries.ts`): another session owns them. They hit the cache on a return when their
  own cache entry is still there; nothing in this plan changes them.
- The user page (`users/$userId.tsx`) keeps its own mount-time anchor.
- Persisting anchors across a page reload: the query cache is in memory, so the anchors are too.

## Decisions this plan relies on

| ID | Summary |
|---|---|
| D-C-23 (new, added in LR5) | Lists keep their window on return: anchors kept in memory per project, list and search for 15 min; list queries cached as long; a check of `[anchor, now)` (limit 50) feeds a pill; a new window keeps the previous rows dimmed. |
| D134 | Traces live mode and its "N new traces" pill. |
| D-C-4 | Live first-page refetch rules (unchanged). |

User answers (2026-11-20):
- Reuse limit: **15 minutes**, measured from the anchor (the window's "now"), not from when the
  list was left.
- Sessions and Users: the pill counts **rows with new activity** in `[anchor, now)` (new or already
  listed): "N sessions with new activity ↑". A click reloads with a new window.

Choices made by this plan (within the decided behaviour):
- **Live mode on return** (Traces, `live=1`): the list does not reuse a remembered anchor; it starts
  a new window, as today. Reusing one would miss the traces that arrived while away, and the live
  stream does not replay them.
- **Absolute ranges** (`from`/`to` in the URL) do not depend on the anchor; they get the same
  caching and scroll restore, but no new-rows check.
- **Choosing a range already seen** within 15 minutes (24h → 1h → 24h) reuses that window too:
  the lookup is per search, wherever the search comes from.
- **Scroll** is restored only when the page mounts with a reused anchor, through the DataTable's
  existing `restoreAnchor`. A `listRestore` from a detail page's back link wins over it.
- **Writes**: any successful mutation marks the four lists' queries stale without refetching the
  one on screen; the next mount refetches them in the background (rows may update then: the user
  made a change). Writes that bypass `useMutation` are not covered.
- **Live toggle**: switching live on or off starts a new window (live inserts rows into the cached
  pages, so a window used in live mode is never remembered).
- **Picking the current range again** refreshes (a new anchor), as today.
- **Accepted edge**: a reused window that was empty while traces arrived since shows "No traces in
  this time range" plus the pill, not onboarding.
- **New window, scroll**: when the window key changes on a mounted page, the scroller goes to the
  top (the dimmed old rows show from the top).

## Versions

No new dependency.

## Phases and dependency graph

```
LR1 (shared pieces) ──► LR2 (spans) ──► LR3 (traces) ──► LR4 (sessions, users) ──► LR5 (docs)
```

Run them one at a time in the same worktree (each commits). Files per phase:
- LR1: `web/src/lib/list-anchor.ts` (new), `web/src/lib/list-anchor.test.ts` (new), `web/src/api/client.ts`, `web/src/api/client.test.ts`,
  `web/src/components/data-table/new-rows.tsx` (new), `web/src/components/data-table/new-rows.test.tsx` (new),
  `web/src/components/data-table/data-table.tsx`, `web/src/components/data-table/data-table.test.tsx`,
  `web/src/components/data-table/use-infinite-list.ts`, `web/src/api/queries.ts` (list options only),
  `web/src/test/setup.ts`, `web/src/test/requests.ts`.
- LR2: `web/src/routes/$org/$project/spans/index.tsx`, `web/src/routes/$org/$project/spans/index.test.tsx`.
- LR3: `web/src/routes/$org/$project/traces/index.tsx`, `web/src/routes/$org/$project/traces/index.test.tsx`,
  `web/src/components/trace/traces-histogram.tsx`.
- LR4: `web/src/routes/$org/$project/sessions/index.tsx`, `.../sessions/index.test.tsx`,
  `web/src/routes/$org/$project/users/index.tsx`, `.../users/index.test.tsx`.
- LR5 (coordinator, not Haiku): `docs/spec/11-ui.md`, `docs/mvp-decisions.md`, `CHANGELOG.md`,
  `website/src/content/docs/docs/` page for the lists if one describes this behaviour.

## Conventions (every phase)

- **Never guess.** If something in your phase is unclear, contradicts the code, or a step cannot be
  done as written, STOP. Make no further edits. Your final message must start with `QUESTION:`
  followed by the question and what you found. You will be resumed with an answer.
- Work only in `/home/sebastien/travaux/llm-observer/.claude/worktrees/list-cache-return`. Do not
  create branches or worktrees; do not stash, reset, rebase or switch branches.
- Edit only the files your phase lists.
- **Do not build, type-check, lint or run tests** (machine load rule: checks run once at the end of
  the plan). Write the code and tests carefully, then format only:
  `export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH` then from the worktree root
  `node_modules/.bin/biome format --write <your files>` (if `web/node_modules` is missing, run
  `cd web && pnpm install --frozen-lockfile` first).
- AGENTS.md rules: no placeholders, no compatibility shims, tests for each behaviour, no payload
  content in logs.
- The UI root font size is 13px: explicit pixel text sizes (`text-[12px]`) if you add any text.
- Tests: vitest + MSW from `fixtures/api`; override endpoints with `server.use(...)`, never mock
  modules. Use `renderApp` from `@/test/render` and `recordRequests` from `@/test/requests`.
- Commit once with the phase's subject, no attribution lines. Report: commit sha, files changed,
  tests added, anything unverified or not done.

---

## LR1 — Shared pieces: anchor store, new-rows check and pill, dimmed table

**Goal:** add the anchor store and its hook, the new-rows check hook and pill, a `stale` prop on
`DataTable`, and a `limit` option on the four list option builders.

**Edit:** `web/src/lib/list-anchor.ts` (new), `web/src/lib/list-anchor.test.ts` (new),
`web/src/api/client.ts`, `web/src/api/client.test.ts`,
`web/src/components/data-table/new-rows.tsx` (new), `web/src/components/data-table/new-rows.test.tsx`
(new), `web/src/components/data-table/data-table.tsx`,
`web/src/components/data-table/data-table.test.tsx`,
`web/src/components/data-table/use-infinite-list.ts`, `web/src/api/queries.ts`,
`web/src/test/setup.ts`, `web/src/test/requests.ts`.

### Steps

1. Create `web/src/lib/list-anchor.ts` with exactly these exports:

   ```ts
   import { useEffect, useState } from "react";
   import type { RowAnchor } from "@/lib/list-return";

   /** The lists whose window is remembered on return. */
   export type AnchoredList = "traces" | "spans" | "sessions" | "users";

   /** How long a list's window ("now") is reused on return, from the anchor itself (D-C-23). */
   export const ANCHOR_REUSE_MS = 15 * 60_000;

   /** List queries keyed by a window stay fresh and cached as long as the window is reused. */
   export const LIST_CACHE = { staleTime: ANCHOR_REUSE_MS, gcTime: ANCHOR_REUSE_MS } as const;

   /** The new-rows check asks for at most this many rows; more reads "50+". */
   export const NEW_ROWS_LIMIT = 50;

   /**
    * What defines a list's window apart from the anchor, normalised by the page: `rangeKey` is the
    * page's existing `rangeKey` (`preset`, or `from\nto`, from `rangeFromSearch`), and `sort` and
    * `order` are the effective values with the page's defaults applied (so `?range=24h` and no
    * range, or `sort=start_time` and no sort, give the same key).
    */
   export type WindowSearch = {
     rangeKey: string;
     filter: string | undefined;
     search: string | undefined;
     sort: string;
     order: string;
     eval: boolean;
     exp: boolean;
   };

   type Entry = { anchor: number; scroll?: RowAnchor };

   /** Tests only: forget every remembered anchor. */
   export function resetListAnchors(): void;

   /** The store key: org, project, list and the window search. */
   export function anchorKey(org: string, project: string, list: AnchoredList, search: WindowSearch): string;

   export type ListAnchor = {
     /** Epoch ms: the window's "now". */
     anchor: number;
     /** True when the anchor came from the store (a return), false when it was set now. */
     reused: boolean;
     /** Epoch ms when this anchor was looked up or set: the new-rows check's `to`. */
     lookedUpAt: number;
     /** The first visible row when the list was left, for a reused anchor. */
     scroll: RowAnchor | undefined;
     /** Sets a new anchor (now) for the current key: the refresh button, the pill, live on/off. */
     renew: () => void;
     /** Remembers the first visible row for the current key and anchor. */
     rememberScroll: (scroll: RowAnchor) => void;
   };

   /**
    * The anchor of the list window `key`. While `fresh` is true the store is neither read nor
    * written (Traces live mode: live inserts rows into the cached pages, and the stream does not
    * replay what arrived while away), and the key's stored entry is deleted. A key change on a
    * mounted page looks the new key up again.
    */
   export function useListAnchor(key: string, fresh: boolean): ListAnchor;
   ```

   Bodies:
   - A module-level `const entries = new Map<string, Entry>();`. `resetListAnchors` clears it.
   - `anchorKey` returns
     `` `${org}/${project}/${list}\n${JSON.stringify([search.rangeKey, search.filter ?? null, search.search ?? null, search.sort, search.order, search.eval, search.exp])}` ``.
   - A private `function lookup(key: string, now: number, fresh: boolean): { key: string; anchor: number; reused: boolean; lookedUpAt: number; scroll: RowAnchor | undefined }`:
     if `!fresh` and `entries.get(key)` exists and `now - entry.anchor < ANCHOR_REUSE_MS`, return
     `{ key, anchor: entry.anchor, reused: true, lookedUpAt: now, scroll: entry.scroll }`; otherwise
     `{ key, anchor: now, reused: false, lookedUpAt: now, scroll: undefined }`. It **never writes**
     the map (React may call it twice in StrictMode).
   - `useListAnchor`:
     1. `const freshRef = useRef(fresh); freshRef.current = fresh;`
     2. `const [state, setState] = useState(() => lookup(key, Date.now(), fresh));`
     3. If `state.key !== key` during render: `const next = lookup(key, Date.now(), fresh); setState(next);`
        and use `next` for the rest of this render (the "adjust state on prop change" pattern).
        Call it `current` (`const current = state.key === key ? state : next`).
     4. Store effect, deps `[current.key, current.anchor, fresh]`:
        - if `fresh`: `entries.delete(current.key)` and return;
        - otherwise prune: delete every entry whose `Date.now() - entry.anchor >= ANCHOR_REUSE_MS`;
          then if `entries.get(current.key)?.anchor !== current.anchor`, set
          `entries.set(current.key, { anchor: current.anchor })` (a stored scroll is kept when the
          anchor is the same).
     5. `renew = useCallback(() => { const now = Date.now(); setState((prev) => ({ key: prev.key, anchor: now, reused: false, lookedUpAt: now, scroll: undefined })); }, [])`
        — `Date.now()` outside the updater, which StrictMode may call twice.
     6. `rememberScroll = useCallback((scroll: RowAnchor) => { if (freshRef.current) { return; } const entry = entries.get(keyRef.current); if (entry && entry.anchor === anchorRef.current) { entry.scroll = scroll; } }, [])`,
        with `keyRef`/`anchorRef` refs assigned every render to `current.key`/`current.anchor`.
     7. Return `{ anchor, reused, lookedUpAt, scroll, renew, rememberScroll }` from `current`.
   Import `useCallback`, `useEffect`, `useRef`, `useState` from React.

2. In `web/src/test/setup.ts`, call `resetListAnchors()` (import from `@/lib/list-anchor`) in the
   existing `afterEach`, next to `sessionStorage.clear()`.

3. In `web/src/test/requests.ts`, leave `recordRequests` unchanged and add:
   ```ts
   /** Records the full URL of every request the page sends until `stop`. */
   export function recordUrls(): { urls: URL[]; stop: () => void };
   ```
   Same pattern as `recordRequests`: a `request:start` listener on `server` that pushes
   `new URL(request.url)`, and `stop` removes it.

4. Add an optional `limit?: number` to the `query` argument of `traceListOptions`
   (`use-infinite-list.ts`), `spanListOptions`, `sessionListOptions` and `userListOptions`
   (`queries.ts`), and use `limit: query.limit ?? 100` in each `listQuery`. Touch nothing else in
   `queries.ts`.

5. In `data-table.tsx`, add a prop to `DataTableProps`:
   ```ts
   /** The rows are the previous window's, shown while the new one loads: dimmed and busy. */
   stale?: boolean;
   ```
   Destructure it. On the `role="grid"` element add `aria-busy={stale ? true : undefined}`. On the
   `data-scroll-container` div append `transition-opacity` to its class and, when `stale`,
   `opacity-60` (use the existing `cn` helper if the file imports it; otherwise a template
   string).

6. Create `web/src/components/data-table/new-rows.tsx`:

   ```ts
   import { type QueryKey, useQuery } from "@tanstack/react-query";
   import type { ReactElement } from "react";
   import { Button } from "@/components/ui/button";

   /** "1 new span", "3 new spans", "50+ new spans": `more` adds "+" and forces the plural. */
   export function countLabel(count: number, more: boolean, one: string, many: string): string;

   /** The pill at the top of a list's table. */
   export function NewRowsPill({ label, onClick }: { label: string; onClick: () => void }): ReactElement;

   type CheckPage = { data: readonly unknown[]; next_cursor?: string | null };

   /**
    * Asks once what arrived in `[anchor, now)`: the list's first page over that window, at most
    * NEW_ROWS_LIMIT rows. `options` is the list's options built with that window and the limit.
    * Returns undefined until the answer arrives, while disabled, or on an error.
    */
   export function useNewRows(
     options: {
       queryKey: QueryKey;
       queryFn: (context: { pageParam: string | null; signal?: AbortSignal }) => Promise<CheckPage>;
     },
     enabled: boolean,
   ): { count: number; more: boolean } | undefined;
   ```
   - `countLabel`: `` `${count}${more ? "+" : ""} ${count === 1 && !more ? one : many}` ``.
   - `NewRowsPill` renders exactly the live pill's markup:
     `<Button type="button" size="xs" variant="secondary" onClick={onClick}>{label} ↑</Button>`.
   - `useNewRows` calls `useQuery({ queryKey: [...options.queryKey, "new-rows"], queryFn: ({ signal }) => options.queryFn({ pageParam: null, signal }), enabled, staleTime: Number.POSITIVE_INFINITY, gcTime: 0, retry: false })`
     and returns `undefined` unless `query.data` is defined, else
     `{ count: query.data.data.length, more: (query.data.next_cursor ?? null) !== null }`.

7. Writes must not leave a list showing deleted or changed rows for 15 minutes. In
   `web/src/api/client.ts`:
   ```ts
   const LIST_PATHS: ReadonlySet<string> = new Set([
     "/api/v1/projects/{project}/traces",
     "/api/v1/projects/{project}/spans",
     "/api/v1/projects/{project}/sessions",
     "/api/v1/projects/{project}/users",
   ]);

   /** A list query of Traces, Spans, Sessions or Users (D-C-23). */
   export function isListQuery(query: { queryKey: QueryKey }): boolean {
     return query.queryKey[0] === "get" && LIST_PATHS.has(String(query.queryKey[1]));
   }

   export function createAppQueryClient(): QueryClient {
     const client: QueryClient = new QueryClient({
       // A write marks the lists stale without refetching the one on screen; the next mount
       // refetches it in the background.
       mutationCache: new MutationCache({
         onSuccess: () => {
           void client.invalidateQueries({ predicate: isListQuery, refetchType: "none" });
         },
       }),
       defaultOptions: { /* unchanged */ },
     });
     return client;
   }
   ```
   Import `MutationCache` and `type QueryKey` from `@tanstack/react-query`. First check that
   `$api.queryOptions("get", path, …).queryKey` is `["get", path, init]` (read
   `node_modules/openapi-react-query`'s `queryOptions`); STOP if it is not.

### Tests

`web/src/api/client.test.ts`: "a successful mutation marks the list queries stale": build
`createAppQueryClient()`, `setQueryData` a traces list key and a non-list key (e.g. `["get", "/api/v1/projects/{project}/datasets", {}]`),
run `client.getMutationCache().build(client, { mutationFn: async () => 1 }).execute(undefined)`,
then `client.getQueryState(listKey)?.isInvalidated` is true and the other's is false.

`web/src/lib/list-anchor.test.ts` (use `renderHook` from `@testing-library/react`,
`vi.useFakeTimers({ toFake: ["Date"] })` and `vi.setSystemTime`, restored in `afterEach`):
- `anchorKey` differs by org, project, list and each of the seven `WindowSearch` fields.
- first mount: `reused` false, `anchor` = now.
- unmount, then mount the same key 14 min 59 s later: `reused` true, same `anchor`.
- mount the same key exactly 15 min after the anchor: `reused` false, new anchor.
- `fresh` true ignores a stored anchor, deletes it, and stores nothing (a later non-fresh mount
  gets `reused` false); `rememberScroll` does nothing while `fresh`.
- `rerender` with `fresh` true and another key: the new key's stored anchor is not reused.
- the store prunes: an entry for key A older than 15 min is gone after a mount of key B
  (mount A again within the same fake time + 15 min − 1 ms is reused, + 16 min is not).
- `lookedUpAt` is the lookup time: mount at T, unmount, mount at T + 5 min → `anchor` T,
  `lookedUpAt` T + 5 min.
- `rerender` with another key: the new key gets its own lookup (a stored fresh anchor is reused,
  otherwise now).
- `renew` sets `anchor` to now and `reused` false, and the next mount reuses the renewed anchor.
- `rememberScroll` is returned as `scroll` on the next mount with the same anchor; after `renew`,
  the next mount has no `scroll`.
- `resetListAnchors` forgets everything.

`web/src/components/data-table/new-rows.test.tsx`:
- `countLabel(1, false, "new span", "new spans")` → `"1 new span"`; `(3, false, …)` → `"3 new spans"`;
  `(50, true, …)` → `"50+ new spans"`; `(1, true, …)` → `"1+ new spans"`.
- `NewRowsPill` renders a button named `"2 new spans ↑"` and calls `onClick`.

`web/src/components/data-table/data-table.test.tsx`: one case "dims the rows and marks the grid
busy while stale": render with `stale` → grid has `aria-busy="true"` and the
`[data-scroll-container]` has class `opacity-60`; without it, neither.

### Done when

The files are written and formatted (no checks run, see Conventions).

**Commit:** `Add the list anchor store and the new-rows pill`

---

## LR2 — Spans: same window on return, pill, dimmed new window

**Goal:** the Spans list reuses its anchor and scroll on return, checks for new spans, and keeps
the old rows dimmed during a new window.

**Edit:** `web/src/routes/$org/$project/spans/index.tsx`,
`web/src/routes/$org/$project/spans/index.test.tsx`.

### Steps

1. Imports: add `keepPreviousData` to the `@tanstack/react-query` import. From `@/lib/list-anchor` import `anchorKey`, `LIST_CACHE`,
   `NEW_ROWS_LIMIT`, `useListAnchor`. From `@/components/data-table/new-rows` import `countLabel`,
   `NewRowsPill`, `useNewRows`.
2. Replace the anchor block (`const [anchor, setAnchor] = useState(() => Date.now());`, the
   `seenRange` ref and its effect) with:
   ```ts
   const windowKey = anchorKey(org, project, "spans", {
     rangeKey,
     filter: search.filter,
     search: search.search,
     sort: search.sort ?? "start_time",
     order: search.order ?? "desc",
     eval: search.eval === 1,
     exp: search.exp === 1,
   });
   const listAnchor = useListAnchor(windowKey, false);
   const anchor = listAnchor.anchor;
   const windowId = `${windowKey}\n${anchor}`;
   const [restoreAtMount] = useState(() => (listAnchor.reused ? listAnchor.scroll : undefined));
   ```
   `rangeKey` stays (the selection key uses it). The defaults are the page's (`SORT_COLUMN[search.sort ?? "start_time"]`
   and `(search.order ?? "desc")` already used for `sorting`).
3. In the `ListToolbar` `onRange` handler replace the two lines that set `seenRange.current` and
   call `setAnchor` with: if the picked range's key (`next.kind === "preset" ? next.preset : `${next.from}\n${next.to}``)
   equals `rangeKey`, call `listAnchor.renew()` (picking the current range again refreshes, as
   today); keep the `navigate`. Change `onRefresh` to `listAnchor.renew`. Add
   `updatedAt={anchor}` to `ListToolbar`.
4. `useInfiniteQuery({...spanListOptions(...), ...LIST_CACHE, placeholderData: keepPreviousData, enabled: !hold })`.
5. The check:
   ```ts
   const newRows = useNewRows(
     spanListOptions(org, project, {
       from: new Date(anchor).toISOString(),
       to: new Date(listAnchor.lookedUpAt).toISOString(),
       search: search.search,
       filter: search.filter,
       includeEval: search.eval === 1,
       includeExperiments: search.exp === 1,
       limit: NEW_ROWS_LIMIT,
     }),
     listAnchor.reused && range.kind === "preset" && !hold && list.isSuccess && !list.isPlaceholderData,
   );
   ```
   No `sort`/`order` (the count does not depend on them).
6. Scroll: pass to `DataTable`
   `onScrollAnchor={list.isPlaceholderData ? undefined : listAnchor.rememberScroll}` (a placeholder
   row must never be remembered for the new window) and `restoreAnchor={restoreAtMount}` (captured
   once at mount; `DataTable` restores once per mount).
7. Dimmed new window: pass `stale={list.isPlaceholderData}` and change
   `hasNextPage={list.hasNextPage}` to `hasNextPage={list.hasNextPage && !list.isPlaceholderData}`.
8. Top on a new window (a search change, refresh or the pill), `windowId` from step 2:
   ```ts
   const seenWindow = useRef(windowId);
   useLayoutEffect(() => {
     if (seenWindow.current === windowId) {
       return;
     }
     seenWindow.current = windowId;
     const node = document.querySelector<HTMLElement>('[aria-label="Spans"] [data-scroll-container]');
     if (node && node.scrollTop !== 0) {
       node.scrollTop = 0;
       node.dispatchEvent(new Event("scroll"));
     }
   }, [windowId]);
   ```
9. Pill: pass to `DataTable`
   `notice={newRows && newRows.count > 0 ? <NewRowsPill label={countLabel(newRows.count, newRows.more, "new span", "new spans")} onClick={listAnchor.renew} /> : null}`.
   After `renew`, `reused` is false, so the check is disabled and `newRows` is `undefined`: the pill
   goes away.
10. Remove imports that became unused (`useState` stays: the selection uses it). Do not import
    `RowAnchor` unless you use it.
11. Existing tests: read every test in `spans/index.test.tsx` that changes the filter, search,
    range, sort or flags, or clicks Refresh, and then expects rows to disappear, an empty state, or
    a count. With `keepPreviousData` the old rows stay until the new page arrives: make such a test
    wait for the new state (`await waitFor(...)` / `findBy…`) rather than asserting at once. Change
    nothing else in them.

### Tests (`spans/index.test.tsx`)

Helpers in the test file: leave with `await router.navigate({ to: "/$org/$project/sessions", params: { org: "acme", project: "support-bot" } })`
and come back with `await router.navigate({ to: "/$org/$project/spans", params: { org: "acme", project: "support-bot" } })`,
then `await screen.findByRole("grid", { name: "Spans" })`. Record with `recordUrls()` from
`@/test/requests`. A **list request** is a URL whose `pathname` ends with `/spans` and whose
`searchParams.get("limit") === "100"`; a **check request** has `limit` `"50"`.
To answer only some requests, `server.use(http.get("*/api/v1/projects/:project/spans", ({ request }) => { … }))`
and return `undefined` from the resolver for the others: MSW then passes them to the default
fixture handler. The fixture page is `fixtures/api/responses/spans.json` (import it with a relative
path as the file already imports `fixtures/api/static/annotation-queues.json`).
- "a return within 15 minutes sends no list request": render `/acme/support-bot/spans`, wait for
  the grid, leave, start recording, come back: no list request; the grid still shows
  `get_order_status`.
- "the pill shows the new spans and loads them": the check (limit 50) answers
  `{ ...spansFixture, data: spansFixture.data.slice(0, 2), next_cursor: null }`. Leave and come
  back: the button `"2 new spans ↑"` appears; start recording, click it: a list request is sent
  and the pill is gone.
- "no pill without new spans": the check answers `{ data: [], next_cursor: null }`: no button
  matching `/new span/`.
- "an expired anchor refetches": with `vi.useFakeTimers({ toFake: ["Date"], shouldAdvanceTime: true })`, render, leave, move
  the clock 16 min ahead with `vi.setSystemTime`, come back: a list request is sent and no check
  request. Restore with `vi.useRealTimers()` in `finally`.
- "a new window keeps the old rows dimmed": render and wait for the grid; override the spans
  endpoint so requests whose `filter` param is set never resolve (`await new Promise(() => {})`);
  navigate to `?filter=status = 'error'` with `router.navigate({ to: …, search: { filter: "status = 'error'" } })`:
  the grid still shows `get_order_status` and has `aria-busy="true"`.
- "an absolute range sends no check": render with `?from=…&to=…` (two RFC 3339 values an hour
  apart in the fixture's range, see existing tests for the format), leave and come back: no list
  and no check request.
- "refresh sets a new window": click the refresh button (find it by its accessible name in
  `list-toolbar.tsx`): one list request is sent.

### Done when

Written and formatted, no checks run.

**Commit:** `Keep the spans list window on return`

---

## LR3 — Traces: the same, plus live mode

**Goal:** the Traces list gets LR2's behaviour; live mode starts a fresh window and keeps its own
pill, which now uses `NewRowsPill`.

**Edit:** `web/src/routes/$org/$project/traces/index.tsx`,
`web/src/routes/$org/$project/traces/index.test.tsx`,
`web/src/components/trace/traces-histogram.tsx`.

### Steps

1. Apply LR2 steps 1–8 to the traces page, with `"traces"`, `traceListOptions` and the
   `[aria-label="Traces"]` selector. Differences:
   - The key's sort default is `"start_time"`, order `"desc"`.
   - `useListAnchor(windowKey, search.live === 1)`.
   - The anchor reset in `changeRange` (`seenRange.current = …; setAnchor(Date.now());`) becomes
     the same "renew when the range is unchanged" rule as LR2 step 3; keep the `navigate`.
   - The `LiveToggle` `onPressedChange` (around line 466) calls `setAnchor(Date.now())` when
     turned on: replace that `if (on) { … }` with an unconditional `listAnchor.renew();` before the
     `navigate`. Turning live on starts a new window as today; turning it off also starts one, so
     the window remembered after live mode holds no live-inserted rows.
   - `onRefresh` becomes `listAnchor.renew`.
   - `scroll`: traces already passes `onScrollAnchor={onScrollAnchor}` (it fills `lastAnchor` for
     the full-page return) and `restoreAnchor={restoreAnchor}` (from `listRestore`). Make
     `onScrollAnchor` also call `listAnchor.rememberScroll(anchor)` (add `listAnchor.rememberScroll`
     to its deps), pass `onScrollAnchor={list.isPlaceholderData ? undefined : onScrollAnchor}` and
     `restoreAnchor={restoreAnchor ?? restoreAtMount}`.
   - The check is also disabled while live is on and while onboarding or probing shows:
     `listAnchor.reused && range.kind === "preset" && !liveOn && !hold && list.isSuccess && !list.isPlaceholderData && !onboarding && !probing`.
     Declare the check after `liveOn`, `onboarding` and `probing` exist.
   - Keep the `useInfiniteQuery` `refetchInterval` as is; add `...LIST_CACHE` and
     `placeholderData: keepPreviousData`.
2. `listEmpty` becomes `!hold && list.data !== undefined && !list.isPlaceholderData && rows.length === 0`.
3. `useLiveTraces` `url`: open the stream only when
   `liveOn && (list.isSuccess || list.isRefetchError) && !list.isPlaceholderData`.
4. The `notice`: the live pill when `live.pending > 0`, else the new-rows pill:
   ```tsx
   notice={
     live.pending > 0 ? (
       <NewRowsPill label={countLabel(live.pending, live.overflowed, "new trace", "new traces")} onClick={live.reveal} />
     ) : newRows && newRows.count > 0 ? (
       <NewRowsPill label={countLabel(newRows.count, newRows.more, "new trace", "new traces")} onClick={listAnchor.renew} />
     ) : null
   }
   ```
5. `traces-histogram.tsx`: `useQuery({ ...widgetQuery(...same args...), ...LIST_CACHE })` so the
   histogram of a reused window is served from the cache too.
6. In `traces/index.test.tsx`, the live tests that look for `"1 new traces ↑"` now look for
   `"1 new trace ↑"` (lines ~1015 and ~1019); `"2 new traces ↑"` and `"2+ new traces ↑"` are
   unchanged.

### Tests (`traces/index.test.tsx`)

The LR2 cases, for traces (`/traces`, list request `limit=100` on a path ending `/traces`, check
`limit=50`, a fixture trace name for the "same rows" assertion; pick one the existing tests use),
plus:
- "live mode starts a new window on return": render `/acme/support-bot/traces?live=1` (see how the
  existing live tests set up the stream with `@/test/event-source`), leave, come back: a list
  request is sent and no check request.
- "a return restores the first visible row": the fixture has 5 traces. Render, set the
  scroller's `scrollTop` to `2 * ROW_HEIGHT` (the row height the file's existing scroll tests use)
  and fire a `scroll` event on it, then wait with real timers for longer than the DataTable's
  `ANCHOR_SETTLE_MS` (150 ms): `await new Promise((resolve) => setTimeout(resolve, 200))`. Leave
  and come back: the new scroller's `scrollTop` is `2 * ROW_HEIGHT`. If jsdom clamps `scrollTop`
  to 0 because the scroller has no height, stub `clientHeight`/`scrollHeight` the way the file's
  existing restore tests do; STOP if they don't.
- Existing tests: as LR2 step 11, for `traces/index.test.tsx` (including the onboarding and empty
  state tests).

### Done when

Written and formatted, no checks run.

**Commit:** `Keep the traces list window on return`

---

## LR4 — Sessions and Users

**Goal:** Sessions and Users get LR2's behaviour, with "with new activity" pills; Users' KPI and
top-users queries are cached like the list.

**Edit:** `web/src/routes/$org/$project/sessions/index.tsx`, `.../sessions/index.test.tsx`,
`web/src/routes/$org/$project/users/index.tsx`, `.../users/index.test.tsx`.

### Steps

1. Sessions: LR2 steps 1–9 with `"sessions"`, `sessionListOptions` (no `search` argument: the
   builder has none), selector `[aria-label="Sessions"]` (check the `ariaLabel` the page passes to
   `DataTable` and use it), and the pill label
   `countLabel(n, more, "session with new activity", "sessions with new activity")`.
   Key defaults: sort `"start_time"`, order `"desc"`, `search: undefined`. Scroll as in LR3:
   the existing `onScrollAnchor` also calls `listAnchor.rememberScroll`, it is passed only while
   `!list.isPlaceholderData`, and `restoreAnchor={restoreAnchor ?? restoreAtMount}`. Add
   `useLayoutEffect` to the React import if missing.
   The `ListToolbar` `notice` (carried filters) is a different prop from the `DataTable` `notice`;
   do not touch the toolbar's.
2. Users: LR2 steps 1–10 with `"users"`, `userListOptions` (no `search`), its grid's aria label,
   key defaults sort `"last_seen"`, order `"desc"`, `search: undefined`, and
   `countLabel(n, more, "user with new activity", "users with new activity")`. Users has no
   `listRestore` plumbing: pass `onScrollAnchor={list.isPlaceholderData ? undefined : listAnchor.rememberScroll}`
   and `restoreAnchor={restoreAtMount}`. The removed `seenRange` block used `useRef`/`useEffect`;
   step 8 needs `useRef` and `useLayoutEffect`: fix the React import accordingly. Add
   `...LIST_CACHE` and `placeholderData: keepPreviousData` to the `headline` and `top` `useQuery`
   calls, so the KPI cards keep their numbers while the list is dimmed.
3. Existing tests in both files: as LR2 step 11.
4. If either page computes something "from the loaded rows" (the sessions summary strip and
   chart), it keeps using `rows`; while `list.isPlaceholderData` they show the previous window's
   numbers, which is intended.

### Tests

Fixtures have 2 sessions and 2 users: the pill check answers `data.slice(0, 2)` (both rows).
In each test file, the LR2 cases "a return within 15 minutes sends no list request", "the pill
shows … and loads them" (labels `"2 sessions with new activity ↑"` / `"2 users with new activity ↑"`),
"an expired anchor refetches" and "a new window keeps the old rows dimmed" (Users: filter
`user_id = 'x'` or any filter the existing tests use; Sessions likewise).

### Done when

Written and formatted, no checks run.

**Commit:** `Keep the sessions and users list windows on return`

---

## LR5 — Docs, checks, browser (coordinator)

1. `docs/mvp-decisions.md`: add D-C-23 after D-C-22 (text from the Decisions table above, with the
   alternatives rejected: plain stale-while-revalidate, which swaps rows under the user;
   `sessionStorage`, which outlives the in-memory cache).
2. `docs/spec/11-ui.md`: a "**Window kept on return** (D-C-23)" paragraph in § Pages after
   "Filters kept across lists"; update the facet sidebar's last sentence; in Traces live mode, note
   that the "N new traces" pill also shows after a return.
3. `CHANGELOG.md` § Unreleased → Changed: one line.
4. The website page for the lists if it describes refresh or live mode
   (`grep -rln "live" website/src/content/docs/docs/`).
5. `just check-web`, fix; `just check`; `just web-dev` and check the four lists in the browser
   (return within the limit: no request; pill; refresh; new filter dims).
6. Sonnet code review; fixes by Haiku; PR.
