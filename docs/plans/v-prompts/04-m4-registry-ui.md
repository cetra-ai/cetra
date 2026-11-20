# V · M4 — Registry UI: prompts pages, labels, metrics, links in traces

Implementation plan for milestone M4 of track V. It has 5 phases, **V4.1 to V4.5**, one commit each, done in order. **Haiku** implements each phase with no context beyond this plan, the design doc and the files it names; **Sonnet** reviews the milestone. If this plan and the code disagree, **STOP and report** instead of guessing. Each phase ends with `just check-web` green (V4.5: `just check`).

**Prerequisites** (each must print at least one line, else STOP): M1 and M3 are merged in this branch and their generated files are current.

```bash
grep -n '"/api/v1/projects/{project}/prompts/{prompt}/resolve"' web/src/api/schema.gen.ts
grep -n '"/api/v1/projects/{project}/prompts/{prompt}/metrics"' web/src/api/schema.gen.ts
grep -n "prompt_name" web/src/api/schema.gen.ts
grep -n "export function canManagePrompts" web/src/lib/roles.ts
ls fixtures/api/responses/prompts/support-reply.json
```

**Read first** (every phase): `AGENTS.md`; `docs/spec/11-ui.md` § Navigation, § Layout and density, § Datasets; `docs/plans/v-prompts/00-design.md` § UI, D-V-8, D-V-9, D-V-10, D-V-13, D-V-17; `docs/plans/v-prompts/01-m1-registry.md` § API types (V1.3 § 3) and § The example prompts; this file up to § V4.1, then your phase.

## What M4 is, and what it is not

M4 delivers the web UI of the registry:
- **Prompts** in the project sidebar (section Evaluate, go-key `g r`): the list with search and label/tag filters, a "New prompt" dialog, and a "Protected labels" dialog for admins;
- the **prompt page** `/:org/:project/prompts/:prompt` with tabs Template (view and edit, saving a new version), Versions (list, labels, diff with the previous version), Labels (label history) and Metrics (per-version table, links to the traces);
- a prompt link in the span header, a `Prompts` facet and column on traces, a `Prompt` facet and column on spans;
- dev-server write mocks, docs screenshots, spec 11, changelog.

M4 does not add the playground (M7 adds "Open in playground"), time-series charts (the metrics route answers aggregates), or dashboard widget fields for prompts (M3: not yet).

## Decisions this plan relies on

| ID | Summary |
|---|---|
| D-V-8 | `latest` is computed: `Prompt.labels` never lists it; `PromptVersion.labels` lists it on the latest version |
| D-V-9 | the create dialog has "Label as production" checked for version 1; the page always shows where `production` points |
| D-V-10 | protected labels (default `production`) are moved by project admins only; the UI disables their controls below admin with a reason, and the server answers `403 forbidden` |
| D-V-13 | `canManagePrompts` (member and above) gates every write control; viewers read |
| D-V-17 | metrics: counts per version over a window (7 days default, ≤ 90 days) |

Made precise by this plan (V4.5 records them in `docs/mvp-decisions.md` § Track V as **D-V-32** and in the design's § UI, unless the M5 session already used D-V-32; then take the next free number and say so):
- **No `/prompts/new` route.** A prompt may be named `new` (D-V-2), so creation is a dialog on the list page (`?create=1` opens it), never a route.
- **Route param** `$prompt` (the name), search `tab` (`template` default, `versions`, `labels`, `metrics`) and `version` (≥ 1; absent = latest).
- **Labels are set from the Versions tab** (a "Set label…" action on a version row) and removed from their chip; protected chips carry a lock.
- **The diff** compares a version with the one before it, line by line (`lib/line-diff.ts`), template then config.
- **Metrics windows** are 24 hours, 7 days (default) and 30 days, matching the traces list presets, so each version's "Traces" link opens the traces list on the same window.

## Versions

No new dependency (recharts, lucide, radix, `lib/line-diff.ts` exist).

## Conventions for every phase

- `web/` rules: biome, the type scale (`text-caption|body|ui|title|heading|display` only), accessible names on every control, no `text-xs` etc. Use `Schema<"…">` from `@/api/types`, `$api` hooks from `@/api/client`, `projectRef(org, project)`, `problemOf`, `fieldErrors`, `problemMessage` from `@/api/problem`. Feedback is inline (`FormError`, `role="status"` banners); there is no toast system.
- Pages read the role with `const { org, project, data } = useProject();` and gate writes with `canManagePrompts(data.role)` and admin checks with `atLeast(data.role, "admin")`; disabled controls are wrapped in `DisabledReason` with the reason text given below.
- Tests: page tests use `renderApp(url)` from `@/test/render` and override endpoints with `server.use(http.get|post|put|patch|delete(…))` from `msw` and `@/mocks/node`; problems with `problemResponse` from `@/test/problem`; request recording with `recordRequests()` from `@/test/requests`. Never edit `fixtures/api/manifest.json` or `fixtures/api/responses/**` by hand.
- **Machine load**: other sessions build on this machine. While iterating run `pnpm --dir web gen:routes` once, then only touched tests: `pnpm --dir web exec vitest run <paths> --maxWorkers=8`; before the commit run once `export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH && just check-web`. Never kill a process you did not start.
- Commit once per phase with the exact subject, staging your files by path; no attribution lines.

## Shared reference

### Queries (`web/src/api/queries-prompts.ts`, V4.1)

In the style of `web/src/api/queries-datasets.ts`, with every path param `project: projectRef(org, project)` and `prompt` (the name):

```ts
export function promptsInit(org: string, project: string, filters: { q?: string; label?: string; tag?: string })   // query: { limit: 100, ...set filters }
export function promptsKey(org: string, project: string)                     // GET /prompts prefix
export function promptQuery(org: string, project: string, prompt: string)    // GET /prompts/{prompt}
export function promptKey(org: string, project: string, prompt: string)
export function promptVersionsInit(org: string, project: string, prompt: string)   // query: { limit: 50 }
export function promptVersionsKey(org: string, project: string, prompt: string)
export function promptVersionQuery(org: string, project: string, prompt: string, version: number)  // GET /versions/{version}
export function promptLabelEventsInit(org: string, project: string, prompt: string) // query: { limit: 100 }
export function promptLabelEventsKey(org: string, project: string, prompt: string)
export function promptSettingsQuery(org: string, project: string)            // GET /prompt-settings
export function promptSettingsKey(org: string, project: string)
export function promptMetricsQuery(org: string, project: string, prompt: string, from: string, to: string)  // GET /metrics?from&to
/** Every key a prompt write can change. */
export function promptWriteKeys(org: string, project: string, prompt: string): QueryKey[]  // list, prompt, versions, version queries prefix, label events
```

### Fixed texts

| Where | Text |
|---|---|
| sidebar label, top-bar title | `Prompts` |
| viewer reason on write controls | `Only members can edit prompts` |
| protected reason | `Only project admins can move protected labels` |
| settings reason | `Only project admins can change protected labels` |
| empty list | title `No prompts yet`, description `Create a prompt here, or with client.prompts.create in the SDK.` |
| no match | `No prompt matches these filters` |
| label 403 | `Only project admins can move this label.` |

## Precisions (from the cold review; they override the phase text where they differ)

1. **Field positions.** `web/src/test/problem.ts` (and its test, if any) joins V4.1's Edit list: its `errors` items gain `position?: { line: number; column: number }`. `lib/prompts.ts` adds `fieldPositions(error: unknown): Record<string, { line: number; column: number }>` (first position per field, from `problemOf(error)?.errors`), with a unit test; `TemplateEditor` takes `positions` from it.
2. **Metrics window.** Compute `{ from, to }` once per chosen preset (`useMemo(() => metricsWindow(preset, Date.now()), [preset])`, with `to` rounded down to the minute), never per render. Test 2 asserts `Date.parse(to) - Date.parse(from)` equals 30 days.
3. **Create dialog and protected `production`.** Read `promptSettingsQuery`. When `production` is protected and the role is below admin, the "Label as production" checkbox is unchecked and disabled inside `DisabledReason` with the protected reason, and the body sends `labels: []`. Add a V4.1 test (member: disabled, unchecked, POST body `labels: []`). The 403 text stays as a fallback.
4. **`LabelChip` in V4.1.** Create `components/prompts/label-chip.tsx` in V4.1 (badge with `categoryClasses(label).pill`, a `Lock` icon with `aria-label="Protected"` when protected; the remove button and its confirm come in V4.2). The list's Labels column is a small cell component that calls `useQuery(promptSettingsQuery(org, project))` itself (`useProject()` for org and project).
5. **Write keys.** `promptWriteKeys` returns the keys of the list prefix, the prompt, the versions list and the label events. Single-version queries (`/versions/{version}`) are invalidated with `queryClient.invalidateQueries({ predicate: (q) => q.queryKey[1] === "/api/v1/projects/{project}/prompts/{prompt}/versions/{version}" })`. Write one helper `invalidatePrompt(queryClient, org, project, prompt)` in `queries-prompts.ts` doing both, and use it after every write. Tests that save mock the refetched GETs (prompt with the new `latest_version`, versions list, the new version) with `server.use`, since the test client never refetches by itself.
6. **Filter options.** The label `Select` offers `All labels` (value `all`, the sentinel), `latest`, `production`, the protected labels, the labels of the loaded rows and the currently selected label (deduplicated, sorted after `latest`/`production`); the tag `Select` offers `All tags` (value `all`), the tags of the loaded rows and the selected tag. Choosing `all` removes the search key.
7. **Other write errors.** In the create, save-version, set-label, delete and settings dialogs, any error not mapped to a field or to the texts above shows `problemMessage(error, "Could not save the prompt.")` as the `FormError` (409 caps, 413, 5xx).
8. **Unsaved edits.** `<UnsavedChangesGuard dirty={editing && changed} ignoreSearchChanges />`; set `editing` to false before navigating after a save.
9. **Spec 11 and docs.** V4.5 also edits `docs/spec/11-ui.md` line ~60 (Prompts first among the Evaluate pages and in the list of pages whose crumb is the item's name), rewrites the design's § UI bullets (`/prompts/:prompt`, the create dialog), and adds the `Prompts` facet/column and the span `Prompt` facet/column to the tracing docs pages that list facets or columns (`grep -rln "Models" website/src/content/docs/docs/tracing` to find them; if none lists columns or facets, nothing to do there).
10. **Tests: roles.** The default project role in tests is `owner` (`fixtures/api/static/project.json`). For `member`/`viewer`, `server.use(http.get("/api/v1/projects/:project", () => HttpResponse.json({ ...projectFixture, role: "member" })))` as `datasets/index.test.tsx` does.
11. **Tests: requests.** `recordRequests()` records bodies for POST, PATCH and PUT; wait with `waitFor`. Radix selects are opened with `user.click(await screen.findByRole("combobox", { name: "…" }))`.
12. **Component props** to copy exactly: `TypeToConfirmDialog` `{ open, onOpenChange, title, description, expected, inputLabel, confirmLabel, pending, error, onConfirm }` (expected = the prompt name); `TopBarCrumb` takes children (`<span className="max-w-64 truncate text-body font-medium">{name}</span>`); `BackLink` needs `fallback={<Link … aria-label="Back to prompts" className={BACK_LINK_CLASS}><ChevronLeft …/>Prompts</Link>}`; `DataTable` props as `datasets/index.tsx` passes them, with `emptyState={null}` and the page rendering `EmptyState`; the version `Select` uses `LOAD_MORE = "load-more"` and a `chooseVersion` that calls `fetchNextPage` for it, as `$datasetId.tsx`.
13. **Bad `version` in the URL.** A `version` above `latest_version` is removed from the search by an effect (as the dataset page does); a version that answers 404 shows `EmptyState` `Version N does not exist.` with a `Go to latest` button.
14. **Metrics edge cases.** Error rate is `—` when `span_count` is 0. Tests find a row's link with `within(row)`.
15. **Trace `prompts` cell.** `(row.original.prompts ?? [])`; write a private `promptsLabel` beside the models cell (first value plus `+N`).
16. **Screenshots** `waitFor` is a plain string.
17. **Dotted names.** Add a V4.2 test: `renderApp("/acme/support-bot/prompts/v1.2")` with `server.use` answering the prompt `v1.2` renders its page (the `$prompt` param matches names with dots).

---

## V4.1 — Queries, sidebar, prompts list, create and settings dialogs

**Commit subject:** `web: add the prompts list, the new prompt dialog and protected labels`

### Read first

`web/src/api/queries-datasets.ts`; `web/src/routes/$org/$project/datasets/index.tsx` and its test; `web/src/routes/$org/$project/evaluators/index.tsx` (a simple list page); `web/src/components/datasets/dataset-dialog.tsx` (form, dirty guard, field errors, 409 detail match); `web/src/components/shell/nav.ts`, `nav.test.ts`; `web/src/lib/shortcuts.ts`, `shortcuts.test.ts`; `web/src/components/disabled-reason.tsx`; `web/src/components/data-table/data-table.tsx`; `web/src/lib/datasets.ts` (`parseJsonField`, `prettyJson`); `web/src/lib/search/common.ts`.

### Edit

| File | Change |
|---|---|
| `web/src/api/queries-prompts.ts` | new |
| `web/src/components/shell/nav.ts` | `{ section: "Evaluate", to: "/$org/$project/prompts", label: "Prompts", icon: FileText }`, placed first in the Evaluate section's run (before Evaluators) |
| `web/src/lib/shortcuts.ts`, `shortcuts.test.ts` | `r: { id: "go-prompts", to: "/$org/$project/prompts" }` and a test case |
| `web/src/routes/$org/$project/prompts/index.tsx` | new list page |
| `web/src/routes/$org/$project/prompts/index.test.tsx` | new |
| `web/src/components/prompts/prompt-columns.tsx` | new |
| `web/src/components/prompts/new-prompt-dialog.tsx` | new |
| `web/src/components/prompts/template-editor.tsx` | new (used by the dialog and V4.2) |
| `web/src/components/prompts/protected-labels-dialog.tsx` | new |
| `web/src/lib/prompts.ts`, `prompts.test.ts` | new: search parsing, label/name rules, template helpers |

### Steps

1. **`lib/prompts.ts`**:
   - `parsePromptsSearch(search) → { q?: string; label?: string; tag?: string; create?: true }` (trimmed non-empty strings ≤ 200 characters; `create` is `true` only for `"1"`/`1`/`true`).
   - `parsePromptSearch(search) → { tab?: "template" | "versions" | "labels" | "metrics"; version?: number }` (`version` an integer ≥ 1 from a number or digit string, else absent).
   - `isValidPromptName(name)`: `/^[A-Za-z0-9][A-Za-z0-9_.-]{0,199}$/`; `isValidLabel(label)`: `/^[a-z0-9][a-z0-9_.-]{0,63}$/`, not all digits, not `latest` (mirrors D-V-2, D-V-8; the server stays the authority).
   - `type ChatEntry = { role: "system" | "user" | "assistant"; content: string } | { type: "placeholder"; name: string }`; `templateText(template: unknown, type: "text" | "chat"): string` serializes for the diff: text → the string; chat → one block per entry, `"[system]\n" + content` (role in brackets) or `"[placeholder: name]"`, blocks joined by `"\n"`. `asChat(template: unknown): ChatEntry[]` (entries that do not fit are dropped).
   - Tests for each function, including a name of 200 characters, `new`, `latest`, `123`.
2. **Queries** as § Queries.
3. **Sidebar and go-key**: the nav entry and the shortcut; `nav.test.ts` and `shortcuts.test.ts` pass with the new entry.
4. **List page** (`prompts/index.tsx`), route `createFileRoute("/$org/$project/prompts/")` with `validateSearch: parsePromptsSearch`:
   - a toolbar: search input (debounced 300 ms into `q`, as the datasets page), a label `Select` (options: `All labels`, then the labels present in the loaded rows plus `production`), a tag `Select` (`All tags` and the tags of the loaded rows), a `Protected labels` button (V4.1 step 7) and a `New prompt` button (`DisabledReason` with the viewer reason when `!canManagePrompts`). Changing a select writes `label`/`tag` into the search.
   - a `DataTable` (`tableId="prompts"`, `ariaLabel="Prompts"`) over `$api.useInfiniteQuery("get", "/api/v1/projects/{project}/prompts", promptsInit(…), { pageParamName: "cursor", … })` with `PROMPT_COLUMNS` (`prompt-columns.tsx`): Name (grow), Type, Labels (chips `label → vN`, `Lock` icon on protected ones from `promptSettingsQuery`), Latest (`v{latest_version}`), Tags, Updated (`RelativeTime`). Activating a row navigates to `/$org/$project/prompts/$prompt` with `params.prompt = row.name`.
   - empty states with the fixed texts; `if (query.error) throw query.error`.
   - `?create=1` opens the new-prompt dialog; closing it removes `create` from the search.
5. **Template editor** (`template-editor.tsx`): `TemplateEditor({ type, value, onChange, errors, disabled })` where `value` is `string` (text) or `ChatEntry[]` (chat):
   - text: one `Textarea` (`font-mono text-body`, `aria-label="Template"`, `spellCheck={false}`, rows 10).
   - chat: an ordered list; each message row has a role `Select` (`aria-label="Role of message {n}"`), a content `Textarea` (`aria-label="Content of message {n}"`), and buttons Move up, Move down, Remove (`aria-label`s with the number); a placeholder row shows `Placeholder` and a name `Input` (`aria-label="Name of placeholder {n}"`). Buttons below: `Add message` (role `user`) and `Add placeholder` (name `history`).
   - `errors: Record<string, string>` from `fieldErrors`: `template` is shown under the editor (with ` (line L, column C)` appended when the problem's error has a `position`: pass a `positions` map too), `template[i]`, `template[i].role`, `template[i].content`, `template[i].name` under the entry `i`.
6. **New prompt dialog** (`new-prompt-dialog.tsx`), on `$api.useMutation("post", "/api/v1/projects/{project}/prompts")`: fields Name (`TextField`), Type (radio Text/Chat; switching resets the template to `""` or `[{ role: "system", content: "" }, { role: "user", content: "" }]`), Template (`TemplateEditor`), Config (JSON `Textarea` checked with `parseJsonField(text, { required: false, object: true })`, default empty), Commit message (`TextField`, optional), a checkbox `Label as production` checked by default (sends `labels: ["production"]` when checked, `[]` otherwise). Submit `Create prompt`. Client checks before sending: `isValidPromptName`, JSON. Field errors from a 400 map onto fields (`name`, `template…`, `config…` shown under the config textarea, `labels[0]` under the checkbox); a 409 shows `A prompt with this name already exists` under Name; a 403 on `labels` (protected `production` as a member) shows `Only project admins can set the production label; uncheck "Label as production".` as the form error. On success invalidate `promptsKey` and navigate to the new prompt's page. Dirty guard as `dataset-dialog.tsx`.
7. **Protected labels dialog** (`protected-labels-dialog.tsx`): reads `promptSettingsQuery`, a `Textarea` with one label per line (`aria-label="Protected labels"`), `Save` → `$api.useMutation("put", "/api/v1/projects/{project}/prompt-settings")` with `{ protected_labels }` (trimmed, empty lines dropped); field errors under the textarea; success invalidates `promptSettingsKey` and closes. The opening button is disabled below admin with the settings reason.

### Tests (`prompts/index.test.tsx`)

1. `lists the example prompts` — `renderApp("/acme/support-bot/prompts")` shows a grid named `Prompts` with `support-reply` and `summarize`, the `production` chip with a lock, and one `h1` `Prompts`.
2. `filters by search, label and tag` — typing `supp` and choosing label `staging` and tag `support` record the GET query (`q`, `label`, `tag`).
3. `opens a prompt` — clicking `support-reply` navigates to `/acme/support-bot/prompts/support-reply`.
4. `creates a chat prompt` — `?create=1`, fill name `faq-bot`, choose Chat, type contents, submit: the recorded POST body has `type: "chat"`, the two messages, `labels: ["production"]`; the page navigates to `/prompts/faq-bot` (mock the POST answer with the `support-reply` v1 fixture and a GET of `faq-bot`).
5. `maps field errors` — a 400 with `errors: [{field: "template[1].content", message: "…", position: {line: 1, column: 4}}]` shows the message under message 2.
6. `viewer cannot create` — project role `viewer`: `New prompt` is disabled with the viewer reason.
7. `admin edits protected labels` — owner: open the dialog, enter `production\nprod`, save → PUT body `{ protected_labels: ["production", "prod"] }`; member: the button is disabled with the settings reason.
8. `empty project` — `server.use` answers `{ data: [] }`: the empty state texts.

### Done when

`pnpm --dir web exec vitest run src/routes/\$org/\$project/prompts src/lib/prompts.test.ts src/components/shell src/lib/shortcuts.test.ts --maxWorkers=8` passes, then `just check-web`.

---

## V4.2 — The prompt page: template, versions, labels, diff, history

**Commit subject:** `web: add the prompt page with versions, labels and diffs`

### Read first

`web/src/routes/$org/$project/datasets/$datasetId.tsx` (loader with `notFound()`, URL tabs, header, version `Select` with load more, read-only banner); `web/src/components/datasets/versions-table.tsx`; `web/src/components/evaluators/evaluator-history.tsx` (diff rendering, selected vs previous); `web/src/lib/line-diff.ts`; `web/src/components/confirm-dialog.tsx`, `type-to-confirm-dialog.tsx`; `web/src/components/created-by.tsx`; V4.1's files.

### Edit

| File | Change |
|---|---|
| `web/src/routes/$org/$project/prompts/$prompt.tsx`, `$prompt.test.tsx` | new |
| `web/src/components/prompts/prompt-header.tsx` | new |
| `web/src/components/prompts/template-view.tsx` | new (read-only template, variables, placeholders, config) |
| `web/src/components/prompts/save-version-dialog.tsx` | new |
| `web/src/components/prompts/versions-tab.tsx` | new |
| `web/src/components/prompts/label-chip.tsx` | new |
| `web/src/components/prompts/set-label-dialog.tsx` | new |
| `web/src/components/prompts/labels-tab.tsx` | new |
| `web/src/components/prompts/line-diff-view.tsx` | new |

### Steps

1. **Route** `createFileRoute("/$org/$project/prompts/$prompt")`, `validateSearch: parsePromptSearch`, loader `ensureQueryData(promptQuery(org, project, params.prompt))` mapping 404 to `notFound()`.
2. **Header** (`prompt-header.tsx`): `BackLink` to the list, `<h2>` with the name, a `Badge` with the type, `TopBarCrumb`, the version `Select` (`Latest (v{n})` first, then `v{n}` newest first from `promptVersionsInit` with a `Load more versions…` sentinel, like the dataset page), label chips of the shown version (`LabelChip`), the description, and a `Delete prompt` action (`TypeToConfirmDialog` typing the name; disabled below member; a 403 shows the protected reason) that invalidates `promptsKey` and returns to the list. An old version shows the read-only banner `Viewing version N.` with `Go to latest`.
3. **Tabs** (URL `tab`): Template, Versions, Labels, Metrics (Metrics content is V4.3: render `null` there for now, the tab exists).
4. **Template tab**: `TemplateView` shows the shown version's template (text in a `pre`; chat as a list of role-labelled blocks and `Placeholder: name` rows), `Variables` and `Placeholders` (badges), `Config` (`prettyJson`), the commit message and `CreatedBy`. An `Edit` button (disabled below member with the viewer reason) switches to `TemplateEditor` pre-filled with the shown version plus a config textarea; `Cancel` returns; `Save as new version…` opens `SaveVersionDialog`: commit message, the labels to set (checkboxes for the prompt's existing labels and a text input for a new one; protected ones disabled below admin with the protected reason), `Save` → `post_prompt_version`; on success invalidate `promptWriteKeys` and show the new version (search `version` removed). Field errors map onto the editor as in V4.1; a 403 shows the label 403 text. An `UnsavedChangesGuard` protects an edit in progress.
5. **Label chips** (`label-chip.tsx`): `LabelChip({ label, version?, protected, canTouch, onRemove })` renders a `Badge` (`categoryClasses(label).pill`), a `Lock` icon (`aria-label="Protected"`) when protected, and a remove button (`aria-label="Remove label {label}"`) when `onRemove` is given; when `protected && !canTouch` the remove button is disabled inside `DisabledReason` with the protected reason. Removing opens a `ConfirmDialog` (`Remove label {label}?`) → `delete_prompt_label`; invalidate `promptWriteKeys`. `latest` is shown as a plain outline badge without remove.
6. **Versions tab** (`versions-tab.tsx`): a list (`<ol aria-label="Versions">`) of `v{n}` rows, newest first, with labels, commit message, `CreatedBy`, `RelativeTime`, buttons `Show`, `Diff` (selects the row) and `Set label…` (disabled below member). The selected row (default: the shown version) shows `LineDiffView` of the previous version's `templateText` against its own, then of `prettyJson(config)`; version 1 shows `First version: nothing to compare.`; the previous version is fetched with `promptVersionQuery` when it is not in the loaded pages. `Set label…` opens `SetLabelDialog`: a label `Input` (validated with `isValidLabel`, with the prompt's existing labels as suggestions in a `datalist`) → `put_prompt_label` with `{ version }`; a protected label below admin disables `Save` with the protected reason; a 403 shows the label 403 text.
7. **`LineDiffView`** (`line-diff-view.tsx`): `LineDiffView({ before, after, label })` runs `diffLines(before, after)` and renders `<pre aria-label={label}>` lines with `data-op`, `+ `/`- `/`  ` prefixes and the colours of `evaluator-history.tsx`'s private renderer (copy its markup; do not import it).
8. **Labels tab** (`labels-tab.tsx`): the label history from `promptLabelEventsInit` (infinite, `Load more`), each row: label chip, `v{from} → v{to}` (`new → v{to}`, `v{from} → removed`), the actor (`CreatedBy` on `actor`), `RelativeTime(at)`.

### Tests (`$prompt.test.tsx`)

1. `shows the latest version` — `/acme/support-bot/prompts/support-reply`: heading `support-reply`, `Chat` badge, the v2 system text, labels `latest` and `staging`, variables `product`, `language`, `question`, placeholder `history`.
2. `shows an older version` — `?version=1`: the banner, label `production`, `Go to latest` clears `version`.
3. `missing prompt` — `/prompts/nope` (GET answers 404): the not-found page.
4. `saves a new version` — Edit, change the system content, `Save as new version…`, commit message, check `production` (as owner): the POST body has the edited template, `commit_message` and `labels: ["production"]`; after the mocked 201 the page shows the new version.
5. `member cannot set production` — role `member`: in the save dialog the `production` checkbox is disabled with the protected reason.
6. `diff with the previous version` — Versions tab, select v2: the diff shows a removed line with the v1 system text and an added line with the v2 text.
7. `sets and removes a label` — `Set label…` on v1, type `canary` → PUT `/labels/canary` body `{ version: 1 }`; remove `staging` (owner) → DELETE `/labels/staging` after confirming.
8. `label history` — the Labels tab lists the two fixture events.
9. `viewer is read-only` — role `viewer`: Edit, Set label…, Delete are disabled with the viewer reason.

### Done when

`pnpm --dir web exec vitest run src/routes/\$org/\$project/prompts src/components/prompts src/lib/prompts.test.ts --maxWorkers=8`, then `just check-web`.

---

## V4.3 — Metrics tab and links to traces

**Commit subject:** `web: add per-version prompt metrics and trace links`

### Read first

`web/src/lib/format.ts` (`formatDuration` takes nanoseconds, `formatCost`, `formatTokens`), `web/src/lib/metrics.ts` (`formatPercent`, `formatCount`), `web/src/components/filter-bar/logic.ts` (`stringContains`), `web/src/lib/search/traces.ts` (`TracesSearch`, the `range` presets), `web/src/components/ui/table.tsx`, V4.2's page.

### Edit

| File | Change |
|---|---|
| `web/src/components/prompts/metrics-tab.tsx`, `metrics-tab.test.tsx` | new |
| `web/src/routes/$org/$project/prompts/$prompt.tsx` | render `MetricsTab` |
| `web/src/lib/prompts.ts`, `prompts.test.ts` | `metricsWindow`, `tracesLink` helpers |

### Steps

1. `metricsWindow(preset: "24h" | "7d" | "30d", nowMs: number) → { from: string; to: string }` (RFC 3339, `to` = now). `tracesSearchFor(name: string, version: number, preset) → { filter: string; range: preset }` with `filter = stringContains("prompts", \`${name}@${version}\`)`.
2. `MetricsTab({ prompt })`: a window `Select` (`24 hours`, `7 days` default, `30 days`; `aria-label="Window"`), the query `promptMetricsQuery(…, from, to)`, and a `Table` (`aria-label="Metrics by version"`) with columns: Version, Spans (`formatCount`), Traces, Error rate (`formatPercent(error_count / span_count)`), Avg cost (`formatCost`), p50 and p95 (`formatDuration(ms * 1e6)`; `—` when absent), Avg tokens in/out (`formatTokens`), then one column per score name present in any version (sorted; cell `mean.toFixed(2)` and `n = count` in `text-muted-foreground`), and a `Traces` link per row (`Link to="/$org/$project/traces" search={tracesSearchFor(…)}`). Empty answer: `No linked spans in this window. Link spans with context(prompt=…) in the SDK.`
3. The Template tab gains, beside the version, a `Traces` link for the shown version (7 days).

### Tests (`metrics-tab.test.tsx`)

1. `shows the per-version table` — `server.use` answers two versions (`span_count` 4/2, `error_count` 1/0, latencies 120.5/80 ms, scores `quality` on both): the cells show `25%`, `121ms` (or the formatter's exact text: assert with the formatter's own output), the `quality` column.
2. `window changes the query` — choosing `30 days` records a GET whose `from` is 30 days before `to`.
3. `trace links` — the v2 row's link has `range=30d` (after choosing 30 days) and the filter `prompts @> ['support-reply@2']` in its URL search.
4. `empty` — `versions: []` shows the empty text.

### Done when

`pnpm --dir web exec vitest run src/components/prompts src/lib/prompts.test.ts --maxWorkers=8`, then `just check-web`.

---

## V4.4 — Prompt links in traces and spans

**Commit subject:** `web: show prompt links on spans and add prompt facets and columns`

### Read first

`web/src/components/trace/span-header.tsx` (the `Pair` row, the session link) and `span-detail.test.tsx`; `web/src/lib/facets.ts` and `facets.test.ts`; `web/src/lib/search/traces.ts`, `web/src/components/trace-columns.tsx` (`HIDDEN`, `LABELS`, the `models` column with `FilterHit`) and its test; `web/src/lib/search/spans.ts`, `web/src/components/span-columns.tsx` and its test.

### Edit

| File | Change |
|---|---|
| `web/src/components/trace/span-header.tsx`, `span-detail.test.tsx` | the prompt pair |
| `web/src/lib/facets.ts`, `facets.test.ts` | two facets |
| `web/src/lib/search/traces.ts`, `web/src/components/trace-columns.tsx`, `trace-columns.test.tsx` | `prompts` column |
| `web/src/lib/search/spans.ts`, `web/src/components/span-columns.tsx`, `span-columns.test.tsx` | `prompt` column |

### Steps

1. **Span header**: after the session pair, when `span.prompt_name` is set: `<Pair label="prompt"><Link to="/$org/$project/prompts/$prompt" params={{ org, project, prompt: span.prompt_name }} search={span.prompt_version ? { version: span.prompt_version } : {}} className="focus-ring rounded-sm underline-offset-2 hover:underline">{span.prompt_name}{span.prompt_version ? `@${span.prompt_version}` : ""}</Link></Pair>`.
2. **Facets**: traces `{ id: "prompts", field: { column: "prompts" }, label: "Prompts", kind: "list" }` right after `models`; spans `{ id: "prompt_name", field: { column: "prompt_name" }, label: "Prompt", kind: "values" }` right after `model`.
3. **Trace column** `prompts`: add to `TRACE_COLUMN_IDS` after `models`, `LABELS.prompts = "Prompts"`, hidden by default (`HIDDEN`), a column definition copied from `models` (first value plus `+N`, `FilterHit` with `field={{ column: "prompts" }}` and `list`).
4. **Span column** `prompt`: add `"prompt"` to `SPAN_COLUMN_IDS` after `model`, `LABELS.prompt = "Prompt"`, hidden by default, cell `prompt_name` plus `@version` with `FilterHit field={{ column: "prompt_name" }}` on the name.
5. If `mocks/generated/handlers.ts`' facet handler refuses unknown fields, leave it (the dev server answers empty facets); do not change generated mocks.

### Tests

1. `span-detail.test.tsx`: `shows the prompt link` — a span detail answer with `prompt_name: "support-reply"`, `prompt_version: 2` shows a link `support-reply@2` to `/acme/support-bot/prompts/support-reply?version=2`; a name-only span links without `version`.
2. `facets.test.ts`: the two facets exist with the given kinds (adapt to how the file enumerates `FACETS`).
3. `trace-columns.test.tsx`: the `prompts` column is hidden by default and renders `a@1 +1` for `prompts: ["a@1", "b@2"]`.
4. `span-columns.test.tsx`: the `prompt` column renders `support-reply@2`.

### Done when

`pnpm --dir web exec vitest run src/components/trace src/lib/facets.test.ts src/components/trace-columns.test.tsx src/components/span-columns.test.tsx --maxWorkers=8`, then `just check-web`.

---

## V4.5 — Dev mocks, screenshots, docs, spec

**Commit subject:** `web, docs: prompt write mocks, screenshots and the UI spec`

### Read first

`web/src/mocks/writes.ts` and `writes.test.ts`; `website/screenshots.json`, `website/scripts/screenshots.mjs`, `website/scripts/check-screenshots.mjs`; `website/src/content/docs/docs/evaluation/prompts.mdx` (V2.5's page) and `datasets.mdx` (screenshot import); `website/src/content/docs/docs/get-started/ui-tour.mdx`; `docs/spec/11-ui.md`.

### Edit

| File | Change |
|---|---|
| `web/src/mocks/writes.ts`, `writes.test.ts` | prompt write answers |
| `website/screenshots.json`, `website/src/assets/screenshots/prompts.png`, `prompt.png` | two shots |
| `website/src/content/docs/docs/evaluation/prompts.mdx` | a "In the UI" section with both screenshots |
| `website/src/content/docs/docs/get-started/ui-tour.mdx` | Prompts in the Evaluate row, `g then r` in the go-keys table |
| `docs/spec/11-ui.md` | navigation lines and a `### Prompts` page section |
| `docs/mvp-decisions.md`, `docs/plans/v-prompts/00-design.md` | D-V-32 |
| `CHANGELOG.md`, `docs/roadmap.md` | lines |

### Steps

1. **Write mocks**: `writes.test.ts`' `covered` prefixes gain `/api/v1/projects/{project}/prompts` and `/api/v1/projects/{project}/prompt-settings`; `WRITE_OPERATIONS` and `RESPONSES` answer `post_prompt` and `post_prompt_version` with the `support-reply` version fixtures (`fixtures/api/responses/prompts/support-reply/versions/1.json`, `…/2.json`), `patch_prompt` and `put_prompt_label` with `responses/prompts/support-reply.json`, `delete_prompt` and `delete_prompt_label` with 204, `put_prompt_settings` with `responses/prompt-settings.json`, following the file's existing pattern exactly.
2. **Screenshots**: `website/screenshots.json` gains `{ "name": "prompts", "path": "/acme/support-bot/prompts", "waitFor": "support-reply", "now": "2026-01-01T11:00:00Z" }` and `{ "name": "prompt", "path": "/acme/support-bot/prompts/support-reply", "waitFor": "Friendlier tone", "now": "2026-01-01T11:00:00Z" }` (use the `waitFor` form the file's other entries use: a text or a test id); run `just docs-screenshots prompts prompt` (needs Chromium: `pnpm --dir website exec playwright install chromium` if missing) and commit the two PNGs. If Chromium cannot run here, STOP and report (do not commit a shot without its PNG).
3. **Docs page**: in `evaluation/prompts.mdx`, a `## In the UI` section: one paragraph on the list (filters, protected labels), one on the prompt page (edit and save a version, labels, diff, history, metrics), the two images imported as `datasets.mdx` does, with alt texts.
4. **UI tour**: add `[Prompts](/docs/evaluation/prompts/)` first in the Evaluate row, and the `g then r` row.
5. **Spec 11**: navigation lines `/:org/:project/prompts · /prompts/:prompt   prompt registry (track V, D-V-32)`; a `### Prompts` section after Datasets summarizing the list, the dialogs, the prompt page tabs, the span link and the facets, citing D-V-8 to D-V-10, D-V-17, D-V-32.
6. **D-V-32** in `docs/mvp-decisions.md` § Track V and the design's decisions table: "Registry UI. Creation is a dialog on the list (`?create=1`), never a `/prompts/new` route, because `new` is a valid prompt name; the page is `/prompts/:prompt` with tabs `template`, `versions`, `labels`, `metrics` and a `version` search; labels are set from a version row and removed from their chip; the diff compares a version with the previous one; metrics windows are 24 h, 7 d (default) and 30 d, the traces list's presets." Check the number is free first (`grep -n "D-V-32" docs/mvp-decisions.md`).
7. **Changelog** `### Added`: `- Prompts pages (D-V-32): the prompt list with protected labels, the prompt page (edit and save versions, labels, diffs, label history, per-version metrics) and prompt links, facets and columns on traces and spans.` **Roadmap**: the V row lists M4 as implemented.

### Done when

`export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH && just check` passes (web, docs and the rest).
