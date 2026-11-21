# Plan: one page shell for the Monitor and Evaluate pages

The Monitor, Evaluate and Explore pages grew apart: three page layouts, two table styles, toolbars from 29 to 43 px high, the search box on either side, three filter-chip implementations and New buttons with and without an icon. This plan gives them one shell: a shared page toolbar (search, filters, summary on the left; actions on the right), one count-chip component, an inset table body, and `ui/table` restyled to match `DataTable`'s headers and row height. It only moves and restyles. No data, API or route changes.

**Prerequisites:** branch `ui-changes` at or after `e5694e8` (origin/main with #200–#203).

**Read first:**
- `docs/spec/11-ui.md` § Layout and density, § Experiments, § Evaluators & online rules, § Issues;
- `web/src/components/ui/table.tsx`;
- `web/src/components/data-table/data-table.tsx` lines 612–720 (the header row classes);
- the page files named in each phase.

## What it is, and what it is not

When this plan is done:
- **One toolbar.** Alerts, Issues, Prompts, Playground, Evaluators, Online rules, Issue sources, Annotation queues, Datasets and Experiments share `PageToolbar`:
  - `min-h-9 border-b px-3 py-1`. The root font is 13 px, so that is about 29 px high (the Traces toolbar's height) with about 10 px of side padding;
  - the search box first (`h-7 w-56`), then the filters and sort, then a muted summary;
  - the primary action on the right, as a small button with a `Plus` icon.
- **One chip style.** The count chips of Alerts, Online rules, Experiments and Evaluators use `FilterChips`: select a chip to filter, select it again to clear. There is no "All" or "Total" chip.
- **Inset tables.** List tables sit inset: the table area has `px-3 py-3` around it (about 10 px).
- **One table look.** `ui/table` headers look like `DataTable`'s: 11 px, uppercase, muted, `h-row`, on `bg-surface`. Its rows get `h-row`, which is a minimum height in a table, so a one-line row is 28 px (32 px in Comfortable). Its cells use `px-2 py-1`.
- **Evaluators** loses the centred 1600 px box. Its template cards stay above the table, inside the inset body.
- **Issue sources** loses its "Issue sources" heading; the top bar already names the page (spec 11: toolbars have no title).

Out of scope:
- Traces, Spans, Sessions, Users and the dashboard (their toolbars already follow spec 11 § Filter row).
- SQL, which keeps its tab bar.
- Item pages (`/alerts/:id`, editors, the dataset page).
- Moving any `ui/table` page to `DataTable` (option C of the review; a later step if wanted).
- The Issues state tabs, which stay a segmented control (D-C-25).
- Docs screenshots: regenerate them with `just docs-screenshots` after merge. That is not part of this plan.

## Decisions this plan relies on

| ID | Summary |
|---|---|
| D-C-25 (new, phase 6) | Monitor/Evaluate list pages share `PageToolbar` (search, filters, summary left; then an optional info hint; actions right), inset tables, `FilterChips` for optional count filters (no All/Total chip; select again to clear), and `ui/table` headers styled like `DataTable`. A required, exclusive choice with a default (Issues' state) stays a segmented `Tabs`. |
| D-C-11 | Named text scale: caption 11 px, body 12 px. |
| D-C-20 | The top bar names the page; toolbars carry no title. |

The user's answers (2026-11-21):
- Option B.
- Tables are **inset**, not edge to edge.
- Headers are **uppercase 11 px**.
- Search goes on the **left, first**.
- Evaluators' template cards **stay above the table**.

## Versions

No new dependency.

## Phases and dependencies

```
P1 shared components ──┬─> P2 Alerts, Online rules, Experiments
                       ├─> P3 Evaluators, Issue sources
                       ├─> P4 Prompts, Annotation queues, Datasets, Issues
                       └─> P5 Playground
P2..P5 ──> P6 spec, decision, changelog, site
```

P2 to P5 touch disjoint files. They run one after another in the `ui-changes` worktree, because they share one index.

**Checks:** following the user's rule, phases don't run builds or test suites. Each phase runs only `pnpm exec biome format --write <its files>` from the repo root, then commits.

**Edit placement:** line numbers are those of `e5694e8`. Where a step says "unchanged" or "the existing …", move that JSX verbatim from the line range given, without changing it. The coordinating session runs the checks once at the end (§ Final checks).

---

## P1: shared toolbar, filter chips and table style

**Goal:** add `PageToolbar`, `PageToolbarSummary` and `FilterChips`, and restyle `ui/table` to match `DataTable`.

**Edit:**
- `web/src/components/page-toolbar.tsx` (new)
- `web/src/components/page-toolbar.test.tsx` (new)
- `web/src/components/filter-chips.tsx` (new)
- `web/src/components/filter-chips.test.tsx` (new)
- `web/src/components/ui/table.tsx`

**Steps:**

1. Create `web/src/components/page-toolbar.tsx` exactly as follows:

```tsx
import type { ReactNode } from "react";

/**
 * A list page's toolbar (D-C-25): the search box, filters and summary on the left, in that order,
 * and the page's actions on the right. It has no title: the top bar names the page.
 */
export function PageToolbar({ children, actions }: { children?: ReactNode; actions?: ReactNode }) {
  return (
    <div
      data-slot="page-toolbar"
      className="flex min-h-9 shrink-0 items-center gap-2 overflow-x-auto border-b px-3 py-1"
    >
      {children}
      {actions === undefined || actions === null ? null : (
        <div className="ml-auto flex shrink-0 items-center gap-2">{actions}</div>
      )}
    </div>
  );
}

/** Muted totals in a page toolbar, after the filters. */
export function PageToolbarSummary({ children }: { children: ReactNode }) {
  return (
    <div className="flex shrink-0 items-center gap-2 text-body tabular-nums text-muted-foreground">
      {children}
    </div>
  );
}
```

2. Create `web/src/components/filter-chips.tsx` exactly as follows:

```tsx
import { cn } from "cn";

export type FilterChip<T extends string> = {
  value: T;
  label: string;
  /** Shown after the label when known. */
  count?: number;
  /** A Tailwind background class for a status dot before the label. */
  dot?: string;
};

/**
 * Optional filters shown as chips with counts (D-C-25). Selecting a chip filters on it; selecting
 * the selected chip clears the filter. No chip stands for "all".
 */
export function FilterChips<T extends string>({
  label,
  chips,
  active,
  onSelect,
}: {
  label: string;
  chips: readonly FilterChip<T>[];
  active: T | undefined;
  onSelect: (value: T | undefined) => void;
}) {
  return (
    <section aria-label={label} className="flex shrink-0 items-center gap-1.5 text-body">
      {chips.map((chip) => {
        const selected = active === chip.value;
        return (
          <button
            key={chip.value}
            type="button"
            aria-pressed={selected}
            onClick={() => onSelect(selected ? undefined : chip.value)}
            className={cn(
              "inline-flex h-6 items-center gap-1.5 rounded-md border px-2 hover:bg-muted",
              selected && "border-foreground/30 bg-muted font-medium",
            )}
          >
            {chip.dot === undefined ? null : (
              <span aria-hidden="true" className={cn("size-1.5 rounded-full", chip.dot)} />
            )}
            <span>{chip.label}</span>
            {chip.count === undefined ? null : (
              <>
                {" "}
                <span className="tabular-nums">{chip.count}</span>
              </>
            )}
          </button>
        );
      })}
    </section>
  );
}
```

3. In `web/src/components/ui/table.tsx`, change only these class strings; keep every other one:
   - `TableHeader`: `"[&_tr]:border-b"` → `"bg-surface [&_tr]:border-b"`.
   - `TableHead`: `"h-10 px-2 text-left align-middle font-medium whitespace-nowrap text-foreground [&:has([role=checkbox])]:pr-0"` → `"h-row px-2 text-left align-middle text-caption font-medium tracking-wide whitespace-nowrap text-muted-foreground uppercase [&:has([role=checkbox])]:pr-0"`.
   - `TableRow`: prepend `h-row ` to its class string, so it starts `"h-row border-b transition-colors …"`.
   - `TableCell`: `"p-2 align-middle whitespace-nowrap [&:has([role=checkbox])]:pr-0"` → `"px-2 py-1 align-middle whitespace-nowrap [&:has([role=checkbox])]:pr-0"`.

4. `pnpm exec biome format --write` on the four new files and `table.tsx`.

**Tests:**
- `page-toolbar.test.tsx`, with `render` from `@testing-library/react`:
  - "renders its children before the actions": `children` = a "Search" input and `actions` = a "New" button. In `container.textContent` order, the input comes first, and the button's parent has the `ml-auto` class.
  - "renders no action group without actions": render with children only. `container.querySelector(".ml-auto")` is null.
  - "PageToolbarSummary shows its children": render `<PageToolbarSummary>3 evaluators</PageToolbarSummary>`; "3 evaluators" is in the document.
- `filter-chips.test.tsx`, with `userEvent`:
  - "shows each chip's label and count": the chips `[{value:"a",label:"Active",count:2},{value:"b",label:"Paused"}]` give buttons named "Active 2" and "Paused".
  - "selecting a chip filters on it": `active` undefined, click "Active 2"; `onSelect` is called with `"a"`.
  - "selecting the active chip clears it": `active="a"`, click "Active 2"; `onSelect` is called with `undefined`. That button has `aria-pressed="true"` and the other has `"false"`.
  - "renders a dot only when given": a chip with `dot: "bg-status-ok"` has one `span.rounded-full`; a chip without `dot` has none.
  - "labels its group": `getByRole("region", { name: "Status" })` exists for `label="Status"`.

**Done when:** biome format has run on the files, and the coordinating session's final checks pass.

**Commit:** `Add the page toolbar and filter chips, and match ui/table to DataTable`

---

## P2: Alerts, Online rules and Experiments

**Goal:** put the three pages' toolbars on `PageToolbar` and their strips on `FilterChips`, and inset their bodies.

**Edit:**
- `web/src/routes/$org/$project/alerts/index.tsx`
- `web/src/routes/$org/$project/online-rules/index.tsx`
- `web/src/routes/$org/$project/experiments/index.tsx`
- `web/src/routes/$org/$project/experiments/index.test.tsx`

**Steps:**

1. **Alerts** (`alerts/index.tsx`):
   1. In the `newRule` constant (around line 157), give both `Button`s `size="sm"`. Keep the `Plus` icon.
   2. Replace the toolbar `<div className="flex min-h-9 shrink-0 items-center gap-2 overflow-x-auto border-b px-3">…</div>` (around line 246) with `<PageToolbar actions={empty ? null : newRule}>`. Its children, in this order:
      - the `StateStrip` replacement (step 4), only when `!(query.isPending || empty)`, as today;
      - then the existing Info `Tooltip` block (lines 247–262), unchanged. It moves from first to last, as on Online rules.
   3. Remove the `<div className="ml-auto …">` that held `newRule`.
   4. Delete the `StateStrip` function. In its place, render:

      ```tsx
      <FilterChips
        label="Summary"
        chips={ALERT_BUCKETS.map((bucket) => ({ value: bucket, label: BUCKET_LABEL[bucket], count: counts[bucket] }))}
        active={search.state}
        onSelect={…the existing navigate call…}
      />
      ```

      The `onSelect` body is the same `navigate` the `StateStrip` usage has today.
   5. In the body, change `<div className="flex flex-col gap-6 px-3 py-4">` to `<div className="flex flex-col gap-6 px-3 py-3">`.
   6. Import `PageToolbar` from `@/components/page-toolbar` and `FilterChips` from `@/components/filter-chips`. Remove imports left unused (for example `cn` if nothing else uses it; check first).
2. **Online rules** (`online-rules/index.tsx`):
   1. In `newRule` (around line 179), give both `Button`s `size="sm"`.
   2. Replace the toolbar div (around line 302) with `<PageToolbar actions={empty ? null : newRule}>`. Its children, in this order, each only when `!(query.isPending || empty)` as today:
      1. the "Search rules" `Input`, moved out of the right-hand group, unchanged;
      2. the `FilterChips` replacing `StatusStrip`;
      3. `<PageToolbarSummary>` holding the spend, jobs and failed spans that are in the current summary div (lines 338–350), without the `Separator`;
      4. the Info `Tooltip` block (lines 303–322), unchanged. Info hints go after the summary on every page (D-C-25).
   3. Delete the old right-hand `ml-auto` div.
   4. Delete the `StatusStrip` function. The replacement:

      ```tsx
      <FilterChips
        label="Status"
        chips={RULE_BUCKETS.map((bucket) => ({ value: bucket, label: BUCKET_LABEL[bucket], count: counts[bucket], dot: BUCKET_DOT[bucket] }))}
        active={search.status}
        onSelect={…the existing navigate call…}
      />
      ```

   5. In the body, change `px-3 py-4` to `px-3 py-3`.
   6. Fix the imports as in 1.6, and remove the `Separator` import: line 339 was its only use.
3. **Experiments** (`experiments/index.tsx`):
   1. Replace the toolbar div (around line 217) with `<PageToolbar actions={<CompareSelection experiments={selectedRows} />}>`. Its children, in this order:
      1. `<FilterChips label="Status" chips={STATUS_COUNTS.map((entry) => ({ value: entry.status, label: entry.label, count: rows.filter((row) => row.status === entry.status).length }))} active={status} onSelect={setStatus} />`;
      2. the dataset `Select`, with its trigger's className changed to `"h-7 w-fit min-w-44 text-body"`.
   2. Delete the `Total` button, the `Button` group div that held the chips, and the `ml-auto` div.
   3. The body keeps `px-3 py-3`. No change there.
   4. Change the doc comment of `setStatus` (line 196) to `/** Sets \`?status=\`; the active status is cleared by picking it again. */`.
   5. Fix the imports.

**Tests:**
- No test in `experiments/index.test.tsx` uses the "Total" chip (checked by the cold review), so nothing changes there. Add one test to it: "selecting the selected status chip again clears the filter". Click "Completed <n>", then click it again; the URL has no `status`, and the running row is back in the table.
- Existing alerts and online-rules tests find the chips by their button names ("Firing 1", "Active 1"…), and those names don't change.
- No new test file. Chip behaviour is covered in P1, and the page tests cover the wiring.

**Done when:** biome format has run on the edited files. Final checks pass.

**Commit:** `Put Alerts, Online rules and Experiments on the page toolbar`

---

## P3: Evaluators and Issue sources

**Goal:** give both pages the toolbar and an inset body; drop the Evaluators centred box and the Issue sources heading.

**Edit:**
- `web/src/routes/$org/$project/evaluators/index.tsx`
- `web/src/routes/$org/$project/evaluators/index.test.tsx`
- `web/src/routes/$org/$project/issue-sources/index.tsx`

**Steps:**

1. **Evaluators:**
   1. Replace `KIND_FILTERS` (line 45) with chips that have no "All" entry:

      ```ts
      const KIND_FILTERS: { value: KindFilter; label: string }[] = [
        { value: "llm_judge", label: "LLM judge" },
        { value: "check", label: "Check" },
        { value: "webhook", label: "Webhook" },
      ];
      ```

   2. Build the action once:

      ```tsx
      const newEvaluator = (
        <DisabledReason reason={canManage ? undefined : "Only members can create evaluators"}>
          {canManage ? (
            <Button asChild size="sm">
              <Link to="/$org/$project/evaluators/new" params={{ org, project }} state={backState}>
                <Plus />
                New evaluator
              </Link>
            </Button>
          ) : (
            <Button disabled size="sm">
              <Plus />
              New evaluator
            </Button>
          )}
        </DisabledReason>
      );
      ```

      Put it just before the `return`. Import `Plus` from `lucide-react`.
   3. The page's return becomes:

      ```tsx
      <div className="flex h-full min-h-0 w-full flex-col">
        <PageToolbar actions={newEvaluator}>
          {query.isPending || rows.length === 0 ? null : (
            <>
              <Input type="search" aria-label="Search evaluators" placeholder="Search evaluators"
                className="h-7 w-56 text-body" value={q} maxLength={QUERY_MAX}
                onChange={(event) => updateSearch({ q: event.target.value || undefined })} />
              <FilterChips label="Kind"
                chips={KIND_FILTERS.map((filter) => ({ ...filter, count: rows.filter((row) => row.kind === filter.value).length }))}
                active={kind} onSelect={(value) => updateSearch({ kind: value })} />
              <PageToolbarSummary>
                {rows.length === 1 ? "1 evaluator" : `${rows.length} evaluators`}
              </PageToolbarSummary>
            </>
          )}
        </PageToolbar>
        <div className="min-h-0 flex-1 overflow-auto">
          <div className="flex flex-col gap-4 px-3 py-3">
            {canManage ? <TemplateGallery org={org} project={project} /> : null}
            {/* lines 205–353 of e5694e8, verbatim: the pending skeletons, the EmptyState, and the
                fragment holding the no-match box (247–256) or the Table (257–353), but WITHOUT the
                search-and-kind row (lines 219–245), which moved to the toolbar */}
          </div>
        </div>
      </div>
      ```

   4. Delete the old outer div (line 183), the old header row (lines 184–203) and the search-and-kind row (lines 219–245). The fragment that held that row and the table keeps only `visible.length === 0 ? … : <Table …>`, so it can become that expression without the fragment. `updateSearch({ kind })` takes `KindFilter | undefined`, which fits.
   5. Fix the imports: add `PageToolbar`, `PageToolbarSummary`, `FilterChips` and `Plus`.
2. **Issue sources:**
   1. The page's return becomes:

      ```tsx
      <div className="flex h-full min-h-0 w-full flex-col">
        <PageToolbar actions={newSource} />
        <div className="min-h-0 flex-1 overflow-auto px-3 py-3">
          {/* the `sources.isPending ? <Skeleton …/> : <SourceList …/>` expression, verbatim */}
        </div>
      </div>
      ```

   2. `newSource` is a constant built before the `return`: the existing `<DisabledReason reason={canManage ? undefined : NO_PERMISSION}>…</DisabledReason>` block, verbatim, except that both `Button`s get `size="sm"` (they keep `<Plus />`).
   3. Delete the `<h2>`, its wrapper div and the old outer div.

**Tests (`evaluators/index.test.tsx`):**
- No test uses an "All" button, a "Kind" group, or the "Issue sources" heading (checked by the cold review). Don't search for them.
- Kind chip names now carry a count: "Webhook" becomes "Webhook 1" in the mocks. Change `getByRole("button", { name: "Webhook" })` (around lines 147 and 151) to `{ name: /^Webhook/ }`.
- Add the test "the kind chips show each kind's count": with the default mocks, the button names are "LLM judge 1", "Check 1" and "Webhook 1". Check the mock counts first and use the real ones.
- Keep the test that the only level-1 heading is "Evaluators" (the top bar's).
- Add a test: "selecting the selected kind chip again clears the filter". Click "Webhook 1", then click it again; every evaluator row is back.

**Done when:** biome format has run. Final checks pass.

**Commit:** `Put Evaluators and Issue sources on the page toolbar`

---

## P4: Prompts, Annotation queues, Datasets and Issues

**Goal:** the same toolbar for the four `DataTable` pages, with their tables inset.

**Edit:**
- `web/src/routes/$org/$project/prompts/index.tsx`
- `web/src/routes/$org/$project/annotation-queues/index.tsx`
- `web/src/routes/$org/$project/datasets/index.tsx`
- `web/src/routes/$org/$project/issues/index.tsx`

**Steps:**

1. **Prompts:**
   1. Replace the toolbar div (line 161) with `<PageToolbar actions={…}>`:
      - **Children:** the search `Input`, its className changed from `"h-7 max-w-xs text-body"` to `"h-7 w-56 text-body"`, then the label and tag `Select`s, unchanged.
      - **Actions:** the two existing `DisabledReason` blocks ("Protected labels", then "New prompt"). "New prompt" gets a `<Plus />` before its text; both keep `size="sm"`.
   2. Change the wrapper `<div className="flex min-h-0 flex-1 flex-col">` around `DataTable` to `<div className="flex min-h-0 flex-1 flex-col px-3 py-3">`.
   3. Change the skeleton wrapper `p-3` to `px-3 py-3`. That is the same padding; it only makes the classes match.
2. **Annotation queues:**
   1. Toolbar (line 137) → `PageToolbar`:
      - **Children:** the search `Input`, its className changed from `"h-7 max-w-xs text-body"` to `"h-7 w-56 text-body"`.
      - **Actions:** the "New queue" `DisabledReason` block, with `<Plus />` added.
   2. Change the `DataTable`'s wrapper `<div className="min-h-0 flex-1">` (line 179) to `<div className="flex min-h-0 flex-1 flex-col px-3 py-3">`.
3. **Datasets:**
   1. Toolbar (line 165) → `PageToolbar`:
      - **Children:** the search `Input`, its className changed from `"h-7 max-w-xs text-body"` to `"h-7 w-56 text-body"`.
      - **Actions:** the "New dataset" block, with `<Plus />` added.
   2. `DatasetsSummary` stays where it is, a strip under the toolbar, as on Sessions and Users.
   3. Wrap the `DatasetTrendsContext` element in `<div className="flex min-h-0 flex-1 flex-col px-3 py-3">`, unless it already has a flex wrapper, in which case add the padding to that wrapper.
4. **Issues:**
   1. Replace the toolbar div (line 115) with `<PageToolbar>` (no actions). Its children, in this order:
      1. the search `Input`, moved from the right-hand group, className `"h-7 w-56 text-body"`;
      2. the state `Tabs`, with `<TabsList className="h-7">`;
      3. the source `Select`;
      4. the kind `Select`;
      5. the sort `Select`.
   2. Delete the `ml-auto` div.
   3. The body keeps `px-3 py-3`.
5. Add the imports. `Plus` comes from `lucide-react`.

**Tests:** none change. Search for `ml-auto` or toolbar-position assertions in these pages' tests; if one fails at the final check, it is fixed there.

**Done when:** biome format has run. Final checks pass.

**Commit:** `Put Prompts, Annotation queues, Datasets and Issues on the page toolbar`

---

## P5: Playground toolbar

**Goal:** the Playground's action row becomes a `PageToolbar` like the other pages, and its body keeps the padding.

**Edit:** `web/src/routes/$org/$project/playground/index.tsx`

**Steps:**

1. The outer div (line 838) keeps `onKeyDown` and its biome-ignore comment. Its className goes from `"flex h-full min-h-0 flex-col gap-3 p-3"` to `"flex h-full min-h-0 flex-col"`.
2. Replace the first child `<div className="flex flex-wrap items-center gap-2">…</div>` with `<PageToolbar actions={…}>`:
   - **Children:** the source `p` (unchanged condition).
   - **Actions:** the buttons from the old `ml-auto` div, in the same order (Recent runs, Add model, Save as new version…, then Stop all or Run all).
   - Each of those `Button`s gets `size="sm"`.
3. Wrap every element after the toolbar in `<div className="flex min-h-0 flex-1 flex-col gap-3 p-3">`:
   - the `FormError`s;
   - the `cut` notice;
   - the `opening` status;
   - the `ResizablePanelGroup`.

**Tests:** none change.

**Done when:** biome format has run. Final checks pass.

**Commit:** `Put the Playground actions in the page toolbar`

---

## P6: spec, decision, changelog and site

**Goal:** record D-C-25 and describe the new toolbars where the docs describe the old ones.

**Edit:**
- `docs/mvp-decisions.md`
- `docs/spec/11-ui.md`
- `CHANGELOG.md`
- `website/src/content/docs/docs/evaluation/experiments.mdx`
- `website/src/content/docs/docs/evaluation/online-rules.mdx`
- `website/src/content/docs/docs/evaluation/evaluators.mdx`

**Steps:**

1. **`docs/mvp-decisions.md`**, § Track C: its decisions are table rows. After the `| D-C-24 | Facet batch | … |` row (line 342), add one row in the same format:

   `| D-C-25 | Page shell | <the text below, on one line> |`

   > **One page shell for the Monitor and Evaluate pages.** Alerts, Issues, Prompts, Playground, Evaluators, Online rules, Issue sources, Annotation queues, Datasets and Experiments share a page toolbar (the Traces toolbar's height, the search box first, then filters and sort, then a muted summary and an optional info hint; actions on the right, the primary one a small button with a plus icon). Their tables are inset (about 10 px around), and `ui/table` headers match `DataTable`'s (11 px uppercase, muted, 28 px). Optional filters with counts are chips: selecting one filters, selecting it again clears it, and no chip stands for "all", so Experiments loses its Total chip and Evaluators its All button. A required, exclusive choice with a default stays a segmented control (Issues' state tabs). Rejected: edge-to-edge tables like Traces (the user chose inset), and moving every list to `DataTable` (rich cells, little visible gain).

2. **`docs/spec/11-ui.md`:**
   1. In § Layout and density, after the sentence "Table rows are a fixed 28 px in Compact and a single line; …", add:

      > The Monitor and Evaluate pages share one shell (D-C-25): a toolbar with the search box first, then the filters and sort, then muted totals, and the actions on the right; then the content, inset by about 10 px. Their status and kind filters are chips with counts; selecting the selected chip clears it. They have a single toolbar row; the second filter row of § Filter row belongs to Traces, Spans, Sessions and Users.

   2. In § Experiments, change "a status strip (total, running, completed, failed; `status=`)" to "status chips (running, completed, failed, each with its count; `status=`)".
3. **`CHANGELOG.md`** § `[Unreleased]` → `### Changed` (create that heading after `### Added`, or after `### Upgrade notes` when there is no `### Added`, if it is missing), add:

   > - The Alerts, Issues, Prompts, Playground, Evaluators, Online rules, Issue sources, Annotation queues, Datasets and Experiments pages share one toolbar layout and table style. Experiments has no Total chip and Evaluators no All button: select the selected chip again to clear the filter.

4. **`website/.../experiments.mdx`** line 121: change "a status strip (**Total**, **Running**, **Completed**, **Failed**, each with a count)" to "status chips (**Running**, **Completed**, **Failed**, each with a count; select a chip again to show every run)".
5. **`website/.../online-rules.mdx`** line 28: change "A search box filters by name." to "A search box before them filters by name."
6. **`website/.../evaluators.mdx`** line 13: change "a **Kind** filter (**All**, **LLM judge**, **Check**, **Webhook**)" to "**Kind** chips (**LLM judge**, **Check**, **Webhook**, each with a count; select a chip again to show every kind)".

**Done when:** the edits are made.

**Commit:** `Document the shared page shell (D-C-25)`

---

## Final checks (coordinating session)

From `web/`, with Node on PATH:
1. `pnpm exec tsc -b --noEmit`
2. From the repo root: `pnpm exec biome check web/src`
3. `pnpm exec vitest run src/components/page-toolbar.test.tsx src/components/filter-chips.test.tsx src/routes/\$org/\$project/{alerts,online-rules,experiments,evaluators,issue-sources,prompts,annotation-queues,datasets,issues,playground} --maxWorkers=8`, then `rm -rf .vitest`.
4. Every `ui/table` user changed look. Also run the tests of every file that imports it: `grep -rl components/ui/table web/src --include=*.tsx`, the matching `*.test.tsx` files and their directories' tests. Update a snapshot only after looking at the diff.
5. `just check-docs` (site text changed).
6. By hand at 1440 × 900 on the mocks (`localhost:5173`):
   - every page in the sidebar's Monitor and Evaluate groups has a toolbar as high as Traces' (about 29 px), the search box first and New on the right;
   - its table is inset;
   - Evaluators' table header matches Datasets' header.
