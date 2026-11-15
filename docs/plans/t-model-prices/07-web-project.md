# T7 — The project Model prices page, the online-rule hint, closeout

**Commit subject:** `web: add the project model prices page`

Adds `/$org/$project/settings/models` (D-T-12): the project's own rows (admins edit), the org's rows (read-only, linked), the built-in rows (search), and "Which price applies?" over `/models/resolve`. The online-rule editor links to it when a budget is refused for lack of a price (D-T-11). The project write mocks come with it. Then the track is closed: roadmap row, README status.

## Read first

- README D-T-7, D-T-11, D-T-12.
- T6 (all of it): `queries-prices.ts`, `lib/prices.ts`, `PriceDialog`, `PriceTable`.
- `web/src/routes/$org/$project/settings/score-definitions.tsx` and its test (a project settings page with `useProject`, `useInfiniteQuery` paging by `cursor`, `canManageEvaluation`), `web/src/routes/$org/$project/settings/api-keys.tsx` (`atLeast(data.role, "admin")`).
- `web/src/components/online-rules/rule-form.tsx` (the budget `TextField` with `error={errors.budget_usd}`, how server field errors land in `errors`), its test file.
- `web/src/mocks/writes.ts`, `web/src/mocks/writes.test.ts` (the covered path prefixes).
- `fixtures/api/static/model-prices.json`, `model-price-resolution.json`, `model-price.json`.

## Files

| File | Change |
|---|---|
| `web/src/routes/$org/$project/settings/models.tsx`, `models.test.tsx` | new |
| `web/src/components/prices/price-resolver.tsx` | new |
| `web/src/components/settings/settings-nav.ts` | project entry "Model prices" |
| `web/src/components/online-rules/rule-form.tsx` and its test | link under the budget error |
| `web/src/mocks/writes.ts`, `web/src/mocks/writes.test.ts` | project price writes |
| `docs/roadmap.md` | T row: Done |
| `docs/plans/t-model-prices/README.md` | status line |
| `CHANGELOG.md` | one `Added` line for the pages |

## 1. Nav

`projectSettingsNav`: add `{ to: "/$org/$project/settings/models", label: "Model prices", adminOnly: false },` after "Score definitions" (viewers can read prices).

## 2. The page (`routes/$org/$project/settings/models.tsx`)

```ts
export const Route = createFileRoute("/$org/$project/settings/models")({ component: ProjectPricesPage });
```

`const { org, project, ref, data } = useProject(); const canManage = atLeast(data.role, "admin");`

`SettingsPage` title "Model prices", description "What costs are computed with in this project. A project price beats an organization price, which beats the built-in LiteLLM prices. New prices apply to spans received from now on; stored costs are not recomputed." Action: "Add price" behind `DisabledReason` ("Only project admins can change prices").

Sections, top to bottom (each an `<section aria-labelledby>` with an `h2`):

1. **"Which price applies?"** — `PriceResolver` (§ 3).
2. **"Project prices"** — `$api.useInfiniteQuery("get", "/api/v1/projects/{project}/models", projectPricesInit(org, project, "project", ""), { pageParamName: "cursor", initialPageParam: null, getNextPageParam: (page) => page.next_cursor ?? undefined })`; rows flattened; `PriceTable` label "Project prices" with Edit / Delete when `canManage`; empty: `EmptyState` "No project prices" / "Organization and built-in prices apply.". A "Load more" button when `hasNextPage`.
3. **"Organization prices"** — the same query with scope `"org"`; `PriceTable` label "Organization prices", read-only; a link "Manage organization prices" to `/$org/settings/models` (`Link` from TanStack router with `params={{ org }}`); empty: one muted line "No organization prices.".
4. **"Built-in prices"** — a search `Input` (`aria-label="Search built-in prices"`, placeholder "Search by model, e.g. gpt-4o"), debounced 250 ms with a `useEffect` and `setTimeout` (the web has no debounce hook; keep it local to the page); the query with scope `"builtin"` and `q`, enabled only when the trimmed search has at least 2 characters; `PriceTable` label "Built-in prices", read-only, "Load more" when there is a next page. Before a search: the muted line "Search the built-in prices from LiteLLM."

Edit opens T6's `PriceDialog` with `scope={{ kind: "project", org, project }}` and the row; Delete uses `ConfirmDialog` as T6's org page, with `blockedByRules` for a `409`, and `DELETE /api/v1/projects/{project}/models/{id}` (path `{ project: ref, id }`). After a delete, invalidate `[...PROJECT_PRICES_PREFIX]` and `[...PROJECT_RESOLVE_PREFIX]`.

## 3. `components/prices/price-resolver.tsx`

```ts
export function PriceResolver({ org, project }: { org: string; project: string }): JSX.Element;
```

A small form: "Model" (`TextField`, id `resolve-model`), "Provider" (`TextField`, id `resolve-provider`, description "Optional, e.g. openai"), and a "Check" submit button. On submit with a non-blank model, set the submitted values in state and run `$api.useQuery("get", "/api/v1/projects/{project}/models/resolve", { params: { path: { project: projectRef(org, project) }, query: { model, ...(provider ? { provider } : {}) } } }, { enabled: submitted })`. Show:
- `price` absent (test `data.price === undefined`; the generated type is `price?: ModelPrice`, never `null`) → "No price matches this model. Its spans get no cost, and a budgeted online rule cannot use it as a judge."
- a row → "Priced by the {scope label} price “{model_name}”: input {formatUnitPrice}, output {formatUnitPrice} per 1M tokens." where the scope label is "project", "organization" or "built-in".

## 4. Online-rule hint (`rule-form.tsx`)

Under the budget `TextField`, when `errors.budget_usd` contains `"no price"`, render a link "Add a model price" to `/$org/$project/settings/models` (`Link` with the `org` and `project` props `RuleForm` already takes). Test in the rule form's test file: the create request answers `400` with `errors: [{field: "budget_usd", message: "the judge model has no price, so the rule cannot have a budget; add one under Settings → Model prices"}]` → the message and the link show; the link's `href` is `/acme/support-bot/settings/models`.

## 5. Write mocks

`writes.test.ts`: add `path.startsWith("/api/v1/projects/{project}/models") ||` to the covered prefixes. `writes.ts`:
- import `modelPriceDocument from "../../../fixtures/api/static/model-price.json";`;
- `WRITE_OPERATIONS`: `post_model_price` (POST `/api/v1/projects/{project}/models`), `patch_model_price` (PATCH `/api/v1/projects/{project}/models/{id}`), `delete_model_price` (DELETE same), after the score definition entries;
- `RESPONSES`: `post_model_price: ({ body }) => HttpResponse.json({ ...modelPriceDocument, ...priced(body) }, { status: 201 })`, `patch_model_price: ({ body, params }) => HttpResponse.json({ ...modelPriceDocument, ...priced(body), id: params.id })`, `delete_model_price: () => noContent()`.
- A test like T5's for the three project writes.

## 6. Page tests (`models.test.tsx`)

1. "shows project and organization prices": the project fixture rows render in "Project prices" (`gpt-4o (negotiated)`, `openai`, `$2.00`) and "Organization prices" (`Llama 3.1 8B (local)`, `Any`), and the "Manage organization prices" link points at `/acme/settings/models`. The manifest mock serves one `list_models` file for any query; override `GET /api/v1/projects/:project/models` with `server.use` to answer per `scope` (`new URL(request.url).searchParams.get("scope")`) from the fixture's rows filtered by scope.
2. "searches built-in prices": type `gpt-4o` → after the debounce the override receives `scope=builtin&q=gpt-4o` and the rows show; one character sends nothing.
3. "resolves a model": type `gpt-4o`, provider `openai`, Check → "Priced by the project price “gpt-4o (negotiated)”: input $2.00, output $8.00 per 1M tokens."; an override answering `{}` → the "No price matches" text.
4. "adds a project price": Add price → dialog → body posted to `/api/v1/projects/:project/models`.
5. "viewers and members read only": with the project role `member` (override the project fetch the way `score-definitions.test.tsx` sets a role), no Edit buttons, "Add price" disabled with the reason.
6. "a delete blocked by rules explains why" (`409`, as T6 test 7).

## 7. Closeout

- `CHANGELOG.md` `### Added`: `- Settings → Model prices, for an organization and for a project: add, edit and delete custom prices, search the built-in prices, and check which price applies to a model.`
- `docs/roadmap.md`, the T row: Status `**Done.** T1–T7: custom prices per org and project, \`/models\` and \`/models/resolve\`, every span and judge priced with its project's table, and the two settings pages (PR <number>).`
- `docs/plans/t-model-prices/README.md`, first line: `Status: T1–T7 done on <date>`.

## Commands

```bash
export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH
```

```bash
pnpm --dir web test -- models prices rule-form writes
```

```bash
just check
```

## Done when

- `/acme/support-bot/settings/models` works on the mocks (`just web-dev`) and in the tests above; the online-rule editor links to it.
- The roadmap and README say Done.
- `just check` is green.

**Commit:** `web: add the project model prices page`
