# T5 — `OrgPerm::ManagePrices` and the org routes

**Commit subject:** `server: add org model price routes`

Adds org rows: `GET/POST /api/v1/orgs/{org}/models` and `PATCH/DELETE /api/v1/orgs/{org}/models/{id}` (spec 09, D-T-6, D-T-7), the new `OrgPerm::ManagePrices`, OpenAPI and fixtures. The write path is T4's `prices::write` with `project` `None`; an org write is checked against the budgeted rules of every project of the org (D-T-8).

## Read first

- README D-T-6, D-T-7, D-T-8.
- T4 (all of it): `api::model_prices`, `prices::{validate_input, write, PriceChange, WriteOutcome, record_json}`, `routes/model_prices.rs`.
- `crates/server/src/routes/connections.rs` (`list_llm_connections`, an org route with `Extension<OrgAccess>`), `crates/server/src/http/access.rs` (`OrgAccess`).
- `crates/server/src/auth/roles.rs`: `OrgPerm`, `min_role`, test `permissions_follow_the_spec_table`.
- `crates/server/src/http/access_suite.rs`: how org permissions map to expected roles. Find every exhaustive `match` on `OrgPerm`:

```bash
grep -rn "OrgPerm::ManageConnections" crates/server/src
```

## Files

| File | Change |
|---|---|
| `crates/server/src/auth/roles.rs` | `OrgPerm::ManagePrices` |
| every file the grep above lists that matches on `OrgPerm` | the new variant next to `ManageConnections` |
| `crates/server/src/routes/model_prices.rs` | `org_routes()` and four handlers |
| `crates/server/src/http/access.rs` | `routes.extend(crate::routes::model_prices::org_routes());` after the line T4 added |
| `crates/server/src/api/openapi.rs`, `crates/server/tests/openapi.rs` | four operations |
| `crates/server/openapi.json`, `web/src/api/schema.gen.ts` | regenerated |
| `fixtures/api/static/org-model-prices.json` | new |
| `crates/server/src/api_examples.rs`, `crates/server/tests/api_examples.rs`, `fixtures/api/manifest.json` | one example |
| `web/src/mocks/writes.ts`, `web/src/mocks/writes.test.ts` | the three org writes (the web test lists every write under `/api/v1/orgs`) |
| `CHANGELOG.md` | extend T4's line |

## 1. `OrgPerm::ManagePrices`

In `roles.rs`, after `ManageConnections`:

```rust
    /// Create, replace and delete the org's model prices (D-T-6).
    ManagePrices,
```

`min_role`: add `| Self::ManagePrices` to the arm that returns `OrgRole::Admin`. In `permissions_follow_the_spec_table`, add `assert_eq!(OrgPerm::ManagePrices.min_role(), OrgRole::Admin);` after the `ManageConnections` line. Add the variant to every other `match` the grep found, with the same arm as `ManageConnections`. If one of them treats `ManageConnections` specially (not as "an admin permission"), STOP and report.

## 2. Routes

```rust
pub(crate) fn org_routes() -> Vec<RouteDef> {
    vec![
        // GET    /api/v1/orgs/{org}/models        OrgPerm::View          list_org_model_prices
        // POST   /api/v1/orgs/{org}/models        OrgPerm::ManagePrices  create_org_model_price
        // PATCH  /api/v1/orgs/{org}/models/{id}   OrgPerm::ManagePrices  update_org_model_price
        // DELETE /api/v1/orgs/{org}/models/{id}   OrgPerm::ManagePrices  delete_org_model_price
    ]
}
```

Each `RouteDef` has `access: Access::Org { perm, session_only: false }` and `documented: true`, as in `connections.rs`.

Handlers take `Extension(access): Extension<OrgAccess>` and use `let org = access.org.id;`:

- **`list_org_model_prices`** (no query parameters): `model_prices::list_org_rows(state.meta.reader(), org)`, map with `record_json`, answer `Page { data, next_cursor: None }` (at most 500 rows, D-T-4; not paged).
- **`create_org_model_price`**: as T4's `create_model_price` with `validate_input(&body, PriceScope::Org)` and `prices::write(&state, org, None, PriceChange::Insert { id, row })`. Audit message `"org model price created"` with `price_id`, `org_id`, `user_id`.
- **`update_org_model_price`**, **`delete_org_model_price`**: as T4's, with `project` `None`; audit `"org model price updated"` / `"org model price deleted"`.

Move T4's outcome-to-response mapping into one private function used by all seven write handlers if T4 wrote it three times:

```rust
fn written(outcome: WriteOutcome) -> Result<Option<PriceRecord>, ApiError>;
```

(`Done(r)` → `Ok(r)`; `NotFound` → `Err(ApiError::NotFound)`; `Invalid(e)` → `Err(ApiError::BadRequest { detail: None, errors: e })`; `BreaksRules(e)` → `Err(ApiError::ConflictWith { detail: "the change would leave budgeted online rules without a price", errors: e })`.)

### Tests

1. `org_round_trip`: an org admin POSTs `{model_name: "llama3.1:8b", price_input: 0, price_output: 0}` → `201`, `scope: "org"`; GET lists it; PATCH → `200`; DELETE → `204`; GET → empty.
2. `an_org_row_reaches_every_project`: after the POST, `GET /projects/{p}/models?scope=org` of two projects of the org list it, and `resolve?model=llama3.1:8b` in each returns it; a project of another org does not see it.
3. `a_project_row_beats_the_org_row`: org row at 1.0, project row at 2.0 for the same name: `resolve` in that project returns the project row, in the other project the org row.
4. `org_writes_guard_rules_in_every_project`: a budgeted rule in project B priced only by the org row: DELETE of the org row → `409` naming the rule; after a project row in B prices the model, the same DELETE → `204` (B is still priced).
5. `permissions`: an org member and viewer can GET and get `403` on POST/PATCH/DELETE; an org admin can write; an id of another org's row → `404`; an org row id through a project route (`PATCH /projects/{p}/models/{org row id}`) → `404`, and a project row id through the org route → `404` (scopes do not cross).
6. `the_501st_org_row_is_409`.

## 3. OpenAPI

In `model_price_paths` (T4), add:

| Operation id | Method, path | Params | Body | Success | Errors |
|---|---|---|---|---|---|
| `list_org_model_prices` | GET `/api/v1/orgs/{org}/models` | the org path parameter (as `list_llm_connections`) | — | `200` `json_named("ModelPricePage", …)` | the errors `list_llm_connections` declares |
| `post_org_model_price` | POST same | org | `json_body("ModelPriceInput")` | `201` `json_of::<ModelPrice>` | `&[400, 401, 403, 404, 409, 429]` |
| `patch_org_model_price` | PATCH `/api/v1/orgs/{org}/models/{id}` | org, `path_uuid("id", "Price row id.")` | `json_body("ModelPriceInput")` | `200` | `&[400, 401, 403, 404, 409, 429]` |
| `delete_org_model_price` | DELETE same | same | — | `204` | `&[400, 401, 403, 404, 409, 429]` |

Descriptions: read "Any org member can read."; writes "Org admins. A PATCH replaces the whole row. A write that would leave a budgeted online rule without a price is 409." Update `tests/openapi.rs` (`OPERATIONS`, `expected_errors`, `pages` for `list_org_model_prices`). Run `just openapi`.

## 4. Fixtures

- `static/org-model-prices.json`: a `ModelPricePage` with the org row `…0c00` (README of fixtures, T4) and `next_cursor: null`.
- `api_examples.rs`: `example("list_org_model_prices", BTreeMap::new(), BTreeMap::new(), "static/org-model-prices.json", "application/json")` (no `org` path param, as `list_llm_connections`'s example has none; if it has one, copy it). `tests/api_examples.rs`: round-trip it as T4's page.
- Run `just api-examples`.

## 5. Dev write mocks

`web/src/mocks/writes.test.ts` ("lists every account and admin write of the contract") expects a mock for every write under `/api/v1/orgs`, so this phase adds the three org writes, or `just check-web` fails.

In `web/src/mocks/writes.ts`:
- import `orgModelPricesDocument from "../../../fixtures/api/static/org-model-prices.json";` (alphabetical among the imports);
- in `WRITE_OPERATIONS`, after the LLM connection entries: `{ operationId: "post_org_model_price", method: "post", path: "/api/v1/orgs/{org}/models" }`, `{ operationId: "patch_org_model_price", method: "patch", path: "/api/v1/orgs/{org}/models/{id}" }`, `{ operationId: "delete_org_model_price", method: "delete", path: "/api/v1/orgs/{org}/models/{id}" }` (format as Biome wants);
- a helper below `named`:

```ts
/** The fields of a price body the mocks echo: the trimmed `model_name` when it is a non-empty string. */
function priced(body: Record<string, unknown>): { model_name: string } | Record<string, never> {
  const name = trimmed(body.model_name);
  return name ? { model_name: name } : {};
}
```

- in `RESPONSES`, after `post_llm_connection_test`:

```ts
  post_org_model_price: ({ body }) =>
    HttpResponse.json({ ...orgModelPricesDocument.data[0], ...priced(body) }, { status: 201 }),
  patch_org_model_price: ({ body, params }) =>
    HttpResponse.json({ ...orgModelPricesDocument.data[0], ...priced(body), id: params.id }),
  delete_org_model_price: () => noContent(),
```

In `writes.test.ts`, add `it("answers the org model price writes", …)`: POST `/api/v1/orgs/acme/models` with `{"model_name": "  Local  ", "price_input": 0, "price_output": 0}` → `201`, `model_name` `"Local"`; PATCH `…/models/01900000-0000-7000-8000-000000000c00` → `200` with that `id`; DELETE → `204`. Use the file's `send` helper as the channel test does.

## 6. CHANGELOG

Spec 08 already names the permissions (the planning commit did it). In `CHANGELOG.md`, extend T4's `Added` line with `Org-wide prices under \`/api/v1/orgs/{org}/models\` apply to every project of the org; a project price beats an org price.`

## Commands

```bash
cargo test -p llmobs-server model_prices
```

```bash
just openapi
```

```bash
just api-examples
```

```bash
just check
```

## Done when

- The four org routes work as § 2 says, with the 6 tests; `ManagePrices` is in every `OrgPerm` match.
- The three org writes have dev mocks and a mock test.
- OpenAPI, the TypeScript schema and the fixtures are regenerated.
- `just check` is green.

**Commit:** `server: add org model price routes`
