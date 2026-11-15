# T6 — API helpers, the price dialog and the org Model prices page

**Commit subject:** `web: add the org model prices page`

Adds the query helpers, one dialog that creates and edits a price row of either scope (D-T-12), a table component both pages share, the org settings page `/$org/settings/models`, and its nav entry. The project page is T7.

## Read first

- README D-T-4, D-T-5, D-T-7, D-T-12; spec 11 § Settings (as amended).
- `web/src/routes/$org/settings/llm-connections.tsx` and its test (the model for an org settings page: `SettingsPage`, `DisabledReason`, `EmptyState`, `ConfirmDialog`, a table, role from `/api/auth/me`, MSW `server.use` overrides, `meAs`).
- `web/src/components/settings/connection-dialog.tsx` (dialog structure, `TextField`, `fieldErrors`, `problemMessage`, `PLACED_FIELDS`, `FormError`, `SubmitButton`, mutations with `$api.useMutation`, invalidation with `queryClient.invalidateQueries`).
- `web/src/components/settings/settings-nav.ts`, `web/src/lib/roles.ts`, `web/src/api/queries-settings.ts`, `web/src/api/problem.ts`, `web/src/lib/format.ts` (`formatCost`).
- `web/src/api/schema.gen.ts`: the `ModelPrice`, `ModelPriceInput`, `ModelPriceTier`, `ModelPriceScope` schemas and the `/models` paths (T4, T5).
- `fixtures/api/static/org-model-prices.json`, `model-price.json`.

## Files

| File | Change |
|---|---|
| `web/src/api/queries-prices.ts` | new |
| `web/src/lib/roles.ts` | `canManageOrgPrices` |
| `web/src/lib/prices.ts`, `web/src/lib/prices.test.ts` | new: formatting and form conversion |
| `web/src/components/prices/price-dialog.tsx` | new |
| `web/src/components/prices/tiers-editor.tsx` | new |
| `web/src/components/prices/price-table.tsx` | new |
| `web/src/components/settings/settings-nav.ts` | org entry "Model prices" |
| `web/src/routes/$org/settings/models.tsx`, `models.test.tsx` | new |

## 1. `api/queries-prices.ts`

```ts
import { $api } from "./client";
import { projectRef } from "./project-ref";

export function orgPricesInit(org: string) {
  return { params: { path: { org } } };
}

export function orgPricesKey(org: string) {
  return $api.queryOptions("get", "/api/v1/orgs/{org}/models", orgPricesInit(org)).queryKey;
}

export function projectPricesInit(
  org: string,
  project: string,
  scope: "project" | "org" | "builtin",
  q: string,
) {
  return {
    params: {
      path: { project: projectRef(org, project) },
      query: { scope, limit: 100, ...(q ? { q } : {}) },
    },
  };
}

/**
 * The key prefix of every project price list and resolution (all projects, all scopes), so one
 * invalidation refreshes them; `org-members.tsx` invalidates a path prefix the same way.
 */
export const PROJECT_PRICES_PREFIX = ["get", "/api/v1/projects/{project}/models"] as const;
export const PROJECT_RESOLVE_PREFIX = ["get", "/api/v1/projects/{project}/models/resolve"] as const;
```

T7 uses the project helpers.

## 2. `lib/roles.ts`

```ts
/** Mirrors `OrgPerm::ManagePrices`: org admins and owners (D-T-6). */
export function canManageOrgPrices(role: OrgRole | undefined): boolean {
  return atLeast(role, "admin");
}
```

## 3. `lib/prices.ts`

```ts
import type { Schema } from "@/api/types";

export type PriceRow = Schema<"ModelPrice">;
export type PriceInput = Schema<"ModelPriceInput">;

/** `$2.50 / 1M` style text for one unit price, or "—" when absent. */
export function formatUnitPrice(value: number | undefined): string;

/** The form's state: every field a string, as typed. */
export interface PriceForm {
  modelName: string;
  /** "exact": the pattern is the escaped name; "regex": `pattern` is used as typed. */
  matchKind: "exact" | "regex";
  pattern: string;
  provider: string;
  /** `YYYY-MM-DD` or "" for "always". Interpreted as 00:00 UTC. */
  validFrom: string;
  input: string;
  output: string;
  cacheRead: string;
  cacheWrite: string;
  reasoning: string;
  tiers: TierForm[];
}

export interface TierForm {
  id: number;
  minInputTokens: string;
  input: string;
  output: string;
  cacheRead: string;
  cacheWrite: string;
  reasoning: string;
}

/** An empty form, or the form of an existing row. A row whose pattern equals `escapeRegex(model_name)` opens as "exact". */
export function priceForm(row?: PriceRow): PriceForm;

/** The request body. Blank prices are left out; `valid_from` is `<date>T00:00:00.000000000Z` or left out; the pattern is left out for "exact". */
export function priceBody(form: PriceForm): PriceInput;

/** Client-side checks that need no server: a name, at least an input or an output price, numbers ≥ 0. Keys are the server's field names (`model_name`, `price_input`, `price_output`, …, `tiers[i].min_input_tokens`). */
export function checkPriceForm(form: PriceForm): Record<string, string>;

/** Rust's `regex::escape`: every one of `\.+*?()|[]{}^$#&-~` gets a backslash. */
export function escapeRegex(text: string): string;
```

Rules:
- `formatUnitPrice(2.5)` → `"$2.50"`, `formatUnitPrice(0)` → `"$0"`, `formatUnitPrice(0.075)` → `"$0.075"` (up to 6 significant decimals, trailing zeros trimmed, at least 2 decimals when not an integer — write it as: `value === 0 ? "$0" : Number.isInteger(value) ? \`$${value}\` : \`$${value.toFixed(Math.max(2, Math.min(6, decimals(value))))}\`` where `decimals` counts the digits after the point of `String(value)`), `undefined` → `"—"`.
- `escapeRegex` must match `regex::escape` exactly, so "exact" rows round-trip: the meta characters escaped by `regex::escape` (regex-syntax `is_meta_character`) are `\ . + * ? ( ) | [ ] { } ^ $ # & - ~`. Test it against the fixture: `escapeRegex("gpt-4o") === "gpt\\-4o"` and `escapeRegex("llama3.1:8b") === "llama3\\.1:8b"`.
- `priceBody`: `model_name: form.modelName.trim()`; `match_pattern` only when `matchKind === "regex"` (trimmed); `provider` only when non-blank (trimmed, lowercased); `valid_from` only when `validFrom` is set; each price as `Number(text)` only when non-blank; `tiers` mapped the same way with `min_input_tokens: Number(minInputTokens)`.
- `checkPriceForm`: blank name → `model_name: "Enter a name."`; `matchKind === "regex"` and blank pattern → `match_pattern: "Enter a pattern, or choose Exact name."`; any non-blank price that is not a finite number ≥ 0 → that field: `"Enter a number of 0 or more."`; input and output both blank → `price_input: "Enter an input or an output price."`; a tier whose `minInputTokens` is not an integer ≥ 1 → `tiers[i].min_input_tokens: "Enter a whole number of 1 or more."`.

Tests (`lib/prices.test.ts`): each rule above, plus a round trip: `priceBody(priceForm(fixtureRow))` equals the fixture row's input fields (use `fixtures/api/static/model-price.json`, whose pattern `gpt\-4o` opens as "exact").

## 4. Components

### `components/prices/tiers-editor.tsx`

`export function TiersEditor({ tiers, onChange, errors, disabled }: { tiers: TierForm[]; onChange: (tiers: TierForm[]) => void; errors: Record<string, string>; disabled?: boolean })`. One row per tier: "From input tokens" (`TextField`, `type="number"`, id `tier-<id>-min`), then Input, Output, Cache read, Cache write, Reasoning (`type="number"`, ids `tier-<id>-<part>`), and a remove button (`aria-label="Remove tier"`). Errors come from `errors["tiers[i].<field>"]` and `errors["tiers[i]"]` (shown under the row). An "Add tier" button appends `{ id: nextId(), minInputTokens: "", … }`, and is disabled at 8 tiers with the hint "At most 8 tiers." Copy the row-id counter pattern of `connection-dialog.tsx`'s header rows (`let tierRowId = 0`).

### `components/prices/price-dialog.tsx`

```ts
export type PriceScope =
  | { kind: "org"; org: string }
  | { kind: "project"; org: string; project: string };

export function PriceDialog({
  scope,
  open,
  onOpenChange,
  row,
}: {
  scope: PriceScope;
  open: boolean;
  onOpenChange: (open: boolean) => void;
  /** The row to edit; absent to create. */
  row?: Schema<"ModelPrice">;
}): JSX.Element;
```

Layout (labels exactly, the tests use them):
- Title "Add model price" or "Edit model price"; description "Prices are USD per 1M tokens. New prices apply to spans received from now on; stored costs are not recomputed." (D-T-5).
- "Model name" (`TextField`, id `price-model-name`).
- "Match" radio group (`RadioGroup`, `RadioGroupItem` from `@/components/ui/radio-group`): "Exact name" and "Regular expression". With "Regular expression": "Pattern" (`TextField`, id `price-pattern`, description "Anchored and case-insensitive, e.g. llama3.*").
- "Provider" (`TextField`, id `price-provider`, description "Leave empty to match any provider. Local models served through an OpenAI-compatible endpoint use openai.").
- "Valid from" (`TextField`, `type="date"`, id `price-valid-from`, description "Leave empty to apply to every span.").
- "Input", "Output", "Cache read", "Cache write", "Reasoning" (`TextField`, `type="number"`, step any, ids `price-input`, `price-output`, `price-cache-read`, `price-cache-write`, `price-reasoning`; Reasoning's description "Empty uses the output price.").
- "Long-context tiers" heading and `TiersEditor`.
- Footer: Cancel, and a `SubmitButton` "Add price" or "Save".

Behaviour:
- State `form` = `priceForm(row)` when the dialog opens (reset on each open: key the form state on `open` and `row?.id` the way `connection-dialog.tsx` resets).
- Submit: `checkPriceForm(form)`; errors → show them, no request. Else `POST` or `PATCH` to `/api/v1/orgs/{org}/models[/{id}]` or `/api/v1/projects/{project}/models[/{id}]` (`projectRef(org, project)` for the project path), body `priceBody(form)`.
- Success: `queryClient.invalidateQueries({ queryKey: orgPricesKey(org) })`, `…({ queryKey: [...PROJECT_PRICES_PREFIX] })` and `…({ queryKey: [...PROJECT_RESOLVE_PREFIX] })` for either scope (an org write changes every project's list and resolution); close.
- Server errors: `fieldErrors(error)` placed under fields whose server name the form renders (`model_name`, `match_pattern`, `provider`, `valid_from`, `price_*`, `tiers…`); a `409` with `rules.<id>` errors shows a `FormError`: "Budgeted online rules use this price: <rule names>." where the names are the text before the first `:` of each message (D-T-8 messages are `<rule name>: …`); any other error → `FormError` with `problemMessage(error, "Could not save the price.")`.

### `components/prices/price-table.tsx`

```ts
export function PriceTable({
  label,
  rows,
  actions,
}: {
  /** The table's accessible name. */
  label: string;
  rows: Schema<"ModelPrice">[];
  /** Per-row actions (Edit, Delete); absent for read-only tables. */
  actions?: (row: Schema<"ModelPrice">) => ReactNode;
}): JSX.Element;
```

Columns: "Model" (name, and under it the pattern in `font-mono text-xs text-muted-foreground` when it is not `escapeRegex(model_name)`), "Provider" (`provider ?? "Any"`), "Input", "Output", "Cache read", "Cache write" (`formatUnitPrice`), "Valid from" (date part of `valid_from`, or "Always" when it is the epoch), "Tiers" (`tiers.length || "—"`), and an actions column (`<span className="sr-only">Actions</span>` header) when `actions` is set. Prices use `tabular-nums` and right alignment, as other numeric columns do.

## 5. Nav

`settings-nav.ts`: add `{ to: "/$org/settings/models", label: "Model prices", ownerOnly: false },` after the LLM connections entry. The route tree is generated by the TanStack router plugin at build and test time and is not committed.

## 6. The page (`routes/$org/settings/models.tsx`)

```ts
export const Route = createFileRoute("/$org/settings/models")({ component: OrgPricesPage });
```

- Role as `llm-connections.tsx` gets it; `canManage = canManageOrgPrices(role)`.
- `SettingsPage` title "Model prices", description "Prices for every project of this organization. A project's own price beats an organization price, which beats the built-in LiteLLM prices. New prices apply to spans received from now on; stored costs are not recomputed." Action: "Add price" behind `DisabledReason` ("Only admins and owners can change prices").
- `$api.useQuery("get", "/api/v1/orgs/{org}/models", orgPricesInit(org))`; pending → three skeletons; empty → `EmptyState` (icon `Coins` from lucide-react) "No organization prices" / "Built-in prices apply. Add a price for a model they do not cover, such as a local model."; else `PriceTable` label "Organization prices" with Edit / Delete buttons (only when `canManage`).
- Delete: `ConfirmDialog` "Delete this price?" / "Spans received after this use the next matching price. Stored costs do not change."; on `409`, show the same rule-name message as the dialog (put the message builder in `lib/prices.ts` as `export function blockedByRules(error: unknown): string | undefined`, and test it there).

## 7. Tests (`routes/$org/settings/models.test.tsx`)

Use MSW overrides only (`server.use`), as the connections test does.

1. "lists organization prices": the fixture row shows its name, "Any", `$0`, and the nav link "Model prices" is current.
2. "shows the empty state": override `GET /api/v1/orgs/:org/models` with `{data: [], next_cursor: null}`.
3. "adds a price with an exact name": fill Model name `llama3.1:8b`, Input `0`, Output `0`, submit; the captured body is `{model_name: "llama3.1:8b", price_input: 0, price_output: 0, tiers: []}` (no `match_pattern`, no provider).
4. "adds a regex price with a tier": choose Regular expression, Pattern `llama3.*`, Input `0.1`, Output `0.2`, add a tier from `128000` with Input `0.2`; body has `match_pattern: "llama3.*"` and one tier.
5. "checks the form before sending": submit with no prices → "Enter an input or an output price." and no request (count requests in the override).
6. "places server field errors": the POST answers `400` with `errors: [{field: "match_pattern", message: "is not a valid regular expression"}]` → the message shows under Pattern.
7. "explains a price that budgeted rules need": DELETE answers `409` with `errors: [{field: "rules.0190…", message: "Relevance on answers: the judge model would have no price"}]` → "Budgeted online rules use this price: Relevance on answers."
8. "members can read but not change": `meAs("member")` → no Edit buttons, "Add price" disabled with the reason.
9. "edits a row": Edit opens the dialog prefilled. The fixture's pattern `llama3\.1:8b` is not `escapeRegex("Llama 3.1 8B (local)")`, so it opens with Regular expression selected and that pattern. Change Output to `0.5`, Save → the PATCH body carries `match_pattern: "llama3\\.1:8b"` and `price_output: 0.5`.

## Commands

```bash
export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH
```

```bash
pnpm --dir web test -- models prices
```

```bash
just check
```

## Done when

- `/acme/settings/models` works on the mocks (`just web-dev`) and in the tests above; `lib/prices.test.ts` passes.
- `just check` is green.

**Commit:** `web: add the org model prices page`
