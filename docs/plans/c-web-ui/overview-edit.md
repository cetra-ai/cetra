# Editable Overview and drag anywhere

Track C. Decision D-C-18 (amends D-C-16). One plan, three phases (OE1–OE3), each one commit.

## Summary

Members can edit the project's Overview in place: it shows the built-in preset until the first save, which stores a project copy (`dashboards.kind = 'overview'`) behind three new endpoints, and "Reset to default" deletes that copy. Members can also drag a chart by its title bar and resize it at any time, without entering Edit: the layout saves itself after the last move, and an Undo bar restores the previous layout.

**Prerequisites:** the custom dashboards plan (`docs/plans/c-web-ui/custom-dashboards.md`, CD1–CD8) is merged; branch `overview-edit` is cut from `cfbfd62`.

**Read first:**
- `docs/mvp-decisions.md` D-C-16, D-C-17, D-C-18
- `docs/spec/04-metadata-db.md` § Dashboards
- `docs/spec/09-api.md`, the `/dashboards` rows
- `docs/spec/11-ui.md` § Dashboard: Overview, **Moving charts**, Editing, Demo org
- `AGENTS.md`

## What it is, and what it is not

**When it's done:**
- A member opens the Overview, clicks Edit, changes it and saves. The project now has its own Overview, and other members see it.
- "Reset to default" in the Overview's ⋯ menu brings the preset back.
- On any dashboard a member can edit (custom or Overview), dragging a chart's title bar or resizing its corner saves the layout about 0.8 s after the last move. An Undo bar appears for 10 s.

**Out of scope:**
- Demo-org viewers keep a read-only Overview (D-C-17). Their own browser dashboards get the always-on drag through the same code, with the local store.
- No per-user layouts. No keyboard drag outside Edit (the widget menu's Move items stay in Edit).

## Decisions this plan relies on

| ID | Summary |
|---|---|
| D-C-16 | Custom dashboards: the dashboards table and API, members edit, version check (409) |
| D-C-17 | Demo-org viewers: browser-local dashboards, preset Overview |
| D-C-18 | Overview editable in place, stored on first save, "Reset to default"; drag/resize with autosave and Undo |

User answers (2026-11-18):
- dragging is "always on, autosave";
- the copy is made "on first save";
- the Overview can only be edited and reset (no rename, no delete).

## Versions

No new dependency.

## Phases and parallelism

```
OE1 (server)  ─┐
               ├─> OE3 (web: Overview editing)
OE2 (web drag) ┘
```

- **OE1 and OE2 run in parallel,** in two worktrees cut from the same base. OE1 touches only `crates/server`, `fixtures/api` and the generated `crates/server/openapi.json` and `web/src/api/schema.gen.ts`. OE2 touches only `web/src/components/dashboard/*` and `web/src/lib/dashboards/*`, and does not regenerate anything.
- **OE3 starts after both are merged** into `overview-edit`.

---

## OE1 — Store and serve the project's Overview (server)

**Goal:** a project can store, read and reset its own Overview through `/dashboards/overview`, and the other dashboard routes ignore it.

**Edit:**
- `crates/server/migrations/meta/0022_dashboard_kind.sql` (new)
- `crates/server/src/meta/dashboards.rs`
- `crates/server/src/api/dashboards.rs`
- `crates/server/src/routes/dashboards.rs`
- `crates/server/src/api/openapi.rs`
- `crates/server/tests/openapi.rs`
- `crates/server/src/api_examples.rs`
- `crates/server/tests/api_examples.rs`
- `crates/server/tests/snapshots/meta_schema.snap`
- `fixtures/api/static/dashboard-overview.json` (new)
- `fixtures/api/manifest.json`
- `fixtures/api/README.md`
- `crates/server/openapi.json` and `web/src/api/schema.gen.ts` (generated)

**Contracts:**

```sql
-- 0022_dashboard_kind.sql
ALTER TABLE dashboards ADD COLUMN kind TEXT NOT NULL DEFAULT 'custom' CHECK (kind IN ('custom', 'overview'));
CREATE UNIQUE INDEX dashboards_overview ON dashboards(project_id) WHERE kind = 'overview';
```

```rust
// meta/dashboards.rs
/// Display name of a project's stored Overview.
pub(crate) const OVERVIEW_NAME: &str = "Overview";

/// What [`save_overview`] did.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum OverviewSave {
    /// Stored. `created` is true when this save made the project's copy.
    Saved { row: DashboardRow, created: bool },
    /// The expected version is not the stored one (0 when nothing is stored). Carries the stored version.
    Stale(i64),
}

/// The project's stored Overview, if a member saved one. Reads like `get`.
pub(crate) async fn get_overview(pool: &SqlitePool, project: ProjectId) -> Result<Option<DashboardRow>, MetaError>;
/// Creates (`expected` None, nothing stored) or updates (`expected` equals the stored version) the project's Overview. Writes like `update`.
pub(crate) async fn save_overview(conn: &mut SqliteConnection, project: ProjectId, expected: Option<i64>, definition: &Definition, user: Uuid, now: i64) -> Result<OverviewSave, MetaError>;
/// Deletes the project's Overview. True when a row was deleted.
pub(crate) async fn reset_overview(conn: &mut SqliteConnection, project: ProjectId) -> Result<bool, MetaError>;
```

```rust
// api/dashboards.rs
/// The body of `PUT /dashboards/overview`.
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SaveOverview {
    /// The version the caller read; absent when the project shows the built-in preset.
    #[serde(default)]
    pub version: Option<i64>,
    /// The widgets.
    pub definition: Definition,
}
```

**Steps:**

1. **Migration.** Add the migration above (if `0022` is taken, use the next free number and say so). Update the meta schema snapshot the way its test documents.

2. **Hide the Overview row from the custom-dashboard queries.** In `meta/dashboards.rs`, add `AND kind = 'custom'` to every existing query:
   - `list`;
   - `get`;
   - `count`;
   - `get_on`;
   - the stale-version `SELECT` that `update` runs after a zero-row `UPDATE`;
   - the `UPDATE` in `update`;
   - `delete`.

   `insert` keeps the column default.

3. **`get_overview`:** select `WHERE project_id = ? AND kind = 'overview'` and map the row with `row_from`.

4. **`save_overview`:** read the stored Overview's `version`, if any, then:

   | Stored | `expected` | Action | Returns |
   |---|---|---|---|
   | nothing | `None` | insert (see below) | `Saved { created: true }` |
   | version `v` | `Some(v)` | update (see below) | `Saved { created: false }` |
   | nothing | `Some(_)` | none | `Stale(0)` |
   | version `v` | `None`, or `Some(e)` with `e ≠ v` | none | `Stale(v)` |

   - **Insert:** a new UUIDv7 id (as `insert` makes it), `OVERVIEW_NAME`, description NULL, `definition_text(definition)`, version 1, `user` as creator and editor, `now` as both times, `kind` `'overview'`.
   - **Update:** the definition, `version = v + 1`, `updated_by`, `updated_at`.
   - Return the row read back with `row_from`.

5. **`reset_overview`:** `DELETE ... WHERE project_id = ? AND kind = 'overview'`. Return `rows_affected() > 0`.

6. **Routes** in `routes/dashboards.rs`, registered next to the others, tag `dashboards`.

   | Route | Operation | Permission | Answers |
   |---|---|---|---|
   | `GET /api/v1/projects/{project}/dashboards/overview` | `get_overview` | `ProjectPerm::View` | `200 Dashboard`, or `ApiError::NotFound` |
   | `PUT …/dashboards/overview` | `save_overview` | `ProjectPerm::ManageDashboards`, `session_only: false`, the dashboard writes' `DefaultBodyLimit` | `201 Dashboard` (created), `200 Dashboard` (updated), `ApiError::StaleVersion { current: v }` (`Stale(v)`) |
   | `DELETE …/dashboards/overview` | `reset_overview` | `ProjectPerm::ManageDashboards` | `204` always |

   - The `PUT` handler, in order:
     1. `principal::session_user`;
     2. `check_definition(&body.definition, "definition", &mut errors)`;
     3. `refuse_if_any`;
     4. `check_expressions(&state, &access, &body.definition, "definition")`;
     5. `save_overview` inside `state.meta.write`.
   - Logs carry ids only: "overview saved" with `project_id` and `dashboard_id`, and "overview reset" with `project_id`, written only when a row was deleted.

7. **OpenAPI** (`api/openapi.rs`):
   - Import `SaveOverview` and add `set.insert::<SaveOverview>()`.
   - Document the three operations. `save_overview` uses `responses_with([("200", …), ("201", …)], errors)` with errors `[400, 401, 403, 404, 409, 413, 415, 429]`.
   - `get_overview` uses the `get_dashboard` error set, and `reset_overview` the `delete_dashboard` one.

8. **`tests/openapi.rs`:**
   - Add the three operations to the operation tuple list (where `get_dashboard` is).
   - Add `get_overview` to `QUERY_OR_BODY` next to `get_dashboard`.
   - Add `reset_overview` to the `delete_dashboard` error-set branch, and `save_overview` to the `create_dashboard` one.

9. **Examples:**
   - Add two examples: `get_overview` (GET, no params, status 200, `static/dashboard-overview.json`) — `tests/api_examples.rs::manifest_covers_the_read_operations` requires every GET to have one; the web mocks stay safe because `dashboardHandlers` are installed before the manifest handlers and `sortBySpecificity` puts `/dashboards/overview` before `/dashboards/:id` — and `save_overview` (PUT, status 200, same file). Add no `reset_overview` example: a 204 has no content type.
   - `fixtures/api/static/dashboard-overview.json`:
     - id `01900000-0000-7000-8000-000000000f02`;
     - name `Overview`, no description, version 3;
     - definition: the first two widgets of `static/dashboard.json`.
     - Add the id to `fixtures/api/README.md`'s table of ids.
   - In `tests/api_examples.rs`:
     - add `roundtrip::<Dashboard>("static/dashboard-overview.json")` next to the other dashboard roundtrips;
     - add `"get_overview"` and `"save_overview"` to the arm that handles `get_dashboard` and the PUT/PATCH operations (next to `get_dashboard`).
   - In `fixtures/api/README.md`, also mention `dashboard-overview.json` where `dashboard.json` is described.

**Tests** (route and store modules, following the CD3 tests):
- `overview_is_absent_until_saved`: GET gives 404.
- `first_save_creates_the_overview_at_version_1`: 201, name "Overview", version 1; GET then returns it.
- `save_with_the_read_version_updates_the_overview`: 200, version 2.
- `save_with_a_stale_version_is_409`: `detail` carries the stored version.
- `save_with_a_version_when_none_is_stored_is_409_with_0`.
- `save_without_a_version_when_one_is_stored_is_409`.
- `an_invalid_overview_definition_is_400_with_paths`: same paths as POST.
- `reset_deletes_the_overview_and_is_idempotent`: 204 twice; GET is then 404.
- `the_overview_is_not_in_the_list_nor_counted`: with 100 custom dashboards plus an Overview, the 101st custom one is refused, and the list never includes the Overview.
- `the_overview_id_is_404_on_the_custom_routes`: GET, PATCH and DELETE `/dashboards/{its id}`.
- `overview_static_segment_wins_over_id`.
- `overview_access`:
  - a viewer reads, and gets 403 on PUT and DELETE;
  - a `read` key reads, and gets 403 on writes;
  - a member writes.
- `project_deletion_removes_its_overview`.
- The updated schema snapshot.

**Done when** (in this order; commit every output):
1. `just api-examples` (it writes the manifest).
2. `just openapi` (it writes `openapi.json` and `schema.gen.ts`).
3. `cargo fmt --all -- --check`.
4. `cargo clippy --workspace --all-targets --all-features -- -D warnings`.
5. `cargo test -p cetra-server`.
6. `cd web && pnpm exec vitest run src/mocks --maxWorkers=8`: the manifest tests still pass.

**Commit:** `Store an editable Overview per project`

---

## OE2 — Drag and resize charts without Edit (web)

**Goal:** on a dashboard the user can edit, outside Edit mode, a finished drag or resize saves the new layout after 800 ms of quiet, with a 10-second Undo. Merely viewing a dashboard never saves anything.

**Edit:**
- `web/src/components/dashboard/use-layout-autosave.ts` (new)
- `web/src/components/dashboard/use-layout-autosave.test.ts` (new)
- `web/src/components/dashboard/dashboard-body.tsx`
- `web/src/components/dashboard/widget-grid.tsx`
- `web/src/components/dashboard/dashboard-edit.test.tsx`
- `web/src/components/dashboard/widget-grid.test.tsx`
- `web/src/components/dashboard/widget.tsx` (one comment only)

**Contracts:**

```ts
// use-layout-autosave.ts
/** Quiet time after the last finished move before the layout is saved. */
export const AUTOSAVE_DELAY_MS = 800;
/** How long the "Layout saved" bar offers Undo. */
export const UNDO_MS = 10_000;

export type LayoutAutosave = {
  /** The moved definition while it waits for or is being saved; undefined otherwise. */
  pending: Definition | undefined;
  /** A finished drag or resize (react-grid-layout's onDragStop / onResizeStop layout). */
  onMoveEnd: (layout: GridLayout) => void;
  /** Set for UNDO_MS after a successful layout save. */
  undo: { run: () => void } | null;
  /** A save (or an undo) is in flight. */
  saving: boolean;
};

export function useLayoutAutosave(args: {
  /** The saved definition on screen. */
  definition: Definition;
  /** The version it was read at; undefined when nothing is stored yet (the preset Overview, OE3). */
  version: number | undefined;
  /** A member (or a demo-org viewer on a browser dashboard), not in Edit, not narrow. */
  enabled: boolean;
  /** Saves a definition with the version read; resolves once the query cache holds the saved version. */
  persist: (definition: Definition, version: number | undefined) => Promise<unknown>;
  /** A 409: the move is dropped; reload. */
  onConflict: () => void;
  /** Another failure: the move is dropped; show the message. */
  onError: (message: string) => void;
}): LayoutAutosave;
```

```ts
// widget-grid.tsx: two more optional props on WidgetGrid (and FreeWidgets)
/** A finished drag, with the final layout. */
onDragStop?: (layout: Layout) => void;
/** A finished resize, with the final layout. */
onResizeStop?: (layout: Layout) => void;
```

**Steps:**

1. **Grid.**
   - Pass `onDragStop` and `onResizeStop` through to react-grid-layout's `onDragStop` and `onResizeStop`. Read their 2.3.0 signatures in `node_modules/react-grid-layout`: the first argument is the final layout. If they differ, stop and ask.
   - `onLayoutChange` stays, for the Edit draft only.

2. **Hook.** Keep `definition`, `version`, `persist`, `onConflict` and `onError` in refs updated on every render, so the timers and callbacks never read stale values.
   - **`onMoveEnd(layout)`:**
     - Compute `withLayouts((pendingRef ?? definitionRef).widgets, layout)` (`lib/dashboards/draft.ts`). If it returns the same array, do nothing.
     - Otherwise:
       - if nothing is pending, set `beforeRef` to the current definition;
       - set `pending = { widgets }`;
       - clear `undo`;
       - (re)arm a single `AUTOSAVE_DELAY_MS` timer.
   - **Timer fires.** If a save is in flight, re-arm the timer and return. Otherwise:
     1. Take `saved = pending` and set `saving = true`.
     2. `await persistRef(saved, versionRef)`, reading the version ref at that moment.
     3. On success: if `pending` is still `saved`, set `pending = undefined`. Set `undo = { run }` and an `UNDO_MS` timer that clears it.
     4. On `isStaleDashboard(error)` (from `web/src/api/dashboards.ts`): `pending = undefined`, `undo = null`, `onConflict()`.
     5. On any other error: `pending = undefined`, then `onError(problemMessage(error, "Could not save the layout."))`.
     6. `saving = false`.
   - **`undo.run()`:**
     - Clear the undo timer and `undo`.
     - Set `pending = beforeRef`, then save it through the same in-flight rule as the timer, with the same handling, but no new undo.
   - **When `enabled` turns false** (Edit starting, a hidden dashboard, a narrow window, a role change): clear `undo` and its timer (an Undo after an Edit save would revert it); if something is pending, save it at once (flush), then stop. Edit itself is disabled while anything is pending or saving, so a flush never races an Edit session.
   - `pending` is both React state (for rendering) and a ref (read by the timer and callbacks).
   - **Accepted:** after a successful save `pending` is cleared once the mutation's invalidation has resolved, so the cache already holds the new definition; a one-frame flicker is accepted if it happens.
   - **On unmount:** flush the same way, then clear every timer.

3. **`DashboardBody`.**
   - Add `async function persist(definition: Definition, version: number | undefined)`. For now it calls `update.mutateAsync({ version: version ?? 0, definition })` on the open custom dashboard, and is only reachable when `dashboard` is defined. `save()` calls `persist` too.
   - Add `notice` state (a string or null) with a 10 s clearing timer.
   - Call `useLayoutAutosave` with:
     - `definition = dashboard?.definition ?? OVERVIEW` and `version = dashboard?.version`;
     - `enabled = canEdit && session === null && active`;
     - `onConflict`: call `reload()` and set `notice` to "Someone else changed this dashboard. It was reloaded.";
     - `onError`: set `notice` to the message.
   - The shown definition is `draft ?? autosave.pending ?? dashboard?.definition ?? OVERVIEW`.
   - The Edit button is disabled while `autosave.pending !== undefined || autosave.saving`.

4. **Grid wiring.**
   - In Edit: `editing={movable}`, `onLayoutChange={applyLayout}` as today, with no stop handlers.
   - Outside Edit, when the autosave is enabled: `editing={true}`, no `onLayoutChange`, and `onDragStop` and `onResizeStop` set to `autosave.onMoveEnd`.
   - Otherwise: static.
   - Drag starts only from the title bar: `TitleBar` in `widget.tsx` already carries `DRAG_HANDLE`, and buttons and links cancel a drag. Chart clicks, bucket clicks and zoom keep working. Update `TitleBar`'s comment ("In edit mode it drags the widget") to say it drags the widget wherever the grid is movable (add `web/src/components/dashboard/widget.tsx` to this phase's edits for that comment only).

5. **Status bar** under the header, outside Edit, `role="status"`, showing the first that applies:
   1. "Saving layout…" while `saving`;
   2. "Layout saved" and a link-style `Button` "Undo" while `undo` is set;
   3. `notice` while set.

6. **Accepted limit:** if the refetch after a successful save fails, the cache keeps the old version, the layout snaps back, and the next save gets a 409, which reloads. Add a comment in the hook saying so.

**Tests:**
- `use-layout-autosave.test.ts` (fake timers, a fake `persist`):
  - `saves once after the last move`: three move ends within 800 ms give one persist with the last layout.
  - `ignores a move that changes nothing`.
  - `a move during a save waits and is saved next, with the newer version`: change `version` between the saves.
  - `offers undo for 10 s and undo saves the layout before the moves`.
  - `a new move clears the undo`.
  - `a 409 drops the move and calls onConflict`.
  - `another error drops the move and reports the message`.
  - `disabling flushes a pending move and clears the undo`.
  - `unmount flushes and clears the timers`.
- `dashboard-edit.test.tsx`:
  - `a member's grid is movable outside edit mode`: no `.react-resizable-hide` handles, and `.react-draggable` items.
  - `a viewer's grid stays static`.
  - `viewing a non-compact layout saves nothing`: render the existing gapped fixture, wait, and assert no PATCH.
  - `a bucket click still opens the list outside edit mode for a member`.
  - `Edit then Save hides Undo` (drag, wait for the save, Edit, Save: no Undo bar afterwards).
  - Existing assertions that a member's grid is static outside Edit (in `dashboard-edit.test.tsx` and `widget-grid.test.tsx`) are updated to the new rule.
  - `Edit is disabled while a layout save is pending`. If this is not reachable without module mocks, test the rule at hook level and say so.
- `widget-grid.test.tsx`: `forwards onDragStop and onResizeStop`. Assert the props reach react-grid-layout by rendering and checking that the handlers are wired. If jsdom cannot fire them, check that they are passed (a shallow prop assertion), and say so.

**Done when:** `just check-web` passes.

**Commit:** `Drag and resize dashboard charts without edit mode`

---

## OE3 — Edit the Overview in place, with a reset (web)

**Goal:** members edit, autosave and reset the project's Overview. Everyone sees the stored copy when there is one, else the preset.

**Prerequisites:** OE1 and OE2 merged into `overview-edit`, so `schema.gen.ts` has `SaveOverview` and the overview operations.

**Edit:**
- `web/src/lib/dashboards/types.ts`
- `web/src/lib/dashboards/store.ts` and `store.test.ts`
- `web/src/api/dashboards.ts` and `dashboards.test.tsx`
- `web/src/components/dashboard/dashboard-view.tsx`
- `web/src/components/dashboard/dashboard-body.tsx`
- `web/src/components/dashboard/dashboard-header.tsx`
- `web/src/components/dashboard/dashboard-dialogs.tsx`
- `web/src/mocks/dashboards.ts` and `dashboards.test.ts`
- `web/src/test/setup.ts`, only if seeding needs it
- `web/src/components/dashboard/dashboard-edit.test.tsx`
- `web/src/components/dashboard/demo-dashboards.test.tsx`
- `web/src/routes/$org/$project/dashboard/index.test.tsx`
- `CHANGELOG.md`
- `docs/roadmap.md`

**Contracts:**

```ts
// lib/dashboards/types.ts
export type SaveOverview = Schema<"SaveOverview">;

// lib/dashboards/store.ts: added to DashboardStore
/** The project's stored Overview, or null while it shows the built-in preset. */
getOverview(): Promise<Dashboard | null>;
/** PUT /dashboards/overview. The local store rejects: demo viewers never edit the Overview. */
saveOverview(body: SaveOverview): Promise<Dashboard>;
/** DELETE /dashboards/overview. */
resetOverview(): Promise<void>;

/** The query key of the stored Overview, with the trailing "local" rule of `dashboardsKey`. */
export function overviewKey(org: string, project: string, local: boolean): readonly unknown[];

// api/dashboards.ts
export function overviewQuery(org: string, project: string, store: DashboardStore); // key overviewKey
export function useSaveOverview(org: string, project: string); // mutation (SaveOverview); awaits the invalidation of overviewKey
export function useResetOverview(org: string, project: string); // mutation; awaits the invalidation of overviewKey

// components/dashboard/dashboard-body.tsx (exported for dashboard-view.tsx)
export type DashboardTarget =
  | { kind: "overview"; stored: Dashboard | null }
  | { kind: "custom"; dashboard: Dashboard };

// mocks/dashboards.ts
export function dashboardHandlers(initial: readonly Dashboard[], overview?: Dashboard): HttpHandler[];
```

**Steps:**

1. **API store.**
   - `getOverview`: a GET; a 404 problem gives `null`, any other error throws.
   - `saveOverview`: a PUT. Leave `version` out of the body when it is undefined.
   - `resetOverview`: a DELETE.
   - **Local store:** `getOverview` resolves `null`. `saveOverview` and `resetOverview` reject with the store's `problem(...)`: status 403, code `forbidden`, detail "The Overview cannot be changed here."

2. **Queries.**
   - `overviewQuery` uses `overviewKey`.
   - Both mutations await `invalidateQueries({ queryKey: overviewKey(org, project, false) })`. It is a prefix match, so it covers both stores, as `useUpdateDashboard` does.
   - **Accepted limit:** a demo viewer whose `me` has not loaded yet uses the API store, the same race the list has today. Do not change it.

3. **`DashboardView`:**
   - Without `dashboardId`, read `overviewQuery` and pass `target = { kind: "overview", stored }`.
   - With one, pass `target = { kind: "custom", dashboard }`.
   - While `overviewQuery` is pending, show `<Skeleton className="h-64" />` (as the custom dashboard loading state). A failure (any error other than the 404, which `getOverview` maps to null) shows the existing `role="alert"` paragraph with `problemMessage(error, "Could not load the dashboard.")`. `DashboardBody` mounts only once `stored` is resolved (a Dashboard or null), so the preset never flashes and autosave never runs on a preset about to be replaced.

4. **`DashboardBody`** takes `target` in place of `dashboard`:
   - **Base:** `target.kind === "overview" ? target.stored?.definition ?? OVERVIEW : target.dashboard.definition`.
   - **Version:** `target.stored?.version` for the Overview; `target.dashboard.version` otherwise.
   - **`canEdit`:** custom as today. For the Overview, `!local && canManageDashboards(role) && !narrow`.
   - **`persist`:**
     - Overview: `saveOverview.mutateAsync(version === undefined ? { definition } : { version, definition })`.
     - Custom: as in OE2.
     - A 409 from either is caught by `isStaleDashboard`.
   - **`reload()`:**
     - Overview: invalidate `overviewKey(org, project, false)`.
     - Custom: invalidate the dashboard as today.
     - Autosave's `onConflict` uses `reload()`.
   - **Children:**
     - `DashboardSwitcher` gets `target.kind === "custom" ? target.dashboard : undefined`.
     - `DashboardMenu` gets `target`.
   - Internals to change for a preset Overview (where `dashboard === undefined` used to mean "not editable"):
     - `startEditing()` and `save()` guard on `canEdit`, not on `dashboard !== undefined`.
     - `Session.version` becomes `number | undefined`; `save()` calls `persist(session.draft, session.version)`.
     - `busy = update.isPending || saveOverview.isPending` (call `useSaveOverview` in `DashboardBody`).
     - `useUpdateDashboard` is still called unconditionally (hooks order) with `target.kind === "custom" ? target.dashboard.id : ""` and only used for custom targets.
   - Edit, save, cancel, the 409 dialog and the OE2 autosave then work on the Overview.

5. **`DashboardMenu`** takes `target`.
   - **Overview:** members get "Duplicate" (from the shown definition) and, only when `target.stored !== null`, "Reset to default".
   - **Reset** opens `ResetOverviewDialog`, added to `dashboard-dialogs.tsx` with the pattern of `DeleteDashboardDialog`:
     - title "Reset the Overview?";
     - text "The project's Overview goes back to the built-in one. Changes made to it are lost.";
     - buttons Cancel and Reset (destructive);
     - Reset calls `useResetOverview`.
   - "Reset to default" is disabled while a layout autosave is pending or saving (pass the flag from `DashboardBody`).
   - **Custom dashboards:** Rename and Delete, as today.

6. **Mocks** (`mocks/dashboards.ts`):
   - `dashboardHandlers(initial, overview?)` resets the stored Overview to `overview` (absent when omitted) on every call.
   - Put the three overview handlers before the `ONE` (`/dashboards/:id`) handler in the array, so `overview` is not matched as an id.
   - **GET:** 200, or a 404 problem shaped as the existing 404s.
   - **PUT:**
     - 400 from `validateDefinition`, with the paths prefixed as the existing PATCH handler does;
     - 201 at version 1 when absent and no version was sent;
     - 200 with version + 1 when the versions match;
     - otherwise 409 with `dashboard-stale.json`'s shape and the detail text the existing PATCH 409 uses, with the stored version (0 when absent).
   - **DELETE:** 204.

7. **Docs:**
   - `CHANGELOG.md` `[Unreleased]` → `Added`: "The Overview dashboard is editable in place, with Reset to default, and charts on any editable dashboard can be dragged and resized without entering Edit; the layout saves itself, with Undo (D-C-18)."
   - `docs/roadmap.md`: in track C's row, after the custom dashboards text, add "Editable Overview and drag anywhere (D-C-18): [`plans/c-web-ui/overview-edit.md`](plans/c-web-ui/overview-edit.md), done."

**Tests:**
- Stores (`store.test.ts`, `dashboards.test.tsx`):
  - `getOverview maps 404 to null`;
  - `saveOverview omits an undefined version`;
  - `the local store has no Overview and refuses to save one`;
  - `overview keys differ between stores`.
- `mocks/dashboards.test.ts`: the PUT, GET and DELETE rules above, including the three 409 cases and the seeded Overview.
- `dashboard-edit.test.tsx`:
  - `a member edits the preset Overview and the first save sends no version`: the PUT body has no `version`, and the page then shows the stored copy.
  - `a member saves the stored Overview with its version` (seeded).
  - `a 409 on the Overview opens the reload dialog, and Reload refetches /dashboards/overview`.
  - `Reset to default appears only when a copy is stored, asks first, and brings the preset back`.
  - `the Overview cannot be renamed or deleted`.
  - `a viewer sees the stored Overview and no controls`.
  - `viewing the preset Overview saves nothing`: no PUT after render.
- `demo-dashboards.test.tsx`: `a demo viewer's Overview is the preset, with no Edit and no request to /dashboards/overview once me is loaded`.
- Route test: the Overview page reads `/dashboards/overview`, and with a seeded copy it shows that copy's widgets.

**Done when:** `just check-web` passes. Then check by hand at http://localhost:5173 with `?mock=large` (the mock keeps its state until a reload):
- edit the Overview, save, open another dashboard and come back: the change is kept;
- drag a chart by its title: "Layout saved" appears, and Undo restores the previous layout;
- "Reset to default": the preset is back.

**Commit:** `Edit the Overview in place, with a reset`
