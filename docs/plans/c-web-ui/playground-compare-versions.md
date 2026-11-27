# Playground: compare prompt versions side by side

When the playground's draft comes from a prompt, each column can be pinned to a version of that
prompt, so `support-reply` v12 and v13 run on the same variables and placeholders, side by side.
A pinned column runs its version's messages, tools, tool choice and response format; it shows the
version's template above its answer, and the usual tokens, cost and latency. A column left on
**Draft** runs the editor's draft, as today. The page also opens a comparison from the URL
(`?prompt=&version=&compare=`). Client only: no server, API or fixture change.

**Prerequisites:** none. Branch `claude/playground-compare-versions`, worktree
`/home/sebastien/travaux/llm-observer/.claude/worktrees/magical-panini-4e0909`.

**Read first:**
- `docs/spec/11-ui.md` § Playground; `docs/spec/09-api.md` § Playground and the prompt rows
  (`GET /prompts/{prompt}/versions`, `GET /prompts/{prompt}/versions/{version}`).
- `docs/mvp-decisions.md` D-V-20 to D-V-28, D-V-38 (playground), D-V-7 (versions).
- `web/src/lib/playground.ts` (whole file), `web/src/lib/playground.test.ts` (style).
- `web/src/routes/$org/$project/playground/index.tsx` (whole file) and its `index.test.tsx`
  (helpers `answer`, `openStream`, `typeIn`, the `VERSION` handler usage).
- `web/src/components/playground/run-column.tsx`, `settings-panel.tsx`.
- `web/src/api/queries-prompts.ts` (`promptVersionsInit`, `promptVersionQuery`).
- `fixtures/api/responses/prompts/support-reply/versions.json`, `versions/1.json`, `versions/2.json`
  (the MSW fixtures the tests use: versions 1 and 2, `production` on 1, `staging` and `latest` on 2).

## What it is, and what it is not

Works when done:
- A draft whose `source` is a prompt shows a **Version** select at the start of each column's
  header: `Draft` (default) and the prompt's versions, newest first (`v2 · latest, staging`).
- Picking a version pins the column: its connection (when the version's config names one of the
  org's connections), model, temperature and max tokens are set from the version's config; they stay
  editable. Picking `Draft` unpins it and keeps the column's settings.
- A pinned column shows a folding **Template of vN** block (open by default) with the version's
  messages (role and text, placeholders as `Placeholder <name>`), then the answer and footer as today.
- A pinned column's run sends the version's messages, tools, tool choice and response format, the
  draft's variables, placeholders and `render_variables`, and `source: {prompt: {name, version: N}}`.
- The Variables and Placeholders lists of the settings panel are the union of the draft's and the
  pinned versions' names, so one set of values feeds every column.
- `?prompt=<name>&version=<v>&compare=<c>` opens version v as the draft with two columns: column 1
  pinned to v, column 2 pinned to c.
- The pinned version survives a reload (stored with the column in the draft).

Not in this plan:
- A "Compare in playground" button on the prompt page (the URL is enough for now; a later UI plan).
- Pinning a version of a different prompt than the draft's source, or pinning when the draft comes
  from a span or a run (the select is hidden).
- Save as new version from a pinned column: the dialog keeps saving the draft (unchanged).
- Any server, OpenAPI, fixture or MSW handler change.

## Decisions this plan relies on

| ID | Summary |
|---|---|
| D-V-28, D-V-38 | Draft in `localStorage` per project; source search keys loaded once then removed from the URL |
| D-V-21 | The run request carries `messages`, `variables`, `placeholders`, `source` (recorded only) |
| D-V-24 | 1–4 columns |
| **D-C-30** (new, phase P3 writes it) | Version compare: a column of a prompt-sourced draft may pin a version of that prompt; a pinned column runs the version's messages, tools, tool choice and response format with the draft's variables, placeholders and render switch, and records the version as the run's source; picking a version applies its config to the column; `compare` search key. Client only. Rejected: a separate "compare" page (duplicates the playground); per-column drafts (four editors, no shared input); a server-side "run version N" request (the client already has the template, and the server renders the same messages). |

No user answers were needed: the task left the UI choice (per-column selector vs. action) to this
session, and nothing here changes the API or data model.

## Versions

No new dependency.

## Phases and dependencies

```
P1 lib (playground.ts + test)
 ├── P2 column and page (run-column.tsx + new test, settings-panel.tsx, route index.tsx + test)
 └── P3 docs (spec 11, decisions, CHANGELOG, website page)   ← may run in parallel with P2
```

P1 → P2 in sequence (P2 uses P1's exports). P2 has two parts (A: the column, B: the page) and
one commit, because the column's new required props and their only caller must change together for
the typecheck to pass. P3 touches only docs files.

Conventions for every phase: TypeScript strict, Biome style as the surrounding code; comments are
full sentences; no `as` casts except where the existing code already does the same thing; tests use
the existing helpers. Check commands are run from the repo root with Node on `PATH`
(`export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH` first). While iterating run only the touched test files:
`cd web && pnpm exec vitest run <paths> --maxWorkers=8`. Don't run `just check` (the coordinator
runs it once at the end). Typecheck with `cd web && pnpm typecheck` once, right before the commit
(not while iterating). If running a command through `rtk` leaves `web/.vitest/output.json`,
delete it before committing.

---

## P1 — lib: pinned columns

**Goal:** the draft model knows pinned versions, and the lib builds a pinned column's draft.

**Edit:** `web/src/lib/playground.ts`, `web/src/lib/playground.test.ts`.

**Steps:**

1. `Column` gains an optional field (after `maxTokens`):
   ```ts
   /**
    * The version of the draft's source prompt this column runs instead of the draft (D-C-30).
    * Absent: the column runs the draft. Ignored unless the draft's source is a prompt.
    */
   version?: number;
   ```
2. `isColumn` also requires `value.version === undefined || isVersionNumber(value.version)`, with
   a new private helper:
   ```ts
   function isVersionNumber(value: unknown): value is number {
     return typeof value === "number" && Number.isSafeInteger(value) && value >= 1;
   }
   ```
3. `draftOf`: after `const source = sourceOf(value.source);`, when `source` is not a prompt source
   (`source === undefined || !("prompt" in source)`), return the columns with `version` removed
   (`columns.map(({ version: _version, ...column }) => column)`); otherwise keep them as they are.
   (A stored column with a malformed `version` already fails `isColumn`, which rejects the whole
   draft, like any other malformed column.)
4. Add, exported, after `draftFromPromptVersion`:
   ```ts
   /** The prompt name of a draft whose source is a prompt, else `undefined`. */
   export function sourcePrompt(draft: Pick<Draft, "source">): string | undefined {
     return draft.source !== undefined && "prompt" in draft.source
       ? draft.source.prompt.name
       : undefined;
   }

   /**
    * The draft a column pinned to `version` runs (D-C-30): the version's messages, tools, tool
    * choice and response format, with the draft's variables, placeholders and render switch. Its
    * source is that version.
    */
   export function pinnedDraft(draft: Draft, version: Schema<"PromptVersion">): Draft {
     const fromVersion = draftFromPromptVersion(version, []);
     return {
       ...draft,
       entries: fromVersion.entries,
       tools: fromVersion.tools,
       toolChoice: fromVersion.toolChoice,
       responseFormat: fromVersion.responseFormat,
       source: { prompt: { name: version.name, version: version.version } },
     };
   }

   /**
    * The draft a column runs: the page's draft, or its pinned version's draft. `undefined` while
    * the pinned version is not loaded (`versionOf` gives `undefined`).
    */
   export function columnDraft(
     draft: Draft,
     column: Column,
     versionOf: (version: number) => Schema<"PromptVersion"> | undefined,
   ): Draft | undefined {
     if (column.version === undefined || sourcePrompt(draft) === undefined) {
       return draft;
     }
     const found = versionOf(column.version);
     return found === undefined ? undefined : pinnedDraft(draft, found);
   }

   /**
    * A column pinned to `version`, with the version's config: its connection when the config names
    * one of `connectionIds` (else the column's own), its model (`""` when none) and its temperature
    * and max tokens (`""` when unset).
    */
   export function pinColumn(
     column: Column,
     version: Schema<"PromptVersion">,
     connectionIds: readonly string[],
   ): Column
   ```
   `pinColumn` body: read `config`, `params` exactly as `draftFromPromptVersion` does (reuse
   `isRecord` and `numberText`); `connectionId` = `config.connection_id` when it is a string in
   `connectionIds`, else `column.connectionId`; return
   `{ ...column, version: version.version, connectionId, model, temperature, maxTokens }`.
5. Add, exported:
   ```ts
   /**
    * The variables and placeholders the settings panel lists: the draft's, then those of each
    * pinned draft, each name once in order of first use (D-C-30).
    */
   export function sharedInputs(
     draft: Draft,
     pinned: readonly Draft[],
   ): { variables: string[]; placeholders: string[] }
   ```
   Body: concatenate `draftVariables(d)` and `placeholderNames(d.entries)` over `[draft, ...pinned]`,
   dropping repeats (keep the first).

**Tests** (in `playground.test.ts`, a new `describe("version compare", …)`; build versions from the
`versions/2.json` fixture, imported the way the route test imports it, or a literal of the same
shape):
- `pinnedDraft takes the version's messages and settings and keeps the draft's inputs`: draft with
  variables `{question: "q"}`, placeholders `{history: "[]"}`, `renderVariables: false`, tools set →
  result has the version's entries (3, the second a placeholder `history`), the version's tools
  (`""` for the fixture), the draft's variables/placeholders/renderVariables, and
  `source = {prompt: {name: "support-reply", version: 2}}`.
- `columnDraft returns the draft for an unpinned column`.
- `columnDraft returns the draft when the source is not a prompt` (span source, column pinned).
- `columnDraft is undefined while the version is not loaded`.
- `columnDraft builds the pinned draft` (and `toRunRequest` on it sends the version's three entries
  and `source.prompt.version === 2`).
- `pinColumn applies the version's config`: model `gpt-4o`, temperature `"0.3"`, maxTokens `"512"`,
  `version: 2`, id kept.
- `pinColumn keeps the column's connection when the config names none or an unknown one`, and
  `takes the config's connection when it is known` (version with `config.connection_id` set).
- `pinColumn clears the model and parameters the version does not set`.
- `sharedInputs lists the draft's names then the pinned ones, once each`: draft with `{{ a }}` and
  `{{ question }}` plus a pinned draft with `product, language, question` and placeholder `history`
  → variables `[a, question, product, language]`, placeholders `[history]`.
- `sharedInputs lists no variables of a draft that does not render them`.
- `loadDraft keeps a pinned column of a prompt-sourced draft` (save then load → `version: 2`).
- `loadDraft drops the pin when the source is not a prompt` (span source → no `version` key).
- `loadDraft rejects a column with a malformed version` (`0`, `1.5`, `"2"` → `undefined`).

**Done when:** `cd web && pnpm exec vitest run src/lib/playground.test.ts --maxWorkers=8` passes,
`cd web && pnpm typecheck` passes,
`pnpm exec biome check src/lib/playground.ts src/lib/playground.test.ts` passes (run from `web/`).

**Commit:** `Pin prompt versions to playground columns in the draft model`

---

## P2 — column and page: pinned runs, errors and the compare URL

**Goal:** a column shows the version select and the pinned version's template; the page loads
versions, runs pinned columns with their version, routes their errors and opens `?compare=`.

**Edit:** `web/src/components/playground/run-column.tsx`,
`web/src/components/playground/run-column-version.test.tsx` (new),
`web/src/components/playground/settings-panel.tsx`,
`web/src/routes/$org/$project/playground/index.tsx`,
`web/src/routes/$org/$project/playground/index.test.tsx`.

### Part A — the column

**Steps:**

1. Export a type and extend `RunColumnProps`:
   ```ts
   /** A version the column's select offers. */
   export type VersionOption = { version: number; labels: readonly string[] };

   // in RunColumnProps:
   /** The source prompt's versions, newest first; `undefined` hides the select (no prompt source). */
   versions: readonly VersionOption[] | undefined;
   /** The pinned version's messages, once loaded; `undefined` for a draft column or while loading. */
   template: readonly Entry[] | undefined;
   /** Pins the column to a version, or back to the draft (`undefined`). */
   onPickVersion: (id: string, version: number | undefined) => void;
   ```
   (Add `Entry` to the existing `import type { Column, Message, RunEvent } from "@/lib/playground"`.)
2. Export a pure helper and test it:
   ```ts
   /** The select's text for a version: `v2`, or `v2 · latest, staging`. */
   export function versionLabel(option: VersionOption): string
   ```
   Labels joined with `", "` after ` · `, in the given order.
3. In the header, before the connection `Select`, when `versions !== undefined`, render a
   `Select` with `aria-label="Prompt version"`, trigger class `w-44 min-w-0 shrink-0`, value
   `column.version === undefined ? "draft" : String(column.version)`. Items: `draft` → `Draft`, then
   one per option (`value={String(option.version)}`, text `versionLabel(option)`). When
   `column.version` is set and not in `versions`, add one item for it (text `v<N>`) after the list,
   so the trigger always shows the value. `onValueChange`: `"draft"` → `onPickVersion(id, undefined)`,
   else `onPickVersion(id, Number(value))`.
4. At the top of the scrolling body (before the "Add an LLM connection" line), when
   `template !== undefined && column.version !== undefined`, render:
   ```tsx
   <details open className="rounded-md border bg-muted/40">
     <summary className="cursor-pointer px-2 py-1 text-caption text-muted-foreground">
       Template of v{column.version}
     </summary>
     <ol aria-label={`Template of version ${column.version}`} className="flex flex-col gap-2 px-2 pb-2">
       {/* one <li> per entry */}
     </ol>
   </details>
   ```
   Each `<li>` (key = index; add the same `biome-ignore lint/suspicious/noArrayIndexKey` comment as
   the tool calls with reason "the template is fixed for the pinned version"): a placeholder entry
   renders `<p className="text-caption text-muted-foreground">Placeholder {name}</p>`; a message
   renders `<p className="text-caption text-muted-foreground">{role}</p>` then
   `<pre className="font-mono text-body break-words whitespace-pre-wrap">{text}</pre>`, where
   `text` is its `text` parts' `content` joined with `"\n"` (other parts are skipped; prompt
   versions hold text only). Use `"type" in entry && entry.type === "placeholder"` to tell them
   apart (the same test as `isPlaceholder` in `message-editor.tsx`; import that one instead).
5. The "Run to see the answer" hint stays as it is (it shows under the template).

**Tests** (`run-column-version.test.tsx`; render `RunColumn` alone, mirroring
`web/src/components/datasets/export-menu.test.tsx`: `render(<ProjectProvider value={{ org: "acme",
project: "support-bot", ref: "acme/support-bot", data: projectFixture }}><RunColumn {...props()} /></ProjectProvider>)`
with `projectFixture` from `fixtures/api/static/project.json` (no QueryClient or router is needed:
`Link` renders only with a `traceId` or with no connections). Write a `props(overrides)` factory
returning every `RunColumnProps` field: `number: 1`, a column
`{ id: "c1", connectionId: CONNECTION, model: "", temperature: "", maxTokens: "" }`, `run: IDLE_RUN`,
`connections: [{ ...one connection object shaped like Schema<"LlmConnection"> with id CONNECTION }]`
(take it from `fixtures/api/static/llm-connections.json` or the file the route test's connection id
comes from; grep `000000000400` in `fixtures/api`), `errors: {}`, `runReason: undefined`,
`shortcut: "Ctrl+Enter"`, `canRemove: false`, `versions: [{version: 2, labels: ["latest","staging"]}, {version: 1, labels: ["production"]}]`,
`template: undefined`, and `vi.fn()` for every callback):
- `versionLabel` gives `v1` without labels and `v2 · latest, staging` with them.
- `hides the version select without versions` (`versions={undefined}` → no combobox named
  "Prompt version").
- `shows Draft for an unpinned column and the version for a pinned one`.
- `shows a pinned version missing from the list` (`version: 7`, options `[2, 1]` → trigger text `v7`).
- `picking a version calls onPickVersion with its number, and Draft with undefined` (open the
  select with `user.click` on the trigger, pick the option by role `option`).
- `shows the pinned template` (entries: system text, placeholder `history`, user text → the list
  named "Template of version 2" has 3 items, with `Placeholder history`).
- `shows no template for a draft column`.

### Part B — the page

**Steps:**

1. **Settings panel.** `SettingsPanelProps` gains `variables: readonly string[]` and
   `placeholders: readonly string[]` (doc: "The names to list: the draft's and the pinned
   versions', D-C-30"). `SettingsPanel` uses them instead of computing `draftVariables(draft)` and
   `placeholderNamesOf(draft.entries)`; remove the imports that become unused. The Placeholders
   section still shows only when the list is not empty. Nothing else changes in the panel.
2. **Search.** `PlaygroundSearch` gains `compare?: number`, parsed with `searchVersion`, and only
   when `prompt` is defined (else `undefined`, so a lone `compare` never sticks in the URL).
   `urlSourceKey` gains a `compare` argument; the prompt key becomes
   `` `prompt:${prompt}@${version ?? ""}~${compare ?? ""}` ``. The source effect passes `compare`
   to `loadPrompt` and its `navigate` also sets `compare: undefined`; add `compare` to the effect's
   dependency list.
3. **`loadPrompt`** gains a `compare: number | undefined` parameter (after `version`). Refactor so
   the loaded `Schema<"PromptVersion">` (the asked version or the latest) is kept in a local `found`,
   and `draft = draftFromPromptVersion(found, connectionIds)`. Then, when `compare !== undefined`
   and `compare !== found.version`:
   - fetch `promptVersionQuery(org, project, name, compare)`; on a 404 problem, return the draft
     unchanged (no second column); rethrow any other error;
   - otherwise (this also holds when the asked `version` was missing and `found` is the latest) set
     `draft.columns` to
     `[pinColumn(first, found, connectionIds), pinColumn({ ...first, id: uuidv7() }, other, connectionIds)]`
     where `first` is `draft.columns[0]` (it exists: `draftFromPromptVersion` makes one column;
     keep the existing no-`!` style — if `first` is `undefined`, return the draft unchanged).
4. **Version data.**
   - `const promptName = sourcePrompt(draft);` (rename the existing destructured `promptName` from
     `search` to `searchPrompt` to avoid the clash, and update its uses).
   - The version list: `$api.useQuery("get", "/api/v1/projects/{project}/prompts/{prompt}/versions",
     promptVersionsInit(org, project, promptName ?? ""), { enabled: promptName !== undefined })`.
     `versionOptions: VersionOption[] | undefined` = `undefined` when `promptName === undefined`,
     else `(list.data?.data ?? []).map((item) => ({ version: item.version, labels: item.labels }))`
     (a failed or loading list gives `[]`: the select still shows `Draft` and the pinned values).
     Memoize it with `useStable`.
   - The pinned numbers: `pinnedNumbers` = the distinct `column.version`s of `draft.columns` that
     are defined, when `promptName !== undefined`, else `[]`.
   - `const pinnedQueries = useQueries({ queries: pinnedNumbers.map((n) => promptVersionQuery(org, project, promptName ?? "", n)) });`
     with each query given `staleTime: Number.POSITIVE_INFINITY` and `retry: false` (versions never
     change; a deleted one should fail fast), spread as `{ ...promptVersionQuery(…), staleTime, retry }`,
     and `versionOf = (n: number) => pinnedQueries[pinnedNumbers.indexOf(n)]?.data`;
     `versionFailed = (n: number) => pinnedQueries[pinnedNumbers.indexOf(n)]?.isError === true`.
     (Import `useQueries` from `@tanstack/react-query`.)
5. **Per-column drafts and checks.** They must stay memoized (streamed chunks re-render the page;
   a new `template` array each render would defeat `RunColumn`'s `memo`). Define at module level
   `type Check = ReturnType<typeof toRunRequest>;` and `const NOT_LOADED: Check = { ok: false, errors: {} };`.
   In the component:
   ```ts
   // Changes when a pinned version loads or fails, so the memo below sees new version data.
   const versionsKey = pinnedQueries.map((query) => `${query.dataUpdatedAt}:${query.status}`).join(",");
   const { columnDrafts, checks } = useMemo(() => {
     const drafts = draft.columns.map((column) => columnDraft(draft, column, versionOf));
     return {
       columnDrafts: drafts,
       checks: draft.columns.map((column, index) => {
         const target = drafts[index];
         return target === undefined ? NOT_LOADED : toRunRequest(target, column, connections);
       }),
     };
   }, [draft, connections, versionsKey]);
   ```
   `versionOf` reads `pinnedQueries`, which is not stable: add a
   `// biome-ignore lint/correctness/useExhaustiveDependencies: versionsKey stands for the pinned version data`
   comment on the `useMemo` line if Biome asks for it (and only then). A column is **pinned** when `promptName !== undefined && column.version !== undefined`;
   build `const pinnedIds = new Set(...)` of their ids.
6. **Run reasons.** Keep `runReason` (the draft's) and add, per column:
   ```ts
   function columnRunReason(column: Column): string | undefined
   ```
   (inside the component): not pinned → `runReason`. Pinned → `VIEWER_REASON` when `!canRun`,
   `NO_CONNECTION_REASON` when `connections.length === 0`, `` `Could not load version ${n}.` `` when
   `versionFailed(n)`, `` `Loading version ${n}…` `` when `versionOf(n) === undefined`, else
   `undefined`. A pinned column ignores the draft's blocked messages. Pass
   `runReason={columnRunReason(column)}` to each `RunColumn`. `runAllReason` = the first column's
   reason when every column has one, else `undefined`; the Run all button, its `DisabledReason`, and
   the `Ctrl/Cmd+Enter` shortcut use `runAllReason` instead of `runReason`. `runAll` starts only the
   columns whose `columnRunReason` is `undefined`.
7. **startRun** builds the request from the column's draft: `const target = column === undefined ? undefined : columnDraft(current, column, (n) => queryClient.getQueryData<Schema<"PromptVersion">>(promptVersionQuery(org, project, sourcePrompt(current) ?? "", n).queryKey));`
   then `toRunRequest(target, column, connections)` when `target` is defined (otherwise return,
   like a failed check). Add `org` and `project` to its dependency list.
8. **Error routing** (the block after `// Problems:`). Let `isShared = (field) => /^(variables|placeholders)(\.|$)/.test(field)`.
   - `runErrorList` keeps every run's errors for the settings merge, but the editor problems loop and
     `formError` use only the runs of columns not in `pinnedIds` (iterate `Object.entries(runs)`
     and skip pinned ids).
   - `settingsErrors`: merge the unpinned checks' and runs' errors with the existing
     `SETTINGS_FIELD` test, plus the pinned columns' checks' and runs' errors filtered by `isShared`.
   - `columnErrors` for a pinned column: `mergeFirst([check errors, run errors], (field) => !isShared(field))`
     (its message, tools and response-format errors stay in the column); unpinned columns keep the
     current filter.
9. **Picking a version.**
   ```ts
   const pickVersion = useCallback((id: string, version: number | undefined) => { … }, [...]);
   ```
   - First, in both cases, clear the column's old run (its answer belongs to the other version),
     exactly as `removeColumn` does for its run: abort and delete `controllers.current.get(id)`,
     delete `pendingText.current` for `id`, and remove `runs[id]` with `setRuns`.
   - `undefined`: `updateColumn(id, { version: undefined })`.
   - a number: `updateColumn(id, { version })` at once; then
     `void queryClient.fetchQuery(promptVersionQuery(org, project, name, version))` where `name` is
     `sourcePrompt(draftRef.current)` (return early if `undefined`), and on success
     `editDraft((previous) => ({ ...previous, columns: previous.columns.map((column) => column.id === id && column.version === version ? pinColumn(column, found, connections.map((item) => item.id)) : column) }))`.
     A failure is ignored here (the column shows "Could not load version N." from step 6).
   Pass `onPickVersion={pickVersion}`, `versions={versionOptions}` and
   `template={pinnedIds.has(column.id) && column.version !== undefined ? columnDrafts[index]?.entries : undefined}`
   to each `RunColumn` (for a loading version `columnDrafts[index]` is `undefined`, so no template).
10. **Settings panel names.** `const inputs = useStable(sharedInputs(draft, pinned drafts))` where the
    pinned drafts are the defined `columnDrafts` of pinned columns; pass `variables={inputs.variables}`
    and `placeholders={inputs.placeholders}`.
11. **Loading a new source:** loaders return unpinned columns (except the `compare` case), and
    `replaceWith` replaces the whole draft. `isBlankDraft` adds `column.version === undefined` to its
    `columns.every` condition, so a draft with a pin asks before it is replaced.
12. **Unchanged on purpose** (D-C-30): "Add answer to conversation" on a pinned column appends the
    answer to the draft's messages, as on any column; Save as new version saves the draft's messages
    and settings with column 1's config, pinned or not.

**Tests** (`index.test.tsx`). The default MSW fixtures serve `support-reply` versions 1 and 2 and the
version list: for these tests **do not** copy the existing `server.use(http.get(VERSION, … versionTwoFixture))`
override (it answers v2 for every version); when a test must change one version, branch on
`params.version` and fall through to the fixture file for the other. Open the page at
`${PAGE}?prompt=support-reply&version=2`. Scope per-column queries with
`within(screen.getByRole("region", { name: "Model 2" }))` (both columns have a "Prompt version"
combobox and a "Model" textbox). Capture run bodies with `server.use(http.post(RUNS, …))`; extend
`RunBody` with `placeholders: Record<string, unknown>` and `source?: { prompt?: { name: string; version: number } }`,
and answer differently per column by branching on `body.source?.prompt?.version`. The select's
labels come after the version list loads: use `findBy…`. Seed drafts with `saveDraft(PROJECT_KEY, draft)`
before rendering, where `draft` is a full `Draft` literal: e.g. for the blocked test
`{ entries: [{ role: "user", parts: [{ type: "uri", uri: "https://example.com/a.png", modality: "image" }] }], variables: {}, placeholders: {}, tools: "", toolChoice: "", responseFormat: "", columns: [{ id: "c1", connectionId: CONNECTION_ID, model: "", temperature: "", maxTokens: "" }, { id: "c2", connectionId: CONNECTION_ID, model: "", temperature: "", maxTokens: "", version: 1 }], source: { prompt: { name: "support-reply", version: 2 } }, renderVariables: true }`
(open `${PAGE}` with no search for seeded drafts):
- `a prompt draft shows the version select with Draft and the versions` (options `Draft`,
  `v2 · latest, staging`, `v1 · production`; check the fixture's label order and use it).
- `a span draft shows no version select`.
- `a pinned column shows the version's template and runs its messages`: add a column, pick `v1` in
  column 2, see "Template of version 1", Run all → two bodies; the pinned body's `messages` equal
  version 1's template entries as draft messages and `source` is `{prompt: {name: "support-reply", version: 1}}`;
  the draft column's body has `source.prompt.version === 2`. Type values into the settings inputs
  named `product` and `question` first; both bodies carry those two values (the v2 body also has
  `language`).
- `picking a version applies its config to the column`: both fixture versions have the same
  config, so override the version-1 handler with `server.use` to answer `versions/1.json` with
  `config: {model: "gpt-4o-mini", params: {temperature: 0.9, max_tokens: 256}}` → the model field
  shows `gpt-4o-mini` and the Parameters button `temp 0.9 · max 256`.
- `both columns show their tokens, cost and latency` (two `done` answers with different usage →
  each column region shows its own input tokens and cost).
- `the settings list the pinned version's variables`: a draft edited to drop `{{ product }}` (or
  start from an empty draft with a prompt source written by `saveDraft`) still lists `product` when a
  column is pinned to a version that uses it.
- `a pinned column's message error stays in the column`: the pinned run answers 400 with a
  `messages[0]` field error → the text shows inside column 2, not under the editor's first message.
- `a pinned column runs while the draft has a blocked message` (draft with a media part saved by
  `saveDraft`, prompt source, column pinned → its Run is enabled, the draft column's is disabled).
- `a pinned version that fails to load disables its Run with the reason` (version handler answers
  500 for that version).
- `the pin survives a reload` (pin, unmount, render again → `findByRole` the column-2 select showing
  `v1 · production`).
- `changing a column's version clears its answer` (run, see the answer, pick another version → the
  answer text is gone and "Run to see the answer" shows).
- `a pinned draft asks before a new source replaces it` (seeded draft with blank text and one column
  pinned, then open `?prompt=support-reply&version=1` → the "Replace the current draft?" dialog).
- `?compare= opens two pinned columns`: `${PAGE}?prompt=support-reply&version=2&compare=1` → column
  1 shows `v2 …`, column 2 `v1 …`, and the URL no longer has `compare`.
- `?compare= with a missing version opens the version alone` (version 9 → 404 → one column).

**Done when:** `cd web && pnpm exec vitest run src/routes/\$org/\$project/playground src/components/playground src/lib/playground.test.ts --maxWorkers=8`
passes; typecheck passes; `biome check` on the edited files passes.

**Done when:** also `cd web && pnpm exec vitest run src/components/playground --maxWorkers=8`
passes (it is in the path list above).

**Commit:** `Compare prompt versions side by side in the playground`

---

## P3 — docs

**Goal:** spec, decision, changelog and user docs describe version compare.

**Edit:** `docs/spec/11-ui.md`, `docs/mvp-decisions.md`, `CHANGELOG.md`,
`website/src/content/docs/docs/evaluation/playground.mdx`.

**Steps:**

1. `docs/spec/11-ui.md` § Playground:
   - first paragraph: the search may also hold `compare` (a version number) with `prompt`; it opens
     two columns pinned to `version` and `compare` (D-C-30);
   - **Columns.** bullet: add, after the first sentence, "When the draft comes from a prompt, the
     header starts with a version select (`Draft`, then the prompt's versions newest first with their
     labels, from the first page of the version list); a pinned column runs that version's messages,
     tools, tool choice and response format with the draft's variables and placeholders, records the
     version as its source, and shows the version's template in a folding block above the answer.
     Picking a version sets the column's connection (when known), model and parameters from the
     version's config (D-C-30)."
   - **Draft.** bullet: the Variables and Placeholders lists also hold the pinned versions' names.
   - **Save as new version.** bullet: add "It saves the draft's messages and settings with the first
     column's config, pinned or not."
   - `Decisions:` line: add D-C-30.
   - Row 31 of the route table (`/:org/:project/playground`): append ", compares prompt versions
     (D-C-30)".
2. `docs/mvp-decisions.md`: add the D-C-30 row after D-C-29 in the C table, using the decision text
   of this plan's table (bold summary sentence first, like the neighbouring rows), and say it extends
   D-V-21 and D-V-38. No roadmap row changes (track C's status does not change).
3. `CHANGELOG.md` § `[Unreleased]` → `### Added`: "The playground compares versions of a prompt side
   by side: each column of a draft opened from a prompt can run one of its versions, with the
   version's template, answer, tokens, cost and latency; `?compare=` opens two versions."
4. `website/src/content/docs/docs/evaluation/playground.mdx`: add a section
   `## Compare prompt versions` after "Models side by side": a draft opened from a prompt has a
   **Version** select in each column; `Draft` runs the messages you edit, a version runs that
   version's messages, tools and response format with the same variables and placeholders; picking
   a version sets the column's model and parameters from the version; the column shows the
   version's template above its answer; the settings panel's tools, tool choice and response format
   apply to `Draft` columns only (a version brings its own); **Add answer to conversation** adds to
   the messages you edit; changing a column's version clears its answer; to compare two versions, open the prompt, add a column and
   pick the other version, or open `/<org>/<project>/playground?prompt=<name>&version=<a>&compare=<b>`.
   Follow `website/STYLE.md` (short sentences, no banned terms). No code block (or mark it per
   `check-samples.mjs` if one is added).

**Done when:** `cd website && node scripts/check-terms.mjs` passes (or the equivalent invocation
`just check-docs` uses for that script; check the justfile), and the files read correctly.

**Commit:** `Document playground version compare`
