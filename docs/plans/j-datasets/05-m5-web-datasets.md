# J · M5 — Web: datasets

Implementation plan for milestone M5 of track J. It has 5 sub-phases, J5.1 to J5.5. **Haiku** implements each one in one go, with no context beyond this plan and the files it names. Sub-phases run in order, one commit each, and each ends with `just check` green. Sonnet reviews the milestone after J5.5.

M5 builds the dataset pages on the API of M1 and M2: the datasets list, the dataset page (items, versions, settings), item editing in a side sheet, add item, the import wizard, export, and "Add to dataset" in the trace view. The experiments pages, the dataset page's Experiments tab and the closeout are M6.

## Prerequisites

Check them before you start. If one fails, STOP and report.

- M1 and M2 are merged on `main`. These commands each print at least one line:

```bash
grep -n '"/api/v1/projects/{project}/datasets/{dataset}/import/preview"' crates/server/openapi.json
```

```bash
grep -n '"/api/v1/projects/{project}/datasets/{dataset}/items/from-trace"' crates/server/openapi.json
```

```bash
grep -n 'datasets' fixtures/api/manifest.json
```

- `docs/plans/j-datasets/00-design.md` exists and lists D-J-1 to D-J-19.
- Node `26.13.0` is installed at `$HOME/.local/share/nvm/v26.13.0/bin`. It is not on the non-interactive `PATH`; export it first (§ Commands).

## Read first (every sub-phase)

- `AGENTS.md`, all of it.
- `docs/plans/j-datasets/00-design.md`: § Decisions (D-J-2 to D-J-8, D-J-17), § API contract, § Fixed identifiers, § Conventions.
- This file up to § J5.1, then your own sub-phase.
- `docs/spec/11-ui.md` § Datasets.
- The web conventions the existing pages follow: `web/src/routes/$org/$project/online-rules/index.tsx` and its test (list page, infinite query, empty state, `DisabledReason`), `web/src/routes/$org/$project/online-rules/$ruleId/index.tsx` (detail page loader with `notFound()`), `web/src/routes/$org/$project/settings/score-definitions.tsx` (dialog form, `ConfirmDialog`, field errors), `web/src/api/queries-online-rules.ts` (query helpers), `web/src/api/problem.ts` (`problemOf`, `fieldErrors`, `problemMessage`).

## Operation ids

M1 and M2 named the operations. Print them once and use those names everywhere this plan says "the operation id of …":

```bash
jq -r '.paths | to_entries[] | select(.key | contains("/datasets")) | .key as $p | .value | to_entries[] | "\(.key) \($p) \(.value.operationId)"' crates/server/openapi.json
```

## Fixtures

M1 and M2 added the example dataset to `fixtures/api` (design § Fixed identifiers: the dataset `faq` is `01900000-0000-7000-8000-000000000c01`, its items are `…0d01`–`…0d03`). Find the files:

```bash
grep -n '"file"' fixtures/api/manifest.json | grep -i dataset
```

```bash
ls fixtures/api/responses/datasets fixtures/api/static | grep -i dataset
```

M1 generates the reads under `fixtures/api/responses/datasets.json` and `fixtures/api/responses/datasets/<id>/…` (the dataset, `versions.json`, `items.json`, `items-v1.json`, `items/<item id>.json`); M2 adds `fixtures/api/static/dataset-import-preview.json`, `dataset-import-result.json` and `dataset-item-from-trace.json`. Operation ids (M1, M2): `list_datasets`, `post_dataset`, `get_dataset`, `patch_dataset`, `delete_dataset`, `list_dataset_versions`, `list_dataset_items`, `post_dataset_items`, `get_dataset_item`, `patch_dataset_item`, `delete_dataset_item`, `post_dataset_import_preview`, `post_dataset_import`, `get_dataset_export`, `post_dataset_item_from_trace`; the `jq` command above is authoritative.

Page tests import these files (as the online-rules test imports `fixtures/api/static/online-rules.json`) and assert on values read from them, not on literals, except where this plan gives a literal. The GET examples are served by the default MSW handlers (`web/src/mocks/handlers.ts`) once they are in the manifest; writes are never served by default in tests, so each test that writes installs its own `server.use(http.post(…))`.

In this plan, `DATASET_ID` is `01900000-0000-7000-8000-000000000c01` and the dataset page URL in tests is `/acme/support-bot/datasets/01900000-0000-7000-8000-000000000c01`.

## Conventions for every sub-phase

- **API calls** go through `$api` (`web/src/api/client.ts`) and the generated types (`Schema<"…">` from `@/api/types`), except the two raw uploads of J5.4, which go through `api.POST` with a `bodySerializer` (§ J5.4).
- **Query helpers** live in `web/src/api/queries-datasets.ts`, in the style of `queries-online-rules.ts`: an `…Init` function for the request init, a `…Query` for `queryOptions`, and a `…Key` for invalidation.
- **Permissions**: `canManageDatasets(role)` (J5.1) mirrors `ProjectPerm::ManageDatasets` (member and above, D-J-5). Every write control is disabled below member and wrapped in `DisabledReason` with the reason given in its step. Reads need nothing.
- **Errors**: a `400` with `errors` shows each message under its field (`fieldErrors`); any other problem shows `problemMessage(error, "<fallback>")` in a `FormError`. The fallbacks are given in each step.
- **JSON fields** are edited in a monospace `Textarea` (`components/ui/textarea.tsx`, `className="font-mono text-xs"`, `spellCheck={false}`), parsed with `parseJsonField` (J5.1). No new dependency (D-J-17).
- **Tests**: `renderApp(url)` from `@/test/render`; MSW overrides with `server.use`; never `vi.mock`. Every new page and component gets a test file next to it. `afterEach` fails a test with an unhandled request, so a test that triggers a request installs its handler.
- **Privacy**: nothing here logs.
- No new npm dependency in M5.

## Parallel work

M5 runs alongside M4 (only `sdks/`) and M6 (web). M6 also edits `components/shell/nav.ts`, `mocks/writes.ts` and `mocks/writes.test.ts`, and adds the Experiments tab to `routes/$org/$project/datasets/$datasetId.tsx` (J6.2). If M6 merged first, rebase and keep both nav entries and both `writes.ts` blocks.

## Final layout after M5

```
web/src/api/queries-datasets.ts                         (J5.1)
web/src/lib/datasets.ts, datasets.test.ts               (J5.1; J5.4 adds the mapping helpers)
web/src/lib/roles.ts                                    + canManageDatasets (J5.1)
web/src/components/shell/nav.ts                         + Datasets (J5.2)
web/src/mocks/writes.ts, writes.test.ts                 + the dataset writes (J5.1)
web/src/routes/$org/$project/datasets/index.tsx         list (J5.2)
web/src/components/datasets/dataset-dialog.tsx          create (J5.2), edit in settings (J5.3)
web/src/routes/$org/$project/datasets/$datasetId.tsx    dataset page (J5.3)
web/src/components/datasets/items-table.tsx             (J5.3)
web/src/components/datasets/item-sheet.tsx              view, edit, delete (J5.3)
web/src/components/datasets/add-item-dialog.tsx         (J5.3)
web/src/components/datasets/versions-table.tsx          (J5.3)
web/src/components/datasets/dataset-settings.tsx        (J5.3)
web/src/components/datasets/import-dialog.tsx           (J5.4)
web/src/components/datasets/export-menu.tsx             (J5.4)
web/src/components/datasets/add-to-dataset-dialog.tsx   (J5.5)
web/src/components/trace/trace-view.tsx                 + the button (J5.5)
```

---

## J5.1 — Query helpers, role, write mocks, JSON helpers

**Commit subject:** `web: add dataset query helpers and mocks`

### Read first

- `web/src/api/queries-online-rules.ts`, `web/src/api/project-ref.ts`, `web/src/api/client.ts`.
- `web/src/lib/roles.ts` and `roles.test.ts`.
- `web/src/mocks/writes.ts` (all of it) and `writes.test.ts` (the `covered` list and the `LATER_TRACKS` array).

### Files

| File | Change |
|---|---|
| `web/src/api/queries-datasets.ts` | new |
| `web/src/lib/datasets.ts` | new |
| `web/src/lib/datasets.test.ts` | new |
| `web/src/lib/roles.ts`, `roles.test.ts` | `canManageDatasets` |
| `web/src/mocks/writes.ts`, `writes.test.ts` | dataset writes |

### Steps

**1. `queries-datasets.ts`.** Import `$api` from `./client`, `projectRef` from `./project-ref`. Export:

```ts
export function datasetsInit(org: string, project: string, q?: string) {
  return {
    params: {
      path: { project: projectRef(org, project) },
      query: { limit: 100, ...(q ? { q } : {}) },
    },
  };
}

/** Prefix of every dataset list query of the project, for invalidation. */
export function datasetsKey(org: string, project: string) { /* as onlineRulesKey, path …/datasets */ }

export function datasetQuery(org: string, project: string, dataset: string) {
  return $api.queryOptions("get", "/api/v1/projects/{project}/datasets/{dataset}", {
    params: { path: { project: projectRef(org, project), dataset } },
  });
}

export function datasetKey(org: string, project: string, dataset: string) {
  return datasetQuery(org, project, dataset).queryKey;
}

/** One page request of a version's items; `version` undefined reads the latest. */
export function datasetItemsInit(org: string, project: string, dataset: string, version?: number) {
  return {
    params: {
      path: { project: projectRef(org, project), dataset },
      query: { limit: 100, ...(version !== undefined ? { version } : {}) },
    },
  };
}

/** Prefix of every item list of one dataset, for invalidation. */
export function datasetItemsKey(org: string, project: string, dataset: string) { /* path …/datasets/{dataset}/items, params path only */ }

export function datasetVersionsInit(org: string, project: string, dataset: string) {
  return { params: { path: { project: projectRef(org, project), dataset }, query: { limit: 100 } } };
}

export function datasetVersionsKey(org: string, project: string, dataset: string) { /* path …/versions, params path only */ }

/** Every key a dataset write makes stale: the list, the dataset, its items and its versions. */
export function datasetWriteKeys(org: string, project: string, dataset: string) {
  return [
    datasetsKey(org, project),
    datasetKey(org, project, dataset),
    datasetItemsKey(org, project, dataset),
    datasetVersionsKey(org, project, dataset),
  ];
}
```

The `…Key` functions follow `onlineRulesKey`: `$api.queryOptions("get", <path>, { params: { path: {…} } }).queryKey`. If a path or query parameter has another name in the generated `schema.gen.ts` (`grep -n "datasets/{dataset}" web/src/api/schema.gen.ts`), STOP and report.

**2. `lib/datasets.ts`.** Pure functions, no React:

```ts
export type JsonParse = { ok: true; value: unknown } | { ok: false; message: string };

/**
 * Parses a JSON textarea. Empty (after trimming): `{ ok: true, value: undefined }` when not
 * required, else `{ ok: false, message: "Required" }`. Invalid: "Not valid JSON". With `object`,
 * a parsed value that is not a plain object (null, an array, a scalar) is
 * "Must be a JSON object".
 */
export function parseJsonField(text: string, options: { required: boolean; object?: boolean }): JsonParse;

/** `JSON.stringify(value, null, 2)`; `""` for `undefined`. */
export function prettyJson(value: unknown): string;

/** Compact JSON of `value` cut to `max` characters (default 120) with "…"; a string shows as itself; `undefined` is "—". */
export function previewJson(value: unknown, max?: number): string;

/** The JSON a span payload stands for (D-J-8): `messages` and `json` → their `value`, `text` → the string; none → `undefined`. */
export function payloadJson(payload: Schema<"Payload"> | undefined): unknown;
```

`previewJson` counts UTF-16 code units (`String.prototype.slice`), as the rest of the web code does.

**3. `roles.ts`.** Add, after `canManageAlerts`:

```ts
/** Mirrors `ProjectPerm::ManageDatasets`: project members and above write datasets and experiments (D-J-5). */
export function canManageDatasets(role: OrgRole | undefined): boolean {
  return atLeast(role, "member");
}
```

In `roles.test.ts`, add a case in the style of the existing ones: `viewer` and `undefined` → false; `member`, `admin`, `owner` → true.

**4.** (No nav entry here: J5.2 adds it together with the real list page, so no stub route is ever committed — AGENTS.md § No placeholders.)

**5. Write mocks.** In `writes.test.ts`, add `path.startsWith("/api/v1/projects/{project}/datasets") ||` to the `covered` condition. In `writes.ts`, add one `WRITE_OPERATIONS` entry per POST/PATCH/DELETE under `/api/v1/projects/{project}/datasets` (ids, methods and paths exactly as the `jq` command of § Operation ids prints them), and one `RESPONSES` entry each. Import `fixtures/api/responses/datasets/01900000-0000-7000-8000-000000000c01.json` as `datasetDocument`, `…/c01/items/01900000-0000-7000-8000-000000000d02.json` as `datasetItemDocument`, `…/c01/versions.json` as `datasetVersionsDocument` (use `datasetVersionsDocument.data[0]` as the version), and M2's `static/dataset-import-preview.json`, `static/dataset-import-result.json` and `static/dataset-item-from-trace.json`. Answers:

| Operation | Answer |
|---|---|
| create dataset | `201` `{ ...datasetDocument, ...named(body) }` |
| update dataset | `200` `{ ...datasetDocument, ...named(body) }` |
| delete dataset | `noContent()` |
| add items | `201` `{ version: datasetDocument.current_version + 1, item_ids: [datasetItemDocument.id] }` |
| update item | `200` `{ ...datasetItemDocument, id: params.item_id }` |
| delete item | `200` `datasetVersionsDocument.data[0]` |
| import preview | `200` the preview fixture |
| import | `201` the import result fixture |
| add from trace | `201` the from-trace fixture |

The path parameter names are those the paths use (`params.item_id` if the path says `{item_id}`). Add to `writes.test.ts` one test, `"creates a dataset with the given name"`: `send("POST", `${PROJECT}/datasets`, JSON.stringify({ name: "golden" }))` answers 201 and `json.name === "golden"`.

### Tests

`lib/datasets.test.ts`:

1. `parseJsonField`: `""` not required → `{ ok: true, value: undefined }`; `"  "` required → `{ ok: false, message: "Required" }`; `"{"` → "Not valid JSON"; `"[1]"` with `object` → "Must be a JSON object"; `"null"` with `object` → "Must be a JSON object"; `'{"a":1}'` with `object` → `{ ok: true, value: { a: 1 } }`; `'"hi"'` → `{ ok: true, value: "hi" }`.
2. `previewJson`: `"hello"` → `hello`; `{ a: 1 }` → `{"a":1}`; a 200-character string with `max` 10 → its first 10 characters plus `…`; `undefined` → `—`.
3. `payloadJson`: messages → the array; json → the value; text → the string; `undefined` → `undefined`.
4. `prettyJson(undefined)` is `""`; `prettyJson({ a: 1 })` is `"{\n  \"a\": 1\n}"`.

`roles.test.ts` and `writes.test.ts` as above.

### Done when

`just check` passes; `writes.test.ts` covers every dataset write.

---

## J5.2 — Datasets list and the New dataset dialog

**Commit subject:** `web: list datasets and create one`

### Read first

- `web/src/routes/$org/$project/online-rules/index.tsx` and `index.test.tsx`.
- `web/src/routes/$org/$project/settings/score-definitions.tsx` (the create dialog: `Dialog`, `TextField`, `Textarea`, `SubmitButton`, `FormError`, field errors).
- `web/src/components/form/*`, `web/src/components/ui/dialog.tsx`, `web/src/components/time-text.tsx`.

### Files

| File | Change |
|---|---|
| `web/src/routes/$org/$project/datasets/index.tsx` | new: the list page |
| `web/src/components/shell/nav.ts` | Datasets entry |
| `web/src/routes/$org/$project/datasets/index.test.tsx` | new |
| `web/src/components/datasets/dataset-dialog.tsx` | new |
| `web/src/components/datasets/dataset-dialog.test.tsx` | new |

### Steps

**1. `DatasetDialog`.** One dialog for create and edit:

```ts
export function DatasetDialog(props: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  /** Absent: create. Present: edit this dataset (J5.3 uses it in the settings tab). */
  dataset?: Schema<"Dataset">;
  /** Called with the saved dataset after a success. */
  onSaved: (dataset: Schema<"Dataset">) => void;
}): JSX.Element;
```

Fields (ids prefixed `dataset-`): **Name** (`TextField`, required, `maxLength={200}`), **Description** (`Textarea`, optional, `maxLength={2000}`), **Input schema** and **Expected output schema** (JSON textareas, optional, description "A JSON Schema every item's input must match. Leave empty for none." and "… every item's expected output must match …"). Parse both with `parseJsonField(text, { required: false, object: true })`; a parse error shows under the field and sends nothing.

- Create: `$api.useMutation("post", "/api/v1/projects/{project}/datasets")` with `{ name: name.trim(), description?, input_schema?, expected_schema? }` (absent keys left out). Title "New dataset", button "Create dataset".
- Edit: `$api.useMutation("patch", "/api/v1/projects/{project}/datasets/{dataset}")` with `dataset: props.dataset.id`. Send only the changed fields; a schema or description that was set and is now empty is sent as `null` (D-J-2 clears it). Title "Edit dataset", button "Save". An edit with no change closes the dialog without a request.
- Errors: `400` field errors map `name`, `description`, `input_schema`, `expected_schema` to their fields; a field key starting with `items[` (a schema that the current items do not match, D-J-2) is listed under the schema fields in one `FormError` block, at most 5 lines then "and N more"; `409` with the detail "a dataset with this name already exists" shows "A dataset with this name exists" under Name (any other `409`, such as the 1,000-dataset cap, shows its detail); any other problem: `problemMessage(error, "The dataset could not be saved")`.
- On success: invalidate `datasetsKey` (and, in edit, `datasetKey`), call `onSaved(data)`, close.

**2. The list page.** Route `createFileRoute("/$org/$project/datasets/")` with `validateSearch: (search) => ({ q: typeof search.q === "string" && search.q.trim() ? search.q.trim() : undefined })`. The page:

- Heading `<h1>` "Datasets"; under it, a muted line "Versioned sets of inputs and expected outputs for experiments."
- Right: "New dataset" button. Below member: disabled, `DisabledReason` "Only members can create datasets". It opens `DatasetDialog` (create); `onSaved` navigates to `/$org/$project/datasets/$datasetId`.
- A search `Input` (`aria-label="Search datasets"`, placeholder "Search by name") that writes `q` into the URL on change, debounced 300 ms (use `setTimeout` in an effect; clear it on change).
- `$api.useInfiniteQuery("get", "/api/v1/projects/{project}/datasets", datasetsInit(org, project, q), { pageParamName: "cursor", initialPageParam: null, getNextPageParam: (page) => page.next_cursor ?? undefined })`.
- Pending: three `Skeleton` rows. Empty with no `q`: `EmptyState` icon `Table2`, title "No datasets", description "Create one, import a CSV or JSONL file, or add items from a trace." Empty with `q`: "No dataset matches" with no description.
- Table `aria-label="Datasets"`, columns **Name** (link to the dataset page, class as the online-rules link), **Items** (`item_count`), **Version** (`v{current_version}`), **Experiments** (`experiment_count`), **Updated** (`<TimeText value={updated_at} style="relative" />`; check `TimeText`'s style names and use the relative one it has).
- "Load more" as on the online-rules page.

**3. `nav.ts`.** Read `web/src/components/shell/nav.ts` and any test that lists the nav entries (`grep -rln "Online rules" web/src --include=*.test.tsx`). Import `Table2` from `lucide-react` and append after Online rules:

```ts
  { to: "/$org/$project/datasets", label: "Datasets", icon: Table2 },
```

If a test lists the nav labels, add "Datasets" to it. The route exists in this same sub-phase (§ 2), so TanStack's typed `to` checks.

### Tests

`dataset-dialog.test.tsx` (render the dialog inside `renderApp` is heavy; render it with a `QueryClientProvider` and a `ProjectProvider` value as other component tests do — find one with `grep -rln "ProjectProvider" web/src/components --include=*.test.tsx` and copy its wrapper):

1. `"creates a dataset with its schemas"`: type name "golden", input schema `{"type":"object"}`; `server.use(http.post("/api/v1/projects/:project/datasets", async ({ request }) => { captured = await request.json(); return HttpResponse.json({ ...datasetFixture, name: "golden" }, { status: 201 }); }))`; click "Create dataset"; `captured` equals `{ name: "golden", input_schema: { type: "object" } }`; `onSaved` was called with a dataset named "golden".
2. `"shows a schema that is not JSON"`: input schema `{`; click; "Not valid JSON" is shown; no request (the unhandled-request check proves it).
3. `"shows the name conflict"`: the handler answers 409 problem `{ type: "about:blank", title: "Conflict", status: 409, code: "conflict", detail: "…" }`; "A dataset with this name exists" is shown.
4. `"lists the items a new schema rejects"`: edit mode; the PATCH answers 400 with `errors: [{ field: "items[01900000-0000-7000-8000-000000000d01].input", message: "\"q\" is a required property" }]`; that message is shown.
5. `"clears a schema"`: edit a dataset whose fixture has an `input_schema` (spread `{ ...datasetFixture, input_schema: { type: "object" } }`), empty the field, save; the body is `{ input_schema: null }`.

`index.test.tsx`:

1. `"lists the datasets"`: `renderApp("/acme/support-bot/datasets")`; the table "Datasets" has a row for each fixture dataset, its name links to `/acme/support-bot/datasets/<id>`, and it shows `v<current_version>` and the item count.
2. `"shows the empty state"`: `server.use` answers `{ data: [], next_cursor: null }`; "No datasets" is shown.
3. `"searches by name"`: type "faq" in "Search datasets"; the next request has `q=faq` (capture it in a `server.use` handler; wait with `waitFor`).
4. `"disables creation for viewers"`: answer the project request with the project fixture's role set to `viewer` (see how online-rules tests do it: `grep -n "viewer" web/src/routes/\$org/\$project/online-rules/*.test.tsx`); "New dataset" is disabled.
5. `"opens a new dataset after creating it"`: create through the dialog (handler answers 201 with the fixture); the router location becomes the dataset page (`router.state.location.pathname`); the dataset page's own requests are answered by the default handlers.

### Done when

`just check` passes; `just web-dev` shows the Datasets entry in the project sidebar and the list with the example dataset, and New dataset opens the dialog.

---

## J5.3 — The dataset page: items, item sheet, add item, versions, settings

**Commit subject:** `web: show and edit a dataset's items and versions`

### Read first

- `web/src/routes/$org/$project/online-rules/$ruleId/index.tsx` (loader, `notFound`).
- `web/src/components/ui/tabs.tsx`, `sheet.tsx`, `select.tsx`, `table.tsx`; `web/src/components/confirm-dialog.tsx`, `type-to-confirm-dialog.tsx`, `id-text.tsx`, `payload/json-view.tsx` (`JsonView`).
- `web/src/routes/$org/$project/traces/$traceId.tsx` (the trace page URL, for the source link).
- J5.2's `DatasetDialog`.

### Files

| File | Change |
|---|---|
| `web/src/routes/$org/$project/datasets/$datasetId.tsx` | new |
| `web/src/routes/$org/$project/datasets/$datasetId.test.tsx` | new |
| `web/src/components/datasets/items-table.tsx` | new |
| `web/src/components/datasets/item-sheet.tsx` | new |
| `web/src/components/datasets/item-sheet.test.tsx` | new |
| `web/src/components/datasets/add-item-dialog.tsx` | new |
| `web/src/components/datasets/add-item-dialog.test.tsx` | new |
| `web/src/components/datasets/versions-table.tsx` | new |
| `web/src/components/datasets/dataset-settings.tsx` | new |

### Steps

**1. The route.** `createFileRoute("/$org/$project/datasets/$datasetId")`:

- `validateSearch`: `tab` is one of `"items" | "versions" | "settings"` (anything else: `undefined`, which means `items`; M6 adds `"experiments"`), and `version` is a non-negative integer parsed from a string or number (anything else: `undefined`). Put the parser in `lib/datasets.ts` as `parseDatasetSearch(search: Record<string, unknown>): { tab?: DatasetTab; version?: number }` with unit tests.
- `loader`: `ensureQueryData(datasetQuery(org, project, datasetId))`; a 404 problem throws `notFound()`, as the online-rule page does.

**2. The page.** `const dataset = useSuspenseQuery(datasetQuery(…)).data` (or `useQuery` with the loader's data, as the rule page does). Latest = `dataset.current_version`; shown version = `search.version ?? latest`; `atLatest = shown === latest`. Layout (`mx-auto max-w-6xl` as the list):

- Header: a "Datasets" breadcrumb link back to the list; `<h1>` the name; the description under it (muted, when set); a line "`{item_count}` items · version `{latest}`".
- Header actions (right): **Add item** (J5.3 § 5), **Import** (J5.4 adds it; leave a slot), **Export** (J5.4). Add item and Import are disabled below member (`DisabledReason` "Only members can edit datasets") and when `!atLatest` (`DisabledReason` "Switch to the latest version to edit").
- A version `Select` (`aria-label="Version"`): options "Latest (v`{latest}`)" (value `latest`), then the versions from `GET …/versions` as "v`{version}` — `{description}`" newest first, except the latest. Use `$api.useInfiniteQuery` with `datasetVersionsInit` (100 per page, paged by `next_cursor`); when `hasNextPage`, the select's last option is "Load more versions…", which calls `fetchNextPage()` and keeps the select open on the current value. A `?version=n` not yet loaded still shows as "v`{n}`" (taken from the URL). Choosing one sets `version` in the URL (`latest` removes it). At version 0 (a new dataset) the select shows only "Latest (v0)".
- When `!atLatest`: a muted banner "You are viewing version `{shown}`. Items are read-only." with a "Go to latest" button.
- `Tabs` with `value={search.tab ?? "items"}` writing `tab` into the URL: **Items**, **Versions**, **Settings**.

**3. Items tab: `ItemsTable`.**

```ts
export function ItemsTable(props: { datasetId: string; version: number; editable: boolean }): JSX.Element;
```

`$api.useInfiniteQuery("get", "/api/v1/projects/{project}/datasets/{dataset}/items", datasetItemsInit(org, project, datasetId, version), {…paging as the list})`. Table `aria-label="Items"`, columns: **Item** (a `Button variant="link"` reading `shortId(item.id)` — `lib/format.ts` — with `aria-label={"Open item " + item.id}`, opening the sheet), **Input** (`previewJson(item.input)`), **Expected output** (`previewJson(item.expected_output)`), **Metadata** (`previewJson(item.metadata)`, "—" for `{}`), **Source** (when `source_trace_id`: a link "trace" to `/$org/$project/traces/$traceId` with `search={{ span: source_span_id }}` when `source_span_id` is set: the trace page's `span` search param, `parseTracePageSearch` in `lib/search/traces.ts`), **Added in** (`v{item.version}`). Cells are `font-mono text-xs` and truncate. Empty: `EmptyState` icon `Table2`, "No items" with description "Add items one by one, import a file, or use Add to dataset on a trace." "Load more" as usual.

**4. `ItemSheet`.**

```ts
export function ItemSheet(props: {
  datasetId: string;
  item: Schema<"DatasetItem"> | undefined;   // undefined: closed
  editable: boolean;                          // atLatest and canManageDatasets
  onOpenChange: (open: boolean) => void;
}): JSX.Element;
```

A right `Sheet` (`SheetContent` wide: `sm:max-w-2xl`), title "Item" and the full id (`IdText`). Read-only (`!editable`): three sections **Input**, **Expected output**, **Metadata**, each a `JsonView`; the source trace link; "Added in v`{version}`", "Revision `{revision_id}`". Editable: three textareas (`aria-label` "Input", "Expected output", "Metadata") filled with `prettyJson(…)` (metadata `{}` when empty), and buttons **Save** and **Delete item**.

- Save: parse input (`required: true`), expected output (`required: false`), metadata (`required: false, object: true`; empty means `{}`). Build the body with only the fields whose parsed value differs from the item's (compare `JSON.stringify` of both); an expected output that was set and is now empty sends `expected_output: null`. No change: close without a request. `PATCH …/items/{item_id}`. Errors: field errors by `input`, `expected_output`, `metadata` (and a field starting with `input`/`expected_output` from a schema check, D-J-2, under that field); `409` (the 50,000-item cap cannot happen on a patch; any 409) and others: `problemMessage(error, "The item could not be saved")`. Success: invalidate `datasetWriteKeys(…)`, close.
- Delete: `ConfirmDialog` title "Delete item", description "The item leaves the dataset in a new version. Older versions keep it.", `confirmLabel` "Delete item", destructive. `DELETE …/items/{item_id}`. Success: invalidate, close both. Error: `problemMessage(error, "The item could not be deleted")` in the dialog.

**5. `AddItemDialog`.** A `Dialog` "Add item" with the same three textareas (input required, expected optional, metadata optional object) and a **Add item** submit. `POST …/items` with `{ items: [ { input, expected_output?, metadata? } ] }`. Field errors arrive as `items[0].input` etc.: strip the `items[0].` prefix to place them. Success: invalidate `datasetWriteKeys`, close, reset the fields.

**6. Versions tab: `VersionsTable`.** `useInfiniteQuery` on `…/versions` (`datasetVersionsInit`). Table `aria-label="Versions"`, columns **Version** (`v{version}`, a link that sets `version` in the URL and `tab=items`), **Description**, **By** (`created_by`: a user id shows as `IdText kind="user"` if `IdText` has a user kind — else plain `shortId`; a value starting with `key:` shows "API key"; absent "—"), **Created** (`TimeText`). Empty (version 0): "No versions yet" (`EmptyState` icon `History`).

**7. Settings tab: `DatasetSettings`.** A definition list: Name, Description, Input schema, Expected output schema (each schema in a `JsonView`, or "None"). Buttons: **Edit** (opens `DatasetDialog` with `dataset`), **Delete dataset** (destructive). Delete uses `TypeToConfirmDialog` (read its props) with the dataset name as the text to type, title "Delete dataset", description "Deletes the dataset and all its versions. Experiments keep their results and show the dataset as deleted." `DELETE …/datasets/{dataset}` then invalidate `datasetsKey` and navigate to the list. Both buttons are disabled below member (`DisabledReason` "Only members can edit datasets").

### Tests

`lib/datasets.test.ts` (extend): `parseDatasetSearch` keeps `tab: "versions"`, drops `tab: "x"`, parses `version: "3"` and `version: 3` to 3, drops `"-1"`, `"1.5"`, `"a"`.

`$datasetId.test.tsx` (URL `/acme/support-bot/datasets/<DATASET_ID>`):

1. `"shows the dataset and its items"`: the heading is the fixture name; the "Items" table has one row per fixture item; the first row shows `previewJson` of its input.
2. `"answers not found for an unknown dataset"`: `server.use` answers 404 for `…/datasets/:dataset`; the not-found view shows (`screen.findByText` of the text `NotFound` renders; read `components/not-found.tsx`).
3. `"reads a past version read-only"`: URL with `?version=1`; the items request has `version=1` (capture it); the banner "You are viewing version 1. Items are read-only." shows; "Add item" is disabled.
4. `"switches version from the selector"`: choose the option of version 1 in "Version"; the URL has `version=1`.
4b. `"loads more versions"`: answer the first versions request with one version and `next_cursor: "c1"`, the second (request with `cursor=c1`) with another; choose "Load more versions…"; the second version's option appears.
5. `"lists the versions"`: `?tab=versions`; the "Versions" table shows `v<version>` and the description of each fixture version.
6. `"links an item to its source trace"`: answer the items request with one item having `source_trace_id: "a1000000000000000000000000000001"`; the "trace" link points to `/acme/support-bot/traces/a1000000000000000000000000000001` with `?span=<source_span_id>` when the item has one.
7. `"deletes the dataset after typing its name"`: Settings tab, Delete dataset, type the name, confirm; a `server.use(http.delete(…))` handler is called; the location becomes `/acme/support-bot/datasets`.

`item-sheet.test.tsx` (component wrapper as in J5.2):

1. `"saves only the changed fields"`: change Metadata to `{"topic":"billing"}`; the PATCH body is `{ metadata: { topic: "billing" } }`.
2. `"clears the expected output"`: an item with an expected output; empty the field; the body is `{ expected_output: null }`.
3. `"refuses input that is not JSON"`: input `{`; "Not valid JSON"; no request.
4. `"places a schema error under the input"`: the PATCH answers 400 `errors: [{ field: "input", message: "\"q\" is a required property" }]`; the message shows next to Input.
5. `"deletes the item after confirming"`: Delete item, confirm; the DELETE handler is called; `onOpenChange(false)` was called.
6. `"is read-only on a past version"`: `editable={false}`; no textbox named "Input"; the JSON is shown.

`add-item-dialog.test.tsx`:

1. `"adds one item"`: input `{"q":"hi"}`, expected `"hello"`; the body is `{ items: [{ input: { q: "hi" }, expected_output: "hello" }] }`.
2. `"maps the batch field errors to the fields"`: the POST answers 400 with `items[0].input`; the message shows under Input.
3. `"shows the item cap"`: the POST answers 409 problem with detail "a dataset holds at most 50000 items"; that text shows.

### Done when

`just check` passes; in `just web-dev`, the example dataset's page shows its items, versions and settings, the sheet opens, and edits answer from the write mocks.

---

## J5.4 — Import wizard and export

**Commit subject:** `web: import a CSV or JSONL file into a dataset and export it`

### Read first

- D-J-6 and D-J-7 in the design; `docs/spec/06-evaluation.md` § Datasets (Import).
- `web/src/lib/sql/export.ts` (`saveBlob`, `exportName`, the `parseAs: "blob"` call).
- `web/src/components/ui/dropdown-menu.tsx`, `select.tsx`, `dialog.tsx`.
- The generated request types of the two uploads: `grep -n "import/preview\|/import\"" -A40 web/src/api/schema.gen.ts`. M2 declares the preview body `application/octet-stream` with the query `format` and `partial`, and the import body `multipart/form-data` (parts `file` and `mapping`). `ImportPreview` has `format`, `columns`, `rows`, `errors` (row errors of the head) and `truncated`; `ImportResult` has a required `version`.
- `node_modules/openapi-fetch`: check how it treats a `FormData` body and a `bodySerializer` (`grep -n "FormData\|bodySerializer" web/node_modules/openapi-fetch/dist/index.js`). openapi-fetch removes the JSON `Content-Type` for a `FormData` body; if this version does not, pass `headers: { "Content-Type": null }` (openapi-fetch drops a header set to `null`). If neither works, STOP and report.

### Files

| File | Change |
|---|---|
| `web/src/lib/datasets.ts`, `datasets.test.ts` | file format, mapping helpers |
| `web/src/components/datasets/import-dialog.tsx` | new |
| `web/src/components/datasets/import-dialog.test.tsx` | new |
| `web/src/components/datasets/export-menu.tsx` | new |
| `web/src/components/datasets/export-menu.test.tsx` | new |
| `web/src/routes/$org/$project/datasets/$datasetId.tsx` | the Import and Export buttons |

### Steps

**1. Helpers in `lib/datasets.ts`.**

```ts
export type ImportFormat = "csv" | "jsonl";
export const IMPORT_MAX_BYTES = 100 * 1024 * 1024;
export const PREVIEW_BYTES = 1024 * 1024;

/** `csv` for `.csv`; `jsonl` for `.jsonl` and `.ndjson` (case-insensitive); else `undefined`. */
export function importFormatOf(fileName: string): ImportFormat | undefined;

/** The import targets of D-J-6. */
export type ImportTarget = "input" | "expected" | "ignore" | `input.${string}` | `expected.${string}` | `metadata.${string}`;

/** A column's default target: `input` → "input"; `expected` or `expected_output` → "expected"; `metadata` → "ignore"; any other column `c` → `input.c`. Names compare case-insensitively after trimming. */
export function defaultTarget(column: string): ImportTarget;

/**
 * Problems with a mapping, in this order, each once:
 * - no column maps to `input` or `input.<key>`: "Map at least one column to the input";
 * - `input` and an `input.<key>` both used: "Map the whole input or input keys, not both";
 * - the same for expected: "Map the whole expected output or its keys, not both";
 * - a target used by two columns: "Two columns map to <target>";
 * - a keyed target with an empty key (`input.`): "<column>: the key is empty".
 */
export function mappingProblems(rows: { column: string; target: ImportTarget }[]): string[];
```

**2. `ImportDialog`.**

```ts
export function ImportDialog(props: { datasetId: string; open: boolean; onOpenChange: (open: boolean) => void }): JSX.Element;
```

A `Dialog` (`sm:max-w-3xl`) titled "Import items" with four steps held in state (`"file" | "preview" | "result"`; the mapping is part of the preview step):

1. **File.** A visually hidden `<input type="file" accept=".csv,.jsonl,.ndjson">` with a visible `<Label>` "Choose a CSV or JSONL file" (`htmlFor` the input). On choice: `importFormatOf(file.name)` undefined → error "Choose a .csv or .jsonl file"; `file.size > IMPORT_MAX_BYTES` → "The file is larger than 100 MiB". Otherwise take the head, `const head = file.slice(0, PREVIEW_BYTES)` (a `Blob`; the bytes go as they are), and call the preview: `api.POST("/api/v1/projects/{project}/datasets/{dataset}/import/preview", { params: { path: {…}, query: { format, partial: file.size > PREVIEW_BYTES } }, body: head as never, bodySerializer: (body) => body, headers: { "Content-Type": "application/octet-stream" } })` (M2 declares the body `application/octet-stream` and the `partial` flag). A problem shows `problemMessage(error, "The file could not be read")` (e.g. a CSV with an empty or duplicate header name). Success → preview step.
2. **Preview and mapping.** Show the file name, its size (`formatBytes`) and the format. `truncated: true` adds "Showing the first rows of the file." The preview's `errors` (lines of the head that could not be read) are listed under the table as "Row {row}: {message}", at most 5 then "and N more". A preview `Table` (`aria-label="Preview"`): CSV → header `columns`, cells the strings; JSONL → one column "Row" with `previewJson(row, 200)`. For CSV, a **Mapping** table (`aria-label="Mapping"`) with one row per column: the column name, and a target control — a `Select` (`aria-label={"Target of " + column}`) with options "Whole input" (`input`), "Input key" (`input.<key>`), "Whole expected output" (`expected`), "Expected output key" (`expected.<key>`), "Metadata key" (`metadata.<key>`), "Ignore" (`ignore`); a keyed choice shows a small `Input` for the key (`aria-label={"Key of " + column}`), defaulting to the column name. Defaults come from `defaultTarget`. Below it, the `mappingProblems` list (role `alert`). JSONL shows the line "Each line must be an object with input and, optionally, expected_output and metadata." and no mapping. The **Import** button is disabled while there are mapping problems; below member it cannot be reached (the dialog only opens for members).
3. **Import.** `const form = new FormData(); form.append("file", file); if (format === "csv") form.append("mapping", new Blob([JSON.stringify({ columns: rows.map(({ column, target }) => ({ column, target })) })], { type: "application/json" }));` then `api.POST("/api/v1/projects/{project}/datasets/{dataset}/import", { params: {…}, body: form as never, bodySerializer: (body) => body })` (plus the `Content-Type: null` header if J5.4 § Read first found it needed). The button shows a spinner and the dialog cannot close while it runs (as `ConfirmDialog` does with `pending`). Errors: `413` → "The file is larger than 100 MiB"; a `400` (detail "no row could be imported", `errors` with fields `rows[<n>]`; or a mapping error) → `problemMessage(error, "No row could be imported")` and, when `errors` are present, a "Row errors" table built from them (the row number parsed from `rows[<n>]`); `409` → `problemMessage(error, "The dataset would hold more than 50000 items")`; others `problemMessage(error, "The import failed")`.
4. **Result.** "Imported `{imported}` items as version `{version}`.", and when `error_count > 0`: "`{error_count}` rows were skipped." and a table `aria-label="Row errors"` (Row, Message) of `errors`; when `error_count > errors.length`: "Showing the first `{errors.length}` errors." A **Done** button closes. On success, invalidate `datasetWriteKeys` once.

Closing resets the dialog to the file step.

**3. `ExportMenu`.**

```ts
export function ExportMenu(props: { datasetId: string; version: number }): JSX.Element;
```

A `DropdownMenu` with trigger "Export" and items "JSONL" and "CSV". Each runs `api.GET("/api/v1/projects/{project}/datasets/{dataset}/export", { params: { path, query: { version, format } }, parseAs: "blob" })`, then `saveBlob(blob, exportName(response.headers.get("Content-Disposition"), `${datasetId}-v${version}.${format}`))` from `@/lib/sql/export`. A failure shows a `FormError` under the trigger with `problemMessage(error, "The export could not be downloaded")`. Export works at any version and for viewers. The CSV item has the hint (a muted line under it in the menu): "Open it in a spreadsheet as text: cells are not escaped against formulas." (D-J-7).

**4. Wire both** into the dataset page header: Import (opens `ImportDialog`) and `<ExportMenu datasetId={…} version={shown} />`.

### Tests

`lib/datasets.test.ts` (extend): `importFormatOf` for `a.CSV`, `b.jsonl`, `c.ndjson`, `d.json` (undefined); `defaultTarget` for `Input`, `expected_output`, `metadata`, `question` (`input.question`); `mappingProblems` for each message, and `[]` for `[{column:"q",target:"input.q"},{column:"a",target:"expected"}]`.

`import-dialog.test.tsx` (component wrapper; build files with `new File(["question,answer\nWhat?,That\n"], "faq.csv", { type: "text/csv" })` and `await user.upload(screen.getByLabelText("Choose a CSV or JSONL file"), file)`):

1. `"previews a CSV file and imports it with the mapping"`: the preview handler answers `{ format: "csv", columns: ["question","answer"], rows: [["What?","That"]], errors: [], truncated: false }` and checks `new URL(request.url).searchParams.get("format") === "csv"`, `partial` is `"false"`, and `await request.text()` starts with `question,answer`; set "Target of answer" to "Whole expected output"; click Import; the import handler reads `await request.formData()`: `file` is a `File` named `faq.csv`, and `JSON.parse(await (form.get("mapping") as Blob).text())` equals `{ columns: [{ column: "question", target: "input.question" }, { column: "answer", target: "expected" }] }`; it answers `{ version: 2, imported: 1, error_count: 0, errors: [] }`; "Imported 1 items as version 2." shows.
2. `"imports JSONL without a mapping"`: a `.jsonl` file; the form has no `mapping` part.
3. `"refuses another extension"`: `data.json`; "Choose a .csv or .jsonl file"; no request.
4. `"blocks an import without an input"`: set every column to Ignore; "Map at least one column to the input" shows and Import is disabled.
5. `"lists the row errors"`: the import answers `{ version: 3, imported: 1, error_count: 150, errors: [{ row: 2, message: "the row has no input" }, …100 entries] }`; "150 rows were skipped." and "Showing the first 100 errors." show, and the "Row errors" table has 100 rows.
6. `"shows a header error from the preview"`: the preview answers 400 with detail "the header has a duplicate column name"; that text shows.
7. `"lists the rows of a refused import"`: the import answers 400 with detail "no row could be imported" and `errors: [{ field: "rows[1]", message: "the row has no input" }]`; "no row could be imported" and a "Row errors" row `1` / `the row has no input` show.

`export-menu.test.tsx`: stub `URL.createObjectURL` and `URL.revokeObjectURL` (`vi.spyOn(URL, "createObjectURL").mockReturnValue("blob:x")`, as the SQL export test does — `grep -rn "createObjectURL" web/src --include=*.test.*`); the menu shows "Open it in a spreadsheet as text: cells are not escaped against formulas."; choose "CSV"; the request has `format=csv` and `version=<n>`; the handler answers a `text/csv` body with `Content-Disposition: attachment; filename="<id>-v3.csv"`; a link with `download="<id>-v3.csv"` was clicked (spy on `HTMLAnchorElement.prototype.click`).

### Done when

`just check` passes; in `just web-dev`, Import walks through the steps against the write mocks.

---

## J5.5 — "Add to dataset" in the trace view

**Commit subject:** `web: add a trace or span to a dataset`

### Read first

- D-J-8 in the design.
- `web/src/components/trace/trace-view.tsx` (where `TraceHeader` gets `actions={<RunEvaluatorButton … />}`) and `web/src/components/trace/run-evaluator-dialog.tsx` (the button, the dialog, the trace/span radio, `selectedIsRoot`).
- `web/src/api/queries.ts` `spanDetailQuery`; the `SpanDetail` type's `input` and `output` (`Schema<"Payload">`).
- J5.2's `DatasetDialog` is not used here: a new dataset is created inline with a name only.

### Files

| File | Change |
|---|---|
| `web/src/components/datasets/add-to-dataset-dialog.tsx` | new |
| `web/src/components/datasets/add-to-dataset-dialog.test.tsx` | new |
| `web/src/components/trace/trace-view.tsx` | the button next to Run evaluator |

### Steps

**1. The button and the dialog.** One entry point serves the trace and the selected span (the design's "trace and span", D-J-8): like Run evaluator, it sits in the trace header's `actions` and offers the trace's root or the selected span.

```ts
export interface AddToDatasetTarget {
  traceId: string;
  /** The trace's root span (a span with no parent), when the loaded trace has one. */
  rootSpanId?: string;
  /** The span the trace view has selected. */
  spanId?: string;
  /** Whether the selected span is the root. */
  selectedIsRoot: boolean;
}

export function AddToDatasetButton(props: AddToDatasetTarget): JSX.Element;
```

The button: `variant="outline" size="xs"`, text "Add to dataset", disabled below member with `DisabledReason` "Only members can add to datasets". It opens a `Dialog` titled "Add to dataset", description "Copies the input and output into a new dataset item. Edit them before saving."

The form:

- **From** (`RadioGroup`, shown only when `spanId` is set and `!selectedIsRoot`): "Trace" (the root) and "Selected span". Default: "Selected span" when shown.
- The span read for the prefill is the selected span when "Selected span" is chosen, else `rootSpanId`. Load it with `useQuery(spanDetailQuery(org, project, traceId, id))` (already cached when it is the selected span). While it loads, the textareas are disabled. When there is no root span (`rootSpanId` undefined and "Trace" chosen), the fields start empty and the line "This trace has no root span; type the input." shows.
- **Dataset**: a search `Input` (`aria-label="Search datasets"`, placeholder "Search by name", debounced 300 ms) above a `Select` (`aria-label="Dataset"`) of the matching datasets (`$api.useQuery` with `datasetsInit(org, project, q)`: the server's `q` filter, 100 per answer; a project may hold 1,000 datasets, so the search, not the first page, reaches them all), by name, plus a last option "New dataset…". When the answer has a `next_cursor`, a muted line "More datasets match; refine the search." shows. Choosing "New dataset…" shows a `TextField` "Name of the new dataset" (required, max 200).
- **Input**, **Expected output**, **Metadata**: JSON textareas, prefilled with `prettyJson(payloadJson(span.input))`, `prettyJson(payloadJson(span.output))` and `{}`; re-filled when the target changes, unless the user has typed in them (track a `dirty` flag per field).
- Submit **Add item**:
  1. Parse the three fields (input required; expected optional; metadata optional object).
  2. With "New dataset…": `POST …/datasets` `{ name }`; its errors show under the name field (`409` with the detail "a dataset with this name already exists" → "A dataset with this name exists"; another problem → its detail); stop on error. Then use the new id.
  3. `POST …/datasets/{dataset}/items/from-trace` with `{ trace_id: traceId, span_id: target === "span" ? spanId : rootSpanId, input, expected_output?, metadata }` (absent `expected_output` left out; `span_id` is left out only when the trace has no root span). "Trace" always sends the root's `span_id`, so the stored `source_span_id` is the span the dialog prefilled from, whatever rule the server would pick (D-J-8).
  4. Errors: field errors under their fields (a schema error from D-J-2 lands on `input` or `expected_output`); `404` → "The trace was not found. It may have been deleted or be outside your plan's data window."; `409` → `problemMessage(error, "The dataset is full")`; others `problemMessage(error, "The item could not be added")`.
  5. Success: invalidate `datasetsKey` and `datasetWriteKeys(…, datasetId)`; replace the form with "Added to `{dataset name}`." and two buttons: **Open dataset** (link to the dataset page) and **Close**.

**2. Wire it.** In `trace-view.tsx`, compute `rootSpanId = loaded.spans.find((row) => row.span.parent_span_id === undefined)?.span.span_id` and pass `actions={<><RunEvaluatorButton … /><AddToDatasetButton traceId={loaded.trace.trace_id} rootSpanId={rootSpanId} spanId={selectedRow ? selected : undefined} selectedIsRoot={…same expression as Run evaluator…} /></>}`.

### Tests

`add-to-dataset-dialog.test.tsx`. Use the trace page (`renderApp("/acme/support-bot/traces/<a trace id of fixtures/api/dataset/spans.json that has a root span>")`) so the default handlers serve the trace and its spans; pick the trace id from the fixture (read `fixtures/api/README.md` for the example trace ids).

1. `"prefills from the root span and adds the item"`: open "Add to dataset"; the Input textarea holds the root span's input as `prettyJson(payloadJson(…))` (compute the expected text from the fixture's span detail response file); pick the fixture dataset; click Add item; the from-trace handler receives `trace_id` and `span_id` equal to the root span's id; "Added to faq." shows (use the fixture's name).
2. `"adds the selected span"`: open the trace with a non-root span selected (`?span=<16 hex>`, the trace page's search param); choose "Selected span"; the body has that `span_id`.
3. `"creates a dataset first"`: choose "New dataset…", type "golden"; the dataset POST is called before the from-trace POST, and the from-trace path uses the id the first answered (answer `{ ...datasetFixture, id: "01900000-0000-7000-8000-000000000c09", name: "golden" }`).
4. `"keeps edited fields when the target changes"`: type in Expected output, switch From; the typed text stays.
5b. `"searches the dataset picker"`: type "gold" in "Search datasets"; the next datasets request has `q=gold` (capture it with `server.use`).
5. `"explains a missing trace"`: the from-trace handler answers 404; the 404 text shows.
6. `"is disabled for viewers"`: viewer role; the button is disabled.

### Done when

`just check` passes; in `just web-dev`, the trace view shows "Add to dataset" next to "Run evaluator", and the dialog adds an item through the write mocks.

---

## Review checklist (Sonnet, after J5.5)

- Every write control checks `canManageDatasets` and past versions are read-only.
- Field errors from the server land on the right field, including the batch prefix `items[0].` and the schema errors of D-J-2.
- No content is put in a URL (dataset names travel only in bodies; ids in paths).
- The import never loads more than 1 MiB of the file into a string (the full file goes as a `File` part).
- Tests do not depend on fixture literals other than those this plan gives.

## Commands

```bash
export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH
```

```bash
just check
```

Focused runs while working: `pnpm --dir web test -- datasets`, `pnpm --dir web test -- writes`, `pnpm --dir web run typecheck`.

## Report

When a sub-phase is done, report what was done, any deviation and why (an operation id, a fixture name, a content type that differed from this plan), and anything left undone or unverified. M5 adds no dependency.
