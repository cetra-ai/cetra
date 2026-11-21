# Plan: the page shell for item pages

The Monitor and Evaluate list pages share a toolbar and an inset body (D-C-25, `docs/plans/c-web-ui/page-shell.md`). Their item pages (new and edit forms, and the evaluator, issue and issue source pages) don't. They use a column centred in the free space, either `max-w-5xl` or `max-w-3xl` wide, with the back link and title as a 15 px heading inside the column. This plan gives them the list pages' shell:
- a `PageToolbar` holding `‹ List / Name` on the left, with the page's header actions on the right;
- a left-aligned body inset like the lists;
- forms capped at a readable width;
- detail pages using the full width.

Save and Cancel stay at the bottom of the forms (user's answer).

**Prerequisites:** branch `ui-changes` at or after `846905a`, where `PageToolbar` exists (`web/src/components/page-toolbar.tsx`).

**Read first:**
- `docs/plans/c-web-ui/page-shell.md` (§ Decisions, P1);
- `web/src/components/page-toolbar.tsx`;
- `web/src/components/back-link.tsx`;
- `docs/spec/11-ui.md` § Layout and density (the paragraph that D-C-25 added).

## What it is, and what it is not

When this plan is done, these pages have the 29 px toolbar, then a body inset by `px-3 py-3`, aligned to the left:
- the new and edit pages of alert rules, evaluators, online rules and issue sources;
- the evaluator page, the issue source page and the issue page;
- the dataset page, the annotation queue page and the queue review page.

In the toolbar:
- **Left:** the back link (`BackLink` with the list as its fallback), a `/`, and the item's name as an `h2` in 13 px medium type. New pages say "New …".
- **Middle:** any muted meta line, such as the evaluator's kind and version, as `PageToolbarSummary`.
- **Right:** the header actions (Delete, Edit) as small buttons.

In the body:
- Single-column forms are capped at `max-w-3xl`.
- The alert rule form, which has a preview column, is capped at `max-w-5xl`.
- Detail pages use the full width.

Out of scope:
- **Pages already on the shell:** the experiment compare view, whose `CompareView` already has a toolbar row (line 170); only its "pick experiments" empty state (line 77) is a centred column, and that stays. Also the prompt page, which has no centred column.
- **Observe pages:** trace, session and user.
- **Org, settings and admin pages.**
- **The issue page's actions row** (Resolve, Ignore, Merge…). It stays in the body under the badges: it has up to eight buttons and dialogs.

## Decisions this plan relies on

| ID | Summary |
|---|---|
| D-C-25 | List pages share `PageToolbar`, inset bodies, `FilterChips`, and `ui/table` styled like `DataTable`. |
| D-C-26 (new, P5) | Item pages use the same shell. The toolbar holds `‹ List / Name` (`PageToolbarTitle`), muted meta and the header actions. The body is left-aligned and inset like the lists. Forms are capped at `max-w-3xl` (`max-w-5xl` for the two-column alert rule form), and detail pages use the full width. Save and Cancel stay at the bottom of the form. Rejected: centred columns of two widths (moving from a list to its item shifted the content sideways), and opening every item in a side panel of its list (route rework). |
| D-C-20 | The top bar names the page. Item pages use a second-level heading. |

The user's answers (2026-11-21):
- Option B.
- Save and Cancel stay at the bottom of the forms.

## Versions

No new dependency.

## Phases and dependencies

```
P1 PageToolbarTitle ──┬─> P2 evaluators + online rules
                      ├─> P3 alerts + issue sources
                      └─> P4 issue page, dataset page, queue pages
P2..P4 ──> P5 spec, decision, changelog
```

Phases run one after another in the `ui-changes` worktree.

**Checks:** following the user's rule (checks once per finished plan, never per phase), a phase runs only `pnpm exec biome check --write <its files>` from the repo root, then commits. The coordinating session runs the type check, the tests and the browser review once at the end (§ Final checks), and fixes what fails.

**Edit placement:** line numbers are those of `846905a`; they may be off by one or two, so match on the code shown. "Verbatim" means move that JSX unchanged.

## The recipe (each page in P2 to P4 follows it)

1. Replace the page's outer `<div className="mx-auto flex w-full max-w-… flex-1 flex-col gap-6 overflow-auto px-4 py-6">` with:

   ```tsx
   <div className="flex h-full min-h-0 w-full flex-col">
     <PageToolbar actions={ACTIONS}>
       <PageToolbarTitle back={BACK} label={LABEL}>
         <h2 className={ITEM_TITLE_CLASS}>{TITLE}</h2>
       </PageToolbarTitle>
       {META}
     </PageToolbar>
     <div className="min-h-0 flex-1 overflow-auto">
       <div className={cn("flex flex-col gap-6 px-3 py-3", WIDTH)}>
         …the rest of the old column's children, verbatim, in the same order…
       </div>
     </div>
   </div>
   ```

   - **`BACK`** is the page's existing `<BackLink fallback={…} />` element, verbatim.
   - **`TITLE`** is the old `h2`'s text.
   - **`LABEL`** is the nav's accessible name, given per page. It is required: the top bar's nav is already named "Breadcrumb".
   - **`ACTIONS`** is the old header's right-hand control (the Delete or Edit button and its `DisabledReason`), verbatim except that each `Button` gets `size="sm"`. Leave `actions` out when there was none.
   - **`META`** is `<PageToolbarSummary>…</PageToolbarSummary>` around an old muted meta line. Leave it out when there was none.
   - **`WIDTH`** is `"max-w-3xl"`, `"max-w-5xl"` or nothing, as the phase says.
2. Delete the old header wrappers: the `flex flex-wrap items-start justify-between gap-4` div, the `flex min-w-0 flex-wrap items-center gap-2` div, and the `/` span (`PageToolbarTitle` draws it).
3. Elements that are not layout stay where they are, inside the body, verbatim: a `TopBarCrumb`, a `ConfirmDialog`, a `Sheet`.
4. Early returns before the main `return` (a `Forbidden`, a loading skeleton, an error) stay as they are.
5. The body keeps the old column's `gap-6`: item pages are stacks of sections, while list bodies use `gap-4`.
6. Imports:
   - add `PageToolbar`, `PageToolbarTitle`, `ITEM_TITLE_CLASS` (and `PageToolbarSummary` when used) from `@/components/page-toolbar`;
   - add `cn` from `"cn"` when `WIDTH` is used;
   - remove imports left unused.
7. **Tests:** headings keep their text and level (`h2`), so `getByRole("heading", { name })` still works. A test that asserted the old outer classes, or the heading's `text-title` class, is updated to the new structure. Say so in the report.

---

## P1: PageToolbarTitle

**Goal:** add the item-page title block to `page-toolbar.tsx`.

**Edit:**
- `web/src/components/page-toolbar.tsx`
- `web/src/components/page-toolbar.test.tsx`

**Steps:**

1. Append to `page-toolbar.tsx`:

```tsx
/** The class of an item page's name in its toolbar (D-C-26). */
export const ITEM_TITLE_CLASS = "min-w-0 truncate text-ui font-medium";

/**
 * An item page's title in its toolbar (D-C-26): the back link, a slash, then the item's name,
 * given as an `h2` with `ITEM_TITLE_CLASS`.
 */
export function PageToolbarTitle({
  back,
  label,
  children,
}: {
  back: ReactNode;
  /** The nav's accessible name, such as "Evaluator". Not "Breadcrumb": the top bar's nav has that name. */
  label: string;
  children: ReactNode;
}) {
  return (
    <nav aria-label={label} className="flex min-w-0 shrink items-center gap-1.5">
      {back}
      <span aria-hidden="true" className="text-muted-foreground/60">
        /
      </span>
      {children}
    </nav>
  );
}
```

**Tests** (add to `page-toolbar.test.tsx`, and add `PageToolbarTitle` and `ITEM_TITLE_CLASS` to its import from `@/components/page-toolbar`):
- "PageToolbarTitle shows the back link, a slash and the name": render `<PageToolbarTitle back={<a href="/list">List</a>} label="Item"><h2 className={ITEM_TITLE_CLASS}>Item</h2></PageToolbarTitle>`.
  - `getByRole("navigation", { name: "Item" })` contains the link "List" and the heading "Item", in that order.
  - Its text is "List/Item".
- "PageToolbarTitle takes a nav label": with `label="Annotation queue"`, `getByRole("navigation", { name: "Annotation queue" })` exists.

**Done when:** biome check is clean.

**Commit:** `Add the item page title to the page toolbar`

---

## P2: evaluators and online rules

**Goal:** the recipe on four pages.

**Edit:**
- `web/src/routes/$org/$project/evaluators/new.tsx`
- `web/src/routes/$org/$project/evaluators/$evaluatorId.tsx`
- `web/src/routes/$org/$project/online-rules/new.tsx`
- `web/src/routes/$org/$project/online-rules/$ruleId/edit.tsx`
- their `*.test.tsx` files, only if step 5 of the recipe needs it.

**Steps:**
Labels: `"Evaluator"` on both evaluator pages, `"Online rule"` on both rule pages.

1. **`evaluators/new.tsx`** (line 45):
   - `WIDTH` is `"max-w-3xl"`, and the title is "New evaluator".
   - There are no actions and no meta.
   - The `EvaluatorForm` goes into the body.
2. **`evaluators/$evaluatorId.tsx`** (line 113):
   - `WIDTH` is none (full width), and the title is `{evaluator.name}`.
   - `TopBarCrumb` (lines 134–136) moves into the body, as the first child.
   - `META` wraps the text of the `<p className="text-caption text-muted-foreground">` (lines 138–141), as a `PageToolbarSummary`.
   - `ACTIONS` is the Delete `DisabledReason` block (lines 143–155), with `size="sm"`.
   - The `Tabs` and everything after it go into the body.
3. **`online-rules/new.tsx`** (line 33):
   - `WIDTH` is `"max-w-3xl"`, and the title is "New rule".
   - There are no actions.
4. **`online-rules/$ruleId/edit.tsx`** (line 70):
   - `WIDTH` is `"max-w-3xl"`, and the title is `{rule.name}`.
   - `ACTIONS` is the Delete block (lines 92–104).
   - `RuleForm` and `ConfirmDialog` go into the body.

**Done when:** biome check is clean.

**Commit:** `Put the evaluator and online rule pages on the page toolbar`

---

## P3: alerts and issue sources

**Goal:** the recipe on five pages, and back links on the alert rule editor, which has none today.

**Edit:**
- `web/src/routes/$org/$project/alerts/new.tsx`
- `web/src/routes/$org/$project/alerts/$ruleId/edit.tsx`
- `web/src/routes/$org/$project/issue-sources/new.tsx`
- `web/src/routes/$org/$project/issue-sources/$sourceId/edit.tsx`
- `web/src/routes/$org/$project/issue-sources/$sourceId/index.tsx`
- their `*.test.tsx` files, only if step 5 of the recipe needs it.

**Steps:**
Labels: `"Alert rule"` on both alert pages, `"Issue source"` on the three issue source pages.

1. **`alerts/new.tsx`** (line 38):
   - `WIDTH` is `"max-w-5xl"`, and the title is "New alert rule".
   - `BACK` is new:

     ```tsx
     <BackLink fallback={<Link to="/$org/$project/alerts" params={{ org, project }} aria-label="Back to alerts" className={BACK_LINK_CLASS}><ChevronLeft aria-hidden="true" className="size-3.5" />Alerts</Link>} />
     ```

     Imports: `BackLink` and `BACK_LINK_CLASS` from `@/components/back-link`, `Link` from `@tanstack/react-router`, `ChevronLeft` from `lucide-react`.
   - `RuleForm` goes into the body.
2. **`alerts/$ruleId/edit.tsx`** (line 45):
   - `WIDTH` is `"max-w-5xl"`.
   - The title becomes `{rule.name}` (was `Edit {rule.name}`), like the other edit pages.
   - `BACK` is like step 1, but the fallback `Link` also has `search={{ rule: rule.id }}`.
   - If a test finds the heading "Edit <name>", change it to "<name>".
3. **`issue-sources/new.tsx`** (line 14): `WIDTH` is `"max-w-3xl"`, and the title is "New source".
4. **`issue-sources/$sourceId/edit.tsx`** (line 69):
   - `WIDTH` is `"max-w-3xl"`, and the title is `{source.name}`.
   - `ACTIONS` is the Delete block.
5. **`issue-sources/$sourceId/index.tsx`** (line 115):
   - `WIDTH` is none, and the title is `{source.name}`.
   - `ACTIONS` is the Edit `Button asChild` (lines 136–143), with `size="sm"`.
   - The paused alert, `SourceDetails` and the rest go into the body.

**Done when:** biome check is clean.

**Commit:** `Put the alert rule and issue source pages on the page toolbar`

---

## P4: issue page, dataset page and queue pages

**Goal:** the recipe on the issue page. The headers that the dataset page, the queue page and the review page already have become `PageToolbar`s, so they are 29 px high like the rest.

**Edit:**
- `web/src/routes/$org/$project/issues/$issueId.tsx`
- `web/src/routes/$org/$project/datasets/$datasetId.tsx`
- `web/src/routes/$org/$project/annotation-queues/$queueId/index.tsx`
- `web/src/routes/$org/$project/annotation-queues/$queueId/review/$itemId.tsx`
- their `*.test.tsx` files, only if step 5 of the recipe needs it.

**Steps:**
1. **`issues/$issueId.tsx`** (line 82): `WIDTH` is none, and the label is `"Issue"`.
   1. The toolbar's title is the `IssueTitle` element (line 101), verbatim, instead of an `h2`: `<PageToolbarTitle back={…}><IssueTitle … /></PageToolbarTitle>`.
   2. In the `IssueTitle` function:
      - the view-mode `h2` className becomes `ITEM_TITLE_CLASS`;
      - the pencil `Button` gets `size="icon-xs"` (was `icon-sm`);
      - the edit-mode `Input` className becomes `"h-7 text-ui"` (was `"h-8 text-title"`);
      - the edit-mode wrapper `<div className="flex min-w-0 flex-1 flex-col gap-1">` loses its `<FormError message={error} />` (line 282). The error moves to the body, because the toolbar is one 29 px row. `IssueTitle` takes a new prop `onError: (message: string | undefined) => void`. Every `setError(x)` in `IssueTitle` also calls `onError(x)`, or `IssueTitle` drops its own `error` state and calls only `onError`; pick the second. The page keeps `const [titleError, setTitleError] = useState<string | undefined>()`, passes `onError={setTitleError}`, and renders `<FormError message={titleError} />` as the body's first child. `useState` and `FormError` are already imported.
   3. The old `<div className="flex flex-col gap-3">` wrapper (line 83) is dropped. Its children, except the title row, go into the body after the `FormError`, verbatim: the badges row, the merged banner and `IssueActions`.
   4. There are no toolbar actions.
2. **`datasets/$datasetId.tsx`** (line 177):
   1. Replace the `<header className="flex min-h-9 flex-wrap items-center gap-2 border-b px-3 py-1.5">` and its `</header>` with `<PageToolbar actions={…}>…</PageToolbar>`.
   2. Its left part (the BackLink, the slash and the `h2`) becomes `<PageToolbarTitle back={…}>` with the `h2` className `ITEM_TITLE_CLASS`.
   3. Whatever sat in its `ml-auto` div becomes `actions`.
   4. The four `MetricChip`s between the title and the actions (lines 201–214) are muted meta, so they go in a `<PageToolbarSummary>` after the title, verbatim. If the toolbar is too narrow for them it scrolls sideways, like the other toolbars.
   5. The label is `"Dataset"`: a test finds that nav by name (`$datasetId.test.tsx` line 398).
   6. The body is unchanged: its tabs already carry their own `px-3 pt-3 pb-3`. This page is the exception to the "`px-3 py-3` body" in § What it is.
3. **`annotation-queues/$queueId/index.tsx`** (line 77): the same as step 2.
   - Its `nav aria-label="Annotation queue"` becomes `label="Annotation queue"`.
   - `TopBarCrumb` stays inside `PageToolbarTitle`'s children, after the `h2`.
   - The target label span stays a child after the title.
   - The `ml-auto` div's content (`QueueProgress` and the Start reviewing block) becomes `actions`.
4. **`annotation-queues/$queueId/review/$itemId.tsx`**:
   - The `<header className="flex min-h-9 shrink-0 items-center gap-2 border-b px-3">` becomes `<PageToolbar>`.
   - Its `nav` becomes `<PageToolbarTitle back={…the existing Link…} label="Annotation queue">`, with `<h2 className={ITEM_TITLE_CLASS}>Review</h2>`.

**Done when:** biome check is clean.

**Commit:** `Put the issue, dataset and queue pages on the page toolbar`

---

## P5: spec, decision and changelog

**Edit:**
- `docs/mvp-decisions.md`
- `docs/spec/11-ui.md`
- `CHANGELOG.md`

**Steps:**
1. **`docs/mvp-decisions.md`**, § Track C: after the `| D-C-25 | Page shell | … |` row, add one row in the same format:

   `| D-C-26 | Item page shell | <the D-C-26 summary from this plan's Decisions table, on one line, its first sentence in bold like the other rows> |`

2. **`docs/spec/11-ui.md`**, § Layout and density: after the sentence that starts "The Monitor and Evaluate pages share one shell (D-C-25)", add:

   > Their item pages (new and edit forms, and the evaluator, issue, issue source, dataset and annotation queue pages) use the same toolbar: `‹ List / Name` on the left, muted details, and the header actions on the right. The body is left-aligned with the same inset. Forms are at most 48 rem wide (64 rem for the alert rule form and its preview), with Save and Cancel at the bottom, and detail pages use the full width (D-C-26).

3. **`CHANGELOG.md`**: the first `### Changed` under `## [Unreleased]` (line 127). Add, right after the bullet that ends "select the selected chip again to clear the filter." (line ~173):

   > - Item pages (new and edit forms, and the evaluator, issue, issue source, dataset and annotation queue pages) use the list pages' toolbar and left-aligned layout. The alert rule editor gets a back link.

4. **Site:** grep `website/src/content/docs/docs` for wording about where an item page's back link, title or Delete button sits ("top of the page", "centred", "back link"). If a sentence describes the old layout, update it; otherwise nothing changes. The cold review found none in `evaluation/{evaluators,online-rules,issues}.mdx`.

**Done when:** the edits read back correctly.

**Commit:** `Document the item page shell (D-C-26)`

---

## Final checks (coordinating session)

1. `pnpm exec tsc -b --noEmit` in `web/`, and `pnpm exec biome check web/src` from the root.
2. The tests of every route folder touched, and `src/components/page-toolbar.test.tsx`, run together once. Fix what fails, with a Haiku agent.
3. `just check-docs`.
4. In the browser at 1440 × 900 on the mocks, check each page in "What it is":
   - its toolbar is 29 px and its title sits at the same x as the list's first toolbar control;
   - the forms are left-aligned and no wider than 48 rem (64 rem for alert rules);
   - Save and Cancel are still at the bottom.
