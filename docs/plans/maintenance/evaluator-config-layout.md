# Evaluator Configuration tab: Test panel beside the form

On a wide page, the evaluator's **Test** panel moves from under the form to a column on its right, so the prompt, the model and a test result with its reason show in one window of about 1400×800 CSS px. Where the form is narrow (the New evaluator page, a narrow window, a phone) nothing changes: the panel stays under the form. One phase, one commit.

**Prerequisites:** none (branch from `origin/main`).

**Read first:**
- `web/src/components/evaluators/evaluator-form.tsx`, the `return (` of `EvaluatorForm` (the `<form>`, then `TestPanel` or `TestPanelLocked`).
- `web/src/components/evaluators/test-panel.tsx`, the root `<section>` of `TestPanel` and of `TestPanelLocked`.
- `web/src/routes/$org/$project/evaluators/$evaluatorId.tsx` (the Configuration tab renders `EvaluatorForm` at the page's full width) and `web/src/routes/$org/$project/evaluators/new.tsx` (renders it inside `max-w-3xl`).
- `web/src/routes/$org/$project/evaluators/$evaluatorId.test.tsx`, the test `opens the configuration tab named in the URL` (the pattern the new test follows).
- `web/src/components/user/user-panel.tsx`, its `@container` div: the existing use of Tailwind 4 container queries.
- `docs/spec/11-ui.md`, the bullet that starts `- Evaluators (track I, D-I-20, D-I-21):`; `website/src/content/docs/docs/evaluation/evaluators.mdx`, the **Configuration** bullet and step 1 of `## Test a judge on a trace`.

## What it is, and what it is not

When it is done:
- On the evaluator page's Configuration tab, when the form's container is at least 72rem wide (Tailwind's `@6xl`; the root font size is 13px, so 936 px, or 1008 px in comfortable density), the form and the Test panel are two equal columns; the panel has a left border instead of a top border.
- Below 72rem, the layout is today's: the panel under the form, with a top border.
- The New evaluator page (`max-w-3xl`, 48rem) always shows one column.
- Keyboard order is unchanged: every form control and **Save**, then the Test panel. The DOM order does not change; only the grid places the panel.

Not in scope:
- No field moves (Output, Score name and the mapping stay where they are). Folding them under **Advanced** was the other candidate; it was rejected because the panel would still sit under the form, and **Score name** is required, so folding it would hide a field that blocks a save.
- No sticky panel: a sticky column taller than the window would hide its own bottom until the end of the page.
- No change to the New evaluator page's width.
- No docs screenshot (they wait for the product freeze).

## Decisions this plan relies on

| ID | Summary |
|---|---|
| — | Layout picked by the coordinating session (a layout detail, no `D-` entry): Test panel in a right column from a 72rem container (`@container` + `@6xl:`), two equal columns, not sticky. Checked by screenshot at 1400×800 on the website's mock evaluator. |

## Versions

No new dependency. Tailwind is 4.3.5 (`web/package.json`); container queries (`@container`, `@6xl:`) are built in.

## Phases

One phase, `L1`.

### L1. Put the Test panel beside the form on wide pages

**Goal:** two columns (form | Test) from a 72rem container, one column below.

**Edit:**
- `web/src/components/evaluators/evaluator-form.tsx`
- `web/src/components/evaluators/test-panel.tsx`
- `web/src/routes/$org/$project/evaluators/$evaluatorId.test.tsx`
- `docs/spec/11-ui.md`
- `website/src/content/docs/docs/evaluation/evaluators.mdx`
- `CHANGELOG.md`

**Steps:**

1. In `EvaluatorForm`'s `return`, replace the fragment `<>…</>` by two nested divs. The outer one is the container; the inner one is the grid (an element cannot query its own size):
   ```tsx
   return (
     <div className="@container">
       <div className="grid gap-6 @6xl:grid-cols-2 @6xl:gap-8">
         <form noValidate className="flex min-w-0 flex-col gap-6" onSubmit={onSubmit}>
           {/* unchanged */}
         </form>
         {canEdit ? <TestPanel … /> : <TestPanelLocked />}  {/* unchanged */}
       </div>
     </div>
   );
   ```
   `TestPanel` / `TestPanelLocked` are direct children of the grid div, with no wrapper (step 4's test relies on it). The only change to the `<form>` is `min-w-0` added to its class (a grid item's default `min-width: auto` would let a long prompt line widen its column). Nothing inside the form changes.
2. In `test-panel.tsx`, `TestPanel`'s root `<section>` class becomes exactly:
   `"flex min-w-0 flex-col gap-4 border-t pt-6 @6xl:border-t-0 @6xl:border-l @6xl:pt-0 @6xl:pl-8"`
3. `TestPanelLocked`'s root `<section>` class becomes exactly:
   `"flex min-w-0 flex-col gap-2 border-t pt-6 @6xl:border-t-0 @6xl:border-l @6xl:pt-0 @6xl:pl-8"`
4. In `$evaluatorId.test.tsx`, add one test right after `opens the configuration tab named in the URL`, using the same `renderApp(CONFIG_URL)` setup as that test (the default fixtures make the user a member, so the panel is `TestPanel`):
   - name: `puts the test panel after the form, in the layout grid`;
   - wait for the **Save** button (`await screen.findByRole("button", { name: "Save" })`), then get the Test heading with `screen.getByRole("heading", { name: "Test" })`;
   - assert `save.compareDocumentPosition(heading) & Node.DOCUMENT_POSITION_FOLLOWING` is truthy (the panel comes after the form in tab order);
   - assert `heading.closest("section")` has the class `@6xl:border-l` (`toHaveClass`) and its `parentElement` has the class `@6xl:grid-cols-2`.
   jsdom has no layout, so the test checks the order and the classes, not sizes; the coordinating session checks the pixels with screenshots. If the heading query finds more than one element, stop and report rather than changing the query's meaning.
5. `docs/spec/11-ui.md`, in the bullet that starts `- Evaluators (track I, D-I-20, D-I-21):`, right after the sentence that ends `…usage and a link to the judge trace.`, insert: ` On a page at least 72rem wide the test panel is a column to the right of the form; narrower, it sits under the form.` (one leading space, same line).
6. `website/src/content/docs/docs/evaluation/evaluators.mdx`:
   - the line `- **Configuration** is the form, with the **Test** section at the bottom.` becomes `- **Configuration** is the form, with the **Test** section beside it, or below it in a narrow window.`
   - the line `1. Open the evaluator and go to the **Configuration** tab. The **Test** section is at the bottom.` becomes `1. Open the evaluator and go to the **Configuration** tab. The **Test** section is beside the form, or below it in a narrow window.`
7. `CHANGELOG.md`, under `## [Unreleased]` → `### Changed`, append at the end of that list: `- In a wide window, the evaluator's Configuration tab shows the **Test** panel beside the form instead of under it.`

**Tests:** `puts the test panel after the form, in the layout grid` (step 4). The existing tests of `web/src/components/evaluators/` and `web/src/routes/$org/$project/evaluators/` pass unchanged.

**Done when** (run from the worktree root, with `export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH` first):
- `cd web && pnpm exec vitest run src/components/evaluators 'src/routes/$org/$project/evaluators' --maxWorkers=8` passes.
- `cd web && pnpm exec biome check src/components/evaluators 'src/routes/$org/$project/evaluators'` and `cd web && pnpm exec tsc --noEmit -p .` pass.
- Do not run `just check-web`; the coordinating session runs it once.
- If `web/.vitest/output.json` exists (written when vitest runs through `rtk`), delete it before committing.

**Commit:** `Put the evaluator Test panel beside the form on wide pages`

## Review fix

The first screenshot round showed two columns at a 900 px window: with the 13px root font, `@5xl` (64rem) is 832 px, which left a ~400 px form column. The breakpoint moved to `@6xl` (72rem, 936 px): a 900 px window keeps one column, a 1400 px window with the sidebar open (container about 1166 px) gets two.
