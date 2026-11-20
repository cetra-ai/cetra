# List return cache: review fixes (LR6)

Follow-up phase of `docs/plans/c-web-ui/list-return-cache.md` (read its Conventions; they apply),
from the code review of the branch. Worktree
`/home/sebastien/travaux/llm-observer/.claude/worktrees/list-cache-return`, branch
`claude/list-cache-return`.

**Edit:** `web/src/api/client.ts`, `web/src/api/client.test.ts`, `web/src/lib/list-anchor.ts`,
`web/src/lib/list-anchor.test.ts`, and the four list routes and their tests
`web/src/routes/$org/$project/{traces,spans,sessions,users}/index.tsx` / `index.test.tsx`.

## Steps

1. **Widget queries are invalidated by writes.** In `web/src/api/client.ts`, rename `isListQuery`
   to `isWindowQuery` (update its doc: "A list query of Traces, Spans, Sessions or Users, or a
   widget query (the traces histogram, the Users KPIs): cached as long as a list window, D-C-23")
   and make it also return true when `query.queryKey[0] === "post"` and `query.queryKey[1]` is
   `WIDGET_QUERY_PATH` (import it from `@/api/dashboards`; if that import creates a cycle that
   tsc or vitest reports, use the string literal `"/api/v1/projects/{project}/dashboards/query"`
   instead). Update every use and `client.test.ts`: add a widget key (`["post", WIDGET_QUERY_PATH, {}]`)
   that must be invalidated too.

2. **Never another project's rows as placeholder.** In `web/src/lib/list-anchor.ts` add:
   ```ts
   import type { QueryKey } from "@tanstack/react-query";

   /** The project ref in an openapi-react-query key (`[method, path, { params: { path: { project } } }]`). */
   function projectOfKey(key: QueryKey | undefined): unknown {
     const init = key?.[2] as { params?: { path?: { project?: unknown } } } | undefined;
     return init?.params?.path?.project;
   }

   /**
    * `placeholderData` for a list: the previous window's data, but only from the same project, so
    * switching projects shows a loading state rather than the other project's rows.
    */
   export function keepSameProject<T>(key: QueryKey) {
     return (previous: T | undefined, previousQuery: { queryKey: QueryKey } | undefined): T | undefined =>
       previousQuery !== undefined && projectOfKey(previousQuery.queryKey) === projectOfKey(key)
         ? previous
         : undefined;
   }
   ```
   In each of the four routes replace `placeholderData: keepPreviousData` on the list's
   `useInfiniteQuery` with `placeholderData: keepSameProject(<the list options>.queryKey)` (build
   the list options into a `const listOptions = …ListOptions(...)` first where the page does not
   already have one; Traces has `listOptions`). Remove `keepPreviousData` imports that become
   unused. If the generic does not type-check against `useInfiniteQuery`'s `placeholderData`,
   STOP and report the tsc error.

3. **Users KPIs follow the old behaviour.** In `users/index.tsx`, remove
   `placeholderData: keepPreviousData` from the `headline` and `top` queries (keep `...LIST_CACHE`).

4. **No dimmed empty state from an empty previous window.** In each route, the condition that
   renders `<DataTable …>` (`!hold && list.data ? (` on Spans, Sessions, Users; `!hold && list.data && !probing ? (`
   on Traces) also requires `!(list.isPlaceholderData && rows.length === 0)`, so an empty
   placeholder shows what a first load shows (nothing / the existing loading state).

5. **A remembered scroll belongs to the last visit.** In `useListAnchor`:
   - Add `atMount: boolean` and `fresh: boolean` to `Lookup`. The `useState` initializer's
     lookup has `atMount: true`; the render-time lookup and `renew` have `atMount: false`.
     Every lookup stores the `fresh` it was made with (`renew` keeps `prev.fresh`).
   - The render-time branch runs when `state.key !== key || state.fresh !== fresh` (so live
     switching on or off starts a new window by itself: when `fresh` goes false the entry was
     deleted while fresh, so the lookup returns a new anchor).
   - In the store effect, after the entry is written or kept for `current.key`, when
     `!current.atMount`, `delete` the entry's `scroll` (a key change or renew on a mounted page;
     the next scroll settle stores a new one). Add `current.atMount` to the effect's deps.
   - Do not expose `atMount` or `fresh` in `ListAnchor`.
   In `traces/index.tsx`, remove the `listAnchor.renew();` call from the `LiveToggle`
   `onPressedChange` (the hook now starts the new window when `live` changes); keep the
   `navigate`.

6. **Tests.**
   - `list-anchor.test.ts`: "`fresh` going from true to false starts a new window and stores it"
     (rerender with `fresh` false: `reused` false, new anchor; unmount and mount again: reused);
     "a key change on a mounted page clears the stored scroll" (mount key A, `rememberScroll`,
     rerender key B, rerender key A, unmount, mount A again: `scroll` undefined);
     `keepSameProject` returns the previous data for the same project ref and `undefined` for
     another one or without a previous query.
   - `client.test.ts`: the widget key case from step 1.
   - In the four route test files, the "no pill" test waits deterministically instead of a fixed
     timeout: after the check request is recorded, `await waitFor(() => expect(queryClient.isFetching()).toBe(0))`
     (`renderApp` returns `queryClient`), then asserts no pill. The "pill loads them" test, after
     the click and the list request, waits until the grid is not `aria-busy`
     (`await waitFor(() => expect(grid).not.toHaveAttribute("aria-busy"))`, re-querying the grid).
   - Traces: "turning live off starts a new window": render `?live=1` (as the existing live
     tests do), navigate to the same search without `live`: a list request is sent.

## Done when

`cd web && ./node_modules/.bin/vitest run src/lib/list-anchor.test.ts src/api/client.test.ts 'src/routes/$org/$project/traces/index.test.tsx' 'src/routes/$org/$project/spans/index.test.tsx' 'src/routes/$org/$project/sessions/index.test.tsx' 'src/routes/$org/$project/users/index.test.tsx' --maxWorkers=4`
passes, `cd web && pnpm exec tsc -b` has no errors, and
`node_modules/.bin/biome check --write <changed files>` (from the worktree root) leaves no error.

**Commit:** `Fix review findings on the kept list windows`
