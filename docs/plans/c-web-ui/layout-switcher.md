# Layout switcher for the Traces and Spans lists

Track C (web UI). Plan for 6 phases, each one commit. Do one phase, nothing more.

## Goal

Today a row click on the Traces or Spans list opens the trace in a horizontal resizable split (spec 11 § Traces, D125). The user picks one of four layouts with a switcher in the list toolbar:

| Mode | Id | What it looks like |
|---|---|---|
| Split | `split` | Today's layout. List left (40%), trace right. **Default.** |
| Drawer | `drawer` | List at full width. The trace is a non-modal panel over the right 70% of the list. |
| Rail | `rail` | List shrinks to a narrow strip on the left (Start, Name, Duration only); the trace takes the rest. |
| Bottom | `bottom` | List on top (40%), trace below (60%), vertical resizable split. |

## Decisions (already made by the user, do not revisit)

1. All four modes. Applies to the **Traces and Spans lists** only. Sessions and the trace page keep their own splits.
2. The choice is a **per-browser preference** in localStorage (not in the URL, not in saved views). One preference for both lists.
3. Drawer is **non-modal**: no scrim, no focus trap, the list stays clickable, `j`/`k`/arrows/Enter keep working, `Esc` closes (existing `dismiss` shortcut). It is a plain absolutely positioned panel, **not** the Radix `Sheet` (that one is modal).
4. Rail columns are a **fixed set** (not the user's column choice): Traces `start`, `name`, `duration`; Spans `kind`, `name`, `duration`.
5. In Rail mode, while a trace is open, the list toolbar is replaced by a compact bar (title + layout switcher).
6. Switching mode remounts the list (its scroll position resets). Accepted.

## Rules from AGENTS.md that matter here

- No compatibility layers, no placeholders, no unneeded abstractions. Do not add a trait/hook/wrapper unless this plan says so.
- Biome formats the code. Run `just fmt` before `just check-web`.
- Page tests use the MSW mocks with `server.use`; do not mock modules.
- Node: `export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH` before `just check-web`.
- Do not touch anything outside `web/`, `docs/`, `CHANGELOG.md` unless a phase says so. No server, storage or SDK changes.
- Commit message: short imperative subject, no attribution line.

## Facts about the current code (verified; do not re-derive)

- `web/src/components/trace/split-pane.tsx` exports `SplitPane({ name, open, start, end })`. `name` is `"traces" | "trace" | "session"`. It wraps `ResizablePanelGroup` from `react-resizable-panels` 4.21 (`orientation="horizontal"`), keeps both panels mounted (so opening the detail does not remount the list), and stores the `{startId: pct, endId: pct}` layout in `localStorage["cetra.split.<name>"]` (`STORAGE_PREFIX` is `"cetra."`, from `@/product`). Panel ids come from the `PANES` map. Both panels render their children inside `<div className="absolute inset-0 flex min-h-0 flex-col">`.
- `web/src/components/ui/resizable.tsx` already styles a vertical group (`aria-[orientation=vertical]:flex-col`) and a horizontal separator (`aria-[orientation=horizontal]:h-px ...`).
- Both list routes use `<SplitPane name="traces" open={traceOpen} start={...} end={...} />` (Spans also uses `name="traces"`, so they share the stored ratio):
  - `web/src/routes/$org/$project/traces/index.tsx` (the `SplitPane` is near line 255; `start` holds `ListToolbar`, `DeleteMatchingDialog`, and `DataTable tableId="traces"`; `end` holds `TraceView`).
  - `web/src/routes/$org/$project/spans/index.tsx` (same shape, `DataTable tableId="spans"`).
  - `traceOpen = search.trace !== undefined`.
- `web/src/components/list-toolbar.tsx` exports `ListToolbar` with slot props `filter`, `columns`, `views`, `live`, `loaded`. The Columns button is the `columns` slot.
- `DataTable` (`web/src/components/data-table/data-table.tsx`) takes `tableId`, `ariaLabel`, `columns` (TanStack column defs with `id`, `size`), etc. Column widths are persisted per `tableId` (`readColumnSizes`/`writeColumnSizes`), so a different `tableId` gives independent widths.
- Trace column ids (`web/src/components/trace-columns.tsx`): `status`, `start` (size 132), `name` (200), `input`, `output`, `duration` (80), `tokens`, ... Span column ids (`web/src/components/span-columns.tsx`): `kind`, `name`, `model`, `input`, `output`, `duration`, ... (check the exact ids in the file before use).
- The stored-preference pattern to copy is `web/src/lib/settings.ts` (`useSyncExternalStore`, try/catch around localStorage, in-memory fallback).
- Lucide icons verified to exist: `Columns2`, `PanelRight`, `PanelBottom`, `PanelLeft`.
- `web/src/components/ui/toggle.tsx` exists (Radix Toggle). There is no `toggle-group`; do not add one.
- Existing tests: `web/src/routes/$org/$project/traces/index.test.tsx` and `spans/index.test.tsx` (50 KB / 3.7 KB). Read how they render the page and open a trace before writing route tests.

---

## Phase 1: layout preference store

**Edit**
- New `web/src/lib/list-layout.ts`.
- New `web/src/lib/list-layout.test.ts`.

**Steps**
1. Copy the structure of `web/src/lib/settings.ts`. Export:
   - `export type ListLayout = "split" | "drawer" | "rail" | "bottom";`
   - `export const LIST_LAYOUTS: readonly ListLayout[] = ["split", "drawer", "rail", "bottom"];`
   - `export const LIST_LAYOUT_STORAGE_KEY = \`${STORAGE_PREFIX}list_layout\`;`
   - `getListLayout(): ListLayout`, `setListLayout(next: ListLayout): void`, `useListLayout(): ListLayout`.
2. `readStored` returns the stored value only if it is one of `LIST_LAYOUTS`; otherwise (missing, garbage, localStorage throws) returns the in-memory fallback, else `"split"`.
3. `setListLayout` sets the fallback, writes localStorage in try/catch, then notifies listeners.
4. `useListLayout` uses `useSyncExternalStore(subscribe, getListLayout, () => "split")`.

**Tests** (`list-layout.test.ts`; model on `web/src/lib/settings.test.tsx`)
- default is `"split"`.
- `setListLayout("rail")` then `getListLayout()` is `"rail"` and `localStorage` holds `"rail"`.
- a garbage stored value (`"nope"`) reads as `"split"`.
- `useListLayout` re-renders when `setListLayout` is called (use `renderHook` + `act`).
- when `localStorage.setItem` throws, `getListLayout()` still returns the value just set.

**Done when** `just check-web` passes and nothing else changed.

---

## Phase 2: the switcher control

**Edit**
- New `web/src/components/layout-switcher.tsx` and `layout-switcher.test.tsx`.
- `web/src/components/list-toolbar.tsx`: add the slot.
- `web/src/routes/$org/$project/traces/index.tsx` and `spans/index.tsx`: pass the control.

**Steps**
1. `LayoutSwitcher` takes no props. It reads `useListLayout()` and calls `setListLayout`. It renders `<div role="radiogroup" aria-label="Layout" className="inline-flex shrink-0 items-center rounded-md border">` with four buttons, each `role="radio"`, `aria-checked`, `aria-label` and a `Tooltip` (use `Tooltip`, `TooltipTrigger asChild`, `TooltipContent` as `list-toolbar.tsx` does for its `Flag`). Use `Button variant="ghost" size="icon-xs"`; the checked one gets `className="bg-muted"`.
   | Id | Label / tooltip | Icon |
   |---|---|---|
   | `split` | Split | `Columns2` |
   | `drawer` | Drawer | `PanelRight` |
   | `rail` | Rail | `PanelLeft` |
   | `bottom` | Bottom | `PanelBottom` |
2. Arrow keys inside the group are not required. Each button is a normal tab stop.
3. `ListToolbar`: add an optional prop `layout?: ReactNode`; render `{layout}` right after `{columns}` and before `{views}`.
4. In both routes, pass `layout={<LayoutSwitcher />}` to `ListToolbar` (the routes already wrap `ListToolbar` in `TooltipProvider`).
5. No layout behaviour yet.

**Tests**
- `layout-switcher.test.tsx`: renders four radios; the default one checked is "Split"; clicking "Rail" checks it and stores `"rail"`. Wrap in `TooltipProvider`.
- In `traces/index.test.tsx` add one test: the page shows `getByRole("radiogroup", { name: "Layout" })`.

**Done when** the switcher shows in both toolbars, clicking changes the checked radio and survives a reload (localStorage), `just check-web` passes.

---

## Phase 3: SplitPane takes a mode; Bottom works

**Edit**
- `web/src/components/trace/split-pane.tsx`.
- `web/src/components/trace/split-pane.test.tsx` (new).
- Traces and Spans routes: pass `mode`.

**Steps**
1. Add a prop `mode?: "split" | "rail" | "bottom"` (default `"split"`). Do **not** add `drawer` here (Phase 5).
2. Orientation: `mode === "bottom" ? "vertical" : "horizontal"`; pass it to `ResizablePanelGroup`.
3. Persisted ratio key per mode, computed by one small function `storageKey(name, mode)`:
   - `split` → `${STORAGE_PREFIX}split.${name}` (unchanged, so today's saved ratios keep working);
   - `rail` → `${STORAGE_PREFIX}split.${name}.rail`;
   - `bottom` → `${STORAGE_PREFIX}split.${name}.bottom`.
   `readSplit` and `writeSplit` take the mode and use `storageKey`.
4. Default start size per mode: `PANES[name].startSize` for `split`; `26` for `rail`; `40` for `bottom` (for `name: "traces"`; for other names keep `PANES[name].startSize` in every mode). Put the numbers in one const map `MODE_START_SIZE`.
5. Min sizes: `split` and `bottom` keep `"15%"`; `rail` start panel `minSize` `"12%"` and `maxSize="40%"` (add `maxSize` only for the rail start panel when open).
6. Key the group by mode so a mode change remounts it: `<ResizablePanelGroup key={mode} ...>`. Also change the `initial` state to depend on mode (it already recomputes on mount because of the key).
7. Routes: `const layout = useListLayout();` then `<SplitPane name="traces" mode={layout === "drawer" ? "split" : layout} ...>` (drawer still falls back to split until Phase 5; Phase 5 removes that ternary).
8. When `open` is false the closed layout (`100/0`) applies in every mode. The handle is not rendered when closed (already so).

**Tests** (`split-pane.test.tsx`; render `SplitPane` directly with two `<div>`s)
- `mode="bottom"`: the group has `aria-orientation="vertical"` (check the group element by `data-slot="resizable-panel-group"`).
- `mode="split"` stores under the old key, `mode="rail"` under `...split.traces.rail` after a user drag is awkward to simulate; instead test `readSplit` behaviour through mounting with a pre-seeded localStorage value for each key and asserting the first panel's `style.flexGrow`/size. If jsdom cannot show sizes, test only the orientation and that a seeded invalid value falls back (no throw).
- In `traces/index.test.tsx`: with `list_layout` = `"bottom"` in localStorage and `?trace=<id>` open, the list and trace are both visible (the Traces grid and the trace tree).

**Done when** choosing Bottom in the browser puts the trace under the list with a draggable horizontal divider; the ratio survives a reload; Split behaves exactly as before; `just check-web` passes.

---

## Phase 4: Rail

**Edit**
- `web/src/components/list-toolbar.tsx` (compact mode).
- Traces and Spans routes (column filter, `tableId`, compact toolbar).
- Tests.

**Steps**
1. `ListToolbar`: add `compact?: boolean`. When `compact`, render only: the `<h1>` title and `{layout}`, in the same container (`flex min-h-9 shrink-0 items-center gap-2 border-b px-3`, with the layout switcher pushed right via `ml-auto`). Everything else (range picker, refresh, live, filter, search, flags, columns, views, loaded) is not rendered. Hooks stay at the top unconditionally (`useShortcut` must still be called).
2. In each route: `const railed = layout === "rail" && traceOpen;`. Pass `compact={railed}` to `ListToolbar`.
3. Rail columns in a module-level const per route:
   - traces: `const RAIL_COLUMNS = { start: 100, name: 120, duration: 72 }` (id → width);
   - spans: `{ kind: 28, name: 120, duration: 72 }` (check the real ids; use the first column of `span-columns.tsx` if `kind` is a narrow icon column).
   Derive the rail columns: `tableColumns.filter(c => c.id && c.id in RAIL_COLUMNS).map(c => ({ ...c, size: RAIL_COLUMNS[c.id] }))`. Column defs are TanStack objects; spreading them is fine.
4. Pass to `DataTable`: `columns={railed ? railColumns : tableColumns}` and `tableId={railed ? "traces-rail" : "traces"}` (`"spans-rail"` for Spans) so the narrow widths are saved apart from the wide table's widths. Keep `ariaLabel="Traces"`/`"Spans"` unchanged (the scroll-restore code queries `[aria-label="Traces"]`).
5. The `DeleteMatchingDialog` stays rendered (it is closed unless opened).
6. Sorting stays as is (headers are clickable in the rail).

**Tests**
- Traces page with `list_layout="rail"` and a trace open: the grid has exactly the columns `Start`, `Name`, `Duration` (`getAllByRole("columnheader")` names), and the toolbar's `Search` box is absent, the `Layout` radiogroup is present.
- Same page with the trace closed (`rail` set, no `?trace=`): full toolbar and all default columns.
- Click a row in the rail: the URL gets `trace=<id>` and the trace view stays open.

**Done when** in the browser, opening a trace in Rail mode shows the narrow list with 3 columns on the left and the trace using the remaining width; closing the trace (Esc or the trace's X) restores the full toolbar and columns; `just check-web` passes.

---

## Phase 5: Drawer

**Edit**
- `web/src/components/trace/split-pane.tsx` (add `drawer` mode).
- Traces and Spans routes (remove the Phase 3 ternary).
- Tests.

**Steps**
1. `mode` becomes `"split" | "rail" | "bottom" | "drawer"`.
2. When `mode === "drawer"`, return a different tree and skip `ResizablePanelGroup` entirely:
   ```tsx
   <div className="relative h-full min-h-0">
     <div className="absolute inset-0 flex min-h-0 flex-col">{start}</div>
     {open ? (
       <aside
         aria-label="Trace"
         className="absolute inset-y-0 right-0 z-20 flex w-[70%] min-w-[480px] max-w-full flex-col border-l bg-background shadow-xl"
       >
         {end}
       </aside>
     ) : null}
   </div>
   ```
   Hooks (`useGroupRef`, `useState`, `useEffect`) are called before the branch; guard the `useEffect` with `if (mode === "drawer") return;` so it does not touch a missing group handle. Nothing is persisted for the drawer.
3. Routes: pass `mode={layout}` directly (remove the ternary).
4. The `end` content (`TraceView`) already has its own close button (`onClose`) and `Esc` is handled by the existing `dismiss` shortcut; add nothing.
5. Row click while the drawer is open must swap the trace (already true: it only changes `?trace=`).

**Tests**
- `split-pane.test.tsx`: `mode="drawer"` and `open={false}`: no `aside`; `open`: `aside` with name "Trace" exists and the `start` content is still there; no `role="dialog"` in the document (it is non-modal).
- Traces route: `list_layout="drawer"` and a trace open: the grid is present and `getByRole("complementary", { name: "Trace" })` is present; pressing `Escape` removes `trace` from the URL.

**Done when** in the browser the list stays full width, the trace slides in over the right 70%, clicking a visible row switches trace, `j`/`k` work, `Esc` closes; `just check-web` passes.

---

## Phase 6: docs, changelog, final check

**Edit**
- `docs/spec/11-ui.md`: § Traces (list) and § Spans (list) (the "resizable split" bullets, around lines 70 and 77). Replace "Row click opens the trace in a resizable split" with the four modes: say that the layout is a per-browser preference stored in localStorage (`cetra.list_layout`, default `split`), shared by Traces and Spans, chosen with a switcher in the toolbar, and describe each mode in one sentence (Drawer is non-modal and `Esc` closes it; Rail keeps `start`/`name`/`duration`, or `kind`/`name`/`duration` for spans, and a compact toolbar; Bottom is a vertical split; per-mode ratios are stored under `cetra.split.traces`, `.rail` and `.bottom`). Keep the sentence about `j`/`k` and `Esc`.
- `docs/mvp-decisions.md`: add `D-C-n` rows in track C's section (next free number; look at the last `D-C-` row) for: (a) four layouts, preference not in the URL; (b) Drawer is non-modal and not the Radix `Sheet`; (c) Rail uses a fixed column set and a compact toolbar.
- `CHANGELOG.md` § `[Unreleased]` → `Added`: "A layout switcher on the Traces and Spans lists: split, drawer, rail and bottom."
- `docs/roadmap.md`: only if track C's row mentions the list layout; otherwise leave it.
- Tick this plan's status line at the top: "Done in PR #…" (leave the number for the PR).

**Steps**
1. Edit the files above.
2. `export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH`, `just fmt`, `just check-web`.
3. Manually try all four modes in `just web-dev` (mock API): open a trace in each, reload, switch modes with the trace open, close with `Esc`.

**Done when** `just check-web` passes, `git status` shows only the files named in this plan, and the report lists any deviation.

## Report (end of every phase)

What was done, any deviation from this plan and why, exact versions of any new dependency (there should be none), anything left undone or unverified.
