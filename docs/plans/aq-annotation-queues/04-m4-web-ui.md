# AQ M4 — Web UI for annotation queues

**Summary.** This milestone builds the annotation-queue pages and entry points in `web/`:
- the nav entry and its go key;
- the queue list with a create dialog;
- the queue page with Items and Settings tabs;
- the reviewer page, with the existing trace or session view, the instructions, the annotation form limited to the queue's definitions and saving with `queue_id`, the 5-minute lease and its heartbeat, Complete and next, Next, Skip and Previous, the shortcuts and the end screen;
- "Add to queue" on the trace and session pages and on the checked rows of the traces, spans and sessions lists, and "Add matching to queue…";
- the "In n queues" chip;
- the plan-limit state;
- the plan page row, the dev-server mocks, the user docs page and its screenshots, spec 11 and the changelog.

**Prerequisites.**
- M2 and M3 are committed: `crates/server/openapi.json` and `web/src/api/schema.gen.ts` hold the annotation-queue operations and schemas, and `fixtures/api/manifest.json` serves the GETs.
- Work on branch `claude/aq-impl` in `/home/sebastien/travaux/llm-observer/.claude/worktrees/aq-annotation-queues-f38be7`.
- Before writing code, open `web/src/api/schema.gen.ts` and check the schema names this plan uses (`AnnotationQueue`, `AnnotationQueueItem`, `CreateAnnotationQueue`, `UpdateAnnotationQueue`, `QueueTarget`, `QueueItemStatus`, `QueueCounts`, `AddQueueItems`, `AddedQueueItems`, `NextQueueItem`, `SkipQueueItem`, `QueueMatchingRequest`, `QueueMatchingPreview`, `QueueMatchingAdded`, `OrgLimits.max_annotation_queues`, `ScoreCreate.queue_id`). If one differs, stop and report it.

**Read first.**
- `00-design.md` § Reviewer flow and the lease, and its UI list.
- `02-m2-queues-server.md` Q4a, Q4b and Q5 (routes, codes, bodies); `03-m3-add-by-filter.md` R2.
- `docs/spec/11-ui.md`; `website/STYLE.md`.
- The mirrors, named per phase. The code facts gathered for this plan (outside the repo) are in `/home/sebastien/.config/Claude-extra/side-session-notes/local_1c672712-2cc7-40e9-8d6b-d582417356b7/aq-m4-code-facts.md`; read its § 0 (pitfalls) before any phase.

## What it is, and what it is not

**What works when it's done:** everything in the summary, on the real API and on the MSW mocks.

**Out of scope:**
- a results tab;
- per-reviewer assignment;
- the SDK (M5);
- a pre-emptive "locked" New queue button. The org-wide queue count is not on the project's list, so the limit shows when the server answers `403 plan_limit` (D-P-15's Upgrade button on that error).

## Decisions this plan relies on

| ID | Summary |
|---|---|
| D-AQ-2 | One kind per queue |
| D-AQ-4 | 5-minute lease, renewed every 60 s; save, then complete; pages refetch every 10 s while visible and on focus |
| D-AQ-5 | Complete, skip with reason, reopen, Next without changing state |
| D-AQ-7 | The queue's live definitions, in order, with D-I-37's controls and own-scores rule; Markdown instructions |
| D-AQ-8 | Adds by id (1–1 000) and by filter (≤ 10 000) |
| D-AQ-10 | Free: 1 queue; the `403 plan_limit` answer shows the Upgrade button |
| D-AQ-11 | Members manage and review; viewers read |
| D-I-37 | The annotation form |
| D-P-15 | Upgrade button |

Plan-level choices:
- **`AnnotationForm` props.** The form gains `definitionIds?: readonly string[]` (restrict to these live definitions, in this order), `saveRef?: { current: AnnotationSaveHandle | null }` (an imperative `save()` the reviewer calls before completing), `hideSave` and `onReady`. `AnnotationContext` gains `queueId?: string`, which `scoreItem` sends as `queue_id`.
- **The reviewer and the form.** The form is hidden, not made read-only, when another reviewer holds the item or the lease is lost.
- **Moving between items.** Every move goes through `/review/next?after=<id>`, which claims once per location and shows the end screen in place.
- **`SessionConversation`.** The turn list and the trace view of `SessionView` move into a new exported component (props in W1b). `SessionView` uses it with its URL search, and the reviewer uses it with local state.
- **The definitions picker.** It is a checkbox list of live definitions, with up and down buttons for the order. No new component library.
- **The "In n queues" chip.** It renders only when n > 0.
- **Dev-server mocks.** The write operations get stateless handlers in `mocks/writes.ts`.

## Versions

No new dependency. `lucide-react` icons come from the installed version. `radix-ui` and `react-markdown` are already installed.

## Phases and parallel runs

```
W1a (queries, roles, nav, mocks, plan row) ─► W1b (form handle, session conversation, shared queue components)
   ─┬─► W2 (queue list and queue page)            ─┐
    ├─► W3 (reviewer page)                         ├─► W4b (list pages) ─► W5 (docs, screenshots, spec, changelog)
    └─► W4a (dialogs, chip, trace and session)    ─┘
```

**Prerequisites of W1a.** All of M2 and M3's R2 are committed. W1a mocks M3's `preview_annotation_queue_matching` and `add_annotation_queue_matching`, and `writes.test.ts` derives its list from `openapi.json`.

**Running W2, W3 and W4a at the same time.** They run in the same worktree. Their Edit lists do not overlap: only W1a and W1b edit the shared files. Because they share the tree, each of them:
- runs `pnpm --dir web run gen:routes` once at the start, and again after creating a route file. The generated `routeTree.gen.ts` is git-ignored and shared, so a race on it is not an error: run `gen:routes` again.
- iterates with targeted tests: `pnpm --dir web exec vitest run <its paths> --maxWorkers=8`;
- runs `pnpm --dir web exec biome check <its files>`;
- runs `pnpm --dir web exec tsc -b`. Two kinds of type error are expected and reported, not fixed: an error in another phase's file, and an error in its own file that names a route another phase creates (W2 links to W3's review route; W3 links back to W2's queue page);
- commits only its own files (`git add <files>`, then `git commit`). Never `git add -A`, never `git stash`;
- does **not** run `just check-web`. The coordinating session runs it after the three commits and fixes what they show together.

W1a, W1b, W4b and W5 run alone and finish with `just check-web` (W5 with `just check-docs` too).

**Rules for every phase.**
- Node: `export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH` first.
- The type scale (`text-caption|body|ui|title|heading|display` only).
- Unhandled requests fail tests: mock every write with `server.use(...)`.
- Never import fixtures or mocks from app code (`check-dist`).

---

### W1a — Queries, roles, nav, mocks and the plan row

**Goal.** The query helpers, the role helpers, the nav entry and go key, the shortcut rows, the dev-server write mocks and the plan page row. No new page.

**Edit:**
- `web/src/api/queries-annotation-queues.ts` (new)
- `web/src/lib/roles.ts`, `web/src/lib/roles.test.ts`
- `web/src/components/shell/nav.ts`, `web/src/lib/shortcuts.ts`, `web/src/lib/shortcuts.test.ts`
- `web/src/mocks/writes.ts`, `web/src/mocks/writes.test.ts`
- `web/src/routes/$org/settings/plan.tsx`, `web/src/routes/$org/settings/plan.test.tsx`
- `web/src/components/command/palette.test.tsx` (only: tolerate two shortcut rows with the same key text, "⌘↵")

**Steps.**
1. **Queries** (`api/queries-annotation-queues.ts`). Mirror `api/queries-datasets.ts`; every path parameter `project` is `projectRef(org, project)`. Export:
   - `annotationQueuesInit(org, project)`: `{ params: { path: { project }, query: { limit: 100 } } }`.
   - `annotationQueuesKey(org, project)`: the prefix key without query, for invalidation.
   - `annotationQueueQuery(org, project, id)` and `annotationQueueKey(org, project, id)`.
   - `annotationQueueItemsInit(org, project, id, status?)`: `query: { limit: 100, ...(status ? { status } : {}) }`.
   - `annotationQueueItemsKey(org, project, id)`: the prefix.
   - `annotationQueueItemQuery(org, project, id, itemId)`: `GET …/annotation-queues/{id}/items/{item_id}`.
   - `queuesHoldingQuery(org, project, target: { traceId: string } | { sessionId: string })`: a plain `queryOptions` for `GET …/annotation-queues` with `query: { limit: 100, target_trace_id }` or `target_session_id`. Never call it with the infinite list's exact init.
   - `annotationQueueWriteKeys(org, project, id?)`: the keys to invalidate after any write (list prefix, the queue, its items prefix and its item queries).
2. **Roles** (`lib/roles.ts`): add `canManageQueues(role)` and `canReview(role)`, both `atLeast(role, "member")`, with doc comments naming `ProjectPerm::ManageQueues` and `ProjectPerm::Review` (D-AQ-11). Add tests next to the others.
3. **Nav.**
   - In `components/shell/nav.ts`, add `{ section: "Evaluate", to: "/$org/$project/annotation-queues", label: "Annotation queues", icon: ClipboardCheck }` between Online rules and Datasets (`ClipboardCheck` from `lucide-react`).
   - In `lib/shortcuts.ts` `GO_TARGETS`, add `r: { id: "go-annotation-queues", to: "/$org/$project/annotation-queues" }`.
   - Add two display-only `SHORTCUTS` rows, shaped like the SQL console's `⌘↵` row: `{ keys: "⌘↵", description: "Complete and go to the next item", scope: "Annotation queue", bindings: [] }` and `{ keys: "⌘⇧↵", description: "Next item without completing", scope: "Annotation queue", bindings: [] }`.
   - In `lib/shortcuts.test.ts`, update the exact key list and add `expect(reduceShortcut("g", { ...plain, key: "r" }, false).action).toBe("go-annotation-queues")`.
4. **Dev-server writes** (`mocks/writes.ts`). Add `WRITE_OPERATIONS` rows and stateless handlers for these operations: `create_annotation_queue`, `update_annotation_queue`, `delete_annotation_queue`, `add_annotation_queue_items`, `remove_annotation_queue_item`, `next_annotation_queue_item`, `lease_annotation_queue_item`, `release_annotation_queue_item`, `complete_annotation_queue_item`, `skip_annotation_queue_item`, `reopen_annotation_queue_item`, `preview_annotation_queue_matching` and `add_annotation_queue_matching`.
   - `create_annotation_queue`: answers `fixtures/api/static/annotation-queue.json` with the body's name, `201`.
   - `update_annotation_queue`: the fixture with `version + 1` and the body's fields.
   - `delete_annotation_queue`, `remove_annotation_queue_item` and `release_annotation_queue_item`: `204`.
   - `add_annotation_queue_items`: `{added: n, existing: 0, skipped: []}` with `201`.
   - `next_annotation_queue_item` and `lease_annotation_queue_item`: `static/annotation-queue-item.json`. The stateless `next` always answers the same item, so "Complete and next" in `just web-dev` comes back to it. This is intended; the screenshots need only the first item.
   - `complete_annotation_queue_item`, `skip_annotation_queue_item` and `reopen_annotation_queue_item`: that item with the matching status.
   - `preview_annotation_queue_matching`: `{matched: 42, capped: false}`.
   - `add_annotation_queue_matching`: `{added: 42, existing: 0, blocked: 0, capped: false}` with `201`.

   Use the file's existing helpers (`named`, `noContent`, …). In `mocks/writes.test.ts`, add `path.startsWith("/api/v1/projects/{project}/annotation-queues")` to the covered prefixes.
5. **Plan page** (`routes/$org/settings/plan.tsx`): add `["Annotation queues", formatLimit(limits.max_annotation_queues)]` after the active online rules row, and assert it in `plan.test.tsx`.

**Tests:** listed in the steps, plus every existing test passing.

**Done when.**
- `pnpm --dir web run gen:routes`
- `pnpm --dir web exec vitest run src/lib src/mocks src/components/shell "src/routes/\$org/settings/plan.test.tsx" --maxWorkers=8`
- `export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH && just check-web`

**Commit:** `Add annotation queue queries, nav and mocks`

---

### W1b — The form handle, the session conversation and the shared queue components

**Goal.**
- `AnnotationForm` can be limited to a queue's definitions, save with `queue_id`, and be saved from outside.
- The session conversation can be embedded.
- The queue dialog, the definitions picker, the progress bar and the queue helpers exist for W2, W3 and W4a.

**Edit:**
- `web/src/lib/annotation.ts`, `web/src/lib/annotation.test.ts`
- `web/src/components/annotation/annotation-form.tsx`, `web/src/components/annotation/annotation-form.test.tsx` (new)
- `web/src/test/scores.ts` (new: `serveScores` and `scoreFor` moved here) and `web/src/components/trace/annotate-tab.test.tsx` (imports only)
- `web/src/components/session/session-view.tsx` and `web/src/components/session/session-conversation.tsx` (new)
- `web/src/lib/annotation-queues.ts` and `web/src/lib/annotation-queues.test.ts` (new)
- `web/src/components/annotation-queues/queue-progress.tsx`, `definitions-picker.tsx`, `queue-dialog.tsx`, and their tests (new)

**Steps.**
1. **`AnnotationContext`** (`lib/annotation.ts`):
   - add `queueId?: string`. `scoreItem` sets `item.queue_id = context.queueId` when it is set, and nothing otherwise;
   - export `type AnnotationSaveHandle = { save: () => Promise<boolean>; isDirty: () => boolean }`. `save` resolves `true` when there was nothing to save or the save succeeded, and `false` when it failed (the form then shows its errors);
   - tests in `lib/annotation.test.ts`: `scoreItem` with `queueId` sets `queue_id`; without it the key is absent.
2. **`AnnotationForm`** (`components/annotation/annotation-form.tsx`). New optional props on `AnnotationForm`, all threaded down to `AnnotationRows` (a separate component in the same file):
   - **`definitionIds?: readonly string[]`.** The rows are the live definitions whose id is in the list, in the list's order (no sort by name). Ids that are absent are dropped silently. When none remain, render `<p className="text-caption text-muted-foreground">This queue's score definitions are all archived.</p>` instead of the rows. Without `definitionIds`, behaviour is unchanged.
   - **`saveRef?: { current: AnnotationSaveHandle | null }`.**
     - In `AnnotationRows`, extract the body of today's `<form onSubmit>` handler into `async function submit(): Promise<boolean>`: same body, same error mapping, same invalidation, resolving as in step 1. With no changed row it resolves `true` without a request; an invalid row resolves `false` without a request.
     - `onSubmit` calls `submit()` after `event.preventDefault()`.
     - `useImperativeHandle(saveRef, () => ({ save: submit, isDirty: () => changed.length > 0 }))`. React 19 accepts a plain object ref.
     - When `AnnotationRows` is not rendered (loading, error, viewer note, no definitions, all archived), `saveRef.current` stays `null`.
   - **`hideSave?: boolean`.** Hides the Save button; the reviewer uses its own buttons.
   - **`onReady?: () => void`.** Called once, when the form has finished loading, whatever it then shows (rows, viewer note, no definitions, archived notice).
   - Existing callers pass none of these and are unchanged.
3. **Form tests** in the new `annotation-form.test.tsx`. Move `serveScores` and `scoreFor` from `annotate-tab.test.tsx` to `web/src/test/scores.ts` and update that file's imports. Use `serveAnnotationDefinitions` (`test/score-definitions.ts`; ids `…0601` helpfulness numeric, `…0602` resolved boolean, `…0603` tone categorical, `…0604` note text):
   - `definitionIds restricts and orders the controls`: ids `[…0603, …0601]` show tone, then helpfulness, and nothing else;
   - `archived queue definitions show the notice`;
   - `save through the handle sends queue_id`: capture the `POST /scores` body; it has `queue_id` and no `score_id`;
   - `save with nothing changed sends nothing and resolves true`;
   - `save that fails resolves false and shows the error`;
   - `onReady fires once after loading`.
4. **`SessionConversation`.** Move the turn list and the stacked `TraceView` out of `SessionView` (`session-view.tsx`, around lines 96–155) into `components/session/session-conversation.tsx`:
   ```tsx
   export function SessionConversation(props: {
     session: <the type of the loaded sessionQuery data that SessionView uses>;
     trace: string | undefined;                 // the selected turn's trace id
     onSelectTrace: (traceId: string) => void;
     scrollTarget: number | null;               // the turn index to scroll to, or null
     onScrolled: () => void;                    // called once the scroll is done
     onCloseTrace?: () => void;                 // passed to TraceView's onClose; omitted by the reviewer
   })
   ```
   - The selected span state (`setSpan`) moves into `SessionConversation`.
   - The fetch of `sessionQuery` (with `includeEval` and `includeExperiments` from the URL search), `scrollTarget`, `failedIndex` and `showFirstError` stay in `SessionView`, because `SessionHeader`'s `onShowErrors` uses them. `SessionView` passes them down.
   - If the code's state differs from this description, keep every piece of state the header uses in `SessionView`, pass what the conversation needs as props, and report the shape you chose.
   - The existing session tests must pass unchanged.
5. **`lib/annotation-queues.ts`:**
   - `parseQueuesSearch` (`{ q? }`);
   - `parseQueueSearch` (`{ tab?: "items" | "settings"; status?: "pending" | "completed" | "skipped" }`). Mirror `parseDatasetSearch`: only set keys are kept;
   - `parseReviewSearch` (`{ after?: string }`);
   - `targetLabel(target)`: "Traces", "Spans" or "Sessions";
   - `progressOf(counts)`: `{ done: completed + skipped, total: done + pending }`;
   - `QUEUE_POLL_MS = 10_000`, `LEASE_RENEW_MS = 60_000`;
   - `QUEUE_NAME_TAKEN = "an annotation queue with this name exists"` (the M2 conflict detail).

   Test each function.
6. **`QueueProgress({ counts })`.** Copy `components/online-rules/spend-bar.tsx`'s markup: `role="progressbar"`, `aria-valuenow` = done, `aria-valuemax` = total, `aria-label="Reviewed"`, and the text "n of N done".
7. **`DefinitionsPicker({ value, onChange })`.**
   - It lists `liveScoreDefinitionsQuery` definitions as checkboxes labelled by name, with their data type as a caption.
   - The checked ones are shown first, in `value` order, each with "Move up" and "Move down" icon buttons (`ArrowUp`, `ArrowDown`) that have `aria-label`s.
   - Unchecked ones follow, sorted by name.
   - Checking appends to `value`. At most 50: further checkboxes are disabled with the caption "A queue holds at most 50 definitions."
   - With no live definition it shows "No score definitions yet." and a link to `/$org/$project/settings/score-definitions`.
8. **`QueueDialog({ open, onOpenChange, queue?, defaultTarget?, onCreated? })`**, which creates, or edits when `queue` is given. Mirror `components/datasets/dataset-dialog.tsx`: the mutation lives in the dialog, and the dialog keeps itself open while busy or dirty.
   - Fields:
     - Name (required, 1–100);
     - Kind, a radio group "Traces / Spans / Sessions", default `defaultTarget ?? "trace"`, only on create (on edit it is shown as text);
     - Description (optional, ≤ 1 000);
     - Instructions (a textarea, optional, ≤ 10 000, with the caption "Markdown. Reviewers see it above the form.");
     - Definitions (`DefinitionsPicker`, at least 1).
   - Create posts `CreateAnnotationQueue` and then calls `onCreated(queue)`. Edit patches `UpdateAnnotationQueue` with the queue's `version` and only the changed fields; a cleared text sends `""`.
   - Errors:
     - field errors map onto fields (`definition_ids[i]` goes onto the picker);
     - `409` whose detail equals `QUEUE_NAME_TAKEN` → name error "A queue with this name exists.";
     - any other `409` on edit → form error "Someone else changed this queue. Close and open it again.";
     - `403 plan_limit` → `FormError` with the problem detail and `UpgradeButton` with `useOrgEntitlements(org).upgradeUrl`, as `settings/new-project-dialog.tsx` does.
   - On success, invalidate `annotationQueueWriteKeys`.
9. **Tests** for the components:
   - Picker: check, reorder with Move up, and the cap of 50.
   - Dialog:
     - create sends `{name, target, definition_ids in order, instructions}` and calls `onCreated`;
     - a name 409 maps to the field;
     - `403 plan_limit` shows the detail and an Upgrade link with the fixture URL;
     - edit sends only changed fields with `version`;
     - a stale 409 shows the message.
   - Progress: the values and the text.

**Done when.**
- `pnpm --dir web run gen:routes`
- `pnpm --dir web exec vitest run src/lib src/components/annotation src/components/annotation-queues src/components/trace/annotate-tab.test.tsx src/components/session "src/routes/\$org/\$project/sessions" --maxWorkers=8`
- `export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH && just check-web`

**Commit:** `Prepare the annotation form and session view for queues`

---

### W2 — Queue list and queue page

**Goal.** `/annotation-queues` and `/annotation-queues/$queueId`, with the Items and Settings tabs.

**Edit:**
- `web/src/routes/$org/$project/annotation-queues/index.tsx` and `index.test.tsx` (new)
- `web/src/routes/$org/$project/annotation-queues/$queueId/index.tsx` and `index.test.tsx` (new)
- `web/src/components/annotation-queues/items-table.tsx`, `queue-settings.tsx`, and their tests (new)

**Steps.**
1. **List page** (`annotation-queues/index.tsx`, route `"/$org/$project/annotation-queues/"`, `validateSearch: parseQueuesSearch`):
   - an infinite query over `annotationQueuesInit` (mirror `datasets/index.tsx`), with `refetchInterval: QUEUE_POLL_MS` and `refetchOnWindowFocus: true`;
   - a toolbar with a debounced search box `q` that filters the loaded rows by name (client-side), and a "New queue" button inside `DisabledReason reason={canManageQueues(role) ? undefined : "Only members can create queues"}`. It opens W1b's `QueueDialog`, and `onCreated` navigates to the new queue's page;
   - a `DataTable` (`ariaLabel="Annotation queues"`) with these columns:
     - Name (a link to the queue page);
     - Kind (`targetLabel`);
     - Progress (`QueueProgress`);
     - Pending (number);
     - Updated (`RelativeTime`);
     - a "Start reviewing" link button to `/$org/$project/annotation-queues/$queueId/review/$itemId` with `itemId: "next"`, disabled for non-reviewers or when pending is 0;
   - an empty state: `EmptyState` with the title "No annotation queues yet" and the description "Create a queue, then add traces, spans or sessions from their lists to review them one at a time."
2. **Queue page** (`annotation-queues/$queueId/index.tsx`, route `"/$org/$project/annotation-queues/$queueId/"`, `validateSearch: parseQueueSearch`):
   - The loader runs `ensureQueryData(annotationQueueQuery)` and maps 404 to `notFound()`.
   - A `TopBarCrumb` with the queue name, and a `BackLink` to the list.
   - Header: name, kind, description, `QueueProgress`, and "Start reviewing" (as on the list).
   - Tabs (`tab` search param, default `items`), using Radix Tabs as the datasets page does:
     - **Items** (`ItemsTable`):
       - a status filter (`All`, `Pending`, `Completed`, `Skipped`; `status` search param);
       - an infinite items query with `refetchInterval: QUEUE_POLL_MS` and `refetchOnWindowFocus: true`;
       - columns:
         - target: a trace → trace page link; a span → trace page link with the span; a session → session page link; ids shown with `IdText`;
         - status;
         - "Reviewing" (`<CreatedBy createdBy={item.leased_by_user_id} />`);
         - "Done by" (`<CreatedBy createdBy={item.done_by_user_id} />`);
         - done at, skip reason, added;
       - each row has:
         - "Open" (to `review/$itemId` with the item id);
         - "Reopen" (for completed or skipped items, needs `canReview`: `POST …/reopen`, then invalidate);
         - "Remove" (needs `canManageQueues`: `ConfirmDialog`, then `DELETE`, then invalidate);
       - the empty state says: "No items yet. Add traces, spans or sessions from their lists with Add to queue."
     - **Settings:** `QueueSettings` shows the queue's fields with an "Edit" button (opens `QueueDialog` with the queue) and a danger zone "Delete queue". Delete uses `TypeToConfirmDialog` with the queue name, then `DELETE`, invalidation, and navigation to the list. The confirm text says "Its items are removed. Scores stay."
   - Viewers see the tabs and their buttons disabled with `DisabledReason`.

**Tests:**
- List:
  - renders the fixture queue (`static/annotation-queues.json`) with its progress and pending count;
  - the `h1` is "Annotation queues";
  - a viewer sees "New queue" disabled;
  - an empty page (`server.use`) shows the empty state.
- Queue page:
  - Items lists the fixture items;
  - the status filter sets `?status=` and sends it: use a `server.use` handler that records the request's `status` query and assert on it;
  - Reopen posts and refetches;
  - Remove confirms and deletes;
  - Settings edit opens the dialog;
  - Delete requires the name;
  - a 404 queue renders not found.

**Done when.**
- `pnpm --dir web run gen:routes`
- `pnpm --dir web exec vitest run "src/routes/\$org/\$project/annotation-queues/index" "src/routes/\$org/\$project/annotation-queues/\$queueId/index" src/components/annotation-queues/items-table src/components/annotation-queues/queue-settings --maxWorkers=8`, all green
- the phase's files are clean under `biome check`, and `tsc -b` reports no error in them other than the expected missing W3 route
- commit only the phase's files

**Commit:** `Add the annotation queue pages`

---

### W3 — Reviewer page

**Goal.** `/annotation-queues/$queueId/review/$itemId`: claim, show, annotate, complete, skip and move on, with the lease heartbeat and shortcuts.

**Edit:**
- `web/src/routes/$org/$project/annotation-queues/$queueId/review/$itemId.tsx` and `$itemId.test.tsx` (new)
- `web/src/components/annotation-queues/reviewer.tsx`, `reviewer-target.tsx`, `next-item.tsx`, `use-lease.ts`, `skip-dialog.tsx`, and a test per component that has logic (new)
- `web/src/components/trace/split-pane.tsx` (only: add the `annotation-review` entry to `SplitName` and `PANES`)

**Steps.**
1. **Route** `"/$org/$project/annotation-queues/$queueId/review/$itemId"`, `validateSearch: parseReviewSearch`:
   - The loader ensures `annotationQueueQuery` (404 → `notFound()`).
   - A `TopBarCrumb` "Review".
   - When `$itemId` is the literal `next`, the page renders `NextItem`; otherwise it renders `Reviewer`.
2. **`NextItem`** (`next-item.tsx`). Every move to another item goes through this one URL, `/review/next?after=<id>`.
   - For reviewers (`canReview`):
     - on mount it posts `POST …/next` with `{ after }` (`{}` without `after`) exactly once per location: guard with a `useRef` keyed by the router's location key, so React StrictMode's double effect does not claim twice;
     - an item → `navigate` to `/review/<its id>` with `replace: true`;
     - `204` → the end screen: `EmptyState` with the title "Nothing left to review" and the description "All pending items are done or being reviewed by someone else.", plus "Back to the queue" and "Check again". "Check again" posts `next` with `{}` and handles the answer the same way.
   - For viewers: no request. The note "Only members can review." and a link back to the queue.
3. **`Reviewer`.** It loads the item with `annotationQueueItemQuery` (`refetchInterval: QUEUE_POLL_MS`, `refetchOnWindowFocus: true`; 404 → `notFound()`), and the queue with `annotationQueueQuery` (same polling). Layout: a two-column `SplitPane` with `name="annotation-review"`.
   - **Left**, `ReviewerTarget`:
     - a trace item: `TraceView traceId={item.trace_id}` with local `spanId` state and `onSelectSpan` setting it, and no `onClose`;
     - a span item: the same, starting at `item.span_id`;
     - a session item: it loads `sessionQuery(org, project, item.session_id, {})` and renders `SessionConversation` with local `trace` state, `scrollTarget={null}` and no `onCloseTrace`;
     - when the target's detail query answers 404, it shows `EmptyState` "This trace is no longer available" ("This session is no longer available" for a session), with "Remove from queue" for `canManageQueues` (`DELETE` the item, then navigate to `/review/next?after=<id>`).
     - `TraceView`'s own header actions (Add to dataset, Run evaluator, and W4a's Add to queue) stay; that is accepted.
   - **Right**, top to bottom:
     - the counter "n of N done" from the queue's counts (`QueueProgress`);
     - the instructions in `<details open><summary>Instructions</summary><Markdown text={queue.instructions} /></details>`, only when set;
     - the item's status line: "Completed by <CreatedBy createdBy={item.done_by_user_id} /> <RelativeTime …/>" or "Skipped by … — <reason>" when it is not pending; "Being reviewed by <CreatedBy createdBy={item.leased_by_user_id} />" when another reviewer holds it;
     - the form, under these rules:
       - Show a `Skeleton` until the target's detail (trace detail for trace and span items, `sessionQuery` for session items) has loaded. The form needs its `session_id` and `user_id`.
       - Then render `AnnotationForm`, keyed by the item id, with:
         - `target` from the item;
         - `context`: `{ sessionId, userId, queueId }`, where `sessionId` and `userId` come from the trace detail for trace and span items, or `userId` from the session for session items;
         - `definitionIds={queue.definition_ids}`, `saveRef`, `hideSave`, and `onReady={() => setFormReady(true)}`.
       - Do **not** render the form when another reviewer holds the item, or when the lease is lost. Render it normally for a completed or skipped item: the caller can still edit their own scores (D-AQ-5).
       - A 404 target renders no form.
     - the buttons:
       - "Complete and next" (primary; pending items only);
       - "Next";
       - "Skip…" (pending items only);
       - "Previous" (`router.history.back()`);
       - "Reopen" (completed or skipped items only).

       All are disabled while busy, and "Complete and next" until `formReady`.
   - Viewers see the target, the instructions and the status only, with the note "Only members can review.", Next and Previous. There is no form and no lease.
4. **Lease** (`use-lease.ts`, `useLease(org, project, queueId, item, enabled)`):
   - It is enabled only for `canReview` users on a pending item that is free or theirs (`leased_by_user_id` absent, `lease_expires_at` past, or equal to `me.user.id`).
   - On mount it posts `…/lease`: `200` → `held`; `409` → `lost`, with the holder id from `errors[0].message`.
   - Every `LEASE_RENEW_MS` it posts `…/lease` again; a `409` turns the state to `lost`.
   - On unmount, or when the item id changes, it calls `api.DELETE("/api/v1/projects/{project}/annotation-queues/{id}/items/{item_id}/lease", { params: { path: { project, id, item_id } }, keepalive: true })` and ignores the result. Under StrictMode the release-then-lease of a remount is acceptable, and a redundant release after Complete answers `204`.
   - It returns `{ state: "idle" | "held" | "lost", holder?: string }`.
   - When `lost`, the page hides the form and shows "Another reviewer took this item." with a "Next" button.
5. **Complete and next.** Steps, in order:
   1. `busy = true`.
   2. `const ok = saveRef.current ? await saveRef.current.save() : true`. A `null` handle means nothing to save (the archived notice, for example). If `ok` is false, set `busy = false` and stop; the form shows the error.
   3. `POST …/complete`. A `409` shows the problem (the holder, or "not pending") and stops.
   4. Invalidate `annotationQueueWriteKeys`, then navigate to `/review/next?after=<item id>`. When `next` answers the same item id (the only pending item, held by the caller), `NextItem`'s `replace` navigation lands on it again, which is correct.
6. **Next.** Navigate to `/review/next?after=<item id>` without saving or changing the state. With unsaved edits (`saveRef.current?.isDirty()`), ask first with `ConfirmDialog` "Discard your unsaved annotations?".
7. **Skip** (`SkipDialog`):
   - an optional reason textarea (≤ 1 000), and "Skip and next";
   - it saves nothing, posts `…/skip` with `{ reason }`, invalidates, then navigates as Next does.
8. **Reopen.** `POST …/reopen`, invalidate, and stay on the item. It becomes pending, and the lease starts.
9. **Shortcuts.** One `useEffect` per item id registers a `window` `keydown` listener:
   - `Enter` with `metaKey || ctrlKey`, without `shiftKey` → Complete and next;
   - with `shiftKey` → Next;
   - ignore `event.isComposing` and `event.repeat`; call `preventDefault()`;
   - read `busy`, `skipOpen`, `discardOpen`, `formReady` and the role through refs (the listener would otherwise see stale values), and do nothing while busy, while either dialog is open, or for viewers.

**Tests.** Use `server.use` for the queue, the item and the trace or session with explicit ids in every test, and for every write. Use `recordRequests` for order. The `GET me`, members and chip requests are answered by the manifest.
- `next resolves to an item`: `/review/next` posts `next` once (StrictMode included) and lands on `/review/<id>`.
- `next with nothing free shows the end screen`; "Check again" posts again.
- `viewer on next sends nothing`.
- `trace item shows the trace and the queue's definitions in order`.
- `span item selects the span`.
- `session item shows the conversation`.
- `the form waits for the trace detail`.
- `complete and next saves, completes, then claims the next`: the requests go in this order: `POST /scores` (with `queue_id`), `POST …/complete`, `POST …/next` (with `after`).
- `a failed save completes nothing`.
- `ctrl+enter completes and next`; `ctrl+shift+enter moves on without completing`.
- `skip sends the reason and moves on`.
- `lease conflict hides the form`: the lease `POST` answers `409` with `errors[0].message = BOB_ID`, and the page shows "Another reviewer took this item.".
- `the heartbeat renews the lease`: use `vi.useFakeTimers({ toFake: ["setInterval", "clearInterval"] })`, advance 60 s, and a second lease `POST` happens.
- `leaving releases the lease`: unmount; `DELETE …/lease` is sent.
- `viewer sees no form and sends no lease`.
- `a deleted trace shows the not-available state and Remove`.
- `completed item shows who completed it, the editable form, and Reopen`.

**Done when.**
- `pnpm --dir web run gen:routes`
- `pnpm --dir web exec vitest run "src/routes/\$org/\$project/annotation-queues/\$queueId/review" src/components/annotation-queues/reviewer src/components/annotation-queues/next-item src/components/annotation-queues/use-lease src/components/annotation-queues/skip-dialog --maxWorkers=8`, all green
- the phase's files are clean under `biome check`, and `tsc -b` reports no error in them other than the expected missing W2 route
- commit only the phase's files

**Commit:** `Add the annotation queue reviewer`

---

### W4a — Add-to-queue dialogs, the chip, and the trace and session pages

**Goal.** "Add to queue" and "Add matching to queue…" dialogs; "Add to queue" on the trace and session pages; the "In n queues" chip.

**Edit:**
- `web/src/components/annotation-queues/add-to-queue-dialog.tsx`, `add-matching-dialog.tsx`, `queues-chip.tsx`, and their tests (new)
- `web/src/components/trace/trace-view.tsx`, `web/src/components/trace/trace-header.tsx`
- `web/src/components/session/session-view.tsx` (the `SessionHeader` part only)
- the existing test files whose exact-text assertions the chip breaks (expected: `web/src/components/trace/trace-view.test.tsx`, the trace header test, `web/src/routes/$org/$project/sessions/$sessionId.test.tsx`). Change only those assertions.

**Steps.**
1. **`AddToQueueDialog`** (`{ open, onOpenChange, target: "trace" | "span" | "session", items: QueueItemTarget[], onAdded?: () => void }`). Mirror `components/datasets/add-traces-to-dataset-dialog.tsx`:
   - the title is "Add N trace(s)/span(s)/session(s) to a queue";
   - a `Select` of the project's queues of the matching kind (the first page of `annotationQueuesInit`, filtered by `target`), with a "New queue…" option that opens W1b's `QueueDialog` with `defaultTarget={target}`; `onCreated` selects the new queue;
   - submit posts `AddQueueItems` (at most `ADD_ITEMS_MAX = 1_000` items; the callers cap the selection);
   - the result view shows "Added N" and "N were already in the queue", plus the skipped list (index → the target's short id and message), and the buttons Close and "Open queue";
   - a `422 too_many_items` shows "The queue is full (100 000 items).";
   - on success it invalidates `annotationQueueWriteKeys` and calls `onAdded`;
   - export `addToQueueReason(role, count)`: "Only members can add to queues" for non-managers, and "Select at most 1 000 rows" above the cap.
2. **`AddMatchingDialog`** (`{ open, onOpenChange, target, selection: { filter?: string; search?: string; from: string; to: string; includeExperiments: boolean } }`). Mirror `components/deletions/delete-matching-dialog.tsx`:
   - choose the queue first (the same `Select`, without "New queue…");
   - then preview with `POST …/items/matching/preview`: "Counting matching …" → "N traces match" (with "Only the first 10 000 are added. Narrow the filter to add the rest." when `capped`);
   - then "Add N to queue" posts `…/items/matching`;
   - the result: "Added N, N already there, N blocked by a deletion";
   - `400 invalid_filter` → "The filter is not valid. Fix it on the list first.";
   - a session queue never sends `search`;
   - the eval toggle of the list is ignored (evaluation traces are never matched).
3. **`QueuesChip`** (`{ traceId } | { sessionId }`):
   - `useQuery(queuesHoldingQuery(...))`. In tests the manifest's `list_annotation_queues` row answers it for any target;
   - renders nothing while loading, on error or at 0;
   - otherwise a `MetricChip` with a `ClipboardCheck` icon and the text "In 1 queue" or "In N queues", linking to that queue for one, else to `/$org/$project/annotation-queues`.
4. **Trace page.**
   - In `trace-view.tsx`, add an `AddToQueueButton` next to `AddToDatasetButton`. Its label is "Add to queue"; it opens `AddToQueueDialog` with `target: "span"` and `{trace_id, span_id}` when a non-root span is selected, else `target: "trace"`.
   - In `trace-header.tsx`, render `<QueuesChip traceId={trace.trace_id} />` in the second row, after the score badges.
5. **Session page.** In `SessionHeader` (`session-view.tsx`), add "Add to queue" next to "Annotate" (same `DisabledReason` pattern, `canManageQueues`), opening the dialog with `target: "session"`. Render `<QueuesChip sessionId=… />` in the `MetricChip` row.
6. **Chip assertions.** Run the trace and session tests. Where an existing exact-text assertion now includes "In 1 queue" because the manifest fixture holds a queue, update that assertion to expect it. Do not change fixtures.

**Tests:**
- `AddToQueueDialog`:
  - lists only queues of the kind;
  - posts the items;
  - shows the result and skipped rows;
  - 422 message;
  - "New queue…" opens the dialog with the kind;
  - viewer reason.
- `AddMatchingDialog`:
  - preview → add, with the exact bodies, including the session queue without `search`;
  - the capped note;
  - invalid filter.
- `QueuesChip`: 0 → nothing; 1 → "In 1 queue" linking to the queue; 3 → "In 3 queues".
- Trace view: "Add to queue" on a selected child span opens the dialog with a span target.
- Session header: "Add to queue" opens the dialog with a session target.

**Done when.**
- `pnpm --dir web run gen:routes`
- `pnpm --dir web exec vitest run src/components/annotation-queues/add-to-queue-dialog src/components/annotation-queues/add-matching-dialog src/components/annotation-queues/queues-chip src/components/trace src/components/session "src/routes/\$org/\$project/sessions/\$sessionId" "src/routes/\$org/\$project/traces/\$traceId" --maxWorkers=8`, all green
- the phase's files are clean under `biome check`, and `tsc -b` reports no error in them
- commit only the phase's files

**Commit:** `Add to annotation queues from the trace and session pages`

---

### W4b — Add to queue from the lists

**Goal.** Checked rows and "Add matching to queue…" on the traces, spans and sessions lists. It runs alone, after W2, W3 and W4a are committed and the coordinating session has run `just check-web` green.

**Edit:**
- `web/src/routes/$org/$project/traces/index.tsx`, `spans/index.tsx`, `sessions/index.tsx`, and their tests

**Steps.**
1. **Traces list.**
   - When rows are checked, add an "Add N to queue" button next to "Add N to dataset" (`DisabledReason reason={addToQueueReason(role, n)}`).
   - Add an "Add matching to queue…" button next to "Delete matching…", enabled for `canManageQueues`, with the selection `{filter, search, from: resolved.from, to: resolved.to, includeExperiments: search.exp === 1}`.
   - Both open their W4a dialog. The selection clears `onAdded`, as the dataset dialog's does.
2. **Spans list.**
   - Add row checking as the traces page does: `selectionKey` from `[search.filter, search.search, rangeKey, search.eval, search.exp]`, plus `checkedIds` and `onCheckedChange` on `DataTable`. Row ids are `` `${trace_id}:${span_id}` ``; split on `:` for the items.
   - Add the `actions` prop of `ListToolbar` with "Add N to queue" and "Add matching to queue…" (`target: "span"`).
3. **Sessions list.** The same as the spans list, with row ids `session_id`, `selectionKey` from `[search.filter, rangeKey, search.eval, search.exp]` (no search text), and `target: "session"`. The matching selection has no `search`.

**Tests:**
- Traces list:
  - "Add matching to queue…" sends the list's filter, search and range;
  - checking rows shows "Add N to queue".
- Spans list:
  - checking two rows shows "Add 2 to queue", and the dialog receives both pairs;
  - clicking a row's cell still opens its panel, and clicking its checkbox does not.
- Sessions list: the same with sessions.
- All existing tests of the three pages pass. If an existing exact-text assertion now sees the chip or the new buttons, update that assertion only.

**Done when.**
- `pnpm --dir web run gen:routes`
- `pnpm --dir web exec vitest run "src/routes/\$org/\$project/traces/index" "src/routes/\$org/\$project/spans" "src/routes/\$org/\$project/sessions/index" --maxWorkers=8`
- `export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH && just check-web`

**Commit:** `Add to annotation queues from the trace, span and session lists`

---

### W5 — Docs, screenshots, spec and changelog

**Goal.** The user page, its screenshots, spec 11 and the changelog. It runs after W4b is committed.

**Edit:**
- `website/src/content/docs/docs/evaluation/annotation-queues.mdx` (new)
- `website/astro.config.mjs` (one sidebar entry)
- `website/src/content/docs/docs/evaluation/index.mdx`, `evaluation/annotation.mdx`, `platform/deletion-and-retention.mdx`, `platform/plans-and-licence.mdx` (one pointer or line each)
- `website/screenshots.json` and `website/src/assets/screenshots/annotation-queues*.png` (new)
- `docs/spec/11-ui.md`
- `CHANGELOG.md`

**Steps.**
1. **Page** `evaluation/annotation-queues.mdx`:
   - Frontmatter: `title: Annotation queues`, and a `description` of at most 160 characters, for example "Collect traces, spans or sessions in a queue and review them one at a time with your score definitions."
   - Follow `website/STYLE.md`: second person, task steps numbered, no banned terms, "member" for people.
   - Add `<Since v="…" />` as `evaluation/datasets.mdx` does for unreleased features.
   - Sections:
     - **Create a queue:** kind, definitions, instructions; Free plan: one queue per organization.
     - **Add items:** the trace page, the session page, checked rows (up to 1 000), Add matching to queue (up to 10 000, never evaluation traces).
     - **Review:** Start reviewing, the lease ("While you review an item, nobody else gets it for five minutes; the page keeps it while it is open."), Complete and next, Next, Skip, Previous, the shortcuts table (`Ctrl+Enter` / `⌘+Enter`, `Ctrl+Shift+Enter` / `⌘+Shift+Enter`), reopen.
     - **Scores from a queue:** they carry the queue id; filter with `queue_id` in the SQL console. Use a `sql` code block (exempt from sample markers), for example `SELECT name, value_num FROM scores WHERE queue_id = '…'`.
     - **What happens on deletion and retention.**
   - Images: the two screenshots below, with alt text.
2. **Sidebar:** in `astro.config.mjs`, add `{ label: "Annotation queues", link: "/docs/evaluation/annotation-queues/" }` after "Human annotation".
3. **Pointers:**
   - `evaluation/index.mdx`: a row or bullet for queues;
   - `annotation.mdx`: one sentence linking to the queue page;
   - `deletion-and-retention.mdx`: queue items are removed with their traces and by retention;
   - `plans-and-licence.mdx`: Free gets 1 annotation queue.
4. **Screenshots** in `website/screenshots.json`:
   - `annotation-queues`: path `/acme/support-bot/annotation-queues`, `waitFor` = the fixture queue's name;
   - `annotation-queue-review`: path `/acme/support-bot/annotation-queues/<fixture queue id>/review/<fixture item id>`, `waitFor` "Complete and next";
   - use the small fixture data set, as other fixture shots do (`"now": "2026-01-01T11:00:00Z"`).

   Run `just docs-screenshots annotation-queues annotation-queue-review` (needs Chromium: `pnpm --dir website exec playwright install chromium` once). If the dev mocks do not render the reviewer (for example, the trace fixture is missing), report it, and keep only the list screenshot.
5. **Spec 11:**
   - add `/:org/:project/annotation-queues`, `…/:queueId` and `…/:queueId/review/:itemId` to the route list;
   - a § Annotation queues section summarizing the pages, the reviewer, the lease and the shortcuts;
   - the Annotate paragraph notes the restricted definitions and `queue_id` when opened from a queue;
   - the lists' bulk actions now include "Add to queue" on traces, spans and sessions;
   - the nav lists the entry with go key `r`.
6. **`CHANGELOG.md`** § `[Unreleased]` § Added: "Annotation queues in the UI: create queues, add traces, spans or sessions from their pages and lists, and review them one at a time with Complete and next."

**Done when.**
- `export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH && just check-docs && just check-web`

**Commit:** `Document annotation queues`
