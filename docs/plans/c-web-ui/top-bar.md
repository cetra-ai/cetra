# Top bar: breadcrumb, ingestion dot, firing-alerts bell (D-C-20)

The top bar becomes a breadcrumb that ends with the current page and, where a page has one, its open item. The page toolbars lose their own title, and the dashboard switcher and saved-views menu move up into the breadcrumb. On the right, an ingestion dot says whether spans are arriving. While alert rules fire, a bell lists them. The server gains `last_ingest_at` on `GET /projects/{project}` for the dot.

**Prerequisites:** `main` at or after `d35e649` (60-column grid). The specs and D-C-20 are written in this branch's first commit, together with this plan.

**Read first:**
- `docs/mvp-decisions.md` D-C-20, D124, D-C-12.
- `docs/spec/11-ui.md` § Layout (the "A thin top bar…" paragraph).
- `docs/spec/02-ingestion.md` (the `genai_log_events` paragraph, now with `last_ingest_at`).
- `crates/server/src/ingest/mod.rs` (`commit_in_task`).
- `crates/server/src/routes/otlp.rs` (`export` path that calls `commit_in_task`, and its tests' `post_traces` and `plain_export`).
- `crates/server/src/ingest/grpc.rs` (the `commit_in_task` call).
- `crates/server/src/routes/projects.rs` (`get_project`, `project_json`, tests).
- `crates/server/src/api/account.rs` (`Project`).
- `crates/server/src/api/alerts.rs` (the `last_fired_at` serde pattern).
- `crates/server/src/http/mod.rs` (`AppInner`).
- `web/src/components/shell/top-bar.tsx`, `top-bar-layout.tsx`, `project-switcher.tsx`, `web/src/components/org-switcher.tsx`, `web/src/components/command/palette.tsx`.
- `web/src/components/list-toolbar.tsx`, `web/src/components/saved-views/saved-views-menu.tsx`.
- `web/src/components/dashboard/dashboard-body.tsx`, `dashboard-header.tsx`.
- `web/src/components/shell/use-nav-badges.ts` (the alert rules query and the firing rule), `web/src/lib/format.ts` (`formatRelative`).

## What it is, and what it is not

When done:
- **Bar layout:** `[mark] cetra / Acme / Support bot / Traces › Errors ▾`, with `● ` `🔔 2` `Search or jump to… ⌘K` on the right.
- **Org and project:** shown as text with an up-down chevron, outlined on hover. The demo org shows a "Demo" badge.
- **Page name:** the page's `<h1>` and a link to the page.
- **Last part:** the dashboard switcher, the saved-views menu, or a detail page's item name.
- **Ingestion dot:** green when spans arrived over OTLP in the last 5 minutes.
- **Bell:** shows only while enabled rules are firing.

Out of scope:
- Trace, alert-rule and experiment panels opened beside a list: they get no crumb, since their panel header names them.
- Org, account, admin and settings pages: they keep their own titles, and the bar shows no page crumb there.
- The sidebar: unchanged, and keeps its alert badge.
- Persisting the last ingest time across restarts.

## Decisions this plan relies on

| ID | Summary |
|---|---|
| D-C-20 | Breadcrumb top bar; page name is the page's only `<h1>` (plain text); switchers move up; ingestion dot from `last_ingest_at` (in memory, OTLP batches only); firing-alerts bell with a popover; the sidebar badge stays. |
| D124 | A thin top bar (kept: 40 px). |
| D-C-16, D-C-17 | Dashboards and the browser-local store (the switcher keeps working on both). |

User's answers (2026-11-18):
- **Ingestion signal:** a new server field.
- **Bell:** a popover of firing rules.
- **Breadcrumb tail:** the open item, with the switchers moved up.
- **Sidebar badge:** kept.

## Versions

No new dependency.

## Phases and dependencies

```
TB1 (server: last_ingest_at) ─────────────────────────────┐
TB2a-1 (demote item headings) ─┐                          │
TB2a-2 (drop list headings)   ─┴─► TB2a-3 (the bar) ─┬────┴─► TB2b (dot + bell)
                                                     ├─► TB3a (switchers into the bar)
                                                     └─► TB3b (detail crumbs)
```

- TB1, TB2a-1 and TB2a-2 run in parallel: no shared file.
- TB2a-3 needs TB2a-1 and TB2a-2 merged. Between them and TB2a-3, pages briefly have no `<h1>`; nothing asserts a level-1 heading until TB2a-3 adds the bar's.
- TB2b needs TB1 (the `last_ingest_at` type in `schema.gen.ts`) and TB2a-3 (`top-bar.tsx`). TB3a and TB3b need TB2a-3 (`TopBarCrumb`).
- TB2b, TB3a and TB3b run in parallel: their Edit lists do not overlap. Each worktree branches from the plan branch once TB1 and TB2a-3 are merged into it.
- Every commit leaves `just check-web` green.
- `schema.gen.ts` and `openapi.json` belong to TB1 only.

**Finding the tests to update.** A phase that changes headings or moves a control first runs `grep -rn 'getByRole("heading"\|findByRole("heading"\|getByRole("link", { name:' web/src --include=*.test.tsx` (and the `queryBy`/`getAllBy` forms, plus `getByText` of the page names and `getByRole("button", { name: "Search" })`) over the pages it touches, updates the ones its change breaks, and lists each updated test in its report.

---

## TB1: `last_ingest_at` on the project

**Goal:** the server records when it last committed an OTLP batch with spans for each project, and `GET /projects/{project}` returns it.

**Edit:**
- `crates/server/src/ingest/last_ingest.rs` (new)
- `crates/server/src/ingest/mod.rs` (module declaration only)
- `crates/server/src/http/mod.rs` (one `AppInner` field)
- `crates/server/src/serve.rs` (`AppInner` built at line ~465) and `crates/server/src/test_support/mod.rs` (~374): construct the field
- `crates/server/src/routes/otlp.rs` (one call, tests) and `crates/server/src/ingest/grpc.rs` (one call)
- `crates/server/src/routes/projects.rs` (`get_project`, `project_json`, tests)
- `crates/server/src/routes/auth.rs` (the `Project` literal at ~202-212, the setup answer)
- `crates/server/src/api/account.rs` (`Project` field, and the literal in its snapshot test at ~438)
- `crates/server/openapi.json` and `web/src/api/schema.gen.ts` (regenerated with `just openapi`)

`fixtures/api/static/project.json` does **not** change: the mocks reuse it for the create and patch answers, which never carry the field. The dev mocks therefore show the dot's "unknown" state. `crates/server/tests/snapshots/project.snap` does not change either (`None` is skipped).

**Contracts:**

```rust
// crates/server/src/ingest/last_ingest.rs
//! When each project last received spans over OTLP (D-C-20). In memory only: unknown after a
//! start until the project's next batch.

use std::collections::HashMap;
use std::sync::Mutex;

use cetra_core::ids::ProjectId;

#[derive(Debug, Default)]
pub(crate) struct LastIngest {
    times: Mutex<HashMap<ProjectId, i64>>,
}

impl LastIngest {
    /// Records a committed batch with spans at `now_ns`; an older time never replaces a newer one.
    pub(crate) fn record(&self, project: ProjectId, now_ns: i64);
    /// The time of the project's last committed batch with spans, if any since the server started.
    pub(crate) fn get(&self, project: ProjectId) -> Option<i64>;
}
```

- A poisoned mutex: `lock().unwrap_or_else(std::sync::PoisonError::into_inner)` (the map holds plain numbers and stays valid).
- `AppInner` gains `pub(crate) last_ingest: LastIngest,`, built with `LastIngest::default()`.
- `Project` (in `api/account.rs`) gains, after `genai_log_events`:

```rust
    /// When the server last committed spans sent over OTLP to this project (D-C-20). Kept in
    /// memory: absent until the first batch after the server starts. Present only when one
    /// project is read.
    #[serde(default, skip_serializing_if = "Option::is_none", with = "crate::api::keys::optional_time")]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub last_ingest_at: Option<i64>,
```

- The three `Project` literals (`project_json`, `auth.rs`, the `account.rs` snapshot test) set `last_ingest_at: None`.

**Steps:**
1. Write `last_ingest.rs`: `record` keeps `max(existing, now_ns)`. Declare `pub(crate) mod last_ingest;` in `ingest/mod.rs`.
2. Add the `AppInner` field and its two constructions.
3. `routes/otlp.rs`, right after `let receipt = commit_in_task(state, target, normalized, reservation).await?;`: `if receipt.accepted > 0 { state.last_ingest.record(project.id, now_ns); }`.
4. `ingest/grpc.rs`, right after its `commit_in_task(...)` call: the same with its `now` variable.
5. Do not touch `commit_in_task`, `Ingest::commit` or `eval/execute.rs`: evaluation commits are not counted.
6. `get_project`: after `genai_log_events`, `project.last_ingest_at = state.last_ingest.get(access.project.id);`. Lists, create, update and the setup answer leave it `None`.
7. Run `just openapi`.

**Tests:**
- `last_ingest.rs`:
  - `an_unknown_project_has_no_time`.
  - `record_keeps_the_newest_time`: record 10, then 5, and `get` is 10; record 20, and `get` is 20.
  - `projects_are_kept_apart`.
- `routes/otlp.rs`:
  - `an_export_with_spans_sets_last_ingest_at`: like `protobuf_export_is_stored_and_answers_an_empty_body` (`post_traces` with `plain_export(2)`), then `app.state.last_ingest.get(seeded.project.id) == Some(app.state.clock.now_ns())`. The test clock is manual, so read it after the request.
  - `an_export_whose_only_span_is_rejected_leaves_last_ingest_at_unset`: the request of `a_request_whose_only_span_is_rejected_stores_nothing`, then `get` is `None`. That request reaches `commit_in_task` with `accepted == 0`, so the test covers the guard.
- `routes/projects.rs`:
  - `get_project_has_no_last_ingest_at_before_spans_arrive`: no `last_ingest_at` key in the JSON.
  - `get_project_shows_last_ingest_at_after_a_batch`, in the style of the `genai_log_events` test (~787, with `signed_in`, `support_bot` and its `read(Method::GET)` request):
    ```rust
    const T: i64 = 1_767_225_600_123_456_789; // 2026-01-01T00:00:00.123456789Z
    app.state.last_ingest.record(created.id, T);
    let response = read(Method::GET).send().await;
    let body: serde_json::Value = response.json();
    assert_eq!(body["last_ingest_at"], "2026-01-01T00:00:00.123456789Z");
    let fetched: Project = response.json();
    assert_eq!(fetched.last_ingest_at, Some(T));
    ```
    The absence tests assert `body.get("last_ingest_at").is_none()`.
  - `lists_never_show_last_ingest_at`.
  - `internal_commits_do_not_count`: `app.commit_spans(...)` (the path evaluation uses) leaves `get` as `None`.

**Done when:**
- `cargo test -p cetra-server --lib -- ingest::last_ingest routes::otlp routes::projects routes::auth api::account` passes.
- `cargo test -p cetra-server --test openapi --test api_examples` passes.
- `cargo clippy -p cetra-server --all-targets -- -D warnings` and `cargo fmt --check` pass.
- `just openapi` leaves no further diff.
- `cd web && pnpm exec vitest run src/mocks --maxWorkers=8` passes, then `rm -rf web/.vitest`.
- These targeted runs replace `just check-rust` for this phase (machine load); the full `just check` runs once after the merges.

**Commit:** `Record when each project last received spans`

---

## TB2a-1: Demote the item pages' headings

**Goal:** form, detail and compare pages use `<h2>` for their own heading, so the bar's page heading will be their only `<h1>`.

**Edit:**
- `<h1>` → `<h2>`, same classes:
  - `routes/$org/$project/alerts/new.tsx:21`, `alerts/$ruleId/edit.tsx:46`;
  - `online-rules/new.tsx:33`, `online-rules/$ruleId/edit.tsx:90`;
  - `evaluators/new.tsx:44`, `evaluators/$evaluatorId.tsx:132`;
  - `datasets/$datasetId.tsx:195`;
  - `experiments/compare.tsx:172`;
  - `components/experiments/experiment-detail.tsx:130`.
- The tests this breaks: see "Finding the tests to update". A `getByRole("heading", { name })` without a level keeps working; one with `level: 1` becomes `level: 2`.

**Steps:** change the tag only. Change nothing else on these pages.

**Tests:** the updated ones. Add one assertion to `datasets/$datasetId.test.tsx`: the dataset's name is a level-2 heading.

**Done when:**
- The touched tests pass (`--maxWorkers=8`).
- `tsc` and Biome pass.
- `just check-web` passes.
- `rm -rf web/.vitest`.

**Commit:** `Demote form and detail headings to second level`

---

## TB2a-2: Drop the list pages' headings

**Goal:** the list pages lose the page heading the top bar will hold.

**Edit:**
- Remove the `<h1>`:
  - `routes/$org/$project/alerts/index.tsx:249`;
  - `online-rules/index.tsx:305`;
  - `datasets/index.tsx:166`;
  - `experiments/index.tsx:218`;
  - `evaluators/index.tsx:186`;
  - `components/sql/sql-console.tsx:170`.
- The tests this breaks.

**Steps:**
1. Delete each element. A removed heading that was its row's only left-hand item leaves the row's layout as is: no empty spacer, and do not restyle the row.
   - Exception, Alerts: its heading is the trigger of the cadence tooltip ("Rules are evaluated every minute, …"). Keep the `Tooltip` and its content, and replace the `<h1>` trigger with `<Button type="button" variant="ghost" size="icon-xs" aria-label="How often rules are evaluated"><Info /></Button>` (`Info` from `lucide-react`), at the same place in the row. Online rules has the same pattern (its heading triggers the rule-count note): the same button, `aria-label="About online rules"`.
2. `ListToolbar` is not touched here (TB2a-3).

**Tests:** the updated ones. A test that found the page by its heading finds it by another element of the page instead, such as its main button or table.

**Done when:**
- The touched tests pass (`--maxWorkers=8`).
- `tsc` and Biome pass.
- `just check-web` passes.
- `rm -rf web/.vitest`.

**Commit:** `Drop the list page headings the top bar will hold`

---

## TB2a-3: The bar is a breadcrumb and holds the page heading

**Goal:** the bar shows `mark cetra / org / project / Page`, with the page name as the page's only `<h1>`, and a slot for the page's item. `ListToolbar` loses its title.

**Edit:**
- `web/src/components/shell/top-bar.tsx`, `top-bar-layout.tsx`
- `web/src/components/shell/top-bar-slot.tsx` (new), `web/src/components/shell/page-crumb.ts` (new)
- `web/src/components/shell/project-switcher.tsx`, `web/src/components/org-switcher.tsx`
- `web/src/components/command/palette.tsx` (trigger only, ~157-170)
- `web/src/components/list-toolbar.tsx` and `list-toolbar.test.tsx` (its cases pass `title`)
- The five callers, dropping `title` only:
  - `routes/$org/$project/traces/index.tsx`, `spans/index.tsx`, `sessions/index.tsx`, `users/index.tsx`;
  - `components/dashboard/dashboard-view.tsx` (~242).
- `web/src/components/shell/top-bar.test.tsx` (new)
- The tests these changes break (see "Finding the tests to update"), plus one `level: 1` assertion per list page's route test (below)

Not changed: `error-state.tsx`, `not-found.tsx`, `forbidden.tsx` and `auth-layout.tsx` keep their `<h1>`, since they are shared with pages outside a project. A project page showing one of them (a missing dataset, say) therefore has two `<h1>`s. This is accepted (D-C-20 is about pages, not error screens). `markdown.tsx` renders user content.

**Contracts:**

```tsx
// top-bar-slot.tsx
/** Holds the top bar's item slot: the element pages portal their crumb into. */
export function TopBarSlotProvider({ children }: { children: ReactNode }): JSX.Element;
/** The slot, rendered by the top bar after the page crumb. */
export function TopBarSlotTarget(): JSX.Element;
/**
 * A page's last crumb: rendered into the top bar after "›", or nothing when the bar has no slot
 * (a page rendered alone in a test, or a page with no page crumb). Render at most one at a time.
 */
export function TopBarCrumb({ children }: { children: ReactNode }): JSX.Element | null;
```

- `TopBarSlotProvider` keeps `const [slot, setSlot] = useState<HTMLElement | null>(null)` in a context with `{ slot, setSlot }`.
- `TopBarSlotTarget` renders `<span ref={setSlot} className="flex min-w-0 items-center gap-1.5" />`. When it unmounts, React calls the ref with `null`, so crumbs render nothing.
- `TopBarCrumb` reads `slot`. Null slot: return null. Otherwise `createPortal(<><ChevronRight aria-hidden className="size-3.5 shrink-0 text-muted-foreground" />{children}</>, slot)`.
- `TopBarLayout` wraps its whole tree in `TopBarSlotProvider`.

```ts
// page-crumb.ts
import { projectNav } from "./nav";
/** The project page a path is on: the nav entry whose path is the path or a prefix of it ending at a `/`. */
export function pageOf(
  pathname: string,
  org: string,
  project: string,
): (typeof projectNav)[number] | undefined;
```

- Each entry's `to` with `$org` and `$project` interpolated (slugs need no encoding, as in `isProjectSettingsPath` in `settings-nav.ts`); match the exact path or a path starting with it plus `/`.
- Settings paths match nothing, since no nav entry is `/settings`.
- Item pages under a page (`/alerts/new`, `/online-rules/$ruleId/edit`) match their page: the bar says "Alerts" and the page's own heading (now `<h2>`) names the form.

**Steps:**
1. `TopBar`, left to right, inside `<nav aria-label="Breadcrumb" className="flex min-w-0 items-center gap-1.5">`:
   1. The product link: `<Link to="/">` containing `<img src="/favicon.svg" alt="" className="size-4" />` then `PRODUCT_NAME`.
   2. When `inOrg`: an `aria-hidden` `/` span (`text-muted-foreground/60`), then `<OrgSwitcher />`, `/`, `<ProjectSwitcher />`.
   3. On a project page where `pageOf(...)` finds an entry: `/`, then `<h1 className="truncate text-body font-medium">{entry.label}</h1>` (plain text, no link: the sidebar has it), then `<TopBarSlotTarget />`.
   4. After the nav: the `flex-1` spacer, the command palette, then the theme toggle and user menu as today.
2. `ProjectSwitcher` and `OrgSwitcher` triggers:
   - `variant="ghost" size="sm"` (a background on hover), with the class `h-7 gap-1 px-1.5 font-medium`.
   - Content: the name in `<span className="max-w-48 truncate">`, then `<ChevronsUpDown className="size-3.5 text-muted-foreground" aria-hidden />`.
   - The org trigger also shows `<Badge variant="secondary">Demo</Badge>` before the chevron when the current membership has `demo: true` (`org-switcher.tsx` already finds it).
   - Keep the `aria-label`s.
3. Palette trigger: width `w-56`, text "Search or jump to…". Keep the `⌘K` kbd and the trigger's `aria-label`.
4. `ListToolbar`: remove the `title` prop and its `<h1>`. Update the five callers and `list-toolbar.test.tsx`.
5. `CHANGELOG.md` is not touched here (TB2b adds the line).

**Tests:**
- `top-bar.test.tsx`:
  - `pageOf`:
    - finds Traces for `/acme/support-bot/traces` and `/acme/support-bot/traces/abc`;
    - finds Dashboard for `/acme/support-bot/dashboard/123` and Alerts for `/acme/support-bot/alerts/new`;
    - returns undefined for `/acme/support-bot/settings/general` and `/acme/support-bot/tracesx`.
  - `shows the breadcrumb with the page as the only heading`: render the app on `/acme/support-bot/traces` the way `routes/$org/$project/layout.test.tsx` does. The Breadcrumb navigation contains the org name, the project name and the level-1 heading "Traces", and `getAllByRole("heading", { level: 1 })` has length 1.
  - `shows no page heading on org pages`: on `/acme`, the Breadcrumb has no heading.
  - `renders a page's crumb after the page`: render `TopBarLayout` around a child that renders `<TopBarCrumb>Item</TopBarCrumb>` on a project route. "Item" is inside the Breadcrumb.
  - `TopBarCrumb renders nothing without the layout`.
  - `marks the demo org`: `server.use` overrides `GET /api/auth/me` with the me fixture's current org marked `demo: true`. The org trigger contains "Demo".
- For each list page whose heading was removed (Traces, Spans, Sessions, Users, Dashboard, Alerts, Online rules, Datasets, Experiments, Evaluators, SQL): its existing route test checks `getAllByRole("heading", { level: 1 })` is one heading named after the page. One assertion per test file, added to a test that renders the page.
- Update the broken tests and list them.

**Done when:**
- The touched tests pass: `pnpm exec vitest run <files> --maxWorkers=8`.
- `pnpm exec tsc -b --noEmit` passes.
- `node_modules/.bin/biome check web/src` (from the repo root) passes.
- `just check-web` passes.
- `rm -rf web/.vitest`.

**Commit:** `Make the top bar a breadcrumb that holds the page heading`

---

## TB2b: Ingestion dot and firing-alerts bell

**Goal:** inside a project, the bar's right side shows the ingestion dot and, while rules fire, the bell.

**Edit:**
- `web/src/components/shell/top-bar.tsx` (render the two components)
- `web/src/components/shell/ingest-dot.tsx`, `alert-bell.tsx` (new), with `ingest-dot.test.tsx` and `alert-bell.test.tsx`
- `web/src/components/shell/use-nav-badges.ts` (export a `firingRules` helper and use it)
- `CHANGELOG.md`

**Contracts:**

```ts
// use-nav-badges.ts
/** The enabled rules that are firing, in the response's order. */
export function firingRules(rules: readonly Schema<"AlertRule">[]): Schema<"AlertRule">[];
```

**Steps:**
1. `firingRules` filters `rule.enabled && rule.state === "firing"`. `useNavBadges` uses it for its count.
2. `TopBar`: inside a project, between the spacer and the palette, render `<IngestDot org={org} project={project} />` then `<AlertBell org={org} project={project} />`.
3. `IngestDot({ org, project })`:
   - Data: `useQuery({ ...projectQuery(org, project), refetchInterval: 30_000 })`. `projectQuery` is in `api/queries.ts`, and its key is the one the project layout reads, so the layout's project data refreshes every 30 s too.
   - Clock: `const [now, setNow] = useState(Date.now)` and `useEffect(() => { const timer = setInterval(() => setNow(Date.now()), 15_000); return () => clearInterval(timer); }, [])`.
   - State:
     - `receiving` when `last_ingest_at` is present and `now - Date.parse(last_ingest_at) < 5 * 60_000`;
     - `quiet` when present and older;
     - `unknown` when absent.
   - While the query loads or after it fails: render nothing.
   - Render inside its own `<TooltipProvider>` (the bar is outside the sidebar's provider):
     - a `Tooltip` whose trigger is a `Link` to `/$org/$project/traces`, `aria-label` set to the tooltip text, with class `flex size-6 items-center justify-center rounded-sm`;
     - inside the link, a `size-2 rounded-full` span: `bg-emerald-500` when receiving, else `bg-muted-foreground/40`.
   - The text, exactly:
     - receiving: `Receiving spans · last ${formatRelative(at, now)}`;
     - quiet: `No spans in the last 5 minutes · last ${formatRelative(at, now)}`;
     - unknown: `No spans received since the server started`.
4. `AlertBell({ org, project })`:
   - Data: `useQuery({ ...alertRulesQuery(org, project), refetchInterval: 60_000 })`, the sidebar's query and options.
   - `firing = firingRules(data.data)`. When it is empty, or the query is loading or failed: render nothing.
   - Otherwise a controlled `Popover` (`open`, `onOpenChange`):
     - Trigger: `<Button variant="ghost" size="sm" className="h-7 gap-1 px-1.5 text-destructive" aria-label={label}>` with `<BellRing className="size-3.5" />` and the count. `label` is `` `${n} alert ${n === 1 ? "rule" : "rules"} firing` ``.
     - `PopoverContent align="end" className="w-72 p-0"`: a header "Firing alerts", one row per rule, then a footer link "All alerts" to `/$org/$project/alerts`.
     - Each row is a `Link` to `/$org/$project/alerts` with `search={{ rule: rule.id }}`. It shows the rule's `name` (truncated) and, when `last_fired_at` is present, `fired ${formatRelative(rule.last_fired_at, Date.now())}` in muted caption.
     - Each link sets `open` to false on click.
5. `CHANGELOG.md` § Unreleased → Changed: "The top bar is a breadcrumb to the current page and its open item, shows whether spans are arriving, and lists firing alerts (D-C-20)."

**Tests** (each overrides its endpoint with `server.use`):
- `ingest-dot.test.tsx`, with fake timers and a fixed `Date.now`:
  - `is green and says when, when spans arrived in the last 5 minutes`.
  - `is grey after 5 minutes`.
  - `says no spans since the start when the field is absent`.
  - `turns grey without a refetch as time passes`: advance past 5 minutes + 15 s.
- `alert-bell.test.tsx`:
  - `is absent when no enabled rule fires`: a disabled firing rule does not count.
  - `shows the count and lists the firing rules with links to them`.
  - `the All alerts link goes to the alerts page and closes the popover`.
- `use-nav-badges.test.tsx`: `firingRules keeps enabled firing rules in order`.

**Done when:**
- The touched tests pass (`--maxWorkers=8`).
- `tsc` and Biome pass.
- `just check-web` passes.
- `rm -rf web/.vitest`.

**Commit:** `Show whether spans are arriving and which alerts fire in the top bar`

---

## TB3a: Switchers into the breadcrumb (list pages, dashboard)

**Goal:** Traces, Spans, Sessions and Users show their saved-views menu as the breadcrumb's last part, and the dashboard shows its switcher there.

**Edit:**
- `web/src/components/list-toolbar.tsx` (where `views` renders) and `list-toolbar.test.tsx`
- `web/src/routes/$org/$project/traces/index.test.tsx` and `web/src/routes/$org/$project/dashboard/$dashboardId.test.tsx` (new tests)
- `web/src/components/dashboard/dashboard-body.tsx`, `dashboard-header.tsx` (the switcher's trigger style only)
- The tests this breaks

**Steps:**
1. `ListToolbar`: where it rendered `{views}`, render `{views ? <TopBarCrumb>{views}</TopBarCrumb> : null}`.
2. In `DashboardGrid` (`dashboard-body.tsx`, where `active` is a destructured prop and the switcher renders at ~381): render `<DashboardSwitcher dashboard={custom} />` inside `<TopBarCrumb>` instead of the toolbar row, and only when `active` is true: `{active ? <TopBarCrumb>…</TopBarCrumb> : null}`. The kept dashboard lives in a hidden `Activity` on other pages, and a hidden portal would still sit in the slot. The row keeps Edit, the menu and the status.
3. `DashboardSwitcher` trigger: `variant="ghost" size="sm"` with `h-7 gap-1 px-1.5 font-medium`. Keep `aria-label="Switch dashboard"`.

**Tests:**
- `list-toolbar.test.tsx`: `renders the views menu in the top bar slot`. Render `TopBarLayout` around a `ListToolbar` with `views={<span>Views</span>}`, and check "Views" is inside the Breadcrumb navigation.
- Traces route test: `the saved views menu sits in the breadcrumb`.
- `dashboard/$dashboardId.test.tsx`:
  - `the dashboard switcher sits in the breadcrumb on the dashboard`.
  - `the breadcrumb has no dashboard switcher on Traces after visiting the dashboard`: no element named "Switch dashboard" inside the Breadcrumb.
- Update the broken tests and list them.

**Done when:**
- The touched tests pass (`--maxWorkers=8`).
- `tsc` and Biome pass.
- `just check-web` passes.
- `rm -rf web/.vitest`.

**Commit:** `Move the saved views and dashboard switchers into the top bar`

---

## TB3b: Detail crumbs

**Goal:** detail pages name their item in the breadcrumb.

**Edit:**
- `web/src/routes/$org/$project/datasets/$datasetId.tsx`, `evaluators/$evaluatorId.tsx`, `users/$userId.tsx`, `sessions/$sessionId.tsx`
- Their route tests

**Steps:**
1. Add `<TopBarCrumb><span className="max-w-64 truncate text-body font-medium">{name}</span></TopBarCrumb>`:
   - **dataset:** `dataset.name`, once loaded;
   - **evaluator:** `evaluator.name`, once loaded;
   - **user:** `identity.name ?? userId`, where `identity` is the page's existing `userIdentity(user?.metadata)` value (~237);
   - **session:** the `sessionId` route param, in `font-mono` with `max-w-48`, at once, since the page loads nothing itself.
2. Their own `<h2>` headings (demoted in TB2a) stay.

**Tests:**
- In each route test: `<item> names itself in the breadcrumb` (the dataset, the evaluator, the user and the session). The text is inside the Breadcrumb navigation.

**Done when:**
- The touched tests pass (`--maxWorkers=8`).
- `tsc` and Biome pass.
- `just check-web` passes.
- `rm -rf web/.vitest`.

**Commit:** `Name open items in the top bar`

---

## After the phases

- Merge TB2b, TB3a and TB3b into the plan branch one at a time, running `just check-web` after each.
- Update `docs/roadmap.md` (track C's row: the top bar, D-C-20) in the branch.
- A Sonnet review covers the whole branch, then fixes go through a Haiku brief.
- Finally run `just check` and look at the bar in the dev server:
  - on Traces with a saved view, the dashboard, a dataset, the SQL page and an org page;
  - the dot shows "No spans received since the server started" on the mocks;
  - the bell shows with the mock's firing rule.
